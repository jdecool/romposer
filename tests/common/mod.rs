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
    Command::new(bin)
        .args(args)
        .current_dir(dir)
        .env("COMPOSER_HOME", dir.join(".composer-home"))
        .env("COMPOSER_NO_INTERACTION", "1")
        .env_remove("COMPOSER")
        .output()
        .expect("spawn")
}

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("romposer-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
