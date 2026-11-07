//! Port of `PlatformRepository`: platform packages (`php`, `php-*`, `ext-*`,
//! `lib-*`, `composer*`) detected from the `php` binary on PATH with Composer's
//! per-extension probes, `config.platform` overrides, and the platform
//! requirement filter (`--ignore-platform-req(s)`).

use crate::json::{Map, Value};
use crate::package::{Link, LinkType, Package, is_platform_package};
use crate::semver::{self, Constraint, intervals};
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::process::Command;

/// One `php` run collecting everything `PlatformRepository::initialize` reads from the runtime.
const PROBE: &str = r#"<?php
$exts = get_loaded_extensions();
$info = [];
foreach ($exts as $e) { ob_start(); try { (new ReflectionExtension($e))->info(); } catch (Throwable $t) {} $info[$e] = (string) ob_get_clean(); }
$consts = [];
foreach (['PHP_DEBUG','PHP_ZTS','PHP_INT_SIZE','AF_INET6','GD_VERSION','GMP_VERSION','ICONV_VERSION','INTL_ICU_VERSION','LIBXML_DOTTED_VERSION','OPENSSL_VERSION_TEXT','PCRE_VERSION','PGSQL_LIBPQ_VERSION','RD_KAFKA_VERSION','SODIUM_LIBRARY_VERSION','LIBXSLT_DOTTED_VERSION','ZLIB_VERSION','MB_ONIGURUMA_VERSION','HHVM_VERSION'] as $c) {
    if (defined($c)) { $consts[$c] = constant($c); }
}
if (class_exists('ZipArchive') && defined('ZipArchive::LIBZIP_VERSION')) { $consts['ZipArchive::LIBZIP_VERSION'] = constant('ZipArchive::LIBZIP_VERSION'); }
$extra = [];
$extra['ipv6'] = defined('AF_INET6') || @inet_pton('::') !== false;
if (class_exists('ResourceBundle')) { try { $rb = ResourceBundle::create('root', 'ICUDATA', false); if ($rb !== null) { $extra['cldr'] = $rb->get('Version'); } } catch (Throwable $t) {} }
if (class_exists('IntlChar')) { $extra['unicode'] = implode('.', array_slice(IntlChar::getUnicodeVersion(), 0, 3)); }
if (class_exists('Imagick')) { try { $extra['imagick'] = (new Imagick())->getVersion()['versionString']; } catch (Throwable $t) {} }
$versions = [];
foreach ($exts as $e) { $versions[$e] = phpversion($e); }
echo json_encode(['version' => PHP_VERSION, 'version_id' => PHP_VERSION_ID, 'extensions' => $exts, 'ext_versions' => $versions, 'info' => $info, 'constants' => $consts, 'extra' => $extra]);
"#;

/// What the `php` binary reports.
#[derive(Clone, Debug, Default)]
pub struct PhpInfo {
    pub version: String,
    pub version_id: i64,
    pub extensions: Vec<String>,
    pub ext_versions: HashMap<String, String>,
    pub info: HashMap<String, String>,
    pub constants: Map<String, Value>,
    pub extra: Map<String, Value>,
}

impl PhpInfo {
    /// Runs `php` from PATH once; `None` when there is no usable php.
    pub fn detect() -> Option<PhpInfo> {
        use std::io::Write;
        let mut child = Command::new("php")
            .args(["-d", "display_errors=0", "-d", "error_reporting=0"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(PROBE.as_bytes()).ok()?;
        let output = child.wait_with_output().ok()?;
        if !output.status.success() {
            return None;
        }
        let json = String::from_utf8_lossy(&output.stdout);
        let start = json.find('{')?;
        let value: Value = serde_json::from_str(&json[start..]).ok()?;
        let obj = value.as_object()?;
        Some(PhpInfo {
            version: obj.get("version")?.as_str()?.to_string(),
            version_id: obj.get("version_id").and_then(Value::as_i64).unwrap_or(0),
            extensions: obj
                .get("extensions")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            ext_versions: obj
                .get("ext_versions")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            info: obj
                .get("info")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            constants: obj
                .get("constants")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            extra: obj
                .get("extra")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        })
    }

    fn constant_str(&self, name: &str) -> Option<String> {
        match self.constants.get(name)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(if *b { "1".into() } else { String::new() }),
            _ => None,
        }
    }

    fn constant_truthy(&self, name: &str) -> bool {
        match self.constants.get(name) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::String(s)) => !(s.is_empty() || s == "0"),
            _ => false,
        }
    }

    fn constant_int(&self, name: &str) -> Option<i64> {
        self.constants.get(name).and_then(Value::as_i64)
    }
}

/// A `config.platform` entry: a version, or `false` to disable the package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Override {
    Version(String),
    Disabled,
}

