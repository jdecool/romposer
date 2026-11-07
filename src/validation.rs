//! Port of `ConfigValidator` and `ValidatingArrayLoader`: the checks behind
//! `composer validate`, with Composer's wording.

use crate::json::{Map, Value, encode_compact, lint};
use crate::package::{is_platform_package, naming_error, suggest_name};
use crate::semver::{self, Constraint, intervals};
use crate::{schema, spdx};
use regex::Regex;
use std::sync::LazyLock;

pub const CHECK_UNBOUND_CONSTRAINTS: u8 = 1;
pub const CHECK_STRICT_CONSTRAINTS: u8 = 2;
pub const CHECK_ALL: u8 = 3;

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub errors: Vec<String>,
    pub publish_errors: Vec<String>,
    pub warnings: Vec<String>,
}

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9!#$%&'*+/=?^_`{|}~.-]+@(?:\[[0-9.]+\]|[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)+)$").unwrap()
});
static GPL_PLUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[AL]?GPL-[123](\.[01])?\+$").unwrap());
static GPL_BARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[AL]?GPL-[123](\.[01])?$").unwrap());
static ISO_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})-(\d{2})-(\d{2})").unwrap());
static LINK_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./-]+$").unwrap());
static DOTDOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\\/])\.\.(?:[\\/]|$)").unwrap());
static UNSAFE_BIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[*$`"&^|<>()%!;\x00-\x1f\x7f]"#).unwrap());
static LEADING_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*-").unwrap());

/// PHP's `filter_var($v, FILTER_VALIDATE_EMAIL)`, close enough for the warnings.
pub fn is_valid_email(email: &str) -> bool {
    EMAIL.is_match(email) && !email.contains("..") && email.is_ascii()
}

/// `parse_url` scheme/host check used by `filterUrl`.
fn filter_url(value: &str, schemes: &[&str]) -> bool {
    if value.is_empty() {
        return true;
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    let valid_scheme = scheme.chars().enumerate().all(|(i, c)| {
        if i == 0 {
            c.is_ascii_alphabetic()
        } else {
            c.is_ascii_alphanumeric() || "+-.".contains(c)
        }
    });
    if scheme.is_empty() || !valid_scheme {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, port)| {
        if port.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    !host.is_empty() && schemes.iter().any(|s| s.eq_ignore_ascii_case(scheme))
}

/// `get_debug_type` for decoded JSON.
fn debug_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "string",
        Value::Array(_) | Value::Object(_) => "array",
    }
}

/// `var_export` for scalars; containers are summarised.
fn var_export(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(b) => {
            if *b {
                "true".into()
            } else {
                "false".into()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        other => encode_compact(other),
    }
}

/// A JSON value that PHP's `empty()` would consider empty.
fn php_empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => !b,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::String(s)) => s.is_empty() || s == "0",
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
    }
}

fn is_set(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null))
}

fn is_php_array(value: &Value) -> bool {
    matches!(value, Value::Array(_) | Value::Object(_))
}

fn php_count(value: &Value) -> usize {
    match value {
        Value::Array(a) => a.len(),
        Value::Object(o) => o.len(),
        _ => 0,
    }
}

/// Iterate a JSON array or object as PHP's `foreach ($arr as $key => $value)`.
fn entries(value: &Value) -> Vec<(String, Value)> {
    match value {
        Value::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => vec![],
    }
}

fn to_string_lossy(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "1".into(),
        Value::Bool(false) | Value::Null => String::new(),
        other => encode_compact(other),
    }
}

/// `ConfigValidator::validate`
pub fn validate_file(
    path: &std::path::Path,
    display: &str,
    flags: u8,
    check_version: bool,
) -> Report {
    let mut report = Report::default();
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            report
                .errors
                .push(format!("Could not read {display}\n\n{e}"));
            return report;
        }
    };
    let manifest: Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            report
                .errors
                .push(format!("\"{display}\" does not contain valid JSON\n{e}"));
            return report;
        }
    };
    let mut lax_valid = false;
    match schema::validate(&manifest, false) {
        Ok(()) => {
            lax_valid = true;
            if let Err(errors) = schema::validate(&manifest, true) {
                report.publish_errors.extend(errors);
            }
        }
        Err(errors) => report.errors.extend(errors),
    }
    let _ = lax_valid;
    if let Some(manifest) = manifest.as_object() {
        if let Some((key, line)) = lint::first_duplicate_key(&contents) {
            report.warnings.push(format!(
                "Key {key} is a duplicate in {display} at line {line}"
            ));
        }
        validate_manifest(manifest, flags, check_version, &mut report);
    }
    report
}

/// The data checks of `ConfigValidator::validate` after the schema, then the `ValidatingArrayLoader`.
pub fn validate_manifest(
    manifest: &Map<String, Value>,
    flags: u8,
    check_version: bool,
    report: &mut Report,
) {
    let warnings = &mut report.warnings;

    match manifest.get("license") {
        v if php_empty(v) => warnings.push(
            "No license specified, it is recommended to do so. For closed-source software you may use \"proprietary\" as license.".into(),
        ),
        Some(license) => {
            let licenses: Vec<String> = match license {
                Value::Array(a) => a.iter().map(to_string_lossy).collect(),
                Value::Object(o) => o.values().map(to_string_lossy).collect(),
                other => vec![to_string_lossy(other)],
            };
            for license in licenses.iter().filter(|l| *l != "proprietary") {
                if let Some(spdx) = spdx::license_by_identifier(license)
                    && spdx.deprecated
                {
                    if GPL_PLUS.is_match(license) {
                        warnings.push(format!(
                            "License \"{license}\" is a deprecated SPDX license identifier, use \"{}-or-later\" instead",
                            license.replace('+', "")
                        ));
                    } else if GPL_BARE.is_match(license) {
                        warnings.push(format!(
                            "License \"{license}\" is a deprecated SPDX license identifier, use \"{license}-only\" or \"{license}-or-later\" instead"
                        ));
                    } else {
                        warnings.push(format!(
                            "License \"{license}\" is a deprecated SPDX license identifier, see https://spdx.org/licenses/"
                        ));
                    }
                }
            }
        }
        None => unreachable!(),
    }

    if check_version && is_set(manifest.get("version")) {
        warnings.push("The version field is present, it is recommended to leave it out if the package is published on Packagist.".into());
    }

    if let Some(name) = manifest
        .get("name")
        .filter(|n| !php_empty(Some(n)))
        .and_then(Value::as_str)
        && name.chars().any(|c| c.is_ascii_uppercase())
    {
        report.publish_errors.push(format!(
            "Name \"{name}\" does not match the best practice (e.g. lower-cased/with-dashes). We suggest using \"{}\" instead. As such you will not be able to submit it to Packagist.",
            suggest_name(name)
        ));
    }

    let warnings = &mut report.warnings;
    if manifest.get("type").and_then(Value::as_str) == Some("composer-installer") {
        warnings.push("The package type 'composer-installer' is deprecated. Please distribute your custom installers as plugins from now on. See https://getcomposer.org/doc/articles/plugins.md for plugin documentation.".into());
    }

    if let (Some(require), Some(require_dev)) =
        (manifest.get("require"), manifest.get("require-dev"))
        && is_set(Some(require))
        && is_set(Some(require_dev))
    {
        let dev_keys: Vec<String> = entries(require_dev).into_iter().map(|(k, _)| k).collect();
        let overrides: Vec<String> = entries(require)
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| dev_keys.contains(k))
            .collect();
        if !overrides.is_empty() {
            let plural = if overrides.len() > 1 { "are" } else { "is" };
            warnings.push(format!(
                "{} {plural} required both in require and require-dev, this can lead to unexpected behavior",
                overrides.join(", ")
            ));
        }
    }

    for link_type in ["provide", "replace"] {
        if let Some(links) = manifest.get(link_type).filter(|v| is_set(Some(v))) {
            for require_type in ["require", "require-dev"] {
                if let Some(requires) = manifest.get(require_type).filter(|v| is_set(Some(v))) {
                    let required: Vec<String> =
                        entries(requires).into_iter().map(|(k, _)| k).collect();
                    for (provide, _) in entries(links) {
                        if required.contains(&provide) {
                            warnings.push(format!(
                                "The package {provide} in {require_type} is also listed in {link_type} which satisfies the requirement. Remove it from {link_type} if you wish to install it."
                            ));
                        }
                    }
                }
            }
        }
    }

    let mut packages = entries(manifest.get("require").unwrap_or(&Value::Null));
    packages.extend(entries(manifest.get("require-dev").unwrap_or(&Value::Null)));
    for (package, version) in &packages {
        if to_string_lossy(version).contains('#') {
            warnings.push(format!(
                "The package \"{package}\" is pointing to a commit-ref, this is bad practice and can cause unforeseen issues."
            ));
        }
    }

    let scripts: Vec<String> = entries(manifest.get("scripts").unwrap_or(&Value::Null))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    for (name, _) in entries(manifest.get("scripts-descriptions").unwrap_or(&Value::Null)) {
        if !scripts.contains(&name) {
            warnings.push(format!(
                "Description for non-existent script \"{name}\" found in \"scripts-descriptions\""
            ));
        }
    }
    for (name, _) in entries(manifest.get("scripts-aliases").unwrap_or(&Value::Null)) {
        if !scripts.contains(&name) {
            warnings.push(format!(
                "Aliases for non-existent script \"{name}\" found in \"scripts-aliases\""
            ));
        }
    }

    if let Some(autoload) = manifest.get("autoload") {
        if is_set(autoload.get("psr-0").and_then(|p| p.get(""))) {
            warnings.push("Defining autoload.psr-0 with an empty namespace prefix is a bad idea for performance".into());
        }
        if is_set(autoload.get("psr-4").and_then(|p| p.get(""))) {
            warnings.push("Defining autoload.psr-4 with an empty namespace prefix is a bad idea for performance".into());
        }
    }

    let mut config = manifest.clone();
    if !is_set(config.get("version")) {
        config.insert("version".into(), Value::String("1.0.0".into()));
    }
    if !is_set(config.get("name")) {
        config.insert("name".into(), Value::String("dummy/dummy".into()));
    }
    let mut loader = Loader {
        config,
        flags,
        errors: vec![],
        warnings: vec![],
    };
    loader.load();
    report.errors.extend(loader.errors);
    report.warnings.extend(loader.warnings);
}

/// `ValidatingArrayLoader`
struct Loader {
    config: Map<String, Value>,
    flags: u8,
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl Loader {
    fn get(&self, key: &str) -> Option<&Value> {
        self.config.get(key).filter(|v| !v.is_null())
    }

    fn unset(&mut self, key: &str) {
        self.config.remove(key);
    }

    fn unset_in(&mut self, key: &str, sub: &str) {
        if let Some(Value::Object(o)) = self.config.get_mut(key) {
            o.remove(sub);
        } else if let Some(Value::Array(a)) = self.config.get_mut(key)
            && let Ok(i) = sub.parse::<usize>()
            && i < a.len()
        {
            a[i] = Value::Null;
        }
    }

    fn validate_string(&mut self, property: &str, mandatory: bool) -> bool {
        if let Some(v) = self.get(property)
            && !v.is_string()
        {
            self.errors.push(format!(
                "{property} : should be a string, {} given",
                debug_type(v)
            ));
            self.unset(property);
            return false;
        }
        if self
            .get(property)
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
        {
            if mandatory {
                self.errors.push(format!("{property} : must be present"));
            }
            self.unset(property);
            return false;
        }
        true
    }

    fn validate_regex(&mut self, property: &str, regex: &str, mandatory: bool) -> bool {
        if !self.validate_string(property, mandatory) {
            return false;
        }
        let value = self
            .get(property)
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        if !Regex::new(&format!("^{regex}$")).unwrap().is_match(&value) {
            let message = format!("{property} : invalid value ({value}), must match {regex}");
            if mandatory {
                self.errors.push(message);
            } else {
                self.warnings.push(message);
            }
            self.unset(property);
            return false;
        }
        true
    }

    fn validate_array(&mut self, property: &str, mandatory: bool) -> bool {
        if let Some(v) = self.get(property)
            && !is_php_array(v)
        {
            self.errors.push(format!(
                "{property} : should be an array, {} given",
                debug_type(v)
            ));
            self.unset(property);
            return false;
        }
        if self.get(property).is_none_or(|v| php_count(v) == 0) {
            if mandatory {
                self.errors.push(format!(
                    "{property} : must be present and contain at least one element"
                ));
            }
            self.unset(property);
            return false;
        }
        true
    }

    fn validate_flat_array(
        &mut self,
        property: &str,
        regex: Option<&str>,
        mandatory: bool,
    ) -> bool {
        if !self.validate_array(property, mandatory) {
            return false;
        }
        let mut pass = true;
        for (key, value) in entries(self.get(property).unwrap()) {
            let is_numeric = match &value {
                Value::Number(_) => true,
                Value::String(s) => s.trim().parse::<f64>().is_ok(),
                _ => false,
            };
            if !value.is_string() && !is_numeric {
                self.errors.push(format!(
                    "{property}.{key} : must be a string or int, {} given",
                    debug_type(&value)
                ));
                self.unset_in(property, &key);
                pass = false;
                continue;
            }
            if let Some(regex) = regex
                && !Regex::new(&format!("^{regex}$"))
                    .unwrap()
                    .is_match(&to_string_lossy(&value))
            {
                self.warnings.push(format!(
                    "{property}.{key} : invalid value ({}), must match {regex}",
                    to_string_lossy(&value)
                ));
                self.unset_in(property, &key);
                pass = false;
            }
        }
        pass
    }

    fn validate_url(&mut self, property: &str, mandatory: bool) -> bool {
        if !self.validate_string(property, mandatory) {
            return false;
        }
        let value = self
            .get(property)
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        if !filter_url(&value, &["http", "https"]) {
            self.warnings.push(format!(
                "{property} : invalid value ({value}), must be an http/https URL"
            ));
            self.unset(property);
            return false;
        }
        true
    }

    fn load(&mut self) {
        self.validate_string("name", true);
        if let Some(name) = self.get("name").and_then(Value::as_str)
            && let Some(err) = naming_error(name, false)
        {
            self.errors.push(format!("name : {err}"));
        }

        if let Some(version) = self.get("version").cloned() {
            match version {
                Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                    let v = to_string_lossy(&version);
                    self.config
                        .insert("version".into(), Value::String(v.clone()));
                    if let Err(e) = semver::normalize(&v, None) {
                        self.errors
                            .push(format!("version : invalid value ({v}): {e}"));
                        self.unset("version");
                    }
                }
                _ => {
                    self.validate_string("version", false);
                }
            }
        }

        if let Some(platform) = self.get("config").and_then(|c| c.get("platform")).cloned() {
            for (key, value) in entries(&platform) {
                if value == Value::Bool(false) {
                    continue;
                }
                let Some(s) = value.as_str() else {
                    self.errors.push(format!(
                        "config.platform.{key} : invalid value ({} {}): expected string or false",
                        debug_type(&value),
                        var_export(&value)
                    ));
                    continue;
                };
                if let Err(e) = semver::normalize(s, None) {
                    self.errors
                        .push(format!("config.platform.{key} : invalid value ({s}): {e}"));
                }
            }
        }

        self.validate_regex("type", "[A-Za-z0-9-]+", false);
        self.validate_string("target-dir", false);
        self.validate_array("extra", false);

        if let Some(bin) = self.get("bin").cloned() {
            if bin.is_string() {
                self.validate_string("bin", false);
            } else {
                self.validate_flat_array("bin", None, false);
            }
            match self.get("bin").cloned() {
                Some(Value::String(b)) => {
                    if let Some(err) = bin_error(&b) {
                        self.errors
                            .push(format!("bin : invalid value ({b}), {err}"));
                        self.unset("bin");
                    }
                }
                Some(list) if is_php_array(&list) => {
                    for (key, b) in entries(&list) {
                        if let Some(b) = b.as_str()
                            && let Some(err) = bin_error(b)
                        {
                            self.errors
                                .push(format!("bin.{key} : invalid value ({b}), {err}"));
                            self.unset_in("bin", &key);
                        }
                    }
                }
                _ => {}
            }
        }

        self.validate_array("scripts", false);
        self.validate_string("description", false);
        self.validate_url("homepage", false);
        self.validate_flat_array("keywords", Some(r"[\p{N}\p{L} ._-]+"), false);

        let mut release_date: Option<i64> = None;
        self.validate_string("time", false);
        if let Some(time) = self.get("time").and_then(Value::as_str).map(str::to_string) {
            match parse_time(&time) {
                Ok(ts) => release_date = Some(ts),
                Err(e) => {
                    self.errors
                        .push(format!("time : invalid value ({time}): {e}"));
                    self.unset("time");
                }
            }
        }

        if let Some(license) = self.get("license").cloned() {
            if is_php_array(&license) || license.is_string() {
                let mut licenses: Vec<Value> = match &license {
                    Value::Array(a) => a.clone(),
                    Value::Object(o) => o.values().cloned().collect(),
                    other => vec![other.clone()],
                };
                licenses.retain(|l| {
                    if l.is_string() {
                        true
                    } else {
                        self.warnings
                            .push(format!("License {} should be a string.", encode_compact(l)));
                        false
                    }
                });
                let eight_days_ago = now_timestamp() - 8 * 86400;
                if release_date.is_none_or(|ts| ts >= eight_days_ago) {
                    for license in &licenses {
                        let license = license.as_str().unwrap();
                        if license == "proprietary" {
                            continue;
                        }
                        let to_validate = license.replace("proprietary", "MIT");
                        if !spdx::is_valid_expression(&to_validate) {
                            if spdx::is_valid_expression(to_validate.trim()) {
                                self.warnings.push(format!(
                                    "License {} must not contain extra spaces, make sure to trim it.",
                                    encode_compact(&Value::String(license.into()))
                                ));
                            } else {
                                self.warnings.push(format!(
                                    "License {} is not a valid SPDX license identifier, see https://spdx.org/licenses/ if you use an open license.\nIf the software is closed-source, you may use \"proprietary\" as license.",
                                    encode_compact(&Value::String(license.into()))
                                ));
                            }
                        }
                    }
                }
                self.config.insert("license".into(), Value::Array(licenses));
            } else {
                self.warnings.push(format!(
                    "License must be a string or array of strings, got {}.",
                    encode_compact(&license)
                ));
                self.unset("license");
            }
        }

        if self.validate_array("authors", false) {
            for (key, author) in entries(self.get("authors").unwrap()) {
                if !is_php_array(&author) {
                    self.errors.push(format!(
                        "authors.{key} : should be an array, {} given",
                        debug_type(&author)
                    ));
                    continue;
                }
                for field in ["homepage", "email", "name", "role"] {
                    if let Some(v) = author.get(field).filter(|v| !v.is_null())
                        && !v.is_string()
                    {
                        self.errors.push(format!(
                            "authors.{key}.{field} : invalid value, must be a string"
                        ));
                    }
                }
                if let Some(homepage) = author.get("homepage").and_then(Value::as_str)
                    && !filter_url(homepage, &["http", "https"])
                {
                    self.warnings.push(format!(
                        "authors.{key}.homepage : invalid value ({homepage}), must be an http/https URL"
                    ));
                }
                if let Some(email) = author.get("email").and_then(Value::as_str)
                    && !is_valid_email(email)
                {
                    self.warnings.push(format!(
                        "authors.{key}.email : invalid value ({email}), must be a valid email address"
                    ));
                }
            }
        }

        if self.validate_array("support", false) {
            let support = self.get("support").cloned().unwrap();
            for key in [
                "issues", "forum", "wiki", "source", "email", "irc", "docs", "rss", "chat",
                "security",
            ] {
                if let Some(v) = support.get(key).filter(|v| !v.is_null())
                    && !v.is_string()
                {
                    self.errors
                        .push(format!("support.{key} : invalid value, must be a string"));
                    self.unset_in("support", key);
                }
            }
            let support = self.get("support").cloned().unwrap_or_default();
            if let Some(email) = support.get("email").and_then(Value::as_str)
                && !is_valid_email(email)
            {
                self.warnings.push(format!(
                    "support.email : invalid value ({email}), must be a valid email address"
                ));
                self.unset_in("support", "email");
            }
            if let Some(irc) = support.get("irc").and_then(Value::as_str)
                && !filter_url(irc, &["irc", "ircs"])
            {
                self.warnings.push(format!(
                    "support.irc : invalid value ({irc}), must be a irc://<server>/<channel> or ircs:// URL"
                ));
                self.unset_in("support", "irc");
            }
            for key in [
                "issues", "forum", "wiki", "source", "docs", "chat", "security",
            ] {
                if let Some(url) = support.get(key).and_then(Value::as_str)
                    && !filter_url(url, &["http", "https"])
                {
                    self.warnings.push(format!(
                        "support.{key} : invalid value ({url}), must be an http/https URL"
                    ));
                    self.unset_in("support", key);
                }
            }
        }

        if self.validate_array("funding", false) {
            for (key, option) in entries(self.get("funding").unwrap()) {
                if !is_php_array(&option) {
                    self.errors.push(format!(
                        "funding.{key} : should be an array, {} given",
                        debug_type(&option)
                    ));
                    continue;
                }
                for field in ["type", "url"] {
                    if let Some(v) = option.get(field).filter(|v| !v.is_null())
                        && !v.is_string()
                    {
                        self.errors.push(format!(
                            "funding.{key}.{field} : invalid value, must be a string"
                        ));
                    }
                }
                if let Some(url) = option.get("url").and_then(Value::as_str)
                    && !filter_url(url, &["http", "https"])
                {
                    self.warnings.push(format!(
                        "funding.{key}.url : invalid value ({url}), must be an http/https URL"
                    ));
                }
            }
        }

        if self.get("php-ext").is_some() && self.validate_array("php-ext", false) {
            self.validate_php_ext();
        }

        let unbound = Constraint::new("=", "10000000-dev");
        let one_zero_dev = Constraint::new(">=", "1.0.0.0-dev");
        for link_type in ["require", "conflict", "provide", "replace", "require-dev"] {
            if !self.validate_array(link_type, false) {
                continue;
            }
            let name = self.get("name").and_then(Value::as_str).map(str::to_string);
            let replace_keys: Vec<String> = entries(self.get("replace").unwrap_or(&Value::Null))
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            let conflict_keys: Vec<String> = entries(self.get("conflict").unwrap_or(&Value::Null))
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            let replace_conflict_overlap = replace_keys.iter().any(|k| conflict_keys.contains(k));
            for (package, constraint) in entries(self.get(link_type).unwrap()) {
                if name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&package))
                {
                    self.errors.push(format!(
                        "{link_type}.{package} : a package cannot set a {link_type} on itself"
                    ));
                    self.unset_in(link_type, &package);
                    continue;
                }
                if let Some(err) = naming_error(&package, true) {
                    self.warnings.push(format!("{link_type}.{err}"));
                } else if !LINK_KEY.is_match(&package) {
                    self.errors.push(format!(
                        "{link_type}.{package} : invalid key, package names must be strings containing only [A-Za-z0-9_./-]"
                    ));
                }
                match constraint.as_str() {
                    None => {
                        self.errors.push(format!(
                            "{link_type}.{package} : invalid value, must be a string containing a version constraint"
                        ));
                        self.unset_in(link_type, &package);
                    }
                    Some("self.version") => {}
                    Some(constraint) => {
                        let parsed = match semver::parse_constraints(constraint) {
                            Ok(c) => c,
                            Err(e) => {
                                self.errors.push(format!(
                                    "{link_type}.{package} : invalid version constraint ({e})"
                                ));
                                self.unset_in(link_type, &package);
                                continue;
                            }
                        };
                        if self.flags & CHECK_UNBOUND_CONSTRAINTS != 0
                            && link_type == "require"
                            && parsed.matches(&unbound)
                            && !is_platform_package(&package)
                        {
                            self.warnings.push(format!(
                                "{link_type}.{package} : unbound version constraints ({constraint}) should be avoided"
                            ));
                        } else if self.flags & CHECK_STRICT_CONSTRAINTS != 0
                            && link_type == "require"
                            && parsed.op() == Some(semver::Op::Eq)
                            && one_zero_dev.matches(&parsed)
                        {
                            self.warnings.push(format!(
                                "{link_type}.{package} : exact version constraints ({constraint}) should be avoided if the package follows semantic versioning"
                            ));
                        }
                        if intervals::compact_constraint(&parsed).is_match_none() {
                            self.warnings.push(format!(
                                "{link_type}.{package} : this version constraint cannot possibly match anything ({constraint})"
                            ));
                        }
                    }
                }
                if link_type == "conflict" && replace_conflict_overlap {
                    self.errors.push(format!(
                        "{link_type}.{package} : you cannot conflict with a package that is also replaced, as replace already creates an implicit conflict rule"
                    ));
                    self.unset_in(link_type, &package);
                }
            }
        }

        if self.validate_array("suggest", false) {
            for (package, description) in entries(self.get("suggest").unwrap()) {
                if !description.is_string() {
                    self.errors.push(format!(
                        "suggest.{package} : invalid value, must be a string describing why the package is suggested"
                    ));
                    self.unset_in("suggest", &package);
                }
            }
        }

        if self.validate_string("minimum-stability", false) {
            let value = self
                .get("minimum-stability")
                .and_then(Value::as_str)
                .unwrap()
                .to_string();
            let known = matches!(
                value.to_lowercase().as_str(),
                "stable" | "beta" | "alpha" | "dev"
            );
            if !known && value != "RC" {
                self.errors.push(format!(
                    "minimum-stability : invalid value ({value}), must be one of stable, RC, beta, alpha, dev"
                ));
                self.unset("minimum-stability");
            }
        }

        if self.validate_array("autoload", false) {
            let types = [
                "psr-0",
                "psr-4",
                "classmap",
                "files",
                "exclude-from-classmap",
            ];
            for (kind, type_config) in entries(self.get("autoload").unwrap()) {
                if !types.contains(&kind.as_str()) {
                    self.errors.push(format!(
                        "autoload : invalid value ({kind}), must be one of {}",
                        types.join(", ")
                    ));
                    self.unset_in("autoload", &kind);
                }
                if kind == "psr-4" {
                    for (namespace, _) in entries(&type_config) {
                        if !namespace.is_empty() && !namespace.ends_with('\\') {
                            self.errors.push(format!(
                                "autoload.psr-4 : invalid value ({namespace}), namespaces must end with a namespace separator, should be {namespace}\\\\"
                            ));
                        }
                    }
                }
            }
        }

        if self.get("autoload").and_then(|a| a.get("psr-4")).is_some()
            && self.get("target-dir").is_some()
        {
            self.errors.push("target-dir : this can not be used together with the autoload.psr-4 setting, remove target-dir to upgrade to psr-4".into());
        }

        for src_type in ["source", "dist"] {
            if !self.validate_array(src_type, false) {
                continue;
            }
            let src = self.get(src_type).cloned().unwrap();
            if !is_set(src.get("type")) {
                self.errors
                    .push(format!("{src_type}.type : must be present"));
            }
            if !is_set(src.get("url")) {
                self.errors
                    .push(format!("{src_type}.url : must be present"));
            }
            if src_type == "source" && !is_set(src.get("reference")) {
                self.errors
                    .push(format!("{src_type}.reference : must be present"));
            }
            if let Some(v) = src.get("type").filter(|v| is_set(Some(v)))
                && !v.is_string()
            {
                self.errors.push(format!(
                    "{src_type}.type : should be a string, {} given",
                    debug_type(v)
                ));
            }
            if let Some(v) = src.get("url").filter(|v| is_set(Some(v)))
                && !v.is_string()
            {
                self.errors.push(format!(
                    "{src_type}.url : should be a string, {} given",
                    debug_type(v)
                ));
            }
            if let Some(v) = src.get("reference").filter(|v| is_set(Some(v)))
                && !v.is_string()
                && !matches!(v, Value::Number(n) if n.is_i64())
            {
                self.errors.push(format!(
                    "{src_type}.reference : should be a string or int, {} given",
                    debug_type(v)
                ));
            }
            if let Some(v) = src.get("reference").filter(|v| is_set(Some(v)))
                && LEADING_DASH.is_match(&to_string_lossy(v))
            {
                self.errors.push(format!(
                    "{src_type}.reference : must not start with a \"-\", \"{}\" given",
                    to_string_lossy(v)
                ));
            }
            if let Some(v) = src.get("url").filter(|v| is_set(Some(v)))
                && LEADING_DASH.is_match(&to_string_lossy(v))
            {
                self.errors.push(format!(
                    "{src_type}.url : must not start with a \"-\", \"{}\" given",
                    to_string_lossy(v)
                ));
            }
        }

        self.validate_flat_array("include-path", None, false);
        self.validate_array("transport-options", false);

        if let Some(aliases) = self
            .get("extra")
            .and_then(|e| e.get("branch-alias"))
            .cloned()
        {
            if !is_php_array(&aliases) {
                self.errors
                    .push("extra.branch-alias : must be an array of versions => aliases".into());
            } else {
                for (source_branch, target_branch) in entries(&aliases) {
                    let Some(target) = target_branch.as_str() else {
                        self.warnings.push(format!(
                            "extra.branch-alias.{source_branch} : the target branch ({}) must be a string, \"{}\" received.",
                            encode_compact(&target_branch),
                            debug_type(&target_branch)
                        ));
                        continue;
                    };
                    if !target.ends_with("-dev") {
                        self.warnings.push(format!(
                            "extra.branch-alias.{source_branch} : the target branch ({target}) must end in -dev"
                        ));
                        continue;
                    }
                    let validated = semver::normalize_branch(&target[..target.len() - 4]);
                    if !validated.ends_with("-dev") {
                        self.warnings.push(format!(
                            "extra.branch-alias.{source_branch} : the target branch ({target}) must be a parseable number like 2.0-dev"
                        ));
                        continue;
                    }
                    if let (Some(source_prefix), Some(target_prefix)) = (
                        semver::parse_numeric_alias_prefix(&source_branch),
                        semver::parse_numeric_alias_prefix(target),
                    ) && !target_prefix
                        .to_lowercase()
                        .starts_with(&source_prefix.to_lowercase())
                    {
                        self.warnings.push(format!(
                            "extra.branch-alias.{source_branch} : the target branch ({target}) is not a valid numeric alias for this version"
                        ));
                    }
                }
            }
        }
    }

    fn validate_php_ext(&mut self) {
        let kind = self.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "php-ext" | "php-ext-zend") {
            self.errors.push("php-ext can only be set by packages of type \"php-ext\" or \"php-ext-zend\" which must be C extensions".into());
            self.unset("php-ext");
            return;
        }
        let ext = self.get("php-ext").cloned().unwrap();
        let field = |name: &str| ext.get(name).filter(|v| !v.is_null());
        if let Some(v) = field("extension-name")
            && !v.is_string()
        {
            self.errors.push(format!(
                "php-ext.extension-name : should be a string, {} given",
                debug_type(v)
            ));
        }
        if let Some(v) = field("priority")
            && !matches!(v, Value::Number(n) if n.is_i64())
        {
            self.errors.push(format!(
                "php-ext.priority : should be an integer, {} given",
                debug_type(v)
            ));
        }
        for name in ["support-zts", "support-nts"] {
            if let Some(v) = field(name)
                && !v.is_boolean()
            {
                self.errors.push(format!(
                    "php-ext.{name} : should be a boolean, {} given",
                    debug_type(v)
                ));
            }
        }
        if let Some(v) = field("build-path")
            && !v.is_string()
        {
            self.errors.push(format!(
                "php-ext.build-path : should be a string or null, {} given",
                debug_type(v)
            ));
        }
        if let Some(v) = field("download-url-method") {
            if !is_php_array(v) && !v.is_string() {
                self.errors.push(format!(
                    "php-ext.download-url-method : should be an array or a string, {} given",
                    debug_type(v)
                ));
            } else {
                let valid = [
                    "composer-default",
                    "pre-packaged-source",
                    "pre-packaged-binary",
                ];
                let methods = if v.is_string() {
                    vec![("0".to_string(), v.clone())]
                } else {
                    entries(v)
                };
                if methods.is_empty() {
                    self.errors.push(
                        "php-ext.download-url-method : must contain at least one element".into(),
                    );
                }
                for (key, method) in methods {
                    match method.as_str() {
                        None => self.errors.push(format!(
                            "php-ext.download-url-method.{key} : should be a string, {} given",
                            debug_type(&method)
                        )),
                        Some(m) if !valid.contains(&m) => self.errors.push(format!(
                            "php-ext.download-url-method.{key} : invalid value ({m}), must be one of {}",
                            valid.join(", ")
                        )),
                        _ => {}
                    }
                }
            }
        }
        if field("os-families").is_some() && field("os-families-exclude").is_some() {
            self.errors.push(
                "php-ext : os-families and os-families-exclude cannot both be specified".into(),
            );
        } else {
            let valid = ["windows", "bsd", "darwin", "solaris", "linux", "unknown"];
            for name in ["os-families", "os-families-exclude"] {
                let Some(v) = field(name) else { continue };
                if !is_php_array(v) {
                    self.errors.push(format!(
                        "php-ext.{name} : should be an array, {} given",
                        debug_type(v)
                    ));
                } else if php_count(v) == 0 {
                    self.errors.push(format!(
                        "php-ext.{name} : must contain at least one element"
                    ));
                } else {
                    for (key, family) in entries(v) {
                        match family.as_str() {
                            None => self.errors.push(format!(
                                "php-ext.{name}.{key} : should be a string, {} given",
                                debug_type(&family)
                            )),
                            Some(f) if !valid.contains(&f) => self.errors.push(format!(
                                "php-ext.{name}.{key} : invalid value ({f}), must be one of {}",
                                valid.join(", ")
                            )),
                            _ => {}
                        }
                    }
                }
            }
        }
        if let Some(options) = field("configure-options") {
            if !is_php_array(options) {
                self.errors.push(format!(
                    "php-ext.configure-options : should be an array, {} given",
                    debug_type(options)
                ));
            } else {
                for (key, option) in entries(options) {
                    if !is_php_array(&option) {
                        self.errors.push(format!(
                            "php-ext.configure-options.{key} : should be an array, {} given",
                            debug_type(&option)
                        ));
                        continue;
                    }
                    match option.get("name").filter(|v| !v.is_null()) {
                        None => {
                            self.errors.push(format!(
                                "php-ext.configure-options.{key}.name : must be present"
                            ));
                            continue;
                        }
                        Some(n) if !n.is_string() => {
                            self.errors.push(format!(
                                "php-ext.configure-options.{key}.name : should be a string, {} given",
                                debug_type(n)
                            ));
                            continue;
                        }
                        _ => {}
                    }
                    if let Some(v) = option.get("needs-value").filter(|v| !v.is_null())
                        && !v.is_boolean()
                    {
                        self.errors.push(format!(
                            "php-ext.configure-options.{key}.needs-value : should be a boolean, {} given",
                            debug_type(v)
                        ));
                    }
                    if let Some(v) = option.get("description").filter(|v| !v.is_null())
                        && !v.is_string()
                    {
                        self.errors.push(format!(
                            "php-ext.configure-options.{key}.description : should be a string, {} given",
                            debug_type(v)
                        ));
                    }
                }
            }
        }
    }
}

/// `ValidatingArrayLoader::findBinError`
fn bin_error(bin: &str) -> Option<&'static str> {
    if DOTDOT.is_match(bin) {
        return Some("must not contain a \"..\" path component");
    }
    if UNSAFE_BIN.is_match(bin) {
        return Some(
            "must not contain any of the characters *$`\"&^|<>()%!; nor control characters",
        );
    }
    None
}

