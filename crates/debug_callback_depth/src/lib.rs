//! debug_callback_depth — minimal scratch component to isolate whether the
//! observability-callbacks ABI works at all.
//!
//! Just calls `observability_callbacks::callback_depth()` (no args, u32
//! return — simplest possible signature) and returns the result as an
//! xsd:integer literal.
//!
//! Migrated from the Stardog overlay `stardog:webfunction@0.3.0` world
//! (which put `callback-depth` on `interface host`) to the base
//! `tegmentum:webfunction/extension-with-all-host-callbacks@0.1.0` world
//! (where `callback-depth` lives on the split-out
//! `observability-callbacks` interface).

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
use bindings::tegmentum::webfunction::observability_callbacks as obs;
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

struct Component;

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

fn integer_literal(n: u32) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: n.to_string(),
        datatype: Some(XSD_INTEGER.into()),
        language: None,
    })
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "debug_callback_depth".into(),
            min_arity: 0,
            max_arity: Some(0),
        }]
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "debug_callback_depth" => Ok(integer_literal(obs::callback_depth())),
            other => Err(format!("debug_callback_depth: unknown function '{other}'")),
        }
    }
}

impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "debug_callback_depth: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("debug_callback_depth: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("debug_callback_depth: aggregate state was never constructed".into())
    }
}

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
            "debug_callback_depth: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
