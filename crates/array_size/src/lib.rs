//! array_size — length of an array literal.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/size.

wit_bindgen::generate!({ world: "webfunction", path: "wit" });

use serde_json::Value as JsonValue;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:      &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER:     &str = "http://www.w3.org/2001/XMLSchema#integer";
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

fn typed_literal(label: String, dt: &str) -> Value {
    Value::Literal(Literal { label, datatype: dt.into(), lang: None })
}
fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn decode_array_len(v: &Value) -> Result<usize, String> {
    match v {
        Value::Literal(l) if l.datatype == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.label)
                .map_err(|e| format!("array_size: invalid array literal JSON: {}", e))?;
            let arr = parsed.as_array()
                .ok_or_else(|| "array_size: array literal is not a JSON array".to_string())?;
            Ok(arr.len())
        }
        _ => Err(format!("array_size: expected array literal (datatype {})", ARRAY_DATATYPE)),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("array_size: expected 1 arg, got {}", args.len()));
        }
        let len = decode_array_len(&args[0])?;
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: typed_literal(len.to_string(), XSD_INTEGER),
            }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("array_size: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("array_size: aggregate not applicable".into())
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
                    "array_size(array) -> xsd:integer number of elements."),
            }]],
        }
    }
}

export!(Component);
