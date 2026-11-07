//! Port of `Installer::run` / `doInstall` for an install from an existing lock,
//! plus the `InstallationManager` execution loop (download everything, then
//! install in order, binaries, installed.json). Zip dists go through the
//! pcomposer store by default; other archives, path packages, metapackages and
//! source checkouts take the plain Composer path.

use crate::binaries::BinaryInstaller;
use crate::config::Config;
use crate::download::{Downloader, extract_zip_hashed, ingest_zip};
use crate::fs::{ensure_dir, is_dir_empty, realpath, remove};
use crate::installed::InstalledRepository;
use crate::io::Io;
use crate::json::Value;
use crate::lock::Lock;
use crate::package::Package;
use crate::package::root::AliasDef;
use crate::package::root::RootPackage;
use crate::platform::{PhpInfo, PlatformRepository, RequirementFilter};
use crate::repository::ComposerRepository;
use crate::semver::{Constraint, Stability, parse_stability};
use crate::solver::builder::{ArrayRepo, Repository, RepositorySet};
use crate::solver::lock_transaction::LockTransaction;
use crate::solver::policy::DefaultPolicy;
use crate::solver::problem::Ctx;
use crate::solver::sat::{Solver, SolverProblems};
use crate::solver::{self, Pool, Request};
use crate::store::{Manifest, Store, StoreConfig};
use crate::transaction::{Operation, calculate_operations};
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const ERROR_GENERIC_FAILURE: i32 = 1;
pub const ERROR_DEPENDENCY_RESOLUTION_FAILED: i32 = 2;
pub const ERROR_NO_LOCK_FILE_FOR_PARTIAL_UPDATE: i32 = 3;
pub const ERROR_LOCK_FILE_INVALID: i32 = 4;
pub const ERROR_AUDIT_FAILED: i32 = 5;
pub const ERROR_PSR_AUTOLOAD_VIOLATION: i32 = 6;
pub const ERROR_TRANSPORT_EXCEPTION: i32 = 100;

/// `RequireCommand`'s `pre-operations-exec` listener: past it, a failure keeps composer.json.
pub static RESOLUTION_COMPLETED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
    pub update: bool,
    pub install: bool,
    pub update_mirrors: bool,
    pub update_allow_list: Option<Vec<String>>,
    pub update_allow_transitive: u8,
    pub prefer_stable: bool,
    pub prefer_lowest: bool,
    pub minimal_update: bool,
    pub temporary_constraints: Vec<(String, Constraint, String)>,
    pub write_lock: bool,
    pub execute_operations: bool,
    pub audit: bool,
    pub audit_format: String,
    /// `install --audit`: advisories fail the command
    pub error_on_audit: bool,
}

impl Options {
    /// `Installer::create` defaults: an install with everything on.
    pub fn defaults() -> Options {
        Options {
            dev_mode: true,
            dry_run: false,
            download_only: false,
            prefer_source: false,
            prefer_dist: false,
            dump_autoloader: true,
            optimize_autoloader: false,
            classmap_authoritative: false,
            apcu_autoloader: false,
            apcu_autoloader_prefix: None,
            run_scripts: true,
            verbose: false,
            strict_psr_autoloader: false,
            ignore_all_platform_reqs: false,
            ignored_platform_reqs: vec![],
            update: false,
            install: true,
            update_mirrors: false,
            update_allow_list: None,
            audit: true,
            audit_format: "summary".into(),
            error_on_audit: false,
            update_allow_transitive: solver::UPDATE_ONLY_LISTED,
            prefer_stable: false,
            prefer_lowest: false,
            minimal_update: false,
            temporary_constraints: vec![],
            write_lock: true,
            execute_operations: true,
        }
    }
}

/// Runs the operations of a transaction: on the filesystem, or as a trace (fixtures).
pub trait Executor {
    fn execute(
        &mut self,
        io: &mut Io,
        config: &Config,
        root: &RootPackage,
        local_repo: &mut InstalledRepository,
        operations: &[Operation],
        options: &Options,
    ) -> Result<(), String>;
    /// Whether vendor/ exists for this executor (autoload dump, binaries).
    fn writes_files(&self) -> bool {
        true
    }
}

pub struct Installer<'a> {
    pub io: &'a mut Io,
    pub config: &'a Config,
    pub root: &'a RootPackage,
    pub lock: Option<Lock>,
    /// where the lock is written; `None` keeps it in `written_lock` only (fixtures)
    pub lock_path: Option<PathBuf>,
    pub composer_json: &'a str,
    pub options: Options,
    pub executor: &'a mut dyn Executor,
    /// the remote repositories, in priority order
    pub repos: Vec<Repository<'a>>,
    pub php: Option<PhpInfo>,
    /// the lock document produced by an update
    pub written_lock: Option<Value>,
    /// packages newly locked by the last update, for the suggestions summary
    pub suggesting_packages: Vec<Package>,
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

fn unsupported(package: &Package, source: &str) -> Option<String> {
    if package.kind == "metapackage" {
        return None;
    }
    if source == "source" {
        let kind = package.source.as_ref().map_or("", |s| s.kind.as_str());
        return (kind != "git").then(|| {
            format!(
                "{}: {kind} sources are not supported by romposer, only git ones are",
                package.pretty_name
            )
        });
    }
    match dist_kind(package) {
        "zip" | "tar" | "xz" | "phar" | "rar" | "path" => None,
        other => Some(format!(
            "{}: dist type \"{other}\" is not supported by romposer",
            package.pretty_name
        )),
    }
}

fn dist_kind(package: &Package) -> &str {
    package.dist.as_ref().map_or("", |d| d.kind.as_str())
}

/// `PathDownloader::remove` keeps a source that is the install path itself.
fn is_own_source(package: &Package, path: &str) -> bool {
    let absolute = |p: &str| {
        crate::fs::normalize_path(&if crate::fs::is_absolute_path(p) {
            p.to_string()
        } else {
            format!("{}/{p}", crate::fs::cwd())
        })
    };
    dist_kind(package) == "path"
        && package
            .dist
            .as_ref()
            .is_some_and(|d| absolute(&d.url) == absolute(path))
}

