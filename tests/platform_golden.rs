//! `romposer check-platform-reqs` against composer 2.11.1: same text, same JSON, same exit code.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::fs;

const PROJECTS: &[(&str, &str)] = &[
    (
        "passing",
        r#"{"name":"acme/passing","require":{"php":">=8.0","ext-json":"*"},"require-dev":{"ext-tokenizer":"*"}}"#,
    ),
    (
        "failing",
        r#"{"name":"acme/fail","require":{"php":"^7.0","ext-nope":"*","ext-json":"^9"},"require-dev":{"ext-curl":"*"}}"#,
    ),
    (
        "provided",
        r#"{"name":"acme/prov","require":{"lib-pcre":"*","ext-nope":"^1","php-64bit":"*"},"provide":{"ext-nope":"1.0"}}"#,
    ),
];

#[test]
fn check_platform_reqs_matches_composer() {
    let Some(composer) = composer() else { return };
    let base = temp_dir("platform");
    let mut failures = Vec::new();
    for (name, manifest) in PROJECTS {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("composer.json"), manifest).unwrap();
        let out = run(
            &composer,
            &dir,
            &[
                "update",
                "--no-install",
                "--ignore-platform-reqs",
                "--no-ansi",
            ],
        );
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for flags in [
            &[][..],
            &["--format=json"][..],
            &["--no-dev"][..],
            &["--lock", "--format=json"][..],
            &["--lock", "--no-dev"][..],
        ] {
            let mut args = vec!["check-platform-reqs", "--no-ansi"];
            args.extend_from_slice(flags);
            let theirs = run(&composer, &dir, &args);
            let mut args = vec!["check-platform-reqs"];
            args.extend_from_slice(flags);
            let ours = run(&romposer(), &dir, &args);
            let t = (
                theirs.status.code(),
                String::from_utf8_lossy(&theirs.stdout).into_owned(),
            );
            let o = (
                ours.status.code(),
                String::from_utf8_lossy(&ours.stdout).into_owned(),
            );
            if t != o {
                failures.push(format!(
                    "{name} {flags:?}\n--- composer {:?}\n{}\n--- romposer {:?}\n{}",
                    t.0, t.1, o.0, o.1
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
