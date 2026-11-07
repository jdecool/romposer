//! Port of `VcsRepository` with the git drivers: `GitDriver` (shelling out to git, bare
//! mirrors in `cache-vcs-dir`), `GitHubDriver`, `GitLabDriver` and `GitBitbucketDriver`
//! (their HTTP APIs, falling back to a git clone when the API refuses), and the
//! `composer.json` per commit cache in `cache-repo-dir`. A scan yields package metadata
//! that `ComposerRepository` serves like a static repository.
//!
//! ponytail: there is no terminal to prompt on, so every path is Composer's
//! non-interactive one; `-vvv` diagnostics ("Skipped tag ...") are not printed.

use crate::config::Config;
use crate::download::{Http, HttpResponse, sanitize_url};
use crate::git::{self, cache_usable, is_local_path};
use crate::io::Io;
use crate::json::{Map, Value, encode};
use crate::semver::version_parser;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// An error with its HTTP status when it is a `TransportException` (0: no response).
type Fail = (Option<u16>, String);

fn other(message: impl Into<String>) -> Fail {
    (None, message.into())
}

/// `VcsRepository::shouldRethrowTransportException`
fn rethrow(fail: &Fail) -> bool {
    fail.0
        .is_some_and(|s| matches!(s, 401 | 403 | 429) || s >= 500)
}

pub const UNSUPPORTED: [&str; 4] = ["svn", "hg", "fossil", "perforce"];

pub fn unsupported(kind: &str) -> String {
    format!("{kind} repositories are not supported by romposer, only git ones are")
}

/// The packages of a VCS repository and the driver that read them.
pub struct Scan {
    pub driver: &'static str,
    pub packages: Vec<Map<String, Value>>,
}

struct GitDriver {
    url: String,
    repo_dir: PathBuf,
    local: bool,
    cache: Option<PathBuf>,
    tags: Vec<(String, String)>,
    branches: Vec<(String, String)>,
    root: Option<String>,
}

impl GitDriver {
    /// `GitDriver::initialize`
    fn new(io: &mut Io, config: &Config, url: &str) -> Result<GitDriver, String> {
        let (url, repo_dir, local, cache_url) = if is_local_path(url) {
            let url = Regex::new(r"[\\/]\.git/?$")
                .unwrap()
                .replace(url, "")
                .into_owned();
            if !Path::new(&url).is_dir() {
                return Err(format!(
                    "Failed to read package information from {} as the path does not exist",
                    sanitize_url(&url)
                ));
            }
            let real = crate::fs::realpath(Path::new(&url));
            (url.clone(), PathBuf::from(&url), true, real)
        } else {
            if !cache_usable(&config.get_str("cache-vcs-dir")) {
                return Err("GitDriver requires a usable cache directory, and it looks like you set it to be disabled".into());
            }
            let dir = git::mirror_dir(config, url)
                .ok_or_else(|| format!("Invalid repository URL {}", sanitize_url(url)))?;
            if let Some(parent) = dir.parent() {
                crate::fs::ensure_dir(parent)?;
            }
            if Regex::new(r"^ssh://[^@]+@[^:]+:[^0-9]+")
                .unwrap()
                .is_match(url)
            {
                return Err(format!(
                    "The source URL {} is invalid, ssh URLs should have a port number after \":\".\nUse ssh://git@example.com:22/path or just git@example.com:path if you do not want to provide a password or custom port.",
                    sanitize_url(url)
                ));
            }
            if !git::sync_mirror(io, config, url, &dir)? {
                if !dir.is_dir() {
                    return Err(format!(
                        "Failed to clone {} to read package information from it",
                        sanitize_url(url)
                    ));
                }
                io.error(&format!(
                    "Failed to update {}, package information from this repository may be outdated",
                    sanitize_url(url)
                ));
            }
            (url.to_string(), dir, false, url.to_string())
        };
        let mut tags: Vec<(String, String)> = Vec::new();
        let tag_re = Regex::new(r"^([a-f0-9]{40}) refs/tags/(\S+?)(\^\{\})?$").unwrap();
        for line in git::git(&["show-ref", "--tags", "--dereference"], Some(&repo_dir))
            .stdout
            .lines()
        {
            if let Some(m) = tag_re.captures(line) {
                insert(&mut tags, &m[2], &m[1]);
            }
        }
        let mut branches = Vec::new();
        let head = Regex::new(r"^ *[^/]+/HEAD ").unwrap();
        let branch_re = Regex::new(r"^(?:\* )? *(\S+) *([a-f0-9]+)(?: .*)?$").unwrap();
        for line in git::git(
            &["branch", "--no-color", "--no-abbrev", "-v"],
            Some(&repo_dir),
        )
        .stdout
        .lines()
        {
            if !line.is_empty()
                && !head.is_match(line)
                && let Some(m) = branch_re.captures(line)
                && !m[1].starts_with('-')
            {
                insert(&mut branches, &m[1], &m[2]);
            }
        }
        Ok(GitDriver {
            url,
            repo_dir,
            local,
            cache: cache_dir(config, &[&sanitize_name(&cache_url)]),
            tags,
            branches,
            root: None,
        })
    }

    /// `GitDriver::getRootIdentifier`
    fn root_identifier(&mut self, io: &mut Io, config: &Config) -> String {
        if let Some(root) = &self.root {
            return root.clone();
        }
        let mut root = "master".to_string();
        let remote = if self.local {
            None
        } else {
            git::mirror_default_branch(io, config, &self.url, &self.repo_dir)
        };
        if let Some(branch) = remote {
            root = branch;
        } else {
            let out = git::git(&["branch", "--no-color"], Some(&self.repo_dir)).stdout;
            if !out.lines().any(|l| l == "* master") {
                let re = Regex::new(r"^\* +(\S+)").unwrap();
                if let Some(m) = out.lines().find_map(|l| re.captures(l)) {
                    root = m[1].to_string();
                }
            }
        }
        self.root = Some(root.clone());
        root
    }

    fn file_content(&self, file: &str, id: &str) -> Result<Option<String>, Fail> {
        if id.starts_with('-') {
            return Err(other(format!(
                "Invalid git identifier detected. Identifier must not start with a -, given: {id}"
            )));
        }
        let content = git::git(&["show", &format!("{id}:{file}")], Some(&self.repo_dir)).stdout;
        Ok((!content.trim().is_empty()).then_some(content))
    }

    fn change_date(&self, id: &str) -> Result<Option<String>, Fail> {
        if id.starts_with('-') {
            return Err(other(format!(
                "Invalid git identifier detected. Identifier must not start with a -, given: {id}"
            )));
        }
        let out = git::git(
            &["rev-list", "--no-commit-header", "-n1", "--format=%at", id],
            Some(&self.repo_dir),
        )
        .stdout;
        Ok(out.trim().parse::<i64>().ok().map(epoch_rfc3339))
    }

    fn source(&self, id: &str) -> Value {
        source("git", &self.url, id)
    }
}

/// PHP array assignment: a repeated key keeps its position, takes the new value.
fn insert(list: &mut Vec<(String, String)>, key: &str, value: &str) {
    match list.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value.to_string(),
        None => list.push((key.to_string(), value.to_string())),
    }
}

fn source(kind: &str, url: &str, id: &str) -> Value {
    serde_json::json!({"type": kind, "url": url, "reference": id})
}

fn zip_dist(url: String, id: &str) -> Value {
    serde_json::json!({"type": "zip", "url": url, "reference": id, "shasum": ""})
}

fn sanitize_name(url: &str) -> String {
    Regex::new(r"(?i)[^a-z0-9.]")
        .unwrap()
        .replace_all(&sanitize_url(url), "-")
        .into_owned()
}

