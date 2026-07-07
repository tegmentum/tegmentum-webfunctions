//! parse_json — turn a JSON string into rows.
//!
//! The XSPARQL problem, done as a composable primitive rather than as a
//! language extension. Given a JSON document as a string literal, this
//! component returns binding-sets shaped as:
//!
//!   * Top-level object   → single row; keys become variables.
//!   * Top-level array-of-objects → one row per element; the union of
//!     all keys becomes the variable set (missing keys → unbound).
//!   * Top-level array-of-scalars → one row per element with variable
//!     `value` bound to the scalar.
//!
//! Scalar values are typed:
//!   * bool  → xsd:boolean
//!   * number (integer)  → xsd:integer
//!   * number (float)    → xsd:decimal
//!   * string → xsd:string
//!   * null → unbound (no Binding)
//!
//! Nested objects/arrays are returned as JSON-stringified xsd:string
//! literals; use `wf:call(<parse_json>, ?nested)` recursively to unfold.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::Value as JsonValue;
use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::collections::BTreeSet;

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

fn typed_literal(label: String, datatype: &str) -> Value {
    Value::Literal(Literal { label, datatype: datatype.into(), lang: None })
}

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

/// Convert a JSON scalar (or non-null structured value) to a WIT Value.
/// Returns None for JSON null so callers can produce an unbound cell.
fn scalar(v: &JsonValue) -> Option<Value> {
    match v {
        JsonValue::Null => None,
        JsonValue::Bool(b) => Some(typed_literal(b.to_string(), XSD_BOOLEAN)),
        JsonValue::Number(n) => {
            if n.is_i64() {
                Some(typed_literal(n.to_string(), XSD_INTEGER))
            } else if n.is_u64() {
                Some(typed_literal(n.to_string(), XSD_INTEGER))
            } else {
                Some(typed_literal(n.to_string(), XSD_DECIMAL))
            }
        }
        JsonValue::String(s) => Some(typed_literal(s.clone(), XSD_STRING)),
        // Structured: preserve as JSON string. Consumer can recurse.
        _ => Some(typed_literal(v.to_string(), XSD_STRING)),
    }
}

fn object_row(vars: &[String], obj: &serde_json::Map<String, JsonValue>) -> Vec<Binding> {
    let mut bindings = Vec::with_capacity(vars.len());
    for name in vars {
        if let Some(v) = obj.get(name) {
            if let Some(value) = scalar(v) {
                bindings.push(Binding { name: name.clone(), value });
            }
            // null → no binding, absent variable in output row
        }
    }
    bindings
}

fn json_source_of(arg: &Value) -> Result<&str, String> {
    match arg {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("parse_json: argument must be a string literal containing JSON".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("parse_json: expected 1 arg, got {}", args.len()));
        }
        let source = json_source_of(&args[0])?;
        let parsed: JsonValue = serde_json::from_str(source)
            .map_err(|e| format!("parse_json: invalid JSON: {}", e))?;

        match parsed {
            JsonValue::Object(obj) => {
                let vars: Vec<String> = obj.keys().cloned().collect();
                let row = object_row(&vars, &obj);
                Ok(BindingSets { vars, rows: vec![row] })
            }
            JsonValue::Array(items) => {
                // Choose vars: union of keys across all object elements,
                // OR just ["value"] if any element is a scalar.
                let mut vars_set: BTreeSet<String> = BTreeSet::new();
                let mut any_scalar = false;
                for item in &items {
                    match item {
                        JsonValue::Object(o) => vars_set.extend(o.keys().cloned()),
                        _ => any_scalar = true,
                    }
                }
                if any_scalar && vars_set.is_empty() {
                    let vars = vec!["value".into()];
                    let rows = items.iter()
                        .filter_map(|item| scalar(item).map(|v| vec![Binding {
                            name: "value".into(),
                            value: v,
                        }]))
                        .collect();
                    return Ok(BindingSets { vars, rows });
                }
                if any_scalar {
                    return Err("parse_json: array mixes objects and scalars — \
                                use one or the other".into());
                }
                let vars: Vec<String> = vars_set.into_iter().collect();
                let rows: Vec<Vec<Binding>> = items.iter().map(|item| {
                    if let JsonValue::Object(o) = item {
                        object_row(&vars, o)
                    } else {
                        Vec::new()
                    }
                }).collect();
                Ok(BindingSets { vars, rows })
            }
            other => {
                // Bare scalar — one row, one column named "value".
                if let Some(value) = scalar(&other) {
                    Ok(BindingSets {
                        vars: vec!["value".into()],
                        rows: vec![vec![Binding { name: "value".into(), value }]],
                    })
                } else {
                    Ok(BindingSets { vars: vec!["value".into()], rows: vec![] })
                }
            }
        }
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("parse_json: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("parse_json: aggregate not applicable".into())
    }
    fn cardinality_estimate(input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: input.value.max(1.0), accuracy: Accuracy::Injected })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "parse_json(json_string) -> binding-sets. \
                     Top-level object -> single row keyed by field name. \
                     Array-of-objects -> one row per element. \
                     Array-of-scalars -> one row per element, column 'value'. \
                     Nested structures returned as JSON-stringified literals; \
                     call parse_json again to unfold."),
            }]],
        }
    }
}

export!(Component);
