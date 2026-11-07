//! Package model and the port of `ArrayLoader` (array → package).

pub mod root;

use crate::semver::{self, Constraint};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkType {
    Require,
    DevRequire,
    Conflict,
    Provide,
    Replace,
}

impl LinkType {
    /// The JSON key (`require`, `require-dev`, ...).
    pub fn key(self) -> &'static str {
        match self {
            LinkType::Require => "require",
            LinkType::DevRequire => "require-dev",
            LinkType::Conflict => "conflict",
            LinkType::Provide => "provide",
            LinkType::Replace => "replace",
        }
    }

    /// `Link::TYPE_*`, used in messages ("requires", "conflicts", ...).
    pub fn description(self) -> &'static str {
        match self {
            LinkType::Require => "requires",
            LinkType::DevRequire => "requires (for development)",
            LinkType::Conflict => "conflicts",
            LinkType::Provide => "provides",
            LinkType::Replace => "replaces",
        }
    }

    pub const ALL: [LinkType; 5] = [
        LinkType::Require,
        LinkType::Conflict,
        LinkType::Provide,
        LinkType::Replace,
        LinkType::DevRequire,
    ];
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub source: String,
    pub target: String,
    pub constraint: Constraint,
    pub pretty_constraint: String,
    pub kind: LinkType,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Source {
    pub kind: String,
    pub url: String,
    pub reference: Option<String>,
    pub mirrors: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dist {
    pub kind: String,
    pub url: String,
    pub reference: Option<String>,
    pub shasum: Option<String>,
    pub mirrors: Option<Value>,
}

/// `CompletePackage` (and `CompleteAliasPackage` when `alias_of` is set).
#[derive(Clone, Debug, PartialEq)]
pub struct Package {
    pub name: String,
    pub pretty_name: String,
    pub version: String,
    pub pretty_version: String,
    pub kind: String,
    pub target_dir: Option<String>,
    pub extra: Map<String, Value>,
    pub binaries: Vec<String>,
    pub installation_source: Option<String>,
    pub default_branch: bool,
    pub source: Option<Source>,
    pub dist: Option<Dist>,
    pub requires: Vec<Link>,
    pub dev_requires: Vec<Link>,
    pub conflicts: Vec<Link>,
    pub provides: Vec<Link>,
    pub replaces: Vec<Link>,
    pub suggests: Map<String, Value>,
    pub autoload: Map<String, Value>,
    pub dev_autoload: Map<String, Value>,
    pub include_paths: Vec<String>,
    pub php_ext: Option<Value>,
    pub release_date: Option<String>,
    pub notification_url: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub keywords: Vec<String>,
    pub license: Vec<String>,
    pub authors: Vec<Value>,
    pub support: Map<String, Value>,
    pub funding: Vec<Value>,
    pub abandoned: Option<Value>,
    pub archive_name: Option<String>,
    pub archive_excludes: Vec<String>,
    pub scripts: Map<String, Value>,
    pub transport_options: Option<Value>,
    /// The real package when this is a branch alias of it.
    pub alias_of: Option<Box<Package>>,
}

impl Package {
    pub fn new(name: &str, version: &str, pretty_version: &str) -> Package {
        Package {
            name: name.to_lowercase(),
            pretty_name: name.to_string(),
            version: version.to_string(),
            pretty_version: pretty_version.to_string(),
            kind: "library".into(),
            target_dir: None,
            extra: Map::new(),
            binaries: vec![],
            installation_source: None,
            default_branch: false,
            source: None,
            dist: None,
            requires: vec![],
            dev_requires: vec![],
            conflicts: vec![],
            provides: vec![],
            replaces: vec![],
            suggests: Map::new(),
            autoload: Map::new(),
            dev_autoload: Map::new(),
            include_paths: vec![],
            php_ext: None,
            release_date: None,
            notification_url: None,
            description: None,
            homepage: None,
            keywords: vec![],
            license: vec![],
            authors: vec![],
            support: Map::new(),
            funding: vec![],
            abandoned: None,
            archive_name: None,
            archive_excludes: vec![],
            scripts: Map::new(),
            transport_options: None,
            alias_of: None,
        }
    }

    pub fn is_dev(&self) -> bool {
        semver::parse_stability(&self.version) == semver::Stability::Dev
    }

    pub fn stability(&self) -> semver::Stability {
        semver::parse_stability(&self.version)
    }

    pub fn links(&self, kind: LinkType) -> &[Link] {
        match kind {
            LinkType::Require => &self.requires,
            LinkType::DevRequire => &self.dev_requires,
            LinkType::Conflict => &self.conflicts,
            LinkType::Provide => &self.provides,
            LinkType::Replace => &self.replaces,
        }
    }

    fn links_mut(&mut self, kind: LinkType) -> &mut Vec<Link> {
        match kind {
            LinkType::Require => &mut self.requires,
            LinkType::DevRequire => &mut self.dev_requires,
            LinkType::Conflict => &mut self.conflicts,
            LinkType::Provide => &mut self.provides,
            LinkType::Replace => &mut self.replaces,
        }
    }

    /// `AliasPackage::__construct`: same package under another version, `self.version`
    /// links re-pointed at the alias.
    pub fn alias(&self, version: &str, pretty_version: &str) -> Package {
        let mut alias = self.clone();
        alias.version = version.to_string();
        alias.pretty_version = pretty_version.to_string();
        alias.alias_of = Some(Box::new(self.clone()));
        for kind in [
            LinkType::Require,
            LinkType::DevRequire,
            LinkType::Conflict,
            LinkType::Provide,
            LinkType::Replace,
        ] {
            for link in alias.links_mut(kind) {
                if link.pretty_constraint == "self.version" {
                    link.constraint = semver::parse_constraints(version).unwrap();
                }
            }
        }
        alias
    }

    /// `AliasPackage::getPrettyVersion` strips the `.9999999` runs for branch aliases.
    pub fn pretty_alias_version(normalized: &str) -> String {
        regex::Regex::new(r"(\.9{7})+")
            .unwrap()
            .replace_all(normalized, ".x")
            .into_owned()
    }
}

static PLATFORM_PACKAGE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(?:php(?:-64bit|-ipv6|-zts|-debug)?|hhvm|(?:ext|lib)-[a-z0-9](?:[_.-]?[a-z0-9]+)*|composer(?:-(?:plugin|runtime)-api)?)$").unwrap()
});
static PACKAGE_NAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)^[a-z0-9](?:[_.-]?[a-z0-9]+)*/[a-z0-9](?:(?:[_.]|-{1,2})?[a-z0-9]+)*$")
        .unwrap()
});
static CAMEL_SPLIT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?:([a-z])([A-Z])|([A-Z])([A-Z][a-z]))").unwrap()
});

