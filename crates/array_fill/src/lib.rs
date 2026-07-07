//! array_fill — build an array of `size` copies of `value`.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/fill.
//! (The source had two typos — used `mappingDictionaryAdd` and forgot to
//! `use std::iter` — that never compiled. This port implements the
//! evidently-intended behaviour.)

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

fn size_of(v: &Value) -> Result<usize, String> {
    match v {
        Value::Literal(l) => l.label.parse::<usize>()
            .map_err(|e| format!("array_fill: size not a non-negative integer: {}", e)),
        _ => Err("array_fill: size must be a numeric literal".into()),
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
        if args.len() != 2 {
            return Err(format!("array_fill: expected 2 args (value, size), got {}", args.len()));
        }
        let element = value_to_json(&args[0]);
        let size = size_of(&args[1])?;
        let filled: Vec<JsonValue> = std::iter::repeat(element).take(size).collect();
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding { name: "result".into(), value: encode_array_json(filled) }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("array_fill: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("array_fill: aggregate not applicable".into())
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
                    "array_fill(value, size) -> array of `size` copies of `value`."),
            }]],
        }
    }
}

export!(Component);
