//! math_covariance — sample covariance.
//!
//! Ported from semantalytics/stardog-webfunctions/function_math/covariance to the
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
            .map_err(|e| format!("math_covariance: not a number: {}", e)),
        _ => Err("math_covariance: argument must be a numeric literal".into()),
    }
}

impl Guest for Component {
fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
    let n = args.len();
    if n < 4 || n % 2 != 0 {
        return Err(format!("math_covariance: need an even number of args >= 4 (first half is X, second half is Y), got {}", n));
    }
    let half = n / 2;
    let xs: Vec<f64> = args[..half].iter().map(number_of).collect::<Result<_, _>>()?;
    let ys: Vec<f64> = args[half..].iter().map(number_of).collect::<Result<_, _>>()?;
    let m = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / m;
    let my = ys.iter().sum::<f64>() / m;
    let cov = xs.iter().zip(ys.iter()).map(|(x, y)| (x - mx) * (y - my)).sum::<f64>() / (m - 1.0);
    Ok(BindingSets {
        vars: vec!["result".into()],
        rows: vec![vec![Binding { name: "result".into(), value: decimal_literal(cov) }]],
    })
}

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("math_covariance: aggregate not applicable".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("math_covariance: aggregate not applicable".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal("math_covariance(x1..xN, y1..yN) -> sample covariance (n-1 denominator). Args must be an even list; first half is X, second half is Y."),
            }]],
        }
    }
}

export!(Component);
