//! `romposer require` / `remove` against composer 2.11.1 on live Packagist: identical
//! composer.json, composer.lock, stdout and stderr.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::path::Path;

const BASE: &str = "{\n    \"name\": \"acme/req\",\n    \"require\": {\n        \"psr/log\": \"^3\"\n    },\n    \"prefer-stable\": true,\n    \"config\": {\"policy\": {\"advisories\": {\"block\": false}}}\n}\n";

fn comparable(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|l| {
            !l.contains("could not detect the root package")
                && !l.starts_with("  - Downloading")
                && !l.starts_with("  - Installing")
                && !l.starts_with("  - Upgrading")
                && !l.starts_with("  - Removing")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Runs `args` for both tools from the same composer.json and composer.lock.
fn compare(
    dir: &Path,
    composer: &Path,
    base_json: &str,
    base_lock: &str,
    args: &[&str],
) -> Vec<String> {
    let mut failures = Vec::new();
    let reset = || {
        std::fs::write(dir.join("composer.json"), base_json).unwrap();
        std::fs::write(dir.join("composer.lock"), base_lock).unwrap();
        let _ = std::fs::remove_dir_all(dir.join("vendor"));
    };
    reset();
    let mut theirs_args = args.to_vec();
    theirs_args.push("--no-ansi");
    let theirs = run(composer, dir, &theirs_args);
    let theirs_json = std::fs::read_to_string(dir.join("composer.json")).unwrap();
    let theirs_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap_or_default();
    reset();
    let ours = run(&romposer(), dir, args);
    let ours_json = std::fs::read_to_string(dir.join("composer.json")).unwrap();
    let ours_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap_or_default();
    if theirs.status.code() != ours.status.code() {
        failures.push(format!(
            "{args:?}: exit {:?} vs {:?}\n{}",
            theirs.status.code(),
            ours.status.code(),
            String::from_utf8_lossy(&ours.stderr)
        ));
    }
    if theirs_json != ours_json {
        failures.push(format!("{args:?}: composer.json differs\n--- composer\n{theirs_json}\n--- romposer\n{ours_json}"));
    }
    if theirs_lock != ours_lock {
        failures.push(format!("{args:?}: composer.lock differs"));
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

#[test]
fn require_and_remove_match_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("require");
    std::fs::write(dir.join("composer.json"), BASE).unwrap();
    let probe = run(&composer, &dir, &["update", "--no-install", "--no-ansi"]);
    if !probe.status.success() {
        eprintln!("skipping live require comparison: composer failed");
        return;
    }
    let base_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap();
    let mut failures = Vec::new();
    for args in [
        &["require", "monolog/monolog", "--no-install"][..],
        &["require", "monolog/monolog:^3.0", "--no-update"][..],
        &["require", "--dev", "phpunit/phpunit", "--no-install"][..],
        &[
            "require",
            "symfony/console:^7.0",
            "--no-install",
            "--sort-packages",
        ][..],
        &["require", "psr/log:^2", "--no-install"][..],
        &["require", "monolog/monolog", "--dry-run"][..],
        &["require", "nope/nope", "--no-install"][..],
        &["require", "monolog/monolog"][..],
        &["remove", "psr/log", "--no-install"][..],
        &["remove", "nope/nope", "--no-install"][..],
    ] {
        failures.extend(compare(&dir, &composer, BASE, &base_lock, args));
    }
    // remove after a require, from the lock the require wrote
    std::fs::write(dir.join("composer.json"), BASE).unwrap();
    std::fs::write(dir.join("composer.lock"), &base_lock).unwrap();
    let _ = run(
        &composer,
        &dir,
        &["require", "monolog/monolog", "--no-install", "--no-ansi"],
    );
    let with_monolog_json = std::fs::read_to_string(dir.join("composer.json")).unwrap();
    let with_monolog_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap();
    failures.extend(compare(
        &dir,
        &composer,
        &with_monolog_json,
        &with_monolog_lock,
        &["remove", "monolog/monolog"],
    ));
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
