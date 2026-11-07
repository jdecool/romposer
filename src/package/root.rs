//! Port of `RootPackageLoader`: the project's own `composer.json` as a package,
//! plus the root-only data (aliases, stability flags, references, config).

use super::{LinkType, Package, load};
use crate::semver::{self, Stability};
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// `RootPackage::DEFAULT_PRETTY_VERSION`
pub const DEFAULT_PRETTY_VERSION: &str = "1.0.0+no-version-set";

#[derive(Clone, Debug, PartialEq)]
pub struct AliasDef {
    pub package: String,
    pub version: String,
    pub alias: String,
    pub alias_normalized: String,
}

#[derive(Clone, Debug)]
pub struct RootPackage {
    pub package: Package,
    pub minimum_stability: Stability,
    pub prefer_stable: bool,
    pub stability_flags: BTreeMap<String, u8>,
    pub aliases: Vec<AliasDef>,
    pub references: BTreeMap<String, String>,
    pub config: Map<String, Value>,
    /// Set when no `version` was given and `1.0.0` was assumed.
    pub auto_versioned: bool,
}

static ALIAS_IN_REQUIRE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|\| *|, *)([^,\s#|]+)(?:#[^ ]+)? +as +([^,\s|]+)(?:$| *\|| *,)").unwrap()
});
static ALIAS_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([^,\s@]+) as .+$").unwrap());
static OR_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\|\|?\s*").unwrap());
static AND_SPLIT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?<!^|as|[=>< ,]) *(?<!-)[, ](?!-) *(?!,|as|$)").unwrap()
});
static STABILITY_FLAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[^@]*?@(stable|RC|beta|alpha|dev)$").unwrap());
static PLAIN_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^,\s@]+$").unwrap());
static REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^,\s@]+?#([a-f0-9]+)$").unwrap());

/// `RootPackageLoader::load` without version guessing: a missing `version` is
/// `1.0.0` (Composer's own fallback when git gives nothing).
pub fn load_root(config: &Map<String, Value>) -> Result<RootPackage, String> {
    let mut config = config.clone();
    match config.get("name").and_then(Value::as_str) {
        None => {
            config.insert("name".into(), Value::String("__root__".into()));
        }
        Some(name) => {
            if let Some(err) = super::naming_error(name, false) {
                return Err(format!("Your package name {err}"));
            }
        }
    }
    let mut auto_versioned = false;
    if !config.contains_key("version") {
        config.insert("version".into(), Value::String("1.0.0".into()));
        auto_versioned = true;
    }

    let mut package = load(&config)?;
    if auto_versioned {
        match package.alias_of.as_deref_mut() {
            Some(real) => real.pretty_version = DEFAULT_PRETTY_VERSION.to_string(),
            None => package.pretty_version = DEFAULT_PRETTY_VERSION.to_string(),
        }
    }

    let minimum_stability = match config.get("minimum-stability").and_then(Value::as_str) {
        Some(s) => Stability::parse(s)?,
        None => Stability::Stable,
    };

    let name = config["name"].as_str().unwrap().to_string();
    let mut aliases = Vec::new();
    let mut stability_flags = BTreeMap::new();
    let mut references = BTreeMap::new();
    let real = package.alias_of.as_deref().unwrap_or(&package);
    for kind in [LinkType::Require, LinkType::DevRequire] {
        if config.get(kind.key()).is_some() {
            let links: Vec<(String, String)> = real
                .links(kind)
                .iter()
                .map(|l| (l.target.clone(), l.pretty_constraint.clone()))
                .collect();
            extract_aliases(&links, &mut aliases)?;
            extract_stability_flags(&links, minimum_stability, &mut stability_flags);
            extract_references(&links, &mut references);
            if links.iter().any(|(target, _)| *target == name) {
                return Err(format!(
                    "Root package '{name}' cannot require itself in its composer.json\nDid you accidentally name your root package after an external package?"
                ));
            }
        }
    }
    for kind in LinkType::ALL {
        if let Some(Value::Object(links)) = config.get(kind.key()) {
            for link_name in links.keys() {
                if let Some(err) = super::naming_error(link_name, true) {
                    return Err(format!("{}.{err}", kind.key()));
                }
            }
        }
    }

    let prefer_stable = match config.get("prefer-stable") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) | None => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !(s.is_empty() || s == "0"),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    };
    let root_config = match config.get("config") {
        Some(Value::Object(c)) => c.clone(),
        _ => Map::new(),
    };

    Ok(RootPackage {
        package,
        minimum_stability,
        prefer_stable,
        stability_flags,
        aliases,
        references,
        config: root_config,
        auto_versioned,
    })
}

