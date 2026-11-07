//! Every composer.lock Composer wrote must survive parse → write byte for byte.

use romposer::json::{Value, encode_pretty};
use std::path::{Path, PathBuf};

fn find_locks(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "vendor" || n == ".git")
            {
                continue;
            }
            find_locks(&path, out);
        } else if path.file_name().is_some_and(|n| n == "composer.lock") {
            out.push(path);
        }
    }
}

#[test]
fn composer_written_locks_round_trip() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("reference/composer");
    let mut locks = Vec::new();
    find_locks(&root, &mut locks);
    if let Ok(home) = std::env::var("HOME") {
        let own = Path::new(&home).join(".composer/composer.lock");
        if own.exists() {
            locks.push(own);
        }
    }
    assert!(
        locks.len() >= 3,
        "expected the submodule lock and its fixtures, found {locks:?}"
    );
    for lock in &locks {
        let original = std::fs::read_to_string(lock).unwrap();
        let value: Value = serde_json::from_str(&original).unwrap();
        let written = encode_pretty(&value) + "\n";
        assert!(
            written == original,
            "{} does not round-trip",
            lock.display()
        );
    }
}
