//! `composer create-project` (`CreateProjectCommand`): the root package is extracted
//! from its dist archive as plain files the user owns (never linked from the store),
//! then its dependencies are installed like `install`, from the store.
//!
//! ponytail: the root package is always installed from dist, so `--prefer-source`,
//! `--keep-vcs` and `--remove-vcs` have no VCS metadata to act on; `--ask` never asks.

use crate::commands::config::{ConfigSource, Edit};
use crate::commands::dump_autoload::load_project;
use crate::commands::update::run_installer;
use crate::config::Config;
use crate::installer::Options;
use crate::io::Io;
use crate::json::Value;
use crate::package::Package;
use crate::platform::{PhpInfo, PlatformRepository, RequirementFilter, overrides_from_config};
use crate::semver::{Stability, parse_stability};
use crate::solver::builder::{Repository, RepositorySet};
use crate::version_selector::{VersionSelector, parse_name_version_pairs};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Package name to be installed
    pub package: Option<String>,
    /// Directory where the files should be created
    pub directory: Option<String>,
    /// Version, will default to latest
    #[arg(id = "package-version", value_name = "VERSION")]
    pub version: Option<String>,
    /// Minimum-stability allowed (unless a version is specified).
    #[arg(short = 's', long)]
    pub stability: Option<String>,
    /// Forces installation from package sources when possible, including VCS information.
    #[arg(long)]
    pub prefer_source: bool,
    /// Forces installation from package dist (default behavior).
    #[arg(long)]
    pub prefer_dist: bool,
    /// Forces installation from package dist|source|auto
    #[arg(long)]
    pub prefer_install: Option<String>,
    /// Add custom repositories to look the package up, either by URL or using JSON arrays
    #[arg(long)]
    pub repository: Vec<String>,
    /// DEPRECATED: Use --repository instead.
    #[arg(long)]
    pub repository_url: Option<String>,
    /// Add the custom repository in the composer.json. If a lock file is present it will be deleted and an update will be run instead of install.
    #[arg(long)]
    pub add_repository: bool,
    /// Require additional package(s) to composer.json after installing the project.
    #[arg(long)]
    pub require: Vec<String>,
    /// Enables installation of require-dev packages (enabled by default, only present for BC).
    #[arg(long)]
    pub dev: bool,
    /// Disables installation of require-dev packages.
    #[arg(long)]
    pub no_dev: bool,
    /// DEPRECATED: Use no-plugins instead.
    #[arg(long)]
    pub no_custom_installers: bool,
    /// Do not output download progress.
    #[arg(long)]
    pub no_progress: bool,
    /// Disable the secure-http config option temporarily while installing the root package.
    #[arg(long)]
    pub no_secure_http: bool,
    /// Whether to prevent deleting the vcs folder.
    #[arg(long)]
    pub keep_vcs: bool,
    /// Whether to force deletion of the vcs folder without prompting.
    #[arg(long)]
    pub remove_vcs: bool,
    /// Whether to skip installation of the package dependencies.
    #[arg(long)]
    pub no_install: bool,
    /// Whether to skip auditing of the installed package dependencies.
    #[arg(long)]
    pub no_audit: bool,
    /// Audit output format. Must be "table", "plain", "json" or "summary".
    #[arg(long, default_value = "summary")]
    pub audit_format: String,
    /// Ignore a specific platform requirement (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_req: Vec<String>,
    /// Ignore all platform requirements (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_reqs: bool,
    /// Whether to ask for project directory.
    #[arg(long)]
    pub ask: bool,
}

