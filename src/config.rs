//! Port of `Composer\Config` and the `Factory` path lookups: home, cache and
//! data directories, `config.json`, `auth.json`, `COMPOSER_AUTH`, `{$ref}`
//! expansion and the `COMPOSER_*` environment overrides.

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

pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn use_xdg() -> bool {
    std::env::vars().any(|(k, _)| k.starts_with("XDG_"))
}

fn user_dir() -> Result<String, String> {
    env("HOME")
        .map(|h| h.trim_end_matches('/').to_string())
        .ok_or_else(|| "The HOME or COMPOSER_HOME environment variable must be set for composer to run correctly".to_string())
}

/// `Factory::getHomeDir` on macOS and Linux.
pub fn home_dir() -> Result<String, String> {
    if let Some(home) = env("COMPOSER_HOME") {
        return Ok(home);
    }
    let user = user_dir()?;
    let mut dirs = Vec::new();
    if use_xdg() {
        let xdg = env("XDG_CONFIG_HOME").unwrap_or_else(|| format!("{user}/.config"));
        dirs.push(format!("{xdg}/composer"));
    }
    dirs.push(format!("{user}/.composer"));
    Ok(dirs
        .iter()
        .find(|d| Path::new(d).is_dir())
        .cloned()
        .unwrap_or_else(|| dirs[0].clone()))
}

/// `Factory::getCacheDir`
pub fn cache_dir(home: &str) -> Result<String, String> {
    if let Some(dir) = env("COMPOSER_CACHE_DIR") {
        return Ok(dir);
    }
    if let Some(home) = env("COMPOSER_HOME") {
        return Ok(format!("{home}/cache"));
    }
    let user = user_dir()?;
    if cfg!(target_os = "macos") {
        let caches = format!("{user}/Library/Caches/composer");
        if Path::new(&format!("{home}/cache")).is_dir() && !Path::new(&caches).is_dir() {
            let _ = std::fs::rename(format!("{home}/cache"), &caches);
        }
        return Ok(caches);
    }
    if home == format!("{user}/.composer") && Path::new(&format!("{home}/cache")).is_dir() {
        return Ok(format!("{home}/cache"));
    }
    if use_xdg() {
        let xdg = env("XDG_CACHE_HOME").unwrap_or_else(|| format!("{user}/.cache"));
        return Ok(format!("{xdg}/composer"));
    }
    Ok(format!("{home}/cache"))
}

/// `Factory::getDataDir`
pub fn data_dir(home: &str) -> Result<String, String> {
    if let Some(home) = env("COMPOSER_HOME") {
        return Ok(home);
    }
    let user = user_dir()?;
    if home != format!("{user}/.composer") && use_xdg() {
        let xdg = env("XDG_DATA_HOME").unwrap_or_else(|| format!("{user}/.local/share"));
        return Ok(format!("{xdg}/composer"));
    }
    Ok(home.to_string())
}

const AUTH_KEYS: [&str; 9] = [
    "bitbucket-oauth",
    "github-oauth",
    "gitlab-oauth",
    "gitlab-token",
    "http-basic",
    "bearer",
    "custom-headers",
    "client-certificate",
    "forgejo-token",
];

/// A credential as `IOInterface::getAuthentication` returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Auth {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub config: Map<String, Value>,
    pub repositories: Vec<(String, Value)>,
    /// Directory relative paths resolve against (the project directory).
    pub base_dir: Option<String>,
}

