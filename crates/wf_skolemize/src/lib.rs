//! wf_skolemize — replace every blank node with a deterministic
//! well-known-genid IRI.
//!
//! Migrated (Follow-up E) from the Stardog overlay
//! `stardog:webfunction@0.5.0` world to the base
//! `tegmentum:webfunction/extension-with-host-callbacks@0.1.0` world.
//!
//! Signature: `wf_skolemize()` -> rdf:JSON literal
//!   `{ "renamed": <int>, "deleted": <int> }`
//!
//! Walks every triple involving a blank-node subject or object, mints
//! a deterministic IRI per bnode from a stable hash of its label, and
//! rewrites the graph in three phases:
//!   1. Enumerate every triple that involves a blank node in either
//!      subject or object position via `graph-callbacks::execute-query`.
//!   2. INSERT rewritten (ground) triples via
//!      `graph-callbacks::execute-update`.
//!   3. DELETE every remaining bnode-bearing triple in one filter-
//!      based sweep.
//!
//! The Stardog-era shape returned one row with two columns (renamed,
//! deleted); the base `extension::call` surface returns a single
//! term, so the counts collapse into one rdf:JSON literal.

#[allow(warnings)]
mod bindings;

use serde_json::json;

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
    self as gc, Binding as WitBinding, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const GENID_PREFIX: &str = "https://tegmentum.ai/.well-known/genid/";
const SALT: u64 = 0x9E3779B97F4A7C15; // fractional part of the golden ratio

// ---------------------------------------------------------------------------
// graph-callbacks helpers
// ---------------------------------------------------------------------------

fn execute_query(sparql: &str) -> Result<CallbackQueryResult, String> {
    gc::execute_query(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

fn execute_update(sparql: &str) -> Result<(), String> {
    gc::execute_update(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

fn group_bindings_into_rows(flat: Vec<WitBinding>) -> Vec<Vec<WitBinding>> {
    let mut rows: Vec<Vec<WitBinding>> = Vec::new();
    let mut current: Vec<WitBinding> = Vec::new();
    for b in flat {
        if current.iter().any(|prior| prior.variable == b.variable) {
            rows.push(std::mem::take(&mut current));
        }
        current.push(b);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

// ---------------------------------------------------------------------------
// Guest impls
// ---------------------------------------------------------------------------

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_skolemize".into(),
            min_arity: 0,
            max_arity: Some(0),
        }]
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_skolemize" => wf_skolemize_impl(),
            other => Err(format!("wf_skolemize: unknown function '{other}'")),
        }
    }
}

fn wf_skolemize_impl() -> Result<WitTerm, String> {
    // Phase 1: enumerate every triple that involves a blank node.
    let read_sparql = "\
        SELECT ?s ?p ?o WHERE { \
          ?s ?p ?o . \
          FILTER(isBlank(?s) || isBlank(?o)) \
        }";
    let flat = match execute_query(read_sparql)? {
        CallbackQueryResult::Bindings(b) => b,
        _ => {
            return Err(
                "wf_skolemize: SELECT ?s ?p ?o unexpectedly returned non-bindings shape"
                    .into(),
            );
        }
    };
    let rows = group_bindings_into_rows(flat);

    // Phase 2: emit rewritten ground triples in one INSERT DATA batch.
    let mut insert_body = String::new();
    let mut renamed = 0u64;
    let mut unique_bnodes = std::collections::HashSet::new();

    for row in &rows {
        let s = binding_value(row, "s");
        let p = binding_value(row, "p");
        let o = binding_value(row, "o");
        let (Some(s), Some(p), Some(o)) = (s, p, o) else {
            continue;
        };

        let s_txt = value_to_sparql(&s, &mut unique_bnodes);
        let p_txt = value_to_sparql(&p, &mut unique_bnodes);
        let o_txt = value_to_sparql(&o, &mut unique_bnodes);

        insert_body.push_str(&s_txt);
        insert_body.push(' ');
        insert_body.push_str(&p_txt);
        insert_body.push(' ');
        insert_body.push_str(&o_txt);
        insert_body.push_str(" .\n");
        renamed += 1;
    }

    if !insert_body.is_empty() {
        let insert = format!("INSERT DATA {{ {insert_body} }}");
        execute_update(&insert)
            .map_err(|e| format!("wf_skolemize: insert rewritten batch: {e}"))?;
    }

    // Phase 3: delete every remaining bnode-bearing triple.
    let delete_update = "\
        DELETE { ?s ?p ?o } \
        WHERE  { ?s ?p ?o . FILTER(isBlank(?s) || isBlank(?o)) }";
    execute_update(delete_update)
        .map_err(|e| format!("wf_skolemize: delete originals: {e}"))?;

    let payload = json!({
        "renamed": unique_bnodes.len() as u64,
        "deleted": renamed,
    });
    let serialized = serde_json::to_string(&payload)
        .map_err(|e| format!("wf_skolemize: serialize summary: {e}"))?;
    Ok(WitTerm::Literal(WitLiteral {
        value: serialized,
        datatype: Some(RDF_JSON.into()),
        language: None,
    }))
}

impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "wf_skolemize: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_skolemize: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_skolemize: aggregate state was never constructed".into())
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
            "wf_skolemize: unknown property function '{name}' (this component provides none)"
        ))
    }
}

// ---------------------------------------------------------------------------
// Rewriting helpers
// ---------------------------------------------------------------------------

fn value_to_sparql(v: &WitTerm, seen_bnodes: &mut std::collections::HashSet<String>) -> String {
    match v {
        WitTerm::NamedNode(s) => format!("<{s}>"),
        WitTerm::BlankNode(label) => {
            seen_bnodes.insert(label.clone());
            format!("<{}>", mint_genid(label))
        }
        WitTerm::Literal(l) => {
            let escaped = l
                .value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            if let Some(lang) = &l.language {
                format!("\"{escaped}\"@{lang}")
            } else if let Some(dt) = &l.datatype {
                format!("\"{escaped}\"^^<{dt}>")
            } else {
                format!("\"{escaped}\"^^<{XSD_STRING}>")
            }
        }
        WitTerm::Triple(_) => {
            // Quoted triples in either subject or object position — skip
            // (skolemization discipline is for blank-node substitution).
            String::new()
        }
    }
}

fn binding_value(row: &[WitBinding], name: &str) -> Option<WitTerm> {
    row.iter()
        .find(|b| b.variable == name)
        .map(|b| b.value.clone())
}

fn mint_genid(label: &str) -> String {
    let mut hash: u64 = SALT;
    for byte in label.bytes() {
        hash = hash.wrapping_mul(0x100000001B3).wrapping_add(byte as u64);
    }
    let mut hash2: u64 = hash.rotate_left(23) ^ 0x428A2F98D728AE22;
    for byte in label.bytes() {
        hash2 = hash2.wrapping_mul(0x100000001B3).wrapping_add(byte as u64);
    }
    format!("{GENID_PREFIX}{hash:016x}{hash2:016x}")
}

bindings::export!(Component with_types_in bindings);
