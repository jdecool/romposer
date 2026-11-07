//! `path` and `artifact` repositories and non-zip dists against composer 2.11.1, offline: a
//! monorepo built by the test (a plain path package, one in its own git checkout on a feature
//! branch, a mirrored one with a `versions` option), an artifact directory holding a tar.gz
//! library and a zip metapackage, and `package` repositories with xz and phar dists. Lock,
//! installed.json, installed.php, the vendor/ listing (symlink targets included), stderr and
//! `status` must match; romposer's store manifest must list none of them.

mod common;

use common::{composer, romposer, run_env, temp_dir};
use std::path::Path;
use std::process::Command;

fn sh(dir: &Path, program: &str, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("COPYFILE_DISABLE", "1")
        .env("GIT_AUTHOR_DATE", "2024-01-01T10:00:00Z")
        .env("GIT_COMMITTER_DATE", "2024-01-01T10:00:00Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git(dir: &Path, args: &[&str]) {
    let mut all = vec![
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@example.com",
        "-c",
        "commit.gpgsign=false",
    ];
    all.extend(args);
    sh(dir, "git", &all);
}

fn write(dir: &Path, file: &str, contents: &str) {
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// The project (`proj/`) and the dists shared by both tools (`dists/`).
fn fixture(root: &Path) {
    let proj = root.join("proj");
    let dists = root.join("dists");
    write(
        &proj,
        "packages/a/composer.json",
        r#"{"name":"acme/a","require":{"acme/b":"^1.0@dev","acme/tool":"^1.0","acme/meta":"*"},"autoload":{"psr-4":{"Acme\\A\\":"src/"}},"bin":["bin/a"]}"#,
    );
    write(
        &proj,
        "packages/a/src/A.php",
        "<?php namespace Acme\\A; class A {}\n",
    );
    write(&proj, "packages/a/bin/a", "#!/bin/sh\necho a\n");
    // acme/b: its own checkout, on a feature branch cut from 1.x
    let b = proj.join("packages/b");
    write(
        &b,
        "composer.json",
        r#"{"name":"acme/b","autoload":{"psr-4":{"Acme\\B\\":"src/"}}}"#,
    );
    write(&b, "src/B.php", "<?php namespace Acme\\B; class B {}\n");
    git(&b, &["init", "-q", "-b", "1.x"]);
    git(&b, &["add", "-A"]);
    git(&b, &["commit", "-qm", "one"]);
    git(&b, &["checkout", "-qb", "feature/y"]);
    write(&b, "y.txt", "y\n");
    git(&b, &["add", "-A"]);
    git(&b, &["commit", "-qm", "two"]);
    write(
        &proj,
        "mirrored/c/composer.json",
        r#"{"name":"acme/c","autoload":{"classmap":["lib/"]}}"#,
    );
    write(&proj, "mirrored/c/lib/C.php", "<?php class C {}\n");
    write(
        &proj,
        "mirrored/c/.gitattributes",
        "/ignored export-ignore\n",
    );
    write(&proj, "mirrored/c/ignored/x.txt", "x\n");
    // artifacts: a tar.gz library with a top-level directory, a zip metapackage
    let work = root.join("work");
    write(
        &work,
        "tool-1.0.0/composer.json",
        r#"{"name":"acme/tool","version":"1.0.0","bin":["bin/tool"],"autoload":{"files":["f.php"]}}"#,
    );
    write(&work, "tool-1.0.0/bin/tool", "#!/bin/sh\necho tool\n");
    write(&work, "tool-1.0.0/f.php", "<?php\n");
    sh(&work, "chmod", &["+x", "tool-1.0.0/bin/tool"]);
    std::fs::create_dir_all(proj.join("artifacts")).unwrap();
    sh(
        &work,
        "tar",
        &["-czf", "../proj/artifacts/tool-1.0.0.tar.gz", "tool-1.0.0"],
    );
    write(
        &work,
        "meta/composer.json",
        r#"{"name":"acme/meta","version":"1.0.0","type":"metapackage","require":{"acme/c":"^2.0"}}"#,
    );
    sh(
        &work.join("meta"),
        "zip",
        &["-q", "../../proj/artifacts/meta.zip", "composer.json"],
    );
    write(&proj, "artifacts/notes.zip", "not a zip");
    // xz and phar dists
    write(
        &work,
        "xz/composer.json",
        r#"{"name":"acme/xz","version":"1.0.0"}"#,
    );
    write(&work, "xz/src/X.php", "<?php\n");
    std::fs::create_dir_all(&dists).unwrap();
    sh(
        &work.join("xz"),
        "tar",
        &["-cJf", "../../dists/xz.tar.xz", "."],
    );
    sh(
        &work,
        "php",
        &[
            "-d",
            "phar.readonly=0",
            "-r",
            "$p = new Phar($argv[1]); $p->addFromString('composer.json', '{\"name\":\"acme/ph\",\"version\":\"1.0.0\"}'); $p->addFromString('src/P.php', '<?php');",
            "--",
            dists.join("ph.phar").to_str().unwrap(),
        ],
    );
    let package = |name: &str, kind: &str, file: &str| {
        format!(
            r#"{{"type":"package","package":{{"name":"{name}","version":"1.0.0","dist":{{"type":"{kind}","url":"{}"}}}}}}"#,
            dists.join(file).display()
        )
    };
    write(
        &proj,
        "composer.json",
        &format!(
            r#"{{"name":"acme/proj","repositories":[{{"type":"path","url":"packages/{{a,b}}"}},{{"type":"path","url":"./mirrored/*","options":{{"symlink":false,"versions":{{"acme/c":"2.0.0"}}}}}},{{"type":"artifact","url":"artifacts/"}},{},{},{{"packagist.org":false}}],"minimum-stability":"dev","require":{{"acme/a":"*","acme/xz":"*","acme/ph":"*"}},"config":{{"policy":{{"advisories":{{"block":false}}}}}}}}"#,
            package("acme/xz", "xz", "xz.tar.xz"),
            package("acme/ph", "phar", "ph.phar")
        ),
    );
}

/// vendor/ as sorted lines: path, kind, and the link target or file contents' size.
fn listing(dir: &Path) -> String {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            if rel == "vendor/composer/pcomposer.json" {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_symlink() {
                out.push(format!(
                    "{rel} -> {}",
                    std::fs::read_link(&path).unwrap().display()
                ));
            } else if meta.is_dir() {
                out.push(format!("{rel}/"));
                walk(&path, root, out);
            } else {
                out.push(format!("{rel} {}", meta.len()));
            }
        }
    }
    let mut out = Vec::new();
    walk(&dir.join("vendor"), dir, &mut out);
    out.sort();
    out.join("\n")
}

fn read(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).unwrap_or_default()
}