/// `--prefer-source` / `--prefer-dist` when given, else `preferred-install` (Composer's
/// `getPreferredInstallOptions`).
fn preferred_install(config: &Config, options: &Options) -> (bool, bool) {
    if options.prefer_source || options.prefer_dist {
        return (options.prefer_source, options.prefer_dist);
    }
    match config.get("preferred-install").and_then(Value::as_str) {
        Some("source") => (true, false),
        Some("dist") => (false, true),
        _ => (false, false),
    }
}

/// `DownloadManager::getAvailableSources`, first choice: `source` or `dist`.
fn install_source(
    config: &Config,
    prefer: (bool, bool),
    package: &Package,
    prev: Option<&Package>,
) -> Result<&'static str, String> {
    let mut sources = Vec::new();
    if package.source.as_ref().is_some_and(|s| !s.kind.is_empty()) {
        sources.push("source");
    }
    if package.dist.as_ref().is_some_and(|d| !d.kind.is_empty()) {
        sources.push("dist");
    }
    if sources.is_empty() {
        return Err(format!(
            "Package {}-{} must have a source or dist specified",
            package.name, package.pretty_version
        ));
    }
    if let Some(prev) = prev
        && let Some(prev_source) = sources
            .iter()
            .find(|s| prev.installation_source.as_deref() == Some(**s))
        && !(!prev.is_dev() && *prev_source == "dist" && package.is_dev())
    {
        return Ok(prev_source);
    }
    let (prefer_source, prefer_dist) = prefer;
    if !prefer_source && (prefer_dist || package_preference(config, package) == "dist") {
        sources.reverse();
    }
    Ok(sources[0])
}

/// `DownloadManager::resolvePackageInstallPreference`
fn package_preference(config: &Config, package: &Package) -> &'static str {
    if let Some(Value::Object(prefs)) = config.get("preferred-install") {
        for (pattern, preference) in prefs {
            let re = format!("(?i)^{}$", regex::escape(pattern).replace(r"\*", ".*"));
            if Regex::new(&re).is_ok_and(|r| r.is_match(&package.name)) {
                let preference = preference.as_str().unwrap_or("");
                return if preference == "dist" || (!package.is_dev() && preference == "auto") {
                    "dist"
                } else {
                    "source"
                };
            }
        }
    }
    if package.is_dev() { "source" } else { "dist" }
}

/// The store side of a run: configuration, store, manifest and the one-per-run fallback warning.
struct StoreRun {
    config: StoreConfig,
    store: Store,
    manifest: Manifest,
    fallback_warned: bool,
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl<'a> Installer<'a> {
    /// `Installer::run` on the project's installed.json.
    pub fn run(&mut self) -> Result<i32, String> {
        let vendor_dir = self.config.vendor_dir();
        let installed_path = PathBuf::from(format!("{vendor_dir}/composer/installed.json"));
        let mut local_repo = InstalledRepository::read(&installed_path)?;
        self.run_with(&mut local_repo)
    }

    fn filter(&self) -> RequirementFilter {
        RequirementFilter::from_options(
            self.options.ignore_all_platform_reqs,
            &self.options.ignored_platform_reqs,
        )
    }

    fn locked(&self) -> Option<&Lock> {
        self.lock.as_ref().filter(|l| l.is_locked())
    }

    /// `Installer::run` on a given local repository (the fixture runner's in-memory one).
    pub fn run_with(&mut self, local_repo: &mut InstalledRepository) -> Result<i32, String> {
        if self.options.update_allow_list.is_some() && self.options.update_mirrors {
            return Err(
                "The installer options updateMirrors and updateAllowList are mutually exclusive."
                    .into(),
            );
        }
        let is_fresh_install = local_repo.is_fresh();
        if !self.options.update && self.locked().is_none() {
            let message = "Updating dependencies to latest instead of installing from lock file. See https://getcomposer.org/install for more information.";
            if !self.options.write_lock {
                self.io.warning(&format!(
                    "Lockfile creation is disabled in config. {message}"
                ));
            } else {
                self.io
                    .warning(&format!("No composer.lock file present. {message}"));
            }
            self.options.update = true;
        }
        if self.options.dry_run {
            self.options.verbose = true;
            self.options.run_scripts = false;
            self.options.execute_operations = false;
            self.options.write_lock = false;
            self.options.dump_autoloader = false;
        }
        if self.options.download_only {
            self.options.dump_autoloader = false;
        }
        if self.options.update && !self.options.install {
            self.options.dump_autoloader = false;
        }
        if self.options.run_scripts {
            let dev = if self.options.dev_mode { "1" } else { "0" };
            crate::scripts::put_env("COMPOSER_DEV_MODE", Some(dev))?;
            let event = if self.options.update {
                "pre-update-cmd"
            } else {
                "pre-install-cmd"
            };
            crate::scripts::fire(self.io, self.config, self.root, event)?;
        }
        let vendor_dir = self.config.vendor_dir();
        let code = if self.options.update {
            self.do_update(local_repo, &vendor_dir)?
        } else {
            self.do_install(local_repo, &vendor_dir, false)?
        };
        if code != 0 {
            return Ok(code);
        }

        if self.options.update {
            self.output_suggestions(is_fresh_install)?;
        }

        // Find abandoned packages and warn user
        if let Some(lock) = self.locked() {
            for package in lock.locked_packages(true)? {
                if package.alias_of.is_some() || !package.is_abandoned() {
                    continue;
                }
                let replacement = match package.replacement_package() {
                    Some(r) => format!("Use {r} instead"),
                    None => "No replacement was suggested".into(),
                };
                self.io.warning(&format!(
                    "Package {} is abandoned, you should avoid using it. {replacement}.",
                    package.pretty_name
                ));
            }
        }

        if self.options.dump_autoloader {
            if self.options.optimize_autoloader {
                self.io.error("Generating optimized autoload files");
            } else {
                self.io.error("Generating autoload files");
            }
        }
        if self.options.dump_autoloader && self.executor.writes_files() {
            let options = crate::autoload::DumpOptions {
                optimize: self.options.optimize_autoloader || self.options.classmap_authoritative,
                authoritative: self.options.classmap_authoritative,
                apcu: self.options.apcu_autoloader,
                apcu_prefix: self.options.apcu_autoloader_prefix.clone(),
                dev_mode: Some(self.options.dev_mode),
                dry_run: false,
                run_scripts: self.options.run_scripts,
                strict_ambiguous: false,
                ignore_all_platform_reqs: self.options.ignore_all_platform_reqs,
                ignored_platform_reqs: self.options.ignored_platform_reqs.clone(),
            };
            let class_map = crate::autoload::dump(
                self.io,
                self.config,
                local_repo,
                self.root,
                self.lock.as_ref(),
                &options,
            )?;
            if self.options.strict_psr_autoloader && !class_map.psr_violations().is_empty() {
                return Ok(ERROR_PSR_AUTOLOAD_VIOLATION);
            }
        }

        if self.options.install && self.options.execute_operations && self.executor.writes_files() {
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
                plural(funding),
                if funding == 1 { "is" } else { "are" }
            ));
            self.io
                .error("Use the `composer fund` command to find out more!");
        }

