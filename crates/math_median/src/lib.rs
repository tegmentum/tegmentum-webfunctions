//! math_median — median value.
//!
//! Ported from semantalytics/stardog-webfunctions/function_math/median to the
//! Component Model ABI. The upstream source was a scaffolding template with no
//! working body; the algorithm here is implemented from scratch based on the
//! function name.

#[allow(warnings)]
mod bindings;

use bindings::exports::tegmentum::webfunction::aggregate::{
    AggregateDescriptor, AggregateState, Guest as AggregateGuest, GuestAggregateState,
};
use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::exports::tegmentum::webfunction::property_function::{
    BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

/// Legacy names — kept as type aliases so the ported business logic
/// below reads with minimum diff against the flat-world original. The
/// `Term::Triple` arm added by the R2 types consolidation is handled
/// in each `match` inside this file.
type Value = WitTerm;
type Literal = WitLiteral;

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
#[allow(dead_code)]
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
#[allow(dead_code)]
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

#[allow(dead_code)]
fn decimal_literal(v: f64) -> Value {
    WitTerm::Literal(WitLiteral { value: v.to_string(), datatype: Some(XSD_DECIMAL.into()), language: None })
}

#[allow(dead_code)]
fn integer_literal(v: i64) -> Value {
    WitTerm::Literal(WitLiteral { value: v.to_string(), datatype: Some(XSD_INTEGER.into()), language: None })
}

fn number_of(arg: &Value) -> Result<f64, String> {
    match arg {
        WitTerm::Literal(literal) => literal.value.parse::<f64>()
            .map_err(|e| format!("math_median: not a number: {}", e)),
        _ => Err("math_median: argument must be a numeric literal".into()),
    }
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "math_median".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "math_median" => evaluate_impl(args),
            other => Err(format!("math_median: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
    if args.is_empty() {
        return Err("math_median: expected at least 1 arg".into());
    }
    let mut xs: Vec<f64> = args.iter().map(number_of).collect::<Result<_, _>>()?;
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    let m = if n % 2 == 1 { xs[n / 2] } else { 0.5 * (xs[n / 2 - 1] + xs[n / 2]) };
    Ok(decimal_literal(m))
}

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "math_median: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("math_median: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("math_median: aggregate state was never constructed".into())
    }
}

/// Property-function interface stub — this component provides none.
impl PropertyFunctionGuest for Component {
    fn register_property_functions() -> Vec<PropertyDescriptor> {
        Vec::new()
    }

    fn evaluate(
        name: String,
        _subjects: Vec<WitTerm>,
        _objects: Vec<WitTerm>,
    ) -> Result<Vec<BindingRow>, String> {
        Err(format!(
            "math_median: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

