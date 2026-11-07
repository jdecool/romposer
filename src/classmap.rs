//! Port of composer/class-map-generator 1.8.0: PHP file cleaner, class
//! scanner, PSR filtering, ambiguity and casing warnings.

use crate::fs::{is_absolute_path, normalize_path};
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const DEFAULT_DUPLICATES_FILTER: &str = r"(?i)(?:^|/)(test|fixture|example|stub)s?/";

static TYPE_PRESENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:class|interface|trait|enum)\s").unwrap());
static CLASS_OR_NAMESPACE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(
        r"(?ix)(?:\b(?<![\\$:>])(?P<type>class|interface|trait|enum)\s+(?P<name>[a-zA-Z_\x7f-\x{10ffff}:][a-zA-Z0-9_\x7f-\x{10ffff}:\-]*)|\b(?<![\\$:>])(?P<ns>namespace)(?P<nsname>\s+[a-zA-Z_\x7f-\x{10ffff}][a-zA-Z0-9_\x7f-\x{10ffff}]*(?:\s*\\\s*[a-zA-Z_\x7f-\x{10ffff}][a-zA-Z0-9_\x7f-\x{10ffff}]*)*)?\s*[\{;])",
    )
    .unwrap()
});
static DOUBLE_SLASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\\/]{2,}").unwrap());
static HEREDOC_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^<<<[ \t]*(['"]?)([a-zA-Z_\x80-\x{10ffff}][a-zA-Z0-9_\x80-\x{10ffff}]*)['"]?(?:\r\n|\n|\r)"#).unwrap()
});