        if self.options.run_scripts {
            let event = if self.options.update {
                "post-update-cmd"
            } else {
                "post-install-cmd"
            };
            crate::scripts::fire(self.io, self.config, self.root, event)?;
        }

        if self.options.audit {
            let (packages, target) = if self.options.update && !self.options.install {
                let lock = self.locked().ok_or("No lock file")?;
                (lock.locked_packages(self.options.dev_mode)?, "locked")
            } else {
                (local_repo.packages.clone(), "installed")
            };
            let mut repos: Vec<&mut ComposerRepository> = self
                .repos
                .iter_mut()
                .filter_map(|r| match r {
                    Repository::Composer(repo) => Some(&mut **repo),
                    _ => None,
                })
                .collect();
            let failed = crate::commands::audit::after_install(
                self.io,
                self.config,
                &mut repos,
                &packages,
                target,
                &self.options.audit_format,
            )?;
            if failed && self.options.error_on_audit {
                return Ok(ERROR_AUDIT_FAILED);
            }
        }
        Ok(0)
    }

    /// `SuggestedPackagesReporter::outputMinimalistic` after an update.
    fn output_suggestions(&mut self, is_fresh_install: bool) -> Result<(), String> {
        let Some(lock) = self.locked() else {
            return Ok(());
        };
        let mut installed = lock.locked_packages(self.options.dev_mode)?;
        installed.extend(self.platform_repo(false)?.packages);
        installed.push(self.root.package.clone());
        // provided name => providing package names
        let mut installed_names: HashMap<String, Vec<String>> = HashMap::new();
        for package in &installed {
            for name in solver::names(package, true) {
                installed_names
                    .entry(name)
                    .or_default()
                    .push(package.name.clone());
            }
        }
        let mut suggestions: Vec<(String, String)> = Vec::new();
        for package in &self.suggesting_packages {
            for target in package.suggests.keys() {
                suggestions.push((package.pretty_name.clone(), target.clone()));
            }
        }
        if is_fresh_install {
            for target in self.root.package.suggests.keys() {
                suggestions.push((self.root.package.pretty_name.clone(), target.clone()));
            }
        }
        let count = suggestions
            .iter()
            .filter(|(source, target)| {
                !installed_names
                    .get(target)
                    .is_some_and(|providers| providers.iter().any(|p| *p != source.to_lowercase()))
            })
            .count();
        if count > 0 {
            self.io.error(&format!(
                "{count} package suggestions were added by new dependencies, use `composer suggest` to see details."
            ));
        }
        Ok(())
    }

    fn binary_installer(&self) -> BinaryInstaller {
        BinaryInstaller {
            bin_dir: self.config.get_str("bin-dir"),
            bin_compat: self.config.get_str("bin-compat"),
            vendor_dir: self.config.vendor_dir(),
        }
    }

    fn platform_repo(&self, for_update: bool) -> Result<PlatformRepository, String> {
        let overrides = if for_update {
            crate::platform::overrides_from_config(self.config)
        } else {
            self.locked()
                .map(|l| l.platform_overrides())
                .unwrap_or_default()
        };
        PlatformRepository::new(&overrides, self.php.as_ref())
    }

    /// `createRepositorySet`: root package repo first, then the platform repo.
    fn create_repository_set(
        &self,
        for_update: bool,
        platform: &'a PlatformRepository,
        root_aliases: &[AliasDef],
        locked_packages: Option<&[Package]>,
    ) -> Result<(RepositorySet<'a>, Package), String> {
        let filter = self.filter();
        let (minimum_stability, mut flags, requires): SetInputs = if for_update {
            let links = self
                .root
                .package
                .requires
                .iter()
                .chain(self.root.package.dev_requires.iter());
            (
                self.root.minimum_stability,
                self.root.stability_flags.clone(),
                links
                    .map(|l| {
                        (
                            l.target.clone(),
                            l.constraint.clone(),
                            l.pretty_constraint.clone(),
                        )
                    })
                    .collect(),
            )
        } else {
            let lock = self.locked().expect("lock");
            (
                lock.minimum_stability(),
                lock.stability_flags(),
                locked_packages
                    .unwrap_or_default()
                    .iter()
                    .map(|p| {
                        (
                            p.name.clone(),
                            Constraint::new("=", &p.version),
                            p.pretty_version.clone(),
                        )
                    })
                    .collect(),
            )
        };
        let mut root_requires = Vec::new();
        for (name, constraint, pretty) in requires {
            if filter.is_ignored(&name) {
                continue;
            }
            let constraint = filter.filter_constraint(&name, &constraint);
            root_requires.push((name, constraint, pretty));
        }
        let mut fixed_root = self.root.package.clone();
        fixed_root.requires = vec![];
        fixed_root.dev_requires = vec![];
        if let Some(real) = &mut fixed_root.alias_of {
            real.requires = vec![];
            real.dev_requires = vec![];
        }
        flags.insert(
            self.root.package.name.clone(),
            parse_stability(&self.root.package.version).priority(),
        );
        let mut set = RepositorySet::new(
            minimum_stability,
            flags,
            root_aliases,
            self.root.references.clone(),
            root_requires,
            self.options.temporary_constraints.clone(),
        );
        set.add(Repository::Root(fixed_root.clone()));
        set.add(Repository::Platform(platform));
        Ok((set, fixed_root))
    }

    fn create_policy(&self, for_update: bool, request: Option<&Request>) -> DefaultPolicy {
        let (mut prefer_stable, mut prefer_lowest) = (None, None);
        if !for_update && let Some(lock) = self.locked() {
            prefer_stable = lock.prefer_stable();
            prefer_lowest = lock.prefer_lowest();
        }
        let prefer_stable =
            prefer_stable.unwrap_or(self.options.prefer_stable || self.root.prefer_stable);
        let prefer_lowest = prefer_lowest.unwrap_or(self.options.prefer_lowest);
        let mut preferred = None;
        if for_update
            && self.options.minimal_update
            && let Some(request) = request
            && let Some((_, locked)) = &request.locked_repo
        {
            let mut map = HashMap::new();
            for p in locked {
                if p.alias_of.is_some() || request.is_in_update_allow_list(&p.name) {
                    continue;
                }
                map.insert(p.name.clone(), p.version.clone());
            }
            preferred = Some(map);
        }
        DefaultPolicy::new(prefer_stable, prefer_lowest, preferred)
    }

    /// `createRequest`: the root package and the platform packages are fixed.
    fn create_request(
        &self,
        fixed_root: &Package,
        platform: &PlatformRepository,
        locked: Option<(usize, Vec<Package>)>,
    ) -> Request {
        let mut request = Request::new(locked);
        request.fix_package(0, fixed_root);
        if let Some(real) = &fixed_root.alias_of {
            request.fix_package(0, real);
        }
        for package in &platform.packages {
            let provided = fixed_root
                .provides
                .iter()
                .find(|l| l.target == package.name)
                .is_some_and(|l| {
                    l.constraint
                        .matches(&Constraint::new("=", &package.version))
                });
            if !provided {
                request.fix_package(1, package);
            }
        }
        request
    }

    fn require_packages_for_update(
        &self,
        request: &mut Request,
        locked: Option<&[Package]>,
        include_dev: bool,
    ) -> Result<(), String> {
        if self.options.update_mirrors {
            let excluded: Vec<String> = if include_dev {
                vec![]
            } else {
                self.locked()
                    .map(|l| l.dev_package_names())
                    .unwrap_or_default()
            };
            for package in locked.unwrap_or_default() {
                if package.alias_of.is_none() && !excluded.contains(&package.name) {
                    request.require_name(
                        &package.name,
                        Constraint::new("==", &package.version),
                        &package.version,
                    )?;
                }
            }
        } else {
            let mut links: Vec<&crate::package::Link> = self.root.package.requires.iter().collect();
            if include_dev {
                links.extend(self.root.package.dev_requires.iter());
            }
            for link in links {
                request.require_name(
                    &link.target,
                    link.constraint.clone(),
                    &link.pretty_constraint,
                )?;
            }
        }
        Ok(())
    }

    fn report_problems(
        &mut self,
        problems: &SolverProblems,
        set: &mut RepositorySet<'a>,
        request: &Request,
        pool: &Pool,
        learned: &[Vec<usize>],
        dev_extraction: bool,
    ) -> Result<String, String> {
        let installed = request.present_indices(pool);
        let needs_ini = problems.problems.iter().any(|p| {
            p.rules().iter().any(|r| {
                r.required_package(pool)
                    .is_some_and(|n| n.starts_with("ext-"))
            })
        });
        let ini = if needs_ini {
            crate::platform::ini_paths()
        } else {
            vec![]
        };
        let verbose = self.io.verbosity >= 1;
        let extensions: HashSet<String> = self
            .php
            .as_ref()
            .map(|p| p.extensions.iter().map(|e| e.to_lowercase()).collect())
            .unwrap_or_default();
        let mut ctx = Ctx {
            io: self.io,
            set,
            request,
            pool,
            verbose,
            installed,
            learned_pool: learned,
            extensions,
        };
        crate::solver::problem::problems_pretty_string(problems, &mut ctx, dev_extraction, &ini)
    }

    /// `Installer::doUpdate`
    fn do_update(
        &mut self,
        local_repo: &mut InstalledRepository,
        vendor_dir: &str,
    ) -> Result<i32, String> {
        let platform = self.platform_repo(true)?;
        // the repository set borrows the platform repo for the whole update
        let platform: &'a PlatformRepository = Box::leak(Box::new(platform));
        let aliases = self.root.aliases.clone();
        let locked_packages: Option<Vec<Package>> = match self.locked() {
            Some(lock) => Some(lock.locked_packages(true)?),
            None => None,
        };
        if (self.options.update_allow_list.is_some() || self.options.update_mirrors)
            && locked_packages.is_none()
        {
            self.io.error(&format!(
                "Cannot update {} without a lock file present. Run `composer update` to generate a lock file.",
                if self.options.update_mirrors { "lock file information" } else { "only a partial set of packages" }
            ));
            return Ok(ERROR_NO_LOCK_FILE_FOR_PARTIAL_UPDATE);
        }
        self.io
            .error("Loading composer repositories with package information");

        let (mut set, fixed_root) = self.create_repository_set(true, platform, &aliases, None)?;
        let remote_count = self.repos.len();
        for repo in std::mem::take(&mut self.repos) {
            set.add(repo);
        }
        let locked_index = locked_packages
            .as_ref()
            .map(|l| set.add(Repository::Locked(l.clone())));
        let mut request = self.create_request(
            &fixed_root,
            platform,
            locked_index.map(|i| (i, locked_packages.clone().unwrap())),
        );
        self.require_packages_for_update(&mut request, locked_packages.as_deref(), true)?;
        if let Some(list) = &self.options.update_allow_list {
            request.set_update_allow_list(list.clone(), self.options.update_allow_transitive);
        }
        let policy = self.create_policy(true, Some(&request));
        let pool = set.create_pool(self.io, &mut request)?;
        self.io.error("Updating dependencies");
        let filter = self.filter();
        let solver = Solver::new(&policy, &pool);
        let (solution, rules) = match solver.solve(self.io, &request, &filter) {
            Ok(r) => r,
            Err(problems) => {
                let err =
                    "Your requirements could not be resolved to an installable set of packages.";
                let pretty = self.report_problems(
                    &problems,
                    &mut set,
                    &request,
                    &pool,
                    &problems.learned_pool,
                    false,
                )?;
                self.io.error(err);
                self.io.error(&pretty);
                if !self.options.dev_mode {
                    self.io.warning("Running update with --no-dev does not mean require-dev is ignored, it just means the packages will not be installed. If dev requirements are blocking the update you have to resolve those problems.");
                }
                self.repos = set.repos.drain(2..2 + remote_count).collect();
                return Ok(ERROR_DEPENDENCY_RESOLUTION_FAILED.max(ERROR_GENERIC_FAILURE));
            }
        };
        let _ = rules;
        if self.io.verbosity >= 1 {
            self.io.error(&format!(
                "Analyzed {} packages to resolve dependencies",
                pool.len()
            ));
            self.io.error(&format!(
                "Analyzed {} rules to resolve dependencies",
                solution.rule_count
            ));
        }
        let mut lock_transaction = LockTransaction::new(&pool, &request, &solution)?;
        self.repos = set.repos.drain(2..2 + remote_count).collect();
        drop(pool);

        if lock_transaction.operations.is_empty() {
            self.io.error("Nothing to modify in lock file");
            if self.options.minimal_update
                && self.options.update_allow_list.is_none()
                && self
                    .locked()
                    .is_some_and(|l| l.is_fresh(self.composer_json))
            {
                self.io.warning("The --minimal-changes option should be used with package arguments or after modifying composer.json requirements, otherwise it will likely not yield any dependency changes.");
            }
        }

        let code = self.extract_dev_packages(
            &mut lock_transaction,
            platform,
            &aliases,
            &policy,
            locked_packages.as_deref(),
        )?;
        if code != 0 {
            return Ok(code);
        }

        let platform_reqs = extract_platform_requirements(&self.root.package.requires);
        let platform_dev_reqs = extract_platform_requirements(&self.root.package.dev_requires);

        let mut installs_updates: Vec<&Operation> = Vec::new();
        let mut uninstalls: Vec<&Operation> = Vec::new();
        if !lock_transaction.operations.is_empty() {
            let mut install_names = Vec::new();
            let mut update_names = Vec::new();
            let mut uninstall_names = Vec::new();
            for operation in &lock_transaction.operations {
                match operation {
                    Operation::Install(p) => {
                        installs_updates.push(operation);
                        install_names.push(format!(
                            "{}:{}",
                            p.pretty_name,
                            p.full_pretty_version()
                        ));
                    }
                    Operation::Update { initial, target } => {
                        if self.options.update_mirrors
                            && initial.name == target.name
                            && initial.version == target.version
                        {
                            continue;
                        }
                        installs_updates.push(operation);
                        update_names.push(format!(
                            "{}:{}",
                            target.pretty_name,
                            target.full_pretty_version()
                        ));
                    }
                    Operation::Uninstall(p) => {
                        uninstalls.push(operation);
                        uninstall_names.push(p.pretty_name.clone());
                    }
                    _ => {}
                }
            }
            if self.config.bool("lock") {
                self.io.error(&format!(
                    "Lock file operations: {} install{}, {} update{}, {} removal{}",
                    install_names.len(),
                    plural(install_names.len()),
                    update_names.len(),
                    plural(update_names.len()),
                    uninstalls.len(),
                    plural(uninstalls.len())
                ));
                if self.io.verbosity >= 1 {
                    if !install_names.is_empty() {
                        self.io
                            .error(&format!("Installs: {}", install_names.join(", ")));
                    }
                    if !update_names.is_empty() {
                        self.io
                            .error(&format!("Updates: {}", update_names.join(", ")));
                    }
                    if !uninstall_names.is_empty() {
                        self.io
                            .error(&format!("Removals: {}", uninstall_names.join(", ")));
                    }
                }
            }
        }
        let by_name = |a: &&Operation, b: &&Operation| a.package().name.cmp(&b.package().name);
        uninstalls.sort_by(by_name);
        installs_updates.sort_by(by_name);
        let mut suggesting = Vec::new();
        for operation in uninstalls.iter().chain(installs_updates.iter()) {
            if let Operation::Install(p) = operation {
                suggesting.push(p.clone());
            }
            if self.config.bool("lock")
                && (!operation.kind().contains("Alias") || self.io.verbosity >= 3)
            {
                self.io.error(&format!("  - {}", operation.show_with(true)));
            }
        }
        self.suggesting_packages = suggesting;

        let lock_data = crate::lock::build_lock_data(
            &crate::lock::content_hash(self.composer_json)?,
            &lock_transaction.new_lock_packages(false, self.options.update_mirrors),
            &lock_transaction.new_lock_packages(true, self.options.update_mirrors),
            &platform_reqs,
            &platform_dev_reqs,
            &lock_transaction.aliases(&aliases),
            self.root.minimum_stability,
            &self.root.stability_flags,
            self.options.prefer_stable || self.root.prefer_stable,
            self.options.prefer_lowest,
            &crate::platform::overrides_from_config(self.config),
        );
        // Composer compares the new document (empty stability-flags/platform/platform-dev
        // are objects) with the parsed file (arrays) using `!==`, so a lock with any of
        // those empty is always rewritten
        let fixup_empty = ["stability-flags", "platform", "platform-dev"]
            .iter()
            .any(|k| {
                lock_data
                    .get(*k)
                    .is_none_or(|v| v.as_object().is_none_or(|o| o.is_empty()))
            });
        let unchanged = !fixup_empty
            && self.locked().is_some_and(|l| {
                crate::json::encode_pretty(&Value::Object(l.data.clone()))
                    == crate::json::encode_pretty(&lock_data)
            });
        if !unchanged {
            let write = self.options.write_lock && self.options.execute_operations;
            if write {
                if let Some(path) = &self.lock_path {
                    crate::lock::write_lock(path, &lock_data)?;
                }
                self.written_lock = Some(lock_data.clone());
                self.io.error("Writing lock file");
            }
            self.lock = Some(Lock {
                data: lock_data.as_object().cloned().unwrap_or_default(),
            });
        }

        if self.options.install {
            return self.do_install(local_repo, vendor_dir, true);
        }
        Ok(0)
    }

    /// `Installer::extractDevPackages`: a second solve with only the non-dev requires.
    fn extract_dev_packages(
        &mut self,
        lock_transaction: &mut LockTransaction,
        platform: &'a PlatformRepository,
        aliases: &[AliasDef],
        policy: &DefaultPolicy,
        locked_packages: Option<&[Package]>,
    ) -> Result<i32, String> {
        if self.root.package.dev_requires.is_empty() {
            return Ok(0);
        }
        let mut result_packages = Vec::new();
        for package in lock_transaction.new_lock_packages(false, false) {
            let dumped = crate::package::dumper::dump(&package);
            let loaded = crate::package::load(&dumped)?;
            if let Some(real) = &loaded.alias_of {
                result_packages.push(loaded.clone());
                result_packages.push((**real).clone());
            } else {
                result_packages.push(loaded);
            }
        }
        let (mut set, fixed_root) = self.create_repository_set(true, platform, aliases, None)?;
        set.add(Repository::Array(ArrayRepo::new(result_packages)));
        let mut request = self.create_request(&fixed_root, platform, None);
        self.require_packages_for_update(&mut request, locked_packages, false)?;
        let pool = set.create_pool_with_all_packages();
        let filter = self.filter();
        let solver = Solver::new(policy, &pool);
        match solver.solve(self.io, &request, &filter) {
            Ok((solution, _)) => {
                let non_dev = LockTransaction::new(&pool, &request, &solution)?;
                lock_transaction.set_non_dev_packages(&non_dev);
                Ok(0)
            }
            Err(problems) => {
                let err = "Unable to find a compatible set of packages based on your non-dev requirements alone.";
                let pretty = self.report_problems(
                    &problems,
                    &mut set,
                    &request,
                    &pool,
                    &problems.learned_pool,
                    true,
                )?;
                self.io.error(err);
                self.io.error("Your requirements can be resolved successfully when require-dev packages are present.");
                self.io.error("You may need to move packages from require-dev or some of their dependencies to require.");
                self.io.error(&pretty);
                Ok(ERROR_DEPENDENCY_RESOLUTION_FAILED)
            }
        }
    }

    /// `Installer::doInstall`
    fn do_install(
        &mut self,
        local_repo: &mut InstalledRepository,
        vendor_dir: &str,
        already_solved: bool,
    ) -> Result<i32, String> {
        let lock = self.lock.clone().ok_or("No lock file")?;
        if !already_solved
            && lock.data.get("content-hash").and_then(|v| v.as_str())
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
        let locked = lock.locked_packages(self.options.dev_mode)?;

        if !already_solved {
            self.io
                .error("Verifying lock file contents can be installed on current platform.");
            let platform = self.platform_repo(false)?;
            let platform: &'a PlatformRepository = Box::leak(Box::new(platform));
            let policy = self.create_policy(false, None);
            let (mut set, fixed_root) =
                self.create_repository_set(false, platform, &[], Some(&locked))?;
            let locked_index = set.add(Repository::Locked(locked.clone()));
            let mut request =
                self.create_request(&fixed_root, platform, Some((locked_index, locked.clone())));

            if !lock.is_fresh(self.composer_json) {
                self.io.warning("Warning: The lock file is not up to date with the latest changes in composer.json. You may be getting outdated dependencies. It is recommended that you run `composer update` or `composer update <package name>`.");
            }
            let missing = lock.missing_requirement_info(self.root, self.options.dev_mode)?;
            if !missing.is_empty() {
                for line in &missing {
                    self.io.error(line);
                }
                if !self.config.bool("allow-missing-requirements") {
                    return Ok(ERROR_LOCK_FILE_INVALID);
                }
            }
            for package in &locked {
                request.fix_locked_package(locked_index, package);
            }
            let mut root_requires: Vec<&crate::package::Link> =
                self.root.package.requires.iter().collect();
            if self.options.dev_mode {
                root_requires.extend(self.root.package.dev_requires.iter());
            }
            for link in &root_requires {
                if crate::package::is_platform_package(&link.target) {
                    request.require_name(
                        &link.target,
                        link.constraint.clone(),
                        &link.pretty_constraint,
                    )?;
                }
            }
            for (name, constraint, pretty) in lock.platform_requirements(self.options.dev_mode) {
                if !root_requires.iter().any(|l| l.target == name) {
                    request.require_name(&name, constraint, &pretty)?;
                }
            }
            let pool = set.create_pool(self.io, &mut request)?;
            let filter = self.filter();
            let solver = Solver::new(&policy, &pool);
            match solver.solve(self.io, &request, &filter) {
                Ok((solution, _)) => {
                    let transaction = LockTransaction::new(&pool, &request, &solution)?;
                    if !transaction.operations.is_empty() {
                        self.io.error("Your lock file cannot be installed on this system without changes. Please run composer update.");
                        return Ok(ERROR_LOCK_FILE_INVALID);
                    }
                }
                Err(problems) => {
                    let err = "Your lock file does not contain a compatible set of packages. Please run composer update.";
                    let pretty = self.report_problems(
                        &problems,
                        &mut set,
                        &request,
                        &pool,
                        &problems.learned_pool,
                        false,
                    )?;
                    self.io.error(err);
                    self.io.error(&pretty);
                    return Ok(ERROR_DEPENDENCY_RESOLUTION_FAILED.max(ERROR_GENERIC_FAILURE));
                }
            }
        }

        let operations = calculate_operations(&local_repo.packages, &locked)?;
        RESOLUTION_COMPLETED.store(true, std::sync::atomic::Ordering::Relaxed);
        crate::scripts::fire(self.io, self.config, self.root, "pre-operations-exec")?;
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
            self.io.error(&format!(
                "Package operations: {} install{}, {} update{}, {} removal{}",
                installs.len(),
                plural(installs.len()),
                updates.len(),
                plural(updates.len()),
                uninstalls.len(),
                plural(uninstalls.len())
            ));
            if self.io.verbosity >= 1 {
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

        if !self.options.execute_operations {
            for op in &operations {
                if !op.kind().contains("Alias") || self.io.verbosity >= 3 {
                    self.io.error(&format!("  - {}", op.show()));
                }
            }
            return Ok(0);
        }

        local_repo.dev_package_names = lock.dev_package_names();
        self.executor.execute(
            self.io,
            self.config,
            self.root,
            local_repo,
            &operations,
            &self.options,
        )?;
        if !operations.is_empty() {
            let vendor = Path::new(vendor_dir);
            if vendor.is_dir() {
                let now = std::time::SystemTime::now();
                let _ = std::fs::File::open(vendor).and_then(|f| f.set_modified(now));
            }
        }
        Ok(0)
    }
}

