//! `composer require`: add requirements to composer.json, then update them.
//!
//! ponytail: interactive prompts (package search, moving between require keys,
//! feature-branch confirmation) are not ported; romposer never asks.

use crate::commands::dump_autoload::load_project;
use crate::commands::update::run_installer;
use crate::config;
use crate::installer::{self, Options};
use crate::io::Io;
use crate::json::manipulator::Manipulator;
use crate::json::{Map, Value};
use crate::platform::{PhpInfo, PlatformRepository, RequirementFilter, overrides_from_config};
use crate::semver::{Stability, version_parser};
use crate::solver;
use crate::solver::builder::{Repository, RepositorySet};
use crate::version_selector::{
    VersionSelector, find_best_version_and_name, parse_name_version_pairs,
};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Optional package name can also include a version constraint, e.g. foo/bar or foo/bar:1.0.0 or foo/bar=1.0.0 or "foo/bar 1.0.0"
    pub packages: Vec<String>,
    /// Add requirement to require-dev.
    #[arg(long)]
    pub dev: bool,
    /// Outputs the operations but will not execute anything (implicitly enables --verbose).
    #[arg(long)]
    pub dry_run: bool,
    /// Forces installation from package sources when possible, including VCS information.
    #[arg(long)]
    pub prefer_source: bool,
    /// Forces installation from package dist (default behavior).
    #[arg(long)]
    pub prefer_dist: bool,
    /// Forces installation from package dist|source|auto
    #[arg(long)]
    pub prefer_install: Option<String>,
    /// Write fixed version to the composer.json.
    #[arg(long)]
    pub fixed: bool,
    /// Do not output download progress.
    #[arg(long)]
    pub no_progress: bool,
    /// Disables the automatic update of the dependencies (implies --no-install).
    #[arg(long)]
    pub no_update: bool,
    /// Skip the install step after updating the composer.lock file.
    #[arg(long)]
    pub no_install: bool,
    /// Run the dependency update with the --no-dev option.
    #[arg(long)]
    pub update_no_dev: bool,
    /// Allows inherited dependencies to be updated, except those that are root requirements.
    #[arg(short = 'w', long)]
    pub update_with_dependencies: bool,
    /// Allows all inherited dependencies to be updated, including those that are root requirements.
    #[arg(short = 'W', long)]
    pub update_with_all_dependencies: bool,
    /// Alias for --update-with-dependencies
    #[arg(long)]
    pub with_dependencies: bool,
    /// Alias for --update-with-all-dependencies
    #[arg(long)]
    pub with_all_dependencies: bool,
    /// Ignore a specific platform requirement (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_req: Vec<String>,
    /// Ignore all platform requirements (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_reqs: bool,
    /// Prefer stable versions of dependencies.
    #[arg(long)]
    pub prefer_stable: bool,
    /// Prefer lowest versions of dependencies.
    #[arg(long)]
    pub prefer_lowest: bool,
    /// During an update with -w/-W, only perform absolutely necessary changes to transitive dependencies.
    #[arg(short = 'm', long)]
    pub minimal_changes: bool,
    /// Sorts packages when adding/updating a new dependency
    #[arg(long)]
    pub sort_packages: bool,
    /// Optimize autoloader during autoloader dump
    #[arg(short = 'o', long)]
    pub optimize_autoloader: bool,
    /// Autoload classes from the classmap only. Implicitly enables `--optimize-autoloader`.
    #[arg(short = 'a', long)]
    pub classmap_authoritative: bool,
    /// Use APCu to cache found/not-found classes.
    #[arg(long)]
    pub apcu_autoloader: bool,
    /// Use a custom prefix for the APCu autoloader cache. Implicitly enables --apcu-autoloader
    #[arg(long)]
    pub apcu_autoloader_prefix: Option<String>,
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

struct Backup {
    file: std::path::PathBuf,
    lock: std::path::PathBuf,
    newly_created: bool,
    composer: String,
    lock_contents: Option<String>,
}

impl Backup {
    /// `revertComposerFile`
    fn revert(&self, io: &mut Io) {
        if self.newly_created {
            io.error(&format!(
                "\nInstallation failed, deleting {}.",
                self.file.display()
            ));
            let _ = std::fs::remove_file(&self.file);
            let _ = std::fs::remove_file(&self.lock);
        } else {
            let msg = if self.lock_contents.is_some() {
                format!(" and {} to their ", self.lock.display())
            } else {
                " to its ".to_string()
            };
            io.error(&format!(
                "\nInstallation failed, reverting {}{msg}original content.",
                self.file.display()
            ));
            let _ = std::fs::write(&self.file, &self.composer);
            if let Some(lock) = &self.lock_contents {
                let _ = std::fs::write(&self.lock, lock);
            }
        }
    }
}

