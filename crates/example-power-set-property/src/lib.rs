//! Reference SPARQL property-function extension. Exports one
//! property function, `power-set`, that emits one binding row per
//! subset of a single input character-string literal. Overkill
//! semantics on purpose — the point is exercising the multi-binding
//! output shape and the subject-arity=1, object-arity=1 descriptor
//! layout, not doing something clever.
//!
//! SPARQL surface (once a host wires custom property-function
//! dispatch — oxigraph 0.5 does not, see `docs/architecture.md`):
//!
//! ```sparql
//! ?subset <urn:webfunction:power-set> "abc" .
//! ```
//!
//! Semantics: for an input literal whose lexical form is a string
//! of `n` characters, emit `2^n` rows. Each row's `values` is
//! `[<subset-literal>, <input-literal>]` — subject-arity=1
//! (`?subset`), object-arity=1 (echo of the input in the object
//! position). Larger inputs blow the row count fast: `"abcd"`
//! -> 16 rows, `"abcdef"` -> 64. A caller who asks for the power
//! set of a 20-character string deserves what they get.
//!
//! This component also exports the `extension` and `aggregate`
//! interfaces required by the shared `sparql-extension` world;
//! both return empty descriptor lists.

#[allow(warnings)]
mod bindings;

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

/// The declared name for this property function.
const PROPERTY_NAME: &str = "power-set";

/// Filter interface: none.
struct Component;

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "example-power-set-property: unknown filter function '{name}' (this component provides none)"
        ))
    }
}

/// Aggregate interface: none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "example-power-set-property: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("example-power-set-property: aggregate state never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("example-power-set-property: aggregate state never constructed".into())
    }
}

/// Property-function interface: one function, `power-set`.
impl PropertyFunctionGuest for Component {
    fn register_property_functions() -> Vec<PropertyDescriptor> {
        vec![PropertyDescriptor {
            name: PROPERTY_NAME.to_string(),
            subject_arity: 1,
            object_arity: 1,
        }]
    }

    fn evaluate(
        name: String,
        subjects: Vec<WitTerm>,
        objects: Vec<WitTerm>,
    ) -> Result<Vec<BindingRow>, String> {
        if name != PROPERTY_NAME {
            return Err(format!(
                "example-power-set-property: unknown property function '{name}'"
            ));
        }
        // subject-arity=1 with the subject position typically unbound
        // in queries (`?subset :power-set "abc"`), so `subjects` is
        // empty on the wire. If a caller pre-binds the subject, the
        // extension has no filtering to apply (every subset is
        // emitted); this is fine for the reference example.
        if !subjects.is_empty() && subjects.len() != 1 {
            return Err(format!(
                "power-set: subject-arity is 1; got {} bound subject term(s)",
                subjects.len()
            ));
        }
        // object-arity=1 with the object bound to the input.
        let [input] = objects.as_slice() else {
            return Err(format!(
                "power-set: expected exactly 1 bound object term (the input string), got {}",
                objects.len()
            ));
        };
        let literal = match input {
            WitTerm::Literal(l) => l,
            WitTerm::NamedNode(_) => {
                return Err("power-set: input must be a string literal, got IRI".into())
            }
            WitTerm::BlankNode(_) => {
                return Err("power-set: input must be a string literal, got blank node".into())
            }
            // R2: types.term is the 4-arm superset; RDF-star quoted
            // triples are out of scope for power-set.
            WitTerm::Triple(_) => {
                return Err(
                    "power-set: input must be a string literal, got quoted triple".into(),
                )
            }
        };

        let chars: Vec<char> = literal.value.chars().collect();
        let n = chars.len();
        if n > 20 {
            return Err(format!(
                "power-set: refusing to enumerate 2^{n} subsets; cap is 20 characters"
            ));
        }
        let mut rows = Vec::with_capacity(1usize << n);
        let input_term = WitTerm::Literal(WitLiteral {
            value: literal.value.clone(),
            datatype: literal.datatype.clone(),
            language: literal.language.clone(),
        });
        for mask in 0..(1u64 << n) {
            let subset: String = (0..n)
                .filter(|i| (mask >> i) & 1 == 1)
                .map(|i| chars[i])
                .collect();
            let subset_term = WitTerm::Literal(WitLiteral {
                value: subset,
                // Result literals are simple string literals — no
                // datatype echo. `None` yields xsd:string per the
                // RDF 1.1 defaulting rule the host applies.
                datatype: None,
                language: None,
            });
            rows.push(BindingRow {
                // Positional convention documented in the WIT: subject
                // terms first, then object terms.
                values: vec![subset_term, input_term.clone()],
            });
        }
        Ok(rows)
    }
}

bindings::export!(Component with_types_in bindings);
