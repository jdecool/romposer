//! Port of composer/semver's `VersionParser` plus the Composer subclass
//! (`Composer\Package\Version\VersionParser`).

use super::constraint::Constraint;
use regex::{Captures, Regex, RegexBuilder};
use std::sync::LazyLock;

const MODIFIER: &str =
    r"[._-]?(?:(stable|beta|b|RC|alpha|a|patch|pl|p)((?:[.-]?\d+)*)?)?([.-]?dev)?";
const STABILITIES: &str = "stable|RC|beta|alpha|dev";

/// `VersionParser::DEFAULT_BRANCH_ALIAS`
pub const DEFAULT_BRANCH_ALIAS: &str = "9999999-dev";

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}

fn re_i(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .unwrap()
}

static STABILITY_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| re_i(&format!("{MODIFIER}(?:\\+.*)?$")));
static HASH_REF: LazyLock<Regex> = LazyLock::new(|| re("#.+$"));
static ALIAS: LazyLock<Regex> = LazyLock::new(|| re(r"^([^,\s]+) +as +([^,\s]+)$"));
static STABILITY_FLAG: LazyLock<Regex> = LazyLock::new(|| re_i(&format!("@(?:{STABILITIES})$")));
static BUILD_METADATA: LazyLock<Regex> = LazyLock::new(|| re(r"^([^,\s+]+)\+[^\s]+$"));
static CLASSICAL: LazyLock<Regex> = LazyLock::new(|| {
    re_i(&format!(
        r"^v?(\d{{1,5}})(\.\d+)?(\.\d+)?(\.\d+)?{MODIFIER}$"
    ))
});
static DATE: LazyLock<Regex> = LazyLock::new(|| {
    re_i(&format!(
        r"^v?(\d{{4}}(?:[.:-]?\d{{2}}){{1,6}}(?:[.:-]?\d{{1,3}}){{0,2}}){MODIFIER}$"
    ))
});
static NON_DIGIT: LazyLock<Regex> = LazyLock::new(|| re(r"\D"));
static DEV_BRANCH: LazyLock<Regex> = LazyLock::new(|| re_i(r"^(.*?)[.-]?dev$"));
static NUMERIC_ALIAS: LazyLock<Regex> =
    LazyLock::new(|| re_i(r"^(?P<version>(\d+\.)*\d+)(?:\.x)?-dev$"));
static BRANCH: LazyLock<Regex> =
    LazyLock::new(|| re_i(r"^v?(\d+)(\.(?:\d+|[xX*]))?(\.(?:\d+|[xX*]))?(\.(?:\d+|[xX*]))?$"));
static OR_SPLIT: LazyLock<Regex> = LazyLock::new(|| re(r"\s*\|\|?\s*"));
static AND_SPLIT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?<!^|as|[=>< ,]) *(?<!-)[, ](?!-) *(?!,|as|$)").unwrap()
});
static CONSTRAINT_STABILITY: LazyLock<Regex> =
    LazyLock::new(|| re_i(&format!(r"^([^,\s]*?)@({STABILITIES})$")));
static DEV_REF: LazyLock<Regex> = LazyLock::new(|| re_i(r"^(dev-[^,\s@]+?|[^,\s@]+?\.x-dev)#.+$"));
static WILDCARD_ALL: LazyLock<Regex> = LazyLock::new(|| re_i(r"^(v)?[xX*](\.[xX*])*$"));
const VERSION_REGEX: &str = r"v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:\.(\d+))?(?:[._-]?(?:(stable|beta|b|RC|alpha|a|patch|pl|p)((?:[.-]?\d+)*)?)?([.-]?dev)?|\.([xX*][.-]?dev))(?:\+[^\s]+)?";
static TILDE: LazyLock<Regex> = LazyLock::new(|| re_i(&format!("^~>?{VERSION_REGEX}$")));
static CARET: LazyLock<Regex> = LazyLock::new(|| re_i(&format!(r"^\^{VERSION_REGEX}($)")));
static X_RANGE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:\.[xX*])+$"));
static HYPHEN: LazyLock<Regex> = LazyLock::new(|| {
    re_i(&format!(
        "^(?P<from>{VERSION_REGEX}) +- +(?P<to>{VERSION_REGEX})($)"
    ))
});
static BASIC: LazyLock<Regex> = LazyLock::new(|| re(r"^(<>|!=|>=?|<=?|==?)?\s*(.*)"));
static DEV_SUFFIX_MODIFIER: LazyLock<Regex> = LazyLock::new(|| re_i(&format!("-{MODIFIER}$")));
static SIMPLE_CHARS: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9a-zA-Z-./]+$"));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stability {
    Stable,
    RC,
    Beta,
    Alpha,
    Dev,
}

impl Stability {
    /// `BasePackage::STABILITIES` priority.
    pub fn priority(self) -> u8 {
        match self {
            Stability::Stable => 0,
            Stability::RC => 5,
            Stability::Beta => 10,
            Stability::Alpha => 15,
            Stability::Dev => 20,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Stability::Stable => "stable",
            Stability::RC => "RC",
            Stability::Beta => "beta",
            Stability::Alpha => "alpha",
            Stability::Dev => "dev",
        }
    }

