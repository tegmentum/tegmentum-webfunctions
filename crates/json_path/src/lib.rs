//! json_path — evaluate a JSONPath expression against a JSON string
//! and return the matched values as rows.
//!
//! Arguments (positional):
//!   0. json_source   — string literal containing a JSON document
//!   1. jsonpath_expr — string literal containing an RFC 9535 JSONPath expression
//!
//! Output: one row per matched value, single variable `value` bound to
//! each match. Scalars are typed exactly as in `parse_json`:
//!
//!   * bool           → xsd:boolean
//!   * number (int)   → xsd:integer
//!   * number (float) → xsd:decimal
//!   * string         → xsd:string
//!   * null           → row omitted (no unbound `value` cell)
//!
//! Structured matches (objects, arrays) are returned as JSON-stringified
//! xsd:string literals; pipe through `parse_json` to unfold further.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::Value as JsonValue;
use serde_json_path::JsonPath;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

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

/// Convert a JSON match into a WIT Value using the same typing rules
/// as `parse_json`. Returns `None` for JSON null so the caller can drop
/// the row rather than emit an unbound `value` cell.
fn scalar(v: &JsonValue) -> Option<Value> {
    match v {
        JsonValue::Null => None,
        JsonValue::Bool(b) => Some(typed_literal(b.to_string(), XSD_BOOLEAN)),
        JsonValue::Number(n) => {
            if n.is_i64() || n.is_u64() {
                Some(typed_literal(n.to_string(), XSD_INTEGER))
            } else {
                Some(typed_literal(n.to_string(), XSD_DECIMAL))
            }
        }
        JsonValue::String(s) => Some(typed_literal(s.clone(), XSD_STRING)),
        // Structured: preserve as JSON-stringified xsd:string. Consumer
        // can recurse with parse_json / json_path.
        _ => Some(typed_literal(v.to_string(), XSD_STRING)),
    }
}

fn string_of(arg: &Value, which: &str) -> Result<String, String> {
    match arg {
        Value::Literal(l) => Ok(l.label.clone()),
        _ => Err(format!("json_path: {} must be a string literal", which)),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!("json_path: expected 2 args, got {}", args.len()));
        }
        let source = string_of(&args[0], "argument 0 (JSON document)")?;
        let expression = string_of(&args[1], "argument 1 (JSONPath expression)")?;

        let document: JsonValue = serde_json::from_str(&source)
            .map_err(|e| format!("json_path: invalid JSON: {}", e))?;
        let path = JsonPath::parse(&expression)
            .map_err(|e| format!("json_path: bad expression: {}", e))?;

        let vars = vec!["value".into()];
        let rows: Vec<Vec<Binding>> = path
            .query(&document)
            .all()
            .into_iter()
            .filter_map(|node| scalar(node).map(|v| vec![Binding {
                name: "value".into(),
                value: v,
            }]))
            .collect();

        Ok(BindingSets { vars, rows })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("json_path: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("json_path: aggregate not applicable".into())
    }
    fn cardinality_estimate(input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        // Match count is data-dependent; be honest and mark it a guess.
        Ok(Cardinality { value: input.value.max(1.0), accuracy: Accuracy::PossiblyOff })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "json_path(json_document, jsonpath_expression) -> one row per match. \
                     Column 'value' holds each matched node: scalars are typed literals \
                     (xsd:boolean/integer/decimal/string); nulls drop the row; \
                     objects and arrays are JSON-stringified xsd:string. \
                     Pipe structured matches through parse_json to unfold rows."),
            }]],
        }
    }
}

export!(Component);
