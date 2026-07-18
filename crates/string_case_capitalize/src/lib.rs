//! string_case_capitalize — uppercase the first character, optionally
//! lowercasing the rest.
//!
//! Wraps voca_rs::case::capitalize(subject, rest_to_lower). Accepts either
//! one arg (defaults rest_to_lower to true) or two args
//! (subject, rest_to_lower flag as a string parsed as bool, e.g. "true").

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

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}
fn string_of(a: &Value) -> Result<&str, String> {
    match a {
        WitTerm::Literal(l) => Ok(l.value.as_str()),
        _ => Err("string_case_capitalize: arguments must be literals".into()),
    }
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "string_case_capitalize".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "string_case_capitalize" => evaluate_impl(args),
            other => Err(format!("string_case_capitalize: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
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
        Ok(string_literal(&out))
    }

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "string_case_capitalize: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("string_case_capitalize: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("string_case_capitalize: aggregate state was never constructed".into())
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
            "string_case_capitalize: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