/// `PlatformRepository::isPlatformPackage`
pub fn is_platform_package(name: &str) -> bool {
    PLATFORM_PACKAGE.is_match(name)
}

/// `ConfigValidator`'s lower-cased/with-dashes suggestion for a CamelCase name.
pub fn suggest_name(name: &str) -> String {
    CAMEL_SPLIT.replace_all(name, "$1$3-$2$4").to_lowercase()
}

/// `ValidatingArrayLoader::hasPackageNamingError`
pub fn naming_error(name: &str, is_link: bool) -> Option<String> {
    if is_platform_package(name) {
        return None;
    }
    if !PACKAGE_NAME.is_match(name) {
        return Some(format!(
            "{name} is invalid, it should have a vendor name, a forward slash, and a package name. The vendor and package name can be words separated by -, . or _. The complete name should match \"^[a-z0-9]([_.-]?[a-z0-9]+)*/[a-z0-9](([_.]?|-{{0,2}})[a-z0-9]+)*$\"."
        ));
    }
    const RESERVED: [&str; 22] = [
        "nul", "con", "prn", "aux", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let lower = name.to_lowercase();
    let (vendor, pkg) = lower.split_once('/').unwrap();
    if RESERVED.contains(&vendor) || RESERVED.contains(&pkg) {
        return Some(format!(
            "{name} is reserved, package and vendor names can not match any of: {}.",
            RESERVED.join(", ")
        ));
    }
    if name.ends_with(".json") {
        return Some(format!(
            "{name} is invalid, package names can not end in .json, consider renaming it or perhaps using a -json suffix instead."
        ));
    }
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        if is_link {
            return Some(format!(
                "{name} is invalid, it should not contain uppercase characters. Please use {lower} instead."
            ));
        }
        return Some(format!(
            "{name} is invalid, it should not contain uppercase characters. We suggest using {} instead.",
            suggest_name(name)
        ));
    }
    None
}

fn str_of<'a>(config: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    config.get(key).and_then(Value::as_str)
}

/// `ArrayLoader::parseLinks` / `createLink`
pub fn parse_links(
    source: &str,
    source_version: &str,
    kind: LinkType,
    links: &Map<String, Value>,
) -> Result<Vec<Link>, String> {
    let mut out = Vec::new();
    for (target, constraint) in links {
        let Some(pretty) = constraint.as_str() else {
            continue;
        };
        let target = target.to_lowercase();
        out.push(create_link(source, source_version, kind, &target, pretty)?);
    }
    Ok(out)
}

