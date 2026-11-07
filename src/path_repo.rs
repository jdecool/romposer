//! Port of Composer's `PathRepository`, `ArtifactRepository`, `PathDownloader` and the git
//! part of `VersionGuesser`. Everything here installs through the plain path, never the store.
//!
//! Path urls are globs from the project's own composer.json; archives in an artifact
//! directory are untrusted: their entries are only listed and read, never extracted here.

use crate::config::expand_path;
use crate::download::sanitize_url;
use crate::git::git;
use crate::io::Io;
use crate::json::{Map, Value};
use crate::package::Package;
use crate::semver::version_parser::{normalize, normalize_branch};
use regex::Regex;
use std::path::{Path, PathBuf};

/// A relative `pattern` resolved from `base`, results spelled as the pattern spells them.
fn resolve(base: &Path, path: &str) -> PathBuf {
    if crate::fs::is_absolute_path(path) {
        PathBuf::from(path)
    } else {
        base.join(path)
    }
}

/// `{a,b}` alternatives, left to right (nested braces included).
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let (mut depth, mut close, mut commas) = (0, None, vec![]);
    for (i, c) in pattern[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(open + i),
            _ => {}
        }
    }
    let Some(close) = close else {
        return vec![pattern.to_string()];
    };
    let mut bounds = vec![open];
    bounds.extend(commas);
    bounds.push(close);
    let rest = &pattern[close + 1..];
    bounds
        .windows(2)
        .flat_map(|w| {
            expand_braces(&format!(
                "{}{}{rest}",
                &pattern[..open],
                &pattern[w[0] + 1..w[1]]
            ))
        })
        .collect()
}

