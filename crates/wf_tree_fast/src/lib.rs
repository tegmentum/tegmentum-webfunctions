//! wf_tree_fast — recursive tree walker over a fixed predicate.
//!
//! Signature: `wf:tree_fast(root, predicate)` — returns an rdf:JSON
//! literal describing the tree rooted at `root` whose edges are the
//! `predicate` triples reachable from it.
//!
//! Migration deviation (Follow-up F): the legacy Stardog overlay
//! `stardog:webfunction@0.3.3` published a `host::follow-predicate`
//! primitive that pushed the "collect all children of one node under one
//! predicate" walk into the plugin — cutting ~5x vs a SPARQL sub-query.
//! The R1 substrate contract does not carry follow-predicate (the
//! optimisation was Stardog-specific and did not survive substrate-
//! neutrality review); this crate now issues a synthesised SPARQL
//! `SELECT ?child WHERE { <node> <predicate> ?child }` via
//! `graph-callbacks::execute-query` at each descent instead. Behavior
//! is preserved end-to-end; the ~5x fast-path speedup is not.
//!
//! Depth-guard remains on the substrate side —
//! `observability-callbacks::callback-depth` is still available under
//! the new world.

#[allow(warnings)]
mod bindings;

use serde_json::{Value as JsonValue, json};
use std::collections::HashSet;

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
use bindings::tegmentum::webfunction::observability_callbacks as obs;
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const DEPTH_SOFT_CAP: u32 = 90;

fn term_key(v: &WitTerm) -> String {
    match v {
        WitTerm::NamedNode(uri) => uri.clone(),
        WitTerm::BlankNode(id) => format!("_:{id}"),
        WitTerm::Literal(l) => l.value.clone(),
        WitTerm::Triple(_) => "<<quoted-triple>>".into(),
    }
}

fn predicate_iri(v: &WitTerm) -> Result<String, String> {
    match v {
        WitTerm::NamedNode(s) => Ok(s.clone()),
        _ => Err("wf:tree_fast: predicate arg must be an IRI".into()),
    }
}

fn node_sparql_term(v: &WitTerm) -> Result<String, String> {
    match v {
        WitTerm::NamedNode(s) => Ok(format!("<{s}>")),
        WitTerm::BlankNode(label) => Ok(format!("_:{label}")),
        _ => Err("wf:tree_fast: root arg must be an IRI or blank node".into()),
    }
}

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.into(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    })
}

/// Follow one hop across a fixed predicate by issuing a SPARQL SELECT.
/// Returns the list of `?child` terms bound at the current node.
fn follow_predicate(node: &WitTerm, predicate_uri: &str) -> Result<Vec<WitTerm>, String> {
    let node_term = node_sparql_term(node)?;
    let sparql = format!(
        "SELECT ?child WHERE {{ {node_term} <{predicate_uri}> ?child }}"
    );
    let result = gc::execute_query(&sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })?;
    match result {
        CallbackQueryResult::Bindings(bs) => Ok(bs
            .into_iter()
            .filter(|b| b.variable == "child")
            .map(|b| b.value)
            .collect()),
        CallbackQueryResult::Quads(_) => Err("wf:tree_fast: unexpected CONSTRUCT result".into()),
        CallbackQueryResult::Boolean(_) => Err("wf:tree_fast: unexpected ASK result".into()),
    }
}

fn walk(node: &WitTerm, predicate_uri: &str, seen: &mut HashSet<String>) -> JsonValue {
    let key = term_key(node);
    let mut obj = serde_json::Map::new();
    obj.insert("uri".into(), json!(key));

    if !seen.insert(key.clone()) {
        obj.insert("cycle".into(), json!(true));
        return JsonValue::Object(obj);
    }

    if obs::callback_depth() >= DEPTH_SOFT_CAP {
        obj.insert("depth_bounded".into(), json!(true));
        seen.remove(&key);
        return JsonValue::Object(obj);
    }

    let children_terms = match follow_predicate(node, predicate_uri) {
        Ok(vs) => vs,
        Err(e) => {
            obj.insert("error".into(), json!(e));
            seen.remove(&key);
            return JsonValue::Object(obj);
        }
    };

    let children: Vec<JsonValue> = children_terms
        .iter()
        .map(|c| walk(c, predicate_uri, seen))
        .collect();
    obj.insert("children".into(), JsonValue::Array(children));

    seen.remove(&key);
    JsonValue::Object(obj)
}

fn tree_fast_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    if args.len() != 2 {
        return Err(format!(
            "wf:tree_fast: expected 2 args (root, predicate), got {}",
            args.len()
        ));
    }
    let root = args[0].clone();
    let predicate_uri = predicate_iri(&args[1])?;

    let mut seen: HashSet<String> = HashSet::new();
    let tree = walk(&root, &predicate_uri, &mut seen);
    Ok(json_literal(&tree.to_string()))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_tree_fast".into(),
            min_arity: 2,
            max_arity: Some(2),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_tree_fast" => tree_fast_impl(&args),
            other => Err(format!("wf_tree_fast: unknown function '{other}'")),
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
            "wf_tree_fast: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_tree_fast: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_tree_fast: aggregate state was never constructed".into())
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
            "wf_tree_fast: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
