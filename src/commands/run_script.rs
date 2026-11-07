//! `run-script`, `exec` and `composer <script>` aliases: ports of `RunScriptCommand`,
//! `ExecCommand` and `ScriptAliasCommand`.

use crate::commands::dump_autoload::load_project;
use crate::io::Io;
use crate::scripts::{self, SCRIPT_EVENTS};
use std::path::Path;
use std::sync::atomic::Ordering;

#[derive(Debug, Default, clap::Args)]
pub struct RunScriptArgs {
    /// Script name to run.
    pub script: Option<String>,
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
    /// Sets script timeout in seconds, or 0 for never.
    #[arg(long)]
    pub timeout: Option<String>,
    /// Sets the dev mode.
    #[arg(long)]
    pub dev: bool,
    /// Disables the dev mode.
    #[arg(long)]
    pub no_dev: bool,
    /// List scripts.
    #[arg(short = 'l', long)]
    pub list: bool,
}

#[derive(Debug, Default, clap::Args)]
pub struct ExecArgs {
    #[arg(short = 'l', long)]
    pub list: bool,
    /// The binary to run, e.g. phpunit
    pub binary: Option<String>,
    /// Arguments to pass to the binary. Use -- to separate from composer arguments
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
}

fn fail(io: &mut Io, result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(message) => scripts::exit_code(io, &message),
    }
}

pub fn run_script(args: &RunScriptArgs, io: &mut Io) -> i32 {
    let r = run_script_inner(args, io);
    fail(io, r)
}

fn run_script_inner(args: &RunScriptArgs, io: &mut Io) -> Result<(), String> {
    let (_, contents, root, config, _) = load_project(io)?;
    if args.list {
        let names: Vec<&String> = root.package.scripts.keys().collect();
        if names.is_empty() {
            return Ok(());
        }
        let manifest: serde_json::Value = serde_json::from_str(&contents).unwrap_or_default();
        io.error("scripts:");
        let rows: Vec<Vec<String>> = names
            .iter()
            .map(|name| {
                let description = if SCRIPT_EVENTS.contains(&name.as_str()) {
                    String::new()
                } else {
                    manifest["scripts-descriptions"][name.as_str()]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!("Runs the {name} script as defined in composer.json")
                        })
                };
                vec![format!("  {name}"), description]
            })
            .collect();
        for line in crate::commands::table::compact(None, &rows) {
            io.write(&line);
        }
        return Ok(());
    }
    let script = args
        .script
        .as_deref()
        .ok_or("Missing required argument \"script\"")?;
    let dev_mode = args.dev || !args.no_dev;
    if scripts::listeners(&root, script).is_empty() {
        return Err(format!(
            "Script \"{script}\" is not defined in this package"
        ));
    }
    if let Some(timeout) = &args.timeout {
        let seconds: i64 = match timeout.parse() {
            Ok(n) if !timeout.is_empty() && timeout.bytes().all(|b| b.is_ascii_digit()) => n,
            _ => {
                return Err(
                    "Timeout value must be numeric and positive if defined, or 0 for forever"
                        .into(),
                );
            }
        };
        scripts::TIMEOUT_OVERRIDE.store(seconds, Ordering::Relaxed);
    }
    scripts::put_env("COMPOSER_DEV_MODE", Some(if dev_mode { "1" } else { "0" }))?;
    scripts::dispatch(io, &config, &root, script, &args.args, None)
}

pub fn exec(args: &ExecArgs, io: &mut Io) -> i32 {
    let r = exec_inner(args, io);
    fail(io, r)
}

fn exec_inner(args: &ExecArgs, io: &mut Io) -> Result<(), String> {
    let (_, _, root, config, _) = load_project(io)?;
    let Some(binary) = args.binary.as_deref().filter(|_| !args.list) else {
        let bin_dir = config.get_str("bin-dir");
        let mut bins: Vec<String> = std::fs::read_dir(&bin_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        bins.sort();
        bins.extend(root.package.binaries.iter().map(|b| format!("{b} (local)")));
        let mut names: Vec<String> = Vec::new();
        let mut previous: Option<&str> = None;
        for bin in &bins {
            if previous.is_some_and(|p| *bin == format!("{p}.bat")) {
                continue;
            }
            previous = Some(bin);
            names.push(
                Path::new(bin)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
        }
        if names.is_empty() {
            return Err(format!(
                "No binaries found in composer.json or in bin-dir ({bin_dir})"
            ));
        }
        io.write("Available binaries:");
        for name in names {
            io.write(&format!("- {name}"));
        }
        return Ok(());
    };
    scripts::dispatch_exec(io, &config, &root, binary, &args.args)
}

/// `composer <script> [--dev|--no-dev] [args...]` for a script that is not a reserved event.
pub fn alias(name: &str, rest: &[String], io: &mut Io) -> i32 {
    let r = alias_inner(name, rest, io);
    fail(io, r)
}

fn alias_inner(name: &str, rest: &[String], io: &mut Io) -> Result<(), String> {
    let project = crate::commands::dump_autoload::try_load_project(io)?;
    let Some((_, _, root, config, _)) = project
        .filter(|p| !SCRIPT_EVENTS.contains(&name) && p.2.package.scripts.contains_key(name))
    else {
        return Err(format!("Command \"{name}\" is not defined."));
    };
    let dev_mode = rest.iter().any(|a| a == "--dev") || !rest.iter().any(|a| a == "--no-dev");
    let args: Vec<String> = rest
        .iter()
        .filter(|a| *a != "--dev" && *a != "--no-dev")
        .cloned()
        .collect();
    scripts::put_env("COMPOSER_DEV_MODE", Some(if dev_mode { "1" } else { "0" }))?;
    let input = rest.join(" ");
    scripts::dispatch(io, &config, &root, name, &args, Some(&input))
}
