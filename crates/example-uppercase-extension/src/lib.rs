//! Reference SPARQL extension. Exports one filter function,
//! `upper`, that uppercases a string literal's lexical form. The
//! whole implementation is the smallest thing that can honestly
//! exercise `register` + `call` end-to-end.
//!
//! Arc 4 added `aggregate` and `property-function` interfaces to
//! the shared `sparql-extension` world. This component provides
//! neither — the empty-`register`-list stubs below satisfy the
//! Guest traits wit-bindgen generates for those interfaces without
//! adding any semantics. `new-aggregate` and `property-function
//! evaluate` are never called because the register lists are empty;
//! they return errors defensively.

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

struct Component;

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "upper".to_string(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "upper" => upper(&args),
            other => Err(format!("example-uppercase-extension: unknown function '{other}'")),
        }
    }
}

/// Aggregate interface stub. This component has no aggregates.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "example-uppercase-extension: unknown aggregate '{name}' (this component provides no aggregates)"
        ))
    }
}

/// A never-constructed state type — the resource exists only to
/// satisfy wit-bindgen's `type AggregateState`. `new-aggregate`
/// above always returns `Err`, so no instance is ever built.
pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("example-uppercase-extension: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("example-uppercase-extension: aggregate state was never constructed".into())
    }
}

/// Property-function interface stub. This component has no
/// property functions.
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
            "example-uppercase-extension: unknown property function '{name}' (this component provides none)"
        ))
    }
}

/// `upper(l)` — uppercase the lexical form of a string literal.
/// Rejects any argument that is not a literal (SPARQL semantics
/// for the caller are: raise an error the extension surface
/// reports as `None` in the host closure).
fn upper(args: &[WitTerm]) -> Result<WitTerm, String> {
    let [arg] = args else {
        return Err(format!(
            "upper: expected 1 argument, got {}",
            args.len()
        ));
    };
    let literal = match arg {
        WitTerm::Literal(l) => l,
        WitTerm::NamedNode(_) => return Err("upper: argument must be a literal, got IRI".into()),
        WitTerm::BlankNode(_) => {
            return Err("upper: argument must be a literal, got blank node".into())
        }
        // R2: types.term is the 4-arm superset; RDF-star quoted
        // triples are out of scope for the uppercase filter.
        WitTerm::Triple(_) => {
            return Err("upper: argument must be a literal, got quoted triple".into())
        }
    };
    Ok(WitTerm::Literal(WitLiteral {
        value: literal.value.to_uppercase(),
        // Preserve the input's datatype / language so a
        // language-tagged input stays language-tagged. The host
        // applies the RDF 1.1 defaulting rule when both are None.
        datatype: literal.datatype.clone(),
        language: literal.language.clone(),
    }))
}

bindings::export!(Component with_types_in bindings);
