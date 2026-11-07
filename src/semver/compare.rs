//! Port of PHP's `version_compare()` (ext/standard/versioning.c), which
//! composer/semver relies on for every numeric comparison.

use std::cmp::Ordering;

fn is_special(c: u8) -> bool {
    matches!(c, b'-' | b'_' | b'+')
}

fn is_ndig(c: u8) -> bool {
    !c.is_ascii_digit() && c != b'.'
}

/// `php_canonicalize_version`: inserts dots between digit/non-digit runs and
/// turns `-`, `_`, `+` and other non alphanumerics into dots.
fn canonicalize(version: &str) -> Vec<u8> {
    let bytes = version.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 2);
    if bytes.is_empty() {
        return out;
    }
    out.push(bytes[0]);
    let mut lp = bytes[0];
    for &p in &bytes[1..] {
        let last_is_dot = out.last() == Some(&b'.');
        if is_special(p) {
            if !last_is_dot {
                out.push(b'.');
            }
        } else if (is_ndig(lp) && p.is_ascii_digit()) || (lp.is_ascii_digit() && is_ndig(p)) {
            if !last_is_dot {
                out.push(b'.');
            }
            out.push(p);
        } else if !p.is_ascii_alphanumeric() {
            if !last_is_dot {
                out.push(b'.');
            }
        } else {
            out.push(p);
        }
        lp = p;
    }
    out
}

fn special_form_order(form: &[u8]) -> i32 {
    const FORMS: &[(&[u8], i32)] = &[
        (b"dev", 0),
        (b"alpha", 1),
        (b"a", 1),
        (b"beta", 2),
        (b"b", 2),
        (b"RC", 3),
        (b"rc", 3),
        (b"#", 4),
        (b"pl", 5),
        (b"p", 5),
    ];
    for (name, order) in FORMS {
        if form.starts_with(name) {
            return *order;
        }
    }
    -6
}

fn compare_special_forms(a: &[u8], b: &[u8]) -> Ordering {
    special_form_order(a).cmp(&special_form_order(b))
}

fn parse_long(part: &[u8]) -> i64 {
    // strtol: leading digits only, saturating like PHP's long
    let mut n: i64 = 0;
    for &c in part {
        if !c.is_ascii_digit() {
            break;
        }
        n = n.saturating_mul(10).saturating_add(i64::from(c - b'0'));
    }
    n
}

fn compare_parts(p1: &[u8], p2: &[u8]) -> Ordering {
    let d1 = p1.first().is_some_and(u8::is_ascii_digit);
    let d2 = p2.first().is_some_and(u8::is_ascii_digit);
    match (d1, d2) {
        (true, true) => parse_long(p1).cmp(&parse_long(p2)),
        (false, false) => compare_special_forms(p1, p2),
        (true, false) => compare_special_forms(b"#N#", p2),
        (false, true) => compare_special_forms(p1, b"#N#"),
    }
}

fn compare_canonical(v1: &[u8], v2: &[u8]) -> Ordering {
    if v1.is_empty() || v2.is_empty() {
        return match (v1.is_empty(), v2.is_empty()) {
            (true, true) => Ordering::Equal,
            (false, true) => Ordering::Greater,
            _ => Ordering::Less,
        };
    }
    let mut parts1 = v1.split(|c| *c == b'.');
    let mut parts2 = v2.split(|c| *c == b'.');
    loop {
        match (parts1.next(), parts2.next()) {
            (Some(p1), Some(p2)) => {
                let cmp = compare_parts(p1, p2);
                if cmp != Ordering::Equal {
                    return cmp;
                }
            }
            (Some(p1), None) => {
                return if p1.first().is_some_and(u8::is_ascii_digit) {
                    Ordering::Greater
                } else {
                    compare_canonical(&rest(p1, parts1), b"#N#")
                };
            }
            (None, Some(p2)) => {
                return if p2.first().is_some_and(u8::is_ascii_digit) {
                    Ordering::Less
                } else {
                    compare_canonical(b"#N#", &rest(p2, parts2))
                };
            }
            (None, None) => return Ordering::Equal,
        }
    }
}

fn rest<'a>(first: &'a [u8], tail: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = first.to_vec();
    for part in tail {
        out.push(b'.');
        out.extend_from_slice(part);
    }
    out
}

/// `version_compare($a, $b)`: -1, 0 or 1 as an [`Ordering`].
pub fn version_compare(a: &str, b: &str) -> Ordering {
    compare_canonical(&canonicalize(a), &canonicalize(b))
}

/// `version_compare($a, $b, $op)` for the operators composer/semver uses.
pub fn version_compare_op(a: &str, b: &str, op: &str) -> bool {
    let c = version_compare(a, b);
    match op {
        "<" | "lt" => c == Ordering::Less,
        "<=" | "le" => c != Ordering::Greater,
        ">" | "gt" => c == Ordering::Greater,
        ">=" | "ge" => c != Ordering::Less,
        "==" | "=" | "eq" => c == Ordering::Equal,
        "!=" | "<>" | "ne" => c != Ordering::Equal,
        _ => panic!("unsupported version_compare operator {op}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values come from `php -r 'var_dump(version_compare(...));'`.
    #[test]
    fn orders_like_php() {
        let cases = [
            ("1.0.0.0", "1.0.0.0", Ordering::Equal),
            ("1.0.0.0-dev", "1.0.0.0", Ordering::Less),
            ("1.0.0.0-alpha1", "1.0.0.0-beta1", Ordering::Less),
            ("1.0.0.0-beta1", "1.0.0.0-RC1", Ordering::Less),
            ("1.0.0.0-RC1", "1.0.0.0", Ordering::Less),
            ("1.0.0.0", "1.0.0.0-patch1", Ordering::Less),
            ("1.0.0.0-patch1", "1.0.0.0-patch2", Ordering::Less),
            ("1.10.0.0", "1.9.0.0", Ordering::Greater),
            ("00.01.03.04", "0.1.3.4", Ordering::Equal),
            ("1.0", "1.0.0.0", Ordering::Less),
            ("9223372036854775807.0.0.0", "1.0.0.0", Ordering::Greater),
            ("1.0.0.0-RC15", "1.0.0.0-RC2", Ordering::Greater),
            ("1.0.0.0-alpha3.1", "1.0.0.0-alpha3", Ordering::Greater),
            ("", "1", Ordering::Less),
            ("1.0.0.0-stable", "1.0.0.0-dev", Ordering::Less),
        ];
        for (a, b, expected) in cases {
            assert_eq!(version_compare(a, b), expected, "{a} vs {b}");
        }
    }
}
