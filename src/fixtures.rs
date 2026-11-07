//! Runner for Composer's installer fixtures (`tests/Composer/Test/Fixtures/installer/*.test`):
//! the `.test` section format of `InstallerTest`, in-memory repositories, an operation
//! trace instead of a filesystem, and PHPUnit's `assertStringMatchesFormat` placeholders.

use crate::config::Config;
use crate::installed::InstalledRepository;
use crate::installer::{Executor, Installer, Options};
use crate::io::Io;
use crate::json::{Map, Value, encode_compact};
use crate::lock::{Lock, php_assoc};
use crate::package::root::load_root;
use crate::package::{self, dumper};
use crate::platform::PhpInfo;
use crate::solver::builder::{ArrayRepo, Repository};
use crate::transaction::Operation;
use regex::Regex;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default)]
pub struct Fixture {
    pub name: String,
    pub test: String,
    pub condition: Option<String>,
    pub composer: Value,
    pub lock: Option<Value>,
    pub installed: Option<Value>,
    pub run: String,
    pub expect_lock: Option<Value>,
    pub expect_lock_never_written: bool,
    pub expect_installed: Option<Value>,
    pub expect_output: Option<String>,
    pub expect: String,
    pub expect_exit_code: i32,
    pub expect_exception: Option<String>,
}

/// `InstallerTest::readTestFile`
pub fn parse(name: &str, contents: &str) -> Result<Fixture, String> {
    let re = Regex::new(r"(?:^|\n*)--([A-Z-]+)--\n").unwrap();
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut last = 0;
    for m in re.captures_iter(contents) {
        let whole = m.get(0).unwrap();
        if let Some(section) = current.take() {
            sections.insert(section, contents[last..whole.start()].to_string());
        }
        current = Some(m[1].to_string());
        last = whole.end();
    }
    if let Some(section) = current {
        sections.insert(section, contents[last..].to_string());
    }
    let known = [
        "TEST",
        "CONDITION",
        "COMPOSER",
        "LOCK",
        "INSTALLED",
        "RUN",
        "EXPECT-LOCK",
        "EXPECT-INSTALLED",
        "EXPECT-OUTPUT",
        "EXPECT-OUTPUT-OPTIMIZED",
        "EXPECT-EXIT-CODE",
        "EXPECT-EXCEPTION",
        "EXPECT",
    ];
    for key in sections.keys() {
        if !known.contains(&key.as_str()) {
            return Err(format!(
                "The test file \"{name}\" must not contain a section named \"{key}\"."
            ));
        }
    }
    for required in ["TEST", "COMPOSER", "RUN", "EXPECT"] {
        if !sections.contains_key(required) {
            return Err(format!(
                "The test file \"{name}\" must have a section named \"{required}\"."
            ));
        }
    }
    let json = |key: &str| -> Result<Option<Value>, String> {
        match sections
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            Some(text) => serde_json::from_str(text)
                .map(Some)
                .map_err(|e| format!("{name}: invalid JSON in --{key}--: {e}")),
            None => Ok(None),
        }
    };
    let mut fixture = Fixture {
        name: name.to_string(),
        test: sections["TEST"].trim().to_string(),
        condition: sections
            .get("CONDITION")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        composer: json("COMPOSER")?.ok_or_else(|| format!("{name}: empty --COMPOSER--"))?,
        lock: json("LOCK")?,
        installed: json("INSTALLED")?,
        run: sections["RUN"].trim().to_string(),
        expect: sections["EXPECT"].clone(),
        expect_output: sections.get("EXPECT-OUTPUT").cloned(),
        ..Default::default()
    };
    match sections.get("EXPECT-LOCK").map(|s| s.trim()) {
        Some("false") => fixture.expect_lock_never_written = true,
        Some("") | None => {}
        Some(text) => {
            fixture.expect_lock = Some(
                serde_json::from_str(text)
                    .map_err(|e| format!("{name}: invalid --EXPECT-LOCK--: {e}"))?,
            )
        }
    }
    fixture.expect_installed = json("EXPECT-INSTALLED")?;
    if let Some(e) = sections
        .get("EXPECT-EXCEPTION")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        fixture.expect_exception = Some(e.to_string());
    } else if let Some(code) = sections
        .get("EXPECT-EXIT-CODE")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        fixture.expect_exit_code = code
            .parse()
            .map_err(|_| format!("{name}: invalid --EXPECT-EXIT-CODE--"))?;
    }
    // a lock without a hash gets the md5 of the manifest, so it counts as fresh
    if let Some(Value::Object(lock)) = &mut fixture.lock
        && !lock.contains_key("hash")
    {
        lock.insert(
            "hash".into(),
            Value::String(format!(
                "{:x}",
                md5::compute(encode_compact(&fixture.composer))
            )),
        );
    }
    Ok(fixture)
}

