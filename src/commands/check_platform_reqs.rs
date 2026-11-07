//! `romposer check-platform-reqs`, a port of `CheckPlatformReqsCommand`.

use crate::commands::dump_autoload::load_project;
use crate::installed::InstalledRepository;
use crate::io::Io;
use crate::json::{Map, Value, encode_pretty};
use crate::lock::find_with_replacers_and_providers;
use crate::package::{Link, Package, is_platform_package};
use crate::platform::{PhpInfo, PlatformRepository};
use crate::semver::Constraint;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, clap::Args)]
pub struct Args {
    /// Disables checking of require-dev packages requirements.
    #[arg(long)]
    pub no_dev: bool,
    /// Checks requirements only from the lock file, not from installed packages.
    #[arg(long)]
    pub lock: bool,
    /// Format of the output: text or json
    #[arg(short = 'f', long, default_value = "text")]
    pub format: String,
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

struct Row {
    name: String,
    version: String,
    link: Option<Link>,
    status: &'static str,
    provider: String,
}

fn execute(args: &Args, io: &mut Io) -> Result<i32, String> {
    let (_file, _contents, root, config, lock) = load_project(io)?;
    let dev_word = if args.no_dev { "non-dev " } else { "" };
    let locked = |with_dev: bool| -> Result<Vec<Package>, String> {
        match &lock {
            Some(l) if l.is_locked() => l.locked_packages(with_dev),
            _ => Err("No lockfile found. Unable to read locked packages".into()),
        }
    };
    let mut remove_packages: Vec<String> = Vec::new();
    let installed: Vec<Package> = if args.lock {
        io.error(&format!(
            "Checking {dev_word}platform requirements using the lock file"
        ));
        locked(!args.no_dev)?
    } else {
        let vendor_dir = config.vendor_dir();
        let local =
            InstalledRepository::read(Path::new(&format!("{vendor_dir}/composer/installed.json")))?;
        if local.packages.is_empty() {
            io.error(&format!(
                "No vendor dir present, checking {dev_word}platform requirements from the lock file"
            ));
            locked(!args.no_dev)?
        } else {
            if args.no_dev {
                remove_packages = local.dev_package_names.clone();
            }
            io.error(&format!(
                "Checking {dev_word}platform requirements for packages in the vendor dir"
            ));
            local.packages
        }
    };

    let mut requires: BTreeMap<String, Vec<Link>> = BTreeMap::new();
    if !args.no_dev {
        for link in &root.package.dev_requires {
            requires.insert(link.target.clone(), vec![link.clone()]);
        }
    }
    let mut all: Vec<&Package> = installed.iter().collect();
    all.push(&root.package);
    if let Some(real) = &root.package.alias_of {
        all.push(real);
    }
    for package in &all {
        if remove_packages.contains(&package.name) {
            continue;
        }
        for link in &package.requires {
            requires
                .entry(link.target.clone())
                .or_default()
                .push(link.clone());
        }
    }

    // this command ignores config.platform and checks the real platform
    let php = PhpInfo::detect();
    if php.is_none() {
        io.error(
            "Warning: no php binary found on PATH, only composer* platform packages are known.",
        );
    }
    let platform = PlatformRepository::new(&Map::new(), php.as_ref())?;
    let mut candidates_pool: Vec<&Package> = all.clone();
    candidates_pool.extend(platform.packages.iter());

    let mut results: Vec<Row> = Vec::new();
    let mut exit_code = 0;
    for (require, links) in &requires {
        if !is_platform_package(require) {
            continue;
        }
        let candidates = find_with_replacers_and_providers(&candidates_pool, require, None);
        if candidates.is_empty() {
            results.push(Row {
                name: require.clone(),
                version: "n/a".into(),
                link: Some(links[0].clone()),
                status: "missing",
                provider: String::new(),
            });
            exit_code = exit_code.max(2);
            continue;
        }
        let mut req_results: Vec<Row> = Vec::new();
        let mut matched = false;
        'candidates: for candidate in candidates {
            let (constraint, pretty) = if candidate.name == *require {
                (
                    Constraint::new("=", &candidate.version),
                    candidate.pretty_version.clone(),
                )
            } else {
                match candidate
                    .provides
                    .iter()
                    .chain(candidate.replaces.iter())
                    .find(|l| l.target == *require)
                {
                    Some(l) => (l.constraint.clone(), l.pretty_constraint.clone()),
                    None => continue,
                }
            };
            let name = if candidate.name == *require {
                candidate.pretty_name.clone()
            } else {
                require.clone()
            };
            let provider = if candidate.name == *require {
                String::new()
            } else {
                format!("provided by {}", candidate.pretty_name)
            };
            for link in links {
                if !link.constraint.matches(&constraint) {
                    req_results.push(Row {
                        name: name.clone(),
                        version: pretty.clone(),
                        link: Some(link.clone()),
                        status: "failed",
                        provider: provider.clone(),
                    });
                    continue 'candidates;
                }
            }
            results.push(Row {
                name,
                version: pretty,
                link: None,
                status: "success",
                provider,
            });
            matched = true;
            break;
        }
        if !matched {
            results.extend(req_results);
            exit_code = exit_code.max(1);
        }
    }

    if args.format == "json" {
        let rows: Vec<Value> = results
            .iter()
            .map(|r| {
                let mut m = Map::new();
                m.insert("name".into(), Value::String(r.name.clone()));
                m.insert("version".into(), Value::String(r.version.clone()));
                m.insert("status".into(), Value::String(r.status.into()));
                m.insert(
                    "failed_requirement".into(),
                    match &r.link {
                        Some(l) => {
                            let mut f = Map::new();
                            f.insert("source".into(), Value::String(l.source.clone()));
                            f.insert("type".into(), Value::String(l.kind.description().into()));
                            f.insert("target".into(), Value::String(l.target.clone()));
                            f.insert(
                                "constraint".into(),
                                Value::String(l.pretty_constraint.clone()),
                            );
                            Value::Object(f)
                        }
                        None => Value::Null,
                    },
                );
                m.insert(
                    "provider".into(),
                    if r.provider.is_empty() {
                        Value::Null
                    } else {
                        Value::String(r.provider.clone())
                    },
                );
                Value::Object(m)
            })
            .collect();
        io.write(&encode_pretty(&Value::Array(rows)));
    } else {
        // Symfony's compact table: each cell padded to its column, one space after each
        let rows: Vec<[String; 5]> = results
            .iter()
            .map(|r| {
                let link_str = r
                    .link
                    .as_ref()
                    .map(|l| {
                        format!(
                            "{} {} {} ({})",
                            l.source,
                            l.kind.description(),
                            l.target,
                            l.constraint
                        )
                    })
                    .unwrap_or_default();
                let text = r
                    .link
                    .as_ref()
                    .map(|l| {
                        format!(
                            "{} {} {} ({})",
                            l.source,
                            l.kind.description(),
                            l.target,
                            l.pretty_constraint
                        )
                    })
                    .unwrap_or_default();
                [
                    r.name.clone(),
                    r.version.clone(),
                    link_str,
                    text,
                    format!("{} {}", r.status, r.provider)
                        .trim_end()
                        .to_string(),
                ]
            })
            .collect();
        let mut widths = [0usize; 5];
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        for row in &rows {
            let line: String = row
                .iter()
                .enumerate()
                .map(|(i, cell)| format!("{:<width$} ", cell, width = widths[i]))
                .collect();
            io.write(&line);
        }
    }
    Ok(exit_code)
}