/// `fnmatch` for one path component: `*`, `?` and `[...]` (leading dots are `glob_dirs`'s).
fn component_regex(component: &str) -> Regex {
    let mut re = String::from("^");
    let mut chars = component.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '[' => {
                let mut class = String::from("[");
                if chars.peek() == Some(&'!') {
                    chars.next();
                    class.push('^');
                }
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    if c == '\\' || c == '[' {
                        class.push('\\');
                    }
                    class.push(c);
                }
                class.push(']');
                re.push_str(&class);
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

/// `glob($pattern, GLOB_MARK | GLOB_ONLYDIR | GLOB_BRACE)` with the marks trimmed: the matching
/// directories, each alternative's matches sorted.
pub fn glob_dirs(pattern: &str, base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for pattern in expand_braces(pattern) {
        let absolute = pattern.starts_with('/');
        let mut found = vec![if absolute {
            "/".to_string()
        } else {
            String::new()
        }];
        for component in pattern.split('/').filter(|c| !c.is_empty()) {
            let wild = component.contains(['*', '?', '[']);
            let mut next = Vec::new();
            for prefix in &found {
                let join = |name: &str| {
                    format!(
                        "{prefix}{}{name}",
                        if prefix.is_empty() || prefix.ends_with('/') {
                            ""
                        } else {
                            "/"
                        }
                    )
                };
                if !wild {
                    next.push(join(component));
                    continue;
                }
                let dir = resolve(base, if prefix.is_empty() { "." } else { prefix });
                let re = component_regex(component);
                let mut names: Vec<String> = std::fs::read_dir(dir)
                    .map(|d| {
                        d.flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .filter(|n| {
                                re.is_match(n)
                                    && (!n.starts_with('.') || component.starts_with('.'))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                names.sort();
                next.extend(names.iter().map(|n| join(n)));
            }
            found = next;
        }
        let mut matches: Vec<String> = found
            .into_iter()
            .filter(|p| !p.is_empty() && resolve(base, p).is_dir())
            .collect();
        matches.sort();
        out.extend(matches);
    }
    out
}

/// PHP `serialize()` of a decoded JSON value (objects are arrays, numeric keys are ints).
fn php_serialize(value: &Value) -> String {
    let key = |k: &str| match k.parse::<i64>() {
        Ok(n) if n.to_string() == k => format!("i:{n};"),
        _ => format!("s:{}:\"{k}\";", k.len()),
    };
    match value {
        Value::Null => "N;".into(),
        Value::Bool(b) => format!("b:{};", u8::from(*b)),
        Value::Number(n) if n.is_i64() || n.is_u64() => format!("i:{n};"),
        // ponytail: floats use Rust's shortest form, PHP's differs for exponents
        Value::Number(n) => format!("d:{};", n.as_f64().unwrap_or(0.0)),
        Value::String(s) => format!("s:{}:\"{s}\";", s.len()),
        Value::Array(list) => format!(
            "a:{}:{{{}}}",
            list.len(),
            list.iter()
                .enumerate()
                .map(|(i, v)| format!("i:{i};{}", php_serialize(v)))
                .collect::<String>()
        ),
        Value::Object(map) => format!(
            "a:{}:{{{}}}",
            map.len(),
            map.iter()
                .map(|(k, v)| format!("{}{}", key(k), php_serialize(v)))
                .collect::<String>()
        ),
    }
}

fn parse_object(json: &str, file: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::Object(o)) => Ok(o),
        Ok(_) => Err(format!(
            "\"{file}\" does not contain valid JSON\nExpected an object"
        )),
        Err(e) => Err(format!("\"{file}\" does not contain valid JSON\n{e}")),
    }
}

/// `PathRepository::initialize`: one package per matching directory with a composer.json.
pub fn scan_path(
    repo: &Map<String, Value>,
    base: &Path,
) -> Result<Vec<Map<String, Value>>, String> {
    let raw_url = repo
        .get("url")
        .and_then(Value::as_str)
        .ok_or("You must specify the `url` configuration for the path repository")?;
    let url = expand_path(raw_url);
    let mut options = match repo.get("options") {
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    };
    if !options.contains_key("relative") {
        options.insert(
            "relative".into(),
            Value::Bool(!crate::fs::is_absolute_path(&url)),
        );
    }
    let matches = glob_dirs(&url, base);
    if matches.is_empty() {
        let wildcard = Regex::new(r"[*{}]").unwrap();
        if wildcard.is_match(&url) {
            let mut dir = url.clone();
            while wildcard.is_match(&dir) {
                dir = Path::new(&dir)
                    .parent()
                    .map_or(".".into(), |p| p.to_string_lossy().into_owned());
                if dir.is_empty() {
                    dir = ".".into();
                }
            }
            if resolve(base, &dir).is_dir() {
                return Ok(vec![]);
            }
        }
        return Err(format!(
            "The `url` supplied for the path ({}) repository does not exist",
            sanitize_url(&url)
        ));
    }
    let reference = options
        .get("reference")
        .and_then(Value::as_str)
        .unwrap_or("auto")
        .to_string();
    let serialized = php_serialize(&Value::Object(options.clone()));
    let mut packages = Vec::new();
    for matched in matches {
        let path = std::fs::canonicalize(resolve(base, &matched)).map_err(|e| e.to_string())?;
        let file = path.join("composer.json");
        if !file.exists() {
            continue;
        }
        let json = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        let file_name = file.to_string_lossy().into_owned();
        let mut package = parse_object(&json, &file_name)?;
        package.remove("published-time");
        let mut dist = Map::new();
        dist.insert("type".into(), Value::String("path".into()));
        dist.insert("url".into(), Value::String(matched.clone()));
        match reference.as_str() {
            "none" => {
                dist.insert("reference".into(), Value::Null);
            }
            "config" | "auto" => {
                dist.insert(
                    "reference".into(),
                    Value::String(crate::download::sha1_hex(
                        format!("{json}{serialized}").as_bytes(),
                    )),
                );
            }
            _ => {}
        }
        let transport: Map<String, Value> = options
            .iter()
            .filter(|(k, _)| *k == "symlink" || *k == "relative")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(version) = name.as_ref().and_then(|n| options.get("versions")?.get(n)) {
            package.insert("version".into(), version.clone());
        }
        // a path repo in the root package's git repository carries its version over
        if !package.contains_key("version")
            && let Some(root_version) = crate::package::root::root_version_from_env()
        {
            let here = git(&["rev-parse", "HEAD"], Some(&path));
            let root = git(&["rev-parse", "HEAD"], None);
            if here.ok && root.ok && here.stdout == root.stdout {
                package.insert("version".into(), Value::String(root_version));
            }
        }
        if reference == "auto" && path.join(".git").is_dir() {
            let out = git(
                &[
                    "rev-list",
                    "--no-commit-header",
                    "-n1",
                    "--format=%H",
                    "HEAD",
                ],
                Some(&path),
            );
            if out.ok {
                dist.insert("reference".into(), Value::String(out.stdout.trim().into()));
            }
        }
        package.insert("dist".into(), Value::Object(dist));
        package.insert("transport-options".into(), Value::Object(transport));
        if !package.contains_key("version") {
            let version = match guess_version(&package, &path) {
                Some(guess) if !guess.pretty_version.is_empty() => {
                    if let Some((_, feature_pretty)) = guess.feature {
                        let mut feature = package.clone();
                        feature.insert("version".into(), Value::String(feature_pretty));
                        packages.push(load(feature, &file_name)?);
                    }
                    guess.pretty_version
                }
                _ => "dev-main".into(),
            };
            package.insert("version".into(), Value::String(version));
        }
        packages.push(load(package, &file_name)?);
    }
    Ok(packages)
}

/// `ArrayLoader::load` as a check: the map is kept for the repository's own loading.
fn load(package: Map<String, Value>, file: &str) -> Result<Map<String, Value>, String> {
    crate::package::load(&package)
        .map_err(|e| format!("Failed loading the package in {file}\n{e}"))?;
    Ok(package)
}

/// `ArtifactRepository::scanDirectory`: every zip/tar archive holding a composer.json.
pub fn scan_artifact(
    io: &mut Io,
    repo: &Map<String, Value>,
    base: &Path,
) -> Result<Vec<Map<String, Value>>, String> {
    let lookup = expand_path(repo.get("url").and_then(Value::as_str).unwrap_or(""));
    let mut files = Vec::new();
    // ponytail: entries are visited sorted by name, RecursiveDirectoryIterator uses readdir order
    fn walk(dir: &Path, shown: &str, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let shown = format!("{shown}/{name}");
            if path.is_dir() {
                walk(&path, &shown, out);
            } else if path.is_file() {
                out.push((path, shown));
            }
        }
    }
    let root = resolve(base, &lookup);
    if !root.is_dir() {
        return Err(format!(
            "RecursiveDirectoryIterator::__construct({lookup}): Failed to open directory: No such file or directory"
        ));
    }
    walk(&root, lookup.trim_end_matches('/'), &mut files);
    let archive = Regex::new(r"(?i)^.+\.(zip|tar|gz|tgz)$").unwrap();
    let mut packages = Vec::new();
    for (path, shown) in files {
        if !archive.is_match(&shown) {
            continue;
        }
        let basename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let extension = basename.rsplit_once('.').map_or("", |(_, e)| e);
        let kind = match extension {
            "gz" | "tar" | "tgz" => "tar",
            "zip" => "zip",
            other => {
                return Err(format!(
                    "Files with \"{other}\" extensions aren't supported. Only ZIP and TAR/TAR.GZ/TGZ archives are supported."
                ));
            }
        };
        let json = match archive_composer_json(&path, kind) {
            Ok(json) => json,
            Err(e) => {
                if io.verbosity >= 1 {
                    io.write(&format!("Failed loading package {shown}: {e}"));
                }
                None
            }
        };
        let Some(json) = json else {
            if io.verbosity >= 1 {
                io.error(&format!("File {basename} doesn't seem to hold a package"));
            }
            continue;
        };
        let mut package = parse_object(&json, &format!("{shown}#composer.json"))?;
        package.remove("published-time");
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        let mut dist = Map::new();
        dist.insert("type".into(), Value::String(kind.into()));
        dist.insert("url".into(), Value::String(shown.replace('\\', "/")));
        dist.insert(
            "shasum".into(),
            Value::String(crate::download::sha1_hex(&bytes)),
        );
        package.insert("dist".into(), Value::Object(dist));
        let loaded = crate::package::load(&package)
            .map_err(|e| format!("Failed loading package in {}: {e}", path.display()))?;
        if io.verbosity >= 1 {
            io.error(&format!(
                "Found package {} ({}) in file {basename}",
                loaded.name, loaded.pretty_version
            ));
        }
        packages.push(package);
    }
    Ok(packages)
}

const TOP_LEVEL: &str = "Archive has more than one top level directories, and no composer.json was found on the top level, so it's an invalid archive. Top level paths found were: ";
const NOT_FOUND: &str =
    "No composer.json found either at the top level or within the topmost directory";

/// `Zip::getComposerJson` / `Tar::getComposerJson`: the top-level composer.json, or the one in
/// the single top-level directory.
fn archive_composer_json(path: &Path, kind: &str) -> Result<Option<String>, String> {
    let metadata = |name: &str| {
        name.split('/')
            .any(|c| Regex::new(r"^(?:\._.*|\.DS_Store|__MACOSX|\.AppleDouble|\.fseventsd|\.Spotlight-V100|\.Trashes|(?i:thumbs\.db|desktop\.ini))$").unwrap().is_match(c))
    };
    if kind == "zip" {
        let Ok(file) = std::fs::File::open(path) else {
            return Ok(None);
        };
        let Ok(mut zip) = zip::ZipArchive::new(file) else {
            return Ok(None);
        };
        if zip.is_empty() {
            return Ok(None);
        }
        let read = |zip: &mut zip::ZipArchive<std::fs::File>, name: &str| -> Option<String> {
            let mut entry = zip.by_name(name).ok()?;
            if entry.is_dir() {
                return None;
            }
            let mut s = String::new();
            std::io::Read::read_to_string(&mut entry, &mut s).ok()?;
            Some(s)
        };
        if let Some(json) = read(&mut zip, "composer.json") {
            return Ok(Some(json));
        }
        let mut top: Vec<String> = Vec::new();
        let names: Vec<String> = zip
            .file_names()
            .filter_map(|n| n.ok().map(|n| n.into_owned()))
            .collect();
        for name in names {
            if metadata(&name) {
                continue;
            }
            let trimmed = name.trim_end_matches('/');
            let candidate = match trimmed.rfind('/') {
                None => name.clone(),
                Some(i) if !trimmed[..i].contains(['/', '\\']) => format!("{}/", &trimmed[..i]),
                Some(_) => continue,
            };
            if !top.contains(&candidate) {
                top.push(candidate);
                if top.len() > 1 {
                    return Err(format!("{TOP_LEVEL}{}", top.join(",")));
                }
            }
        }
        if let Some(dir) = top.first()
            && let Some(json) = read(&mut zip, &format!("{dir}composer.json"))
        {
            return Ok(Some(json));
        }
        return Err(NOT_FOUND.into());
    }
    let Ok(raw) = crate::download::tar_entries(path) else {
        return Ok(None);
    };
    let entries: Vec<String> = raw
        .iter()
        .map(|e| e.trim_start_matches("./").to_string())
        .filter(|e| !e.is_empty())
        .collect();
    let member = if entries.iter().any(|e| e == "composer.json") {
        "composer.json".to_string()
    } else {
        let mut top: Vec<String> = Vec::new();
        for entry in &entries {
            let first = entry.split('/').next().unwrap_or("");
            let is_dir = entry.contains('/');
            if metadata(first) || !is_dir || top.iter().any(|t| t == first) {
                continue;
            }
            top.push(first.to_string());
            if top.len() > 1 {
                return Err(format!("{TOP_LEVEL}{}", top.join(",")));
            }
        }
        let member = top.first().map(|t| format!("{t}/composer.json"));
        match member {
            Some(m) if entries.contains(&m) => m,
            _ => return Err(NOT_FOUND.into()),
        }
    };
    // the listing may spell it with a leading ./
    let spelled = raw
        .into_iter()
        .find(|e| e.trim_start_matches("./") == member)
        .unwrap_or(member);
    let out = std::process::Command::new("tar")
        .arg("-xOf")
        .arg(path)
        .arg("--")
        .arg(&spelled)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// `VersionGuesser::guessVersion` result.
pub struct Guess {
    pub version: String,
    pub pretty_version: String,
    pub commit: Option<String>,
    pub feature: Option<(String, String)>,
}

fn is_feature_branch(package: &Map<String, Value>, branch: &str) -> bool {
    let non_feature = match package.get("non-feature-branches") {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|"),
        _ => String::new(),
    };
    !Regex::new(&format!(
        r"^({non_feature}|master|main|latest|next|current|support|tip|trunk|default|develop|\d+\..+)$"
    ))
    .is_ok_and(|r| r.is_match(branch))
}

/// `VersionGuesser::guessVersion` for git checkouts.
// ponytail: hg, fossil and svn working copies are not guessed, they end up as dev-main
pub fn guess_version(package: &Map<String, Value>, path: &Path) -> Option<Guess> {
    let mut version: Option<String> = None;
    let mut pretty: Option<String> = None;
    let mut commit: Option<String> = None;
    let mut feature: Option<(String, String)> = None;
    let mut detached = false;
    let out = git(
        &["branch", "-a", "--no-color", "--no-abbrev", "-v"],
        Some(path),
    );
    if out.ok {
        let current = Regex::new(r"^(?:\* ) *(\(no branch\)|\(detached from \S+\)|\(HEAD detached at \S+\)|\S+) *([a-f0-9]+) .*$").unwrap();
        let head = Regex::new(r"^ *.+/HEAD ").unwrap();
        let listed =
            Regex::new(r"^(?:\* )? *((?:remotes/(?:origin|upstream)/)?[^\s/]+) *([a-f0-9]+) .*$")
                .unwrap();
        let mut branches = Vec::new();
        let mut is_feature = false;
        for line in out.stdout.lines() {
            if let Some(m) = current.captures(line) {
                if &m[1] == "(no branch)"
                    || m[1].starts_with("(detached ")
                    || m[1].starts_with("(HEAD detached at")
                {
                    version = Some(format!("dev-{}", &m[2]));
                    pretty = version.clone();
                    is_feature = true;
                    detached = true;
                } else {
                    version = Some(normalize_branch(&m[1]));
                    pretty = Some(format!("dev-{}", &m[1]));
                    is_feature = is_feature_branch(package, &m[1]);
                }
                commit = Some(m[2].to_string());
            }
            if !line.is_empty()
                && !head.is_match(line)
                && let Some(m) = listed.captures(line)
            {
                branches.push(m[1].to_string());
            }
        }
        if is_feature && let (Some(v), Some(p)) = (version.clone(), pretty.clone()) {
            feature = Some((v.clone(), p));
            let (v, p) = guess_feature_version(package, &v, branches, path);
            version = Some(v);
            pretty = Some(p);
        }
    }
    if version.is_none() || detached {
        let out = git(&["describe", "--exact-match", "--tags"], Some(path));
        if out.ok
            && let Ok(v) = normalize(out.stdout.trim(), None)
        {
            version = Some(v);
            pretty = Some(out.stdout.trim().to_string());
            feature = None;
        }
    }
    if commit.is_none() {
        let out = git(
            &[
                "rev-list",
                "--no-commit-header",
                "--format=%H",
                "-n1",
                "HEAD",
            ],
            Some(path),
        );
        if out.ok {
            commit = Some(out.stdout.trim().to_string()).filter(|c| !c.is_empty());
        }
    }
    let mut guess = Guess {
        version: version?,
        pretty_version: pretty.unwrap_or_default(),
        commit,
        feature,
    };
    // `postprocess`
    if guess
        .feature
        .as_ref()
        .is_some_and(|(v, p)| *v == guess.version && *p == guess.pretty_version)
    {
        guess.feature = None;
    }
    let nines = Regex::new(r"(\.9{7})+").unwrap();
    if guess.version.ends_with("-dev") && guess.version.contains(".9999999") {
        guess.pretty_version = nines.replace_all(&guess.version, ".x").into_owned();
    }
    if let Some((v, p)) = &mut guess.feature
        && v.ends_with("-dev")
        && v.contains(".9999999")
    {
        *p = nines.replace_all(v, ".x").into_owned();
    }
    Some(guess)
}

/// `guessFeatureVersion`: the nearest version branch a feature branch came from.
fn guess_feature_version(
    package: &Map<String, Value>,
    version: &str,
    mut branches: Vec<String>,
    path: &Path,
) -> (String, String) {
    let aliased = package
        .get("extra")
        .and_then(|e| e.get("branch-alias"))
        .and_then(|a| a.get(version))
        .is_some();
    let mut result = (version.to_string(), version.to_string());
    if aliased
        && !crate::json::encode_compact(&Value::Object(package.clone()))
            .contains("\"self.version\"")
    {
        return result;
    }
    let branch = version.strip_prefix("dev-").unwrap_or(version).to_string();
    if !is_feature_branch(package, &branch) {
        return result;
    }
    crate::zend_sort::usort(&mut branches, |a, b| {
        let (ar, br) = (a.starts_with("remotes/"), b.starts_with("remotes/"));
        if ar != br {
            return if ar {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
        }
        crate::autoload::natcasecmp(b, a)
    });
    let remote = Regex::new(r"^remotes/\S+/").unwrap();
    let mut length = usize::MAX;
    for candidate in &branches {
        let candidate_version = remote.replace(candidate, "").into_owned();
        if *candidate == branch || is_feature_branch(package, &candidate_version) {
            continue;
        }
        // revisions are refs from `git branch`, never options
        if candidate.starts_with('-') || branch.starts_with('-') {
            continue;
        }
        let out = git(&["rev-list", &format!("{candidate}..{branch}")], Some(path));
        if !out.ok {
            continue;
        }
        if out.stdout.len() <= length {
            length = out.stdout.len();
            result = (
                normalize_branch(&candidate_version),
                format!("dev-{candidate_version}"),
            );
            if length == 0 {
                break;
            }
        }
    }
    result
}

// --- PathDownloader ---

fn dist_url(package: &Package) -> &str {
    package.dist.as_ref().map_or("", |d| d.url.as_str())
}

/// `computeAllowedStrategies`: (symlink first, mirror allowed as a fallback)
fn strategies(package: &Package) -> (bool, bool) {
    let symlink = package
        .transport_options
        .as_ref()
        .and_then(|t| t.get("symlink"))
        .and_then(Value::as_bool);
    let mirror_env = crate::scripts::get_env("COMPOSER_MIRROR_PATH_REPOS")
        .is_some_and(|v| !v.is_empty() && v != "0");
    match symlink {
        Some(true) => (true, false),
        Some(false) => (false, true),
        None => (!mirror_env, true),
    }
}

/// `PathDownloader::download`: the source must exist and not contain the install path.
pub fn check_source(package: &Package, path: &Path) -> Result<(), String> {
    let url = dist_url(package);
    let real = std::fs::canonicalize(url)
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| {
            format!(
                "Source path \"{url}\" is not found for package {}",
                package.name
            )
        })?;
    if let Ok(target) = std::fs::canonicalize(path)
        && target != real
        && target.starts_with(&real)
    {
        return Err(format!(
            "Package {} cannot install to \"{}\" inside its source at \"{}\"",
            package.name,
            target.display(),
            real.display()
        ));
    }
    Ok(())
}

/// `getInstallOperationAppendix`
pub fn appendix(package: &Package, path: &Path) -> String {
    let url = dist_url(package);
    if std::fs::canonicalize(url).is_ok_and(|r| std::fs::canonicalize(path).is_ok_and(|p| p == r)) {
        return ": Source already present".into();
    }
    if strategies(package).0 {
        format!(": Symlinking from {url}")
    } else {
        format!(": Mirroring from {url}")
    }
}

/// `PathDownloader::install`: a relative (or absolute) symlink to the source, or a copy of its
/// archivable files. `label` is printed with what was done, when given.
pub fn install(
    io: &mut Io,
    package: &Package,
    path: &Path,
    label: Option<&str>,
) -> Result<(), String> {
    let url = dist_url(package);
    let real = std::fs::canonicalize(url).map_err(|_| format!("Failed to realpath {url}"))?;
    if std::fs::canonicalize(path).is_ok_and(|p| p == real) {
        if let Some(label) = label {
            io.error(&format!("  - {label}: Source already present"));
        }
        return Ok(());
    }
    let (mut symlink, mirror_allowed) = strategies(package);
    let relative = package
        .transport_options
        .as_ref()
        .and_then(|t| t.get("relative"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    crate::fs::remove(path)?;
    let mut line = label.map(|l| format!("  - {l}: "));
    let mut fallback = false;
    if symlink {
        if let Some(line) = &mut line {
            line.push_str(&format!("Symlinking from {url}"));
        }
        let real_str = real.to_string_lossy().into_owned();
        let target = if relative {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                Path::new(&crate::fs::cwd()).join(path)
            };
            crate::fs::find_shortest_path(&absolute.to_string_lossy(), &real_str, false, true)
        } else {
            real_str
        };
        let created = path
            .parent()
            .map_or(Ok(()), crate::fs::ensure_dir)
            .and_then(|()| {
                std::os::unix::fs::symlink(format!("{target}/"), path).map_err(|e| e.to_string())
            });
        if created.is_err() {
            if !mirror_allowed {
                return Err(format!(
                    "Symlink from \"{}\" to \"{}\" failed!",
                    real.display(),
                    path.display()
                ));
            }
            if let Some(l) = line.take() {
                io.error(&l);
                io.error("    Symlink failed, fallback to use mirroring!");
                line = Some(String::new());
            }
            symlink = false;
            fallback = true;
        }
    }
    if !symlink {
        if let Some(line) = &mut line {
            line.push_str(&format!(
                "{}Mirroring from {url}",
                if fallback { "    " } else { "" }
            ));
        }
        mirror(&real, path)?;
    }
    if let Some(line) = line {
        io.error(&line);
    }
    Ok(())
}

/// Symfony `Filesystem::mirror` over `ArchivableFilesFinder($source, [])`.
fn mirror(source: &Path, target: &Path) -> Result<(), String> {
    let sources = crate::fs::normalize_path(&source.to_string_lossy());
    crate::fs::ensure_dir(target)?;
    for (name, is_dir) in crate::commands::archive::archivable(Path::new(&sources), &[], false) {
        let from = source.join(&name);
        let to = target.join(&name);
        if let Some(parent) = to.parent() {
            crate::fs::ensure_dir(parent)?;
        }
        if std::fs::symlink_metadata(&from).is_ok_and(|m| m.is_symlink()) {
            let link = std::fs::read_link(&from).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&to);
            std::os::unix::fs::symlink(link, &to).map_err(|e| e.to_string())?;
        } else if is_dir {
            crate::fs::ensure_dir(&to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| {
                format!(
                    "Failed to copy \"{}\" to \"{}\": {e}",
                    from.display(),
                    to.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_options_like_php() {
        let v: Value = serde_json::from_str(
            r#"{"symlink":false,"versions":{"a/b":"1.0"},"0":1,"relative":true}"#,
        )
        .unwrap();
        assert_eq!(
            php_serialize(&v),
            r#"a:4:{s:7:"symlink";b:0;s:8:"versions";a:1:{s:3:"a/b";s:3:"1.0";}i:0;i:1;s:8:"relative";b:1;}"#
        );
    }

    #[test]
    fn globs_directories_with_braces() {
        let dir = std::env::temp_dir().join(format!("romposer-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["p/b", "p/a", "p/.hidden", "q/c"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("p/file"), "").unwrap();
        assert_eq!(glob_dirs("p/*", &dir), ["p/a", "p/b"]);
        assert_eq!(glob_dirs("{q,p}/*/", &dir), ["q/c", "p/a", "p/b"]);
        assert_eq!(glob_dirs("./p/[ab]", &dir), ["./p/a", "./p/b"]);
        assert!(glob_dirs("p/missing", &dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
