//! `romposer install`, a port of `InstallCommand` for projects with a lock file.

use crate::config::{self, Config};
use crate::installer::{Installer, Options};
use crate::io::Io;
use crate::json::Value;
use crate::lock::Lock;
use crate::package::root::load_root;
use crate::schema;

#[derive(Debug, Default, clap::Args)]
pub struct Args {
    /// Forces installation from package sources when possible, including VCS information.
    #[arg(long)]
    pub prefer_source: bool,
    /// Forces installation from package dist (default behavior).
    #[arg(long)]
    pub prefer_dist: bool,
    /// Forces installation from package dist|source|auto (auto chooses source for dev versions, dist for the rest).
    #[arg(long)]
    pub prefer_install: Option<String>,
    /// Outputs the operations but will not execute anything (implicitly enables --verbose).
    #[arg(long)]
    pub dry_run: bool,
    /// Download only, do not install packages.
    #[arg(long)]
    pub download_only: bool,
    /// DEPRECATED: Enables installation of require-dev packages (enabled by default, only present for BC).
    #[arg(long)]
    pub dev: bool,
    /// DEPRECATED: This flag does not exist anymore.
    #[arg(long)]
    pub no_suggest: bool,
    /// Disables installation of require-dev packages.
    #[arg(long)]
    pub no_dev: bool,
    /// Skips the execution of all scripts defined in composer.json file.
    #[arg(long)]
    pub no_scripts: bool,
    /// Whether to disable plugins.
    #[arg(long)]
    pub no_plugins: bool,
    /// Skips autoloader generation
    #[arg(long)]
    pub no_autoloader: bool,
    /// Do not output download progress.
    #[arg(long)]
    pub no_progress: bool,
    /// Do not use, only defined here to catch misuse of the install command.
    #[arg(long)]
    pub no_install: bool,
    /// Run an audit after installation is complete.
    #[arg(long)]
    pub audit: bool,
    /// Audit output format. Must be "table", "plain", "json", or "summary".
    #[arg(long, default_value = "summary")]
    pub audit_format: String,
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
    /// Ignore a specific platform requirement (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_req: Vec<String>,
    /// Ignore all platform requirements (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_reqs: bool,
    /// Should not be provided, use composer require instead to add a given package to composer.json.
    pub packages: Vec<String>,
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
    if !args.packages.is_empty() {
        io.error(&format!("Invalid argument {}. Use \"composer require {}\" instead to add packages to your composer.json.", args.packages.join(" "), args.packages.join(" ")));
        return Ok(1);
    }
    if args.no_install {
        io.error("Invalid option \"--no-install\". Use \"composer update --no-install\" instead if you are trying to update the composer.lock file.");
        return Ok(1);
    }

    let file = config::composer_file()?;
    let cwd = crate::fs::cwd();
    if !file.exists() {
        let message = if file.to_string_lossy() == "./composer.json"
            || file.to_string_lossy() == "composer.json"
        {
            format!("Composer could not find a composer.json file in {cwd}")
        } else {
            format!(
                "Composer could not find the config file: {}",
                file.display()
            )
        };
        return Err(format!(
            "{message}\nTo initialize a project, please create a composer.json file. See https://getcomposer.org/basic-usage"
        ));
    }
    let contents = std::fs::read_to_string(&file)
        .map_err(|e| format!("Could not read {}\n\n{e}", file.display()))?;
    let manifest: Value = serde_json::from_str(&contents)
        .map_err(|e| format!("\"{}\" does not contain valid JSON\n{e}", file.display()))?;
    if let Err(errors) = schema::validate(&manifest, false) {
        return Err(format!(
            "\"{}\" does not match the expected JSON schema:\n - {}",
            file.display(),
            errors.join("\n - ")
        ));
    }
    let manifest_obj = manifest.as_object().cloned().unwrap_or_default();
    let root = load_root(&manifest_obj)?;
    let config = Config::for_project(&cwd, &manifest_obj, &file)?;

    let lock_path = config::lock_file(&file);
    let lock = if config.bool("lock") {
        Lock::read(&lock_path)?
    } else {
        if lock_path.exists() {
            io.error(&format!(
                "{} is present but ignored as the \"lock\" config option is disabled.",
                lock_path.display()
            ));
        }
        None
    };
    let Some(lock) = lock.filter(Lock::is_locked) else {
        let message = "Updating dependencies to latest instead of installing from lock file. See https://getcomposer.org/install for more information.";
        if !config.bool("lock") {
            io.error(&format!(
                "Lockfile creation is disabled in config. {message}"
            ));
        } else {
            io.error(&format!("No composer.lock file present. {message}"));
        }
        return Err("romposer slice 1 installs from an existing composer.lock only; `update` arrives with slice 2".into());
    };

    let dev_mode = !args.no_dev;
    let mut installer = Installer {
        io,
        config: &config,
        root: &root,
        lock: &lock,
        composer_json: &contents,
        options: Options {
            dev_mode,
            dry_run: args.dry_run,
            download_only: args.download_only,
            prefer_source: args.prefer_source || args.prefer_install.as_deref() == Some("source"),
            prefer_dist: args.prefer_dist || args.prefer_install.as_deref() == Some("dist"),
            dump_autoloader: !args.no_autoloader && !args.dry_run && !args.download_only,
            optimize_autoloader: args.optimize_autoloader
                || args.classmap_authoritative
                || config.bool("optimize-autoloader")
                || config.bool("classmap-authoritative"),
            classmap_authoritative: args.classmap_authoritative
                || config.bool("classmap-authoritative"),
            apcu_autoloader: args.apcu_autoloader
                || args.apcu_autoloader_prefix.is_some()
                || config.bool("apcu-autoloader"),
            apcu_autoloader_prefix: args.apcu_autoloader_prefix.clone(),
            run_scripts: !args.no_scripts,
            verbose: args.dry_run,
            strict_psr_autoloader: args.strict_psr_autoloader
                || config.bool("strict-psr-autoloader"),
            ignore_all_platform_reqs: args.ignore_platform_reqs,
            ignored_platform_reqs: args.ignore_platform_req.clone(),
        },
    };
    installer.run()
}
