//! `RepositorySet` and `PoolBuilder`: which packages the solver gets to see.
//!
//! ponytail: `PoolOptimizer` is not ported (the pool holds every loaded version);
//! add it if a fixture's lock differs because of it.

use super::{Key, Pool, Request, is_package_acceptable, names};
use crate::io::Io;
use crate::package::{Package, is_platform_package};
use crate::platform::PlatformRepository;
use crate::repository::ComposerRepository;
use crate::semver::{Constraint, Stability, intervals, parse_stability};
use std::collections::{BTreeMap, HashMap, HashSet};

/// `ArrayRepository` / `PackageRepository`, optionally behind a `FilterRepository`.
pub struct ArrayRepo {
    pub packages: Vec<Package>,
    pub canonical: bool,
    pub only: Option<regex::Regex>,
    pub exclude: Option<regex::Regex>,
}

impl ArrayRepo {
    pub fn new(packages: Vec<Package>) -> ArrayRepo {
        ArrayRepo {
            packages,
            canonical: true,
            only: None,
            exclude: None,
        }
    }

    /// `FilterRepository::isAllowed`
    pub fn is_allowed(&self, name: &str) -> bool {
        if let Some(only) = &self.only {
            return only.is_match(name);
        }
        if let Some(exclude) = &self.exclude {
            return !exclude.is_match(name);
        }
        true
    }
}

#[allow(clippy::large_enum_variant)]
pub enum Repository<'a> {
    /// `RootPackageRepository`
    Root(Package),
    Platform(&'a PlatformRepository),
    /// `LockArrayRepository`
    Locked(Vec<Package>),
    Array(ArrayRepo),
    Composer(&'a mut ComposerRepository),
}

impl Repository<'_> {
    pub fn name(&self) -> String {
        match self {
            Repository::Root(_) => "root package repo".into(),
            Repository::Platform(_) => "platform repo".into(),
            Repository::Locked(_) => "lock repo".into(),
            Repository::Array(repo) => format!(
                "package repo (defining {} package{})",
                repo.packages.len(),
                if repo.packages.len() > 1 { "s" } else { "" }
            ),
            Repository::Composer(repo) => format!(
                "composer repo ({})",
                crate::download::sanitize_url(&repo.url)
            ),
        }
    }

    pub fn packages(&self) -> Vec<Package> {
        match self {
            Repository::Root(p) => vec![p.clone()],
            Repository::Platform(p) => p.packages.clone(),
            Repository::Locked(l) => l.clone(),
            Repository::Array(repo) => repo
                .packages
                .iter()
                .filter(|p| repo.is_allowed(&p.name))
                .cloned()
                .collect(),
            Repository::Composer(_) => vec![],
        }
    }

    /// `loadPackages`: (names found, packages)
    pub fn load_packages(
        &mut self,
        io: &mut Io,
        names_map: &[(String, Option<Constraint>)],
        acceptable: &[Stability],
        flags: &BTreeMap<String, u8>,
        already_loaded: &HashMap<String, HashSet<String>>,
    ) -> Result<(Vec<String>, Vec<Package>), String> {
        match self {
            Repository::Composer(repo) => {
                repo.load_packages_filtered(io, names_map, acceptable, flags, already_loaded)
            }
            Repository::Array(repo) => {
                let names_map: Vec<(String, Option<Constraint>)> = names_map
                    .iter()
                    .filter(|(n, _)| repo.is_allowed(n))
                    .cloned()
                    .collect();
                if names_map.is_empty() {
                    return Ok((vec![], vec![]));
                }
                let (found, packages) = array_load_packages(
                    &repo.packages,
                    &names_map,
                    acceptable,
                    flags,
                    already_loaded,
                );
                Ok((if repo.canonical { found } else { vec![] }, packages))
            }
            _ => Ok(array_load_packages(
                &self.packages(),
                names_map,
                acceptable,
                flags,
                already_loaded,
            )),
        }
    }

    /// `findPackages`
    pub fn find_packages(
        &mut self,
        io: &mut Io,
        name: &str,
        constraint: Option<&Constraint>,
    ) -> Result<Vec<Package>, String> {
        match self {
            Repository::Composer(repo) => {
                repo.load_packages(io, &[(name.to_string(), constraint.cloned())])
            }
            _ => Ok(self
                .packages()
                .into_iter()
                .filter(|p| {
                    p.name == name
                        && constraint.is_none_or(|c| c.matches(&Constraint::new("==", &p.version)))
                })
                .collect()),
        }
    }
}

