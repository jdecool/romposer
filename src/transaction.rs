//! Port of `DependencyResolver\Transaction` (and `LocalRepoTransaction`): the
//! ordered list of install/update/uninstall operations between a present set
//! of packages (installed.json) and a result set (the lock).

use crate::package::{Package, is_platform_package};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Operation {
    Install(Package),
    Update { initial: Package, target: Package },
    Uninstall(Package),
    MarkAliasInstalled(Package),
    MarkAliasUninstalled(Package),
}

impl Operation {
    pub fn package(&self) -> &Package {
        match self {
            Operation::Install(p)
            | Operation::Uninstall(p)
            | Operation::MarkAliasInstalled(p)
            | Operation::MarkAliasUninstalled(p) => p,
            Operation::Update { target, .. } => target,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Operation::Install(_) => "install",
            Operation::Update { .. } => "update",
            Operation::Uninstall(_) => "uninstall",
            Operation::MarkAliasInstalled(_) => "markAliasInstalled",
            Operation::MarkAliasUninstalled(_) => "markAliasUninstalled",
        }
    }

    /// `OperationInterface::show(false)`
    pub fn show(&self) -> String {
        match self {
            Operation::Install(p) => {
                format!("Installing {} ({})", p.pretty_name, p.full_pretty_version())
            }
            Operation::Update { initial, target } => {
                let action = if crate::semver::is_upgrade(&initial.version, &target.version) {
                    "Upgrading"
                } else {
                    "Downgrading"
                };
                format!(
                    "{action} {} ({} => {})",
                    initial.pretty_name,
                    initial.full_pretty_version(),
                    target.full_pretty_version()
                )
            }
            Operation::Uninstall(p) => {
                format!("Removing {} ({})", p.pretty_name, p.full_pretty_version())
            }
            Operation::MarkAliasInstalled(p) => {
                format!(
                    "Marking {} ({}) as installed, alias of {} ({})",
                    p.pretty_name,
                    p.full_pretty_version(),
                    p.pretty_name,
                    p.alias_of
                        .as_ref()
                        .map_or(String::new(), |r| r.full_pretty_version())
                )
            }
            Operation::MarkAliasUninstalled(p) => {
                format!(
                    "Marking {} ({}) as uninstalled, alias of {} ({})",
                    p.pretty_name,
                    p.full_pretty_version(),
                    p.pretty_name,
                    p.alias_of
                        .as_ref()
                        .map_or(String::new(), |r| r.full_pretty_version())
                )
            }
        }
    }
}

/// `BasePackage::getNames`: name plus provided and replaced names.
fn names(package: &Package) -> Vec<&str> {
    let mut names = vec![package.name.as_str()];
    for link in package.provides.iter().chain(package.replaces.iter()) {
        if !names.contains(&link.target.as_str()) {
            names.push(&link.target);
        }
    }
    names
}

