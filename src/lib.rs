//! romposer: Composer 2.11.1, observably identical, in Rust.

pub mod json;
pub mod semver;

/// Composer version whose observable behaviour romposer reproduces.
pub const COMPOSER_VERSION: &str = "2.11.1";