/// `InstallationManagerMock`: records `(string) $operation` and keeps the local repository in sync.
#[derive(Default)]
pub struct TraceExecutor {
    pub trace: Vec<String>,
}

impl Executor for TraceExecutor {
    fn execute(
        &mut self,
        _io: &mut Io,
        _config: &Config,
        _root: &crate::package::root::RootPackage,
        local_repo: &mut InstalledRepository,
        operations: &[Operation],
        _options: &Options,
    ) -> Result<(), String> {
        for op in operations {
            self.trace.push(op.show());
            match op {
                Operation::Install(p) | Operation::MarkAliasInstalled(p) => {
                    if !local_repo.has_package(p) {
                        local_repo.add_package(p.clone());
                    }
                }
                Operation::Update { initial, target } => {
                    local_repo.remove_package(initial);
                    if !local_repo.has_package(target) {
                        local_repo.add_package(target.clone());
                    }
                }
                Operation::Uninstall(p) | Operation::MarkAliasUninstalled(p) => {
                    local_repo.remove_package(p)
                }
            }
        }
        Ok(())
    }

    fn writes_files(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub passed: bool,
    pub detail: String,
}

/// `assertStringMatchesFormat`: PHPUnit's `%s %S %a %A %w %i %d %x %f %c %e` placeholders.
pub fn matches_format(format: &str, actual: &str) -> bool {
    let mut pattern = String::from("^");
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('e') => pattern.push_str(&regex::escape("/")),
                Some('s') => pattern.push_str("[^\\r\\n]+"),
                Some('S') => pattern.push_str("[^\\r\\n]*"),
                Some('a') => pattern.push_str("(?s:.+)"),
                Some('A') => pattern.push_str("(?s:.*)"),
                Some('w') => pattern.push_str("\\s*"),
                Some('i') => pattern.push_str("[+-]?\\d+"),
                Some('d') => pattern.push_str("\\d+"),
                Some('x') => pattern.push_str("[0-9a-fA-F]+"),
                Some('f') => pattern.push_str("[+-]?\\.?\\d+\\.?\\d*(?:[Ee][+-]?\\d+)?"),
                Some('c') => pattern.push('.'),
                Some('%') => pattern.push('%'),
                Some(other) => {
                    pattern.push_str(&regex::escape("%"));
                    pattern.push_str(&regex::escape(&other.to_string()));
                }
                None => pattern.push('%'),
            }
        } else {
            pattern.push_str(&regex::escape(&c.to_string()));
        }
    }
    pattern.push('$');
    Regex::new(&pattern)
        .map(|re| re.is_match(actual))
        .unwrap_or(false)
}

fn dumped_installed(repo: &InstalledRepository) -> Value {
    let mut packages: Vec<Map<String, Value>> = repo
        .canonical_packages()
        .iter()
        .map(|p| {
            let mut d = dumper::dump(p);
            d.shift_remove("version_normalized");
            d
        })
        .collect();
    packages.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Value::Array(packages.into_iter().map(Value::Object).collect())
}

