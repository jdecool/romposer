//! `vendor/composer/installed.json`: the `InstalledFilesystemRepository`.

use crate::fs::{ensure_dir, find_shortest_path, is_absolute_path, normalize_path, realpath};
use crate::json::{Map, Value, encode_pretty};
use crate::package::{self, Package, dumper};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct InstalledRepository {
    pub path: PathBuf,
    pub packages: Vec<Package>,
    pub dev_mode: Option<bool>,
    pub dev_package_names: Vec<String>,
}

impl InstalledRepository {
    /// `FilesystemRepository::initialize`
    pub fn read(path: &Path) -> Result<InstalledRepository, String> {
        let mut repo = InstalledRepository {
            path: path.to_path_buf(),
            ..Default::default()
        };
        if !path.exists() {
            return Ok(repo);
        }
        let fail = |e: String| {
            format!(
                "Invalid repository data in {}, packages could not be loaded: {e}",
                path.display()
            )
        };
        let text = std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
        let data: Value = serde_json::from_str(&text).map_err(|e| fail(e.to_string()))?;
        let packages = match &data {
            Value::Object(o) if o.contains_key("packages") => o["packages"].clone(),
            other => other.clone(),
        };
        if let Value::Object(o) = &data {
            if let Some(Value::Array(names)) = o.get("dev-package-names") {
                repo.dev_package_names = names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
            }
            if let Some(Value::Bool(dev)) = o.get("dev") {
                repo.dev_mode = Some(*dev);
            }
        }
        let Value::Array(list) = packages else {
            return Err(fail(
                "Could not parse package list from the repository".into(),
            ));
        };
        for entry in list {
            let config = entry
                .as_object()
                .ok_or_else(|| fail("package entry is not an object".into()))?;
            let package = package::load(config).map_err(fail)?;
            repo.add_package(package);
        }
        Ok(repo)
    }

    /// `ArrayRepository::addPackage`: an alias also registers its real package.
    pub fn add_package(&mut self, package: Package) {
        if let Some(real) = &package.alias_of
            && !self
                .packages
                .iter()
                .any(|p| p.alias_of.is_none() && p.name == real.name && p.version == real.version)
        {
            self.packages.push((**real).clone());
        }
        self.packages.push(package);
    }

    pub fn has_package(&self, package: &Package) -> bool {
        self.packages.iter().any(|p| {
            p.unique_name() == package.unique_name()
                && p.alias_of.is_some() == package.alias_of.is_some()
        })
    }

    pub fn remove_package(&mut self, package: &Package) {
        let key = package.unique_name();
        let is_alias = package.alias_of.is_some();
        self.packages
            .retain(|p| !(p.unique_name() == key && p.alias_of.is_some() == is_alias));
    }

    /// `ArrayRepository::getCanonicalPackages`: real packages only.
    pub fn canonical_packages(&self) -> Vec<&Package> {
        self.packages
            .iter()
            .filter(|p| p.alias_of.is_none())
            .collect()
    }

    /// `InstalledRepositoryInterface::isFresh`
    pub fn is_fresh(&self) -> bool {
        self.packages.is_empty()
    }

    /// `FilesystemRepository::write` minus the installed.php/InstalledVersions.php part.
    pub fn write(
        &self,
        dev_mode: bool,
        install_path: impl Fn(&Package) -> Option<String>,
    ) -> Result<(), String> {
        let repo_dir = self.path.parent().unwrap_or(Path::new("."));
        ensure_dir(repo_dir)?;
        let repo_dir = normalize_path(&realpath(repo_dir));
        let cwd = crate::fs::cwd();
        let mut packages: Vec<Map<String, Value>> = Vec::new();
        let mut dev_names: Vec<String> = Vec::new();
        for package in self.canonical_packages() {
            let mut dumped = dumper::dump(package);
            let path = install_path(package);
            let rel = match path.as_deref().filter(|p| !p.is_empty()) {
                Some(p) => {
                    let normalized = normalize_path(&if is_absolute_path(p) {
                        p.to_string()
                    } else {
                        format!("{cwd}/{p}")
                    });
                    Value::String(find_shortest_path(&repo_dir, &normalized, true, false))
                }
                None => Value::Null,
            };
            dumped.insert("install-path".into(), rel);
            packages.push(dumped);
            if self.dev_package_names.contains(&package.name) {
                dev_names.push(package.name.clone());
            }
        }
        dev_names.sort();
        packages.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
        let mut data = Map::new();
        data.insert(
            "packages".into(),
            Value::Array(packages.into_iter().map(Value::Object).collect()),
        );
        data.insert("dev".into(), Value::Bool(dev_mode));
        data.insert(
            "dev-package-names".into(),
            Value::Array(dev_names.into_iter().map(Value::String).collect()),
        );
        let content = encode_pretty(&Value::Object(data)) + "\n";
        if std::fs::read_to_string(&self.path).ok().as_deref() != Some(content.as_str()) {
            std::fs::write(&self.path, content)
                .map_err(|e| format!("Could not write {}: {e}", self.path.display()))?;
        }
        Ok(())
    }
}