    /// `VersionParser::normalizeStability`
    pub fn parse(stability: &str) -> Result<Stability, String> {
        Ok(match stability.to_lowercase().as_str() {
            "stable" => Stability::Stable,
            "rc" => Stability::RC,
            "beta" => Stability::Beta,
            "alpha" => Stability::Alpha,
            "dev" => Stability::Dev,
            other => {
                return Err(format!(
                    "Invalid stability string \"{other}\", expected one of stable, RC, beta, alpha or dev"
                ));
            }
        })
    }
}

fn group<'a>(caps: &'a Captures<'_>, i: usize) -> &'a str {
    caps.get(i).map_or("", |m| m.as_str())
}

/// `VersionParser::parseStability`
pub fn parse_stability(version: &str) -> Stability {
    let version = HASH_REF.replace(version, "");
    if version.starts_with("dev-") || version.ends_with("-dev") {
        return Stability::Dev;
    }
    let lower = version.to_lowercase();
    if let Some(m) = STABILITY_SUFFIX.captures(&lower) {
        if !group(&m, 3).is_empty() {
            return Stability::Dev;
        }
        match group(&m, 1) {
            "beta" | "b" => return Stability::Beta,
            "alpha" | "a" => return Stability::Alpha,
            "rc" => return Stability::RC,
            _ => {}
        }
    }
    Stability::Stable
}

pub fn is_valid(version: &str) -> bool {
    normalize(version, None).is_ok()
}