fn create_link(
    source: &str,
    source_version: &str,
    kind: LinkType,
    target: &str,
    pretty: &str,
) -> Result<Link, String> {
    let constraint_str = if pretty == "self.version" {
        source_version
    } else {
        pretty
    };
    let constraint = semver::parse_constraints(constraint_str).map_err(|_| {
        format!(
            "Link constraint in {source} {} > {target} should be a valid version constraint, got \"{constraint_str}\"",
            kind.description()
        )
    })?;
    Ok(Link {
        source: source.to_string(),
        target: target.to_string(),
        constraint,
        pretty_constraint: pretty.to_string(),
        kind,
    })
}

/// `ArrayLoader::load`
pub fn load(config: &Map<String, Value>) -> Result<Package, String> {
    let name = str_of(config, "name").ok_or_else(|| {
        format!(
            "Unknown package has no name defined ({}).",
            crate::json::encode_compact(&Value::Object(config.clone()))
        )
    })?;
    let version = match config.get("version") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => {
            if *b {
                "1".into()
            } else {
                String::new()
            }
        }
        _ => return Err(format!("Package {name} has no version defined.")),
    };
    let normalized = match str_of(config, "version_normalized") {
        Some(v) if v != semver::DEFAULT_BRANCH_ALIAS => v.to_string(),
        _ => semver::normalize(&version, None)
            .map_err(|e| format!("Failed to normalize version for package \"{name}\": {e}"))?,
    };
    let mut package = Package::new(name, &normalized, &version);

    for kind in LinkType::ALL {
        if let Some(Value::Object(links)) = config.get(kind.key()) {
            *package.links_mut(kind) =
                parse_links(&package.name, &package.pretty_version, kind, links)?;
        }
    }

    configure(&mut package, config)?;

    if let Some(alias_normalized) = branch_alias(config)? {
        let pretty = Package::pretty_alias_version(&alias_normalized);
        return Ok(package.alias(&alias_normalized, &pretty));
    }
    Ok(package)
}

fn string_list(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => crate::json::encode_compact(other),
            })
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => vec![],
    }
}

