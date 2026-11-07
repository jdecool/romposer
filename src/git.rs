//! Port of Composer's `Util\Git` (git run as a process, credentials retried from the
//! configuration, bare mirrors in `cache-vcs-dir`) and `GitDownloader` (source installs:
//! a clone with its `.git` in `vendor/<name>`).
//!
//! git always gets an argument vector, never a shell string. URLs from metadata come
//! after `--`, references starting with `-` are refused, and `GIT_ALLOW_PROTOCOL` keeps
//! git's `ext::`/`fd::` transports (which run commands) off.

use crate::config::Config;
use crate::download::sanitize_url;
use crate::io::Io;
use crate::package::Package;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// One git process with `Git::cleanEnv`'s environment.
pub fn git(args: &[&str], cwd: Option<&Path>) -> Out {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh")
        .env("LANGUAGE", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("DYLD_LIBRARY_PATH")
        .stdin(Stdio::null());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    match cmd.output() {
        Ok(o) => Out {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Out {
            ok: false,
            stdout: String::new(),
            stderr: e.to_string(),
        },
    }
}

/// `Filesystem::isLocalPath`
pub fn is_local_path(path: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(/?[a-z]:[\\/]|\.\.[\\/]|[a-z0-9_.-]+[\\/])").unwrap());
    (path.starts_with("file://") && !path.starts_with("file:////"))
        || (path.starts_with('/') && !path.starts_with("//"))
        || RE.is_match(path)
}

/// `Cache::isUsable`
pub fn cache_usable(dir: &str) -> bool {
    !Regex::new(r"(^|[\\/])(\$null|nul|NUL|/dev/null)([\\/]|$)")
        .unwrap()
        .is_match(dir)
}

/// `<cache-vcs-dir>/<sanitised url>`: `None` when the cache is disabled, or when the
/// name would be `.`/`..` and so point outside of it.
pub fn mirror_dir(config: &Config, url: &str) -> Option<PathBuf> {
    let dir = config.get_str("cache-vcs-dir");
    if !cache_usable(&dir) {
        return None;
    }
    let name = Regex::new(r"(?i)[^a-z0-9.]")
        .unwrap()
        .replace_all(&sanitize_url(url), "-")
        .into_owned();
    if name.chars().all(|c| c == '.') {
        return None;
    }
    Some(Path::new(&dir).join(name))
}

/// PHP `rawurlencode`
fn rawurlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `Url::stripCredentials`
fn strip_credentials(url: &str) -> String {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?P<prefix>[a-z0-9][a-z0-9+.-]*://)(?P<user>[^:/\s?#]*)(?::(?P<password>[^\s/?#]*))?@").unwrap()
    });
    RE.replace_all(url, |c: &regex::Captures<'_>| {
        let prefix = &c["prefix"];
        let empty_password = c.name("password").is_none_or(|p| p.as_str().is_empty());
        if empty_password && !Regex::new(r"(?i)^https?://$").unwrap().is_match(prefix) {
            format!("{prefix}{}@", &c["user"])
        } else {
            prefix.to_string()
        }
    })
    .into_owned()
}

/// `Git::maskCredentials`
fn mask_credentials(text: &str, credentials: &[String]) -> String {
    let mut out = text.to_string();
    for c in credentials {
        let masked = if matches!(
            c.as_str(),
            "private-token" | "x-token-auth" | "oauth2" | "gitlab-ci-token" | "x-oauth-basic"
        ) {
            continue;
        } else if c.len() > 6 {
            format!("{}...{}", &c[..3], &c[c.len() - 3..])
        } else if c.len() > 3 {
            format!("{}...", &c[..3])
        } else {
            "XXX".to_string()
        };
        if !c.is_empty() {
            out = out.replace(c.as_str(), &masked);
        }
    }
    out
}