fn package_sort(a: &Package, b: &Package) -> std::cmp::Ordering {
    if a.name == b.name {
        if a.alias_of.is_some() != b.alias_of.is_some() {
            return if a.alias_of.is_some() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        return b.version.cmp(&a.version);
    }
    b.name.cmp(&a.name)
}

/// `Transaction::__construct` + `getOperations`
pub fn calculate_operations(present: &[Package], result: &[Package]) -> Vec<Operation> {
    // resultPackageMap: identity by index, sorted by name descending (aliases first)
    let mut order: Vec<usize> = (0..result.len()).collect();
    order.sort_by(|a, b| package_sort(&result[*a], &result[*b]));
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, package) in result.iter().enumerate() {
        for name in names(package) {
            by_name.entry(name).or_default().push(i);
        }
    }
    for list in by_name.values_mut() {
        list.sort_by(|a, b| package_sort(&result[*a], &result[*b]));
    }
    let providers =
        |target: &str| -> Vec<usize> { by_name.get(target).cloned().unwrap_or_default() };
    // alias → index of its real package (same name, same version as alias_of)
    let alias_target = |i: usize| -> Option<usize> {
        let real = result[i].alias_of.as_deref()?;
        result
            .iter()
            .position(|p| p.alias_of.is_none() && p.name == real.name && p.version == real.version)
    };

    let mut present_map: HashMap<&str, &Package> = HashMap::new();
    let mut remove_map: Vec<&Package> = Vec::new();
    let mut present_alias: HashSet<String> = HashSet::new();
    let mut remove_alias: Vec<&Package> = Vec::new();
    for package in present {
        if package.alias_of.is_some() {
            present_alias.insert(format!("{}::{}", package.name, package.version));
            remove_alias.push(package);
        } else {
            present_map.insert(&package.name, package);
            remove_map.push(package);
        }
    }

    // getRootPackages: packages not required by any other package in the result
    let mut roots: Vec<usize> = order.clone();
    for &i in &order {
        if !roots.contains(&i) {
            continue;
        }
        for link in &result[i].requires {
            for req in providers(&link.target) {
                if req != i {
                    roots.retain(|r| *r != req);
                }
            }
        }
    }

    let mut operations: Vec<Operation> = Vec::new();
    let mut stack = roots;
    let mut visited: HashSet<usize> = HashSet::new();
    let mut processed: HashSet<usize> = HashSet::new();
    while let Some(i) = stack.pop() {
        if processed.contains(&i) {
            continue;
        }
        if !visited.contains(&i) {
            visited.insert(i);
            stack.push(i);
            if result[i].alias_of.is_some() {
                if let Some(real) = alias_target(i) {
                    stack.push(real);
                }
            } else {
                for link in &result[i].requires {
                    stack.extend(providers(&link.target));
                }
            }
        } else {
            processed.insert(i);
            let package = &result[i];
            if package.alias_of.is_some() {
                let key = format!("{}::{}", package.name, package.version);
                if present_alias.contains(&key) {
                    remove_alias.retain(|p| format!("{}::{}", p.name, p.version) != key);
                } else {
                    operations.push(Operation::MarkAliasInstalled(package.clone()));
                }
            } else if let Some(source) = present_map.get(package.name.as_str()) {
                let dist_ref = |p: &Package| p.dist.as_ref().and_then(|d| d.reference.clone());
                let source_ref = |p: &Package| p.source.as_ref().and_then(|s| s.reference.clone());
                if package.version != source.version
                    || dist_ref(package) != dist_ref(source)
                    || source_ref(package) != source_ref(source)
                    || package.is_abandoned() != source.is_abandoned()
                    || package.replacement_package() != source.replacement_package()
                {
                    operations.push(Operation::Update {
                        initial: (*source).clone(),
                        target: package.clone(),
                    });
                }
                remove_map.retain(|p| p.name != package.name);
            } else {
                operations.push(Operation::Install(package.clone()));
                remove_map.retain(|p| p.name != package.name);
            }
        }
    }

    for package in remove_map {
        operations.insert(0, Operation::Uninstall(package.clone()));
    }
    for package in remove_alias {
        operations.push(Operation::MarkAliasUninstalled(package.clone()));
    }

    let operations = move_plugins_to_front(operations);
    move_uninstalls_to_front(operations)
}