/// `VersionParser::normalize($version, $fullVersion)`
pub fn normalize(version: &str, full_version: Option<&str>) -> Result<String, String> {
    let mut version = version.trim().to_string();
    let orig_version = version.clone();
    let full_version = full_version.map_or_else(|| version.clone(), str::to_string);

    if let Some(m) = ALIAS.captures(&version) {
        version = m[1].to_string();
    }
    if let Some(m) = STABILITY_FLAG.find(&version) {
        version.truncate(m.start());
    }
    if matches!(version.as_str(), "master" | "trunk" | "default") {
        version = format!("dev-{version}");
    }
    if version.len() >= 4 && version[..4].eq_ignore_ascii_case("dev-") {
        return Ok(format!("dev-{}", &version[4..]));
    }
    if let Some(m) = BUILD_METADATA.captures(&version) {
        version = m[1].to_string();
    }

    let mut matched: Option<(String, Captures<'_>, usize)> = None;
    let version_for_match = version.clone();
    if let Some(m) = CLASSICAL.captures(&version_for_match) {
        let v = format!(
            "{}{}{}{}",
            &m[1],
            if group(&m, 2).is_empty() {
                ".0"
            } else {
                group(&m, 2)
            },
            if group(&m, 3).is_empty() {
                ".0"
            } else {
                group(&m, 3)
            },
            if group(&m, 4).is_empty() {
                ".0"
            } else {
                group(&m, 4)
            },
        );
        matched = Some((v, m, 5));
    } else if let Some(m) = DATE.captures(&version_for_match) {
        let v = NON_DIGIT.replace_all(&m[1], ".").into_owned();
        matched = Some((v, m, 2));
    }

    if let Some((mut v, m, index)) = matched {
        if !group(&m, index).is_empty() {
            if group(&m, index) == "stable" {
                return Ok(v);
            }
            v.push('-');
            v.push_str(expand_stability(group(&m, index)));
            v.push_str(group(&m, index + 1).trim_start_matches(['.', '-']));
        }
        if !group(&m, index + 2).is_empty() {
            v.push_str("-dev");
        }
        return Ok(v);
    }

    if let Some(m) = DEV_BRANCH.captures(&version) {
        let normalized = normalize_branch(&m[1]);
        if !normalized.contains("dev-") {
            return Ok(normalized);
        }
    }

    let quoted = regex::escape(&version);
    let mut extra = String::new();
    if re(&format!(" +as +{quoted}(?:@(?:{STABILITIES}))?$")).is_match(&full_version) {
        extra = format!(" in \"{full_version}\", the alias must be an exact version");
    } else if re(&format!("^{quoted}(?:@(?:{STABILITIES}))? +as +")).is_match(&full_version) {
        extra = format!(
            " in \"{full_version}\", the alias source must be an exact version, if it is a branch name you should prefix it with dev-"
        );
    }
    Err(format!("Invalid version string \"{orig_version}\"{extra}"))
}

/// `VersionParser::parseNumericAliasPrefix`
pub fn parse_numeric_alias_prefix(branch: &str) -> Option<String> {
    NUMERIC_ALIAS
        .captures(branch)
        .map(|m| format!("{}.", &m["version"]))
}

/// `VersionParser::normalizeBranch`
pub fn normalize_branch(name: &str) -> String {
    let name = name.trim();
    if let Some(m) = BRANCH.captures(name) {
        let mut version = String::new();
        for i in 1..5 {
            match m.get(i) {
                Some(part) => version.push_str(&part.as_str().replace(['*', 'X'], "x")),
                None => version.push_str(".x"),
            }
        }
        return format!("{}-dev", version.replace('x', "9999999"));
    }
    format!("dev-{name}")
}

/// `VersionParser::parseConstraints`; the pretty string is the trimmed input.
pub fn parse_constraints(constraints: &str) -> Result<Constraint, String> {
    let mut or_groups = Vec::new();
    for or_constraint in OR_SPLIT.split(constraints.trim()) {
        let and_constraints: Vec<&str> =
            AND_SPLIT.split(or_constraint).map(|r| r.unwrap()).collect();
        let mut objects = Vec::new();
        for c in and_constraints {
            objects.extend(parse_constraint(c)?);
        }
        or_groups.push(if objects.len() == 1 {
            objects.pop().unwrap()
        } else {
            Constraint::multi(objects, true)
        });
    }
    Ok(Constraint::multi(or_groups, false))
}

/// The hyphen-range `$empty` closure: `"0"` counts as present, only a missing group is empty.
fn empty(s: &str) -> bool {
    s.is_empty()
}

fn parse_constraint(constraint: &str) -> Result<Vec<Constraint>, String> {
    let mut constraint = constraint.to_string();
    if let Some(m) = ALIAS.captures(&constraint) {
        constraint = m[1].to_string();
    }
    let mut stability_modifier: Option<String> = None;
    if let Some(m) = CONSTRAINT_STABILITY.captures(&constraint.clone()) {
        constraint = if m[1].is_empty() {
            "*".into()
        } else {
            m[1].to_string()
        };
        if !m[2].eq_ignore_ascii_case("stable") {
            stability_modifier = Some(m[2].to_string());
        }
    }
    if let Some(m) = DEV_REF.captures(&constraint.clone()) {
        constraint = m[1].to_string();
    }
    if let Some(m) = WILDCARD_ALL.captures(&constraint) {
        if m.get(1).is_some() || m.get(2).is_some() {
            return Ok(vec![Constraint::new(">=", "0.0.0.0-dev")]);
        }
        return Ok(vec![Constraint::MatchAll]);
    }

    // Tilde range
    if let Some(m) = TILDE.captures(&constraint) {
        if constraint.starts_with("~>") {
            return Err(format!(
                "Could not parse version constraint {constraint}: Invalid operator \"~>\", you probably meant to use the \"~\" operator"
            ));
        }
        let mut position: usize = if !group(&m, 4).is_empty() {
            4
        } else if !group(&m, 3).is_empty() {
            3
        } else if !group(&m, 2).is_empty() {
            2
        } else {
            1
        };
        if !group(&m, 8).is_empty() {
            position += 1;
        }
        let stability_suffix =
            if group(&m, 5).is_empty() && group(&m, 7).is_empty() && group(&m, 8).is_empty() {
                "-dev"
            } else {
                ""
            };
        let low_version = normalize(&format!("{constraint}{stability_suffix}")[1..], None)?;
        let high_position = position.saturating_sub(1).max(1);
        let high_version = format!(
            "{}-dev",
            manipulate_version_string(&parts(&m, 1), high_position, 1).unwrap()
        );
        return Ok(vec![
            Constraint::new(">=", &low_version),
            Constraint::new("<", &high_version),
        ]);
    }

    // Caret range
    if let Some(m) = CARET.captures(&constraint) {
        let position = if &m[1] != "0" || group(&m, 2).is_empty() {
            1
        } else if group(&m, 2) != "0" || group(&m, 3).is_empty() {
            2
        } else {
            3
        };
        let stability_suffix =
            if group(&m, 5).is_empty() && group(&m, 7).is_empty() && group(&m, 8).is_empty() {
                "-dev"
            } else {
                ""
            };
        let low_version = normalize(&format!("{constraint}{stability_suffix}")[1..], None)?;
        let high_version = format!(
            "{}-dev",
            manipulate_version_string(&parts(&m, 1), position, 1).unwrap()
        );
        return Ok(vec![
            Constraint::new(">=", &low_version),
            Constraint::new("<", &high_version),
        ]);
    }

    // X range
    if let Some(m) = X_RANGE.captures(&constraint) {
        let position = if !group(&m, 3).is_empty() {
            3
        } else if !group(&m, 2).is_empty() {
            2
        } else {
            1
        };
        let p = parts(&m, 1);
        let low_version = format!(
            "{}-dev",
            manipulate_version_string(&p, position, 0).unwrap()
        );
        let high_version = format!(
            "{}-dev",
            manipulate_version_string(&p, position, 1).unwrap()
        );
        if low_version == "0.0.0.0-dev" {
            return Ok(vec![Constraint::new("<", &high_version)]);
        }
        return Ok(vec![
            Constraint::new(">=", &low_version),
            Constraint::new("<", &high_version),
        ]);
    }

    // Hyphen range
    if let Some(m) = HYPHEN.captures(&constraint) {
        let low_stability_suffix =
            if group(&m, 6).is_empty() && group(&m, 8).is_empty() && group(&m, 9).is_empty() {
                "-dev"
            } else {
                ""
            };
        let low_version = normalize(&m["from"], None)?;
        let lower = Constraint::new(">=", &format!("{low_version}{low_stability_suffix}"));
        let upper = if (!empty(group(&m, 12)) && !empty(group(&m, 13)))
            || !group(&m, 15).is_empty()
            || !group(&m, 17).is_empty()
            || !group(&m, 18).is_empty()
        {
            Constraint::new("<=", &normalize(&m["to"], None)?)
        } else {
            let high_match = [group(&m, 11), group(&m, 12), group(&m, 13), group(&m, 14)];
            normalize(&m["to"], None)?;
            let pos = if empty(group(&m, 12)) { 1 } else { 2 };
            Constraint::new(
                "<",
                &format!(
                    "{}-dev",
                    manipulate_version_string(&high_match, pos, 1).unwrap()
                ),
            )
        };
        return Ok(vec![lower, upper]);
    }

    // Basic comparators
    let mut error: Option<String> = None;
    if let Some(m) = BASIC.captures(&constraint) {
        let op_str = group(&m, 1);
        let rest = &m[2];
        let normalized = match normalize(rest, None) {
            Ok(v) => Ok(v),
            Err(e) => {
                if rest.ends_with("-dev") && SIMPLE_CHARS.is_match(rest) {
                    normalize(&format!("dev-{}", &rest[..rest.len() - 4]), None)
                } else {
                    Err(e)
                }
            }
        };
        match normalized {
            Ok(mut version) => {
                let op = if op_str.is_empty() { "=" } else { op_str };
                if op != "=="
                    && op != "="
                    && stability_modifier.is_some()
                    && parse_stability(&version) == Stability::Stable
                {
                    version.push('-');
                    version.push_str(stability_modifier.as_deref().unwrap());
                } else if (op == "<" || op == ">=")
                    && !DEV_SUFFIX_MODIFIER.is_match(rest)
                    && !rest.starts_with("dev-")
                {
                    version.push_str("-dev");
                }
                return Ok(vec![Constraint::new(op, &version)]);
            }
            Err(e) => error = Some(e),
        }
    }

    let mut message = format!("Could not parse version constraint {constraint}");
    if let Some(e) = error {
        message.push_str(": ");
        message.push_str(&e);
    }
    Err(message)
}

fn parts<'a>(m: &'a Captures<'_>, first: usize) -> [&'a str; 4] {
    [
        group(m, first),
        group(m, first + 1),
        group(m, first + 2),
        group(m, first + 3),
    ]
}

