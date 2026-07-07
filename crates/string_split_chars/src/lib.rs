//! string_split_chars — split a string into its constituent characters.
//!
//! Ports the semantalytics function_string/split_chars crate. The original
//! stashed each character in Stardog's MappingDictionary and returned a
//! `tag:stardog:api:array` literal. Under the Component Model that
//! side-channel is gone — instead we return one row per character with the
//! `result` variable bound to a `xsd:string` literal for that character.

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
        _ => Err("string_split_chars: argument must be a string literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("string_split_chars: expected 1 arg, got {}", args.len()));
        }
        let rows: Vec<Vec<Binding>> = voca_rs::split::chars(string_of(&args[0])?)
            .into_iter()
            .map(|c| vec![Binding { name: "result".into(), value: string_literal(c) }])
            .collect();
        Ok(BindingSets { vars: vec!["result".into()], rows })
    }
    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("string_split_chars: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("string_split_chars: aggregate not applicable".into())
    }
    fn cardinality_estimate(_i: Cardinality, args: Vec<Value>) -> Result<Cardinality, String> {
        let n = match args.first() {
            Some(Value::Literal(l)) => voca_rs::split::chars(&l.label).len() as f64,
            _ => 1.0,
        };
        Ok(Cardinality { value: n, accuracy: Accuracy::Accurate })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "string_split_chars(s) -> one row per character in s (var: result, xsd:string)."),
            }]],
        }
    }
}

export!(Component);
