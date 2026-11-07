//! `audit`: Composer's `Auditor` over the security advisories of the composer repositories.

use crate::commands::dump_autoload::load_project;
use crate::commands::table;
use crate::config::Config;
use crate::io::Io;
use crate::json::{Map, Value, encode};
use crate::package::Package;
use crate::repository::{Advisory, ComposerRepository, package_names_to_regexp};
use crate::semver::Constraint;
use std::collections::BTreeMap;

pub const FORMATS: [&str; 4] = ["table", "plain", "json", "summary"];
pub const ABANDONED_MODES: [&str; 3] = ["ignore", "report", "fail"];

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Disables auditing of require-dev packages.
    #[arg(long)]
    pub no_dev: bool,
    /// Output format. Must be "table", "plain", "json", or "summary".
    #[arg(short = 'f', long, default_value = "table")]
    pub format: String,
    /// Audit based on the lock file instead of the installed packages.
    #[arg(long)]
    pub locked: bool,
    /// Behavior on abandoned packages. Must be "ignore", "report", or "fail".
    #[arg(long)]
    pub abandoned: Option<String>,
    /// Ignore advisories of a certain severity level.
    #[arg(long)]
    pub ignore_severity: Vec<String>,
    /// Ignore repositories that are unreachable or return a non-200 status code.
    #[arg(long)]
    pub ignore_unreachable: bool,
    /// Only audit packages directly required by the root package.
    #[arg(short = 'D', long)]
    pub direct: bool,
}

/// `PolicyConfig` reduced to what the audit operation reads.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// package names, advisory ids, CVEs and remote ids → reason
    pub ignore: BTreeMap<String, Option<String>>,
    pub ignore_severity: BTreeMap<String, Option<String>>,
    /// `ignore` | `report` | `fail`
    pub abandoned: String,
    pub ignore_abandoned: Vec<String>,
    pub ignore_unreachable: bool,
}

