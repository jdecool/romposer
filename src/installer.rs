//! Port of `Installer::run` / `doInstall` for an install from an existing lock,
//! plus the `InstallationManager` execution loop (download everything, then
//! install in order, binaries, installed.json).

use crate::binaries::BinaryInstaller;
use crate::config::Config;
use crate::download::Downloader;
use crate::fs::{ensure_dir, realpath};
use crate::installed::InstalledRepository;
use crate::io::Io;
use crate::lock::Lock;
use crate::package::Package;
use crate::package::root::RootPackage;
use crate::transaction::{Operation, calculate_operations};
use std::path::{Path, PathBuf};

pub const ERROR_GENERIC_FAILURE: i32 = 1;
pub const ERROR_DEPENDENCY_RESOLUTION_FAILED: i32 = 2;
pub const ERROR_NO_LOCK_FILE_FOR_PARTIAL_UPDATE: i32 = 3;
pub const ERROR_LOCK_FILE_INVALID: i32 = 4;
pub const ERROR_AUDIT_FAILED: i32 = 5;
pub const ERROR_PSR_AUTOLOAD_VIOLATION: i32 = 6;
pub const ERROR_TRANSPORT_EXCEPTION: i32 = 100;

#[derive(Clone, Debug)]
pub struct Options {
    pub dev_mode: bool,
    pub dry_run: bool,
    pub download_only: bool,
    pub prefer_source: bool,
    pub prefer_dist: bool,
    pub dump_autoloader: bool,
    pub optimize_autoloader: bool,
    pub classmap_authoritative: bool,
    pub apcu_autoloader: bool,
    pub apcu_autoloader_prefix: Option<String>,
    pub run_scripts: bool,
    pub verbose: bool,
}

pub struct Installer<'a> {
    pub io: &'a mut Io,
    pub config: &'a Config,
    pub root: &'a RootPackage,
    pub lock: &'a Lock,
    pub composer_json: &'a str,
    pub options: Options,
}

/// `LibraryInstaller::getInstallPath`
pub fn install_path(vendor_dir: &str, package: &Package) -> Option<String> {
    if package.kind == "metapackage" {
        return None;
    }
    let base = format!("{vendor_dir}/{}", package.pretty_name);
    Some(match &package.target_dir {
        Some(target) if !target.is_empty() => format!("{base}/{target}"),
        _ => base,
    })
}

fn unsupported(package: &Package) -> Option<String> {
    let dist_type = package.dist.as_ref().map(|d| d.kind.as_str());
    if package.kind == "metapackage" {
        return Some(format!(
            "{}: metapackages are not yet supported",
            package.pretty_name
        ));
    }
    match dist_type {
        Some("zip") => None,
        Some(other) => Some(format!(
            "{}: dist type \"{other}\" is not yet supported",
            package.pretty_name
        )),
        None => Some(format!(
            "{}: source-only packages are not yet supported",
            package.pretty_name
        )),
    }
}

