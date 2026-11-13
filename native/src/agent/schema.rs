//! JSON Schema subset used to check tool arguments and skill/workflow metadata.
//!
//! Supported keywords:
//! - `type`, either one name or a list of names: `null`, `boolean`, `object`, `array`,
//!   `string`, `number`, and `integer` (a JSON number that is a whole i64/u64);
//! - `enum`, compared with plain [`serde_json::Value`] equality;
//! - `required` and `properties` on objects. Unknown keys are allowed: `additionalProperties`
//!   is not enforced;
//! - `items` on arrays (recurse into every element);
//! - `minItems` / `maxItems` on arrays, `minLength` / `maxLength` on strings (counted in
//!   characters, not bytes), and `minimum` / `maximum` on numbers;
//! - `nullable: true` (OpenAPI style): `null` is accepted whatever `type` says.
//!
//! Everything else is ignored, so an unsupported keyword never makes a value invalid. That
//! covers `$schema`, `$ref`, `oneOf`, `anyOf`, `allOf`, `not`, `format`, `pattern`,
//! `patternProperties`, `additionalProperties`, annotations, boolean schemas (`true` / `false`)
//! and tuple-style `items` arrays. Unknown `type` names are ignored the same way.
//!
//! Errors are one line, starting with a JSON-pointer-ish path: `$` for the root value, `.key`
//! for an object member (quoted when the key has characters outside `[A-Za-z0-9_-]`) and `[i]`
//! for an array element, as in `at $.items[2]: expected integer, found string`.
//!
//! Nesting is capped at 64 schema levels, so a hostile schema cannot exhaust the stack.

use serde_json::Value;

/// Deepest schema nesting accepted during validation.
const MAX_DEPTH: usize = 64;

/// JSON type of `v` as named in schema errors.
///
/// A whole number is reported as `"integer"` and any other number as `"number"`. A number
/// written with a decimal point (`1.0`) is held as a float, so it reports `"number"` and does
/// not satisfy `"type": "integer"`.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.as_i64().is_some() || n.as_u64().is_some() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validate `value` against the JSON schema subset this app supports.
///
/// Returns `Ok(())` when the value is valid, otherwise a short `Err` naming the first failing
/// path and the reason. Keywords outside the subset are ignored, as documented at module level.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "$", 0)
}

/// Validate one (schema, value) pair at `path`, `depth` levels into the schema.
fn check(schema: &Value, value: &Value, path: &str, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("schema nesting is too deep".to_owned());
    }
    // A boolean schema, an array schema or any other non-object cannot constrain a value here.
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };

    if value.is_null() && schema.get("nullable").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }

    if let Some(kind) = schema.get("type") {
        check_type(kind, value, path)?;
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|candidate| candidate == value) {
            return Err(format!("at {path}: {value} is not one of {}", enum_list(allowed)));
        }
    }

    match value {
        Value::Object(map) => {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(key) {
                        return Err(format!("at {path}: missing required key \"{key}\""));
                    }
                }
            }
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, sub_schema) in properties {
                    if let Some(child) = map.get(key) {
                        check(sub_schema, child, &member_path(path, key), depth + 1)?;
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min {
                    return Err(format!(
                        "at {path}: expected at least {min} items, found {}",
                        items.len()
                    ));
                }
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
                if items.len() as u64 > max {
                    return Err(format!(
                        "at {path}: expected at most {max} items, found {}",
                        items.len()
                    ));
                }
            }
            // A tuple (`items` as an array) is outside the subset and is ignored.
            if let Some(sub_schema) = schema.get("items").filter(|s| s.is_object()) {
                for (i, child) in items.iter().enumerate() {
                    check(sub_schema, child, &format!("{path}[{i}]"), depth + 1)?;
                }
            }
        }
        Value::String(s) => {
            let length = s.chars().count() as u64;
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
                if length < min {
                    return Err(format!(
                        "at {path}: expected at least {min} characters, found {length}"
                    ));
                }
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
                if length > max {
                    return Err(format!(
                        "at {path}: expected at most {max} characters, found {length}"
                    ));
                }
            }
        }
        Value::Number(n) => {
            let Some(n) = n.as_f64() else {
                return Ok(());
            };
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
                if n < min {
                    return Err(format!("at {path}: expected at least {min}, found {n}"));
                }
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
                if n > max {
                    return Err(format!("at {path}: expected at most {max}, found {n}"));
                }
            }
        }
        Value::Null | Value::Bool(_) => {}
    }

    Ok(())
}

