//! Port of `FileDownloader` + `ZipDownloader` + `ArchiveDownloader` for zip
//! dists: parallel HTTP downloads with Composer's auth rules, the files cache
//! at Composer's path, checksum verification, extraction and single-root
//! unwrapping.

use crate::config::{Auth, Config};
use crate::fs::{empty_directory, ensure_dir, is_dir_empty, normalize_path, realpath, remove};
use crate::io::Io;
use crate::package::Package;
use regex::Regex;
use sha1::Digest;

fn sha1_hex(bytes: &[u8]) -> String {
    sha1::Sha1::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static METADATA_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\._.*|\.DS_Store|__MACOSX|\.AppleDouble|\.fseventsd|\.Spotlight-V100|\.Trashes|(?i:thumbs\.db|desktop\.ini))$").unwrap()
});

pub struct Downloader {
    config: Config,
    vendor_dir: String,
    cache_dir: Option<String>,
    cache_read_only: bool,
    write_cache: bool,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    last_cache_writes: HashMap<String, String>,
}

/// `Url::getOrigin`
pub fn origin(config: &Config, url: &str) -> String {
    if url.starts_with("file://") {
        return url.to_string();
    }
    let parsed = reqwest::Url::parse(url).ok();
    let mut origin = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .unwrap_or("")
        .to_string();
    if let Some(port) = parsed.as_ref().and_then(|u| u.port()) {
        origin.push_str(&format!(":{port}"));
    }
    if origin.ends_with(".github.com") && origin != "codeload.github.com" {
        return "github.com".into();
    }
    if origin == "repo.packagist.org" {
        return "packagist.org".into();
    }
    if origin.is_empty() {
        origin = url.to_string();
    }
    if !origin.contains('/') {
        let gitlab = config.domains("gitlab-domains");
        if !gitlab.contains(&origin) {
            for domain in gitlab {
                if domain.starts_with(&format!("{origin}/"))
                    || domain.split_once(':').is_some_and(|(h, _)| h == origin)
                {
                    return domain;
                }
            }
        }
    }
    origin
}

/// `AuthHelper::applyAuthentication` reduced to the headers it adds.
pub fn auth_headers(
    config: &Config,
    origin: &str,
    url: &str,
    auth: &Auth,
) -> Vec<(String, String)> {
    use base64_encode as b64;
    if auth.password == "bearer" {
        return vec![("Authorization".into(), format!("Bearer {}", auth.username))];
    }
    if auth.password == "custom-headers" {
        return serde_json::from_str::<serde_json::Value>(&auth.username)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|h| {
                h.as_str()?
                    .split_once(':')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            })
            .collect();
    }
    if origin == "github.com" && auth.password == "x-oauth-basic" {
        if !url.starts_with("https://api.github.com/") && !url.starts_with("http://api.github.com/")
        {
            return vec![];
        }
        return vec![("Authorization".into(), format!("token {}", auth.username))];
    }
    let gitlab_marker = auth.username == "gitlab-ci-token"
        || matches!(
            auth.password.as_str(),
            "gitlab-ci-token" | "oauth2" | "private-token"
        );
    if gitlab_marker
        && config
            .domains("gitlab-domains")
            .contains(&origin.to_string())
    {
        if auth.username == "gitlab-ci-token" || auth.password == "gitlab-ci-token" {
            let token = if auth.password == "gitlab-ci-token" {
                &auth.username
            } else {
                &auth.password
            };
            return vec![("JOB-TOKEN".into(), token.clone())];
        }
        if auth.password == "oauth2" {
            return vec![("Authorization".into(), format!("Bearer {}", auth.username))];
        }
        if auth.password == "private-token" {
            return vec![("PRIVATE-TOKEN".into(), auth.username.clone())];
        }
    }
    if origin == "bitbucket.org" && auth.username == "x-token-auth" {
        return vec![("Authorization".into(), format!("Bearer {}", auth.password))];
    }
    if auth.username == "client-certificate" {
        return vec![];
    }
    vec![(
        "Authorization".into(),
        format!(
            "Basic {}",
            b64(&format!("{}:{}", auth.username, auth.password))
        ),
    )]
}