/// `cache-repo-dir/<parts>`, `None` when disabled or a part would leave the directory.
fn cache_dir(config: &Config, parts: &[&str]) -> Option<PathBuf> {
    let root = config.get_str("cache-repo-dir");
    if !cache_usable(&root)
        || parts
            .iter()
            .flat_map(|p| p.split('/'))
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return None;
    }
    Some(Path::new(&root).join(parts.join("/")))
}

/// `DateTime::format(DATE_RFC3339)` of a unix timestamp, in UTC.
fn epoch_rfc3339(secs: i64) -> String {
    let (y, m, d) = crate::commands::show::civil_from_days(secs.div_euclid(86400));
    let s = secs.rem_euclid(86400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// `new DateTimeImmutable($date)` then RFC 3339 for the API dates (fractions dropped).
fn api_date(date: &str) -> String {
    let date = Regex::new(r"\.\d+").unwrap().replace(date, "");
    match date.strip_suffix('Z') {
        Some(utc) => format!("{utc}+00:00"),
        None => crate::package::dumper::rfc3339(&date),
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn json(response: &HttpResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap_or(Value::Null)
}

fn s(v: &Value, path: &[&str]) -> String {
    path.iter()
        .try_fold(v, |v, k| v.get(*k))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !(s.is_empty() || s == "0"),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `getNextPage`: the `rel="next"` link.
fn next_page(response: &HttpResponse) -> Option<String> {
    let re = Regex::new(r#"<(.+?)>; *rel="next""#).unwrap();
    response
        .link
        .as_deref()?
        .split(',')
        .find_map(|l| re.captures(l).map(|m| m[1].to_string()))
}

static GITHUB_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?:https?|git)://([^/]+)/|git@([^:]+):/?)([^/]+)/([^/]+?)(?:\.git|/)?$")
        .unwrap()
});
static GITLAB_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?P<scheme>https?)://(?P<domain>.+?)(?::(?P<port>[0-9]+))?/|git@(?P<domain2>[^:]+):)(?P<parts>.+)/(?P<repo>[^/]+?)(?:\.git|/)?$").unwrap()
});
static DEV_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[.-]?dev$").unwrap());
static DEV_MARKERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(^dev-|[.-]?dev$)").unwrap());
static NINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\.9{7})+").unwrap());
static BITBUCKET_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^https?://bitbucket\.org/([^/]+)/([^/]+?)(?:\.git|/?)?$").unwrap()
});

enum Api {
    Git,
    GitHub {
        owner: String,
        repo: String,
        origin: String,
        private: bool,
        issues: bool,
        archived: bool,
        allow_fallback: bool,
        root: String,
        funding: Option<Option<Value>>,
    },
    GitLab {
        scheme: String,
        origin: String,
        namespace: String,
        repo: String,
        project: Value,
        commits: HashMap<String, Value>,
        protocol: Option<String>,
        private: bool,
    },
    Bitbucket {
        owner: String,
        repo: String,
        data: Option<Value>,
    },
}

struct Driver<'a> {
    kind: &'static str,
    config: &'a Config,
    http: &'a Http,
    url: String,
    api: Api,
    /// the git driver, or the API driver's clone fallback
    git: Option<GitDriver>,
    cache: Option<PathBuf>,
    info: HashMap<String, Option<Map<String, Value>>>,
    tags: Option<Vec<(String, String)>>,
    branches: Option<Vec<(String, String)>>,
}

