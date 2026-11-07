//! pcomposer's content-addressable store, same on-disk format:
//!
//! ```text
//! files/<hh>/<sha256>      regular file, 0444
//! files/<hh>/<sha256>-x    executable file, 0555
//! index/<vendor>/<name>@<version>-<reference>.json
//!   {"files": {<path>: {"hash", "mode"}}, "dirs": [<path>], "links": {<path>: <target>}}
//! ```

pub mod config;
pub mod manifest;

use crate::fs::{ensure_dir, random_hex};
use crate::json::{Map, Value, encode};
use sha2::Digest;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use config::{StoreConfig, Strategy};
pub use manifest::Manifest;

/// Orphan files younger than this are kept by `prune`: an install may be ingesting them.
pub const PRUNE_GRACE_PERIOD: u64 = 3600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexFile {
    pub hash: String,
    pub mode: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub files: BTreeMap<String, IndexFile>,
    pub dirs: Vec<String>,
    pub links: BTreeMap<String, String>,
}

/// Hard link or clone, injectable so the fallback path can be tested.
pub type Linker = Box<dyn Fn(&Path, &Path) -> std::io::Result<()>>;

pub struct Store {
    root: PathBuf,
    linker: Linker,
    cloner: Linker,
    fallback_reason: Option<String>,
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// `clonefile(2)` on macOS, `ioctl(FICLONE)` on Linux: one copy-on-write clone per file.
pub fn reflink(source: &Path, dest: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let src = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let dst = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
        // SAFETY: both strings are valid NUL-terminated paths; clonefile does not retain them.
        let rc = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        const FICLONE: libc::c_ulong = 0x4004_9409;
        let src = std::fs::File::open(source)?;
        let dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)?;
        // SAFETY: both descriptors are open and owned by this function.
        let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            let _ = std::fs::remove_file(dest);
            return Err(err);
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, dest);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "reflink is only supported on macOS and Linux",
        ))
    }
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Store {
        Store::with_operations(
            root,
            Box::new(|s, d| std::fs::hard_link(s, d)),
            Box::new(reflink),
        )
    }

    pub fn with_operations(root: impl Into<PathBuf>, linker: Linker, cloner: Linker) -> Store {
        Store {
            root: root.into(),
            linker,
            cloner,
            fallback_reason: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Total size in bytes of the stored files (0 when the store does not exist yet).
    pub fn size(&self) -> u64 {
        self.store_files()
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum()
    }

    /// `vendor/name@version-reference`; artifact packages have no reference, their zip shasum stands in.
    pub fn key(package: &crate::package::Package) -> String {
        let dist = package.dist.as_ref();
        let reference = dist
            .and_then(|d| d.reference.clone())
            .or_else(|| dist.and_then(|d| d.shasum.clone()))
            .unwrap_or_default();
        format!("{}@{}-{reference}", package.name, package.version)
    }

    fn index_path(&self, key: &str) -> Result<PathBuf, String> {
        if key.contains("..") {
            return Err(format!("Invalid store key {key}"));
        }
        let (vendor, rest) = key.split_once('/').unwrap_or((key, ""));
        let safe = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "_.@-".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        Ok(self
            .root
            .join("index")
            .join(safe(vendor))
            .join(format!("{}.json", safe(rest))))
    }

    fn file_path(&self, hash: &str, executable: bool) -> PathBuf {
        self.root
            .join("files")
            .join(&hash[..2.min(hash.len())])
            .join(if executable {
                format!("{hash}-x")
            } else {
                hash.to_string()
            })
    }

    fn file_path_for(&self, file: &IndexFile) -> PathBuf {
        self.file_path(&file.hash, file.mode & 0o100 != 0)
    }

    /// Whether `key` was ingested before (its index entry exists).
    pub fn has(&self, key: &str) -> bool {
        self.index_path(key).is_ok_and(|p| p.exists())
    }

    /// Copies every regular file under `dir` into the store and writes the index entry for `key`.
    pub fn ingest(&self, key: &str, dir: &Path) -> Result<(), String> {
        let mut entry = IndexEntry::default();
        self.walk(dir, dir, &mut entry)?;
        let index = self.index_path(key)?;
        ensure_dir(index.parent().unwrap())?;
        let tmp = PathBuf::from(format!("{}.{}", index.display(), random_hex(4)));
        std::fs::write(&tmp, Self::encode_entry(&entry))
            .map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &index)
            .map_err(|e| format!("Cannot write {}: {e}", index.display()))?;
        Ok(())
    }

    fn walk(&self, root: &Path, dir: &Path, entry: &mut IndexEntry) -> Result<(), String> {
        for item in std::fs::read_dir(dir)
            .map_err(|e| format!("Cannot read {}: {e}", dir.display()))?
            .flatten()
        {
            let path = item.path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if meta.is_symlink() {
                let target = std::fs::read_link(&path).map_err(|e| e.to_string())?;
                entry
                    .links
                    .insert(relative, target.to_string_lossy().into_owned());
            } else if meta.is_dir() {
                if std::fs::read_dir(&path)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true)
                {
                    entry.dirs.push(relative);
                } else {
                    self.walk(root, &path, entry)?;
                }
            } else if meta.is_file() {
                let executable = meta.permissions().mode() & 0o111 != 0;
                let hash = sha256_file(&path)
                    .map_err(|e| format!("Cannot hash {}: {e}", path.display()))?;
                self.put(&self.file_path(&hash, executable), &path, executable)?;
                entry.files.insert(
                    relative,
                    IndexFile {
                        hash,
                        mode: if executable { 0o755 } else { 0o644 },
                    },
                );
            }
        }
        entry.dirs.sort();
        Ok(())
    }

    /// `json_encode(..., JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES)` of the entry, PHP arrays and all.
    fn encode_entry(entry: &IndexEntry) -> String {
        let files = if entry.files.is_empty() {
            Value::Array(vec![])
        } else {
            Value::Object(
                entry
                    .files
                    .iter()
                    .map(|(path, f)| {
                        let mut m = Map::new();
                        m.insert("hash".into(), Value::String(f.hash.clone()));
                        m.insert("mode".into(), Value::from(f.mode));
                        (path.clone(), Value::Object(m))
                    })
                    .collect(),
            )
        };
        let links = if entry.links.is_empty() {
            Value::Array(vec![])
        } else {
            Value::Object(
                entry
                    .links
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            )
        };
        let mut m = Map::new();
        m.insert("files".into(), files);
        m.insert(
            "dirs".into(),
            Value::Array(
                entry
                    .dirs
                    .iter()
                    .map(|d| Value::String(d.clone()))
                    .collect(),
            ),
        );
        m.insert("links".into(), links);
        encode(&Value::Object(m), true, true, false)
    }

    pub fn entry(&self, key: &str) -> Result<IndexEntry, String> {
        Self::entry_at(&self.index_path(key)?)
    }

    fn entry_at(index: &Path) -> Result<IndexEntry, String> {
        let json = std::fs::read_to_string(index)
            .map_err(|_| format!("No store entry at {}", index.display()))?;
        let value: Value = serde_json::from_str(&json)
            .map_err(|e| format!("Invalid store entry at {}: {e}", index.display()))?;
        let mut entry = IndexEntry::default();
        if let Some(Value::Object(files)) = value.get("files") {
            for (path, f) in files {
                entry.files.insert(
                    path.clone(),
                    IndexFile {
                        hash: f
                            .get("hash")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        mode: f.get("mode").and_then(Value::as_u64).unwrap_or(0o644) as u32,
                    },
                );
            }
        }
        if let Some(Value::Array(dirs)) = value.get("dirs") {
            entry.dirs = dirs
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        if let Some(Value::Object(links)) = value.get("links") {
            entry.links = links
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|t| (k.clone(), t.to_string())))
                .collect();
        }
        Ok(entry)
    }

    /// Atomic temp + link: an existing store file is never rewritten, so concurrent ingests
    /// of the same content never replace each other's inode.
    fn put(&self, store_file: &Path, source: &Path, executable: bool) -> Result<(), String> {
        if store_file.exists() {
            return Ok(());
        }
        let dir = store_file.parent().unwrap();
        ensure_dir(dir)?;
        let tmp = dir.join(format!(".tmp-{}", random_hex(8)));
        let result = (|| -> std::io::Result<()> {
            std::fs::copy(source, &tmp)?;
            std::fs::set_permissions(
                &tmp,
                std::fs::Permissions::from_mode(if executable { 0o555 } else { 0o444 }),
            )?;
            if std::fs::hard_link(&tmp, store_file).is_err() && !store_file.exists() {
                return Err(std::io::Error::other("link failed"));
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(&tmp);
        result.map_err(|e| format!("Cannot write {} to the store: {e}", store_file.display()))
    }

    /// Materialises the `key` entry into `target` with the strategy, refreshing the entry's mtime
    /// (prune's "last used"). Returns the strategy actually used.
    pub fn materialise(
        &mut self,
        key: &str,
        target: &Path,
        strategy: Strategy,
        copy: &[String],
    ) -> Result<Strategy, String> {
        let index = self.index_path(key)?;
        let _ = std::fs::File::open(&index).and_then(|f| f.set_modified(SystemTime::now()));
        match strategy {
            Strategy::Hardlink if self.can_materialise_into(key, target, strategy)? => {
                self.link(key, target, copy)?;
                Ok(Strategy::Hardlink)
            }
            Strategy::Reflink if self.can_materialise_into(key, target, strategy)? => {
                self.clone_into(key, target, copy)?;
                Ok(Strategy::Reflink)
            }
            _ => {
                self.copy(key, target)?;
                Ok(Strategy::Copy)
            }
        }
    }

    /// Why the first materialisation of this run fell back to copy, if any did.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    /// Tries the entry's first file against a probe path: no upfront filesystem detection.
    fn can_materialise_into(
        &mut self,
        key: &str,
        target: &Path,
        strategy: Strategy,
    ) -> Result<bool, String> {
        let entry = self.entry(key)?;
        let Some((_, first)) = entry.files.iter().next() else {
            return Ok(true);
        };
        let source = self.file_path_for(first);
        ensure_dir(target)?;
        let probe = if strategy == Strategy::Reflink {
            target.join(source.file_name().unwrap())
        } else {
            target.join(".pcomposer-probe")
        };
        let op = if strategy == Strategy::Reflink {
            &self.cloner
        } else {
            &self.linker
        };
        let result = op(&source, &probe);
        let _ = std::fs::remove_file(&probe);
        if let Err(e) = result {
            self.fallback_reason.get_or_insert_with(|| e.to_string());
            return Ok(false);
        }
        Ok(true)
    }

    /// Files hard-linked (declared bins copied), empty directories created, symlinks recreated.
    pub fn link(&self, key: &str, target: &Path, copy: &[String]) -> Result<(), String> {
        let entry = self.entry(key)?;
        for (relative, file) in &entry.files {
            let dest = target.join(relative);
            if copy.iter().any(|c| c == relative) {
                self.copy_private(&self.file_path_for(file), &dest, file.mode)?;
                continue;
            }
            ensure_dir(dest.parent().unwrap())?;
            (self.linker)(&self.file_path_for(file), &dest).map_err(|e| {
                format!("Cannot materialise {} from the store: {e}", dest.display())
            })?;
        }
        Self::recreate(target, &entry)
    }

    /// Like `link`, with files cloned copy-on-write and given normal permissions.
    fn clone_into(&self, key: &str, target: &Path, copy: &[String]) -> Result<(), String> {
        let entry = self.entry(key)?;
        for (relative, file) in &entry.files {
            let dest = target.join(relative);
            if copy.iter().any(|c| c == relative) {
                self.copy_private(&self.file_path_for(file), &dest, file.mode)?;
                continue;
            }
            ensure_dir(dest.parent().unwrap())?;
            (self.cloner)(&self.file_path_for(file), &dest)
                .map_err(|e| format!("Cannot clone {} from the store: {e}", dest.display()))?;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(file.mode)).map_err(
                |e| format!("Cannot materialise {} from the store: {e}", dest.display()),
            )?;
        }
        Self::recreate(target, &entry)
    }

    /// Every file of the entry as a private copy with normal permissions.
    pub fn copy(&self, key: &str, target: &Path) -> Result<(), String> {
        let all: Vec<String> = self.entry(key)?.files.keys().cloned().collect();
        self.link(key, target, &all)
    }

    fn copy_private(&self, source: &Path, dest: &Path, mode: u32) -> Result<(), String> {
        ensure_dir(dest.parent().unwrap())?;
        std::fs::copy(source, dest)
            .and_then(|_| std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode)))
            .map_err(|e| format!("Cannot materialise {} from the store: {e}", dest.display()))
    }

    fn recreate(target: &Path, entry: &IndexEntry) -> Result<(), String> {
        for dir in &entry.dirs {
            ensure_dir(&target.join(dir))?;
        }
        for (relative, to) in &entry.links {
            let dest = target.join(relative);
            ensure_dir(dest.parent().unwrap())?;
            std::os::unix::fs::symlink(to, &dest)
                .map_err(|e| format!("Cannot create symlink {}: {e}", dest.display()))?;
        }
        Ok(())
    }

    /// Every file under files/, sorted.
    fn store_files(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Ok(shards) = std::fs::read_dir(self.root.join("files")) {
            for shard in shards.flatten() {
                if let Ok(files) = std::fs::read_dir(shard.path()) {
                    paths.extend(files.flatten().map(|f| f.path()).filter(|p| p.is_file()));
                }
            }
        }
        paths.sort();
        paths
    }

    fn index_files(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Ok(vendors) = std::fs::read_dir(self.root.join("index")) {
            for vendor in vendors.flatten() {
                if let Ok(entries) = std::fs::read_dir(vendor.path()) {
                    paths.extend(
                        entries
                            .flatten()
                            .map(|f| f.path())
                            .filter(|p| p.extension().is_some_and(|e| e == "json")),
                    );
                }
            }
        }
        paths.sort();
        paths
    }

    /// Rehashes every store file; the expected hash is its file name.
    pub fn verify(&self) -> (usize, Vec<PathBuf>) {
        let mut checked = 0;
        let mut corrupted = Vec::new();
        for path in self.store_files() {
            checked += 1;
            let expected = path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .split('-')
                .next()
                .unwrap_or("")
                .to_string();
            if sha256_file(&path).ok().as_deref() != Some(expected.as_str()) {
                corrupted.push(path);
            }
        }
        (checked, corrupted)
    }

    /// Deletes index entries last used before `older_than` (unix time), then every store file no
    /// remaining entry references and older than the grace period. Returns (entries, files).
    pub fn prune(
        &self,
        older_than: u64,
        dry_run: bool,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
        let mut stale = Vec::new();
        let mut referenced = std::collections::HashSet::new();
        for index in self.index_files() {
            let mtime = std::fs::metadata(&index)
                .and_then(|m| m.modified())
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                .unwrap_or(0);
            if mtime < older_than {
                stale.push(index);
                continue;
            }
            for file in Self::entry_at(&index)?.files.values() {
                referenced.insert(self.file_path_for(file));
            }
        }
        let young = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(PRUNE_GRACE_PERIOD);
        let orphans: Vec<PathBuf> = self
            .store_files()
            .into_iter()
            .filter(|p| {
                let mtime = std::fs::metadata(p)
                    .map(|m| m.mtime().max(0) as u64)
                    .unwrap_or(0);
                !referenced.contains(p) && mtime < young
            })
            .collect();
        if !dry_run {
            for path in stale.iter().chain(orphans.iter()) {
                std::fs::remove_file(path)
                    .map_err(|e| format!("Cannot delete {}: {e}", path.display()))?;
            }
        }
        Ok((stale, orphans))
    }
}

