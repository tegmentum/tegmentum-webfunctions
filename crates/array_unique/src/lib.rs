//! array_unique — remove *all* duplicate elements from an array literal
//! while preserving first-occurrence order (matches the source crate's use
//! of `itertools::unique`).
//!
//! Ports semantalytics/stardog-webfunctions/function_array/unique.

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
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

fn decode_array(v: &Value) -> Result<Vec<JsonValue>, String> {
    match v {
        WitTerm::Literal(l) if l.datatype.as_deref().unwrap_or("http://www.w3.org/2001/XMLSchema#string") == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.value)
                .map_err(|e| format!("array_unique: invalid array literal JSON: {}", e))?;
            match parsed {
                JsonValue::Array(a) => Ok(a),
                _ => Err("array_unique: array literal is not a JSON array".into()),
            }
        }
        _ => Err(format!("array_unique: expected array literal (datatype {})", ARRAY_DATATYPE)),
    }
}

fn encode_array_json(items: Vec<JsonValue>) -> Value {
    WitTerm::Literal(WitLiteral { value: JsonValue::Array(items).to_string(), datatype: Some(ARRAY_DATATYPE.into()), language: None })
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "array_unique".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "array_unique" => evaluate_impl(args),
            other => Err(format!("array_unique: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 1 {
            return Err(format!("array_unique: expected 1 arg, got {}", args.len()));
        }
        let arr = decode_array(&args[0])?;
        let mut seen: Vec<JsonValue> = Vec::with_capacity(arr.len());
        for item in arr {
            if !seen.iter().any(|s| s == &item) {
                seen.push(item);
            }
        }
        Ok(encode_array_json(seen))
    }

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "array_unique: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("array_unique: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("array_unique: aggregate state was never constructed".into())
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
            "array_unique: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

