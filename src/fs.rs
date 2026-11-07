//! Port of the parts of `Composer\Util\Filesystem` whose exact string output
//! is observable (relative install paths, proxy code) plus directory helpers.

use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

static PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^([0-9a-z]{2,}:(?://(?:[a-z]:)?)?|[a-z]:)").unwrap());
static DRIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[A-Z]:/?$").unwrap());

/// `Filesystem::isAbsolutePath`
pub fn is_absolute_path(path: &str) -> bool {
    path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') || path.starts_with("\\\\")
}

/// `Filesystem::normalizePath`
pub fn normalize_path(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    let mut prefix = String::new();
    let mut absolute = String::new();
    if path.starts_with("//") && path.len() > 2 {
        absolute = "//".into();
        path = path[2..].to_string();
    }
    if let Some(m) = PREFIX.find(&path) {
        prefix = m.as_str().to_string();
        path = path[m.end()..].to_string();
    }
    if path.starts_with('/') {
        absolute = "/".into();
        path = path[1..].to_string();
    }
    let mut parts: Vec<&str> = Vec::new();
    let mut up = false;
    for chunk in path.split('/') {
        if chunk == ".." && (!absolute.is_empty() || up) {
            parts.pop();
            up = !(parts.is_empty() || parts.last() == Some(&".."));
        } else if chunk != "." && !chunk.is_empty() {
            parts.push(chunk);
            up = chunk != "..";
        }
    }
    let prefix = {
        // ensure c: is normalized to C:
        let bytes = prefix.as_bytes();
        if bytes.len() >= 2
            && bytes[bytes.len() - 1] == b':'
            && bytes[bytes.len() - 2].is_ascii_alphabetic()
        {
            let (head, tail) = prefix.split_at(prefix.len() - 2);
            if head.is_empty() || head.ends_with("://") {
                format!("{head}{}", tail.to_uppercase())
            } else {
                prefix.clone()
            }
        } else {
            prefix
        }
    };
    format!("{prefix}{absolute}{}", parts.join("/"))
}

fn php_dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => path[..i].to_string(),
        None => ".".into(),
    }
}

fn php_basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// `Filesystem::findShortestPath`
pub fn find_shortest_path(
    from: &str,
    to: &str,
    directories: bool,
    prefer_relative: bool,
) -> String {
    assert!(
        is_absolute_path(from) && is_absolute_path(to),
        "$from ({from}) and $to ({to}) must be absolute paths."
    );
    let mut from = normalize_path(from);
    let to = normalize_path(to);
    if directories {
        from = format!("{}/dummy_file", from.trim_end_matches('/'));
    }
    if php_dirname(&from) == php_dirname(&to) {
        return format!("./{}", php_basename(&to));
    }
    let mut common = to.clone();
    while !format!("{from}/").starts_with(&format!("{common}/"))
        && common != "/"
        && !DRIVE.is_match(&common)
    {
        common = php_dirname(&common).replace('\\', "/");
    }
    if !from.starts_with(&common) {
        return to;
    }
    let common = format!("{}/", common.trim_end_matches('/'));
    let source_depth = from[common.len().min(from.len())..].matches('/').count();
    if !prefer_relative && common == "/" && source_depth > 1 {
        return to;
    }
    let result = format!(
        "{}{}",
        "../".repeat(source_depth),
        &to[common.len().min(to.len())..]
    );
    if result.is_empty() {
        "./".into()
    } else {
        result
    }
}

