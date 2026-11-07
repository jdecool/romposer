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
    /// Dumps the autoloader.
    #[command(name = "dump-autoload", visible_aliases = ["dumpautoload", "dump"])]
    DumpAutoload(commands::dump_autoload::Args),
    /// Inspect and maintain the shared package store
    #[command(subcommand)]
    Store(commands::store::Command),
}

/// Runs the CLI and returns the process exit code.
pub fn run() -> i32 {
    let cli = Cli::parse();
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
    let mut io = Io {
        quiet: cli.quiet,
        verbosity: cli.verbose,
    };
    match cli.command {
        Some(Command::Validate(args)) => commands::validate::run(&args, &mut io),
        Some(Command::Install(args)) => commands::install::run(&args, &mut io),
        Some(Command::DumpAutoload(args)) => commands::dump_autoload::run(&args, &mut io),
        Some(Command::Store(command)) => commands::store::run(&command, &mut io),
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
