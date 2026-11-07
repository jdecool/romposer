//! `composer show`: installed, locked, platform and available package listings and
//! the single-package view, text and JSON.
//!
//! ponytail: `--latest`/`--outdated`/`--tree` (ticket 12) are not accepted yet.

use crate::commands::dump_autoload::{load_project, try_load_project};
use crate::config::Config;
use crate::download::Http;
use crate::installed::InstalledRepository;
use crate::installer::install_path;
use crate::io::Io;
use crate::json::{Map, Value, encode};
use crate::package::root::RootPackage;
use crate::package::{LinkType, Package, is_platform_package};
use crate::platform::{PhpInfo, PlatformRepository, overrides_from_config};
use crate::repository::{self, ComposerRepository, package_name_to_regexp};
use crate::semver::{Constraint, version_compare, version_parser};
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Package to inspect. Or a name including a wildcard (*) to filter lists of packages instead.
    package: Option<String>,
    /// Version or version constraint to inspect
    #[arg(value_name = "VERSION", id = "package_version")]
    version: Option<String>,
    /// List all packages
    #[arg(long)]
    all: bool,
    /// List all locked packages
    #[arg(long)]
    locked: bool,
    /// List installed packages only (enabled by default, only present for BC).
    #[arg(short = 'i', long)]
    installed: bool,
    /// List platform packages only
    #[arg(short = 'p', long)]
    platform: bool,
    /// List available packages only
    #[arg(short = 'a', long)]
    available: bool,
    /// Show the root package information
    #[arg(short = 's', long = "self")]
    self_: bool,
    /// List package names only
    #[arg(short = 'N', long)]
    name_only: bool,
    /// Show package paths
    #[arg(short = 'P', long)]
    path: bool,
    /// Format of the output: text or json
    #[arg(short = 'f', long, default_value = "text")]
    format: String,
    /// Disables search in require-dev packages.
    #[arg(long)]
    no_dev: bool,
    /// Shows only packages that are directly required by the root package
    #[arg(short = 'D', long)]
    direct: bool,
}

pub fn run(args: &Args, io: &mut Io) -> i32 {
    match execute(args, io) {
        Ok(code) => code,
        Err(message) => {
            io.error(&message);
            1
        }
    }
}

/// One repository as the listing sees it, tagged with its output section.
enum Source {
    Packages(Vec<Package>),
    Remote(usize),
}

enum Entry {
    Name(String),
    Pkg(Box<Package>),
}

