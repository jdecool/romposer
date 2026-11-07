//! `romposer validate`, a port of `ValidateCommand`.
//!
//! Exit codes: 0 valid, 1 warnings with `--strict` (or an exception, as in
//! Composer), 2 errors, 3 file unreadable or missing.

use crate::config::{self, Config};
use crate::io::Io;
use crate::json::Value;
use crate::lock::Lock;
use crate::package::root::load_root;
use crate::schema;
use crate::validation::{self, CHECK_ALL, Report};
use std::path::Path;

#[derive(Debug, Default, clap::Args)]
pub struct Args {
    /// Do not validate requires for overly strict/loose constraints
    #[arg(long)]
    pub no_check_all: bool,
    /// Check if lock file is up to date (even when config.lock is false)
    #[arg(long)]
    pub check_lock: bool,
    /// Do not check if lock file is up to date
    #[arg(long)]
    pub no_check_lock: bool,
    /// Do not check for publish errors
    #[arg(long)]
    pub no_check_publish: bool,
    /// Do not report a warning if the version field is present
    #[arg(long)]
    pub no_check_version: bool,
    /// Also validate the composer.json of all installed dependencies
    #[arg(short = 'A', long)]
    pub with_dependencies: bool,
    /// Return a non-zero exit code for warnings as well as errors
    #[arg(long)]
    pub strict: bool,
    /// path to composer.json file
    pub file: Option<String>,
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
    let file = match &args.file {
        Some(f) => f.clone(),
        None => config::composer_file()?.to_string_lossy().into_owned(),
    };
    let path = Path::new(&file);
    if !path.exists() {
        io.error(&format!("{file} not found."));
        return Ok(3);
    }
    if std::fs::File::open(path).is_err() {
        io.error(&format!("{file} is not readable."));
        return Ok(3);
    }

    let flags = if args.no_check_all { 0 } else { CHECK_ALL };
    let check_publish = !args.no_check_publish;
    let check_version = !args.no_check_version;
    let mut report = validation::validate_file(path, &file, flags, check_version);

    // createComposerInstance: Factory re-reads and lax-validates the file, then
    // RootPackageLoader loads it; any failure is an exception (exit 1).
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("Could not read {file}\n\n{e}"))?;
    let manifest: Value = serde_json::from_str(&contents)
        .map_err(|e| format!("\"{file}\" does not contain valid JSON\n{e}"))?;
    if let Err(errors) = schema::validate(&manifest, false) {
        return Err(format!(
            "\"{file}\" does not match the expected JSON schema:\n - {}",
            errors.join("\n - ")
        ));
    }
    let manifest_obj = manifest.as_object().cloned().unwrap_or_default();
    let root = load_root(&manifest_obj)?;
    let config = Config::load(Some(&root.config));
    let check_lock = (!args.no_check_lock && config.bool("lock")) || args.check_lock;

    let mut lock_errors = Vec::new();
    let lock_path = config::lock_file(path);
    // Factory: a disabled `lock` config means the Locker reads /dev/null.
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
    if let Some(lock) = lock.as_ref().filter(|l| l.is_locked()) {
        if !lock.is_fresh(&contents) {
            lock_errors.push("- The lock file is not up to date with the latest changes in composer.json, it is recommended that you run `composer update` or `composer update <package name>`.".into());
        }
        lock_errors.extend(lock.missing_requirement_info(&root, true)?);
    }

    output_result(
        io,
        &file,
        &mut report,
        check_publish,
        check_lock,
        lock_errors,
        true,
    );
    let mut exit_code = exit_code_for(&report, args.strict);

    if args.with_dependencies {
        let vendor = path
            .parent()
            .unwrap_or(Path::new("."))
            .join(config.vendor_dir());
        for (name, dir) in installed_packages(&vendor) {
            let dep_file = dir.join("composer.json");
            if dir.is_dir() && dep_file.exists() {
                let mut dep = validation::validate_file(
                    &dep_file,
                    &dep_file.to_string_lossy(),
                    flags,
                    check_version,
                );
                output_result(io, &name, &mut dep, check_publish, false, vec![], false);
                exit_code = exit_code.max(exit_code_for(&dep, args.strict));
            }
        }
    }

    Ok(exit_code)
}

fn exit_code_for(report: &Report, strict: bool) -> i32 {
    if !report.errors.is_empty() {
        2
    } else if strict && !report.warnings.is_empty() {
        1
    } else {
        0
    }
}

/// Installed packages from `vendor/composer/installed.json`, as (pretty name, install dir).
fn installed_packages(vendor: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(text) = std::fs::read_to_string(vendor.join("composer/installed.json")) else {
        return vec![];
    };
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return vec![];
    };
    let packages = match &json {
        Value::Object(o) => o
            .get("packages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Value::Array(a) => a.clone(),
        _ => vec![],
    };
    packages
        .iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?.to_string();
            let dir = match p.get("install-path").and_then(Value::as_str) {
                Some(rel) => vendor.join("composer").join(rel),
                None => vendor.join(&name),
            };
            Some((name, dir))
        })
        .collect()
}

/// `ValidateCommand::outputResult`; mutates `report.errors` like the PHP by-reference arrays.
fn output_result(
    io: &mut Io,
    name: &str,
    report: &mut Report,
    check_publish: bool,
    check_lock: bool,
    lock_errors: Vec<String>,
    print_schema_url: bool,
) {
    let mut do_print_schema_url = false;
    if !report.errors.is_empty() {
        io.error(&format!(
            "{name} is invalid, the following errors/warnings were found:"
        ));
    } else if !report.publish_errors.is_empty() && check_publish {
        io.error(&format!(
            "{name} is valid for simple usage with Composer but has"
        ));
        io.error("strict errors that make it unable to be published as a package");
        do_print_schema_url = print_schema_url;
    } else if !report.warnings.is_empty() {
        io.error(&format!("{name} is valid, but with a few warnings"));
        do_print_schema_url = print_schema_url;
    } else if !lock_errors.is_empty() {
        io.write(&format!(
            "{name} is valid but your composer.lock has some {}",
            if check_lock { "errors" } else { "warnings" }
        ));
    } else {
        io.write(&format!("{name} is valid"));
    }

    if do_print_schema_url {
        io.error("See https://getcomposer.org/doc/04-schema.md for details on the schema");
    }

    let mut errors: Vec<String> = Vec::new();
    if !report.errors.is_empty() {
        errors.push("# General errors".into());
        errors.extend(report.errors.iter().map(|e| format!("- {e}")));
    }
    let mut warnings: Vec<String> = Vec::new();
    if !report.warnings.is_empty() {
        warnings.push("# General warnings".into());
        warnings.extend(report.warnings.iter().map(|w| format!("- {w}")));
    }
    if !report.publish_errors.is_empty() && check_publish {
        errors.push("# Publish errors".into());
        errors.extend(report.publish_errors.iter().map(|e| format!("- {e}")));
    }
    if !lock_errors.is_empty() {
        if check_lock {
            errors.push("# Lock file errors".into());
            errors.extend(lock_errors);
        } else {
            warnings.push("# Lock file warnings".into());
            warnings.extend(lock_errors);
        }
    }
    for msg in errors.iter().chain(warnings.iter()) {
        io.error(msg);
    }
    // $errors now includes publish and lock errors, which drives the exit code
    report.errors = errors.into_iter().filter(|e| !e.starts_with('#')).collect();
}