fn now_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A small stand-in for `new \DateTime($time)`: ISO-like dates give a timestamp,
/// relative keywords and `@ts` are accepted, anything else fails as PHP would.
fn parse_time(time: &str) -> Result<i64, String> {
    if let Some(m) = ISO_DATE.captures(time) {
        let (y, mo, d): (i64, u32, u32) = (
            m[1].parse().unwrap(),
            m[2].parse().unwrap(),
            m[3].parse().unwrap(),
        );
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
            let pos = if !(1..=12).contains(&mo) { 5 } else { 8 };
            return Err(format!(
                "Failed to parse time string ({time}) at position {pos} ({}): Unexpected character",
                time.as_bytes()[pos] as char
            ));
        }
        return Ok(days_from_civil(y, mo, d) * 86400);
    }
    if let Some(ts) = time.strip_prefix('@')
        && let Ok(ts) = ts.parse::<i64>()
    {
        return Ok(ts);
    }
    if time
        .bytes()
        .all(|b| b.is_ascii_digit() || b"/ :-".contains(&b))
        && time.bytes().any(|b| b.is_ascii_digit())
    {
        return Ok(now_timestamp());
    }
    if matches!(
        time.to_lowercase().as_str(),
        "now" | "today" | "yesterday" | "tomorrow" | "midnight" | "noon"
    ) {
        return Ok(now_timestamp());
    }
    Err(format!(
        "Failed to parse time string ({time}) at position 0 ({}): The timezone could not be found in the database",
        time.chars().next().unwrap_or(' ')
    ))
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(json: &str) -> Report {
        let v: Value = serde_json::from_str(json).unwrap();
        let mut r = Report::default();
        validate_manifest(v.as_object().unwrap(), CHECK_ALL, true, &mut r);
        r
    }

    // Wording checked against `composer validate` 2.11.1.
    #[test]
    fn warnings_match_composer() {
        let r = report(
            r#"{"name":"a/b","description":"d","license":"MIT","version":"1.0.0","require":{"php":">=8","foo/bar":"*"}}"#,
        );
        assert_eq!(
            r.warnings,
            vec![
                "The version field is present, it is recommended to leave it out if the package is published on Packagist.",
                "require.foo/bar : unbound version constraints (*) should be avoided",
            ]
        );
        let r = report(r#"{"name":"a/b","description":"d","license":"GPL-2.0"}"#);
        assert_eq!(
            r.warnings,
            vec![
                "License \"GPL-2.0\" is a deprecated SPDX license identifier, use \"GPL-2.0-only\" or \"GPL-2.0-or-later\" instead"
            ]
        );
        let r = report(r#"{"name":"a/b","description":"d","license":"Foo"}"#);
        assert_eq!(
            r.warnings,
            vec![
                "License \"Foo\" is not a valid SPDX license identifier, see https://spdx.org/licenses/ if you use an open license.\nIf the software is closed-source, you may use \"proprietary\" as license."
            ]
        );
        let r = report(r#"{"description":"d"}"#);
        assert_eq!(
            r.warnings,
            vec![
                "No license specified, it is recommended to do so. For closed-source software you may use \"proprietary\" as license."
            ]
        );
    }

    #[test]
    fn loader_errors_match_composer() {
        let r = report(
            r#"{"name":"a/b","license":"MIT","require":{"a/b":"1.0","c/d":"^^1","e/f":">2 <1"},"conflict":{"c/d":"1.0"},"replace":{"c/d":"1.0"},"minimum-stability":"rc","autoload":{"psr-4":{"Foo":"src/"},"nope":[]}}"#,
        );
        assert_eq!(
            r.errors,
            vec![
                "require.a/b : a package cannot set a require on itself",
                "require.c/d : invalid version constraint (Could not parse version constraint ^^1: Invalid version string \"^^1\")",
                "conflict.c/d : you cannot conflict with a package that is also replaced, as replace already creates an implicit conflict rule",
                "minimum-stability : invalid value (rc), must be one of stable, RC, beta, alpha, dev",
                "autoload.psr-4 : invalid value (Foo), namespaces must end with a namespace separator, should be Foo\\\\",
                "autoload : invalid value (nope), must be one of psr-0, psr-4, classmap, files, exclude-from-classmap",
            ]
        );
        assert!(
            r.warnings.contains(
                &"require.e/f : this version constraint cannot possibly match anything (>2 <1)"
                    .to_string()
            )
        );
        let r = report(
            r#"{"name":"a/b","license":"MIT","type":"composer-installer","authors":[{"name":"x","email":"nope","homepage":"ftp://x"}]}"#,
        );
        assert_eq!(
            r.warnings,
            vec![
                "The package type 'composer-installer' is deprecated. Please distribute your custom installers as plugins from now on. See https://getcomposer.org/doc/articles/plugins.md for plugin documentation.",
                "authors.0.homepage : invalid value (ftp://x), must be an http/https URL",
                "authors.0.email : invalid value (nope), must be a valid email address",
            ]
        );
    }

    #[test]
    fn email_and_url_filters() {
        for (email, ok) in [
            ("a@b", false),
            ("a@b.c", true),
            ("a.b+c@d-e.fg", true),
            ("a@b..c", false),
            ("é@b.c", false),
            ("a@[1.2.3.4]", true),
        ] {
            assert_eq!(is_valid_email(email), ok, "{email}");
        }
        assert!(filter_url("https://example.com/x", &["http", "https"]));
        assert!(filter_url("", &["http"]));
        assert!(!filter_url("example.com", &["http"]));
        assert!(!filter_url("ftp://x", &["http"]));
        assert!(filter_url(
            "irc://irc.freenode.org/composer",
            &["irc", "ircs"]
        ));
    }
}