/// (type, rows, name length, version length)
type Section = (&'static str, Vec<Map<String, Value>>, usize, usize);
/// (matched package, pretty version => normalized version)
type Found = (Option<Package>, Vec<(String, String)>);

struct Context {
    root: Option<RootPackage>,
    vendor_dir: String,
    repos: Vec<ComposerRepository>,
    /// what `$installedRepo` holds, for `hasPackage` and install paths
    installed: Vec<Package>,
    sources: Vec<(&'static str, Source)>,
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    if args.installed && !args.self_ {
        io.warning("You are using the deprecated option \"installed\". Only installed packages are shown by default now. The --all option can be used to show all packages.");
    }
    if args.direct && (args.all || args.available || args.platform) {
        io.error("The --direct (-D) option is not usable in combination with --all, --platform (-p) or --available (-a)");
        return Ok(1);
    }
    if args.format != "text" && args.format != "json" {
        io.error(&format!(
            "Unsupported format \"{}\". See help for supported formats.",
            args.format
        ));
        return Ok(1);
    }

    let project = try_load_project(io)?;
    let (root, config, lock) = match project {
        Some((_, _, root, config, lock)) => (Some(root), config, lock),
        None => (None, Config::create(&crate::fs::cwd())?, None),
    };
    let php = PhpInfo::detect();
    let platform = PlatformRepository::new(&overrides_from_config(&config), php.as_ref())?;
    let vendor_dir = config.vendor_dir();
    let local =
        InstalledRepository::read(Path::new(&format!("{vendor_dir}/composer/installed.json")))?;
    let http = Rc::new(Http::new(&config)?);
    let repos = repository::from_config(&config, http)?;
    let remote: Vec<(&'static str, Source)> = (0..repos.len())
        .map(|i| ("available", Source::Remote(i)))
        .collect();
    let no_composer_notice = |io: &mut Io, repos: &[ComposerRepository]| {
        let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
        io.error(&format!(
            "No composer.json found in the current directory, showing available packages from {}",
            names.join(", ")
        ));
    };

    let mut ctx = Context {
        root: None,
        vendor_dir,
        repos,
        installed: vec![],
        sources: vec![],
    };
    let mut self_package: Option<Package> = None;
    let mut locked_shown = false;

    if args.self_ && !args.installed && !args.locked {
        let root = root.ok_or_else(no_composer_error)?;
        if args.name_only {
            io.write(&root.package.name);
            return Ok(0);
        }
        if args.package.is_some() {
            return Err("You cannot use --self together with a package name".into());
        }
        self_package = Some(root.package.clone());
        ctx.installed = vec![root.package.clone()];
        ctx.sources = vec![("installed", Source::Packages(vec![root.package.clone()]))];
        ctx.root = Some(root);
    } else if args.platform {
        ctx.installed = platform.packages.clone();
        ctx.sources = vec![("platform", Source::Packages(platform_listing(&platform)))];
        ctx.root = root;
    } else if args.available {
        ctx.installed = platform.packages.clone();
        if root.is_some() {
            ctx.installed.extend(local.packages.iter().cloned());
        } else {
            no_composer_notice(io, &ctx.repos);
        }
        ctx.sources = remote;
        ctx.root = root;
    } else if args.all && root.is_some() {
        let mut sources = Vec::new();
        if let Some(lock) = &lock {
            let locked = lock.locked_packages(true)?;
            ctx.installed.extend(locked.iter().cloned());
            sources.push(("locked", Source::Packages(locked)));
            locked_shown = true;
        }
        ctx.installed.extend(local.packages.iter().cloned());
        ctx.installed.extend(platform.packages.iter().cloned());
        sources.push(("installed", Source::Packages(local.packages.clone())));
        sources.push(("platform", Source::Packages(platform_listing(&platform))));
        sources.extend(remote);
        ctx.sources = sources;
        ctx.root = root;
    } else if args.all {
        no_composer_notice(io, &ctx.repos);
        ctx.installed = platform.packages.clone();
        ctx.sources = vec![("platform", Source::Packages(platform_listing(&platform)))];
        ctx.sources.extend(remote);
    } else if args.locked {
        let (Some(root), Some(lock)) = (root, lock) else {
            return Err("A valid composer.json and composer.lock files is required to run this command with --locked".into());
        };
        let mut locked = lock.locked_packages(!args.no_dev)?;
        if args.self_ {
            locked.push(root.package.clone());
        }
        ctx.installed = locked.clone();
        ctx.sources = vec![("locked", Source::Packages(locked))];
        ctx.root = Some(root);
        locked_shown = true;
    } else {
        let root = match root {
            Some(root) => root,
            None => load_project(io)?.2,
        };
        let mut packages = Vec::new();
        if args.self_ {
            packages.push(root.package.clone());
        }
        if args.no_dev {
            packages.extend(filter_required_packages(
                &local.packages,
                &root.package,
                &mut vec![],
            ));
        } else {
            packages.extend(local.packages.iter().cloned());
        }
        if packages.is_empty() {
            let has_non_platform = |links: &[crate::package::Link]| {
                links.iter().any(|l| !is_platform_package(&l.target))
            };
            if has_non_platform(&root.package.requires)
                || has_non_platform(&root.package.dev_requires)
            {
                io.warning("No dependencies installed. Try running composer install or update.");
            }
        }
        ctx.installed = packages.clone();
        ctx.sources = vec![("installed", Source::Packages(packages))];
        ctx.root = Some(root);
    }
    let _ = locked_shown;

    // single package
    let filter = args.package.as_deref();
    let (package, versions) = if let Some(p) = self_package {
        let versions = vec![(p.pretty_version.clone(), p.version.clone())];
        (Some(p), versions)
    } else if let Some(name) = filter.filter(|f| !f.contains('*')) {
        let (package, versions) = get_package(&mut ctx, io, name, args.version.as_deref())?;
        if let Some(package) = &package
            && args.direct
            && !root_requires(&ctx.root).contains(&package.name)
        {
            return Err(format!(
                "Package \"{}\" is installed but not a direct dependent of the root package.",
                package.name
            ));
        }
        if package.is_none() {
            let mut hint = String::new();
            if args.locked {
                hint.push_str(" in lock file");
            }
            if is_platform_package(name) && !args.platform {
                hint.push_str(", try using --platform (-p) to show platform packages");
            }
            if !args.all && !args.available {
                hint.push_str(", try using --available (-a) to show all available packages");
            }
            return Err(format!("Package \"{name}\" not found{hint}."));
        }
        (package, versions)
    } else {
        (None, vec![])
    };
    if let Some(package) = package {
        if args.path {
            let path =
                install_path(&ctx.vendor_dir, &package).map(|p| crate::fs::realpath(Path::new(&p)));
            io.write(&format!(
                "{} {}",
                package.name,
                path.as_deref().map_or("null".to_string(), first_line)
            ));
            return Ok(0);
        }
        if args.format == "json" {
            print_package_json(&ctx, io, &package, &versions);
        } else {
            print_package_text(&ctx, io, &package, &versions);
        }
        return Ok(0);
    }

    // listing
    let filter_regex = filter.map(package_name_to_regexp);
    let list_filter = args.direct.then(|| root_requires(&ctx.root));
    let path_option = args.path && ctx.root.is_some();
    if args.path && ctx.root.is_none() {
        io.error("No composer.json found in the current directory, disabling \"path\" option");
    }
    let mut packages: BTreeMap<&'static str, BTreeMap<String, Entry>> = BTreeMap::new();
    let sources = std::mem::take(&mut ctx.sources);
    for (kind, source) in &sources {
        match source {
            Source::Remote(i) => {
                for name in ctx.repos[*i].package_names(io, filter)? {
                    packages
                        .entry(kind)
                        .or_default()
                        .insert(name.clone(), Entry::Name(name));
                }
            }
            Source::Packages(list) => {
                for package in list {
                    let newer = match packages.get(kind).and_then(|s| s.get(&package.name)) {
                        Some(Entry::Pkg(existing)) => {
                            version_compare(&existing.version, &package.version)
                                == std::cmp::Ordering::Less
                        }
                        _ => true,
                    };
                    if !newer {
                        continue;
                    }
                    let mut package = package;
                    while let Some(real) = &package.alias_of {
                        package = real;
                    }
                    if filter_regex
                        .as_ref()
                        .is_none_or(|r| r.is_match(&package.name))
                        && list_filter
                            .as_ref()
                            .is_none_or(|l| l.contains(&package.name))
                    {
                        packages
                            .entry(kind)
                            .or_default()
                            .insert(package.name.clone(), Entry::Pkg(Box::new(package.clone())));
                    }
                }
            }
        }
    }

    let indent = if args.all { "  " } else { "" };
    let mut view: Vec<Section> = Vec::new();
    let requires = root_requires(&ctx.root);
    for (kind, show_version) in [
        ("platform", true),
        ("locked", true),
        ("available", false),
        ("installed", true),
    ] {
        let Some(section) = packages.get(kind) else {
            continue;
        };
        let write_path = !args.name_only && args.path && path_option;
        let write_version = !args.name_only && !args.path && show_version;
        let write_description = !args.name_only && !args.path;
        let mut name_length = 0;
        let mut version_length = 0;
        let mut rows = Vec::new();
        for entry in section.values() {
            let mut row = Map::new();
            match entry {
                Entry::Pkg(package) => {
                    row.insert("name".into(), Value::String(package.pretty_name.clone()));
                    row.insert(
                        "direct-dependency".into(),
                        Value::Bool(requires.contains(&package.name)),
                    );
                    if args.format != "json" || !args.name_only {
                        row.insert(
                            "homepage".into(),
                            package.homepage.clone().map_or(Value::Null, Value::String),
                        );
                        row.insert(
                            "source".into(),
                            view_source_url(package).map_or(Value::Null, Value::String),
                        );
                    }
                    name_length = name_length.max(package.pretty_name.len());
                    if write_version {
                        let mut version = package.full_pretty_version();
                        if args.format == "text" {
                            version = version.trim_start_matches('v').to_string();
                        }
                        version_length = version_length.max(version.len());
                        row.insert("version".into(), Value::String(version));
                    }
                    if write_description {
                        row.insert(
                            "description".into(),
                            package
                                .description
                                .clone()
                                .map_or(Value::Null, Value::String),
                        );
                    }
                    if write_path {
                        let path = install_path(&ctx.vendor_dir, package)
                            .map(|p| first_line(&crate::fs::realpath(Path::new(&p))));
                        row.insert("path".into(), path.map_or(Value::Null, Value::String));
                    }
                    row.insert("abandoned".into(), Value::Bool(false));
                }
                Entry::Name(name) => {
                    row.insert("name".into(), Value::String(name.clone()));
                    name_length = name_length.max(name.len());
                }
            }
            rows.push(row);
        }
        view.push((kind, rows, name_length, version_length));
    }

    if args.format == "json" {
        let mut out = Map::new();
        for (kind, rows, _, _) in view {
            out.insert(
                kind.to_string(),
                Value::Array(rows.into_iter().map(Value::Object).collect()),
            );
        }
        io.write(&encode(&Value::Object(out), true, true, true));
        return Ok(0);
    }

    let width = Io::terminal_width();
    // Composer's `$writeVersion`/`$writeDescription` are whatever the last prepared type left behind
    let last_shows_version = view.last().is_some_and(|(kind, ..)| *kind != "available");
    let write_version = !args.name_only && !args.path && last_shows_version;
    let write_description = !args.name_only && !args.path;
    for (kind, rows, name_length, version_length) in view {
        let version_fits = name_length + version_length + 3 <= width;
        let description_fits = name_length + version_length + 24 <= width;
        if args.all {
            io.write(&format!("{kind}:"));
        }
        print_packages(
            io,
            &rows,
            indent,
            write_version && version_fits,
            write_description && description_fits,
            width,
            version_length,
            name_length,
        );
        if args.all {
            io.write("");
        }
    }
    Ok(0)
}

fn no_composer_error() -> String {
    format!(
        "Composer could not find a composer.json file in {}\nTo initialize a project, please create a composer.json file. See https://getcomposer.org/basic-usage",
        crate::fs::cwd()
    )
}

/// `strtok($s, "\r\n")`
fn first_line(s: &str) -> String {
    s.split(['\r', '\n'])
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `PlatformRepository::getPackages` plus the disabled ones (`$packages[$type][$name] = $package`).
fn platform_listing(platform: &PlatformRepository) -> Vec<Package> {
    let mut list = platform.packages.clone();
    list.extend(platform.disabled.values().cloned());
    list
}

fn root_requires(root: &Option<RootPackage>) -> Vec<String> {
    let Some(root) = root else { return vec![] };
    root.package
        .requires
        .iter()
        .chain(root.package.dev_requires.iter())
        .map(|l| l.target.to_lowercase())
        .collect()
}

/// `RepositoryUtils::filterRequiredPackages`
fn filter_required_packages(
    packages: &[Package],
    requirer: &Package,
    bucket: &mut Vec<Package>,
) -> Vec<Package> {
    let requires: Vec<&str> = requirer
        .requires
        .iter()
        .map(|l| l.target.as_str())
        .collect();
    for candidate in packages {
        for name in names(candidate) {
            if requires.contains(&name.as_str()) {
                if !bucket.iter().any(|b| b == candidate) {
                    bucket.push(candidate.clone());
                    filter_required_packages(packages, candidate, bucket);
                }
                break;
            }
        }
    }
    bucket.clone()
}

/// `BasePackage::getNames`
fn names(package: &Package) -> Vec<String> {
    let mut out = vec![package.name.clone()];
    for link in package.provides.iter().chain(package.replaces.iter()) {
        if !out.contains(&link.target) {
            out.push(link.target.clone());
        }
    }
    out
}

/// `PackageInfo::getViewSourceUrl`
fn view_source_url(package: &Package) -> Option<String> {
    if let Some(Value::String(s)) = package.support.get("source")
        && !s.is_empty()
    {
        return Some(s.clone());
    }
    package.source.as_ref().map(|s| s.url.clone())
}

fn installed_has(ctx: &Context, package: &Package) -> bool {
    ctx.installed
        .iter()
        .any(|p| p.name == package.name && p.version == package.version)
}

/// `ShowCommand::getPackage`: the installed match, else the policy's preferred version.
fn get_package(
    ctx: &mut Context,
    io: &mut Io,
    name: &str,
    version: Option<&str>,
) -> Result<Found, String> {
    let name = name.to_lowercase();
    let constraint = match version {
        Some(v) => Some(version_parser::parse_constraints(v)?),
        None => None,
    };
    let mut candidates: Vec<Package> = Vec::new();
    let sources = std::mem::take(&mut ctx.sources);
    for (_, source) in &sources {
        match source {
            Source::Packages(list) => candidates.extend(
                list.iter()
                    .filter(|p| p.name == name)
                    .filter(|p| {
                        constraint
                            .as_ref()
                            .is_none_or(|c| c.matches(&Constraint::new("==", &p.version)))
                    })
                    .cloned(),
            ),
            Source::Remote(i) => candidates
                .extend(ctx.repos[*i].load_packages(io, &[(name.clone(), constraint.clone())])?),
        }
    }
    ctx.sources = sources;
    let mut matched: Option<Package> = None;
    let mut versions: Vec<(String, String)> = Vec::new();
    let mut literals: Vec<Package> = Vec::new();
    for mut package in candidates {
        if package.alias_of.is_some() && package.version == "9999999-dev" {
            package = *package.alias_of.take().unwrap();
        }
        if version.is_none() && installed_has(ctx, &package) {
            matched = Some(package.clone());
        }
        match versions
            .iter_mut()
            .find(|(p, _)| *p == package.pretty_version)
        {
            Some(entry) => entry.1 = package.version.clone(),
            None => versions.push((package.pretty_version.clone(), package.version.clone())),
        }
        literals.push(package);
    }
    if matched.is_none() && !literals.is_empty() {
        // DefaultPolicy::selectPreferredPackages: aliases first, then the best version
        literals.sort_by_key(|p| p.alias_of.is_none());
        let mut best = &literals[0];
        for p in &literals[1..] {
            if Constraint::new(">", &best.version)
                .match_specific(&Constraint::new("==", &p.version), true)
            {
                best = p;
            }
        }
        matched = Some(best.clone());
    }
    Ok((matched, versions))
}

fn license_json(id: &str) -> Value {
    match crate::spdx::license_by_identifier(id) {
        Some(license) => {
            let mut m = Map::new();
            m.insert("name".into(), Value::String(license.name.clone()));
            m.insert("osi".into(), Value::String(id.to_string()));
            m.insert(
                "url".into(),
                Value::String(format!("https://spdx.org/licenses/{}.html#licenseText", id)),
            );
            Value::Object(m)
        }
        None => Value::String(id.to_string()),
    }
}

fn links_map(package: &Package, kind: LinkType) -> Option<Value> {
    let links = package.links(kind);
    if links.is_empty() {
        return None;
    }
    let mut m = Map::new();
    for link in links {
        m.insert(
            link.target.clone(),
            Value::String(link.pretty_constraint.clone()),
        );
    }
    Some(Value::Object(m))
}

/// `appendAutoload`: psr-0/psr-4 with `*` for the empty prefix and `.` for empty paths, classmap as is.
fn autoload_json(package: &Package) -> Option<Value> {
    if package.autoload.is_empty() {
        return None;
    }
    let mut out = Map::new();
    for (kind, entries) in &package.autoload {
        if kind == "psr-0" || kind == "psr-4" {
            let mut psr = Map::new();
            if let Value::Object(entries) = entries {
                for (prefix, path) in entries {
                    let path = match path {
                        Value::String(s) if s.is_empty() => Value::String(".".into()),
                        Value::Null | Value::Bool(false) => Value::String(".".into()),
                        Value::Array(a) if a.is_empty() => Value::String(".".into()),
                        other => other.clone(),
                    };
                    let key = if prefix.is_empty() { "*" } else { prefix };
                    psr.insert(key.to_string(), path);
                }
            }
            out.insert(kind.clone(), Value::Object(psr));
        } else if kind == "classmap" {
            out.insert(kind.clone(), entries.clone());
        }
    }
    Some(Value::Object(out))
}

fn print_package_json(
    ctx: &Context,
    io: &mut Io,
    package: &Package,
    versions: &[(String, String)],
) {
    let mut json = Map::new();
    json.insert("name".into(), Value::String(package.pretty_name.clone()));
    json.insert(
        "description".into(),
        package
            .description
            .clone()
            .map_or(Value::Null, Value::String),
    );
    json.insert(
        "keywords".into(),
        Value::Array(
            package
                .keywords
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    json.insert("type".into(), Value::String(package.kind.clone()));
    json.insert(
        "homepage".into(),
        package.homepage.clone().map_or(Value::Null, Value::String),
    );
    json.insert(
        "names".into(),
        Value::Array(names(package).into_iter().map(Value::String).collect()),
    );
    let mut sorted: Vec<&(String, String)> = versions.iter().collect();
    sorted.sort_by(|a, b| version_compare(&a.1, &b.1));
    sorted.reverse();
    json.insert(
        "versions".into(),
        Value::Array(
            sorted
                .iter()
                .map(|(p, _)| Value::String(p.clone()))
                .collect(),
        ),
    );
    if !package.license.is_empty() {
        json.insert(
            "licenses".into(),
            Value::Array(package.license.iter().map(|l| license_json(l)).collect()),
        );
    }
    if let Some(source) = &package.source {
        let mut m = Map::new();
        m.insert("type".into(), Value::String(source.kind.clone()));
        m.insert("url".into(), Value::String(source.url.clone()));
        m.insert(
            "reference".into(),
            source.reference.clone().map_or(Value::Null, Value::String),
        );
        json.insert("source".into(), Value::Object(m));
    }
    if let Some(dist) = &package.dist {
        let mut m = Map::new();
        m.insert("type".into(), Value::String(dist.kind.clone()));
        m.insert("url".into(), Value::String(dist.url.clone()));
        m.insert(
            "reference".into(),
            dist.reference.clone().map_or(Value::Null, Value::String),
        );
        json.insert("dist".into(), Value::Object(m));
    }
    if !is_platform_package(&package.name) && installed_has(ctx, package) {
        match install_path(&ctx.vendor_dir, package) {
            Some(path) => {
                let real = crate::fs::realpath(Path::new(&path));
                if Path::new(&real).exists() {
                    json.insert("path".into(), Value::String(real));
                }
            }
            None => {
                json.insert("path".into(), Value::Null);
            }
        }
        if let Some(date) = &package.release_date {
            json.insert(
                "released".into(),
                Value::String(crate::package::dumper::rfc3339(date)),
            );
        }
    }
    if package.is_abandoned() {
        json.insert(
            "replacement".into(),
            package
                .replacement_package()
                .map_or(Value::Null, Value::String),
        );
    }
    if !package.suggests.is_empty() {
        json.insert("suggests".into(), Value::Object(package.suggests.clone()));
    }
    if !package.support.is_empty() {
        json.insert("support".into(), Value::Object(package.support.clone()));
    }
    if let Some(autoload) = autoload_json(package) {
        json.insert("autoload".into(), autoload);
    }
    if !package.include_paths.is_empty() {
        json.insert(
            "include_path".into(),
            Value::Array(
                package
                    .include_paths
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    for (kind, key) in [
        (LinkType::Require, "requires"),
        (LinkType::DevRequire, "devRequires"),
        (LinkType::Provide, "provides"),
        (LinkType::Conflict, "conflicts"),
        (LinkType::Replace, "replaces"),
    ] {
        if let Some(links) = links_map(package, kind) {
            json.insert(key.into(), links);
        }
    }
    io.write(&encode(&Value::Object(json), true, true, true));
}

/// `printPackageInfo` without ANSI styles.
fn print_package_text(
    ctx: &Context,
    io: &mut Io,
    package: &Package,
    versions: &[(String, String)],
) {
    let is_installed = !is_platform_package(&package.name) && installed_has(ctx, package);
    io.write(&format!("name     : {}", package.pretty_name));
    io.write(&format!(
        "descrip. : {}",
        package.description.as_deref().unwrap_or("")
    ));
    io.write(&format!("keywords : {}", package.keywords.join(", ")));
    // Semver::rsort: usort with a comparator that never orders two branches
    let mut sorted: Vec<&(String, String)> = versions.iter().collect();
    let default_branch = |v: &str| -> String {
        if matches!(v, "dev-master" | "dev-default" | "dev-trunk") {
            "9999999-dev".into()
        } else {
            v.to_string()
        }
    };
    let less_than = |a: &str, b: &str| {
        !(a.starts_with("dev-") && b.starts_with("dev-"))
            && version_compare(a, b) == std::cmp::Ordering::Less
    };
    crate::zend_sort::usort(&mut sorted, |a, b| {
        let (a, b) = (default_branch(&a.1), default_branch(&b.1));
        if a == b {
            std::cmp::Ordering::Equal
        } else if less_than(&a, &b) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Less
        }
    });
    let installed_versions: Vec<&str> = ctx
        .installed
        .iter()
        .filter(|p| p.name == package.name)
        .map(|p| p.pretty_version.as_str())
        .collect();
    let shown: Vec<String> = sorted
        .iter()
        .map(|(p, _)| {
            if installed_versions.contains(&p.as_str()) {
                format!("* {p}")
            } else {
                p.clone()
            }
        })
        .collect();
    io.write(&format!("versions : {}", shown.join(", ")));
    if is_installed
        && let Some(date) = &package.release_date
        && let Some(day) = crate::package::dumper::rfc3339(date).get(..10)
    {
        io.write(&format!("released : {day}, {}", relative_time(date)));
    }
    io.write(&format!("type     : {}", package.kind));
    for id in &package.license {
        let line = match crate::spdx::license_by_identifier(id) {
            Some(l) if l.osi_approved => format!(
                "{} ({id}) (OSI approved) https://spdx.org/licenses/{id}.html#licenseText",
                l.name
            ),
            Some(l) => format!(
                "{} ({id}) https://spdx.org/licenses/{id}.html#licenseText",
                l.name
            ),
            None => id.clone(),
        };
        io.write(&format!("license  : {line}"));
    }
    io.write(&format!(
        "homepage : {}",
        package.homepage.as_deref().unwrap_or("")
    ));
    let (st, su, sr) = package.source.as_ref().map_or(("", "", ""), |s| {
        (
            s.kind.as_str(),
            s.url.as_str(),
            s.reference.as_deref().unwrap_or(""),
        )
    });
    io.write(&format!("source   : [{st}] {su} {sr}"));
    let (dt, du, dr) = package.dist.as_ref().map_or(("", "", ""), |d| {
        (
            d.kind.as_str(),
            d.url.as_str(),
            d.reference.as_deref().unwrap_or(""),
        )
    });
    io.write(&format!("dist     : [{dt}] {du} {dr}"));
    if is_installed {
        // `realpath()` of a missing directory is `false`, printed as an empty string
        let path = install_path(&ctx.vendor_dir, package).map(|p| {
            let real = crate::fs::realpath(Path::new(&p));
            if Path::new(&real).exists() {
                real
            } else {
                String::new()
            }
        });
        io.write(&format!(
            "path     : {}",
            path.unwrap_or_else(|| "null".into())
        ));
    }
    io.write(&format!("names    : {}", names(package).join(", ")));
    if package.is_abandoned() {
        let replacement = package
            .replacement_package()
            .map(|r| format!(" The author suggests using the {r} package instead."))
            .unwrap_or_default();
        io.warning(&format!(
            "Attention: This package is abandoned and no longer maintained.{replacement}"
        ));
    }
    if !package.support.is_empty() {
        io.write("\nsupport");
        for (kind, value) in &package.support {
            io.write(&format!("{kind} : {}", value.as_str().unwrap_or("")));
        }
    }
    if !package.autoload.is_empty() {
        io.write("\nautoload");
        for (kind, entries) in &package.autoload {
            io.write(kind);
            if kind == "psr-0" || kind == "psr-4" {
                if let Value::Object(entries) = entries {
                    for (prefix, path) in entries {
                        let path = match path {
                            Value::Array(a) => a
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", "),
                            Value::String(s) if !s.is_empty() => s.clone(),
                            _ => ".".into(),
                        };
                        let prefix = if prefix.is_empty() { "*" } else { prefix };
                        io.write(&format!("{prefix} => {path}"));
                    }
                }
            } else if kind == "classmap"
                && let Value::Array(list) = entries
            {
                let items: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
                io.write(&items.join(", "));
            }
        }
        if !package.include_paths.is_empty() {
            io.write("include-path");
            io.write(&package.include_paths.join(", "));
        }
    }
    let print_links = |io: &mut Io, kind: LinkType, title: &str| {
        let links = package.links(kind);
        if !links.is_empty() {
            io.write(&format!("\n{title}"));
            for link in links {
                io.write(&format!("{} {}", link.target, link.pretty_constraint));
            }
        }
    };
    print_links(io, LinkType::Require, "requires");
    print_links(io, LinkType::DevRequire, "requires (dev)");
    if !package.suggests.is_empty() {
        io.write("\nsuggests");
        for (name, reason) in &package.suggests {
            io.write(&format!("{name} {}", reason.as_str().unwrap_or("")));
        }
    }
    print_links(io, LinkType::Provide, "provides");
    print_links(io, LinkType::Conflict, "conflicts");
    print_links(io, LinkType::Replace, "replaces");
}

/// `ShowCommand::getRelativeTime`
fn relative_time(release_date: &str) -> String {
    let iso = crate::package::dumper::rfc3339(release_date);
    let parse = |s: &str| -> Option<(i64, u32, u32)> {
        Some((
            s[..4].parse().ok()?,
            s[5..7].parse().ok()?,
            s[8..10].parse().ok()?,
        ))
    };
    let Some((y1, m1, d1)) = parse(&iso) else {
        return String::new();
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y2, m2, d2) = civil_from_days(now_secs.div_euclid(86400));
    if (y1, m1, d1) == (y2, m2, d2) {
        return "today".into();
    }
    let days = (days_from_civil(y2, m2, d2) - days_from_civil(y1, m1, d1)).abs();
    if days < 7 {
        return "this week".into();
    }
    if days < 14 {
        return "last week".into();
    }
    let mut months = (y2 - y1) * 12 + (m2 as i64 - m1 as i64);
    if d2 < d1 {
        months -= 1;
    }
    let years = months / 12;
    let months = months % 12;
    if months < 1 && days < 31 {
        return format!("{} weeks ago", days / 7);
    }
    if years < 1 {
        return format!("{months} month{} ago", if months > 1 { "s" } else { "" });
    }
    format!("{years} year{} ago", if years > 1 { "s" } else { "" })
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `printPackages` without `--latest` columns.
#[allow(clippy::too_many_arguments)]
fn print_packages(
    io: &mut Io,
    rows: &[Map<String, Value>],
    indent: &str,
    write_version: bool,
    write_description: bool,
    width: usize,
    version_length: usize,
    name_length: usize,
) {
    let pad_name = write_version || write_description;
    let pad_version = write_description;
    let str_of = |row: &Map<String, Value>, key: &str| -> Option<String> {
        row.get(key).and_then(Value::as_str).map(str::to_string)
    };
    for row in rows {
        let name = str_of(row, "name").unwrap_or_default();
        let mut line = format!("{indent}{name}");
        if pad_name {
            line.push_str(&" ".repeat(name_length.saturating_sub(name.len())));
        }
        if let Some(version) = str_of(row, "version")
            && write_version
        {
            line.push(' ');
            line.push_str(&version);
            if pad_version {
                line.push_str(&" ".repeat(version_length.saturating_sub(version.len())));
            }
        }
        if let Some(description) = str_of(row, "description")
            && write_description
        {
            let mut description = first_line(&description);
            let remaining = width as i64 - name_length as i64 - version_length as i64 - 4;
            if remaining <= 0 {
                description = String::new();
            } else if description.chars().count() as i64 > remaining {
                // mb_strimwidth: the trim marker counts towards the width
                let keep = (remaining - 3).max(0) as usize;
                description = format!("{}...", description.chars().take(keep).collect::<String>());
            }
            line.push(' ');
            line.push_str(&description);
        }
        if let Some(path) = row.get("path") {
            line.push(' ');
            line.push_str(path.as_str().unwrap_or("null"));
        }
        io.write(&Io::clean(&line));
    }
}
