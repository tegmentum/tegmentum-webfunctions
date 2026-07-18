//! array_equals — deep equality on two array literals.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/equals. The
//! source in the semantalytics tree was a broken copy of `dedupe` (called a
//! non-existent `.dedupe()` iterator method, read only `value_1`, returned
//! an array datatype instead of a boolean). This crate implements the
//! obviously-intended behaviour: pairwise element equality of two arrays.

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

use serde_json::Value as JsonValue;
struct Component;

const XSD_STRING:      &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_BOOLEAN:     &str = "http://www.w3.org/2001/XMLSchema#boolean";
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

fn typed_literal(label: String, dt: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: label, datatype: Some(dt.into()), language: None })
}
fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

fn decode_array(v: &Value, side: &str) -> Result<Vec<JsonValue>, String> {
    match v {
        WitTerm::Literal(l) if l.datatype.as_deref().unwrap_or("http://www.w3.org/2001/XMLSchema#string") == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.value)
                .map_err(|e| format!("array_equals: {} arg invalid array literal JSON: {}", side, e))?;
            match parsed {
                JsonValue::Array(a) => Ok(a),
                _ => Err(format!("array_equals: {} arg array literal is not a JSON array", side)),
            }
        }
        _ => Err(format!("array_equals: {} arg not an array literal (datatype {})", side, ARRAY_DATATYPE)),
    }
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "array_equals".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "array_equals" => evaluate_impl(args),
            other => Err(format!("array_equals: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 2 {
            return Err(format!("array_equals: expected 2 args, got {}", args.len()));
        }
        let a = decode_array(&args[0], "first")?;
        let b = decode_array(&args[1], "second")?;
        let eq = a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y);
        Ok(typed_literal(eq.to_string(), XSD_BOOLEAN))
    }

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "array_equals: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("array_equals: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("array_equals: aggregate state was never constructed".into())
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
            "array_equals: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

