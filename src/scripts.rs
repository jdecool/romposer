//! `EventDispatcher` for the shell listeners of composer.json `scripts`: `@script` references,
//! `@composer`, `@php`, `@putenv`, additional arguments and the process timeout. PHP callbacks and
//! command classes are reported as not autoloadable, which for romposer they never are.
//!
//! Scripts run through `sh -c` with inherited stdio, like Composer's `Process::fromShellCommandline`.
//! `@composer x` and `composer x` re-run this executable as a child process, as Composer re-runs
//! its own `COMPOSER_BINARY`: the shell still expands the line, so the semantics are Composer's.
//!
//! A failing script is Composer's `ScriptExecutionException`: the `Err(FAILURE)` marker unwinds
//! like the exception and [`exit_code`] turns it into the script's own exit code.

use crate::config::Config;
use crate::io::Io;
use crate::package::root::RootPackage;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::time::{Duration, Instant};

static PHP_PATH_NAME: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"^[^'"\s/\\]+"#).unwrap());

/// `ScriptEvents`: the names `run-script` and script aliases treat as reserved.
pub const SCRIPT_EVENTS: &[&str] = &[
    "pre-install-cmd",
    "post-install-cmd",
    "pre-update-cmd",
    "post-update-cmd",
    "pre-status-cmd",
    "post-status-cmd",
    "pre-autoload-dump",
    "post-autoload-dump",
    "post-root-package-install",
    "post-create-project-cmd",
    "pre-archive-cmd",
    "post-archive-cmd",
];

/// `run-script --timeout`: overrides `process-timeout` when set (seconds).
pub static TIMEOUT_OVERRIDE: AtomicI64 = AtomicI64::new(-1);

/// The dispatcher's `setRunScripts`: off with `--no-scripts` anywhere on the command line.
pub static RUN_SCRIPTS: AtomicBool = AtomicBool::new(true);

/// `--no-plugins`: no warning about plugins that are not executed anyway.
pub static NO_PLUGINS: AtomicBool = AtomicBool::new(false);

/// `ScriptExecutionException`: already reported, the exit code is in [`FAILED_CODE`].
pub const FAILURE: &str = "\0script failed";
static FAILED_CODE: AtomicI32 = AtomicI32::new(1);

/// The exit code of a command that ended in `Err(message)`: a failed script's own code, or 1
/// after printing the message.
pub fn exit_code(io: &mut Io, message: &str) -> i32 {
    if message == FAILURE {
        return FAILED_CODE.load(Ordering::Relaxed);
    }
    io.error(message);
    1
}

/// `Platform::putEnv` as an overlay: every process we spawn sees it, ours stays untouched.
static ENV: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

/// Sets (or with `None` clears) a variable for every later script and child process.
pub fn put_env(name: &str, value: Option<&str>) -> Result<(), String> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return Err(format!("Invalid environment variable name \"{name}\""));
    }
    let mut env = ENV.lock().unwrap();
    env.retain(|(n, _)| n != name);
    env.push((name.to_string(), value.map(str::to_string)));
    Ok(())
}

pub fn get_env(name: &str) -> Option<String> {
    let env = ENV.lock().unwrap();
    match env.iter().find(|(n, _)| n == name) {
        Some((_, value)) => value.clone(),
        None => std::env::var(name).ok(),
    }
}

/// The script strings listening to `event` in the root package.
pub fn listeners(root: &RootPackage, event: &str) -> Vec<String> {
    if !RUN_SCRIPTS.load(Ordering::Relaxed) {
        return vec![];
    }
    match root.package.scripts.get(event) {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => vec![],
    }
}

