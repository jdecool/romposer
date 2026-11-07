//! `composer init`, the non-interactive path of `InitCommand`.
//!
//! ponytail: romposer never asks, so the interactive wizard (package search, .gitignore
//! and install prompts) is not ported; `--repository` takes URLs and JSON objects, not
//! local `.json` files.

use crate::io::Io;
use crate::json::{Map, Value};
use crate::version_selector::parse_name_version_pairs;
use regex::Regex;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Name of the package
    #[arg(long)]
    pub name: Option<String>,
    /// Description of package
    #[arg(long)]
    pub description: Option<String>,
    /// Author name of package
    #[arg(long)]
    pub author: Option<String>,
    /// Type of package (e.g. library, project, metapackage, composer-plugin)
    #[arg(long = "type")]
    pub kind: Option<String>,
    /// Homepage of package
    #[arg(long)]
    pub homepage: Option<String>,
    /// Package to require with a version constraint, e.g. foo/bar:1.0.0 or foo/bar=1.0.0 or "foo/bar 1.0.0"
    #[arg(long)]
    pub require: Vec<String>,
    /// Package to require for development with a version constraint, e.g. foo/bar:1.0.0 or foo/bar=1.0.0 or "foo/bar 1.0.0"
    #[arg(long)]
    pub require_dev: Vec<String>,
    /// Minimum stability (empty or one of: stable, RC, beta, alpha, dev)
    #[arg(short = 's', long)]
    pub stability: Option<String>,
    /// License of package
    #[arg(short = 'l', long)]
    pub license: Option<String>,
    /// Add custom repositories, either by URL or using JSON arrays
    #[arg(long)]
    pub repository: Vec<String>,
    /// Add PSR-4 autoload mapping. Maps your package's namespace to the provided directory. (Expects a relative path, e.g. src/)
    #[arg(short = 'a', long)]
    pub autoload: Option<String>,
}

pub fn run(args: &Args, io: &mut Io) -> i32 {
    match execute(args, io) {
        Ok(code) => code,
        Err(message) => crate::scripts::exit_code(io, &message),
    }
}