/// `Url::sanitize`: credentials and access tokens never reach messages.
pub fn sanitize_url(url: &str) -> String {
    let url = Regex::new(r"([&?]access_token=)[^&]+")
        .unwrap()
        .replace_all(url, "$1***")
        .into_owned();
    let re = Regex::new(r"(?i)(?:(?P<prefix>[a-z0-9][a-z0-9+.-]*://)|^)(?P<user>[^:/\s?#]*)(?::(?P<password>[^\s/?#]*))?@").unwrap();
    re.replace_all(&url, |caps: &regex::Captures<'_>| {
        let prefix = caps.name("prefix").map_or("", |m| m.as_str());
        let user = caps.name("user").map_or("", |m| m.as_str());
        let user = if matches!(
            user,
            "x-token-auth" | "gitlab-ci-token" | "oauth2" | "private-token" | "x-oauth-basic"
        ) {
            user.to_string()
        } else if user.len() >= 12
            || Regex::new(r"^gh[a-z]_[a-zA-Z0-9_]+$")
                .unwrap()
                .is_match(user)
        {
            format!("{}***", &user[..user.len().min(3)])
        } else {
            user.to_string()
        };
        if caps
            .name("password")
            .is_some_and(|p| !p.as_str().is_empty())
        {
            format!("{prefix}{user}:***@")
        } else {
            format!("{prefix}{user}@")
        }
    })
    .into_owned()
}

fn base64_encode(input: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `ComposerMirror::processUrl`
fn process_mirror_url(mirror: &str, package: &Package) -> String {
    let reference = package
        .dist
        .as_ref()
        .and_then(|d| d.reference.clone())
        .map(|r| {
            if Regex::new(r"^([a-f0-9]*|%reference%)$")
                .unwrap()
                .is_match(&r)
            {
                r
            } else {
                format!("{:x}", md5::compute(&r))
            }
        });
    let version = if package.version.contains('/') {
        format!("{:x}", md5::compute(&package.version))
    } else {
        package.version.clone()
    };
    mirror
        .replace("%package%", &package.name)
        .replace("%version%", &version)
        .replace("%reference%", reference.as_deref().unwrap_or(""))
        .replace(
            "%type%",
            package.dist.as_ref().map_or("", |d| d.kind.as_str()),
        )
        .replace("%prettyVersion%", &package.pretty_version)
}

/// `Package::getDistUrls`
pub fn dist_urls(package: &Package) -> Vec<String> {
    let Some(dist) = &package.dist else {
        return vec![];
    };
    if dist.url.is_empty() {
        return vec![];
    }
    let url = if dist.url.contains('%') {
        process_mirror_url(&dist.url, package)
    } else {
        dist.url.clone()
    };
    let mut urls = vec![url];
    if let Some(serde_json::Value::Array(mirrors)) = &dist.mirrors {
        for mirror in mirrors {
            let Some(mirror_url) = mirror.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let processed = process_mirror_url(mirror_url, package);
            if !urls.contains(&processed) {
                if mirror
                    .get("preferred")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    urls.insert(0, processed);
                } else {
                    urls.push(processed);
                }
            }
        }
    }
    urls
}

