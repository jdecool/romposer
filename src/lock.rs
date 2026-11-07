//! `composer.lock` reading and the parts of `Locker` that need no resolver:
//! freshness via `content-hash`, locked packages, missing-requirement report.

use crate::json::{Map, Value, encode_compact};
use crate::package::root::{AliasDef, RootPackage};
use crate::package::{self, Package};
use crate::semver::{self, Constraint};
use std::collections::BTreeMap;
use std::path::Path;

/// `JsonFile::CONFLICTED_CONTENT_HASH`
pub const CONFLICTED_CONTENT_HASH: &str =
    "VCS merge conflict detected. Please run `composer update --lock`.";

/// `Locker::getContentHash`: md5 of the relevant keys, as PHP would re-encode them.
pub fn content_hash(composer_json: &str) -> Result<String, String> {
    const RELEVANT: [&str; 11] = [
        "name",
        "version",
        "require",
        "require-dev",
        "conflict",
        "replace",
        "provide",
        "minimum-stability",
        "prefer-stable",
        "repositories",
        "extra",
    ];
    let content: Value = serde_json::from_str(composer_json).map_err(|e| e.to_string())?;
    let content = php_assoc(content);
    let content = content.as_object().cloned().unwrap_or_default();
    let mut relevant = Map::new();
    for key in RELEVANT {
        if let Some(v) = content.get(key) {
            relevant.insert(key.to_string(), v.clone());
        }
    }
    if let Some(Value::Object(config)) = content.get("config") {
        let mut sub = Map::new();
        for key in ["platform", "policy"] {
            if let Some(v) = config.get(key).filter(|v| !v.is_null()) {
                sub.insert(key.to_string(), v.clone());
            }
        }
        if !sub.is_empty() {
            relevant.insert("config".into(), Value::Object(sub));
        }
    }
    let mut sorted: Vec<(String, Value)> = relevant.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let value = Value::Object(sorted.into_iter().collect());
    Ok(format!("{:x}", md5::compute(encode_compact(&value))))
}

/// What `json_decode($json, true)` then `json_encode` does to a document:
/// empty objects become `[]`, objects keyed `0..n-1` become lists.
pub fn php_assoc(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                return Value::Array(vec![]);
            }
            let is_list = map.keys().enumerate().all(|(i, k)| k == &i.to_string());
            if is_list {
                Value::Array(map.into_iter().map(|(_, v)| php_assoc(v)).collect())
            } else {
                Value::Object(map.into_iter().map(|(k, v)| (k, php_assoc(v))).collect())
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(php_assoc).collect()),
        other => other,
    }
}

#[derive(Clone, Debug)]
pub struct Lock {
    pub data: Map<String, Value>,
}

impl Lock {
    pub fn parse(json: &str) -> Result<Lock, String> {
        let value: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        match value {
            Value::Object(data) => Ok(Lock { data }),
            _ => Err(
                "Your composer.lock is invalid. Run \"composer update\" to generate a new one."
                    .into(),
            ),
        }
    }

    /// `None` when the file does not exist.
    pub fn read(path: &Path) -> Result<Option<Lock>, String> {
        if !path.is_file() {
            return Ok(None);
        }
        let json = std::fs::read_to_string(path)
            .map_err(|e| format!("Could not read {}\n\n{e}", path.display()))?;
        Lock::parse(&json)
            .map(Some)
            .map_err(|e| format!("\"{}\" does not contain valid JSON\n{e}", path.display()))
    }

    /// `Locker::isLocked`
    pub fn is_locked(&self) -> bool {
        self.data.contains_key("packages")
    }

    /// `Locker::isFresh`: compares `content-hash` (or the legacy `hash`) with the manifest.
    pub fn is_fresh(&self, composer_json: &str) -> bool {
        if let Some(hash) = self
            .data
            .get("content-hash")
            .and_then(Value::as_str)
            .filter(|h| !h.is_empty())
        {
            return content_hash(composer_json).ok().as_deref() == Some(hash);
        }
        if let Some(hash) = self
            .data
            .get("hash")
            .and_then(Value::as_str)
            .filter(|h| !h.is_empty())
        {
            return format!("{:x}", md5::compute(composer_json)) == hash;
        }
        false
    }

    fn package_list(&self, key: &str) -> Option<&Vec<Value>> {
        self.data.get(key).and_then(Value::as_array)
    }

