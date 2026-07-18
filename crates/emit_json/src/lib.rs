//! emit_json — aggregate rows into a JSON string.
//!
//! Dual to `parse_json`: turns binding-sets into a JSON document. The
//! caller passes name-value pairs as pairs of arguments:
//!
//!   (agg wf:call ?k1 ?v1 ?k2 ?v2 ...)
//!
//! Even-indexed arguments (0, 2, 4, …) are keys and must be string
//! literals. Odd-indexed arguments are values and may be any WIT term
//! variant — IRIs stringify to their IRI, literals to their label,
//! bnodes to their id.
//!
//! **Multiplicity note.** The old flat `aggregate-step` received a
//! per-row `mult: u64`; the base sparql-extension world folds that into
//! a single `step` call per row.

#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

use serde_json::{Map as JsonMap, Value as JsonValue};

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

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const AGGREGATE_NAME: &str = "emit_json";

struct Component;

/// Filter interface stub.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "emit_json: unknown filter function '{name}' (use via SPARQL aggregate)"
        ))
    }
}

/// Aggregate interface: one aggregate, `emit_json`.
impl AggregateGuest for Component {
    type AggregateState = JsonAccumulator;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        vec![AggregateDescriptor {
            name: AGGREGATE_NAME.to_string(),
            min_arity: 2,
            max_arity: None,
        }]
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        match name.as_str() {
            AGGREGATE_NAME => Ok(AggregateState::new(JsonAccumulator::new())),
            other => Err(format!("emit_json: unknown aggregate '{other}'")),
        }
    }
}

/// Property-function interface stub.
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
            "emit_json: unknown property function '{name}' (this component provides none)"
        ))
    }
}

pub struct JsonAccumulator {
    rows: RefCell<Vec<JsonValue>>,
}

impl JsonAccumulator {
    fn new() -> Self {
        Self {
            rows: RefCell::new(Vec::new()),
        }
    }
}

fn key_of(v: &WitTerm, index: usize) -> Result<String, String> {
    match v {
        WitTerm::Literal(lit) => Ok(lit.value.clone()),
        WitTerm::NamedNode(_) => Err(format!(
            "emit_json: key at argument index {index} must be a string literal, got IRI"
        )),
        WitTerm::BlankNode(_) => Err(format!(
            "emit_json: key at argument index {index} must be a string literal, got blank node"
        )),
        WitTerm::Triple(_) => Err(format!(
            "emit_json: key at argument index {index} must be a string literal, got quoted triple"
        )),
    }
}

fn value_as_json_string(v: &WitTerm) -> String {
    match v {
        WitTerm::NamedNode(s) => s.clone(),
        WitTerm::Literal(lit) => lit.value.clone(),
        WitTerm::BlankNode(s) => s.clone(),
        WitTerm::Triple(_) => String::from("<<quoted triple>>"),
    }
}

impl GuestAggregateState for JsonAccumulator {
    fn step(&self, args: Vec<WitTerm>) -> Result<(), String> {
        if args.len() % 2 != 0 {
            return Err(format!(
                "emit_json: expected an even number of arguments (key/value pairs), got {}",
                args.len()
            ));
        }
        let mut obj: JsonMap<String, JsonValue> = JsonMap::new();
        let mut i = 0;
        while i < args.len() {
            let key = key_of(&args[i], i)?;
            let value = value_as_json_string(&args[i + 1]);
            obj.insert(key, JsonValue::String(value));
            i += 2;
        }
        self.rows.borrow_mut().push(JsonValue::Object(obj));
        Ok(())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        let rows = std::mem::take(&mut *self.rows.borrow_mut());
        let json = serde_json::to_string(&JsonValue::Array(rows))
            .map_err(|e| format!("emit_json: serialisation failed: {e}"))?;
        Ok(WitTerm::Literal(WitLiteral {
            value: json,
            datatype: Some(XSD_STRING.to_string()),
            language: None,
        }))
    }
}

bindings::export!(Component with_types_in bindings);
