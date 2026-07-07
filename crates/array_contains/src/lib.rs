//! array_contains — membership test on an array literal.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/contains.
//! Element equality is component-wise on the Value variants (iri, literal
//! including datatype+lang, or bnode).

wit_bindgen::generate!({ world: "webfunction", path: "wit" });

use serde_json::{Map as JsonMap, Value as JsonValue};
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

fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Iri(s) => {
            let mut m = JsonMap::new();
            m.insert("iri".into(), JsonValue::String(s.clone()));
            JsonValue::Object(m)
        }
        Value::Literal(l) => {
            let mut inner = JsonMap::new();
            inner.insert("label".into(), JsonValue::String(l.label.clone()));
            inner.insert("datatype".into(), JsonValue::String(l.datatype.clone()));
            inner.insert("lang".into(), match &l.lang {
                Some(s) => JsonValue::String(s.clone()),
                None => JsonValue::Null,
            });
            let mut m = JsonMap::new();
            m.insert("literal".into(), JsonValue::Object(inner));
            JsonValue::Object(m)
        }
        Value::Bnode(s) => {
            let mut m = JsonMap::new();
            m.insert("bnode".into(), JsonValue::String(s.clone()));
            JsonValue::Object(m)
        }
    }
}

fn decode_array(v: &Value) -> Result<Vec<JsonValue>, String> {
    match v {
        Value::Literal(l) if l.datatype == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.label)
                .map_err(|e| format!("array_contains: invalid array literal JSON: {}", e))?;
            match parsed {
                JsonValue::Array(a) => Ok(a),
                _ => Err("array_contains: array literal is not a JSON array".into()),
            }
        }
        _ => Err(format!("array_contains: expected array literal (datatype {})", ARRAY_DATATYPE)),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!("array_contains: expected 2 args (array, value), got {}", args.len()));
        }
        let arr = decode_array(&args[0])?;
        let needle = value_to_json(&args[1]);
        let found = arr.iter().any(|e| e == &needle);
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: typed_literal(found.to_string(), XSD_BOOLEAN),
            }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("array_contains: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("array_contains: aggregate not applicable".into())
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
                    "array_contains(array, value) -> xsd:boolean whether array contains value."),
            }]],
        }
    }
}

export!(Component);