/// One ignore entry in any of its shapes: `"id"` list item, `"id": null`, `"id": "reason"`,
/// `"id": {reason, on-audit}` (policy) or `{reason, apply}` (legacy), or a list of objects.
fn parse_ignore_rules(config: Option<&Value>) -> Result<Vec<(String, Option<String>)>, String> {
    let mut rules = Vec::new();
    let from_object = |key: &str,
                       object: &Map<String, Value>|
     -> Result<Option<Option<String>>, String> {
        let reason = object
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let apply = object.get("apply").and_then(Value::as_str).unwrap_or("all");
        if !["audit", "block", "all"].contains(&apply) {
            return Err(format!(
                "Invalid 'apply' value for '{key}': {apply}. Expected 'audit', 'block', or 'all'."
            ));
        }
        let on_audit = object
            .get("on-audit")
            .is_none_or(|v| v != &Value::Bool(false))
            && ["audit", "all"].contains(&apply);
        Ok(on_audit.then_some(reason))
    };
    match config {
        Some(Value::Array(items)) => {
            for item in items {
                if let Value::String(id) = item {
                    rules.push((id.clone(), None));
                }
            }
        }
        Some(Value::Object(map)) => {
            for (key, value) in map {
                // a list item that was merged into an object keeps its numeric key
                if key.bytes().all(|b| b.is_ascii_digit())
                    && let Value::String(id) = value
                {
                    rules.push((id.clone(), None));
                    continue;
                }
                match value {
                    Value::Null => rules.push((key.clone(), None)),
                    Value::String(reason) => rules.push((key.clone(), Some(reason.clone()))),
                    Value::Object(object) => {
                        if let Some(reason) = from_object(key, object)? {
                            rules.push((key.clone(), reason));
                        }
                    }
                    Value::Array(list) => {
                        for entry in list {
                            if let Value::Object(object) = entry
                                && let Some(reason) = from_object(key, object)?
                            {
                                rules.push((key.clone(), reason));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(rules)
}

/// `ListPolicyConfig::mergeReason`
fn merge_reason(map: &mut BTreeMap<String, Option<String>>, key: String, reason: Option<String>) {
    let merged = match (map.get(&key), reason) {
        (None, reason) => reason,
        (Some(existing), None) => existing.clone(),
        (Some(None), Some(new)) => Some(new),
        (Some(Some(existing)), Some(new)) => {
            if existing.split(';').map(str::trim).any(|p| p == new) {
                Some(existing.clone())
            } else {
                Some(format!("{existing}; {new}"))
            }
        }
    };
    map.insert(key, merged);
}

impl Policy {
    /// `PolicyConfig::fromConfig` for the audit operation: `config.policy` when set,
    /// else the legacy `config.audit` keys.
    pub fn from_config(config: &Config) -> Result<Policy, String> {
        let audit = match config.get("audit") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        let policy = match config.get("policy") {
            Some(Value::Bool(false)) => None,
            Some(Value::Object(m)) => Some(m.clone()),
            _ => Some(Map::new()),
        };
        let Some(policy) = policy else {
            return Ok(Policy {
                abandoned: "ignore".into(),
                ignore_unreachable: true,
                ..Policy::default()
            });
        };
        let mut out = Policy::default();
        let advisories = policy.get("advisories");
        if advisories.is_none() && !audit.is_empty() {
            for (id, reason) in parse_ignore_rules(audit.get("ignore"))? {
                merge_reason(&mut out.ignore, id, reason);
            }
            for (severity, reason) in parse_ignore_rules(audit.get("ignore-severity"))? {
                out.ignore_severity.insert(severity, reason);
            }
        } else if let Some(Value::Object(advisories)) = advisories {
            for (id, reason) in parse_ignore_rules(advisories.get("ignore-id"))? {
                out.ignore.insert(id, reason);
            }
            for (name, reason) in parse_ignore_rules(advisories.get("ignore"))? {
                merge_reason(&mut out.ignore, name, reason);
            }
            for (severity, reason) in parse_ignore_rules(advisories.get("ignore-severity"))? {
                out.ignore_severity.insert(severity, reason);
            }
        }
        let abandoned = policy.get("abandoned");
        if abandoned.is_none() && !audit.is_empty() {
            out.abandoned = audit
                .get("abandoned")
                .and_then(Value::as_str)
                .unwrap_or("fail")
                .to_string();
            out.ignore_abandoned = parse_ignore_rules(audit.get("ignore-abandoned"))?
                .into_iter()
                .map(|(n, _)| n)
                .collect();
        } else {
            match abandoned {
                Some(Value::Bool(false)) => out.abandoned = "ignore".into(),
                Some(Value::Object(m)) => {
                    out.abandoned = m
                        .get("audit")
                        .and_then(Value::as_str)
                        .unwrap_or("fail")
                        .to_string();
                    out.ignore_abandoned = parse_ignore_rules(m.get("ignore"))?
                        .into_iter()
                        .map(|(n, _)| n)
                        .collect();
                }
                _ => out.abandoned = "fail".into(),
            }
        }
        if let Ok(mode) = std::env::var("COMPOSER_AUDIT_ABANDONED") {
            if !ABANDONED_MODES.contains(&mode.as_str()) {
                return Err(format!(
                    "Invalid value for COMPOSER_AUDIT_ABANDONED: {mode}. Expected one of ignore, report, fail."
                ));
            }
            out.abandoned = mode;
        }
        let unreachable = policy
            .get("ignore-unreachable")
            .or_else(|| audit.get("ignore-unreachable"));
        out.ignore_unreachable = match unreachable {
            Some(Value::Bool(b)) => *b,
            Some(Value::Array(scopes)) => scopes.iter().any(|s| s == "audit"),
            _ => false,
        };
        Ok(out)
    }
}

type AdvisoryMap = BTreeMap<String, Vec<Advisory>>;

/// `RepositorySet::getMatchingSecurityAdvisories`: every repository's advisories merged
/// per package name; unreachable repositories collected when ignored.
fn matching_advisories(
    io: &mut Io,
    repos: &mut [&mut ComposerRepository],
    packages: &[Package],
    allow_partial: bool,
    ignore_unreachable: bool,
    unreachable: &mut Vec<String>,
) -> Result<AdvisoryMap, String> {
    let mut versions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for package in packages {
        if package.root_alias
            || package.version.starts_with("dev-")
            || package.version == "9999999-dev"
        {
            continue;
        }
        let list = versions.entry(package.name.clone()).or_default();
        if !list.contains(&package.version) {
            list.push(package.version.clone());
        }
    }
    let constraints: BTreeMap<String, Constraint> = versions
        .into_iter()
        .map(|(name, versions)| {
            let list: Vec<Constraint> = versions.iter().map(|v| Constraint::new("=", v)).collect();
            (name, Constraint::multi(list, false))
        })
        .collect();
    let mut merged: AdvisoryMap = BTreeMap::new();
    for repo in repos.iter_mut() {
        let result = match repo.has_security_advisories(io) {
            Ok(false) => continue,
            Ok(true) => repo.get_security_advisories(io, &constraints, allow_partial),
            Err(e) => Err(e),
        };
        match result {
            Ok(advisories) => {
                for (name, list) in advisories {
                    merged.entry(name).or_default().extend(list);
                }
            }
            Err(message) if ignore_unreachable => unreachable.push(message),
            Err(message) => return Err(message),
        }
    }
    Ok(merged)
}

/// `Auditor::audit`: writes the report in `format`, returns 1 when something fails the policy.
pub fn audit(
    io: &mut Io,
    repos: &mut [&mut ComposerRepository],
    policy: &Policy,
    packages: &[Package],
    format: &str,
) -> Result<i32, String> {
    let mut unreachable = Vec::new();
    let summary = format == "summary";
    let mut all = matching_advisories(
        io,
        repos,
        packages,
        summary,
        policy.ignore_unreachable,
        &mut unreachable,
    )?;
    // partial advisories cannot be matched against CVE or remote ids: load them in full
    let needs_full = summary
        && !all.values().all(|l| l.iter().all(|a| a.full))
        && policy.ignore.keys().any(|id| !id.starts_with("PKSA-"));
    if needs_full {
        all = matching_advisories(
            io,
            repos,
            packages,
            false,
            policy.ignore_unreachable,
            &mut unreachable,
        )?;
    }
    let (advisories, ignored) = process_advisories(all, policy);

    let abandoned: Vec<&Package> = if policy.abandoned == "ignore" {
        vec![]
    } else {
        let filter = (!policy.ignore_abandoned.is_empty())
            .then(|| package_names_to_regexp(&policy.ignore_abandoned));
        packages
            .iter()
            .filter(|p| p.alias_of.is_none() && p.is_abandoned())
            .filter(|p| filter.as_ref().is_none_or(|f| !f.is_match(&p.name)))
            .collect()
    };
    let abandoned_count = if policy.abandoned == "fail" {
        abandoned.len()
    } else {
        0
    };
    let status = i32::from(!advisories.is_empty() || abandoned_count > 0);

    if format == "json" {
        let to_json = |map: &AdvisoryMap| -> Value {
            if map.is_empty() {
                return Value::Array(vec![]);
            }
            Value::Object(
                map.iter()
                    .map(|(name, list)| {
                        (
                            name.clone(),
                            Value::Array(list.iter().map(Advisory::to_json).collect()),
                        )
                    })
                    .collect(),
            )
        };
        let mut json = Map::new();
        json.insert("advisories".into(), to_json(&advisories));
        if !ignored.is_empty() {
            json.insert("ignored-advisories".into(), to_json(&ignored));
        }
        if !unreachable.is_empty() {
            json.insert(
                "unreachable-repositories".into(),
                Value::Array(unreachable.iter().cloned().map(Value::String).collect()),
            );
        }
        json.insert(
            "abandoned".into(),
            if abandoned.is_empty() {
                Value::Array(vec![])
            } else {
                Value::Object(
                    abandoned
                        .iter()
                        .map(|p| {
                            (
                                p.pretty_name.clone(),
                                p.replacement_package().map_or(Value::Null, Value::String),
                            )
                        })
                        .collect(),
                )
            },
        );
        json.insert("filter".into(), Value::Array(vec![]));
        json.insert("cooldown".into(), Value::Array(vec![]));
        io.write(&encode(&Value::Object(json), true, true, true));
        return Ok(status);
    }

    if !advisories.is_empty() || !ignored.is_empty() {
        for (map, label) in [(&ignored, "ignored security"), (&advisories, "security")] {
            let total: usize = map.values().map(Vec::len).sum();
            if map.is_empty() {
                continue;
            }
            io.write(&format!(
                "Found {total} {label} vulnerability advisor{} affecting {} package{}{}",
                if total == 1 { "y" } else { "ies" },
                map.len(),
                if map.len() == 1 { "" } else { "s" },
                if summary { "." } else { ":" }
            ));
            output_advisories(io, map, format, label.starts_with("ignored"));
        }
        if summary {
            io.write("Run \"composer audit\" for a full list of advisories.");
        }
    } else {
        io.write("No security vulnerability advisories found.");
    }
    if !unreachable.is_empty() {
        io.warning("The following repositories were unreachable:");
        for repo in &unreachable {
            io.error(&format!("  - {repo}"));
        }
    }
    if !abandoned.is_empty() && !summary {
        io.write(&format!(
            "Found {} abandoned package{}:",
            abandoned.len(),
            if abandoned.len() > 1 { "s" } else { "" }
        ));
        if format == "plain" {
            for package in &abandoned {
                let replacement = match package.replacement_package() {
                    Some(r) => format!("Use {r} instead"),
                    None => "No replacement was suggested".into(),
                };
                io.write(&format!(
                    "{} is abandoned. {replacement}.",
                    package.pretty_name
                ));
            }
        } else {
            let rows: Vec<Vec<String>> = abandoned
                .iter()
                .map(|p| {
                    vec![
                        p.pretty_name.clone(),
                        p.replacement_package().unwrap_or_else(|| "none".into()),
                    ]
                })
                .collect();
            for line in table::box_table(
                Some(&["Abandoned Package", "Suggested Replacement"]),
                &rows,
                Some((80, 80)),
            ) {
                io.write(&line);
            }
        }
    }
    Ok(status)
}

/// `Auditor::processAdvisories`: splits the advisories into active and ignored ones.
fn process_advisories(all: AdvisoryMap, policy: &Policy) -> (AdvisoryMap, AdvisoryMap) {
    if policy.ignore.is_empty() && policy.ignore_severity.is_empty() {
        return (all, BTreeMap::new());
    }
    let (mut active, mut ignored): (AdvisoryMap, AdvisoryMap) = Default::default();
    let mut reason: Option<String> = None;
    for (package, list) in all {
        for mut advisory in list {
            let mut is_active = true;
            if let Some(r) = policy.ignore.get(&package) {
                is_active = false;
                reason = r.clone();
            }
            if let Some(r) = policy.ignore.get(&advisory.id) {
                is_active = false;
                reason = r.clone();
            }
            if advisory.full {
                if let Some(severity) = advisory.str("severity")
                    && let Some(r) = policy.ignore_severity.get(severity)
                {
                    is_active = false;
                    reason = Some(
                        r.clone()
                            .unwrap_or_else(|| format!("{severity} severity is ignored")),
                    );
                }
                if let Some(cve) = advisory.str("cve")
                    && let Some(r) = policy.ignore.get(cve)
                {
                    is_active = false;
                    reason = r.clone();
                }
                if let Some(Value::Array(sources)) = advisory.data.get("sources") {
                    for source in sources {
                        if let Some(remote) = source.get("remoteId").and_then(Value::as_str)
                            && let Some(r) = policy.ignore.get(remote)
                        {
                            is_active = false;
                            reason = r.clone();
                            break;
                        }
                    }
                }
            }
            if is_active {
                active.entry(package.clone()).or_default().push(advisory);
                continue;
            }
            if advisory.full {
                advisory.ignore_reason = reason.clone();
            }
            ignored.entry(package.clone()).or_default().push(advisory);
        }
    }
    (active, ignored)
}

/// `Auditor::getAdvisoryFields`
fn fields(advisory: &Advisory, ignored: bool) -> Vec<(&'static str, String)> {
    let mut out = vec![("Package", advisory.package.clone())];
    if !advisory.full {
        out.push(("Advisory ID", advisory.id.clone()));
        out.push(("Affected versions", advisory.affected.clone()));
        return out;
    }
    out.push((
        "Severity",
        advisory.str("severity").unwrap_or("").to_string(),
    ));
    out.push(("Advisory ID", advisory.id.clone()));
    out.push(("CVE", advisory.str("cve").unwrap_or("NO CVE").to_string()));
    out.push(("Title", advisory.str("title").unwrap_or("").to_string()));
    out.push(("URL", advisory.str("link").unwrap_or("").to_string()));
    out.push(("Affected versions", advisory.affected.clone()));
    out.push(("Reported at", advisory.reported_at()));
    if ignored {
        out.push((
            "Ignore reason",
            advisory
                .ignore_reason
                .clone()
                .unwrap_or_else(|| "None specified".into()),
        ));
    }
    out
}

fn output_advisories(io: &mut Io, map: &AdvisoryMap, format: &str, ignored: bool) {
    let mut first = true;
    for advisory in map.values().flatten() {
        let fields = fields(advisory, ignored);
        match format {
            "table" => {
                let rows: Vec<Vec<String>> = fields
                    .iter()
                    .map(|(label, value)| vec![label.to_string(), value.clone()])
                    .collect();
                for line in table::box_table(None, &rows, Some((80, 80))) {
                    io.write(&line);
                }
            }
            "plain" => {
                if !first {
                    io.write("--------");
                }
                for (label, value) in &fields {
                    io.write(&format!("{label}: {value}"));
                }
                first = false;
            }
            _ => {}
        }
    }
}

/// `AuditCommand::execute`
pub fn run(args: &Args, io: &mut Io) -> i32 {
    match run_inner(args, io) {
        Ok(code) => code.min(255),
        Err(message) => {
            io.error(&message);
            1
        }
    }
}

fn run_inner(args: &Args, io: &mut Io) -> Result<i32, String> {
    if !FORMATS.contains(&args.format.as_str()) {
        return Err(format!("--format must be one of {}.", FORMATS.join(", ")));
    }
    if let Some(mode) = &args.abandoned
        && !ABANDONED_MODES.contains(&mode.as_str())
    {
        return Err(format!(
            "--abandoned must be one of {}.",
            ABANDONED_MODES.join(", ")
        ));
    }
    let (_, _, root, config, lock) = load_project(io)?;
    let packages: Vec<Package> = if args.locked {
        let Some(lock) = lock.as_ref().filter(|l| l.is_locked()) else {
            return Err("Valid composer.json and composer.lock files are required to run this command with --locked".into());
        };
        lock.locked_packages(!args.no_dev)?
    } else {
        let installed = crate::installed::InstalledRepository::read(std::path::Path::new(
            &format!("{}/composer/installed.json", config.vendor_dir()),
        ))?;
        if args.no_dev {
            crate::commands::show::filter_required_packages(&installed.packages, &root.package)
        } else {
            installed.packages
        }
    };
    if packages.is_empty() {
        if !root.package.requires.is_empty()
            || (!args.no_dev && !root.package.dev_requires.is_empty())
        {
            io.error("No installed packages found. Please run \"composer install\" before running \"audit\" or pass \"--locked\" to audit the lock file.");
            return Ok(1);
        }
        io.error("No packages - skipping audit.");
        return Ok(0);
    }
    let packages = if args.direct {
        let requires: Vec<&str> = root
            .package
            .requires
            .iter()
            .chain(if args.no_dev {
                [].iter()
            } else {
                root.package.dev_requires.iter()
            })
            .map(|l| l.target.as_str())
            .collect();
        let direct: Vec<Package> = packages
            .into_iter()
            .filter(|p| {
                std::iter::once(&p.name)
                    .chain(
                        p.provides
                            .iter()
                            .chain(p.replaces.iter())
                            .map(|l| &l.target),
                    )
                    .any(|n| requires.contains(&n.as_str()))
            })
            .collect();
        if direct.is_empty() {
            io.error("No direct dependencies - skipping audit.");
            return Ok(0);
        }
        direct
    } else {
        packages
    };
    let mut policy = Policy::from_config(&config)?;
    if let Some(mode) = &args.abandoned {
        policy.abandoned = mode.clone();
    }
    for severity in &args.ignore_severity {
        policy
            .ignore_severity
            .entry(severity.clone())
            .or_insert(None);
    }
    if args.ignore_unreachable {
        policy.ignore_unreachable = true;
    }
    let http = std::rc::Rc::new(crate::download::Http::new(&config)?);
    let mut repos = crate::repository::from_config(&config, http)?;
    let mut refs: Vec<&mut ComposerRepository> = repos.iter_mut().collect();
    audit(io, &mut refs, &policy, &packages, &args.format)
}

/// `--no-audit`, or `COMPOSER_NO_AUDIT=1`, skips the post-update audit.
pub fn enabled(no_audit: bool) -> bool {
    !no_audit
        && !std::env::var("COMPOSER_NO_AUDIT")
            .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// `Installer`'s post-install/update audit: `installed` or `locked` packages, summary
/// format by default; `true` when the audit failed (an unreachable repository does not).
pub fn after_install(
    io: &mut Io,
    config: &Config,
    repos: &mut [&mut ComposerRepository],
    packages: &[Package],
    target: &str,
    format: &str,
) -> Result<bool, String> {
    if packages.is_empty() {
        io.error(&format!("No {target} packages - skipping audit."));
        return Ok(false);
    }
    let policy = Policy::from_config(config)?;
    match audit(io, repos, &policy, packages, format) {
        Ok(status) => Ok(status > 0),
        Err(message) => {
            io.error(&format!("Failed to audit {target} packages."));
            if io.verbosity > 0 {
                io.error(&format!(
                    "[Composer\\Downloader\\TransportException] {message}"
                ));
            }
            Ok(false)
        }
    }
}
