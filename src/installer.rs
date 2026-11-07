//! Port of `Installer::run` / `doInstall` for an install from an existing lock,
//! plus the `InstallationManager` execution loop (download everything, then
//! install in order, binaries, installed.json). Zip dists go through the
//! pcomposer store by default; the plain Composer path is the fallback.

use crate::binaries::BinaryInstaller;
use crate::config::Config;
use crate::download::{Downloader, extract_zip};
use crate::fs::{ensure_dir, is_dir_empty, realpath, remove};
use crate::installed::InstalledRepository;
use crate::io::Io;
use crate::lock::Lock;
use crate::package::Package;
use crate::package::root::RootPackage;
use crate::store::{Manifest, Store, StoreConfig};
use crate::transaction::{Operation, calculate_operations};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const ERROR_GENERIC_FAILURE: i32 = 1;
pub const ERROR_DEPENDENCY_RESOLUTION_FAILED: i32 = 2;
pub const ERROR_NO_LOCK_FILE_FOR_PARTIAL_UPDATE: i32 = 3;
pub const ERROR_LOCK_FILE_INVALID: i32 = 4;
pub const ERROR_AUDIT_FAILED: i32 = 5;
pub const ERROR_PSR_AUTOLOAD_VIOLATION: i32 = 6;
pub const ERROR_TRANSPORT_EXCEPTION: i32 = 100;

static METADATA_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\._.*|\.DS_Store|__MACOSX|\.AppleDouble|\.fseventsd|\.Spotlight-V100|\.Trashes|(?i:thumbs\.db|desktop\.ini))$").unwrap()
});

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
    pub strict_psr_autoloader: bool,
    pub ignore_all_platform_reqs: bool,
    pub ignored_platform_reqs: Vec<String>,
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

/// The store side of a run: configuration, store, manifest and the one-per-run fallback warning.
struct StoreRun {
    config: StoreConfig,
    store: Store,
    manifest: Manifest,
    fallback_warned: bool,
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
            let options = crate::autoload::DumpOptions {
                optimize: self.options.optimize_autoloader || self.options.classmap_authoritative,
                authoritative: self.options.classmap_authoritative,
                apcu: self.options.apcu_autoloader,
                apcu_prefix: self.options.apcu_autoloader_prefix.clone(),
                dev_mode: Some(self.options.dev_mode),
                dry_run: false,
                strict_ambiguous: false,
                ignore_all_platform_reqs: self.options.ignore_all_platform_reqs,
                ignored_platform_reqs: self.options.ignored_platform_reqs.clone(),
            };
            let class_map = crate::autoload::dump(
                self.io,
                self.config,
                &local_repo,
                self.root,
                Some(self.lock),
                &options,
            )?;
            if self.options.strict_psr_autoloader && !class_map.psr_violations().is_empty() {
                return Ok(ERROR_PSR_AUTOLOAD_VIOLATION);
            }
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