/// `Config::prohibitUrlByConfig` for the protocols git can use.
pub fn prohibit_url(config: &Config, url: &str) -> Result<(), String> {
    let mut checked = url;
    let transport = Regex::new(r"(?i)^[a-z][a-z0-9+.-]*::(.+)$").unwrap();
    while let Some(m) = transport.captures(checked) {
        checked = m.get(1).unwrap().as_str();
    }
    let Some(m) = Regex::new(r"(?i)^([a-z][a-z0-9+.-]*)://")
        .unwrap()
        .captures(checked)
    else {
        return Ok(());
    };
    if matches!(m[1].to_lowercase().as_str(), "http" | "git" | "ftp") && config.bool("secure-http")
    {
        return Err(format!(
            "Your configuration does not allow connections to {}. See https://getcomposer.org/doc/06-config.md#secure-http for details.",
            sanitize_url(url)
        ));
    }
    Ok(())
}

fn domains_regex(config: &Config, key: &str) -> String {
    let domains: Vec<String> = config
        .domains(key)
        .iter()
        .map(|d| regex::escape(d))
        .collect();
    format!("({})", domains.join("|"))
}

/// `Preg::replace('#^#m', '  ', $text)`
fn indent(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len() - 1;
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if i > 0 && i == last && l.is_empty() {
                String::new()
            } else {
                format!("  {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Attempt {
    ok: bool,
    output: String,
    stderr: String,
    command: String,
}

/// `Git::runCommands`: each command is git's arguments with `%url%` / `%sanitizedUrl%`
/// placeholders; on failure GitHub protocols are switched and configured credentials
/// retried, as Composer does without a terminal to prompt on.
pub fn run_commands(
    io: &mut Io,
    config: &Config,
    commands: &[&[&str]],
    url: &str,
    cwd: &Path,
    initial_clone: bool,
) -> Result<String, String> {
    prohibit_url(config, url)?;
    if Regex::new(r"^ssh://[^@]+@[^:]+:[^0-9]+")
        .unwrap()
        .is_match(url)
    {
        return Err(format!(
            "The source URL {} is invalid, ssh URLs should have a port number after \":\".\nUse ssh://git@example.com:22/path or just git@example.com:path if you do not want to provide a password or custom port.",
            sanitize_url(url)
        ));
    }
    let attempt = |u: &str| -> Attempt {
        let mut output = String::new();
        let mut last = Attempt {
            ok: true,
            output: String::new(),
            stderr: String::new(),
            command: String::new(),
        };
        for (i, cmd) in commands.iter().enumerate() {
            let stripped = strip_credentials(u);
            let args: Vec<&str> = cmd
                .iter()
                .map(|a| match *a {
                    "%url%" => u,
                    "%sanitizedUrl%" => stripped.as_str(),
                    other => other,
                })
                .collect();
            let dir = if initial_clone && i == 0 {
                None
            } else {
                Some(cwd)
            };
            let out = git(&args, dir);
            output.push_str(&out.stdout);
            last.command = format!("git {}", args.join(" "));
            last.stderr = out.stderr;
            if !out.ok {
                last.ok = false;
                break;
            }
        }
        last.output = output;
        last
    };
    let wipe = || {
        if initial_clone {
            let _ = crate::fs::remove(cwd);
        }
    };

    let github = domains_regex(config, "github-domains");
    let protocols = config.string_list("github-protocols");
    if let Some(m) = Regex::new(&format!(r"^(?:https?|git)://{github}/(.*)"))
        .unwrap()
        .captures(url)
    {
        let mut messages = Vec::new();
        for protocol in &protocols {
            let proto_url = if protocol == "ssh" {
                format!("git@{}:{}", &m[1], &m[2])
            } else {
                format!("{protocol}://{}/{}", &m[1], &m[2])
            };
            let a = attempt(&proto_url);
            if a.ok {
                return Ok(a.output);
            }
            messages.push(format!("- {proto_url}\n{}", indent(&a.stderr)));
            wipe();
        }
        if config.authentication(&m[1]).is_none() {
            return Err(failure(
                &format!(
                    "Failed to clone {url} via {} protocols, aborting.\n\n{}",
                    protocols.join(", "),
                    messages.join("\n")
                ),
                url,
            ));
        }
    }

    let github_ssh = Regex::new(&format!(r"(?i)^git@{github}:(.+?)\.git$")).unwrap();
    let bypass_ssh = github_ssh.is_match(url) && !protocols.iter().any(|p| p == "ssh");
    let mut last = if bypass_ssh {
        Attempt {
            ok: false,
            output: String::new(),
            stderr: String::new(),
            command: String::new(),
        }
    } else {
        attempt(url)
    };
    if last.ok {
        return Ok(last.output);
    }
    let mut credentials: Vec<String> = Vec::new();
    let retry = |auth_url: String, last: &mut Attempt| -> bool {
        let a = attempt(&auth_url);
        if a.ok {
            last.output = a.output;
            return true;
        }
        *last = a;
        false
    };
    let gitlab = domains_regex(config, "gitlab-domains");
    let github_https = Regex::new(&format!(r"(?i)^https?://{github}/(.*?)(?:\.git)?$")).unwrap();
    let bb = [
        Regex::new(r"(?i)^(https?)://(bitbucket\.org)/(.*?)(?:\.git)?$").unwrap(),
        Regex::new(r"(?i)^(git)@(?:ssh\.)?(bitbucket\.org):(.+?\.git)$").unwrap(),
        Regex::new(r"(?i)^(ssh)://git@(?:ssh\.)?(bitbucket\.org)/(.*?)(?:\.git)?$").unwrap(),
    ];
    let gl = [
        Regex::new(&format!(r"(?i)^(git)@{gitlab}:(.+?\.git)$")).unwrap(),
        Regex::new(&format!(r"(?i)^(https?)://{gitlab}/(.*)")).unwrap(),
    ];
    if let Some(m) = github_ssh
        .captures(url)
        .or_else(|| github_https.captures(url))
    {
        if let Some(auth) = config.authentication(&m[1]) {
            let (u, p) = (rawurlencode(&auth.username), rawurlencode(&auth.password));
            if retry(
                format!("https://{u}:{p}@{}/{}.git", &m[1], &m[2]),
                &mut last,
            ) {
                return Ok(last.output);
            }
            credentials = vec![u, p];
        }
    } else if let Some(m) = bb.iter().find_map(|r| r.captures(url)) {
        let domain = m[2].to_string();
        let mut repo = m[3].to_string();
        if !repo.ends_with(".git") {
            repo.push_str(".git");
        }
        // ponytail: `bitbucket-oauth` consumers are not exchanged for an access token
        // here; app passwords (`http-basic`) and API tokens work
        if let Some(mut auth) = config.authentication(&domain) {
            if auth.password.starts_with("ATAT") {
                auth.username = "x-bitbucket-api-token-auth".into();
            }
            let (u, p) = (rawurlencode(&auth.username), rawurlencode(&auth.password));
            if retry(format!("https://{u}:{p}@{domain}/{repo}"), &mut last) {
                return Ok(last.output);
            }
            credentials = vec![u, p];
        }
        let ssh_url = format!("git@ssh.bitbucket.org:{repo}");
        if ssh_url != url {
            io.error("    No bitbucket authentication configured. Falling back to ssh.");
            if retry(ssh_url, &mut last) {
                return Ok(last.output);
            }
        }
    } else if let Some(m) = gl.iter().find_map(|r| r.captures(url)) {
        let scheme = if &m[1] == "git" { "https" } else { &m[1] };
        if let Some(auth) = config.authentication(&m[2]) {
            let (u, p) = (rawurlencode(&auth.username), rawurlencode(&auth.password));
            let userinfo = if matches!(
                auth.password.as_str(),
                "private-token" | "oauth2" | "gitlab-ci-token"
            ) {
                format!("{p}:{u}")
            } else {
                format!("{u}:{p}")
            };
            if retry(
                format!("{scheme}://{userinfo}@{}/{}", &m[2], &m[3]),
                &mut last,
            ) {
                return Ok(last.output);
            }
            credentials = vec![u, p];
        }
    } else if let Some(m) = Regex::new(r"(?i)^(https?://)([^/]+)(.*)$")
        .unwrap()
        .captures(url)
        && [
            "fatal: Authentication failed",
            "remote error: Invalid username or password.",
            "error: 401 Unauthorized",
            "fatal: unable to access",
            "fatal: could not read Username",
        ]
        .iter()
        .any(|f| last.stderr.contains(f))
    {
        let host = m[2].rsplit_once('@').map_or(&m[2], |(_, h)| h).to_string();
        if let Some(auth) = config.authentication(&host) {
            let (u, p) = (rawurlencode(&auth.username), rawurlencode(&auth.password));
            if retry(format!("{}{u}:{p}@{host}{}", &m[1], &m[3]), &mut last) {
                return Ok(last.output);
            }
            credentials = vec![u, p];
        }
    }
    wipe();
    let message = format!("Failed to execute {}\n\n{}", last.command, last.stderr);
    Err(failure(&mask_credentials(&message, &credentials), url))
}

/// `Git::throwException`
fn failure(message: &str, url: &str) -> String {
    let version = git(&["--version"], None);
    if !version.ok {
        return sanitize_url(&format!(
            "Failed to clone {url}, git was not found, check that it is installed and in your PATH env.\n\n{}",
            version.stderr
        ));
    }
    sanitize_url(message)
}

fn network_disabled() -> Option<String> {
    crate::config::env("COMPOSER_DISABLE_NETWORK").filter(|v| !v.is_empty() && v != "0")
}

fn is_bare_repo(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let out = git(&["rev-parse", "--git-dir"], Some(dir));
    out.ok && out.stdout.trim() == "."
}

/// `Git::syncMirror`: update the bare mirror in the cache, or clone it afresh.
pub fn sync_mirror(io: &mut Io, config: &Config, url: &str, dir: &Path) -> Result<bool, String> {
    if network_disabled().is_some_and(|v| v != "prime") {
        io.warning(&format!(
            "Aborting git mirror sync of {} as network is disabled",
            sanitize_url(url)
        ));
        return Ok(false);
    }
    if is_bare_repo(dir) {
        let result = run_commands(
            io,
            config,
            &[
                &["remote", "set-url", "origin", "--", "%url%"],
                &["remote", "update", "--prune", "origin"],
                &["gc", "--auto"],
            ],
            url,
            dir,
            false,
        );
        let reset = run_commands(
            io,
            config,
            &[&["remote", "set-url", "origin", "--", "%sanitizedUrl%"]],
            url,
            dir,
            false,
        );
        if let Err(e) = result.and(reset) {
            if io.verbosity >= 3 {
                io.error(&format!("Sync mirror failed: {e}"));
            }
            return Ok(false);
        }
        return Ok(true);
    }
    crate::fs::remove(dir)?;
    let dir_str = dir.to_string_lossy();
    run_commands(
        io,
        config,
        &[&["clone", "--mirror", "--", "%url%", &dir_str]],
        url,
        dir,
        true,
    )?;
    run_commands(
        io,
        config,
        &[&["remote", "set-url", "origin", "--", "%sanitizedUrl%"]],
        url,
        dir,
        false,
    )?;
    Ok(true)
}

fn ref_in_mirror(dir: &Path, reference: &str) -> bool {
    is_bare_repo(dir)
        && git(
            &[
                "rev-parse",
                "--quiet",
                "--verify",
                &format!("{reference}^{{commit}}"),
            ],
            Some(dir),
        )
        .ok
}

/// `Git::fetchRefOrSyncMirror`
fn fetch_ref_or_sync_mirror(
    io: &mut Io,
    config: &Config,
    url: &str,
    dir: &Path,
    reference: &str,
    pretty_version: &str,
) -> Result<bool, String> {
    if ref_in_mirror(dir, reference) {
        if is_sha(reference) {
            let branch = branch_of(pretty_version);
            let quoted = regex::escape(&branch);
            let branches = git(&["branch"], Some(dir));
            let tags = git(&["tag"], Some(dir));
            if branches.ok
                && !Regex::new(&format!(r"(?m)^[\s*]*v?{quoted}$"))
                    .unwrap()
                    .is_match(&branches.stdout)
                && tags.ok
                && !Regex::new(&format!(r"(?m)^[\s*]*{quoted}$"))
                    .unwrap()
                    .is_match(&tags.stdout)
            {
                sync_mirror(io, config, url, dir)?;
            }
        }
        return Ok(true);
    }
    if sync_mirror(io, config, url, dir)? {
        return Ok(ref_in_mirror(dir, reference));
    }
    Ok(false)
}

/// `Git::getMirrorDefaultBranch` (the remote's `HEAD branch`).
pub fn mirror_default_branch(
    io: &mut Io,
    config: &Config,
    url: &str,
    dir: &Path,
) -> Option<String> {
    if network_disabled().is_some() {
        return None;
    }
    let output = run_commands(
        io,
        config,
        &[
            &["remote", "set-url", "origin", "--", "%url%"],
            &["remote", "show", "origin"],
        ],
        url,
        dir,
        false,
    );
    let _ = run_commands(
        io,
        config,
        &[&["remote", "set-url", "origin", "--", "%sanitizedUrl%"]],
        url,
        dir,
        false,
    );
    let re = Regex::new(r"^\s*HEAD branch:\s(.+)\s*$").unwrap();
    output
        .ok()?
        .lines()
        .find_map(|l| re.captures(l).map(|m| m[1].to_string()))
}

fn is_sha(reference: &str) -> bool {
    reference.len() == 40
        && reference
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The branch name a dev pretty version stands for (`dev-` prefix, `.x-dev` suffix stripped).
fn branch_of(pretty_version: &str) -> String {
    Regex::new(r"(?i)(?:^dev-|(?:\.x)?-dev$)")
        .unwrap()
        .replace_all(pretty_version, "")
        .into_owned()
}

/// References and branch names from metadata must never be read as git options.
fn safe_ref(package: &Package) -> Result<String, String> {
    let reference = package
        .source
        .as_ref()
        .and_then(|s| s.reference.clone())
        .filter(|r| !r.is_empty())
        .ok_or_else(|| {
            format!(
                "Package {} is missing reference information",
                package.pretty_name
            )
        })?;
    if reference.starts_with('-') || branch_of(&package.pretty_version).starts_with('-') {
        return Err(format!(
            "Invalid git reference detected for {}: {reference}",
            package.pretty_name
        ));
    }
    Ok(reference)
}

/// `VcsDownloader::prepareUrls` for the main source URL.
pub fn source_url(package: &Package) -> String {
    let url = package.source.as_ref().map_or("", |s| s.url.as_str());
    if !is_local_path(url) {
        return url.to_string();
    }
    let (path, file) = match url.strip_prefix("file://") {
        Some(rest) => (rest, true),
        None => (url, false),
    };
    let real = crate::fs::realpath(Path::new(path));
    if file { format!("file://{real}") } else { real }
}

fn short_hash(io: &Io, reference: &str) -> String {
    if io.verbosity == 0 && is_sha(reference) {
        reference[..10].to_string()
    } else {
        reference.to_string()
    }
}

/// `GitDownloader::doDownload`: sync the mirror in the cache; true when the reference is in it.
pub fn download(io: &mut Io, config: &Config, package: &Package) -> Result<bool, String> {
    let reference = safe_ref(package)?;
    let url = source_url(package);
    if is_local_path(&url) {
        return Ok(false);
    }
    let Some(cache) = mirror_dir(config, &url) else {
        return Ok(false);
    };
    io.error(&format!(
        "  - Syncing {} ({}) into cache",
        package.name,
        package.full_pretty_version()
    ));
    Ok(fetch_ref_or_sync_mirror(
        io,
        config,
        &url,
        &cache,
        &reference,
        &package.pretty_version,
    )? && cache.is_dir())
}

/// `GitDownloader::doInstall`: `label` is the operation (`Installing x (1.0)`).
pub fn install(
    io: &mut Io,
    config: &Config,
    package: &Package,
    path: &Path,
    label: &str,
    cached: bool,
) -> Result<(), String> {
    let reference = safe_ref(package)?;
    let url = source_url(package);
    let path_str = path.to_string_lossy().into_owned();
    crate::fs::empty_directory(path)?;
    let cache = mirror_dir(config, &url).unwrap_or_default();
    let cache_str = cache.to_string_lossy().into_owned();
    let (msg, commands): (String, Vec<Vec<&str>>) = if cached {
        (
            format!("Cloning {} from cache", short_hash(io, &reference)),
            vec![
                vec![
                    "clone",
                    "--no-checkout",
                    "--dissociate",
                    "--reference",
                    &cache_str,
                    "--",
                    &cache_str,
                    &path_str,
                ],
                vec!["remote", "set-url", "origin", "--", "%sanitizedUrl%"],
                vec!["remote", "add", "composer", "--", "%sanitizedUrl%"],
            ],
        )
    } else {
        if network_disabled().is_some() && !is_local_path(&url) {
            return Err(format!(
                "The required git reference for {} is not in cache and network is disabled, aborting",
                package.name
            ));
        }
        (
            format!("Cloning {}", short_hash(io, &reference)),
            vec![
                vec!["clone", "--no-checkout", "--", "%url%", &path_str],
                vec!["remote", "add", "composer", "--", "%url%"],
                vec!["fetch", "composer"],
                vec!["remote", "set-url", "origin", "--", "%sanitizedUrl%"],
                vec!["remote", "set-url", "composer", "--", "%sanitizedUrl%"],
            ],
        )
    };
    io.error(&format!("  - {label}: {msg}"));
    let commands: Vec<&[&str]> = commands.iter().map(Vec::as_slice).collect();
    run_commands(io, config, &commands, &url, path, true)?;
    let source = package.source.as_ref().map_or("", |s| s.url.as_str());
    if url != source && !source.is_empty() {
        git(&["remote", "set-url", "origin", "--", source], Some(path));
        set_push_url(config, path, source);
    } else {
        set_push_url(config, path, &url);
    }
    update_to_commit(io, package, path, &reference, false)
}

/// `GitDownloader::doUpdate`: fetch into the existing clone and check the target out.
pub fn update(
    io: &mut Io,
    config: &Config,
    target: &Package,
    path: &Path,
    label: &str,
    cached: bool,
    forced: bool,
) -> Result<(), String> {
    if !path.join(".git").is_dir() {
        return Err(format!(
            "The .git directory is missing from {}, see https://getcomposer.org/commit-deps for more information",
            path.display()
        ));
    }
    let reference = safe_ref(target)?;
    let url = source_url(target);
    let cache = mirror_dir(config, &url).unwrap_or_default();
    let cache_str = cache.to_string_lossy().into_owned();
    let (msg, remote) = if cached {
        (
            format!("Checking out {} from cache", short_hash(io, &reference)),
            cache_str.as_str(),
        )
    } else {
        if network_disabled().is_some() && !is_local_path(&url) {
            return Err(format!(
                "The required git reference for {} is not in cache and network is disabled, aborting",
                target.name
            ));
        }
        (
            format!("Checking out {}", short_hash(io, &reference)),
            "%url%",
        )
    };
    io.error(&format!("  - {label}: {msg}"));
    let verify = format!("{reference}^{{commit}}");
    if !git(&["rev-parse", "--quiet", "--verify", &verify], Some(path)).ok {
        run_commands(
            io,
            config,
            &[
                &["remote", "set-url", "composer", "--", remote],
                &["fetch", "composer"],
                &["fetch", "--tags", "composer"],
            ],
            &url,
            path,
            false,
        )?;
    }
    run_commands(
        io,
        config,
        &[&["remote", "set-url", "composer", "--", "%sanitizedUrl%"]],
        &url,
        path,
        false,
    )?;
    update_to_commit(io, target, path, &reference, forced)?;
    let remotes = git(&["remote", "-v"], Some(path));
    let origin = Regex::new(r"(?m)^origin\s+(\S+)").unwrap();
    let composer = Regex::new(r"(?m)^composer\s+(\S+)").unwrap();
    let source = target.source.as_ref().map_or("", |s| s.url.as_str());
    if remotes.ok
        && let (Some(o), Some(c)) = (
            origin.captures(&remotes.stdout),
            composer.captures(&remotes.stdout),
        )
        && o[1] == c[1]
        && &c[1] != source
        && !source.is_empty()
    {
        git(&["remote", "set-url", "origin", "--", source], Some(path));
        set_push_url(config, path, source);
    }
    Ok(())
}

/// `GitDownloader::updateToCommit`
fn update_to_commit(
    io: &mut Io,
    package: &Package,
    path: &Path,
    reference: &str,
    forced: bool,
) -> Result<(), String> {
    let force: &[&str] = if forced { &["-f"] } else { &[] };
    let mut branch = branch_of(&package.pretty_version);
    let mut last_stderr = String::new();
    let mut run = |args: &[&str]| -> bool {
        let out = git(args, Some(path));
        last_stderr = out.stderr;
        out.ok
    };
    let remote_branches = git(&["branch", "-r"], Some(path));
    let branches = remote_branches.ok.then_some(remote_branches.stdout);
    let has_branch = |name: &str| {
        branches.as_deref().is_some_and(|b| {
            Regex::new(&format!(r"(?m)^\s+composer/{}$", regex::escape(name)))
                .unwrap()
                .is_match(b)
        })
    };
    if !is_sha(reference) && has_branch(reference) {
        let remote = format!("composer/{reference}");
        let checkout = [&["checkout"][..], force, &["-B", &branch, &remote, "--"]].concat();
        if run(&checkout) && run(&["reset", "--hard", &remote, "--"]) {
            return Ok(());
        }
    }
    if is_sha(reference) {
        if branches.is_some() && !has_branch(&branch) && has_branch(&format!("v{branch}")) {
            branch = format!("v{branch}");
        }
        let remote = format!("composer/{branch}");
        let fallback = [&["checkout"][..], force, &["-B", &branch, &remote, "--"]].concat();
        if (run(&["checkout", &branch, "--"]) || run(&fallback))
            && run(&["reset", "--hard", reference, "--"])
        {
            return Ok(());
        }
    }
    let checkout = [&["checkout"][..], force, &[reference, "--"]].concat();
    if run(&checkout) && run(&["reset", "--hard", reference, "--"]) {
        return Ok(());
    }
    let mut extra = String::new();
    if last_stderr.contains(reference) {
        io.warning(&format!("    {reference} is gone (history was rewritten?)"));
        extra = format!(
            "\nIt looks like the commit hash is not available in the repository, maybe {}? Run \"composer update {}\" to resolve this.",
            if package.is_dev() {
                "the commit was removed from the branch"
            } else {
                "the tag was recreated"
            },
            package.pretty_name
        );
    }
    Err(sanitize_url(&format!(
        "Failed to execute git {} && git reset --hard {reference} --\n\n{last_stderr}{extra}",
        checkout.join(" ")
    )))
}

/// `GitDownloader::setPushUrl`: GitHub clones push over ssh (or https without ssh).
fn set_push_url(config: &Config, path: &Path, url: &str) {
    let github = domains_regex(config, "github-domains");
    if let Some(m) = Regex::new(&format!(
        r"^(?:https?|git)://{github}/([^/]+)/([^/]+?)(?:\.git)?$"
    ))
    .unwrap()
    .captures(url)
    {
        let push = if config
            .string_list("github-protocols")
            .iter()
            .any(|p| p == "ssh")
        {
            format!("git@{}:{}/{}.git", &m[1], &m[2], &m[3])
        } else {
            format!("https://{}/{}/{}.git", &m[1], &m[2], &m[3])
        };
        git(
            &["remote", "set-url", "--push", "origin", "--", &push],
            Some(path),
        );
    }
}

/// `VcsDownloader::cleanChanges` / `GitDownloader::cleanChanges` without a terminal:
/// local changes abort unless `discard-changes` says to drop or stash them. Returns
/// whether the next checkout must be forced.
pub fn clean_changes(config: &Config, path: &Path, update: bool) -> Result<bool, String> {
    if !path.join(".git").is_dir() {
        return Ok(false);
    }
    // ponytail: Composer's unpushed-commits check (`getUnpushedChanges`) is not ported;
    // a reset leaves such commits in the reflog
    let status = git(
        &["status", "--porcelain", "--untracked-files=no"],
        Some(path),
    );
    if !status.ok {
        return Err(format!(
            "Failed to execute git status --porcelain --untracked-files=no\n\n{}",
            status.stderr
        ));
    }
    if status.stdout.trim().is_empty() {
        return Ok(false);
    }
    let changed = Err(format!(
        "Source directory {} has uncommitted changes.",
        path.display()
    ));
    match config.get("discard-changes") {
        Some(serde_json::Value::Bool(true)) => {
            if !git(&["clean", "-df"], Some(path)).ok || !git(&["reset", "--hard"], Some(path)).ok {
                return Err("Could not reset changes".into());
            }
            Ok(true)
        }
        Some(serde_json::Value::String(s)) if s == "stash" && update => {
            if !git(&["stash", "--include-untracked"], Some(path)).ok {
                return Err("Could not stash changes".into());
            }
            Ok(true)
        }
        _ => changed,
    }
}

/// `GitDownloader::reapplyChanges`: pop what `clean_changes` stashed.
pub fn reapply_changes(
    io: &mut Io,
    config: &Config,
    path: &Path,
    forced: bool,
) -> Result<(), String> {
    if forced
        && config
            .get("discard-changes")
            .and_then(serde_json::Value::as_str)
            == Some("stash")
    {
        io.error("    Re-applying stashed changes");
        let out = git(&["stash", "pop"], Some(path));
        if !out.ok {
            return Err(format!(
                "Failed to apply stashed changes:\n\n{}",
                out.stderr
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_follow_composer() {
        assert!(is_local_path("/tmp/x"));
        assert!(is_local_path("../x"));
        assert!(is_local_path("file:///tmp/x"));
        assert!(!is_local_path("https://github.com/a/b"));
        assert!(!is_local_path("git@github.com:a/b.git"));
        assert_eq!(
            strip_credentials("https://tok:x-oauth-basic@github.com/a/b.git"),
            "https://github.com/a/b.git"
        );
        assert_eq!(
            strip_credentials("ssh://git@host/a.git"),
            "ssh://git@host/a.git"
        );
        assert_eq!(indent("a\nb\n"), "  a\n  b\n");
        assert_eq!(branch_of("dev-main"), "main");
        assert_eq!(branch_of("1.x-dev"), "1");
        assert_eq!(
            mask_credentials(
                "https://abcdefgh:x-oauth-basic@x",
                &["abcdefgh".into(), "x-oauth-basic".into()]
            ),
            "https://abc...fgh:x-oauth-basic@x"
        );
    }
}
