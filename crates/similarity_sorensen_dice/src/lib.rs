//! similarity_sorensen_dice — Sørensen-Dice coefficient between two strings.
//!
//! Ports the semantalytics function_string_similarity/sorensen_dice crate.
//! Uses the strsim crate.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn decimal_literal(v: f64) -> Value {
    Value::Literal(Literal { label: v.to_string(), datatype: XSD_DECIMAL.into(), lang: None })
}

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("similarity_sorensen_dice: arguments must be string literals".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!("similarity_sorensen_dice: expected 2 args, got {}", args.len()));
        }
        let a = string_of(&args[0])?;
        let b = string_of(&args[1])?;
        let s = strsim::sorensen_dice(a, b);
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: decimal_literal(s),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("similarity_sorensen_dice: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("similarity_sorensen_dice: aggregate not applicable".into())
    }
    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "similarity_sorensen_dice(a, b) -> Sørensen-Dice similarity coefficient between a and b in [0.0, 1.0] (via strsim crate)."),
            }]],
        }
    }
}

export!(Component);
