//! `romposer dump-autoload`, a port of `DumpAutoloadCommand`.

use crate::autoload::{self, DumpOptions};
use crate::config::{self, Config};
use crate::installed::InstalledRepository;
use crate::installer::install_path;
use crate::io::Io;
use crate::json::Value;
use crate::lock::Lock;
use crate::package::root::load_root;
use crate::schema;
use std::path::Path;

#[derive(Debug, Default, clap::Args)]
pub struct Args {
    /// Optimizes PSR0 and PSR4 packages to be loaded with classmaps too, good for production.
    #[arg(short = 'o', long)]
    pub optimize: bool,
    /// Autoload classes from the classmap only. Implicitly enables `--optimize`.
    #[arg(short = 'a', long)]
    pub classmap_authoritative: bool,
    /// Use APCu to cache found/not-found classes.
    #[arg(long)]
    pub apcu: bool,
    /// Use a custom prefix for the APCu autoloader cache. Implicitly enables --apcu
    #[arg(long)]
    pub apcu_prefix: Option<String>,
    /// Outputs the operations but will not execute anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Enables autoload-dev rules.
    #[arg(long)]
    pub dev: bool,
    /// Disables autoload-dev rules.
    #[arg(long)]
    pub no_dev: bool,
    /// Ignore a specific platform requirement (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_req: Vec<String>,
    /// Ignore all platform requirements (php & ext- packages).
    #[arg(long)]
    pub ignore_platform_reqs: bool,
    /// Return a failed status code (1) if PSR-4 or PSR-0 mapping errors are present. Requires --optimize to work.
    #[arg(long)]
    pub strict_psr: bool,
    /// Return a failed status code (2) if the same class is found in multiple files. Requires --optimize to work.
    #[arg(long)]
    pub strict_ambiguous: bool,
}

pub fn run(args: &Args, io: &mut Io) -> i32 {
    match execute(args, io) {
        Ok(code) => code,
        Err(message) => crate::scripts::exit_code(io, &message),
    }
}

/// Loads the project (manifest, root package, config, lock) the way `requireComposer` does.
pub fn load_project(
    io: &mut Io,
) -> Result<
    (
        std::path::PathBuf,
        String,
        crate::package::root::RootPackage,
        Config,
        Option<Lock>,
    ),
    String,
> {
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
    Ok((file, contents, root, config, lock))
}

/// `tryComposer`: `None` when there is no composer.json, any other failure is an error.
#[allow(clippy::type_complexity)]
pub fn try_load_project(
    io: &mut Io,
) -> Result<
    Option<(
        std::path::PathBuf,
        String,
        crate::package::root::RootPackage,
        Config,
        Option<Lock>,
    )>,
    String,
> {
    if !config::composer_file()?.exists() {
        return Ok(None);
    }
    load_project(io).map(Some)
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    let (_file, _contents, root, config, lock) = load_project(io)?;
    let vendor_dir = config.vendor_dir();
    let local_repo =
        InstalledRepository::read(Path::new(&format!("{vendor_dir}/composer/installed.json")))?;
    for package in local_repo.canonical_packages() {
        if let Some(path) = install_path(&vendor_dir, package)
            && !Path::new(&path).exists()
        {
            io.write("Not all dependencies are installed. Make sure to run a \"composer install\" to install missing dependencies");
            break;
        }
    }

    let optimize = args.optimize || config.bool("optimize-autoloader");
    let authoritative = args.classmap_authoritative || config.bool("classmap-authoritative");
    let strict_psr = args.strict_psr || config.bool("strict-psr-autoloader");
    let apcu = args.apcu_prefix.is_some() || args.apcu || config.bool("apcu-autoloader");
    if strict_psr && !optimize && !authoritative {
        return Err("--strict-psr mode only works with optimized autoloader, use --optimize or --classmap-authoritative if you want a strict return value.".into());
    }
    if args.strict_ambiguous && !optimize && !authoritative {
        return Err("--strict-ambiguous mode only works with optimized autoloader, use --optimize or --classmap-authoritative if you want a strict return value.".into());
    }
    if args.dev && args.no_dev {
        return Err(
            "You can not use both --no-dev and --dev as they conflict with each other.".into(),
        );
    }
    if authoritative {
        io.write("Generating optimized autoload files (authoritative)");
    } else if optimize {
        io.write("Generating optimized autoload files");
    } else {
        io.write("Generating autoload files");
    }

    let options = DumpOptions {
        optimize,
        authoritative,
        apcu,
        apcu_prefix: args.apcu_prefix.clone(),
        dev_mode: if args.no_dev {
            Some(false)
        } else if args.dev {
            Some(true)
        } else {
            None
        },
        dry_run: args.dry_run,
        run_scripts: true,
        strict_ambiguous: args.strict_ambiguous,
        ignore_all_platform_reqs: args.ignore_platform_reqs,
        ignored_platform_reqs: args.ignore_platform_req.clone(),
    };
    let class_map = autoload::dump(io, &config, &local_repo, &root, lock.as_ref(), &options)?;
    let count = class_map.len();
    if authoritative {
        io.write(&format!(
            "Generated optimized autoload files (authoritative) containing {count} classes"
        ));
    } else if optimize {
        io.write(&format!(
            "Generated optimized autoload files containing {count} classes"
        ));
    } else {
        io.write("Generated autoload files");
    }
    let mut exit = 0;
    if strict_psr && !class_map.psr_violations().is_empty() {
        exit = 1;
    }
    if args.strict_ambiguous && !class_map.ambiguous_classes(false).is_empty() {
        exit = 2;
    }
    Ok(exit)
}
