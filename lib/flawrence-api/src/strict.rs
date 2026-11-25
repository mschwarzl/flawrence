use serde_json::Value;

/// Whether a schema fits the subset providers can enforce in strict tool-call
/// mode. A schema that does not fit is still sent — only the `strict` flag is
/// dropped, so the tool degrades to ordinary (unconstrained) calling instead of
/// being rejected by the API.
pub fn strict_schema_supported(schema: &Value) -> bool {
    let Some(root) = schema.as_object() else {
        return false;
    };
    if root.get("type").and_then(Value::as_str) != Some("object")
        && !root.contains_key("properties")
    {
        return false;
    }
    strict_schema_node_supported(schema)
}

fn strict_schema_node_supported(schema: &Value) -> bool {
    const UNSUPPORTED_KEYWORDS: &[&str] = &[
        "allOf",
        "not",
        "if",
        "then",
        "else",
        "dependentRequired",
        "dependentSchemas",
        "patternProperties",
        "unevaluatedProperties",
        "propertyNames",
        "minProperties",
        "maxProperties",
        "unevaluatedItems",
        "contains",
        "minContains",
        "maxContains",
        "uniqueItems",
        "default",
        // Numeric range facets. Strict mode rejects these on integer/number
        // types ("For 'integer' type, property 'minimum' is not supported"), and
        // they arrive unbidden: an unsigned Rust field (`usize`/`u32`) is
        // serialised with `minimum: 0`. Dropping strict here degrades the tool
        // to unconstrained calling, which the API accepts, rather than 400ing.
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        // String facets and `format` are accepted unevenly across providers
        // and versions; `prepare_schema` folds ours into descriptions, so a
        // schema that still carries one degrades rather than risking a 400.
        "minLength",
        "maxLength",
        "pattern",
        "format",
    ];

    match schema {
        Value::Object(object) => {
            if UNSUPPORTED_KEYWORDS
                .iter()
                .any(|keyword| object.contains_key(*keyword))
            {
                return false;
            }

            let properties = object.get("properties").and_then(Value::as_object);
            if object.get("type").and_then(Value::as_str) == Some("object")
                || object.contains_key("properties")
            {
                let Some(properties) = properties else {
                    return false;
                };
                let Some(required) = object.get("required").and_then(Value::as_array) else {
                    return properties.is_empty()
                        && object.get("additionalProperties") == Some(&Value::Bool(false));
                };
                let required_names: std::collections::HashSet<&str> =
                    required.iter().filter_map(Value::as_str).collect();
                if object.get("additionalProperties") != Some(&Value::Bool(false))
                    || required.len() != properties.len()
                    || required_names.len() != required.len()
                    || properties
                        .keys()
                        .any(|name| !required_names.contains(name.as_str()))
                {
                    return false;
                }
            }

            // Recurse into each property's schema, but never into the
            // `properties` map itself: its keys are field names chosen by the
            // tool author, and a tool with a `pattern` or `format` argument
            // must not be mistaken for a schema using those keywords.
            if let Some(properties) = properties
                && !properties.values().all(strict_schema_node_supported)
            {
                return false;
            }
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "properties")
                .all(|(_, value)| strict_schema_node_supported(value))
        }
        Value::Array(values) => values.iter().all(strict_schema_node_supported),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(props: Value, required: Value) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": props,
            "required": required,
        })
    }

    #[test]
    fn a_plain_object_is_strict_supported() {
        let schema = obj(json!({ "q": { "type": "string" } }), json!(["q"]));
        assert!(strict_schema_supported(&schema));
    }

    #[test]
    fn an_integer_with_minimum_is_not_strict_supported() {
        // The artifact tool's `offset` (Option<usize>) serialises like this;
        // strict mode rejects `minimum`, so the flag must be dropped instead.
        let schema = obj(
            json!({ "offset": { "type": ["integer", "null"], "minimum": 0 } }),
            json!(["offset"]),
        );
        assert!(!strict_schema_supported(&schema));
    }

    #[test]
    fn other_numeric_range_facets_are_also_rejected() {
        for facet in [
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ] {
            let schema = obj(
                json!({ "n": { "type": "integer", facet: 1 } }),
                json!(["n"]),
            );
            assert!(
                !strict_schema_supported(&schema),
                "{facet} should drop strict support"
            );
        }
    }
}
