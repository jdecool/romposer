//! The subset of `Composer\Config` and `Factory` path lookups needed so far:
//! composer file, lock file, home directory, and merged `config` keys.

use crate::json::{Map, Value};
use std::path::{Path, PathBuf};

/// `Factory::getComposerFile`
pub fn composer_file() -> Result<PathBuf, String> {
    if let Ok(env) = std::env::var("COMPOSER") {
        let env = env.trim();
        if !env.is_empty() {
            if Path::new(env).is_dir() {
                return Err(format!(
                    "The COMPOSER environment variable is set to {env} which is a directory, this variable should point to a composer.json or be left unset."
                ));
            }
            return Ok(PathBuf::from(env));
        }
    }
    Ok(PathBuf::from("./composer.json"))
}

/// `Factory::getLockFile`
pub fn lock_file(composer_file: &Path) -> PathBuf {
    let s = composer_file.to_string_lossy();
    if let Some(stem) = s.strip_suffix(".json") {
        PathBuf::from(format!("{stem}.lock"))
    } else {
        PathBuf::from(format!("{s}.lock"))
    }
}

/// `Factory::getHomeDir` on macOS and Linux.
pub fn home_dir() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("COMPOSER_HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home));
    }
    let user_dir = std::env::var("HOME").map_err(|_| "The HOME or COMPOSER_HOME environment variable must be set for composer to run correctly")?;
    let user_dir = user_dir.trim_end_matches('/');
    let mut dirs = Vec::new();
    let use_xdg = std::env::vars().any(|(k, _)| k.starts_with("XDG_"));
    if use_xdg {
        let xdg = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{user_dir}/.config"));
        dirs.push(PathBuf::from(format!("{xdg}/composer")));
    }
    dirs.push(PathBuf::from(format!("{user_dir}/.composer")));
    Ok(dirs
        .iter()
        .find(|d| d.is_dir())
        .cloned()
        .unwrap_or_else(|| dirs[0].clone()))
}

/// Merged configuration: Composer's defaults, then `$COMPOSER_HOME/config.json`,
/// then the project's `config` key. Only the keys read so far are typed.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub values: Map<String, Value>,
}

impl Config {
    pub fn load(project_config: Option<&Map<String, Value>>) -> Config {
        let mut values = Map::new();
        values.insert("lock".into(), Value::Bool(true));
        values.insert("vendor-dir".into(), Value::String("vendor".into()));
        if let Ok(home) = home_dir()
            && let Ok(text) = std::fs::read_to_string(home.join("config.json"))
            && let Ok(Value::Object(global)) = serde_json::from_str::<Value>(&text)
            && let Some(Value::Object(config)) = global.get("config")
        {
            for (k, v) in config {
                values.insert(k.clone(), v.clone());
            }
        }
        if let Some(project) = project_config {
            for (k, v) in project {
                values.insert(k.clone(), v.clone());
            }
        }
        Config { values }
    }

    pub fn bool(&self, key: &str) -> bool {
        match self.values.get(key) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Null) | None => false,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::String(s)) => !(s.is_empty() || s == "0"),
            Some(other) => !matches!(other, Value::Array(a) if a.is_empty()),
        }
    }

    pub fn string(&self, key: &str) -> Option<String> {
        self.values
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn vendor_dir(&self) -> String {
        self.string("vendor-dir").unwrap_or_else(|| "vendor".into())
    }
}
