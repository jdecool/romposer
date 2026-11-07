//! `romposer create-project` against composer 2.11.1 on live Packagist: identical
//! composer.json, composer.lock, file listing (romposer's store manifest aside), stdout,
//! exit code, and stderr apart from download and install progress lines.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::path::Path;

fn listing(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            if rel == ".composer-home" || rel.ends_with("vendor/composer/pcomposer.json") {
                continue;
            }
            out.push(rel);
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn comparable(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|l| {
            !l.contains("could not detect the root package")
                && !l.starts_with("  - Downloading")
                && !l.starts_with("  - Installing")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read(dir: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(file)).ok()
}

fn compare(
    dir: &Path,
    composer: &Path,
    setup: &dyn Fn(&Path),
    args: &[&str],
    project: &str,
) -> Vec<String> {
    let mut failures = Vec::new();
    let reset = || {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        setup(dir);
    };
    reset();
    let mut theirs_args = args.to_vec();
    theirs_args.push("--no-ansi");
    let theirs = run(composer, dir, &theirs_args);
    let p = dir.join(project);
    let theirs_state = (
        listing(dir),
        read(&p, "composer.json"),
        read(&p, "composer.lock"),
    );
    reset();
    let ours = run(&romposer(), dir, args);
    let ours_state = (
        listing(dir),
        read(&p, "composer.json"),
        read(&p, "composer.lock"),
    );
    if theirs.status.code() != ours.status.code() {
        failures.push(format!(
            "{args:?}: exit {:?} vs {:?}\n{}\n{}",
            theirs.status.code(),
            ours.status.code(),
            String::from_utf8_lossy(&theirs.stderr),
            String::from_utf8_lossy(&ours.stderr)
        ));
    }
    if theirs_state.0 != ours_state.0 {
        let only_theirs: Vec<_> = theirs_state
            .0
            .iter()
            .filter(|f| !ours_state.0.contains(f))
            .collect();
        let only_ours: Vec<_> = ours_state
            .0
            .iter()
            .filter(|f| !theirs_state.0.contains(f))
            .collect();
        failures.push(format!("{args:?}: listing differs\nonly composer: {only_theirs:?}\nonly romposer: {only_ours:?}"));
    }
    if theirs_state.1 != ours_state.1 {
        failures.push(format!(
            "{args:?}: composer.json differs\n--- composer\n{:?}\n--- romposer\n{:?}",
            theirs_state.1, ours_state.1
        ));
    }
    if theirs_state.2 != ours_state.2 {
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
fn create_project_matches_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("create-project");
    let probe = run(&composer, &dir, &["show", "-a", "psr/log", "--no-ansi"]);
    if !probe.status.success() {
        eprintln!("skipping live create-project comparison: composer failed");
        return;
    }
    let none = |_: &Path| {};
    let occupied = |dir: &Path| {
        std::fs::create_dir_all(dir.join("taken")).unwrap();
        std::fs::write(dir.join("taken/file"), "x").unwrap();
    };
    let project = |dir: &Path| {
        std::fs::write(
            dir.join("composer.json"),
            "{\n    \"name\": \"acme/app\",\n    \"require\": {\n        \"psr/log\": \"^3\"\n    },\n    \"scripts\": {\n        \"post-root-package-install\": \"echo root-installed\",\n        \"post-create-project-cmd\": \"echo created\"\n    },\n    \"config\": {\"policy\": {\"advisories\": {\"block\": false}}}\n}\n",
        )
        .unwrap();
    };
    let mut failures = Vec::new();
    for (setup, args, project_dir) in [
        (
            &none as &dyn Fn(&Path),
            &["create-project", "symfony/skeleton", "app", "--no-plugins"][..],
            "app",
        ),
        (
            &none,
            &["create-project", "psr/log", "log", "3.0.0", "--no-install"][..],
            "log",
        ),
        (
            &none,
            &["create-project", "psr/log:^2", "--no-install"][..],
            "log",
        ),
        (
            &none,
            &[
                "create-project",
                "psr/log",
                "lg",
                "--stability",
                "dev",
                "--no-install",
            ][..],
            "lg",
        ),
        (
            &none,
            &[
                "create-project",
                "psr/log",
                "log",
                "3.0.0",
                "--no-install",
                "--require",
                "psr/container:^2",
                "--add-repository",
                "--repository",
                "https://repo.packagist.org",
            ][..],
            "log",
        ),
        (
            &occupied,
            &["create-project", "psr/log", "taken"][..],
            "taken",
        ),
        (
            &none,
            &["create-project", "acme/does-not-exist-anywhere"][..],
            "x",
        ),
        (
            &none,
            &["create-project", "psr/log", "x", "--stability", "foo"][..],
            "x",
        ),
        (&project, &["create-project"][..], "."),
    ] {
        failures.extend(compare(&dir, &composer, setup, args, project_dir));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