/// minimum stability, stability flags, root requires
type SetInputs = (
    Stability,
    BTreeMap<String, u8>,
    Vec<(String, Constraint, String)>,
);

/// `Installer::extractPlatformRequirements`
fn extract_platform_requirements(links: &[crate::package::Link]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for link in links {
        if crate::package::is_platform_package(&link.target) {
            match out.iter_mut().find(|(n, _)| *n == link.target) {
                Some(entry) => entry.1 = link.pretty_constraint.clone(),
                None => out.push((link.target.clone(), link.pretty_constraint.clone())),
            }
        }
    }
    out
}

/// The real `InstallationManager`: downloads, store or archive extraction, binaries, installed.json.
pub struct FilesystemExecutor;

impl Executor for FilesystemExecutor {
    fn execute(
        &mut self,
        io: &mut Io,
        config: &Config,
        root: &RootPackage,
        local_repo: &mut InstalledRepository,
        operations: &[Operation],
        options: &Options,
    ) -> Result<(), String> {
        let vendor_dir = config.vendor_dir();
        let mut run = FilesystemRun {
            io,
            config,
            root,
            options,
        };
        run.execute(local_repo, operations, &vendor_dir)
    }
}

struct FilesystemRun<'a> {
    io: &'a mut Io,
    config: &'a Config,
    root: &'a RootPackage,
    options: &'a Options,
}

