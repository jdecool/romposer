//! Port of `ComposerRepository` (v2 `metadata-url` protocol plus static
//! `packages`/`includes` repositories) with Composer's metadata cache layout, and
//! the pieces of `ArrayRepository`/`FilterRepository` the commands need.
//!
//! ponytail: v1 `providers-url`/`provider-includes` lazy repositories are not
//! loaded (Composer 2 repositories all publish `metadata-url`); add when a
//! private v1-only repository shows up.

use crate::config::Config;
use crate::download::{Http, sanitize_url};
use crate::io::Io;
use crate::json::{Map, Value, encode};
use crate::package::{self, Package, is_platform_package};
use crate::semver::{Constraint, Stability, version_parser};
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::LazyLock;

pub const SEARCH_FULLTEXT: u8 = 0;
pub const SEARCH_NAME: u8 = 1;
pub const SEARCH_VENDOR: u8 = 2;

/// `BasePackage::packageNameToRegexp`
pub fn package_name_to_regexp(name: &str) -> Regex {
    Regex::new(&format!(
        "(?i)^{}$",
        regex::escape(name).replace("\\*", ".*?")
    ))
    .unwrap()
}

/// `BasePackage::packageNamesToRegexp`
pub fn package_names_to_regexp(names: &[String]) -> Regex {
    let parts: Vec<String> = names
        .iter()
        .map(|n| regex::escape(n).replace("\\*", ".*?"))
        .collect();
    Regex::new(&format!("(?i)^(?:{})$", parts.join("|"))).unwrap()
}

/// PHP `urlencode`
pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `MetadataMinifier::expand`
pub fn expand_minified(versions: &[Value]) -> Vec<Map<String, Value>> {
    let mut expanded = Vec::new();
    let mut current: Option<Map<String, Value>> = None;
    for data in versions {
        let Value::Object(data) = data else { continue };
        match &mut current {
            None => current = Some(data.clone()),
            Some(cur) => {
                for (k, v) in data {
                    if v == "__unset" {
                        cur.remove(k);
                    } else {
                        cur.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        expanded.push(current.clone().unwrap());
    }
    expanded
}

/// `ArrayRepository::search` over loaded packages (also used for installed and platform repos).
pub fn search_packages(
    packages: &[&Package],
    query: &str,
    mode: u8,
    kind: Option<&str>,
) -> Vec<Map<String, Value>> {
    let regex = search_regex(query, mode == SEARCH_FULLTEXT);
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for package in packages {
        let name = if mode == SEARCH_VENDOR {
            package.name.split('/').next().unwrap_or("").to_string()
        } else {
            package.name.clone()
        };
        if seen.contains(&name) || kind.is_some_and(|k| k != package.kind) {
            continue;
        }
        let text = format!(
            "{} {}",
            package.keywords.join(" "),
            package.description.as_deref().unwrap_or("")
        );
        if regex.is_match(&name) || (mode == SEARCH_FULLTEXT && regex.is_match(&text)) {
            seen.insert(name.clone());
            let mut m = Map::new();
            if mode == SEARCH_VENDOR {
                m.insert("name".into(), Value::String(name));
                m.insert("description".into(), Value::Null);
            } else {
                m.insert("name".into(), Value::String(package.pretty_name.clone()));
                m.insert(
                    "description".into(),
                    package
                        .description
                        .clone()
                        .map_or(Value::Null, Value::String),
                );
                if package.is_abandoned() {
                    m.insert(
                        "abandoned".into(),
                        package
                            .replacement_package()
                            .map_or(Value::Bool(true), Value::String),
                    );
                }
            }
            matches.push(m);
        }
    }
    matches
}

/// `{(?:a|b)}i` over the whitespace-split tokens; fulltext queries get `preg_quote`d first.
fn search_regex(query: &str, quote: bool) -> Regex {
    let query = if quote {
        regex::escape(query)
    } else {
        query.to_string()
    };
    let tokens: Vec<&str> = query.split_whitespace().collect();
    let joined = if tokens.is_empty() {
        String::new()
    } else {
        tokens.join("|")
    };
    Regex::new(&format!("(?i)(?:{joined})")).unwrap()
}

struct RootData {
    metadata_url: Option<String>,
    search_url: Option<String>,
    list_url: Option<String>,
    notify_url: Option<String>,
    available: Option<HashSet<String>>,
    patterns: Vec<Regex>,
    dist_mirrors: Vec<Value>,
    /// inline `packages` (v1 static repositories, v2 partial packages) and `includes`
    inline: Vec<Map<String, Value>>,
    /// `security-advisories`: (`metadata`, `api-url`)
    advisories: Option<(bool, Option<String>)>,
}

/// `PartialSecurityAdvisory` / `SecurityAdvisory`: a full advisory carries `title`, `sources`
/// and `reportedAt` in `data`; the partial ones come from the p2 metadata.
#[derive(Clone, Debug)]
pub struct Advisory {
    pub id: String,
    pub package: String,
    /// `affectedVersions` as given (the constraint's pretty string)
    pub affected: String,
    pub constraint: Constraint,
    pub data: Map<String, Value>,
    pub full: bool,
    /// set by the auditor for an ignored full advisory
    pub ignore_reason: Option<String>,
}

impl Advisory {
    /// `PartialSecurityAdvisory::create`
    fn create(package: &str, data: &Map<String, Value>) -> Option<Advisory> {
        let id = data.get("advisoryId")?.as_str()?.to_string();
        let affected = data.get("affectedVersions")?.as_str()?.to_string();
        let (constraint, pretty) = match version_parser::parse_constraints(&affected) {
            Ok(c) => (c, affected.clone()),
            Err(_) => {
                static PREFIX: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r"(^[>=<^~]*[\d.]+).*").unwrap());
                let stripped = PREFIX.replace(&affected, "$1").into_owned();
                match version_parser::parse_constraints(&stripped) {
                    Ok(c) => (c, stripped),
                    Err(_) => (
                        Constraint::new("==", "0.0.0-invalid-version"),
                        "== 0.0.0-invalid-version".to_string(),
                    ),
                }
            }
        };
        let full = ["title", "sources", "reportedAt"]
            .iter()
            .all(|k| data.get(*k).is_some_and(|v| !v.is_null()));
        Some(Advisory {
            id,
            package: package.to_string(),
            affected: pretty,
            constraint,
            data: data.clone(),
            full,
            ignore_reason: None,
        })
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }

    /// `jsonSerialize`: property order of the PHP classes.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("advisoryId".into(), Value::String(self.id.clone()));
        m.insert("packageName".into(), Value::String(self.package.clone()));
        m.insert(
            "affectedVersions".into(),
            Value::String(self.affected.clone()),
        );
        if self.full {
            for key in ["title", "cve", "link"] {
                m.insert(
                    key.into(),
                    self.data.get(key).cloned().unwrap_or(Value::Null),
                );
            }
            m.insert("reportedAt".into(), Value::String(self.reported_at()));
            m.insert(
                "sources".into(),
                self.data.get("sources").cloned().unwrap_or(Value::Null),
            );
            m.insert(
                "severity".into(),
                self.data.get("severity").cloned().unwrap_or(Value::Null),
            );
            if let Some(reason) = &self.ignore_reason {
                m.insert("ignoreReason".into(), Value::String(reason.clone()));
            }
        }
        Value::Object(m)
    }

    /// `new DateTimeImmutable($reportedAt, UTC)->format(DATE_RFC3339)`
    pub fn reported_at(&self) -> String {
        static DATE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"^(\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2}:\d{2})(?:\.\d+)?(Z|[+-]\d{2}:\d{2})?$",
            )
            .unwrap()
        });
        let raw = self.str("reportedAt").unwrap_or("");
        // ponytail: only the `Y-m-d H:i:s` shape Packagist emits, taken as UTC
        match DATE.captures(raw) {
            Some(c) => format!(
                "{}T{}{}",
                &c[1],
                &c[2],
                c.get(3)
                    .map_or("+00:00".to_string(), |z| z.as_str().replace('Z', "+00:00"))
            ),
            None => raw.to_string(),
        }
    }
}

