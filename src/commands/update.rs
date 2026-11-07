//! `composer update`: resolve, write the lock, then install.
//!
//! ponytail: `--interactive`, `--root-reqs`, `--patch-only`, `--bump-after-update`
//! and `--with` temporary constraints on wildcards are not implemented yet.

use crate::commands::dump_autoload::load_project;
use crate::config;
use crate::installer::{Installer, Options};
use crate::io::Io;
use crate::platform::PhpInfo;
use crate::semver::{intervals, version_parser};
use crate::solver;
use crate::solver::builder::Repository;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Packages that should be updated, if not provided all packages are.
    pub packages: Vec<String>,
    /// Temporary version constraint to add, e.g. foo/bar:1.0.0 or foo/bar=1.0.0
    #[arg(long = "with")]
    pub with: Vec<String>,
    /// Forces installation from package sources when possible, including VCS information.
    #[arg(long)]
    pub prefer_source: bool,
    /// Forces installation from package dist (default behavior).
    #[arg(long)]
    pub prefer_dist: bool,
    /// Forces installation from package dist|source|auto
    #[arg(long)]
    pub prefer_install: Option<String>,
    /// Outputs the operations but will not execute anything (implicitly enables --verbose).
    #[arg(long)]
    pub dry_run: bool,
    /// DEPRECATED: Enables installation of require-dev packages (enabled by default, only present for BC).
    #[arg(long)]
    pub dev: bool,
    /// Disables installation of require-dev packages.
    #[arg(long)]
    pub no_dev: bool,
    /// Overwrites the lock file hash to suppress warning about the lock file being out of date.
    #[arg(long)]
    pub lock: bool,
    /// Skip the install step after updating the composer.lock file.
    #[arg(long)]
    pub no_install: bool,
    /// Skip the audit step after updating the composer.lock file (can also be set via the COMPOSER_NO_AUDIT=1 env var).
    #[arg(long)]
    pub no_audit: bool,
    /// Audit output format. Must be "table", "plain", "json", or "summary".
    #[arg(long, default_value = "summary")]
    pub audit_format: String,
    /// Skips the execution of all scripts defined in composer.json file.
    #[arg(long)]
    pub no_scripts: bool,
    /// Skips autoloader generation
    #[arg(long)]
    pub no_autoloader: bool,
    /// Do not output download progress.
    #[arg(long)]
    pub no_progress: bool,
    /// Add also dependencies of allowed packages to the allowed packages, except those defined in root package.
    #[arg(short = 'w', long)]
    pub with_dependencies: bool,
    /// Add also all dependencies of allowed packages to the allowed packages.
    #[arg(short = 'W', long)]
    pub with_all_dependencies: bool,
    /// Optimize autoloader during autoloader dump
    #[arg(short = 'o', long)]
    pub optimize_autoloader: bool,
    /// Autoload classes from the classmap only. Implicitly enables `--optimize-autoloader`.
    #[arg(short = 'a', long)]
    pub classmap_authoritative: bool,
    /// Return a failed status code (6) if PSR-4 or PSR-0 mapping errors are present.
    #[arg(long)]
    pub strict_psr_autoloader: bool,
    /// Use APCu to cache found/not-found classes.
    #[arg(long)]
    pub apcu_autoloader: bool,
    /// Use a custom prefix for the APCu autoloader cache. Implicitly enables --apcu-autoloader
    #[arg(long)]
    pub apcu_autoloader_prefix: Option<String>,
    /// Ignore all platform requirements (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_reqs: bool,
    /// Ignore a specific platform requirement (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_req: Vec<String>,
    /// Prefer stable versions of dependencies (can also be set via the COMPOSER_PREFER_STABLE=1 env var).
    #[arg(long)]
    pub prefer_stable: bool,
    /// Prefer lowest versions of dependencies (can also be set via the COMPOSER_PREFER_LOWEST=1 env var).
    #[arg(long)]
    pub prefer_lowest: bool,
    /// During an update with -w/-W, only perform absolutely necessary changes to transitive dependencies.
    #[arg(short = 'm', long)]
    pub minimal_changes: bool,
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

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    if args.dev {
        io.warning("You are using the deprecated option \"--dev\". It has no effect and will break in Composer 3.");
    }
    let (file, contents, root, config, lock) = load_project(io)?;

    // temporary constraints from --with and foo/bar:constraint arguments
    let mut packages = args.packages.clone();
    let mut reqs: Vec<(String, String)> = Vec::new();
    let with_req = regex::Regex::new(r"^([^ =:]+)[ =:](.+)$").unwrap();
    for w in &args.with {
        let Some(m) = with_req.captures(w) else {
            return Err(format!(
                "Invalid requirement \"{w}\", expected package:constraint"
            ));
        };
        reqs.push((m[1].to_lowercase(), m[2].trim().to_string()));
    }
    for p in packages.iter_mut() {
        if let Some(m) = with_req.captures(&p.clone())
            && p.contains([' ', '=', ':'])
        {
            reqs.push((m[1].to_lowercase(), m[2].trim().to_string()));
            *p = m[1].to_string();
        }
    }
    let mut temporary_constraints = Vec::new();
    let root_requirements: Vec<&crate::package::Link> = root
        .package
        .requires
        .iter()
        .chain(root.package.dev_requires.iter())
        .collect();
    for (name, constraint) in &reqs {
        let parsed = version_parser::parse_constraints(constraint)?;
        if name.contains('*') {
            let regex = crate::repository::package_name_to_regexp(name);
            for link in &root_requirements {
                if !regex.is_match(&link.target) {
                    continue;
                }
                temporary_constraints.push((
                    link.target.clone(),
                    parsed.clone(),
                    constraint.clone(),
                ));
                if !intervals::have_intersections(&parsed, &link.constraint) {
                    io.error(&format!(
                        "The temporary constraint \"{constraint}\" for \"{name}\" matching \"{}\" must be a subset of the constraint in your composer.json ({})",
                        link.target, link.pretty_constraint
                    ));
                    return Ok(1);
                }
            }
        } else {
            temporary_constraints.push((name.clone(), parsed.clone(), constraint.clone()));
            if let Some(link) = root_requirements.iter().find(|l| l.target == *name)
                && !intervals::have_intersections(&parsed, &link.constraint)
            {
                io.error(&format!(
                    "The temporary constraint \"{constraint}\" for \"{name}\" must be a subset of the constraint in your composer.json ({})",
                    link.pretty_constraint
                ));
                io.write(&format!(
                    "Run `composer require {name}` or `composer require {name}:{constraint}` instead to replace the constraint"
                ));
                return Ok(1);
            }
        }
    }

    let filtered: Vec<String> = packages
        .iter()
        .filter(|p| !matches!(p.as_str(), "lock" | "nothing" | "mirrors"))
        .cloned()
        .collect();
    let update_mirrors = args.lock || filtered.len() != packages.len();
    if update_mirrors && !filtered.is_empty() {
        io.error("You cannot simultaneously update only a selection of packages and regenerate the lock file metadata.");
        return Ok(1);
    }
    let mut allow_list: Vec<String> = Vec::new();
    for p in filtered.iter().map(|p| p.to_lowercase()) {
        if !allow_list.contains(&p) {
            allow_list.push(p);
        }
    }

    let mut options = Options::defaults();
    options.update = true;
    options.install = !args.no_install;
    options.dev_mode = !args.no_dev;
    options.dry_run = args.dry_run;
    options.prefer_source = args.prefer_source || args.prefer_install.as_deref() == Some("source");
    options.prefer_dist = args.prefer_dist || args.prefer_install.as_deref() == Some("dist");
    options.dump_autoloader = !args.no_autoloader;
    options.optimize_autoloader = args.optimize_autoloader
        || args.classmap_authoritative
        || config.bool("optimize-autoloader")
        || config.bool("classmap-authoritative");
    options.classmap_authoritative =
        args.classmap_authoritative || config.bool("classmap-authoritative");
    options.apcu_autoloader = args.apcu_autoloader
        || args.apcu_autoloader_prefix.is_some()
        || config.bool("apcu-autoloader");
    options.apcu_autoloader_prefix = args.apcu_autoloader_prefix.clone();
    options.run_scripts = !args.no_scripts;
    options.strict_psr_autoloader =
        args.strict_psr_autoloader || config.bool("strict-psr-autoloader");
    if options.strict_psr_autoloader
        && !options.optimize_autoloader
        && !options.classmap_authoritative
    {
        return Err("--strict-psr-autoloader mode only works with optimized autoloader, use --optimize-autoloader or --classmap-authoritative if you want a strict return value.".into());
    }
    options.ignore_all_platform_reqs = args.ignore_platform_reqs;
    options.ignored_platform_reqs = args.ignore_platform_req.clone();
    options.update_mirrors = update_mirrors;
    options.update_allow_list = (!allow_list.is_empty()).then_some(allow_list);
    options.update_allow_transitive = if args.with_all_dependencies {
        solver::UPDATE_LISTED_WITH_TRANSITIVE_DEPS
    } else if args.with_dependencies {
        solver::UPDATE_LISTED_WITH_TRANSITIVE_DEPS_NO_ROOT_REQUIRE
    } else {
        solver::UPDATE_ONLY_LISTED
    };
    let env_flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1" || v == "true");
    options.prefer_stable = args.prefer_stable || env_flag("COMPOSER_PREFER_STABLE");
    options.prefer_lowest = args.prefer_lowest || env_flag("COMPOSER_PREFER_LOWEST");
    options.minimal_update = args.minimal_changes || config.bool("update-with-minimal-changes");
    options.temporary_constraints = temporary_constraints;
    options.write_lock = config.bool("lock");
    options.audit = crate::commands::audit::enabled(args.no_audit);
    options.audit_format = args.audit_format.clone();
    run_installer(io, &file, &contents, &root, &config, lock, options).map(|(status, _)| status)
}

/// Builds the installer on the project's repositories and runs it.
pub fn run_installer(
    io: &mut Io,
    file: &std::path::Path,
    contents: &str,
    root: &crate::package::root::RootPackage,
    config: &crate::config::Config,
    lock: Option<crate::lock::Lock>,
    options: Options,
) -> Result<(i32, Option<crate::lock::Lock>), String> {
    let mut executor = crate::installer::FilesystemExecutor;
    let http = std::rc::Rc::new(crate::download::Http::new(config)?);
    let mut composer_repos = crate::repository::from_config(config, http)?;
    let repos: Vec<Repository> = composer_repos
        .iter_mut()
        .map(Repository::Composer)
        .collect();
    let mut installer = Installer {
        io,
        config,
        root,
        lock,
        lock_path: Some(config::lock_file(file)),
        composer_json: contents,
        options,
        executor: &mut executor,
        repos,
        php: PhpInfo::detect(),
        written_lock: None,
        suggesting_packages: vec![],
    };
    let status = installer.run()?;
    Ok((status, installer.lock.take()))
}