impl FilesystemRun<'_> {
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

    fn fire_post_events(&mut self, events: &mut Vec<&str>) -> Result<(), String> {
        for event in events.drain(..) {
            crate::scripts::fire(self.io, self.config, self.root, &format!("post-{event}"))?;
        }
        Ok(())
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
        // installation source (`source` or `dist`) of every package to install
        let prefer = preferred_install(self.config, self.options);
        let mut sources: HashMap<String, &'static str> = HashMap::new();
        for op in operations {
            let (p, prev) = match op {
                Operation::Install(p) => (p, None),
                Operation::Update { initial, target } => (target, Some(initial)),
                _ => continue,
            };
            let source = if p.kind == "metapackage" {
                "dist"
            } else {
                install_source(self.config, prefer, p, prev)?
            };
            if let Some(reason) = unsupported(p, source) {
                return Err(reason);
            }
            sources.insert(p.unique_name(), source);
        }
        let from_source = |p: &Package| sources.get(&p.unique_name()) == Some(&"source");

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
        // ponytail: git mirrors sync before the dist downloads start, so a mixed run lists
        // its "Syncing" lines first where Composer interleaves them in operation order
        let mut cached: HashMap<String, bool> = HashMap::new();
        for p in to_install.iter().filter(|p| from_source(p)) {
            cached.insert(
                p.unique_name(),
                crate::git::download(self.io, self.config, p)?,
            );
        }
        // `PathDownloader::download`: only checks, nothing to fetch
        for p in &to_install {
            if dist_kind(p) == "path" && !from_source(p) {
                crate::path_repo::check_source(
                    p,
                    Path::new(&install_path(&vendor_dir, p).unwrap_or_default()),
                )?;
            }
        }
        let to_download: Vec<&Package> = to_install
            .iter()
            .copied()
            .filter(|p| {
                !from_source(p)
                    && p.kind != "metapackage"
                    && dist_kind(p) != "path"
                    && (!goes_through_store(&store_run, &vendor_dir, p)
                        || !store_run
                            .as_ref()
                            .is_some_and(|run| run.store.has(&Store::key(p))))
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

        // pre events fire as each operation starts, post events once its batch is done; plugins
        // and installers get a batch of their own
        let mut post_events: Vec<&str> = Vec::new();
        for op in operations {
            let (event, plugin) = match op {
                Operation::Install(p) => ("package-install", Some(p)),
                Operation::Update { target, .. } => ("package-update", Some(target)),
                Operation::Uninstall(_) => ("package-uninstall", None),
                _ => ("", None),
            };
            let plugin = plugin
                .filter(|p| matches!(p.kind.as_str(), "composer-plugin" | "composer-installer"));
            if plugin.is_some() {
                self.fire_post_events(&mut post_events)?;
            }
            if !event.is_empty() && self.options.run_scripts {
                crate::scripts::fire(self.io, self.config, self.root, &format!("pre-{event}"))?;
                post_events.push(event);
            }
            match op {
                Operation::Install(package) if package.kind == "metapackage" => {
                    // `MetapackageInstaller`: nothing on disk
                    self.io.error(&format!("  - {}", label(package, None)));
                    if !local_repo.has_package(package) {
                        local_repo.add_package(package.clone());
                    }
                }
                Operation::Install(package) => {
                    let path = install_path(&vendor_dir, package).unwrap();
                    if !Path::new(&path).exists() && local_repo.has_package(package) {
                        binaries.remove_binaries(package)?;
                    }
                    if let Some(run) = &mut store_run {
                        run.manifest.forget(&package.name);
                    }
                    let source = from_source(package);
                    if !source && dist_kind(package) == "path" {
                        crate::path_repo::install(
                            self.io,
                            package,
                            Path::new(&path),
                            Some(&label(package, None)),
                        )?;
                    } else if source {
                        crate::git::install(
                            self.io,
                            self.config,
                            package,
                            Path::new(&path),
                            &label(package, None),
                            cached[&package.unique_name()],
                        )?;
                    } else {
                        self.install_code(
                            &mut store_run,
                            &mut downloader,
                            &mut archives,
                            package,
                            Path::new(&path),
                            &vendor_dir,
                            None,
                        )?;
                    }
                    binaries.install_binaries(self.io, package, Path::new(&path), true)?;
                    if !local_repo.has_package(package) {
                        let mut installed = package.clone();
                        installed.installation_source =
                            Some(if source { "source" } else { "dist" }.into());
                        local_repo.add_package(installed);
                    }
                    downloader.cleanup(package, Path::new(&path));
                }
                Operation::Update { initial, target }
                    if initial.kind == "metapackage" || target.kind == "metapackage" =>
                {
                    // ponytail: a library becoming a metapackage (or back) is updated in place,
                    // Composer uninstalls and installs as the installer type changes
                    if let Some(path) = install_path(&vendor_dir, initial) {
                        binaries.remove_binaries(initial)?;
                        remove(Path::new(&path))?;
                    }
                    self.io
                        .error(&format!("  - {}", label(target, Some(initial))));
                    local_repo.remove_package(initial);
                    if !local_repo.has_package(target) {
                        local_repo.add_package(target.clone());
                    }
                }
                Operation::Update { initial, target } => {
                    let initial_path = install_path(&vendor_dir, initial).unwrap();
                    let target_path = install_path(&vendor_dir, target).unwrap();
                    let source = from_source(target);
                    let was_source = initial.installation_source.as_deref() == Some("source");
                    if let Some(run) = &mut store_run {
                        run.manifest.forget(&target.name);
                    }
                    let from_path = !was_source && dist_kind(initial) == "path";
                    let to_path = !source && dist_kind(target) == "path";
                    // a source checkout is only replaced once its local changes are dealt with
                    let forced = if was_source {
                        crate::git::clean_changes(self.config, Path::new(&initial_path), source)?
                    } else {
                        false
                    };
                    binaries.remove_binaries(initial)?;
                    let target_label = label(target, Some(initial));
                    if source && was_source && initial_path == target_path {
                        crate::git::update(
                            self.io,
                            self.config,
                            target,
                            Path::new(&target_path),
                            &target_label,
                            cached[&target.unique_name()],
                            forced,
                        )?;
                        crate::git::reapply_changes(
                            self.io,
                            self.config,
                            Path::new(&target_path),
                            forced,
                        )?;
                    } else if to_path && from_path {
                        // `FileDownloader::update`: one line, then remove and install silently
                        self.io.error(&format!(
                            "  - {target_label}{}",
                            crate::path_repo::appendix(target, Path::new(&target_path))
                        ));
                        if !is_own_source(initial, &initial_path) {
                            remove(Path::new(&initial_path))?;
                        }
                        crate::path_repo::install(self.io, target, Path::new(&target_path), None)?;
                    } else if to_path || from_path {
                        // the downloader type changed: removal and a fresh install
                        self.io.error(&format!(
                            "  - Removing {} ({})",
                            initial.pretty_name,
                            initial.full_pretty_version()
                        ));
                        if !is_own_source(initial, &initial_path) {
                            remove(Path::new(&initial_path))?;
                        }
                        if to_path {
                            crate::path_repo::install(
                                self.io,
                                target,
                                Path::new(&target_path),
                                Some(&label(target, None)),
                            )?;
                        } else if source {
                            crate::git::install(
                                self.io,
                                self.config,
                                target,
                                Path::new(&target_path),
                                &label(target, None),
                                cached[&target.unique_name()],
                            )?;
                        } else {
                            self.install_code(
                                &mut store_run,
                                &mut downloader,
                                &mut archives,
                                target,
                                Path::new(&target_path),
                                &vendor_dir,
                                None,
                            )?;
                        }
                    } else {
                        remove(Path::new(&initial_path))?;
                        if source {
                            crate::git::install(
                                self.io,
                                self.config,
                                target,
                                Path::new(&target_path),
                                &target_label,
                                cached[&target.unique_name()],
                            )?;
                        } else {
                            self.install_code(
                                &mut store_run,
                                &mut downloader,
                                &mut archives,
                                target,
                                Path::new(&target_path),
                                &vendor_dir,
                                Some(initial),
                            )?;
                        }
                    }
                    binaries.install_binaries(self.io, target, Path::new(&target_path), true)?;
                    local_repo.remove_package(initial);
                    if !local_repo.has_package(target) {
                        let mut installed = target.clone();
                        installed.installation_source =
                            Some(if source { "source" } else { "dist" }.into());
                        local_repo.add_package(installed);
                    }
                    downloader.cleanup(target, Path::new(&target_path));
                }
                Operation::Uninstall(package) => {
                    let path = install_path(&vendor_dir, package);
                    let own_source = path.as_ref().is_some_and(|p| is_own_source(package, p));
                    self.io.error(&format!(
                        "  - Removing {} ({}){}",
                        package.pretty_name,
                        package.full_pretty_version(),
                        match (&path, own_source) {
                            (Some(p), true) => format!(", source is still present in {p}"),
                            _ => String::new(),
                        }
                    ));
                    if let Some(path) = path.filter(|_| !own_source) {
                        if package.installation_source.as_deref() == Some("source") {
                            crate::git::clean_changes(self.config, Path::new(&path), false)?;
                        }
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
            if let Some(plugin) = plugin {
                // ponytail: plugins are installed, never activated; the warning stands in for
                // `PluginManager::registerPackage`
                if plugin.kind == "composer-plugin" {
                    if crate::scripts::NO_PLUGINS.load(std::sync::atomic::Ordering::Relaxed) {
                        self.io.warning(&format!(
                            "The \"{}\" plugin was not loaded as plugins are disabled.",
                            plugin.name
                        ));
                    } else {
                        self.io.warning(&format!(
                            "{} is a Composer plugin: romposer installed it but does not execute plugins",
                            plugin.pretty_name
                        ));
                    }
                }
                self.fire_post_events(&mut post_events)?;
            }
        }
        self.fire_post_events(&mut post_events)?;
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
        let label = label(package, initial);
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
            let parent = path.parent().unwrap_or(Path::new(vendor_dir));
            let result = match ingest_zip(&archive, &run.store, &key) {
                Ok(true) => remove(&archive),
                Ok(false) => extract_and_ingest(&run.store, &archive, &key, parent),
                Err(e) => Err(e),
            };
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

/// The store ingest through a temporary extraction next to the package, for the zips
/// `ingest_zip` cannot record without the extracted tree.
fn extract_and_ingest(
    store: &Store,
    archive: &Path,
    key: &str,
    parent: &Path,
) -> Result<(), String> {
    ensure_dir(parent)?;
    let mut temporary_dir;
    loop {
        temporary_dir = parent.join(format!(".pcomposer-{}", crate::fs::random_hex(4)));
        if !temporary_dir.is_dir() {
            break;
        }
    }
    ensure_dir(&temporary_dir)?;
    let result = extract_zip_hashed(archive, &temporary_dir).and_then(|hashes| {
        let _ = remove(archive);
        store.ingest_with_hashes(key, &package_root(&temporary_dir), &hashes)
    });
    let _ = remove(&temporary_dir);
    result
}

/// `InstallOperation::format` / `UpdateOperation::format`
fn label(package: &Package, initial: Option<&Package>) -> String {
    match initial {
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