impl<'a> Driver<'a> {
    /// `VcsRepository::getDriver`
    fn detect(
        io: &mut Io,
        config: &'a Config,
        http: &'a Http,
        repo: &Map<String, Value>,
        url: &str,
        kind: &str,
    ) -> Result<Driver<'a>, String> {
        let driver = match kind {
            "github" => "github",
            "gitlab" => "gitlab",
            "bitbucket" | "git-bitbucket" => "bitbucket",
            "git" => "git",
            "forgejo" => return Err(unsupported("forgejo")),
            k if UNSUPPORTED.contains(&k) => return Err(unsupported(k)),
            _ => Self::supported(config, url)?,
        };
        let mut d = Driver {
            kind: driver,
            config,
            http,
            url: url.to_string(),
            api: Api::Git,
            git: None,
            cache: None,
            info: HashMap::new(),
            tags: None,
            branches: None,
        };
        match driver {
            "github" => d.init_github(io, repo)?,
            "gitlab" => d.init_gitlab(io, repo)?,
            "bitbucket" => d.init_bitbucket(),
            _ => d.git = Some(GitDriver::new(io, config, url)?),
        }
        Ok(d)
    }

    /// The `supports()` checks, shallow then deep, in Composer's driver order.
    fn supported(config: &Config, url: &str) -> Result<&'static str, String> {
        if let Some(m) = GITHUB_URL.captures(url) {
            let origin = m
                .get(1)
                .or(m.get(2))
                .map_or("", |m| m.as_str())
                .to_lowercase();
            let origin = origin.strip_prefix("www.").unwrap_or(&origin);
            if config.domains("github-domains").iter().any(|d| d == origin) {
                return Ok("github");
            }
        }
        if let Some(m) = GITLAB_URL.captures(url) {
            let domain = m
                .name("domain")
                .or(m.name("domain2"))
                .map_or("", |m| m.as_str());
            let mut parts: Vec<&str> = m["parts"].split('/').collect();
            if gitlab_origin(
                config,
                domain,
                &mut parts,
                m.name("port").map(|p| p.as_str()),
            )
            .is_some()
            {
                return Ok("gitlab");
            }
        }
        if BITBUCKET_URL.is_match(url) {
            return Ok("bitbucket");
        }
        if let Some(m) = GITHUB_URL.captures(url) {
            let origin = m
                .get(1)
                .or(m.get(2))
                .map_or("", |m| m.as_str())
                .to_lowercase();
            if config.string_list("forgejo-domains").contains(&origin) {
                return Err(unsupported("forgejo"));
            }
        }
        if Regex::new(r"(?i)(^git://|\.git/?$|git(?:olite)?@|//git\.|//github.com/)")
            .unwrap()
            .is_match(url)
        {
            return Ok("git");
        }
        if is_local_path(url) && Path::new(url).is_dir() {
            if git::git(&["tag"], Some(Path::new(url))).ok {
                return Ok("git");
            }
            for (marker, kind) in [(".hg", "hg"), (".svn", "svn"), (".fslckout", "fossil")] {
                if Path::new(url).join(marker).exists() {
                    return Err(unsupported(kind));
                }
            }
        }
        if Regex::new(
            r"(?i)(^(?:https?|ssh)://(?:[^@]+@)?bitbucket.org|https://(?:.*?)\.kilnhg.com)",
        )
        .unwrap()
        .is_match(url)
        {
            return Err(unsupported("hg"));
        }
        if Regex::new(r"(?i)\b(perforce|p4)\b").unwrap().is_match(url) {
            return Err(unsupported("perforce"));
        }
        if Regex::new(
            r"(?i)(^(?:https?|ssh)://(?:[^@]@)?(?:chiselapp\.com|fossil\.))|/fossil/|\.fossil",
        )
        .unwrap()
        .is_match(url)
        {
            return Err(unsupported("fossil"));
        }
        if Regex::new(r"(?i)(^svn://|^svn\+ssh://|svn\.)")
            .unwrap()
            .is_match(url)
        {
            return Err(unsupported("svn"));
        }
        // the deep git check: `git ls-remote` reaches the server
        if git::prohibit_url(config, url).is_ok()
            && git::git(
                &["ls-remote", "--heads", "--", url],
                Some(&std::env::temp_dir()),
            )
            .ok
        {
            return Ok("git");
        }
        Err(format!(
            "No driver found to handle VCS repository {}",
            sanitize_url(url)
        ))
    }

    fn get(&self, url: &str) -> Result<HttpResponse, Fail> {
        self.http
            .get(url, &[])
            .map_err(|(status, message)| (Some(status.unwrap_or(0)), message))
    }

    /// `setupGitDriver` / `attemptCloneFallback`: `message` is printed when the clone fails.
    fn fallback(&mut self, io: &mut Io, url: &str, message: Option<String>) -> Result<(), Fail> {
        match GitDriver::new(io, self.config, url) {
            Ok(g) => {
                self.git = Some(g);
                Ok(())
            }
            Err(e) => {
                if let Some(m) = message {
                    io.error(&m);
                }
                Err(other(e))
            }
        }
    }

    // --- GitHub ---

    fn init_github(&mut self, io: &mut Io, repo: &Map<String, Value>) -> Result<(), String> {
        let m = GITHUB_URL.captures(&self.url).ok_or_else(|| {
            format!(
                "The GitHub repository URL {} is invalid.",
                sanitize_url(&self.url)
            )
        })?;
        let mut origin = m
            .get(1)
            .or(m.get(2))
            .map_or("", |m| m.as_str())
            .to_lowercase();
        if origin == "www.github.com" {
            origin = "github.com".into();
        }
        let (owner, name) = (m[3].to_string(), m[4].to_string());
        self.cache = cache_dir(self.config, &[&origin, &owner, &name]);
        let allow_fallback = repo.get("allow-git-fallback") != Some(&Value::Bool(false));
        self.api = Api::GitHub {
            owner,
            repo: name,
            origin,
            private: false,
            issues: false,
            archived: false,
            allow_fallback,
            root: "master".into(),
            funding: None,
        };
        if self.config.get("use-github-api") == Some(&Value::Bool(false))
            || truthy(repo.get("no-api"))
        {
            if !allow_fallback {
                return Err("Fallback to git driver disabled".into());
            }
            let url = self.url.clone();
            self.git = Some(GitDriver::new(io, self.config, &url)?);
            return Ok(());
        }
        let Api::GitHub { owner, repo, .. } = &self.api else {
            unreachable!()
        };
        let url = format!("{}/repos/{owner}/{repo}", self.github_api());
        let data = match self.github_get(io, &url, true) {
            Ok(Some(r)) => json(&r),
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.1),
        };
        if let Api::GitHub {
            owner,
            repo,
            private,
            issues,
            archived,
            root,
            ..
        } = &mut self.api
        {
            *owner = s(&data, &["owner", "login"]);
            *repo = s(&data, &["name"]);
            *private = truthy(data.get("private"));
            *root = data
                .get("default_branch")
                .or(data.get("master_branch"))
                .and_then(Value::as_str)
                .unwrap_or("master")
                .to_string();
            *issues = truthy(data.get("has_issues"));
            *archived = truthy(data.get("archived"));
        }
        Ok(())
    }

    fn github_api(&self) -> String {
        match &self.api {
            Api::GitHub { origin, .. } if origin == "github.com" => "https://api.github.com".into(),
            Api::GitHub { origin, .. } => format!("https://{origin}/api/v3"),
            _ => String::new(),
        }
    }

    fn github_ssh_url(&self) -> String {
        let Api::GitHub {
            owner,
            repo,
            origin,
            ..
        } = &self.api
        else {
            return String::new();
        };
        if origin.contains(':') {
            format!("ssh://git@{origin}/{owner}/{repo}.git")
        } else {
            format!("git@{origin}:{owner}/{repo}.git")
        }
    }

    /// `GitHubDriver::getContents`: `None` once the git fallback took over.
    fn github_get(
        &mut self,
        io: &mut Io,
        url: &str,
        repo_data: bool,
    ) -> Result<Option<HttpResponse>, Fail> {
        let fail = match self.get(url) {
            Ok(r) => return Ok(Some(r)),
            Err(f) => f,
        };
        let Api::GitHub {
            origin,
            allow_fallback,
            ..
        } = &self.api
        else {
            return Err(fail);
        };
        let (origin, allow_fallback) = (origin.clone(), *allow_fallback);
        let authorized = self.config.authentication(&origin).is_some();
        match fail.0 {
            Some(401 | 404) if repo_data => {
                if authorized {
                    return self.get(url).map(Some);
                }
                self.github_clone_fallback(io, allow_fallback)?;
                Ok(None)
            }
            Some(403) => {
                if repo_data {
                    if let Err(e) = self.github_clone_fallback(io, allow_fallback) {
                        if !authorized {
                            self.github_rate_limit_error(io);
                        }
                        return Err(e);
                    }
                    return Ok(None);
                }
                if !authorized {
                    self.github_rate_limit_error(io);
                }
                // ponytail: the authenticated "limit (n calls/hr) is exhausted" message needs
                // the rate limit headers, which the HTTP layer does not keep on errors
                Err(fail)
            }
            _ => Err(fail),
        }
    }

    fn github_rate_limit_error(&self, io: &mut Io) {
        io.error(&format!(
            "GitHub API limit exhausted. Failed to get metadata for the {} repository, try running in interactive mode so that you can enter your GitHub credentials to increase the API limit",
            sanitize_url(&self.url)
        ));
    }

    fn github_clone_fallback(&mut self, io: &mut Io, allow: bool) -> Result<(), Fail> {
        if !allow {
            return Err(other("Fallback to git driver disabled"));
        }
        if let Api::GitHub { private, .. } = &mut self.api {
            *private = true;
        }
        let ssh = self.github_ssh_url();
        self.fallback(
            io,
            &ssh,
            Some(format!(
                "Failed to clone the {ssh} repository, try running in interactive mode so that you can enter your GitHub credentials"
            )),
        )
    }

    /// `GitHubDriver::getFundingInfo`
    fn github_funding(&mut self, io: &mut Io) -> Option<Value> {
        let Api::GitHub {
            owner,
            repo,
            origin,
            funding,
            ..
        } = &self.api
        else {
            return None;
        };
        if let Some(f) = funding {
            return f.clone();
        }
        if origin != "github.com" {
            return None;
        }
        let api = self.github_api();
        let urls = [
            format!("{api}/repos/{owner}/{repo}/contents/.github/FUNDING.yml"),
            format!("{api}/repos/{owner}/.github/contents/FUNDING.yml"),
        ];
        let mut text = None;
        for url in urls {
            let Ok(r) = self.get(&url) else { continue };
            let data = json(&r);
            if s(&data, &["encoding"]) != "base64" {
                continue;
            }
            if let Some(bytes) = base64_decode(&s(&data, &["content"])).filter(|b| !b.is_empty()) {
                text = Some(String::from_utf8_lossy(&bytes).into_owned());
                break;
            }
        }
        let result = text.and_then(|t| parse_funding(io, &t));
        if let Api::GitHub { funding, .. } = &mut self.api {
            *funding = Some(result.clone());
        }
        result
    }

    // --- GitLab ---

    fn init_gitlab(&mut self, io: &mut Io, repo: &Map<String, Value>) -> Result<(), String> {
        let m = GITLAB_URL.captures(&self.url).ok_or_else(|| {
            format!(
                "The GitLab repository URL {} is invalid. It must be the HTTP URL of a GitLab project.",
                sanitize_url(&self.url)
            )
        })?;
        let domain = m
            .name("domain")
            .or(m.name("domain2"))
            .map_or("", |m| m.as_str());
        let mut parts: Vec<&str> = m["parts"].split('/').collect();
        let scheme = match m.name("scheme").map(|s| s.as_str()) {
            Some(s) => s.to_string(),
            None if repo.get("secure-http") == Some(&Value::Bool(false)) => "http".into(),
            None => "https".into(),
        };
        let origin = gitlab_origin(self.config, domain, &mut parts, m.name("port").map(|p| p.as_str()))
            .ok_or_else(|| {
                format!(
                    "It should not be possible to create a gitlab driver with an unparsable origin URL ({})",
                    sanitize_url(&self.url)
                )
            })?;
        let protocol = match self.config.get("gitlab-protocol").and_then(Value::as_str) {
            Some("git") => Some("ssh".to_string()),
            Some("http" | "https") => Some("http".to_string()),
            Some(_) => return Err("gitlab-protocol must be one of git, http.".into()),
            None => None,
        };
        let namespace = parts.join("/");
        let name = m["repo"].trim_end_matches(".git").to_string();
        self.cache = cache_dir(self.config, &[&origin, &namespace, &name]);
        self.api = Api::GitLab {
            scheme,
            origin,
            namespace,
            repo: name,
            project: Value::Null,
            commits: HashMap::new(),
            protocol,
            private: true,
        };
        let url = self.gitlab_api();
        match self.gitlab_get(io, &url, true) {
            Ok(Some(r)) => {
                let project = json(&r);
                if let Api::GitLab {
                    project: p,
                    private,
                    ..
                } = &mut self.api
                {
                    *private = project
                        .get("visibility")
                        .and_then(Value::as_str)
                        .is_some_and(|v| v != "public");
                    *p = project;
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => Err(e.1),
        }
    }

    fn gitlab_api(&self) -> String {
        let Api::GitLab {
            scheme,
            origin,
            namespace,
            repo,
            ..
        } = &self.api
        else {
            return String::new();
        };
        format!(
            "{scheme}://{origin}/api/v4/projects/{}%2F{}",
            url_encode_all(namespace),
            url_encode_all(repo)
        )
    }

    /// `GitLabDriver::getContents`
    fn gitlab_get(
        &mut self,
        io: &mut Io,
        url: &str,
        repo_data: bool,
    ) -> Result<Option<HttpResponse>, Fail> {
        let result = self.get(url).and_then(|r| {
            if !repo_data {
                return Ok(r);
            }
            let data = json(&r);
            if data.get("default_branch").is_none() && data.get("permissions").is_some() {
                // a Guest or Planner token sees the project but not its files
                let more_than_guest = data["permissions"]
                    .as_object()
                    .is_some_and(|p| {
                        p.values().any(|v| {
                            v.get("access_level").and_then(Value::as_i64).unwrap_or(0) >= 20
                        })
                    });
                if let Api::GitLab { private, .. } = &mut self.api {
                    *private = s(&data, &["visibility"]) != "public";
                }
                if !more_than_guest {
                    return Err((Some(1), "guest".into()));
                }
            }
            if data.get("default_branch").is_none() {
                if s(&data, &["repository_access_level"]) == "disabled" {
                    return Err((
                        Some(400),
                        "The GitLab repository is disabled in the project".into(),
                    ));
                }
                if truthy(data.get("id"))
                    && let Api::GitLab { private, .. } = &mut self.api
                {
                    *private = false;
                }
                return Err((
                    Some(401),
                    "GitLab API seems to not be authenticated as it did not return a default_branch".into(),
                ));
            }
            Ok(r)
        });
        let fail = match result {
            Ok(r) => return Ok(Some(r)),
            Err((Some(1), _)) => {
                io.warning("GitLab token with Guest or Planner only access detected");
                self.gitlab_clone_fallback(io)?;
                return Ok(None);
            }
            Err(f) => f,
        };
        let origin = match &self.api {
            Api::GitLab { origin, .. } => origin.clone(),
            _ => String::new(),
        };
        let authorized = self.config.authentication(&origin).is_some();
        match fail.0 {
            Some(401 | 404) if repo_data => {
                if authorized {
                    return self.get(url).map(Some);
                }
                self.gitlab_clone_fallback(io)?;
                Ok(None)
            }
            Some(403) if repo_data => {
                self.gitlab_clone_fallback(io)?;
                Ok(None)
            }
            _ => Err(fail),
        }
    }

    fn gitlab_clone_fallback(&mut self, io: &mut Io) -> Result<(), Fail> {
        let Api::GitLab {
            scheme,
            origin,
            namespace,
            repo,
            private,
            ..
        } = &self.api
        else {
            return Ok(());
        };
        let url = if !*private {
            format!("{scheme}://{origin}/{namespace}/{repo}.git")
        } else if origin.contains(':') || origin.contains('/') {
            format!("ssh://git@{origin}/{namespace}/{repo}.git")
        } else {
            format!("git@{origin}:{namespace}/{repo}.git")
        };
        let message = format!(
            "Failed to clone the {} repository, try running in interactive mode so that you can enter your credentials",
            sanitize_url(&url)
        );
        self.fallback(io, &url, Some(message))
    }

    /// `GitLabDriver::getReferences`
    fn gitlab_references(
        &mut self,
        io: &mut Io,
        kind: &str,
    ) -> Result<Vec<(String, String)>, Fail> {
        let mut url = Some(format!(
            "{}/repository/{kind}?per_page=100",
            self.gitlab_api()
        ));
        let mut refs = Vec::new();
        while let Some(u) = url {
            let r = self
                .gitlab_get(io, &u, false)?
                .ok_or_else(|| other("no response"))?;
            let data = json(&r);
            let list = data.as_array().cloned().unwrap_or_default();
            for datum in &list {
                let id = s(datum, &["commit", "id"]);
                insert(&mut refs, &s(datum, &["name"]), &id);
                if let Api::GitLab { commits, .. } = &mut self.api {
                    commits.insert(id, datum["commit"].clone());
                }
            }
            url = if list.len() >= 100 {
                next_page(&r)
            } else {
                None
            };
        }
        Ok(refs)
    }

    // --- Bitbucket ---

    fn init_bitbucket(&mut self) {
        let (owner, repo) = match BITBUCKET_URL.captures(&self.url) {
            Some(m) => (m[1].to_string(), m[2].to_string()),
            None => (String::new(), String::new()),
        };
        self.cache = cache_dir(self.config, &["bitbucket.org", &owner, &repo]);
        self.api = Api::Bitbucket {
            owner,
            repo,
            data: None,
        };
    }

    /// `GitBitbucketDriver::fetchWithOAuthCredentials`
    fn bitbucket_get(
        &mut self,
        io: &mut Io,
        url: &str,
        repo_data: bool,
    ) -> Result<Option<HttpResponse>, Fail> {
        // ponytail: `bitbucket-oauth` consumers are sent as basic auth instead of being
        // exchanged for an access token first; app passwords work as in Composer
        match self.get(url) {
            Ok(r) => Ok(Some(r)),
            Err(f) if repo_data && matches!(f.0, Some(401 | 403 | 404)) => {
                let Api::Bitbucket { owner, repo, .. } = &self.api else {
                    return Err(f);
                };
                let ssh = format!("git@ssh.bitbucket.org:{owner}/{repo}.git");
                self.fallback(
                    io,
                    &ssh,
                    Some(format!(
                        "Failed to clone the {ssh} repository, try running in interactive mode so that you can enter your Bitbucket OAuth consumer credentials"
                    )),
                )?;
                Ok(None)
            }
            Err(f) => Err(f),
        }
    }

    fn bitbucket_data(&mut self, io: &mut Io) -> Result<Option<Value>, Fail> {
        let Api::Bitbucket { owner, repo, data } = &self.api else {
            return Ok(None);
        };
        if data.is_some() || self.git.is_some() {
            return Ok(data.clone());
        }
        if !BITBUCKET_URL.is_match(&self.url) {
            return Err(other(format!(
                "The Bitbucket repository URL {} is invalid. It must be the HTTPS URL of a Bitbucket repository.",
                sanitize_url(&self.url)
            )));
        }
        let url = format!(
            "https://api.bitbucket.org/2.0/repositories/{owner}/{repo}?fields=-project%2C-owner"
        );
        let Some(r) = self.bitbucket_get(io, &url, true)? else {
            return Ok(None);
        };
        let value = json(&r);
        if s(&value, &["scm"]) != "git" {
            return Err(other(format!(
                "{} does not appear to be a git repository, use {} but remember that Bitbucket no longer supports the mercurial repositories. https://bitbucket.org/blog/sunsetting-mercurial-support-in-bitbucket",
                sanitize_url(&self.url),
                bitbucket_clone_url(&value)
            )));
        }
        if let Api::Bitbucket { data, .. } = &mut self.api {
            *data = Some(value.clone());
        }
        Ok(Some(value))
    }

    fn bitbucket_refs(
        &mut self,
        io: &mut Io,
        link: &str,
        branches: bool,
    ) -> Result<Vec<(String, String)>, Fail> {
        let Some(data) = self.bitbucket_data(io)? else {
            return Ok(vec![]);
        };
        let fields = if branches {
            "values.name%2Cvalues.target.hash%2Cvalues.heads%2Cnext"
        } else {
            "values.name%2Cvalues.target.hash%2Cnext"
        };
        let mut url = Some(format!(
            "{}?pagelen=100&fields={fields}&sort=-target.date",
            s(&data, &["links", link, "href"])
        ));
        let mut refs = Vec::new();
        while let Some(u) = url {
            let r = self
                .bitbucket_get(io, &u, false)?
                .ok_or_else(|| other("no response"))?;
            let page = json(&r);
            for v in page["values"].as_array().into_iter().flatten() {
                insert(&mut refs, &s(v, &["name"]), &s(v, &["target", "hash"]));
            }
            url = page
                .get("next")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .map(str::to_string);
        }
        Ok(refs)
    }

    // --- the driver interface ---

    fn root_identifier(&mut self, io: &mut Io) -> Result<String, Fail> {
        if self.git.is_none() && matches!(self.api, Api::Bitbucket { .. }) {
            let data = self.bitbucket_data(io)?;
            if self.git.is_none() {
                return Ok(data
                    .as_ref()
                    .and_then(|d| d.get("mainbranch"))
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("master")
                    .to_string());
            }
        }
        if let Some(g) = &mut self.git {
            return Ok(g.root_identifier(io, self.config));
        }
        Ok(match &self.api {
            Api::GitHub { root, .. } => root.clone(),
            Api::GitLab { project, .. } => s(project, &["default_branch"]),
            _ => "master".into(),
        })
    }

    fn tags(&mut self, io: &mut Io) -> Result<Vec<(String, String)>, Fail> {
        if let Some(g) = &self.git {
            return Ok(g.tags.clone());
        }
        if let Some(t) = &self.tags {
            return Ok(t.clone());
        }
        let tags = match &self.api {
            Api::GitHub { owner, repo, .. } => {
                let mut url = Some(format!(
                    "{}/repos/{owner}/{repo}/tags?per_page=100",
                    self.github_api()
                ));
                let mut tags = Vec::new();
                while let Some(u) = url {
                    let r = self
                        .github_get(io, &u, false)?
                        .ok_or_else(|| other("no response"))?;
                    for tag in json(&r).as_array().into_iter().flatten() {
                        insert(&mut tags, &s(tag, &["name"]), &s(tag, &["commit", "sha"]));
                    }
                    url = next_page(&r);
                }
                tags
            }
            Api::GitLab { .. } => self.gitlab_references(io, "tags")?,
            Api::Bitbucket { .. } => self.bitbucket_refs(io, "tags", false)?,
            Api::Git => vec![],
        };
        if let Some(g) = &self.git {
            return Ok(g.tags.clone());
        }
        self.tags = Some(tags.clone());
        Ok(tags)
    }

    fn branches(&mut self, io: &mut Io) -> Result<Vec<(String, String)>, Fail> {
        if let Some(g) = &self.git {
            return Ok(g.branches.clone());
        }
        if let Some(b) = &self.branches {
            return Ok(b.clone());
        }
        let branches = match &self.api {
            Api::GitHub { owner, repo, .. } => {
                let mut url = Some(format!(
                    "{}/repos/{owner}/{repo}/git/refs/heads?per_page=100",
                    self.github_api()
                ));
                let mut branches = Vec::new();
                while let Some(u) = url {
                    let r = self
                        .github_get(io, &u, false)?
                        .ok_or_else(|| other("no response"))?;
                    for b in json(&r).as_array().into_iter().flatten() {
                        let name = s(b, &["ref"]).get(11..).unwrap_or("").to_string();
                        if name != "gh-pages" {
                            insert(&mut branches, &name, &s(b, &["object", "sha"]));
                        }
                    }
                    url = next_page(&r);
                }
                branches
            }
            Api::GitLab { .. } => self.gitlab_references(io, "branches")?,
            Api::Bitbucket { .. } => self.bitbucket_refs(io, "branches", true)?,
            Api::Git => vec![],
        };
        if let Some(g) = &self.git {
            return Ok(g.branches.clone());
        }
        self.branches = Some(branches.clone());
        Ok(branches)
    }

    fn file_content(&mut self, io: &mut Io, file: &str, id: &str) -> Result<Option<String>, Fail> {
        if let Some(g) = &self.git {
            return g.file_content(file, id);
        }
        match &self.api {
            Api::GitHub { owner, repo, .. } => {
                let url = format!(
                    "{}/repos/{owner}/{repo}/contents/{file}?ref={}",
                    self.github_api(),
                    crate::repository::urlencode(id)
                );
                let r = self
                    .github_get(io, &url, false)?
                    .ok_or_else(|| other("no response"))?;
                let mut data = json(&r);
                if s(&data, &["content"]).is_empty()
                    && s(&data, &["encoding"]) == "none"
                    && let Some(git_url) = data.get("git_url").and_then(Value::as_str)
                {
                    let git_url = git_url.to_string();
                    let r = self
                        .github_get(io, &git_url, false)?
                        .ok_or_else(|| other("no response"))?;
                    data = json(&r);
                }
                match base64_decode(&s(&data, &["content"])) {
                    Some(bytes)
                        if data.get("content").is_some() && s(&data, &["encoding"]) == "base64" =>
                    {
                        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
                    }
                    _ => Err(other(format!("Could not retrieve {file} for {id}"))),
                }
            }
            Api::GitLab { .. } => {
                let mut id = id.to_string();
                if !Regex::new(r"(?i)[a-f0-9]{40}").unwrap().is_match(&id)
                    && let Some((_, sha)) = self.branches(io)?.into_iter().find(|(b, _)| *b == id)
                {
                    id = sha;
                }
                let url = format!(
                    "{}/repository/files/{}/raw?ref={id}",
                    self.gitlab_api(),
                    url_encode_all(file)
                );
                match self.gitlab_get(io, &url, false) {
                    Ok(r) => Ok(r.map(|r| String::from_utf8_lossy(&r.body).into_owned())),
                    Err((Some(404), _)) => Ok(None),
                    Err(e) => Err(e),
                }
            }
            Api::Bitbucket { .. } => {
                let mut id = id.to_string();
                if id.contains('/')
                    && let Some((_, sha)) = self.branches(io)?.into_iter().find(|(b, _)| *b == id)
                {
                    id = sha;
                }
                let Api::Bitbucket { owner, repo, .. } = &self.api else {
                    unreachable!()
                };
                let url = format!(
                    "https://api.bitbucket.org/2.0/repositories/{owner}/{repo}/src/{id}/{file}"
                );
                let r = self.bitbucket_get(io, &url, false)?;
                Ok(r.map(|r| String::from_utf8_lossy(&r.body).into_owned()))
            }
            Api::Git => Ok(None),
        }
    }

    fn change_date(&mut self, io: &mut Io, id: &str) -> Result<Option<String>, Fail> {
        if let Some(g) = &self.git {
            return g.change_date(id);
        }
        match &self.api {
            Api::GitHub { owner, repo, .. } => {
                let url = format!(
                    "{}/repos/{owner}/{repo}/commits/{}",
                    self.github_api(),
                    crate::repository::urlencode(id)
                );
                let r = self
                    .github_get(io, &url, false)?
                    .ok_or_else(|| other("no response"))?;
                Ok(Some(api_date(&s(
                    &json(&r),
                    &["commit", "committer", "date"],
                ))))
            }
            Api::GitLab { commits, .. } => Ok(commits
                .get(id)
                .map(|c| api_date(&s(c, &["committed_date"])))),
            Api::Bitbucket { .. } => {
                let mut id = id.to_string();
                if id.contains('/')
                    && let Some((_, sha)) = self.branches(io)?.into_iter().find(|(b, _)| *b == id)
                {
                    id = sha;
                }
                let Api::Bitbucket { owner, repo, .. } = &self.api else {
                    unreachable!()
                };
                let url = format!(
                    "https://api.bitbucket.org/2.0/repositories/{owner}/{repo}/commit/{id}?fields=date"
                );
                let r = self
                    .bitbucket_get(io, &url, false)?
                    .ok_or_else(|| other("no response"))?;
                Ok(Some(api_date(&s(&json(&r), &["date"]))))
            }
            Api::Git => Ok(None),
        }
    }

    /// `VcsDriver::getComposerInformation` with the per-commit cache, plus the API
    /// drivers' additions (support links, abandoned, funding, homepage).
    fn composer_information(
        &mut self,
        io: &mut Io,
        id: &str,
    ) -> Result<Option<Map<String, Value>>, Fail> {
        if let Some(info) = self.info.get(id) {
            return Ok(info.clone());
        }
        let cache = match &self.git {
            Some(g) => g.cache.clone(),
            None => self.cache.clone(),
        };
        let should_cache =
            cache.is_some() && id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit());
        let cache_file = cache.filter(|_| should_cache).map(|c| {
            c.join(
                Regex::new(r"(?i)[^a-z0-9._]")
                    .unwrap()
                    .replace_all(id, "-")
                    .as_ref(),
            )
        });
        let cached = cache_file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .filter(|c| !c.is_empty());
        let mut composer = match cached {
            Some(c) => serde_json::from_str::<Value>(&c)
                .map_err(|e| {
                    other(format!(
                        "\"{id}:composer.json\" does not contain valid JSON\n{e}"
                    ))
                })?
                .as_object()
                .cloned(),
            None => {
                let composer = self.base_information(io, id)?;
                if let Some(file) = &cache_file
                    && !self.config.bool("cache-read-only")
                {
                    let body = match &composer {
                        Some(c) => encode(&Value::Object(c.clone()), false, true, true),
                        None => "null".into(),
                    };
                    if let Some(parent) = file.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let tmp = file.with_extension(format!("{}.tmp", crate::fs::random_hex(5)));
                    if std::fs::write(&tmp, body).is_ok() {
                        let _ = std::fs::rename(&tmp, file);
                    }
                }
                composer
            }
        };
        if self.git.is_none()
            && let Some(c) = &mut composer
        {
            self.add_api_information(io, id, c)?;
        }
        self.info.insert(id.to_string(), composer.clone());
        Ok(composer)
    }

    /// `VcsDriver::getBaseComposerInformation`
    fn base_information(
        &mut self,
        io: &mut Io,
        id: &str,
    ) -> Result<Option<Map<String, Value>>, Fail> {
        let Some(content) = self.file_content(io, "composer.json", id)? else {
            return Ok(None);
        };
        if content.is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&content).map_err(|e| {
            other(format!(
                "\"{id}:composer.json\" does not contain valid JSON\n{e}"
            ))
        })?;
        let Value::Object(mut composer) = value else {
            return Ok(None);
        };
        if composer.is_empty() {
            return Ok(None);
        }
        if !truthy(composer.get("time"))
            && let Some(date) = self.change_date(io, id)?
        {
            composer.insert("time".into(), Value::String(date));
        }
        Ok(Some(composer))
    }

    fn label(&mut self, io: &mut Io, id: &str) -> Result<String, Fail> {
        let find = |list: Vec<(String, String)>| {
            list.into_iter()
                .find(|(_, sha)| sha == id)
                .map(|(name, _)| name)
                .filter(|n| !n.is_empty() && n != "0")
        };
        if let Some(tag) = find(self.tags(io)?) {
            return Ok(tag);
        }
        Ok(find(self.branches(io)?).unwrap_or_else(|| id.to_string()))
    }

    fn add_api_information(
        &mut self,
        io: &mut Io,
        id: &str,
        composer: &mut Map<String, Value>,
    ) -> Result<(), Fail> {
        if composer
            .get("support")
            .is_some_and(|s| !s.is_object() && !s.is_array())
        {
            composer.insert("support".into(), Value::Object(Map::new()));
        }
        let mut support = match composer.get("support") {
            Some(Value::Object(o)) => o.clone(),
            _ => Map::new(),
        };
        let has_support = composer.contains_key("support");
        match &self.api {
            Api::GitHub { .. } => {
                if !support.contains_key("source") {
                    let label = self.label(io, id)?;
                    let Api::GitHub {
                        owner,
                        repo,
                        origin,
                        ..
                    } = &self.api
                    else {
                        unreachable!()
                    };
                    support.insert(
                        "source".into(),
                        Value::String(format!("https://{origin}/{owner}/{repo}/tree/{label}")),
                    );
                }
                let Api::GitHub {
                    owner,
                    repo,
                    origin,
                    issues,
                    archived,
                    ..
                } = &self.api
                else {
                    unreachable!()
                };
                if !support.contains_key("issues") && *issues {
                    support.insert(
                        "issues".into(),
                        Value::String(format!("https://{origin}/{owner}/{repo}/issues")),
                    );
                }
                let archived = *archived;
                composer.insert("support".into(), Value::Object(support));
                if !composer.contains_key("abandoned") && archived {
                    composer.insert("abandoned".into(), Value::Bool(true));
                }
                if !composer.contains_key("funding")
                    && let Some(funding) = self.github_funding(io)
                {
                    composer.insert("funding".into(), funding);
                }
            }
            Api::GitLab { project, .. } => {
                let web_url = project
                    .get("web_url")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let issues_enabled = truthy(project.get("issues_enabled"));
                let archived = truthy(project.get("archived"));
                if !support.contains_key("source")
                    && let Some(web) = &web_url
                {
                    let label = self.label(io, id)?;
                    support.insert(
                        "source".into(),
                        Value::String(format!("{web}/-/tree/{label}")),
                    );
                }
                if !support.contains_key("issues")
                    && issues_enabled
                    && let Some(web) = &web_url
                {
                    support.insert("issues".into(), Value::String(format!("{web}/-/issues")));
                }
                if has_support || !support.is_empty() {
                    composer.insert("support".into(), Value::Object(support));
                }
                if !composer.contains_key("abandoned") && archived {
                    composer.insert("abandoned".into(), Value::Bool(true));
                }
            }
            Api::Bitbucket { owner, repo, data } => {
                let (owner, repo) = (owner.clone(), repo.clone());
                let data = data.clone().unwrap_or(Value::Null);
                if !support.contains_key("source") {
                    let label = self.label(io, id)?;
                    let hash = self
                        .tags(io)?
                        .into_iter()
                        .chain(self.branches(io)?)
                        .find(|(name, _)| *name == label)
                        .map(|(_, h)| h);
                    let url = match hash {
                        Some(h) => {
                            format!("https://bitbucket.org/{owner}/{repo}/src/{h}/?at={label}")
                        }
                        None => format!("https://bitbucket.org/{owner}/{repo}/src"),
                    };
                    support.insert("source".into(), Value::String(url));
                }
                if !support.contains_key("issues") && truthy(data.get("has_issues")) {
                    support.insert(
                        "issues".into(),
                        Value::String(format!("https://bitbucket.org/{owner}/{repo}/issues")),
                    );
                }
                composer.insert("support".into(), Value::Object(support));
                if !composer.contains_key("homepage") {
                    let website = s(&data, &["website"]);
                    let homepage = if website.is_empty() {
                        s(&data, &["links", "html", "href"])
                    } else {
                        website
                    };
                    composer.insert("homepage".into(), Value::String(homepage));
                }
            }
            Api::Git => {}
        }
        Ok(())
    }

    fn source(&mut self, id: &str) -> Value {
        if let Some(g) = &self.git {
            return g.source(id);
        }
        match &self.api {
            Api::GitHub {
                owner,
                repo,
                origin,
                private,
                ..
            } => {
                let url = if *private {
                    self.github_ssh_url()
                } else {
                    format!("https://{origin}/{owner}/{repo}.git")
                };
                source("git", &url, id)
            }
            Api::GitLab {
                project,
                protocol,
                private,
                ..
            } => {
                let key = match protocol {
                    Some(p) => format!("{p}_url_to_repo"),
                    None if *private => "ssh_url_to_repo".into(),
                    None => "http_url_to_repo".into(),
                };
                source("git", &s(project, &[&key]), id)
            }
            Api::Bitbucket { data, .. } => {
                let data = data.clone().unwrap_or(Value::Null);
                source(&s(&data, &["scm"]), &bitbucket_clone_url(&data), id)
            }
            Api::Git => Value::Null,
        }
    }

    fn dist(&self, id: &str) -> Option<Value> {
        match &self.api {
            Api::GitHub { owner, repo, .. } => Some(zip_dist(
                format!("{}/repos/{owner}/{repo}/zipball/{id}", self.github_api()),
                id,
            )),
            Api::GitLab { .. } => Some(zip_dist(
                format!("{}/repository/archive.zip?sha={id}", self.gitlab_api()),
                id,
            )),
            Api::Bitbucket { owner, repo, .. } if self.git.is_none() => Some(zip_dist(
                format!("https://bitbucket.org/{owner}/{repo}/get/{id}.zip"),
                id,
            )),
            _ => None,
        }
    }
}