/// `ArrayRepository::loadPackages`
pub fn array_load_packages(
    packages: &[Package],
    names_map: &[(String, Option<Constraint>)],
    acceptable: &[Stability],
    flags: &BTreeMap<String, u8>,
    already_loaded: &HashMap<String, HashSet<String>>,
) -> (Vec<String>, Vec<Package>) {
    let mut result: Vec<Package> = Vec::new();
    let mut found: Vec<String> = Vec::new();
    let contains = |list: &[Package], p: &Package| {
        list.iter().any(|r| {
            r.alias_of.is_some() == p.alias_of.is_some()
                && r.name == p.name
                && r.version == p.version
        })
    };
    for package in packages {
        let Some((_, constraint)) = names_map.iter().find(|(n, _)| *n == package.name) else {
            continue;
        };
        if constraint
            .as_ref()
            .is_none_or(|c| c.matches(&Constraint::new("==", &package.version)))
            && is_package_acceptable(
                acceptable,
                flags,
                &names(package, true),
                package.stability(),
            )
            && !already_loaded
                .get(&package.name)
                .is_some_and(|v| v.contains(&package.version))
        {
            if !contains(&result, package) {
                result.push(package.clone());
            }
            if let Some(real) = &package.alias_of
                && !contains(&result, real)
            {
                result.push((**real).clone());
            }
        }
        if !found.contains(&package.name) {
            found.push(package.name.clone());
        }
    }
    for package in packages {
        if let Some(real) = &package.alias_of
            && contains(&result, real)
            && !contains(&result, package)
        {
            result.push(package.clone());
        }
    }
    (found, result)
}

pub struct RepositorySet<'a> {
    pub repos: Vec<Repository<'a>>,
    pub acceptable: Vec<Stability>,
    pub flags: BTreeMap<String, u8>,
    /// name => version => (alias, alias_normalized)
    pub root_aliases: HashMap<String, HashMap<String, (String, String)>>,
    pub root_references: BTreeMap<String, String>,
    pub root_requires: Vec<(String, Constraint, String)>,
    pub temporary_constraints: Vec<(String, Constraint, String)>,
    pub locked_repo: Option<usize>,
}

