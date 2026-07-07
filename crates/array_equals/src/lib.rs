//! array_equals — deep equality on two array literals.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/equals. The
//! source in the semantalytics tree was a broken copy of `dedupe` (called a
//! non-existent `.dedupe()` iterator method, read only `value_1`, returned
//! an array datatype instead of a boolean). This crate implements the
//! obviously-intended behaviour: pairwise element equality of two arrays.

wit_bindgen::generate!({ world: "webfunction", path: "wit" });

use serde_json::Value as JsonValue;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:      &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_BOOLEAN:     &str = "http://www.w3.org/2001/XMLSchema#boolean";
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

fn typed_literal(label: String, dt: &str) -> Value {
    Value::Literal(Literal { label, datatype: dt.into(), lang: None })
}
fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn decode_array(v: &Value, side: &str) -> Result<Vec<JsonValue>, String> {
    match v {
        Value::Literal(l) if l.datatype == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.label)
                .map_err(|e| format!("array_equals: {} arg invalid array literal JSON: {}", side, e))?;
            match parsed {
                JsonValue::Array(a) => Ok(a),
                _ => Err(format!("array_equals: {} arg array literal is not a JSON array", side)),
            }
        }
        _ => Err(format!("array_equals: {} arg not an array literal (datatype {})", side, ARRAY_DATATYPE)),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!("array_equals: expected 2 args, got {}", args.len()));
        }
        let a = decode_array(&args[0], "first")?;
        let b = decode_array(&args[1], "second")?;
        let eq = a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y);
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: typed_literal(eq.to_string(), XSD_BOOLEAN),
            }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("array_equals: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("array_equals: aggregate not applicable".into())
    }
    fn cardinality_estimate(_i: Cardinality, _a: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "array_equals(a, b) -> xsd:boolean, true iff arrays a and b \
                     have the same length and elements in the same order."),
            }]],
        }
    }
}

export!(Component);
