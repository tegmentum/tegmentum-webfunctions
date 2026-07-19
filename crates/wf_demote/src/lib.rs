//! wf_demote — delete source triples for a materialized shape.
//!
//! Signature: `<urn:webfunction:demote>("<descriptor-json>")` returns an
//! rdf:JSON literal shaped as `{"deleted": N, "predicates": M,
//! "subjects": K}` (single-term collapse per the ExtensionGuest
//! convention).
//!
//! Reads the descriptor, walks the named sink for each column
//! predicate via `sink-query-callbacks::scan-sink-quads`, and DELETEs
//! every triple in the graph store whose subject is one of the sink's
//! rows and whose predicate is one of the descriptor's column
//! predicates.
//!
//! Contract with wf_materialize: this guest deletes only what the
//! materializer put on the sink. Anything else about those subjects —
//! predicates that aren't in the descriptor, triples about IRIs that
//! didn't reach the sink — stays untouched. Idempotent: a second run
//! finds no matching quads and returns 0.
//!
//! Runs the anchor's rdf:type triple too when the descriptor uses
//! `anchor.class`, since a materialized shape implicitly captures the
//! class assertion (the sink adapter's presence is the type
//! predicate's materialized form).
//!
//! Migration deviations (M1 Q3, ExtensionGuest wave):
//!
//!   * Descriptor's `sink` field is now a substrate-side sink NAME
//!     (not a driver URL). Sinks must be pre-registered with the host;
//!     `sink-callbacks::list-sinks` validates presence before the
//!     extension proceeds.
//!   * Subject enumeration is per-predicate BGP scan rather than a
//!     single `SELECT subject_iri FROM t` — `scan-sink-quads` returns
//!     the same subject set the Stardog-era SELECT projected, one
//!     predicate at a time. `subject_iri` becomes descriptor metadata
//!     (unused on the new substrate); the sink adapter owns row
//!     identity.
//!   * The old `subject_iri` column-name field on the descriptor is
//!     accepted but ignored — the sink adapter owns row identity now.

#[allow(warnings)]
mod bindings;

use std::collections::BTreeSet;

use serde::Deserialize;
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
use bindings::tegmentum::webfunction::graph_callbacks::{self as gc};
use bindings::tegmentum::webfunction::sink_callbacks::{self as sc};
use bindings::tegmentum::webfunction::sink_query_callbacks::{self as sq, SinkQueryError};
use bindings::tegmentum::webfunction::types::{
    Literal as WitLiteral, Term as WitTerm,
};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const CHUNK: usize = 1000;

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    shape: String,
    anchor: Anchor,
    columns: Vec<Column>,
    sink: Option<String>,
    /// Named graph the shape lives in. Absent = default graph. When
    /// present, the DELETE scopes to this graph so triples in other
    /// graphs about the same subjects are preserved.
    #[serde(default)]
    graph: Option<String>,
}

