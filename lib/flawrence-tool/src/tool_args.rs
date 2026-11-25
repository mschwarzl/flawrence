//! Shared argument-deserialization helpers for tool ID/key string fields.
//!
//! These keep tool APIs **strict** (wrong types are rejected, never silently
//! coerced) while replacing serde's opaque type-mismatch message with an
//! **actionable** one that teaches a weak model how to retry correctly. See
//! `docs/SPEC_TOOL_ID_ROBUSTNESS.md` for the rationale.
//!
//! The helpers inspect the actual [`serde_json::Value`] variant, so they do
//! NOT assume the error is an integer — they report whatever type the caller
//! passed (number, boolean, array, object, …). For numbers they give the
//! exact corrected form (the stringified value, quoted); for other types
//! there is no derivable corrected value, so the message just says to retry
//! with a string.

use serde::de::Error as SerdeError;
use serde_json::Value;

/// Validate an optional (`Option<String>`) field. `null`/absent → `None`.
/// A string → `Some(s)`. Any other type → a teaching error.
pub(crate) fn optional_string_field_from_value<E: SerdeError>(
    value: Option<Value>,
    field: &str,
) -> Result<Option<String>, E> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(SerdeError::custom(wrong_type_message(&other, field))),
    }
}

/// Validate a required (`String`) field. A string → `Ok(s)`. `null` → a
/// "required, got null" error. Any other type → a teaching error.
pub(crate) fn required_string_field_from_value<E: SerdeError>(
    value: Value,
    field: &str,
) -> Result<String, E> {
    match value {
        Value::String(s) => Ok(s),
        Value::Null => Err(SerdeError::custom(format!(
            "field '{}' is required and must be a string, but you passed null. \
             Retry with \"{}\": \"<a string value>\".",
            field, field
        ))),
        other => Err(SerdeError::custom(wrong_type_message(&other, field))),
    }
}

/// Build the teaching message for a non-string, non-null value. Names the
/// field, reports the actual JSON type, and gives a concrete correction: for
/// numbers the exact quoted string form; for other types, a generic "retry
/// with a string" instruction.
fn wrong_type_message(other: &Value, field: &str) -> String {
    let (type_name, correction) = match other {
        Value::Number(n) => ("number", format!("Retry with \"{}\": \"{}\".", field, n)),
        Value::Bool(_) => ("boolean", format!("Retry with \"{}\" as a string.", field)),
        Value::Array(_) => ("array", format!("Retry with \"{}\" as a string.", field)),
        Value::Object(_) => ("object", format!("Retry with \"{}\" as a string.", field)),
        _ => ("value", format!("Retry with \"{}\" as a string.", field)),
    };
    format!(
        "field '{}' must be a string, but you passed a {}. {}",
        field, type_name, correction
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_accepts_string() {
        let r: Result<Option<String>, serde_json::Error> =
            optional_string_field_from_value(Some(Value::String("t1".into())), "id");
        assert_eq!(r.unwrap().as_deref(), Some("t1"));
    }

    #[test]
    fn optional_none_for_absent_or_null() {
        let r: Result<Option<String>, serde_json::Error> =
            optional_string_field_from_value(None, "id");
        assert!(r.unwrap().is_none());
        let r: Result<Option<String>, serde_json::Error> =
            optional_string_field_from_value(Some(Value::Null), "id");
        assert!(r.unwrap().is_none());
    }

    #[test]
    fn optional_integer_error_names_field_and_corrected_value() {
        let r: Result<Option<String>, serde_json::Error> =
            optional_string_field_from_value(Some(Value::Number(1.into())), "id");
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("'id' must be a string"), "{msg}");
        assert!(msg.contains("number"), "{msg}");
        assert!(msg.contains("\"id\": \"1\""), "{msg}");
    }

    #[test]
    fn optional_bool_error_names_type() {
        let r: Result<Option<String>, serde_json::Error> =
            optional_string_field_from_value(Some(Value::Bool(true)), "key");
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("boolean"), "{msg}");
        assert!(msg.contains("string"), "{msg}");
    }

    #[test]
    fn required_accepts_string() {
        let r: Result<String, serde_json::Error> =
            required_string_field_from_value(Value::String("k".into()), "finding_key");
        assert_eq!(r.unwrap(), "k");
    }

    #[test]
    fn required_integer_error_gives_corrected_value() {
        let r: Result<String, serde_json::Error> =
            required_string_field_from_value(Value::Number(7.into()), "finding_key");
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("number"), "{msg}");
        assert!(msg.contains("\"finding_key\": \"7\""), "{msg}");
    }

    #[test]
    fn required_null_error_says_required() {
        let r: Result<String, serde_json::Error> =
            required_string_field_from_value(Value::Null, "name");
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("required"), "{msg}");
        assert!(msg.contains("null"), "{msg}");
    }
}