impl<'a> RepositorySet<'a> {
    pub fn new(
        minimum_stability: Stability,
        flags: BTreeMap<String, u8>,
        root_aliases: &[crate::package::root::AliasDef],
        root_references: BTreeMap<String, String>,
        root_requires: Vec<(String, Constraint, String)>,
        temporary_constraints: Vec<(String, Constraint, String)>,
    ) -> RepositorySet<'a> {
        let acceptable: Vec<Stability> = [
            Stability::Stable,
            Stability::RC,
            Stability::Beta,
            Stability::Alpha,
            Stability::Dev,
        ]
        .into_iter()
        .filter(|s| s.priority() <= minimum_stability.priority())
        .collect();
        let mut aliases: HashMap<String, HashMap<String, (String, String)>> = HashMap::new();
        for a in root_aliases {
            aliases.entry(a.package.clone()).or_default().insert(
                a.version.clone(),
                (a.alias.clone(), a.alias_normalized.clone()),
            );
        }
        RepositorySet {
            repos: vec![],
            acceptable,
            flags,
            root_aliases: aliases,
            root_references,
            root_requires: root_requires
                .into_iter()
                .filter(|(n, _, _)| !is_platform_package(n))
                .collect(),
            temporary_constraints,
            locked_repo: None,
        }
    }

    pub fn add(&mut self, repo: Repository<'a>) -> usize {
        if matches!(repo, Repository::Locked(_)) {
            self.locked_repo = Some(self.repos.len());
        }
        self.repos.push(repo);
        self.repos.len() - 1
    }

    pub const ALLOW_UNACCEPTABLE_STABILITIES: u8 = 1;
    pub const ALLOW_SHADOWED_REPOSITORIES: u8 = 2;

    /// `findPackages`: (repository index, package) pairs in repository order.
    pub fn find_packages(
        &mut self,
        io: &mut Io,
        name: &str,
        constraint: Option<&Constraint>,
        flags: u8,
    ) -> Result<Vec<(usize, Package)>, String> {
        let ignore_stability = flags & Self::ALLOW_UNACCEPTABLE_STABILITIES != 0;
        let all_repos = flags & Self::ALLOW_SHADOWED_REPOSITORIES != 0;
        let mut found = Vec::new();
        if all_repos {
            for (i, repo) in self.repos.iter_mut().enumerate() {
                found.extend(
                    repo.find_packages(io, name, constraint)?
                        .into_iter()
                        .map(|p| (i, p)),
                );
            }
        } else {
            let all: Vec<Stability> = vec![
                Stability::Stable,
                Stability::RC,
                Stability::Beta,
                Stability::Alpha,
                Stability::Dev,
            ];
            for i in 0..self.repos.len() {
                let (acceptable, stability_flags) = if ignore_stability {
                    (all.clone(), BTreeMap::new())
                } else {
                    (self.acceptable.clone(), self.flags.clone())
                };
                let (names_found, packages) = self.repos[i].load_packages(
                    io,
                    &[(name.to_string(), constraint.cloned())],
                    &acceptable,
                    &stability_flags,
                    &HashMap::new(),
                )?;
                found.extend(packages.into_iter().map(|p| (i, p)));
                if names_found.iter().any(|n| n == name) {
                    break;
                }
            }
        }
        if ignore_stability || !all_repos {
            return Ok(found);
        }
        Ok(found
            .into_iter()
            .filter(|(_, p)| {
                is_package_acceptable(
                    &self.acceptable,
                    &self.flags,
                    &names(p, true),
                    p.stability(),
                )
            })
            .collect())
    }

    pub fn is_package_acceptable(&self, package: &Package) -> bool {
        is_package_acceptable(
            &self.acceptable,
            &self.flags,
            &names(package, true),
            package.stability(),
        )
    }

    /// `createPoolWithAllPackages`
    pub fn create_pool_with_all_packages(&self) -> Pool {
        let mut packages = Vec::new();
        for (i, repo) in self.repos.iter().enumerate() {
            for package in repo.packages() {
                if let Some((alias, alias_normalized)) = self
                    .root_aliases
                    .get(&package.name)
                    .and_then(|v| v.get(&package.version))
                {
                    let mut base = package.clone();
                    while let Some(real) = base.alias_of.take() {
                        base = *real;
                    }
                    let mut alias_package = base.alias(alias_normalized, alias);
                    alias_package.root_alias = true;
                    packages.push((i, package));
                    packages.push((i, alias_package));
                } else {
                    packages.push((i, package));
                }
            }
        }
        Pool::new(packages, vec![])
    }

    /// `createPool`
    pub fn create_pool(&mut self, io: &mut Io, request: &mut Request) -> Result<Pool, String> {
        PoolBuilder::new(self).build(io, request)
    }
}

struct PoolBuilder<'s, 'a> {
    set: &'s mut RepositorySet<'a>,
    alias_map: HashMap<Key, Vec<usize>>,
    packages_to_load: Vec<(String, Constraint)>,
    loaded_packages: HashMap<String, Constraint>,
    loaded_per_repo: HashMap<usize, HashMap<String, HashSet<String>>>,
    packages: Vec<Option<(usize, Package)>>,
    unacceptable: Vec<Package>,
    update_allow_list: Vec<String>,
    skipped_load: HashMap<String, Vec<Package>>,
    restricted: Option<Vec<String>>,
    path_repo_unlocked: HashSet<String>,
    max_extended_reqs: HashSet<String>,
    update_allow_warned: HashSet<String>,
}

impl<'s, 'a> PoolBuilder<'s, 'a> {
    fn new(set: &'s mut RepositorySet<'a>) -> PoolBuilder<'s, 'a> {
        PoolBuilder {
            set,
            alias_map: HashMap::new(),
            packages_to_load: Vec::new(),
            loaded_packages: HashMap::new(),
            loaded_per_repo: HashMap::new(),
            packages: Vec::new(),
            unacceptable: Vec::new(),
            update_allow_list: Vec::new(),
            skipped_load: HashMap::new(),
            restricted: None,
            path_repo_unlocked: HashSet::new(),
            max_extended_reqs: HashSet::new(),
            update_allow_warned: HashSet::new(),
        }
    }

