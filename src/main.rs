use clap::Parser;

/// Composer version whose observable behaviour romposer reproduces.
pub const COMPOSER_VERSION: &str = "2.11.1";

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
struct Cli {
    /// Print version information and exit.
    #[arg(short = 'V', long)]
    version: bool,
}

fn main() {
    let cli = Cli::parse();
    if cli.version {
        println!("{VERSION_LINE}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_names_the_pinned_composer() {
        assert_eq!(VERSION_LINE, "Romposer 0.1.0 (Composer 2.11.1 compatible)");
        assert!(VERSION_LINE.contains(COMPOSER_VERSION));
    }
}