/// The running PHP, probed once for every fixture.
fn php_info() -> Option<&'static PhpInfo> {
    static PHP: std::sync::OnceLock<Option<PhpInfo>> = std::sync::OnceLock::new();
    PHP.get_or_init(PhpInfo::detect).as_ref()
}

/// Runs one fixture in memory and compares it with its expectations.
pub fn run(fixture: &Fixture) -> Outcome {
    let fail = |detail: String| Outcome {
        passed: false,
        detail,
    };
    let mut env_guard: Option<String> = None;
    if let Some(cond) = &fixture.condition {
        let putenv = Regex::new(r"^putenv\('([A-Z_]+)=([^']*)'\)$").unwrap();
        if let Some(m) = putenv.captures(cond.trim()) {
            // SAFETY: fixtures run one at a time on this thread; the variable is reset below
            unsafe { std::env::set_var(&m[1], &m[2]) };
            env_guard = Some(m[1].to_string());
        } else if cond.trim() != "!defined('HHVM_VERSION')" {
            return fail(format!("condition not evaluated: {cond}"));
        }
    }
    let outcome = run_inner(fixture);
    if let Some(name) = env_guard {
        // SAFETY: see above
        unsafe { std::env::remove_var(name) };
    }
    outcome
}

fn run_inner(fixture: &Fixture) -> Outcome {
    let fail = |detail: String| Outcome {
        passed: false,
        detail,
    };
    let Some(manifest) = fixture.composer.as_object() else {
        return fail("--COMPOSER-- is not an object".into());
    };
    // repositories: inline `package` repositories become array repositories
    let mut package_repos: Vec<ArrayRepo> = Vec::new();
    let repo_list: Vec<&Value> = match manifest.get("repositories") {
        Some(Value::Array(repos)) => repos.iter().collect(),
        Some(Value::Object(repos)) => repos.values().collect(),
        _ => vec![],
    };
    {
        // InstallerTest runs from tests/Composer/Test: path repositories resolve from there
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("reference/composer/tests/Composer/Test");
        for repo in repo_list {
            let scanned: Vec<Value>;
            let defs: Vec<&Value> = match repo.get("type").and_then(Value::as_str) {
                Some("package") => match repo.get("package") {
                    Some(Value::Array(list)) => list.iter().collect(),
                    Some(single @ Value::Object(_)) => vec![single],
                    _ => vec![],
                },
                Some("path") => {
                    match crate::path_repo::scan_path(repo.as_object().unwrap(), &base) {
                        Ok(list) => scanned = list.into_iter().map(Value::Object).collect(),
                        Err(e) => return exception_outcome(fixture, &e),
                    }
                    scanned.iter().collect()
                }
                other => {
                    return fail(format!(
                        "repository type {other:?} is not supported in the fixture runner"
                    ));
                }
            };
            let mut packages = Vec::new();
            for def in defs {
                let Some(obj) = def.as_object() else { continue };
                match package::load(obj) {
                    Ok(p) => {
                        let real = p.alias_of.clone();
                        packages.push(p);
                        if let Some(real) = real {
                            packages.push(*real);
                        }
                    }
                    Err(e) => return exception_outcome(fixture, &e),
                }
            }
            let patterns = |key: &str| -> Option<regex::Regex> {
                match repo.get(key) {
                    Some(Value::Array(list)) => Some(crate::repository::package_names_to_regexp(
                        &list
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>(),
                    )),
                    _ => None,
                }
            };
            package_repos.push(ArrayRepo {
                packages,
                canonical: repo.get("canonical") != Some(&Value::Bool(false)),
                only: patterns("only"),
                exclude: patterns("exclude"),
            });
        }
    }

    let run = fixture.run.split_whitespace().collect::<Vec<_>>();
    let (command, flags) = match run.split_first() {
        Some((c, f)) => (*c, f.to_vec()),
        None => return fail("empty --RUN--".into()),
    };
    if command != "install" && command != "update" {
        return fail("The run command only supports install and update".into());
    }

    let root = match load_root(manifest) {
        Ok(r) => r,
        Err(e) => return exception_outcome(fixture, &e),
    };
    let mut config = Config::in_memory();
    config.merge(manifest);
    let lock = fixture
        .lock
        .as_ref()
        .and_then(Value::as_object)
        .map(|l| Lock { data: l.clone() });
    let mut local_repo = InstalledRepository::default();
    if let Some(Value::Array(installed)) = &fixture.installed {
        for entry in installed {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            match package::load(obj) {
                Ok(p) => local_repo.add_package(p),
                Err(e) => return exception_outcome(fixture, &e),
            }
        }
    }
    let contents = encode_compact(&fixture.composer);
    let mut io = Io::buffered();
    let mut executor = TraceExecutor::default();
    let has = |flag: &str| flags.contains(&flag);
    let mut options = Options::defaults();
    // InstallerTest: `setAuditConfig(new AuditConfig(false))`
    options.audit = false;
    options.write_lock = config.bool("lock");
    options.dev_mode = !has("--no-dev");
    options.dry_run = has("--dry-run");
    options.ignore_all_platform_reqs = has("--ignore-platform-reqs");
    options.ignored_platform_reqs = flags
        .iter()
        .filter_map(|f| f.strip_prefix("--ignore-platform-req=").map(str::to_string))
        .collect();
    if command == "update" {
        let packages: Vec<String> = flags
            .iter()
            .filter(|f| !f.starts_with('-'))
            .map(|f| f.to_string())
            .collect();
        let filtered: Vec<String> = packages
            .iter()
            .filter(|p| !matches!(p.as_str(), "lock" | "nothing" | "mirrors"))
            .cloned()
            .collect();
        options.update = true;
        options.install = !has("--no-install");
        options.update_mirrors = has("--lock") || filtered.len() != packages.len();
        options.update_allow_list = (!filtered.is_empty()).then(|| {
            let mut list: Vec<String> = Vec::new();
            for p in filtered.iter().map(|p| p.to_lowercase()) {
                if !list.contains(&p) {
                    list.push(p);
                }
            }
            list
        });
        options.update_allow_transitive = if has("--with-all-dependencies") {
            crate::solver::UPDATE_LISTED_WITH_TRANSITIVE_DEPS
        } else if has("--with-dependencies") {
            crate::solver::UPDATE_LISTED_WITH_TRANSITIVE_DEPS_NO_ROOT_REQUIRE
        } else {
            crate::solver::UPDATE_ONLY_LISTED
        };
        options.prefer_stable = has("--prefer-stable");
        options.prefer_lowest = has("--prefer-lowest");
        options.minimal_update = has("--minimal-changes");
    }
    let repos: Vec<Repository> = package_repos.into_iter().map(Repository::Array).collect();
    let mut installer = Installer {
        io: &mut io,
        config: &config,
        root: &root,
        lock,
        lock_path: None,
        composer_json: &contents,
        options,
        executor: &mut executor,
        repos,
        php: php_info().cloned(),
        written_lock: None,
        suggesting_packages: vec![],
    };
    let result = installer.run_with(&mut local_repo);
    let written_lock = installer.written_lock.take();
    let output = io.output();

    // the console application turns an exception into exit code 1 and prints it
    let (code, output) = match result {
        Ok(code) => (code, output),
        Err(e) if fixture.expect_exception.is_some() => return exception_outcome(fixture, &e),
        Err(e) => (1, format!("{output}{e}\n")),
    };
    if let Some(exception) = &fixture.expect_exception {
        return fail(format!(
            "expected exception {exception} but the run returned {code}\n{output}"
        ));
    }
    if code != fixture.expect_exit_code {
        return fail(format!(
            "exit code {code}, expected {}\n{output}",
            fixture.expect_exit_code
        ));
    }
    if fixture.expect_lock_never_written && written_lock.is_some() {
        return fail(format!(
            "the lock file was written but --EXPECT-LOCK-- is false\n{output}"
        ));
    }
    if let Some(expected) = &fixture.expect_lock {
        let Some(actual) = written_lock else {
            return fail(format!("the lock file was not written\n{output}"));
        };
        let strip = |v: &Value| -> Value {
            let mut v = v.clone();
            if let Some(o) = v.as_object_mut() {
                for key in ["hash", "content-hash", "_readme", "plugin-api-version"] {
                    o.remove(key);
                }
            }
            php_assoc(v)
        };
        if strip(expected) != strip(&actual) {
            return fail(format!(
                "lock differs\n--- expected\n{}\n--- actual\n{}\n--- output\n{output}",
                crate::json::encode_pretty(&strip(expected)),
                crate::json::encode_pretty(&strip(&actual))
            ));
        }
    }
    if let Some(expected) = &fixture.expect_installed {
        let actual = php_assoc(dumped_installed(&local_repo));
        if php_assoc(expected.clone()) != actual {
            return fail(format!(
                "installed packages differ\n--- expected\n{}\n--- actual\n{}",
                crate::json::encode_pretty(expected),
                crate::json::encode_pretty(&actual)
            ));
        }
    }
    let trace = executor.trace.join("\n");
    if trace != fixture.expect.trim_end() {
        return fail(format!(
            "operations differ\n--- expected\n{}\n--- actual\n{trace}\n--- output\n{output}",
            fixture.expect.trim_end()
        ));
    }
    if let Some(expected) = fixture.expect_output.as_ref().map(|s| s.trim_end())
        && !expected.is_empty()
    {
        let normalized = Regex::new(r"(?m)^    - .*?\.ini$")
            .unwrap()
            .replace_all(&output, "__inilist__")
            .into_owned();
        let normalized = Regex::new(r"(__inilist__\r?\n)+")
            .unwrap()
            .replace_all(&normalized, "__inilist__\n")
            .into_owned();
        if !matches_format(expected, normalized.trim_end()) {
            return fail(format!(
                "output differs\n--- expected\n{expected}\n--- actual\n{}",
                normalized.trim_end()
            ));
        }
    }
    Outcome {
        passed: true,
        detail: String::new(),
    }
}