/// `Config::$defaultConfig`, in Composer's key order (`config --list` prints it).
const DEFAULTS: &str = r#"{
    "process-timeout": 300,
    "use-include-path": false,
    "allow-plugins": {},
    "use-parent-dir": "prompt",
    "preferred-install": "dist",
    "audit": {"ignore": [], "abandoned": "fail"},
    "policy": true,
    "notify-on-install": true,
    "github-protocols": ["https", "ssh", "git"],
    "gitlab-protocol": null,
    "vendor-dir": "vendor",
    "bin-dir": "{$vendor-dir}/bin",
    "cache-dir": "{$home}/cache",
    "data-dir": "{$home}",
    "cache-files-dir": "{$cache-dir}/files",
    "cache-repo-dir": "{$cache-dir}/repo",
    "cache-vcs-dir": "{$cache-dir}/vcs",
    "cache-ttl": 15552000,
    "cache-files-ttl": null,
    "cache-files-maxsize": "300MiB",
    "cache-read-only": false,
    "bin-compat": "auto",
    "discard-changes": false,
    "autoloader-suffix": null,
    "sort-packages": false,
    "optimize-autoloader": false,
    "strict-psr-autoloader": false,
    "classmap-authoritative": false,
    "apcu-autoloader": false,
    "prepend-autoloader": true,
    "update-with-minimal-changes": false,
    "github-domains": ["github.com"],
    "bitbucket-expose-hostname": true,
    "disable-tls": false,
    "secure-http": true,
    "secure-svn-domains": [],
    "cafile": null,
    "capath": null,
    "github-expose-hostname": true,
    "gitlab-domains": ["gitlab.com"],
    "store-auths": "prompt",
    "platform": {},
    "archive-format": "tar",
    "archive-dir": ".",
    "htaccess-protect": true,
    "use-github-api": true,
    "lock": true,
    "platform-check": "php-only",
    "bitbucket-oauth": {},
    "github-oauth": {},
    "gitlab-oauth": {},
    "gitlab-token": {},
    "http-basic": {},
    "bearer": {},
    "custom-headers": {},
    "bump-after-update": false,
    "allow-missing-requirements": false,
    "client-certificate": {},
    "forgejo-domains": ["codeberg.org"],
    "forgejo-token": {},
    "source-fallback": false
}"#;