fn extract_aliases(
    requires: &[(String, String)],
    aliases: &mut Vec<AliasDef>,
) -> Result<(), String> {
    for (name, version) in requires {
        if let Some(m) = ALIAS_IN_REQUIRE.captures(version) {
            aliases.push(AliasDef {
                package: name.to_lowercase(),
                version: semver::normalize(&m[1], Some(version))?,
                alias: m[2].to_string(),
                alias_normalized: semver::normalize(&m[2], Some(version))?,
            });
        } else if version.contains(" as ") {
            return Err(format!(
                "Invalid alias definition in \"{name}\": \"{version}\". Aliases should be in the form \"exact-version as other-exact-version\"."
            ));
        }
    }
    Ok(())
}

/// `RootPackageLoader::extractStabilityFlags`
pub fn extract_stability_flags(
    requires: &[(String, String)],
    minimum_stability: Stability,
    flags: &mut BTreeMap<String, u8>,
) {
    let minimum = minimum_stability.priority();
    for (name, version) in requires {
        let name = name.to_lowercase();
        let mut constraints: Vec<String> = Vec::new();
        for or_constraint in OR_SPLIT.split(version.trim()) {
            for and_constraint in AND_SPLIT.split(or_constraint) {
                constraints.push(and_constraint.unwrap().to_string());
            }
        }
        let mut matched = false;
        for constraint in &constraints {
            if let Some(m) = STABILITY_FLAG.captures(constraint) {
                let stability = Stability::parse(&m[1]).unwrap().priority();
                if flags.get(&name).is_some_and(|f| *f > stability) {
                    continue;
                }
                flags.insert(name.clone(), stability);
                matched = true;
            }
        }
        if matched {
            continue;
        }
        for constraint in &constraints {
            let req_version = ALIAS_PREFIX.replace(constraint, "$1");
            if PLAIN_VERSION.is_match(&req_version) {
                let stability_name = semver::parse_stability(&req_version);
                if stability_name == Stability::Stable {
                    continue;
                }
                let stability = stability_name.priority();
                if flags.get(&name).is_some_and(|f| *f > stability) || minimum > stability {
                    continue;
                }
                flags.insert(name.clone(), stability);
            }
        }
    }
}

pub fn extract_references(
    requires: &[(String, String)],
    references: &mut BTreeMap<String, String>,
) {
    for (name, version) in requires {
        let req_version = ALIAS_PREFIX.replace(version, "$1").into_owned();
        if let Some(m) = REFERENCE.captures(&req_version)
            && semver::parse_stability(&req_version) == Stability::Dev
        {
            references.insert(name.to_lowercase(), m[1].to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(json: &str) -> Result<RootPackage, String> {
        let v: Value = serde_json::from_str(json).unwrap();
        load_root(v.as_object().unwrap())
    }

    #[test]
    fn defaults_and_flags() {
        let r = root(r#"{"name":"a/b","require":{"c/d":"dev-main as 1.0.0","e/f":"1.0@beta","g/h":"dev-x#abc12f"},"minimum-stability":"RC","prefer-stable":true}"#)
            .unwrap();
        assert_eq!(r.package.pretty_version, DEFAULT_PRETTY_VERSION);
        assert_eq!(r.package.version, "1.0.0.0");
        assert_eq!(r.minimum_stability, Stability::RC);
        assert!(r.prefer_stable);
        assert_eq!(r.aliases[0].alias_normalized, "1.0.0.0");
        assert_eq!(r.stability_flags.get("e/f"), Some(&10));
        assert_eq!(r.stability_flags.get("c/d"), Some(&20));
        assert_eq!(r.references.get("g/h").map(String::as_str), Some("abc12f"));
    }

    #[test]
    fn composer_error_messages() {
        assert_eq!(
            root(r#"{"name":"a/b","require":{"a/b":"1.0"}}"#).unwrap_err(),
            "Root package 'a/b' cannot require itself in its composer.json\nDid you accidentally name your root package after an external package?"
        );
        assert!(
            root(r#"{"name":"A/b"}"#)
                .unwrap_err()
                .starts_with("Your package name A/b is invalid, it should not contain uppercase")
        );
        assert!(
            root(r#"{"name":"a/b","require":{"C/d":"1.0"}}"#)
                .unwrap_err()
                .starts_with("require.C/d is invalid")
        );
        assert_eq!(root(r#"{}"#).unwrap().package.name, "__root__");
    }
}
