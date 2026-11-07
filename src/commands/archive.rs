//! `composer archive` of the current project (`ArchiveCommand`, `ArchiveManager`,
//! `ArchivableFilesFinder` with the `.gitattributes` export-ignore and `archive.exclude`
//! filters).
//!
//! ponytail: only the root package is archived (a named package would be downloaded
//! first); the archive holds the same entries in the same order as Composer's, but its
//! bytes come from the zip crate and our own ustar writer, not from PHP's PharData. A
//! `--file` that is absolute or climbs with `..` is refused (Composer writes it anywhere).

use crate::commands::dump_autoload::load_project;
use crate::io::Io;
use crate::json::Value;
use fancy_regex::Regex;
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// The package to archive instead of the current project
    pub package: Option<String>,
    /// A version constraint to find the package to archive
    #[arg(id = "package-version", value_name = "VERSION")]
    pub version: Option<String>,
    /// Format of the resulting archive: tar, tar.gz, tar.bz2 or zip (default tar)
    #[arg(short = 'f', long)]
    pub format: Option<String>,
    /// Write the archive to this directory
    #[arg(long)]
    pub dir: Option<String>,
    /// Write the archive with the given file name. Note that the format will be appended.
    #[arg(long)]
    pub file: Option<String>,
    /// Ignore filters when saving package
    #[arg(long)]
    pub ignore_filters: bool,
}

pub fn run(args: &Args, io: &mut Io) -> i32 {
    match execute(args, io) {
        Ok(code) => code,
        Err(message) => crate::scripts::exit_code(io, &message),
    }
}

/// Symfony `Glob::toRegex` (strict leading dot and wildcard slash), without delimiters
/// and anchors, as `BaseExcludeFilter::generatePattern` strips them.
fn glob_to_regex(glob: &str) -> String {
    let g: Vec<char> = glob.chars().collect();
    let (mut first, mut escaping, mut curlies) = (true, false, 0);
    let mut regex = String::new();
    let mut i = 0;
    while i < g.len() {
        let mut car = g[i].to_string();
        if first && g[i] != '.' {
            regex.push_str(r"(?=[^\.])");
        }
        first = g[i] == '/';
        if first
            && g.get(i + 1) == Some(&'*')
            && g.get(i + 2) == Some(&'*')
            && g.get(i + 3).is_none_or(|c| *c == '/')
        {
            let tail = if g.get(i + 3).is_none() { "?" } else { "" };
            car = format!(r"/(?:(?=[^\.])(?>[^/]+)/{tail})*");
            i += 2 + usize::from(g.get(i + 3).is_some());
        }
        match car.as_str() {
            "#" | "." | "(" | ")" | "|" | "+" | "^" | "$" => regex.push_str(&format!("\\{car}")),
            "*" => regex.push_str(if escaping { r"\*" } else { "[^/]*" }),
            "?" => regex.push_str(if escaping { r"\?" } else { "[^/]" }),
            "{" => {
                regex.push_str(if escaping { r"\{" } else { "(" });
                if !escaping {
                    curlies += 1;
                }
            }
            "}" if curlies > 0 => {
                regex.push_str(if escaping { "}" } else { ")" });
                if !escaping {
                    curlies -= 1;
                }
            }
            "," if curlies > 0 => regex.push_str(if escaping { "," } else { "|" }),
            "\\" => {
                if escaping {
                    regex.push_str(r"\\");
                }
                escaping = !escaping;
                i += 1;
                continue;
            }
            _ => regex.push_str(&car),
        }
        escaping = false;
        i += 1;
    }
    regex
}

/// `BaseExcludeFilter::generatePattern`: (regex, negate).
fn exclude_pattern(rule: &str) -> Option<(Regex, bool)> {
    let negate = rule.starts_with('!');
    let rule = rule.trim_start_matches('!');
    let prefix = match rule.find('/') {
        Some(0) => "^/",
        Some(p) if p == rule.len() - 1 => "/",
        None => "/",
        _ => "",
    };
    let body = glob_to_regex(rule.trim_matches('/'));
    Regex::new(&format!("{prefix}{body}(?=$|/)"))
        .ok()
        .map(|r| (r, negate))
}

/// `GitExcludeFilter`: `export-ignore` attributes of `.gitattributes`.
fn git_excludes(sources: &Path) -> Vec<(Regex, bool)> {
    let Ok(text) = std::fs::read_to_string(sources.join(".gitattributes")) else {
        return vec![];
    };
    let ws = regex::Regex::new(r"\s+").unwrap();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let parts: Vec<&str> = ws.split(line).collect();
            match parts.as_slice() {
                [path, "export-ignore"] => exclude_pattern(path),
                [path, "-export-ignore"] => exclude_pattern(&format!("!{path}")),
                _ => None,
            }
        })
        .collect()
}