/// `updateFile`: cleanly through the manipulator, else a full rewrite.
pub fn update_file(
    file: &Path,
    new: &[(String, String)],
    require_key: &str,
    remove_key: &str,
    sort_packages: bool,
) -> Result<(), String> {
    let contents = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let mut manipulator = Manipulator::new(&contents)?;
    let mut clean = true;
    for (package, constraint) in new {
        if !manipulator.add_link(require_key, package, constraint, sort_packages)
            || !manipulator.remove_link(remove_key, package)
        {
            clean = false;
            break;
        }
    }
    if clean {
        return std::fs::write(file, manipulator.contents()).map_err(|e| e.to_string());
    }
    let mut definition: Map<String, Value> =
        serde_json::from_str(&contents).map_err(|e| e.to_string())?;
    for (package, constraint) in new {
        let entry = definition
            .entry(require_key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(map) = entry {
            map.insert(package.clone(), Value::String(constraint.clone()));
        }
        if let Some(Value::Object(map)) = definition.get_mut(remove_key) {
            map.shift_remove(package);
            if map.is_empty() {
                definition.shift_remove(remove_key);
            }
        }
    }
    std::fs::write(
        file,
        crate::json::encode(&Value::Object(definition), true, true, true) + "\n",
    )
    .map_err(|e| e.to_string())
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    let file = config::composer_file()?;
    let newly_created = !file.exists();
    if newly_created {
        std::fs::write(&file, "{\n}\n")
            .map_err(|_| format!("{} could not be created.", file.display()))?;
    }
    if std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) == 0 {
        std::fs::write(&file, "{\n}\n").map_err(|e| e.to_string())?;
    }
    let lock_path = config::lock_file(&file);
    let backup = Backup {
        file: file.clone(),
        lock: lock_path.clone(),
        newly_created,
        composer: std::fs::read_to_string(&file).map_err(|e| e.to_string())?,
        lock_contents: std::fs::read_to_string(&lock_path).ok(),
    };

    if args.fixed {
        let definition: Value = serde_json::from_str(&backup.composer).unwrap_or(Value::Null);
        let kind = definition
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("library");
        if kind != "project" && !args.dev {
            io.error("The \"--fixed\" option is only allowed when the root package has a \"project\" type or for dev dependencies to prevent possible misuses.");
            if definition.get("type").is_none() {
                io.error("If your package is not a library, you can explicitly specify the \"type\" by using \"composer config type project\".");
            }
            return Ok(1);
        }
    }

    let (_, contents, root, config, _lock) = match load_project(io) {
        Ok(p) => p,
        Err(e) => {
            if newly_created {
                backup.revert(io);
                return Err(format!(
                    "No composer.json present in the current directory ({}), this may be the cause of the following exception.\n{e}",
                    file.display()
                ));
            }
            return Err(e);
        }
    };
    let php = PhpInfo::detect();
    let platform = PlatformRepository::new(&overrides_from_config(&config), php.as_ref())?;
    let filter =
        RequirementFilter::from_options(args.ignore_platform_reqs, &args.ignore_platform_req);
    let preferred_stability = if root.prefer_stable {
        Stability::Stable
    } else {
        root.minimum_stability
    };

    // determineRequirements
    let mut requirements: Vec<(String, String)> = Vec::new();
    let mut to_guess: Vec<String> = Vec::new();
    {
        if args.packages.iter().any(|p| p.to_lowercase() == "as") {
            return Err("Cannot use \"as\" as a separate argument. Quote the inline alias as one argument, e.g. \"vendor/package:dev-main as 1.2.x-dev\".".into());
        }
        let http = std::rc::Rc::new(crate::download::Http::new(&config)?);
        let mut composer_repos = crate::repository::from_config(&config, http)?;
        let mut set = RepositorySet::new(
            root.minimum_stability,
            BTreeMap::new(),
            &[],
            BTreeMap::new(),
            vec![],
            vec![],
        );
        set.add(Repository::Platform(&platform));
        for repo in composer_repos.iter_mut() {
            set.add(Repository::Composer(repo));
        }
        let installed_names: Vec<String> = crate::installed::InstalledRepository::read(Path::new(
            &format!("{}/composer/installed.json", config.vendor_dir()),
        ))
        .map(|r| r.packages.iter().map(|p| p.name.clone()).collect())
        .unwrap_or_default();
        let too_strict = regex::Regex::new(r"^\d+(\.\d+)?$").unwrap();
        for (name, version) in parse_name_version_pairs(&args.packages) {
            if let Some(v) = &version
                && too_strict.is_match(v)
            {
                io.warning(&format!(
                    "The \"{v}\" constraint for \"{name}\" appears too strict and will likely not match what you want. See https://getcomposer.org/constraints"
                ));
            }
            match version {
                Some(v) => requirements.push((name, v)),
                None => {
                    let (found, version) = match find_best_version_and_name(
                        io,
                        &mut set,
                        &platform,
                        &name,
                        preferred_stability,
                        root.minimum_stability,
                        &filter,
                        args.fixed,
                        php.as_ref().map(|p| p.version.as_str()),
                        &installed_names,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            if newly_created {
                                backup.revert(io);
                                return Err(format!(
                                    "No composer.json present in the current directory ({}), this may be the cause of the following exception.\n{e}",
                                    file.display()
                                ));
                            }
                            return Err(e);
                        }
                    };
                    if args.no_update {
                        io.error(&format!("Using version {version} for {found}"));
                        requirements.push((found, version));
                    } else {
                        to_guess.push(found.clone());
                        requirements.push((found, "*".into()));
                    }
                }
            }
        }
    }
    // formatRequirements: a map, later entries override earlier ones
    let mut map: Vec<(String, String)> = Vec::new();
    for (name, version) in requirements {
        match map.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = version,
            None => map.push((name, version)),
        }
    }
    let requirements = map;
    let (require_key, remove_key) = if args.dev {
        ("require-dev", "require")
    } else {
        ("require", "require-dev")
    };
    for (package, constraint) in &requirements {
        if package.to_lowercase() == root.package.name {
            io.error(&format!(
                "Root package '{package}' cannot require itself in its composer.json"
            ));
            return Ok(1);
        }
        if constraint != "self.version" {
            version_parser::parse_constraints(constraint)?;
        }
    }
    let definition: Map<String, Value> = serde_json::from_str(&contents).unwrap_or_default();
    let keys_of = |key: &str| -> Vec<String> {
        definition
            .get(key)
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    for (package, _) in &requirements {
        if keys_of(remove_key).contains(package) && !keys_of(require_key).contains(package) {
            io.warning(&format!(
                "{package} is currently present in the {remove_key} key and you ran the command {} the --dev flag, which will move it to the {require_key} key.",
                if args.dev { "with" } else { "without" }
            ));
        }
    }
    let sort_packages = args.sort_packages || config.bool("sort-packages");
    let first_require =
        newly_created || (keys_of("require").is_empty() && keys_of("require-dev").is_empty());

    if !args.dry_run {
        update_file(&file, &requirements, require_key, remove_key, sort_packages)?;
    }
    io.error(&format!(
        "{} has been {}",
        file.display(),
        if newly_created { "created" } else { "updated" }
    ));
    if args.no_update {
        return Ok(0);
    }

    // doUpdate on the edited file
    let (file, contents, mut root, config, lock) = load_project(io)?;
    if args.dry_run {
        let new_links = crate::package::parse_links(
            &root.package.name,
            &root.package.pretty_version,
            if args.dev {
                crate::package::LinkType::DevRequire
            } else {
                crate::package::LinkType::Require
            },
            &requirements
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )?;
        let (target, other) = if args.dev {
            (&mut root.package.dev_requires, &mut root.package.requires)
        } else {
            (&mut root.package.requires, &mut root.package.dev_requires)
        };
        for link in new_links {
            match target.iter_mut().find(|l| l.target == link.target) {
                Some(existing) => *existing = link,
                None => target.push(link),
            }
        }
        other.retain(|l| !requirements.iter().any(|(n, _)| *n == l.target));
        crate::package::root::extract_references(&requirements, &mut root.references);
        crate::package::root::extract_stability_flags(
            &requirements,
            root.minimum_stability,
            &mut root.stability_flags,
        );
    }
    let with_all = args.update_with_all_dependencies || args.with_all_dependencies;
    let with_deps = args.update_with_dependencies || args.with_dependencies;
    let (transitive, flags) = if with_all {
        (
            solver::UPDATE_LISTED_WITH_TRANSITIVE_DEPS,
            " --with-all-dependencies",
        )
    } else if with_deps {
        (
            solver::UPDATE_LISTED_WITH_TRANSITIVE_DEPS_NO_ROOT_REQUIRE,
            " --with-dependencies",
        )
    } else {
        (solver::UPDATE_ONLY_LISTED, "")
    };
    let names: Vec<String> = requirements.iter().map(|(n, _)| n.clone()).collect();
    io.error(&format!(
        "Running composer update {}{flags}",
        names.join(" ")
    ));
    let mut options = Options::defaults();
    options.update = true;
    options.install = !args.no_install;
    options.dry_run = args.dry_run;
    options.dev_mode = !args.update_no_dev;
    options.prefer_source = args.prefer_source || args.prefer_install.as_deref() == Some("source");
    options.prefer_dist = args.prefer_dist || args.prefer_install.as_deref() == Some("dist");
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
    options.ignore_all_platform_reqs = args.ignore_platform_reqs;
    options.ignored_platform_reqs = args.ignore_platform_req.clone();
    options.prefer_stable = args.prefer_stable;
    options.prefer_lowest = args.prefer_lowest;
    options.minimal_update = args.minimal_changes || config.bool("update-with-minimal-changes");
    options.update_allow_transitive = transitive;
    options.write_lock = config.bool("lock");
    if !first_require && lock.as_ref().is_some_and(|l| l.is_locked()) {
        options.update_allow_list = Some(names.iter().map(|n| n.to_lowercase()).collect());
    }
    let (status, resolved_lock) =
        run_installer(io, &file, &contents, &root, &config, lock, options)?;
    if status != 0 && status != installer::ERROR_AUDIT_FAILED {
        if status == installer::ERROR_DEPENDENCY_RESOLUTION_FAILED {
            for (name, version) in parse_name_version_pairs(&args.packages) {
                if version.is_none() {
                    io.error(&format!(
                        "You can also try re-running composer require with an explicit version constraint, e.g. \"composer require {name}:*\" to figure out if any version is installable, or \"composer require {name}:^2.1\" if you know which you need."
                    ));
                    break;
                }
            }
        }
        backup.revert(io);
        return Ok(status);
    }
    if !to_guess.is_empty() {
        return update_requirements_after_resolution(
            io,
            &file,
            &config,
            resolved_lock,
            &to_guess,
            require_key,
            remove_key,
            sort_packages,
            args.dry_run,
            args.fixed,
            php.as_ref(),
        );
    }
    Ok(status)
}

