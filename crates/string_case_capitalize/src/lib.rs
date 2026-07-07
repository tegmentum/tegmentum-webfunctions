//! string_case_capitalize — uppercase the first character, optionally
//! lowercasing the rest.
//!
//! Wraps voca_rs::case::capitalize(subject, rest_to_lower). Accepts either
//! one arg (defaults rest_to_lower to true) or two args
//! (subject, rest_to_lower flag as a string parsed as bool, e.g. "true").

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
        _ => Err("string_case_capitalize: arguments must be literals".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let (subject, rest_to_lower) = match args.len() {
            1 => (string_of(&args[0])?, true),
            2 => {
                let s = string_of(&args[0])?;
                let flag = string_of(&args[1])?
                    .parse::<bool>()
                    .map_err(|e| format!("string_case_capitalize: bad bool arg: {}", e))?;
                (s, flag)
            }
            n => return Err(format!("string_case_capitalize: expected 1 or 2 args, got {}", n)),
        };
        let out = voca_rs::case::capitalize(subject, rest_to_lower);
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding { name: "result".into(), value: string_literal(&out) }]],
        })
    }
    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("string_case_capitalize: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("string_case_capitalize: aggregate not applicable".into())
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
                    "string_case_capitalize(s [, rest_to_lower]) -> uppercase first char, optionally lowercase the rest (default true)."),
            }]],
        }
    }
}

export!(Component);
