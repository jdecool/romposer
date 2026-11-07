//! `config`, `init`, `global` and `clear-cache` against composer 2.11.1: identical
//! composer.json, auth.json, global config.json and auth.json, stdout, exit code, and
//! stderr when the command succeeds.

mod common;

use common::{composer, romposer, run_env, temp_dir};
use std::path::Path;

const BASE: &str = "{\n    \"name\": \"acme/cfg\",\n    \"config\": {\"vendor-dir\": \"lib\"},\n    \"extra\": {\n        \"foo\": {\n            \"bar\": 1,\n            \"baz\": {}\n        }\n    },\n    \"repositories\": {\n        \"first\": {\"type\": \"vcs\", \"url\": \"https://example.org/first\"}\n    },\n    \"scripts\": {\n        \"test\": \"phpunit\"\n    }\n}\n";

const FILES: [&str; 5] = [
    "composer.json",
    "auth.json",
    ".composer-home/config.json",
    ".composer-home/auth.json",
    ".composer-home/composer.json",
];

fn comparable(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|l| !l.contains("could not detect the root package"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot(dir: &Path) -> Vec<Option<String>> {
    FILES
        .iter()
        .map(|f| std::fs::read_to_string(dir.join(f)).ok())
        .collect()
}

/// Runs `args` for both tools from `setup`, comparing outputs and the files above.
fn compare(dir: &Path, composer: &Path, setup: &dyn Fn(&Path), args: &[&str]) -> Vec<String> {
    compare_env(dir, composer, setup, args, &[])
}

fn compare_env(
    dir: &Path,
    composer: &Path,
    setup: &dyn Fn(&Path),
    args: &[&str],
    env: &[(&str, &str)],
) -> Vec<String> {
    let mut failures = Vec::new();
    let reset = || {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let _ = std::fs::remove_dir_all(entry.path());
            let _ = std::fs::remove_file(entry.path());
        }
        setup(dir);
    };
    reset();
    let mut theirs_args = args.to_vec();
    theirs_args.push("--no-ansi");
    let theirs = run_env(composer, dir, &theirs_args, env);
    let theirs_files = snapshot(dir);
    reset();
    let ours = run_env(&romposer(), dir, args, env);
    let ours_files = snapshot(dir);
    if theirs.status.code() != ours.status.code() {
        failures.push(format!(
            "{args:?}: exit {:?} vs {:?}\n{}\n{}",
            theirs.status.code(),
            ours.status.code(),
            String::from_utf8_lossy(&theirs.stderr),
            String::from_utf8_lossy(&ours.stderr)
        ));
    }
    for ((name, t), o) in FILES.iter().zip(&theirs_files).zip(&ours_files) {
        if t != o {
            failures.push(format!(
                "{args:?}: {name} differs\n--- composer\n{}\n--- romposer\n{}",
                t.as_deref().unwrap_or("<missing>"),
                o.as_deref().unwrap_or("<missing>")
            ));
        }
    }
    if theirs.stdout != ours.stdout {
        failures.push(format!(
            "{args:?}: stdout differs\n--- composer\n{}\n--- romposer\n{}",
            String::from_utf8_lossy(&theirs.stdout),
            String::from_utf8_lossy(&ours.stdout)
        ));
    }
    if ours.status.success() {
        let (t, o) = (comparable(&theirs.stderr), comparable(&ours.stderr));
        if t != o {
            failures.push(format!(
                "{args:?}: stderr differs\n--- composer\n{t}\n--- romposer\n{o}"
            ));
        }
    }
    failures
}

fn base(dir: &Path) {
    std::fs::write(dir.join("composer.json"), BASE).unwrap();
}

#[test]
fn config_matches_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("config");
    let mut failures = Vec::new();
    let cases: &[&[&str]] = &[
        &["config", "--list"],
        &["config", "vendor-dir"],
        &["config", "vendor-dir", "--absolute"],
        &["config", "bin-dir"],
        &["config", "process-timeout"],
        &["config", "github-protocols"],
        &["config", "allow-plugins"],
        &["config", "platform"],
        &["config", "cache-files-maxsize"],
        &["config", "extra.foo"],
        &["config", "extra.foo.bar"],
        &["config", "extra.nope"],
        &["config", "repositories"],
        &["config", "repositories.first"],
        &["config", "repositories.nope"],
        &["config", "name"],
        &["config", "type"],
        &["config", "license"],
        &["config", "nope"],
        &["config", "sort-packages", "true"],
        &["config", "sort-packages", "maybe"],
        &["config", "process-timeout", "600"],
        &["config", "process-timeout", "1", "2"],
        &["config", "vendor-dir", "--unset"],
        &["config", "vendor-dir", "x", "--unset"],
        &["config", "preferred-install.acme/*", "source"],
        &["config", "allow-plugins.acme/plugin", "true"],
        &["config", "allow-plugins.acme/plugin", "nah"],
        &["config", "platform.php", "8.1.0"],
        &["config", "platform.ext-foo", "false"],
        &["config", "github-protocols", "https", "ssh"],
        &["config", "github-protocols", "http"],
        &["config", "minimum-stability", "rc"],
        &["config", "prefer-stable", "1"],
        &["config", "description", "A thing"],
        &["config", "license", "MIT", "GPL-2.0-only"],
        &["config", "keywords", "a", "b"],
        &["config", "name", "--unset"],
        &["config", "extra.foo.qux", "hello"],
        &[
            "config",
            "extra.new",
            "--json",
            "{\"a\": [1, 2], \"b\": {}}",
        ],
        &["config", "extra.foo", "--json", "--merge", "{\"q\": true}"],
        &["config", "extra.foo.bar", "--unset"],
        &["config", "extra.foo", "--unset"],
        &["config", "suggest.acme/x", "for", "fun"],
        &["config", "scripts.lint", "phpcs"],
        &["config", "scripts.multi", "a", "b"],
        &["config", "scripts.test", "--unset"],
        &[
            "config",
            "repositories.foo",
            "vcs",
            "https://example.org/foo",
        ],
        &[
            "config",
            "--append",
            "repo.bar",
            "composer",
            "https://example.org/bar",
        ],
        &[
            "config",
            "repositories.baz",
            "{\"type\": \"path\", \"url\": \"../baz\"}",
        ],
        &["config", "repositories.packagist.org", "false"],
        &["config", "repositories.first", "--unset"],
        &["config", "repositories.foo", "vcs"],
        &["config", "github-oauth.github.com", "tok"],
        &["config", "http-basic.example.org", "user", "pass"],
        &["config", "http-basic.example.org", "user"],
        &["config", "gitlab-token.gitlab.example.org", "u", "t"],
        &["config", "bearer.example.org", "abc"],
        &["config", "custom-headers.example.org", "X-A: 1", "X-B: 2"],
        &["config", "custom-headers.example.org", "bad"],
        &["config", "audit.ignore", "CVE-1", "CVE-2"],
        &["config", "audit.abandoned", "report"],
        &["config", "audit", "--unset"],
        &["config", "policy.advisories.block", "false"],
        &["config", "policy.cooldown.period", "7 days"],
        &[
            "config",
            "policy.advisories.ignore-id",
            "--json",
            "[\"GHSA-1\"]",
        ],
        &["config", "policy.mylist", "true"],
        &["config", "policy.ignorex", "true"],
        &["config", "policy.malware.sources", "x"],
        &["config", "policy.ignore-unreachable", "update", "install"],
        &["config", "unknown-key", "x"],
        &["config", "foo", "--unset"],
        &["config", "--global", "process-timeout", "10"],
        &["config", "--global", "github-oauth.github.com", "tok"],
        &["config", "--global", "name", "x/y"],
        &["config", "--global", "--list"],
        &["config", "--global", "vendor-dir"],
        &["config", "--file", "nope.json", "name"],
        &["config", "--file", "x.json", "--global", "name"],
    ];
    for args in cases {
        failures.extend(compare(&dir, &composer, &base, args));
    }
    // successive edits from a file composer already edited
    let edited = |dir: &Path| {
        base(dir);
        std::fs::write(
            dir.join("auth.json"),
            "{\n    \"github-oauth\": {\n        \"github.com\": \"tok\"\n    }\n}\n",
        )
        .unwrap();
    };
    for args in [
        &["config", "github-oauth.github.com", "--unset"][..],
        &["config", "github-oauth.github.com"][..],
        &["config", "--list"][..],
    ] {
        failures.extend(compare(&dir, &composer, &edited, args));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn init_and_global_match_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("init");
    let env = [
        ("COMPOSER_DEFAULT_VENDOR", "Acme Corp"),
        ("COMPOSER_DEFAULT_AUTHOR", "Jane Doe"),
        ("COMPOSER_DEFAULT_EMAIL", "jane@example.org"),
    ];
    let empty = |_: &Path| {};
    let mut failures = Vec::new();
    let cases: &[&[&str]] = &[
        &["init"],
        &["init", "--name", "acme/thing"],
        &[
            "init",
            "--name",
            "acme/thing",
            "--description",
            "A thing",
            "--author",
            "John Smith <john@example.com>",
            "--type",
            "project",
            "--homepage",
            "https://example.org",
            "--require",
            "psr/log:^3",
            "--require",
            "php >=8.1",
            "--require-dev",
            "phpunit/phpunit=^11",
            "--stability",
            "dev",
            "--license",
            "MIT",
            "--repository",
            "https://repo.example.org",
            "--repository",
            "{\"type\": \"vcs\", \"url\": \"https://example.org/x\"}",
        ],
        &["init", "--name", "acme/thing", "--author", "Solo"],
        &["init", "--name", "acme/thing", "--autoload", "src/"],
        &[
            "init",
            "--name",
            "acme/thing",
            "--autoload",
            "lib/",
            "--require",
            "psr/log:^3",
        ],
        &["init", "--name", "Acme/Thing"],
        &["init", "--name", "acme/thing", "--require", "psr/log"],
        &["init", "--name", "acme/thing", "--author", "<nope>"],
        &[
            "init",
            "--name",
            "acme/thing",
            "--author",
            "Jo <not-an-email>",
        ],
        &["init", "--name", "acme/thing", "--repository", "ftp://nope"],
        &["init", "--name", "acme/thing", "--type", "a b c"],
        &["global", "config", "process-timeout", "5"],
        &["global", "config", "--list"],
        &["global", "config", "--global", "sort-packages", "true"],
    ];
    for args in cases {
        failures.extend(compare_env(&dir, &composer, &empty, args, &env));
    }
    failures.extend(compare_env(
        &dir,
        &composer,
        &base,
        &["init", "--name", "acme/over"],
        &env,
    ));
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn clear_cache_matches_composer_and_keeps_the_store() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("clear-cache");
    let filled = |dir: &Path| {
        let cache = dir.join(".composer-home/cache");
        for sub in [
            "files/acme/x",
            "repo/https---repo.packagist.org",
            "vcs/foo",
            "pcomposer-store/objects",
        ] {
            std::fs::create_dir_all(cache.join(sub)).unwrap();
            std::fs::write(cache.join(sub).join("f"), "x").unwrap();
        }
    };
    let mut failures = Vec::new();
    for args in [
        &["clear-cache"][..],
        &["clear-cache", "--gc"][..],
        &["cc"][..],
    ] {
        failures.extend(compare(&dir, &composer, &filled, args));
    }
    // no cache at all
    let missing = |_: &Path| {};
    failures.extend(compare(&dir, &composer, &missing, &["clear-cache"]));
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));

    filled(&dir);
    let out = run_env(&romposer(), &dir, &["clear-cache"], &[]);
    assert!(out.status.success());
    let cache = dir.join(".composer-home/cache");
    assert!(cache.join("pcomposer-store/objects/f").exists());
    assert!(!cache.join("files").exists());
}