/// `ProcessExecutor::escape` on a unix shell.
pub fn escape(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// `dispatchScript` for a script event without arguments, as the installer fires them.
pub fn fire(io: &mut Io, config: &Config, root: &RootPackage, event: &str) -> Result<(), String> {
    dispatch(io, config, root, event, &[], None)
}

/// `dispatchScript`: runs the listeners of `event` with the user's additional arguments.
pub fn dispatch(
    io: &mut Io,
    config: &Config,
    root: &RootPackage,
    event: &str,
    args: &[String],
    alias_input: Option<&str>,
) -> Result<(), String> {
    let listeners = listeners(root, event);
    Dispatcher {
        io,
        config,
        root,
        stack: vec![],
    }
    .dispatch(event, listeners, args, alias_input)
}

/// `exec`: the binary is the only listener of the `__exec_command` event.
pub fn dispatch_exec(
    io: &mut Io,
    config: &Config,
    root: &RootPackage,
    binary: &str,
    args: &[String],
) -> Result<(), String> {
    Dispatcher {
        io,
        config,
        root,
        stack: vec![],
    }
    .dispatch("__exec_command", vec![binary.to_string()], args, None)
}

struct Dispatcher<'a> {
    io: &'a mut Io,
    config: &'a Config,
    root: &'a RootPackage,
    stack: Vec<String>,
}