#[derive(Debug, Default)]
pub struct PlatformRepository {
    pub packages: Vec<Package>,
    pub disabled: BTreeMap<String, Package>,
    overrides: BTreeMap<String, Override>,
    libraries: std::collections::HashSet<String>,
}

fn complete(name: &str, version: &str, pretty: &str, description: &str) -> Package {
    let mut p = Package::new(name, version, pretty);
    p.description = Some(description.to_string());
    p
}

/// Named groups of the first match, owned (the regexes here are built per call).
fn named_caps(re: &str, text: &str) -> Option<HashMap<String, String>> {
    let regex = Regex::new(re).ok()?;
    let caps = regex.captures(text)?;
    Some(
        regex
            .capture_names()
            .flatten()
            .filter_map(|name| {
                caps.name(name)
                    .map(|m| (name.to_string(), m.as_str().to_string()))
            })
            .collect(),
    )
}

/// `Version::convertAlphaVersionToIntVersion`
fn alpha_to_int(alpha: &str) -> i64 {
    alpha.len() as i64 * (-(b'a' as i64) + 1) + alpha.bytes().map(i64::from).sum::<i64>()
}

/// `Version::parseOpenssl`
pub fn parse_openssl(version: &str) -> Option<(String, bool)> {
    let re = Regex::new(r"^(?P<version>[0-9.]+)(?P<patch>[a-z]{0,2})(?P<suffix>(?:-?(?:dev|pre|alpha|beta|rc|fips)[\d]*)*)(?:-\w+)?(?: \(.+?\))?$").unwrap();
    let m = re.captures(version)?;
    let mut patch = String::new();
    if semver::version_compare(&m["version"], "3.0.0") == std::cmp::Ordering::Less {
        patch = format!(".{}", alpha_to_int(&m["patch"]));
    }
    let is_fips = m["suffix"].contains("fips");
    let suffix = format!("-{}", m["suffix"].trim_start_matches('-'))
        .replace("-fips", "")
        .replace("-pre", "-alpha");
    Some((
        format!("{}{patch}{suffix}", &m["version"])
            .trim_end_matches('-')
            .to_string(),
        is_fips,
    ))
}

/// `Version::parseLibjpeg`
fn parse_libjpeg(v: &str) -> Option<String> {
    let m = Regex::new(r"^(?P<major>\d+)(?P<minor>[a-z]*)$")
        .unwrap()
        .captures(v)?;
    Some(format!("{}.{}", &m["major"], alpha_to_int(&m["minor"])))
}

/// `Version::parseZoneinfoVersion`
fn parse_zoneinfo(v: &str) -> Option<String> {
    let m = Regex::new(r"^(?P<year>\d{4})(?P<revision>[a-z]*)$")
        .unwrap()
        .captures(v)?;
    Some(format!("{}.{}", &m["year"], alpha_to_int(&m["revision"])))
}

fn convert_version_id(id: i64, base: i64) -> String {
    format!(
        "{}.{}.{}",
        id / (base * base),
        (id / base) % base,
        id % base
    )
}