/// `Url::updateDistReference`
pub fn update_dist_reference(config: &Config, url: &str, reference: &str) -> String {
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let re = |p: &str| Regex::new(p).unwrap();
    if matches!(
        host.as_str(),
        "api.github.com" | "github.com" | "www.github.com"
    ) {
        if let Some(m) =
            re(r"(?i)^https?://(?:www\.)?github\.com/([^/]+)/([^/]+)/(zip|tar)ball/(.+)$")
                .captures(url)
        {
            return format!(
                "https://api.github.com/repos/{}/{}/{}ball/{reference}",
                &m[1], &m[2], &m[3]
            );
        }
        if let Some(m) = re(
            r"(?i)^https?://(?:www\.)?github\.com/([^/]+)/([^/]+)/archive/.+\.(zip|tar)(?:\.gz)?$",
        )
        .captures(url)
        {
            return format!(
                "https://api.github.com/repos/{}/{}/{}ball/{reference}",
                &m[1], &m[2], &m[3]
            );
        }
        if let Some(m) =
            re(r"(?i)^https?://api\.github\.com/repos/([^/]+)/([^/]+)/(zip|tar)ball(?:/.+)?$")
                .captures(url)
        {
            return format!(
                "https://api.github.com/repos/{}/{}/{}ball/{reference}",
                &m[1], &m[2], &m[3]
            );
        }
    } else if matches!(host.as_str(), "bitbucket.org" | "www.bitbucket.org") {
        if let Some(m) = re(r"(?i)^https?://(?:www\.)?bitbucket\.org/([^/]+)/([^/]+)/get/(.+)\.(zip|tar\.gz|tar\.bz2)$").captures(url) {
            return format!("https://bitbucket.org/{}/{}/get/{reference}.{}", &m[1], &m[2], &m[4]);
        }
    } else if matches!(host.as_str(), "gitlab.com" | "www.gitlab.com") {
        if let Some(m) = re(r"(?i)^https?://(?:www\.)?gitlab\.com/api/v[34]/projects/([^/]+)/repository/archive\.(zip|tar\.gz|tar\.bz2|tar)\?sha=.+$").captures(url) {
            return format!("https://gitlab.com/api/v4/projects/{}/repository/archive.{}?sha={reference}", &m[1], &m[2]);
        }
    } else if config.domains("github-domains").contains(&host) {
        return re(r"(?i)(/repos/[^/]+/[^/]+/(zip|tar)ball)(?:/.+)?$")
            .replace(url, format!("${{1}}/{reference}"))
            .into_owned();
    } else if config.domains("gitlab-domains").contains(&host) {
        return re(r"(?i)(/api/v[34]/projects/[^/]+/repository/archive\.(?:zip|tar\.gz|tar\.bz2|tar)\?sha=).+$")
            .replace(url, format!("${{1}}{reference}"))
            .into_owned();
    }
    url.to_string()
}

fn cache_key(package: &Package, processed_url: &str) -> String {
    let hash = sha1_hex(processed_url.as_bytes());
    let key = format!(
        "{}/{}.{}",
        package.name,
        hash,
        package.dist.as_ref().map_or("", |d| d.kind.as_str())
    );
    Regex::new(r"(?i)[^a-z0-9_./]")
        .unwrap()
        .replace_all(&key, "-")
        .into_owned()
}

/// PATHINFO_EXTENSION / BASENAME of the dist URL path.
fn dist_path(package: &Package, extension: bool) -> String {
    let url = package
        .dist
        .as_ref()
        .map_or("", |d| d.url.as_str())
        .replace('\\', "/");
    let path = reqwest::Url::parse(&url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.split(['?', '#']).next().unwrap_or("").to_string());
    let base = path.rsplit('/').next().unwrap_or("").to_string();
    if extension {
        match base.rfind('.') {
            Some(i) if i > 0 => base[i + 1..].to_string(),
            _ => String::new(),
        }
    } else {
        base
    }
}

fn sha1_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(sha1_hex(&bytes))
}

pub struct Downloaded {
    pub archive: PathBuf,
}