fn move_plugins_to_front(operations: Vec<Operation>) -> Vec<Operation> {
    let mut dl_no_deps = Vec::new();
    let mut dl_with_deps = Vec::new();
    let mut dl_requires: Vec<String> = Vec::new();
    let mut plugins_no_deps = Vec::new();
    let mut plugins_with_deps = Vec::new();
    let mut plugin_requires: Vec<String> = Vec::new();
    let mut remaining: Vec<Option<Operation>> = operations.into_iter().map(Some).collect();

    for idx in (0..remaining.len()).rev() {
        let Some(op) = &remaining[idx] else { continue };
        let package = match op {
            Operation::Install(p) => p,
            Operation::Update { target, .. } => target,
            _ => continue,
        };
        let is_dl_plugin = package.kind == "composer-plugin"
            && package.extra.get("plugin-modifies-downloads")
                == Some(&serde_json::Value::Bool(true));
        let pkg_names = names(package);
        let requires: Vec<String> = package
            .requires
            .iter()
            .map(|l| l.target.clone())
            .filter(|r| !is_platform_package(r))
            .collect();
        if is_dl_plugin || pkg_names.iter().any(|n| dl_requires.iter().any(|r| r == n)) {
            if is_dl_plugin && requires.is_empty() {
                dl_no_deps.insert(0, remaining[idx].take().unwrap());
            } else {
                dl_requires.extend(requires);
                dl_with_deps.insert(0, remaining[idx].take().unwrap());
            }
            continue;
        }
        let is_plugin = package.kind == "composer-plugin" || package.kind == "composer-installer";
        if is_plugin
            || pkg_names
                .iter()
                .any(|n| plugin_requires.iter().any(|r| r == n))
        {
            if is_plugin && requires.is_empty() {
                plugins_no_deps.insert(0, remaining[idx].take().unwrap());
            } else {
                plugin_requires.extend(requires);
                plugins_with_deps.insert(0, remaining[idx].take().unwrap());
            }
        }
    }

    let mut out = Vec::new();
    out.extend(dl_no_deps);
    out.extend(dl_with_deps);
    out.extend(plugins_no_deps);
    out.extend(plugins_with_deps);
    out.extend(remaining.into_iter().flatten());
    out
}

fn move_uninstalls_to_front(operations: Vec<Operation>) -> Vec<Operation> {
    let (uninstalls, rest): (Vec<Operation>, Vec<Operation>) =
        operations.into_iter().partition(|op| {
            matches!(
                op,
                Operation::Uninstall(_) | Operation::MarkAliasUninstalled(_)
            )
        });
    uninstalls.into_iter().chain(rest).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Value;

    fn pkg(json: &str) -> Package {
        let v: Value = serde_json::from_str(json).unwrap();
        crate::package::load(v.as_object().unwrap()).unwrap()
    }

    #[test]
    fn installs_dependencies_before_dependents_and_lowest_name_first() {
        let result = vec![
            pkg(r#"{"name":"c/app","version":"1.0","require":{"b/lib":"*","a/base":"*"}}"#),
            pkg(r#"{"name":"b/lib","version":"1.0","require":{"a/base":"*"}}"#),
            pkg(r#"{"name":"a/base","version":"1.0"}"#),
            pkg(r#"{"name":"d/other","version":"1.0"}"#),
        ];
        let ops = calculate_operations(&[], &result);
        let names: Vec<&str> = ops.iter().map(|o| o.package().name.as_str()).collect();
        assert_eq!(names, vec!["a/base", "b/lib", "c/app", "d/other"]);
    }

    #[test]
    fn plugins_move_to_front_and_removals_first() {
        let present = vec![
            pkg(r#"{"name":"z/gone","version":"1.0"}"#),
            pkg(r#"{"name":"a/base","version":"0.9"}"#),
        ];
        let result = vec![
            pkg(r#"{"name":"a/base","version":"1.0"}"#),
            pkg(
                r#"{"name":"p/plugin","version":"1.0","type":"composer-plugin","require":{"composer-plugin-api":"^2"}}"#,
            ),
        ];
        let ops = calculate_operations(&present, &result);
        let shown: Vec<String> = ops.iter().map(Operation::show).collect();
        assert_eq!(
            shown,
            vec![
                "Removing z/gone (1.0)",
                "Installing p/plugin (1.0)",
                "Upgrading a/base (0.9 => 1.0)"
            ]
        );
    }

    #[test]
    fn aliases_are_marked_after_their_package() {
        let result = vec![pkg(
            r#"{"name":"a/lib","version":"dev-main","extra":{"branch-alias":{"dev-main":"1.x-dev"}}}"#,
        )];
        let mut packages = Vec::new();
        for p in result {
            let real = (**p.alias_of.as_ref().unwrap()).clone();
            packages.push(p);
            packages.push(real);
        }
        let ops = calculate_operations(&[], &packages);
        assert_eq!(
            ops.iter().map(Operation::kind).collect::<Vec<_>>(),
            vec!["install", "markAliasInstalled"]
        );
    }
}
