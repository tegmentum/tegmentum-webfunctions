//! array_reverse — reverse the order of elements in an array literal.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/reverse.

wit_bindgen::generate!({ world: "webfunction", path: "wit" });

use serde_json::Value as JsonValue;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:      &str = "http://www.w3.org/2001/XMLSchema#string";
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn decode_array(v: &Value) -> Result<Vec<JsonValue>, String> {
    match v {
        Value::Literal(l) if l.datatype == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.label)
                .map_err(|e| format!("array_reverse: invalid array literal JSON: {}", e))?;
            match parsed {
                JsonValue::Array(a) => Ok(a),
                _ => Err("array_reverse: array literal is not a JSON array".into()),
            }
        }
        _ => Err(format!("array_reverse: expected array literal (datatype {})", ARRAY_DATATYPE)),
    }
}

fn encode_array_json(items: Vec<JsonValue>) -> Value {
    Value::Literal(Literal {
        label: JsonValue::Array(items).to_string(),
        datatype: ARRAY_DATATYPE.into(),
        lang: None,
    })
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("array_reverse: expected 1 arg, got {}", args.len()));
        }
        let mut arr = decode_array(&args[0])?;
        arr.reverse();
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding { name: "result".into(), value: encode_array_json(arr) }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("array_reverse: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("array_reverse: aggregate not applicable".into())
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
                    "array_reverse(array) -> array with element order reversed."),
            }]],
        }
    }
}

export!(Component);
