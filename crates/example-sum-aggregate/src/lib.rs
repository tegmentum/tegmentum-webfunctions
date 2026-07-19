//! Reference SPARQL aggregate extension. Exports one aggregate,
//! `sum`, that adds up i64-shaped literals over a SPARQL group and
//! returns the running total as an xsd:integer literal.
//!
//! Complements `example-count-aggregate` (which exercises the
//! set-semantics / distinct-count shape) by exercising the
//! numeric-reduction shape: one accumulator per group, one step
//! per row that mutates the accumulator, one finish per group.
//!
//! Migrated from `stardog-webfunction-plugin/src/test/rust/aggregate/
//! sum_component` — same semantics, same 1-arg-per-step surface,
//! same xsd:integer return — so the four JVM/Rust engine bindings
//! (stardog, jena, rdf4j, oxigraph) all load a single sum-aggregate
//! wasm fixture from the shared webfunctions target.
//!
//! Filter and property-function interfaces stub out defensively:
//! their register lists are empty; the associated dispatch
//! functions error rather than reach unreachable states.

#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

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

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

/// Declared name for this aggregate. The host binds it inside its
/// aggregate registry and passes it into `new-aggregate`.
const AGGREGATE_NAME: &str = "sum";

struct Component;

/// Filter interface stub — this component provides no filter functions.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "example-sum-aggregate: unknown filter function '{name}' (this component provides none)"
        ))
    }
}

/// Aggregate interface: one aggregate, `sum`.
impl AggregateGuest for Component {
    type AggregateState = SumAccumulator;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        vec![AggregateDescriptor {
            name: AGGREGATE_NAME.to_string(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        match name.as_str() {
            AGGREGATE_NAME => Ok(AggregateState::new(SumAccumulator::new())),
            other => Err(format!("example-sum-aggregate: unknown aggregate '{other}'")),
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
            "example-sum-aggregate: unknown property function '{name}' (this component provides none)"
        ))
    }
}

/// Per-group accumulator: a running i64 sum. Interior mutability
/// via `RefCell` because wit-bindgen generates `&self` (not
/// `&mut self`) method signatures for resource methods — the guest
/// owns the mutation discipline.
pub struct SumAccumulator {
    total: RefCell<i64>,
}

impl SumAccumulator {
    fn new() -> Self {
        Self {
            total: RefCell::new(0),
        }
    }
}

impl GuestAggregateState for SumAccumulator {
    fn step(&self, args: Vec<WitTerm>) -> Result<(), String> {
        let [arg] = args.as_slice() else {
            return Err(format!(
                "sum: expected 1 argument per step, got {}",
                args.len()
            ));
        };
        let n = match arg {
            WitTerm::Literal(l) => l
                .value
                .parse::<i64>()
                .map_err(|e| format!("sum: value not parseable as integer: {e}"))?,
            WitTerm::NamedNode(_) => {
                return Err("sum: argument must be a literal, got IRI".into())
            }
            WitTerm::BlankNode(_) => {
                return Err("sum: argument must be a literal, got blank node".into())
            }
            WitTerm::Triple(_) => {
                return Err("sum: argument must be a literal, got quoted triple".into())
            }
        };
        *self.total.borrow_mut() += n;
        Ok(())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        let total = *self.total.borrow();
        Ok(WitTerm::Literal(WitLiteral {
            value: total.to_string(),
            datatype: Some(XSD_INTEGER.to_string()),
            language: None,
        }))
    }
}

bindings::export!(Component with_types_in bindings);
