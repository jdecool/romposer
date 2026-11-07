//! romposer: Composer 2.11.1, observably identical, in Rust.

pub mod autoload;
pub mod binaries;
pub mod cli;
pub mod commands;
pub mod config;
pub mod download;
pub mod fs;
pub mod installed;
pub mod installer;
pub mod io;
pub mod json;
pub mod lock;
pub mod package;
pub mod schema;
pub mod semver;
pub mod spdx;
pub mod transaction;
pub mod validation;

/// Composer version whose observable behaviour romposer reproduces.
pub const COMPOSER_VERSION: &str = "2.11.1";