pub fn run(args: &Args, io: &mut Io) -> i32 {
    let old_cwd = std::env::current_dir().ok();
    let code = match execute(args, io) {
        Ok(code) => code,
        Err(message) => crate::scripts::exit_code(io, &message),
    };
    if let Some(cwd) = old_cwd {
        let _ = std::env::set_current_dir(cwd);
    }
    code
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    if args.dev {
        io.warning("You are using the deprecated option \"dev\". Dev packages are installed by default now.");
    }
    let no_plugins = crate::scripts::NO_PLUGINS.load(std::sync::atomic::Ordering::Relaxed);
    if args.no_custom_installers {
        io.warning("You are using the deprecated option \"no-custom-installers\". Use \"no-plugins\" instead.");
    }
    let repositories: Option<Vec<String>> = if !args.repository.is_empty() {
        Some(args.repository.clone())
    } else {
        args.repository_url.clone().map(|r| vec![r])
    };
    let filter =
        RequirementFilter::from_options(args.ignore_platform_reqs, &args.ignore_platform_req);
    let dev_mode = !args.no_dev;

    if let Some(package) = &args.package {
        install_root_package(
            args,
            io,
            package,
            repositories.as_deref(),
            &filter,
            no_plugins || args.no_custom_installers,
        )?;
    }

    let mut flush_lock = false;
    if let Some(repos) = repositories.as_deref().filter(|_| args.add_repository) {
        let source = ConfigSource::new("composer.json", false);
        let scheme = regex::Regex::new(r"(?i)^https?://").unwrap();
        for (index, repo) in repos.iter().enumerate() {
            let repo_config = crate::commands::init::repository_config(repo)?;
            let disables_packagist = repo_config.as_object().is_some_and(|o| {
                o.len() == 1
                    && ["packagist", "packagist.org"]
                        .iter()
                        .any(|k| o.get(*k) == Some(&Value::Bool(false)))
            });
            if disables_packagist {
                source.edit(Edit::AddRepository("packagist.org", None, true))?;
            } else {
                // `RepositoryFactory::generateRepositoryName`
                let (_, _, _, config, _) = load_project(io)?;
                let mut name = match repo_config.get("url").and_then(Value::as_str) {
                    Some(url) => scheme.replace(url, "").into_owned(),
                    None => index.to_string(),
                };
                while config.repositories.iter().any(|(n, _)| *n == name) {
                    name.push('2');
                }
                source.edit(Edit::AddRepository(&name, Some(&repo_config), false))?;
            }
            flush_lock = true;
        }
    }
    if !args.require.is_empty() {
        let source = ConfigSource::new("composer.json", false);
        for requirement in &args.require {
            let mut parts = requirement.split(':');
            let name = parts.next().unwrap_or("");
            let constraint = parts.next().unwrap_or("*");
            source.edit(Edit::AddLink("require", name, constraint))?;
        }
        flush_lock = true;
    }
    if flush_lock && Path::new("composer.lock").is_file() {
        let _ = std::fs::remove_file("composer.lock");
    }

    let (file, contents, root, config, lock) = load_project(io)?;
    let dev = if dev_mode { "1" } else { "0" };
    crate::scripts::put_env("COMPOSER_DEV_MODE", Some(dev))?;
    crate::scripts::fire(io, &config, &root, "post-root-package-install")?;

    if !args.no_install {
        let mut options = Options::defaults();
        options.update = !lock.as_ref().is_some_and(|l| l.is_locked());
        options.dev_mode = dev_mode;
        options.prefer_source =
            args.prefer_source || args.prefer_install.as_deref() == Some("source");
        options.prefer_dist = args.prefer_dist || args.prefer_install.as_deref() == Some("dist");
        options.optimize_autoloader = config.bool("optimize-autoloader");
        options.classmap_authoritative = config.bool("classmap-authoritative");
        options.apcu_autoloader = config.bool("apcu-autoloader");
        options.ignore_all_platform_reqs = args.ignore_platform_reqs;
        options.ignored_platform_reqs = args.ignore_platform_req.clone();
        options.write_lock = config.bool("lock");
        options.audit = crate::commands::audit::enabled(args.no_audit);
        options.audit_format = args.audit_format.clone();
        let (status, _) = run_installer(io, &file, &contents, &root, &config, lock, options)?;
        if status != 0 {
            return Ok(status);
        }
    }

    // the root package has no VCS metadata: pin `self.version` links to its version
    let source = ConfigSource::new("composer.json", false);
    for kind in crate::package::LinkType::ALL {
        let links = match kind {
            crate::package::LinkType::Require => &root.package.requires,
            crate::package::LinkType::DevRequire => &root.package.dev_requires,
            crate::package::LinkType::Conflict => &root.package.conflicts,
            crate::package::LinkType::Provide => &root.package.provides,
            crate::package::LinkType::Replace => &root.package.replaces,
        };
        for link in links
            .iter()
            .filter(|l| l.pretty_constraint == "self.version")
        {
            source.edit(Edit::AddLink(
                kind.key(),
                &link.target,
                &root.package.pretty_version,
            ))?;
        }
    }

    crate::scripts::fire(io, &config, &root, "post-create-project-cmd")?;
    Ok(0)
}

/// `getFullPrettyVersion(false)`: dev versions show their full reference.
fn full_version(package: &Package) -> String {
    let short = package.full_pretty_version();
    if short == package.pretty_version {
        return short;
    }
    let reference = package
        .source
        .as_ref()
        .and_then(|s| s.reference.clone())
        .or_else(|| package.dist.as_ref().and_then(|d| d.reference.clone()))
        .unwrap_or_default();
    format!("{} {reference}", package.pretty_version)
}