const VCS: [&str; 9] = [
    ".svn",
    "_svn",
    "CVS",
    "_darcs",
    ".arch-params",
    ".monotone",
    ".bzr",
    ".git",
    ".hg",
];

/// `ArchivableFilesFinder`: kept files and empty directories, relative, sorted by name.
pub(crate) fn archivable(
    sources: &Path,
    excludes: &[String],
    ignore_filters: bool,
) -> Vec<(String, bool)> {
    let filters: Vec<Vec<(Regex, bool)>> = if ignore_filters {
        vec![]
    } else {
        vec![
            git_excludes(sources),
            excludes.iter().filter_map(|r| exclude_pattern(r)).collect(),
        ]
    };
    let kept = |rel: &str| {
        let mut exclude = false;
        for filter in &filters {
            for (pattern, negate) in filter {
                if pattern.is_match(rel).unwrap_or(false) {
                    exclude = !negate;
                }
            }
        }
        !exclude
    };
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && VCS.contains(&entry.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            out.push(path.clone());
            if is_dir {
                walk(&path, out);
            }
        }
    }
    let mut all = Vec::new();
    walk(sources, &mut all);
    let root = sources.to_string_lossy().to_string();
    let mut out: Vec<(String, bool)> = all
        .into_iter()
        .filter_map(|path| {
            // symlinks leaving the project are skipped
            let real = std::fs::canonicalize(&path).ok()?;
            let real = crate::fs::normalize_path(&real.to_string_lossy());
            let rel = real.strip_prefix(&root)?;
            // `/a/bc` is not under `/a/b`: only an exact match or a `/` boundary counts
            if !rel.is_empty() && !rel.starts_with('/') {
                return None;
            }
            let rel = rel.to_string();
            if !kept(&rel) {
                return None;
            }
            let is_dir = path.is_dir();
            if is_dir && !crate::fs::is_dir_empty(&path) {
                return None;
            }
            let name = path
                .strip_prefix(sources)
                .ok()?
                .to_string_lossy()
                .to_string();
            Some((name, is_dir))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn write_zip(sources: &Path, entries: &[(String, bool)], target: &Path) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let file = std::fs::File::create(target).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    for (name, is_dir) in entries {
        let mode = std::fs::metadata(sources.join(name))
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644);
        let options = zip::write::SimpleFileOptions::default().unix_permissions(mode);
        if *is_dir {
            zip.add_directory(name.as_str(), options)
                .map_err(|e| e.to_string())?;
        } else {
            zip.start_file(name.as_str(), options)
                .map_err(|e| e.to_string())?;
            let bytes = std::fs::read(sources.join(name)).map_err(|e| e.to_string())?;
            zip.write_all(&bytes).map_err(|e| e.to_string())?;
        }
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// One ustar header block; long names split into prefix and name at a `/`.
fn tar_header(
    name: &str,
    mode: u32,
    size: u64,
    mtime: u64,
    dir: bool,
) -> Result<[u8; 512], String> {
    let mut h = [0u8; 512];
    let (prefix, base) = if name.len() <= 100 {
        ("", name)
    } else {
        name.char_indices()
            .filter(|(i, c)| *c == '/' && *i <= 155 && name.len() - i - 1 <= 100)
            .map(|(i, _)| (&name[..i], &name[i + 1..]))
            .next()
            .ok_or_else(|| format!("Could not archive {name}: path too long for tar"))?
    };
    let put =
        |h: &mut [u8; 512], at: usize, bytes: &[u8]| h[at..at + bytes.len()].copy_from_slice(bytes);
    put(&mut h, 0, base.as_bytes());
    put(&mut h, 100, format!("{:07o}\0", mode & 0o7777).as_bytes());
    put(&mut h, 108, b"0000000\0");
    put(&mut h, 116, b"0000000\0");
    put(&mut h, 124, format!("{size:011o}\0").as_bytes());
    put(&mut h, 136, format!("{mtime:011o}\0").as_bytes());
    put(&mut h, 148, b"        ");
    h[156] = if dir { b'5' } else { b'0' };
    put(&mut h, 257, b"ustar\x0000");
    put(&mut h, 345, prefix.as_bytes());
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    put(&mut h, 148, format!("{sum:06o}\0 ").as_bytes());
    Ok(h)
}

/// `PharArchiver`: files first, then the empty directories. The tar stream is written
/// here and only piped through `gzip`/`bzip2`, so no file name reaches a command line.
fn write_tar(
    sources: &Path,
    entries: &[(String, bool)],
    target: &Path,
    format: &str,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut tar = Vec::new();
    for (name, dir) in entries
        .iter()
        .filter(|(_, d)| !d)
        .chain(entries.iter().filter(|(_, d)| *d))
    {
        let path = sources.join(name);
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        if *dir {
            tar.extend(tar_header(
                &format!("{name}/"),
                meta.permissions().mode(),
                0,
                mtime,
                true,
            )?);
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        tar.extend(tar_header(
            name,
            meta.permissions().mode(),
            bytes.len() as u64,
            mtime,
            false,
        )?);
        tar.extend(&bytes);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.resize(tar.len() + 1024, 0);
    tar.resize(tar.len().div_ceil(10240) * 10240, 0);
    let compressor = match format {
        "tar.gz" => Some("gzip"),
        "tar.bz2" => Some("bzip2"),
        _ => None,
    };
    let Some(compressor) = compressor else {
        return std::fs::write(target, tar).map_err(|e| e.to_string());
    };
    let out = std::fs::File::create(target).map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(compressor)
        .arg("-c")
        .stdin(std::process::Stdio::piped())
        .stdout(out)
        .spawn()
        .map_err(|e| format!("Can not compress to {format} format: {e}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&tar)
        .map_err(|e| e.to_string())?;
    match child.wait() {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!("Can not compress to {format} format")),
    }
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    if args.package.is_some() {
        return Err(
            "romposer archive only archives the current project, run it without a package name."
                .into(),
        );
    }
    let (file, _, root, config, _) = load_project(io)?;
    crate::scripts::fire(io, &config, &root, "pre-archive-cmd")?;
    let format = args
        .format
        .clone()
        .unwrap_or_else(|| config.get_str("archive-format"));
    let dir = args
        .dir
        .clone()
        .unwrap_or_else(|| config.get_str("archive-dir"));

    io.error(&format!("Creating the archive into \"{dir}\"."));
    if !["zip", "tar", "tar.gz", "tar.bz2"].contains(&format.as_str()) {
        return Err(format!("No archiver found to support {format} format"));
    }
    // trust boundary: the archive stays inside --dir
    if let Some(f) = &args.file
        && (f.starts_with('/') || f.split(['/', '\\']).any(|part| part == ".."))
    {
        return Err(format!(
            "The archive file name \"{f}\" must stay inside the archive directory."
        ));
    }
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&file).map_err(|e| e.to_string())?)
            .unwrap_or(Value::Null);
    let archive = &manifest["archive"];
    // `getPackageFilenameParts` of the root package: no dist or source reference
    let parts: Vec<String> = match &args.file {
        Some(f) => vec![f.clone()],
        None => {
            let base = match archive["name"].as_str().filter(|n| !n.is_empty()) {
                Some(n) => n.to_string(),
                None => regex::Regex::new(r"(?i)[^a-z0-9\-_]")
                    .unwrap()
                    .replace_all(&root.package.name, "-")
                    .into_owned(),
            };
            vec![base, root.package.pretty_version.clone()]
        }
    }
    .into_iter()
    .map(|p| p.replace('/', "-"))
    .collect();
    let name = parts.join("-");
    let base = if parts.len() > 1 {
        format!("{}-*", parts[0])
    } else {
        parts[0].clone()
    };
    let mut excludes: Vec<String> = ["zip", "tar", "tar.gz", "tar.bz2"]
        .iter()
        .map(|f| format!("{base}.{f}"))
        .collect();
    if let Some(list) = archive["exclude"].as_array() {
        excludes.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
    }

    crate::fs::ensure_dir(Path::new(&dir))?;
    let target = PathBuf::from(format!(
        "{}/{name}.{format}",
        crate::fs::realpath(Path::new(&dir))
    ));
    if let Some(parent) = target.parent() {
        crate::fs::ensure_dir(parent)?;
    }
    if !target.exists() {
        let sources = PathBuf::from(crate::fs::realpath(Path::new(".")));
        let entries = archivable(&sources, &excludes, args.ignore_filters);
        if format == "zip" {
            write_zip(&sources, &entries, &target)?;
        } else {
            write_tar(&sources, &entries, &target, &format)?;
        }
    }
    let target = target.to_string_lossy().to_string();
    let short = crate::fs::find_shortest_path(&crate::fs::cwd(), &target, true, false);
    if !io.quiet {
        eprint!("Created: ");
    }
    io.write(if short.len() < target.len() {
        &short
    } else {
        &target
    });
    crate::scripts::fire(io, &config, &root, "post-archive-cmd")?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_as_symfony_writes_them() {
        assert_eq!(glob_to_regex("*.txt"), r"(?=[^\.])[^/]*\.txt");
        assert_eq!(glob_to_regex("{a,b}.c"), r"(?=[^\.])(a|b)\.c");
        let (re, negate) = exclude_pattern("/tests").unwrap();
        assert!(
            !negate && re.is_match("/tests/a.php").unwrap() && !re.is_match("/src/tests").unwrap()
        );
        let (re, _) = exclude_pattern("*.md").unwrap();
        assert!(re.is_match("/docs/README.md").unwrap());
    }
}