/// `VersionParser::manipulateVersionString` with the default `'0'` pad.
fn manipulate_version_string(
    matches: &[&str; 4],
    position: usize,
    increment: i64,
) -> Option<String> {
    let mut out: [String; 4] = std::array::from_fn(|i| matches[i].to_string());
    let mut position = position;
    for i in (1..=4).rev() {
        if i > position {
            out[i - 1] = "0".into();
        } else if i == position && increment != 0 {
            let n: i64 = out[i - 1].parse().unwrap_or(0) + increment;
            if n < 0 {
                out[i - 1] = "0".into();
                position -= 1;
                if i == 1 {
                    return None;
                }
            } else {
                out[i - 1] = n.to_string();
            }
        }
    }
    Some(out.join("."))
}

fn expand_stability(stability: &str) -> &str {
    match stability.to_lowercase().as_str() {
        "a" => "alpha",
        "b" => "beta",
        "p" | "pl" => "patch",
        "rc" => "RC",
        "stable" => "stable",
        "beta" => "beta",
        "alpha" => "alpha",
        "patch" => "patch",
        "dev" => "dev",
        _ => unreachable!("modifier regex only matches known stabilities"),
    }
}

/// `VersionParser::isUpgrade`
pub fn is_upgrade(normalized_from: &str, normalized_to: &str) -> bool {
    if normalized_from == normalized_to {
        return true;
    }
    let alias = |v: &str| {
        if matches!(v, "dev-master" | "dev-trunk" | "dev-default") {
            DEFAULT_BRANCH_ALIAS.to_string()
        } else {
            v.to_string()
        }
    };
    let (from, to) = (alias(normalized_from), alias(normalized_to));
    if from.starts_with("dev-") || to.starts_with("dev-") {
        return true;
    }
    super::compare::version_compare(&from, &to) != std::cmp::Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_alias_prefix() {
        let cases = [
            ("0.x-dev", Some("0.")),
            ("1.0.x-dev", Some("1.0.")),
            ("1.x-dev", Some("1.")),
            ("1.2.x-dev", Some("1.2.")),
            ("1.2-dev", Some("1.2.")),
            ("1-dev", Some("1.")),
            ("dev-develop", None),
            ("dev-master", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_numeric_alias_prefix(input).as_deref(),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn validity() {
        for (input, expected) in [
            ("0.x-dev", true),
            ("dev-develop", true),
            ("1.0.2", true),
            ("1.0.2.5", true),
            ("1.0.2.5.5", false),
            ("foo", false),
        ] {
            assert_eq!(is_valid(input), expected, "{input}");
        }
    }

    #[test]
    fn normalize_succeeds() {
        let cases = [
            ("1.0.0", "1.0.0.0"),
            ("1.2.3.4", "1.2.3.4"),
            ("1.0.0RC1dev", "1.0.0.0-RC1-dev"),
            ("1.0.0-rC15-dev", "1.0.0.0-RC15-dev"),
            ("1.0.0.RC.15-dev", "1.0.0.0-RC15-dev"),
            ("1.0.0-rc1", "1.0.0.0-RC1"),
            ("1.0.0.pl3-dev", "1.0.0.0-patch3-dev"),
            ("1.0-dev", "1.0.0.0-dev"),
            ("0", "0.0.0.0"),
            ("99999", "99999.0.0.0"),
            ("10.4.13-beta", "10.4.13.0-beta"),
            ("10.4.13beta2", "10.4.13.0-beta2"),
            ("10.4.13beta.2", "10.4.13.0-beta2"),
            ("v1.13.11-beta.0", "1.13.11.0-beta0"),
            ("1.13.11.0-beta0", "1.13.11.0-beta0"),
            ("10.4.13-b", "10.4.13.0-beta"),
            ("10.4.13-b5", "10.4.13.0-beta5"),
            ("v1.0.0", "1.0.0.0"),
            ("2010.01", "2010.01.0.0"),
            ("2010.01.02", "2010.01.02.0"),
            ("2010.1.555", "2010.1.555.0"),
            ("2010.10.200", "2010.10.200.0"),
            ("20230131.0.0", "20230131.0.0"),
            ("202301310000.0.0", "202301310000.0.0"),
            ("v20100102", "20100102"),
            ("20100102", "20100102"),
            ("20100102.0", "20100102.0"),
            ("20100102.1.0", "20100102.1.0"),
            ("20100102.0.3", "20100102.0.3"),
            ("100000", "100000"),
            ("2010-01-02-10-20-30.0.3", "2010.01.02.10.20.30.0.3"),
            ("2010-01-02-10-20-30.5", "2010.01.02.10.20.30.5"),
            ("2010-01-02", "2010.01.02"),
            ("2012.06.07", "2012.06.07.0"),
            ("2010-01-02.5", "2010.01.02.5"),
            ("20100102-203040", "20100102.203040"),
            ("20100102.x-dev", "20100102.9999999.9999999.9999999-dev"),
            (
                "20100102.203040.x-dev",
                "20100102.203040.9999999.9999999-dev",
            ),
            ("20100102203040-10", "20100102203040.10"),
            ("20100102-203040-p1", "20100102.203040-patch1"),
            ("201903.0", "201903.0"),
            ("201903.x-dev", "201903.9999999.9999999.9999999-dev"),
            ("201903.0-p2", "201903.0-patch2"),
            ("dev-master", "dev-master"),
            ("master", "dev-master"),
            ("dev-trunk", "dev-trunk"),
            ("1.x-dev", "1.9999999.9999999.9999999-dev"),
            ("dev-feature-foo", "dev-feature-foo"),
            ("DEV-FOOBAR", "dev-FOOBAR"),
            ("dev-feature/foo", "dev-feature/foo"),
            ("dev-feature+issue-1", "dev-feature+issue-1"),
            ("dev-master as 1.0.0", "dev-master"),
            (
                "dev-load-varnish-only-when-used as ^2.0",
                "dev-load-varnish-only-when-used",
            ),
            (
                "dev-load-varnish-only-when-used@dev as ^2.0@dev",
                "dev-load-varnish-only-when-used",
            ),
            ("1.0.0+foo@dev", "1.0.0.0"),
            (
                "dev-load-varnish-only-when-used@stable",
                "dev-load-varnish-only-when-used",
            ),
            ("1.0.0-beta.5+foo", "1.0.0.0-beta5"),
            ("1.0.0+foo", "1.0.0.0"),
            ("1.0.0-alpha.3.1+foo", "1.0.0.0-alpha3.1"),
            ("1.0.0-alpha2.1+foo", "1.0.0.0-alpha2.1"),
            ("1.0.0-alpha-2.1-3+foo", "1.0.0.0-alpha2.1-3"),
            ("1.0.0+foo as 2.0", "1.0.0.0"),
            ("00.01.03.04", "00.01.03.04"),
            ("000.001.003.004", "000.001.003.004"),
            ("0.000.103.204", "0.000.103.204"),
            ("0700", "0700.0.0.0"),
            ("041.x-dev", "041.9999999.9999999.9999999-dev"),
            ("dev-041.003", "dev-041.003"),
            ("dev-1.0.0-dev<1.0.5-dev", "dev-1.0.0-dev<1.0.5-dev"),
            ("dev-foo bar", "dev-foo bar"),
            (" 1.0.0", "1.0.0.0"),
            ("1.0.0 ", "1.0.0.0"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize(input, None).as_deref(), Ok(expected), "{input}");
        }
    }

    #[test]
    fn normalize_fails() {
        for input in [
            "",
            "a",
            "1.0.0-meh",
            "1.0.0.0.0",
            "feature-foo",
            "1.0.0+foo bar",
            "1.0.1-SNAPSHOT",
            "1.0.0<1.0.5-dev",
            "1.0.0-dev<1.0.5-dev",
            "foo bar-dev",
            "1.0 .2",
            " as ",
            " as 1.2",
            "^",
            "^8 || ^",
            "~",
            "~1 ~",
            "~1",
            "^1",
            "1.*",
            "20100102.0.3.4",
            "100000.0.0.0",
            "2023013.0.0",
            "202301311.0.0",
            "20230131000.0.0",
            "2023013100000.0.0",
        ] {
            assert!(normalize(input, None).is_err(), "{input:?} should fail");
        }
    }

    #[test]
    fn normalize_reports_alias_issues() {
        let err = normalize("^2.0", Some("1.0.0+foo as ^2.0")).unwrap_err();
        assert_eq!(
            err,
            "Invalid version string \"^2.0\" in \"1.0.0+foo as ^2.0\", the alias must be an exact version"
        );
        let err = normalize("<2.0@dev", Some("<2.0@dev as 1.2.3@dev")).unwrap_err();
        assert_eq!(
            err,
            "Invalid version string \"<2.0@dev\" in \"<2.0@dev as 1.2.3@dev\", the alias source must be an exact version, if it is a branch name you should prefix it with dev-"
        );
    }

    #[test]
    fn normalize_branches() {
        for (input, expected) in [
            ("v1.x", "1.9999999.9999999.9999999-dev"),
            ("v1.*", "1.9999999.9999999.9999999-dev"),
            ("v1.0", "1.0.9999999.9999999-dev"),
            ("2.0", "2.0.9999999.9999999-dev"),
            ("v1.0.x", "1.0.9999999.9999999-dev"),
            ("v1.0.3.*", "1.0.3.9999999-dev"),
            ("v2.4.0", "2.4.0.9999999-dev"),
            ("2.4.4", "2.4.4.9999999-dev"),
            ("master", "dev-master"),
            ("trunk", "dev-trunk"),
            ("feature-a", "dev-feature-a"),
            ("FOOBAR", "dev-FOOBAR"),
            ("feature+issue-1", "dev-feature+issue-1"),
        ] {
            assert_eq!(normalize_branch(input), expected, "{input}");
        }
    }

    fn parsed(input: &str) -> String {
        parse_constraints(input)
            .unwrap_or_else(|e| panic!("{input}: {e}"))
            .to_string()
    }

    #[test]
    fn parse_constraints_table() {
        let cases = [
            ("1.0@dev", "== 1.0.0.0"),
            (">=1.0@beta", ">= 1.0.0.0-beta"),
            (
                "dev-load-varnish-only-when-used as ^2.0@dev",
                "== dev-load-varnish-only-when-used",
            ),
            (
                "dev-load-varnish-only-when-used@dev as ^2.0@dev",
                "== dev-load-varnish-only-when-used",
            ),
            ("1.0.x-dev#abcd123", "== 1.0.9999999.9999999-dev"),
            ("1.0.x-dev#trunk/@123", "== 1.0.9999999.9999999-dev"),
            ("*", "*"),
            ("v*", ">= 0.0.0.0-dev"),
            ("*.*", ">= 0.0.0.0-dev"),
            ("v*.*", ">= 0.0.0.0-dev"),
            ("*.x.*", ">= 0.0.0.0-dev"),
            ("x.X.x.*", ">= 0.0.0.0-dev"),
            ("<>1.0.0", "!= 1.0.0.0"),
            ("!=1.0.0", "!= 1.0.0.0"),
            (">1.0.0", "> 1.0.0.0"),
            ("<1.2.3.4", "< 1.2.3.4-dev"),
            ("<=1.2.3", "<= 1.2.3.0"),
            (">=1.2.3", ">= 1.2.3.0-dev"),
            ("=1.2.3", "== 1.2.3.0"),
            ("==1.2.3", "== 1.2.3.0"),
            ("1.2.3", "== 1.2.3.0"),
            ("=1.0", "== 1.0.0.0"),
            ("1.2.3b5", "== 1.2.3.0-beta5"),
            ("1.2.3a1", "== 1.2.3.0-alpha1"),
            ("1.2.3p1234", "== 1.2.3.0-patch1234"),
            ("1.2.3pl1234", "== 1.2.3.0-patch1234"),
            (">= 1.2.3", ">= 1.2.3.0-dev"),
            ("< 1.2.3", "< 1.2.3.0-dev"),
            ("> 1.2.3", "> 1.2.3.0"),
            (">=dev-master", ">= dev-master"),
            ("dev-master", "== dev-master"),
            ("dev-feature-a", "== dev-feature-a"),
            ("dev-some-fix", "== dev-some-fix"),
            ("dev-CAPS", "== dev-CAPS"),
            ("dev-master as 1.0.0", "== dev-master"),
            ("<1.2.3.4-stable", "< 1.2.3.4"),
            (">=1.2.3.4-stable", ">= 1.2.3.4"),
            ("<1.2.3-beta1", "< 1.2.3.0-beta1"),
            (">=1.2.3-alpha1", ">= 1.2.3.0-alpha1"),
            ("<1.2.3-RC1", "< 1.2.3.0-RC1"),
            (">=1.2.3-RC1", ">= 1.2.3.0-RC1"),
            (">=1.2.3-rc1", ">= 1.2.3.0-RC1"),
            // wildcards
            ("v2.*", "[>= 2.0.0.0-dev < 3.0.0.0-dev]"),
            ("2.*.*", "[>= 2.0.0.0-dev < 3.0.0.0-dev]"),
            ("20.*", "[>= 20.0.0.0-dev < 21.0.0.0-dev]"),
            ("2.0.*", "[>= 2.0.0.0-dev < 2.1.0.0-dev]"),
            ("2.x", "[>= 2.0.0.0-dev < 3.0.0.0-dev]"),
            ("2.2.x", "[>= 2.2.0.0-dev < 2.3.0.0-dev]"),
            ("2.10.X", "[>= 2.10.0.0-dev < 2.11.0.0-dev]"),
            ("2.1.3.*", "[>= 2.1.3.0-dev < 2.1.4.0-dev]"),
            ("0.*", "< 1.0.0.0-dev"),
            ("0.x.x", "< 1.0.0.0-dev"),
            // tilde
            ("~v1", "[>= 1.0.0.0-dev < 2.0.0.0-dev]"),
            ("~1.0", "[>= 1.0.0.0-dev < 2.0.0.0-dev]"),
            ("~1.0.0", "[>= 1.0.0.0-dev < 1.1.0.0-dev]"),
            ("~1.2.3", "[>= 1.2.3.0-dev < 1.3.0.0-dev]"),
            ("~1.2.3.4", "[>= 1.2.3.4-dev < 1.2.4.0-dev]"),
            ("~1.2-beta", "[>= 1.2.0.0-beta < 2.0.0.0-dev]"),
            ("~1.2-b2", "[>= 1.2.0.0-beta2 < 2.0.0.0-dev]"),
            ("~1.2-BETA2", "[>= 1.2.0.0-beta2 < 2.0.0.0-dev]"),
            ("~1.2.2-dev", "[>= 1.2.2.0-dev < 1.3.0.0-dev]"),
            ("~1.2.2-stable", "[>= 1.2.2.0 < 1.3.0.0-dev]"),
            ("~201903.0", "[>= 201903.0-dev < 201904.0.0.0-dev]"),
            ("~201903.0-beta", "[>= 201903.0-beta < 201904.0.0.0-dev]"),
            (
                "~201903.205830.1-stable",
                "[>= 201903.205830.1 < 201903.205831.0.0-dev]",
            ),
            (
                "~2.x-dev",
                "[>= 2.9999999.9999999.9999999-dev < 3.0.0.0-dev]",
            ),
            ("~2.0.x-dev", "[>= 2.0.9999999.9999999-dev < 2.1.0.0-dev]"),
            ("~2.0.3.x-dev", "[>= 2.0.3.9999999-dev < 2.0.4.0-dev]"),
            (
                "~0.x-dev",
                "[>= 0.9999999.9999999.9999999-dev < 1.0.0.0-dev]",
            ),
            // caret
            ("^v1", "[>= 1.0.0.0-dev < 2.0.0.0-dev]"),
            ("^0", "[>= 0.0.0.0-dev < 1.0.0.0-dev]"),
            ("^0.0", "[>= 0.0.0.0-dev < 0.1.0.0-dev]"),
            ("^1.2", "[>= 1.2.0.0-dev < 2.0.0.0-dev]"),
            ("^1.2.3-beta.2", "[>= 1.2.3.0-beta2 < 2.0.0.0-dev]"),
            ("^1.2.3.4", "[>= 1.2.3.4-dev < 2.0.0.0-dev]"),
            ("^0.2.3", "[>= 0.2.3.0-dev < 0.3.0.0-dev]"),
            ("^0.0.3", "[>= 0.0.3.0-dev < 0.0.4.0-dev]"),
            ("^0.0.3-alpha", "[>= 0.0.3.0-alpha < 0.0.4.0-dev]"),
            ("^0.0.3-stable", "[>= 0.0.3.0 < 0.0.4.0-dev]"),
            (
                "^201903.205830.1-stable",
                "[>= 201903.205830.1 < 201904.0.0.0-dev]",
            ),
            (
                "^2.x-dev",
                "[>= 2.9999999.9999999.9999999-dev < 3.0.0.0-dev]",
            ),
            ("^2.0.*-dev", "[>= 2.0.9999999.9999999-dev < 3.0.0.0-dev]"),
            ("^2.0.3.x-dev", "[>= 2.0.3.9999999-dev < 3.0.0.0-dev]"),
            // hyphen
            ("v1 - v2", "[>= 1.0.0.0-dev < 3.0.0.0-dev]"),
            ("1.2.3 - 2.3.4.5", "[>= 1.2.3.0-dev <= 2.3.4.5]"),
            ("1.2-beta - 2.3", "[>= 1.2.0.0-beta < 2.4.0.0-dev]"),
            ("1.2-beta - 2.3-dev", "[>= 1.2.0.0-beta <= 2.3.0.0-dev]"),
            ("1.2-RC - 2.3.1", "[>= 1.2.0.0-RC <= 2.3.1.0]"),
            ("1 - 2.0", "[>= 1.0.0.0-dev < 2.1.0.0-dev]"),
            ("1.3 - 2.1.3", "[>= 1.3.0.0-dev <= 2.1.3.0]"),
            (
                "2.0.3.x-dev - 3.0.3.x-dev",
                "[>= 2.0.3.9999999-dev <= 3.0.3.9999999-dev]",
            ),
            (
                "0.x-dev - 1.x-dev",
                "[>= 0.9999999.9999999.9999999-dev <= 1.9999999.9999999.9999999-dev]",
            ),
            // misc
            ("3.x-dev", "== 3.9999999.9999999.9999999-dev"),
            ("3-dev", "== 3.0.0.0-dev"),
            ("dev-3.x", "== dev-3.x"),
            ("xsd2php-dev", "== dev-xsd2php"),
            ("3.next-dev", "== dev-3.next"),
            ("dev-1.0.0-dev<1.0.5-dev", "== dev-1.0.0-dev<1.0.5-dev"),
            ("foobar-dev as 2.1.0", "== dev-foobar"),
            (
                "foobar-dev as 2.1.0 || 3.5",
                "[== dev-foobar || == 3.5.0.0]",
            ),
            (
                "foobar-dev as 2.1.0 || 3.5 as 1.5",
                "[== dev-foobar || == 3.5.0.0]",
            ),
            ("2.1.0 - 2.3-dev", "[>= 2.1.0.0-dev <= 2.3.0.0-dev]"),
            (
                "1.0 - 2.0.x-dev",
                "[>= 1.0.0.0-dev <= 2.0.9999999.9999999-dev]",
            ),
            ("^1.", "[>= 1.0.0.0-dev < 2.0.0.0-dev]"),
            ("~1.", "[>= 1.0.0.0-dev < 2.0.0.0-dev]"),
            ("1.2.", "== 1.2.0.0"),
            ("1.2..dev", "== 1.2.0.0-dev"),
            ("1.2-.dev", "== 1.2.0.0-dev"),
            ("1.2_-dev", "== 1.2.0.0-dev"),
            (
                "~2.5.9|~2.6,>=2.6.2",
                "[[>= 2.5.9.0-dev < 2.6.0.0-dev] || [>= 2.6.0.0-dev < 3.0.0.0-dev >= 2.6.2.0-dev]]",
            ),
            (">2.0,<=3.0", "[> 2.0.0.0 <= 3.0.0.0]"),
            (">2.0 <=3.0", "[> 2.0.0.0 <= 3.0.0.0]"),
            (">2.0 , <=3.0", "[> 2.0.0.0 <= 3.0.0.0]"),
            ("  > 2.0  ,  <=  3.0 ", "[> 2.0.0.0 <= 3.0.0.0]"),
            (
                ">=1.1.0-alpha4,<1.2.x-dev",
                "[>= 1.1.0.0-alpha4 < 1.2.9999999.9999999-dev]",
            ),
            (
                ">=1.1.0-alpha4,<1.2-beta2",
                "[>= 1.1.0.0-alpha4 < 1.2.0.0-beta2]",
            ),
            (
                ">2.0,<2.0.5 | >2.0.6",
                "[[> 2.0.0.0 < 2.0.5.0-dev] || > 2.0.6.0]",
            ),
            (
                "> 2.0 , <2.0.5 | >  2.0.6",
                "[[> 2.0.0.0 < 2.0.5.0-dev] || > 2.0.6.0]",
            ),
            (">2.0@stable,<=3.0@dev", "[> 2.0.0.0 <= 3.0.0.0-dev]"),
            (">2.0@stable,@dev", "[> 2.0.0.0 *]"),
            (">2.0@stable || 0@dev", "[> 2.0.0.0 || == 0.0.0.0]"),
        ];
        for (input, expected) in cases {
            assert_eq!(parsed(input), expected, "{input}");
        }
    }

    #[test]
    fn parse_constraints_failures() {
        for input in [
            "",
            "1.0.0-meh",
            ">2.0,,<=3.0",
            ">2.0 ,, <=3.0",
            ">2.0 ||| <=3.0",
            ",^1@dev || ^4@dev",
            ",^1@dev",
            "|| ^1@dev",
            "^1@dev ||",
            "^1@dev ,",
            "^2.0.*",
            "^2.0.x",
            "^2.0.x-beta",
            "^2.*",
            "^2.x",
            "^2.x-beta",
            "^2.1.2.*",
            "^2.1.2.x",
            "~2.0.*",
            "~2.0.x-beta",
            "~2.*",
            "~2.1.2.x",
            "1.x - 2.*",
            "2.x.x.x-dev - 3.x.x.x-dev",
            "^1.*-beta-dev",
            "^1. *-dev",
            "~1.*-beta-dev",
            "1.0.0-dev<1.0.5-dev",
            "*-dev",
            "^",
            "^8 || ^",
            "~",
            "~1 ~",
            "1.0#abcd123",
        ] {
            assert!(parse_constraints(input).is_err(), "{input:?} should fail");
        }
        assert!(
            parse_constraints("~>1.2")
                .unwrap_err()
                .contains("Invalid operator \"~>\", you probably meant to use the \"~\" operator")
        );
    }

    #[test]
    fn stability() {
        for (expected, version) in [
            (Stability::Stable, "1"),
            (Stability::Stable, "v3.2.1"),
            (Stability::Dev, "v2.0.x-dev"),
            (Stability::Dev, "v2.0.x-dev#abc123"),
            (Stability::RC, "3.0-RC2"),
            (Stability::Dev, "dev-master"),
            (Stability::Dev, "3.1.2-dev"),
            (Stability::Dev, "dev-feature+issue-1"),
            (Stability::Stable, "3.1.2-p1"),
            (Stability::Stable, "3.1.2-patch"),
            (Stability::Alpha, "3.1.2-alpha5"),
            (Stability::Beta, "2.0B1"),
            (Stability::Alpha, "1.2_a1"),
            (Stability::RC, "2.0.0rc1"),
            (Stability::Alpha, "1.0.0-alpha11+cs-1.1.0"),
            (Stability::Dev, "1-2_dev"),
        ] {
            assert_eq!(parse_stability(version), expected, "{version}");
        }
        assert_eq!(Stability::parse("rc"), Ok(Stability::RC));
        assert_eq!(Stability::parse("BeTa"), Ok(Stability::Beta));
    }

    #[test]
    fn complex_conjunctive() {
        let parsed = parse_constraints("~0.1 || ~1.0 !=1.0.1").unwrap();
        assert!(!parsed.matches(&Constraint::new("=", "1.0.1.0")));
    }
}
