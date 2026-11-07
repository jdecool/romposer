//! `romposer store status|unlink|verify|prune`: pcomposer's commands, same output and exit codes.

use crate::commands::dump_autoload::load_project;
use crate::installed::InstalledRepository;
use crate::installer::install_path;
use crate::io::Io;
use crate::store::{Manifest, Store, StoreConfig, parse_window, privatise_tree};
use std::path::Path;

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Lists the packages linked from the store and the store size
    Status,
    /// Replaces the hard links of a package with private writable copies
    Unlink {
        /// Package name
        package: String,
    },
    /// Rehashes every store file and reports mismatches
    Verify,
    /// Removes index entries not used within the window, then unreferenced store files
    Prune {
        /// Window as <N>d, <N>h or <N>m
        #[arg(long, default_value = "30d")]
        older_than: String,
        /// List what would be removed without deleting anything
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run(command: &Command, io: &mut Io) -> i32 {
    match execute(command, io) {
        Ok(code) => code,
        Err(message) => {
            io.error(&message);
            1
        }
    }
}

fn execute(command: &Command, io: &mut Io) -> Result<i32, String> {
    let (_file, _contents, root, config, _lock) = load_project(io)?;
    let store_config = StoreConfig::from_root(&root.package.extra, &config.get_str("cache-dir"))?;
    let store = Store::new(&store_config.root);
    let vendor_dir = config.vendor_dir();
    let manifest = Manifest::in_vendor_dir(&vendor_dir);
    match command {
        Command::Status => {
            let entries = manifest.read();
            if entries.is_empty() {
                io.write("No package linked from the store (no vendor/composer/pcomposer.json).");
            } else {
                let rows: Vec<[String; 3]> = entries
                    .values()
                    .map(|e| [e.name.clone(), e.version.clone(), e.strategy.clone()])
                    .collect();
                for line in table(&["Package", "Version", "Strategy"], &rows) {
                    io.write(&line);
                }
            }
            io.write(&format!(
                "Store: {} ({} bytes)",
                store.path().display(),
                store.size()
            ));
            Ok(0)
        }
        Command::Verify => {
            let (checked, corrupted) = store.verify();
            for path in &corrupted {
                io.error(&format!("Corrupted: {}", path.display()));
            }
            io.write(&format!(
                "{checked} files checked, {} corrupted",
                corrupted.len()
            ));
            Ok(if corrupted.is_empty() { 0 } else { 1 })
        }
        Command::Unlink { package: name } => {
            let entries = manifest.read();
            let local_repo = InstalledRepository::read(Path::new(&format!(
                "{vendor_dir}/composer/installed.json"
            )))?;
            let installed = local_repo
                .canonical_packages()
                .into_iter()
                .find(|p| p.name == *name)
                .cloned();
            let (Some(entry), Some(package)) = (entries.get(name), installed) else {
                io.write(&format!("{name} is not linked from the store."));
                return Ok(1);
            };
            if entry.strategy != "hardlink" {
                io.write(&format!(
                    "{name} is installed with the {} strategy, nothing to unlink.",
                    entry.strategy
                ));
                return Ok(0);
            }
            let dir = install_path(&vendor_dir, &package).unwrap_or_default();
            privatise_tree(Path::new(&dir))?;
            manifest.set_strategy(name, "copy")?;
            io.write(&format!("{name} is now a private copy."));
            Ok(0)
        }
        Command::Prune {
            older_than,
            dry_run,
        } => {
            let Some(seconds) = parse_window(older_than) else {
                io.error(&format!(
                    "Invalid --older-than \"{older_than}\": expected <N>d, <N>h or <N>m"
                ));
                return Ok(1);
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let (entries, files) = store.prune(now.saturating_sub(seconds), *dry_run)?;
            let verb = if *dry_run { "Would remove" } else { "Removed" };
            if *dry_run || io.verbosity >= 1 {
                for path in entries.iter().chain(files.iter()) {
                    io.write(&format!("{verb} {}", path.display()));
                }
            }
            io.write(&format!(
                "{verb} {} index entries and {} store files",
                entries.len(),
                files.len()
            ));
            Ok(0)
        }
    }
}

/// Symfony Console's default table style.
fn table(headers: &[&str], rows: &[[String; 3]]) -> Vec<String> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let border = format!(
        "+{}+",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("+")
    );
    let line = |cells: &[&str]| -> String {
        let mut out = String::from("|");
        for (i, cell) in cells.iter().enumerate() {
            out.push_str(&format!(" {:<width$} |", cell, width = widths[i]));
        }
        out
    };
    let mut lines = vec![border.clone(), line(headers), border.clone()];
    for row in rows {
        lines.push(line(&[row[0].as_str(), row[1].as_str(), row[2].as_str()]));
    }
    lines.push(border);
    lines
}

#[cfg(test)]
mod tests {
    use super::table;

    #[test]
    fn renders_like_symfony_console() {
        let rows = [
            [
                "acme/lib".to_string(),
                "1.2.0".to_string(),
                "hardlink".to_string(),
            ],
            [
                "acme/zed".to_string(),
                "2.0.0".to_string(),
                "copy".to_string(),
            ],
        ];
        assert_eq!(
            table(&["Package", "Version", "Strategy"], &rows),
            vec![
                "+----------+---------+----------+",
                "| Package  | Version | Strategy |",
                "+----------+---------+----------+",
                "| acme/lib | 1.2.0   | hardlink |",
                "| acme/zed | 2.0.0   | copy     |",
                "+----------+---------+----------+",
            ]
        );
    }
}
