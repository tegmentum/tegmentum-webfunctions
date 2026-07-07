//! math_mean — arithmetic mean.
//!
//! Ported from semantalytics/stardog-webfunctions/function_math/mean to the
//! Component Model ABI. The upstream source was a scaffolding template with no
//! working body; the algorithm here is implemented from scratch based on the
//! function name.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
#[allow(dead_code)]
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
#[allow(dead_code)]
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

#[allow(dead_code)]
fn decimal_literal(v: f64) -> Value {
    Value::Literal(Literal { label: v.to_string(), datatype: XSD_DECIMAL.into(), lang: None })
}

#[allow(dead_code)]
fn integer_literal(v: i64) -> Value {
    Value::Literal(Literal { label: v.to_string(), datatype: XSD_INTEGER.into(), lang: None })
}

fn number_of(arg: &Value) -> Result<f64, String> {
    match arg {
        Value::Literal(literal) => literal.label.parse::<f64>()
            .map_err(|e| format!("math_mean: not a number: {}", e)),
        _ => Err("math_mean: argument must be a numeric literal".into()),
    }
}

impl Guest for Component {
fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
    if args.is_empty() {
        return Err("math_mean: expected at least 1 arg".into());
    }
    let mut sum: f64 = 0.0;
    for arg in &args { sum += number_of(arg)?; }
    let m = sum / (args.len() as f64);
    Ok(BindingSets {
        vars: vec!["result".into()],
        rows: vec![vec![Binding { name: "result".into(), value: decimal_literal(m) }]],
    })
}

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("math_mean: aggregate not applicable".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("math_mean: aggregate not applicable".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal("math_mean(x1, x2, ...) -> arithmetic mean."),
            }]],
        }
    }
}

export!(Component);