/// `git config -l` as a map.
fn git_config() -> Map<String, Value> {
    let Ok(out) = std::process::Command::new("git")
        .args(["config", "-l"])
        .output()
    else {
        return Map::new();
    };
    if !out.status.success() {
        return Map::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `sanitizePackageNameComponent`
fn sanitize(name: &str) -> String {
    let name = Regex::new(r"([a-z])([A-Z])|([A-Z])([A-Z][a-z])")
        .unwrap()
        .replace_all(name, "${1}${3}-${2}${4}")
        .to_lowercase();
    let name = Regex::new(r"^[_.-]+|[_.-]+$|[^a-z0-9_.-]")
        .unwrap()
        .replace_all(&name, "");
    Regex::new(r"([_.-]){2,}")
        .unwrap()
        .replace_all(&name, "$1")
        .into_owned()
}

fn default_name(git: &Map<String, Value>) -> String {
    let cwd = crate::fs::realpath(std::path::Path::new("."));
    let name = sanitize(cwd.rsplit('/').next().unwrap_or(""));
    let vendor = env("COMPOSER_DEFAULT_VENDOR")
        .or_else(|| {
            git.get("github.user")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| env("USERNAME"))
        .or_else(|| env("USER"))
        .unwrap_or_else(|| name.clone());
    format!("{}/{name}", sanitize(&vendor))
}

fn default_author(git: &Map<String, Value>) -> Option<String> {
    let from_git = |k: &str| git.get(k).and_then(Value::as_str).map(str::to_string);
    let name = env("COMPOSER_DEFAULT_AUTHOR").or_else(|| from_git("user.name"))?;
    let email = env("COMPOSER_DEFAULT_EMAIL").or_else(|| from_git("user.email"))?;
    Some(format!("{name} <{email}>"))
}

/// `formatAuthors` / `parseAuthorString`
fn authors(author: &str) -> Result<Value, String> {
    let re =
        Regex::new(r#"^(?P<name>[- .,\p{L}\p{N}\p{Mn}'’"()]+)(?:\s+<(?P<email>.+?)>)?$"#).unwrap();
    let Some(m) = re.captures(author) else {
        return Err("Invalid author string.  Must be in the formats: Jane Doe or John Smith <john@example.com>".into());
    };
    let mut entry = Map::new();
    entry.insert("name".into(), Value::String(m["name"].trim().to_string()));
    if let Some(email) = m.name("email") {
        // ponytail: a shape check stands in for PHP's FILTER_VALIDATE_EMAIL
        let valid = Regex::new(r"^[^@\s]+@[^@\s.]+(\.[^@\s.]+)+$")
            .unwrap()
            .is_match(email.as_str());
        if !valid {
            return Err(format!("Invalid email \"{}\"", email.as_str()));
        }
        entry.insert("email".into(), Value::String(email.as_str().to_string()));
    }
    Ok(Value::Array(vec![Value::Object(entry)]))
}

/// `formatRequirements`: every requirement needs a constraint here.
fn requirements(list: &[String]) -> Result<Value, String> {
    let mut map = Map::new();
    for (name, version) in parse_name_version_pairs(list) {
        let Some(version) = version else {
            return Err(format!(
                "Option {name} is missing a version constraint, use e.g. {name}:^1.0"
            ));
        };
        map.insert(name, Value::String(version));
    }
    Ok(Value::Object(map))
}

/// `RepositoryFactory::configFromString` for URLs and inline JSON.
pub fn repository_config(repo: &str) -> Result<Value, String> {
    if repo.to_lowercase().starts_with("http") {
        return Ok(serde_json::json!({"type": "composer", "url": repo}));
    }
    if repo.starts_with('{') {
        return serde_json::from_str(repo)
            .map_err(|e| format!("\"{repo}\" does not contain valid JSON\n{e}"));
    }
    Err(format!(
        "Invalid repository url ({}) given. Has to be a .json file, an http url or a JSON object.",
        crate::download::sanitize_url(repo)
    ))
}

/// `namespaceFromPackageName`
fn namespace(name: &str) -> String {
    let non_alnum = Regex::new(r"(?i)[^a-z0-9]").unwrap();
    name.split('/')
        .map(|part| {
            non_alnum
                .replace_all(part, " ")
                .split(' ')
                .map(|w| {
                    let mut c = w.chars();
                    c.next()
                        .map(|f| f.to_uppercase().chain(c).collect::<String>())
                        .unwrap_or_default()
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\\")
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    let git = git_config();
    let name = args.name.clone().unwrap_or_else(|| default_name(&git));
    let author = args.author.clone().or_else(|| default_author(&git));

    let valid_name =
        Regex::new(r"^[a-z0-9]([_.-]?[a-z0-9]+)*/[a-z0-9](([_.]|-{1,2})?[a-z0-9]+)*$").unwrap();
    if !valid_name.is_match(&name) {
        return Err(format!(
            "The package name {name} is invalid, it should be lowercase and have a vendor name, a forward slash, and a package name, matching: [a-z0-9_.-]+/[a-z0-9_.-]+"
        ));
    }

    // the options in definition order; author, repositories and stability move to the end
    let mut options = Map::new();
    let string = |v: &Option<String>| v.clone().map(Value::String);
    for (key, value) in [
        ("name", Some(Value::String(name.clone()))),
        ("description", string(&args.description)),
        ("author", string(&author)),
        ("type", string(&args.kind)),
        ("homepage", string(&args.homepage)),
        ("require", (!args.require.is_empty()).then_some(Value::Null)),
        (
            "require-dev",
            (!args.require_dev.is_empty()).then_some(Value::Null),
        ),
        ("stability", string(&args.stability)),
        ("license", string(&args.license)),
        ("autoload", string(&args.autoload)),
    ] {
        if let Some(value) = value {
            options.insert(key.to_string(), value);
        }
    }
    if let Some(author) = options.shift_remove("author") {
        options.insert("authors".into(), authors(author.as_str().unwrap_or(""))?);
    }
    if !args.repository.is_empty() {
        let repos = args
            .repository
            .iter()
            .map(|r| repository_config(r))
            .collect::<Result<Vec<_>, _>>()?;
        options.insert("repositories".into(), Value::Array(repos));
    }
    if let Some(stability) = options.shift_remove("stability") {
        options.insert("minimum-stability".into(), stability);
    }
    options.insert("require".into(), requirements(&args.require)?);
    if !args.require_dev.is_empty() {
        options.insert("require-dev".into(), requirements(&args.require_dev)?);
    }
    if let Some(path) = &args.autoload {
        let mut psr4 = Map::new();
        psr4.insert(
            format!("{}\\", namespace(&name)),
            Value::String(path.clone()),
        );
        let mut autoload = Map::new();
        autoload.insert("psr-4".into(), Value::Object(psr4));
        options.insert("autoload".into(), Value::Object(autoload));
    }

    let file = crate::config::composer_file()?;
    io.error(&format!("Writing {}", file.display()));
    let data = Value::Object(options);
    std::fs::write(&file, crate::json::encode_pretty(&data) + "\n").map_err(|e| e.to_string())?;
    if let Err(errors) = crate::schema::validate(&data, false) {
        io.error("Schema validation error, aborting");
        io.error(&format!(
            "\"{}\" does not match the expected JSON schema:\n - {}",
            file.display(),
            errors.join("\n - ")
        ));
        let _ = std::fs::remove_file(&file);
        return Ok(1);
    }

    if let Some(path) = &args.autoload {
        crate::fs::ensure_dir(std::path::Path::new(path))?;
        if args.require.is_empty() && args.require_dev.is_empty() {
            crate::commands::dump_autoload::run(&Default::default(), io);
        }
        io.error(&format!(
            "PSR-4 autoloading configured. Use \"namespace {};\" in {path}",
            namespace(&name)
        ));
        io.error("Include the Composer autoloader with: require 'vendor/autoload.php';");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_namespaces() {
        assert_eq!(sanitize("MyCoolProject"), "my-cool-project");
        assert_eq!(sanitize("--a__b--"), "a_b");
        assert_eq!(
            namespace("new_projects.acme-extra/package-name"),
            "NewProjectsAcmeExtra\\PackageName"
        );
    }
}
