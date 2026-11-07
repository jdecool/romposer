//! The small inspection commands: `licenses`, `fund`, `suggests`, `status`, `outdated`.
//!
//! ponytail: `status` only knows dist installs, so it always reports no local
//! changes (VCS checks arrive with ticket 16).

use crate::commands::dump_autoload::load_project;
use crate::commands::table;
use crate::installed::InstalledRepository;
use crate::io::Io;
use crate::json::{Map, Value, encode};
use crate::package::Package;
use crate::platform::{PhpInfo, PlatformRepository, overrides_from_config};
use crate::semver::Stability;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

fn fail(io: &mut Io, result: Result<i32, String>) -> i32 {
    match result {
        Ok(code) => code,
        Err(message) => {
            io.error(&message);
            1
        }
    }
}

fn local_repo(config: &crate::config::Config) -> Result<InstalledRepository, String> {
    InstalledRepository::read(Path::new(&format!(
        "{}/composer/installed.json",
        config.vendor_dir()
    )))
}

// ---------------------------------------------------------------- licenses

#[derive(clap::Args, Debug)]
pub struct LicensesArgs {
    /// Format of the output: text, json or summary
    #[arg(short = 'f', long, default_value = "text")]
    pub format: String,
    /// Disables search in require-dev packages.
    #[arg(long)]
    pub no_dev: bool,
    /// Shows licenses for packages in the lock file instead of installed packages
    #[arg(long)]
    pub locked: bool,
}

pub fn licenses(args: &LicensesArgs, io: &mut Io) -> i32 {
    let r = licenses_inner(args, io);
    fail(io, r)
}

