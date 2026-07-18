//! wf_apply — call-by-reference higher-order combinator.
//!
//! Signature: `wf:apply(<function-iri>, args...)`.
//!
//! The first argument names a resource in the local graph whose
//! `<http://tegmentum.ai/ns/composition/source>` triple carries the wasm
//! URL to invoke. Remaining arguments flow through unchanged as that
//! wasm's positional inputs. Semantically equivalent to
//! `wf:call(?url, args...)` after the dereference — the point is the
//! late-binding through RDF: a function's identity is an IRI you also
//! use elsewhere in the graph.
//!
//! Migration (Follow-up F): moved off the Stardog overlay onto the
//! substrate `tegmentum:webfunction/extension-with-all-host-callbacks
//! @0.1.0` world. Dereference uses
//! `graph-callbacks::execute-query`; sub-invocation uses
//! `wasm-callbacks::invoke-wasm-service` (the R1 property-function-
//! shape call, `(url, list<term>) -> list<binding>`). The sub-wasm's
//! flat binding list is packed into an rdf:JSON payload keyed by the
//! projected variable names so the base-substrate filter surface
//! returns a single term.

#[allow(warnings)]
mod bindings;

use serde_json::{Map as JsonMap, Value as JsonValue, json};

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
const COMP_SOURCE_IRI: &str = "http://tegmentum.ai/ns/composition/source";

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.into(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    })
}

fn term_to_json(v: &WitTerm) -> JsonValue {
    match v {
        WitTerm::NamedNode(uri) => json!(uri),
        WitTerm::BlankNode(id) => json!(format!("_:{id}")),
        WitTerm::Literal(l) => json!(l.value),
        WitTerm::Triple(_) => json!("<<quoted-triple>>"),
    }
}

fn map_wasm_err(e: WasmCallError) -> String {
    match e {
        WasmCallError::NotFound(m) => format!("wasm-callbacks not-found: {m}"),
        WasmCallError::InvocationError(m) => format!("wasm-callbacks invocation-error: {m}"),
        WasmCallError::NotPermitted(m) => format!("wasm-callbacks not-permitted: {m}"),
    }
}

fn map_graph_err(e: gc::GraphCallError) -> String {
    match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    }
}

/// Split the flat `list<binding>` returned by `invoke-wasm-service` on
/// repeated variable identity (the R1 property-function-shape return
/// convention). Then pack the row grid as an rdf:JSON payload keyed by
/// projected variable name — the single-term filter surface's honest
/// encoding of a multi-row / multi-column sub-result.
fn pack_binding_rows(flat: Vec<WitBinding>) -> WitTerm {
    let mut rows: Vec<Vec<WitBinding>> = Vec::new();
    let mut current: Vec<WitBinding> = Vec::new();
    for b in flat {
        if current.iter().any(|prev| prev.variable == b.variable) {
            rows.push(std::mem::take(&mut current));
        }
        current.push(b);
    }
    if !current.is_empty() {
        rows.push(current);
    }

    let mut vars_ordered: Vec<String> = Vec::new();
    let json_rows: Vec<JsonValue> = rows
        .into_iter()
        .map(|row| {
            let mut obj = JsonMap::new();
            for b in row {
                if !vars_ordered.contains(&b.variable) {
                    vars_ordered.push(b.variable.clone());
                }
                obj.insert(b.variable, term_to_json(&b.value));
            }
            JsonValue::Object(obj)
        })
        .collect();

    let out = json!({
        "vars": vars_ordered,
        "rows": json_rows,
    });
    json_literal(&out.to_string())
}

fn apply_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    if args.is_empty() {
        return Err("wf_apply: need at least a function IRI (first arg)".into());
    }
    let fn_iri = match &args[0] {
        WitTerm::NamedNode(iri) => iri.clone(),
        other => {
            return Err(format!(
                "wf_apply: first arg must be an IRI, got {other:?}"
            ));
        }
    };

    let sparql = format!(
        "SELECT ?url WHERE {{ <{fn_iri}> <{COMP_SOURCE_IRI}> ?url }} LIMIT 1"
    );
    let result = gc::execute_query(&sparql).map_err(map_graph_err)?;
    let url = match result {
        CallbackQueryResult::Bindings(bs) => {
            let url = bs
                .into_iter()
                .find(|b| b.variable == "url")
                .ok_or_else(|| {
                    format!("wf_apply: no <{COMP_SOURCE_IRI}> triple for {fn_iri}")
                })?;
            match url.value {
                WitTerm::NamedNode(s) => s,
                WitTerm::Literal(l) => l.value,
                other => {
                    return Err(format!(
                        "wf_apply: comp:source of {fn_iri} not an IRI or literal: {other:?}"
                    ));
                }
            }
        }
        CallbackQueryResult::Quads(_) => {
            return Err("wf_apply: dereference SELECT returned CONSTRUCT-shape result".into());
        }
        CallbackQueryResult::Boolean(_) => {
            return Err("wf_apply: dereference SELECT returned ASK-shape result".into());
        }
    };

    let sub_args: Vec<WitTerm> = args[1..].to_vec();
    let sub_bindings = wc::invoke_wasm_service(&url, &sub_args).map_err(map_wasm_err)?;

    Ok(pack_binding_rows(sub_bindings))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_apply".into(),
            min_arity: 1,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_apply" => apply_impl(&args),
            other => Err(format!("wf_apply: unknown function '{other}'")),
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
            "wf_apply: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_apply: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_apply: aggregate state was never constructed".into())
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
            "wf_apply: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