/// `updateRequirementsAfterResolution`: replace the `*` placeholders by the constraints
/// recommended for the versions the solver picked.
#[allow(clippy::too_many_arguments)]
fn update_requirements_after_resolution(
    io: &mut Io,
    file: &Path,
    config: &crate::config::Config,
    resolved_lock: Option<crate::lock::Lock>,
    to_guess: &[String],
    require_key: &str,
    remove_key: &str,
    sort_packages: bool,
    dry_run: bool,
    fixed: bool,
    php: Option<&PhpInfo>,
) -> Result<i32, String> {
    let lock_path = config::lock_file(file);
    // the installer's lock: in a dry run it was resolved but never written
    let lock = resolved_lock.filter(|l| l.is_locked());
    let packages: Vec<crate::package::Package> = match &lock {
        Some(lock) => lock.locked_packages(true)?,
        None => {
            crate::installed::InstalledRepository::read(Path::new(&format!(
                "{}/composer/installed.json",
                config.vendor_dir()
            )))?
            .packages
        }
    };
    let mut requirements: Vec<(String, String)> = Vec::new();
    for name in to_guess {
        let lower = name.to_lowercase();
        let Some(package) = packages.iter().find(|p| p.name == lower) else {
            continue;
        };
        let mut package = package.clone();
        while let Some(real) = package.alias_of.take() {
            package = *real;
        }
        let version = if fixed {
            package.pretty_version.clone()
        } else {
            VersionSelector::recommended_require_version(&package, php.map(|p| p.version.as_str()))
        };
        io.error(&format!("Using version {version} for {name}"));
        let feature_branch = version.starts_with("dev-")
            && !matches!(
                version.as_str(),
                "dev-main" | "dev-master" | "dev-trunk" | "dev-latest"
            );
        if feature_branch {
            io.warning(&format!(
                "Version {version} looks like it may be a feature branch which is unlikely to keep working in the long run and may be in an unstable state"
            ));
        }
        requirements.push((name.clone(), version));
    }
    if !dry_run {
        update_file(file, &requirements, require_key, remove_key, sort_packages)?;
        if let Some(lock) = lock
            && config.bool("lock")
        {
            // Locker::updateHash with the new stability flags, keeping the lock's mtime
            let contents = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
            let mut data = lock.data.clone();
            data.insert(
                "content-hash".into(),
                Value::String(crate::lock::content_hash(&contents)?),
            );
            let minimum = crate::semver::Stability::parse(
                data.get("minimum-stability")
                    .and_then(Value::as_str)
                    .unwrap_or("stable"),
            )
            .unwrap_or(Stability::Stable);
            let mut flags = BTreeMap::new();
            crate::package::root::extract_stability_flags(&requirements, minimum, &mut flags);
            if !flags.is_empty() {
                let entry = data
                    .entry("stability-flags".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Array(a) = entry
                    && a.is_empty()
                {
                    *entry = Value::Object(Map::new());
                }
                if let Value::Object(map) = entry {
                    for (name, flag) in flags {
                        map.insert(name, Value::from(flag));
                    }
                }
            }
            let mtime = std::fs::metadata(&lock_path)
                .and_then(|m| m.modified())
                .ok();
            crate::lock::write_lock(&lock_path, &Value::Object(data))?;
            if let Some(mtime) = mtime {
                let _ = std::fs::File::open(&lock_path).and_then(|f| f.set_modified(mtime));
            }
        }
    }
    Ok(0)
}