    fn build(mut self, io: &mut Io, request: &mut Request) -> Result<Pool, String> {
        self.restricted = request.restricted.clone();
        if !request.update_allow_list.is_empty() {
            self.update_allow_list = request.update_allow_list.clone();
            self.warn_about_non_matching_update_allow_list(io, request);
            let Some((locked_index, locked)) = request.locked_repo.clone() else {
                return Err("No lock repo present and yet a partial update was requested.".into());
            };
            for locked_package in &locked {
                if !request.is_in_update_allow_list(&locked_package.name) {
                    self.skipped_load
                        .entry(locked_package.name.clone())
                        .or_default()
                        .push(locked_package.clone());
                    for link in &locked_package.replaces {
                        self.skipped_load
                            .entry(link.target.clone())
                            .or_default()
                            .push(locked_package.clone());
                    }
                    if locked_package
                        .dist
                        .as_ref()
                        .is_some_and(|d| d.kind == "path")
                    {
                        let symlink_off = locked_package
                            .transport_options
                            .as_ref()
                            .and_then(|t| t.get("symlink"))
                            == Some(&crate::json::Value::Bool(false));
                        if !symlink_off {
                            self.path_repo_unlocked.insert(locked_package.name.clone());
                            continue;
                        }
                    }
                    request.lock_package(locked_index, locked_package);
                }
            }
        }

        for (repo, package) in request.fixed_or_locked_packages() {
            self.loaded_packages
                .insert(package.name.clone(), Constraint::MatchAll);
            for link in &package.replaces {
                self.loaded_packages
                    .insert(link.target.clone(), Constraint::MatchAll);
            }
            let fixed_repo = matches!(
                self.set.repos[repo],
                Repository::Root(_) | Repository::Platform(_)
            );
            if fixed_repo || self.set.is_package_acceptable(&package) {
                self.load_package(io, request, repo, package, false);
            } else {
                self.unacceptable.push(package);
            }
        }

        for (name, constraint, _) in &request.requires {
            if self.loaded_packages.contains_key(name) {
                continue;
            }
            self.packages_to_load
                .push((name.clone(), constraint.clone()));
            self.max_extended_reqs.insert(name.clone());
        }
        let loaded = self.loaded_packages.clone();
        self.packages_to_load
            .retain(|(n, _)| !loaded.contains_key(n));

        while !self.packages_to_load.is_empty() {
            self.load_packages_marked_for_loading(io, request)?;
        }

        if !self.set.temporary_constraints.is_empty() {
            for i in 0..self.packages.len() {
                let Some((repo, package)) = self.packages[i].clone() else {
                    continue;
                };
                if package.alias_of.is_some() {
                    continue;
                }
                for name in names(&package, true) {
                    let Some((_, constraint, _)) = self
                        .set
                        .temporary_constraints
                        .iter()
                        .find(|(n, _, _)| *n == name)
                    else {
                        continue;
                    };
                    let mut indices = vec![i];
                    indices.extend(
                        self.alias_map
                            .get(&Key::of(repo, &package))
                            .cloned()
                            .unwrap_or_default(),
                    );
                    let found = indices.iter().any(|&j| {
                        self.packages[j].as_ref().is_some_and(|(_, p)| {
                            constraint.matches(&Constraint::new("==", &p.version))
                        })
                    });
                    if !found {
                        for j in indices {
                            self.packages[j] = None;
                        }
                    }
                }
            }
        }

        let packages: Vec<(usize, Package)> = self.packages.into_iter().flatten().collect();
        Ok(Pool::new(packages, self.unacceptable))
    }

    fn mark_package_name_for_loading(
        &mut self,
        request: &Request,
        name: &str,
        constraint: &Constraint,
    ) {
        if is_platform_package(name) || self.max_extended_reqs.contains(name) {
            return;
        }
        let mut constraint = constraint.clone();
        if let Some(root) = request.require(name)
            && !intervals::is_subset_of(&constraint, root)
        {
            constraint = root.clone();
        }
        if !self.loaded_packages.contains_key(name) {
            if let Some((_, existing)) = self.packages_to_load.iter_mut().find(|(n, _)| n == name) {
                if intervals::is_subset_of(&constraint, existing) {
                    return;
                }
                *existing = intervals::compact_constraint(&Constraint::multi(
                    vec![existing.clone(), constraint],
                    false,
                ));
                return;
            }
            self.packages_to_load.push((name.to_string(), constraint));
            return;
        }
        if intervals::is_subset_of(&constraint, &self.loaded_packages[name]) {
            return;
        }
        let merged = intervals::compact_constraint(&Constraint::multi(
            vec![self.loaded_packages[name].clone(), constraint],
            false,
        ));
        self.packages_to_load.retain(|(n, _)| n != name);
        self.packages_to_load.push((name.to_string(), merged));
        self.loaded_packages.remove(name);
    }

