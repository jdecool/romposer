//! Port of `BinaryInstaller`: the `vendor/bin` proxies, byte for byte.

use crate::fs::{
    ensure_dir, find_shortest_path, find_shortest_path_code, is_dir_empty, normalize_path, realpath,
};
use crate::io::Io;
use crate::package::Package;
use regex::Regex;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static UNSAFE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[*$`"&^|<>()%!;\x00-\x1f\x7f]"#).unwrap());
static SHEBANG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^#!(?:/[a-zA-Z0-9_.+-]+)*/([a-zA-Z0-9_.+][a-zA-Z0-9_.+-]*(?:[ \t].*)?)$").unwrap()
});
static ENV_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^env[ \t]+(?:-S[ \t]*)?").unwrap());
static PHP_CALLER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^php[0-9.]*(?:[ \t]|$)").unwrap());
static PHP_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#!.*\r?\n)?[\r\n\t ]*<\?php").unwrap());

pub struct BinaryInstaller {
    pub bin_dir: String,
    pub bin_compat: String,
    pub vendor_dir: String,
}

/// The process umask, read by setting and restoring it.
pub fn umask() -> u32 {
    // SAFETY: plain libc calls; the value is restored immediately.
    unsafe {
        let old = libc::umask(0o022);
        libc::umask(old);
        u32::from(old)
    }
}

fn chmod_executable(path: &Path) {
    let mode = 0o777 & !umask();
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

fn escape_shell_arg(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn php_dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => ".",
    }
}

fn php_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl BinaryInstaller {
    pub fn is_safe_bin_path(bin: &str) -> bool {
        !UNSAFE.is_match(bin)
    }

    fn shebang_caller(shebang: &str) -> Option<String> {
        let m = SHEBANG.captures(shebang)?;
        let cmd = &m[1];
        if UNSAFE.is_match(cmd) {
            return None;
        }
        let caller = ENV_PREFIX.replace(cmd, "").into_owned();
        if caller.is_empty() || caller == "env" {
            return None;
        }
        Some(if caller.starts_with('-') {
            format!("env {caller}")
        } else {
            caller
        })
    }

    fn is_php_interpreter(caller: &str) -> bool {
        PHP_CALLER.is_match(caller)
    }

    /// `BinaryInstaller::installBinaries`
    pub fn install_binaries(
        &mut self,
        io: &mut Io,
        package: &Package,
        install_path: &Path,
        warn_on_overwrite: bool,
    ) -> Result<(), String> {
        if package.binaries.is_empty() {
            return Ok(());
        }
        for bin in &package.binaries {
            if !Self::is_safe_bin_path(bin) {
                io.error(&format!("    Skipped installation of bin {bin} for package {}: the bin path contains invalid characters", package.name));
                continue;
            }
            let bin_path = install_path.join(bin);
            if !bin_path.exists() {
                io.error(&format!("    Skipped installation of bin {bin} for package {}: file not found in package", package.name));
                continue;
            }
            if bin_path.is_dir() {
                io.error(&format!("    Skipped installation of bin {bin} for package {}: found a directory at that path", package.name));
                continue;
            }
            let real_bin = std::fs::canonicalize(&bin_path).ok();
            let real_install = std::fs::canonicalize(install_path).ok();
            let inside = match (&real_bin, &real_install) {
                (Some(b), Some(i)) => b.starts_with(i) && b != i,
                _ => false,
            };
            if !inside {
                io.error(&format!("    Skipped installation of bin {bin} for package {}: the bin resolves to a path outside of the package directory", package.name));
                continue;
            }
            let bin_path = real_bin.unwrap().to_string_lossy().into_owned();
            ensure_dir(Path::new(&self.bin_dir))?;
            self.bin_dir = realpath(Path::new(&self.bin_dir));
            let link = format!("{}/{}", self.bin_dir, php_basename(bin));
            let link_path = PathBuf::from(&link);
            if let Ok(meta) = std::fs::symlink_metadata(&link_path) {
                if !meta.is_symlink() {
                    if warn_on_overwrite {
                        io.error(&format!("    Skipped installation of bin {bin} for package {}: name conflicts with an existing file", package.name));
                    }
                    continue;
                }
                if std::fs::canonicalize(&link_path)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    == Some(bin_path.clone())
                {
                    let _ = std::fs::remove_file(&link_path);
                }
            }
            if self.bin_compat == "full" {
                if bin_path.contains(['"', '\r', '\n', '\u{1a}'])
                    || self.bin_dir.contains(['"', '\r', '\n', '\u{1a}'])
                {
                    io.error(&format!("    Skipped installation of bin {bin} for package {}: the path contains a double quote or line break, which cannot be used in a Windows bin proxy", package.name));
                    continue;
                }
                self.install_full_binaries(io, &bin_path, &link, bin, package)?;
            } else {
                self.install_unixy_proxy(&bin_path, &link)?;
            }
            chmod_executable(Path::new(&bin_path));
        }
        Ok(())
    }

    /// `BinaryInstaller::removeBinaries`
    pub fn remove_binaries(&mut self, package: &Package) -> Result<(), String> {
        ensure_dir(Path::new(&self.bin_dir))?;
        self.bin_dir = realpath(Path::new(&self.bin_dir));
        if package.binaries.is_empty() {
            return Ok(());
        }
        for bin in &package.binaries {
            let link = PathBuf::from(format!("{}/{}", self.bin_dir, php_basename(bin)));
            if std::fs::symlink_metadata(&link).is_ok() {
                std::fs::remove_file(&link).map_err(|e| e.to_string())?;
            }
            let bat = PathBuf::from(format!("{}.bat", link.display()));
            if bat.is_file() {
                std::fs::remove_file(&bat).map_err(|e| e.to_string())?;
            }
        }
        let dir = Path::new(&self.bin_dir);
        if dir.is_dir() && is_dir_empty(dir) {
            let _ = std::fs::remove_dir(dir);
        }
        Ok(())
    }

    fn install_full_binaries(
        &self,
        io: &mut Io,
        bin_path: &str,
        link: &str,
        bin: &str,
        package: &Package,
    ) -> Result<(), String> {
        let mut link = link.to_string();
        if !bin_path.ends_with(".bat") {
            self.install_unixy_proxy(bin_path, &link)?;
            link.push_str(".bat");
            if Path::new(&link).exists() {
                io.error(&format!("    Skipped installation of bin {bin}.bat proxy for package {}: a .bat proxy was already installed", package.name));
            }
        }
        if !Path::new(&link).exists() {
            std::fs::write(&link, self.windows_proxy_code(bin_path, &link))
                .map_err(|e| e.to_string())?;
            chmod_executable(Path::new(&link));
        }
        Ok(())
    }

    fn install_unixy_proxy(&self, bin_path: &str, link: &str) -> Result<(), String> {
        std::fs::write(link, self.unixy_proxy_code(bin_path, link))
            .map_err(|e| format!("Could not write {link}: {e}"))?;
        chmod_executable(Path::new(link));
        Ok(())
    }

    pub fn determine_binary_caller(bin: &str) -> String {
        if bin.ends_with(".bat") || bin.ends_with(".exe") {
            return "call".into();
        }
        let first_line = std::fs::read(bin)
            .ok()
            .map(|b| {
                b.split(|c| *c == b'\n')
                    .next()
                    .map(|l| String::from_utf8_lossy(l).into_owned())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let shebang = first_line.trim_end_matches([' ', '\t', '\r', '\n']);
        Self::shebang_caller(shebang).unwrap_or_else(|| "php".into())
    }

    fn windows_proxy_code(&self, bin: &str, link: &str) -> String {
        let caller = Self::determine_binary_caller(bin);
        let target = if Self::is_php_interpreter(&caller) {
            php_basename(link).trim_end_matches(".bat").to_string()
        } else {
            find_shortest_path(link, bin, false, false)
        };
        format!(
            "@ECHO OFF\r\nsetlocal DISABLEDELAYEDEXPANSION\r\nSET \"BIN_TARGET=%~dp0/{}\"\r\nSET \"COMPOSER_RUNTIME_BIN_DIR=%~dp0\"\r\n{caller} \"%BIN_TARGET%\" %*\r\n",
            target.replace('%', "%%")
        )
    }

    /// `BinaryInstaller::generateUnixyProxyCode`
    pub fn unixy_proxy_code(&self, bin: &str, link: &str) -> String {
        let bin_path = find_shortest_path(link, bin, false, false);
        let bin_dir = escape_shell_arg(php_dirname(&bin_path));
        let bin_file = escape_shell_arg(php_basename(&bin_path));
        let head: Vec<u8> = std::fs::read(bin)
            .map(|b| b.into_iter().take(500).collect())
            .unwrap_or_default();
        let bin_contents = String::from_utf8_lossy(&head).into_owned();
        if let Some(m) = PHP_FILE.captures(&bin_contents) {
            let shebang = m
                .get(1)
                .map_or("", |s| s.as_str())
                .trim_end_matches([' ', '\t', '\r', '\n'])
                .to_string();
            let caller = Self::shebang_caller(&shebang);
            let proxy_code = if caller.as_deref().is_some_and(Self::is_php_interpreter) {
                shebang
            } else {
                "#!/usr/bin/env php".into()
            };
            let bin_path_exported = find_shortest_path_code(link, bin, false, true, false);
            let bin_path_comment = bin_path.replace(['\r', '\n'], " ").replace("*/", "* /");
            let mut globals_code = "$GLOBALS['_composer_bin_dir'] = __DIR__;\n".to_string();
            let vendor_real = std::fs::canonicalize(&self.vendor_dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| self.vendor_dir.clone());
            globals_code.push_str(&format!(
                "$GLOBALS['_composer_autoload_path'] = {};\n",
                find_shortest_path_code(
                    link,
                    &format!("{vendor_real}/autoload.php"),
                    false,
                    true,
                    false
                )
            ));
            let mut phpunit_hack1 = "";
            let mut phpunit_hack2 = "";
            if normalize_path(bin)
                == normalize_path(&format!("{}/phpunit/phpunit/phpunit", self.vendor_dir))
            {
                globals_code.push_str(&format!(
                    "$GLOBALS['__PHPUNIT_ISOLATION_EXCLUDE_LIST'] = $GLOBALS['__PHPUNIT_ISOLATION_BLACKLIST'] = array(realpath({bin_path_exported}));\n"
                ));
                phpunit_hack1 = "'phpvfscomposer://'.";
                phpunit_hack2 = "\n                $data = str_replace('__DIR__', var_export(dirname($this->realpath), true), $data);\n                $data = str_replace('__FILE__', var_export($this->realpath, true), $data);";
            }
            let mut stream_hint = String::new();
            let mut stream_proxy_code = String::new();
            if m[0].trim() != "<?php" {
                stream_hint =
                    " using a stream wrapper to prevent the shebang from being output on PHP<8\n *"
                        .into();
                stream_proxy_code = STREAM_PROXY
                    .replace("{phpunitHack1}", phpunit_hack1)
                    .replace("{phpunitHack2}", phpunit_hack2)
                    .replace("{binPathExported}", &bin_path_exported);
            }
            return format!(
                "{proxy_code}\n<?php\n\n/**\n * Proxy PHP file generated by Composer\n *\n * This file includes the referenced bin path ({bin_path_comment})\n *{stream_hint}\n * @generated\n */\n\nnamespace Composer;\n\n{globals_code}\n{stream_proxy_code}\nreturn include {bin_path_exported};\n"
            );
        }
        SH_PROXY
            .replace("{binDir}", &bin_dir)
            .replace("{binFile}", &bin_file)
    }
}

const STREAM_PROXY: &str = r#"if (PHP_VERSION_ID < 80000) {
    if (!class_exists('Composer\BinProxyWrapper')) {
        /**
         * @internal
         */
        final class BinProxyWrapper
        {
            private $handle;
            private $position;
            private $realpath;

            public function stream_open($path, $mode, $options, &$opened_path)
            {
                // get rid of phpvfscomposer:// prefix for __FILE__ & __DIR__ resolution
                $opened_path = substr($path, 17);
                $this->realpath = realpath($opened_path) ?: $opened_path;
                $opened_path = {phpunitHack1}$this->realpath;
                $this->handle = fopen($this->realpath, $mode);
                $this->position = 0;

                return (bool) $this->handle;
            }

            public function stream_read($count)
            {
                $data = fread($this->handle, $count);

                if ($this->position === 0) {
                    $data = preg_replace('{^#!.*\r?\n}', '', $data);
                }{phpunitHack2}

                $this->position += strlen($data);

                return $data;
            }

            public function stream_cast($castAs)
            {
                return $this->handle;
            }

            public function stream_close()
            {
                fclose($this->handle);
            }

            public function stream_lock($operation)
            {
                return $operation ? flock($this->handle, $operation) : true;
            }

            public function stream_seek($offset, $whence)
            {
                if (0 === fseek($this->handle, $offset, $whence)) {
                    $this->position = ftell($this->handle);
                    return true;
                }

                return false;
            }

            public function stream_tell()
            {
                return $this->position;
            }

            public function stream_eof()
            {
                return feof($this->handle);
            }

            public function stream_stat()
            {
                return array();
            }

            public function stream_set_option($option, $arg1, $arg2)
            {
                return true;
            }

            public function url_stat($path, $flags)
            {
                $path = substr($path, 17);
                if (file_exists($path)) {
                    return stat($path);
                }

                return false;
            }
        }
    }

    if (
        (function_exists('stream_get_wrappers') && in_array('phpvfscomposer', stream_get_wrappers(), true))
        || (function_exists('stream_wrapper_register') && stream_wrapper_register('phpvfscomposer', 'Composer\BinProxyWrapper'))
    ) {
        return include("phpvfscomposer://" . {binPathExported});
    }
}
"#;

const SH_PROXY: &str = r#"#!/usr/bin/env sh

# Support bash to support `source` with fallback on $0 if this does not run with bash
# https://stackoverflow.com/a/35006505/6512
selfArg="$BASH_SOURCE"
if [ -z "$selfArg" ]; then
    selfArg="$0"
fi

self=$(realpath "$selfArg" 2> /dev/null)
if [ -z "$self" ]; then
    self="$selfArg"
fi

dir=$(cd "${self%[/\\]*}" > /dev/null; cd {binDir} && pwd)

if [ -d /proc/cygdrive ]; then
    case $(which php) in
        $(readlink -n /proc/cygdrive)/*)
            # We are in Cygwin using Windows php, so the path must be translated
            dir=$(cygpath -m "$dir");
            ;;
    esac
fi

export COMPOSER_RUNTIME_BIN_DIR="$(cd "${self%[/\\]*}" > /dev/null; pwd)"

# If bash is sourcing this file, we have to source the target as well
bashSource="$BASH_SOURCE"
if [ -n "$bashSource" ]; then
    if [ "$bashSource" != "$0" ]; then
        source "${dir}/"{binFile} "$@"
        return
    fi
fi

exec "${dir}/"{binFile} "$@"
"#;
