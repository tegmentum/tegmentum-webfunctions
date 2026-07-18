//! debug_execute_update — scratch component proving the graph-callbacks
//! `execute-update` + `execute-query` pair round-trips inside a single
//! wf:call frame.
//!
//! Migrated (Follow-up E) from the Stardog overlay
//! `stardog:webfunction@0.3.1` world to the base
//! `tegmentum:webfunction/extension-with-host-callbacks@0.1.0` world.
//!
//! Now exports a single filter function `debug_execute_update(s, p, o)`
//! via `extension::call`. The three arguments are the subject IRI,
//! predicate IRI, and object (IRI or literal). The function issues an
//! `INSERT DATA` through `graph-callbacks::execute-update`, then a
//! follow-up `SELECT` through `graph-callbacks::execute-query`, and
//! returns an xsd:boolean literal indicating whether the INSERT was
//! visible to the follow-up read in the same transaction.

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
use bindings::tegmentum::webfunction::graph_callbacks::{
    self as gc, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

struct Component;

const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

fn iri_of(v: &WitTerm) -> Result<String, String> {
    match v {
        WitTerm::NamedNode(s) => Ok(s.clone()),
        _ => Err("debug_execute_update: expected IRI argument".into()),
    }
}

/// Serialize an object term as its SPARQL surface form for use in the
/// INSERT DATA text.
fn as_object_literal(v: &WitTerm) -> Result<String, String> {
    match v {
        WitTerm::Literal(l) => {
            let escaped = l
                .value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            if let Some(lang) = &l.language {
                Ok(format!("\"{escaped}\"@{lang}"))
            } else if let Some(dt) = &l.datatype {
                Ok(format!("\"{escaped}\"^^<{dt}>"))
            } else {
                Ok(format!("\"{escaped}\""))
            }
        }
        WitTerm::NamedNode(s) => Ok(format!("<{s}>")),
        WitTerm::BlankNode(label) => Ok(format!("_:{label}")),
        WitTerm::Triple(_) => {
            Err("debug_execute_update: quoted-triple objects are not supported".into())
        }
    }
}

fn execute_update(sparql: &str) -> Result<(), String> {
    gc::execute_update(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

fn execute_query(sparql: &str) -> Result<CallbackQueryResult, String> {
    gc::execute_query(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

fn boolean_literal(b: bool) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: b.to_string(),
        datatype: Some(XSD_BOOLEAN.into()),
        language: None,
    })
}

fn debug_execute_update_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    if args.len() != 3 {
        return Err(format!(
            "debug_execute_update: expected 3 args (s, p, o), got {}",
            args.len()
        ));
    }
    let s = iri_of(&args[0])?;
    let p = iri_of(&args[1])?;
    let o = as_object_literal(&args[2])?;

    let insert = format!("INSERT DATA {{ <{s}> <{p}> {o} }}");
    execute_update(&insert)?;

    let select = format!("SELECT ?o WHERE {{ <{s}> <{p}> ?o }}");
    let confirmed = match execute_query(&select)? {
        CallbackQueryResult::Bindings(bs) => !bs.is_empty(),
        CallbackQueryResult::Quads(qs) => !qs.is_empty(),
        CallbackQueryResult::Boolean(b) => b,
    };
    Ok(boolean_literal(confirmed))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "debug_execute_update".into(),
            min_arity: 3,
            max_arity: Some(3),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "debug_execute_update" => debug_execute_update_impl(&args),
            other => Err(format!("debug_execute_update: unknown function '{other}'")),
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
            "debug_execute_update: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("debug_execute_update: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("debug_execute_update: aggregate state was never constructed".into())
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
            "debug_execute_update: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
