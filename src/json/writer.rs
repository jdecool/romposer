//! Byte-exact port of PHP's `json_encode` for the two option sets Composer uses.
//!
//! * [`encode_pretty`]: `JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE`,
//!   what `JsonFile::write` and `JsonFile::encode` produce.
//! * [`encode_compact`]: options `0`, what `Locker::getContentHash` hashes.
//!
//! Floats follow `php_gcvt` with `serialize_precision = -1` (shortest round-trip
//! digits, exponential form outside `1e-4 <= |x| < 1e17`, no forced `.0`).

use serde_json::Value;
use std::fmt::Write;

const INDENT: &str = "    ";

pub fn encode_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, true, 0);
    out
}

pub fn encode_compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, false, 0);
    out
}

fn write_value(out: &mut String, value: &Value, pretty: bool, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|_| !n.is_i64() && !n.is_u64()) {
                out.push_str(&php_float(f));
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_string(out, s, pretty),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, pretty, depth + 1);
                write_value(out, item, pretty, depth + 1);
            }
            newline(out, pretty, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, pretty, depth + 1);
                write_string(out, k, pretty);
                out.push(':');
                if pretty {
                    out.push(' ');
                }
                write_value(out, v, pretty, depth + 1);
            }
            newline(out, pretty, depth);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, pretty: bool, depth: usize) {
    if pretty {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(INDENT);
        }
    }
}

fn write_string(out: &mut String, s: &str, unescaped: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' if !unescaped => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).unwrap(),
            // PHP keeps escaping the line terminators even with JSON_UNESCAPED_UNICODE
            '\u{2028}' | '\u{2029}' => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c if unescaped || c.is_ascii() => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    write!(out, "\\u{unit:04x}").unwrap();
                }
            }
        }
    }
    out.push('"');
}

/// `php_gcvt(value, 17, '.', 'e')` in mode 0 (shortest digits), as `json_encode` uses.
fn php_float(f: f64) -> String {
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1;
    let mut out = String::new();
    if f.is_sign_negative() {
        out.push('-');
    }
    if if decpt < 0 { decpt < -3 } else { decpt > 17 } {
        let (first, rest) = digits.split_at(1);
        out.push_str(first);
        out.push('.');
        out.push_str(if rest.is_empty() { "0" } else { rest });
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        write!(out, "{}", exp.abs()).unwrap();
    } else if decpt <= 0 {
        out.push_str("0.");
        for _ in decpt..0 {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let decpt = decpt as usize;
        if decpt >= digits.len() {
            out.push_str(&digits);
            for _ in digits.len()..decpt {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..decpt]);
            out.push('.');
            out.push_str(&digits[decpt..]);
        }
    }
    out
}