impl Dispatcher<'_> {
    fn dispatch(
        &mut self,
        event: &str,
        listeners: Vec<String>,
        args: &[String],
        alias_input: Option<&str>,
    ) -> Result<(), String> {
        if std::env::var("COMPOSER_DEBUG_EVENTS").is_ok_and(|v| !v.is_empty() && v != "0") {
            self.io.error(&format!("Dispatching {event} event"));
        }
        let listeners = self.script_listeners(event, listeners);
        if self.stack.iter().any(|e| e == event) {
            return Err(format!(
                "Circular call to script handler '{event}' detected"
            ));
        }
        self.stack.push(event.to_string());
        let result = self.run_listeners(event, listeners, args, alias_input);
        self.stack.pop();
        result
    }

    /// `getScriptListeners` with `COMPOSER_SKIP_SCRIPTS`.
    fn script_listeners(&mut self, event: &str, listeners: Vec<String>) -> Vec<String> {
        if listeners.is_empty() {
            return listeners;
        }
        let skipped = std::env::var("COMPOSER_SKIP_SCRIPTS").unwrap_or_default();
        if skipped.split(',').map(str::trim).any(|s| s == event) {
            if self.io.verbosity >= 1 {
                self.io.error(&format!(
                    "Skipped script listeners for {event} because of COMPOSER_SKIP_SCRIPTS"
                ));
            }
            return vec![];
        }
        listeners
    }

    fn run_listeners(
        &mut self,
        event: &str,
        listeners: Vec<String>,
        args: &[String],
        alias_input: Option<&str>,
    ) -> Result<(), String> {
        for mut callable in listeners {
            self.ensure_bin_dir_in_path();
            let mut additional_args: Vec<String> = args.to_vec();
            if callable.contains("@no_additional_args") {
                callable = callable
                    .replace(" @no_additional_args", "")
                    .replace("@no_additional_args", "");
                additional_args.clear();
            }
            let formatted = if additional_args.is_empty() {
                event.to_string()
            } else {
                format!("{event} ({})", additional_args.join(", "))
            };
            let escaped_args = additional_args
                .iter()
                .map(|a| escape(a))
                .collect::<Vec<_>>()
                .join(" ");
            if is_composer_script(&callable) {
                if self.io.verbosity >= 1 {
                    self.io.error(&format!("> {formatted}: {callable}"));
                }
                if let Some(rest) = callable.strip_prefix("@composer ") {
                    let mut exec = if rest.contains("@additional_args") {
                        rest.replace("@additional_args", &escaped_args)
                    } else if escaped_args.is_empty() {
                        rest.to_string()
                    } else {
                        format!("{rest} {escaped_args}")
                    };
                    exec = format!("{} {exec}", escape(&composer_binary()));
                    self.execute_checked(&exec, &callable, event)?;
                } else {
                    let mut script = split_script_args(&callable[1..]);
                    let name = if script.is_empty() {
                        String::new()
                    } else {
                        script.remove(0)
                    };
                    let has_additional = script.iter().any(|a| a == "@additional_args");
                    let mut inner_args = Vec::new();
                    let mut alias_args = Vec::new();
                    for arg in script {
                        if arg == "@additional_args" {
                            inner_args.extend(additional_args.iter().cloned());
                            alias_args.push(alias_input.unwrap_or("").to_string());
                        } else {
                            alias_args.push(escape(&arg));
                            inner_args.push(arg);
                        }
                    }
                    if !has_additional {
                        inner_args.extend(additional_args.iter().cloned());
                        alias_args.push(alias_input.unwrap_or("").to_string());
                    }
                    let inner_alias = alias_input.map(|_| alias_args.join(" "));
                    let inner = crate::scripts::listeners(self.root, &name);
                    if inner.is_empty() {
                        self.io.warning(&format!(
                            "You made a reference to a non-existent script {callable}"
                        ));
                    }
                    let result = self.dispatch(&name, inner, &inner_args, inner_alias.as_deref());
                    if result.as_ref().is_err_and(|e| e == FAILURE) {
                        self.io
                            .error(&format!("Script {callable} was called via {event}"));
                    }
                    result?;
                }
            } else if is_php_script(&callable) || is_command_class(&callable) {
                // ponytail: PHP callbacks never run here, so they are reported the way Composer
                // reports a class it cannot autoload
                let class = callable.split("::").next().unwrap_or(&callable);
                self.io.warning(&format!(
                    "Class {class} is not autoloadable, can not call {event} script"
                ));
            } else {
                if callable.is_empty() {
                    continue;
                }
                let mut exec = if callable.starts_with("@putenv ") {
                    callable.clone()
                } else if callable.contains("@additional_args") {
                    callable.replace("@additional_args", &escaped_args)
                } else if escaped_args.is_empty() {
                    callable.clone()
                } else {
                    format!("{callable} {escaped_args}")
                };
                if self.io.verbosity >= 1 {
                    self.io.error(&format!("> {event}: {exec}"));
                } else if event != "__exec_command" && alias_input.is_none() {
                    self.io.error(&format!("> {exec}"));
                }
                for local in &self.root.package.binaries {
                    if Regex::new(&format!(r"\b{}$", regex::escape(&callable)))
                        .unwrap()
                        .is_match(local)
                    {
                        let caller =
                            crate::binaries::BinaryInstaller::determine_binary_caller(local);
                        exec = Regex::new(&format!("^{}", regex::escape(&callable)))
                            .unwrap()
                            .replace(&exec, format!("{caller} {local}").as_str())
                            .into_owned();
                        break;
                    }
                }
                if let Some(assignment) = exec.strip_prefix("@putenv ") {
                    match assignment.split_once('=') {
                        Some((name, value)) => put_env(name, Some(value))?,
                        None => put_env(assignment, None)?,
                    }
                    continue;
                }
                if let Some(path_and_args) = exec.strip_prefix("@php ") {
                    let mut path_and_args = path_and_args.to_string();
                    // `@php foo` where foo is a binary in PATH rather than a path from CWD
                    if let Some(m) = PHP_PATH_NAME.find(&path_and_args)
                        && !Path::new(m.as_str()).exists()
                        && let Some(found) = find_in_path(m.as_str())
                    {
                        path_and_args = format!("{}{}", found.display(), &path_and_args[m.end()..]);
                    }
                    exec = format!("{} {path_and_args}", php_exec_command()?);
                } else if let Some(php) = php_binary() {
                    put_env("PHP_BINARY", Some(&php.to_string_lossy()))?;
                }
                // `composer ...` runs this very binary, not one from bin-dir
                if let Some(rest) = exec.strip_prefix("composer ") {
                    exec = format!("{} {rest}", escape(&composer_binary()));
                }
                self.execute_checked(&exec, &callable, event)?;
            }
        }
        Ok(())
    }

    fn execute_checked(&mut self, exec: &str, callable: &str, event: &str) -> Result<(), String> {
        let code = self.execute(exec)?;
        if code == 0 {
            return Ok(());
        }
        self.io.error(&format!(
            "Script {callable} handling the {event} event returned with error code {code}"
        ));
        FAILED_CODE.store(code, Ordering::Relaxed);
        Err(FAILURE.into())
    }

    /// `ensureBinDirIsInPath`: local binaries of dependencies are usable in scripts.
    fn ensure_bin_dir_in_path(&self) {
        let bin_dir = self.config.get_str("bin-dir");
        if !Path::new(&bin_dir).is_dir() {
            return;
        }
        let bin_dir = crate::fs::realpath(Path::new(&bin_dir));
        let path = get_env("PATH").unwrap_or_default();
        if !path.split(':').any(|p| p == bin_dir) {
            let _ = put_env("PATH", Some(&format!("{bin_dir}:{path}")));
        }
    }

    /// `ProcessExecutor::execute` through the shell, output inherited, with the process timeout.
    fn execute(&mut self, exec: &str) -> Result<i32, String> {
        let timeout = match TIMEOUT_OVERRIDE.load(Ordering::Relaxed) {
            t if t >= 0 => t as u64,
            _ => self.config.int("process-timeout").max(0) as u64,
        };
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(exec)
            .env("COMPOSER_BINARY", composer_binary());
        // ponytail: Composer routes child output through its IO, so -q silences it; we inherit
        // stdio and only mute it, never reformat it
        if self.io.quiet {
            command
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        for (name, value) in ENV.lock().unwrap().iter() {
            match value {
                Some(v) => command.env(name, v),
                None => command.env_remove(name),
            };
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Unable to run \"{exec}\": {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(timeout);
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok(status_code(status));
            }
            if timeout > 0 && Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "The following exception is caused by a process timeout\nCheck https://getcomposer.org/doc/06-config.md#process-timeout for details\n\nThe process \"{exec}\" exceeded the timeout of {timeout} seconds."
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn is_composer_script(callable: &str) -> bool {
    callable.starts_with('@') && !callable.starts_with("@php ") && !callable.starts_with("@putenv ")
}

fn is_php_script(callable: &str) -> bool {
    !callable.contains(' ') && callable.contains("::")
}

fn is_command_class(callable: &str) -> bool {
    callable.contains('\\') && !callable.contains(' ') && callable.ends_with("Command")
}

/// `splitScriptArgs`: whitespace-separated, quoted parts kept together and unquoted.
fn split_script_args(command: &str) -> Vec<String> {
    Regex::new(r#"(?:[^\s"']+|"[^"]*"|'[^']*'|["'])+"#)
        .unwrap()
        .find_iter(command)
        .map(|m| unquote(m.as_str()))
        .collect()
}

fn unquote(token: &str) -> String {
    let mut out = String::new();
    let mut rest = token;
    while let Some(start) = rest.find(['"', '\'']) {
        let quote = &rest[start..start + 1];
        out.push_str(&rest[..start]);
        match rest[start + 1..].find(quote) {
            Some(end) => {
                out.push_str(&rest[start + 1..start + 1 + end]);
                rest = &rest[start + 2 + end..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// `COMPOSER_BINARY`: this executable, for `@composer` and `composer ...` scripts.
fn composer_binary() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "romposer".into())
}

/// `getPhpExecCommand` without the ini flags Composer copies from its own process.
fn php_exec_command() -> Result<String, String> {
    php_binary()
        .map(|p| escape(&p.to_string_lossy()))
        .ok_or_else(|| "Failed to locate PHP binary to execute ".to_string())
}

/// The interpreter from the PATH romposer started with: never one from bin-dir, which a
/// package could ship to hijack `@php` (Composer uses its running `PHP_BINARY`).
fn php_binary() -> Option<PathBuf> {
    std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|dir| Path::new(dir).join("php"))
        .find(|p| p.is_file())
}

/// `ExecutableFinder::find` on PATH.
fn find_in_path(name: &str) -> Option<PathBuf> {
    get_env("PATH")?
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_args_split_like_composer() {
        assert_eq!(
            split_script_args("addargs 'one two' three \"a b\"c"),
            vec!["addargs", "one two", "three", "a bc"]
        );
        assert_eq!(
            split_script_args("x 'unterminated"),
            vec!["x", "'unterminated"]
        );
    }

    #[test]
    fn escape_quotes_for_sh() {
        assert_eq!(escape("it's"), "'it'\\''s'");
        assert_eq!(escape(""), "''");
    }

    #[test]
    fn putenv_rejects_bad_names() {
        assert!(put_env("", Some("x")).is_err());
        assert!(put_env("A=B", Some("x")).is_err());
    }
}
