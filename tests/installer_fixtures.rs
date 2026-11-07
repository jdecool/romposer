//! Composer's installer fixtures run in-process. The pass rate is the project's first number:
//! every fixture must land exactly where `tests/installer-fixtures-known-failures.txt` says,
//! so an unexpected pass or failure turns CI red. Regenerate the list with
//! `UPDATE_KNOWN_FAILURES=1 cargo test --test installer_fixtures`.

use romposer::fixtures;
use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn installer_fixtures_pass_rate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = root.join("reference/composer/tests/Composer/Test/Fixtures/installer");
    let files = fixtures::list(&dir);
    assert!(
        files.len() > 200,
        "expected Composer's fixtures in the submodule, found {}",
        files.len()
    );
    let known_path = root.join("tests/installer-fixtures-known-failures.txt");
    let known: BTreeSet<String> = std::fs::read_to_string(&known_path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();

    let mut failed: BTreeSet<String> = BTreeSet::new();
    let mut details: Vec<String> = Vec::new();
    for file in &files {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        let contents = std::fs::read_to_string(file).unwrap();
        let outcome = match fixtures::parse(&name, &contents) {
            Ok(fixture) => fixtures::run(&fixture),
            Err(e) => fixtures::Outcome {
                passed: false,
                detail: e,
            },
        };
        if !outcome.passed {
            if std::env::var("FIXTURE_DETAILS").is_ok_and(|v| name.contains(&v)) {
                println!("===== {name}\n{}", outcome.detail);
            }
            failed.insert(name.clone());
            details.push(format!(
                "{name}: {}",
                outcome.detail.lines().next().unwrap_or("")
            ));
        }
    }
    let passing = files.len() - failed.len();
    println!("{passing}/{} fixtures passing", files.len());

    if std::env::var("UPDATE_KNOWN_FAILURES").is_ok() {
        let mut text = String::from(
            "# Composer installer fixtures romposer does not pass yet, one per line.\n",
        );
        for name in &failed {
            text.push_str(name);
            text.push('\n');
        }
        std::fs::write(&known_path, text).unwrap();
        return;
    }

    let unexpected_failures: Vec<&String> = failed.difference(&known).collect();
    let unexpected_passes: Vec<&String> = known.difference(&failed).collect();
    assert!(
        unexpected_failures.is_empty() && unexpected_passes.is_empty(),
        "fixture results drifted from the known-failure list\nunexpected failures: {unexpected_failures:?}\nunexpected passes (remove them from the list): {unexpected_passes:?}\n\n{}",
        details
            .iter()
            .filter(|d| unexpected_failures
                .iter()
                .any(|f| d.starts_with(f.as_str())))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