pub struct ComposerRepository {
    pub name: String,
    pub url: String,
    base_url: String,
    pub canonical: bool,
    only: Option<Regex>,
    exclude: Option<Regex>,
    cache_root: Option<PathBuf>,
    cache_read_only: bool,
    http: Rc<Http>,
    root: Option<RootData>,
    degraded: bool,
    not_found: HashSet<String>,
    fresh: HashSet<String>,
    /// a `vcs`/`git`/`github`/... repository, scanned on first use
    pub vcs: Option<Vcs>,
}

/// A `VcsRepository`: its configuration, and the driver once the scan picked it.
pub struct Vcs {
    repo: Map<String, Value>,
    config: Config,
    pub driver: Option<&'static str>,
}

static SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w.]+\??://").unwrap());
static ORIGIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^:]+://[^/]*").unwrap());

/// `RepositoryFactory`: every `composer` repository in the config, in priority order.
pub fn from_config(config: &Config, http: Rc<Http>) -> Result<Vec<ComposerRepository>, String> {
    let mut repos = Vec::new();
    for (name, value) in &config.repositories {
        let Value::Object(repo) = value else { continue };
        let kind = repo.get("type").and_then(Value::as_str).unwrap_or("");
        if crate::vcs::UNSUPPORTED.contains(&kind) || kind == "forgejo" {
            return Err(crate::vcs::unsupported(kind));
        }
        let vcs = matches!(
            kind,
            "vcs" | "git" | "github" | "gitlab" | "bitbucket" | "git-bitbucket"
        );
        // ponytail: path/artifact repositories arrive with ticket 17
        if kind != "composer" && !vcs {
            continue;
        }
        let url = repo
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Repository \"{name}\" ({kind}) must have a url"))?;
        if vcs {
            let mut r = ComposerRepository::new(name, "file:///", repo, config, http.clone())?;
            r.url = crate::config::expand_path(url);
            r.vcs = Some(Vcs {
                repo: repo.clone(),
                config: config.clone(),
                driver: None,
            });
            repos.push(r);
            continue;
        }
        repos.push(ComposerRepository::new(
            name,
            url,
            repo,
            config,
            http.clone(),
        )?);
    }
    Ok(repos)
}