const VCS_DIRS: [&str; 9] = [
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

#[derive(Default)]
pub struct ClassMap {
    pub map: BTreeMap<String, String>,
    /// insertion order of the map, as PHP arrays keep it until `sort()`
    order: Vec<String>,
    pub ambiguous_classes: Vec<(String, Vec<String>)>,
    pub psr_violations: Vec<(String, String)>, // (path, warning)
}

impl ClassMap {
    pub fn has_class(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    pub fn class_path(&self, name: &str) -> Option<&str> {
        self.map.get(name).map(String::as_str)
    }

    pub fn add_class(&mut self, name: &str, path: &str) {
        self.psr_violations
            .retain(|(p, _)| p != &path.replace('\\', "/"));
        if !self.map.contains_key(name) {
            self.order.push(name.to_string());
        }
        self.map.insert(name.to_string(), path.to_string());
    }

    pub fn add_ambiguous_class(&mut self, name: &str, path: &str) {
        match self.ambiguous_classes.iter_mut().find(|(n, _)| n == name) {
            Some((_, paths)) => paths.push(path.to_string()),
            None => self
                .ambiguous_classes
                .push((name.to_string(), vec![path.to_string()])),
        }
    }

    pub fn add_psr_violation(&mut self, warning: String, path: &str) {
        self.psr_violations.push((
            path.replace('\\', "/").trim_end_matches('/').to_string(),
            warning,
        ));
    }

    pub fn clear_psr_violations_by_path(&mut self, prefix: &str) {
        let prefix = prefix.replace('\\', "/").trim_end_matches('/').to_string();
        self.psr_violations
            .retain(|(p, _)| !(p == &prefix || p.starts_with(&format!("{prefix}/"))));
    }

    pub fn psr_violations(&self) -> Vec<String> {
        self.psr_violations.iter().map(|(_, w)| w.clone()).collect()
    }

    /// `getAmbiguousClasses`: `filter` false keeps every duplicate.
    pub fn ambiguous_classes(&self, filter: bool) -> Vec<(String, Vec<String>)> {
        if !filter {
            return self.ambiguous_classes.clone();
        }
        let re = Regex::new(DEFAULT_DUPLICATES_FILTER).unwrap();
        self.ambiguous_classes
            .iter()
            .filter_map(|(class, paths)| {
                let kept: Vec<String> = paths
                    .iter()
                    .filter(|p| !re.is_match(&p.replace('\\', "/")))
                    .cloned()
                    .collect();
                if kept.is_empty() {
                    None
                } else {
                    Some((class.clone(), kept))
                }
            })
            .collect()
    }

    /// `getAmbiguousFolders`: paths that only differ in casing.
    pub fn ambiguous_folders(&self, filter: bool) -> Vec<Vec<String>> {
        let mut paths: Vec<String> = self.map.values().map(|p| p.replace('\\', "/")).collect();
        for (_, dups) in &self.ambiguous_classes {
            paths.extend(dups.iter().map(|p| p.replace('\\', "/")));
        }
        paths.sort();
        let root_re = Regex::new(r"(?i)^(?:[0-9a-z]{2,}:(?://(?:[a-z]:)?)?|[a-z]:)").unwrap();
        let mut first: HashMap<String, String> = HashMap::new();
        let mut variants: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut previous = String::new();
        for path in &paths {
            let common = path
                .bytes()
                .zip(previous.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            let skip_until = if common > 0 {
                path[..common].rfind('/').unwrap_or(0)
            } else {
                0
            };
            previous = path.clone();
            let folded = path.to_lowercase();
            let root_len = root_re.find(path).map_or(0, |m| m.end());
            let mut offset = 0;
            for name in path.split('/') {
                let end = offset + name.len();
                if !name.is_empty() && end > root_len && end > skip_until {
                    let folded_prefix = folded[..end].to_string();
                    let written = path[..end].to_string();
                    match first.get(&folded_prefix) {
                        None => {
                            first.insert(folded_prefix, written);
                        }
                        Some(f) => {
                            if &f[offset..] != name
                                && !variants
                                    .get(&folded_prefix)
                                    .is_some_and(|v| v.iter().any(|(n, _)| n == name))
                            {
                                variants
                                    .entry(folded_prefix)
                                    .or_default()
                                    .push((name.to_string(), written));
                            }
                        }
                    }
                }
                offset = end + 1;
            }
        }
        if variants.is_empty() {
            return vec![];
        }
        let relevant: Option<HashSet<String>> = if filter {
            let re = Regex::new(DEFAULT_DUPLICATES_FILTER).unwrap();
            let mut set = HashSet::new();
            for path in &paths {
                if re.is_match(path) {
                    continue;
                }
                let folded = path.to_lowercase();
                let mut offset = 0;
                while let Some(i) = folded[offset..].find('/') {
                    set.insert(folded[..offset + i].to_string());
                    offset += i + 1;
                }
                set.insert(folded);
            }
            Some(set)
        } else {
            None
        };
        variants
            .into_iter()
            .filter(|(prefix, _)| relevant.as_ref().is_none_or(|r| r.contains(prefix)))
            .map(|(prefix, names)| {
                let mut group = vec![first[&prefix].clone()];
                group.extend(names.into_iter().map(|(_, p)| p));
                group
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

pub struct ClassMapGenerator {
    extensions: Vec<String>,
    scanned: Option<HashSet<String>>,
    pub class_map: ClassMap,
}

impl Default for ClassMapGenerator {
    fn default() -> Self {
        Self::new(&["php", "inc", "hh"])
    }
}

/// Symfony Finder `files()->followLinks()->in($dir)` with the default ignore
/// of VCS directories and dot files, in readdir order.
fn find_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || VCS_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            find_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

impl ClassMapGenerator {
    pub fn new(extensions: &[&str]) -> ClassMapGenerator {
        ClassMapGenerator {
            extensions: extensions.iter().map(|e| e.to_string()).collect(),
            scanned: None,
            class_map: ClassMap::default(),
        }
    }

    pub fn avoid_duplicate_scans(&mut self) {
        self.scanned = Some(HashSet::new());
    }

    /// `scanPaths`
    pub fn scan_paths(
        &mut self,
        path: &str,
        excluded: Option<&Regex>,
        autoload_type: &str,
        namespace: Option<&str>,
    ) -> Result<(), String> {
        let p = Path::new(path);
        let files: Vec<PathBuf> = if p.is_file() {
            vec![p.to_path_buf()]
        } else if p.is_dir() {
            let mut out = Vec::new();
            find_files(p, &mut out);
            out
        } else {
            return Err(format!(
                "Could not scan for classes inside \"{path}\" which does not appear to be a file nor a folder"
            ));
        };
        let cwd = crate::fs::realpath(Path::new(&crate::fs::cwd()));
        for file in files {
            let file_path = file.to_string_lossy().into_owned();
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !self.extensions.iter().any(|e| e == ext) {
                continue;
            }
            let file_path = if !is_absolute_path(&file_path) {
                normalize_path(&format!("{cwd}/{file_path}"))
            } else {
                DOUBLE_SLASHES.replace_all(&file_path, "/").into_owned()
            };
            let real_path = crate::fs::realpath(Path::new(&file_path));
            if self
                .scanned
                .as_ref()
                .is_some_and(|s| s.contains(&real_path))
            {
                continue;
            }
            if let Some(re) = excluded
                && (re.is_match(&real_path.replace('\\', "/"))
                    || re.is_match(&file_path.replace('\\', "/")))
            {
                continue;
            }
            let mut classes = find_classes(&file_path)?;
            if autoload_type != "classmap"
                && let Some(ns) = namespace
            {
                classes =
                    self.filter_by_namespace(classes, &file_path, ns, autoload_type, path, &cwd);
                if !classes.is_empty()
                    && let Some(s) = &mut self.scanned
                {
                    s.insert(real_path.clone());
                }
            } else if let Some(s) = &mut self.scanned {
                s.insert(real_path.clone());
            }
            for class in classes {
                if !self.class_map.has_class(&class) {
                    self.class_map.add_class(&class, &file_path);
                } else if self.class_map.class_path(&class) != Some(file_path.as_str()) {
                    self.class_map.add_ambiguous_class(&class, &file_path);
                }
            }
        }
        Ok(())
    }

    fn filter_by_namespace(
        &mut self,
        classes: Vec<String>,
        file_path: &str,
        base_namespace: &str,
        kind: &str,
        base_path: &str,
        cwd: &str,
    ) -> Vec<String> {
        let mut valid = Vec::new();
        let mut rejected = Vec::new();
        let real_sub = &file_path[(base_path.len() + 1).min(file_path.len())..];
        let real_sub = match real_sub.rfind('.') {
            Some(i) => &real_sub[..i],
            None => real_sub,
        };
        for class in classes {
            let sub_path = if kind == "psr-0" {
                if !base_namespace.is_empty() && !class.starts_with(base_namespace) {
                    rejected.push(class);
                    continue;
                }
                match class.rfind('\\') {
                    Some(i) => format!(
                        "{}{}",
                        class[..i + 1].replace('\\', "/"),
                        class[i + 1..].replace('_', "/")
                    ),
                    None => class.replace('_', "/"),
                }
            } else {
                let sub = if base_namespace.is_empty() {
                    class.as_str()
                } else {
                    &class[base_namespace.len().min(class.len())..]
                };
                sub.replace('\\', "/")
            };
            if sub_path == real_sub {
                valid.push(class);
            } else {
                rejected.push(class);
            }
        }
        if valid.is_empty() {
            let cwd = normalize_path(cwd);
            let short = |p: &str| {
                let n = normalize_path(p);
                match n.strip_prefix(&cwd) {
                    Some(rest) => format!(".{rest}"),
                    None => n,
                }
            };
            let short_path = short(file_path);
            let short_base = short(base_path);
            for class in rejected {
                self.class_map.add_psr_violation(
                    format!("Class {class} located in {short_path} does not comply with {kind} autoloading standard (rule: {base_namespace} => {short_base}). Skipping."),
                    file_path,
                );
            }
            return vec![];
        }
        valid
    }
}

/// `PhpFileParser::findClasses`
pub fn find_classes(path: &str) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|_| {
        if !Path::new(path).exists() {
            format!("File at \"{path}\" does not exist, check your classmap definitions")
        } else {
            format!("File at \"{path}\" is not readable, check its permissions")
        }
    })?;
    let contents = String::from_utf8_lossy(&bytes);
    if contents.trim().is_empty() {
        return Ok(vec![]);
    }
    let matches = TYPE_PRESENT.find_iter(&contents).count();
    if matches == 0 {
        return Ok(vec![]);
    }
    let cleaned = PhpFileCleaner::new(&contents, matches).clean();
    let mut classes = Vec::new();
    let mut namespace = String::new();
    for caps in CLASS_OR_NAMESPACE.captures_iter(&cleaned).flatten() {
        if caps.name("ns").is_some() {
            namespace = format!(
                "{}\\",
                caps.name("nsname")
                    .map_or("", |m| m.as_str())
                    .replace([' ', '\t', '\r', '\n'], "")
            );
            continue;
        }
        let mut name = caps.name("name").map_or("", |m| m.as_str()).to_string();
        if name == "extends" || name == "implements" {
            continue;
        }
        if name.starts_with(':') {
            name = format!("xhp{}", name[1..].replace('-', "_").replace(':', "__"));
        } else if caps
            .name("type")
            .is_some_and(|t| t.as_str().eq_ignore_ascii_case("enum"))
            && let Some(i) = name.rfind(':')
        {
            name.truncate(i);
        }
        classes.push(
            format!("{namespace}{name}")
                .trim_start_matches('\\')
                .to_string(),
        );
    }
    Ok(classes)
}

/// `PhpFileCleaner`: strips strings, heredocs and comments so the class regex
/// only sees code. Works on bytes like the PHP original.
struct PhpFileCleaner<'a> {
    contents: &'a [u8],
    index: usize,
    max_matches: usize,
}

impl<'a> PhpFileCleaner<'a> {
    fn new(contents: &'a str, max_matches: usize) -> PhpFileCleaner<'a> {
        PhpFileCleaner {
            contents: contents.as_bytes(),
            index: 0,
            max_matches,
        }
    }

    fn peek(&self, c: u8) -> bool {
        self.index + 1 < self.contents.len() && self.contents[self.index + 1] == c
    }

    fn clean(&mut self) -> String {
        let len = self.contents.len();
        let mut clean: Vec<u8> = Vec::with_capacity(len);
        let types: [(&[u8], &str); 4] = [
            (b"class", "class"),
            (b"interface", "interface"),
            (b"trait", "trait"),
            (b"enum", "enum"),
        ];
        let reject: &[u8] = b"?\"'</cite";
        while self.index < len {
            self.skip_to_php();
            clean.extend_from_slice(b"<?");
            'inner: while self.index < len {
                let c = self.contents[self.index];
                if c == b'?' && self.peek(b'>') {
                    clean.extend_from_slice(b"?>");
                    self.index += 2;
                    continue 'inner;
                }
                if c == b'"' || c == b'\'' {
                    self.skip_string(c);
                    clean.extend_from_slice(b"null");
                    continue;
                }
                if c == b'<' && self.peek(b'<') {
                    let rest = String::from_utf8_lossy(
                        &self.contents[self.index..self.contents.len().min(self.index + 200)],
                    )
                    .into_owned();
                    if let Some(m) = HEREDOC_START.captures(&rest) {
                        self.index += m[0].len();
                        self.skip_heredoc(m[2].as_bytes());
                        clean.extend_from_slice(b"null");
                        continue;
                    }
                }
                if c == b'/' {
                    if self.peek(b'/') {
                        self.skip_to_newline();
                        continue;
                    }
                    if self.peek(b'*') {
                        self.skip_comment();
                        continue;
                    }
                }
                if self.max_matches == 1 && b"cite".contains(&c) {
                    for (kw, _) in &types {
                        if self.contents[self.index..].starts_with(kw) {
                            let prev_ok =
                                self.index == 0 || !b"$:>".contains(&self.contents[self.index - 1]);
                            let after = &self.contents[self.index + kw.len()..];
                            let ws = after.iter().take_while(|b| b.is_ascii_whitespace()).count();
                            let word_boundary = self.index == 0
                                || !(self.contents[self.index - 1].is_ascii_alphanumeric()
                                    || self.contents[self.index - 1] == b'_');
                            if prev_ok && word_boundary && ws > 0 {
                                let name_len = after[ws..]
                                    .iter()
                                    .take_while(|b| {
                                        b.is_ascii_alphanumeric()
                                            || **b == b'_'
                                            || **b >= 0x7f
                                            || **b == b':'
                                            || **b == b'-'
                                    })
                                    .count();
                                if name_len > 0
                                    && (after[ws].is_ascii_alphabetic()
                                        || after[ws] == b'_'
                                        || after[ws] >= 0x7f
                                        || after[ws] == b':')
                                {
                                    clean.extend_from_slice(
                                        &self.contents
                                            [self.index..self.index + kw.len() + ws + name_len],
                                    );
                                    return String::from_utf8_lossy(&clean).into_owned();
                                }
                            }
                        }
                    }
                }
                self.index += 1;
                let skip = self.contents[self.index..]
                    .iter()
                    .take_while(|b| !reject.contains(b))
                    .count();
                clean.push(c);
                if skip > 0 {
                    clean.extend_from_slice(&self.contents[self.index..self.index + skip]);
                    self.index += skip;
                }
            }
        }
        String::from_utf8_lossy(&clean).into_owned()
    }

    fn skip_to_php(&mut self) {
        while self.index < self.contents.len() {
            if self.contents[self.index] == b'<' && self.peek(b'?') {
                self.index += 2;
                break;
            }
            self.index += 1;
        }
    }

    fn skip_string(&mut self, delimiter: u8) {
        self.index += 1;
        while self.index < self.contents.len() {
            self.index += self.contents[self.index..]
                .iter()
                .take_while(|b| **b != b'\\' && **b != delimiter)
                .count();
            if self.index >= self.contents.len() {
                break;
            }
            if self.contents[self.index] == b'\\' && (self.peek(b'\\') || self.peek(delimiter)) {
                self.index += 2;
                continue;
            }
            if self.contents[self.index] == delimiter {
                self.index += 1;
                break;
            }
            self.index += 1;
        }
    }

    fn skip_comment(&mut self) {
        self.index += 2;
        while self.index < self.contents.len() {
            self.index += self.contents[self.index..]
                .iter()
                .take_while(|b| **b != b'*')
                .count();
            if self.peek(b'/') {
                self.index += 2;
                break;
            }
            self.index += 1;
        }
    }

    fn skip_to_newline(&mut self) {
        self.index += self.contents[self.index..]
            .iter()
            .take_while(|b| **b != b'\r' && **b != b'\n')
            .count();
    }

    fn skip_heredoc(&mut self, delimiter: &[u8]) {
        while self.index < self.contents.len() {
            match self.contents[self.index] {
                b'\t' | b' ' => {
                    self.index += 1;
                    continue;
                }
                c if c == delimiter[0] && self.contents[self.index..].starts_with(delimiter) => {
                    let after = self.contents.get(self.index + delimiter.len()).copied();
                    if after.is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80))
                    {
                        self.index += delimiter.len();
                        return;
                    }
                }
                _ => {}
            }
            self.skip_to_newline();
            self.index += self.contents[self.index..]
                .iter()
                .take_while(|b| **b == b'\r' || **b == b'\n')
                .count();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes_in(src: &str) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!("romposer-classmap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("t{}.php", src.len()));
        std::fs::write(&file, src).unwrap();
        find_classes(file.to_str().unwrap()).unwrap()
    }

    #[test]
    fn finds_classes_interfaces_traits_enums_with_namespaces() {
        assert_eq!(
            classes_in(
                "<?php\nnamespace Foo\\Bar;\nclass Baz {}\ninterface Qux {}\ntrait T {}\nenum E: int { case A = 1; }"
            ),
            vec![
                "Foo\\Bar\\Baz",
                "Foo\\Bar\\Qux",
                "Foo\\Bar\\T",
                "Foo\\Bar\\E"
            ]
        );
        assert_eq!(
            classes_in("<?php\nnamespace A { class X {} }\nnamespace B { class Y {} }"),
            vec!["A\\X", "B\\Y"]
        );
        assert_eq!(
            classes_in(
                "<?php\n// class Nope\n/* class Nope2 */\n$a = 'class Nope3';\n$b = \"class Nope4\";\n$c = <<<EOT\nclass Nope5\nEOT;\nclass Real {}"
            ),
            vec!["Real"]
        );
        assert_eq!(
            classes_in("<?php\n$x = new class extends Foo {};\n$y = Bar::class;\nclass Ok {}"),
            vec!["Ok"]
        );
        assert!(classes_in("<html><div class=\"x\"></div>").is_empty());
        assert!(classes_in("").is_empty());
    }

    #[test]
    fn psr4_filtering_and_violations() {
        let dir = std::env::temp_dir().join(format!("romposer-psr4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/Sub")).unwrap();
        std::fs::write(
            dir.join("src/Good.php"),
            "<?php namespace Acme; class Good {}",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/Sub/Deep.php"),
            "<?php namespace Acme\\Sub; class Deep {}",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/Bad.php"),
            "<?php namespace Other; class Bad {}",
        )
        .unwrap();
        let mut g = ClassMapGenerator::default();
        g.avoid_duplicate_scans();
        g.scan_paths(
            dir.join("src").to_str().unwrap(),
            None,
            "psr-4",
            Some("Acme\\"),
        )
        .unwrap();
        let classes: Vec<&String> = g.class_map.map.keys().collect();
        assert_eq!(classes, vec!["Acme\\Good", "Acme\\Sub\\Deep"]);
        assert_eq!(g.class_map.psr_violations().len(), 1);
        assert!(g.class_map.psr_violations()[0].starts_with("Class Other\\Bad located in"));
    }
}
