//! Port of composer/spdx-licenses 1.6.0: identifier lookup and license
//! expression validation. The data files in `res/` are the ones shipped with
//! that release.

use std::collections::HashMap;
use std::sync::LazyLock;

const LICENSES_JSON: &str = include_str!("../res/spdx-licenses.json");
const EXCEPTIONS_JSON: &str = include_str!("../res/spdx-exceptions.json");

pub struct License {
    pub identifier: String,
    pub name: String,
    pub osi_approved: bool,
    pub deprecated: bool,
}

struct Data {
    licenses: HashMap<String, License>,
    exceptions: HashMap<String, String>,
}

static DATA: LazyLock<Data> = LazyLock::new(|| {
    let licenses: serde_json::Value = serde_json::from_str(LICENSES_JSON).unwrap();
    let exceptions: serde_json::Value = serde_json::from_str(EXCEPTIONS_JSON).unwrap();
    Data {
        licenses: licenses
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, v)| {
                let v = v.as_array().unwrap();
                (
                    id.to_lowercase(),
                    License {
                        identifier: id.clone(),
                        name: v[0].as_str().unwrap().to_string(),
                        osi_approved: v[1].as_bool().unwrap(),
                        deprecated: v[2].as_bool().unwrap(),
                    },
                )
            })
            .collect(),
        exceptions: exceptions
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, v)| (id.to_lowercase(), v[0].as_str().unwrap().to_string()))
            .collect(),
    }
});

/// `SpdxLicenses::getLicenseByIdentifier`
pub fn license_by_identifier(identifier: &str) -> Option<&'static License> {
    DATA.licenses.get(&identifier.to_lowercase())
}

/// `SpdxLicenses::validate` for a single string.
pub fn is_valid_expression(license: &str) -> bool {
    if DATA.licenses.contains_key(&license.to_lowercase()) {
        return true;
    }
    if license.eq_ignore_ascii_case("NONE") || license.eq_ignore_ascii_case("NOASSERTION") {
        return true;
    }
    let mut p = Parser {
        s: license.as_bytes(),
        pos: 0,
    };
    p.compound_expression() && p.pos == p.s.len()
}

/// `SpdxLicenses::validate` for an array: `(A OR B)` when several, else the single entry.
pub fn is_valid_license_list(licenses: &[&str]) -> bool {
    match licenses {
        [] => is_valid_expression(""),
        [single] => is_valid_expression(single),
        many => is_valid_expression(&format!("({})", many.join(" OR "))),
    }
}

/// Recursive-descent equivalent of the PCRE grammar in `SpdxLicenses::isValidLicenseString`.
struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn ws(&mut self) -> bool {
        let start = self.pos;
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        self.pos > start
    }

    fn keyword(&mut self, kw: &str) -> bool {
        let end = self.pos + kw.len();
        if end <= self.s.len() && self.s[self.pos..end].eq_ignore_ascii_case(kw.as_bytes()) {
            self.pos = end;
            return true;
        }
        false
    }

    /// idstring: 1*( ALPHA / DIGIT / - / . ), unicode letters and digits included.
    fn idstring(&mut self) -> Option<&str> {
        let rest = std::str::from_utf8(&self.s[self.pos..]).ok()?;
        let len = rest
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '.' || *c == '-'))
            .map_or(rest.len(), |(i, _)| i);
        if len == 0 {
            return None;
        }
        self.pos += len;
        Some(&rest[..len])
    }

    fn simple_expression(&mut self) -> bool {
        let start = self.pos;
        let Some(id) = self.idstring() else {
            return false;
        };
        let lower = id.to_lowercase();
        // license-ref: [DocumentRef-idstring:]LicenseRef-idstring
        if lower.starts_with("licenseref-") && id.len() > "licenseref-".len() {
            return true;
        }
        if lower.starts_with("documentref-") && self.pos < self.s.len() && self.s[self.pos] == b':'
        {
            self.pos += 1;
            if let Some(next) = self.idstring()
                && next.to_lowercase().starts_with("licenseref-")
                && next.len() > "licenseref-".len()
            {
                return true;
            }
            self.pos = start;
            return false;
        }
        if DATA.licenses.contains_key(&lower) {
            if self.pos < self.s.len() && self.s[self.pos] == b'+' {
                self.pos += 1;
            }
            return true;
        }
        self.pos = start;
        false
    }

    fn compound_head(&mut self) -> bool {
        if self.pos < self.s.len() && self.s[self.pos] == b'(' {
            self.pos += 1;
            self.ws();
            if !self.compound_expression() {
                return false;
            }
            self.ws();
            if self.pos < self.s.len() && self.s[self.pos] == b')' {
                self.pos += 1;
                return true;
            }
            return false;
        }
        if !self.simple_expression() {
            return false;
        }
        let save = self.pos;
        if self.ws()
            && self.keyword("WITH")
            && self.ws()
            && let Some(exc) = self.idstring()
            && DATA.exceptions.contains_key(&exc.to_lowercase())
        {
            return true;
        }
        self.pos = save;
        true
    }

    fn compound_expression(&mut self) -> bool {
        if !self.compound_head() {
            return false;
        }
        let save = self.pos;
        if self.ws()
            && (self.keyword("AND") || self.keyword("OR"))
            && self.ws()
            && self.compound_expression()
        {
            return true;
        }
        self.pos = save;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_expressions() {
        assert!(is_valid_expression("MIT"));
        assert!(is_valid_expression("mit"));
        assert!(is_valid_expression("MIT OR GPL-2.0-only"));
        assert!(is_valid_expression("(MIT or Apache-2.0) AND BSD-3-Clause"));
        assert!(is_valid_expression("GPL-2.0+"));
        assert!(is_valid_expression(
            "GPL-2.0-or-later WITH Classpath-exception-2.0"
        ));
        assert!(is_valid_expression("LicenseRef-Proprietary"));
        assert!(is_valid_expression("DocumentRef-foo:LicenseRef-bar"));
        assert!(is_valid_expression("NONE"));
        assert!(!is_valid_expression("Foo"));
        assert!(!is_valid_expression("MIT AND"));
        assert!(!is_valid_expression(" MIT"));
        assert!(!is_valid_expression("MIT WITH Nope"));
        assert!(!is_valid_expression("(MIT"));
        assert!(is_valid_license_list(&["MIT", "GPL-3.0-only"]));
        assert!(!is_valid_license_list(&["MIT", "nope"]));
    }

    #[test]
    fn deprecated_flag() {
        assert!(license_by_identifier("GPL-2.0").unwrap().deprecated);
        assert!(!license_by_identifier("GPL-2.0-only").unwrap().deprecated);
        assert!(license_by_identifier("nope").is_none());
    }
}
