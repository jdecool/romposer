//! Scripts and events against composer 2.11.1 on a live project: every event fires in the
//! same order with the same output, `run-script`, `exec` and script aliases included.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::path::Path;

const PROJECT: &str = r#"{
    "name": "acme/scripts",
    "require": {"psr/log": "^3"},
    "bin": ["bin/hello"],
    "config": {"policy": {"advisories": {"block": false}}},
    "scripts": {
        "pre-install-cmd": "echo pre-install-cmd",
        "post-install-cmd": ["echo post-install-cmd", "@hello world"],
        "pre-update-cmd": "echo pre-update-cmd",
        "post-update-cmd": "echo post-update-cmd",
        "pre-autoload-dump": "echo pre-autoload-dump",
        "post-autoload-dump": "echo post-autoload-dump",
        "pre-package-install": "echo pre-package-install",
        "post-package-install": "echo post-package-install",
        "pre-package-update": "echo pre-package-update",
        "post-package-update": "echo post-package-update",
        "pre-package-uninstall": "echo pre-package-uninstall",
        "post-package-uninstall": "echo post-package-uninstall",
        "pre-operations-exec": "echo pre-operations-exec",
        "pre-status-cmd": "echo pre-status-cmd",
        "post-status-cmd": "echo post-status-cmd",
        "post-root-package-install": "echo post-root-package-install",
        "post-create-project-cmd": "echo post-create-project-cmd",
        "pre-archive-cmd": "echo pre-archive-cmd",
        "post-archive-cmd": "echo post-archive-cmd",
        "hello": "echo hello",
        "env": "sh -c 'echo dev=$COMPOSER_DEV_MODE; echo $PATH | tr : \"\\n\" | head -1'",
        "chain": ["@hello", "@env"],
        "setenv": ["@putenv FOO=bar baz", "sh -c 'echo FOO=$FOO'", "@putenv FOO", "sh -c 'echo FOO=$FOO'"],
        "phpcode": "@php -r 'echo \"php ok\\n\";'",
        "fail": "exit 3",
        "callfail": "@fail",
        "viacomposer": "@composer hello",
        "composer-prefix": "composer hello extra",
        "missing": "@nope",
        "cls": "Acme\\Foo::bar",
        "noargs": "echo noargs @no_additional_args",
        "addargs": "echo begin @additional_args end",
        "quoted": "@addargs 'one two' three",
        "slow": "sleep 5"
    },
    "scripts-descriptions": {"hello": "Says hello"}
}
"#;