impl ComposerRepository {
    pub fn new(
        name: &str,
        url: &str,
        repo: &Map<String, Value>,
        config: &Config,
        http: Rc<Http>,
    ) -> Result<ComposerRepository, String> {
        let mut url = url.to_string();
        if !SCHEME.is_match(&url) {
            url = if std::path::Path::new(&url).exists() {
                format!("file://{}", crate::fs::realpath(std::path::Path::new(&url)))
            } else {
                format!("http://{url}")
            };
        }
        let url = url.trim_end_matches('/').to_string();
        if url.is_empty() {
            return Err("The repository url must not be an empty string".into());
        }
        let url = match url.strip_prefix("https?") {
            Some(rest) => format!("https{rest}"),
            None => url,
        };
        let packagist = Regex::new(r"(?i)^(https?)://packagist\.org/?$").unwrap();
        let url = match packagist.captures(&url) {
            Some(m) => format!("{}://repo.packagist.org", &m[1]),
            None => url,
        };
        let base_url = Regex::new(r"(?:/[^/\\]+\.json)?(?:[?#].*)?$")
            .unwrap()
            .replace(&url, "")
            .trim_end_matches('/')
            .to_string();
        let cache_dir = config.get_str("cache-repo-dir");
        let usable = !Regex::new(r"(^|[\\/])(\$null|nul|NUL|/dev/null)([\\/]|$)")
            .unwrap()
            .is_match(&cache_dir);
        let cache_root = usable.then(|| {
            PathBuf::from(format!(
                "{cache_dir}/{}",
                Regex::new(r"(?i)[^a-z0-9.]")
                    .unwrap()
                    .replace_all(&sanitize_url(&url), "-")
            ))
        });
        let patterns = |key: &str| -> Option<Regex> {
            match repo.get(key) {
                Some(Value::Array(list)) => Some(package_names_to_regexp(
                    &list
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                )),
                _ => None,
            }
        };
        Ok(ComposerRepository {
            name: name.to_string(),
            url,
            base_url,
            canonical: repo.get("canonical") != Some(&Value::Bool(false)),
            only: patterns("only"),
            exclude: patterns("exclude"),
            cache_root,
            cache_read_only: config.bool("cache-read-only"),
            http,
            root: None,
            degraded: false,
            not_found: HashSet::new(),
            fresh: HashSet::new(),
            vcs: None,
        })
    }

    /// `FilterRepository::isAllowed`
    fn is_allowed(&self, name: &str) -> bool {
        if let Some(only) = &self.only {
            return only.is_match(name);
        }
        if let Some(exclude) = &self.exclude {
            return !exclude.is_match(name);
        }
        true
    }

    // --- cache (`Composer\Cache` with allowlist `a-z0-9.$~_`) ---

    fn cache_path(&self, key: &str) -> Option<PathBuf> {
        let safe = Regex::new(r"(?i)[^a-z0-9.$~_]")
            .unwrap()
            .replace_all(key, "-")
            .into_owned();
        // an `includes` key from packages.json must stay inside the cache directory
        if safe.is_empty() || safe == "." || safe == ".." {
            return None;
        }
        self.cache_root.as_ref().map(|r| r.join(safe))
    }

    fn cache_read(&self, key: &str) -> Option<String> {
        std::fs::read_to_string(self.cache_path(key)?).ok()
    }

    fn cache_age(&self, key: &str) -> Option<u64> {
        let modified = std::fs::metadata(self.cache_path(key)?)
            .ok()?
            .modified()
            .ok()?;
        Some(
            std::time::SystemTime::now()
                .duration_since(modified)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        )
    }