    fn load_packages_marked_for_loading(
        &mut self,
        io: &mut Io,
        request: &mut Request,
    ) -> Result<(), String> {
        if let Some(restricted) = &self.restricted {
            let restricted = restricted.clone();
            self.packages_to_load
                .retain(|(n, _)| restricted.contains(n));
        }
        for (name, constraint) in &self.packages_to_load {
            self.loaded_packages
                .insert(name.clone(), constraint.clone());
        }
        let mut batch: Vec<(String, Option<Constraint>)> = self
            .packages_to_load
            .drain(..)
            .map(|(n, c)| (n, Some(c)))
            .collect();
        for repo_index in 0..self.set.repos.len() {
            if matches!(self.set.repos[repo_index], Repository::Platform(_))
                || Some(repo_index) == self.set.locked_repo
            {
                continue;
            }
            if batch.is_empty() {
                break;
            }
            let acceptable = self.set.acceptable.clone();
            let flags = self.set.flags.clone();
            let already = self
                .loaded_per_repo
                .get(&repo_index)
                .cloned()
                .unwrap_or_default();
            let (found, packages) = self.set.repos[repo_index].load_packages(
                io,
                &batch,
                &acceptable,
                &flags,
                &already,
            )?;
            batch.retain(|(n, _)| !found.contains(n));
            for package in packages {
                self.loaded_per_repo
                    .entry(repo_index)
                    .or_default()
                    .entry(package.name.clone())
                    .or_default()
                    .insert(package.version.clone());
                let propagate = !self.path_repo_unlocked.contains(&package.name);
                self.load_package(io, request, repo_index, package, propagate);
            }
        }
        Ok(())
    }

    fn load_package(
        &mut self,
        io: &mut Io,
        request: &mut Request,
        repo: usize,
        mut package: Package,
        propagate_update: bool,
    ) {
        let name = package.name.clone();
        if let Some(reference) = self.set.root_references.get(&name).cloned()
            && !request.is_locked_package(repo, &package)
            && !request.is_fixed_package(repo, &package)
        {
            set_source_dist_references(&mut package, &reference);
        }
        let index = self.packages.len();
        if let Some(real) = &package.alias_of {
            self.alias_map
                .entry(Key::of(repo, real))
                .or_default()
                .push(index);
        }
        self.packages.push(Some((repo, package.clone())));

        if (propagate_update || self.path_repo_unlocked.contains(&name))
            && let Some((alias, alias_normalized)) = self
                .set
                .root_aliases
                .get(&name)
                .and_then(|v| v.get(&package.version))
                .cloned()
        {
            let mut base = package.clone();
            while let Some(real) = base.alias_of.take() {
                base = *real;
            }
            let mut alias_package = base.alias(&alias_normalized, &alias);
            alias_package.root_alias = true;
            let new_index = self.packages.len();
            self.alias_map
                .entry(Key::of(repo, &base))
                .or_default()
                .push(new_index);
            self.packages.push(Some((repo, alias_package)));
        }

        for link in package.requires.clone() {
            let require = link.target.clone();
            if self.skipped_load.contains_key(&require) {
                if propagate_update && request.update_allow_transitive_dependencies() {
                    let skipped_root = self.skipped_root_requires(request, &require);
                    if request.update_allow_transitive_root_dependencies()
                        || skipped_root.is_empty()
                    {
                        self.unlock_package(request, &require);
                        self.mark_package_name_for_loading(request, &require, &link.constraint);
                    } else {
                        self.warn_root_requirement(io, &skipped_root);
                    }
                } else if self.path_repo_unlocked.contains(&require)
                    && !self.loaded_packages.contains_key(&require)
                {
                    self.mark_package_name_for_loading(request, &require, &link.constraint);
                }
            } else {
                self.mark_package_name_for_loading(request, &require, &link.constraint);
            }
        }

        if propagate_update && request.update_allow_transitive_dependencies() {
            for link in package.replaces.clone() {
                let replace = link.target.clone();
                if self.loaded_packages.contains_key(&replace)
                    && self.skipped_load.contains_key(&replace)
                {
                    let skipped_root = self.skipped_root_requires(request, &replace);
                    if request.update_allow_transitive_root_dependencies()
                        || skipped_root.is_empty()
                    {
                        self.unlock_package(request, &replace);
                        self.mark_package_name_for_loading_if_required(request, &replace);
                    } else {
                        self.warn_root_requirement(io, &skipped_root);
                    }
                }
            }
        }
    }

