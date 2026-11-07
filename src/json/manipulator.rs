//! Port of `JsonManipulator`: edits composer.json in place, keeping its formatting,
//! key order and indentation. Composer locates nodes with recursive PCRE patterns;
//! this uses a small JSON scanner for the same spans.
//!
//! ponytail: the repository and list helpers (`addRepository`, `addListItem`,
//! `removeListItem`, policy config blocks) are not ported; `require`/`remove`/`config`
//! only need links, sub nodes and main keys.

use super::{Map, Value, encode};
use crate::package::is_platform_package;
use regex::Regex;

pub struct Manipulator {
    contents: String,
    newline: String,
    indent: String,
}

/// A `"key": value` pair inside an object: byte offsets into the object's text.
#[derive(Clone, Debug)]
struct Pair {
    /// offset of the opening quote of the key
    key_start: usize,
    key: String,
    /// raw key text between the quotes
    raw_key: String,
    value_start: usize,
    value_end: usize,
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// End offset (exclusive) of the JSON value starting at `i`.
fn scan_value(s: &[u8], i: usize) -> Option<usize> {
    let i = skip_ws(s, i);
    match *s.get(i)? {
        b'"' => scan_string(s, i),
        b'{' | b'[' => {
            let close = if s[i] == b'{' { b'}' } else { b']' };
            let mut j = skip_ws(s, i + 1);
            if s.get(j) == Some(&close) {
                return Some(j + 1);
            }
            loop {
                if s[i] == b'{' {
                    j = scan_string(s, skip_ws(s, j))?;
                    j = skip_ws(s, j);
                    if s.get(j) != Some(&b':') {
                        return None;
                    }
                    j += 1;
                }
                j = scan_value(s, j)?;
                j = skip_ws(s, j);
                match s.get(j)? {
                    b',' => j += 1,
                    c if *c == close => return Some(j + 1),
                    _ => return None,
                }
            }
        }
        b't' | b'f' | b'n' | b'-' | b'0'..=b'9' => {
            let mut j = i;
            while j < s.len() && !matches!(s[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                j += 1;
            }
            Some(j)
        }
        _ => None,
    }
}

fn scan_string(s: &[u8], i: usize) -> Option<usize> {
    if s.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    while j < s.len() {
        match s[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// The pairs of the object whose `{` is at `start`.
fn object_pairs(text: &str, start: usize) -> Vec<Pair> {
    let s = text.as_bytes();
    let mut pairs = Vec::new();
    let mut j = skip_ws(s, start + 1);
    if s.get(j) == Some(&b'}') {
        return pairs;
    }
    loop {
        let key_start = skip_ws(s, j);
        let Some(key_end) = scan_string(s, key_start) else {
            return pairs;
        };
        let raw_key = text[key_start + 1..key_end - 1].to_string();
        let key: String =
            serde_json::from_str::<String>(&text[key_start..key_end]).unwrap_or_default();
        let colon = skip_ws(s, key_end);
        let value_start = skip_ws(s, colon + 1);
        let Some(value_end) = scan_value(s, value_start) else {
            return pairs;
        };
        pairs.push(Pair {
            key_start,
            key,
            raw_key,
            value_start,
            value_end,
        });
        j = skip_ws(s, value_end);
        match s.get(j) {
            Some(b',') => j += 1,
            _ => return pairs,
        }
    }
}

fn encode_value(value: &Value) -> String {
    encode(value, false, true, true)
}

/// `strnatcmp`
fn strnatcmp(a: &str, b: &str) -> std::cmp::Ordering {
    let chunks = |s: &str| -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, chunk)) if *d == digit => chunk.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    };
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(cb.iter()) {
        let ord = if x.0 && y.0 {
            let (nx, ny) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
            nx.len().cmp(&ny.len()).then_with(|| nx.cmp(ny))
        } else {
            x.1.cmp(&y.1)
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    ca.len().cmp(&cb.len())
}

impl Manipulator {
    pub fn new(contents: &str) -> Result<Manipulator, String> {
        let mut contents = contents.trim().to_string();
        if contents.is_empty() {
            contents = "{}".into();
        }
        if !contents.starts_with('{') || !contents.ends_with('}') {
            return Err("The json file must be an object ({})".into());
        }
        let newline = if contents.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        }
        .to_string();
        if contents == "{}" {
            contents = format!("{{{newline}}}");
        }
        let indent = Regex::new(r#"(?m)^([ \t]+)""#)
            .unwrap()
            .captures(&contents)
            .map(|m| m[1].to_string())
            .unwrap_or_else(|| "    ".into());
        Ok(Manipulator {
            contents,
            newline,
            indent,
        })
    }

    pub fn contents(&self) -> String {
        format!("{}{}", self.contents, self.newline)
    }

    fn decoded(&self) -> Map<String, Value> {
        serde_json::from_str::<Value>(&self.contents)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    fn main_pair(&self, key: &str) -> Option<Pair> {
        object_pairs(&self.contents, 0)
            .into_iter()
            .find(|p| p.key == key)
    }

    /// `JsonManipulator::format`
    pub fn format(&self, value: &Value, depth: usize, was_object: bool) -> String {
        match value {
            Value::Array(items) if items.is_empty() => {
                if was_object {
                    format!("{{{}{}}}", self.newline, self.indent.repeat(depth + 1))
                } else {
                    "[]".into()
                }
            }
            Value::Object(map) if map.is_empty() => {
                format!("{{{}{}}}", self.newline, self.indent.repeat(depth + 1))
            }
            Value::Array(items) => {
                let inner: Vec<String> = items
                    .iter()
                    .map(|v| self.format(v, depth + 1, false))
                    .collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Object(map) => {
                let elems: Vec<String> = map
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "{}{}: {}",
                            self.indent.repeat(depth + 2),
                            encode_value(&Value::String(k.clone())),
                            self.format(v, depth + 1, false)
                        )
                    })
                    .collect();
                format!(
                    "{{{}{}{}{}}}",
                    self.newline,
                    elems.join(&format!(",{}", self.newline)),
                    self.newline,
                    self.indent.repeat(depth + 1)
                )
            }
            other => encode_value(other),
        }
    }

    /// `addLink`
    pub fn add_link(
        &mut self,
        kind: &str,
        package: &str,
        constraint: &str,
        sort_packages: bool,
    ) -> bool {
        let decoded = self.decoded();
        if !decoded.contains_key(kind) {
            let mut map = Map::new();
            map.insert(package.to_string(), Value::String(constraint.to_string()));
            return self.add_main_key(kind, &Value::Object(map));
        }
        let Some(pair) = self.main_pair(kind) else {
            return false;
        };
        let mut links = self.contents[pair.value_start..pair.value_end].to_string();
        let lower = package.to_lowercase();
        let existing = object_pairs(&links, 0)
            .into_iter()
            .find(|p| p.raw_key.replace("\\/", "/").to_lowercase() == lower);
        if let Some(existing) = existing {
            let key = encode_value(&Value::String(existing.raw_key.replace("\\/", "/")));
            let replacement = format!("{key}: \"{constraint}\"");
            links = format!(
                "{}{}{}",
                &links[..existing.key_start],
                replacement,
                &links[existing.value_end..]
            );
        } else if let Some(m) = Regex::new(r"(?s)^\s*\{\s*\S+.*?(\s*\}\s*)$")
            .unwrap()
            .captures(&links)
        {
            let tail = m[1].to_string();
            let trimmed = links[..links.len() - tail.len()].to_string();
            links = format!(
                "{trimmed},{}{}{}{}: {}{tail}",
                self.newline,
                self.indent,
                self.indent,
                encode_value(&Value::String(package.to_string())),
                encode_value(&Value::String(constraint.to_string()))
            );
        } else {
            links = format!(
                "{{{}{}{}{}: {}{}{}}}",
                self.newline,
                self.indent,
                self.indent,
                encode_value(&Value::String(package.to_string())),
                encode_value(&Value::String(constraint.to_string())),
                self.newline,
                self.indent
            );
        }
        if sort_packages {
            let Ok(Value::Object(requirements)) = serde_json::from_str::<Value>(&links) else {
                return false;
            };
            let mut entries: Vec<(String, Value)> = requirements.into_iter().collect();
            let prefix = |name: &str| -> String {
                if is_platform_package(name) {
                    if name.starts_with("php") {
                        format!("0-{name}")
                    } else if name.starts_with("hhvm") {
                        format!("1-{name}")
                    } else if name.starts_with("ext") {
                        format!("2-{name}")
                    } else if name.starts_with("lib") {
                        format!("3-{name}")
                    } else {
                        format!("4-{name}")
                    }
                } else {
                    format!("5-{name}")
                }
            };
            crate::zend_sort::usort(&mut entries, |a, b| strnatcmp(&prefix(&a.0), &prefix(&b.0)));
            links = self.format(&Value::Object(entries.into_iter().collect()), 0, false);
        }
        self.contents = format!(
            "{}{}{}",
            &self.contents[..pair.value_start],
            links,
            &self.contents[pair.value_end..]
        );
        true
    }

    /// `removeLink`: removes the link, and the link type key if that emptied it.
    pub fn remove_link(&mut self, kind: &str, package: &str) -> bool {
        let before = self.contents.clone();
        self.remove_sub_node(kind, package)
            && (before == self.contents || self.remove_main_key_if_empty(kind))
    }

    /// `addSubNode` for plain (undotted) names.
    pub fn add_sub_node(&mut self, main: &str, name: &str, value: &Value) -> bool {
        let decoded = self.decoded();
        if !decoded.contains_key(main) {
            let mut map = Map::new();
            map.insert(name.to_string(), value.clone());
            return self.add_main_key(main, &Value::Object(map));
        }
        let Some(pair) = self.main_pair(main) else {
            return false;
        };
        let children = self.contents[pair.value_start..pair.value_end].to_string();
        if !children.starts_with('{') {
            return false;
        }
        let child_pairs = object_pairs(&children, 0);
        let new_children = if let Some(child) = child_pairs.iter().find(|p| p.key == name) {
            format!(
                "{}{}{}",
                &children[..child.value_start],
                self.format(value, 1, false),
                &children[child.value_end..]
            )
        } else if let Some(m) =
            Regex::new(r"(?s)^\{(?P<leading>\s*?)(?P<content>\S+.*?)?(?P<trailing>\s*)\}$")
                .unwrap()
                .captures(&children)
        {
            let whitespace = m
                .name("trailing")
                .map(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if m.name("content").is_some() {
                let body = &children[..children.len() - whitespace.len() - 1];
                format!(
                    "{body},{}{}{}{}: {}{whitespace}}}",
                    self.newline,
                    self.indent,
                    self.indent,
                    encode_value(&Value::String(name.to_string())),
                    self.format(value, 1, false)
                )
            } else {
                format!(
                    "{{{}{}{}{}: {}{whitespace}}}",
                    self.newline,
                    self.indent,
                    self.indent,
                    encode_value(&Value::String(name.to_string())),
                    self.format(value, 1, false)
                )
            }
        } else {
            return false;
        };
        self.contents = format!(
            "{}{}{}",
            &self.contents[..pair.value_start],
            new_children,
            &self.contents[pair.value_end..]
        );
        true
    }

    /// `removeSubNode` for plain (undotted) names.
    pub fn remove_sub_node(&mut self, main: &str, name: &str) -> bool {
        let decoded = self.decoded();
        let empty = match decoded.get(main) {
            None | Some(Value::Null) => true,
            Some(Value::Object(o)) => o.is_empty(),
            Some(Value::Array(a)) => a.is_empty(),
            _ => false,
        };
        if empty {
            return true;
        }
        let Some(pair) = self.main_pair(main) else {
            return false;
        };
        let children = self.contents[pair.value_start..pair.value_end].to_string();
        if !children.starts_with('{') {
            return false;
        }
        if !decoded[main]
            .as_object()
            .is_some_and(|o| o.contains_key(name))
        {
            return true;
        }
        let Some(child) = object_pairs(&children, 0)
            .into_iter()
            .find(|p| p.key == name)
        else {
            return false;
        };
        let prefix = &children[..child.key_start];
        let rest = &children[child.value_end..];
        let trailing_comma = Regex::new(r"(?s),\s*$").unwrap();
        let clean = if trailing_comma.is_match(prefix) {
            format!("{}{}", trailing_comma.replace(prefix, ""), rest)
        } else {
            format!(
                "{prefix}{}",
                Regex::new(r"(?s)^\s*,?\s*").unwrap().replace(rest, "")
            )
        };
        let only = Regex::new(r"(?s)^\{\s*?(?P<content>\S+.*?)?(?P<trailing>\s*)\}$").unwrap();
        let new_children = match only.captures(&clean) {
            Some(m) if m.name("content").is_none() => {
                format!("{{{}{}}}", self.newline, self.indent)
            }
            _ => clean,
        };
        self.contents = format!(
            "{}{}{}",
            &self.contents[..pair.value_start],
            new_children,
            &self.contents[pair.value_end..]
        );
        true
    }

    /// `addMainKey`
    pub fn add_main_key(&mut self, key: &str, content: &Value) -> bool {
        let decoded = self.decoded();
        let content = self.format(content, 0, false);
        let encoded_key = encode_value(&Value::String(key.to_string()));
        if decoded.get(key).is_some_and(|v| !v.is_null())
            && let Some(pair) = self.main_pair(key)
        {
            self.contents = format!(
                "{}{encoded_key}: {content}{}",
                &self.contents[..pair.key_start],
                &self.contents[pair.value_end..]
            );
            return true;
        }
        if let Some(m) = Regex::new(r"(?s)[^{\s](\s*)\}$")
            .unwrap()
            .captures(&self.contents)
        {
            let ws = m[1].to_string();
            let body = &self.contents[..self.contents.len() - ws.len() - 1];
            self.contents = format!(
                "{body},{}{}{encoded_key}: {content}{}}}",
                self.newline, self.indent, self.newline
            );
            return true;
        }
        let body = &self.contents[..self.contents.len() - 1];
        self.contents = format!(
            "{body}{}{encoded_key}: {content}{}}}",
            self.indent, self.newline
        );
        true
    }

    /// `removeMainKey`
    pub fn remove_main_key(&mut self, key: &str) -> bool {
        let decoded = self.decoded();
        if !decoded.contains_key(key) {
            return true;
        }
        let Some(pair) = self.main_pair(key) else {
            return false;
        };
        let mut start = self.contents[..pair.key_start].to_string();
        let after = &self.contents[pair.value_end..];
        let end = Regex::new(r"(?s)^\s*,?\s*")
            .unwrap()
            .replace(after, "")
            .into_owned();
        let trailing_comma = Regex::new(r"(?s),\s*$").unwrap();
        if trailing_comma.is_match(&start) && end == "}" {
            start = Regex::new(r"(?s),(\s*)$")
                .unwrap()
                .replace(&start, "$1")
                .trim_end_matches(|c: char| self.indent.contains(c))
                .to_string();
        }
        self.contents = format!("{start}{end}");
        if Regex::new(r"(?s)^\{\s*\}\s*$")
            .unwrap()
            .is_match(&self.contents)
        {
            self.contents = "{\n}".into();
        }
        true
    }

    /// `removeMainKeyIfEmpty`
    pub fn remove_main_key_if_empty(&mut self, key: &str) -> bool {
        let decoded = self.decoded();
        match decoded.get(key) {
            None => true,
            Some(Value::Object(o)) if o.is_empty() => self.remove_main_key(key),
            Some(Value::Array(a)) if a.is_empty() => self.remove_main_key(key),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(json: &str, kind: &str, package: &str, constraint: &str) -> String {
        let mut m = Manipulator::new(json).unwrap();
        assert!(m.add_link(kind, package, constraint, false));
        m.contents()
    }

    #[test]
    fn add_link_cases_from_composer() {
        assert_eq!(
            add("{}", "require", "vendor/baz", "qux"),
            "{\n    \"require\": {\n        \"vendor/baz\": \"qux\"\n    }\n}\n"
        );
        assert_eq!(
            add("{\n    \"foo\": \"bar\"\n}", "require", "vendor/baz", "qux"),
            "{\n    \"foo\": \"bar\",\n    \"require\": {\n        \"vendor/baz\": \"qux\"\n    }\n}\n"
        );
        assert_eq!(
            add(
                "{\n    \"require\": {\n    }\n}",
                "require",
                "vendor/baz",
                "qux"
            ),
            "{\n    \"require\": {\n        \"vendor/baz\": \"qux\"\n    }\n}\n"
        );
        assert_eq!(
            add(
                "{\n    \"empty\": \"\",\n    \"require\": {\n        \"foo\": \"bar\"\n    }\n}",
                "require",
                "vendor/baz",
                "qux"
            ),
            "{\n    \"empty\": \"\",\n    \"require\": {\n        \"foo\": \"bar\",\n        \"vendor/baz\": \"qux\"\n    }\n}\n"
        );
        // existing link, case-insensitive and escaped-slash matches
        for key in ["vendor/baz", "vendor\\/baz"] {
            for package in ["vendor/baz", "vEnDoR/bAz"] {
                assert_eq!(
                    add(
                        &format!(
                            "{{\n    \"require\":\n    {{\n        \"foo\": \"bar\",\n        \"{key}\": \"baz\"\n    }}\n}}"
                        ),
                        "require",
                        package,
                        "qux"
                    ),
                    "{\n    \"require\":\n    {\n        \"foo\": \"bar\",\n        \"vendor/baz\": \"qux\"\n    }\n}\n"
                );
            }
        }
        // a nested require in a repository is not the root require
        assert_eq!(
            add(
                "{\n    \"repositories\": [{\n        \"type\": \"package\",\n        \"package\": {\n            \"require\": {\n                \"foo\": \"bar\"\n            }\n        }\n    }]\n}",
                "require",
                "foo",
                "qux"
            ),
            "{\n    \"repositories\": [{\n        \"type\": \"package\",\n        \"package\": {\n            \"require\": {\n                \"foo\": \"bar\"\n            }\n        }\n    }],\n    \"require\": {\n        \"foo\": \"qux\"\n    }\n}\n"
        );
    }

    #[test]
    fn remove_sub_node_from_require() {
        let json = "{\n    \"repositories\": [\n        {\n            \"package\": {\n                \"require\": {\n                    \"this/should-not-end-up-in-root-require\": \"~2.0\"\n                }\n            }\n        }\n    ],\n    \"require\": {\n        \"package/a\": \"*\",\n        \"package/b\": \"*\",\n        \"package/c\": \"*\"\n    },\n    \"require-dev\": {\n        \"package/d\": \"*\"\n    }\n}";
        let mut m = Manipulator::new(json).unwrap();
        assert!(m.remove_sub_node("require", "package/c"));
        assert!(m.remove_sub_node("require-dev", "package/d"));
        assert_eq!(
            m.contents(),
            "{\n    \"repositories\": [\n        {\n            \"package\": {\n                \"require\": {\n                    \"this/should-not-end-up-in-root-require\": \"~2.0\"\n                }\n            }\n        }\n    ],\n    \"require\": {\n        \"package/a\": \"*\",\n        \"package/b\": \"*\"\n    },\n    \"require-dev\": {\n    }\n}\n"
        );
        // removeLink drops the key once it is emptied
        let mut m = Manipulator::new("{\n    \"require\": {\n        \"a/a\": \"1.0\"\n    },\n    \"require-dev\": {\n        \"b/b\": \"*\"\n    }\n}").unwrap();
        assert!(m.remove_link("require-dev", "b/b"));
        assert_eq!(
            m.contents(),
            "{\n    \"require\": {\n        \"a/a\": \"1.0\"\n    }\n}\n"
        );
    }

    #[test]
    fn sorted_links_and_natural_order() {
        let mut m = Manipulator::new("{\n    \"require\": {\n        \"vendor/b\": \"1\",\n        \"ext-json\": \"*\",\n        \"php\": \">=8\"\n    }\n}").unwrap();
        assert!(m.add_link("require", "vendor/a10", "2", true));
        assert_eq!(
            m.contents(),
            "{\n    \"require\": {\n        \"php\": \">=8\",\n        \"ext-json\": \"*\",\n        \"vendor/a10\": \"2\",\n        \"vendor/b\": \"1\"\n    }\n}\n"
        );
        assert_eq!(strnatcmp("a2", "a10"), std::cmp::Ordering::Less);
    }
}
