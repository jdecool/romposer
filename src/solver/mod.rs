//! Port of Composer's `DependencyResolver`: the pool of candidate packages, the
//! solver request, rule generation, the SAT solver, the default policy and the
//! problem reporter. Packages live in the pool's vector; a literal is `±(index + 1)`.

pub mod builder;
pub mod lock_transaction;
pub mod policy;
pub mod problem;
pub mod rules;
pub mod sat;

use crate::package::Package;
use crate::semver::{Constraint, Stability};
use regex::Regex;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

pub type Literal = i32;

/// `StabilityFilter::isPackageAcceptable`
pub fn is_package_acceptable(
    acceptable: &[Stability],
    flags: &BTreeMap<String, u8>,
    names: &[String],
    stability: Stability,
) -> bool {
    for name in names {
        if let Some(flag) = flags.get(name) {
            if stability.priority() <= *flag {
                return true;
            }
        } else if acceptable.contains(&stability) {
            return true;
        }
    }
    false
}

/// `BasePackage::getNames`
pub fn names(package: &Package, provides: bool) -> Vec<String> {
    let mut out = vec![package.name.clone()];
    if provides {
        for link in &package.provides {
            if !out.contains(&link.target) {
                out.push(link.target.clone());
            }
        }
    }
    for link in &package.replaces {
        if !out.contains(&link.target) {
            out.push(link.target.clone());
        }
    }
    out
}

/// `$package->getPrettyString()`
pub fn pretty_string(package: &Package) -> String {
    format!("{} {}", package.pretty_name, package.pretty_version)
}

/// `(string) $package`: the unique name, with the alias suffix of `AliasPackage::__toString`.
pub fn to_string(package: &Package) -> String {
    match &package.alias_of {
        Some(real) => format!(
            "{} ({}alias of {})",
            package.unique_name(),
            if package.root_alias { "root " } else { "" },
            real.version
        ),
        None => package.unique_name(),
    }
}

/// Identity of a package before it lands in a pool: repository, name, version.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub repo: usize,
    pub name: String,
    pub version: String,
}

impl Key {
    pub fn of(repo: usize, package: &Package) -> Key {
        Key {
            repo,
            name: package.name.clone(),
            version: package.version.clone(),
        }
    }
}

pub struct Pool {
    pub packages: Vec<Package>,
    /// repository index of each package
    pub repo: Vec<usize>,
    by_name: HashMap<String, Vec<usize>>,
    cache: RefCell<HashMap<(String, String), Vec<Literal>>>,
    pub unacceptable: Vec<Package>,
    /// name => normalized version => pretty version, versions the optimizer dropped
    pub removed_versions: HashMap<String, BTreeMap<String, String>>,
    pub removed_versions_by_package: HashMap<usize, BTreeMap<String, String>>,
}

