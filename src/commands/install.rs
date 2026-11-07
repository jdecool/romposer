//! `romposer install`, a port of `InstallCommand` for projects with a lock file.

use crate::commands::dump_autoload::load_project;
use crate::config;
use crate::installer::{Installer, Options};
use crate::io::Io;
use crate::platform::PhpInfo;
use crate::solver::builder::Repository;

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

    let (file, contents, root, config, lock) = load_project(io)?;
    let dev_mode = !args.no_dev;
    let mut executor = crate::installer::FilesystemExecutor;
    let http = std::rc::Rc::new(crate::download::Http::new(&config)?);
    let mut composer_repos = crate::repository::from_config(&config, http)?;
    let repos: Vec<Repository> = composer_repos
        .iter_mut()
        .map(Repository::Composer)
        .collect();
    let mut options = Options::defaults();
    options.dev_mode = dev_mode;
    options.dry_run = args.dry_run;
    options.download_only = args.download_only;
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
    options.ignore_all_platform_reqs = args.ignore_platform_reqs;
    options.ignored_platform_reqs = args.ignore_platform_req.clone();
    options.write_lock = config.bool("lock");
    options.audit = args.audit;
    options.audit_format = args.audit_format.clone();
    options.error_on_audit = args.audit;
    let mut installer = Installer {
        io,
        config: &config,
        root: &root,
        lock,
        lock_path: Some(config::lock_file(&file)),
        composer_json: &contents,
        options,
        executor: &mut executor,
        repos,
        php: PhpInfo::detect(),
        written_lock: None,
        suggesting_packages: vec![],
    };
    installer.run()
}