impl Downloader {
    pub fn new(config: &Config) -> Result<Downloader, String> {
        let cache_dir = if config.int("cache-files-ttl") > 0 {
            let dir = config.get_str("cache-files-dir");
            let usable = !Regex::new(r"(^|[\\/])(\$null|nul|NUL|/dev/null)([\\/]|$)")
                .unwrap()
                .is_match(&dir);
            if usable { Some(dir) } else { None }
        } else {
            None
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!(
                "Composer/{} (romposer; {})",
                crate::COMPOSER_VERSION,
                std::env::consts::OS
            ))
            .build()
            .map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Downloader {
            config: config.clone(),
            vendor_dir: config.vendor_dir(),
            cache_dir,
            cache_read_only: config.bool("cache-read-only"),
            write_cache: true,
            client,
            runtime,
            last_cache_writes: HashMap::new(),
        })
    }

    /// When false the zip cache is read but never written (the store holds the files instead).
    pub fn set_write_cache(&mut self, write: bool) {
        self.write_cache = write;
    }

    fn cache_path(&self, key: &str) -> Option<PathBuf> {
        self.cache_dir.as_ref().map(|d| Path::new(d).join(key))
    }

    /// `FileDownloader::getFileName`
    pub fn file_name(&self, package: &Package) -> PathBuf {
        let mut ext = dist_path(package, true);
        if ext.is_empty() {
            ext = package
                .dist
                .as_ref()
                .map_or(String::new(), |d| d.kind.clone());
        }
        let name = format!(
            "{}/composer/tmp-{:x}.{ext}",
            self.vendor_dir,
            md5::compute(package.unique_name())
        );
        PathBuf::from(name.trim_end_matches('.'))
    }

    /// `FileDownloader::download` for every package, in parallel, archives landing in `vendor/composer/tmp-*`.
    pub fn download_all(
        &mut self,
        io: &mut Io,
        packages: &[&Package],
    ) -> Result<HashMap<String, Downloaded>, String> {
        ensure_dir(Path::new(&format!("{}/composer", self.vendor_dir)))?;
        let mut jobs = Vec::new();
        let mut results: HashMap<String, Downloaded> = HashMap::new();
        for package in packages {
            let dist = package
                .dist
                .as_ref()
                .ok_or_else(|| "The given package is missing url information".to_string())?;
            if dist.url.is_empty() {
                return Err("The given package is missing url information".into());
            }
            let file_name = self.file_name(package);
            let urls: Vec<(String, String, String)> = dist_urls(package)
                .into_iter()
                .map(|url| {
                    let processed = match &dist.reference {
                        Some(r) => update_dist_reference(&self.config, &url, r),
                        None => url.clone(),
                    };
                    let key = cache_key(package, &processed);
                    (url, processed, key)
                })
                .collect();
            let checksum = dist.shasum.clone().filter(|s| !s.is_empty());

            // use from cache if present and the checksum matches (or there is none)
            let mut from_cache = false;
            if let Some(cache) = self.cache_path(&urls[0].2)
                && cache.is_file()
                && checksum
                    .as_deref()
                    .is_none_or(|c| sha1_file(&cache).as_deref() == Some(c))
            {
                std::fs::copy(&cache, &file_name).map_err(|e| e.to_string())?;
                let now = std::time::SystemTime::now();
                let _ = std::fs::File::open(&cache).and_then(|f| f.set_modified(now));
                if io.verbosity >= 2 {
                    io.error(&format!(
                        "  - Loading {} ({}) from cache",
                        package.name,
                        package.full_pretty_version()
                    ));
                }
                if !self.cache_read_only {
                    self.last_cache_writes
                        .insert(package.name.clone(), urls[0].2.clone());
                }
                from_cache = true;
            }
            if from_cache {
                results.insert(package.unique_name(), Downloaded { archive: file_name });
                continue;
            }
            io.error(&format!(
                "  - Downloading {} ({})",
                package.name,
                package.full_pretty_version()
            ));
            jobs.push(((*package).clone(), file_name, urls, checksum));
        }

        if !jobs.is_empty() {
            let client = self.client.clone();
            let config = self.config.clone();
            let cache_dir = self
                .cache_dir
                .clone()
                .filter(|_| !self.cache_read_only && self.write_cache);
            let outcomes = self.runtime.block_on(async move {
                let mut handles = Vec::new();
                for (package, file_name, urls, checksum) in jobs {
                    let client = client.clone();
                    let config = config.clone();
                    let cache_dir = cache_dir.clone();
                    handles.push(tokio::spawn(async move {
                        let res = fetch_with_retries(
                            &client,
                            &config,
                            &package,
                            &file_name,
                            &urls,
                            checksum.as_deref(),
                            cache_dir.as_deref(),
                        )
                        .await;
                        (package.unique_name(), package.name.clone(), file_name, res)
                    }));
                }
                let mut out = Vec::new();
                for h in handles {
                    out.push(h.await.map_err(|e| e.to_string()));
                }
                out
            });
            for outcome in outcomes {
                let (unique, name, file_name, res) = outcome?;
                match res {
                    Ok(cache_key) => {
                        if let Some(k) = cache_key {
                            self.last_cache_writes.insert(name, k);
                        }
                        results.insert(unique, Downloaded { archive: file_name });
                    }
                    Err(e) => {
                        let _ = remove(&file_name);
                        return Err(e);
                    }
                }
            }
        }
        Ok(results)
    }

    /// `ArchiveDownloader::install` + `ZipDownloader::extract`: extract into a temp
    /// dir under vendor/composer, unwrap a single top-level directory, move into place.
    pub fn install_archive(
        &mut self,
        package: &Package,
        archive: &Path,
        path: &Path,
    ) -> Result<(), String> {
        let vendor = normalize_path(&self.vendor_dir);
        let path_str = normalize_path(&path.to_string_lossy());
        if !vendor.starts_with(&format!("{path_str}/")) {
            empty_directory(path)?;
        }
        let mut temporary_dir;
        loop {
            temporary_dir = PathBuf::from(format!(
                "{}/composer/{}",
                self.vendor_dir,
                crate::fs::random_hex(4)
            ));
            if !temporary_dir.is_dir() {
                break;
            }
        }
        ensure_dir(&temporary_dir)?;
        let result = extract_zip(archive, &temporary_dir).and_then(|()| {
            let _ = remove(archive);
            // if the target is clear, the whole package is renamed in one go
            let rename_as_one =
                !path.exists() || (is_dir_empty(path) && std::fs::remove_dir(path).is_ok());
            let content: Vec<PathBuf> = std::fs::read_dir(&temporary_dir)
                .map_err(|e| e.to_string())?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    !p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| METADATA_NAME.is_match(n))
                })
                .collect();
            let single_dir = content.len() == 1 && content[0].is_dir();
            if rename_as_one {
                let extracted = if single_dir {
                    content[0].clone()
                } else {
                    temporary_dir.clone()
                };
                rename(&extracted, path)?;
            } else {
                let from = if single_dir {
                    content[0].clone()
                } else {
                    temporary_dir.clone()
                };
                rename_recursively(package, &from, path)?;
            }
            let _ = remove(&temporary_dir);
            Ok(())
        });
        if let Err(e) = result {
            if let Some(key) = self.last_cache_writes.remove(&package.name)
                && let Some(cache) = self.cache_path(&key)
            {
                let _ = remove(&cache);
            }
            let _ = remove(&temporary_dir);
            if path.is_dir() && realpath(path) != crate::fs::cwd() {
                let _ = remove(path);
            }
            return Err(e);
        }
        Ok(())
    }

    /// `FileDownloader::cleanup`: drop the archive and empty directories left behind.
    pub fn cleanup(&self, package: &Package, path: &Path) {
        let file_name = self.file_name(package);
        let _ = remove(&file_name);
        let vendor = PathBuf::from(&self.vendor_dir);
        let vendor_part = package.pretty_name.split('/').next().unwrap_or("");
        for dir in [
            path.to_path_buf(),
            vendor.join(vendor_part),
            vendor.join("composer"),
            vendor.clone(),
        ] {
            if dir.is_dir() && is_dir_empty(&dir) && realpath(&dir) != crate::fs::cwd() {
                let _ = std::fs::remove_dir(&dir);
            }
        }
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        ensure_dir(parent)?;
    }
    std::fs::rename(from, to).map_err(|e| {
        format!(
            "Could not rename {} to {}: {e}",
            from.display(),
            to.display()
        )
    })
}