fn comparable(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|l| {
            !l.contains("could not detect the root package")
                && !l.starts_with("  - ")
                && !l.starts_with("pcomposer:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compare(dir: &Path, composer: &Path, args: &[&str], loose: bool) -> Option<String> {
    let mut theirs_args = args.to_vec();
    theirs_args.insert(0, "--no-ansi");
    let theirs = run(composer, dir, &theirs_args);
    let ours = run(&romposer(), dir, args);
    let (t_out, o_out) = (
        String::from_utf8_lossy(&theirs.stdout).into_owned(),
        String::from_utf8_lossy(&ours.stdout).into_owned(),
    );
    let (t_err, o_err) = if loose {
        (String::new(), String::new())
    } else {
        (comparable(&theirs.stderr), comparable(&ours.stderr))
    };
    if theirs.status.code() != ours.status.code() || t_out != o_out || t_err != o_err {
        return Some(format!(
            "{args:?}\n--- composer {:?}\n{t_out}\n{t_err}\n--- romposer {:?}\n{o_out}\n{o_err}\n--- romposer raw stderr\n{}",
            theirs.status.code(),
            ours.status.code(),
            String::from_utf8_lossy(&ours.stderr)
        ));
    }
    None
}

/// Both tools run `args` from the same project state; the state they leave is kept for the next case.
fn compare_mutating(dir: &Path, composer: &Path, args: &[&str]) -> Option<String> {
    let snapshot = temp_dir("scripts-snapshot");
    let _ = std::fs::remove_dir_all(&snapshot);
    copy_dir(dir, &snapshot);
    let mut theirs_args = args.to_vec();
    theirs_args.insert(0, "--no-ansi");
    let theirs = run(composer, dir, &theirs_args);
    let theirs_json = std::fs::read_to_string(dir.join("composer.json")).unwrap();
    let _ = std::fs::remove_dir_all(dir);
    copy_dir(&snapshot, dir);
    let ours = run(&romposer(), dir, args);
    let ours_json = std::fs::read_to_string(dir.join("composer.json")).unwrap();
    if theirs_json != ours_json {
        return Some(format!(
            "{args:?}: composer.json differs
{theirs_json}
{ours_json}"
        ));
    }
    let (t, o) = (comparable(&theirs.stderr), comparable(&ours.stderr));
    let (t_out, o_out) = (
        String::from_utf8_lossy(&theirs.stdout).into_owned(),
        String::from_utf8_lossy(&ours.stdout).into_owned(),
    );
    if theirs.status.code() != ours.status.code() || t != o || t_out != o_out {
        return Some(format!(
            "{args:?}\n--- composer {:?}\n{t_out}\n{t}\n--- romposer {:?}\n{o_out}\n{o}",
            theirs.status.code(),
            ours.status.code()
        ));
    }
    None
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn scripts_and_events_match_composer() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("scripts");
    std::fs::write(dir.join("composer.json"), PROJECT).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(
        dir.join("bin/hello"),
        "#!/bin/sh\necho \"hello from bin $*\"\n",
    )
    .unwrap();
    let probe = run(
        &composer,
        &dir,
        &["validate", "--no-check-publish", "--no-ansi"],
    );
    if !probe.status.success() {
        eprintln!("skipping live scripts comparison: composer failed");
        return;
    }
    let mut failures = Vec::new();
    // the project lifecycle: every installer event, in order
    for args in [
        &["update"][..],
        &["install"][..],
        &["require", "psr/log:^2"][..],
        &["remove", "psr/log"][..],
        &["require", "psr/log:^3", "--no-scripts"][..],
    ] {
        failures.extend(compare_mutating(&dir, &composer, args));
    }
    let _ = std::fs::remove_dir_all(dir.join("vendor"));
    failures.extend(compare(&dir, &composer, &["install", "--dry-run"], false));
    failures.extend(compare_mutating(
        &dir,
        &composer,
        &["install", "--no-scripts"],
    ));
    let _ = std::fs::remove_dir_all(dir.join("vendor"));
    failures.extend(compare_mutating(&dir, &composer, &["install"]));
    let cases: &[(&[&str], bool)] = &[
        (&["dump-autoload"], false),
        (&["dump-autoload", "--no-scripts"], false),
        (&["dump-autoload", "--no-dev"], false),
        (&["status"], false),
        (&["run-script", "hello"], false),
        (&["run", "hello"], false),
        (&["run-script", "--list"], false),
        (&["run-script", "-l"], false),
        (&["run-script", "env"], false),
        (&["run-script", "env", "--no-dev"], false),
        (&["run-script", "chain"], false),
        (&["run-script", "setenv"], false),
        (&["run-script", "phpcode"], false),
        (&["run-script", "fail"], false),
        (&["run-script", "callfail"], false),
        (&["run-script", "viacomposer"], false),
        (&["run-script", "composer-prefix"], false),
        (&["run-script", "missing"], false),
        (&["run-script", "cls"], true),
        (&["run-script", "nope"], true),
        (&["run-script", "pre-package-install"], true),
        (&["run-script"], true),
        (&["run-script", "hello", "--", "a", "b c"], false),
        (&["run-script", "addargs", "--", "x", "y z"], false),
        (&["run-script", "noargs", "--", "x"], false),
        (&["run-script", "quoted", "--", "four"], false),
        (&["run-script", "slow", "--timeout", "1"], true),
        (&["run-script", "slow", "--timeout", "x"], true),
        (&["run-script", "-v", "chain"], false),
        (&["exec", "--list"], false),
        (&["exec", "-l"], false),
        (&["exec", "hello", "--", "a"], false),
        (&["exec", "nope"], false),
        (&["hello"], false),
        (&["hello", "a", "b c"], false),
        (&["addargs", "x", "--dev"], false),
        (&["env", "--no-dev"], false),
        (&["quoted", "four"], false),
        (&["nope"], true),
    ];
    for (args, loose) in cases {
        failures.extend(compare(&dir, &composer, args, *loose));
    }

    // failing listeners: the script's exit code ends the command, composer.json is reverted
    // only when require fails before dependency resolution
    let failing = PROJECT
        .replace(r#""echo pre-update-cmd""#, r#""exit 5""#)
        .replace(
            r#""echo post-autoload-dump""#,
            r#"["@hello", "exit 4", "echo never"]"#,
        );
    std::fs::write(dir.join("composer.json"), failing).unwrap();
    for args in [
        &["require", "psr/log:^2"][..],
        &["dump-autoload"][..],
        &["install"][..],
        &["-q", "run-script", "chain"][..],
    ] {
        failures.extend(compare_mutating(&dir, &composer, args));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