#[test]
fn monorepo_with_path_artifact_and_archive_dists_matches_composer() {
    let Some(composer) = composer() else { return };
    let root = temp_dir("path");
    fixture(&root);
    let dir = root.join("proj");
    let steps: &[&[&str]] = &[
        &["update"],
        &["status", "-v"],
        &["install"],
        &["status", "-v"],
    ];
    let mut results = Vec::new();
    for (tool, bin) in [("composer", composer.clone()), ("romposer", romposer())] {
        let _ = std::fs::remove_dir_all(dir.join("vendor"));
        let _ = std::fs::remove_file(dir.join("composer.lock"));
        let home = root.join(format!("home-{tool}"));
        let mut outputs = Vec::new();
        let mut snapshots = Vec::new();
        for (i, args) in steps.iter().enumerate() {
            if i == 2 {
                // reinstall from the lock, then dirty the extracted tar package
                std::fs::remove_dir_all(dir.join("vendor")).unwrap();
            }
            if i == 3 {
                write(&dir, "vendor/acme/tool/f.php", "<?php // changed\n");
                write(&dir, "vendor/acme/tool/new.txt", "new\n");
                std::fs::remove_file(dir.join("vendor/acme/tool/bin/tool")).unwrap();
            }
            let mut args = args.to_vec();
            if tool == "composer" {
                args.push("--no-ansi");
            }
            outputs.push(run_env(
                &bin,
                &dir,
                &args,
                &[
                    ("COMPOSER_HOME", home.to_str().unwrap()),
                    ("COMPOSER_ROOT_VERSION", "1.0.0"),
                ],
            ));
            snapshots.push(vec![
                ("composer.lock", read(&dir, "composer.lock")),
                (
                    "installed.json",
                    read(&dir, "vendor/composer/installed.json"),
                ),
                ("installed.php", read(&dir, "vendor/composer/installed.php")),
                (
                    "autoload_static.php",
                    read(&dir, "vendor/composer/autoload_static.php"),
                ),
                ("vendor listing", listing(&dir)),
            ]);
        }
        if tool == "romposer" {
            assert_eq!(read(&dir, "vendor/composer/pcomposer.json"), "{}\n");
            let store = home.join("cache/pcomposer-store/packages");
            assert!(
                !store.exists() || std::fs::read_dir(&store).unwrap().next().is_none(),
                "nothing may be ingested into the store"
            );
        }
        results.push((outputs, snapshots));
    }
    let (theirs, ours) = (&results[0], &results[1]);
    let mut failures = Vec::new();
    for (i, (t, o)) in theirs.0.iter().zip(&ours.0).enumerate() {
        for (what, a, b) in [
            (
                "exit",
                format!("{:?}", t.status.code()),
                format!("{:?}", o.status.code()),
            ),
            (
                "stdout",
                String::from_utf8_lossy(&t.stdout).into(),
                String::from_utf8_lossy(&o.stdout).into(),
            ),
            (
                "stderr",
                String::from_utf8_lossy(&t.stderr).into(),
                String::from_utf8_lossy(&o.stderr).into(),
            ),
        ] {
            if a != b {
                failures.push(format!(
                    "step {i} {:?}: {what} differs\n--- composer\n{a}\n--- romposer\n{b}",
                    steps[i]
                ));
            }
        }
    }
    for (i, (t, o)) in theirs.1.iter().zip(&ours.1).enumerate() {
        for ((file, a), (_, b)) in t.iter().zip(o) {
            if a != b {
                failures.push(format!(
                    "step {i}: {file} differs\n--- composer\n{a}\n--- romposer\n{b}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    let _ = std::fs::remove_dir_all(&root);
}
