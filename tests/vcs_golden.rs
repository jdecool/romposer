//! `vcs` / `git` repositories against composer 2.11.1: a git repository built by the test
//! (tags, branches, branch alias, a mismatching tag) read through a local path, through a
//! bare mirror in the cache, installed from source and updated in place; then the live
//! GitHub case, read with the API driver and with `no-api`. Locks, installed.json,
//! installed.php, the vendor checkout and stderr must match, each tool with its own home.

mod common;

use common::{composer, romposer, run_env, temp_dir};
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str], date: &str) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, file: &str, contents: &str) {
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn commit(dir: &Path, message: &str, date: &str) {
    git(dir, &["add", "-A"], date);
    git(dir, &["commit", "-qm", message], date);
}

/// acme/lib: v1.0.0, 1.1.0 (annotated), 2.0.0 whose composer.json says 1.9.0 (skipped),
/// main aliased to 1.x-dev, feature/x, and a branch without composer.json.
fn library(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"], "2024-01-01T10:00:00Z");
    write(
        dir,
        "composer.json",
        r#"{"name":"acme/lib","description":"Lib","license":"MIT","bin":["bin/tool"],"autoload":{"psr-4":{"Acme\\Lib\\":"src/"}},"extra":{"branch-alias":{"dev-main":"1.x-dev"}}}"#,
    );
    write(dir, "src/A.php", "<?php namespace Acme\\Lib; class A {}\n");
    write(dir, "bin/tool", "#!/bin/sh\necho tool\n");
    commit(dir, "one", "2024-01-01T10:00:00Z");
    git(dir, &["tag", "v1.0.0"], "2024-01-01T10:00:00Z");
    write(dir, "src/B.php", "<?php namespace Acme\\Lib; class B {}\n");
    commit(dir, "two", "2024-02-01T10:00:00Z");
    git(
        dir,
        &["tag", "-a", "1.1.0", "-m", "1.1.0"],
        "2024-02-01T10:00:00Z",
    );
    git(dir, &["checkout", "-qb", "broken"], "2024-02-01T10:00:00Z");
    write(
        dir,
        "composer.json",
        r#"{"name":"acme/lib","version":"1.9.0","autoload":{"psr-4":{"Acme\\Lib\\":"src/"}}}"#,
    );
    commit(dir, "mismatch", "2024-02-15T10:00:00Z");
    git(dir, &["tag", "2.0.0"], "2024-02-15T10:00:00Z");
    git(dir, &["checkout", "-q", "main"], "2024-02-15T10:00:00Z");
    git(dir, &["branch", "-D", "broken"], "2024-02-15T10:00:00Z");
    git(
        dir,
        &["checkout", "-qb", "feature/x"],
        "2024-03-01T10:00:00Z",
    );
    write(dir, "x.txt", "x\n");
    commit(dir, "three", "2024-03-01T10:00:00Z");
    git(
        dir,
        &["checkout", "-q", "--orphan", "docs"],
        "2024-03-02T10:00:00Z",
    );
    git(dir, &["rm", "-rfq", "."], "2024-03-02T10:00:00Z");
    write(dir, "README", "docs\n");
    commit(dir, "docs", "2024-03-02T10:00:00Z");
    git(dir, &["checkout", "-q", "main"], "2024-03-02T10:00:00Z");
}

fn project(dir: &Path, repo: &str, require: &str) {
    write(
        dir,
        "composer.json",
        &format!(
            r#"{{"name":"acme/proj","repositories":[{repo}],"minimum-stability":"dev","prefer-stable":true,"require":{{{require}}},"config":{{"policy":{{"advisories":{{"block":false}}}}}}}}"#
        ),
    );
}

fn comparable(stderr: &[u8], skip_installs: bool) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|l| {
            !l.contains("could not detect the root package")
                && !(skip_installs
                    && (l.starts_with("  - Downloading") || l.starts_with("  - Installing")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn git_state(dir: &Path) -> String {
    let path = dir.join("vendor/acme/lib");
    let mut state = String::new();
    for args in [
        &["rev-parse", "HEAD"][..],
        &["status", "--porcelain"],
        &["branch", "-a"],
        &["remote", "-v"],
        &["config", "--get", "remote.origin.pushurl"],
    ] {
        let out = Command::new("git")
            .args(args)
            .current_dir(&path)
            .output()
            .unwrap();
        state.push_str(&String::from_utf8_lossy(&out.stdout));
    }
    state
}

fn read(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).unwrap_or_default()
}

/// The project files to diff after a run.
fn snapshot(dir: &Path, with_git: bool) -> Vec<(&'static str, String)> {
    let mut files = vec![
        ("composer.lock", read(dir, "composer.lock")),
        (
            "installed.json",
            read(dir, "vendor/composer/installed.json"),
        ),
        ("installed.php", read(dir, "vendor/composer/installed.php")),
        (
            "autoload_psr4.php",
            read(dir, "vendor/composer/autoload_psr4.php"),
        ),
        ("bin", read(dir, "vendor/bin/tool")),
    ];
    if with_git {
        files.push(("vendor/acme/lib git", git_state(dir)));
    }
    files
}

/// Runs `steps` (argument lists) with each tool from the same starting files and compares.
#[allow(clippy::too_many_arguments)]
fn compare(
    name: &str,
    dir: &Path,
    homes: &Path,
    composer: &Path,
    reset: &dyn Fn(),
    between: &dyn Fn(),
    steps: &[&[&str]],
    with_git: bool,
) -> Vec<String> {
    let mut results = Vec::new();
    for (tool, bin) in [
        ("composer", composer.to_path_buf()),
        ("romposer", romposer()),
    ] {
        reset();
        let home = homes.join(tool);
        let mut outputs = Vec::new();
        for (i, args) in steps.iter().enumerate() {
            if i > 0 {
                between();
            }
            let mut args = args.to_vec();
            if tool == "composer" {
                args.push("--no-ansi");
            }
            outputs.push(run_env(
                &bin,
                dir,
                &args,
                &[("COMPOSER_HOME", home.to_str().unwrap())],
            ));
        }
        results.push((outputs, snapshot(dir, with_git)));
    }
    let mut failures = Vec::new();
    let (theirs, ours) = (&results[0], &results[1]);
    for (i, (t, o)) in theirs.0.iter().zip(&ours.0).enumerate() {
        if t.status.code() != o.status.code() {
            failures.push(format!(
                "{name} step {i}: exit {:?} vs {:?}\n{}",
                t.status.code(),
                o.status.code(),
                String::from_utf8_lossy(&o.stderr)
            ));
        }
        if o.status.success() && comparable(&t.stderr, false) != comparable(&o.stderr, false) {
            failures.push(format!(
                "{name} step {i}: stderr differs\n--- composer\n{}\n--- romposer\n{}",
                comparable(&t.stderr, false),
                comparable(&o.stderr, false)
            ));
        }
    }
    for ((file, t), (_, o)) in theirs.1.iter().zip(&ours.1) {
        if t != o {
            failures.push(format!(
                "{name}: {file} differs\n--- composer\n{t}\n--- romposer\n{o}"
            ));
        }
    }
    failures
}

#[test]
fn git_repositories_match_composer() {
    let Some(composer) = composer() else { return };
    let root = temp_dir("vcs");
    let lib = root.join("lib");
    library(&lib);
    let dir = root.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let homes = root.join("homes");
    let lib_json = serde_json::to_string(lib.to_str().unwrap()).unwrap();
    let mut failures = Vec::new();
    let clean = |dir: &Path| {
        let _ = std::fs::remove_dir_all(dir.join("vendor"));
        let _ = std::fs::remove_file(dir.join("composer.lock"));
    };

    // tags through a local path: v1.0.0 and 1.1.0 kept, 2.0.0 skipped; installed from source
    for (name, repo, require) in [
        (
            "vcs local tag",
            format!(r#"{{"type":"vcs","url":{lib_json}}}"#),
            r#""acme/lib":"^1.0""#,
        ),
        (
            "git local branch",
            format!(r#"{{"type":"git","url":{lib_json}}}"#),
            r#""acme/lib":"dev-main""#,
        ),
        (
            "branch alias",
            format!(r#"{{"type":"vcs","url":{lib_json}}}"#),
            r#""acme/lib":"1.x-dev""#,
        ),
        (
            "feature branch",
            format!(r#"{{"type":"vcs","url":{lib_json},"only-branches":["feature/*"]}}"#),
            r#""acme/lib":"dev-feature/x""#,
        ),
        (
            "missing version",
            format!(r#"{{"type":"vcs","url":{lib_json}}}"#),
            r#""acme/lib":"^3.0""#,
        ),
    ] {
        let reset = || {
            clean(&dir);
            project(&dir, &repo, require);
        };
        failures.extend(compare(
            name,
            &dir,
            &homes,
            &composer,
            &reset,
            &|| {},
            &[&["update"], &["show", "-a", "acme/lib", "--format=json"]],
            require != r#""acme/lib":"^3.0""#,
        ));
        // installing from the lock in an empty vendor
        failures.extend(compare(
            &format!("{name} install"),
            &dir,
            &homes,
            &composer,
            &|| {
                let _ = std::fs::remove_dir_all(dir.join("vendor"));
            },
            &|| {},
            &[&["install"]],
            require != r#""acme/lib":"^3.0""#,
        ));
    }

    // a URL that is not a local path goes through the bare mirror in cache-vcs-dir
    let mirror = dir.join("lib.git");
    let status = Command::new("git")
        .args([
            "clone",
            "-q",
            "--bare",
            lib.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let reset = || {
        clean(&dir);
        project(
            &dir,
            r#"{"type":"vcs","url":"lib.git"}"#,
            r#""acme/lib":"dev-main""#,
        );
    };
    failures.extend(compare(
        "mirror",
        &dir,
        &homes,
        &composer,
        &reset,
        &|| {},
        &[&["update"]],
        true,
    ));
    let listing = |home: &Path| -> Vec<String> {
        let mut out = Vec::new();
        for sub in ["cache/vcs", "cache/repo/lib.git"] {
            for entry in std::fs::read_dir(home.join(sub))
                .into_iter()
                .flatten()
                .flatten()
            {
                out.push(format!("{sub}/{}", entry.file_name().to_string_lossy()));
            }
        }
        out.sort();
        out
    };
    if listing(&homes.join("composer")) != listing(&homes.join("romposer")) {
        failures.push(format!(
            "mirror: cache layout differs\n{:?}\n{:?}",
            listing(&homes.join("composer")),
            listing(&homes.join("romposer"))
        ));
    }
    let _ = std::fs::remove_dir_all(&mirror);

    // a new upstream commit: refused while the checkout has local changes, then updated in place
    let repo = format!(r#"{{"type":"vcs","url":{lib_json}}}"#);
    let base = Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(&lib)
        .output()
        .unwrap();
    let base = String::from_utf8_lossy(&base.stdout).trim().to_string();
    let reset = || {
        git(
            &lib,
            &["reset", "-q", "--hard", &base],
            "2024-04-01T10:00:00Z",
        );
        clean(&dir);
        project(&dir, &repo, r#""acme/lib":"dev-main""#);
        let out = run_env(
            &romposer(),
            &dir,
            &["update"],
            &[("COMPOSER_HOME", homes.join("setup").to_str().unwrap())],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        write(&lib, "src/C.php", "<?php namespace Acme\\Lib; class C {}\n");
        commit(&lib, "four", "2024-04-01T10:00:00Z");
        write(&dir, "vendor/acme/lib/src/A.php", "<?php // edited\n");
    };
    let between = || {
        let _ = Command::new("git")
            .args(["checkout", "-q", "--", "src/A.php"])
            .current_dir(dir.join("vendor/acme/lib"))
            .status();
    };
    failures.extend(compare(
        "update in place",
        &dir,
        &homes,
        &composer,
        &reset,
        &between,
        &[&["update"], &["update"]],
        true,
    ));

    // source checkouts never reach the store manifest
    let manifest = read(&dir, "vendor/composer/pcomposer.json");
    if manifest.contains("acme/lib") {
        failures.push(format!(
            "the store manifest lists a source install:\n{manifest}"
        ));
    }

    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn unsupported_vcs_repositories_fail_explicitly() {
    let dir = temp_dir("vcs-unsupported");
    for (repo, kind) in [
        (r#"{"type":"svn","url":"https://example.org/svn"}"#, "svn"),
        (r#"{"type":"hg","url":"https://example.org/hg"}"#, "hg"),
        (
            r#"{"type":"fossil","url":"https://example.org/f"}"#,
            "fossil",
        ),
        (
            r#"{"type":"perforce","url":"perforce://example.org"}"#,
            "perforce",
        ),
        (r#"{"type":"vcs","url":"svn://example.org/repo"}"#, "svn"),
    ] {
        project(&dir, repo, r#""acme/lib":"*""#);
        let out = run_env(&romposer(), &dir, &["update"], &[]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_ne!(out.status.code(), Some(0), "{repo}");
        assert!(
            stderr.contains(&format!(
                "{kind} repositories are not supported by romposer"
            )),
            "{repo}: {stderr}"
        );
    }
}

/// A GitHub token for the live case, so the API rate limit does not decide the outcome.
fn github_token() -> Option<String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        return Some(token);
    }
    let out = Command::new("gh").args(["auth", "token"]).output().ok()?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !token.is_empty()).then_some(token)
}

#[test]
fn github_package_locks_like_composer_with_both_drivers() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("vcs-github");
    let homes = temp_dir("vcs-github-homes");
    let auth = github_token()
        .map(|t| format!(r#"{{"github-oauth":{{"github.com":"{t}"}}}}"#))
        .unwrap_or_default();
    let mut failures = Vec::new();
    for (name, repo) in [
        (
            "github api",
            r#"{"type":"vcs","url":"https://github.com/php-fig/link"}"#,
        ),
        (
            "github no-api",
            r#"{"type":"vcs","url":"https://github.com/php-fig/link","no-api":true}"#,
        ),
    ] {
        let mut locks = Vec::new();
        let mut errs = Vec::new();
        for (tool, bin) in [("composer", composer.clone()), ("romposer", romposer())] {
            let _ = std::fs::remove_dir_all(dir.join("vendor"));
            let _ = std::fs::remove_file(dir.join("composer.lock"));
            project(&dir, repo, r#""psr/link":"^2.0""#);
            let home = homes.join(format!("{tool}-{name}"));
            let mut env = vec![("COMPOSER_HOME", home.to_str().unwrap())];
            if !auth.is_empty() {
                env.push(("COMPOSER_AUTH", auth.as_str()));
            }
            let mut args = vec!["update"];
            if tool == "composer" {
                args.push("--no-ansi");
            }
            let out = run_env(&bin, &dir, &args, &env);
            if tool == "composer" && !out.status.success() {
                eprintln!(
                    "skipping live GitHub comparison: composer failed (offline or rate limited)"
                );
                return;
            }
            assert!(
                out.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            locks.push(read(&dir, "composer.lock"));
            errs.push(comparable(&out.stderr, true));
        }
        if locks[0] != locks[1] {
            failures.push(format!(
                "{name}: composer.lock differs\n--- composer\n{}\n--- romposer\n{}",
                locks[0], locks[1]
            ));
        }
        if errs[0] != errs[1] {
            failures.push(format!(
                "{name}: stderr differs\n--- composer\n{}\n--- romposer\n{}",
                errs[0], errs[1]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