/// `parseCloneUrls`: the https clone link without its user.
fn bitbucket_clone_url(data: &Value) -> String {
    let mut url = String::new();
    let user = Regex::new(r"https://([^@]+@)?").unwrap();
    for link in data["links"]["clone"].as_array().into_iter().flatten() {
        if s(link, &["name"]) == "https" {
            url = user.replace(&s(link, &["href"]), "https://").into_owned();
        }
    }
    url
}

/// `GitLabDriver::urlEncodeAll`
fn url_encode_all(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `GitLabDriver::determineOrigin`
fn gitlab_origin(
    config: &Config,
    domain: &str,
    parts: &mut Vec<&str>,
    port: Option<&str>,
) -> Option<String> {
    let domains = config.domains("gitlab-domains");
    let mut guessed = domain.to_lowercase();
    let with_port = port.map(|p| format!("{guessed}:{p}"));
    if domains.contains(&guessed) || with_port.as_ref().is_some_and(|w| domains.contains(w)) {
        return Some(with_port.unwrap_or(guessed));
    }
    if let Some(p) = port {
        guessed = format!("{guessed}:{p}");
    }
    let port_re = Regex::new(r":\d+").unwrap();
    while !parts.is_empty() {
        let part = parts.remove(0);
        guessed = format!("{guessed}/{part}");
        let without_port = port_re.replace(&guessed, "").into_owned();
        if domains.contains(&guessed) || (port.is_some() && domains.contains(&without_port)) {
            return Some(guessed);
        }
    }
    None
}

/// The FUNDING.yml reader of `GitHubDriver::getFundingInfo`.
fn parse_funding(io: &mut Io, text: &str) -> Option<Value> {
    let trim = |s: &str| {
        s.trim_matches(|c| c == '"' || c == '\'' || c == ' ')
            .to_string()
    };
    let key_value = Regex::new(r"^(\w+)\s*:\s*(.+)$").unwrap();
    let inline_list = Regex::new(r"^\[(.*?)\](?:\s*#.*)?$").unwrap();
    let scalar = Regex::new(r"^([^#].*?)(?:\s+#.*)?$").unwrap();
    let key_only = Regex::new(r"^(\w+)\s*:\s*#\s*$").unwrap();
    let item = Regex::new(r"^-\s*(.+)(?:\s+#.*)?$").unwrap();
    let item_comma = Regex::new(r"^(.+),(?:\s*#.*)?$").unwrap();
    let list_split = Regex::new(r#"['"]?\s*,\s*['"]?"#).unwrap();
    let has_scheme = Regex::new(r"^[a-zA-Z][a-zA-Z0-9+.-]*:|^//").unwrap();
    let bare_domain = Regex::new(r"^[a-z0-9-]+\.[a-z]{2,3}$").unwrap();
    let mut result: Vec<(String, String)> = Vec::new();
    let mut key: Option<String> = None;
    for line in Regex::new(r"\r?\n").unwrap().split(text) {
        let line = line.trim();
        if let Some(m) = key_value.captures(line) {
            if &m[2] == "[" {
                key = Some(m[1].to_string());
                continue;
            }
            if let Some(m2) = inline_list.captures(&m[2]) {
                for entry in list_split.split(&m2[1]) {
                    result.push((m[1].to_string(), trim(entry.trim())));
                }
            } else if let Some(m2) = scalar.captures(&m[2]) {
                result.push((m[1].to_string(), trim(&m2[1])));
            }
            key = None;
        } else if let Some(m) = key_only.captures(line) {
            key = Some(m[1].to_string());
        } else if let Some(k) = &key
            && let Some(m) = item.captures(line).or_else(|| item_comma.captures(line))
        {
            result.push((k.clone(), trim(&m[1])));
        } else if key.is_some() && line == "]" {
            key = None;
        }
    }
    let basename = |u: &str| {
        u.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string()
    };
    let mut out = Vec::new();
    for (kind, url) in result {
        let url = match kind.as_str() {
            "community_bridge" => format!(
                "https://funding.communitybridge.org/projects/{}",
                basename(&url)
            ),
            "github" => format!("https://github.com/{}", basename(&url)),
            "issuehunt" => format!("https://issuehunt.io/r/{url}"),
            "ko_fi" => format!("https://ko-fi.com/{}", basename(&url)),
            "liberapay" => format!("https://liberapay.com/{}", basename(&url)),
            "open_collective" => format!("https://opencollective.com/{}", basename(&url)),
            "patreon" => format!("https://www.patreon.com/{}", basename(&url)),
            "tidelift" => format!("https://tidelift.com/funding/github/{url}"),
            "polar" => format!("https://polar.sh/{}", basename(&url)),
            "buy_me_a_coffee" => format!("https://www.buymeacoffee.com/{}", basename(&url)),
            "thanks_dev" => format!("https://thanks.dev/{url}"),
            "otechie" => format!("https://otechie.com/{}", basename(&url)),
            "custom" => {
                if has_scheme.is_match(&url) {
                    url
                } else if bare_domain.is_match(&url) {
                    format!("https://{url}")
                } else {
                    io.warning(&format!("Funding URL {url} not in a supported format."));
                    continue;
                }
            }
            _ => url,
        };
        out.push(serde_json::json!({"type": kind, "url": url}));
    }
    // ponytail: PHP keeps the numeric keys after `unset`, so a dropped custom entry would
    // turn Composer's list into an object; romposer keeps a list
    Some(Value::Array(out))
}

/// `VcsRepository::initialize`: every tag and branch with a valid composer.json.
pub fn scan(
    io: &mut Io,
    config: &Config,
    http: &Http,
    repo: &Map<String, Value>,
) -> Result<Scan, String> {
    let url = crate::config::expand_path(repo.get("url").and_then(Value::as_str).unwrap_or(""));
    let kind = repo.get("type").and_then(Value::as_str).unwrap_or("vcs");
    let only_branches = match repo.get("only-branches") {
        None => None,
        Some(Value::Array(list)) if list.iter().all(Value::is_string) => {
            let names: Vec<String> = list
                .iter()
                .filter_map(Value::as_str)
                .map(regex::escape)
                .collect();
            let names: Vec<String> = names.iter().map(|n| n.replace(r"\*", ".*")).collect();
            Some(Regex::new(&format!("^(?:{})$", names.join("|"))).map_err(|e| e.to_string())?)
        }
        Some(_) => {
            return Err(format!(
                "\"only-branches\" key for repository {} should be an array of strings",
                sanitize_url(&url)
            ));
        }
    };
    let mut d = Driver::detect(io, config, http, repo, &url, kind)?;

    let mut package_name: Option<String> = None;
    let mut has_root_composer = false;
    let root = match d.root_identifier(io) {
        Ok(root) => {
            if let Ok(Some(data)) = d.composer_information(io, &root) {
                has_root_composer = true;
                package_name = data
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);
            }
            root
        }
        Err(e) if rethrow(&e) => return Err(e.1),
        Err(_) => String::new(),
    };

    let mut packages: Vec<Map<String, Value>> = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    for (tag, id) in d.tags(io).map_err(|e| e.1)? {
        let tag = tag.replace("release-", "");
        let Ok(parsed) = version_parser::normalize(&tag, None) else {
            continue;
        };
        let mut data = match d.composer_information(io, &id) {
            Ok(Some(data)) => data,
            Ok(None) => continue,
            Err(e) if rethrow(&e) => return Err(e.1),
            Err(_) => continue,
        };
        let normalized = match data.get("version").and_then(Value::as_str) {
            Some(v) => match version_parser::normalize(v, None) {
                Ok(n) => n,
                Err(_) => continue,
            },
            None => {
                data.insert("version".into(), Value::String(tag.clone()));
                parsed.clone()
            }
        };
        let version = data
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        data.insert(
            "version".into(),
            Value::String(DEV_SUFFIX.replace(&version, "").into_owned()),
        );
        let normalized = DEV_MARKERS.replace_all(&normalized, "").into_owned();
        data.insert(
            "version_normalized".into(),
            Value::String(normalized.clone()),
        );
        data.shift_remove("default-branch");
        if normalized != parsed {
            continue;
        }
        let name = package_name
            .clone()
            .or_else(|| data.get("name").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
            .to_lowercase();
        if seen.contains(&(name.clone(), normalized.clone())) {
            continue;
        }
        let data = pre_process(&mut d, data, &id, &package_name);
        if crate::package::load(&data).is_err() {
            continue;
        }
        seen.push((name, normalized));
        packages.push(data);
    }

    let mut branches = d.branches(io).map_err(|e| e.1)?;
    if has_root_composer && let Some(pos) = branches.iter().position(|(b, _)| *b == root) {
        let entry = branches.remove(pos);
        branches.insert(0, entry);
    }
    if let Some(only) = &only_branches {
        branches.retain(|(b, id)| *b == root || *id == root || only.is_match(b));
    }
    for (branch, id) in branches {
        let normalized = version_parser::normalize_branch(&branch);
        if version_parser::parse_constraints(&normalized).is_err() {
            continue;
        }
        let (version, parsed) = if normalized.starts_with("dev-") || normalized == "9999999-dev" {
            (
                format!("dev-{}", branch.replace('#', "+")),
                normalized.replace('#', "+"),
            )
        } else {
            let prefix = if branch.starts_with('v') { "v" } else { "" };
            let pretty = NINES.replace_all(&normalized, ".x");
            (format!("{prefix}{pretty}"), normalized)
        };
        let mut data = match d.composer_information(io, &id) {
            Ok(Some(data)) => data,
            Ok(None) => continue,
            Err(e) if e.0.is_some() => {
                if rethrow(&e) {
                    return Err(e.1);
                }
                continue;
            }
            Err(e) => {
                branch_error(io, &branch, &e.1);
                continue;
            }
        };
        data.insert("version".into(), Value::String(version));
        data.insert("version_normalized".into(), Value::String(parsed));
        data.shift_remove("default-branch");
        if branch == root {
            data.insert("default-branch".into(), Value::Bool(true));
        }
        let data = pre_process(&mut d, data, &id, &package_name);
        match crate::package::load(&data) {
            Ok(_) => packages.push(data),
            Err(e) => branch_error(io, &branch, &e),
        }
    }

    if packages.is_empty() {
        return Err(format!(
            "No valid composer.json was found in any branch or tag of {}, could not load a package from it.",
            sanitize_url(&url)
        ));
    }
    Ok(Scan {
        driver: d.kind,
        packages,
    })
}

fn branch_error(io: &mut Io, branch: &str, message: &str) {
    io.error("");
    io.error(&format!("Skipped branch {branch}, {message}"));
    io.error("");
}

/// `VcsRepository::preProcess`
fn pre_process(
    d: &mut Driver<'_>,
    mut data: Map<String, Value>,
    id: &str,
    package_name: &Option<String>,
) -> Map<String, Value> {
    let name = package_name
        .clone()
        .map(Value::String)
        .or_else(|| data.get("name").cloned())
        .unwrap_or(Value::Null);
    data.insert("name".into(), name);
    data.shift_remove("published-time");
    if data.get("dist").is_none_or(Value::is_null) {
        match d.dist(id) {
            Some(dist) => data.insert("dist".into(), dist),
            None => data.shift_remove("dist"),
        };
    }
    if data.get("source").is_none_or(Value::is_null) {
        data.insert("source".into(), d.source(id));
    }
    let reference = data.get("source").and_then(|s| s.get("reference")).cloned();
    if let Some(Value::Object(dist)) = data.get_mut("dist")
        && !dist.contains_key("reference")
        && let Some(r) = reference
    {
        dist.insert("reference".into(), r);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funding_files_are_read_like_composer() {
        let mut io = Io::buffered();
        let yml = "github: [foo, 'bar']\npatreon: someone # comment\ncustom: [\n  \"https://example.com/donate\",\n  nope,\n]\nko_fi: kofi\n";
        let funding = parse_funding(&mut io, yml).unwrap();
        assert_eq!(
            funding,
            serde_json::json!([
                {"type": "github", "url": "https://github.com/foo"},
                {"type": "github", "url": "https://github.com/bar"},
                {"type": "patreon", "url": "https://www.patreon.com/someone"},
                {"type": "custom", "url": "https://example.com/donate"},
                {"type": "ko_fi", "url": "https://ko-fi.com/kofi"},
            ])
        );
        assert!(
            io.output()
                .contains("Funding URL nope not in a supported format.")
        );
    }

    #[test]
    fn dates_and_encodings() {
        assert_eq!(epoch_rfc3339(1_700_000_000), "2023-11-14T22:13:20+00:00");
        assert_eq!(
            api_date("2024-09-11T13:17:53Z"),
            "2024-09-11T13:17:53+00:00"
        );
        assert_eq!(
            api_date("2024-01-01T10:00:00.000+01:00"),
            "2024-01-01T10:00:00+01:00"
        );
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
        assert_eq!(url_encode_all("a.b/c-d_e"), "a%2Eb%2Fc-d_e");
        assert!(
            cache_dir(
                &crate::config::Config::in_memory(),
                &["github.com", "..", "x"]
            )
            .is_none()
        );
    }
}