    fn store_run(&self, vendor_dir: &str) -> Result<Option<StoreRun>, String> {
        let config =
            StoreConfig::from_root(&self.root.package.extra, &self.config.get_str("cache-dir"))?;
        if !config.enabled {
            return Ok(None);
        }
        let store = Store::new(&config.root);
        Ok(Some(StoreRun {
            config,
            store,
            manifest: Manifest::in_vendor_dir(vendor_dir),
            fallback_warned: false,
        }))
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

        let operations = calculate_operations(&local_repo.packages, &locked)?;
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

    /// `InstalledRepository::write` with `$dumpVersions`: installed.json, installed.php, InstalledVersions.php.
    fn write_local_repo(
        &self,
        local_repo: &InstalledRepository,
        vendor_dir: &str,
    ) -> Result<(), String> {
        local_repo.write(self.options.dev_mode, |p| install_path(vendor_dir, p))?;
        crate::autoload::write_installed_versions(
            &format!("{vendor_dir}/composer"),
            local_repo,
            self.root,
            self.options.dev_mode,
            |p| install_path(vendor_dir, p),
        )
    }

    /// `InstallationManager::execute`, with pcomposer's `StoreZipDownloader::install` for zip dists.
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
        let mut store_run = self.store_run(&vendor_dir)?;
        let mut downloader = Downloader::new(self.config)?;
        // no zip cache when the store is active: the store is the cache
        downloader.set_write_cache(store_run.is_none());
        let mut binaries = self.binary_installer();

        // execute all downloads first, skipping packages the store already holds
        let to_install: Vec<&Package> = operations
            .iter()
            .filter_map(|op| match op {
                Operation::Install(p) | Operation::Update { target: p, .. } => Some(p),
                _ => None,
            })
            .collect();
        let to_download: Vec<&Package> = to_install
            .iter()
            .copied()
            .filter(|p| {
                !goes_through_store(&store_run, &vendor_dir, p)
                    || !store_run
                        .as_ref()
                        .is_some_and(|run| run.store.has(&Store::key(p)))
            })
            .collect();
        let mut archives = downloader.download_all(self.io, &to_download)?;

        if self.options.download_only {
            for package in &to_install {
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
                    if !Path::new(&path).exists() && local_repo.has_package(package) {
                        binaries.remove_binaries(package)?;
                    }
                    self.install_code(
                        &mut store_run,
                        &mut downloader,
                        &mut archives,
                        package,
                        Path::new(&path),
                        &vendor_dir,
                        None,
                    )?;
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
                    binaries.remove_binaries(initial)?;
                    remove(Path::new(&initial_path))?;
                    self.install_code(
                        &mut store_run,
                        &mut downloader,
                        &mut archives,
                        target,
                        Path::new(&target_path),
                        &vendor_dir,
                        Some(initial),
                    )?;
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
                        remove(Path::new(&path))?;
                        binaries.remove_binaries(package)?;
                        if let Some(parent) = Path::new(&path).parent()
                            && package.name.contains('/')
                            && parent.is_dir()
                            && is_dir_empty(parent)
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
            self.write_local_repo(local_repo, &vendor_dir)?;
        }
        // do a last write so that we write the repository even if nothing changed
        self.write_local_repo(local_repo, &vendor_dir)?;
        if let Some(run) = &store_run {
            let installed: Vec<Package> = local_repo
                .canonical_packages()
                .into_iter()
                .cloned()
                .collect();
            run.manifest.write(&installed)?;
        }
        Ok(())
    }

    /// `ArchiveDownloader::install` or `StoreZipDownloader::install` for one package.
    #[allow(clippy::too_many_arguments)]
    fn install_code(
        &mut self,
        store_run: &mut Option<StoreRun>,
        downloader: &mut Downloader,
        archives: &mut std::collections::HashMap<String, crate::download::Downloaded>,
        package: &Package,
        path: &Path,
        vendor_dir: &str,
        initial: Option<&Package>,
    ) -> Result<(), String> {
        let label = match initial {
            Some(i) => {
                let action = if crate::semver::is_upgrade(&i.version, &package.version) {
                    "Upgrading"
                } else {
                    "Downgrading"
                };
                format!(
                    "{action} {} ({} => {})",
                    i.pretty_name,
                    i.full_pretty_version(),
                    package.full_pretty_version()
                )
            }
            None => format!(
                "Installing {} ({})",
                package.pretty_name,
                package.full_pretty_version()
            ),
        };
        if !goes_through_store(store_run, vendor_dir, package) {
            self.io.error(&format!("  - {label}: Extracting archive"));
            let archive = archives
                .remove(&package.unique_name())
                .ok_or("missing download")?
                .archive;
            return downloader.install_archive(package, &archive, path);
        }
        let run = store_run.as_mut().unwrap();
        let key = Store::key(package);
        let cached = run.store.has(&key);
        self.io.error(&format!(
            "  - {label}: Linking from store{}",
            if cached { " (cached)" } else { "" }
        ));
        crate::fs::empty_directory(path)?;
        if !cached {
            let archive = archives
                .remove(&package.unique_name())
                .ok_or("missing download")?
                .archive;
            let parent = path.parent().unwrap_or(Path::new(vendor_dir)).to_path_buf();
            ensure_dir(&parent)?;
            let mut temporary_dir;
            loop {
                temporary_dir = parent.join(format!(".pcomposer-{}", crate::fs::random_hex(4)));
                if !temporary_dir.is_dir() {
                    break;
                }
            }
            ensure_dir(&temporary_dir)?;
            let result = extract_zip(&archive, &temporary_dir).and_then(|()| {
                let _ = remove(&archive);
                run.store.ingest(&key, &package_root(&temporary_dir))
            });
            let _ = remove(&temporary_dir);
            if let Err(e) = result {
                let _ = remove(path);
                return Err(e);
            }
        }
        let wanted = run.config.strategy_for(&package.name, &package.kind);
        let used = run
            .store
            .materialise(&key, path, wanted, &package.binaries)?;
        if !run.fallback_warned
            && let Some(reason) = run.store.fallback_reason()
        {
            run.fallback_warned = true;
            self.io.error(&format!(
                "pcomposer: {wanted} failed ({reason}), falling back to copy for this run"
            ));
        }
        run.manifest.record(package, used.as_str());
        Ok(())
    }
}

/// pcomposer's boundary: zip dist, store enabled, target empty or missing.
fn goes_through_store(store_run: &Option<StoreRun>, vendor_dir: &str, package: &Package) -> bool {
    if store_run.is_none() {
        return false;
    }
    if package.dist.as_ref().map(|d| d.kind.as_str()) != Some("zip") {
        return false;
    }
    match install_path(vendor_dir, package) {
        Some(path) => !(Path::new(&path).is_dir() && !is_dir_empty(Path::new(&path))),
        None => false,
    }
}

/// Unwraps a single top-level directory (GitHub zipballs), ignoring OS metadata siblings.
fn package_root(extracted: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = std::fs::read_dir(extracted)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    !p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| METADATA_NAME.is_match(n))
                })
                .collect()
        })
        .unwrap_or_default();
    if entries.len() == 1 && entries[0].is_dir() {
        return entries[0].clone();
    }
    extracted.to_path_buf()
}
