//! `composer depends`/`why` and `prohibits`/`why-not`: `BaseDependencyCommand` and
//! `InstalledRepository::getDependents`.

use crate::commands::dump_autoload::load_project;
use crate::commands::table;
use crate::installed::InstalledRepository;
use crate::io::Io;
use crate::package::root::DEFAULT_PRETTY_VERSION;
use crate::package::{Link, LinkType, Package, is_platform_package};
use crate::platform::{PhpInfo, PlatformRepository, overrides_from_config};
use crate::semver::{Constraint, version_parser};
use std::path::Path;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Package to inspect
    pub package: String,
    /// Optional version constraint
    #[arg(value_name = "VERSION", id = "package_version")]
    pub version: Option<String>,
    /// Recursively resolves up to the root package
    #[arg(short = 'r', long)]
    pub recursive: bool,
    /// Prints the results as a nested tree
    #[arg(short = 't', long)]
    pub tree: bool,
    /// Read dependency information from composer.lock
    #[arg(long)]
    pub locked: bool,
}

pub fn depends(args: &Args, io: &mut Io) -> i32 {
    run(args, io, false)
}

pub fn prohibits(args: &Args, io: &mut Io) -> i32 {
    run(args, io, true)
}

fn run(args: &Args, io: &mut Io, inverted: bool) -> i32 {
    match execute(args, io, inverted) {
        Ok(code) => code,
        Err(message) => {
            io.error(&message);
            1
        }
    }
}

/// One line of the result: the dependent, the link, and its own dependents (`None` = circular).
struct Dependent {
    package: Package,
    link: Link,
    /// `Link::TYPE_DOES_NOT_REQUIRE` has no link type of its own
    description: Option<&'static str>,
    children: Option<Vec<Dependent>>,
}

fn dep(package: &Package, link: &Link, children: Option<Vec<Dependent>>) -> Dependent {
    Dependent {
        package: package.clone(),
        link: link.clone(),
        description: None,
        children,
    }
}

fn link_string(link: &Link) -> String {
    format!(
        "{} {} {} ({})",
        link.source,
        link.kind.description(),
        link.target,
        link.pretty_constraint
    )
}

fn link_with(source: &str, target: &str, kind: LinkType, pretty: &str) -> Link {
    Link {
        source: source.to_string(),
        target: target.to_string(),
        constraint: Constraint::MatchAll,
        pretty_constraint: pretty.to_string(),
        kind,
    }
}

