//! array_of — pack N values into an array literal.
//!
//! Variadic input: `array_of(v1, v2, ..., vN)` returns a single "array
//! literal" — a Value::Literal with datatype `tag:stardog:api:array` whose
//! label is the JSON-encoded list of the input Values. Composes with the
//! other array_* crates.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/of.

wit_bindgen::generate!({ world: "webfunction", path: "wit" });

use serde_json::{Map as JsonMap, Value as JsonValue};
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:      &str = "http://www.w3.org/2001/XMLSchema#string";
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

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

fn encode_array(items: &[Value]) -> Value {
    let arr: Vec<JsonValue> = items.iter().map(value_to_json).collect();
    Value::Literal(Literal {
        label: JsonValue::Array(arr).to_string(),
        datatype: ARRAY_DATATYPE.into(),
        lang: None,
    })
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding { name: "result".into(), value: encode_array(&args) }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("array_of: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("array_of: aggregate not applicable".into())
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
                    "array_of(v1, ..., vN) -> array literal (datatype tag:stardog:api:array). \
                     Composes with array_size, array_get, array_contains, etc."),
            }]],
        }
    }
}

export!(Component);
