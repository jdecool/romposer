//! pcomposer's settings: `extra.pcomposer.{enabled,strategy,copy}`, `extra.patches`
//! keys, overridden by `PCOMPOSER_DISABLE`, `PCOMPOSER_STRATEGY`, `PCOMPOSER_STORE`.

use crate::json::{Map, Value};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    Hardlink,
    Reflink,
    Copy,
}

impl Strategy {
    pub fn parse(s: &str) -> Result<Strategy, String> {
        match s {
            "hardlink" => Ok(Strategy::Hardlink),
            "reflink" => Ok(Strategy::Reflink),
            "copy" => Ok(Strategy::Copy),
            other => Err(format!(
                "pcomposer: invalid strategy \"{other}\", expected one of hardlink, reflink, copy"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Hardlink => "hardlink",
            Strategy::Reflink => "reflink",
            Strategy::Copy => "copy",
        }
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreConfig {
    pub enabled: bool,
    pub strategy: Strategy,
    /// package names always materialised with `copy`
    pub copy: Vec<String>,
    /// `PCOMPOSER_STORE` or `<cache-dir>/pcomposer-store`
    pub root: String,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl StoreConfig {
    /// `Config::fromComposer` plus the store location.
    pub fn from_root(extra: &Map<String, Value>, cache_dir: &str) -> Result<StoreConfig, String> {
        let settings = match extra.get("pcomposer") {
            Some(Value::Object(o)) => o.clone(),
            _ => Map::new(),
        };
        let strategy = match env("PCOMPOSER_STRATEGY") {
            Some(s) => s,
            None => match settings.get("strategy") {
                None | Some(Value::Null) => "hardlink".to_string(),
                Some(Value::String(s)) => s.clone(),
                Some(other) => crate::json::encode_compact(other),
            },
        };
        let mut copy: Vec<String> = match settings.get("copy") {
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            Some(Value::Object(map)) => map
                .values()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => vec![],
        };
        if let Some(Value::Object(patches)) = extra.get("patches") {
            copy.extend(patches.keys().cloned());
        }
        let enabled = env("PCOMPOSER_DISABLE").is_none()
            && settings.get("enabled") != Some(&Value::Bool(false));
        Ok(StoreConfig {
            enabled,
            strategy: Strategy::parse(&strategy)?,
            copy,
            root: env("PCOMPOSER_STORE").unwrap_or_else(|| format!("{cache_dir}/pcomposer-store")),
        })
    }

    /// Plugins and listed packages are always copied; the others use the configured strategy.
    pub fn strategy_for(&self, name: &str, kind: &str) -> Strategy {
        if kind == "composer-plugin" || self.copy.iter().any(|c| c == name) {
            Strategy::Copy
        } else {
            self.strategy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_extra_and_patches() {
        let extra: Value = serde_json::json!({"pcomposer": {"strategy": "reflink", "copy": ["a/b"]}, "patches": {"c/d": {"fix": "x.patch"}}});
        let config = StoreConfig::from_root(extra.as_object().unwrap(), "/cache").unwrap();
        assert!(config.enabled);
        assert_eq!(config.strategy, Strategy::Reflink);
        assert_eq!(config.strategy_for("a/b", "library"), Strategy::Copy);
        assert_eq!(config.strategy_for("c/d", "library"), Strategy::Copy);
        assert_eq!(
            config.strategy_for("e/f", "composer-plugin"),
            Strategy::Copy
        );
        assert_eq!(config.strategy_for("e/f", "library"), Strategy::Reflink);
        assert_eq!(config.root, "/cache/pcomposer-store");
        let bad: Value = serde_json::json!({"pcomposer": {"strategy": "symlink"}});
        assert_eq!(
            StoreConfig::from_root(bad.as_object().unwrap(), "/cache").unwrap_err(),
            "pcomposer: invalid strategy \"symlink\", expected one of hardlink, reflink, copy"
        );
        let disabled: Value = serde_json::json!({"pcomposer": {"enabled": false}});
        assert!(
            !StoreConfig::from_root(disabled.as_object().unwrap(), "/cache")
                .unwrap()
                .enabled
        );
    }
}
