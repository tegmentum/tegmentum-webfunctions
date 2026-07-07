//! math_sqrt — square root of a numeric literal.
//!
//! Ported from the module-mode ABI (semantalytics/stardog-webfunctions/
//! function_math/sqrt) to the Component Model ABI.

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

fn number_of(arg: &Value) -> Result<f64, String> {
    match arg {
        Value::Literal(literal) => literal.label.parse::<f64>()
            .map_err(|e| format!("math_sqrt: not a number: {}", e)),
        _ => Err("math_sqrt: argument must be a numeric literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("math_sqrt: expected 1 arg, got {}", args.len()));
        }
        let x = number_of(&args[0])?;
        if x < 0.0 {
            return Err(format!("math_sqrt: negative input {}", x));
        }
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: decimal_literal(x.sqrt()),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("math_sqrt: aggregate-step not implemented".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("math_sqrt: aggregate-finish not implemented".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal("math_sqrt(x) -> sqrt(x). Rejects negative inputs."),
            }]],
        }
    }
}

export!(Component);
