//! math_pearson_r — Pearson correlation coefficient.
//!
//! Ported from semantalytics/stardog-webfunctions/function_math/pearson_r to the
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
            .map_err(|e| format!("math_pearson_r: not a number: {}", e)),
        _ => Err("math_pearson_r: argument must be a numeric literal".into()),
    }
}

impl Guest for Component {
fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
    let n = args.len();
    if n < 4 || n % 2 != 0 {
        return Err(format!("math_pearson_r: need an even number of args >= 4 (first half is X, second half is Y), got {}", n));
    }
    let half = n / 2;
    let xs: Vec<f64> = args[..half].iter().map(number_of).collect::<Result<_, _>>()?;
    let ys: Vec<f64> = args[half..].iter().map(number_of).collect::<Result<_, _>>()?;
    let m = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / m;
    let my = ys.iter().sum::<f64>() / m;
    let mut sxy = 0.0f64;
    let mut sxx = 0.0f64;
    let mut syy = 0.0f64;
    for (x, y) in xs.iter().zip(ys.iter()) {
        let dx = x - mx;
        let dy = y - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    let denom = (sxx * syy).sqrt();
    if denom == 0.0 {
        return Err("math_pearson_r: zero variance in X or Y".into());
    }
    let r = sxy / denom;
    Ok(BindingSets {
        vars: vec!["result".into()],
        rows: vec![vec![Binding { name: "result".into(), value: decimal_literal(r) }]],
    })
}

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("math_pearson_r: aggregate not applicable".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("math_pearson_r: aggregate not applicable".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal("math_pearson_r(x1..xN, y1..yN) -> Pearson r. Args must be an even list; first half is X, second half is Y."),
            }]],
        }
    }
}

export!(Component);
