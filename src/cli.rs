//! Command-line surface, mirroring Composer's global options where they exist.

use crate::commands;
use crate::io::Io;
use clap::{Parser, Subcommand};

const VERSION_LINE: &str = concat!(
    "Romposer ",
    env!("CARGO_PKG_VERSION"),
    " (Composer 2.11.1 compatible)"
);

#[derive(Parser)]
#[command(
    name = "romposer",
    about = "Composer, in Rust, with a shared package store",
    disable_version_flag = true
)]
pub struct Cli {
    /// Print version information and exit.
    #[arg(short = 'V', long, global = true)]
    version: bool,
    /// Do not output any message
    #[arg(short = 'q', long, global = true)]
    quiet: bool,
    /// Increase the verbosity of messages
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Do not ask any interactive question
    #[arg(short = 'n', long, global = true)]
    no_interaction: bool,
    /// Do not output ANSI escape sequences
    #[arg(long, global = true)]
    no_ansi: bool,
    /// If specified, use the given directory as working directory.
    #[arg(short = 'd', long, global = true)]
    working_dir: Option<String>,
    /// Whether to disable plugins.
    #[arg(long, global = true)]
    no_plugins: bool,
    /// Skips the execution of all scripts defined in composer.json file.
    #[arg(long, global = true)]
    no_scripts: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Validates a composer.json and composer.lock
    Validate(commands::validate::Args),
    /// Installs the project dependencies from the composer.lock file if present, or falls back on the composer.json.
    #[command(visible_alias = "i")]
    Install(commands::install::Args),
    /// Updates your dependencies to the latest version according to composer.json, and updates the composer.lock file.
    #[command(visible_aliases = ["u", "upgrade"])]
    Update(commands::update::Args),
    /// Adds required packages to your composer.json and installs them.
    #[command(visible_alias = "r")]
    Require(commands::require::Args),
    /// Removes a package from the require or require-dev.
    #[command(visible_alias = "rm")]
    Remove(commands::remove::Args),
    /// Checks for security vulnerability advisories for installed packages
    Audit(commands::audit::Args),
    /// Shows a list of installed packages that have updates available, including their latest version.
    Outdated(commands::readonly::OutdatedArgs),
    /// Shows information about licenses of dependencies.
    Licenses(commands::readonly::LicensesArgs),
    /// Discover how to help fund the maintenance of your dependencies.
    Fund(commands::readonly::FundArgs),
    /// Shows package suggestions.
    Suggests(commands::readonly::SuggestsArgs),
    /// Shows a list of locally modified packages.
    Status(commands::readonly::StatusArgs),
    /// Shows which packages cause the given package to be installed.
    #[command(visible_alias = "why")]
    Depends(commands::depends::Args),
    /// Shows which packages prevent the given package from being installed.
    #[command(visible_alias = "why-not")]
    Prohibits(commands::depends::Args),
    /// Dumps the autoloader.
    #[command(name = "dump-autoload", visible_aliases = ["dumpautoload", "dump"])]
    DumpAutoload(commands::dump_autoload::Args),
    /// Check that platform requirements are satisfied.
    #[command(name = "check-platform-reqs")]
    CheckPlatformReqs(commands::check_platform_reqs::Args),
    /// Searches for packages.
    Search(commands::search::Args),
    /// Shows information about packages.
    #[command(visible_alias = "info")]
    Show(commands::show::Args),
    /// Inspect and maintain the shared package store
    #[command(subcommand)]
    Store(commands::store::Command),
    /// Runs the scripts defined in composer.json
    #[command(name = "run-script", visible_alias = "run")]
    RunScript(commands::run_script::RunScriptArgs),
    /// Executes a vendored binary/script.
    Exec(commands::run_script::ExecArgs),
    /// Sets config options.
    Config(commands::config::Args),
    /// Creates an archive of this composer package.
    Archive(commands::archive::Args),
    /// Creates new project from a package into given directory.
    #[command(name = "create-project")]
    CreateProject(commands::create_project::Args),
    /// Diagnoses the system to identify common errors.
    Diagnose(commands::diagnose::Args),
    /// Creates a basic composer.json file in current directory.
    Init(commands::init::Args),
    /// Clears composer's internal package cache.
    #[command(name = "clear-cache", visible_aliases = ["clearcache", "cc"])]
    ClearCache(commands::clear_cache::Args),
    /// Allows running commands in the global composer dir ($COMPOSER_HOME).
    Global {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// A script of composer.json run as its own command (`ScriptAliasCommand`).
    #[command(external_subcommand)]
    Script(Vec<String>),
}

/// Runs the CLI and returns the process exit code.
pub fn run() -> i32 {
    run_from(std::env::args().collect())
}

/// `GlobalCommand`: the same command line without its `global` token, run from the home dir.
fn global(argv: &[String], io: &mut Io) -> i32 {
    // the COMPOSER env var does not apply to the global scope
    // SAFETY: single-threaded here, before any command spawns threads
    unsafe { std::env::remove_var("COMPOSER") };
    let home = match crate::config::Config::create(&crate::fs::cwd()) {
        Ok(config) => config.get_str("home"),
        Err(e) => return crate::scripts::exit_code(io, &e),
    };
    if crate::fs::ensure_dir(std::path::Path::new(&home)).is_err() {
        return crate::scripts::exit_code(io, "Could not create home directory");
    }
    if std::env::set_current_dir(&home).is_err() {
        return crate::scripts::exit_code(
            io,
            &format!("Could not switch to home directory \"{home}\""),
        );
    }
    io.error(&format!("Changed current directory to {home}"));
    let token = regex::Regex::new(r"^g(?:l(?:o(?:b(?:a(?:l)?)?)?)?)?$").unwrap();
    let mut argv = argv.to_vec();
    if let Some(i) = argv.iter().skip(1).position(|a| token.is_match(a)) {
        argv.remove(i + 1);
    }
    run_from(argv)
}

/// Runs a command line (program name first).
pub fn run_from(argv: Vec<String>) -> i32 {
    let cli = Cli::parse_from(&argv);
    if cli.version {
        println!("{VERSION_LINE}");
        return 0;
    }
    if let Some(dir) = &cli.working_dir
        && let Err(e) = std::env::set_current_dir(dir)
    {
        eprintln!("Invalid working directory specified, {dir} does not exist. ({e})");
        return 1;
    }
    // `Factory::createComposer`: `--no-scripts` disables the event dispatcher's scripts
    if cli.no_scripts {
        crate::scripts::RUN_SCRIPTS.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    crate::scripts::NO_PLUGINS.store(cli.no_plugins, std::sync::atomic::Ordering::Relaxed);
    let mut io = Io::new(cli.quiet, cli.verbose);
    match cli.command {
        Some(Command::Validate(args)) => commands::validate::run(&args, &mut io),
        Some(Command::Install(args)) => commands::install::run(&args, &mut io),
        Some(Command::Update(args)) => commands::update::run(&args, &mut io),
        Some(Command::Require(args)) => commands::require::run(&args, &mut io),
        Some(Command::Remove(args)) => commands::remove::run(&args, &mut io),
        Some(Command::DumpAutoload(args)) => commands::dump_autoload::run(&args, &mut io),
        Some(Command::Store(command)) => commands::store::run(&command, &mut io),
        Some(Command::Search(args)) => commands::search::run(&args, &mut io),
        Some(Command::Show(args)) => commands::show::run(&args, &mut io),
        Some(Command::Audit(args)) => commands::audit::run(&args, &mut io),
        Some(Command::Outdated(args)) => commands::readonly::outdated(&args, &mut io),
        Some(Command::Licenses(args)) => commands::readonly::licenses(&args, &mut io),
        Some(Command::Fund(args)) => commands::readonly::fund(&args, &mut io),
        Some(Command::Suggests(args)) => commands::readonly::suggests(&args, &mut io),
        Some(Command::Status(args)) => commands::readonly::status(&args, &mut io),
        Some(Command::Depends(args)) => commands::depends::depends(&args, &mut io),
        Some(Command::Prohibits(args)) => commands::depends::prohibits(&args, &mut io),
        Some(Command::RunScript(args)) => commands::run_script::run_script(&args, &mut io),
        Some(Command::Exec(args)) => commands::run_script::exec(&args, &mut io),
        Some(Command::Config(args)) => commands::config::run(&args, &mut io),
        Some(Command::Archive(args)) => commands::archive::run(&args, &mut io),
        Some(Command::CreateProject(args)) => commands::create_project::run(&args, &mut io),
        Some(Command::Diagnose(args)) => commands::diagnose::run(&args, &mut io),
        Some(Command::Init(args)) => commands::init::run(&args, &mut io),
        Some(Command::ClearCache(args)) => commands::clear_cache::run(&args, &mut io),
        Some(Command::Global { .. }) => global(&argv, &mut io),
        Some(Command::Script(argv)) => commands::run_script::alias(&argv[0], &argv[1..], &mut io),
        Some(Command::CheckPlatformReqs(args)) => {
            commands::check_platform_reqs::run(&args, &mut io)
        }
        None => {
            println!("{VERSION_LINE}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_names_the_pinned_composer() {
        assert_eq!(VERSION_LINE, "Romposer 0.1.0 (Composer 2.11.1 compatible)");
        assert!(VERSION_LINE.contains(crate::COMPOSER_VERSION));
    }
}
