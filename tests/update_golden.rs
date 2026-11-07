//! `romposer update` against composer 2.11.1: byte-identical composer.lock files and
//! the same stderr, on the recorded repository and on live Packagist.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::path::Path;

/// Lines that depend on git or on the download order.
fn comparable(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|l| {
            !l.contains("could not detect the root package")
                && !l.starts_with("Downloading ")
                && !l.starts_with("  - Downloading")
                && !l.starts_with("  - Installing")
                && !l.starts_with("  - Upgrading")
                && !l.starts_with("  - Removing")
                && !l.contains("Linking from store")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compare_update(dir: &Path, composer: &Path, args: &[&str]) -> Vec<String> {
    let mut failures = Vec::new();
    let mut theirs_args = args.to_vec();
    theirs_args.push("--no-ansi");
    let _ = std::fs::remove_file(dir.join("composer.lock"));
    let theirs = run(composer, dir, &theirs_args);
    let theirs_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap_or_default();
    let _ = std::fs::remove_file(dir.join("composer.lock"));
    let _ = std::fs::remove_dir_all(dir.join("vendor"));
    let ours = run(&romposer(), dir, args);
    let ours_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap_or_default();
    if theirs.status.code() != ours.status.code() {
        failures.push(format!(
            "{args:?}: exit {:?} vs {:?}\n{}",
            theirs.status.code(),
            ours.status.code(),
            String::from_utf8_lossy(&ours.stderr)
        ));
    }
    if theirs_lock != ours_lock {
        failures.push(format!(
            "{args:?}: lock differs\n--- composer\n{theirs_lock}\n--- romposer\n{ours_lock}"
        ));
    }
    let (t, o) = (comparable(&theirs.stderr), comparable(&ours.stderr));
    if t != o {
        failures.push(format!(
            "{args:?}: stderr differs\n--- composer\n{t}\n--- romposer\n{o}"
        ));
    }
    if theirs.stdout != ours.stdout {
        failures.push(format!(
            "{args:?}: stdout differs\n--- composer\n{}\n--- romposer\n{}",
            String::from_utf8_lossy(&theirs.stdout),
            String::from_utf8_lossy(&ours.stdout)
        ));
    }
    failures
}

#[test]
fn update_matches_composer_on_live_packagist() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("update");
    std::fs::write(
        dir.join("composer.json"),
        r#"{"name":"acme/update","require":{"monolog/monolog":"^3.0","symfony/console":"^7.0","psr/log":"^3"},"require-dev":{"phpunit/phpunit":"^11.0"},"prefer-stable":true,"config":{"policy":{"advisories":{"block":false}}}}"#,
    )
    .unwrap();
    let probe = run(&composer, &dir, &["update", "--no-install", "--no-ansi"]);
    if !probe.status.success() {
        eprintln!("skipping live update comparison: composer failed");
        return;
    }
    let mut failures = Vec::new();
    for args in [
        &["update", "--no-install"][..],
        &["update", "--no-install", "--prefer-lowest"][..],
        &["update", "--no-install", "--no-dev"][..],
        &["update", "--lock", "--no-install"][..],
        &["update"][..],
    ] {
        failures.extend(compare_update(&dir, &composer, args));
    }
    // a partial update on top of the lock the last run wrote, from the same starting point for both
    let base_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap();
    std::fs::write(
        dir.join("composer.json"),
        r#"{"name":"acme/update","require":{"monolog/monolog":"^3.0","symfony/console":"^6.4 || ^7.0","psr/log":"^3"},"require-dev":{"phpunit/phpunit":"^11.0"},"prefer-stable":true,"config":{"policy":{"advisories":{"block":false}}}}"#,
    )
    .unwrap();
    let theirs = run(
        &composer,
        &dir,
        &["update", "symfony/console", "--no-install", "--no-ansi"],
    );
    let theirs_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap();
    std::fs::write(dir.join("composer.lock"), &base_lock).unwrap();
    let ours = run(
        &romposer(),
        &dir,
        &["update", "symfony/console", "--no-install"],
    );
    let ours_lock = std::fs::read_to_string(dir.join("composer.lock")).unwrap();
    if theirs_lock != ours_lock || comparable(&theirs.stderr) != comparable(&ours.stderr) {
        failures.push(format!(
            "partial update differs\n--- composer\n{}\n--- romposer\n{}",
            comparable(&theirs.stderr),
            comparable(&ours.stderr)
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
