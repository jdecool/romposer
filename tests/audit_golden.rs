//! `romposer audit` against composer 2.11.1 on live Packagist: identical stdout and exit
//! codes for a project pinning a vulnerable guzzle and an abandoned swiftmailer, in every
//! format, from the lock file and from the installed packages, with ignore rules in both
//! the legacy `config.audit` and the `config.policy` shapes.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::path::Path;

const PROJECT: &str = r#"{"name":"acme/au","require":{"guzzlehttp/guzzle":"6.5.0","swiftmailer/swiftmailer":"^6.0"},"require-dev":{"psr/log":"^1"},"prefer-stable":true,"config":{"policy":{"advisories":{"block":false}},"audit":{"ignore":["CVE-2022-31091","PKSA-k1b4-kshy-xgbh"]}}}"#;

/// No `policy` key: the legacy `audit.*` rules apply, with every value shape.
const LEGACY: &str = r#"{"name":"acme/au","require":{"guzzlehttp/guzzle":"6.5.0","swiftmailer/swiftmailer":"^6.0"},"require-dev":{"psr/log":"^1"},"prefer-stable":true,"config":{"audit":{"ignore":{"CVE-2022-31091":"not exploitable","PKSA-k1b4-kshy-xgbh":{"apply":"audit"},"GHSA-cwmx-hcrq-mhc3":null},"ignore-severity":["medium"],"abandoned":"report","ignore-abandoned":["swiftmailer/*"]}}}"#;

/// `policy.advisories` set: the legacy `audit.ignore` is not read any more.
const POLICY: &str = r#"{"name":"acme/au","require":{"guzzlehttp/guzzle":"6.5.0","swiftmailer/swiftmailer":"^6.0"},"require-dev":{"psr/log":"^1"},"prefer-stable":true,"config":{"policy":{"advisories":{"block":false,"ignore-id":{"PKSA-k1b4-kshy-xgbh":{"reason":"x","on-audit":false},"CVE-2022-31091":"not exploitable"},"ignore":["guzzlehttp/psr7"],"ignore-severity":{"high":null}},"abandoned":{"audit":"report"}},"audit":{"ignore":["CVE-2022-31042"]}}}"#;

const CLEAN: &str = r#"{"name":"acme/au3","require":{"psr/log":"^3"},"config":{"policy":{"advisories":{"block":false}}}}"#;

fn compare(dir: &Path, composer: &Path, args: &[&str], failures: &mut Vec<String>) {
    let mut theirs_args = args.to_vec();
    theirs_args.push("--no-ansi");
    let theirs = run(composer, dir, &theirs_args);
    let ours = run(&romposer(), dir, args);
    let t = String::from_utf8_lossy(&theirs.stdout);
    let o = String::from_utf8_lossy(&ours.stdout);
    if theirs.status.code() != ours.status.code() || t != o {
        failures.push(format!(
            "{args:?}\n--- composer {:?}\n{t}\n--- romposer {:?}\n{o}\n--- romposer stderr\n{}",
            theirs.status.code(),
            ours.status.code(),
            String::from_utf8_lossy(&ours.stderr)
        ));
    }
}

#[test]
fn audit_matches_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("audit");
    std::fs::write(dir.join("composer.json"), PROJECT).unwrap();
    let setup = run(
        &composer,
        &dir,
        &["update", "--no-install", "--no-ansi", "-q"],
    );
    if !setup.status.success() {
        eprintln!("skipping live audit comparison: composer failed");
        return;
    }
    let mut failures = Vec::new();
    // the post-update audit summary, from the lock composer wrote
    let _ = std::fs::remove_dir_all(dir.join("vendor"));
    compare(&dir, &composer, &["update", "--no-install"], &mut failures);
    for args in [
        &["audit", "--locked"][..],
        &["audit", "--locked", "--format=json"],
        &["audit", "--locked", "--format=plain"],
        &["audit", "--locked", "--format=summary"],
        &["audit", "--locked", "--no-dev", "--format=json"],
        &["audit", "--locked", "--abandoned=report"],
        &["audit", "--locked", "--abandoned=ignore", "--format=plain"],
        &[
            "audit",
            "--locked",
            "--ignore-severity=high",
            "--format=json",
        ],
        &["audit", "--locked", "--direct", "--format=plain"],
        &["audit"],
    ] {
        compare(&dir, &composer, args, &mut failures);
    }
    let installed = run(&composer, &dir, &["install", "--no-ansi", "-q"]);
    if installed.status.success() {
        for args in [
            &["audit"][..],
            &["audit", "--no-dev", "--format=json"],
            &["audit", "--format=summary"],
            &["install", "--audit"],
            &["install", "--audit", "--audit-format=plain"],
        ] {
            compare(&dir, &composer, args, &mut failures);
        }
    }
    for project in [LEGACY, POLICY] {
        std::fs::write(dir.join("composer.json"), project).unwrap();
        for args in [
            &["audit", "--locked", "--format=json"][..],
            &["audit", "--locked", "--format=summary"],
            &["audit", "--locked"],
            &["audit", "--locked", "--abandoned=fail", "--format=plain"],
        ] {
            compare(&dir, &composer, args, &mut failures);
        }
    }
    // nothing to report
    let clean = temp_dir("audit-clean");
    std::fs::write(clean.join("composer.json"), CLEAN).unwrap();
    let _ = run(
        &composer,
        &clean,
        &["update", "--no-install", "--no-ansi", "-q"],
    );
    for args in [
        &["audit", "--locked"][..],
        &["audit", "--locked", "--format=json"],
        &["audit", "--locked", "--format=summary"],
        &["audit"],
    ] {
        compare(&clean, &composer, args, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
