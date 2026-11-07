//! The store as the default install path: files come from pcomposer's store layout,
//! hard links share inodes with it, the manifest has pcomposer's shape, the generated
//! autoload files are still Composer's, and pcomposer itself reads romposer's manifest.

mod common;

use common::{composer, romposer, run, run_env, temp_dir};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MANIFEST: &str = r#"{
    "name": "acme/store-golden",
    "description": "store fixture",
    "license": "MIT",
    "require": {
        "psr/log": "^3.0",
        "nikic/php-parser": "^5.0"
    }
}
"#;

fn prepare(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let composer = composer().expect("composer 2.11.1");
    let base = temp_dir(name);
    let theirs = base.join("composer");
    let ours = base.join("romposer");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("composer.json"), MANIFEST).unwrap();
    let out = run(&composer, &theirs, &["update", "--no-install", "--no-ansi"]);
    assert!(
        out.status.success(),
        "composer update failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::create_dir_all(&ours).unwrap();
    fs::copy(theirs.join("composer.json"), ours.join("composer.json")).unwrap();
    fs::copy(theirs.join("composer.lock"), ours.join("composer.lock")).unwrap();
    (base, theirs, ours)
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn hardlinked_install_shares_inodes_with_the_store_and_matches_composer_content() {
    if composer().is_none() {
        return;
    }
    let (base, theirs, ours) = prepare("store-golden");
    let store = base.join("store");
    let out = run(
        &composer().unwrap(),
        &theirs,
        &["install", "--no-scripts", "--no-plugins", "--no-ansi"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = run_env(
        &romposer(),
        &ours,
        &["install"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("  - Installing psr/log (3.0.2): Linking from store\n")
            || stderr.contains("): Linking from store\n"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("(cached)"),
        "first run must ingest: {stderr}"
    );

    // store layout
    assert!(store.join("index/psr").read_dir().unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("log@3.")
    }));
    let mut store_files = Vec::new();
    for shard in fs::read_dir(store.join("files")).unwrap().flatten() {
        for file in fs::read_dir(shard.path()).unwrap().flatten() {
            let mode = file.metadata().unwrap().permissions().mode() & 0o777;
            let name = file.file_name().to_string_lossy().into_owned();
            assert_eq!(
                mode,
                if name.ends_with("-x") { 0o555 } else { 0o444 },
                "{name}"
            );
            store_files.push(file.path());
        }
    }
    assert!(!store_files.is_empty());

    // same files and contents as Composer, hard-linked to the store (0444/0555), bins private
    let theirs_files = files_under(&theirs.join("vendor"));
    let ours_files = files_under(&ours.join("vendor"));
    let rel = |root: &Path, files: &[PathBuf]| -> Vec<String> {
        files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .filter(|f| f != "composer/pcomposer.json")
            .collect()
    };
    assert_eq!(
        rel(&theirs.join("vendor"), &theirs_files),
        rel(&ours.join("vendor"), &ours_files)
    );
    let store_inodes: std::collections::HashSet<u64> = store_files
        .iter()
        .map(|f| fs::metadata(f).unwrap().ino())
        .collect();
    let mut linked = 0;
    for file in &ours_files {
        let rel_path = file
            .strip_prefix(ours.join("vendor"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if rel_path.starts_with("composer/")
            || rel_path == "autoload.php"
            || rel_path.starts_with("bin/")
        {
            continue;
        }
        assert_eq!(
            fs::read(file).unwrap(),
            fs::read(theirs.join("vendor").join(&rel_path)).unwrap(),
            "{rel_path}"
        );
        let meta = fs::metadata(file).unwrap();
        if rel_path == "nikic/php-parser/bin/php-parse" {
            assert!(
                !store_inodes.contains(&meta.ino()),
                "bins are private copies"
            );
            assert_eq!(meta.permissions().mode() & 0o777, 0o755);
            continue;
        }
        assert!(
            store_inodes.contains(&meta.ino()),
            "{rel_path} is not linked from the store"
        );
        assert!(meta.nlink() >= 2);
        assert_eq!(
            meta.permissions().mode() & 0o777,
            if meta.permissions().mode() & 0o111 != 0 {
                0o555
            } else {
                0o444
            },
            "{rel_path}"
        );
        linked += 1;
    }
    assert!(linked > 10);

    // Composer's generated files are untouched by the store
    for file in [
        "vendor/autoload.php",
        "vendor/composer/installed.json",
        "vendor/composer/autoload_static.php",
        "vendor/composer/installed.php",
        "vendor/bin/php-parse",
    ] {
        assert_eq!(
            fs::read_to_string(theirs.join(file)).unwrap(),
            fs::read_to_string(ours.join(file)).unwrap(),
            "{file}"
        );
    }

    // manifest in pcomposer's shape
    let manifest = fs::read_to_string(ours.join("vendor/composer/pcomposer.json")).unwrap();
    assert!(manifest.starts_with("{\n    \"nikic/php-parser\": {\n        \"name\": \"nikic/php-parser\",\n        \"version\": \"v5."), "{manifest}");
    assert!(
        manifest.contains("        \"strategy\": \"hardlink\"\n"),
        "{manifest}"
    );
    assert!(manifest.ends_with("}\n"));

    // second project on the same store: no download, "(cached)"
    let again = base.join("again");
    fs::create_dir_all(&again).unwrap();
    fs::copy(ours.join("composer.json"), again.join("composer.json")).unwrap();
    fs::copy(ours.join("composer.lock"), again.join("composer.lock")).unwrap();
    let out = run_env(
        &romposer(),
        &again,
        &["install"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Linking from store (cached)"), "{stderr}");
    assert!(!stderr.contains("Downloading"), "{stderr}");
    assert!(
        !again.join(".composer-home/cache/files").exists(),
        "no zip cache when the store is active"
    );

    // store commands
    let out = run_env(
        &romposer(),
        &ours,
        &["store", "status"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout.starts_with("+------------------+"), "{stdout}");
    assert!(stdout.contains("| nikic/php-parser | v5."), "{stdout}");
    assert!(
        stdout.contains(&format!("Store: {} (", store.display())),
        "{stdout}"
    );
    let out = run_env(
        &romposer(),
        &ours,
        &["store", "verify"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).ends_with(" files checked, 0 corrupted\n"));
    let out = run_env(
        &romposer(),
        &ours,
        &["store", "unlink", "psr/log"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "psr/log is now a private copy.\n"
    );
    let meta = fs::metadata(ours.join("vendor/psr/log/src/LoggerInterface.php")).unwrap();
    assert!(!store_inodes.contains(&meta.ino()));
    assert_eq!(meta.permissions().mode() & 0o777, 0o644);
    assert!(
        fs::read_to_string(ours.join("vendor/composer/pcomposer.json"))
            .unwrap()
            .contains("\"strategy\": \"copy\"")
    );
    let out = run_env(
        &romposer(),
        &ours,
        &["store", "unlink", "psr/log"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "psr/log is installed with the copy strategy, nothing to unlink.\n"
    );
    let out = run_env(
        &romposer(),
        &ours,
        &["store", "unlink", "nope/nope"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(1));
    let out = run_env(
        &romposer(),
        &ours,
        &["store", "prune", "--older-than=1x"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("Invalid --older-than \"1x\""));
    let out = run_env(
        &romposer(),
        &ours,
        &["store", "prune", "--dry-run"],
        &[("PCOMPOSER_STORE", store.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Would remove 0 index entries and 0 store files"
    );

    // pcomposer's own status command reads the manifest romposer wrote, when its sources are at hand
    if let Ok(src) = std::env::var("PCOMPOSER_SRC") {
        let composer = composer().unwrap();
        let home = ours.join(".composer-home");
        for args in [
            vec!["global", "config", "repositories.pcomposer", "path", &src],
            vec![
                "global",
                "config",
                "allow-plugins.jdecool/pcomposer",
                "true",
            ],
            vec![
                "global",
                "require",
                "jdecool/pcomposer",
                "@dev",
                "--no-ansi",
            ],
        ] {
            let out = run(&composer, &ours, &args);
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert!(home.join("vendor/jdecool/pcomposer").exists());
        let out = run_env(
            &composer,
            &ours,
            &["pcomposer:status", "--no-ansi"],
            &[("PCOMPOSER_STORE", store.to_str().unwrap())],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("| nikic/php-parser | v5."), "{stdout}");
        assert!(
            stdout.contains("| psr/log          | 3.0.2   | copy     |"),
            "{stdout}"
        );
    }
}

#[test]
fn disable_switch_gives_the_plain_path_and_plugins_are_copied() {
    if composer().is_none() {
        return;
    }
    let (base, _theirs, ours) = prepare("store-switches");
    let store = base.join("store");
    let out = run_env(
        &romposer(),
        &ours,
        &["install"],
        &[
            ("PCOMPOSER_STORE", store.to_str().unwrap()),
            ("PCOMPOSER_DISABLE", "1"),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!store.exists(), "disabled runs never touch the store");
    assert!(!ours.join("vendor/composer/pcomposer.json").exists());
    assert_eq!(
        fs::metadata(ours.join("vendor/psr/log/src/LoggerInterface.php"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );

    // copy strategy via env: files are private copies but the store is filled
    fs::remove_dir_all(ours.join("vendor")).unwrap();
    let out = run_env(
        &romposer(),
        &ours,
        &["install"],
        &[
            ("PCOMPOSER_STORE", store.to_str().unwrap()),
            ("PCOMPOSER_STRATEGY", "copy"),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(store.join("files").exists());
    assert_eq!(
        fs::metadata(ours.join("vendor/psr/log/src/LoggerInterface.php"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    assert!(
        fs::read_to_string(ours.join("vendor/composer/pcomposer.json"))
            .unwrap()
            .contains("\"strategy\": \"copy\"")
    );

    // invalid strategy aborts with pcomposer's message
    let out = run_env(
        &romposer(),
        &ours,
        &["install"],
        &[
            ("PCOMPOSER_STORE", store.to_str().unwrap()),
            ("PCOMPOSER_STRATEGY", "symlink"),
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "pcomposer: invalid strategy \"symlink\", expected one of hardlink, reflink, copy"
    ));
}

/// On macOS a non-APFS volume refuses clonefile: the package falls back to copy with pcomposer's warning.
#[test]
#[cfg(target_os = "macos")]
fn reflink_falls_back_to_copy_on_a_non_apfs_volume() {
    if composer().is_none() {
        return;
    }
    let (base, _theirs, ours) = prepare("store-reflink");
    let image = base.join("hfs.dmg");
    let mount = base.join("hfs");
    fs::create_dir_all(&mount).unwrap();
    let created = std::process::Command::new("hdiutil")
        .args([
            "create",
            "-quiet",
            "-size",
            "64m",
            "-fs",
            "HFS+",
            "-volname",
            "romposerhfs",
            image.to_str().unwrap(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !created {
        eprintln!("skipping: hdiutil could not create an HFS+ image");
        return;
    }
    let attached = std::process::Command::new("hdiutil")
        .args([
            "attach",
            "-quiet",
            "-nobrowse",
            "-mountpoint",
            mount.to_str().unwrap(),
            image.to_str().unwrap(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !attached {
        eprintln!("skipping: hdiutil could not attach the image");
        return;
    }
    let store = mount.join("store");
    let out = run_env(
        &romposer(),
        &ours,
        &["install"],
        &[
            ("PCOMPOSER_STORE", store.to_str().unwrap()),
            ("PCOMPOSER_STRATEGY", "reflink"),
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let _ = std::process::Command::new("hdiutil")
        .args(["detach", "-quiet", "-force", mount.to_str().unwrap()])
        .status();
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("pcomposer: reflink failed ("), "{stderr}");
    assert!(
        stderr.contains("), falling back to copy for this run"),
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("falling back to copy").count(),
        1,
        "one warning per run: {stderr}"
    );
    assert!(
        fs::read_to_string(ours.join("vendor/composer/pcomposer.json"))
            .unwrap()
            .contains("\"strategy\": \"copy\"")
    );
}