fn configure(package: &mut Package, config: &Map<String, Value>) -> Result<(), String> {
    package.kind = str_of(config, "type").map_or("library".into(), str::to_lowercase);
    package.target_dir = str_of(config, "target-dir").map(str::to_string);
    if let Some(Value::Object(extra)) = config.get("extra") {
        package.extra = extra.clone();
    }
    if let Some(bin) = config.get("bin") {
        package.binaries = string_list(bin)
            .into_iter()
            .map(|b| b.trim_start_matches('/').to_string())
            .collect();
    }
    package.installation_source = str_of(config, "installation-source").map(str::to_string);
    package.default_branch = config.get("default-branch") == Some(&Value::Bool(true));
    if let Some(source) = config.get("source") {
        let (Some(kind), Some(url), Some(reference)) = (
            source.get("type").and_then(Value::as_str),
            source.get("url").and_then(Value::as_str),
            source.get("reference"),
        ) else {
            return Err(format!(
                "Package {}'s source key should be specified as {{\"type\": ..., \"url\": ..., \"reference\": ...}},\n{} given.",
                package.pretty_name,
                crate::json::encode_compact(source)
            ));
        };
        package.source = Some(Source {
            kind: kind.to_string(),
            url: url.to_string(),
            reference: scalar_string(reference),
            mirrors: source.get("mirrors").cloned(),
        });
    }
    if let Some(dist) = config.get("dist") {
        let (Some(kind), Some(url)) = (
            dist.get("type").and_then(Value::as_str),
            dist.get("url").and_then(Value::as_str),
        ) else {
            return Err(format!(
                "Package {}'s dist key should be specified as {{\"type\": ..., \"url\": ..., \"reference\": ..., \"shasum\": ...}},\n{} given.",
                package.pretty_name,
                crate::json::encode_compact(dist)
            ));
        };
        package.dist = Some(Dist {
            kind: kind.to_string(),
            url: url.to_string(),
            reference: dist.get("reference").and_then(scalar_string),
            shasum: dist
                .get("shasum")
                .and_then(Value::as_str)
                .map(str::to_string),
            mirrors: dist.get("mirrors").cloned(),
        });
    }
    if let Some(Value::Object(suggest)) = config.get("suggest") {
        package.suggests = suggest
            .iter()
            .map(|(k, v)| {
                let v = match v.as_str() {
                    Some(s) if s.trim() == "self.version" => {
                        Value::String(package.pretty_version.clone())
                    }
                    _ => v.clone(),
                };
                (k.clone(), v)
            })
            .collect();
    }
    if let Some(Value::Object(autoload)) = config.get("autoload") {
        package.autoload = autoload.clone();
    }
    if let Some(Value::Object(autoload)) = config.get("autoload-dev") {
        package.dev_autoload = autoload.clone();
    }
    if let Some(paths) = config.get("include-path") {
        package.include_paths = string_list(paths);
    }
    package.php_ext = config.get("php-ext").cloned();
    package.release_date = match config.get("time") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    package.notification_url = str_of(config, "notification-url")
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(archive) = config.get("archive") {
        package.archive_name = archive
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        package.archive_excludes = archive.get("exclude").map(string_list).unwrap_or_default();
    }
    if let Some(Value::Object(scripts)) = config.get("scripts") {
        package.scripts = scripts
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    if v.is_array() {
                        v.clone()
                    } else {
                        Value::Array(vec![v.clone()])
                    },
                )
            })
            .collect();
    }
    package.description = str_of(config, "description")
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    package.homepage = str_of(config, "homepage")
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(Value::Array(keywords)) = config.get("keywords") {
        package.keywords = keywords
            .iter()
            .map(|k| match k {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
    }
    if let Some(license) = config.get("license") {
        package.license = string_list(license);
    }
    if let Some(Value::Array(authors)) = config.get("authors") {
        package.authors = authors.clone();
    }
    if let Some(Value::Object(support)) = config.get("support") {
        package.support = support.clone();
    }
    if let Some(Value::Array(funding)) = config.get("funding") {
        package.funding = funding.clone();
    }
    package.abandoned = config.get("abandoned").cloned();
    package.transport_options = config.get("transport-options").cloned();
    Ok(())
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `ArrayLoader::getBranchAlias`
pub fn branch_alias(config: &Map<String, Value>) -> Result<Option<String>, String> {
    let version = match config.get("version") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return Err("no/invalid version defined".into()),
    };
    if !version.starts_with("dev-") && !version.ends_with("-dev") {
        return Ok(None);
    }
    let Some(Value::Object(aliases)) = config.get("extra").and_then(|e| e.get("branch-alias"))
    else {
        return Ok(None);
    };
    for (source_branch, target_branch) in aliases {
        let Some(target_branch) = target_branch.as_str() else {
            continue;
        };
        if !target_branch.ends_with("-dev") {
            continue;
        }
        let validated = if target_branch == semver::DEFAULT_BRANCH_ALIAS {
            semver::DEFAULT_BRANCH_ALIAS.to_string()
        } else {
            semver::normalize_branch(&target_branch[..target_branch.len() - 4])
        };
        if !validated.ends_with("-dev") {
            continue;
        }
        // ensure that it is the current branch aliasing itself
        if version.to_lowercase() == source_branch.to_lowercase() {
            return Ok(Some(validated));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_a_lock_package_with_links_and_branch_alias() {
        let config: Value = serde_json::from_str(
            r#"{"name":"Acme/Lib","version":"dev-main","type":"Library","require":{"php":">=8.1","foo/bar":"self.version"},
                "replace":{"acme/old":"self.version"},"extra":{"branch-alias":{"dev-main":"2.x-dev"}},
                "dist":{"type":"zip","url":"https://example.com/a.zip","reference":"abc","shasum":""},"bin":["/bin/acme"]}"#,
        )
        .unwrap();
        let p = load(config.as_object().unwrap()).unwrap();
        assert_eq!(p.name, "acme/lib");
        assert_eq!(p.pretty_name, "Acme/Lib");
        assert_eq!(p.version, "2.9999999.9999999.9999999-dev");
        assert_eq!(p.pretty_version, "2.x-dev");
        let real = p.alias_of.as_ref().unwrap();
        assert_eq!(real.version, "dev-main");
        assert_eq!(p.kind, "library");
        assert_eq!(p.binaries, vec!["bin/acme"]);
        assert_eq!(
            p.requires[1].constraint.to_string(),
            "== 2.9999999.9999999.9999999-dev"
        );
        assert_eq!(real.requires[1].constraint.to_string(), "== dev-main");
        assert_eq!(p.replaces[0].pretty_constraint, "self.version");
    }

    #[test]
    fn reports_composer_messages_for_bad_input() {
        let bad: Value =
            serde_json::from_str(r#"{"name":"a/b","version":"1.0","require":{"c/d":"^^1"}}"#)
                .unwrap();
        assert_eq!(
            load(bad.as_object().unwrap()).unwrap_err(),
            "Link constraint in a/b requires > c/d should be a valid version constraint, got \"^^1\""
        );
        let no_version: Value = serde_json::from_str(r#"{"name":"a/b"}"#).unwrap();
        assert_eq!(
            load(no_version.as_object().unwrap()).unwrap_err(),
            "Package a/b has no version defined."
        );
    }
}