fn defaults() -> Map<String, Value> {
    match serde_json::from_str(DEFAULTS).expect("defaults are valid JSON") {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

impl Config {
    /// `FactoryMock::createConfig`: defaults with a throwaway home and no packagist, nothing read from disk.
    pub fn in_memory() -> Config {
        let mut config = Config {
            config: defaults(),
            repositories: vec![],
            base_dir: None,
        };
        let home = std::env::temp_dir()
            .join("romposer-in-memory-home")
            .to_string_lossy()
            .into_owned();
        let mut dirs = Map::new();
        dirs.insert("home".into(), Value::String(home.clone()));
        dirs.insert("cache-dir".into(), Value::String(format!("{home}/cache")));
        dirs.insert("data-dir".into(), Value::String(home));
        config.merge_config(&dirs);
        config
    }

    /// `Factory::createConfig`: defaults, dirs, global config.json and auth.json, COMPOSER_AUTH.
    pub fn create(cwd: &str) -> Result<Config, String> {
        let mut config = Config {
            config: defaults(),
            repositories: vec![(
                "packagist.org".to_string(),
                serde_json::json!({"type": "composer", "url": "https://repo.packagist.org"}),
            )],
            base_dir: Some(cwd.to_string()),
        };
        let home = home_dir()?;
        let mut dirs = Map::new();
        dirs.insert("home".into(), Value::String(home.clone()));
        dirs.insert("cache-dir".into(), Value::String(cache_dir(&home)?));
        dirs.insert("data-dir".into(), Value::String(data_dir(&home)?));
        config.merge_config(&dirs);

        let global = Path::new(&home).join("config.json");
        if global.exists() {
            let text = std::fs::read_to_string(&global)
                .map_err(|e| format!("Could not read {}\n\n{e}", global.display()))?;
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                format!("\"{}\" does not contain valid JSON\n{e}", global.display())
            })?;
            if let Value::Object(map) = value {
                config.merge(&map);
            }
        }

        if config.bool("htaccess-protect") {
            for dir in [
                config.get_str("home"),
                config.get_str("cache-dir"),
                config.get_str("data-dir"),
            ] {
                let htaccess = Path::new(&dir).join(".htaccess");
                if !htaccess.exists() {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(&htaccess, "Deny from all");
                }
            }
        }

        let auth = Path::new(&home).join("auth.json");
        config.merge_auth_file(&auth)?;
        config.load_composer_auth_env()?;
        Ok(config)
    }

    /// `Factory::createComposer`'s config step: project `config`, local auth.json, env.
    pub fn for_project(
        cwd: &str,
        manifest: &Map<String, Value>,
        composer_file: &Path,
    ) -> Result<Config, String> {
        let mut config = Config::create(cwd)?;
        config.merge(manifest);
        let dir = composer_file.parent().unwrap_or(Path::new("."));
        let local_auth = if dir.as_os_str().is_empty() {
            PathBuf::from("auth.json")
        } else {
            dir.join("auth.json")
        };
        config.merge_auth_file(&local_auth)?;
        config.load_composer_auth_env()?;
        Ok(config)
    }

    fn merge_auth_file(&mut self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Could not read {}\n\n{e}", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| format!("\"{}\" does not contain valid JSON\n{e}", path.display()))?;
        if let Value::Object(map) = value {
            self.merge_config(&map);
        }
        Ok(())
    }

    fn load_composer_auth_env(&mut self) -> Result<(), String> {
        let Some(raw) = env("COMPOSER_AUTH") else {
            return Ok(());
        };
        let value: Value = serde_json::from_str(&raw).map_err(|_| {
            "COMPOSER_AUTH environment variable is malformed, should be a valid JSON object"
                .to_string()
        })?;
        if let Value::Object(map) = value {
            self.merge_config(&map);
        }
        Ok(())
    }

    /// `Config::merge` for a `{"config": ..., "repositories": ...}` document.
    pub fn merge(&mut self, data: &Map<String, Value>) {
        if let Some(Value::Object(config)) = data.get("config") {
            self.merge_config(config);
        }
        if let Some(repos) = data.get("repositories") {
            let entries: Vec<(String, Value)> = match repos {
                Value::Array(list) => list
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i.to_string(), v.clone()))
                    .collect(),
                Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                _ => vec![],
            };
            // later definitions take precedence (unshift)
            for (name, repo) in entries.into_iter().rev() {
                if repo == Value::Bool(false)
                    || matches!(&repo, Value::Object(o) if o.values().any(|v| *v == Value::Bool(false)) && o.len() == 1)
                {
                    let disabled = match &repo {
                        Value::Object(o) => o.keys().next().cloned().unwrap_or(name.clone()),
                        _ => name.clone(),
                    };
                    self.repositories.retain(|(n, _)| {
                        *n != disabled && !(disabled == "packagist" && n == "packagist.org")
                    });
                    continue;
                }
                self.repositories.retain(|(n, _)| *n != name);
                self.repositories.insert(0, (name, repo));
            }
        }
    }

    /// The `config` key part of `Config::merge`.
    pub fn merge_config(&mut self, values: &Map<String, Value>) {
        for (key, val) in values {
            if AUTH_KEYS.contains(&key.as_str())
                && let (Some(Value::Object(existing)), Value::Object(new)) =
                    (self.config.get_mut(key), val)
            {
                for (k, v) in new {
                    existing.insert(k.clone(), v.clone());
                }
                continue;
            }
            if matches!(key.as_str(), "gitlab-domains" | "github-domains")
                && let (Some(Value::Array(existing)), Value::Array(new)) =
                    (self.config.get_mut(key), val)
            {
                for v in new {
                    if !existing.contains(v) {
                        existing.push(v.clone());
                    }
                }
                continue;
            }
            if key == "audit"
                && let (Some(Value::Object(existing)), Value::Object(new)) =
                    (self.config.get_mut(key), val)
            {
                let mut ignores = existing
                    .get("ignore")
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                for (k, v) in new {
                    existing.insert(k.clone(), v.clone());
                }
                // `array_merge($currentIgnores, $val['ignore'])`
                ignores = match (ignores, new.get("ignore")) {
                    (cur, None) => cur,
                    (Value::Array(cur), Some(add)) if cur.is_empty() => add.clone(),
                    (Value::Array(mut cur), Some(Value::Array(add))) => {
                        cur.extend(add.iter().cloned());
                        Value::Array(cur)
                    }
                    (cur, Some(add)) => {
                        // ponytail: string keys win, list items get their index as key
                        let mut merged = Map::new();
                        for value in [&cur, add] {
                            match value {
                                Value::Array(list) => {
                                    for v in list {
                                        merged.insert(merged.len().to_string(), v.clone());
                                    }
                                }
                                Value::Object(map) => {
                                    for (k, v) in map {
                                        merged.insert(k.clone(), v.clone());
                                    }
                                }
                                _ => {}
                            }
                        }
                        Value::Object(merged)
                    }
                };
                existing.insert("ignore".into(), ignores);
                continue;
            }
            self.config.insert(key.clone(), val.clone());
        }
    }

    fn composer_env(key: &str) -> Option<String> {
        env(&format!(
            "COMPOSER_{}",
            key.to_uppercase().replace('-', "_")
        ))
    }

    fn process(&self, value: &str) -> String {
        let re = regex::Regex::new(r"\{\$(.+?)\}").unwrap();
        re.replace_all(value, |caps: &regex::Captures<'_>| self.get_str(&caps[1]))
            .into_owned()
    }

    fn realpath(&self, path: &str) -> String {
        if regex::Regex::new(r"(?i)^(?:/|[a-z]:|[a-z0-9.]+://|\\\\)")
            .unwrap()
            .is_match(path)
        {
            return path.to_string();
        }
        match &self.base_dir {
            Some(base) => format!("{base}/{path}"),
            None => path.to_string(),
        }
    }

    /// `Config::get` for string and path keys.
    pub fn get_str(&self, key: &str) -> String {
        match key {
            "vendor-dir" | "bin-dir" | "data-dir" | "cache-dir" | "cache-files-dir"
            | "cache-repo-dir" | "cache-vcs-dir" | "cafile" | "capath" => {
                let raw = Self::composer_env(key).or_else(|| {
                    self.config
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
                let Some(raw) = raw else { return String::new() };
                let val = self.process(&raw);
                let val = expand_path(val.trim_end_matches(['/', '\\']));
                if !key.ends_with("-dir") {
                    return val;
                }
                self.realpath(&val)
            }
            "home" => expand_path(
                self.config
                    .get("home")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
            .trim_end_matches(['/', '\\'])
            .to_string(),
            "bin-compat" => Self::composer_env("COMPOSER_BIN_COMPAT")
                .or_else(|| {
                    self.config
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "auto".into()),
            _ => match self.config.get(key) {
                Some(Value::String(s)) => self.process(s),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            },
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.config.get(key).filter(|v| !v.is_null())
    }

    /// `Config::get` for the boolean keys.
    pub fn bool(&self, key: &str) -> bool {
        let value = match key {
            "cache-read-only" | "htaccess-protect" => match Self::composer_env(key) {
                Some(v) => return v != "false" && !(v.is_empty() || v == "0"),
                None => self.config.get(key),
            },
            "secure-http" if self.bool("disable-tls") => return false,
            _ => self.config.get(key),
        };
        match value {
            Some(Value::Bool(b)) => *b,
            Some(Value::Null) | None => false,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::String(s)) => s != "false" && !(s.is_empty() || s == "0"),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
        }
    }

    pub fn int(&self, key: &str) -> i64 {
        match key {
            "cache-files-ttl" => match self.config.get(key) {
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0).max(0),
                Some(Value::String(s)) => s.parse().unwrap_or(0),
                _ => self.int("cache-ttl"),
            },
            "process-timeout" => match Self::composer_env(key) {
                Some(v) => v.trim().parse::<i64>().unwrap_or(0).max(0),
                None => match self.config.get(key) {
                    Some(Value::Number(n)) => n.as_i64().unwrap_or(0).max(0),
                    _ => 0,
                },
            },
            _ => match self.config.get(key) {
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
                Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
                Some(Value::Bool(true)) => 1,
                _ => 0,
            },
        }
    }

    pub fn string_list(&self, key: &str) -> Vec<String> {
        match self.config.get(key) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => vec![],
        }
    }

    pub fn vendor_dir(&self) -> String {
        self.get_str("vendor-dir")
    }

    /// `BaseIO::loadConfiguration` + `IOInterface::getAuthentication`, including
    /// `AuthHelper::findAuthOrigin`'s api.* fallback.
    pub fn authentication(&self, origin: &str) -> Option<Auth> {
        let lookup = |origin: &str| -> Option<Auth> {
            let obj = |key: &str| self.config.get(key).and_then(Value::as_object);
            if let Some(cred) = obj("http-basic").and_then(|m| m.get(origin)) {
                return Some(Auth {
                    username: cred
                        .get("username")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    password: cred
                        .get("password")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                });
            }
            if let Some(token) = obj("bearer")
                .and_then(|m| m.get(origin))
                .and_then(Value::as_str)
            {
                return Some(Auth {
                    username: token.into(),
                    password: "bearer".into(),
                });
            }
            if let Some(cred) = obj("forgejo-token").and_then(|m| m.get(origin)) {
                return Some(Auth {
                    username: cred
                        .get("username")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    password: cred
                        .get("token")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                });
            }
            if let Some(token) = obj("gitlab-token").and_then(|m| m.get(origin)) {
                return Some(match token {
                    Value::Object(t) => Auth {
                        username: t
                            .get("username")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        password: t.get("token").and_then(Value::as_str).unwrap_or("").into(),
                    },
                    other => Auth {
                        username: other.as_str().unwrap_or("").into(),
                        password: "private-token".into(),
                    },
                });
            }
            if let Some(token) = obj("gitlab-oauth").and_then(|m| m.get(origin)) {
                let token = match token {
                    Value::Object(t) => t.get("token").and_then(Value::as_str).unwrap_or(""),
                    other => other.as_str().unwrap_or(""),
                };
                return Some(Auth {
                    username: token.into(),
                    password: "oauth2".into(),
                });
            }
            if let Some(token) = obj("github-oauth")
                .and_then(|m| m.get(origin))
                .and_then(Value::as_str)
            {
                return Some(Auth {
                    username: token.into(),
                    password: "x-oauth-basic".into(),
                });
            }
            if let Some(cred) = obj("bitbucket-oauth").and_then(|m| m.get(origin)) {
                return Some(Auth {
                    username: cred
                        .get("consumer-key")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    password: cred
                        .get("consumer-secret")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                });
            }
            None
        };
        if let Some(auth) = lookup(origin) {
            return Some(auth);
        }
        if origin == "api.bitbucket.org" || origin == "api.github.com" {
            return lookup(&origin.replacen("api.", "", 1));
        }
        None
    }

    /// Domains where `github-oauth` / `gitlab-token` credentials apply, including
    /// the implicit ones Composer adds when auth is configured for them.
    pub fn domains(&self, key: &str) -> Vec<String> {
        let mut domains = self.string_list(key);
        let auth_keys: &[&str] = match key {
            "github-domains" => &["github-oauth"],
            "gitlab-domains" => &["gitlab-oauth", "gitlab-token"],
            _ => &[],
        };
        for auth_key in auth_keys {
            if let Some(Value::Object(m)) = self.config.get(*auth_key) {
                for d in m.keys() {
                    if !domains.contains(d) {
                        domains.push(d.clone());
                    }
                }
            }
        }
        domains
    }
}

/// `Platform::expandPath`: `~` and `$VAR` expansion.
pub fn expand_path(path: &str) -> String {
    let path = if let Some(rest) = path.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            format!("{}{rest}", env("HOME").unwrap_or_default())
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };
    let re = regex::Regex::new(r"^(?:\$(\w+)|%(\w+)%)(?:[/\\]|$)").unwrap();
    match re.captures(&path) {
        Some(caps) => {
            let name = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map_or("", |m| m.as_str());
            format!(
                "{}{}",
                env(name).unwrap_or_default(),
                &path[caps.get(0).unwrap().end().min(path.len())
                    - usize::from(caps[0].ends_with(['/', '\\']))..]
            )
        }
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_env_and_auth() {
        let manifest: Value = serde_json::json!({
            "config": {"vendor-dir": "lib", "bin-dir": "{$vendor-dir}/tools", "http-basic": {"example.com": {"username": "u", "password": "p"}}, "github-oauth": {"ghe.corp": "tok"}}
        });
        let mut config = Config {
            config: defaults(),
            repositories: vec![],
            base_dir: Some("/proj".into()),
        };
        config.merge(manifest.as_object().unwrap());
        assert_eq!(config.vendor_dir(), "/proj/lib");
        assert_eq!(config.get_str("bin-dir"), "/proj/lib/tools");
        assert_eq!(
            config.authentication("example.com"),
            Some(Auth {
                username: "u".into(),
                password: "p".into()
            })
        );
        assert_eq!(
            config.authentication("ghe.corp"),
            Some(Auth {
                username: "tok".into(),
                password: "x-oauth-basic".into()
            })
        );
        assert!(
            config
                .domains("github-domains")
                .contains(&"ghe.corp".to_string())
        );
        assert!(config.bool("lock"));
        assert_eq!(config.int("cache-files-ttl"), 15552000);
    }
}
