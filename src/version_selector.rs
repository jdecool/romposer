//! `VersionSelector` and `PackageDiscoveryTrait::findBestVersionAndNameForPackage`:
//! which version `require` picks for a package given without a constraint, and the
//! `^x.y` constraint recommended for it.
//!
//! ponytail: the cooldown policy filter (ticket 13) and the interactive "did you
//! mean" selection are not ported.

use crate::io::Io;
use crate::package::{Package, is_platform_package};
use crate::platform::{PlatformRepository, RequirementFilter};
use crate::semver::{Constraint, Stability, parse_stability, version_compare, version_parser};
use crate::solver::builder::RepositorySet;
use crate::zend_sort::usort;
use std::collections::{HashMap, HashSet};

pub struct VersionSelector<'s, 'a> {
    pub set: &'s mut RepositorySet<'a>,
    /// name => `==` constraints of the platform packages
    platform: HashMap<String, Vec<Constraint>>,
}

impl<'s, 'a> VersionSelector<'s, 'a> {
    pub fn new(
        set: &'s mut RepositorySet<'a>,
        platform: Option<&PlatformRepository>,
    ) -> VersionSelector<'s, 'a> {
        let mut map: HashMap<String, Vec<Constraint>> = HashMap::new();
        if let Some(platform) = platform {
            for p in &platform.packages {
                map.entry(p.name.clone())
                    .or_default()
                    .push(Constraint::new("==", &p.version));
            }
        }
        VersionSelector { set, platform: map }
    }

    /// `findBestCandidate`: (repository index, package), warnings written to `io`.
    #[allow(clippy::too_many_arguments)]
    pub fn find_best_candidate(
        &mut self,
        io: &mut Io,
        name: &str,
        constraint: Option<&str>,
        preferred_stability: Stability,
        filter: &RequirementFilter,
        flags: u8,
        warn: bool,
    ) -> Result<Option<(usize, Package)>, String> {
        let constraint = match constraint {
            Some(c) => Some(version_parser::parse_constraints(c)?),
            None => None,
        };
        let mut candidates =
            self.set
                .find_packages(io, &name.to_lowercase(), constraint.as_ref(), flags)?;
        let min_priority = preferred_stability.priority();
        usort(&mut candidates, |(_, a), (_, b)| {
            let (pa, pb) = (a.stability().priority(), b.stability().priority());
            if min_priority < pa && pb < pa {
                return std::cmp::Ordering::Greater;
            }
            if min_priority < pa && pa < pb {
                return std::cmp::Ordering::Less;
            }
            if min_priority >= pa && min_priority < pb {
                return std::cmp::Ordering::Less;
            }
            version_compare(&b.version, &a.version)
        });
        let mut package: Option<(usize, Package)> = None;
        if !self.platform.is_empty() && !matches!(filter, RequirementFilter::IgnoreAll) {
            let mut warned: HashSet<String> = HashSet::new();
            let mut seen: HashSet<String> = HashSet::new();
            'candidates: for (repo, pkg) in candidates {
                let mut skip = false;
                for link in &pkg.requires {
                    let target = &link.target;
                    if !is_platform_package(target) || filter.is_ignored(target) {
                        continue;
                    }
                    let reason = if let Some(provided) = self.platform.get(target) {
                        let satisfied = provided.iter().any(|p| {
                            link.constraint.matches(p)
                                || (filter.is_upper_bound_ignored(target)
                                    && filter
                                        .filter_constraint(target, &link.constraint)
                                        .matches(p))
                        });
                        if satisfied {
                            continue;
                        }
                        "is not satisfied by your platform"
                    } else {
                        "is missing from your platform"
                    };
                    let is_latest = !seen.contains(&pkg.name);
                    seen.insert(pkg.name.clone());
                    let first_warning = warned.insert(format!("{}/{target}", pkg.name));
                    if warn && (first_warning || io.verbosity >= 1) {
                        io.warning(&format!(
                            "Cannot use {}{} {} as it {} {target} {} which {reason}.",
                            pkg.pretty_name,
                            if is_latest { "'s latest version" } else { "" },
                            pkg.pretty_version,
                            link.kind.description(),
                            link.pretty_constraint
                        ));
                    }
                    skip = true;
                }
                if skip {
                    continue 'candidates;
                }
                package = Some((repo, pkg));
                break;
            }
        } else {
            package = candidates.into_iter().next();
        }
        Ok(package.map(|(repo, mut p)| {
            if p.alias_of.is_some() && p.version == "9999999-dev" {
                p = *p.alias_of.take().unwrap();
            }
            (repo, p)
        }))
    }

    /// `findRecommendedRequireVersion`
    pub fn recommended_require_version(package: &Package, php_version: Option<&str>) -> String {
        if package.name.starts_with("ext-")
            && let Some(php) = php_version
        {
            let ext: Vec<&str> = package.version.split('.').take(3).collect();
            if php == ext.join(".") {
                return "*".into();
            }
        }
        if !package.is_dev() {
            return transform_version(
                &package.version,
                &package.pretty_version,
                package.stability(),
            );
        }
        let dumped = crate::package::dumper::dump(package);
        if let Ok(Some(alias)) = crate::package::branch_alias(&dumped)
            && alias != "9999999-dev"
        {
            let re = regex::Regex::new(r"^(\d+\.\d+\.\d+)(\.9999999)-dev$").unwrap();
            if re.is_match(&alias) {
                let extra = re.replace(&alias, "${1}.0").replace(".9999999", ".0");
                return transform_version(&extra, &extra, Stability::Dev);
            }
        }
        package.pretty_version.clone()
    }
}