/// Check `value` against the `type` keyword, which is a name or a list of names.
fn check_type(kind: &Value, value: &Value, path: &str) -> Result<(), String> {
    let names: Vec<&str> = match kind {
        Value::String(name) => vec![name.as_str()],
        Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
        _ => return Ok(()),
    };
    let names: Vec<&str> = names.into_iter().filter(|name| is_type_name(name)).collect();
    if names.is_empty() || names.iter().any(|name| matches_type(name, value)) {
        return Ok(());
    }
    Err(format!(
        "at {path}: expected {}, found {}",
        join_alternatives(&names),
        type_name(value)
    ))
}

/// Whether `name` is a type this validator knows.
fn is_type_name(name: &str) -> bool {
    matches!(
        name,
        "null" | "boolean" | "number" | "integer" | "string" | "array" | "object"
    )
}

/// Whether `value` satisfies the named type.
///
/// `"number"` accepts every JSON number, including whole ones; `"integer"` accepts only numbers
/// that fit an i64 or u64 without a fractional part.
fn matches_type(name: &str, value: &Value) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

/// `"a"`, `"a or b"`, `"a, b or c"`.
fn join_alternatives(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// The `enum` entries as compact JSON, e.g. `["a","b"]`.
fn enum_list(allowed: &[Value]) -> String {
    let entries: Vec<String> = allowed.iter().map(Value::to_string).collect();
    format!("[{}]", entries.join(","))
}

/// Path of an object member: `.key`, or `."quoted key"` when the key needs quoting.
fn member_path(parent: &str, key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if plain {
        format!("{parent}.{key}")
    } else {
        format!("{parent}.{}", Value::String(key.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn type_name_reports_every_json_type() {
        assert_eq!(type_name(&json!(null)), "null");
        assert_eq!(type_name(&json!(true)), "boolean");
        assert_eq!(type_name(&json!(7)), "integer");
        assert_eq!(type_name(&json!(7.5)), "number");
        assert_eq!(type_name(&json!("text")), "string");
        assert_eq!(type_name(&json!([1])), "array");
        assert_eq!(type_name(&json!({"a": 1})), "object");
    }

    #[test]
    fn each_simple_type_matches_and_mismatches() {
        // null
        assert!(validate(&json!({"type": "null"}), &json!(null)).is_ok());
        assert_eq!(
            validate(&json!({"type": "null"}), &json!(0)).unwrap_err(),
            "at $: expected null, found integer"
        );
        // boolean
        assert!(validate(&json!({"type": "boolean"}), &json!(false)).is_ok());
        assert_eq!(
            validate(&json!({"type": "boolean"}), &json!("no")).unwrap_err(),
            "at $: expected boolean, found string"
        );
        // number: whole numbers are numbers as well
        assert!(validate(&json!({"type": "number"}), &json!(2)).is_ok());
        assert!(validate(&json!({"type": "number"}), &json!(2.5)).is_ok());
        assert_eq!(
            validate(&json!({"type": "number"}), &json!("2")).unwrap_err(),
            "at $: expected number, found string"
        );
        // integer
        assert!(validate(&json!({"type": "integer"}), &json!(-3)).is_ok());
        assert_eq!(
            validate(&json!({"type": "integer"}), &json!(1.5)).unwrap_err(),
            "at $: expected integer, found number"
        );
        // string
        assert!(validate(&json!({"type": "string"}), &json!("a")).is_ok());
        assert_eq!(
            validate(&json!({"type": "string"}), &json!(1.5)).unwrap_err(),
            "at $: expected string, found number"
        );
        // array
        assert!(validate(&json!({"type": "array"}), &json!([])).is_ok());
        assert_eq!(
            validate(&json!({"type": "array"}), &json!({})).unwrap_err(),
            "at $: expected array, found object"
        );
        // object
        assert!(validate(&json!({"type": "object"}), &json!({})).is_ok());
        assert_eq!(
            validate(&json!({"type": "object"}), &json!([])).unwrap_err(),
            "at $: expected object, found array"
        );
    }

    #[test]
    fn type_arrays_accept_any_listed_type() {
        let schema = json!({"type": ["string", "null"]});
        assert!(validate(&schema, &json!(null)).is_ok());
        assert!(validate(&schema, &json!("a")).is_ok());
        assert_eq!(
            validate(&schema, &json!(1)).unwrap_err(),
            "at $: expected string or null, found integer"
        );

        let schema = json!({"type": ["boolean", "integer", "array"]});
        assert!(validate(&schema, &json!([1])).is_ok());
        assert_eq!(
            validate(&schema, &json!("a")).unwrap_err(),
            "at $: expected boolean, integer or array, found string"
        );
    }

    #[test]
    fn nullable_accepts_null_whatever_the_type() {
        let schema = json!({"type": "string", "nullable": true});
        assert!(validate(&schema, &json!(null)).is_ok());
        assert_eq!(
            validate(&schema, &json!(1)).unwrap_err(),
            "at $: expected string, found integer"
        );
        // Without the flag, null does not pass a non-null type.
        assert!(validate(&json!({"type": "string"}), &json!(null)).is_err());
    }

    #[test]
    fn enum_passes_listed_values_and_reports_the_rest() {
        let schema = json!({"type": "string", "enum": ["a", "b"]});
        assert!(validate(&schema, &json!("b")).is_ok());
        assert_eq!(
            validate(&schema, &json!("x")).unwrap_err(),
            "at $: \"x\" is not one of [\"a\",\"b\"]"
        );

        let schema = json!({"enum": [1, 2]});
        assert!(validate(&schema, &json!(2)).is_ok());
        assert_eq!(
            validate(&schema, &json!(3)).unwrap_err(),
            "at $: 3 is not one of [1,2]"
        );
    }

    #[test]
    fn missing_required_key_is_named_at_the_object() {
        let schema = json!({
            "type": "object",
            "required": ["query"],
            "properties": {"query": {"type": "string"}}
        });
        assert_eq!(
            validate(&schema, &json!({})).unwrap_err(),
            "at $: missing required key \"query\""
        );
        assert!(validate(&schema, &json!({"query": "hi"})).is_ok());
        // Extra keys are allowed; `required` says nothing about them.
        assert!(validate(&schema, &json!({"query": "hi", "extra": 1})).is_ok());
        // `required` does not apply to a value that is not an object.
        assert!(validate(&json!({"required": ["query"]}), &json!(null)).is_ok());
    }

    #[test]
    fn nested_properties_recurse_and_allow_unknown_keys() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "object",
                    "properties": {"n": {"type": "integer"}}
                }
            }
        });
        assert!(validate(&schema, &json!({"value": {"n": 4}})).is_ok());
        assert_eq!(
            validate(&schema, &json!({"value": {"n": "4"}})).unwrap_err(),
            "at $.value.n: expected integer, found string"
        );
        assert!(validate(&schema, &json!({"value": {"n": 4, "other": true}})).is_ok());
    }

    #[test]
    fn array_item_errors_name_the_index() {
        let schema = json!({
            "properties": {
                "items": {"type": "array", "items": {"type": "integer"}}
            }
        });
        assert!(validate(&schema, &json!({"items": [1, 2, 3]})).is_ok());
        assert_eq!(
            validate(&schema, &json!({"items": [1, 2, "x"]})).unwrap_err(),
            "at $.items[2]: expected integer, found string"
        );

        let root = json!({"type": "array", "items": {"type": "string"}});
        assert_eq!(
            validate(&root, &json!(["a", 1])).unwrap_err(),
            "at $[1]: expected string, found integer"
        );
    }

    #[test]
    fn string_lengths_count_characters_not_bytes() {
        let schema = json!({"type": "string", "minLength": 2, "maxLength": 3});
        // "é€" is two characters but five bytes.
        assert!(validate(&schema, &json!("é€")).is_ok());
        assert_eq!(
            validate(&schema, &json!("é")).unwrap_err(),
            "at $: expected at least 2 characters, found 1"
        );
        assert_eq!(
            validate(&schema, &json!("abcd")).unwrap_err(),
            "at $: expected at most 3 characters, found 4"
        );
    }

    #[test]
    fn numeric_bounds_are_inclusive() {
        let schema = json!({"type": "number", "minimum": 1, "maximum": 10});
        assert!(validate(&schema, &json!(1)).is_ok());
        assert!(validate(&schema, &json!(10)).is_ok());
        assert_eq!(
            validate(&schema, &json!(0)).unwrap_err(),
            "at $: expected at least 1, found 0"
        );
        assert_eq!(
            validate(&schema, &json!(11)).unwrap_err(),
            "at $: expected at most 10, found 11"
        );
    }

    #[test]
    fn item_counts_are_checked() {
        let schema = json!({"type": "array", "minItems": 2, "maxItems": 3});
        assert!(validate(&schema, &json!([1, 2])).is_ok());
        assert_eq!(
            validate(&schema, &json!([1])).unwrap_err(),
            "at $: expected at least 2 items, found 1"
        );
        assert_eq!(
            validate(&schema, &json!([1, 2, 3, 4])).unwrap_err(),
            "at $: expected at most 3 items, found 4"
        );
    }

    #[test]
    fn unknown_keywords_are_ignored() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "description": "an address",
            "format": "email",
            "pattern": "^[0-9]+$",
            "additionalProperties": false,
            "oneOf": [{"type": "integer"}],
            "type": "string"
        });
        assert!(validate(&schema, &json!("not an email at all")).is_ok());
        assert!(validate(&schema, &json!("with keys")).is_ok());

        // A schema made only of unsupported keywords constrains nothing.
        let only_unknown = json!({"oneOf": [{"type": "integer"}], "anyOf": [{"type": "null"}]});
        assert!(validate(&only_unknown, &json!("a")).is_ok());
        assert!(validate(&only_unknown, &json!(1)).is_ok());

        // An unknown type name places no constraint either.
        assert!(validate(&json!({"type": "date"}), &json!(1)).is_ok());
    }

    #[test]
    fn unusual_keys_are_quoted_in_the_path() {
        let schema = json!({"properties": {"a b": {"type": "integer"}}});
        assert_eq!(
            validate(&schema, &json!({"a b": "x"})).unwrap_err(),
            "at $.\"a b\": expected integer, found string"
        );
        // Keys outside the plain set are quoted; dashes and underscores are not.
        let schema = json!({"properties": {"run-cmd_2": {"type": "integer"}}});
        assert_eq!(
            validate(&schema, &json!({"run-cmd_2": "x"})).unwrap_err(),
            "at $.run-cmd_2: expected integer, found string"
        );
    }

    #[test]
    fn schema_nesting_is_depth_limited() {
        // A comfortable nesting still validates.
        let (schema, value) = nested(MAX_DEPTH - 1);
        assert!(validate(&schema, &value).is_ok());
        // Past the cap the walk stops instead of recursing further.
        let (schema, value) = nested(MAX_DEPTH + 10);
        assert_eq!(
            validate(&schema, &value).unwrap_err(),
            "schema nesting is too deep"
        );
    }

    /// A chain of `depth` single-member objects on both sides.
    fn nested(depth: usize) -> (Value, Value) {
        let mut schema = json!({"type": "integer"});
        let mut value = json!(1);
        for _ in 0..depth {
            schema = json!({"type": "object", "properties": {"a": schema}});
            value = json!({"a": value});
        }
        (schema, value)
    }
}
