//! `vendor/composer/pcomposer.json`: the packages this project materialised from the store.

use crate::json::{Map, Value, encode};
use crate::package::Package;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    pub name: String,
    pub version: String,
    pub reference: String,
    pub strategy: String,
}

pub struct Manifest {
    pub path: PathBuf,
    recorded: BTreeMap<String, ManifestEntry>,
    /// installed through the plain path this run: never listed
    dropped: std::collections::BTreeSet<String>,
}

impl Manifest {
    pub fn in_vendor_dir(vendor_dir: &str) -> Manifest {
        Manifest {
            path: PathBuf::from(format!("{vendor_dir}/composer/pcomposer.json")),
            recorded: BTreeMap::new(),
            dropped: Default::default(),
        }
    }

    /// A package (re)installed outside the store: its entry goes, unless recorded again.
    pub fn forget(&mut self, name: &str) {
        self.recorded.remove(name);
        self.dropped.insert(name.to_string());
    }

    /// Remembers a package materialised during this run, with its effective strategy.
    pub fn record(&mut self, package: &Package, strategy: &str) {
        let dist = package.dist.as_ref();
        self.recorded.insert(
            package.name.clone(),
            ManifestEntry {
                name: package.name.clone(),
                version: package.pretty_version.clone(),
                reference: dist
                    .and_then(|d| d.reference.clone())
                    .or_else(|| dist.and_then(|d| d.shasum.clone()))
                    .unwrap_or_default(),
                strategy: strategy.to_string(),
            },
        );
    }

    pub fn read(&self) -> BTreeMap<String, ManifestEntry> {
        Self::read_at(&self.path)
    }

    pub fn read_at(path: &Path) -> BTreeMap<String, ManifestEntry> {
        let Ok(json) = std::fs::read_to_string(path) else {
            return BTreeMap::new();
        };
        let Ok(Value::Object(entries)) = serde_json::from_str::<Value>(&json) else {
            return BTreeMap::new();
        };
        entries
            .iter()
            .map(|(name, e)| {
                let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                (
                    name.clone(),
                    ManifestEntry {
                        name: s("name"),
                        version: s("version"),
                        reference: s("reference"),
                        strategy: s("strategy"),
                    },
                )
            })
            .collect()
    }

    /// Merges this run's records over the file and drops packages no longer installed at the recorded version.
    pub fn write(&self, installed: &[Package]) -> Result<(), String> {
        // source checkouts never come from the store
        let versions: BTreeMap<&str, &str> = installed
            .iter()
            .filter(|p| p.installation_source.as_deref() != Some("source"))
            .map(|p| (p.name.as_str(), p.pretty_version.as_str()))
            .collect();
        let mut entries = self.read();
        entries.retain(|name, _| !self.dropped.contains(name));
        for (name, entry) in &self.recorded {
            entries.insert(name.clone(), entry.clone());
        }
        entries.retain(|name, entry| {
            versions
                .get(name.as_str())
                .is_some_and(|v| *v == entry.version)
        });
        self.save(&entries)
    }

    /// Rewrites the strategy of an existing entry in place.
    pub fn set_strategy(&self, name: &str, strategy: &str) -> Result<(), String> {
        let mut entries = self.read();
        let entry = entries
            .get_mut(name)
            .ok_or_else(|| format!("{name} is not in the manifest"))?;
        entry.strategy = strategy.to_string();
        self.save(&entries)
    }

    fn save(&self, entries: &BTreeMap<String, ManifestEntry>) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            crate::fs::ensure_dir(dir)?;
        }
        let mut object = Map::new();
        for (name, e) in entries {
            let mut m = Map::new();
            m.insert("name".into(), Value::String(e.name.clone()));
            m.insert("version".into(), Value::String(e.version.clone()));
            m.insert("reference".into(), Value::String(e.reference.clone()));
            m.insert("strategy".into(), Value::String(e.strategy.clone()));
            object.insert(name.clone(), Value::Object(m));
        }
        let json = encode(&Value::Object(object), true, true, false) + "\n";
        std::fs::write(&self.path, json)
            .map_err(|e| format!("Cannot write {}: {e}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_pcomposer_shape_and_drops_stale_entries() {
        let dir = std::env::temp_dir().join(format!("romposer-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut manifest = Manifest::in_vendor_dir(dir.to_str().unwrap());
        let a: Value = serde_json::json!({"name":"acme/lib","version":"1.2.0","dist":{"type":"zip","url":"u","reference":"0123abcd"}});
        let a = crate::package::load(a.as_object().unwrap()).unwrap();
        manifest.record(&a, "hardlink");
        manifest.write(std::slice::from_ref(&a)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&manifest.path).unwrap(),
            "{\n    \"acme/lib\": {\n        \"name\": \"acme/lib\",\n        \"version\": \"1.2.0\",\n        \"reference\": \"0123abcd\",\n        \"strategy\": \"hardlink\"\n    }\n}\n"
        );
        // a later run without the package drops it; an empty manifest is `{}`
        let other = Manifest::in_vendor_dir(dir.to_str().unwrap());
        other.write(&[]).unwrap();
        assert_eq!(std::fs::read_to_string(&manifest.path).unwrap(), "{}\n");
        manifest.write(&[a]).unwrap();
        other.set_strategy("acme/lib", "copy").unwrap();
        assert_eq!(other.read()["acme/lib"].strategy, "copy");
    }
}