fn exception_outcome(fixture: &Fixture, error: &str) -> Outcome {
    match &fixture.expect_exception {
        Some(_) => {
            let expected = fixture.expect.trim_end();
            if error.contains(expected) {
                Outcome {
                    passed: true,
                    detail: String::new(),
                }
            } else {
                Outcome {
                    passed: false,
                    detail: format!(
                        "exception message differs\n--- expected\n{expected}\n--- actual\n{error}"
                    ),
                }
            }
        }
        None => Outcome {
            passed: false,
            detail: format!("unexpected error: {error}"),
        },
    }
}

/// Every `.test` file under `dir`, sorted by name.
pub fn list(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "test"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_and_matches_formats() {
        let f = parse("x.test", "--TEST--\nhello\n--COMPOSER--\n{\"require\": {}}\n--RUN--\ninstall --no-dev\n--EXPECT-EXIT-CODE--\n4\n--EXPECT--\nInstalling a/a (1.0.0)\n").unwrap();
        assert_eq!(f.test, "hello");
        assert_eq!(f.run, "install --no-dev");
        assert_eq!(f.expect_exit_code, 4);
        assert_eq!(f.expect.trim_end(), "Installing a/a (1.0.0)");
        assert!(matches_format(
            "Package operations: %d install%S\n%a",
            "Package operations: 1 install, 0 updates\nGenerating autoload files"
        ));
        assert!(!matches_format(
            "Package operations: %d install",
            "Package operations: x install"
        ));
    }
}
