//! `composer clear-cache` (`ClearCacheCommand`, `Cache::clear` / `gc`). The pcomposer
//! store under the cache directory is never touched: `romposer store prune` owns it.

use crate::config::Config;
use crate::io::Io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Only run garbage collection, not a full cache clear
    #[arg(long)]
    pub gc: bool,
}

pub fn run(args: &Args, io: &mut Io) -> i32 {
    match execute(args, io) {
        Ok(code) => code,
        Err(message) => crate::scripts::exit_code(io, &message),
    }
}

/// `tryComposer`'s config: the project's when a valid composer.json is here.
fn config() -> Result<Config, String> {
    let cwd = crate::fs::cwd();
    let file = crate::config::composer_file()?;
    if let Ok(text) = std::fs::read_to_string(&file)
        && let Ok(serde_json::Value::Object(manifest)) = serde_json::from_str(&text)
    {
        return Config::for_project(&cwd, &manifest, &file);
    }
    Config::create(&cwd)
}

/// Every file under `dir`, the store excepted.
fn files(dir: &Path, store: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == store {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            files(&path, store, out);
        } else {
            out.push(path);
        }
    }
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    let config = config()?;
    let store = crate::store::config::StoreConfig::from_root(
        &Default::default(),
        &config.get_str("cache-dir"),
    )?;
    let store = PathBuf::from(crate::fs::realpath(Path::new(&store.root)));
    let read_only = config.bool("cache-read-only");
    let unusable = regex::Regex::new(r"(?i)^/dev/null$|^nul$").unwrap();
    let now = SystemTime::now();
    for key in [
        "cache-vcs-dir",
        "cache-repo-dir",
        "cache-files-dir",
        "cache-dir",
    ] {
        if key == "cache-dir" && args.gc {
            continue;
        }
        let path = config.get_str(key);
        if unusable.is_match(&path) {
            io.error(&format!("Cache is not enabled ({key}): {path}"));
            continue;
        }
        let Ok(real) = std::fs::canonicalize(&path) else {
            io.error(&format!("Cache directory does not exist ({key}): "));
            continue;
        };
        let shown = real.display().to_string();
        // a project composer.json can point cache-dir anywhere: never wipe `/` or the home dir
        let home = std::env::var("HOME")
            .ok()
            .and_then(|h| std::fs::canonicalize(h).ok());
        if real.parent().is_none() || Some(&real) == home.as_ref() {
            io.error(&format!("Cache is not enabled ({key}): {shown}"));
            continue;
        }
        let writable = std::fs::metadata(&real).is_ok_and(|m| !m.permissions().readonly());
        if !writable {
            io.error(&format!("Cache is not enabled ({key}): {shown}"));
            continue;
        }
        if args.gc {
            io.error(&format!("Garbage-collecting cache ({key}): {shown}"));
            if read_only {
                continue;
            }
            let ttl = config.int(if key == "cache-files-dir" {
                "cache-files-ttl"
            } else {
                "cache-ttl"
            });
            let expire = now - Duration::from_secs(ttl.max(0) as u64);
            if key == "cache-vcs-dir" {
                // `gcVcsCache`: whole repositories untouched since the ttl
                for entry in std::fs::read_dir(&real).into_iter().flatten().flatten() {
                    let old = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t <= expire);
                    if entry.file_type().is_ok_and(|t| t.is_dir()) && old {
                        crate::fs::remove(&entry.path())?;
                    }
                }
                continue;
            }
            let max_size = if key == "cache-files-dir" {
                match crate::commands::config::get(&config, "cache-files-maxsize", false) {
                    serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
                    _ => 0,
                }
            } else {
                1024 * 1024 * 1024
            };
            let mut all = Vec::new();
            files(&real, &store, &mut all);
            all.retain(|f| {
                let old = std::fs::metadata(f)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t <= expire);
                if old {
                    let _ = std::fs::remove_file(f);
                }
                !old
            });
            let size = |f: &PathBuf| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
            let mut total: u64 = all.iter().map(size).sum();
            all.sort_by_key(|f| std::fs::metadata(f).and_then(|m| m.accessed()).ok());
            for f in all {
                if total <= max_size {
                    break;
                }
                total -= size(&f);
                let _ = std::fs::remove_file(&f);
            }
        } else {
            io.error(&format!("Clearing cache ({key}): {shown}"));
            if read_only {
                continue;
            }
            for entry in std::fs::read_dir(&real)
                .map_err(|e| e.to_string())?
                .flatten()
            {
                if entry.path() != store {
                    crate::fs::remove(&entry.path())?;
                }
            }
        }
    }
    io.error(if args.gc {
        "All caches garbage-collected."
    } else {
        "All caches cleared."
    });
    Ok(0)
}
