//! agg_sum — integer sum aggregator.
//!
//! Migrated from the flat stardog:webfunction@0.2.0 exports
//! (`aggregate-step` + `aggregate-finish` as free functions with a
//! shared thread-local accumulator) to the base sparql-extension world's
//! `aggregate.new-aggregate` factory + `aggregate-state` resource.
//! Per-group state now lives inside `SumAccumulator` behind a `RefCell`;
//! the host constructs a fresh accumulator per SPARQL GROUP, steps it
//! once per row, and drops the resource after `finish`.
//!
//! **Semantic note on multiplicity.** The old flat `aggregate-step`
//! took a `mult: u64` alongside the args; the base world's resource
//! `step(args)` does not. Stardog historically passed `mult = 1` in
//! practice, so this migration folds the arithmetic to a single
//! `step`-per-row without a repeat loop. Callers that relied on
//! non-unit multiplicity semantics need a per-row repeat at the host
//! side; see docs/bridging-dispatch.md.

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
const AGGREGATE_NAME: &str = "agg_sum";

struct Component;

/// Filter interface stub — this component provides no filter functions.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "agg_sum: unknown filter function '{name}' (use via SPARQL aggregate)"
        ))
    }
}

/// Aggregate interface: one aggregate, `agg_sum`.
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
            other => Err(format!("agg_sum: unknown aggregate '{other}'")),
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
            "agg_sum: unknown property function '{name}' (this component provides none)"
        ))
    }
}

/// Per-group i64 running sum. Interior mutability via `RefCell` because
/// wit-bindgen generates `&self` (not `&mut self`) for resource methods —
/// the guest owns the mutation discipline.
pub struct SumAccumulator {
    total: RefCell<i64>,
}

impl SumAccumulator {
    fn new() -> Self {
        Self { total: RefCell::new(0) }
    }
}

fn integer_of(v: &WitTerm) -> Result<i64, String> {
    match v {
        WitTerm::Literal(lit) => lit
            .value
            .parse::<i64>()
            .map_err(|e| format!("agg_sum: value not parseable as integer: {e}")),
        WitTerm::NamedNode(_) => Err("agg_sum: argument must be a literal, got IRI".into()),
        WitTerm::BlankNode(_) => Err("agg_sum: argument must be a literal, got blank node".into()),
        WitTerm::Triple(_) => Err("agg_sum: argument must be a literal, got quoted triple".into()),
    }
}

impl GuestAggregateState for SumAccumulator {
    fn step(&self, args: Vec<WitTerm>) -> Result<(), String> {
        let n = match args.first() {
            Some(v) => integer_of(v)?,
            None => return Err("agg_sum: expected at least one argument".into()),
        };
        *self.total.borrow_mut() = self.total.borrow().saturating_add(n);
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