impl PlatformRepository {
    /// `new PlatformRepository([], $overrides)` with `$overrides` from `config.platform`.
    pub fn new(
        overrides: &Map<String, Value>,
        php: Option<&PhpInfo>,
    ) -> Result<PlatformRepository, String> {
        let mut repo = PlatformRepository::default();
        for (name, value) in overrides {
            if !is_platform_package(name) {
                return Err(format!(
                    "Invalid platform package name in config.platform: {name}"
                ));
            }
            let name = name.to_lowercase();
            match value {
                Value::Bool(false) => {
                    repo.overrides.insert(name, Override::Disabled);
                }
                Value::String(v) => {
                    repo.overrides
                        .insert(name.clone(), Override::Version(v.clone()));
                    repo.add_overridden(&name, v, None)?;
                }
                other => {
                    return Err(format!(
                        "config.platform.{name} : invalid value ({}): expected string or false",
                        crate::json::encode_compact(other)
                    ));
                }
            }
        }

        let composer_version = crate::COMPOSER_VERSION;
        repo.add(complete(
            "composer",
            &semver::normalize(composer_version, None)?,
            composer_version,
            "Composer package",
        ))?;
        repo.add(complete(
            "composer-plugin-api",
            &semver::normalize(crate::PLUGIN_API_VERSION, None)?,
            crate::PLUGIN_API_VERSION,
            "The Composer Plugin API",
        ))?;
        repo.add(complete(
            "composer-runtime-api",
            &semver::normalize(crate::RUNTIME_API_VERSION, None)?,
            crate::RUNTIME_API_VERSION,
            "The Composer Runtime API",
        ))?;

        let Some(php) = php else { return Ok(repo) };
        let (pretty, version) = match semver::normalize(&php.version, None) {
            Ok(v) => (php.version.clone(), v),
            Err(_) => {
                let pretty = Regex::new(r"^([^~+-]+).*$")
                    .unwrap()
                    .replace(&php.version, "$1")
                    .into_owned();
                let v = semver::normalize(&pretty, None)?;
                (pretty, v)
            }
        };
        repo.add(complete("php", &version, &pretty, "The PHP interpreter"))?;
        if php.constant_truthy("PHP_DEBUG") {
            repo.add(complete(
                "php-debug",
                &version,
                &pretty,
                "The PHP interpreter, with debugging symbols",
            ))?;
        }
        if php.constant_truthy("PHP_ZTS") {
            repo.add(complete(
                "php-zts",
                &version,
                &pretty,
                "The PHP interpreter, with Zend Thread Safety",
            ))?;
        }
        if php.constant_int("PHP_INT_SIZE") == Some(8) {
            repo.add(complete(
                "php-64bit",
                &version,
                &pretty,
                "The PHP interpreter, 64bit",
            ))?;
        }
        if php.extra.get("ipv6") == Some(&Value::Bool(true)) {
            repo.add(complete(
                "php-ipv6",
                &version,
                &pretty,
                "The PHP interpreter, with IPv6 support",
            ))?;
        }

        for name in &php.extensions {
            if name == "standard" || name == "Core" {
                continue;
            }
            let ext_version = php.ext_versions.get(name).cloned().unwrap_or_default();
            repo.add_extension(name, &ext_version)?;
        }

        for name in &php.extensions {
            repo.add_libraries_for(name, php)?;
        }

        if let Some(hhvm) = php.constant_str("HHVM_VERSION").or_else(hhvm_version) {
            let (pretty, version) = match semver::normalize(&hhvm, None) {
                Ok(v) => (hhvm.clone(), v),
                Err(_) => {
                    let pretty = Regex::new(r"^([^~+-]+).*$")
                        .unwrap()
                        .replace(&hhvm, "$1")
                        .into_owned();
                    let v = semver::normalize(&pretty, None)?;
                    (pretty, v)
                }
            };
            repo.add(complete(
                "hhvm",
                &version,
                &pretty,
                "The HHVM Runtime (64bit)",
            ))?;
        }
        Ok(repo)
    }

