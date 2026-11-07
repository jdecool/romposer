//! `composer remove`: drop requirements from composer.json, then update.

use crate::commands::dump_autoload::load_project;
use crate::commands::update::run_installer;

use crate::installer::Options;
use crate::io::Io;
use crate::json::manipulator::Manipulator;
use crate::json::{Map, Value};
use crate::solver;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Packages that should be removed.
    pub packages: Vec<String>,
    /// Removes a package from the require-dev section.
    #[arg(long)]
    pub dev: bool,
    /// Skip the audit step after updating the composer.lock file (can also be set via the COMPOSER_NO_AUDIT=1 env var).
    #[arg(long)]
    pub no_audit: bool,
    /// Audit output format. Must be "table", "plain", "json", or "summary".
    #[arg(long, default_value = "summary")]
    pub audit_format: String,
    /// Outputs the operations but will not execute anything (implicitly enables --verbose).
    #[arg(long)]
    pub dry_run: bool,
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
    /// DEPRECATED: This is now default behaviour.
    #[arg(long)]
    pub update_with_dependencies: bool,
    /// Allows all inherited dependencies to be updated, including those that are root requirements.
    #[arg(short = 'W', long)]
    pub update_with_all_dependencies: bool,
    /// Alias for --update-with-all-dependencies
    #[arg(long)]
    pub with_all_dependencies: bool,
    /// Does not allow inherited dependencies to be updated with explicit dependencies.
    #[arg(long)]
    pub no_update_with_dependencies: bool,
    /// Remove all packages which are locked but not required by any other package.
    #[arg(long)]
    pub unused: bool,
    /// Ignore a specific platform requirement (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_req: Vec<String>,
    /// Ignore all platform requirements (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_reqs: bool,
    /// During an update with -w/-W, only perform absolutely necessary changes to transitive dependencies.
    #[arg(short = 'm', long)]
    pub minimal_changes: bool,
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
        Err(message) => crate::scripts::exit_code(io, &message),
    }
}

