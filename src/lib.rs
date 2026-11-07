//! romposer: Composer 2.11.1, observably identical, in Rust.

pub mod cli;
pub mod commands;
pub mod config;
pub mod io;
pub mod json;
pub mod lock;
pub mod package;
pub mod schema;
pub mod semver;
pub mod spdx;
pub mod validation;

/// Composer version whose observable behaviour romposer reproduces.
pub const COMPOSER_VERSION: &str = "2.11.1";
