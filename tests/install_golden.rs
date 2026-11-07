//! `romposer install` against `composer install --no-scripts --no-plugins` 2.11.1 on a
//! real project: same vendor/ listing (paths and modes), same installed.json, same bin
//! proxies. Needs network access to packagist and the reference composer.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const MANIFEST: &str = r#"{
    "name": "acme/golden",
    "description": "golden fixture",
    "license": "MIT",
    "require": {
        "psr/log": "^3.0",
        "nikic/php-parser": "^5.0"
    },
    "require-dev": {
        "psr/container": "^2.0"
    }
}
"#;

/// vendor/ entries as `path mode`, skipping what ticket 04 (autoload generation) owns.
fn listing(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == "vendor/autoload.php"
                || (rel.starts_with("vendor/composer/") && rel != "vendor/composer/installed.json")
            {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            out.push(format!("{rel} {:o}", meta.permissions().mode()));
            if meta.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join("vendor"), root, &mut out);
    out.sort();
    out
}

#[test]
fn install_from_lock_matches_composer() {
    let Some(composer) = composer() else { return };
    let base = temp_dir("install-golden");
    let theirs = base.join("composer");
    let ours = base.join("romposer");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("composer.json"), MANIFEST).unwrap();
    let out = run(&composer, &theirs, &["update", "--no-install", "--no-ansi"]);
    assert!(
        out.status.success(),
        "composer update failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::create_dir_all(&ours).unwrap();
    fs::copy(theirs.join("composer.json"), ours.join("composer.json")).unwrap();
    fs::copy(theirs.join("composer.lock"), ours.join("composer.lock")).unwrap();

    let out = run(
        &composer,
        &theirs,
        &["install", "--no-scripts", "--no-plugins", "--no-ansi"],
    );
    assert!(
        out.status.success(),
        "composer install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(
        &romposer(),
        &ours,
        &["install", "--no-scripts", "--no-plugins"],
    );
    assert!(
        out.status.success(),
        "romposer install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(listing(&theirs), listing(&ours), "vendor/ listing differs");
    assert_generated_files_equal(&theirs, &ours, "install");

    // optimized and authoritative dumps
    for flags in [
        &["-o"][..],
        &["-a"][..],
        &["--apcu", "--apcu-prefix=romposer"][..],
    ] {
        let mut t_args = vec!["dump-autoload", "--no-ansi"];
        t_args.extend_from_slice(flags);
        let out = run(&composer, &theirs, &t_args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut o_args = vec!["dump-autoload"];
        o_args.extend_from_slice(flags);
        let out = run(&romposer(), &ours, &o_args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_generated_files_equal(&theirs, &ours, &format!("dump-autoload {flags:?}"));
    }
    assert_eq!(
        fs::read_to_string(theirs.join("vendor/bin/php-parse")).unwrap(),
        fs::read_to_string(ours.join("vendor/bin/php-parse")).unwrap(),
        "bin proxy differs"
    );

    // a second install is a no-op for both, and --no-dev drops the dev package
    let out = run(&romposer(), &ours, &["install", "--no-dev"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(
        &composer,
        &theirs,
        &[
            "install",
            "--no-dev",
            "--no-scripts",
            "--no-plugins",
            "--no-ansi",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        listing(&theirs),
        listing(&ours),
        "vendor/ listing differs after --no-dev"
    );
    assert_generated_files_equal(&theirs, &ours, "install --no-dev");
}

/// vendor/autoload.php and every file under vendor/composer/ must be byte-identical.
fn assert_generated_files_equal(theirs: &Path, ours: &Path, step: &str) {
    let mut files = vec!["vendor/autoload.php".to_string()];
    for entry in fs::read_dir(theirs.join("vendor/composer"))
        .unwrap()
        .flatten()
    {
        files.push(format!(
            "vendor/composer/{}",
            entry.file_name().to_string_lossy()
        ));
    }
    for file in files {
        let t = fs::read_to_string(theirs.join(&file)).unwrap();
        let o = fs::read_to_string(ours.join(&file))
            .unwrap_or_else(|_| panic!("{step}: romposer did not write {file}"));
        assert!(
            t == o,
            "{step}: {file} differs\n--- composer\n{t}\n--- romposer\n{o}"
        );
    }
}

#[test]
fn unsupported_dists_fail_cleanly() {
    let dir = temp_dir("install-unsupported");
    fs::write(
        dir.join("composer.json"),
        r#"{"name":"a/b","require":{"c/d":"*"}}"#,
    )
    .unwrap();
    fs::write(
        dir.join("composer.lock"),
        r#"{"content-hash":"x","packages":[{"name":"c/d","version":"1.0.0","source":{"type":"git","url":"https://example.com/c/d.git","reference":"abc"}}],"packages-dev":[],"aliases":[],"minimum-stability":"stable","stability-flags":{},"prefer-stable":false,"prefer-lowest":false,"platform":{},"platform-dev":{},"plugin-api-version":"2.11.0"}"#,
    )
    .unwrap();
    let out = run(&romposer(), &dir, &["install"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not yet supported"));
    assert!(!dir.join("vendor/c").exists());
}