    fn warn_root_requirement(&mut self, io: &mut Io, roots: &[String]) {
        for root in roots {
            if self.update_allow_warned.insert(root.clone()) {
                io.warning(&format!(
                    "Dependency {root} is also a root requirement. Package has not been listed as an update argument, so keeping locked at old version. Use --with-all-dependencies (-W) to include root dependencies."
                ));
            }
        }
    }

    fn skipped_root_requires(&self, request: &Request, name: &str) -> Vec<String> {
        let Some(skipped) = self.skipped_load.get(name) else {
            return vec![];
        };
        if request.require(name).is_some() {
            return skipped
                .iter()
                .map(|p| {
                    if p.name != name {
                        format!("{} (via replace of {name})", p.name)
                    } else {
                        p.name.clone()
                    }
                })
                .collect();
        }
        let mut matches = Vec::new();
        for p in skipped {
            if request.require(&p.name).is_some() {
                matches.push(p.name.clone());
            }
            for link in &p.replaces {
                if request.require(&link.target).is_some() {
                    if p.name != name {
                        matches.push(format!("{} (via replace of {name})", p.name));
                    } else {
                        matches.push(p.name.clone());
                    }
                    break;
                }
            }
        }
        matches
    }

    fn warn_about_non_matching_update_allow_list(&self, io: &mut Io, request: &Request) {
        let Some((_, locked)) = &request.locked_repo else {
            return;
        };
        for pattern in &self.update_allow_list {
            let regex = crate::repository::package_name_to_regexp(pattern);
            if locked.iter().any(|p| regex.is_match(&p.name)) {
                continue;
            }
            let mut matched_platform = false;
            let mut matched = false;
            for (name, _, _) in &request.requires {
                if regex.is_match(name) {
                    if is_platform_package(name) {
                        matched_platform = true;
                        continue;
                    }
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
            if matched_platform {
                io.warning(&format!("Pattern \"{pattern}\" listed for update matches platform packages, but these cannot be updated by Composer."));
            } else if pattern.contains('*') {
                io.warning(&format!(
                    "Pattern \"{pattern}\" listed for update does not match any locked packages."
                ));
            } else {
                io.warning(&format!(
                    "Package \"{pattern}\" listed for update is not locked."
                ));
            }
        }
    }

    fn is_root_require(request: &Request, name: &str) -> bool {
        request.require(name).is_some()
    }

    fn unlock_package(&mut self, request: &mut Request, name: &str) {
        for package_or_replacer in self.skipped_load.get(name).cloned().unwrap_or_default() {
            let replacer = package_or_replacer.name.clone();
            if replacer != name
                && self.skipped_load.contains_key(&replacer)
                && (request.update_allow_transitive_root_dependencies()
                    || (!Self::is_root_require(request, name)
                        && !Self::is_root_require(request, &replacer)))
            {
                {
                    self.unlock_package(request, &replacer);
                    if Self::is_root_require(request, &replacer) {
                        self.mark_package_name_for_loading(
                            request,
                            &replacer,
                            &Constraint::MatchAll,
                        );
                    } else {
                        let requirers: Vec<Constraint> = self
                            .packages
                            .iter()
                            .flatten()
                            .flat_map(|(_, p)| p.requires.iter())
                            .filter(|l| l.target == replacer)
                            .map(|l| l.constraint.clone())
                            .collect();
                        for c in requirers {
                            self.mark_package_name_for_loading(request, &replacer, &c);
                        }
                    }
                }
            }
        }
        if self.path_repo_unlocked.contains(name) {
            for i in 0..self.packages.len() {
                if self.packages[i]
                    .as_ref()
                    .is_some_and(|(_, p)| p.name == name)
                {
                    self.remove_loaded_package(i);
                }
            }
        }
        self.skipped_load.remove(name);
        self.loaded_packages.remove(name);
        self.max_extended_reqs.remove(name);
        self.path_repo_unlocked.remove(name);

        for (repo, locked_package) in request
            .locked_packages()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
        {
            if locked_package.alias_of.is_some() || locked_package.name != name {
                continue;
            }
            let index = self.packages.iter().position(|e| {
                e.as_ref().is_some_and(|(r, p)| {
                    *r == repo
                        && p.alias_of.is_none()
                        && p.name == locked_package.name
                        && p.version == locked_package.version
                })
            });
            let Some(index) = index else { continue };
            request.unlock_package(repo, &locked_package);
            self.remove_loaded_package(index);
            for (_, fixed_or_locked) in request.fixed_or_locked_packages() {
                if fixed_or_locked.name == locked_package.name
                    && fixed_or_locked.version == locked_package.version
                {
                    continue;
                }
                if self.skipped_load.contains_key(&fixed_or_locked.name) {
                    if let Some(link) = fixed_or_locked
                        .requires
                        .iter()
                        .find(|l| l.target == locked_package.name)
                    {
                        let c = link.constraint.clone();
                        self.mark_package_name_for_loading(request, &locked_package.name, &c);
                    }
                    for replace in &locked_package.replaces {
                        if fixed_or_locked
                            .requires
                            .iter()
                            .any(|l| l.target == replace.target)
                            && self.skipped_load.contains_key(&replace.target)
                        {
                            self.unlock_package(request, &replace.target);
                            self.mark_package_name_for_loading(
                                request,
                                &replace.target,
                                &replace.constraint,
                            );
                        }
                    }
                }
            }
        }
    }

    fn mark_package_name_for_loading_if_required(&mut self, request: &Request, name: &str) {
        if let Some(c) = request.require(name).cloned() {
            self.mark_package_name_for_loading(request, name, &c);
        }
        let constraints: Vec<Constraint> = self
            .packages
            .iter()
            .flatten()
            .flat_map(|(_, p)| p.requires.iter())
            .filter(|l| l.target == name)
            .map(|l| l.constraint.clone())
            .collect();
        for c in constraints {
            self.mark_package_name_for_loading(request, name, &c);
        }
    }

    fn remove_loaded_package(&mut self, index: usize) {
        let Some((repo, package)) = self.packages[index].take() else {
            return;
        };
        if let Some(per_repo) = self.loaded_per_repo.get_mut(&repo)
            && let Some(versions) = per_repo.get_mut(&package.name)
        {
            versions.remove(&package.version);
        }
        if let Some(aliases) = self.alias_map.remove(&Key::of(repo, &package)) {
            for alias_index in aliases {
                if let Some((r, alias)) = self.packages[alias_index].take()
                    && let Some(per_repo) = self.loaded_per_repo.get_mut(&r)
                    && let Some(versions) = per_repo.get_mut(&alias.name)
                {
                    versions.remove(&alias.version);
                }
            }
        }
    }
}

/// `Package::setSourceDistReferences`
pub fn set_source_dist_references(package: &mut Package, reference: &str) {
    if let Some(source) = &mut package.source {
        source.reference = Some(reference.to_string());
    }
    let known = regex::Regex::new(
        r"(?i)^https?://(?:(?:www\.)?bitbucket\.org|(api\.)?github\.com|(?:www\.)?gitlab\.com)/",
    )
    .unwrap();
    if let Some(dist) = &mut package.dist {
        if known.is_match(&dist.url) {
            dist.reference = Some(reference.to_string());
            dist.url = fancy_regex::Regex::new(r"(?i)(?<=/|sha=)[a-f0-9]{40}(?=/|$)")
                .unwrap()
                .replace(&dist.url, reference)
                .into_owned();
        } else if dist.reference.as_deref().is_some_and(|r| !r.is_empty()) {
            dist.reference = Some(reference.to_string());
        }
    }
    if let Some(real) = &mut package.alias_of {
        set_source_dist_references(real, reference);
    }
}

/// `VersionParser::parseStability` of a package version
pub fn stability_of(version: &str) -> Stability {
    parse_stability(version)
}
