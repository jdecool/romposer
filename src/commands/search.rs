//! `composer search`: installed and platform packages first, then every repository.

use crate::commands::dump_autoload::try_load_project;
use crate::config::Config;
use crate::download::Http;
use crate::installed::InstalledRepository;
use crate::io::Io;
use crate::json::{Map, Value, encode};
use crate::platform::{PhpInfo, PlatformRepository, overrides_from_config};
use crate::repository::{self, SEARCH_FULLTEXT, SEARCH_NAME, SEARCH_VENDOR, search_packages};
use std::path::Path;
use std::rc::Rc;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// tokens to search for
    #[arg(required = true)]
    tokens: Vec<String>,
    /// Search only in package names
    #[arg(short = 'N', long)]
    only_name: bool,
    /// Search only for vendor / organization names, returns only "vendor" as result
    #[arg(short = 'O', long)]
    only_vendor: bool,
    /// Search for a specific package type
    #[arg(short = 't', long = "type")]
    kind: Option<String>,
    /// Format of the output: text or json
    #[arg(short = 'f', long, default_value = "text")]
    format: String,
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
    if args.format != "text" && args.format != "json" {
        io.error(&format!(
            "Unsupported format \"{}\". See help for supported formats.",
            args.format
        ));
        return Ok(1);
    }
    let config = match try_load_project(io)? {
        Some((_, _, _, config, _)) => config,
        None => Config::create(&crate::fs::cwd())?,
    };
    let php = PhpInfo::detect();
    let platform = PlatformRepository::new(&overrides_from_config(&config), php.as_ref())?;
    let local = InstalledRepository::read(Path::new(&format!(
        "{}/composer/installed.json",
        config.vendor_dir()
    )))?;
    let http = Rc::new(Http::new(&config)?);
    let mut repos = repository::from_config(&config, http)?;

    let mode = if args.only_name {
        if args.only_vendor {
            return Err("--only-name and --only-vendor cannot be used together".into());
        }
        SEARCH_NAME
    } else if args.only_vendor {
        SEARCH_VENDOR
    } else {
        SEARCH_FULLTEXT
    };
    let mut query = args.tokens.join(" ");
    if mode != SEARCH_FULLTEXT {
        query = regex::escape(&query);
    }
    let kind = args.kind.as_deref();

    let mut results = Vec::new();
    let installed: Vec<&crate::package::Package> = local.packages.iter().collect();
    results.extend(search_packages(&installed, &query, mode, kind));
    let platform_packages: Vec<&crate::package::Package> = platform.packages.iter().collect();
    results.extend(search_packages(&platform_packages, &query, mode, kind));
    for repo in &mut repos {
        results.extend(repo.search(io, &query, mode, kind)?);
    }

    if args.format == "json" {
        io.write(&encode(
            &Value::Array(results.into_iter().map(Value::Object).collect()),
            true,
            true,
            true,
        ));
        return Ok(0);
    }
    if results.is_empty() {
        return Ok(0);
    }
    let width = Io::terminal_width();
    let name_of = |r: &Map<String, Value>| {
        r.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let name_length = results.iter().map(|r| name_of(r).len()).max().unwrap_or(0) + 1;
    for result in &results {
        let name = name_of(result);
        let mut description = match result.get("description") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => crate::json::encode_compact(other),
        };
        let abandoned = result
            .get("abandoned")
            .is_some_and(|a| a != &Value::Bool(false) && a != &Value::Null);
        // `<warning>! Abandoned !</warning> `: Composer measures the tagged string
        let (warning, warning_len) = if abandoned {
            ("! Abandoned ! ", "<warning>! Abandoned !</warning> ".len())
        } else {
            ("", 0)
        };
        let remaining = width as i64 - name_length as i64 - warning_len as i64 - 2;
        if description.len() as i64 > remaining {
            let cut = (remaining - 3).max(0) as usize;
            let mut end = cut.min(description.len());
            while !description.is_char_boundary(end) {
                end -= 1;
            }
            description = format!("{}...", &description[..end]);
        }
        io.write(&format!("{name:<name_length$}{warning}{description}"));
    }
    Ok(0)
}