/// Turns the hard links of an installed package into private copies: copy to a sibling
/// temp file, chmod, rename over the link (`pcomposer:unlink`).
pub fn privatise_tree(dir: &Path) -> Result<(), String> {
    for item in std::fs::read_dir(dir)
        .map_err(|e| format!("Cannot read {}: {e}", dir.display()))?
        .flatten()
    {
        let path = item.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            privatise_tree(&path)?;
        } else if meta.is_file() {
            let mode = if meta.permissions().mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            let tmp = path
                .parent()
                .unwrap()
                .join(format!(".pcomposer-unlink-{}", random_hex(4)));
            let result = std::fs::copy(&path, &tmp)
                .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)))
                .and_then(|()| std::fs::rename(&tmp, &path));
            if let Err(e) = result {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("Cannot unlink {}: {e}", path.display()));
            }
        }
    }
    Ok(())
}

/// The age window of `prune --older-than`: `<N>d`, `<N>h` or `<N>m` in seconds.
pub fn parse_window(window: &str) -> Option<u64> {
    let (digits, unit) = window.split_at(window.len().checked_sub(1)?);
    let n: u64 = digits.parse().ok()?;
    let mult = match unit {
        "d" => 86400,
        "h" => 3600,
        "m" => 60,
        _ => return None,
    };
    Some(n * mult)
}