impl Pool {
    pub fn new(packages: Vec<(usize, Package)>, unacceptable: Vec<Package>) -> Pool {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut repo = Vec::new();
        let mut list = Vec::new();
        for (i, (r, package)) in packages.into_iter().enumerate() {
            for name in names(&package, true) {
                by_name.entry(name).or_default().push(i);
            }
            repo.push(r);
            list.push(package);
        }
        Pool {
            packages: list,
            repo,
            by_name,
            cache: RefCell::new(HashMap::new()),
            unacceptable,
            removed_versions: HashMap::new(),
            removed_versions_by_package: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    pub fn id(index: usize) -> Literal {
        index as Literal + 1
    }

    pub fn index(literal: Literal) -> usize {
        (literal.unsigned_abs() - 1) as usize
    }

    pub fn package(&self, literal: Literal) -> &Package {
        &self.packages[Self::index(literal)]
    }

    /// `whatProvides`: literals of the packages matching the name and constraint.
    pub fn what_provides(&self, name: &str, constraint: Option<&Constraint>) -> Vec<Literal> {
        let key = (
            name.to_string(),
            constraint.map(|c| c.to_string()).unwrap_or_default(),
        );
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let result: Vec<Literal> = self
            .by_name
            .get(name)
            .map(|candidates| {
                candidates
                    .iter()
                    .filter(|&&i| self.matches(i, name, constraint))
                    .map(|&i| Self::id(i))
                    .collect()
            })
            .unwrap_or_default();
        self.cache.borrow_mut().insert(key, result.clone());
        result
    }

    /// `Pool::match`
    pub fn matches(&self, index: usize, name: &str, constraint: Option<&Constraint>) -> bool {
        let candidate = &self.packages[index];
        if candidate.name == name {
            return constraint
                .is_none_or(|c| c.matches(&Constraint::new("==", &candidate.version)));
        }
        candidate
            .provides
            .iter()
            .chain(candidate.replaces.iter())
            .any(|link| {
                link.target == name && constraint.is_none_or(|c| c.matches(&link.constraint))
            })
    }

    pub fn literal_to_pretty_string(&self, literal: Literal, installed: &HashSet<usize>) -> String {
        let package = self.package(literal);
        let prefix = if installed.contains(&Self::index(literal)) {
            if literal > 0 { "keep" } else { "remove" }
        } else if literal > 0 {
            "install"
        } else {
            "don't install"
        };
        format!("{prefix} {}", pretty_string(package))
    }

    pub fn is_unacceptable(&self, package: &Package) -> bool {
        self.unacceptable
            .iter()
            .any(|p| p.name == package.name && p.version == package.version)
    }

    pub fn removed_versions_for(
        &self,
        name: &str,
        constraint: &Constraint,
    ) -> BTreeMap<String, String> {
        self.removed_versions
            .get(name)
            .map(|versions| {
                versions
                    .iter()
                    .filter(|(v, _)| constraint.matches(&Constraint::new("==", v)))
                    .map(|(v, p)| (v.clone(), p.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub const UPDATE_ONLY_LISTED: u8 = 0;
pub const UPDATE_LISTED_WITH_TRANSITIVE_DEPS_NO_ROOT_REQUIRE: u8 = 1;
pub const UPDATE_LISTED_WITH_TRANSITIVE_DEPS: u8 = 2;

#[derive(Default)]
pub struct Request {
    /// the lock repository's index in the repository set and its packages
    pub locked_repo: Option<(usize, Vec<Package>)>,
    /// name => (constraint, pretty constraint), in insertion order
    pub requires: Vec<(String, Constraint, String)>,
    pub fixed: Vec<(usize, Package)>,
    pub locked: Vec<(usize, Package)>,
    pub fixed_locked: Vec<(usize, Package)>,
    pub update_allow_list: Vec<String>,
    allow_regex: Option<Regex>,
    pub update_allow_transitive: Option<u8>,
    pub restricted: Option<Vec<String>>,
}

impl Request {
    pub fn new(locked_repo: Option<(usize, Vec<Package>)>) -> Request {
        Request {
            locked_repo,
            ..Default::default()
        }
    }

    pub fn require_name(
        &mut self,
        name: &str,
        constraint: Constraint,
        pretty: &str,
    ) -> Result<(), String> {
        let name = name.to_lowercase();
        if let Some((_, _, p)) = self.requires.iter().find(|(n, _, _)| *n == name) {
            return Err(format!(
                "Overwriting requires seems like a bug ({name} {p} => {pretty}, check why it is happening, might be a root alias"
            ));
        }
        self.requires.push((name, constraint, pretty.to_string()));
        Ok(())
    }

    pub fn require(&self, name: &str) -> Option<&Constraint> {
        self.requires
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, c, _)| c)
    }

    fn insert(list: &mut Vec<(usize, Package)>, repo: usize, package: &Package) {
        if !list
            .iter()
            .any(|(r, p)| *r == repo && p.name == package.name && p.version == package.version)
        {
            list.push((repo, package.clone()));
        }
    }

    pub fn fix_package(&mut self, repo: usize, package: &Package) {
        Self::insert(&mut self.fixed, repo, package);
    }

    pub fn lock_package(&mut self, repo: usize, package: &Package) {
        Self::insert(&mut self.locked, repo, package);
    }

    pub fn fix_locked_package(&mut self, repo: usize, package: &Package) {
        Self::insert(&mut self.fixed, repo, package);
        Self::insert(&mut self.fixed_locked, repo, package);
    }

    pub fn unlock_package(&mut self, repo: usize, package: &Package) {
        self.locked.retain(|(r, p)| {
            !(*r == repo && p.name == package.name && p.version == package.version)
        });
    }

    pub fn set_update_allow_list(&mut self, list: Vec<String>, transitive: u8) {
        self.allow_regex =
            (!list.is_empty()).then(|| crate::repository::package_names_to_regexp(&list));
        self.update_allow_list = list;
        self.update_allow_transitive = Some(transitive);
    }

    pub fn is_in_update_allow_list(&self, name: &str) -> bool {
        self.allow_regex.as_ref().is_some_and(|r| r.is_match(name))
    }

    pub fn update_allow_transitive_dependencies(&self) -> bool {
        self.update_allow_transitive != Some(UPDATE_ONLY_LISTED)
    }

    pub fn update_allow_transitive_root_dependencies(&self) -> bool {
        self.update_allow_transitive == Some(UPDATE_LISTED_WITH_TRANSITIVE_DEPS)
    }

    pub fn is_fixed_package(&self, repo: usize, package: &Package) -> bool {
        self.fixed
            .iter()
            .any(|(r, p)| *r == repo && p.name == package.name && p.version == package.version)
    }

    pub fn is_locked_package(&self, repo: usize, package: &Package) -> bool {
        self.locked
            .iter()
            .chain(self.fixed_locked.iter())
            .any(|(r, p)| *r == repo && p.name == package.name && p.version == package.version)
    }

    /// `getLockedPackages`: locked then fixed-locked
    pub fn locked_packages(&self) -> Vec<&(usize, Package)> {
        self.locked.iter().chain(self.fixed_locked.iter()).collect()
    }

    /// `getFixedOrLockedPackages`: fixed then locked
    pub fn fixed_or_locked_packages(&self) -> Vec<(usize, Package)> {
        self.fixed
            .iter()
            .chain(self.locked.iter())
            .cloned()
            .collect()
    }

    /// `getPresentMap`: the lock repository's packages, then the fixed ones.
    pub fn present_packages(&self) -> Vec<(usize, Package)> {
        let mut out: Vec<(usize, Package)> = Vec::new();
        if let Some((repo, packages)) = &self.locked_repo {
            for p in packages {
                Self::insert(&mut out, *repo, p);
            }
        }
        for (r, p) in &self.fixed {
            Self::insert(&mut out, *r, p);
        }
        out
    }

    /// Pool indices of the present packages (`getPresentMap(true)`).
    pub fn present_indices(&self, pool: &Pool) -> HashSet<usize> {
        let present = self.present_packages();
        (0..pool.len())
            .filter(|&i| {
                present.iter().any(|(r, p)| {
                    *r == pool.repo[i]
                        && p.name == pool.packages[i].name
                        && p.version == pool.packages[i].version
                })
            })
            .collect()
    }

    /// Pool indices of the fixed packages (`getFixedPackagesMap`).
    pub fn fixed_indices(&self, pool: &Pool) -> HashSet<usize> {
        (0..pool.len())
            .filter(|&i| self.is_fixed_package(pool.repo[i], &pool.packages[i]))
            .collect()
    }
}