    fn cache_write(&self, key: &str, contents: &str) {
        if self.cache_read_only {
            return;
        }
        let Some(path) = self.cache_path(key) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Some(name) = path.file_name() else { return };
        let tmp = path.with_file_name(format!(
            "{}{}.tmp",
            name.to_string_lossy(),
            crate::fs::random_hex(5)
        ));
        if std::fs::write(&tmp, contents).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    // --- fetching ---

    fn canonicalize_url(&self, url: &str) -> String {
        if let Some(rest) = url.strip_prefix('/') {
            if let Some(m) = ORIGIN.find(&self.url) {
                return format!("{}/{rest}", m.as_str());
            }
            return self.url.clone();
        }
        url.to_string()
    }

    fn packages_json_url(&self) -> String {
        let path = reqwest::Url::parse(&self.url.replace('\\', "/"))
            .map(|u| u.path().to_string())
            .unwrap_or_default();
        if path.contains(".json") {
            self.url.clone()
        } else {
            format!("{}/packages.json", self.url)
        }
    }

    /// `$` url-encoded as bad proxies choke on it.
    fn encode_dollar(url: &str) -> String {
        match url.find('$') {
            Some(pos) if pos > 0 && Regex::new(r"(?i)^https?://").unwrap().is_match(url) => {
                format!("{}%24{}", &url[..pos], &url[pos + 1..])
            }
            _ => url.to_string(),
        }
    }

    fn degraded_warning(&mut self, io: &mut Io, error: &str) {
        if !self.degraded {
            io.warning(&format!(
                "{} could not be fully loaded ({error}), package information was loaded from the local cache and may be out of date",
                sanitize_url(&self.url)
            ));
        }
        self.degraded = true;
    }

    fn decode(url: &str, body: &[u8]) -> Result<Map<String, Value>, String> {
        let text = String::from_utf8_lossy(body);
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(o)) => Ok(o),
            Ok(_) => Err(format!(
                "\"{}\" does not contain valid JSON\nExpected an object",
                sanitize_url(url)
            )),
            Err(e) => Err(format!(
                "\"{}\" does not contain valid JSON\n{e}",
                sanitize_url(url)
            )),
        }
    }

    /// `fetchFile`: GET, cache the body (with `last-modified` folded in when asked),
    /// fall back to the cache in degraded mode on transport errors.
    fn fetch_file(
        &mut self,
        io: &mut Io,
        url: &str,
        cache_key: &str,
        store_last_modified: bool,
    ) -> Result<Map<String, Value>, String> {
        let url = Self::encode_dollar(url);
        match self.http.get(&url, &[]) {
            Ok(response) => {
                let mut data = Self::decode(&url, &response.body)?;
                let mut json = String::from_utf8_lossy(&response.body).into_owned();
                if store_last_modified && let Some(lm) = &response.last_modified {
                    data.insert("last-modified".into(), Value::String(lm.clone()));
                    json = encode(&Value::Object(data.clone()), false, false, false);
                }
                self.cache_write(cache_key, &json);
                Ok(data)
            }
            Err((status, message)) => {
                if status == Some(404) {
                    return Err(message);
                }
                if let Some(cached) = self.cache_read(cache_key) {
                    self.degraded_warning(io, &message);
                    return Self::decode(&url, cached.as_bytes());
                }
                Err(message)
            }
        }
    }

    /// `fetchFileIfLastModified`: `None` when the cache is still fresh (304 or unreachable).
    fn fetch_if_modified(
        &mut self,
        io: &mut Io,
        url: &str,
        cache_key: &str,
        last_modified: &str,
    ) -> Result<Option<Map<String, Value>>, String> {
        let headers = vec![("If-Modified-Since".to_string(), last_modified.to_string())];
        match self.http.get(url, &headers) {
            Ok(response) => {
                if response.status == 304 && response.body.is_empty() {
                    return Ok(None);
                }
                let mut data = Self::decode(url, &response.body)?;
                let mut json = String::from_utf8_lossy(&response.body).into_owned();
                if let Some(lm) = &response.last_modified {
                    data.insert("last-modified".into(), Value::String(lm.clone()));
                    json = encode(&Value::Object(data.clone()), false, false, false);
                }
                self.cache_write(cache_key, &json);
                Ok(Some(data))
            }
            Err((Some(404), message)) => Err(message),
            Err((_, message)) => {
                self.degraded_warning(io, &message);
                Ok(None)
            }
        }
    }

    /// `loadRootServerFile`
    fn load_root(&mut self, io: &mut Io, max_age: Option<u64>) -> Result<&RootData, String> {
        if self.root.is_none() {
            let root = self.build_root(io, max_age)?;
            self.root = Some(root);
        }
        Ok(self.root.as_ref().unwrap())
    }

    fn build_root(&mut self, io: &mut Io, max_age: Option<u64>) -> Result<RootData, String> {
        if let Some(vcs) = &mut self.vcs {
            let scan = crate::vcs::scan(io, &vcs.config, &self.http, &vcs.repo)?;
            vcs.driver = Some(scan.driver);
            return Ok(RootData {
                metadata_url: None,
                search_url: None,
                list_url: None,
                notify_url: None,
                available: None,
                patterns: vec![],
                dist_mirrors: vec![],
                inline: scan.packages,
                advisories: None,
            });
        }
        let mut data: Option<Map<String, Value>> = None;
        if let Some(cached) = self.cache_read("packages.json")
            && let Ok(Value::Object(cached)) = serde_json::from_str::<Value>(&cached)
        {
            let fresh = max_age
                .zip(self.cache_age("packages.json"))
                .is_some_and(|(max, age)| age <= max);
            if fresh {
                data = Some(cached);
            } else if let Some(Value::String(lm)) = cached.get("last-modified").cloned() {
                let url = self.packages_json_url();
                data = Some(
                    self.fetch_if_modified(io, &url, "packages.json", &lm)?
                        .unwrap_or(cached),
                );
            }
        }
        let data = match data {
            Some(d) => d,
            None => {
                let url = self.packages_json_url();
                self.fetch_file(io, &url, "packages.json", true)?
            }
        };
        let non_empty = |v: Option<&Value>| -> Option<String> {
            match v {
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            }
        };
        let notify_url = non_empty(data.get("notify-batch"))
            .or_else(|| non_empty(data.get("notify")))
            .map(|u| self.canonicalize_url(&u));
        let search_url = non_empty(data.get("search")).map(|u| self.canonicalize_url(&u));
        let list_url = non_empty(data.get("list")).map(|u| self.canonicalize_url(&u));
        let mut dist_mirrors = Vec::new();
        if let Some(Value::Array(mirrors)) = data.get("mirrors") {
            for mirror in mirrors {
                if let Some(url) = non_empty(mirror.get("dist-url")) {
                    let mut m = Map::new();
                    m.insert("url".into(), Value::String(self.canonicalize_url(&url)));
                    m.insert(
                        "preferred".into(),
                        Value::Bool(mirror.get("preferred").is_some_and(truthy)),
                    );
                    dist_mirrors.push(Value::Object(m));
                }
            }
        }
        let metadata_url = non_empty(data.get("metadata-url")).map(|u| self.canonicalize_url(&u));
        let mut available = None;
        let mut patterns = Vec::new();
        if metadata_url.is_some() {
            if let Some(Value::Array(list)) = data.get("available-packages")
                && !list.is_empty()
            {
                available = Some(
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_lowercase)
                        .collect(),
                );
            }
            if let Some(Value::Array(list)) = data.get("available-package-patterns") {
                patterns = list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(package_name_to_regexp)
                    .collect();
            }
        }
        let inline = self.load_includes(io, &data)?;
        let mut advisories = None;
        if let Some(Value::Object(config)) = data.get("security-advisories") {
            let api_url = config
                .get("api-url")
                .and_then(Value::as_str)
                .map(|u| self.canonicalize_url(u));
            if api_url.is_none() && available.is_none() && patterns.is_empty() {
                return Err(format!(
                    "Invalid security advisory configuration on composer repo ({}): If the repository does not provide a security-advisories.api-url then available-packages or available-package-patterns are required to be provided for performance reason.",
                    sanitize_url(&self.url)
                ));
            }
            advisories = Some((config.get("metadata").is_some_and(truthy), api_url));
        }
        Ok(RootData {
            metadata_url,
            search_url,
            list_url,
            notify_url,
            available,
            patterns,
            dist_mirrors,
            inline,
            advisories,
        })
    }

    /// `loadIncludes`: inline `packages` plus every `includes` file (sha1-checked cache).
    fn load_includes(
        &mut self,
        io: &mut Io,
        data: &Map<String, Value>,
    ) -> Result<Vec<Map<String, Value>>, String> {
        let mut packages = Vec::new();
        if let Some(Value::Object(by_name)) = data.get("packages") {
            for versions in by_name.values() {
                let list: Vec<&Value> = match versions {
                    Value::Object(o) => o.values().collect(),
                    Value::Array(a) => a.iter().collect(),
                    _ => vec![],
                };
                for meta in list {
                    if let Value::Object(m) = meta {
                        packages.push(m.clone());
                    }
                }
            }
        }
        if let Some(Value::Object(includes)) = data.get("includes") {
            for (include, meta) in includes {
                let sha1 = meta.get("sha1").and_then(Value::as_str);
                let cached = self
                    .cache_read(include)
                    .filter(|c| sha1.is_some_and(|s| crate::download::sha1_hex(c.as_bytes()) == s));
                let included = match cached {
                    Some(c) => Self::decode(include, c.as_bytes())?,
                    None => {
                        let url = format!("{}/{include}", self.base_url);
                        self.fetch_file(io, &url, include, false)?
                    }
                };
                packages.extend(self.load_includes(io, &included)?);
            }
        }
        Ok(packages)
    }

    fn has_available_list(&self) -> bool {
        self.root
            .as_ref()
            .is_some_and(|r| r.available.is_some() || !r.patterns.is_empty())
    }

    /// `lazyProvidersRepoContains`
    fn contains(&self, name: &str) -> bool {
        let root = self.root.as_ref().unwrap();
        if let Some(available) = &root.available
            && available.contains(name)
        {
            return true;
        }
        root.patterns.iter().any(|p| p.is_match(name))
    }

    /// `createPackages` for one metadata entry.
    fn create_package(&self, mut data: Map<String, Value>) -> Result<Package, String> {
        let root = self.root.as_ref().unwrap();
        if !data.contains_key("notification-url")
            && let Some(url) = &root.notify_url
        {
            data.insert("notification-url".into(), Value::String(url.clone()));
        }
        let mut package = package::load(&data)
            .map_err(|e| format!("Could not load packages in {}: {e}", self.name))?;
        // `configureCachedLinks`: recursive links are not supported
        drop_self_links(&mut package);
        if !root.dist_mirrors.is_empty() {
            let mirrors = Some(Value::Array(root.dist_mirrors.clone()));
            if let Some(dist) = &mut package.dist {
                dist.mirrors = mirrors.clone();
            }
            if let Some(real) = &mut package.alias_of
                && let Some(dist) = &mut real.dist
            {
                dist.mirrors = mirrors;
            }
        }
        Ok(package)
    }

    /// `isVersionAcceptable`
    fn version_acceptable(
        constraint: Option<&Constraint>,
        name: &str,
        data: &Map<String, Value>,
        stability: Option<(&[Stability], &BTreeMap<String, u8>)>,
    ) -> bool {
        let mut versions = vec![
            data.get("version_normalized")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ];
        if let Ok(Some(alias)) = package::branch_alias(data) {
            versions.push(alias);
        }
        versions.iter().any(|v| {
            if let Some((acceptable, flags)) = stability
                && !crate::solver::is_package_acceptable(
                    acceptable,
                    flags,
                    &[name.to_string()],
                    crate::semver::parse_stability(v),
                )
            {
                return false;
            }
            constraint.is_none_or(|c| c.matches(&Constraint::new("==", v)))
        })
    }

    /// `findPackages`: every version of each name (aliases included) matching its constraint.
    pub fn load_packages(
        &mut self,
        io: &mut Io,
        names: &[(String, Option<Constraint>)],
    ) -> Result<Vec<Package>, String> {
        let all = [
            Stability::Stable,
            Stability::RC,
            Stability::Beta,
            Stability::Alpha,
            Stability::Dev,
        ];
        self.load_packages_filtered(io, names, &all, &BTreeMap::new(), &HashMap::new())
            .map(|(_, p)| p)
    }

    /// `loadPackages`: (names found, packages) for the v2 protocol or a static repository.
    /// `startCachedAsyncDownload` for every `%package%` file at once: the cache answers
    /// fresh and 404 urls, conditional requests revalidate the rest; `None` when missing.
    fn fetch_provider_files(
        &mut self,
        io: &mut Io,
        files: &[String],
    ) -> Result<Vec<Option<Map<String, Value>>>, String> {
        let metadata_url = self.root.as_ref().unwrap().metadata_url.clone().unwrap();
        let mut requests = Vec::new();
        let mut cached: Vec<Option<Map<String, Value>>> = Vec::new();
        for file in files {
            let url = metadata_url.replace("%package%", file);
            let key = format!("provider-{}.json", file.replace('/', "~"));
            let contents = self
                .cache_read(&key)
                .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                .and_then(|v| v.as_object().cloned());
            let last_modified = contents
                .as_ref()
                .and_then(|c| c.get("last-modified"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut headers = Vec::new();
            if let Some(lm) = &last_modified {
                headers.push(("If-Modified-Since".to_string(), lm.clone()));
            }
            let skip = self.not_found.contains(&url)
                || (self.fresh.contains(&url) && last_modified.is_some());
            cached.push(contents);
            requests.push((url, key, headers, skip, last_modified.is_some()));
        }
        let responses = self.http.get_all(
            requests
                .iter()
                .filter(|r| !r.3)
                .map(|r| (r.0.clone(), r.2.clone()))
                .collect(),
        );
        let mut responses = responses.into_iter();
        let mut out = Vec::new();
        for (i, (url, key, _, skip, had_cache)) in requests.into_iter().enumerate() {
            let data = if skip {
                if self.not_found.contains(&url) {
                    None
                } else {
                    cached[i].take()
                }
            } else {
                match responses.next().unwrap() {
                    Ok(response) if response.status == 304 && response.body.is_empty() => {
                        self.fresh.insert(url.clone());
                        cached[i].take()
                    }
                    Ok(response) => {
                        let mut data = Self::decode(&url, &response.body)?;
                        let mut json = String::from_utf8_lossy(&response.body).into_owned();
                        if let Some(lm) = &response.last_modified {
                            data.insert("last-modified".into(), Value::String(lm.clone()));
                            json = encode(&Value::Object(data.clone()), false, true, true);
                        }
                        self.cache_write(&key, &json);
                        self.fresh.insert(url.clone());
                        Some(data)
                    }
                    Err((Some(404), _)) => {
                        self.not_found.insert(url.clone());
                        None
                    }
                    Err((_, message)) => {
                        self.degraded_warning(io, &message);
                        if had_cache {
                            cached[i].take()
                        } else {
                            return Err(message);
                        }
                    }
                }
            };
            out.push(data);
        }
        Ok(out)
    }

    /// `hasSecurityAdvisories`
    pub fn has_security_advisories(&mut self, io: &mut Io) -> Result<bool, String> {
        if self.vcs.is_some() {
            return Ok(false);
        }
        let root = self.load_root(io, Some(600))?;
        Ok(root
            .advisories
            .as_ref()
            .is_some_and(|(metadata, api)| *metadata || api.is_some()))
    }

    /// `getSecurityAdvisories`: the advisories matching each package's constraint, read
    /// from the p2 metadata when partial ones are allowed (or no API exists), the rest
    /// asked from the `api-url` in batches.
    pub fn get_security_advisories(
        &mut self,
        io: &mut Io,
        constraints: &BTreeMap<String, Constraint>,
        allow_partial: bool,
    ) -> Result<BTreeMap<String, Vec<Advisory>>, String> {
        if self.vcs.is_some() {
            return Ok(BTreeMap::new());
        }
        self.load_root(io, Some(600))?;
        let Some((metadata, api_url)) = self.root.as_ref().unwrap().advisories.clone() else {
            return Ok(BTreeMap::new());
        };
        let mut pending: BTreeMap<String, Constraint> = constraints
            .iter()
            .filter(|(name, _)| !self.has_available_list() || self.contains(name))
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect();
        let mut advisories = BTreeMap::new();
        let mut collect =
            |name: &str, list: &[Value], pending: &mut BTreeMap<String, Constraint>| {
                let Some(constraint) = pending.remove(name) else {
                    return;
                };
                let matching: Vec<Advisory> = list
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(|data| Advisory::create(name, data))
                    .filter(|a| a.constraint.matches(&constraint))
                    .collect();
                if !matching.is_empty() {
                    advisories.insert(name.to_string(), matching);
                }
            };
        if metadata && (allow_partial || api_url.is_none()) {
            let names: Vec<String> = pending
                .keys()
                .filter(|n| !is_platform_package(n) && n.as_str() != "__root__")
                .cloned()
                .collect();
            let files = self.fetch_provider_files(io, &names)?;
            for (name, file) in names.iter().zip(files) {
                if let Some(Value::Array(list)) =
                    file.and_then(|f| f.get("security-advisories").cloned())
                {
                    collect(name, &list, &mut pending);
                }
            }
        }
        if let Some(api_url) = api_url
            && !pending.is_empty()
        {
            let names: Vec<String> = pending.keys().cloned().collect();
            for batch in names.chunks(500) {
                let body = batch
                    .iter()
                    .map(|n| format!("packages%5B%5D={}", n.replace('/', "%2F")))
                    .collect::<Vec<_>>()
                    .join("&");
                let response = self
                    .http
                    .post_form(&api_url, &body)
                    .map_err(|(_, message)| message)?;
                let data = Self::decode(&api_url, &response.body)?;
                let Some(Value::Object(by_name)) = data.get("advisories") else {
                    continue;
                };
                for (name, list) in by_name {
                    if !pending.contains_key(name) {
                        io.warning(&format!(
                            "composer repo ({}) returned names which were not requested in response to the security-advisories API. {name} was not requested but is present in the response. Requested names were: {}",
                            sanitize_url(&self.url),
                            if names.len() > 20 {
                                format!("{} and {} more", names[..20].join(", "), names.len() - 20)
                            } else {
                                names.join(", ")
                            }
                        ));
                        continue;
                    }
                    if let Value::Array(list) = list {
                        collect(name, list, &mut pending);
                    }
                }
            }
        }
        Ok(advisories)
    }

    pub fn load_packages_filtered(
        &mut self,
        io: &mut Io,
        names: &[(String, Option<Constraint>)],
        acceptable: &[Stability],
        flags: &BTreeMap<String, u8>,
        already_loaded: &HashMap<String, HashSet<String>>,
    ) -> Result<(Vec<String>, Vec<Package>), String> {
        self.load_root(io, None)?;
        let root = self.root.as_ref().unwrap();
        let mut packages = Vec::new();
        let mut found: Vec<String> = Vec::new();
        if root.metadata_url.is_none() {
            // ArrayRepository::loadPackages over the inline packages
            let all = self.all_packages(io)?;
            let names: Vec<(String, Option<Constraint>)> = names
                .iter()
                .map(|(n, c)| (n.to_lowercase(), c.clone()))
                .collect();
            return Ok(crate::solver::builder::array_load_packages(
                &all,
                &names,
                acceptable,
                flags,
                already_loaded,
            ));
        }
        let only_dev = acceptable == [Stability::Dev] && flags.is_empty();
        let mut jobs: Vec<(String, String, Option<Constraint>)> = Vec::new();
        for (name, constraint) in names {
            let name = name.to_lowercase();
            if is_platform_package(&name) || name == "__root__" || !self.is_allowed(&name) {
                continue;
            }
            if self.has_available_list() && !self.contains(&name) {
                continue;
            }
            if !only_dev {
                jobs.push((name.clone(), name.clone(), constraint.clone()));
            }
            if crate::solver::is_package_acceptable(
                acceptable,
                flags,
                std::slice::from_ref(&name),
                Stability::Dev,
            ) {
                jobs.push((format!("{name}~dev"), name.clone(), constraint.clone()));
            }
        }
        // partial packages inlined in packages.json win over the lazy files
        let inline = root.inline.clone();
        let files: Vec<String> = jobs.iter().map(|(file, _, _)| file.clone()).collect();
        let fetched = self.fetch_provider_files(io, &files)?;
        for ((file, real_name, constraint), fetched) in jobs.into_iter().zip(fetched) {
            let inline_versions: Vec<Map<String, Value>> = inline
                .iter()
                .filter(|m| {
                    m.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.to_lowercase() == real_name)
                })
                .cloned()
                .collect();
            let data: Option<Map<String, Value>> = if !inline_versions.is_empty() {
                if file.ends_with("~dev") {
                    continue;
                }
                let mut m = Map::new();
                let mut by_name = Map::new();
                by_name.insert(
                    real_name.clone(),
                    Value::Array(inline_versions.into_iter().map(Value::Object).collect()),
                );
                m.insert("packages".into(), Value::Object(by_name));
                Some(m)
            } else {
                fetched
            };
            let Some(data) = data else { continue };
            let Some(versions) = data
                .get("packages")
                .and_then(|p| p.get(&real_name))
                .and_then(Value::as_array)
            else {
                continue;
            };
            if !found.contains(&real_name) {
                found.push(real_name.clone());
            }
            let versions = if data.get("minified") == Some(&Value::String("composer/2.0".into())) {
                expand_minified(versions)
            } else {
                versions
                    .iter()
                    .filter_map(|v| v.as_object().cloned())
                    .collect()
            };
            for mut version in versions {
                normalize_version(&mut version)?;
                let normalized = version
                    .get("version_normalized")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if already_loaded
                    .get(&real_name)
                    .is_some_and(|v| v.contains(&normalized))
                {
                    continue;
                }
                if Self::version_acceptable(
                    constraint.as_ref(),
                    &real_name,
                    &version,
                    Some((acceptable, flags)),
                ) {
                    push_with_alias_of(&mut packages, self.create_package(version)?);
                }
            }
        }
        Ok((found, packages))
    }

    /// `getPackageNames`
    pub fn package_names(
        &mut self,
        io: &mut Io,
        filter: Option<&str>,
    ) -> Result<Vec<String>, String> {
        self.load_root(io, None)?;
        let root = self.root.as_ref().unwrap();
        let regex = filter.filter(|f| !f.is_empty()).map(package_name_to_regexp);
        let apply = |names: Vec<String>| -> Vec<String> {
            match &regex {
                Some(r) => names.into_iter().filter(|n| r.is_match(n)).collect(),
                None => names,
            }
        };
        if root.metadata_url.is_some() {
            if let Some(available) = &root.available {
                let mut names: Vec<String> = available.iter().cloned().collect();
                names.sort();
                return Ok(apply(names));
            }
            if root.list_url.is_some() {
                return self.load_package_list(io, filter);
            }
            let names: Vec<String> = root
                .inline
                .iter()
                .filter_map(|m| m.get("name").and_then(Value::as_str))
                .map(str::to_lowercase)
                .collect();
            return Ok(apply(names));
        }
        let names: Vec<String> = root
            .inline
            .iter()
            .filter_map(|m| m.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        Ok(apply(names))
    }

    /// `loadPackageList`
    fn load_package_list(
        &mut self,
        io: &mut Io,
        filter: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let list_url = self.root.as_ref().unwrap().list_url.clone().unwrap();
        if let Some(filter) = filter.filter(|f| !f.is_empty()) {
            let url = format!("{list_url}?filter={}", urlencode(filter));
            return self.fetch_names(&url);
        }
        if self
            .cache_age("package-list.txt")
            .is_some_and(|age| age < 600)
            && let Some(cached) = self.cache_read("package-list.txt")
        {
            return Ok(cached.split('\n').map(str::to_string).collect());
        }
        let names = self.fetch_names(&list_url)?;
        self.cache_write("package-list.txt", &names.join("\n"));
        let _ = io;
        Ok(names)
    }

    fn fetch_names(&self, url: &str) -> Result<Vec<String>, String> {
        let response = self.http.get(url, &[]).map_err(|(_, m)| m)?;
        let data = Self::decode(url, &response.body)?;
        Ok(data
            .get("packageNames")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `getVendorNames`
    fn vendor_names(&mut self, io: &mut Io) -> Result<Vec<String>, String> {
        if self
            .cache_age("vendor-list.txt")
            .is_some_and(|age| age < 600)
            && let Some(cached) = self.cache_read("vendor-list.txt")
        {
            return Ok(cached.split('\n').map(str::to_string).collect());
        }
        let names = self.package_names(io, None)?;
        let mut vendors: Vec<String> = Vec::new();
        for name in names {
            let vendor = name.split('/').next().unwrap_or("").to_string();
            if !vendors.contains(&vendor) {
                vendors.push(vendor);
            }
        }
        self.cache_write("vendor-list.txt", &vendors.join("\n"));
        Ok(vendors)
    }

    /// `search`
    pub fn search(
        &mut self,
        io: &mut Io,
        query: &str,
        mode: u8,
        kind: Option<&str>,
    ) -> Result<Vec<Map<String, Value>>, String> {
        self.load_root(io, Some(600))?;
        let root = self.root.as_ref().unwrap();
        if let Some(search_url) = &root.search_url
            && mode == SEARCH_FULLTEXT
        {
            let url = search_url
                .replace("%query%", &urlencode(query))
                .replace("%type%", kind.unwrap_or(""));
            let response = self.http.get(&url, &[]).map_err(|(_, m)| m)?;
            let data = Self::decode(&url, &response.body)?;
            let results = data
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            return Ok(results
                .into_iter()
                .filter_map(|r| r.as_object().cloned())
                .filter(|r| !r.get("virtual").is_some_and(truthy))
                .filter(|r| {
                    r.get("name")
                        .and_then(Value::as_str)
                        .is_none_or(|n| self.is_allowed(n))
                })
                .collect());
        }
        let named = |names: Vec<String>| -> Vec<Map<String, Value>> {
            names
                .into_iter()
                .map(|n| {
                    let mut m = Map::new();
                    m.insert("name".into(), Value::String(n));
                    m.insert("description".into(), Value::String(String::new()));
                    m
                })
                .collect()
        };
        if mode == SEARCH_VENDOR {
            let regex = search_regex(query, false);
            let vendors = self.vendor_names(io)?;
            return Ok(named(
                vendors.into_iter().filter(|v| regex.is_match(v)).collect(),
            ));
        }
        if root.metadata_url.is_some() {
            let vendor_prefix =
                Regex::new(r"(?i)^\^(?P<query>(?P<vendor>[a-z0-9_.-]+)/[a-z0-9_.-]*)\*?$").unwrap();
            if let Some(m) = vendor_prefix.captures(query)
                && let Some(list_url) = &root.list_url
            {
                let url = format!(
                    "{list_url}?vendor={}&filter={}",
                    urlencode(&m["vendor"]),
                    urlencode(&format!("{}*", &m["query"]))
                );
                let names = self.fetch_names(&url)?;
                return Ok(named(
                    names.into_iter().filter(|n| self.is_allowed(n)).collect(),
                ));
            }
            let regex = search_regex(query, false);
            let names = self.package_names(io, None)?;
            return Ok(named(
                names
                    .into_iter()
                    .filter(|n| regex.is_match(n) && self.is_allowed(n))
                    .collect(),
            ));
        }
        let packages = self.all_packages(io)?;
        let refs: Vec<&Package> = packages.iter().collect();
        Ok(search_packages(&refs, query, mode, kind))
    }

    /// `getPackages` for static repositories (every inline version).
    pub fn all_packages(&mut self, io: &mut Io) -> Result<Vec<Package>, String> {
        self.load_root(io, None)?;
        let inline = self.root.as_ref().unwrap().inline.clone();
        let mut packages = Vec::new();
        for mut data in inline {
            let name = data
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            if !self.is_allowed(&name) {
                continue;
            }
            normalize_version(&mut data)?;
            push_with_alias_of(&mut packages, self.create_package(data)?);
        }
        Ok(packages)
    }

    /// Whether this repository can list every package it holds (`getPackageNames` is
    /// finite): v2 repositories without a list fall back to an empty listing.
    pub fn is_lazy(&self) -> bool {
        self.root.as_ref().is_some_and(|r| r.metadata_url.is_some())
    }
}

/// `ArrayLoader::configureCachedLinks` skips links a package declares on its own name.
fn drop_self_links(package: &mut Package) {
    let name = package.name.clone();
    for links in [
        &mut package.requires,
        &mut package.dev_requires,
        &mut package.conflicts,
        &mut package.provides,
        &mut package.replaces,
    ] {
        links.retain(|l| l.target != name);
    }
    if let Some(real) = &mut package.alias_of {
        drop_self_links(real);
    }
}

/// `loadAsyncPackages`: an alias is followed by the package it aliases.
fn push_with_alias_of(packages: &mut Vec<Package>, package: Package) {
    let real = package.alias_of.clone();
    packages.push(package);
    if let Some(real) = real
        && !packages
            .iter()
            .any(|p| p.alias_of.is_none() && p.name == real.name && p.version == real.version)
    {
        packages.push(*real);
    }
}

/// PHP `!empty()`
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !(s.is_empty() || s == "0"),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Missing or v1-style `version_normalized` is recomputed from `version`.
fn normalize_version(data: &mut Map<String, Value>) -> Result<(), String> {
    let normalized = data.get("version_normalized").and_then(Value::as_str);
    if normalized.is_none_or(|n| n == "9999999-dev") {
        let version = data
            .get("version")
            .and_then(Value::as_str)
            .ok_or("Package metadata is missing a version")?;
        let n = version_parser::normalize(version, None)?;
        data.insert("version_normalized".into(), Value::String(n));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_minified_metadata_like_composer() {
        let versions: Value = serde_json::json!([
            {"name": "a/b", "version": "2.0.0", "require": {"php": ">=8"}, "license": ["MIT"]},
            {"version": "1.1.0", "require": "__unset"},
            {"version": "1.0.0", "license": ["BSD"]}
        ]);
        let expanded = expand_minified(versions.as_array().unwrap());
        assert_eq!(expanded.len(), 3);
        assert!(expanded[1].get("require").is_none());
        assert_eq!(expanded[1]["license"], serde_json::json!(["MIT"]));
        assert_eq!(expanded[2]["license"], serde_json::json!(["BSD"]));
        assert_eq!(expanded[2]["name"], "a/b");
        assert!(expanded[2].get("require").is_none());
    }

    #[test]
    fn name_patterns_and_urlencode() {
        assert!(package_name_to_regexp("symfony/*").is_match("Symfony/console"));
        assert!(!package_name_to_regexp("symfony/*").is_match("psr/log"));
        assert_eq!(urlencode("psr/log *"), "psr%2Flog+%2A");
        let r = search_regex("foo bar", true);
        assert!(r.is_match("x/BAR") && !r.is_match("x/baz"));
    }
}
