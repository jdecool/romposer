//! `DefaultPolicy`: which of several candidate literals to install first.

use super::{Literal, Pool};
use crate::package::Package;
use crate::semver::{Constraint, Stability};
use crate::zend_sort::usort;
use std::cmp::Ordering;
use std::collections::HashMap;

pub struct DefaultPolicy {
    pub prefer_stable: bool,
    pub prefer_lowest: bool,
    prefer_dev_over_prerelease: bool,
    /// name => normalized version (`--minimal-changes`)
    pub preferred_versions: Option<HashMap<String, String>>,
}

impl DefaultPolicy {
    pub fn new(
        prefer_stable: bool,
        prefer_lowest: bool,
        preferred_versions: Option<HashMap<String, String>>,
    ) -> DefaultPolicy {
        DefaultPolicy {
            prefer_stable,
            prefer_lowest,
            prefer_dev_over_prerelease: std::env::var("COMPOSER_PREFER_DEV_OVER_PRERELEASE")
                .is_ok_and(|v| !v.is_empty() && v != "0"),
            preferred_versions,
        }
    }

    /// `versionCompare($a, $b, $operator)`
    pub fn version_compare(&self, a: &Package, b: &Package, operator: &str) -> bool {
        let (mut stab_a, mut stab_b) = (a.stability(), b.stability());
        if self.prefer_stable && stab_a != stab_b {
            if self.prefer_lowest
                && self.prefer_dev_over_prerelease
                && stab_a != Stability::Stable
                && stab_b != Stability::Stable
            {
                if stab_a == Stability::Dev {
                    stab_a = Stability::Stable;
                }
                if stab_b == Stability::Dev {
                    stab_b = Stability::Stable;
                }
            }
            return stab_a.priority() < stab_b.priority();
        }
        let constraint = Constraint::new(operator, &b.version);
        let version = Constraint::new("==", &a.version);
        if (a.is_dev() && a.version.starts_with("dev-"))
            || (b.is_dev() && b.version.starts_with("dev-"))
        {
            return constraint.match_specific(&version, true);
        }
        constraint.matches(&version)
    }

    /// `selectPreferredPackages`
    pub fn select_preferred_packages(
        &self,
        pool: &Pool,
        mut literals: Vec<Literal>,
        required_package: Option<&str>,
    ) -> Vec<Literal> {
        literals.sort_unstable();
        // group by name, keeping first-seen name order
        let mut groups: Vec<(String, Vec<Literal>)> = Vec::new();
        for &literal in &literals {
            let name = &pool.package(literal).name;
            match groups.iter_mut().find(|(n, _)| n == name) {
                Some((_, list)) => list.push(literal),
                None => groups.push((name.clone(), vec![literal])),
            }
        }
        let mut selected = Vec::new();
        for (_, mut group) in groups {
            usort(&mut group, |a, b| {
                self.compare_by_priority(pool, *a, *b, required_package, true)
            });
            let group = self.prune_to_best_version(pool, group);
            let group = Self::prune_remote_aliases(pool, group);
            selected.extend(group);
        }
        usort(&mut selected, |a, b| {
            self.compare_by_priority(pool, *a, *b, required_package, false)
        });
        selected
    }

    pub fn compare_by_priority(
        &self,
        pool: &Pool,
        la: Literal,
        lb: Literal,
        required_package: Option<&str>,
        ignore_replace: bool,
    ) -> Ordering {
        let (a, b) = (pool.package(la), pool.package(lb));
        if a.name == b.name {
            let (a_alias, b_alias) = (a.alias_of.is_some(), b.alias_of.is_some());
            if a_alias && !b_alias {
                return Ordering::Less;
            }
            if !a_alias && b_alias {
                return Ordering::Greater;
            }
        }
        if !ignore_replace {
            if Self::replaces(a, b) {
                return Ordering::Greater;
            }
            if Self::replaces(b, a) {
                return Ordering::Less;
            }
            if let Some(required) = required_package
                && let Some(pos) = required.find('/')
            {
                let vendor = &required[..pos];
                let a_same = a.name.starts_with(vendor);
                let b_same = b.name.starts_with(vendor);
                if a_same != b_same {
                    return if a_same {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    };
                }
            }
        }
        Pool::index(la).cmp(&Pool::index(lb))
    }

    fn replaces(source: &Package, target: &Package) -> bool {
        source.replaces.iter().any(|l| l.target == target.name)
    }

    fn prune_to_best_version(&self, pool: &Pool, literals: Vec<Literal>) -> Vec<Literal> {
        if let Some(preferred) = &self.preferred_versions {
            let name = &pool.package(literals[0]).name;
            if let Some(version) = preferred.get(name) {
                let best: Vec<Literal> = literals
                    .iter()
                    .copied()
                    .filter(|l| pool.package(*l).version == *version)
                    .collect();
                if !best.is_empty() {
                    return best;
                }
            }
        }
        let operator = if self.prefer_lowest { "<" } else { ">" };
        let mut best = vec![literals[0]];
        let mut best_package = pool.package(literals[0]);
        for &literal in &literals[1..] {
            let package = pool.package(literal);
            if self.version_compare(package, best_package, operator) {
                best_package = package;
                best = vec![literal];
            } else if self.version_compare(package, best_package, "==") {
                best.push(literal);
            }
        }
        best
    }

    fn prune_remote_aliases(pool: &Pool, literals: Vec<Literal>) -> Vec<Literal> {
        let is_root_alias = |l: &Literal| {
            let p = pool.package(*l);
            p.alias_of.is_some() && p.root_alias
        };
        if !literals.iter().any(is_root_alias) {
            return literals;
        }
        literals.into_iter().filter(is_root_alias).collect()
    }
}