fn licenses_inner(args: &LicensesArgs, io: &mut Io) -> Result<i32, String> {
    let (_, _, root, config, lock) = load_project(io)?;
    let mut packages: Vec<Package> = if args.locked {
        let Some(lock) = lock.as_ref().filter(|l| l.is_locked()) else {
            return Err("Valid composer.json and composer.lock files are required to run this command with --locked".into());
        };
        lock.locked_packages(!args.no_dev)?
    } else {
        let repo = local_repo(&config)?;
        if args.no_dev {
            crate::commands::show::filter_required_packages(&repo.packages, &root.package)
        } else {
            repo.packages
        }
    };
    crate::zend_sort::usort(&mut packages, |a, b| a.name.cmp(&b.name));
    let license_text = |p: &Package| {
        if p.license.is_empty() {
            "none".to_string()
        } else {
            p.license.join(", ")
        }
    };
    match args.format.as_str() {
        "text" => {
            io.write(&format!("Name: {}", root.package.pretty_name));
            io.write(&format!("Version: {}", root.package.full_pretty_version()));
            io.write(&format!("Licenses: {}", license_text(&root.package)));
            io.write("Dependencies:");
            io.write("");
            let rows: Vec<Vec<String>> = packages
                .iter()
                .map(|p| {
                    vec![
                        p.pretty_name.clone(),
                        p.full_pretty_version(),
                        license_text(p),
                    ]
                })
                .collect();
            for line in table::compact(Some(&["Name", "Version", "Licenses"]), &rows) {
                io.write(&line);
            }
        }
        "json" => {
            let mut dependencies = Map::new();
            for p in &packages {
                let mut entry = Map::new();
                entry.insert("version".into(), Value::String(p.full_pretty_version()));
                entry.insert(
                    "license".into(),
                    Value::Array(p.license.iter().cloned().map(Value::String).collect()),
                );
                dependencies.insert(p.pretty_name.clone(), Value::Object(entry));
            }
            let mut out = Map::new();
            out.insert(
                "name".into(),
                Value::String(root.package.pretty_name.clone()),
            );
            out.insert(
                "version".into(),
                Value::String(root.package.full_pretty_version()),
            );
            out.insert(
                "license".into(),
                Value::Array(
                    root.package
                        .license
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
            out.insert("dependencies".into(), Value::Object(dependencies));
            io.write(&encode(&Value::Object(out), true, true, true));
        }
        "summary" => {
            let mut used: Vec<(String, usize)> = Vec::new();
            for p in &packages {
                let mut licenses = p.license.clone();
                if licenses.is_empty() {
                    licenses.push("none".into());
                }
                for name in licenses {
                    match used.iter_mut().find(|(n, _)| *n == name) {
                        Some(entry) => entry.1 += 1,
                        None => used.push((name, 1)),
                    }
                }
            }
            crate::zend_sort::usort(&mut used, |a, b| b.1.cmp(&a.1));
            let rows: Vec<Vec<String>> = used
                .iter()
                .map(|(n, c)| vec![n.clone(), c.to_string()])
                .collect();
            for line in table::symfony_style(&["License", "Number of dependencies"], &rows) {
                io.write(&line);
            }
        }
        other => {
            return Err(format!(
                "Unsupported format \"{other}\".  See help for supported formats."
            ));
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- fund

#[derive(clap::Args, Debug)]
pub struct FundArgs {
    /// Format of the output: text or json
    #[arg(short = 'f', long, default_value = "text")]
    pub format: String,
}

pub fn fund(args: &FundArgs, io: &mut Io) -> i32 {
    let r = fund_inner(args, io);
    fail(io, r)
}

fn insert_funding(fundings: &mut BTreeMap<String, Vec<(String, Vec<String>)>>, package: &Package) {
    let (vendor, name) = package
        .pretty_name
        .split_once('/')
        .unwrap_or((&package.pretty_name, ""));
    let sponsors = regex::Regex::new(r"^https://github.com/([^/]+)$").unwrap();
    for option in &package.funding {
        let Some(url) = option
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
        else {
            continue;
        };
        let mut url = url.to_string();
        if option.get("type").and_then(Value::as_str) == Some("github")
            && let Some(m) = sponsors.captures(&url)
        {
            url = format!("https://github.com/sponsors/{}", &m[1]);
        }
        let entries = fundings.entry(vendor.to_string()).or_default();
        match entries.iter_mut().find(|(u, _)| *u == url) {
            Some((_, names)) => names.push(name.to_string()),
            None => entries.push((url, vec![name.to_string()])),
        }
    }
}

fn fund_inner(args: &FundArgs, io: &mut Io) -> Result<i32, String> {
    let (_, _, _, config, _) = load_project(io)?;
    let repo = local_repo(&config)?;
    let http = std::rc::Rc::new(crate::download::Http::new(&config)?);
    let mut repos = crate::repository::from_config(&config, http)?;
    let mut to_load: Vec<String> = repo
        .canonical_packages()
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let mut fundings: BTreeMap<String, Vec<(String, Vec<String>)>> = BTreeMap::new();
    // funding from the default branches, loaded in parallel
    let names: Vec<(String, Option<crate::semver::Constraint>)> =
        to_load.iter().map(|n| (n.clone(), None)).collect();
    for r in repos.iter_mut() {
        let (_, packages) = r.load_packages_filtered(
            io,
            &names,
            &[Stability::Dev],
            &BTreeMap::new(),
            &HashMap::new(),
        )?;
        for package in packages {
            if package.alias_of.is_none()
                && package.default_branch
                && !package.funding.is_empty()
                && to_load.contains(&package.name)
            {
                insert_funding(&mut fundings, &package);
                to_load.retain(|n| *n != package.name);
            }
        }
    }
    for package in repo.canonical_packages() {
        if to_load.contains(&package.name) && !package.funding.is_empty() {
            insert_funding(&mut fundings, package);
        }
    }
    if args.format != "text" && args.format != "json" {
        io.error(&format!(
            "Unsupported format \"{}\". See help for supported formats.",
            args.format
        ));
        return Ok(1);
    }
    if !fundings.is_empty() && args.format == "text" {
        let mut prev: Option<String> = None;
        io.write("The following packages were found in your dependencies which publish funding information:");
        for (vendor, links) in &fundings {
            io.write("");
            io.write(vendor);
            for (url, packages) in links {
                let line = format!("  {}", packages.join(", "));
                if prev.as_deref() != Some(&line) {
                    io.write(&line);
                    prev = Some(line);
                }
                io.write(&format!("    {url}"));
            }
        }
        io.write("");
        io.write(
            "Please consider following these links and sponsoring the work of package authors!",
        );
        io.write("Thank you!");
    } else if args.format == "json" {
        let mut out = Map::new();
        for (vendor, links) in &fundings {
            let mut by_url = Map::new();
            for (url, packages) in links {
                by_url.insert(
                    url.clone(),
                    Value::Array(packages.iter().cloned().map(Value::String).collect()),
                );
            }
            out.insert(vendor.clone(), Value::Object(by_url));
        }
        io.write(&encode(&Value::Object(out), true, true, true));
    } else {
        io.write("No funding links were found in your package dependencies. This doesn't mean they don't need your support!");
    }
    Ok(0)
}

// ---------------------------------------------------------------- suggests

#[derive(clap::Args, Debug)]
pub struct SuggestsArgs {
    /// Packages that you want to list suggestions from.
    pub packages: Vec<String>,
    /// Groups output by suggesting package
    #[arg(long)]
    pub by_package: bool,
    /// Groups output by suggested package
    #[arg(long)]
    pub by_suggestion: bool,
    /// Show suggestions from all dependencies, including transitive ones
    #[arg(long)]
    pub all: bool,
    /// Show only list of suggested package names
    #[arg(long)]
    pub list: bool,
    /// Exclude suggestions from require-dev packages
    #[arg(long)]
    pub no_dev: bool,
}

pub fn suggests(args: &SuggestsArgs, io: &mut Io) -> i32 {
    let r = suggests_inner(args, io);
    fail(io, r)
}

fn suggests_inner(args: &SuggestsArgs, io: &mut Io) -> Result<i32, String> {
    let (_, _, root, config, lock) = load_project(io)?;
    let php = PhpInfo::detect();
    let mut installed: Vec<Package> = vec![root.package.clone()];
    let mut validation: Option<Vec<Package>> = None;
    match lock.as_ref().filter(|l| l.is_locked()) {
        Some(lock) => {
            installed.extend(
                PlatformRepository::new(&lock.platform_overrides(), php.as_ref())?.packages,
            );
            installed.extend(lock.locked_packages(!args.no_dev)?);
            if args.no_dev {
                validation = Some(lock.locked_packages(true)?);
            }
        }
        None => {
            installed.extend(
                PlatformRepository::new(&overrides_from_config(&config), php.as_ref())?.packages,
            );
            installed.extend(local_repo(&config)?.packages);
        }
    }
    let filter: Vec<String> = args.packages.iter().map(|p| p.to_lowercase()).collect();
    let mut missing = Vec::new();
    let mut excluded = Vec::new();
    for (i, name) in filter.iter().enumerate() {
        if installed.iter().any(|p| p.name == *name) {
            continue;
        }
        if validation
            .as_ref()
            .is_some_and(|v| v.iter().any(|p| p.name == *name))
        {
            excluded.push(args.packages[i].clone());
        } else {
            missing.push(args.packages[i].clone());
        }
    }
    let mut errors = Vec::new();
    if !missing.is_empty() {
        errors.push(if missing.len() == 1 {
            format!("Package \"{}\" is not installed.", missing[0])
        } else {
            format!("Packages \"{}\" are not installed.", missing.join("\", \""))
        });
    }
    if !excluded.is_empty() {
        errors.push(if excluded.len() == 1 {
            format!("Package \"{}\" is excluded by --no-dev.", excluded[0])
        } else {
            format!(
                "Packages \"{}\" are excluded by --no-dev.",
                excluded.join("\", \"")
            )
        });
    }
    if !errors.is_empty() {
        for e in errors {
            io.error(&e);
        }
        return Ok(1);
    }
    // (source, target, reason)
    let mut suggestions: Vec<(String, String, String)> = Vec::new();
    let mut sources: Vec<&Package> = installed.iter().collect();
    sources.push(&root.package);
    for package in sources {
        if !filter.is_empty() && !filter.contains(&package.name) {
            continue;
        }
        for (target, reason) in &package.suggests {
            suggestions.push((
                package.pretty_name.clone(),
                target.clone(),
                reason.as_str().unwrap_or("").to_string(),
            ));
        }
    }
    let mut installed_names: HashMap<String, Vec<String>> = HashMap::new();
    for p in &installed {
        for name in crate::solver::names(p, true) {
            installed_names
                .entry(name)
                .or_default()
                .push(p.name.clone());
        }
    }
    let only_dependents_of = (filter.is_empty() && !args.all).then_some(&root.package);
    let filtered = |only: Option<&Package>| -> Vec<(String, String, String)> {
        let source_filter: Vec<String> = match only {
            Some(root) => {
                let mut f: Vec<String> = root
                    .requires
                    .iter()
                    .chain(root.dev_requires.iter())
                    .map(|l| l.target.clone())
                    .collect();
                f.push(root.name.clone());
                f
            }
            None => vec![],
        };
        suggestions
            .iter()
            .filter(|(source, target, _)| {
                let provided_by_others = installed_names
                    .get(target)
                    .is_some_and(|providers| providers.iter().any(|p| *p != source.to_lowercase()));
                !provided_by_others
                    && (source_filter.is_empty()
                        || source_filter.contains(&source.to_lowercase())
                        || source_filter.contains(source))
            })
            .cloned()
            .collect()
    };
    let shown = filtered(only_dependents_of);
    let mut by_package: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut by_suggestion: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (source, target, reason) in &shown {
        let entry = by_package.entry(source.clone()).or_default();
        match entry.iter_mut().find(|(t, _)| t == target) {
            Some(e) => e.1 = reason.clone(),
            None => entry.push((target.clone(), reason.clone())),
        }
        let entry = by_suggestion.entry(target.clone()).or_default();
        match entry.iter_mut().find(|(s, _)| s == source) {
            Some(e) => e.1 = reason.clone(),
            None => entry.push((source.clone(), reason.clone())),
        }
    }
    let mode_list = args.list;
    let mode_by_suggestion = !mode_list && args.by_suggestion;
    let mode_by_package = !mode_list && (!args.by_suggestion || args.by_package);
    if mode_list {
        for name in by_suggestion.keys() {
            io.write(name);
        }
        return Ok(0);
    }
    if mode_by_package {
        for (source, targets) in &by_package {
            io.write(&format!("{source} suggests:"));
            for (target, reason) in targets {
                io.write(&if reason.is_empty() {
                    format!(" - {target}")
                } else {
                    format!(" - {target}: {}", Io::clean(reason))
                });
            }
            io.write("");
        }
    }
    if mode_by_suggestion {
        if mode_by_package {
            io.write(&"-".repeat(78));
        }
        for (target, srcs) in &by_suggestion {
            io.write(&format!("{target} is suggested by:"));
            for (source, reason) in srcs {
                io.write(&if reason.is_empty() {
                    format!(" - {source}")
                } else {
                    format!(" - {source}: {}", Io::clean(reason))
                });
            }
            io.write("");
        }
    }
    if only_dependents_of.is_some() {
        let diff = filtered(None).len() - shown.len();
        if diff > 0 {
            io.write(&format!(
                "{diff} additional suggestions by transitive dependencies can be shown with --all"
            ));
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- status

#[derive(clap::Args, Debug)]
pub struct StatusArgs {}

pub fn status(_args: &StatusArgs, io: &mut Io) -> i32 {
    let r = status_inner(io);
    fail(io, r)
}

fn status_inner(io: &mut Io) -> Result<i32, String> {
    let (_, _, _, config, _) = load_project(io)?;
    let repo = local_repo(&config)?;
    let _ = repo;
    io.error("No local changes");
    Ok(0)
}

// ---------------------------------------------------------------- outdated

#[derive(clap::Args, Debug)]
pub struct OutdatedArgs {
    /// Package to inspect. Or a name including a wildcard (*) to filter lists of packages instead.
    pub package: Option<String>,
    /// Show all installed packages with their latest versions
    #[arg(short = 'a', long)]
    pub all: bool,
    /// Shows updates for packages from the lock file, regardless of what is currently in vendor dir
    #[arg(long)]
    pub locked: bool,
    /// Shows only packages that are directly required by the root package
    #[arg(short = 'D', long)]
    pub direct: bool,
    /// Return a non-zero exit code when there are outdated packages
    #[arg(long)]
    pub strict: bool,
    /// Show only packages that have major SemVer-compatible updates.
    #[arg(short = 'M', long)]
    pub major_only: bool,
    /// Show only packages that have minor SemVer-compatible updates.
    #[arg(short = 'm', long)]
    pub minor_only: bool,
    /// Show only packages that have patch SemVer-compatible updates.
    #[arg(short = 'p', long)]
    pub patch_only: bool,
    /// Displays the installed version's age, and sorts packages oldest first.
    #[arg(short = 'A', long)]
    pub sort_by_age: bool,
    /// Format of the output: text or json
    #[arg(short = 'f', long, default_value = "text")]
    pub format: String,
    /// Ignore specified package(s). Can contain wildcards (*).
    #[arg(long)]
    pub ignore: Vec<String>,
    /// Disables search in require-dev packages.
    #[arg(long)]
    pub no_dev: bool,
    /// Ignore a specific platform requirement (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_req: Vec<String>,
    /// Ignore all platform requirements (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_reqs: bool,
}

/// `OutdatedCommand`: `show --latest` with `--outdated` unless `--all`.
pub fn outdated(args: &OutdatedArgs, io: &mut Io) -> i32 {
    let mut show = crate::commands::show::Args::for_outdated();
    show.package = args.package.clone();
    show.latest = true;
    show.outdated = !args.all;
    show.direct = args.direct;
    show.strict = args.strict;
    show.major_only = args.major_only;
    show.minor_only = args.minor_only;
    show.patch_only = args.patch_only;
    show.locked = args.locked;
    show.no_dev = args.no_dev;
    show.sort_by_age = args.sort_by_age;
    show.ignore_platform_req = args.ignore_platform_req.clone();
    show.ignore_platform_reqs = args.ignore_platform_reqs;
    show.format = args.format.clone();
    show.ignore = args.ignore.clone();
    crate::commands::show::run(&show, io)
}
