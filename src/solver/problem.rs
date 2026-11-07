//! `Problem`, `Rule::getPrettyString` and `SolverProblemsException::getPrettyString`.
//!
//! ponytail: the dependency-policy (cooldown, advisories, filter list) explanations
//! of 2.11 are not ported; those pool filters do not exist here yet (ticket 13).

use super::builder::{Repository, RepositorySet};
use super::rules::{Reason, Rule, RuleSet};
use super::sat::SolverProblems;
use super::{Pool, Request, names, pretty_string, to_string};
use crate::io::Io;
use crate::package::{Package, is_platform_package};
use crate::semver::{Constraint, version_compare, version_parser};
use crate::zend_sort::usort;
use std::collections::{BTreeMap, HashSet};

#[derive(Default)]
pub struct Problem {
    seen: HashSet<usize>,
    /// sections of rules; a rule from the rule set carries its id
    sections: Vec<Vec<(Option<usize>, Rule)>>,
    section: usize,
}

impl Problem {
    pub fn add_rule(&mut self, id: usize, rules: &RuleSet) {
        if self.seen.insert(id) {
            self.push(Some(id), rules.rules[id].clone());
        }
    }

    pub fn add_rule_detached(&mut self, rule: Rule) {
        self.push(None, rule);
    }

    fn push(&mut self, id: Option<usize>, rule: Rule) {
        while self.sections.len() <= self.section {
            self.sections.push(Vec::new());
        }
        self.sections[self.section].push((id, rule));
    }

    pub fn next_section(&mut self) {
        self.section += 1;
    }

    pub fn rules(&self) -> Vec<&Rule> {
        self.sections.iter().flatten().map(|(_, r)| r).collect()
    }

    /// `Problem::getPrettyString`
    pub fn pretty_string(&self, ctx: &mut Ctx<'_, '_>) -> Result<String, String> {
        let mut reasons: Vec<&Rule> = self
            .sections
            .iter()
            .rev()
            .flatten()
            .map(|(_, r)| r)
            .collect();
        if reasons.len() == 1 {
            let rule = reasons[0];
            if let Reason::RootRequire(name, constraint, pretty) = &rule.reason
                && ctx.pool.what_provides(name, Some(constraint)).is_empty()
            {
                let (a, b) = missing_package_reason(ctx, name, Some((constraint, pretty)))?;
                return Ok(format!("\n    {a}{b}"));
            }
        }
        let priority = |rule: &Rule| -> i32 {
            match rule.reason {
                Reason::Fixed(_) => 3,
                Reason::RootRequire(..) => 2,
                Reason::PackageConflict(_) | Reason::PackageRequires(_) => 1,
                _ => 0,
            }
        };
        let pool = ctx.pool;
        usort(&mut reasons, |a, b| {
            let (pa, pb) = (priority(a), priority(b));
            if pa != pb {
                return pb.cmp(&pa);
            }
            php_compare(&sortable_string(pool, a), &sortable_string(pool, b))
        });
        format_deduplicated_rules(&reasons, "    ", ctx)
    }

    pub fn is_caused_by_lock(&self, ctx: &Ctx<'_, '_>) -> bool {
        self.rules().iter().any(|r| rule_is_caused_by_lock(r, ctx))
    }
}

/// PHP's `<=>` on two strings: numeric strings compare as numbers.
fn php_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let numeric = |s: &str| -> Option<f64> {
        let t = s.trim_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
        if t.is_empty()
            || !t
                .bytes()
                .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+' | b'e' | b'E'))
        {
            return None;
        }
        t.parse::<f64>().ok()
    };
    match (numeric(a), numeric(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.cmp(b),
    }
}

/// Everything the messages need to look at.
pub struct Ctx<'s, 'a> {
    pub io: &'s mut Io,
    pub set: &'s mut RepositorySet<'a>,
    pub request: &'s Request,
    pub pool: &'s Pool,
    pub verbose: bool,
    pub installed: HashSet<usize>,
    pub learned_pool: &'s [Vec<usize>],
    /// loaded PHP extensions, lowercased
    pub extensions: HashSet<String>,
}

fn sortable_string(pool: &Pool, rule: &Rule) -> String {
    match &rule.reason {
        Reason::RootRequire(name, ..) => name.clone(),
        Reason::Fixed(i) => to_string(&pool.packages[*i]),
        Reason::PackageConflict(link) | Reason::PackageRequires(link) => {
            let source = source_package(pool, rule);
            format!(
                "{}//{}",
                to_string(source),
                link_pretty_string(link, source)
            )
        }
        Reason::PackageSameName(name) => name.clone(),
        Reason::PackageAlias(i) | Reason::PackageInverseAlias(i) => to_string(&pool.packages[*i]),
        Reason::Learned(_) => rule
            .literals
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("-"),
    }
}

/// `Link::getPrettyString`
fn link_pretty_string(link: &crate::package::Link, source: &Package) -> String {
    format!(
        "{} {} {} {}",
        pretty_string(source),
        link.kind.description(),
        link.target,
        link.pretty_constraint
    )
}

fn dedup_default_branch_alias(package: &Package) -> &Package {
    match &package.alias_of {
        Some(real) if package.pretty_version == "9999999-dev" => real,
        _ => package,
    }
}

