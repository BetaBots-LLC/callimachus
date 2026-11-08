//! Test-only JSON Schema checker for the published contracts (`cal --json`, MCP output
//! schemas). It covers the subset schemars emits (type, properties, required, items,
//! prefixItems, additionalProperties, anyOf / oneOf, enum, const, `$ref` into `$defs`) and is
//! stricter than JSON Schema in one way: an object with declared `properties` may carry no
//! other key, so a field added to a payload without reaching its schema fails a test instead
//! of shipping undocumented.

use serde_json::Value;

/// Check `value` against the root `schema`. The error names the path of the first mismatch.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, schema, value, "$")
}

fn check(root: &Value, schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let obj = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{path}: no value is allowed here")),
        Value::Object(o) => o,
        other => return Err(format!("{path}: schema is not an object: {other}")),
    };
    if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
        let name = r
            .strip_prefix("#/$defs/")
            .ok_or_else(|| format!("{path}: unsupported $ref {r}"))?;
        let target = root
            .get("$defs")
            .and_then(|d| d.get(name))
            .ok_or_else(|| format!("{path}: dangling $ref {r}"))?;
        check(root, target, value, path)?;
    }
    if let Some(t) = obj.get("type") {
        let types: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            other => return Err(format!("{path}: bad type keyword {other}")),
        };
        if !types.iter().any(|t| is_type(value, t)) {
            return Err(format!("{path}: {value} is not {types:?}"));
        }
    }
    if let Some(c) = obj.get("const") {
        if c != value {
            return Err(format!("{path}: {value} is not the constant {c}"));
        }
    }
    if let Some(e) = obj.get("enum").and_then(Value::as_array) {
        if !e.contains(value) {
            return Err(format!("{path}: {value} is not one of {e:?}"));
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = obj.get(key).and_then(Value::as_array) {
            let errors: Vec<String> = branches
                .iter()
                .filter_map(|b| check(root, b, value, path).err())
                .collect();
            if errors.len() == branches.len() {
                return Err(format!(
                    "{path}: matches no {key} branch: {}",
                    errors.join(" | ")
                ));
            }
        }
    }
    if let Value::Object(map) = value {
        let props = obj.get("properties").and_then(Value::as_object);
        if let Some(required) = obj.get("required").and_then(Value::as_array) {
            for r in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(r) {
                    return Err(format!("{path}: missing required `{r}`"));
                }
            }
        }
        let extra = obj.get("additionalProperties");
        for (k, v) in map {
            let child = format!("{path}.{k}");
            match props.and_then(|p| p.get(k)) {
                Some(s) => check(root, s, v, &child)?,
                None => match extra {
                    Some(s) => check(root, s, v, &child)?,
                    // Declared properties and no additionalProperties: an undeclared key.
                    None if props.is_some() => {
                        return Err(format!("{child}: not declared in the schema"))
                    }
                    None => {}
                },
            }
        }
    }
    if let Value::Array(items) = value {
        let prefix = obj.get("prefixItems").and_then(Value::as_array);
        for (i, v) in items.iter().enumerate() {
            let child = format!("{path}[{i}]");
            if let Some(s) = prefix.and_then(|p| p.get(i)) {
                check(root, s, v, &child)?;
            } else if let Some(s) = obj.get("items") {
                check(root, s, v, &child)?;
            }
        }
    }
    Ok(())
}

fn is_type(value: &Value, t: &str) -> bool {
    match t {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_checker_catches_what_the_contract_tests_rely_on() {
        let schema = json!({
            "type": "object",
            "properties": {
                "n": { "type": "integer" },
                "s": { "type": ["string", "null"] },
                "list": { "type": "array", "items": { "$ref": "#/$defs/Row" } }
            },
            "required": ["n"],
            "$defs": { "Row": { "type": "object", "properties": { "id": { "type": "integer" } } } }
        });
        assert!(validate(
            &schema,
            &json!({ "n": 1, "s": null, "list": [{ "id": 2 }] })
        )
        .is_ok());
        assert!(validate(&schema, &json!({ "s": "x" }))
            .unwrap_err()
            .contains("missing"));
        assert!(validate(&schema, &json!({ "n": 1.5 })).is_err());
        assert!(validate(&schema, &json!({ "n": 1, "extra": 0 }))
            .unwrap_err()
            .contains("not declared"));
        assert!(
            validate(&schema, &json!({ "n": 1, "list": [{ "id": "x" }] }))
                .unwrap_err()
                .contains("$.list[0].id")
        );
        let either = json!({ "anyOf": [{ "type": "string" }, { "type": "object" }] });
        assert!(validate(&either, &json!({})).is_ok());
        assert!(validate(&either, &json!(3)).is_err());
    }
}
