//! Tool arguments a model got almost right, mended against the tool's JSON
//! schema before the tool sees them: opencode's tool input repair. Models
//! send `"20"` for a number, an object as a JSON string, or one item where
//! a list goes. A repair is made only where the schema leaves no doubt
//! about what was meant; anything else reaches the tool as sent, and its
//! own error tells the model.

use serde_json::{Map, Value};

/// The largest integer a model's string is taken as, as in opencode: past
/// it, a double no longer holds every integer.
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// `value` with what `schema` unambiguously allows repaired:
/// - a JSON string where an object or array goes is parsed,
/// - `"20"` where an integer or number goes becomes the number (`"1.5"`
///   is no integer, `"abc"` no number),
/// - `"true"` or `"false"` where a boolean goes becomes it,
/// - one item where an array goes becomes an array of it,
/// - `null`, or `{}` where no object goes, is dropped for a property that
///   is not required.
///
/// Only `type`, `properties`, `required` and `items` are read, which is all
/// nth's tools write; a schema without a `type` leaves the value alone.
pub(crate) fn repair(value: Value, schema: &Value) -> Value {
    match schema.get("type").and_then(Value::as_str) {
        Some("integer") => integer(value),
        Some("number") => number(value),
        Some("boolean") => match value.as_str() {
            Some("true") => Value::Bool(true),
            Some("false") => Value::Bool(false),
            _ => value,
        },
        Some("object") => object(value, schema),
        Some("array") => array(value, schema),
        _ => value,
    }
}

fn integer(value: Value) -> Value {
    let Some(text) = value.as_str() else {
        return value;
    };
    match text.trim().parse::<i64>() {
        Ok(n) if n.abs() <= MAX_SAFE_INTEGER => Value::from(n),
        _ => value,
    }
}

fn number(value: Value) -> Value {
    let Some(text) = value.as_str().map(str::trim) else {
        return value;
    };
    // An integer stays one, so a tool reading it into an integer type
    // still can.
    if let Ok(n) = text.parse::<i64>() {
        return Value::from(n);
    }
    // Rust also reads `inf` and `NaN`, which JSON has no number for.
    match text.parse::<f64>() {
        Ok(n) if n.is_finite() => Value::from(n),
        _ => value,
    }
}

fn object(value: Value, schema: &Value) -> Value {
    let Some(mut fields) = parsed(value.clone()).and_then(|v| match v {
        Value::Object(fields) => Some(fields),
        _ => None,
    }) else {
        return value;
    };
    let empty = Map::new();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required = |key: &str| {
        schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.iter().any(|k| k == key))
    };
    for (key, property) in properties {
        let Some(current) = fields.remove(key) else {
            continue;
        };
        if !required(key) && placeholder(&current, property) {
            continue;
        }
        fields.insert(key.clone(), repair(current, property));
    }
    Value::Object(fields)
}

/// Whether `value` stands in for an optional property left out: `null`, or
/// `{}` where no object goes. Only for a property of one plain type, since
/// an enum may list `null` among its values.
fn placeholder(value: &Value, property: &Value) -> bool {
    let Some(kind) = property.get("type").and_then(Value::as_str) else {
        return false;
    };
    if kind == "null" || property.get("enum").is_some() {
        return false;
    }
    match value {
        Value::Null => true,
        Value::Object(fields) => fields.is_empty() && kind != "object",
        _ => false,
    }
}

fn array(value: Value, schema: &Value) -> Value {
    let items = schema.get("items").filter(|items| items.is_object());
    if let Some(Value::Array(list)) = parsed(value.clone()) {
        return match items {
            Some(items) => list.into_iter().map(|item| repair(item, items)).collect(),
            None => Value::Array(list),
        };
    }
    // One item in place of the list: wrapped only when, repaired, it is
    // what an item has to be.
    let Some(items) = items else {
        return value;
    };
    let item = repair(value.clone(), items);
    let fits = match items.get("type").and_then(Value::as_str) {
        Some("object") => item.is_object(),
        Some("array") => item.is_array(),
        Some("integer") => item.is_i64() || item.is_u64(),
        Some("number") => item.is_number(),
        Some("string") => item.is_string(),
        Some("boolean") => item.is_boolean(),
        _ => false,
    };
    if fits {
        Value::Array(vec![item])
    } else {
        value
    }
}

