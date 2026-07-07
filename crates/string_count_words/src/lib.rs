//! string_count_words — number of words in a string literal.
//!
//! Ports the semantalytics function_string/count_words crate
//! (voca_rs::count::count_words).

wit_bindgen::generate!({ world: "webfunction", path: "wit" });

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}
fn integer_literal(v: i64) -> Value {
    Value::Literal(Literal { label: v.to_string(), datatype: XSD_INTEGER.into(), lang: None })
}
fn string_of(a: &Value) -> Result<&str, String> {
    match a {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("string_count_words: argument must be a string literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("string_count_words: expected 1 arg, got {}", args.len()));
        }
        let n = voca_rs::count::count_words(string_of(&args[0])?, "") as i64;
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding { name: "result".into(), value: integer_literal(n) }]],
        })
    }
    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("string_count_words: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("string_count_words: aggregate not applicable".into())
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
                    "string_count_words(s) -> number of words in s (xsd:integer)."),
            }]],
        }
    }
}

export!(Component);