    /// `Locker::getLockedRepository`: packages (and their aliases) as an `ArrayRepository` holds them.
    pub fn locked_packages(&self, with_dev: bool) -> Result<Vec<Package>, String> {
        let mut entries: Vec<&Value> = self
            .package_list("packages")
            .map(|p| p.iter().collect())
            .unwrap_or_default();
        if with_dev {
            match self.package_list("packages-dev") {
                Some(dev) => entries.extend(dev.iter()),
                None => {
                    return Err("The lock file does not contain require-dev information, run install with the --no-dev option or delete it and run composer update to generate a new lock file.".into());
                }
            }
        }
        if entries.is_empty() {
            return Ok(vec![]);
        }
        if entries[0].get("name").is_none() {
            return Err(
                "Your composer.lock is invalid. Run \"composer update\" to generate a new one."
                    .into(),
            );
        }
        let mut packages = Vec::new();
        for entry in entries {
            let config = entry.as_object().ok_or(
                "Your composer.lock is invalid. Run \"composer update\" to generate a new one.",
            )?;
            let package = package::load(config)?;
            if let Some(real) = &package.alias_of {
                let real = (**real).clone();
                packages.push(package);
                packages.push(real);
            } else {
                packages.push(package);
            }
        }
        if let Some(aliases) = self.data.get("aliases").and_then(Value::as_array) {
            for alias in aliases {
                let (Some(name), Some(normalized), Some(pretty)) = (
                    alias.get("package").and_then(Value::as_str),
                    alias.get("alias_normalized").and_then(Value::as_str),
                    alias.get("alias").and_then(Value::as_str),
                ) else {
                    continue;
                };
                // packageByName keeps the real package for aliased entries
                if let Some(real) = packages
                    .iter()
                    .rev()
                    .find(|p| p.name == name && p.alias_of.is_none())
                    .cloned()
                {
                    packages.push(real.alias(normalized, pretty));
                }
            }
        }
        Ok(packages)
    }

    /// `Locker::getDevPackageNames`
    pub fn dev_package_names(&self) -> Vec<String> {
        self.package_list("packages-dev")
            .map(|p| {
                p.iter()
                    .filter_map(|e| e.get("name").and_then(Value::as_str))
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `Locker::getMissingRequirementInfo`
    pub fn missing_requirement_info(
        &self,
        root: &RootPackage,
        include_dev: bool,
    ) -> Result<Vec<String>, String> {
        let mut info = Vec::new();
        let mut missing = false;
        let root_pkg = &root.package;
        let mut sets = vec![(self.locked_packages(false)?, &root_pkg.requires, "Required")];
        if include_dev {
            sets.push((
                self.locked_packages(true)?,
                &root_pkg.dev_requires,
                "Required (in require-dev)",
            ));
        }
        for (locked, links, description) in sets {
            let mut installed: Vec<&Package> = locked.iter().collect();
            installed.push(root_pkg);
            if let Some(real) = &root_pkg.alias_of {
                installed.push(real);
            }
            for link in links {
                if package::is_platform_package(&link.target)
                    || link.pretty_constraint == "self.version"
                {
                    continue;
                }
                if !find_with_replacers_and_providers(
                    &installed,
                    &link.target,
                    Some(&link.constraint),
                )
                .is_empty()
                {
                    continue;
                }
                let results = find_with_replacers_and_providers(&installed, &link.target, None);
                if let Some(provider) = results.first() {
                    let mut desc = provider.pretty_version.clone();
                    if provider.name != link.target {
                        'outer: for (links, text) in [
                            (&provider.replaces, "replaced as"),
                            (&provider.provides, "provided as"),
                        ] {
                            for pl in links {
                                if pl.target == link.target {
                                    desc = format!(
                                        "{text} {} by {} {}",
                                        pl.pretty_constraint,
                                        provider.pretty_name,
                                        provider.pretty_version
                                    );
                                    break 'outer;
                                }
                            }
                        }
                    }
                    info.push(format!(
                        "- {description} package \"{}\" is in the lock file as \"{desc}\" but that does not satisfy your constraint \"{}\".",
                        link.target, link.pretty_constraint
                    ));
                } else {
                    info.push(format!(
                        "- {description} package \"{}\" is not present in the lock file.",
                        link.target
                    ));
                }
                missing = true;
            }
        }
        if missing {
            info.push("This usually happens when composer files are incorrectly merged or the composer.json file is manually edited.".into());
            info.push("Read more about correctly resolving merge conflicts https://getcomposer.org/doc/articles/resolving-merge-conflicts.md".into());
            info.push("and prefer using the \"require\" command over editing the composer.json file directly https://getcomposer.org/doc/03-cli.md#require--r".into());
        }
        Ok(info)
    }
}

/// `InstalledRepository::findPackagesWithReplacersAndProviders`
pub fn find_with_replacers_and_providers<'a>(
    packages: &[&'a Package],
    name: &str,
    constraint: Option<&Constraint>,
) -> Vec<&'a Package> {
    let name = name.to_lowercase();
    let mut matches = Vec::new();
    for candidate in packages {
        if candidate.name == name {
            if constraint.is_none_or(|c| c.matches(&Constraint::new("==", &candidate.version))) {
                matches.push(*candidate);
            }
            continue;
        }
        for link in candidate.provides.iter().chain(candidate.replaces.iter()) {
            if link.target == name && constraint.is_none_or(|c| c.matches(&link.constraint)) {
                matches.push(*candidate);
                break;
            }
        }
    }
    matches
}

impl Lock {
    fn str_key(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }

    /// `Locker::getMinimumStability`
    pub fn minimum_stability(&self) -> semver::Stability {
        self.str_key("minimum-stability")
            .and_then(|s| semver::Stability::parse(s).ok())
            .unwrap_or(semver::Stability::Stable)
    }

    /// `Locker::getStabilityFlags`
    pub fn stability_flags(&self) -> BTreeMap<String, u8> {
        self.data
            .get("stability-flags")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as u8)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `Locker::getPreferStable` / `getPreferLowest`: `None` for lock files without the key.
    pub fn prefer_stable(&self) -> Option<bool> {
        self.data.get("prefer-stable").and_then(Value::as_bool)
    }

    pub fn prefer_lowest(&self) -> Option<bool> {
        self.data.get("prefer-lowest").and_then(Value::as_bool)
    }

    /// `Locker::getPlatformOverrides`
    pub fn platform_overrides(&self) -> Map<String, Value> {
        self.data
            .get("platform-overrides")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    /// `Locker::getAliases`
    pub fn aliases(&self) -> Vec<AliasDef> {
        self.data
            .get("aliases")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
                        Some(AliasDef {
                            package: s("package")?,
                            version: s("version")?,
                            alias: s("alias")?,
                            alias_normalized: s("alias_normalized")?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `Locker::getPlatformRequirements`: (name, constraint, pretty constraint)
    pub fn platform_requirements(&self, with_dev: bool) -> Vec<(String, Constraint, String)> {
        let mut keys = vec!["platform"];
        if with_dev {
            keys.push("platform-dev");
        }
        let mut out = Vec::new();
        for key in keys {
            if let Some(Value::Object(reqs)) = self.data.get(key) {
                for (name, constraint) in reqs {
                    let pretty = constraint.as_str().unwrap_or("").to_string();
                    if let Ok(c) = semver::parse_constraints(&pretty) {
                        out.push((name.to_lowercase(), c, pretty));
                    }
                }
            }
        }
        out
    }
}

/// `Locker::lockPackages`
fn lock_packages(packages: &[Package]) -> Vec<Value> {
    let mut locked: Vec<Map<String, Value>> = Vec::new();
    for package in packages {
        if package.alias_of.is_some() {
            continue;
        }
        let mut spec = crate::package::dumper::dump(package);
        spec.shift_remove("version_normalized");
        let time = spec.shift_remove("time");
        let published = spec.shift_remove("published-time");
        if let Some(time) = time {
            spec.insert("time".into(), time);
        }
        if let Some(published) = published {
            spec.insert("published-time".into(), published);
        }
        spec.shift_remove("installation-source");
        locked.push(spec);
    }
    locked.sort_by(|a, b| {
        let name = |m: &Map<String, Value>, k: &str| {
            m.get(k).and_then(Value::as_str).unwrap_or("").to_string()
        };
        name(a, "name")
            .cmp(&name(b, "name"))
            .then_with(|| name(a, "version").cmp(&name(b, "version")))
    });
    locked.into_iter().map(Value::Object).collect()
}

/// `Locker::setLockData` without the write: the lock document.
#[allow(clippy::too_many_arguments)]
pub fn build_lock_data(
    content_hash: &str,
    packages: &[Package],
    dev_packages: &[Package],
    platform_reqs: &[(String, String)],
    platform_dev_reqs: &[(String, String)],
    aliases: &[AliasDef],
    minimum_stability: semver::Stability,
    stability_flags: &BTreeMap<String, u8>,
    prefer_stable: bool,
    prefer_lowest: bool,
    platform_overrides: &Map<String, Value>,
) -> Value {
    let mut lock = Map::new();
    lock.insert(
        "_readme".into(),
        serde_json::json!([
            "This file locks the dependencies of your project to a known state",
            "Read more about it at https://getcomposer.org/doc/01-basic-usage.md#installing-dependencies",
            "This file is @generated automatically"
        ]),
    );
    lock.insert(
        "content-hash".into(),
        Value::String(content_hash.to_string()),
    );
    lock.insert("packages".into(), Value::Array(lock_packages(packages)));
    lock.insert(
        "packages-dev".into(),
        Value::Array(lock_packages(dev_packages)),
    );
    let aliases: Vec<Value> = aliases
        .iter()
        .map(|a| {
            let version = if matches!(a.version.as_str(), "dev-master" | "dev-trunk" | "dev-default") {
                "9999999-dev"
            } else {
                &a.version
            };
            serde_json::json!({"package": a.package, "version": version, "alias": a.alias, "alias_normalized": a.alias_normalized})
        })
        .collect();
    lock.insert("aliases".into(), Value::Array(aliases));
    lock.insert(
        "minimum-stability".into(),
        Value::String(minimum_stability.as_str().to_string()),
    );
    let flags: Map<String, Value> = stability_flags
        .iter()
        .map(|(k, v)| (k.clone(), Value::from(*v)))
        .collect();
    lock.insert("stability-flags".into(), Value::Object(flags));
    lock.insert("prefer-stable".into(), Value::Bool(prefer_stable));
    lock.insert("prefer-lowest".into(), Value::Bool(prefer_lowest));
    let reqs = |list: &[(String, String)]| -> Value {
        Value::Object(
            list.iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )
    };
    lock.insert("platform".into(), reqs(platform_reqs));
    lock.insert("platform-dev".into(), reqs(platform_dev_reqs));
    if !platform_overrides.is_empty() {
        lock.insert(
            "platform-overrides".into(),
            Value::Object(platform_overrides.clone()),
        );
    }
    lock.insert(
        "plugin-api-version".into(),
        Value::String(crate::PLUGIN_API_VERSION.to_string()),
    );
    Value::Object(lock)
}

/// `JsonFile::write` of a lock document.
pub fn write_lock(path: &Path, lock: &Value) -> Result<(), String> {
    let json = crate::json::encode(lock, true, true, true) + "\n";
    std::fs::write(path, json).map_err(|e| format!("Could not write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_matches_composer() {
        // Hashes read from the composer.lock that Composer 2.11.1 wrote for these manifests.
        let manifest = r#"{
    "name": "acme/demo",
    "description": "x",
    "require": {
        "php": ">=8.1"
    },
    "require-dev": {},
    "config": {
        "sort-packages": true,
        "platform": {"php": "8.1.0"}
    },
    "extra": {"foo": {}, "list": {"0": "x", "1": "y"}},
    "minimum-stability": "dev",
    "prefer-stable": true
}"#;
        assert_eq!(
            content_hash(manifest).unwrap(),
            "8b79051b1db8d507ca4aea2674a5166f"
        );
        let manifest = r#"{"name":"a/b","extra":{"0":"x","1":"y"},"require":{"php":">=7"},"config":{"platform":{"php":"8.0.0"}}}"#;
        assert_eq!(
            content_hash(manifest).unwrap(),
            "12cd4f752e3485da149330842ff4048c"
        );
    }

    #[test]
    fn missing_requirements_are_reported_like_composer() {
        let lock = Lock::parse(
            r#"{"content-hash":"x","packages":[{"name":"psr/log","version":"3.0.0","replace":{"acme/old":"self.version"}}],
                "packages-dev":[],"aliases":[]}"#,
        )
        .unwrap();
        let root: Value = serde_json::from_str(
            r#"{"name":"acme/demo","require":{"psr/log":"^2.0","acme/old":"^1.0","foo/bar":"*","php":"^8"},"require-dev":{"psr/log":"^3"}}"#,
        )
        .unwrap();
        let root = crate::package::root::load_root(root.as_object().unwrap()).unwrap();
        let info = lock.missing_requirement_info(&root, true).unwrap();
        assert_eq!(
            info[0],
            "- Required package \"psr/log\" is in the lock file as \"3.0.0\" but that does not satisfy your constraint \"^2.0\"."
        );
        assert_eq!(
            info[1],
            "- Required package \"acme/old\" is in the lock file as \"replaced as self.version by psr/log 3.0.0\" but that does not satisfy your constraint \"^1.0\"."
        );
        assert_eq!(
            info[2],
            "- Required package \"foo/bar\" is not present in the lock file."
        );
        assert_eq!(info.len(), 6);
    }
}
