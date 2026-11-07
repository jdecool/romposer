//! `LockTransaction`: the solver's decisions turned into lock packages and operations.

use super::sat::Solution;
use super::{Pool, Request};
use crate::package::Package;
use crate::package::root::AliasDef;
use crate::transaction::{Operation, calculate_operations};
use std::collections::HashSet;

pub struct LockTransaction {
    present: Vec<Package>,
    /// result packages with whether they are the present (locked/fixed) object itself
    pub all: Vec<(Package, bool)>,
    pub non_dev: Vec<(Package, bool)>,
    pub dev: Vec<(Package, bool)>,
    pub operations: Vec<Operation>,
}

impl LockTransaction {
    pub fn new(
        pool: &Pool,
        request: &Request,
        solution: &Solution,
    ) -> Result<LockTransaction, String> {
        let unlockable: HashSet<usize> = request.fixed_indices(pool);
        let present_indices = request.present_indices(pool);
        let mut all = Vec::new();
        let mut non_dev = Vec::new();
        for (literal, _) in &solution.decisions {
            if *literal > 0 {
                let index = Pool::index(*literal);
                let package = pool.packages[index].clone();
                let is_present = present_indices.contains(&index);
                if !unlockable.contains(&index) {
                    non_dev.push((package.clone(), is_present));
                }
                all.push((package, is_present));
            }
        }
        let present: Vec<Package> = request
            .present_packages()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        let result: Vec<Package> = all.iter().map(|(p, _)| p.clone()).collect();
        let operations = calculate_operations(&present, &result)?;
        Ok(LockTransaction {
            present,
            all,
            non_dev,
            dev: vec![],
            operations,
        })
    }

    /// `setNonDevPackages`: everything the non-dev solve did not keep becomes a dev package.
    pub fn set_non_dev_packages(&mut self, non_dev_result: &LockTransaction) {
        let keep = non_dev_result.new_lock_packages(false, false);
        self.dev = std::mem::take(&mut self.non_dev);
        let mut non_dev = Vec::new();
        for package in &keep {
            let mut i = 0;
            while i < self.dev.len() {
                if self.dev[i].0.name == package.name {
                    non_dev.push(self.dev.remove(i));
                } else {
                    i += 1;
                }
            }
        }
        self.non_dev = non_dev;
    }

    /// `getNewLockPackages`
    pub fn new_lock_packages(&self, dev_mode: bool, update_mirrors: bool) -> Vec<Package> {
        let list = if dev_mode { &self.dev } else { &self.non_dev };
        list.iter()
            .filter(|(p, _)| p.alias_of.is_none())
            .map(|(p, is_present)| {
                if update_mirrors && !is_present {
                    self.update_mirror_and_urls(p)
                } else {
                    p.clone()
                }
            })
            .collect()
    }

    /// `updateMirrorAndUrls`: keep the present package, refreshing only its urls and mirrors.
    fn update_mirror_and_urls(&self, package: &Package) -> Package {
        let known = regex::Regex::new(r"(?i)^https?://(?:(?:www\.)?bitbucket\.org|(api\.)?github\.com|(?:www\.)?gitlab\.com)/").unwrap();
        let sha = fancy_regex::Regex::new(r"(?i)(?<=/|sha=)[a-f0-9]{40}(?=/|$)").unwrap();
        for present in &self.present {
            if present.name != package.name || present.version != package.version {
                continue;
            }
            let Some(present_source) = &present.source else {
                continue;
            };
            if present_source.reference.is_none() {
                continue;
            }
            if present.source.as_ref().map(|s| &s.kind) != package.source.as_ref().map(|s| &s.kind)
            {
                continue;
            }
            let mut updated = present.clone();
            if let (Some(us), Some(ps)) = (&mut updated.source, &package.source) {
                us.url = ps.url.clone();
                us.mirrors = ps.mirrors.clone();
            }
            if present.dist.as_ref().map(|d| &d.kind) != package.dist.as_ref().map(|d| &d.kind) {
                return updated;
            }
            if let (Some(ud), Some(pd)) = (&mut updated.dist, &package.dist) {
                if let Some(reference) = ud.reference.clone()
                    && known.is_match(&pd.url)
                {
                    ud.url = sha.replace(&pd.url, reference.as_str()).into_owned();
                }
                ud.mirrors = pd.mirrors.clone();
            }
            return updated;
        }
        package.clone()
    }

    /// `getAliases`: the root aliases actually used, sorted by package name.
    pub fn aliases(&self, aliases: &[AliasDef]) -> Vec<AliasDef> {
        let mut remaining: Vec<AliasDef> = aliases.to_vec();
        let mut used = Vec::new();
        for (package, _) in &self.all {
            if package.alias_of.is_some() {
                let mut i = 0;
                while i < remaining.len() {
                    if remaining[i].package == package.name {
                        used.push(remaining.remove(i));
                    } else {
                        i += 1;
                    }
                }
            }
        }
        used.sort_by(|a, b| a.package.cmp(&b.package));
        used
    }
}