#[allow(dead_code)]
fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("romposer-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_package(dir: &Path) {
        std::fs::create_dir_all(dir.join("src/empty")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("src/A.php"), "<?php class A {}").unwrap();
        std::fs::write(dir.join("src/B.php"), "<?php class A {}").unwrap(); // same content as A
        std::fs::write(dir.join("bin/run"), "#!/bin/sh\necho hi").unwrap();
        std::fs::set_permissions(dir.join("bin/run"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("src/A.php", dir.join("link.php")).unwrap();
    }

    #[test]
    fn ingest_writes_pcomposer_layout_and_index() {
        let base = temp("ingest");
        let pkg = base.join("pkg");
        sample_package(&pkg);
        let store = Store::new(base.join("store"));
        store.ingest("acme/lib@1.0.0.0-abc", &pkg).unwrap();
        let entry = store.entry("acme/lib@1.0.0.0-abc").unwrap();
        assert_eq!(entry.files["src/A.php"].mode, 0o644);
        assert_eq!(entry.files["bin/run"].mode, 0o755);
        assert_eq!(entry.files["src/A.php"].hash, entry.files["src/B.php"].hash);
        assert_eq!(entry.dirs, vec!["src/empty"]);
        assert_eq!(entry.links["link.php"], "src/A.php");
        let hash = &entry.files["src/A.php"].hash;
        let stored = base.join("store/files").join(&hash[..2]).join(hash);
        assert_eq!(
            std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
            0o444
        );
        let exec = base
            .join("store/files")
            .join(&entry.files["bin/run"].hash[..2])
            .join(format!("{}-x", entry.files["bin/run"].hash));
        assert_eq!(
            std::fs::metadata(&exec).unwrap().permissions().mode() & 0o777,
            0o555
        );
        let index =
            std::fs::read_to_string(base.join("store/index/acme/lib@1.0.0.0-abc.json")).unwrap();
        assert!(
            index.starts_with(
                "{\n    \"files\": {\n        \"bin/run\": {\n            \"hash\": \""
            )
        );
        assert!(index.contains("\"mode\": 493"));
        assert!(index.contains("\"dirs\": [\n        \"src/empty\"\n    ]"));
        assert!(index.contains("\"links\": {\n        \"link.php\": \"src/A.php\"\n    }"));
        assert!(store.has("acme/lib@1.0.0.0-abc"));
        assert!(!store.has("acme/lib@2.0.0.0-abc"));
    }

    #[test]
    fn hardlink_shares_inodes_bins_are_private_and_fallback_copies() {
        let base = temp("materialise");
        let pkg = base.join("pkg");
        sample_package(&pkg);
        let mut store = Store::new(base.join("store"));
        store.ingest("acme/lib@1.0.0.0-abc", &pkg).unwrap();
        let target = base.join("vendor/acme/lib");
        let used = store
            .materialise(
                "acme/lib@1.0.0.0-abc",
                &target,
                Strategy::Hardlink,
                &["bin/run".into()],
            )
            .unwrap();
        assert_eq!(used, Strategy::Hardlink);
        let entry = store.entry("acme/lib@1.0.0.0-abc").unwrap();
        let stored = store.file_path_for(&entry.files["src/A.php"]);
        assert_eq!(
            std::fs::metadata(&stored).unwrap().ino(),
            std::fs::metadata(target.join("src/A.php")).unwrap().ino()
        );
        assert_eq!(
            std::fs::metadata(target.join("src/A.php"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o444
        );
        assert_ne!(
            std::fs::metadata(target.join("bin/run")).unwrap().ino(),
            std::fs::metadata(store.file_path_for(&entry.files["bin/run"]))
                .unwrap()
                .ino()
        );
        assert_eq!(
            std::fs::metadata(target.join("bin/run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(target.join("src/empty").is_dir());
        assert_eq!(
            std::fs::read_link(target.join("link.php"))
                .unwrap()
                .to_string_lossy(),
            "src/A.php"
        );
        assert!(!target.join(".pcomposer-probe").exists());

        let failing: Linker = Box::new(|_, _| Err(std::io::Error::other("Cross-device link")));
        let mut store2 = Store::with_operations(base.join("store"), failing, Box::new(reflink));
        let target2 = base.join("vendor2/acme/lib");
        let used = store2
            .materialise("acme/lib@1.0.0.0-abc", &target2, Strategy::Hardlink, &[])
            .unwrap();
        assert_eq!(used, Strategy::Copy);
        assert_eq!(store2.fallback_reason(), Some("Cross-device link"));
        assert_ne!(
            std::fs::metadata(&stored).unwrap().ino(),
            std::fs::metadata(target2.join("src/A.php")).unwrap().ino()
        );
        assert_eq!(
            std::fs::metadata(target2.join("src/A.php"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    fn reflink_clones_with_normal_permissions_when_supported() {
        let base = temp("reflink");
        let pkg = base.join("pkg");
        sample_package(&pkg);
        let mut store = Store::new(base.join("store"));
        store.ingest("acme/lib@1.0.0.0-abc", &pkg).unwrap();
        let target = base.join("vendor/acme/lib");
        let used = store
            .materialise("acme/lib@1.0.0.0-abc", &target, Strategy::Reflink, &[])
            .unwrap();
        // the temp dir may or may not support clones; either outcome must be consistent
        match used {
            Strategy::Reflink => {
                assert!(store.fallback_reason().is_none());
                assert_eq!(
                    std::fs::metadata(target.join("src/A.php"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o644
                );
            }
            Strategy::Copy => assert!(store.fallback_reason().is_some()),
            Strategy::Hardlink => panic!("reflink never degrades to hardlink"),
        }
        assert_eq!(
            std::fs::read_to_string(target.join("src/A.php")).unwrap(),
            "<?php class A {}"
        );
    }

    #[test]
    fn verify_and_prune() {
        let base = temp("prune");
        let pkg = base.join("pkg");
        sample_package(&pkg);
        let store = Store::new(base.join("store"));
        store.ingest("acme/lib@1.0.0.0-abc", &pkg).unwrap();
        store
            .ingest("acme/old@0.1.0.0-def", &pkg.join("src"))
            .unwrap();
        let (checked, corrupted) = store.verify();
        assert_eq!(
            (checked, corrupted.len()),
            (2, 0),
            "A and B share one store file"
        );
        // corrupt one file
        let entry = store.entry("acme/lib@1.0.0.0-abc").unwrap();
        let victim = store.file_path_for(&entry.files["bin/run"]);
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&victim, "changed").unwrap();
        let (_, corrupted) = store.verify();
        assert_eq!(corrupted, vec![victim.clone()]);

        // an entry unused for a day goes, its exclusive files only after the grace period
        let old = SystemTime::now() - Duration::from_secs(2 * 86400);
        let index = base.join("store/index/acme/old@0.1.0.0-def.json");
        std::fs::File::open(&index)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let (entries, files) = store.prune(now().as_secs() - 86400, true).unwrap();
        assert_eq!(entries, vec![index.clone()]);
        assert!(
            files.is_empty(),
            "all files are still referenced by acme/lib or too young"
        );
        assert!(index.exists(), "dry run deletes nothing");
        let (entries, _) = store.prune(now().as_secs() - 86400, false).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!index.exists());
        // now make the lib entry stale too and age the files: the files become orphans
        let lib_index = base.join("store/index/acme/lib@1.0.0.0-abc.json");
        std::fs::File::open(&lib_index)
            .unwrap()
            .set_modified(old)
            .unwrap();
        for f in store.store_files() {
            std::fs::File::open(&f).unwrap().set_modified(old).unwrap();
        }
        let (entries, files) = store.prune(now().as_secs() - 86400, false).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(files.len(), 2);
        assert_eq!(store.size(), 0);
    }

    #[test]
    fn windows_and_unlink() {
        assert_eq!(parse_window("30d"), Some(30 * 86400));
        assert_eq!(parse_window("2h"), Some(7200));
        assert_eq!(parse_window("15m"), Some(900));
        assert_eq!(parse_window("1w"), None);
        assert_eq!(parse_window(""), None);
        let base = temp("unlink");
        let pkg = base.join("pkg");
        sample_package(&pkg);
        let mut store = Store::new(base.join("store"));
        store.ingest("acme/lib@1.0.0.0-abc", &pkg).unwrap();
        let target = base.join("vendor/acme/lib");
        store
            .materialise("acme/lib@1.0.0.0-abc", &target, Strategy::Hardlink, &[])
            .unwrap();
        let entry = store.entry("acme/lib@1.0.0.0-abc").unwrap();
        let stored = store.file_path_for(&entry.files["src/A.php"]);
        privatise_tree(&target).unwrap();
        assert_ne!(
            std::fs::metadata(&stored).unwrap().ino(),
            std::fs::metadata(target.join("src/A.php")).unwrap().ino()
        );
        assert_eq!(
            std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert_eq!(
            std::fs::metadata(target.join("src/A.php"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(target.join("bin/run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}