fn archive_project(dir: &Path) {
    std::fs::write(
        dir.join("composer.json"),
        "{\n    \"name\": \"acme/arch\",\n    \"version\": \"1.2.0\",\n    \"archive\": {\"exclude\": [\"/tests\", \"*.md\", \"!keep.md\"]}\n}\n",
    )
    .unwrap();
    for (file, text) in [
        ("src/A.php", "<?php"),
        ("src/sub/B.php", "<?php"),
        ("tests/T.php", "<?php"),
        ("README.md", "x"),
        ("keep.md", "x"),
        ("docs/x.txt", "x"),
        (
            ".gitattributes",
            "/docs export-ignore\n*.txt export-ignore\n",
        ),
        (".git/HEAD", "ref"),
        ("bin/tool", "#!/bin/sh"),
        // a name tar would read as an option
        ("--checkpoint=1", "x"),
        (
            "src/a/very/long/path/that/goes/beyond/one/hundred/bytes/to/exercise/the/ustar/prefix/field/File.php",
            "<?php",
        ),
    ] {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::create_dir_all(dir.join("empty")).unwrap();
}

/// The entries of every archive under `dir`, by archive name.
fn archives(dir: &Path) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for sub in [dir.to_path_buf(), dir.join("out")] {
        for entry in std::fs::read_dir(&sub).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !(name.ends_with(".zip") || name.contains(".tar")) {
                continue;
            }
            let listing = if name.ends_with(".zip") {
                std::process::Command::new("unzip")
                    .arg("-Z1")
                    .arg(entry.path())
                    .output()
            } else {
                std::process::Command::new("tar")
                    .arg("-tf")
                    .arg(entry.path())
                    .output()
            }
            .unwrap();
            let mut entries: Vec<String> = String::from_utf8_lossy(&listing.stdout)
                .lines()
                .map(|l| l.trim_end_matches('/').to_string())
                .collect();
            entries.sort();
            out.push((name, entries));
        }
    }
    out.sort();
    out
}

#[test]
fn archive_matches_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("archive");
    // a home outside the project, so Composer's cache directories stay out of the archive
    let home = temp_dir("archive-home");
    let env = [("COMPOSER_HOME", home.to_str().unwrap())];
    let mut failures = Vec::new();
    for args in [
        &["archive"][..],
        &["archive", "--format", "zip"][..],
        &["archive", "--format", "tar.gz", "--dir", "out"][..],
        &["archive", "--format", "zip", "--file", "custom"][..],
        &["archive", "--format", "zip", "--ignore-filters"][..],
        &["archive", "--format", "rar"][..],
    ] {
        failures.extend(compare_env(&dir, &composer, &archive_project, args, &env));
        let reset = || {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            archive_project(&dir);
        };
        reset();
        let mut theirs_args = args.to_vec();
        theirs_args.push("--no-ansi");
        run_env(&composer, &dir, &theirs_args, &env);
        let theirs = archives(&dir);
        reset();
        run_env(&romposer(), &dir, args, &env);
        let ours = archives(&dir);
        if theirs != ours {
            failures.push(format!("{args:?}: archives differ\n{theirs:?}\n{ours:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
