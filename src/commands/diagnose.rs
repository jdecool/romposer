//! `composer diagnose`, adapted: the checks that still mean something for romposer
//! (composer.json, git, packagist connectivity, GitHub rate limit, disk space) plus the
//! pcomposer store. No pubkeys or self-update version check: romposer has no self-update.
//!
//! ponytail: `checkPlatform` (PHP ini advice), proxy and custom repository checks are not
//! ported; their output depends on the PHP runtime Composer runs in, which romposer is not.

use crate::config::Config;
use crate::io::Io;
use std::path::Path;

#[derive(clap::Args, Debug)]
pub struct Args {}

pub fn run(_args: &Args, io: &mut Io) -> i32 {
    match execute(io) {
        Ok(code) => code,
        Err(message) => crate::scripts::exit_code(io, &message),
    }
}

/// `outputResult`: `Ok(())` is OK, else messages tagged `error` (exit 2) or `warning` (exit 1).
fn output(io: &mut Io, exit: &mut i32, label: &str, result: Result<(), Vec<(bool, String)>>) {
    if !io.quiet {
        print!("{label}");
    }
    match result {
        Ok(()) => io.write("OK"),
        Err(messages) => {
            let error = messages.iter().any(|(e, _)| *e);
            io.write(if error { "FAIL" } else { "WARNING" });
            *exit = (*exit).max(if error { 2 } else { 1 });
            for (_, m) in messages {
                io.write(m.trim());
            }
        }
    }
}

fn git_check() -> Result<(), Vec<(bool, String)>> {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if run(&["config", "color.ui"]).is_some_and(|c| c.eq_ignore_ascii_case("always")) {
        return Err(vec![(false, "Your git color.ui setting is set to always, this is known to create issues. Use \"git config --global color.ui true\" to set it correctly.".into())]);
    }
    let Some(version) = run(&["--version"]).and_then(|v| {
        regex::Regex::new(r"(\d+(?:\.\d+)+)")
            .unwrap()
            .captures(&v)
            .map(|m| m[1].to_string())
    }) else {
        return Err(vec![(false, "No git process found".into())]);
    };
    if crate::semver::version_compare(&version, "2.24.0") == std::cmp::Ordering::Less {
        return Err(vec![(
            false,
            format!(
                "Your git version ({version}) is too old and possibly will cause issues. Please upgrade to git 2.24 or above"
            ),
        )]);
    }
    Ok(())
}

fn disk_free(dir: &str) -> Option<u64> {
    let path = std::ffi::CString::new(dir).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs fills the struct we pass for a NUL-terminated path.
    (unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0)
        .then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

fn execute(io: &mut Io) -> Result<i32, String> {
    let cwd = crate::fs::cwd();
    let file = crate::config::composer_file()?;
    let manifest = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().cloned());
    let mut config = match &manifest {
        Some(m) => Config::for_project(&cwd, m, &file)?,
        None => Config::create(&cwd)?,
    };
    config
        .config
        .insert("secure-http".into(), serde_json::Value::Bool(false));
    let mut exit = 0;

    io.write(&format!(
        "Composer version: {} (romposer {})",
        crate::COMPOSER_VERSION,
        env!("CARGO_PKG_VERSION")
    ));
    match crate::platform::PhpInfo::detect() {
        Some(php) => io.write(&format!("PHP version: {}", php.version)),
        None => io.write("PHP version: php not found on PATH"),
    }

    if manifest.is_some() {
        let report = crate::validation::validate_file(
            &file,
            &file.to_string_lossy(),
            crate::validation::CHECK_ALL,
            true,
        );
        let mut messages: Vec<(bool, String)> =
            report.errors.into_iter().map(|e| (true, e)).collect();
        messages.extend(report.warnings.into_iter().map(|w| (false, w)));
        output(
            io,
            &mut exit,
            "Checking composer.json: ",
            if messages.is_empty() {
                Ok(())
            } else {
                Err(messages)
            },
        );
    }

    output(io, &mut exit, "Checking git settings: ", git_check());

    let http = crate::download::Http::new(&config)?;
    for proto in ["http", "https"] {
        let result = http
            .get(&format!("{proto}://repo.packagist.org/packages.json"), &[])
            .map(|_| ())
            .map_err(|(_, e)| vec![(true, format!("[TransportException] {e}"))]);
        output(
            io,
            &mut exit,
            &format!("Checking {proto} connectivity to packagist: "),
            result,
        );
    }

    // the tokens themselves go out through the config's auth, as for every request
    let oauth: Vec<String> = config
        .get("github-oauth")
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    // `checkGithubOauth`, else `getGithubRateLimit` for github.com
    let checks: Vec<(String, String, Option<String>)> = if oauth.is_empty() {
        vec![(
            "github.com rate limit".into(),
            "https://api.github.com/rate_limit".into(),
            None,
        )]
    } else {
        oauth
            .into_iter()
            .map(|domain| {
                let url = if domain == "github.com" {
                    "https://api.github.com/".to_string()
                } else {
                    format!("https://{domain}/api/v3/")
                };
                (format!("{domain} oauth access"), url, Some(domain))
            })
            .collect()
    };
    for (label, url, domain) in checks {
        let label = format!("Checking {label}: ");
        match http.get(&url, &[]) {
            Err((Some(401), _)) => {
                let domain = domain.unwrap_or_else(|| "github.com".into());
                if !io.quiet {
                    print!("{label}");
                }
                io.write(&format!("The oauth token for {domain} seems invalid, run \"composer config --global --unset github-oauth.{domain}\" to remove it"));
            }
            Err((_, e)) => output(
                io,
                &mut exit,
                &label,
                Err(vec![(true, format!("[TransportException] {e}"))]),
            ),
            Ok(r) if domain.is_none() => {
                let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
                let core = &body["resources"]["core"];
                let (remaining, limit) = (
                    core["remaining"].as_u64().unwrap_or(0),
                    core["limit"].as_u64().unwrap_or(0),
                );
                let result = if remaining < 10 {
                    Err(vec![(
                        false,
                        format!(
                            "GitHub has a rate limit on their API. You currently have {remaining} out of {limit} requests left.\nSee https://developer.github.com/v3/#rate-limiting and also\n    https://getcomposer.org/doc/articles/troubleshooting.md#api-rate-limit-and-oauth-tokens"
                        ),
                    )])
                } else {
                    Ok(())
                };
                output(io, &mut exit, &label, result);
            }
            Ok(_) => output(io, &mut exit, &label, Ok(())),
        }
    }

    let mut disk = Ok(());
    for dir in [config.get_str("home"), config.vendor_dir()] {
        if disk_free(&dir).is_some_and(|free| free < 1024 * 1024) {
            disk = Err(vec![(true, format!("The disk hosting {dir} is full"))]);
            break;
        }
    }
    output(io, &mut exit, "Checking disk free space: ", disk);

    let store = crate::store::config::StoreConfig::from_root(
        manifest
            .as_ref()
            .and_then(|m| m.get("extra"))
            .and_then(|e| e.as_object())
            .unwrap_or(&Default::default()),
        &config.get_str("cache-dir"),
    )?;
    let store_check = if !store.enabled {
        Err(vec![(
            false,
            "The store is disabled, packages are extracted into vendor/".to_string(),
        )])
    } else {
        crate::fs::ensure_dir(Path::new(&store.root)).map_err(|e| vec![(true, e)])
    };
    output(
        io,
        &mut exit,
        &format!(
            "Checking pcomposer store ({}, {}): ",
            store.root,
            store.strategy.as_str()
        ),
        store_check,
    );
    Ok(exit)
}