/// `InstalledRepository::getDependents`
fn dependents(
    packages: &[Package],
    root_name: &str,
    needles: &[String],
    constraint: Option<&Constraint>,
    invert: bool,
    recurse: bool,
    found: Option<&[String]>,
) -> Vec<Dependent> {
    let mut needles: Vec<String> = needles.iter().map(|n| n.to_lowercase()).collect();
    let found: Vec<String> = found.map(|f| f.to_vec()).unwrap_or_else(|| needles.clone());
    let root = packages.iter().find(|p| p.name == root_name);
    let find_packages =
        |name: &str| -> Vec<&Package> { packages.iter().filter(|p| p.name == name).collect() };
    let mut results = Vec::new();
    for package in packages {
        let mut links: Vec<Link> = package.requires.clone();
        let mut in_tree = found.clone();
        if !invert {
            links.extend(package.replaces.iter().cloned());
            for link in &package.replaces {
                for needle in needles.clone() {
                    if link.source == needle
                        && constraint.is_none_or(|c| link.constraint.matches(c))
                    {
                        if in_tree.contains(&link.target) {
                            results.push(dep(package, link, None));
                            continue;
                        }
                        in_tree.push(link.target.clone());
                        let children = if recurse {
                            dependents(
                                packages,
                                root_name,
                                std::slice::from_ref(&link.target),
                                None,
                                false,
                                true,
                                Some(&in_tree),
                            )
                        } else {
                            vec![]
                        };
                        results.push(dep(package, link, Some(children)));
                        needles.push(link.target.clone());
                    }
                }
            }
        }
        if package.name == root_name {
            links.extend(package.dev_requires.iter().cloned());
        }
        for link in &links {
            for needle in &needles {
                if link.target == *needle
                    && constraint.is_none_or(|c| link.constraint.matches(c) == !invert)
                {
                    if in_tree.contains(&link.source) {
                        results.push(dep(package, link, None));
                        continue;
                    }
                    in_tree.push(link.source.clone());
                    let children = if recurse {
                        dependents(
                            packages,
                            root_name,
                            std::slice::from_ref(&link.source),
                            None,
                            false,
                            true,
                            Some(&in_tree),
                        )
                    } else {
                        vec![]
                    };
                    results.push(dep(package, link, Some(children)));
                }
            }
        }
        if invert && needles.contains(&package.name) {
            for link in &package.conflicts {
                for pkg in find_packages(&link.target) {
                    if link.constraint.matches(&Constraint::new("=", &pkg.version)) == invert {
                        results.push(dep(package, link, None));
                    }
                }
            }
        }
        for link in &package.conflicts {
            if needles.contains(&link.target) {
                for pkg in find_packages(&link.target) {
                    if link.constraint.matches(&Constraint::new("=", &pkg.version)) == invert {
                        results.push(dep(package, link, None));
                    }
                }
            }
        }
        if invert
            && let Some(c) = constraint
            && needles.contains(&package.name)
            && c.matches(&Constraint::new("=", &package.version))
        {
            'requires: for link in &package.requires {
                if is_platform_package(&link.target) {
                    if packages.iter().any(|p| {
                        p.name == link.target
                            && link.constraint.matches(&Constraint::new("==", &p.version))
                    }) {
                        continue;
                    }
                    let description = match packages.iter().find(|p| p.name == link.target) {
                        Some(p) => format!("but {} is installed", p.pretty_version),
                        None => "but it is missing".into(),
                    };
                    results.push(dep(
                        package,
                        &link_with(
                            &package.name,
                            &link.target,
                            LinkType::Require,
                            &format!("{} {description}", link.pretty_constraint),
                        ),
                        None,
                    ));
                    continue;
                }
                for pkg in packages {
                    if !crate::solver::names(pkg, true).contains(&link.target) {
                        continue;
                    }
                    let mut version = Constraint::new("=", &pkg.version);
                    if link.target != pkg.name {
                        for prov in pkg.replaces.iter().chain(pkg.provides.iter()) {
                            if link.target == prov.target {
                                version = prov.constraint.clone();
                                break;
                            }
                        }
                    }
                    if !link.constraint.matches(&version) {
                        if let Some(root) = root {
                            for root_req in root.requires.iter().chain(root.dev_requires.iter()) {
                                if crate::solver::names(pkg, true).contains(&root_req.target)
                                    && !root_req.constraint.matches(&link.constraint)
                                {
                                    results.push(dep(package, link, None));
                                    results.push(dep(root, root_req, None));
                                    continue 'requires;
                                }
                            }
                            results.push(dep(package, link, None));
                            results.push(Dependent {
                                package: root.clone(),
                                link: link_with(
                                    &root.name,
                                    &link.target,
                                    LinkType::Require,
                                    &format!("but {} is installed", pkg.pretty_version),
                                ),
                                description: Some("does not require"),
                                children: None,
                            });
                        } else {
                            results.push(dep(package, link, None));
                        }
                    }
                    continue 'requires;
                }
            }
        }
    }
    results
}

