//! math_const_frac_pi_2 — mathematical constant pi/2.
//!
//! Ported from the module-mode ABI (semantalytics/stardog-webfunctions/
//! function_math_constants/frac_pi_2) to the Component Model ABI.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::f64::consts;

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn decimal_literal(v: f64) -> Value {
    Value::Literal(Literal { label: v.to_string(), datatype: XSD_DECIMAL.into(), lang: None })
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if !args.is_empty() {
            return Err(format!("math_const_frac_pi_2: expected 0 args, got {}", args.len()));
        }
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: decimal_literal(consts::FRAC_PI_2),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("math_const_frac_pi_2: aggregate-step not implemented".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("math_const_frac_pi_2: aggregate-finish not implemented".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal("math_const_frac_pi_2() -> pi/2."),
            }]],
        }
    }
}

export!(Component);
