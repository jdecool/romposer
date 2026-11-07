//! The inspection commands against composer 2.11.1 on an installed live project:
//! identical stdout and exit codes (`fund` package lists are order-insensitive).

mod common;

use common::{composer, romposer, run, temp_dir};

const PROJECT: &str = r#"{"name":"acme/ro","require":{"monolog/monolog":"^3.0","symfony/console":"^7.0","psr/log":"^3"},"require-dev":{"phpunit/phpunit":"^11.0"},"prefer-stable":true,"config":{"policy":{"advisories":{"block":false}}}}"#;

/// `fund` lists urls and packages in the order the repository answered: compare canonically.
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.iter()
                    .map(|k| format!("{k:?}:{}", canonical(&map[*k])))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        serde_json::Value::Array(items) => {
            let mut parts: Vec<String> = items.iter().map(canonical).collect();
            parts.sort();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

fn sorted_fund_json(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => canonical(&value),
        Err(_) => text.to_string(),
    }
}

#[test]
fn readonly_commands_match_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("readonly");
    std::fs::write(dir.join("composer.json"), PROJECT).unwrap();
    let setup = run(&composer, &dir, &["update", "--no-ansi"]);
    if !setup.status.success() {
        eprintln!("skipping live read-only comparison: composer failed");
        return;
    }
    let _ = run(
        &composer,
        &dir,
        &["require", "psr/container:1.1.2", "--no-ansi", "-q"],
    );
    let cases: &[&[&str]] = &[
        &["licenses"],
        &["licenses", "--format=json"],
        &["licenses", "--format=summary"],
        &["licenses", "--no-dev"],
        &["licenses", "--locked", "--no-dev", "--format=json"],
        &["depends", "psr/log"],
        &["depends", "psr/log", "--tree"],
        &["depends", "psr/log", "--recursive"],
        &["why", "ext-mbstring"],
        &["why-not", "psr/container", "2.0.0"],
        &["prohibits", "psr/log", "^4"],
        &["why-not", "psr/log", "^3"],
        &["why-not", "php", "9"],
        &["why-not", "symfony/console", "7.0.0", "--locked"],
        &["fund"],
        &["fund", "--format=json"],
        &["suggests"],
        &["suggests", "--list"],
        &["suggests", "--all", "--by-suggestion"],
        &["suggests", "--by-package", "--by-suggestion", "--all"],
        &["suggests", "monolog/monolog"],
        &["status"],
        &["show", "--latest"],
        &["show", "--latest", "--direct"],
        &["show", "--latest", "--format=json", "psr/container"],
        &["show", "psr/container", "--latest"],
        &["show", "-a", "psr/log", "--latest"],
        &["outdated"],
        &["outdated", "--format=json"],
        &["outdated", "--strict"],
        &["outdated", "--direct", "--format=json"],
        &["outdated", "--major-only"],
        &["outdated", "--minor-only"],
        &["outdated", "--patch-only"],
        &["outdated", "--sort-by-age"],
        &["outdated", "--ignore", "psr/*"],
        &["outdated", "-D", "--strict"],
        &["show", "--tree"],
        &["show", "--tree", "--format=json"],
        &["show", "--tree", "phpunit/phpunit"],
        &["show", "psr/log", "--tree"],
    ];
    let mut failures = Vec::new();
    for case in cases {
        let mut theirs_args = case.to_vec();
        theirs_args.push("--no-ansi");
        let theirs = run(&composer, &dir, &theirs_args);
        let ours = run(&romposer(), &dir, case);
        let mut t = String::from_utf8_lossy(&theirs.stdout).into_owned();
        let mut o = String::from_utf8_lossy(&ours.stdout).into_owned();
        if case[0] == "fund" {
            t = sorted_fund_json(&t);
            o = sorted_fund_json(&o);
            if case.len() == 1 {
                // the text listing names packages in that order too, and dedups adjacent
                // lists: compare the other lines as a set
                let keep = |l: &&str| !(l.starts_with("  ") && !l.starts_with("    "));
                let mut tl: Vec<&str> = t.lines().filter(keep).collect();
                let mut ol: Vec<&str> = o.lines().filter(keep).collect();
                tl.sort();
                ol.sort();
                t = tl.join("\n");
                o = ol.join("\n");
            }
        }
        if theirs.status.code() != ours.status.code() || t != o {
            failures.push(format!(
                "{case:?}\n--- composer {:?}\n{t}\n--- romposer {:?}\n{o}\n--- romposer stderr\n{}",
                theirs.status.code(),
                ours.status.code(),
                String::from_utf8_lossy(&ours.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