/// PHP `var_export` of a string.
pub fn var_export_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `Filesystem::findShortestPathCode`
pub fn find_shortest_path_code(
    from: &str,
    to: &str,
    directories: bool,
    static_code: bool,
    prefer_relative: bool,
) -> String {
    assert!(
        is_absolute_path(from) && is_absolute_path(to),
        "$from ({from}) and $to ({to}) must be absolute paths."
    );
    let from = normalize_path(from);
    let to = normalize_path(to);
    if from == to {
        return if directories {
            "__DIR__".into()
        } else {
            "__FILE__".into()
        };
    }
    let mut common = to.clone();
    while !format!("{from}/").starts_with(&format!("{common}/"))
        && common != "/"
        && !DRIVE.is_match(&common)
        && common != "."
    {
        common = php_dirname(&common).replace('\\', "/");
    }
    if !from.starts_with(&common) || common == "." {
        return var_export_str(&to);
    }
    let common = format!("{}/", common.trim_end_matches('/'));
    if to.starts_with(&format!("{from}/")) {
        return format!("__DIR__ . {}", var_export_str(&to[from.len()..]));
    }
    let source_depth =
        from[common.len().min(from.len())..].matches('/').count() + usize::from(directories);
    if !prefer_relative && common == "/" && source_depth > 1 {
        return var_export_str(&to);
    }
    let common_code = if static_code {
        format!("__DIR__ . '{}'", "/..".repeat(source_depth))
    } else {
        format!(
            "{}__DIR__{}",
            "dirname(".repeat(source_depth),
            ")".repeat(source_depth)
        )
    };
    let rel = &to[common.len().min(to.len())..];
    if rel.is_empty() {
        common_code
    } else {
        format!("{common_code}.{}", var_export_str(&format!("/{rel}")))
    }
}

/// `Filesystem::isDirEmpty` (dot files count)
pub fn is_dir_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

/// `Filesystem::ensureDirectoryExists`
pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        return Ok(());
    }
    if dir.exists() {
        return Err(format!("{} exists and is not a directory.", dir.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| {
        format!(
            "{} does not exist and could not be created: {e}",
            dir.display()
        )
    })
}

/// `Filesystem::remove`: file, symlink or directory tree.
pub fn remove(path: &Path) -> Result<(), String> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    let res = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    res.map_err(|e| format!("Could not delete {}: {e}", path.display()))
}

/// `Filesystem::emptyDirectory`
pub fn empty_directory(dir: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_symlink()) && dir.exists() {
        std::fs::remove_file(dir).map_err(|e| e.to_string())?;
    }
    ensure_dir(dir)?;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        remove(&entry.map_err(|e| e.to_string())?.path())?;
    }
    Ok(())
}

/// `realpath()` that falls back to the normalised input when the path does not exist.
pub fn realpath(path: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => normalize_path(&path.to_string_lossy()),
    }
}

