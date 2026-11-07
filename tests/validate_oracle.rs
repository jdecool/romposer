//! `romposer validate` against `composer validate` 2.11.1: same exit code on every
//! manifest, and the same report lines whenever Composer does not die with an exception.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::fs;

const MANIFESTS: &[(&str, &str)] = &[
    (
        "valid",
        r#"{"name":"a/b","description":"d","license":"MIT","require":{"php":"^8.1"}}"#,
    ),
    (
        "empty object",
        r#"{"name":"a/b","description":"d","require":{},"license":"MIT"}"#,
    ),
    (
        "unknown prop",
        r#"{"name":"a/b","description":"d","foo":1,"license":"MIT"}"#,
    ),
    ("no name", r#"{"description":"d","license":"MIT"}"#),
    ("no license", r#"{"name":"a/b","description":"d"}"#),
    (
        "caps name",
        r#"{"name":"Acme/Foo","description":"d","license":"MIT"}"#,
    ),
    ("bad type name", r#"{"name": 123}"#),
    (
        "bad constraint",
        r#"{"name":"a/b","description":"d","require":{"foo/bar":"^^1"}}"#,
    ),
    (
        "deprecated license",
        r#"{"name":"a/b","description":"d","license":"GPL-2.0"}"#,
    ),
    (
        "invalid license",
        r#"{"name":"a/b","description":"d","license":"Foo"}"#,
    ),
    ("syntax", r#"{"name": }"#),
    (
        "version warn",
        r#"{"name":"a/b","description":"d","license":"MIT","version":"1.0.0","require":{"php":">=8","foo/bar":"*"}}"#,
    ),
    (
        "dup key",
        r#"{"name":"a/b","description":"d","license":"MIT","name":"a/c"}"#,
    ),
    (
        "bad author",
        r#"{"name":"a/b","description":"d","license":"MIT","type":"composer-installer","authors":[{"name":"x","email":"nope","homepage":"ftp://x"}]}"#,
    ),
    (
        "self require",
        r#"{"name":"a/b","description":"d","license":"MIT","require":{"a/b":"1.0"},"conflict":{"c/d":"1.0"},"replace":{"c/d":"1.0"}}"#,
    ),
    (
        "exact and impossible",
        r#"{"name":"a/b","description":"d","license":"MIT","require":{"foo/bar":"1.2.3","baz/qux":">2 <1"},"require-dev":{"foo/bar":"1.2.3"},"provide":{"foo/bar":"*"}}"#,
    ),
    (
        "branch alias",
        r#"{"name":"a/b","description":"d","license":"MIT","extra":{"branch-alias":{"dev-main":"1.x-dev","dev-foo":"nope","2.x-dev":"1.x-dev"}}}"#,
    ),
    (
        "psr4 prefix",
        r#"{"name":"a/b","description":"d","license":"MIT","autoload":{"psr-4":{"Foo":"src/"}}}"#,
    ),
    (
        "loader errors",
        r#"{"name":"a/b","description":"d","license":"MIT","minimum-stability":"rc","autoload":{"psr-0":{"":"x/"},"nope":[]},"scripts-descriptions":{"x":"y"},"support":{"email":"bad","irc":"http://x"},"keywords":["ok","not ok!"]}"#,
    ),
    (
        "commit ref",
        r#"{"name":"a/b","description":"d","license":["MIT","Apache-2.0"],"require":{"foo/bar":"dev-main#abc"},"replace":{"foo/bar":"*"}}"#,
    ),
];

fn report_lines(stdout: &[u8], stderr: &[u8]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for text in [stdout, stderr] {
        for line in String::from_utf8_lossy(text).lines() {
            let line = line.trim_end();
            if line.is_empty() || line.contains("could not detect the root package") {
                continue;
            }
            lines.push(line.to_string());
        }
    }
    lines.sort();
    lines.dedup();
    lines
}

#[test]
fn exit_codes_and_reports_match_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("validate");
    let mut failures = Vec::new();
    for (name, manifest) in MANIFESTS {
        fs::write(dir.join("composer.json"), manifest).unwrap();
        for flags in [
            &[][..],
            &["--strict"][..],
            &["--no-check-publish", "--no-check-all"][..],
        ] {
            let mut args = vec!["validate", "--no-ansi"];
            args.extend_from_slice(flags);
            let theirs = run(&composer, &dir, &args);
            let ours = run(&romposer(), &dir, &args);
            if theirs.status.code() != ours.status.code() {
                failures.push(format!(
                    "{name} {flags:?}: composer exit {:?}, romposer exit {:?}\n--- composer\n{}\n--- romposer\n{}",
                    theirs.status.code(),
                    ours.status.code(),
                    String::from_utf8_lossy(&theirs.stderr),
                    String::from_utf8_lossy(&ours.stderr)
                ));
                continue;
            }
            // Exception output (exit 1 with a trace) is human-readable only.
            if theirs.status.code() != Some(1) {
                let t = report_lines(&theirs.stdout, &theirs.stderr);
                let o = report_lines(&ours.stdout, &ours.stderr);
                if t != o {
                    failures.push(format!(
                        "{name} {flags:?}: report differs\n--- composer\n{}\n--- romposer\n{}",
                        t.join("\n"),
                        o.join("\n")
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn lock_freshness_and_missing_requirements_match_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("validate-lock");
    let manifest = r#"{"name":"a/b","description":"d","license":"MIT","require":{"php":">=7.4"},"config":{"platform":{"php":"8.1.0"}}}"#;
    fs::write(dir.join("composer.json"), manifest).unwrap();
    let out = run(&composer, &dir, &["update", "--no-install", "--no-ansi"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cases = [
        ("fresh", manifest.to_string(), vec!["validate", "--no-ansi"]),
        (
            "stale",
            manifest.replace(">=7.4", ">=8.0"),
            vec!["validate", "--no-ansi"],
        ),
        (
            "stale no-check-lock",
            manifest.replace(">=7.4", ">=8.0"),
            vec!["validate", "--no-ansi", "--no-check-lock"],
        ),
        (
            "missing package",
            manifest.replace(r#""php":">=7.4""#, r#""php":">=7.4","foo/bar":"^1.0""#),
            vec!["validate", "--no-ansi"],
        ),
        (
            "config lock false",
            manifest
                .replace(r#""config":{"#, r#""config":{"lock":false,"#)
                .replace(">=7.4", ">=8.0"),
            vec!["validate", "--no-ansi"],
        ),
        (
            "config lock false forced",
            manifest
                .replace(r#""config":{"#, r#""config":{"lock":false,"#)
                .replace(">=7.4", ">=8.0"),
            vec!["validate", "--no-ansi", "--check-lock"],
        ),
    ];
    let mut failures = Vec::new();
    for (name, manifest, args) in cases {
        fs::write(dir.join("composer.json"), &manifest).unwrap();
        let theirs = run(&composer, &dir, &args);
        let ours = run(&romposer(), &dir, &args);
        let t = (
            theirs.status.code(),
            report_lines(&theirs.stdout, &theirs.stderr),
        );
        let o = (ours.status.code(), report_lines(&ours.stdout, &ours.stderr));
        if t != o {
            failures.push(format!("{name}: composer {t:?}\nromposer {o:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn missing_file_exits_3() {
    let dir = temp_dir("validate-missing");
    let out = run(&romposer(), &dir, &["validate"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "./composer.json not found."
    );
}
