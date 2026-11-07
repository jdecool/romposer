//! `composer config` (`ConfigCommand`, `BaseConfigCommand`) and the `JsonConfigSource`
//! edits it makes to composer.json, config.json and auth.json.
//!
//! ponytail: `--editor` is rejected and `--source` is not ported (Composer tracks the
//! file every value came from; romposer's `Config` does not).

use crate::config::{self, Config, expand_path};
use crate::io::Io;
use crate::json::manipulator::Manipulator;
use crate::json::{Map, Value};
use crate::lock::php_assoc;
use regex::Regex;
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Setting key
    pub setting_key: Option<String>,
    /// Setting value
    #[arg(allow_hyphen_values = true)]
    pub setting_value: Vec<String>,
    /// Apply command to the global config file
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Open editor
    #[arg(short = 'e', long)]
    pub editor: bool,
    /// Affect auth config file (only used for --editor)
    #[arg(short = 'a', long)]
    pub auth: bool,
    /// Unset the given setting-key
    #[arg(long)]
    pub unset: bool,
    /// List configuration settings
    #[arg(short = 'l', long)]
    pub list: bool,
    /// If you want to choose a different composer.json or config.json
    #[arg(short = 'f', long)]
    pub file: Option<String>,
    /// Returns absolute paths when fetching *-dir config values instead of relative
    #[arg(long)]
    pub absolute: bool,
    /// JSON decode the setting value, only supported for extra.*, policy.*.ignore, policy.advisories.ignore-id, policy.ignore-unreachable and audit.ignore[-abandoned] keys
    #[arg(short = 'j', long)]
    pub json: bool,
    /// Merge the setting value with the current value, only supported for extra.* keys in combination with --json, and for policy.*.ignore, policy.advisories.ignore-id and audit.ignore[-abandoned] keys
    #[arg(short = 'm', long)]
    pub merge: bool,
    /// When adding a repository, append it (lowest priority) to the existing ones instead of prepending it (highest priority)
    #[arg(long)]
    pub append: bool,
}

pub fn run(args: &Args, io: &mut Io) -> i32 {
    match execute(args, io) {
        Ok(code) => code,
        Err(message) => crate::scripts::exit_code(io, &message),
    }
}

// --- JsonConfigSource ---------------------------------------------------------------

