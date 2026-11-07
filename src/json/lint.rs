//! The one thing seld/jsonlint adds over a plain parser that Composer surfaces:
//! duplicate object keys (`JsonParser::DETECT_KEY_CONFLICTS`).

/// Returns the first duplicated key and its 1-based line, scanning the raw text.
pub fn first_duplicate_key(json: &str) -> Option<(String, usize)> {
    let bytes = json.as_bytes();
    let mut line = 1;
    let mut i = 0;
    // One key set per open container; `None` for arrays.
    let mut stack: Vec<Option<Vec<String>>> = Vec::new();
    let mut expect_key = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => line += 1,
            b'{' => {
                stack.push(Some(Vec::new()));
                expect_key = true;
            }
            b'[' => {
                stack.push(None);
                expect_key = false;
            }
            b'}' | b']' => {
                stack.pop();
                expect_key = false;
            }
            b',' => expect_key = matches!(stack.last(), Some(Some(_))),
            b':' => expect_key = false,
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b'"' {
                    if bytes[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                let raw = &json[start..j.min(bytes.len())];
                if expect_key && let Some(Some(keys)) = stack.last_mut() {
                    let key: String = serde_json::from_str(&format!("\"{raw}\""))
                        .unwrap_or_else(|_| raw.to_string());
                    if keys.contains(&key) {
                        return Some((key, line));
                    }
                    keys.push(key);
                    expect_key = false;
                }
                i = j;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::first_duplicate_key;

    #[test]
    fn finds_duplicates_at_the_right_depth() {
        assert_eq!(
            first_duplicate_key(r#"{"name":"a/b","name":"a/c"}"#),
            Some(("name".into(), 1))
        );
        assert_eq!(
            first_duplicate_key(
                "{\n  \"require\": {\"a/b\": \"1\"},\n  \"extra\": {\"a/b\": \"2\",\n \"a/b\": 3}\n}"
            ),
            Some(("a/b".into(), 4))
        );
        assert_eq!(first_duplicate_key(r#"{"a":["x","x"],"b":{"a":1}}"#), None);
        assert_eq!(first_duplicate_key(r#"{"a":"\"a\"","\"a\"":1}"#), None);
    }
}
