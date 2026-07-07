//! math_std_dev — sample standard deviation.
//!
//! Ported from semantalytics/stardog-webfunctions/function_math/std_dev to the
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
            .map_err(|e| format!("math_std_dev: not a number: {}", e)),
        _ => Err("math_std_dev: argument must be a numeric literal".into()),
    }
}

impl Guest for Component {
fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
    if args.len() < 2 {
        return Err("math_std_dev: need at least 2 samples".into());
    }
    let xs: Vec<f64> = args.iter().map(number_of).collect::<Result<_, _>>()?;
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| { let d = x - mean; d * d }).sum::<f64>() / (n - 1.0);
    let sd = var.sqrt();
    Ok(BindingSets {
        vars: vec!["result".into()],
        rows: vec![vec![Binding { name: "result".into(), value: decimal_literal(sd) }]],
    })
}

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("math_std_dev: aggregate not applicable".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("math_std_dev: aggregate not applicable".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal("math_std_dev(x1, x2, ...) -> sample standard deviation (Bessel's correction; n-1 denominator). Requires >= 2 args."),
            }]],
        }
    }
}

export!(Component);