fn rename_recursively(package: &Package, from: &Path, to: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(from)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let file = entry.path();
        let target = to.join(entry.file_name());
        if target.is_dir() {
            if !file.is_dir() {
                return Err(format!(
                    "Installing {} would lead to overwriting the {} directory with a file from the package, invalid operation.",
                    package.unique_name(),
                    target.display()
                ));
            }
            rename_recursively(package, &file, &target)?;
        } else {
            rename(&file, &target)?;
        }
    }
    Ok(())
}

/// What `unzip -qq` leaves on disk: entries with their stored unix modes, symlinks restored.
/// Regular entries are written first and symlinks last, so no entry is ever written
/// through a link, and a link may only point inside the extraction root.
pub fn extract_zip(archive: &Path, target: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| format!("Failed to extract {}: {e}", archive.display()))?;
    let umask = crate::binaries::umask();
    let mut symlinks: Vec<(PathBuf, String)> = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(format!(
                "Failed to extract {}: invalid entry name {:?}",
                archive.display(),
                entry.name()
            ));
        };
        let out = target.join(rel);
        let mode = entry.unix_mode();
        if entry.is_dir() {
            ensure_dir(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            ensure_dir(parent)?;
        }
        if mode.is_some_and(|m| m & 0o170000 == 0o120000) {
            let mut link = String::new();
            std::io::Read::read_to_string(&mut entry, &mut link).map_err(|e| e.to_string())?;
            symlinks.push((out, link));
            continue;
        }
        let mut dest = std::fs::File::create(&out)
            .map_err(|e| format!("Could not create {}: {e}", out.display()))?;
        std::io::copy(&mut entry, &mut dest).map_err(|e| e.to_string())?;
        let perm = match mode {
            Some(m) if m & 0o777 != 0 => m & 0o777,
            _ => 0o666 & !umask,
        };
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(perm));
    }
    // links are only ever relative and downward: no absolute target, no ".." component,
    // and once created each one must still resolve inside the extraction root
    let root = std::fs::canonicalize(target).map_err(|e| e.to_string())?;
    for (out, link) in &symlinks {
        let escapes = Path::new(link).is_absolute()
            || Path::new(link)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        if escapes {
            return Err(format!(
                "Failed to extract {}: symlink {} points outside of the package ({link})",
                archive.display(),
                out.display()
            ));
        }
        let _ = std::fs::remove_file(out);
        std::os::unix::fs::symlink(link, out).map_err(|e| e.to_string())?;
    }
    for (out, link) in &symlinks {
        let inside = std::fs::canonicalize(out).is_ok_and(|p| p.starts_with(&root));
        if !inside {
            let _ = std::fs::remove_file(out);
            return Err(format!(
                "Failed to extract {}: symlink {} points outside of the package ({link})",
                archive.display(),
                out.display()
            ));
        }
    }
    Ok(())
}