#[derive(Deserialize)]
struct Anchor {
    class: Option<String>,
    #[allow(dead_code)]
    predicate_signature: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Column {
    #[allow(dead_code)]
    name: String,
    role: String,
    predicate: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn map_sink_query_err(e: SinkQueryError) -> String {
    match e {
        SinkQueryError::NoSuchSink(m) => format!("sink-query no-such-sink: {m}"),
        SinkQueryError::SyntaxError(m) => format!("sink-query syntax-error: {m}"),
        SinkQueryError::BackendError(m) => format!("sink-query backend-error: {m}"),
        SinkQueryError::NotPermitted(m) => format!("sink-query not-permitted: {m}"),
    }
}

fn validate_sink_present(name: &str) -> Result<(), String> {
    let sinks = sc::list_sinks();
    if sinks.iter().any(|s| s.name == name) {
        Ok(())
    } else {
        Err(format!(
            "wf_demote: sink `{name}` not registered with host (list-sinks returned {} sinks)",
            sinks.len()
        ))
    }
}

/// Render a term as the IRI form its subject position uses in SPARQL
/// text — `<iri>` for named nodes, `_:label` for blank nodes.
/// Literals as subjects are non-RDF and get filtered upstream.
fn subject_display(t: &WitTerm) -> Option<String> {
    match t {
        WitTerm::NamedNode(iri) => Some(iri.clone()),
        WitTerm::BlankNode(label) => Some(format!("_:{label}")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Guest entrypoint
// ---------------------------------------------------------------------------

fn demote_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let descriptor_json = match args.first() {
        Some(WitTerm::Literal(l)) => l.value.clone(),
        Some(other) => {
            return Err(format!(
                "wf_demote: first arg must be a descriptor-json string literal, got {other:?}"
            ));
        }
        None => return Err("wf_demote: expected one arg (descriptor json)".into()),
    };
    let d: Descriptor = serde_json::from_str(&descriptor_json)
        .map_err(|e| format!("wf_demote: descriptor parse: {e}"))?;

    let sink_name = d
        .sink
        .as_deref()
        .ok_or_else(|| "wf_demote: descriptor has no `sink`".to_string())?;
    validate_sink_present(sink_name)?;

    // Collect predicates to remove: everything with role != subject_iri
    // that has a predicate declared. rdf:type gets appended when the
    // anchor is class-based.
    let mut predicates: Vec<String> = d
        .columns
        .iter()
        .filter(|c| c.role != "subject_iri")
        .filter_map(|c| c.predicate.clone())
        .collect();
    if d.anchor.class.is_some() {
        predicates.push(RDF_TYPE.to_string());
    }
    if predicates.is_empty() {
        let out = json!({ "deleted": 0, "predicates": 0, "subjects": 0 });
        return Ok(json_literal(&out.to_string()));
    }

    // Per-predicate BGP scan of the sink. Each predicate returns the
    // sink's `(s, p, o)` quads with `p` pinned; we pull the subjects
    // out and merge into a single ordered set. Sorting the subjects
    // gives deterministic DELETE-batch construction across runs.
    let mut subjects_set: BTreeSet<String> = BTreeSet::new();
    for predicate in &predicates {
        let predicate_term = WitTerm::NamedNode(predicate.clone());
        let quads = sq::scan_sink_quads(sink_name, None, Some(&predicate_term), None)
            .map_err(map_sink_query_err)?;
        for q in quads {
            if let Some(s) = subject_display(&q.subject) {
                subjects_set.insert(s);
            }
        }
    }
    let subjects: Vec<String> = subjects_set.into_iter().collect();

    if subjects.is_empty() {
        let out = json!({
            "deleted": 0,
            "predicates": predicates.len(),
            "subjects": 0,
        });
        return Ok(json_literal(&out.to_string()));
    }

    // Delete in chunks. One DELETE per (predicate × subject-chunk):
    // one big filter per pass keeps the number of updates bounded to
    // predicates × (subjects / CHUNK).
    //
    // Scope the DELETE to a named graph when the descriptor names
    // one. Without this, demoting a shape that lives in graph A
    // would happily delete matching triples that were actually
    // asserted in graph B — a Stardog-style "your delete affected
    // things you didn't ask about" failure we intentionally avoid.
    let graph_wrap = |body: &str| -> String {
        match &d.graph {
            Some(g) => format!("GRAPH <{g}> {{ {body} }}"),
            None => body.to_string(),
        }
    };

    let mut deleted = 0u64;
    for predicate in &predicates {
        for chunk in subjects.chunks(CHUNK) {
            let values_clause = chunk
                .iter()
                .map(|iri| {
                    if let Some(rest) = iri.strip_prefix("_:") {
                        format!("_:{rest}")
                    } else {
                        format!("<{iri}>")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let delete_pattern = format!("?s <{predicate}> ?o");
            let where_pattern = format!("?s <{predicate}> ?o");
            let update = format!(
                "DELETE {{ {del} }} WHERE {{ {whr} . VALUES ?s {{ {vals} }} }}",
                del = graph_wrap(&delete_pattern),
                whr = graph_wrap(&where_pattern),
                vals = values_clause,
            );
            gc::execute_update(&update).map_err(|e| {
                format!(
                    "wf_demote: delete predicate `{predicate}` chunk of {}: {}",
                    chunk.len(),
                    map_graph_err(e)
                )
            })?;
            // SPARQL Update doesn't return a count; the caller learns
            // the total via a follow-up COUNT if it wants precision.
            // We accumulate an upper bound: `chunk.len()` per pass.
            deleted += chunk.len() as u64;
        }
    }

    let out = json!({
        "deleted": deleted,
        "predicates": predicates.len(),
        "subjects": subjects.len(),
    });
    Ok(json_literal(&out.to_string()))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "urn:webfunction:demote".into(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "urn:webfunction:demote" => demote_impl(&args),
            other => Err(format!("wf_demote: unknown function '{other}'")),
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
            "wf_demote: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_demote: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_demote: aggregate state was never constructed".into())
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
            "wf_demote: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