/// `Rule::getSourcePackage`
fn source_package<'p>(pool: &'p Pool, rule: &Rule) -> &'p Package {
    match &rule.reason {
        Reason::PackageConflict(link) => {
            let p1 = dedup_default_branch_alias(pool.package(rule.literals[0]));
            let p2 = dedup_default_branch_alias(pool.package(rule.literals[1]));
            if link.source == p1.name { p1 } else { p2 }
        }
        _ => dedup_default_branch_alias(pool.package(rule.literals[0])),
    }
}

fn rule_is_caused_by_lock(rule: &Rule, ctx: &Ctx<'_, '_>) -> bool {
    let (target, constraint) = match &rule.reason {
        Reason::PackageRequires(link) => (link.target.clone(), link.constraint.clone()),
        Reason::RootRequire(name, constraint, _) => (name.clone(), constraint.clone()),
        _ => return false,
    };
    if is_platform_package(&target) {
        return false;
    }
    let Some((repo, locked)) = &ctx.request.locked_repo else {
        return false;
    };
    for package in locked {
        if package.name == target {
            if ctx.pool.is_unacceptable(package) {
                return true;
            }
            if !constraint.matches(&Constraint::new("==", &package.version)) {
                return true;
            }
            if matches!(rule.reason, Reason::PackageRequires(_))
                && !ctx.request.is_locked_package(*repo, package)
            {
                return true;
            }
            break;
        }
    }
    false
}