/// `JsonConfigSource::removeLink`: cleanly through the manipulator, else a full rewrite.
fn remove_link(file: &std::path::Path, kind: &str, name: &str) -> Result<(), String> {
    let contents = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let mut manipulator = Manipulator::new(&contents)?;
    if manipulator.remove_link(kind, name) {
        return std::fs::write(file, manipulator.contents()).map_err(|e| e.to_string());
    }
    let mut definition: Map<String, Value> =
        serde_json::from_str(&contents).map_err(|e| e.to_string())?;
    if let Some(Value::Object(map)) = definition.get_mut(kind) {
        map.shift_remove(name);
        if map.is_empty() {
            definition.shift_remove(kind);
        }
    }
    std::fs::write(
        file,
        crate::json::encode(&Value::Object(definition), true, true, true) + "\n",
    )
    .map_err(|e| e.to_string())
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    if args.packages.is_empty() && !args.unused {
        return Err("Not enough arguments (missing: \"packages\").".into());
    }
    let mut packages: Vec<String> = args.packages.iter().map(|p| p.to_lowercase()).collect();
    let (file, contents, root, _config, lock) = load_project(io)?;
    if args.unused {
        let Some(lock) = lock.as_ref().filter(|l| l.is_locked()) else {
            return Err(
                "A valid composer.lock file is required to run this command with --unused".into(),
            );
        };
        let mut locked = lock.locked_packages(true)?;
        let mut required: Vec<String> = root
            .package
            .requires
            .iter()
            .chain(root.package.dev_requires.iter())
            .map(|l| l.target.clone())
            .collect();
        loop {
            let mut found = false;
            let mut i = 0;
            while i < locked.len() {
                if solver::names(&locked[i], true)
                    .iter()
                    .any(|n| required.contains(n))
                {
                    for link in &locked[i].requires {
                        required.push(link.target.clone());
                    }
                    found = true;
                    locked.remove(i);
                } else {
                    i += 1;
                }
            }
            if !found {
                break;
            }
        }
        packages.extend(locked.iter().map(|p| p.name.clone()));
        if packages.is_empty() {
            io.error("No unused packages to remove");
            return Ok(0);
        }
    }
    let composer_backup = contents.clone();
    let definition: Map<String, Value> = serde_json::from_str(&contents).unwrap_or_default();
    let (kind, alt) = if args.dev {
        ("require-dev", "require")
    } else {
        ("require", "require-dev")
    };
    if args.update_with_dependencies {
        io.warning("You are using the deprecated option \"update-with-dependencies\". This is now default behaviour. The --no-update-with-dependencies option can be used to remove a package without its dependencies.");
    }
    // lowercased name => name as written
    let names_in = |key: &str| -> Vec<(String, String)> {
        definition
            .get(key)
            .and_then(Value::as_object)
            .map(|m| m.keys().map(|k| (k.to_lowercase(), k.clone())).collect())
            .unwrap_or_default()
    };
    let (in_kind, in_alt) = (names_in(kind), names_in(alt));
    let mut to_remove: Vec<(&str, String)> = Vec::new();
    for package in &packages {
        let regex = crate::repository::package_name_to_regexp(package);
        if let Some((_, written)) = in_kind.iter().find(|(l, _)| l == package) {
            to_remove.push((kind, written.clone()));
        } else if let Some((_, written)) = in_alt.iter().find(|(l, _)| l == package) {
            io.warning(&format!(
                "{written} could not be found in {kind} but it is present in {alt}"
            ));
        } else if in_kind.iter().any(|(_, w)| regex.is_match(w)) {
            for (_, written) in in_kind.iter().filter(|(_, w)| regex.is_match(w)) {
                to_remove.push((kind, written.clone()));
            }
        } else if in_alt.iter().any(|(_, w)| regex.is_match(w)) {
            for (_, written) in in_alt.iter().filter(|(_, w)| regex.is_match(w)) {
                io.warning(&format!(
                    "{written} could not be found in {kind} but it is present in {alt}"
                ));
            }
        } else {
            io.warning(&format!(
                "{package} is not required in your composer.json and has not been removed"
            ));
        }
    }
    if !args.dry_run {
        for (key, name) in &to_remove {
            remove_link(&file, key, name)?;
        }
    }
    io.error(&format!("{} has been updated", file.display()));
    if args.no_update {
        return Ok(0);
    }

    let (file, contents, mut root, config, lock) = load_project(io)?;
    if args.dry_run {
        for (key, name) in &to_remove {
            let lower = name.to_lowercase();
            if *key == "require" {
                root.package.requires.retain(|l| l.target != lower);
            } else {
                root.package.dev_requires.retain(|l| l.target != lower);
            }
        }
    }
    let (transitive, flags) = if args.update_with_all_dependencies || args.with_all_dependencies {
        (
            solver::UPDATE_LISTED_WITH_TRANSITIVE_DEPS,
            " --with-all-dependencies",
        )
    } else if args.no_update_with_dependencies {
        (solver::UPDATE_ONLY_LISTED, " --with-dependencies")
    } else {
        (
            solver::UPDATE_LISTED_WITH_TRANSITIVE_DEPS_NO_ROOT_REQUIRE,
            "",
        )
    };
    io.error(&format!(
        "Running composer update {}{flags}",
        packages.join(" ")
    ));
    let mut options = Options::defaults();
    options.update = true;
    options.install = !args.no_install;
    options.dry_run = args.dry_run;
    options.dev_mode = !args.update_no_dev;
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
    options.minimal_update = args.minimal_changes || config.bool("update-with-minimal-changes");
    options.update_allow_transitive = transitive;
    options.write_lock = config.bool("lock");
    options.audit = crate::commands::audit::enabled(args.no_audit);
    options.audit_format = args.audit_format.clone();
    if lock.as_ref().is_some_and(|l| l.is_locked()) {
        options.update_allow_list = Some(packages.clone());
    }
    let (status, _) = run_installer(io, &file, &contents, &root, &config, lock, options)?;
    if status != 0 {
        io.error(&format!(
            "\nRemoval failed, reverting {} to its original content.",
            file.display()
        ));
        std::fs::write(&file, composer_backup).map_err(|e| e.to_string())?;
    }
    if !args.dry_run {
        let installed = crate::installed::InstalledRepository::read(std::path::Path::new(
            &format!("{}/composer/installed.json", config.vendor_dir()),
        ))?;
        for package in &packages {
            if installed.packages.iter().any(|p| p.name == *package) {
                io.error(&format!(
                    "Removal failed, {package} is still present, it may be required by another package. See `composer why {package}`."
                ));
                return Ok(2);
            }
        }
    }
    Ok(status)
}