/// `Platform::getCwd`
pub fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures from Composer's tests/Composer/Test/Util/FilesystemTest.php.
    #[test]
    fn shortest_paths() {
        let cases: &[(&str, &str, &str, bool, bool)] = &[
            ("/foo/bar", "/foo/bar", "./bar", false, false),
            ("/foo/bar", "/foo/baz", "./baz", false, false),
            ("/foo/bar/", "/foo/baz", "./baz", false, false),
            ("/foo/bar", "/foo/bar", "./", true, false),
            ("/foo/bar", "/foo/baz", "../baz", true, false),
            ("C:/foo/bar/", "c:/foo/baz", "../baz", true, false),
            (
                "/foo/bin/run",
                "/foo/vendor/acme/bin/run",
                "../vendor/acme/bin/run",
                false,
                false,
            ),
            ("/foo/bin/run", "/bar/bin/run", "/bar/bin/run", false, false),
            ("/foo/bin/run", "/bar/bin/run", "/bar/bin/run", true, false),
            (
                "c:/foo/bin/run",
                "d:/bar/bin/run",
                "D:/bar/bin/run",
                true,
                false,
            ),
            (
                "c:\\bin\\run",
                "c:/vendor/acme/bin/run",
                "../vendor/acme/bin/run",
                false,
                false,
            ),
            ("C:/Temp/test", "C:\\Temp", "./", false, false),
            ("/tmp/test", "/tmp", "./", false, false),
            ("/tmp/test/sub", "/tmp", "../", false, false),
            ("/tmp/test/sub", "/tmp", "../../", true, false),
            ("/tmp", "/tmp/test", "test", false, false),
            ("/tmp/test/./", "/tmp/test", "./", true, false),
            ("/tmp/test/../vendor", "/tmp/test", "../test", true, false),
            ("/tmp/test/.././vendor", "/tmp/test", "../test", true, false),
            (
                "C:/Temp/../..",
                "c:\\Temp\\..\\..\\test",
                "./test",
                true,
                false,
            ),
            ("/app/vendor/foo/bar", "/lib", "../../../../lib", true, true),
            ("/tmp", "/tmp/../../test", "../test", true, false),
            ("/tmp", "/test", "../test", true, false),
            ("/foo/bar", "/foo/bar_vendor", "../bar_vendor", true, false),
            ("/foo/bar_vendor", "/foo/bar/src", "../bar/src", true, false),
            (
                "/foo/bar_vendor/src2",
                "/foo/bar/src/lib",
                "../../bar/src/lib",
                true,
                false,
            ),
            ("C:/", "C:/foo/bar/", "foo/bar", true, false),
        ];
        for (from, to, expected, directories, prefer_relative) in cases {
            assert_eq!(
                find_shortest_path(from, to, *directories, *prefer_relative),
                *expected,
                "{from} -> {to}"
            );
        }
    }

    #[test]
    fn shortest_path_code() {
        let cases: &[(&str, &str, bool, &str, bool, bool)] = &[
            ("/foo/bar", "/foo/bar", false, "__FILE__", false, false),
            (
                "/foo/bar",
                "/foo/baz",
                false,
                "__DIR__.'/baz'",
                false,
                false,
            ),
            (
                "/foo/bin/run",
                "/foo/vendor/acme/bin/run",
                false,
                "dirname(__DIR__).'/vendor/acme/bin/run'",
                false,
                false,
            ),
            (
                "/foo/bin/run",
                "/bar/bin/run",
                false,
                "'/bar/bin/run'",
                false,
                false,
            ),
            (
                "c:/bin/run",
                "D:/vendor/acme/bin/run",
                false,
                "'D:/vendor/acme/bin/run'",
                false,
                false,
            ),
            ("/foo/bar", "/foo/bar", true, "__DIR__", false, false),
            ("/foo/bar/", "/foo/bar", true, "__DIR__", false, false),
            (
                "/foo",
                "/baz",
                true,
                "dirname(__DIR__).'/baz'",
                false,
                false,
            ),
            (
                "/foo/bar",
                "/foo/baz",
                true,
                "dirname(__DIR__).'/baz'",
                false,
                false,
            ),
            (
                "/foo/bin/run",
                "/foo/vendor/acme/bin/run",
                true,
                "dirname(dirname(__DIR__)).'/vendor/acme/bin/run'",
                false,
                false,
            ),
            (
                "/app/vendor/foo/bar",
                "/lib",
                true,
                "dirname(dirname(dirname(dirname(__DIR__)))).'/lib'",
                false,
                true,
            ),
            ("/bin/run", "/bin/run", true, "__DIR__", false, false),
            ("/tmp/test", "/tmp", true, "dirname(__DIR__)", false, false),
            ("/tmp", "/tmp/test", true, "__DIR__ . '/test'", false, false),
            ("/tmp/test/./", "/tmp/test/", true, "__DIR__", false, false),
            (
                "/tmp/test/../vendor",
                "/tmp/test",
                true,
                "dirname(__DIR__).'/test'",
                false,
                false,
            ),
            (
                "/foo/bar_vendor/src2",
                "/foo/bar/src/lib",
                true,
                "dirname(dirname(__DIR__)).'/bar/src/lib'",
                false,
                false,
            ),
            (
                "/tmp/test/../vendor",
                "/tmp/test",
                true,
                "__DIR__ . '/..'.'/test'",
                true,
                false,
            ),
            (
                "/foo/bar",
                "/foo/bar_vendor",
                true,
                "__DIR__ . '/..'.'/bar_vendor'",
                true,
                false,
            ),
            (
                "/foo/bar_vendor/src2",
                "/foo/bar/src/lib",
                true,
                "__DIR__ . '/../..'.'/bar/src/lib'",
                true,
                false,
            ),
            (
                "C:/Temp/../..",
                "d:\\Temp\\..\\..\\test",
                true,
                "'D:/test'",
                true,
                false,
            ),
        ];
        for (from, to, directories, expected, static_code, prefer_relative) in cases {
            assert_eq!(
                find_shortest_path_code(from, to, *directories, *static_code, *prefer_relative),
                *expected,
                "{from} -> {to}"
            );
        }
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_path("/foo/./bar//baz/../qux"), "/foo/bar/qux");
        assert_eq!(normalize_path("../foo/../bar"), "../bar");
        assert_eq!(normalize_path("c:/foo/../bar"), "C:/bar");
        assert_eq!(normalize_path("phar:///foo/bar/../baz"), "phar:///foo/baz");
        assert_eq!(normalize_path("/foo/bar/"), "/foo/bar");
        assert_eq!(normalize_path("//foo/bar"), "//foo/bar");
    }
}
