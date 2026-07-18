//! wf_map — map-over-rows higher-order combinator.
//!
//! Signature: `wf:map(<wasm-url>, "SELECT ?x WHERE {...}")`.
//!
//! Runs the second-argument SELECT against the local store via
//! `graph-callbacks::execute-query`, invokes the first-argument wasm
//! on each row's first cell via `wasm-callbacks::invoke-wasm-service`,
//! and packs the resulting scalar values into an rdf:JSON array —
//! one output element per input row, in the query's solution order.
//! Semantics match Rust's `Iterator::map`.
//!
//! Migration (Follow-up F): moved off the Stardog overlay onto the
//! substrate `tegmentum:webfunction/extension-with-all-host-callbacks
//! @0.1.0` world. `execute-query` is on `graph-callbacks`;
//! `invoke-wasm-service` is on `wasm-callbacks` and returns a flat
//! `list<binding>`. This crate takes the first row's first binding as
//! the mapper's output for a given input row.

#[allow(warnings)]
mod bindings;

use serde_json::{Value as JsonValue, json};

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
use bindings::tegmentum::webfunction::types::{
    Binding as WitBinding, Literal as WitLiteral, Term as WitTerm,
};
use bindings::tegmentum::webfunction::wasm_callbacks::{
    self as wc, WasmCallError,
};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.into(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    })
}

fn map_graph_err(e: gc::GraphCallError) -> String {
    match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    }
}

fn map_wasm_err(e: WasmCallError) -> String {
    match e {
        WasmCallError::NotFound(m) => format!("wasm-callbacks not-found: {m}"),
        WasmCallError::InvocationError(m) => format!("wasm-callbacks invocation-error: {m}"),
        WasmCallError::NotPermitted(m) => format!("wasm-callbacks not-permitted: {m}"),
    }
}

fn term_to_json_scalar(v: &WitTerm) -> JsonValue {
    match v {
        WitTerm::NamedNode(uri) => json!(uri),
        WitTerm::BlankNode(id) => json!(format!("_:{id}")),
        WitTerm::Literal(l) => {
            let dt = l.datatype.as_deref().unwrap_or("");
            if dt.ends_with("integer") || dt.ends_with("long") || dt.ends_with("int") {
                if let Ok(n) = l.value.parse::<i64>() {
                    return json!(n);
                }
            }
            if dt.ends_with("decimal") || dt.ends_with("double") || dt.ends_with("float") {
                if let Ok(n) = l.value.parse::<f64>() {
                    if n.is_finite() {
                        return json!(n);
                    }
                }
            }
            if dt.ends_with("boolean") {
                match l.value.as_str() {
                    "true" | "1" => return json!(true),
                    "false" | "0" => return json!(false),
                    _ => {}
                }
            }
            json!(l.value)
        }
        WitTerm::Triple(_) => json!("<<quoted-triple>>"),
    }
}

/// Split a flat binding list on repeated variable identity and return
/// the first (row_0, binding_0.value). Returns Null when the sub-wasm
/// produced no output — a valid stand-in for a missing SPARQL binding.
fn first_scalar_of(flat: Vec<WitBinding>) -> JsonValue {
    let mut current: Vec<WitBinding> = Vec::new();
    for b in flat {
        if current.iter().any(|prev| prev.variable == b.variable) {
            break;
        }
        current.push(b);
    }
    current
        .first()
        .map(|b| term_to_json_scalar(&b.value))
        .unwrap_or(JsonValue::Null)
}

fn map_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    if args.len() != 2 {
        return Err(format!(
            "wf_map: expected 2 args (wasm URL, inner SPARQL), got {}",
            args.len()
        ));
    }
    let url = match &args[0] {
        WitTerm::NamedNode(s) => s.clone(),
        WitTerm::Literal(l) => l.value.clone(),
        other => {
            return Err(format!(
                "wf_map: first arg must be an IRI or string, got {other:?}"
            ));
        }
    };
    let inner_sparql = match &args[1] {
        WitTerm::Literal(l) => l.value.clone(),
        WitTerm::NamedNode(s) => s.clone(),
        other => {
            return Err(format!(
                "wf_map: second arg must be a SPARQL string, got {other:?}"
            ));
        }
    };

    let result = gc::execute_query(&inner_sparql).map_err(map_graph_err)?;
    let flat_bindings = match result {
        CallbackQueryResult::Bindings(bs) => bs,
        CallbackQueryResult::Quads(_) => {
            return Err("wf_map: inner query returned CONSTRUCT-shape result".into());
        }
        CallbackQueryResult::Boolean(_) => {
            return Err("wf_map: inner query returned ASK-shape result".into());
        }
    };

    // Reconstruct rows by splitting on repeated variable identity.
    let mut rows: Vec<Vec<WitBinding>> = Vec::new();
    let mut current: Vec<WitBinding> = Vec::new();
    for b in flat_bindings {
        if current.iter().any(|prev| prev.variable == b.variable) {
            rows.push(std::mem::take(&mut current));
        }
        current.push(b);
    }
    if !current.is_empty() {
        rows.push(current);
    }

    let mut mapped: Vec<JsonValue> = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(first) = row.into_iter().next() else {
            mapped.push(JsonValue::Null);
            continue;
        };
        let sub_args = vec![first.value];
        let sub = wc::invoke_wasm_service(&url, &sub_args).map_err(map_wasm_err)?;
        mapped.push(first_scalar_of(sub));
    }

    let payload = serde_json::to_string(&JsonValue::Array(mapped))
        .map_err(|e| format!("wf_map: serializing output: {e}"))?;
    Ok(json_literal(&payload))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_map".into(),
            min_arity: 2,
            max_arity: Some(2),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_map" => map_impl(&args),
            other => Err(format!("wf_map: unknown function '{other}'")),
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
            "wf_map: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_map: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_map: aggregate state was never constructed".into())
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
            "wf_map: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