/// `Rule::getPrettyString`
pub fn rule_pretty_string(rule: &Rule, ctx: &mut Ctx<'_, '_>) -> Result<String, String> {
    let pool = ctx.pool;
    let literals = &rule.literals;
    Ok(match &rule.reason {
        Reason::RootRequire(name, constraint, pretty) => {
            let packages = pool.what_provides(name, Some(constraint));
            if packages.is_empty() {
                return Ok(format!(
                    "No package found to satisfy root composer.json require {name} {pretty}"
                ));
            }
            let non_alias: Vec<&Package> = packages
                .iter()
                .map(|l| pool.package(*l))
                .filter(|p| p.alias_of.is_none())
                .collect();
            if non_alias.len() == 1 {
                let package = non_alias[0];
                let index = Pool::index(
                    packages
                        .iter()
                        .copied()
                        .find(|l| std::ptr::eq(pool.package(*l), package))
                        .unwrap(),
                );
                if ctx.request.is_locked_package(pool.repo[index], package) {
                    return Ok(format!(
                        "{} is locked to version {} and an update of this package was not requested.",
                        package.pretty_name, package.pretty_version
                    ));
                }
            }
            let list: Vec<&Package> = packages.iter().map(|l| pool.package(*l)).collect();
            format!(
                "Root composer.json requires {name} {pretty} -> satisfiable by {}.",
                package_list(&list, ctx.verbose, Some(pool), Some(constraint), false)
            )
        }
        Reason::Fixed(index) => {
            let package = dedup_default_branch_alias(&pool.packages[*index]);
            if ctx.request.is_locked_package(pool.repo[*index], package) {
                format!(
                    "{} is locked to version {} and an update of this package was not requested.",
                    package.pretty_name, package.pretty_version
                )
            } else {
                format!(
                    "{} is present at version {} and cannot be modified by Composer",
                    package.pretty_name, package.pretty_version
                )
            }
        }
        Reason::PackageConflict(link) => {
            let mut p1 = dedup_default_branch_alias(pool.package(literals[0]));
            let mut p2 = dedup_default_branch_alias(pool.package(literals[1]));
            let mut target = pretty_string(p1);
            if link.source == p1.name {
                std::mem::swap(&mut p1, &mut p2);
                target = format!("{} {}", p1.pretty_name, link.pretty_constraint);
            }
            if link.target != p1.name {
                let found = p1
                    .provides
                    .iter()
                    .find(|l| l.target == link.target)
                    .map(|l| ("provides", l.pretty_constraint.clone()))
                    .or_else(|| {
                        p1.replaces
                            .iter()
                            .find(|l| l.target == link.target)
                            .map(|l| ("replaces", l.pretty_constraint.clone()))
                    });
                if let Some((kind, provided)) = found {
                    target = format!(
                        "{} {} ({} {kind} {} {provided})",
                        link.target,
                        link.pretty_constraint,
                        pretty_string(p1),
                        link.target
                    );
                }
            }
            format!("{} conflicts with {target}.", pretty_string(p2))
        }
        Reason::PackageRequires(link) => {
            let source = dedup_default_branch_alias(pool.package(literals[0]));
            let requires: Vec<&Package> = literals[1..].iter().map(|l| pool.package(*l)).collect();
            let text = link_pretty_string(link, source);
            if !requires.is_empty() {
                format!(
                    "{text} -> satisfiable by {}.",
                    package_list(
                        &requires,
                        ctx.verbose,
                        Some(pool),
                        Some(&link.constraint),
                        false
                    )
                )
            } else {
                let (_, reason) = missing_package_reason(
                    ctx,
                    &link.target,
                    Some((&link.constraint, &link.pretty_constraint)),
                )?;
                format!("{text} -> {reason}")
            }
        }
        Reason::PackageSameName(replaced) => {
            let mut package_names: Vec<String> = Vec::new();
            for l in literals {
                let name = &pool.package(*l).name;
                if !package_names.contains(name) {
                    package_names.push(name.clone());
                }
            }
            if package_names.len() > 1 {
                let reason = if !package_names.contains(replaced) {
                    format!(
                        "They {} replace {replaced} and thus cannot coexist.",
                        if literals.len() == 2 { "both" } else { "all" }
                    )
                } else {
                    let replacers: Vec<&String> =
                        package_names.iter().filter(|n| *n != replaced).collect();
                    let head = if replacers.len() == 1 {
                        format!("{} replaces ", replacers[0])
                    } else {
                        format!(
                            "[{}] replace ",
                            replacers
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    format!("{head}{replaced} and thus cannot coexist with it.")
                };
                let mut installed = Vec::new();
                let mut removable = Vec::new();
                for l in literals {
                    if ctx.installed.contains(&Pool::index(*l)) {
                        installed.push(pool.package(*l));
                    } else {
                        removable.push(pool.package(*l));
                    }
                }
                if !installed.is_empty() && !removable.is_empty() {
                    return Ok(format!(
                        "{} cannot be installed as that would require removing {}. {reason}",
                        package_list(&removable, ctx.verbose, Some(pool), None, true),
                        package_list(&installed, ctx.verbose, Some(pool), None, true)
                    ));
                }
                let all: Vec<&Package> = literals.iter().map(|l| pool.package(*l)).collect();
                format!(
                    "Only one of these can be installed: {}. {reason}",
                    package_list(&all, ctx.verbose, Some(pool), None, true)
                )
            } else {
                let all: Vec<&Package> = literals.iter().map(|l| pool.package(*l)).collect();
                format!(
                    "You can only install one version of a package, so only one of these can be installed: {}.",
                    package_list(&all, ctx.verbose, Some(pool), None, true)
                )
            }
        }
        Reason::Learned(_) => {
            let text = if literals.len() == 1 {
                pool.literal_to_pretty_string(literals[0], &ctx.installed)
            } else {
                let mut groups: Vec<(&str, Vec<&Package>)> = Vec::new();
                for l in literals {
                    let package = pool.package(*l);
                    let group = if ctx.installed.contains(&Pool::index(*l)) {
                        if *l > 0 { "keep" } else { "remove" }
                    } else if *l > 0 {
                        "install"
                    } else {
                        "don't install"
                    };
                    match groups.iter_mut().find(|(g, _)| *g == group) {
                        Some((_, list)) => list.push(dedup_default_branch_alias(package)),
                        None => groups.push((group, vec![dedup_default_branch_alias(package)])),
                    }
                }
                groups
                    .iter()
                    .map(|(group, packages)| {
                        format!(
                            "{group}{} {}",
                            if packages.len() > 1 { " one of" } else { "" },
                            package_list(packages, ctx.verbose, Some(pool), None, false)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | ")
            };
            format!("Conclusion: {text} (conflict analysis result)")
        }
        Reason::PackageAlias(_) => {
            let alias = pool.package(literals[0]);
            if alias.version == "9999999-dev" {
                return Ok(String::new());
            }
            let package = dedup_default_branch_alias(pool.package(literals[1]));
            format!(
                "{} is an alias of {} and thus requires it to be installed too.",
                pretty_string(alias),
                pretty_string(package)
            )
        }
        Reason::PackageInverseAlias(_) => {
            let alias = pool.package(literals[1]);
            if alias.version == "9999999-dev" {
                return Ok(String::new());
            }
            let package = dedup_default_branch_alias(pool.package(literals[0]));
            format!(
                "{} is an alias of {} and must be installed with it.",
                pretty_string(alias),
                pretty_string(package)
            )
        }
    })
}

/// `Problem::formatDeduplicatedRules`
pub fn format_deduplicated_rules(
    rules: &[&Rule],
    indent: &str,
    ctx: &mut Ctx<'_, '_>,
) -> Result<String, String> {
    let header =
        regex::Regex::new(r"^(?P<package>\S+) (?P<version>\S+) (?P<type>requires|conflicts)")
            .unwrap();
    let strip_head = regex::Regex::new(r"^\S+ \S+ ").unwrap();
    let singularize = regex::Regex::new(r"^(%s%s (?:require|conflict))s").unwrap();
    let mut messages: Vec<String> = Vec::new();
    // template => package => normalized version => pretty version
    let mut templates: Vec<(String, Templates)> = Vec::new();
    for rule in rules {
        let message = rule_pretty_string(rule, ctx)?;
        let dedup = matches!(
            rule.reason,
            Reason::PackageRequires(_) | Reason::PackageConflict(_)
        );
        if dedup && let Some(m) = header.captures(&message) {
            let escaped = message.replace('%', "%%");
            let template = strip_head.replace(&escaped, "%s%s ").into_owned();
            messages.push(template.clone());
            let normalized = version_parser::normalize(&m["version"], None)
                .unwrap_or_else(|_| m["version"].to_string());
            let entry = match templates.iter_mut().find(|(t, _)| *t == template) {
                Some(e) => e,
                None => {
                    templates.push((template.clone(), Vec::new()));
                    templates.last_mut().unwrap()
                }
            };
            let package_entry = match entry.1.iter_mut().find(|(p, _)| *p == m["package"]) {
                Some(e) => e,
                None => {
                    entry.1.push((m["package"].to_string(), BTreeMap::new()));
                    entry.1.last_mut().unwrap()
                }
            };
            package_entry.1.insert(normalized, m["version"].to_string());
            let source = source_package(ctx.pool, rule);
            let source_index = (source as *const Package as usize
                - ctx.pool.packages.as_ptr() as usize)
                / std::mem::size_of::<Package>();
            if let Some(removed) = ctx.pool.removed_versions_by_package.get(&source_index) {
                for (v, p) in removed {
                    package_entry.1.insert(v.clone(), p.clone());
                }
            }
        } else if !message.is_empty() {
            messages.push(message);
        }
    }
    let mut unique: Vec<String> = Vec::new();
    for m in messages {
        if !unique.contains(&m) {
            unique.push(m);
        }
    }
    let mut result = Vec::new();
    for message in unique {
        if let Some((_, packages)) = templates.iter().find(|(t, _)| *t == message) {
            for (package, versions) in packages {
                let mut list: Vec<(&String, &String)> = versions.iter().collect();
                list.sort_by(|a, b| version_compare(a.0, b.0));
                let versions: Vec<String> = if ctx.verbose {
                    list.iter().map(|(_, p)| (*p).clone()).collect()
                } else {
                    condense_version_list(
                        &list
                            .iter()
                            .map(|(n, p)| ((*n).clone(), (*p).clone()))
                            .collect::<Vec<_>>(),
                        1,
                        16,
                    )
                };
                let text = if versions.len() > 1 {
                    let singular = singularize.replace(&message, "$1").into_owned();
                    sprintf2(&singular, package, &format!("[{}]", versions.join(", ")))
                } else {
                    sprintf2(&message, package, &format!(" {}", versions[0]))
                };
                result.push(text);
            }
        } else {
            result.push(message);
        }
    }
    Ok(format!(
        "\n{indent}- {}",
        result.join(&format!("\n{indent}- "))
    ))
}

/// package => normalized version => pretty version
type Templates = Vec<(String, BTreeMap<String, String>)>;

/// `sprintf($template, $a, $b)` for the `%s%s` templates, with `%%` unescaped.
fn sprintf2(template: &str, a: &str, b: &str) -> String {
    template
        .replacen("%s%s", &format!("{a}{b}"), 1)
        .replace("%%", "%")
}

/// `Problem::getPackageList`
pub fn package_list(
    packages: &[&Package],
    verbose: bool,
    pool: Option<&Pool>,
    constraint: Option<&Constraint>,
    use_removed_group: bool,
) -> String {
    // name => (pretty name, normalized version => pretty version)
    let mut prepared: Vec<(String, String, BTreeMap<String, String>)> = Vec::new();
    let mut has_default_branch: HashSet<String> = HashSet::new();
    for package in packages {
        let entry = match prepared.iter_mut().find(|(n, _, _)| *n == package.name) {
            Some(e) => e,
            None => {
                prepared.push((
                    package.name.clone(),
                    package.pretty_name.clone(),
                    BTreeMap::new(),
                ));
                prepared.last_mut().unwrap()
            }
        };
        entry.1 = package.pretty_name.clone();
        let pretty = match &package.alias_of {
            Some(real) => format!(
                "{} (alias of {})",
                package.pretty_version, real.pretty_version
            ),
            None => package.pretty_version.clone(),
        };
        entry.2.insert(package.version.clone(), pretty);
        if let (Some(pool), Some(constraint)) = (pool, constraint) {
            for (v, p) in pool.removed_versions_for(&package.name, constraint) {
                entry.2.insert(v, p);
            }
        }
        if let Some(pool) = pool
            && use_removed_group
        {
            let index = (0..pool.len()).find(|&i| std::ptr::eq(&pool.packages[i], *package));
            if let Some(index) = index
                && let Some(removed) = pool.removed_versions_by_package.get(&index)
            {
                for (v, p) in removed {
                    entry.2.insert(v.clone(), p.clone());
                }
            }
        }
        if package.default_branch {
            has_default_branch.insert(package.name.clone());
        }
    }
    let mut strings = Vec::new();
    for (name, pretty_name, mut versions) in prepared {
        if has_default_branch.contains(&name) {
            versions.remove("9999999-dev");
        }
        let mut list: Vec<(String, String)> = versions.into_iter().collect();
        list.sort_by(|a, b| version_compare(&a.0, &b.0));
        let shown: Vec<String> = if verbose {
            list.into_iter().map(|(_, p)| p).collect()
        } else {
            condense_version_list(&list, 4, 16)
        };
        strings.push(format!("{pretty_name}[{}]", shown.join(", ")));
    }
    strings.join(", ")
}

/// `Problem::condenseVersionList`
fn condense_version_list(versions: &[(String, String)], max: usize, max_dev: usize) -> Vec<String> {
    if versions.len() <= max {
        return versions.iter().map(|(_, p)| p.clone()).collect();
    }
    let major = regex::Regex::new(r"^(\d+)\..*").unwrap();
    let mut by_major: Vec<(String, Vec<String>)> = Vec::new();
    for (version, pretty) in versions {
        let key = if version.to_lowercase().starts_with("dev-") {
            "dev".to_string()
        } else {
            major.replace(version, "$1").into_owned()
        };
        match by_major.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(pretty.clone()),
            None => by_major.push((key, vec![pretty.clone()])),
        }
    }
    let mut filtered = Vec::new();
    for (key, list) in by_major {
        let limit = if key == "dev" { max_dev } else { max };
        if list.len() > limit {
            filtered.push(list[0].clone());
            filtered.push("...".to_string());
            filtered.push(list[list.len() - 1].clone());
        } else {
            filtered.extend(list);
        }
    }
    filtered
}

fn has_multiple_names(packages: &[(usize, Package)]) -> bool {
    let mut name: Option<&str> = None;
    for (_, p) in packages {
        match name {
            None => name = Some(&p.name),
            Some(n) if n == p.name => {}
            _ => return true,
        }
    }
    false
}

/// `Problem::constraintToText`
fn constraint_to_text(constraint: Option<(&Constraint, &str)>) -> String {
    let Some((constraint, pretty)) = constraint else {
        return String::new();
    };
    if let Constraint::Single { op, version } = constraint
        && op.as_str() == "=="
        && !version.starts_with("dev-")
    {
        if !regex::Regex::new(r"^\d+(?:\.\d+)*$")
            .unwrap()
            .is_match(pretty)
        {
            return format!(" {pretty} (exact version match)");
        }
        let mut versions = vec![pretty.to_string()];
        let dots = pretty.matches('.').count();
        for _ in 0..(3usize.saturating_sub(dots)) {
            versions.push(format!("{}.0", versions.last().unwrap()));
        }
        let text = if versions.len() > 1 {
            format!(
                "{} or {}",
                versions[..versions.len() - 1].join(", "),
                versions.last().unwrap()
            )
        } else {
            versions[0].clone()
        };
        return format!(" {pretty} (exact version match: {text})");
    }
    format!(" {pretty}")
}

fn refs(packages: &[(usize, Package)]) -> Vec<&Package> {
    packages.iter().map(|(_, p)| p).collect()
}

/// `Problem::getMissingPackageReason`
pub fn missing_package_reason(
    ctx: &mut Ctx<'_, '_>,
    name: &str,
    constraint: Option<(&Constraint, &str)>,
) -> Result<(String, String), String> {
    let c = constraint.map(|(c, _)| c);
    if is_platform_package(name) {
        let lower = name.to_lowercase();
        if lower.starts_with("php") || lower == "hhvm" {
            let msg = format!(
                "- Root composer.json requires {name}{} but ",
                constraint_to_text(constraint)
            );
            if name == "hhvm" {
                return Ok((
                    msg,
                    "HHVM was not detected on this machine, make sure it is in your PATH.".into(),
                ));
            }
            return Ok(match platform_package_version(ctx, name) {
                None => (
                    msg,
                    format!(
                        "the {name} package is disabled by your platform config. Enable it again with \"composer config platform.{name} --unset\"."
                    ),
                ),
                Some(version) => (
                    msg,
                    format!("your {name} version ({version}) does not satisfy that requirement."),
                ),
            });
        }
        if lower.starts_with("ext-") {
            if name.contains(' ') {
                return Ok((
                    "- ".into(),
                    format!(
                        "PHP extension {name} should be required as {}.",
                        name.replace(' ', "-")
                    ),
                ));
            }
            let ext = &name[4..];
            let msg = format!(
                "- Root composer.json requires PHP extension {name}{} but ",
                constraint_to_text(constraint)
            );
            return Ok(match platform_package_version(ctx, name) {
                None if ctx.extensions.contains(&ext.to_lowercase()) => (
                    msg,
                    format!(
                        "the {name} package is disabled by your platform config. Enable it again with \"composer config platform.{name} --unset\"."
                    ),
                ),
                None => (
                    msg,
                    format!(
                        "it is missing from your system. Install or enable PHP's {ext} extension."
                    ),
                ),
                Some(version) => (
                    msg,
                    format!("it has the wrong version installed ({version})."),
                ),
            });
        }
        if lower.starts_with("lib-") {
            if lower == "lib-icu" {
                let error = if ctx.extensions.contains("intl") {
                    "it has the wrong version installed, try upgrading the intl extension."
                } else {
                    "it is missing from your system, make sure the intl extension is loaded."
                };
                return Ok((
                    format!(
                        "- Root composer.json requires linked library {name}{} but ",
                        constraint_to_text(constraint)
                    ),
                    error.into(),
                ));
            }
            return Ok((
                format!("- Root composer.json requires linked library {name}{} but ", constraint_to_text(constraint)),
                "it has the wrong version installed or is missing from your system, make sure to load the extension providing it.".into(),
            ));
        }
    }

    let mut locked_package: Option<Package> = None;
    for (_, package) in ctx.request.locked_packages() {
        if package.name == name {
            if ctx.pool.is_unacceptable(package) {
                return Ok((
                    "- ".into(),
                    format!(
                        "{} is fixed to {} (lock file version) by a partial update but that version is rejected by your minimum-stability. Make sure you list it as an argument for the update command.",
                        package.pretty_name, package.pretty_version
                    ),
                ));
            }
            locked_package = Some(package.clone());
            break;
        }
    }

    if let Some((Constraint::Single { op, .. }, pretty)) = constraint
        && op.as_str() == "=="
        && regex::Regex::new(r"^dev-.*#.*").unwrap().is_match(pretty)
    {
        let new_constraint = regex::Regex::new(r" +as +([^,\s|]+)$")
            .unwrap()
            .replace(pretty, "")
            .into_owned();
        let alt = Constraint::multi(
            vec![
                Constraint::new("==", &new_constraint),
                Constraint::new("==", &new_constraint.replace('#', "+")),
            ],
            false,
        );
        let packages = ctx.set.find_packages(ctx.io, name, Some(&alt), 0)?;
        if !packages.is_empty() {
            return Ok((
                format!(
                    "- Root composer.json requires {name}{}, ",
                    constraint_to_text(constraint)
                ),
                format!(
                    "found {}. The # character in branch names is replaced by a + character. Make sure to require it as \"{}\".",
                    package_list(&refs(&packages), ctx.verbose, Some(ctx.pool), c, false),
                    pretty.replace('#', "+")
                ),
            ));
        }
    }

    let packages = ctx.set.find_packages(ctx.io, name, c, 0)?;
    if !packages.is_empty() {
        let list = package_list(&refs(&packages), ctx.verbose, Some(ctx.pool), c, false);
        let multiple = has_multiple_names(&packages);
        if let Some((_, root, root_pretty)) =
            ctx.set.root_requires.iter().find(|(n, _, _)| n == name)
            && !packages
                .iter()
                .any(|(_, p)| root.matches(&Constraint::new("==", &p.version)))
        {
            return Ok((
                format!(
                    "- Root composer.json requires {name}{}, ",
                    constraint_to_text(constraint)
                ),
                format!(
                    "found {list} but {} with your root composer.json require ({root_pretty}).",
                    if multiple {
                        "these conflict"
                    } else {
                        "it conflicts"
                    }
                ),
            ));
        }
        for n in names(&packages[0].1, true) {
            if let Some((_, temp, temp_pretty)) = ctx
                .set
                .temporary_constraints
                .iter()
                .find(|(t, _, _)| *t == n)
                && !packages
                    .iter()
                    .any(|(_, p)| temp.matches(&Constraint::new("==", &p.version)))
            {
                return Ok((
                    format!(
                        "- Root composer.json requires {n}{}, ",
                        constraint_to_text(constraint)
                    ),
                    format!(
                        "found {list} but {} with your temporary update constraint ({n}:{temp_pretty}).",
                        if multiple {
                            "these conflict"
                        } else {
                            "it conflicts"
                        }
                    ),
                ));
            }
        }
        if let Some(locked) = &locked_package
            && !packages.iter().any(|(_, p)| p.version == locked.version)
        {
            return Ok((
                format!(
                    "- Root composer.json requires {name}{}, ",
                    constraint_to_text(constraint)
                ),
                format!(
                    "found {list} but the package is fixed to {} (lock file version) by a partial update and that version does not match. Make sure you list it as an argument for the update command.",
                    locked.pretty_version
                ),
            ));
        }
        if packages
            .iter()
            .all(|(r, _)| Some(*r) == ctx.set.locked_repo)
        {
            return Ok((
                format!(
                    "- Root composer.json requires {name}{}, ",
                    constraint_to_text(constraint)
                ),
                format!(
                    "found {list} in the lock file but not in remote repositories, make sure you avoid updating this package to keep the one from the lock file."
                ),
            ));
        }
        return Ok((
            format!(
                "- Root composer.json requires {name}{}, ",
                constraint_to_text(constraint)
            ),
            format!(
                "found {list} but these were not loaded, likely because {} with another require.",
                if multiple {
                    "they conflict"
                } else {
                    "it conflicts"
                }
            ),
        ));
    }

    let packages = ctx.set.find_packages(
        ctx.io,
        name,
        c,
        RepositorySet::ALLOW_UNACCEPTABLE_STABILITIES,
    )?;
    if !packages.is_empty() {
        let all =
            ctx.set
                .find_packages(ctx.io, name, c, RepositorySet::ALLOW_SHADOWED_REPOSITORIES)?;
        if !all.is_empty() {
            return Ok(check_for_lower_prio_repo(
                ctx,
                name,
                &packages,
                &all,
                "minimum-stability",
                constraint,
            ));
        }
        let multiple = has_multiple_names(&packages);
        return Ok((
            format!(
                "- Root composer.json requires {name}{}, ",
                constraint_to_text(constraint)
            ),
            format!(
                "found {} but {} not match your minimum-stability.",
                package_list(&refs(&packages), ctx.verbose, Some(ctx.pool), c, false),
                if multiple { "these do" } else { "it does" }
            ),
        ));
    }

    let packages = ctx.set.find_packages(
        ctx.io,
        name,
        None,
        RepositorySet::ALLOW_UNACCEPTABLE_STABILITIES,
    )?;
    if !packages.is_empty() {
        let all =
            ctx.set
                .find_packages(ctx.io, name, c, RepositorySet::ALLOW_SHADOWED_REPOSITORIES)?;
        if !all.is_empty() {
            return Ok(check_for_lower_prio_repo(
                ctx,
                name,
                &packages,
                &all,
                "constraint",
                constraint,
            ));
        }
        let mut suffix = String::new();
        if let Some((Constraint::Single { version, .. }, _)) = constraint
            && version == "dev-master"
        {
            for (_, candidate) in &packages {
                if candidate.version == "dev-default" || candidate.version == "dev-main" {
                    suffix = format!(
                        " Perhaps dev-master was renamed to {}?",
                        candidate.pretty_version
                    );
                    break;
                }
            }
        }
        if matches!(ctx.set.repos[packages[0].0], Repository::Root(_)) {
            suffix = " See https://getcomposer.org/dep-on-root for details and assistance.".into();
        }
        let multiple = has_multiple_names(&packages);
        return Ok((
            format!(
                "- Root composer.json requires {name}{}, ",
                constraint_to_text(constraint)
            ),
            format!(
                "found {} but {} not match the constraint.{suffix}",
                package_list(&refs(&packages), ctx.verbose, Some(ctx.pool), c, false),
                if multiple { "these do" } else { "it does" }
            ),
        ));
    }

    if let Some(providers) = providers_list(ctx, name, 15) {
        return Ok((
            format!(
                "- Root composer.json requires {name}{}, it ",
                constraint_to_text(constraint)
            ),
            format!(
                "could not be found in any version, but the following packages provide it:\n{providers}      Consider requiring one of these to satisfy the {name} requirement."
            ),
        ));
    }
    if !regex::Regex::new(r"^[A-Za-z0-9_./-]+$")
        .unwrap()
        .is_match(name)
    {
        let illegal = regex::Regex::new(r"[A-Za-z0-9_./-]+")
            .unwrap()
            .replace_all(name, "")
            .into_owned();
        return Ok((
            format!("- Root composer.json requires {name}, it "),
            format!(
                "could not be found, it looks like its name is invalid, \"{illegal}\" is not allowed in package names."
            ),
        ));
    }
    Ok((
        format!("- Root composer.json requires {name}, it "),
        "could not be found in any version, there may be a typo in the package name.".into(),
    ))
}

/// `Problem::getProvidersList`
fn providers_list(ctx: &Ctx<'_, '_>, name: &str, max: usize) -> Option<String> {
    let mut providers: Vec<(String, Option<String>)> = Vec::new();
    for repo in &ctx.set.repos {
        for candidate in repo.packages() {
            if providers.iter().any(|(n, _)| *n == candidate.name) {
                continue;
            }
            if candidate.provides.iter().any(|l| l.target == name) {
                providers.push((candidate.name.clone(), candidate.description.clone()));
            }
        }
    }
    if providers.is_empty() {
        return None;
    }
    let shown = if providers.len() > max + 1 {
        &providers[..max]
    } else {
        &providers[..]
    };
    let mut text = String::new();
    for (n, description) in shown {
        let description = description
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(|d| format!(" {}", d.chars().take(100).collect::<String>()))
            .unwrap_or_default();
        text.push_str(&format!("      - {n}{description}\n"));
    }
    if providers.len() > max + 1 {
        text.push_str(&format!("      ... and {} more.\n", providers.len() - max));
    }
    Some(text)
}

/// `Problem::getPlatformPackageVersion`: `None` when the package is disabled.
fn platform_package_version(ctx: &Ctx<'_, '_>, name: &str) -> Option<String> {
    let pool = ctx.pool;
    let available = pool.what_provides(name, None);
    let selected = available
        .iter()
        .map(|l| Pool::index(*l))
        .find(|&i| matches!(ctx.set.repos[pool.repo[i]], Repository::Platform(_)))
        .or_else(|| available.first().map(|l| Pool::index(*l)))?;
    let package = &pool.packages[selected];
    if package.name != name {
        for link in package.provides.iter().chain(package.replaces.iter()) {
            if link.target == name {
                let description = link.kind.description();
                return Some(format!(
                    "{} {}d by {}",
                    link.pretty_constraint,
                    &description[..description.len() - 1],
                    pretty_string(package)
                ));
            }
        }
    }
    let mut version = package.pretty_version.clone();
    if package.extra.get("config.platform") == Some(&crate::json::Value::Bool(true)) {
        version.push_str(&format!(
            "; {}",
            package
                .description
                .as_deref()
                .unwrap_or("")
                .replace("Package ", "")
        ));
    }
    Some(version)
}

fn check_for_lower_prio_repo(
    ctx: &mut Ctx<'_, '_>,
    name: &str,
    higher: &[(usize, Package)],
    all: &[(usize, Package)],
    reason: &str,
    constraint: Option<(&Constraint, &str)>,
) -> (String, String) {
    let c = constraint.map(|(c, _)| c);
    let next_repo = all[0].0;
    let next_packages: Vec<&Package> = all
        .iter()
        .take_while(|(r, _)| *r == next_repo)
        .map(|(_, p)| p)
        .collect();
    let next_name = ctx.set.repos[next_repo].name();
    if let Some((repo, top)) = higher.first()
        && matches!(ctx.set.repos[*repo], Repository::Root(_))
    {
        return (
            format!(
                "- Root composer.json requires {name}{}, it is ",
                constraint_to_text(constraint)
            ),
            format!(
                "satisfiable by {} from {next_name} but {} {} is the root package and cannot be modified. See https://getcomposer.org/dep-on-root for details and assistance.",
                package_list(&next_packages, ctx.verbose, Some(ctx.pool), c, false),
                top.pretty_name,
                top.pretty_version
            ),
        );
    }
    if Some(next_repo) == ctx.set.locked_repo {
        let singular = higher.len() == 1;
        let mut suggestion = format!(
            "Make sure you either fix the {reason} or avoid updating this package to keep the one present in the lock file ({}).",
            package_list(&next_packages, ctx.verbose, Some(ctx.pool), c, false)
        );
        if next_packages[0]
            .dist
            .as_ref()
            .is_some_and(|d| d.kind == "path")
        {
            let symlink_off = next_packages[0]
                .transport_options
                .as_ref()
                .and_then(|t| t.get("symlink"))
                == Some(&crate::json::Value::Bool(false));
            if !symlink_off {
                suggestion = format!(
                    "Make sure you fix the {reason} as packages installed from symlinked path repos are updated even in partial updates and the one from the lock file can thus not be used."
                );
            }
        }
        return (
            format!(
                "- Root composer.json requires {name}{}, ",
                constraint_to_text(constraint)
            ),
            format!(
                "found {} but {} not match your {reason} and {} therefore not installable. {suggestion}",
                package_list(&refs(higher), ctx.verbose, Some(ctx.pool), c, false),
                if singular { "it does" } else { "these do" },
                if singular { "is" } else { "are" }
            ),
        );
    }
    (
        format!(
            "- Root composer.json requires {name}{}, it is ",
            constraint_to_text(constraint)
        ),
        format!(
            "satisfiable by {} from {next_name} but {} from {} has higher repository priority. The packages from the higher priority repository do not match your {reason} and are therefore not installable. That repository is canonical so the lower priority repo's packages are not installable. See https://getcomposer.org/repoprio for details and assistance.",
            package_list(&next_packages, ctx.verbose, Some(ctx.pool), c, false),
            package_list(&refs(higher), ctx.verbose, Some(ctx.pool), c, false),
            ctx.set.repos[higher[0].0].name()
        ),
    )
}

/// `SolverProblemsException::getPrettyString`
pub fn problems_pretty_string(
    problems: &SolverProblems,
    ctx: &mut Ctx<'_, '_>,
    is_dev_extraction: bool,
    ini_paths: &[String],
) -> Result<String, String> {
    let mut texts: Vec<String> = Vec::new();
    let mut missing_extensions: Vec<String> = Vec::new();
    let mut caused_by_lock = false;
    for problem in &problems.problems {
        let text = format!("{}\n", problem.pretty_string(ctx)?);
        if !texts.contains(&text) {
            texts.push(text);
        }
        for rule in problem.rules() {
            if let Some(required) = rule.required_package(ctx.pool)
                && required.starts_with("ext-")
                && !missing_extensions.contains(&required)
            {
                missing_extensions.push(required);
            }
        }
        caused_by_lock = caused_by_lock || problem.is_caused_by_lock(ctx);
    }
    let mut text = String::from("\n");
    for (i, problem) in texts.iter().enumerate() {
        text.push_str(&format!("  Problem {}{problem}", i + 1));
    }
    let mut hints = Vec::new();
    if !is_dev_extraction
        && (text.contains("could not be found") || text.contains("no matching package found"))
    {
        hints.push("Potential causes:\n - A typo in the package name\n - The package is not available in a stable-enough version according to your minimum-stability setting\n   see <https://getcomposer.org/doc/04-schema.md#minimum-stability-root-only> for more details.\n - It's a private package and you forgot to add a custom repository to find it\n\nRead <https://getcomposer.org/doc/articles/troubleshooting.md> for further common problems.".to_string());
    }
    if !missing_extensions.is_empty() {
        let mut paths = ini_paths.to_vec();
        if paths.first().is_some_and(|p| p.is_empty()) {
            paths.remove(0);
        }
        if !paths.is_empty() || !ini_paths.is_empty() {
            let ignore: Vec<String> = missing_extensions
                .iter()
                .map(|e| format!("--ignore-platform-req={e}"))
                .collect();
            let mut hint = format!(
                "To enable extensions, verify that they are enabled in your .ini files:\n    - {}",
                paths.join("\n    - ")
            );
            hint.push_str("\nYou can also run `php --ini` in a terminal to see which files are used by PHP in CLI mode.");
            hint.push_str(&format!(
                "\nAlternatively, you can run Composer with `{}` to temporarily ignore these required extensions.",
                ignore.join(" ")
            ));
            hints.push(hint);
        }
    }
    if caused_by_lock
        && !is_dev_extraction
        && !ctx.request.update_allow_transitive_root_dependencies()
    {
        hints.push("Use the option --with-all-dependencies (-W) to allow upgrades, downgrades and removals for packages currently locked to specific versions.".to_string());
    }
    if text.contains("found composer-plugin-api[2.0.0] but it does not match")
        && text.contains("- ocramius/package-versions")
    {
        hints.push("<warning>ocramius/package-versions only provides support for Composer 2 in 1.8+, which requires PHP 7.4.</warning>\nIf you can not upgrade PHP you can require <info>composer/package-versions-deprecated</info> to resolve this with PHP 7.0+.".to_string());
    }
    if !hints.is_empty() {
        text.push('\n');
        text.push_str(&hints.join("\n\n"));
    }
    Ok(text)
}
