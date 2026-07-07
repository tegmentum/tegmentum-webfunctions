//! string_case_camel_case — convert to camelCase.
//!
//! Wraps voca_rs::case::camel_case.

wit_bindgen::generate!({ world: "webfunction", path: "wit" });

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}
fn string_of(a: &Value) -> Result<&str, String> {
    match a {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("string_case_camel_case: argument must be a string literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("string_case_camel_case: expected 1 arg, got {}", args.len()));
        }
        let out = voca_rs::case::camel_case(string_of(&args[0])?);
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding { name: "result".into(), value: string_literal(&out) }]],
        })
    }
    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("string_case_camel_case: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("string_case_camel_case: aggregate not applicable".into())
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
                    "string_case_camel_case(s) -> convert to camelCase."),
            }]],
        }
    }
}

export!(Component);