/// One edit of a config file, `JsonConfigSource`'s methods.
pub enum Edit<'a> {
    AddRepository(&'a str, Option<&'a Value>, bool),
    RemoveRepository(&'a str),
    AddConfig(&'a str, &'a Value),
    RemoveConfig(&'a str),
    AddProperty(&'a str, &'a Value),
    RemoveProperty(&'a str),
    AddLink(&'a str, &'a str, &'a str),
}

const AUTH_PREFIX: &str = r"^(bitbucket-oauth|github-oauth|gitlab-oauth|gitlab-token|bearer|http-basic|custom-headers|forgejo-token|platform)\.";

/// `JsonConfigSource`: `auth` files hold the auth keys at their root.
pub struct ConfigSource {
    pub path: PathBuf,
    pub auth: bool,
}

impl ConfigSource {
    pub fn new(path: impl Into<PathBuf>, auth: bool) -> ConfigSource {
        ConfigSource {
            path: path.into(),
            auth,
        }
    }

    /// `manipulateJson`: a clean edit through the manipulator, else a full rewrite; the
    /// result must pass the lax schema or the original content is restored.
    pub fn edit(&self, edit: Edit<'_>) -> Result<(), String> {
        let exists = self.path.exists();
        let contents = if exists {
            std::fs::read_to_string(&self.path)
                .map_err(|_| format!("The file \"{}\" is not readable.", self.path.display()))?
        } else if self.auth {
            "{\n}\n".to_string()
        } else {
            "{\n    \"config\": {\n    }\n}\n".to_string()
        };
        let mut m = Manipulator::new(&contents)?;
        let clean = match &edit {
            Edit::AddConfig(name, value) if self.auth => match name.split_once('.') {
                Some((main, sub)) => m.add_sub_node(main, sub, value),
                None => false,
            },
            Edit::RemoveConfig(name) if self.auth => match name.split_once('.') {
                Some((main, sub)) => m.remove_sub_node(main, sub),
                None => false,
            },
            Edit::AddRepository(name, config, append) => m.add_repository(name, *config, *append),
            Edit::RemoveRepository(name) => m.remove_repository(name),
            Edit::AddConfig(name, value) => m.add_config_setting(name, value),
            Edit::RemoveConfig(name) => m.remove_config_setting(name),
            Edit::AddProperty(name, value) => m.add_property(name, value),
            Edit::RemoveProperty(name) => m.remove_property(name),
            Edit::AddLink(kind, name, value) => m.add_link(kind, name, value, false),
        };
        let written = if clean {
            m.contents()
        } else {
            let data: Value = serde_json::from_str(&contents).map_err(|e| {
                format!(
                    "\"{}\" does not contain valid JSON\n{e}",
                    self.path.display()
                )
            })?;
            let mut data = match php_assoc(data) {
                Value::Object(map) => map,
                _ => Map::new(),
            };
            self.fallback(&mut data, &edit);
            restore_empty_objects(&mut data);
            crate::json::encode_pretty(&Value::Object(data)) + "\n"
        };
        if exists {
            std::fs::write(&self.path, &written)
        } else {
            write_private(&self.path, &written)
        }
        .map_err(|_| format!("The file \"{}\" is not writable.", self.path.display()))?;
        let parsed: Value = serde_json::from_str(&written).unwrap_or(Value::Null);
        if let Err(errors) = crate::schema::validate(&parsed, false) {
            let _ = std::fs::write(&self.path, &contents);
            return Err(format!(
                "Failed to update composer.json with a valid format, reverting to the original content. Please report an issue to us with details (command you run and a copy of your composer.json). \n{}",
                errors.join("\n")
            ));
        }
        Ok(())
    }

    /// The fallback closures of `JsonConfigSource`, on the decoded file.
    fn fallback(&self, data: &mut Map<String, Value>, edit: &Edit<'_>) {
        let auth_key = Regex::new(AUTH_PREFIX).unwrap();
        match *edit {
            Edit::AddConfig(key, _) | Edit::RemoveConfig(key) => {
                let value = match edit {
                    Edit::AddConfig(_, v) => Some((*v).clone()),
                    _ => None,
                };
                let path: Vec<&str> = if auth_key.is_match(key) {
                    let (k, host) = key.split_once('.').unwrap();
                    if self.auth {
                        vec![k, host]
                    } else {
                        vec!["config", k, host]
                    }
                } else if key.starts_with("policy.") {
                    std::iter::once("config").chain(key.split('.')).collect()
                } else {
                    vec!["config", key]
                };
                if let Some(value) = value {
                    set_path(data, &path, value);
                } else {
                    remove_path(data, &path, false);
                    if key.starts_with("policy.") {
                        // drop emptied ancestors inside the policy subtree
                        for depth in (3..path.len()).rev() {
                            if get_path(data, &path[..depth]).is_some_and(is_empty_container) {
                                remove_path(data, &path[..depth], false);
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
            Edit::AddProperty(key, value) => match property_path(key, false) {
                Some(path) => set_path(data, &path, value.clone()),
                None => {
                    data.insert(key.to_string(), value.clone());
                }
            },
            Edit::RemoveProperty(key) => match property_path(key, true) {
                Some(path) => remove_path(data, &path, true),
                None => {
                    data.shift_remove(key);
                }
            },
            Edit::AddLink(kind, name, value) => {
                set_path(data, &[kind, name], Value::String(value.to_string()))
            }
            Edit::RemoveRepository(name) => {
                if let Some(Value::Object(repos)) = data.get_mut("repositories") {
                    repos.shift_remove(name);
                } else if let Some(Value::Array(repos)) = data.get_mut("repositories") {
                    repos.retain(|r| {
                        r.get("name").and_then(Value::as_str) != Some(name)
                            || r.as_object().is_some_and(|o| o.len() == 1)
                    });
                }
                if data.get("repositories").is_some_and(is_empty_container) {
                    data.shift_remove("repositories");
                }
            }
            Edit::AddRepository(name, config, append) => {
                let mut list: Vec<Value> = match data.shift_remove("repositories") {
                    Some(Value::Array(list)) => list,
                    Some(Value::Object(map)) => map
                        .into_iter()
                        .map(|(k, v)| match v {
                            Value::Object(mut repo) if !repo.contains_key("name") => {
                                let mut named = Map::new();
                                named.insert("name".into(), Value::String(k));
                                named.append(&mut repo);
                                Value::Object(named)
                            }
                            Value::Object(repo) => Value::Object(repo),
                            other => Value::Object(Map::from_iter([(k, other)])),
                        })
                        .collect(),
                    _ => vec![],
                };
                let entry = match config {
                    None => Value::Object(Map::from_iter([(name.to_string(), Value::Bool(false))])),
                    Some(Value::Object(repo)) if !name.is_empty() && !repo.contains_key("name") => {
                        let mut named = Map::new();
                        named.insert("name".into(), Value::String(name.into()));
                        named.extend(repo.clone());
                        Value::Object(named)
                    }
                    Some(other) => other.clone(),
                };
                list.retain(|r| r.get("name").and_then(Value::as_str) != Some(name));
                if append {
                    list.push(entry);
                } else {
                    list.insert(0, entry);
                }
                data.insert("repositories".into(), Value::Array(list));
            }
        }
    }
}

/// Writes a new file with mode 600, never through a symlink, so secrets are never readable
/// by others; fails if anything already sits at `path`.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .write_all(contents.as_bytes())
}

/// `splitPropertyKey`: extra/scripts nest two levels deep, suggest/autoload one.
fn property_path(key: &str, removing: bool) -> Option<Vec<&str>> {
    let mains: &[&str] = if removing {
        &["suggest", "extra", "scripts", "autoload", "autoload-dev"]
    } else {
        &["suggest", "extra", "scripts"]
    };
    let (main, name) = key.split_once('.')?;
    if !mains.contains(&main) {
        return None;
    }
    let mut path = vec![main];
    if matches!(main, "extra" | "scripts") {
        path.extend(name.splitn(2, '.'));
    } else {
        path.push(name);
    }
    Some(path)
}

fn is_empty_container(v: &Value) -> bool {
    matches!(v, Value::Object(o) if o.is_empty()) || matches!(v, Value::Array(a) if a.is_empty())
}

fn get_path<'a>(data: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    rest.iter().try_fold(data.get(*first)?, |v, k| v.get(*k))
}

fn set_path(data: &mut Map<String, Value>, path: &[&str], value: Value) {
    let (last, parents) = path.split_last().unwrap();
    let mut map = data;
    for key in parents {
        let entry = map
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        map = entry.as_object_mut().unwrap();
    }
    map.insert(last.to_string(), value);
}

/// Removes the leaf; `keep_shape` keeps lists as lists and emptied objects as objects.
fn remove_path(data: &mut Map<String, Value>, path: &[&str], keep_shape: bool) {
    let (last, parents) = path.split_last().unwrap();
    let mut node: &mut Value = match parents.split_first() {
        None => {
            data.shift_remove(*last);
            return;
        }
        Some((first, _)) => match data.get_mut(*first) {
            Some(v) => v,
            None => return,
        },
    };
    for key in &parents[1..] {
        node = match node.get_mut(*key) {
            Some(v) => v,
            None => return,
        };
    }
    match node {
        Value::Object(map) => {
            map.shift_remove(*last);
            if keep_shape && map.is_empty() {
                *node = Value::Object(Map::new());
            }
        }
        Value::Array(list) => {
            if let Ok(i) = last.parse::<usize>()
                && i < list.len()
            {
                list.remove(i);
            }
        }
        _ => {}
    }
}

/// `JsonConfigSource::restoreEmptyObjects`
pub fn restore_empty_objects(config: &mut Map<String, Value>) {
    let empty = |v: &Value| matches!(v, Value::Array(a) if a.is_empty());
    let object = || Value::Object(Map::new());
    if let Some(Value::Object(c)) = config.get_mut("config") {
        if let Some(Value::Object(policy)) = c.get_mut("policy") {
            for v in policy.values_mut() {
                if empty(v) {
                    *v = object();
                }
            }
        }
        if let Some(policy) = c.get_mut("policy")
            && empty(policy)
        {
            *policy = object();
        }
        for prop in [
            "platform",
            "http-basic",
            "bearer",
            "gitlab-token",
            "gitlab-oauth",
            "github-oauth",
            "custom-headers",
            "forgejo-token",
            "preferred-install",
        ] {
            if let Some(v) = c.get_mut(prop)
                && empty(v)
            {
                *v = object();
            }
        }
    }
    for key in ["autoload", "autoload-dev"] {
        if let Some(Value::Object(a)) = config.get_mut(key) {
            for prop in ["psr-0", "psr-4"] {
                if let Some(v) = a.get_mut(prop)
                    && empty(v)
                {
                    *v = object();
                }
            }
        }
    }
    for prop in [
        "require",
        "require-dev",
        "conflict",
        "provide",
        "replace",
        "suggest",
        "config",
        "autoload",
        "autoload-dev",
        "scripts",
        "scripts-descriptions",
        "scripts-aliases",
        "support",
    ] {
        if let Some(v) = config.get_mut(prop)
            && empty(v)
        {
            *v = object();
        }
    }
}

// --- Config::get ----------------------------------------------------------------------

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// PHP string conversion of a scalar.
fn php_str(v: &Value) -> String {
    match v {
        Value::Null | Value::Bool(false) => String::new(),
        Value::Bool(true) => "1".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => "Array".into(),
    }
}

/// PHP `(bool)` with Composer's `!== 'false'` twist.
fn php_bool(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => s != "false" && !s.is_empty() && s != "0",
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// PHP `(int)` of a numeric-leading string or a number.
fn php_int(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::Bool(true) => 1,
        Value::String(s) => {
            let t = s.trim_start();
            let end = t
                .char_indices()
                .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+'))))
                .map_or(t.len(), |(i, _)| i);
            t[..end].parse().unwrap_or(0)
        }
        _ => 0,
    }
}

/// `Config::get`, `relative` being `Config::RELATIVE_PATHS`.
pub fn get(config: &Config, key: &str, relative: bool) -> Value {
    let raw = config.config.get(key).cloned().unwrap_or(Value::Null);
    let env_name = format!("COMPOSER_{}", key.to_uppercase().replace('-', "_"));
    match key {
        "vendor-dir" | "bin-dir" | "process-timeout" | "data-dir" | "cache-dir"
        | "cache-files-dir" | "cache-repo-dir" | "cache-vcs-dir" | "cafile" | "capath" => {
            let val = env(&env_name).map(Value::String);
            if key == "process-timeout" {
                return Value::from(php_int(val.as_ref().unwrap_or(&raw)).max(0));
            }
            let processed = php_str(&process(config, val.unwrap_or(raw), relative));
            let val = expand_path(processed.trim_end_matches(['/', '\\']));
            if !key.ends_with("-dir") || relative {
                return Value::String(val);
            }
            let absolute = Regex::new(r"(?i)^(?:/|[a-z]:|[a-z0-9.]+://|\\\\)").unwrap();
            Value::String(match &config.base_dir {
                Some(base) if !absolute.is_match(&val) => format!("{base}/{val}"),
                _ => val,
            })
        }
        "cache-read-only" | "htaccess-protect" => {
            Value::Bool(php_bool(&env(&env_name).map(Value::String).unwrap_or(raw)))
        }
        "disable-tls"
        | "secure-http"
        | "use-github-api"
        | "lock"
        | "source-fallback"
        | "strict-psr-autoloader" => {
            if key == "secure-http" && get(config, "disable-tls", relative) == Value::Bool(true) {
                return Value::Bool(false);
            }
            Value::Bool(php_bool(&raw))
        }
        "cache-ttl" => Value::from(php_int(&raw).max(0)),
        "cache-files-maxsize" => {
            let re = Regex::new(r"(?i)^\s*([0-9.]+)\s*(?:([kmg])(?:i?b)?)?\s*$").unwrap();
            let text = php_str(&raw);
            let Some(m) = re.captures(&text) else {
                return Value::Null;
            };
            let mut size: f64 = m[1].parse().unwrap_or(0.0);
            size *= match m.get(2).map(|u| u.as_str().to_lowercase()).as_deref() {
                Some("g") => 1024.0 * 1024.0 * 1024.0,
                Some("m") => 1024.0 * 1024.0,
                Some("k") => 1024.0,
                _ => 1.0,
            };
            Value::from((size as i64).max(0))
        }
        "cache-files-ttl" => {
            if raw.is_null() {
                get(config, "cache-ttl", relative)
            } else {
                Value::from(php_int(&raw).max(0))
            }
        }
        "home" => {
            let v = process(config, Value::String(expand_path(&php_str(&raw))), relative);
            Value::String(php_str(&v).trim_end_matches(['/', '\\']).to_string())
        }
        "bin-compat" => match env("COMPOSER_BIN_COMPAT").filter(|v| !v.is_empty() && v != "0") {
            Some(v) => Value::String(v),
            None => raw,
        },
        "discard-changes" => match env("COMPOSER_DISCARD_CHANGES") {
            Some(v) if v == "stash" => Value::String(v),
            Some(v) => Value::Bool(php_bool(&Value::String(v))),
            None => raw,
        },
        "github-protocols" => {
            let mut protos = raw;
            if php_bool(config.config.get("secure-http").unwrap_or(&Value::Null))
                && let Value::Array(list) = &mut protos
                && let Some(i) = list.iter().position(|p| p == "git")
            {
                list.remove(i);
            }
            protos
        }
        "autoloader-suffix" => {
            if raw == "" {
                Value::Null
            } else {
                process(config, raw, relative)
            }
        }
        "audit" => {
            let mut result = raw;
            if let Value::Object(map) = &mut result {
                if let Some(v) = env("COMPOSER_AUDIT_ABANDONED") {
                    map.insert("abandoned".into(), Value::String(v));
                }
                if let Some(v) = env("COMPOSER_SECURITY_BLOCKING_ABANDONED") {
                    map.insert(
                        "block-abandoned".into(),
                        Value::Bool(php_bool(&Value::String(v))),
                    );
                }
            }
            result
        }
        "policy" => match env("COMPOSER_POLICY").map(|v| php_bool(&Value::String(v))) {
            Some(false) => Value::Bool(false),
            Some(true) if raw == Value::Bool(false) => Value::Bool(true),
            _ => raw,
        },
        _ => process(config, raw, relative),
    }
}

/// `Config::process`: `{$key}` references (greedy, as Composer's pattern).
fn process(config: &Config, value: Value, relative: bool) -> Value {
    let Value::String(s) = value else {
        return value;
    };
    let re = Regex::new(r"\{\$(.+)\}").unwrap();
    Value::String(
        re.replace_all(&s, |c: &regex::Captures<'_>| {
            php_str(&get(config, &c[1], relative))
        })
        .into_owned(),
    )
}

/// `Config::getRepositories` as a PHP array: list repositories keep their index.
fn repositories(config: &Config) -> Value {
    let map: Map<String, Value> = config.repositories.iter().cloned().collect();
    php_assoc(Value::Object(map))
}

/// `Config::all`
fn all(config: &Config, relative: bool) -> Value {
    let mut cfg = Map::new();
    for key in config.config.keys() {
        cfg.insert(key.clone(), get(config, key, relative));
    }
    let mut out = Map::new();
    out.insert("repositories".into(), repositories(config));
    out.insert("config".into(), Value::Object(cfg));
    Value::Object(out)
}

fn entries(v: &Value) -> Vec<(String, Value)> {
    match v {
        Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        _ => vec![],
    }
}

/// `listConfiguration`
fn list_configuration(io: &mut Io, contents: &Value, raw: &Value, k: Option<&str>) {
    for (key, value) in entries(contents) {
        if k.is_none() && key != "config" && key != "repositories" {
            continue;
        }
        let raw_val = raw.get(key.as_str()).cloned().unwrap_or(Value::Null);
        let is_array = matches!(value, Value::Array(_) | Value::Object(_));
        let first_numeric = entries(&value)
            .first()
            .is_some_and(|(k, _)| k.parse::<f64>().is_ok());
        if is_array && (!first_numeric || (key == "repositories" && k.is_none())) {
            let prefix = format!("{}{key}.", k.unwrap_or(""));
            let prefix = prefix
                .strip_prefix("config.")
                .unwrap_or(&prefix)
                .to_string();
            list_configuration(io, &value, &raw_val, Some(&prefix));
            continue;
        }
        let (text, is_string) = match &value {
            Value::Array(_) | Value::Object(_) => {
                let items: Vec<String> = entries(&value)
                    .iter()
                    .map(|(_, v)| match v {
                        Value::Array(_) | Value::Object(_) => {
                            crate::json::encode(v, false, false, false)
                        }
                        other => php_str(other),
                    })
                    .collect();
                (format!("[{}]", items.join(", ")), true)
            }
            Value::Bool(b) => (b.to_string(), true),
            Value::String(s) => (s.clone(), true),
            other => (php_str(other), false),
        };
        let name = format!("{}{key}", k.unwrap_or(""));
        match raw_val {
            Value::String(raw) if !is_string || raw != text => {
                io.write(&format!("[{name}] {raw} ({text})"))
            }
            _ => io.write(&format!("[{name}] {text}")),
        }
    }
}

// --- ConfigCommand --------------------------------------------------------------------

const PACKAGE_PROPERTIES: [&str; 12] = [
    "name",
    "type",
    "description",
    "homepage",
    "version",
    "minimum-stability",
    "prefer-stable",
    "keywords",
    "license",
    "repositories",
    "suggest",
    "extra",
];

const BUILTIN_LISTS: [&str; 4] = ["advisories", "abandoned", "malware", "cooldown"];
const AUDITS: [&str; 3] = ["ignore", "report", "fail"];

fn read_json(path: &Path) -> Result<Map<String, Value>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Could not read {}\n\n{e}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Ok(Map::new()),
        Err(e) => Err(format!(
            "\"{}\" does not contain valid JSON\n{e}",
            path.display()
        )),
    }
}

/// `JsonFile::parseJson`
fn parse_json(text: &str) -> Result<Value, String> {
    serde_json::from_str::<Value>(text)
        .map(php_assoc)
        .map_err(|e| format!("\"{text}\" does not contain valid JSON\n{e}"))
}

fn is_bool_string(v: &str) -> bool {
    matches!(v, "true" | "false" | "1" | "0")
}

fn bool_value(v: &str) -> Value {
    Value::Bool(php_bool(&Value::String(v.to_string())))
}

fn reserved_list_error(name: &str) -> Option<String> {
    if name.starts_with("ignore") {
        return Some(format!(
            "Invalid dependency policy name: \"{name}\" starts with reserved prefix \"ignore\"."
        ));
    }
    const FUTURE: [&str; 10] = [
        "package",
        "packages",
        "license",
        "licence",
        "licenses",
        "licences",
        "support",
        "maintenance",
        "security",
        "minimum-release-age",
    ];
    FUTURE
        .contains(&name)
        .then(|| format!("Invalid dependency policy name: \"{name}\" is reserved for future use."))
}

/// `$uniqueConfigValues`: the validator and normalizer of a single-valued config key.
fn unique_config(key: &str, v: &str) -> Option<Result<Value, ()>> {
    let ok = |valid: bool, value: Value| Some(if valid { Ok(value) } else { Err(()) });
    let s = || Value::String(v.to_string());
    let one_of = |list: &[&str]| list.contains(&v);
    let bool_or = |word: &str, list: &[&str]| {
        ok(
            list.contains(&v),
            if v == word { s() } else { bool_value(v) },
        )
    };
    match key {
        "process-timeout" | "cache-ttl" | "cache-files-ttl" => {
            let numeric = v.trim().parse::<f64>().is_ok();
            ok(numeric, Value::from(php_int(&s())))
        }
        "use-include-path"
        | "use-github-api"
        | "notify-on-install"
        | "sort-packages"
        | "optimize-autoloader"
        | "strict-psr-autoloader"
        | "classmap-authoritative"
        | "apcu-autoloader"
        | "prepend-autoloader"
        | "update-with-minimal-changes"
        | "disable-tls"
        | "secure-http"
        | "source-fallback"
        | "github-expose-hostname"
        | "htaccess-protect"
        | "lock"
        | "allow-plugins"
        | "audit.ignore-unreachable"
        | "audit.block-insecure"
        | "audit.block-abandoned"
        | "policy.advisories.block"
        | "policy.malware.block"
        | "policy.abandoned.block"
        | "policy.cooldown.block"
        | "policy.ignore-unreachable" => ok(is_bool_string(v), bool_value(v)),
        "preferred-install" => ok(one_of(&["auto", "source", "dist"]), s()),
        "gitlab-protocol" => ok(one_of(&["git", "http", "https"]), s()),
        "store-auths" | "use-parent-dir" => bool_or("prompt", &["true", "false", "prompt"]),
        "vendor-dir" | "bin-dir" | "archive-dir" | "archive-format" | "data-dir" | "cache-dir"
        | "cache-files-dir" | "cache-repo-dir" | "cache-vcs-dir" => ok(true, s()),
        "cache-files-maxsize" => ok(
            Regex::new(r"(?i)^\s*([0-9.]+)\s*(?:([kmg])(?:i?b)?)?\s*$")
                .unwrap()
                .is_match(v),
            s(),
        ),
        "bin-compat" => ok(one_of(&["auto", "full", "proxy", "symlink"]), s()),
        "discard-changes" => bool_or("stash", &["stash", "true", "false", "1", "0"]),
        "autoloader-suffix" => ok(true, if v == "null" { Value::Null } else { s() }),
        "bump-after-update" => {
            let valid = one_of(&["dev", "no-dev", "true", "false", "1", "0"]);
            ok(
                valid,
                if v == "dev" || v == "no-dev" {
                    s()
                } else {
                    bool_value(v)
                },
            )
        }
        "cafile" => ok(
            Path::new(v).is_file() || Path::new(v).exists(),
            if v == "null" { Value::Null } else { s() },
        ),
        "capath" => ok(
            Path::new(v).is_dir(),
            if v == "null" { Value::Null } else { s() },
        ),
        "platform-check" => bool_or("php-only", &["php-only", "true", "false", "1", "0"]),
        "audit.abandoned"
        | "policy.advisories.audit"
        | "policy.malware.audit"
        | "policy.abandoned.audit"
        | "policy.cooldown.audit" => ok(one_of(&AUDITS), s()),
        "policy.malware.block-scope" => ok(one_of(&["update", "install", "all"]), s()),
        "policy.cooldown.period" => {
            if v == "null" || v.is_empty() {
                return ok(true, Value::Null);
            }
            let valid = Regex::new(r"^\d+$").unwrap().is_match(v)
                || Regex::new(r"^(\d+)\s*(second|minute|hour|day|week)s?$")
                    .unwrap()
                    .is_match(v);
            ok(
                valid,
                v.parse::<i64>().map(Value::from).unwrap_or_else(|_| s()),
            )
        }
        _ => None,
    }
}

/// `$multiConfigValues`: validation message on failure.
fn multi_config(key: &str, values: &[String]) -> Option<Result<(), String>> {
    let all_in = |list: &[&str], msg: &str| {
        if values.iter().all(|v| list.contains(&v.as_str())) {
            Ok(())
        } else {
            Err(msg.to_string())
        }
    };
    Some(match key {
        "github-protocols" => all_in(
            &["git", "https", "ssh"],
            "valid protocols include: git, https, ssh",
        ),
        "github-domains" | "gitlab-domains" | "policy.malware.ignore-source" => Ok(()),
        "audit.ignore-severity" | "policy.advisories.ignore-severity" => all_in(
            &["low", "medium", "high", "critical"],
            "valid severities include: low, medium, high, critical",
        ),
        _ => return None,
    })
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    if args.global && args.file.is_some() {
        return Err("--file and --global can not be combined".into());
    }
    let cwd = crate::fs::cwd();
    let mut config = Config::create(&cwd)?;
    let home = config.get_str("home");
    if args.global {
        config.base_dir = Some(home.clone());
    }
    let config_file: PathBuf = if args.global {
        PathBuf::from(format!("{home}/config.json"))
    } else {
        match &args.file {
            Some(f) => PathBuf::from(f),
            None => config::composer_file()?,
        }
    };
    let name = config_file.to_string_lossy().to_string();
    if (name == "composer.json" || name == "./composer.json")
        && !config_file.exists()
        && crate::fs::realpath(Path::new(".")) == crate::fs::realpath(Path::new(&home))
    {
        let _ = std::fs::write(&config_file, "{\n}\n");
    }
    if args.global && !config_file.exists() {
        let _ = std::fs::create_dir_all(&home);
        write_private(&config_file, "{\n    \"config\": {}\n}\n").map_err(|e| e.to_string())?;
    }
    if !config_file.exists() {
        return Err(format!(
            "File \"{name}\" cannot be found in the current directory"
        ));
    }
    let auth_file = if args.global {
        PathBuf::from(format!("{home}/auth.json"))
    } else {
        let dir = config_file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        PathBuf::from(format!("{}/auth.json", dir.display()))
    };
    if args.global && !auth_file.exists() {
        let empty: String = [
            "bitbucket-oauth",
            "github-oauth",
            "gitlab-oauth",
            "gitlab-token",
            "http-basic",
            "bearer",
            "forgejo-token",
        ]
        .iter()
        .map(|k| format!("    \"{k}\": {{}}"))
        .collect::<Vec<_>>()
        .join(",\n");
        write_private(&auth_file, &format!("{{\n{empty}\n}}\n")).map_err(|e| e.to_string())?;
    }
    let source = ConfigSource::new(&config_file, false);
    let auth_source = ConfigSource::new(&auth_file, true);

    if args.editor {
        return Err(
            "The --editor option is not supported by romposer, edit the file directly.".into(),
        );
    }

    let raw_data = read_json(&config_file)?;
    if !args.global {
        config.merge(&raw_data);
        if auth_file.exists() {
            config.merge_config(&read_json(&auth_file)?);
        }
    }

    if args.list {
        let mut raw = Map::new();
        raw.insert("repositories".into(), repositories(&config));
        raw.insert("config".into(), Value::Object(config.config.clone()));
        list_configuration(io, &all(&config, false), &Value::Object(raw), None);
        return Ok(0);
    }

    let Some(key) = args.setting_key.as_deref() else {
        return Ok(0);
    };
    let key = key.to_string();
    let values = &args.setting_value;
    if !values.is_empty() && args.unset {
        return Err("You can not combine a setting value with --unset".into());
    }

    if values.is_empty() && !args.unset {
        let value = show(&config, &raw_data, &key, args.absolute)?;
        let text = match &value {
            Value::Array(_) | Value::Object(_) | Value::Bool(_) => {
                crate::json::encode(&value, false, true, true)
            }
            other => php_str(other),
        };
        io.write(&text);
        return Ok(0);
    }
    set(
        args,
        io,
        &config,
        &source,
        &auth_source,
        &config_file,
        &key,
        values,
    )?;
    Ok(0)
}

/// Reads a setting, for `config <key>`.
fn show(
    config: &Config,
    raw_data: &Map<String, Value>,
    key: &str,
    absolute: bool,
) -> Result<Value, String> {
    let data = all(config, false);
    if let Some(m) = Regex::new(r"^repos?(?:itories)?(?:\.(.+))?")
        .unwrap()
        .captures(key)
    {
        let repos = data["repositories"].clone();
        return match m.get(1) {
            None => Ok(repos),
            Some(name) => repos
                .get(name.as_str())
                .cloned()
                .filter(|v| !v.is_null())
                .ok_or_else(|| format!("There is no {} repository defined", name.as_str())),
        };
    }
    if key.find('.').is_some_and(|i| i > 0) {
        let bits: Vec<&str> = key.split('.').collect();
        let mut node = if bits[0] == "extra" || bits[0] == "suggest" {
            php_assoc(Value::Object(raw_data.clone()))
        } else {
            data["config"].clone()
        };
        let mut pending: Option<String> = None;
        let mut matched = false;
        for bit in bits {
            let joined = match pending.take() {
                Some(p) => format!("{p}.{bit}"),
                None => bit.to_string(),
            };
            matched = false;
            match node.get(joined.as_str()).filter(|v| !v.is_null()).cloned() {
                Some(next) => {
                    matched = true;
                    node = next;
                }
                None => pending = Some(joined),
            }
        }
        if !matched {
            return Err(format!("{key} is not defined."));
        }
        return Ok(node);
    }
    if data["config"].get(key).is_some_and(|v| !v.is_null()) {
        let value = php_assoc(get(config, key, !absolute));
        if value == Value::Array(vec![]) {
            let schema: Value = serde_json::from_str(crate::schema::COMPOSER_SCHEMA).unwrap();
            let kind = &schema["properties"]["config"]["properties"][key]["type"];
            let object = kind == "object"
                || kind
                    .as_array()
                    .is_some_and(|k| k.iter().any(|t| t == "object"));
            if object {
                return Ok(Value::Object(Map::new()));
            }
        }
        return Ok(value);
    }
    if let Some(v) = raw_data.get(key).filter(|v| !v.is_null())
        && PACKAGE_PROPERTIES.contains(&key)
    {
        return Ok(php_assoc(v.clone()));
    }
    Ok(match key {
        "type" => Value::String("library".into()),
        "description" | "homepage" => Value::String(String::new()),
        "minimum-stability" => Value::String("stable".into()),
        "prefer-stable" => Value::Bool(false),
        "keywords" | "license" | "suggest" | "extra" => Value::Array(vec![]),
        _ => return Err(format!("{key} is not defined")),
    })
}

/// `--merge` of a list or object value into the current one.
fn merge_value(
    current: Option<&Value>,
    value: Value,
    key: &str,
    strict: bool,
) -> Result<Value, String> {
    match (current, value) {
        (Some(Value::Array(cur)), Value::Array(new)) => {
            Ok(Value::Array(cur.iter().cloned().chain(new).collect()))
        }
        (Some(Value::Object(cur)), Value::Object(mut new)) => {
            for (k, v) in cur {
                new.entry(k.clone()).or_insert_with(|| v.clone());
            }
            Ok(Value::Object(new))
        }
        (Some(Value::Array(cur)), Value::Object(new)) if !strict => {
            let mut new = new;
            for (i, v) in cur.iter().enumerate() {
                new.entry(i.to_string()).or_insert_with(|| v.clone());
            }
            Ok(Value::Object(new))
        }
        (Some(Value::Object(cur)), Value::Array(new)) if !strict => {
            let mut out: Map<String, Value> = new
                .into_iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v))
                .collect();
            for (k, v) in cur {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
            Ok(Value::Object(out))
        }
        (Some(Value::Array(_) | Value::Object(_)), _) if strict => {
            Err(format!("Cannot merge array and object for {key}"))
        }
        (_, value) => Ok(value),
    }
}

fn strings(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

#[allow(clippy::too_many_arguments)]
fn set(
    args: &Args,
    io: &mut Io,
    config: &Config,
    source: &ConfigSource,
    auth_source: &ConfigSource,
    config_file: &Path,
    key: &str,
    values: &[String],
) -> Result<(), String> {
    let first = || values.first().cloned().unwrap_or_default();
    let unset = args.unset;
    let current_file = || read_json(config_file).map(|m| php_assoc(Value::Object(m)));

    if unset && (key == "audit" || key == "policy" || key.starts_with("policy.")) {
        return source.edit(Edit::RemoveConfig(key));
    }

    // policy.*.ignore and policy.advisories.ignore-id, with --json and --merge
    let custom_ignore = Regex::new(r"^policy\.([^.]+)\.ignore$").unwrap();
    let custom_name = custom_ignore
        .captures(key)
        .map(|m| m[1].to_string())
        .filter(|n| !BUILTIN_LISTS.contains(&n.as_str()) && n != "ignore-unreachable");
    let builtin_ignore = key == "policy.advisories.ignore-id"
        || BUILTIN_LISTS
            .iter()
            .any(|l| key == format!("policy.{l}.ignore"));
    if builtin_ignore || custom_name.is_some() {
        if let Some(name) = &custom_name
            && let Some(e) = reserved_list_error(name)
        {
            return Err(e);
        }
        let mut value = strings(values);
        if args.json {
            value = parse_json(&first())?;
            if !matches!(value, Value::Array(_) | Value::Object(_)) {
                return Err(format!("Expected an array or object for {key}"));
            }
        }
        if args.merge {
            let file = current_file()?;
            let current = key
                .split('.')
                .try_fold(file.get("config"), |node, bit| {
                    node.and_then(|n| n.get(bit)).map(Some)
                })
                .flatten();
            value = merge_value(current, value, key, true)?;
        }
        return source.edit(Edit::AddConfig(key, &value));
    }

    const SCOPES: [&str; 3] = ["audit", "install", "update"];
    if key == "policy.ignore-unreachable" {
        let scope_error = || format!("valid values for {key} include: {}", SCOPES.join(", "));
        if args.json {
            let value = parse_json(&first())?;
            let Value::Array(items) = &value else {
                return Err(format!("Expected a boolean or array for {key}"));
            };
            if items
                .iter()
                .any(|v| !v.as_str().is_some_and(|s| SCOPES.contains(&s)))
            {
                return Err(scope_error());
            }
            return source.edit(Edit::AddConfig(key, &value));
        }
        if values.first().is_some_and(|v| SCOPES.contains(&v.as_str())) {
            if values.iter().any(|v| !SCOPES.contains(&v.as_str())) {
                return Err(scope_error());
            }
            return source.edit(Edit::AddConfig(key, &strings(values)));
        }
    }

    if let Some(m) = Regex::new(r"^policy\.([^.]+)$").unwrap().captures(key)
        && &m[1] != "ignore-unreachable"
    {
        if let Some(e) = reserved_list_error(&m[1]) {
            return Err(e);
        }
        if !is_bool_string(&first()) {
            return Err(format!(
                "\"{}\" is an invalid value for {key}, expected a boolean",
                first()
            ));
        }
        return source.edit(Edit::AddConfig(key, &bool_value(&first())));
    }

    if let Some(m) = Regex::new(r"^policy\.([^.]+)\.(block|block-scope|audit)$")
        .unwrap()
        .captures(key)
        && !BUILTIN_LISTS.contains(&&m[1])
        && &m[1] != "ignore-unreachable"
    {
        if let Some(e) = reserved_list_error(&m[1]) {
            return Err(e);
        }
        let v = first();
        let value = match &m[2] {
            "block" if is_bool_string(&v) => bool_value(&v),
            "block" => {
                return Err(format!(
                    "\"{v}\" is an invalid value for {key}, expected a boolean"
                ));
            }
            "block-scope" if ["update", "install", "all"].contains(&v.as_str()) => Value::String(v),
            "block-scope" => {
                return Err(format!(
                    "\"{v}\" is an invalid value for {key}, must be one of: all, update, install"
                ));
            }
            _ if AUDITS.contains(&v.as_str()) => Value::String(v),
            _ => {
                return Err(format!(
                    "\"{v}\" is an invalid value for {key}, must be one of: ignore, report, fail"
                ));
            }
        };
        return source.edit(Edit::AddConfig(key, &value));
    }

    if let Some(m) = Regex::new(r"^policy\.([^.]+)\.sources$")
        .unwrap()
        .captures(key)
    {
        return Err(format!(
            "Setting dependency policy sources is not supported by `composer config`. Use `composer policy add-source {} url <https-url>` instead.",
            &m[1]
        ));
    }

    let is_unique = unique_config(key, "").is_some();
    let is_multi = multi_config(key, &[]).is_some();
    if unset && (is_unique || is_multi) {
        if key == "disable-tls" && config.bool("disable-tls") {
            io.error("You are now running Composer with SSL/TLS protection enabled.");
        }
        return source.edit(Edit::RemoveConfig(key));
    }
    if is_unique {
        let value = single_value(key, values, |v| unique_config(key, v).unwrap())?;
        if key == "disable-tls" {
            let on = php_bool(&value);
            if !on && config.bool("disable-tls") {
                io.error("You are now running Composer with SSL/TLS protection enabled.");
            } else if on && !config.bool("disable-tls") {
                io.error("You are now running Composer with SSL/TLS protection disabled.");
            }
        }
        return source.edit(Edit::AddConfig(key, &value));
    }
    if is_multi {
        if let Some(Err(message)) = multi_config(key, values) {
            return Err(format!(
                "{} is an invalid value ({message})",
                crate::json::encode_compact(&strings(values))
            ));
        }
        return source.edit(Edit::AddConfig(key, &strings(values)));
    }

    if key.starts_with("preferred-install.") && key.len() > "preferred-install.".len() {
        if unset {
            return source.edit(Edit::RemoveConfig(key));
        }
        if !["auto", "source", "dist"].contains(&first().as_str()) {
            return Err(format!(
                "Invalid value for {key}. Should be one of: auto, source, or dist"
            ));
        }
        return source.edit(Edit::AddConfig(key, &Value::String(first())));
    }

    if Regex::new(r"^allow-plugins\.([a-zA-Z0-9/*-]+)")
        .unwrap()
        .is_match(key)
    {
        if unset {
            return source.edit(Edit::RemoveConfig(key));
        }
        if !is_bool_string(&first()) {
            return Err(format!("\"{}\" is an invalid value", first()));
        }
        return source.edit(Edit::AddConfig(key, &bool_value(&first())));
    }

    // package properties
    let unique_prop = matches!(
        key,
        "name"
            | "type"
            | "description"
            | "homepage"
            | "version"
            | "minimum-stability"
            | "prefer-stable"
    );
    let multi_prop = matches!(key, "keywords" | "license");
    if args.global && (unique_prop || multi_prop || key.starts_with("extra.")) {
        return Err(format!(
            "The {key} property can not be set in the global config.json file. Use `composer global config` to apply changes to the global composer.json"
        ));
    }
    if unset && (unique_prop || multi_prop) {
        return source.edit(Edit::RemoveProperty(key));
    }
    if unique_prop {
        let value = single_value(key, values, |v| match key {
            "minimum-stability" => crate::semver::Stability::parse(v)
                .map(|s| Value::String(s.as_str().into()))
                .map_err(|_| ()),
            "prefer-stable" if is_bool_string(v) => Ok(bool_value(v)),
            "prefer-stable" => Err(()),
            _ => Ok(Value::String(v.to_string())),
        })?;
        return source.edit(Edit::AddProperty(key, &value));
    }
    if multi_prop {
        return source.edit(Edit::AddProperty(key, &strings(values)));
    }

    if let Some(m) = Regex::new(r"^repos?(?:itories)?\.(.+)")
        .unwrap()
        .captures(key)
    {
        let name = &m[1];
        if unset {
            return source.edit(Edit::RemoveRepository(name));
        }
        if values.len() == 2 {
            let mut repo = Map::new();
            repo.insert("type".into(), Value::String(values[0].clone()));
            repo.insert("url".into(), Value::String(values[1].clone()));
            return source.edit(Edit::AddRepository(
                name,
                Some(&Value::Object(repo)),
                args.append,
            ));
        }
        if values.len() == 1 {
            let lower = values[0].to_lowercase();
            if is_bool_string(&lower) {
                if !php_bool(&Value::String(lower)) {
                    return source.edit(Edit::AddRepository(name, None, args.append));
                }
            } else {
                let value = parse_json(&values[0])?;
                return source.edit(Edit::AddRepository(name, Some(&value), args.append));
            }
        }
        return Err("You must pass the type and a url. Example: php composer.phar config repositories.foo vcs https://bar.com".into());
    }

    if key.starts_with("extra.") && key.len() > 6 {
        if unset {
            return source.edit(Edit::RemoveProperty(key));
        }
        let mut value = Value::String(first());
        if args.json {
            value = parse_json(&first())?;
            if args.merge {
                let file = current_file()?;
                let current = key
                    .split('.')
                    .try_fold(Some(&file), |node, bit| Some(node.and_then(|n| n.get(bit))))
                    .flatten();
                value = merge_value(current, value, key, false)?;
            }
        }
        return source.edit(Edit::AddProperty(key, &value));
    }

    if key.starts_with("suggest.") && key.len() > 8 {
        if unset {
            return source.edit(Edit::RemoveProperty(key));
        }
        return source.edit(Edit::AddProperty(key, &Value::String(values.join(" "))));
    }

    if (key == "suggest" || key == "extra") && unset {
        return source.edit(Edit::RemoveProperty(key));
    }

    if key.starts_with("platform.") && key.len() > 9 {
        if unset {
            return source.edit(Edit::RemoveConfig(key));
        }
        let value = if first() == "false" {
            Value::Bool(false)
        } else {
            Value::String(first())
        };
        return source.edit(Edit::AddConfig(key, &value));
    }
    if key == "platform" && unset {
        return source.edit(Edit::RemoveConfig(key));
    }

    if key == "audit.ignore" || key == "audit.ignore-abandoned" {
        if unset {
            return source.edit(Edit::RemoveConfig(key));
        }
        let mut value = strings(values);
        if args.json {
            value = parse_json(&first())?;
            if !matches!(value, Value::Array(_) | Value::Object(_)) {
                return Err(format!("Expected an array or object for {key}"));
            }
        }
        if args.merge {
            let file = current_file()?;
            let current = file
                .get("config")
                .and_then(|c| c.get("audit"))
                .and_then(|a| a.get(&key[6..]));
            value = merge_value(current, value, key, true)?;
        }
        return source.edit(Edit::AddConfig(key, &value));
    }

    if let Some(m) = Regex::new(
        r"^(bitbucket-oauth|github-oauth|gitlab-oauth|gitlab-token|http-basic|custom-headers|bearer|forgejo-token)\.(.+)",
    )
    .unwrap()
    .captures(key)
    {
        let kind = &m[1];
        if unset {
            auth_source.edit(Edit::RemoveConfig(key))?;
            return source.edit(Edit::RemoveConfig(key));
        }
        let pair = |a: &str, b: &str| {
            let mut map = Map::new();
            map.insert(a.into(), Value::String(values[0].clone()));
            map.insert(b.into(), Value::String(values[1].clone()));
            Value::Object(map)
        };
        let value = match kind {
            "bitbucket-oauth" => {
                if values.len() != 2 {
                    return Err(format!(
                        "Expected two arguments (consumer-key, consumer-secret), got {}",
                        values.len()
                    ));
                }
                pair("consumer-key", "consumer-secret")
            }
            "gitlab-token" if values.len() == 2 => pair("username", "token"),
            "github-oauth" | "gitlab-oauth" | "gitlab-token" | "bearer" => {
                if values.len() != 1 {
                    return Err("Too many arguments, expected only one token".into());
                }
                Value::String(values[0].clone())
            }
            "http-basic" => {
                if values.len() != 2 {
                    return Err(format!(
                        "Expected two arguments (username, password), got {}",
                        values.len()
                    ));
                }
                pair("username", "password")
            }
            "custom-headers" => {
                if values.is_empty() {
                    return Err("Expected at least one argument (header), got none".into());
                }
                let header = Regex::new(r"^[^:]+:\s*.+$").unwrap();
                if let Some(bad) = values.iter().find(|h| !header.is_match(h)) {
                    return Err(format!(
                        "Header \"{bad}\" is not in \"Header-Name: Header-Value\" format"
                    ));
                }
                strings(values)
            }
            _ => {
                if values.len() != 2 {
                    return Err(format!(
                        "Expected two arguments (username, access token), got {}",
                        values.len()
                    ));
                }
                pair("username", "token")
            }
        };
        source.edit(Edit::RemoveConfig(key))?;
        return auth_source.edit(Edit::AddConfig(key, &value));
    }

    if key.starts_with("scripts.") && key.len() > 8 {
        if unset {
            return source.edit(Edit::RemoveProperty(key));
        }
        let value = if values.len() > 1 {
            strings(values)
        } else {
            Value::String(first())
        };
        return source.edit(Edit::AddProperty(key, &value));
    }

    if unset {
        return source.edit(Edit::RemoveProperty(key));
    }
    Err(format!(
        "Setting {key} does not exist or is not supported by this command"
    ))
}

/// `handleSingleValue`
fn single_value(
    _key: &str,
    values: &[String],
    check: impl Fn(&str) -> Result<Value, ()>,
) -> Result<Value, String> {
    if values.len() != 1 {
        return Err(
            "You can only pass one value. Example: php composer.phar config process-timeout 300"
                .into(),
        );
    }
    check(&values[0]).map_err(|()| format!("\"{}\" is an invalid value", values[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_formatting() {
        let mut m = Manipulator::new("{\n    \"config\": {\"vendor-dir\": \"lib\"}\n}").unwrap();
        assert!(m.add_config_setting("sort-packages", &Value::Bool(true)));
        assert_eq!(
            m.contents(),
            "{\n    \"config\": {\"vendor-dir\": \"lib\",\n        \"sort-packages\": true}\n}\n"
        );
        let mut m = Manipulator::new("{\n    \"name\": \"a/b\"\n}").unwrap();
        let repo = serde_json::json!({"type": "vcs", "url": "https://x.org/y"});
        assert!(m.add_repository("foo", Some(&repo), false));
        let path = serde_json::json!({"type": "path", "url": "../b"});
        assert!(m.add_repository("bar", Some(&path), false));
        assert_eq!(
            m.contents(),
            "{\n    \"name\": \"a/b\",\n    \"repositories\": [{\n        \"name\": \"bar\",\n        \"type\": \"path\",\n        \"url\": \"../b\"\n    },{\n        \"name\": \"foo\",\n        \"type\": \"vcs\",\n        \"url\": \"https://x.org/y\"\n    }]\n}\n"
        );
        assert!(m.remove_repository("bar"));
        assert!(m.remove_repository("foo"));
        assert_eq!(m.contents(), "{\n    \"name\": \"a/b\"\n}\n");
        let mut m = Manipulator::new("{\n    \"extra\": {\n        \"foo\": {\n            \"bar\": 1,\n            \"baz\": 2\n        }\n    }\n}").unwrap();
        assert!(m.remove_property("extra.foo.bar"));
        assert_eq!(
            m.contents(),
            "{\n    \"extra\": {\n        \"foo\": {\n            \"baz\": 2\n        }\n    }\n}\n"
        );
    }
}