/// `VersionSelector::transformVersion`: `2.1.1.0` becomes `^2.1`, `0.1.2.0` becomes `^0.1.2`.
fn transform_version(version: &str, pretty: &str, stability: Stability) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 4 || !parts[3].chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return pretty.to_string();
    }
    let kept = if parts[0] == "0" {
        &parts[..3]
    } else {
        &parts[..2]
    };
    let mut out = kept.join(".");
    if stability != Stability::Stable {
        out.push('@');
        out.push_str(stability.as_str());
    }
    format!("^{out}")
}

/// `findBestVersionAndNameForPackage`: (pretty name, constraint) or Composer's explanation.
#[allow(clippy::too_many_arguments)]
pub fn find_best_version_and_name(
    io: &mut Io,
    set: &mut RepositorySet<'_>,
    platform: &PlatformRepository,
    name: &str,
    preferred_stability: Stability,
    minimum_stability: Stability,
    filter: &RequirementFilter,
    fixed: bool,
    php_version: Option<&str>,
    installed_names: &[String],
) -> Result<(String, String), String> {
    let mut selector = VersionSelector::new(set, Some(platform));
    if let Some((_, package)) =
        selector.find_best_candidate(io, name, None, preferred_stability, filter, 0, true)?
    {
        let version = if fixed {
            package.pretty_version.clone()
        } else {
            VersionSelector::recommended_require_version(&package, php_version)
        };
        return Ok((package.pretty_name, version));
    }
    if filter.is_ignored(name) {
        return Ok((name.to_string(), "*".into()));
    }
    let providers: Vec<String> = selector
        .set
        .repos
        .iter()
        .flat_map(|r| r.packages())
        .filter(|p| p.provides.iter().any(|l| l.target == name))
        .map(|p| p.name.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if !providers.is_empty() {
        return Ok((name.to_string(), "*".into()));
    }
    let mut quiet = Io::buffered();
    if !matches!(filter, RequirementFilter::IgnoreAll)
        && let Some((_, candidate)) = selector.find_best_candidate(
            &mut quiet,
            name,
            None,
            preferred_stability,
            &RequirementFilter::IgnoreAll,
            0,
            false,
        )?
    {
        return Err(format!(
            "Package {name} has requirements incompatible with your PHP version, PHP extensions and Composer version{}",
            platform_exception_details(&candidate, platform)
        ));
    }
    if let Some((repo, _)) = selector.find_best_candidate(
        &mut quiet,
        name,
        None,
        preferred_stability,
        filter,
        RepositorySet::ALLOW_UNACCEPTABLE_STABILITIES,
        false,
    )? {
        if let Some((all_repo, _)) = selector.find_best_candidate(
            &mut quiet,
            name,
            None,
            preferred_stability,
            filter,
            RepositorySet::ALLOW_SHADOWED_REPOSITORIES,
            false,
        )? {
            return Err(format!(
                "Package {name} exists in {} and {} which has a higher repository priority. The packages from the higher priority repository do not match your minimum-stability and are therefore not installable. That repository is canonical so the lower priority repo's packages are not installable. See https://getcomposer.org/repoprio for details and assistance.",
                selector.set.repos[all_repo].name(),
                selector.set.repos[repo].name()
            ));
        }
        return Err(format!(
            "Could not find a version of package {name} matching your minimum-stability ({}). Require it with an explicit version constraint allowing its desired stability.",
            minimum_stability.as_str()
        ));
    }
    if !matches!(filter, RequirementFilter::IgnoreAll)
        && let Some((_, candidate)) = selector.find_best_candidate(
            &mut quiet,
            name,
            None,
            preferred_stability,
            &RequirementFilter::IgnoreAll,
            RepositorySet::ALLOW_UNACCEPTABLE_STABILITIES,
            false,
        )?
    {
        let mut additional = String::new();
        if selector
            .find_best_candidate(
                &mut quiet,
                name,
                None,
                preferred_stability,
                &RequirementFilter::IgnoreAll,
                0,
                false,
            )?
            .is_none()
        {
            additional = format!(
                "\n\nAdditionally, the package was only found with a stability of \"{}\" while your minimum stability is \"{}\".",
                candidate.stability().as_str(),
                minimum_stability.as_str()
            );
        }
        return Err(format!(
            "Could not find package {name} in any version matching your PHP version, PHP extensions and Composer version{}{additional}",
            platform_exception_details(&candidate, platform)
        ));
    }
    // similar names via the repositories' search
    let mut similar: Vec<(String, usize)> = Vec::new();
    for repo in selector.set.repos.iter_mut() {
        if let crate::solver::builder::Repository::Composer(r) = repo
            && let Ok(results) =
                r.search(&mut quiet, name, crate::repository::SEARCH_FULLTEXT, None)
        {
            for result in results {
                if let Some(found) = result.get("name").and_then(|v| v.as_str())
                    && !installed_names.iter().any(|n| n == found)
                    && !similar.iter().any(|(n, _)| n == found)
                {
                    similar.push((found.to_string(), levenshtein(name, found)));
                }
            }
        }
    }
    similar.sort_by_key(|(_, d)| *d);
    let similar: Vec<String> = similar.into_iter().take(5).map(|(n, _)| n).collect();
    if !similar.is_empty() {
        if similar.iter().any(|n| n == name) {
            return Err(format!(
                "Could not find package {name}. It was however found via repository search, which indicates a consistency issue with the repository."
            ));
        }
        return Err(format!(
            "Could not find package {name}.\n\nDid you mean {}?\n    {}",
            if similar.len() > 1 {
                "one of these"
            } else {
                "this"
            },
            similar.join("\n    ")
        ));
    }
    Err(format!(
        "Could not find a matching version of package {name}. Check the package spelling, your version constraint and that the package is available in a stability which matches your minimum-stability ({}).",
        minimum_stability.as_str()
    ))
}

/// `getPlatformExceptionDetails`
fn platform_exception_details(candidate: &Package, platform: &PlatformRepository) -> String {
    let mut details = Vec::new();
    for link in &candidate.requires {
        if !is_platform_package(&link.target) {
            continue;
        }
        let Some(platform_pkg) = platform.find(&link.target) else {
            if platform.disabled.contains_key(&link.target) {
                details.push(format!(
                    "{} {} requires {} {} but it is disabled by your platform config. Enable it again with \"composer config platform.{} --unset\".",
                    candidate.pretty_name, candidate.pretty_version, link.target, link.pretty_constraint, link.target
                ));
            } else {
                details.push(format!(
                    "{} {} requires {} {} but it is not present.",
                    candidate.pretty_name,
                    candidate.pretty_version,
                    link.target,
                    link.pretty_constraint
                ));
            }
            continue;
        };
        if !link
            .constraint
            .matches(&Constraint::new("==", &platform_pkg.version))
        {
            let mut version = platform_pkg.pretty_version.clone();
            if platform_pkg.extra.contains_key("config.platform") {
                version.push_str(&format!(
                    " ({})",
                    platform_pkg.description.as_deref().unwrap_or("")
                ));
            }
            details.push(format!(
                "{} {} requires {} {} which does not match your installed version {version}.",
                candidate.pretty_name,
                candidate.pretty_version,
                link.target,
                link.pretty_constraint
            ));
        }
    }
    if details.is_empty() {
        return String::new();
    }
    format!(":\n  - {}", details.join("\n  - "))
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<u8>, Vec<u8>) = (a.bytes().collect(), b.bytes().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// `VersionParser::parseNameVersionPairs`: `foo/bar:^1`, `foo/bar ^1`, `foo/bar=^1`.
pub fn parse_name_version_pairs(pairs: &[String]) -> Vec<(String, Option<String>)> {
    let split = regex::Regex::new(r"^([^=: ]+)[=: ](.*)$").unwrap();
    let wildcard = fancy_regex::Regex::new(r"(?i)(?<=[a-z0-9_/-])\*|\*(?=[a-z0-9_/-])").unwrap();
    let mut result = Vec::new();
    let mut i = 0;
    while i < pairs.len() {
        let mut pair = split.replace(pairs[i].trim(), "$1 $2").into_owned();
        if !pair.contains(' ')
            && let Some(next) = pairs.get(i + 1)
            && !next.contains('/')
            && !wildcard.is_match(next).unwrap_or(false)
            && !is_platform_package(next)
        {
            pair = format!("{pair} {next}");
            i += 1;
        }
        match pair.split_once(' ') {
            Some((name, version)) if !name.is_empty() => {
                result.push((name.to_string(), Some(version.to_string())));
            }
            _ => result.push((pair, None)),
        }
        i += 1;
    }
    result
}

pub fn stability_of(version: &str) -> Stability {
    parse_stability(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommended_versions() {
        let p = |v: &str, pretty: &str| Package::new("a/b", v, pretty);
        assert_eq!(
            VersionSelector::recommended_require_version(&p("1.2.1.0", "1.2.1"), None),
            "^1.2"
        );
        assert_eq!(
            VersionSelector::recommended_require_version(&p("0.1.2.0", "0.1.2"), None),
            "^0.1.2"
        );
        assert_eq!(
            VersionSelector::recommended_require_version(&p("2.0.0.0-beta1", "2.0-beta.1"), None),
            "^2.0@beta"
        );
        assert_eq!(
            VersionSelector::recommended_require_version(&p("dev-main", "dev-main"), None),
            "dev-main"
        );
        assert_eq!(
            VersionSelector::recommended_require_version(&p("3.2.1.0", "v3.2.1"), None),
            "^3.2"
        );
        assert_eq!(
            parse_name_version_pairs(&["a/b:^1".into(), "c/d".into(), "^2".into(), "e/f".into()]),
            vec![
                ("a/b".into(), Some("^1".into())),
                ("c/d".into(), Some("^2".into())),
                ("e/f".into(), None)
            ]
        );
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