fn execute(args: &Args, io: &mut Io, inverted: bool) -> Result<i32, String> {
    let (_, _, root, config, lock) = load_project(io)?;
    let php = PhpInfo::detect();
    let mut installed: Vec<Package> = vec![root.package.clone()];
    if args.locked {
        let Some(lock) = lock.as_ref().filter(|l| l.is_locked()) else {
            return Err(
                "A valid composer.lock file is required to run this command with --locked".into(),
            );
        };
        installed.extend(lock.locked_packages(true)?);
        installed
            .extend(PlatformRepository::new(&lock.platform_overrides(), php.as_ref())?.packages);
    } else {
        let local = InstalledRepository::read(Path::new(&format!(
            "{}/composer/installed.json",
            config.vendor_dir()
        )))?;
        if local.packages.is_empty()
            && (!root.package.requires.is_empty() || !root.package.dev_requires.is_empty())
        {
            io.write("No dependencies installed. Try running composer install or update, or use --locked.");
            return Ok(1);
        }
        installed.extend(local.packages);
        installed.extend(
            PlatformRepository::new(&overrides_from_config(&config), php.as_ref())?.packages,
        );
    }
    let needle = args.package.to_lowercase();
    let text_constraint = args.version.clone().unwrap_or_else(|| "*".into());
    let refs: Vec<&Package> = installed.iter().collect();
    let packages: Vec<Package> =
        crate::lock::find_with_replacers_and_providers(&refs, &needle, None)
            .into_iter()
            .cloned()
            .collect();
    if packages.is_empty() {
        return Err(format!(
            "Could not find package \"{needle}\" in your project"
        ));
    }
    let parsed = version_parser::parse_constraints(&text_constraint)?;
    let matched = installed
        .iter()
        .find(|p| p.name == needle && parsed.matches(&Constraint::new("==", &p.version)))
        .cloned();
    match &matched {
        None => {
            let http = std::rc::Rc::new(crate::download::Http::new(&config)?);
            let mut repos = crate::repository::from_config(&config, http)?;
            let mut found: Option<Package> = None;
            for repo in repos.iter_mut() {
                if let Some(p) = repo
                    .load_packages(io, &[(needle.clone(), Some(parsed.clone()))])?
                    .into_iter()
                    .next()
                {
                    found = Some(p);
                    break;
                }
            }
            if let Some(p) = found {
                installed.push(p);
            } else if is_platform_package(&needle) {
                let lower = parsed.lower_bound();
                if !lower.is_zero() {
                    let version = lower.version.clone();
                    installed.push(Package::new(&needle, &version, &version));
                }
            } else {
                io.error(&format!(
                    "Package \"{needle}\" could not be found with constraint \"{text_constraint}\", results below will most likely be incomplete."
                ));
            }
        }
        Some(m) if is_platform_package(&needle) => {
            let extra = if m.extra.get("config.platform") == Some(&crate::json::Value::Bool(true)) {
                " (version provided by config.platform)"
            } else {
                ""
            };
            io.error(&format!(
                "Package \"{needle} {text_constraint}\" found in version \"{}\"{extra}.",
                m.pretty_version
            ));
        }
        Some(m) if inverted => {
            io.write(&format!(
                "Package \"{needle}\" {} is already installed! To find out why, run `composer why {needle}`",
                m.pretty_version
            ));
            return Ok(0);
        }
        _ => {}
    }
    let mut needles = vec![needle.clone()];
    if inverted {
        for p in &packages {
            needles.extend(p.replaces.iter().map(|l| l.target.clone()));
        }
    }
    let constraint = if text_constraint != "*" {
        Some(parsed)
    } else {
        None
    };
    let recursive = args.tree || args.recursive;
    let mut code = if inverted { 1 } else { 0 };
    let results = dependents(
        &installed,
        &root.package.name,
        &needles,
        constraint.as_ref(),
        inverted,
        recursive,
        None,
    );
    if results.is_empty() {
        let extra = match constraint {
            Some(_) => format!(
                " in versions {}matching {text_constraint}",
                if inverted { "not " } else { "" }
            ),
            None => String::new(),
        };
        io.error(&format!(
            "There is no installed package depending on \"{needle}\"{extra}"
        ));
        code = if inverted { 0 } else { 1 };
    } else if args.tree {
        let top = &packages[0];
        io.write(&format!(
            "{} {} {}",
            top.pretty_name,
            top.pretty_version,
            top.description.as_deref().unwrap_or("")
        ));
        print_tree(io, &results, "");
    } else {
        print_table(io, results);
    }
    if inverted && args.version.is_some() && !is_platform_package(&needle) {
        let mut command = "update";
        if root.package.requires.iter().any(|l| l.target == needle) {
            command = "require";
        }
        if root.package.dev_requires.iter().any(|l| l.target == needle) {
            command = "require --dev";
        }
        io.error(&format!(
            "Not finding what you were looking for? Try calling `composer {command} \"{needle}:{text_constraint}\" --dry-run` to get another view on the problem."
        ));
    }
    Ok(code)
}

/// `printTable`: bottom-up, deduplicated by link.
fn print_table(io: &mut Io, mut results: Vec<Dependent>) {
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut doubles: Vec<String> = Vec::new();
    loop {
        let mut queue = Vec::new();
        let mut rows = Vec::new();
        for result in results {
            let unique = link_string(&result.link);
            if doubles.contains(&unique) {
                continue;
            }
            doubles.push(unique);
            let version = if result.package.pretty_version == DEFAULT_PRETTY_VERSION {
                "-".to_string()
            } else {
                result.package.pretty_version.clone()
            };
            rows.push(vec![
                result.package.pretty_name.clone(),
                version,
                result
                    .description
                    .unwrap_or(result.link.kind.description())
                    .to_string(),
                format!("{} ({})", result.link.target, result.link.pretty_constraint),
            ]);
            if let Some(children) = result.children {
                queue.extend(children);
            }
        }
        results = queue;
        rows.extend(table);
        table = rows;
        if results.is_empty() {
            break;
        }
    }
    for line in table::compact(None, &table) {
        io.write(&line);
    }
}

/// `printTree`
fn print_tree(io: &mut Io, results: &[Dependent], prefix: &str) {
    let count = results.len();
    for (idx, result) in results.iter().enumerate() {
        let is_last = idx + 1 == count;
        let version = if result.package.pretty_version == DEFAULT_PRETTY_VERSION {
            String::new()
        } else {
            result.package.pretty_version.clone()
        };
        let package_text = format!("{} {version}", result.package.pretty_name)
            .trim_end()
            .to_string();
        let link_text = format!(
            "{} {} {}",
            result.description.unwrap_or(result.link.kind.description()),
            result.link.target,
            result.link.pretty_constraint
        );
        let circular = if result.children.is_none() {
            "(circular dependency aborted here)"
        } else {
            ""
        };
        let line = format!(
            "{prefix}{}{package_text} ({link_text}) {circular}",
            if is_last { "└──" } else { "├──" }
        )
        .trim_end()
        .to_string();
        io.write(&table::plain_tree_line(&line));
        if let Some(children) = &result.children {
            print_tree(
                io,
                children,
                &format!("{prefix}{}", if is_last { "   " } else { "│  " }),
            );
        }
    }
}