    fn add_libraries_for(&mut self, name: &str, php: &PhpInfo) -> Result<(), String> {
        let info = php.info.get(name).cloned().unwrap_or_default();
        let cap = |re: &str, text: &str, group: &str| -> Option<String> {
            named_caps(re, text).and_then(|m| m.get(group).cloned())
        };
        match name {
            "amqp" => {
                if let Some(v) = cap(
                    r"(?mi)^librabbitmq version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-librabbitmq"),
                        &v,
                        Some("AMQP librabbitmq version"),
                        &[],
                        &[],
                    )?;
                }
                if let Some(v) = cap(
                    r"(?mi)^AMQP protocol version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-protocol"),
                        &v.replace('-', "."),
                        Some("AMQP protocol version"),
                        &[],
                        &[],
                    )?;
                }
            }
            "bz2" => {
                if let Some(v) = cap(r"(?mi)^BZip2 Version => (?P<version>.*),", &info, "version") {
                    self.add_library(name, &v, None, &[], &[])?;
                }
            }
            "curl" => {
                if let Some(v) = cap(
                    r"(?mi)^cURL Information => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(name, &v, None, &[], &[])?;
                }
                if let Some(m) = named_caps(
                    r"(?mi)^SSL Version => (?P<library>[^\r\n/]+)/(?P<version>[^\r\n]+?)\r?$",
                    &info,
                ) {
                    let library = m["library"].to_lowercase();
                    let version = m["version"].to_string();
                    if library == "openssl" {
                        if let Some((parsed, fips)) = parse_openssl(&version) {
                            let provides: Vec<&str> =
                                if fips { vec!["curl-openssl"] } else { vec![] };
                            self.add_library(
                                &format!("{name}-openssl{}", if fips { "-fips" } else { "" }),
                                &parsed,
                                Some(&format!("curl OpenSSL version ({parsed})")),
                                &[],
                                &provides,
                            )?;
                        }
                    } else {
                        let (shortlib, ssl_lib) = if let Some(m2) =
                            library.strip_prefix("(securetransport) ").and_then(|r| {
                                Regex::new(r"^([a-z0-9]+)")
                                    .unwrap()
                                    .captures(r)
                                    .map(|c| c[1].to_string())
                            }) {
                            ("securetransport".to_string(), format!("curl-{m2}"))
                        } else {
                            (library.clone(), "curl-openssl".to_string())
                        };
                        self.add_library(
                            &format!("{name}-{shortlib}"),
                            &version,
                            Some(&format!("curl {library} version ({version})")),
                            &[ssl_lib.as_str()],
                            &[],
                        )?;
                    }
                }
                if let Some(m) = named_caps(
                    r"(?mi)^libSSH Version => (?P<library>[^\r\n/]+)/(?P<version>.+?)(?:/.*)?$",
                    &info,
                ) {
                    self.add_library(
                        &format!("{name}-{}", m["library"].to_lowercase()),
                        &m["version"],
                        Some(&format!("curl {} version", m["library"])),
                        &[],
                        &[],
                    )?;
                }
                if let Some(v) = cap(r"(?mi)^ZLib Version => (?P<version>.+)$", &info, "version") {
                    self.add_library(
                        &format!("{name}-zlib"),
                        &v,
                        Some("curl zlib version"),
                        &[],
                        &[],
                    )?;
                }
            }
            "date" => {
                if let Some(v) = cap(
                    r"(?mi)^timelib version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-timelib"),
                        &v,
                        Some("date timelib version"),
                        &[],
                        &[],
                    )?;
                }
                if let Some(source) = cap(
                    r"(?mi)^Timezone Database => (?P<source>internal|external)$",
                    &info,
                    "source",
                ) {
                    let external = source == "external";
                    if let Some(v) = cap(
                        r#"(?mi)^"Olson" Timezone Database Version => (?P<version>.+?)(?:\.system)?$"#,
                        &info,
                        "version",
                    ) {
                        if external && php.extensions.iter().any(|e| e == "timezonedb") {
                            self.add_library("timezonedb-zoneinfo", &v, Some("zoneinfo (\"Olson\") database for date (replaced by timezonedb)"), &[&format!("{name}-zoneinfo")], &[])?;
                        } else {
                            self.add_library(
                                &format!("{name}-zoneinfo"),
                                &v,
                                Some("zoneinfo (\"Olson\") database for date"),
                                &[],
                                &[],
                            )?;
                        }
                    }
                }
            }
            "fileinfo" => {
                if let Some(v) = cap(r"(?mi)^libmagic => (?P<version>.+)$", &info, "version") {
                    self.add_library(
                        &format!("{name}-libmagic"),
                        &v,
                        Some("fileinfo libmagic version"),
                        &[],
                        &[],
                    )?;
                }
            }
            "gd" => {
                if let Some(v) = php.constant_str("GD_VERSION") {
                    self.add_library(name, &v, None, &[], &[])?;
                }
                if let Some(v) = cap(
                    r"(?mi)^libJPEG Version => (?P<version>.+?)(?: compatible)?$",
                    &info,
                    "version",
                ) && let Some(parsed) = parse_libjpeg(&v)
                {
                    self.add_library(
                        &format!("{name}-libjpeg"),
                        &parsed,
                        Some("libjpeg version for gd"),
                        &[],
                        &[],
                    )?;
                }
                if let Some(v) = cap(
                    r"(?mi)^libPNG Version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-libpng"),
                        &v,
                        Some("libpng version for gd"),
                        &[],
                        &[],
                    )?;
                }
                if let Some(v) = cap(
                    r"(?mi)^FreeType Version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-freetype"),
                        &v,
                        Some("freetype version for gd"),
                        &[],
                        &[],
                    )?;
                }
                if let Some(v) = cap(
                    r"(?mi)^libXpm Version => (?P<versionId>\d+)$",
                    &info,
                    "versionId",
                ) {
                    self.add_library(
                        &format!("{name}-libxpm"),
                        &convert_version_id(v.parse().unwrap_or(0), 100),
                        Some("libxpm version for gd"),
                        &[],
                        &[],
                    )?;
                }
            }
            "gmp" => {
                if let Some(v) = php.constant_str("GMP_VERSION") {
                    self.add_library(name, &v, None, &[], &[])?;
                }
            }
            "iconv" => {
                if let Some(v) = php.constant_str("ICONV_VERSION") {
                    self.add_library(name, &v, None, &[], &[])?;
                }
            }
            "intl" => {
                let description = "The ICU unicode and globalization support library";
                if let Some(v) = php.constant_str("INTL_ICU_VERSION") {
                    self.add_library("icu", &v, Some(description), &[], &[])?;
                } else if let Some(v) =
                    cap(r"(?mi)^ICU version => (?P<version>.+)$", &info, "version")
                {
                    self.add_library("icu", &v, Some(description), &[], &[])?;
                }
                if let Some(v) = cap(
                    r"(?mi)^ICU TZData version => (?P<version>.*)$",
                    &info,
                    "version",
                ) && let Some(parsed) = parse_zoneinfo(&v)
                {
                    self.add_library(
                        "icu-zoneinfo",
                        &parsed,
                        Some("zoneinfo (\"Olson\") database for icu"),
                        &[],
                        &[],
                    )?;
                }
                if let Some(v) = php.extra.get("cldr").and_then(Value::as_str) {
                    self.add_library("icu-cldr", v, Some("ICU CLDR project version"), &[], &[])?;
                }
                if let Some(v) = php.extra.get("unicode").and_then(Value::as_str) {
                    self.add_library("icu-unicode", v, Some("ICU unicode version"), &[], &[])?;
                }
            }
            "imagick" => {
                if let Some(v) = php.extra.get("imagick").and_then(Value::as_str)
                    && let Some(m) =
                        named_caps(r"^ImageMagick (?P<version>[\d.]+)(?:-(?P<patch>\d+))?", v)
                {
                    let mut version = m["version"].to_string();
                    if let Some(p) = m.get("patch") {
                        version.push_str(&format!(".{p}"));
                    }
                    self.add_library(
                        &format!("{name}-imagemagick"),
                        &version,
                        None,
                        &["imagick"],
                        &[],
                    )?;
                }
            }
            "ldap" => {
                if let (Some(id), Some(vendor)) = (
                    cap(
                        r"(?mi)^Vendor Version => (?P<versionId>\d+)$",
                        &info,
                        "versionId",
                    ),
                    cap(r"(?mi)^Vendor Name => (?P<vendor>.+)$", &info, "vendor"),
                ) {
                    self.add_library(
                        &format!("{name}-{}", vendor.to_lowercase()),
                        &convert_version_id(id.parse().unwrap_or(0), 100),
                        Some(&format!("{vendor} version of ldap")),
                        &[],
                        &[],
                    )?;
                }
            }
            "libxml" => {
                let provides: Vec<String> = ["dom", "simplexml", "xml", "xmlreader", "xmlwriter"]
                    .iter()
                    .filter(|e| php.extensions.iter().any(|x| x == *e))
                    .map(|e| format!("{e}-libxml"))
                    .collect();
                let provides_ref: Vec<&str> = provides.iter().map(String::as_str).collect();
                if let Some(v) = php.constant_str("LIBXML_DOTTED_VERSION") {
                    self.add_library(name, &v, Some("libxml library version"), &[], &provides_ref)?;
                }
            }
            "mbstring" => {
                if let Some(v) = cap(
                    r"(?mi)^libmbfl version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-libmbfl"),
                        &v,
                        Some("mbstring libmbfl version"),
                        &[],
                        &[],
                    )?;
                }
                if php.version_id < 80600
                    && let Some(v) = php.constant_str("MB_ONIGURUMA_VERSION")
                {
                    self.add_library(
                        &format!("{name}-oniguruma"),
                        &v,
                        Some("mbstring oniguruma version"),
                        &[],
                        &[],
                    )?;
                } else if let Some(v) = cap(
                    r"(?mi)^(?:oniguruma|Multibyte regex \(oniguruma\)) version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-oniguruma"),
                        &v,
                        Some("mbstring oniguruma version"),
                        &[],
                        &[],
                    )?;
                }
            }
            "memcached" => {
                if let Some(v) = cap(
                    r"(?mi)^libmemcached version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-libmemcached"),
                        &v,
                        Some("libmemcached version"),
                        &[],
                        &[],
                    )?;
                }
            }
            "openssl" => {
                if let Some(text) = php.constant_str("OPENSSL_VERSION_TEXT")
                    && let Some(m) =
                        named_caps(r"(?i)^(?:OpenSSL|LibreSSL)?\s*(?P<version>\S+)", &text)
                    && let Some((parsed, fips)) = parse_openssl(&m["version"])
                {
                    let provides: Vec<&str> = if fips { vec![name] } else { vec![] };
                    self.add_library(
                        &format!("{name}{}", if fips { "-fips" } else { "" }),
                        &parsed,
                        Some(&text),
                        &[],
                        &provides,
                    )?;
                }
            }
            "pcre" => {
                if let Some(v) = php.constant_str("PCRE_VERSION") {
                    let first = Regex::new(r"^(\S+).*")
                        .unwrap()
                        .replace(&v, "$1")
                        .into_owned();
                    self.add_library(name, &first, None, &[], &[])?;
                }
                if let Some(v) = cap(
                    r"(?mi)^PCRE Unicode Version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-unicode"),
                        &v,
                        Some("PCRE Unicode version support"),
                        &[],
                        &[],
                    )?;
                }
            }
            "mysqlnd" | "pdo_mysql" => {
                if let Some(v) = cap(
                    r"(?mi)^(?:Client API version|Version) => mysqlnd (?P<version>.+?) ",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-mysqlnd"),
                        &v,
                        Some(&format!("mysqlnd library version for {name}")),
                        &[],
                        &[],
                    )?;
                }
            }
            "mongodb" => {
                if let Some(v) = cap(
                    r"(?mi)^libmongoc bundled version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-libmongoc"),
                        &v,
                        Some("libmongoc version of mongodb"),
                        &[],
                        &[],
                    )?;
                }
                if let Some(v) = cap(
                    r"(?mi)^libbson bundled version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-libbson"),
                        &v,
                        Some("libbson version of mongodb"),
                        &[],
                        &[],
                    )?;
                }
            }
            "pgsql" | "pdo_pgsql" => {
                if name == "pgsql"
                    && let Some(v) = php.constant_str("PGSQL_LIBPQ_VERSION")
                {
                    self.add_library("pgsql-libpq", &v, Some("libpq for pgsql"), &[], &[])?;
                } else if let Some(v) = cap(
                    r"(?mi)^PostgreSQL\(libpq\) Version => (?P<version>.*)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-libpq"),
                        &v,
                        Some(&format!("libpq for {name}")),
                        &[],
                        &[],
                    )?;
                }
            }
            "pq" => {
                if let Some(v) = cap(
                    r"(?mi)^libpq => (?P<compiled>.+) => (?P<linked>.+)$",
                    &info,
                    "linked",
                ) {
                    self.add_library(
                        &format!("{name}-libpq"),
                        &v,
                        Some(&format!("libpq for {name}")),
                        &[],
                        &[],
                    )?;
                }
            }
            "rdkafka" => {
                if let Some(v) = php.constant_int("RD_KAFKA_VERSION") {
                    self.add_library(
                        &format!("{name}-librdkafka"),
                        &format!(
                            "{}.{}.{}",
                            (v & 0x7F00_0000) >> 24,
                            (v & 0x00FF_0000) >> 16,
                            (v & 0x0000_FF00) >> 8
                        ),
                        Some(&format!("librdkafka for {name}")),
                        &[],
                        &[],
                    )?;
                }
            }
            "libsodium" | "sodium" => {
                if let Some(v) = php.constant_str("SODIUM_LIBRARY_VERSION") {
                    self.add_library("libsodium", &v, None, &[], &[])?;
                }
            }
            "sqlite3" | "pdo_sqlite" => {
                if let Some(v) = cap(
                    r"(?mi)^SQLite Library => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(&format!("{name}-sqlite"), &v, None, &[], &[])?;
                }
            }
            "ssh2" => {
                if let Some(v) = cap(
                    r"(?mi)^libssh2 version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(&format!("{name}-libssh2"), &v, None, &[], &[])?;
                }
            }
            "xsl" => {
                if let Some(v) = php.constant_str("LIBXSLT_DOTTED_VERSION") {
                    self.add_library("libxslt", &v, None, &["xsl"], &[])?;
                }
                if let Some(v) = cap(
                    r"(?mi)^libxslt compiled against libxml Version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        "libxslt-libxml",
                        &v,
                        Some("libxml version libxslt is compiled against"),
                        &[],
                        &[],
                    )?;
                }
            }
            "yaml" => {
                if let Some(v) = cap(
                    r"(?mi)^LibYAML Version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(
                        &format!("{name}-libyaml"),
                        &v,
                        Some("libyaml version of yaml"),
                        &[],
                        &[],
                    )?;
                }
            }
            "zip" => {
                if let Some(v) = php.constant_str("ZipArchive::LIBZIP_VERSION") {
                    self.add_library(&format!("{name}-libzip"), &v, None, &["zip"], &[])?;
                }
            }
            "zlib" => {
                if let Some(v) = php.constant_str("ZLIB_VERSION") {
                    self.add_library(name, &v, None, &[], &[])?;
                } else if let Some(v) = cap(
                    r"(?mi)^Linked Version => (?P<version>.+)$",
                    &info,
                    "version",
                ) {
                    self.add_library(name, &v, None, &[], &[])?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `PlatformRepository::addPackage` with the override rules.
    fn add(&mut self, package: Package) -> Result<(), String> {
        if let Some(o) = self.overrides.get(&package.name) {
            match o {
                Override::Disabled => {
                    let mut p = package;
                    p.description = Some(format!(
                        "{}. <warning>Package disabled via config.platform</warning>",
                        p.description.clone().unwrap_or_default()
                    ));
                    p.extra.insert("config.platform".into(), Value::Bool(true));
                    self.disabled.insert(p.name.clone(), p);
                }
                Override::Version(_) => {
                    if let Some(overrider) =
                        self.packages.iter_mut().find(|p| p.name == package.name)
                    {
                        let actual = if package.version == overrider.version {
                            "same as actual".to_string()
                        } else {
                            format!("actual: {}", package.pretty_version)
                        };
                        overrider.description = Some(format!(
                            "{}, {actual}",
                            overrider.description.clone().unwrap_or_default()
                        ));
                    }
                }
            }
            return Ok(());
        }
        if let Some(Override::Version(v)) = self.overrides.get("php").cloned()
            && package.name.starts_with("php-")
        {
            let overrider_version = semver::normalize(&v, None)?;
            let actual = if package.version == overrider_version {
                "same as actual".to_string()
            } else {
                format!("actual: {}", package.pretty_version)
            };
            self.add_overridden(&package.name, &v, Some(&actual))?;
            return Ok(());
        }
        self.packages.push(package);
        Ok(())
    }

    fn add_overridden(
        &mut self,
        name: &str,
        version: &str,
        actual: Option<&str>,
    ) -> Result<(), String> {
        let normalized = semver::normalize(version, None)
            .map_err(|e| format!("config.platform.{name} : invalid value ({version}): {e}"))?;
        let mut p = Package::new(name, &normalized, version);
        p.description = Some(match actual {
            Some(a) => format!("Package overridden via config.platform, {a}"),
            None => "Package overridden via config.platform".into(),
        });
        p.extra.insert("config.platform".into(), Value::Bool(true));
        self.packages.push(p);
        Ok(())
    }

    fn add_extension(&mut self, name: &str, pretty: &str) -> Result<(), String> {
        let mut pretty = pretty.to_string();
        let mut extra = String::new();
        let version = match semver::normalize(&pretty, None) {
            Ok(v) => v,
            Err(_) => {
                extra = format!(" (actual version: {pretty})");
                pretty = match Regex::new(r"^(\d+\.\d+\.\d+(?:\.\d+)?)")
                    .unwrap()
                    .captures(&pretty)
                {
                    Some(m) => m[1].to_string(),
                    None => "0".into(),
                };
                semver::normalize(&pretty, None)?
            }
        };
        let package_name = format!("ext-{}", name.to_lowercase().replace(' ', "-"));
        let mut ext = complete(
            &package_name,
            &version,
            &pretty,
            &format!("The {name} PHP extension{extra}"),
        );
        ext.kind = "php-ext".into();
        if name == "uuid" {
            ext.replaces.push(Link {
                source: "ext-uuid".into(),
                target: "lib-uuid".into(),
                constraint: Constraint::new("=", &version),
                pretty_constraint: pretty.clone(),
                kind: LinkType::Replace,
            });
        }
        self.add(ext)
    }

    fn add_library(
        &mut self,
        name: &str,
        pretty: &str,
        description: Option<&str>,
        replaces: &[&str],
        provides: &[&str],
    ) -> Result<(), String> {
        let Ok(version) = semver::normalize(pretty, None) else {
            return Ok(());
        };
        let full = format!("lib-{name}");
        if !is_platform_package(&full) || self.libraries.contains(&full) {
            return Ok(());
        }
        self.libraries.insert(full.clone());
        let mut lib = complete(
            &full,
            &version,
            pretty,
            description.unwrap_or(&format!("The {name} library")),
        );
        for r in replaces {
            let target = format!("lib-{}", r.to_lowercase());
            lib.replaces.push(Link {
                source: full.clone(),
                target,
                constraint: Constraint::new("=", &version),
                pretty_constraint: pretty.to_string(),
                kind: LinkType::Replace,
            });
        }
        for p in provides {
            let target = format!("lib-{}", p.to_lowercase());
            lib.provides.push(Link {
                source: full.clone(),
                target,
                constraint: Constraint::new("=", &version),
                pretty_constraint: pretty.to_string(),
                kind: LinkType::Provide,
            });
        }
        self.add(lib)
    }

    pub fn find(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }
}

/// `HhvmDetector::getVersion` for a separate hhvm binary.
fn hhvm_version() -> Option<String> {
    let out = Command::new("hhvm")
        .args(["--php", "-d", "hhvm.jit=0", "-r", "echo HHVM_VERSION;"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

/// `config.platform` as the repository expects it.
pub fn overrides_from_config(config: &crate::config::Config) -> Map<String, Value> {
    match config.config.get("platform") {
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    }
}

/// `PlatformRequirementFilterInterface` and its three implementations.
#[derive(Clone, Debug)]
pub enum RequirementFilter {
    IgnoreNothing,
    IgnoreAll,
    IgnoreList {
        ignore: Regex,
        ignore_upper_bound: Regex,
    },
}

/// `BasePackage::packageNamesToRegexp`
fn names_to_regex(names: &[String]) -> Regex {
    let parts: Vec<String> = names
        .iter()
        .map(|n| regex::escape(n).replace("\\*", ".*"))
        .collect();
    let pattern = if parts.is_empty() {
        "(?!)".to_string()
    } else {
        parts.join("|")
    };
    Regex::new(&format!("(?i)^(?:{pattern})$")).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

impl RequirementFilter {
    /// `PlatformRequirementFilterFactory::fromBoolOrList` from the two CLI options.
    pub fn from_options(ignore_all: bool, list: &[String]) -> RequirementFilter {
        if ignore_all {
            return RequirementFilter::IgnoreAll;
        }
        if list.is_empty() {
            return RequirementFilter::IgnoreNothing;
        }
        let mut all = Vec::new();
        let mut upper = Vec::new();
        for req in list {
            match req.strip_suffix('+') {
                Some(base) => upper.push(base.to_string()),
                None => all.push(req.clone()),
            }
        }
        RequirementFilter::IgnoreList {
            ignore: names_to_regex(&all),
            ignore_upper_bound: names_to_regex(&upper),
        }
    }

    pub fn is_ignored(&self, req: &str) -> bool {
        match self {
            RequirementFilter::IgnoreNothing => false,
            RequirementFilter::IgnoreAll => is_platform_package(req),
            RequirementFilter::IgnoreList { ignore, .. } => {
                is_platform_package(req) && ignore.is_match(req)
            }
        }
    }

    pub fn is_upper_bound_ignored(&self, req: &str) -> bool {
        match self {
            RequirementFilter::IgnoreList {
                ignore_upper_bound, ..
            } => {
                is_platform_package(req)
                    && (self.is_ignored(req) || ignore_upper_bound.is_match(req))
            }
            _ => self.is_ignored(req),
        }
    }

    /// `IgnoreListPlatformRequirementFilter::filterConstraint`
    pub fn filter_constraint(&self, req: &str, constraint: &Constraint) -> Constraint {
        let RequirementFilter::IgnoreList {
            ignore,
            ignore_upper_bound,
        } = self
        else {
            return constraint.clone();
        };
        if !is_platform_package(req) || !ignore_upper_bound.is_match(req) {
            return constraint.clone();
        }
        if ignore.is_match(req) {
            return Constraint::MatchAll;
        }
        let intervals = intervals::get(constraint);
        if let Some(last) = intervals.numeric.last()
            && last.end.to_string() != intervals::Interval::until_positive_infinity().to_string()
        {
            return Constraint::Multi {
                constraints: vec![
                    constraint.clone(),
                    Constraint::new(">=", last.end.version().unwrap_or("0")),
                ],
                conjunctive: false,
            };
        }
        constraint.clone()
    }
}

/// `IniHelper::getAll`: the loaded php.ini (or an empty string) followed by the scanned files.
pub fn ini_paths() -> Vec<String> {
    let out = std::process::Command::new("php")
        .args([
            "-r",
            "echo php_ini_loaded_file(), PHP_EOL, php_ini_scanned_files();",
        ])
        .output();
    let Ok(out) = out else {
        return vec![String::new()];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.splitn(2, '\n');
    let mut paths = vec![lines.next().unwrap_or("").trim().to_string()];
    if let Some(scanned) = lines.next() {
        paths.extend(
            scanned
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_helpers() {
        assert_eq!(parse_openssl("1.1.1g"), Some(("1.1.1.7".into(), false)));
        assert_eq!(parse_openssl("3.0.2"), Some(("3.0.2".into(), false)));
        assert_eq!(
            parse_openssl("1.0.2k-fips"),
            Some(("1.0.2.11".into(), true))
        );
        assert_eq!(
            parse_openssl("3.1.0-pre1"),
            Some(("3.1.0-alpha1".into(), false))
        );
        assert_eq!(parse_libjpeg("9d"), Some("9.4".into()));
        assert_eq!(parse_zoneinfo("2019c"), Some("2019.3".into()));
        assert_eq!(convert_version_id(20428, 100), "2.4.28");
    }

    #[test]
    fn overrides_and_filters() {
        let mut overrides = Map::new();
        overrides.insert("php".into(), Value::String("7.4.0".into()));
        overrides.insert("ext-nope".into(), Value::Bool(false));
        let php = PhpInfo {
            version: "8.3.1".into(),
            extensions: vec!["json".into(), "nope".into()],
            ext_versions: [
                ("json".to_string(), "8.3.1".to_string()),
                ("nope".to_string(), "1.0".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let repo = PlatformRepository::new(&overrides, Some(&php)).unwrap();
        let php_pkg = repo.find("php").unwrap();
        assert_eq!(php_pkg.pretty_version, "7.4.0");
        assert_eq!(
            php_pkg.description.as_deref(),
            Some("Package overridden via config.platform, actual: 8.3.1")
        );
        assert!(repo.find("ext-nope").is_none());
        assert!(repo.disabled.contains_key("ext-nope"));
        assert_eq!(repo.find("ext-json").unwrap().kind, "php-ext");
        assert!(repo.find("composer-plugin-api").is_some());

        let f = RequirementFilter::from_options(false, &["ext-*".into(), "php+".into()]);
        assert!(f.is_ignored("ext-gd"));
        assert!(!f.is_ignored("php"));
        assert!(f.is_upper_bound_ignored("php"));
        assert!(!f.is_ignored("vendor/pkg"));
        let c = semver::parse_constraints("^8.1").unwrap();
        assert_eq!(
            f.filter_constraint("php", &c).to_string(),
            "[[>= 8.1.0.0-dev < 9.0.0.0-dev] || >= 9.0.0.0-dev]"
        );
        assert_eq!(
            f.filter_constraint("ext-gd", &c),
            c,
            "only upper-bound ignores rewrite constraints"
        );
        assert!(RequirementFilter::IgnoreAll.is_ignored("php"));
    }
}
