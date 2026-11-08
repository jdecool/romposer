#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub fn romposer() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_romposer"))
}

/// The reference `composer` binary, when one is installed and is the pinned version.
pub fn composer() -> Option<PathBuf> {
    let out = Command::new("composer").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains(&format!("Composer version {}", romposer::COMPOSER_VERSION)) {
        Some(PathBuf::from("composer"))
    } else {
        eprintln!(
            "skipping oracle comparison: `composer` is not {} ({})",
            romposer::COMPOSER_VERSION,
            text.trim()
        );
        None
    }
}

pub fn run(bin: &Path, dir: &Path, args: &[&str]) -> Output {
    run_env(bin, dir, args, &[])
}

/// Runs with an isolated COMPOSER_HOME (and so cache and store) under `dir`.
pub fn run_env(bin: &Path, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(dir)
        .env("COMPOSER_HOME", dir.join(".composer-home"))
        .env("COMPOSER_NO_INTERACTION", "1")
        .env_remove("COMPOSER")
        // composer adds `::error::` annotations to stdout on GitHub Actions; romposer does not
        .env_remove("GITHUB_ACTIONS")
        .env_remove("PCOMPOSER_DISABLE")
        .env_remove("PCOMPOSER_STRATEGY")
        .env_remove("PCOMPOSER_STORE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn")
}

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("romposer-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
