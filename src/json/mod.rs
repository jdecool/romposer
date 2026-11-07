//! JSON reading and writing with the exact byte layout of Composer's `JsonFile`.
//!
//! Composer writes JSON through PHP's `json_encode` with
//! `JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE`.
//! `serde_json` is only used for parsing (with `preserve_order`); every byte
//! that ends up in a file goes through [`writer`].

pub mod lint;
pub mod writer;

pub use serde_json::{Map, Value};
pub use writer::{encode, encode_compact, encode_pretty};

#[cfg(test)]
mod tests {
    use super::*;

    /// Output of PHP 8.5 `json_encode($data, JSON_PRETTY_PRINT|JSON_UNESCAPED_SLASHES|JSON_UNESCAPED_UNICODE)`,
    /// captured once; the oracle, not derived from this crate.
    const PHP_ORACLE: &str = "{\n    \"a\": 0.1,\n    \"b\": 1.0e+25,\n    \"c\": 1.5e-7,\n    \"d\": 1.2345678901234568e+17,\n    \"e\": 1000000000000000,\n    \"f\": 1.0e+17,\n    \"g\": 10,\n    \"h\": -0,\n    \"i\": 3.14159265358979,\n    \"j\": 1.0e-5,\n    \"k\": 0.0001,\n    \"l\": 10000000000000000,\n    \"m\": \"é/\\u2028\u{7f}\\u0001\\\"\\\\\",\n    \"n\": [],\n    \"o\": {},\n    \"p\": {\n        \"x\": []\n    },\n    \"q\": null,\n    \"r\": true,\n    \"s\": 1.0e+21,\n    \"t\": 1.0e+20,\n    \"u\": 2.5e+22\n}";

    fn oracle_input() -> Value {
        let mut m = Map::new();
        m.insert("a".into(), Value::from(0.1));
        m.insert("b".into(), Value::from(1e25));
        m.insert("c".into(), Value::from(1.5e-7));
        m.insert("d".into(), Value::from(123456789012345678.0));
        m.insert("e".into(), Value::from(1e15));
        m.insert("f".into(), Value::from(1e17));
        m.insert("g".into(), Value::from(10.0));
        m.insert("h".into(), Value::from(-0.0));
        #[allow(clippy::approx_constant)]
        m.insert("i".into(), Value::from(3.14159265358979));
        m.insert("j".into(), Value::from(1e-5));
        m.insert("k".into(), Value::from(0.0001));
        m.insert("l".into(), Value::from(1e16));
        m.insert("m".into(), Value::from("é/\u{2028}\u{7f}\u{1}\"\\"));
        m.insert("n".into(), Value::Array(vec![]));
        m.insert("o".into(), Value::Object(Map::new()));
        let mut p = Map::new();
        p.insert("x".into(), Value::Array(vec![]));
        m.insert("p".into(), Value::Object(p));
        m.insert("q".into(), Value::Null);
        m.insert("r".into(), Value::Bool(true));
        m.insert("s".into(), Value::from(1e21));
        m.insert("t".into(), Value::from(1e20));
        m.insert("u".into(), Value::from(2.5e22));
        Value::Object(m)
    }

    #[test]
    fn pretty_print_matches_php_json_encode() {
        assert_eq!(encode_pretty(&oracle_input()), PHP_ORACLE);
    }

    #[test]
    fn compact_matches_php_json_encode_without_flags() {
        // php -r 'echo json_encode(["a"=>[1,2],"b"=>["c"=>"d/é"]]);'
        let v: Value = serde_json::from_str(r#"{"a":[1,2],"b":{"c":"d/é"}}"#).unwrap();
        assert_eq!(encode_compact(&v), r#"{"a":[1,2],"b":{"c":"d\/\u00e9"}}"#);
    }

    #[test]
    fn integers_are_written_verbatim() {
        let v: Value =
            serde_json::from_str("[0, -1, 9007199254740993, 18446744073709551615]").unwrap();
        assert_eq!(
            encode_compact(&v),
            "[0,-1,9007199254740993,18446744073709551615]"
        );
    }
}