impl Installer<'_> {
    /// `Installer::run` when a lock file exists.
    pub fn run(&mut self) -> Result<i32, String> {
        let vendor_dir = self.config.vendor_dir();
        let installed_path = PathBuf::from(format!("{vendor_dir}/composer/installed.json"));
        let mut local_repo = InstalledRepository::read(&installed_path)?;

        let code = self.do_install(&mut local_repo, &vendor_dir)?;
        if code != 0 {
            return Ok(code);
        }

        // Find abandoned packages and warn user
        for package in self.lock.locked_packages(true)? {
            if package.alias_of.is_some() || !package.is_abandoned() {
                continue;
            }
            let replacement = match package.replacement_package() {
                Some(r) => format!("Use {r} instead"),
                None => "No replacement was suggested".into(),
            };
            self.io.error(&format!(
                "Package {} is abandoned, you should avoid using it. {replacement}.",
                package.pretty_name
            ));
        }

        if self.options.dump_autoloader {
            if self.options.optimize_autoloader {
                self.io.error("Generating optimized autoload files");
            } else {
                self.io.error("Generating autoload files");
            }
            crate::autoload::dump(
                self.io,
                self.config,
                &local_repo,
                self.root,
                self.options.optimize_autoloader || self.options.classmap_authoritative,
                self.options.classmap_authoritative,
                self.options.apcu_autoloader,
                self.options.apcu_autoloader_prefix.as_deref(),
                self.options.dev_mode,
            )?;
        }

        if !self.options.dry_run && !self.options.download_only {
            let mut binaries = self.binary_installer();
            for package in local_repo.canonical_packages() {
                if let Some(path) = install_path(&vendor_dir, package) {
                    binaries.install_binaries(self.io, package, Path::new(&path), false)?;
                }
            }
        }

        let funding = local_repo
            .canonical_packages()
            .iter()
            .filter(|p| !p.funding.is_empty())
            .count();
        let show_funding = std::env::var("COMPOSER_FUND")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .is_none_or(|v| v != 0);
        if show_funding && funding > 0 {
            self.io.error(&format!(
                "{funding} package{} you are using {} looking for funding.",
                if funding == 1 { "" } else { "s" },
                if funding == 1 { "is" } else { "are" }
            ));
            self.io
                .error("Use the `composer fund` command to find out more!");
        }
        Ok(0)
    }

    fn binary_installer(&self) -> BinaryInstaller {
        BinaryInstaller {
            bin_dir: self.config.get_str("bin-dir"),
            bin_compat: self.config.get_str("bin-compat"),
            vendor_dir: self.config.vendor_dir(),
        }
    }

    /// `Installer::doInstall`
    fn do_install(
        &mut self,
        local_repo: &mut InstalledRepository,
        vendor_dir: &str,
    ) -> Result<i32, String> {
        if self.lock.data.get("content-hash").and_then(|v| v.as_str())
            == Some(crate::lock::CONFLICTED_CONTENT_HASH)
        {
            self.io.error(crate::lock::CONFLICTED_CONTENT_HASH);
            return Ok(ERROR_LOCK_FILE_INVALID);
        }
        if self.config.bool("lock") {
            self.io.error(&format!(
                "Installing dependencies from lock file{}",
                if self.options.dev_mode {
                    " (including require-dev)"
                } else {
                    ""
                }
            ));
        }
        let locked = self.lock.locked_packages(self.options.dev_mode)?;

        self.io
            .error("Verifying lock file contents can be installed on current platform.");
        if !self.lock.is_fresh(self.composer_json) {
            self.io.error("Warning: The lock file is not up to date with the latest changes in composer.json. You may be getting outdated dependencies. It is recommended that you run `composer update` or `composer update <package name>`.");
        }
        let missing = self
            .lock
            .missing_requirement_info(self.root, self.options.dev_mode)?;
        if !missing.is_empty() {
            for line in &missing {
                self.io.error(line);
            }
            if !self.config.bool("allow-missing-requirements") {
                return Ok(ERROR_LOCK_FILE_INVALID);
            }
        }
        // ponytail: no platform/solver verification of the lock yet (tickets 07 and 10)

        let operations = calculate_operations(&local_repo.packages, &locked);
        let mut installs = Vec::new();
        let mut updates = Vec::new();
        let mut uninstalls = Vec::new();
        for op in &operations {
            match op {
                Operation::Install(p) => {
                    installs.push(format!("{}:{}", p.pretty_name, p.full_pretty_version()))
                }
                Operation::Update { target, .. } => updates.push(format!(
                    "{}:{}",
                    target.pretty_name,
                    target.full_pretty_version()
                )),
                Operation::Uninstall(p) => uninstalls.push(p.pretty_name.clone()),
                _ => {}
            }
        }
        if installs.is_empty() && updates.is_empty() && uninstalls.is_empty() {
            self.io.error("Nothing to install, update or remove");
        } else {
            let plural = |n: usize| if n == 1 { "" } else { "s" };
            self.io.error(&format!(
                "Package operations: {} install{}, {} update{}, {} removal{}",
                installs.len(),
                plural(installs.len()),
                updates.len(),
                plural(updates.len()),
                uninstalls.len(),
                plural(uninstalls.len())
            ));
            if self.options.verbose {
                if !installs.is_empty() {
                    self.io.error(&format!("Installs: {}", installs.join(", ")));
                }
                if !updates.is_empty() {
                    self.io.error(&format!("Updates: {}", updates.join(", ")));
                }
                if !uninstalls.is_empty() {
                    self.io
                        .error(&format!("Removals: {}", uninstalls.join(", ")));
                }
            }
        }

        if self.options.dry_run {
            for op in &operations {
                if !op.kind().contains("Alias") || self.io.verbosity >= 3 {
                    self.io.error(&format!("  - {}", op.show()));
                }
            }
            return Ok(0);
        }

        local_repo.dev_package_names = self.lock.dev_package_names();
        self.execute(local_repo, &operations, vendor_dir)?;
        if !operations.is_empty() {
            let vendor = Path::new(vendor_dir);
            if vendor.is_dir() {
                let now = std::time::SystemTime::now();
                let _ = std::fs::File::open(vendor).and_then(|f| f.set_modified(now));
            }
        }
        Ok(0)
    }

    /// `InstallationManager::execute`
    fn execute(
        &mut self,
        local_repo: &mut InstalledRepository,
        operations: &[Operation],
        vendor_dir: &str,
    ) -> Result<(), String> {
        for op in operations {
            if let Operation::Install(p) | Operation::Update { target: p, .. } = op
                && let Some(reason) = unsupported(p)
            {
                return Err(format!(
                    "{reason} (romposer slice 1 installs zip dists only)"
                ));
            }
        }

        ensure_dir(Path::new(vendor_dir))?;
        let vendor_dir = realpath(Path::new(vendor_dir));
        let mut downloader = Downloader::new(self.config)?;
        let mut binaries = self.binary_installer();

        // execute all downloads first
        let to_download: Vec<&Package> = operations
            .iter()
            .filter_map(|op| match op {
                Operation::Install(p) | Operation::Update { target: p, .. } => Some(p),
                _ => None,
            })
            .collect();
        let mut archives = downloader.download_all(self.io, &to_download)?;

        if self.options.download_only {
            for package in &to_download {
                downloader.cleanup(
                    package,
                    Path::new(&install_path(&vendor_dir, package).unwrap_or_default()),
                );
            }
            return Ok(());
        }

        for op in operations {
            match op {
                Operation::Install(package) => {
                    let path = install_path(&vendor_dir, package).unwrap();
                    self.io.error(&format!(
                        "  - Installing {} ({}): Extracting archive",
                        package.pretty_name,
                        package.full_pretty_version()
                    ));
                    if !Path::new(&path).exists() && local_repo.has_package(package) {
                        binaries.remove_binaries(package)?;
                    }
                    let archive = archives
                        .remove(&package.unique_name())
                        .ok_or("missing download")?
                        .archive;
                    downloader.install_archive(package, &archive, Path::new(&path))?;
                    binaries.install_binaries(self.io, package, Path::new(&path), true)?;
                    if !local_repo.has_package(package) {
                        let mut installed = package.clone();
                        installed.installation_source = Some("dist".into());
                        local_repo.add_package(installed);
                    }
                    downloader.cleanup(package, Path::new(&path));
                }
                Operation::Update { initial, target } => {
                    let initial_path = install_path(&vendor_dir, initial).unwrap();
                    let target_path = install_path(&vendor_dir, target).unwrap();
                    let action = if crate::semver::is_upgrade(&initial.version, &target.version) {
                        "Upgrading"
                    } else {
                        "Downgrading"
                    };
                    self.io.error(&format!(
                        "  - {action} {} ({} => {}): Extracting archive",
                        initial.pretty_name,
                        initial.full_pretty_version(),
                        target.full_pretty_version()
                    ));
                    binaries.remove_binaries(initial)?;
                    crate::fs::remove(Path::new(&initial_path))?;
                    let archive = archives
                        .remove(&target.unique_name())
                        .ok_or("missing download")?
                        .archive;
                    downloader.install_archive(target, &archive, Path::new(&target_path))?;
                    binaries.install_binaries(self.io, target, Path::new(&target_path), true)?;
                    local_repo.remove_package(initial);
                    if !local_repo.has_package(target) {
                        let mut installed = target.clone();
                        installed.installation_source = Some("dist".into());
                        local_repo.add_package(installed);
                    }
                    downloader.cleanup(target, Path::new(&target_path));
                }
                Operation::Uninstall(package) => {
                    self.io.error(&format!(
                        "  - Removing {} ({})",
                        package.pretty_name,
                        package.full_pretty_version()
                    ));
                    if let Some(path) = install_path(&vendor_dir, package) {
                        crate::fs::remove(Path::new(&path))?;
                        binaries.remove_binaries(package)?;
                        if let Some(parent) = Path::new(&path).parent()
                            && package.name.contains('/')
                            && parent.is_dir()
                            && crate::fs::is_dir_empty(parent)
                        {
                            let _ = std::fs::remove_dir(parent);
                        }
                    }
                    local_repo.remove_package(package);
                }
                Operation::MarkAliasInstalled(package) => {
                    if !local_repo.has_package(package) {
                        local_repo.add_package(package.clone());
                    }
                }
                Operation::MarkAliasUninstalled(package) => local_repo.remove_package(package),
            }
            let dev = self.options.dev_mode;
            local_repo.write(dev, |p| install_path(&vendor_dir, p))?;
        }
        // do a last write so that we write the repository even if nothing changed
        local_repo.write(self.options.dev_mode, |p| install_path(&vendor_dir, p))?;
        Ok(())
    }
}