/// `value` read as JSON when it is a string, else as it is; `None` for a
/// string that is no JSON.
fn parsed(value: Value) -> Option<Value> {
    match value {
        Value::String(text) => serde_json::from_str(&text).ok(),
        value => Some(value),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(properties: Value) -> Value {
        json!({ "type": "object", "properties": properties })
    }

    #[test]
    fn valid_input_is_kept() {
        let input =
            json!({ "count": 2, "nested": { "enabled": true }, "items": [2, 3], "extra": "keep" });
        let schema = object(json!({
            "count": { "type": "integer" },
            "nested": object(json!({ "enabled": { "type": "boolean" } })),
            "items": { "type": "array", "items": { "type": "number" } },
        }));
        assert_eq!(repair(input.clone(), &schema), input);
    }

    #[test]
    fn stringified_objects_and_arrays_are_parsed() {
        let schema = object(json!({
            "count": { "type": "integer" },
            "item": object(json!({ "enabled": { "type": "boolean" } })),
            "list": { "type": "array", "items": { "type": "integer" } },
        }));
        let input = json!(r#"{"count":"2","item":"{\"enabled\":\"false\"}","list":"[\"3\"]"}"#);
        assert_eq!(
            repair(input, &schema),
            json!({ "count": 2, "item": { "enabled": false }, "list": [3] })
        );
        for kept in [json!("{broken"), json!("[]"), Value::Null] {
            assert_eq!(repair(kept.clone(), &schema), kept);
        }
    }

    #[test]
    fn optional_nulls_and_placeholders_are_dropped() {
        let schema = json!({
            "type": "object",
            "properties": {
                "optional": { "type": "string" },
                "required": { "type": "string" },
                "enumerated": { "type": "string", "enum": ["a", "b"] },
                "placeholder": { "type": "integer" },
                "array": { "type": "array", "items": { "type": "string" } },
                "requiredPlaceholder": { "type": "boolean" },
                "object": { "type": "object" },
                "unknown": {},
            },
            "required": ["required", "requiredPlaceholder"],
        });
        let input = json!({
            "optional": null,
            "required": null,
            "enumerated": null,
            "placeholder": {},
            "array": {},
            "requiredPlaceholder": {},
            "object": {},
            "unknown": null,
            "undeclared": null,
        });
        assert_eq!(
            repair(input, &schema),
            json!({
                "required": null,
                "enumerated": null,
                "requiredPlaceholder": {},
                "object": {},
                "unknown": null,
                "undeclared": null,
            })
        );
    }

    #[test]
    fn numeric_and_boolean_strings_are_coerced() {
        let input = json!({
            "number": "1.5",
            "integer": "42",
            "whole": "20",
            "enabled": "true",
            "disabled": "false",
            "valid": 3,
            "empty": " ",
            "word": "abc",
            "infinite": "Infinity",
            "inf": "inf",
            "fractional": "1.5",
            "unsafe": "9007199254740992",
            "uppercase": "TRUE",
        });
        let schema = object(json!({
            "number": { "type": "number" },
            "integer": { "type": "integer" },
            "whole": { "type": "number" },
            "enabled": { "type": "boolean" },
            "disabled": { "type": "boolean" },
            "valid": { "type": "integer" },
            "empty": { "type": "number" },
            "word": { "type": "integer" },
            "infinite": { "type": "number" },
            "inf": { "type": "number" },
            "fractional": { "type": "integer" },
            "unsafe": { "type": "integer" },
            "uppercase": { "type": "boolean" },
        }));
        let mut expected = input.clone();
        expected["number"] = json!(1.5);
        expected["integer"] = json!(42);
        expected["whole"] = json!(20);
        expected["enabled"] = json!(true);
        expected["disabled"] = json!(false);
        let repaired = repair(input, &schema);
        assert_eq!(repaired, expected);
        assert!(repaired["whole"].is_i64(), "20 stays an integer");
    }

    #[test]
    fn compatible_scalars_are_wrapped() {
        let input = json!({
            "text": "one",
            "integer": "42",
            "boolean": "false",
            "item": r#"{"count":"2"}"#,
            "incompatible": 2,
            "fractional": 1.5,
            "unconstrained": "4",
        });
        let schema = object(json!({
            "text": { "type": "array", "items": { "type": "string" } },
            "integer": { "type": "array", "items": { "type": "integer" } },
            "boolean": { "type": "array", "items": { "type": "boolean" } },
            "item": { "type": "array", "items": object(json!({ "count": { "type": "integer" } })) },
            "incompatible": { "type": "array", "items": { "type": "string" } },
            "fractional": { "type": "array", "items": { "type": "integer" } },
            "unconstrained": { "type": "array" },
        }));
        assert_eq!(
            repair(input, &schema),
            json!({
                "text": ["one"],
                "integer": [42],
                "boolean": [false],
                "item": [{ "count": 2 }],
                "incompatible": 2,
                "fractional": 1.5,
                "unconstrained": "4",
            })
        );
    }

    #[test]
    fn nested_question_like_input_is_repaired() {
        let input = json!({ "questions": [{
            "question": "Pick one",
            "multiple": "false",
            "options": { "label": "First", "description": null },
        }] });
        let option = json!({
            "type": "object",
            "properties": { "label": { "type": "string" }, "description": { "type": "string" } },
            "required": ["label"],
        });
        let schema = object(json!({ "questions": {
            "type": "array",
            "items": object(json!({
                "question": { "type": "string" },
                "multiple": { "type": "boolean" },
                "options": { "type": "array", "items": option },
            })),
        } }));
        assert_eq!(
            repair(input, &schema),
            json!({ "questions": [{
                "question": "Pick one",
                "multiple": false,
                "options": [{ "label": "First" }],
            }] })
        );
    }

    #[test]
    fn schemas_without_a_type_leave_the_value_alone() {
        let input = json!({ "numeric": "2" });
        assert_eq!(repair(input.clone(), &json!({})), input);
        let untyped = json!({ "properties": { "numeric": { "type": "integer" } } });
        assert_eq!(repair(input.clone(), &untyped), input);
    }
}
