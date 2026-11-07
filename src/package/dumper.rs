//! Port of `ArrayDumper`: a package back to the array shape written in
//! `composer.lock` and `installed.json`, key order included.

use super::{LinkType, Package};
use crate::json::{Map, Value};

fn sorted_links(links: &[super::Link]) -> Value {
    let mut entries: Vec<(&str, &str)> = links
        .iter()
        .map(|l| (l.target.as_str(), l.pretty_constraint.as_str()))
        .collect();
    // PHP arrays keep the last value for a duplicate key, then ksort
    let mut map = Map::new();
    for (k, v) in entries.drain(..) {
        map.insert(k.to_string(), Value::String(v.to_string()));
    }
    let mut sorted: Vec<(String, Value)> = map.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(sorted.into_iter().collect())
}

/// `ArrayDumper::dump`
pub fn dump(package: &Package) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("name".into(), Value::String(package.pretty_name.clone()));
    data.insert(
        "version".into(),
        Value::String(package.pretty_version.clone()),
    );
    data.insert(
        "version_normalized".into(),
        Value::String(package.version.clone()),
    );
    if let Some(target_dir) = &package.target_dir {
        data.insert("target-dir".into(), Value::String(target_dir.clone()));
    }
    if let Some(source) = &package.source {
        let mut s = Map::new();
        s.insert("type".into(), Value::String(source.kind.clone()));
        s.insert("url".into(), Value::String(source.url.clone()));
        if let Some(reference) = &source.reference {
            s.insert("reference".into(), Value::String(reference.clone()));
        }
        if let Some(mirrors) = source.mirrors.as_ref().filter(|m| !php_empty(m)) {
            s.insert("mirrors".into(), mirrors.clone());
        }
        data.insert("source".into(), Value::Object(s));
    }
    if let Some(dist) = &package.dist {
        let mut d = Map::new();
        d.insert("type".into(), Value::String(dist.kind.clone()));
        d.insert("url".into(), Value::String(dist.url.clone()));
        if let Some(reference) = &dist.reference {
            d.insert("reference".into(), Value::String(reference.clone()));
        }
        if let Some(shasum) = &dist.shasum {
            d.insert("shasum".into(), Value::String(shasum.clone()));
        }
        if let Some(mirrors) = dist.mirrors.as_ref().filter(|m| !php_empty(m)) {
            d.insert("mirrors".into(), mirrors.clone());
        }
        data.insert("dist".into(), Value::Object(d));
    }
    for kind in LinkType::ALL {
        let links = package.links(kind);
        if !links.is_empty() {
            data.insert(kind.key().into(), sorted_links(links));
        }
    }
    if !package.suggests.is_empty() {
        let mut sorted: Vec<(String, Value)> = package
            .suggests
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        data.insert(
            "suggest".into(),
            Value::Object(sorted.into_iter().collect()),
        );
    }
    if let Some(time) = &package.release_date {
        data.insert("time".into(), Value::String(rfc3339(time)));
    }
    if let Some(time) = &package.published_date {
        data.insert("published-time".into(), Value::String(rfc3339(time)));
    }
    if package.default_branch {
        data.insert("default-branch".into(), Value::Bool(true));
    }
    if !package.binaries.is_empty() {
        data.insert(
            "bin".into(),
            Value::Array(
                package
                    .binaries
                    .iter()
                    .map(|b| Value::String(b.clone()))
                    .collect(),
            ),
        );
    }
    data.insert("type".into(), Value::String(package.kind.clone()));
    if !package.extra.is_empty() {
        data.insert("extra".into(), Value::Object(package.extra.clone()));
    }
    if let Some(source) = &package.installation_source {
        data.insert("installation-source".into(), Value::String(source.clone()));
    }
    if !package.autoload.is_empty() {
        data.insert("autoload".into(), Value::Object(package.autoload.clone()));
    }
    if !package.dev_autoload.is_empty() {
        data.insert(
            "autoload-dev".into(),
            Value::Object(package.dev_autoload.clone()),
        );
    }
    if let Some(url) = &package.notification_url {
        data.insert("notification-url".into(), Value::String(url.clone()));
    }
    if !package.include_paths.is_empty() {
        data.insert(
            "include-path".into(),
            Value::Array(
                package
                    .include_paths
                    .iter()
                    .map(|p| Value::String(p.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(ext) = package.php_ext.as_ref().filter(|v| !php_empty(v)) {
        data.insert("php-ext".into(), ext.clone());
    }
    if package.archive_name.is_some() || !package.archive_excludes.is_empty() {
        let mut a = Map::new();
        if let Some(name) = &package.archive_name {
            a.insert("name".into(), Value::String(name.clone()));
        }
        if !package.archive_excludes.is_empty() {
            a.insert(
                "exclude".into(),
                Value::Array(
                    package
                        .archive_excludes
                        .iter()
                        .map(|e| Value::String(e.clone()))
                        .collect(),
                ),
            );
        }
        data.insert("archive".into(), Value::Object(a));
    }
    if !package.scripts.is_empty() {
        data.insert("scripts".into(), Value::Object(package.scripts.clone()));
    }
    if !package.license.is_empty() {
        data.insert(
            "license".into(),
            Value::Array(
                package
                    .license
                    .iter()
                    .map(|l| Value::String(l.clone()))
                    .collect(),
            ),
        );
    }
    if !package.authors.is_empty() {
        data.insert("authors".into(), Value::Array(package.authors.clone()));
    }
    if let Some(description) = &package.description {
        data.insert("description".into(), Value::String(description.clone()));
    }
    if let Some(homepage) = &package.homepage {
        data.insert("homepage".into(), Value::String(homepage.clone()));
    }
    if !package.keywords.is_empty() {
        let mut keywords = package.keywords.clone();
        keywords.sort();
        data.insert(
            "keywords".into(),
            Value::Array(keywords.into_iter().map(Value::String).collect()),
        );
    }
    if !package.support.is_empty() {
        data.insert("support".into(), Value::Object(package.support.clone()));
    }
    if !package.funding.is_empty() {
        data.insert("funding".into(), Value::Array(package.funding.clone()));
    }
    if let Some(abandoned) = &package.abandoned
        && !matches!(abandoned, Value::Bool(false) | Value::Null)
    {
        data.insert(
            "abandoned".into(),
            match abandoned {
                Value::String(s) if !s.is_empty() => Value::String(s.clone()),
                _ => Value::Bool(true),
            },
        );
    }
    if let Some(options) = package.transport_options.as_ref().filter(|v| !php_empty(v)) {
        data.insert("transport-options".into(), options.clone());
    }
    data
}

fn php_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::String(s) => s.is_empty() || s == "0",
        Value::Number(n) => n.as_f64() == Some(0.0),
    }
}

/// `DateTime::format(DATE_RFC3339)` for the forms Composer stores in locks.
pub fn rfc3339(time: &str) -> String {
    let t = time.trim();
    if t.len() >= 25
        && t.as_bytes()[10] == b'T'
        && (t.ends_with('Z') || t[19..].contains(['+', '-']))
    {
        if let Some(stripped) = t.strip_suffix('Z') {
            return format!("{stripped}+00:00");
        }
        return t.to_string();
    }
    if t.len() == 19 && t.as_bytes()[10] == b' ' {
        return format!("{}T{}+00:00", &t[..10], &t[11..]);
    }
    if t.len() == 10 {
        return format!("{t}T00:00:00+00:00");
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::encode_pretty;

    // A psr/log entry copied from a Composer-written composer.lock; the dump must
    // reproduce it byte for byte (with the lock's key order).
    const PSR_LOG: &str = r#"{
    "name": "psr/log",
    "version": "3.0.2",
    "source": {
        "type": "git",
        "url": "https://github.com/php-fig/log.git",
        "reference": "f16e1d5863e37f8d8c2a01719f5b34baa2b714d3"
    },
    "dist": {
        "type": "zip",
        "url": "https://api.github.com/repos/php-fig/log/zipball/f16e1d5863e37f8d8c2a01719f5b34baa2b714d3",
        "reference": "f16e1d5863e37f8d8c2a01719f5b34baa2b714d3",
        "shasum": ""
    },
    "require": {
        "php": ">=8.0.0"
    },
    "type": "library",
    "extra": {
        "branch-alias": {
            "dev-master": "3.x-dev"
        }
    },
    "autoload": {
        "psr-4": {
            "Psr\\Log\\": "src"
        }
    },
    "notification-url": "https://packagist.org/downloads/",
    "license": [
        "MIT"
    ],
    "authors": [
        {
            "name": "PHP-FIG",
            "homepage": "https://www.php-fig.org/"
        }
    ],
    "description": "Common interface for logging libraries",
    "homepage": "https://github.com/php-fig/log",
    "keywords": [
        "log",
        "psr",
        "psr-3"
    ],
    "support": {
        "source": "https://github.com/php-fig/log/tree/3.0.2"
    },
    "time": "2024-09-11T13:17:53+00:00"
}"#;

    #[test]
    fn round_trips_a_lock_entry_with_installed_json_key_order() {
        let value: Value = serde_json::from_str(PSR_LOG).unwrap();
        let package = super::super::load(value.as_object().unwrap()).unwrap();
        let dumped = dump(&package);
        let mut expected: Value = serde_json::from_str(PSR_LOG).unwrap();
        // installed.json / ArrayDumper puts version_normalized after version and time after suggest
        let obj = expected.as_object_mut().unwrap();
        let time = obj.remove("time").unwrap();
        let mut reordered = Map::new();
        for (k, v) in obj.iter() {
            reordered.insert(k.clone(), v.clone());
            if k == "version" {
                reordered.insert("version_normalized".into(), Value::String("3.0.2.0".into()));
            }
            if k == "require" {
                reordered.insert("time".into(), time.clone());
            }
        }
        assert_eq!(
            encode_pretty(&Value::Object(dumped)),
            encode_pretty(&Value::Object(reordered))
        );
    }
}
