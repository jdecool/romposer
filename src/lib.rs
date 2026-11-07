//! romposer: Composer 2.11.1, observably identical, in Rust.

pub mod autoload;
pub mod binaries;
pub mod classmap;
pub mod cli;
pub mod commands;
pub mod config;
pub mod download;
pub mod fixtures;
pub mod fs;
pub mod git;
pub mod installed;
pub mod installer;
pub mod io;
pub mod json;
pub mod lock;
pub mod package;
pub mod path_repo;
pub mod platform;
pub mod repository;
pub mod schema;
pub mod scripts;
pub mod semver;
pub mod solver;
pub mod spdx;
pub mod store;
pub mod transaction;
pub mod validation;
pub mod vcs;
pub mod version_selector;
pub mod zend_sort;

/// Composer version whose observable behaviour romposer reproduces.
pub const COMPOSER_VERSION: &str = "2.11.1";
/// `PluginInterface::PLUGIN_API_VERSION`
pub const PLUGIN_API_VERSION: &str = "2.11.0";
/// `Composer::RUNTIME_API_VERSION`
pub const RUNTIME_API_VERSION: &str = "2.2.2";
