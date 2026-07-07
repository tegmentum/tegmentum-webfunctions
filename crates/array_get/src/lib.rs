//! array_get — return the element at a 0-based index of an array literal.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/get. The source
//! looked the value up in Stardog's mapping dictionary; here the value is
//! carried inside the array literal itself.

wit_bindgen::generate!({ world: "webfunction", path: "wit" });

use serde_json::Value as JsonValue;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:      &str = "http://www.w3.org/2001/XMLSchema#string";
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn json_to_value(j: &JsonValue) -> Result<Value, String> {
    let obj = j.as_object().ok_or_else(|| "array_get: element is not an object".to_string())?;
    if let Some(iri) = obj.get("iri").and_then(|v| v.as_str()) {
        return Ok(Value::Iri(iri.to_string()));
    }
    if let Some(bnode) = obj.get("bnode").and_then(|v| v.as_str()) {
        return Ok(Value::Bnode(bnode.to_string()));
    }
    if let Some(lit) = obj.get("literal").and_then(|v| v.as_object()) {
        let label = lit.get("label").and_then(|v| v.as_str())
            .ok_or_else(|| "array_get: literal.label missing".to_string())?.to_string();
        let datatype = lit.get("datatype").and_then(|v| v.as_str())
            .ok_or_else(|| "array_get: literal.datatype missing".to_string())?.to_string();
        let lang = lit.get("lang").and_then(|v| v.as_str()).map(String::from);
        return Ok(Value::Literal(Literal { label, datatype, lang }));
    }
    Err("array_get: unknown element shape".into())
}

fn decode_array(v: &Value) -> Result<Vec<JsonValue>, String> {
    match v {
        Value::Literal(l) if l.datatype == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.label)
                .map_err(|e| format!("array_get: invalid array literal JSON: {}", e))?;
            match parsed {
                JsonValue::Array(a) => Ok(a),
                _ => Err("array_get: array literal is not a JSON array".into()),
            }
        }
        _ => Err(format!("array_get: expected array literal (datatype {})", ARRAY_DATATYPE)),
    }
}

fn index_of(v: &Value) -> Result<usize, String> {
    match v {
        Value::Literal(l) => l.label.parse::<usize>()
            .map_err(|e| format!("array_get: index not a non-negative integer: {}", e)),
        _ => Err("array_get: index must be a numeric literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!("array_get: expected 2 args (array, index), got {}", args.len()));
        }
        let arr = decode_array(&args[0])?;
        let idx = index_of(&args[1])?;
        let elem = arr.get(idx)
            .ok_or_else(|| format!("array_get: index {} out of bounds (len {})", idx, arr.len()))?;
        let value = json_to_value(elem)?;
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding { name: "result".into(), value }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("array_get: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("array_get: aggregate not applicable".into())
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
                    "array_get(array, index) -> element at 0-based index."),
            }]],
        }
    }
}

export!(Component);