async fn fetch_with_retries(
    client: &reqwest::Client,
    config: &Config,
    package: &Package,
    file_name: &Path,
    urls: &[(String, String, String)],
    checksum: Option<&str>,
    cache_dir: Option<&str>,
) -> Result<Option<String>, String> {
    let mut last_err = String::new();
    for (index, (base, processed, key)) in urls.iter().enumerate() {
        let mut retries = 3;
        loop {
            match fetch_once(client, config, processed, file_name).await {
                Ok(()) => {
                    // a truncated download lacks the end of central directory record
                    let bytes = std::fs::read(file_name).map_err(|e| e.to_string())?;
                    let tail_start = bytes.len().saturating_sub(22 + 65535);
                    let tail = &bytes[tail_start..];
                    if !tail.windows(4).any(|w| w == b"PK\x05\x06") {
                        last_err = format!(
                            "The downloaded archive for {} is truncated or corrupt, zip end of central directory not found (file size: {} bytes)",
                            package.name,
                            bytes.len()
                        );
                        let _ = std::fs::remove_file(file_name);
                        if retries > 0 && processed.starts_with("http") {
                            retries -= 1;
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        }
                        break;
                    }
                    if let Some(c) = checksum
                        && sha1_hex(&bytes) != c
                    {
                        return Err(format!(
                            "The checksum verification of the file failed (downloaded from {base})"
                        ));
                    }
                    let mut written = None;
                    if let Some(dir) = cache_dir {
                        let cache = Path::new(dir).join(key);
                        if let Some(parent) = cache.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if std::fs::copy(file_name, &cache).is_ok() {
                            written = Some(key.clone());
                        }
                    }
                    return Ok(written);
                }
                Err((status, msg)) => {
                    last_err = msg;
                    let _ = std::fs::remove_file(file_name);
                    let retryable = status.is_none_or(|s| matches!(s, 500 | 502 | 503 | 504));
                    if retryable && retries > 0 {
                        retries -= 1;
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                    break;
                }
            }
        }
        if index + 1 < urls.len() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        }
    }
    Err(last_err)
}