/// `installRootPackage`: picks the version, extracts it into the target directory and
/// moves there.
fn install_root_package(
    args: &Args,
    io: &mut Io,
    package_name: &str,
    repositories: Option<&[String]>,
    filter: &RequirementFilter,
    no_plugins: bool,
) -> Result<(), String> {
    let cwd = crate::fs::cwd();
    let (name, parsed_version) = parse_name_version_pairs(&[package_name.to_string()])
        .into_iter()
        .next()
        .ok_or("Not enough arguments (missing: \"package\").")?;
    let name = name.to_lowercase();
    let version = args
        .version
        .clone()
        .filter(|v| !v.is_empty())
        .or(parsed_version);

    let directory = args
        .directory
        .clone()
        .unwrap_or_else(|| format!("{cwd}/{}", name.rsplit('/').next().unwrap_or(&name)));
    let mut directory = directory.trim_end_matches(['/', '\\']).to_string();
    if !crate::fs::is_absolute_path(&directory) {
        directory = format!("{cwd}/{directory}");
    }
    let dir = PathBuf::from(&directory);

    let mut config = Config::create(&cwd)?;
    config.base_dir = Some(directory.clone());
    if args.no_secure_http {
        config
            .config
            .insert("secure-http".into(), Value::Bool(false));
    }
    io.error(&format!(
        "Creating a \"{package_name}\" project at \"{}\"",
        crate::fs::find_shortest_path(&cwd, &directory, true, false)
    ));
    if dir.exists() {
        if !dir.is_dir() {
            return Err(format!(
                "Cannot create project directory at \"{directory}\", it exists as a file."
            ));
        }
        if !crate::fs::is_dir_empty(&dir) {
            return Err(format!("Project directory \"{directory}\" is not empty."));
        }
    }

    let stability = match (&args.stability, &version) {
        (Some(s), _) => s.clone(),
        (None, None) => "stable".into(),
        (None, Some(v)) => match regex::Regex::new(r"(?i)^[^,\s]*?@(stable|RC|beta|alpha|dev)$")
            .unwrap()
            .captures(v)
        {
            Some(m) => m[1].to_string(),
            None => parse_stability(v).as_str().to_string(),
        },
    };
    let stability = Stability::parse(&stability).map_err(|_| {
        format!(
            "Invalid stability provided ({}), must be one of: stable, RC, beta, alpha, dev",
            if stability.eq_ignore_ascii_case("rc") {
                "RC".into()
            } else {
                stability.to_lowercase()
            }
        )
    })?;

    if let Some(repos) = repositories {
        config.repositories.clear();
        for (i, repo) in repos.iter().enumerate() {
            let repo = crate::commands::init::repository_config(repo)?;
            let disables = repo.as_object().is_some_and(|o| {
                o.len() == 1
                    && ["packagist", "packagist.org"]
                        .iter()
                        .any(|k| o.get(*k) == Some(&Value::Bool(false)))
            });
            if !disables {
                config.repositories.push((i.to_string(), repo));
            }
        }
    }
    let http = std::rc::Rc::new(crate::download::Http::new(&config)?);
    let mut composer_repos = crate::repository::from_config(&config, http)?;
    let php = PhpInfo::detect();
    let platform = PlatformRepository::new(&overrides_from_config(&config), php.as_ref())?;
    let mut set = RepositorySet::new(
        stability,
        BTreeMap::new(),
        &[],
        BTreeMap::new(),
        vec![],
        vec![],
    );
    for repo in composer_repos.iter_mut() {
        set.add(Repository::Composer(repo));
    }
    let mut selector = VersionSelector::new(&mut set, Some(&platform));
    let found =
        selector.find_best_candidate(io, &name, version.as_deref(), stability, filter, 0, true)?;
    let Some((_, package)) = found else {
        let message = format!(
            "Could not find package {name} with {}",
            match &version {
                Some(v) => format!("version {v}"),
                None => format!("stability {}", stability.as_str()),
            }
        );
        if !matches!(filter, RequirementFilter::IgnoreAll)
            && selector
                .find_best_candidate(
                    io,
                    &name,
                    version.as_deref(),
                    stability,
                    &RequirementFilter::IgnoreAll,
                    0,
                    false,
                )?
                .is_some()
        {
            return Err(format!(
                "{message} in a version installable using your PHP version, PHP extensions and Composer version."
            ));
        }
        return Err(format!("{message}."));
    };
    crate::fs::ensure_dir(&dir)?;
    io.error(&format!(
        "Installing {} ({})",
        package.name,
        full_version(&package)
    ));
    if no_plugins {
        io.error("Plugins have been disabled.");
    }
    let mut package = package;
    while let Some(real) = package.alias_of.take() {
        package = *real;
    }
    // ponytail: the root package is extracted, never linked: its files are the user's to edit
    let mut downloader = crate::download::Downloader::new(&config)?;
    let store = crate::store::config::StoreConfig::from_root(
        &Default::default(),
        &config.get_str("cache-dir"),
    )?;
    downloader.set_write_cache(!store.enabled);
    let downloaded = downloader.download_all(io, &[&package])?;
    let archive = &downloaded[&package.unique_name()].archive;
    io.error(&format!(
        "  - Installing {} ({}): Extracting archive",
        package.name,
        package.full_pretty_version()
    ));
    downloader.install_archive(&package, archive, &dir)?;
    downloader.cleanup(&package, &dir);
    io.error(&format!("Created project in {directory}"));
    std::env::set_current_dir(&dir).map_err(|e| e.to_string())?;
    if dir.join("composer.json").exists() && std::env::var_os("COMPOSER").is_some() {
        // SAFETY: single-threaded here, before the installer spawns threads
        unsafe { std::env::remove_var("COMPOSER") };
    }
    crate::scripts::put_env("COMPOSER_ROOT_VERSION", Some(&package.pretty_version))?;
    Ok(())
}
