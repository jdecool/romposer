//! Port of `JsonManipulator`: edits composer.json in place, keeping its formatting,
//! key order and indentation. Composer locates nodes with recursive PCRE patterns;
//! this uses a small JSON scanner for the same spans.
//!
//! ponytail: `insertRepository`, `setRepositoryUrl` and `insertListItem` are not ported;
//! no command reaches them.

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

/// `config`, `extra` and `scripts` sub node names nest one level: `a.b` is `b` inside `a`.
fn split_sub(main: &str, name: &str) -> (String, Option<String>) {
    if matches!(main, "config" | "extra" | "scripts")
        && let Some((name, sub)) = name.split_once('.')
    {
        return (name.to_string(), Some(sub.to_string()));
    }
    (name.to_string(), None)
}

/// `policy.<list>.<field>` split into list and field.
fn policy_field(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix("policy.")?.split_once('.')
}

/// Value spans of the array whose `[` starts `text`.
fn array_elements(text: &str) -> Vec<(usize, usize)> {
    let s = text.as_bytes();
    let mut out = Vec::new();
    let mut j = skip_ws(s, 1);
    if s.get(j) == Some(&b']') {
        return out;
    }
    loop {
        let start = skip_ws(s, j);
        let Some(end) = scan_value(s, start) else {
            return out;
        };
        out.push((start, end));
        j = skip_ws(s, end);
        match s.get(j) {
            Some(b',') => j += 1,
            _ => return out,
        }
    }
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
            let replacement = format!(
                "{key}: {}",
                encode_value(&Value::String(constraint.to_string()))
            );
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

    /// `addSubNode` (appending): `config`, `extra` and `scripts` names nest one level on
    /// their first dot.
    pub fn add_sub_node(&mut self, main: &str, name: &str, value: &Value) -> bool {
        let (name, sub) = split_sub(main, name);
        let decoded = self.decoded();
        let wrap = |value: &Value| match &sub {
            Some(sub) => {
                let mut map = Map::new();
                map.insert(sub.clone(), value.clone());
                Value::Object(map)
            }
            None => value.clone(),
        };
        if decoded.get(main).is_none_or(Value::is_null) {
            let mut map = Map::new();
            map.insert(name.to_string(), wrap(value));
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
            let value = match &sub {
                Some(sub) => {
                    let current = serde_json::from_str::<Value>(
                        &children[child.value_start..child.value_end],
                    )
                    .map(crate::lock::php_assoc);
                    let mut map = match current {
                        Ok(Value::Object(m)) => m,
                        _ => Map::new(),
                    };
                    map.insert(sub.clone(), value.clone());
                    Value::Object(map)
                }
                None => value.clone(),
            };
            format!(
                "{}{}{}",
                &children[..child.value_start],
                self.format(&value, 1, false),
                &children[child.value_end..]
            )
        } else if let Some(m) =
            Regex::new(r"(?s)^\{(?P<leading>\s*?)(?P<content>\S+.*?)?(?P<trailing>\s*)\}$")
                .unwrap()
                .captures(&children)
        {
            let value = wrap(value);
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
                    self.format(&value, 1, false)
                )
            } else {
                format!(
                    "{{{}{}{}{}: {}{whitespace}}}",
                    self.newline,
                    self.indent,
                    self.indent,
                    encode_value(&Value::String(name.to_string())),
                    self.format(&value, 1, false)
                )
            }
        } else {
            return false;
        };
        self.replace_span(pair.value_start, pair.value_end, &new_children);
        true
    }

    fn replace_span(&mut self, start: usize, end: usize, with: &str) {
        self.contents = format!("{}{with}{}", &self.contents[..start], &self.contents[end..]);
    }

    /// `removeSubNode`, with the same one-level nesting as [`Self::add_sub_node`].
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
        let (name, sub) = split_sub(main, name);
        let node = decoded[main].as_object().and_then(|o| o.get(&name));
        let missing = match (&sub, node) {
            (_, None | Some(Value::Null)) => true,
            (Some(sub), Some(Value::Object(o))) => o.get(sub).is_none_or(Value::is_null),
            (Some(_), Some(_)) => true,
            (None, Some(_)) => false,
        };
        if missing {
            return true;
        }
        let Some(child) = object_pairs(&children, 0)
            .into_iter()
            .find(|p| p.key == name)
        else {
            return false;
        };
        // the main node's value with `name.sub` dropped, as PHP rebuilds it
        let without_sub = |text: &str, sub: &str| -> Value {
            let mut current = serde_json::from_str::<Value>(text)
                .map(crate::lock::php_assoc)
                .unwrap_or(Value::Null);
            if let Some(Value::Object(node)) = current.get_mut(name.as_str()) {
                node.shift_remove(sub);
            }
            if current.get(name.as_str()) == Some(&Value::Array(vec![])) {
                current[name.as_str()] = Value::Object(Map::new());
            }
            current
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
        if only
            .captures(&clean)
            .is_some_and(|m| m.name("content").is_none())
        {
            let emptied = format!("{{{}{}}}", self.newline, self.indent);
            self.replace_span(pair.value_start, pair.value_end, &emptied);
            if let Some(sub) = &sub {
                let current = without_sub(&children, sub);
                return self.add_sub_node(main, &name, &current[name.as_str()]);
            }
            return true;
        }
        let new_children = match &sub {
            Some(sub) => self.format(&without_sub(&children, sub), 0, true),
            None => clean,
        };
        self.replace_span(pair.value_start, pair.value_end, &new_children);
        true
    }

    /// `addListItem`
    pub fn add_list_item(&mut self, main: &str, value: &Value, append: bool) -> bool {
        if self.decoded().get(main).is_none_or(Value::is_null)
            && !self.add_main_key(main, &Value::Array(vec![]))
        {
            return false;
        }
        let Some(pair) = self.main_pair(main) else {
            return false;
        };
        let children = self.contents[pair.value_start..pair.value_end].to_string();
        let Some(m) =
            Regex::new(r"(?s)^\[(?P<leading>\s*?)(?P<content>\S+.*?)?(?P<trailing>\s*)\]$")
                .unwrap()
                .captures(&children)
        else {
            return false;
        };
        let leading = m.name("leading").map_or("", |t| t.as_str());
        let whitespace = m.name("trailing").map_or("", |t| t.as_str());
        let (item_lead, item_trail, depth) = if whitespace.contains(&self.newline) {
            (
                format!("{}{}{}", self.newline, self.indent, self.indent),
                whitespace,
                1,
            )
        } else {
            (leading.to_string(), leading, 0)
        };
        let item = self.format(value, depth, false);
        let new_children = if m.name("content").is_none() {
            format!("[{item_lead}{item}{item_trail}]")
        } else if append {
            let body = &children[..children.len() - whitespace.len() - 1];
            format!("{body},{item_lead}{item}{item_trail}]")
        } else {
            format!(
                "[{leading}{item},{item_lead}{}",
                &children[1 + leading.len()..]
            )
        };
        self.replace_span(pair.value_start, pair.value_end, &new_children);
        true
    }

    /// `removeListItem`
    pub fn remove_list_item(&mut self, main: &str, index: usize) -> bool {
        let Some(Value::Array(items)) = self.decoded().get(main).cloned() else {
            return true;
        };
        if index >= items.len() {
            return true;
        }
        let Some(pair) = self.main_pair(main) else {
            return false;
        };
        let children = self.contents[pair.value_start..pair.value_end].to_string();
        let elements = array_elements(&children);
        let Some(&(start, end)) = elements.get(index) else {
            return false;
        };
        let bytes = children.as_bytes();
        let new_children = if index == 0 {
            let mut j = skip_ws(bytes, end);
            if bytes.get(j) == Some(&b',') {
                j = skip_ws(bytes, j + 1);
            }
            format!("{}{}", &children[..start], &children[j..])
        } else {
            let keep = skip_ws(bytes, elements[index - 1].1);
            format!("{}{}", &children[..keep], &children[end..])
        };
        self.replace_span(pair.value_start, pair.value_end, &new_children);
        true
    }

    /// `changeEmptyMainKeyFromAssocToList`
    fn change_empty_main_key_from_assoc_to_list(&mut self, key: &str) -> bool {
        let Some(pair) = self.main_pair(key) else {
            return true;
        };
        let value = &self.contents[pair.value_start..pair.value_end];
        let Some(inner) = value
            .strip_prefix('{')
            .and_then(|v| v.strip_suffix('}'))
            .filter(|i| i.trim().is_empty())
        else {
            return false;
        };
        let list = format!("[{inner}]");
        self.replace_span(pair.value_start, pair.value_end, &list);
        true
    }

    /// `addRepository`: `None` adds the `{"name": false}` disabling entry.
    pub fn add_repository(&mut self, name: &str, config: Option<&Value>, append: bool) -> bool {
        if !name.is_empty() && !self.do_remove_repository(name) {
            return false;
        }
        if !self.convert_repositories_to_list() {
            return false;
        }
        let mut entry = Map::new();
        let value = match config {
            Some(Value::Object(config)) if !name.is_empty() && name.parse::<f64>().is_err() => {
                entry.insert("name".into(), Value::String(name.into()));
                for (k, v) in config {
                    if k != "name" {
                        entry.insert(k.clone(), v.clone());
                    }
                }
                Value::Object(entry)
            }
            Some(other) => other.clone(),
            None => {
                entry.insert(name.into(), Value::Bool(false));
                Value::Object(entry)
            }
        };
        self.add_list_item("repositories", &value, append)
    }

    fn convert_repositories_to_list(&mut self) -> bool {
        let Some(Value::Object(repos)) = self.decoded().get("repositories").cloned() else {
            return true;
        };
        for key in repos.keys().rev() {
            if !self.remove_sub_node("repositories", key) {
                return false;
            }
        }
        self.change_empty_main_key_from_assoc_to_list("repositories");
        for (name, repo) in repos {
            let item = match repo {
                Value::Object(repo) if name.parse::<f64>().is_err() => {
                    let mut entry = Map::new();
                    entry.insert("name".into(), Value::String(name));
                    for (k, v) in repo {
                        entry.entry(k).or_insert(v);
                    }
                    Value::Object(entry)
                }
                Value::Object(repo) => Value::Object(repo),
                other => {
                    let mut entry = Map::new();
                    entry.insert(name, other);
                    Value::Object(entry)
                }
            };
            if !self.add_list_item("repositories", &item, true) {
                return false;
            }
        }
        true
    }

    /// `removeRepository`: drops the key only if this removal emptied it.
    pub fn remove_repository(&mut self, name: &str) -> bool {
        let before = self.contents.clone();
        self.do_remove_repository(name)
            && (before == self.contents || self.remove_main_key_if_empty("repositories"))
    }

    fn do_remove_repository(&mut self, name: &str) -> bool {
        let (repos, assoc): (Vec<(String, Value)>, bool) = match self.decoded().get("repositories")
        {
            Some(Value::Object(m)) => (m.clone().into_iter().collect(), true),
            Some(Value::Array(a)) => (
                a.iter()
                    .enumerate()
                    .map(|(i, v)| (i.to_string(), v.clone()))
                    .collect(),
                false,
            ),
            _ => return true,
        };
        for (index, repo) in repos {
            let named = repo.get("name").and_then(Value::as_str) == Some(name);
            if (assoc && index == name) || named {
                return if assoc {
                    self.remove_sub_node("repositories", &index)
                } else {
                    self.remove_list_item("repositories", index.parse().unwrap_or(0))
                };
            }
            if !assoc
                && repo.as_object().is_some_and(|o| o.len() == 1)
                && repo.get(name) == Some(&Value::Bool(false))
            {
                return self.remove_list_item("repositories", index.parse().unwrap_or(0));
            }
        }
        true
    }

    /// `addConfigSetting`: `policy.<list>.<field>` rewrites the whole list block.
    pub fn add_config_setting(&mut self, name: &str, value: &Value) -> bool {
        if let Some((list, field)) = policy_field(name) {
            if field.contains('.') {
                return false;
            }
            let mut block = self.policy_list_block(list);
            block.insert(field.to_string(), value.clone());
            return self.add_sub_node("config", &format!("policy.{list}"), &Value::Object(block));
        }
        self.add_sub_node("config", name, value)
    }

    pub fn remove_config_setting(&mut self, name: &str) -> bool {
        if let Some((list, field)) = policy_field(name) {
            if field.contains('.') {
                return false;
            }
            let mut block = self.policy_list_block(list);
            if !block.contains_key(field) {
                return true;
            }
            block.shift_remove(field);
            if block.is_empty() {
                return self.remove_config_setting(&format!("policy.{list}"));
            }
            return self.add_sub_node("config", &format!("policy.{list}"), &Value::Object(block));
        }
        self.remove_sub_node("config", name)
    }

    fn policy_list_block(&self, list: &str) -> Map<String, Value> {
        match self
            .decoded()
            .get("config")
            .and_then(|c| c.get("policy"))
            .and_then(|p| p.get(list))
            .cloned()
            .map(crate::lock::php_assoc)
        {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    /// `addProperty`
    pub fn add_property(&mut self, name: &str, value: &Value) -> bool {
        for main in ["suggest", "extra", "scripts"] {
            if let Some(rest) = name.strip_prefix(main).and_then(|r| r.strip_prefix('.')) {
                return self.add_sub_node(main, rest, value);
            }
        }
        self.add_main_key(name, value)
    }

    /// `removeProperty`
    pub fn remove_property(&mut self, name: &str) -> bool {
        for main in ["suggest", "extra", "scripts", "autoload", "autoload-dev"] {
            if let Some(rest) = name.strip_prefix(main).and_then(|r| r.strip_prefix('.')) {
                return self.remove_sub_node(main, rest);
            }
        }
        self.remove_main_key(name)
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