/// One request with Composer's redirect handling: at most 20 hops, auth
/// recomputed for each hop's origin, plain http refused under secure-http.
async fn fetch_once(
    client: &reqwest::Client,
    config: &Config,
    url: &str,
    file_name: &Path,
) -> Result<(), (Option<u16>, String)> {
    let mut url = url.to_string();
    for _ in 0..=20 {
        if url.starts_with("http://") && config.bool("secure-http") {
            return Err((
                None,
                format!(
                    "Your configuration does not allow connections to {}. See https://getcomposer.org/doc/06-config.md#secure-http for details.",
                    sanitize_url(&url)
                ),
            ));
        }
        let mut request = client.get(&url);
        let origin = origin(config, &url);
        if let Some(auth) = config.authentication(&origin) {
            for (k, v) in auth_headers(config, &origin, &url, &auth) {
                request = request.header(k, v);
            }
        }
        let response = request.send().await.map_err(|e| {
            (
                None,
                format!(
                    "curl error while downloading {}: {}",
                    sanitize_url(&url),
                    e.without_url()
                ),
            )
        })?;
        let status = response.status();
        if status.is_redirection()
            && let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
        {
            let base = reqwest::Url::parse(&url).map_err(|e| (None, e.to_string()))?;
            let next = base.join(location).map_err(|e| (None, e.to_string()))?;
            url = next.to_string();
            continue;
        }
        if !status.is_success() {
            let version = match response.version() {
                reqwest::Version::HTTP_2 => "HTTP/2".to_string(),
                reqwest::Version::HTTP_11 => "HTTP/1.1".to_string(),
                v => format!("{v:?}"),
            };
            return Err((
                Some(status.as_u16()),
                format!(
                    "The \"{}\" file could not be downloaded ({version} {} returned)",
                    sanitize_url(&url),
                    status.as_u16()
                ),
            ));
        }
        let bytes = response.bytes().await.map_err(|e| {
            (
                None,
                format!(
                    "curl error while downloading {}: {}",
                    sanitize_url(&url),
                    e.without_url()
                ),
            )
        })?;
        std::fs::write(file_name, &bytes).map_err(|e| (None, format!("{} could not be saved to {}, make sure the directory is writable and you have internet connectivity ({e})", sanitize_url(&url), file_name.display())))?;
        return Ok(());
    }
    Err((
        None,
        format!(
            "The \"{}\" file could not be downloaded: too many redirects",
            sanitize_url(&url)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_dist_urls_are_rewritten_to_the_api_with_the_reference() {
        let config = Config {
            config: serde_json::Map::new(),
            repositories: vec![],
            base_dir: None,
        };
        assert_eq!(
            update_dist_reference(
                &config,
                "https://api.github.com/repos/php-fig/log/zipball/old",
                "abc"
            ),
            "https://api.github.com/repos/php-fig/log/zipball/abc"
        );
        assert_eq!(
            update_dist_reference(
                &config,
                "https://github.com/foo/bar/archive/v1.0.zip",
                "abc"
            ),
            "https://api.github.com/repos/foo/bar/zipball/abc"
        );
        assert_eq!(
            update_dist_reference(&config, "https://example.com/a.zip", "abc"),
            "https://example.com/a.zip"
        );
    }

    #[test]
    fn cache_keys_and_file_names_follow_composer() {
        let v: serde_json::Value = serde_json::json!({"name":"Psr/Log","version":"3.0.2","dist":{"type":"zip","url":"https://api.github.com/repos/php-fig/log/zipball/f16e1d5863e37f8d8c2a01719f5b34baa2b714d3","reference":"f16e1d5863e37f8d8c2a01719f5b34baa2b714d3"}});
        let p = crate::package::load(v.as_object().unwrap()).unwrap();
        let processed = dist_urls(&p)[0].clone();
        assert_eq!(
            cache_key(&p, &processed),
            format!("psr/log/{}.zip", sha1_hex(processed.as_bytes()))
        );
        assert_eq!(dist_path(&p, true), "");
        assert_eq!(base64_encode("user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn urls_are_sanitised_in_messages() {
        assert_eq!(
            sanitize_url("https://user:secret@example.com/a.zip"),
            "https://user:***@example.com/a.zip"
        );
        assert_eq!(
            sanitize_url("https://ghp_abcdefghijklmnop:x-oauth-basic@github.com/x"),
            "https://ghp***:***@github.com/x"
        );
        assert_eq!(
            sanitize_url("https://api.github.com/x?access_token=abc&b=1"),
            "https://api.github.com/x?access_token=***&b=1"
        );
        assert_eq!(
            sanitize_url("https://example.com/plain.zip"),
            "https://example.com/plain.zip"
        );
    }

    #[test]
    fn origins_and_auth_headers() {
        let mut config = Config {
            config: serde_json::Map::new(),
            repositories: vec![],
            base_dir: None,
        };
        config
            .config
            .insert("gitlab-domains".into(), serde_json::json!(["gitlab.com"]));
        config
            .config
            .insert("github-domains".into(), serde_json::json!(["github.com"]));
        assert_eq!(
            origin(&config, "https://api.github.com/repos/x/y/zipball/abc"),
            "github.com"
        );
        assert_eq!(
            origin(&config, "https://repo.packagist.org/p2/x.json"),
            "packagist.org"
        );
        assert_eq!(
            origin(&config, "https://example.com:8080/a.zip"),
            "example.com:8080"
        );
        let gh = Auth {
            username: "tok".into(),
            password: "x-oauth-basic".into(),
        };
        assert_eq!(
            auth_headers(&config, "github.com", "https://api.github.com/x", &gh),
            vec![("Authorization".to_string(), "token tok".to_string())]
        );
        assert_eq!(
            auth_headers(&config, "github.com", "https://codeload.github.com/x", &gh),
            vec![]
        );
        let basic = Auth {
            username: "u".into(),
            password: "p".into(),
        };
        assert_eq!(
            auth_headers(&config, "example.com", "https://example.com", &basic),
            vec![("Authorization".to_string(), "Basic dTpw".to_string())]
        );
        let gl = Auth {
            username: "t".into(),
            password: "private-token".into(),
        };
        assert_eq!(
            auth_headers(&config, "gitlab.com", "https://gitlab.com", &gl),
            vec![("PRIVATE-TOKEN".to_string(), "t".to_string())]
        );
    }
}
