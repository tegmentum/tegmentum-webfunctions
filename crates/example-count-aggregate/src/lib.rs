//! Reference SPARQL aggregate extension. Exports one aggregate,
//! `count-distinct-strings`, that counts the distinct string
//! lexical forms accumulated per SPARQL group.
//!
//! The aggregate is deliberately simple — set semantics, one
//! argument per `step` — because the point is to prove the
//! lifecycle (`new-aggregate` -> N * `step` -> `finish`) through
//! the WIT resource, not to demo interesting statistics.
//!
//! This component also exports the `extension` (filter) and
//! `property-function` interfaces required by the shared
//! `sparql-extension` world. Both return empty descriptor lists;
//! the associated dispatch functions error defensively.

#[allow(warnings)]
mod bindings;

use std::cell::RefCell;
use std::collections::BTreeSet;

use bindings::exports::tegmentum::webfunction::aggregate::{
    AggregateDescriptor, AggregateState, Guest as AggregateGuest,
    GuestAggregateState,
};
use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::exports::tegmentum::webfunction::property_function::{
    BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

/// xsd:integer — the datatype `finish` stamps on the returned count.
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

/// The declared name for this aggregate. The host binds it under
/// `<urn:webfunction:count-distinct-strings>` in the SPARQL
/// evaluator's custom-aggregate registry.
const AGGREGATE_NAME: &str = "count-distinct-strings";

struct Component;

/// Filter interface: this component provides none.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "example-count-aggregate: unknown filter function '{name}' (this component provides none)"
        ))
    }
}

/// Aggregate interface: one aggregate, `count-distinct-strings`.
impl AggregateGuest for Component {
    type AggregateState = DistinctStringSet;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        vec![AggregateDescriptor {
            name: AGGREGATE_NAME.to_string(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        match name.as_str() {
            AGGREGATE_NAME => Ok(AggregateState::new(DistinctStringSet::new())),
            other => Err(format!(
                "example-count-aggregate: unknown aggregate '{other}'"
            )),
        }
    }
}

/// Property-function interface: this component provides none.
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
            "example-count-aggregate: unknown property function '{name}' (this component provides none)"
        ))
    }
}

/// Per-group accumulator: a set of distinct literal lexical forms.
/// `BTreeSet` for a deterministic iteration order — helpful when
/// debugging, cheap for the aggregate result which only looks at
/// the size.
///
/// Interior mutability via `RefCell` because the WIT resource
/// method signatures wit-bindgen generates are `&self` (not
/// `&mut self`) — the guest owns the mutation discipline.
pub struct DistinctStringSet {
    values: RefCell<BTreeSet<String>>,
}

impl DistinctStringSet {
    fn new() -> Self {
        Self {
            values: RefCell::new(BTreeSet::new()),
        }
    }
}

impl GuestAggregateState for DistinctStringSet {
    /// Accumulate one row's value. Non-literal arguments are a hard
    /// error — the aggregate is `count-distinct-STRINGS`, not
    /// `count-distinct-anything`. Datatype and language are ignored
    /// (a plain literal `"a"` and an xsd:string `"a"` collapse to
    /// one), matching how many SPARQL DISTINCT flavors treat the
    /// simple-literal / xsd:string equivalence.
    fn step(&self, args: Vec<WitTerm>) -> Result<(), String> {
        let [arg] = args.as_slice() else {
            return Err(format!(
                "count-distinct-strings: expected 1 argument per step, got {}",
                args.len()
            ));
        };
        match arg {
            WitTerm::Literal(l) => {
                self.values.borrow_mut().insert(l.value.clone());
                Ok(())
            }
            WitTerm::NamedNode(_) => {
                Err("count-distinct-strings: argument must be a literal, got IRI".into())
            }
            WitTerm::BlankNode(_) => {
                Err("count-distinct-strings: argument must be a literal, got blank node".into())
            }
        }
    }

    /// Produce the final count as an xsd:integer literal.
    fn finish(&self) -> Result<WitTerm, String> {
        let count = self.values.borrow().len();
        Ok(WitTerm::Literal(WitLiteral {
            value: count.to_string(),
            datatype: Some(XSD_INTEGER.to_string()),
            language: None,
        }))
    }
}

bindings::export!(Component with_types_in bindings);
