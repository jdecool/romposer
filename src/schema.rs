//! `composer.json` validation against the schema shipped in the Composer
//! submodule, reporting errors with the wording of justinrainbow/json-schema
//! as `JsonFile::validateJsonSchema` prints them (`property : message`).

use jsonschema::error::{TypeKind, ValidationErrorKind};
use jsonschema::{Draft, JsonType, Validator};
use serde_json::Value;
use std::sync::LazyLock;

pub const COMPOSER_SCHEMA: &str = include_str!("../reference/composer/res/composer-schema.json");

static LAX: LazyLock<Validator> = LazyLock::new(|| build(false));
static STRICT: LazyLock<Validator> = LazyLock::new(|| build(true));

fn build(strict: bool) -> Validator {
    let mut schema: Value =
        serde_json::from_str(COMPOSER_SCHEMA).expect("composer-schema.json is valid JSON");
    if strict {
        let obj = schema.as_object_mut().unwrap();
        obj.insert("additionalProperties".into(), Value::Bool(false));
        obj.insert(
            "required".into(),
            serde_json::json!(["name", "description"]),
        );
    }
    jsonschema::options()
        .with_draft(Draft::Draft4)
        .should_validate_formats(true)
        .build(&schema)
        .expect("composer-schema.json compiles")
}

/// `JsonFile::LAX_SCHEMA` (the schema as shipped) or `STRICT_SCHEMA`
/// (no additional properties, `name` and `description` required).
pub fn validate(data: &Value, strict: bool) -> Result<(), Vec<String>> {
    let validator = if strict { &*STRICT } else { &*LAX };
    let mut errors = Vec::new();
    for error in validator.iter_errors(data) {
        let path: Vec<String> = error
            .instance_path()
            .segments()
            .map(|s| s.to_string())
            .collect();
        let instance = error.instance().as_ref();
        match error.kind() {
            ValidationErrorKind::Required { property } => {
                let mut path = path.clone();
                path.push(property.as_str().unwrap_or_default().to_string());
                errors.push(format_error(
                    &path,
                    &format!(
                        "The property {} is required",
                        property.as_str().unwrap_or_default()
                    ),
                ));
            }
            ValidationErrorKind::AdditionalProperties { unexpected } => {
                for name in unexpected {
                    errors.push(format_error(
                        &path,
                        &format!("The property {name} is not defined and the definition does not allow additional properties"),
                    ));
                }
            }
            kind => errors.push(format_error(&path, &message(kind, instance))),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// `BaseConstraint::convertJsonPointerIntoPropertyPath` plus the `property : message` layout.
fn format_error(path: &[String], message: &str) -> String {
    let mut property = String::new();
    for segment in path {
        if segment.bytes().all(|b| b.is_ascii_digit()) && !segment.is_empty() {
            property.push_str(&format!("[{segment}]"));
        } else {
            property.push('.');
            property.push_str(segment);
        }
    }
    let property = property.trim_matches('.');
    if property.is_empty() {
        message.to_string()
    } else {
        format!("{property} : {message}")
    }
}

fn gettype(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "double",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn wording(t: JsonType) -> &'static str {
    match t {
        JsonType::Integer => "an integer",
        JsonType::Number => "a number",
        JsonType::Boolean => "a boolean",
        JsonType::Object => "an object",
        JsonType::Array => "an array",
        JsonType::String => "a string",
        JsonType::Null => "a null",
    }
}

fn ucfirst(s: String) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => s,
    }
}

fn message(kind: &ValidationErrorKind, instance: &Value) -> String {
    let msg = match kind {
        ValidationErrorKind::Type { kind } => {
            let expected: Vec<&str> = match kind {
                TypeKind::Single(t) => vec![wording(*t)],
                TypeKind::Multiple(set) => set.iter().map(wording).collect(),
            };
            let expected = match expected.split_last() {
                Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
                _ => expected.join(", "),
            };
            format!(
                "{} value found, but {expected} is required",
                gettype(instance)
            )
        }
        ValidationErrorKind::Pattern { pattern } => {
            format!("Does not match the regex pattern {pattern}")
        }
        ValidationErrorKind::Enum { options } => {
            format!(
                "Does not have a value in the enumeration {}",
                crate::json::encode_compact(options)
            )
        }
        ValidationErrorKind::Format { format } => match format.as_str() {
            "uri" => "Invalid URL format".to_string(),
            "email" => "Invalid email".to_string(),
            other => format!("Invalid {other} format"),
        },
        ValidationErrorKind::MinItems { limit } => {
            format!(
                "There must be a minimum of {limit} items in the array, {} found",
                instance.as_array().map_or(0, Vec::len)
            )
        }
        ValidationErrorKind::MaxItems { limit } => {
            format!(
                "There must be a maximum of {limit} items in the array, {} found",
                instance.as_array().map_or(0, Vec::len)
            )
        }
        ValidationErrorKind::MinLength { limit } => {
            format!("Must be at least {limit} characters long")
        }
        ValidationErrorKind::MaxLength { limit } => {
            format!("Must be at most {limit} characters long")
        }
        ValidationErrorKind::Minimum { limit } => {
            format!("Must have a minimum value greater than or equal to {limit}")
        }
        ValidationErrorKind::Maximum { limit } => {
            format!("Must have a maximum value less than or equal to {limit}")
        }
        ValidationErrorKind::AnyOf { .. } => "Failed to match at least one schema".to_string(),
        ValidationErrorKind::OneOfNotValid { .. }
        | ValidationErrorKind::OneOfMultipleValid { .. } => {
            "Failed to match exactly one schema".to_string()
        }
        ValidationErrorKind::Not { .. } => "Matched a schema which it should not".to_string(),
        ValidationErrorKind::UniqueItems => {
            "There are no duplicates allowed in the array".to_string()
        }
        ValidationErrorKind::MinProperties { limit } => {
            format!("Must contain a minimum of {limit} properties")
        }
        ValidationErrorKind::MaxProperties { limit } => {
            format!("Must contain no more than {limit} properties")
        }
        other => format!("{other:?}"),
    };
    ucfirst(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    // Expected strings are what `composer validate` 2.11.1 prints for these manifests.
    #[test]
    fn strict_schema_reports_missing_name_and_extra_properties() {
        assert_eq!(
            validate(&json(r#"{"description":"d"}"#), true),
            Err(vec!["name : The property name is required".into()])
        );
        assert_eq!(
            validate(&json(r#"{"name":"a/b","description":"d","foo":1}"#), true),
            Err(vec!["The property foo is not defined and the definition does not allow additional properties".into()])
        );
    }

    #[test]
    fn lax_schema_reports_pattern_type_and_format_errors() {
        assert_eq!(
            validate(&json(r#"{"name":"Acme/Foo"}"#), false),
            Err(vec!["name : Does not match the regex pattern ^[a-z0-9]([_.-]?[a-z0-9]+)*/[a-z0-9](([_.]|-{1,2})?[a-z0-9]+)*$".into()])
        );
        assert_eq!(
            validate(&json(r#"{"authors":[{"name":"x","email":"nope"}]}"#), false),
            Err(vec!["authors[0].email : Invalid email".into()])
        );
        assert_eq!(
            validate(&json(r#"{"name":123}"#), false),
            Err(vec![
                "name : Integer value found, but a string is required".into()
            ])
        );
        assert!(
            validate(
                &json(r#"{"name":"a/b","require":{},"license":"MIT"}"#),
                false
            )
            .is_ok()
        );
    }
}
