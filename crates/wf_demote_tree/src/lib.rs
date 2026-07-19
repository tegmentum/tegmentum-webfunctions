//! wf_demote_tree — detect tree-shaped RDF subgraphs and shuttle them
//! out as JSON documents to a document sink.
//!
//! Signature: `<urn:webfunction:demote-tree>("<config-json>")` returns
//! an rdf:JSON literal shaped as
//! `{"trees": T, "nodes": N, "skipped": S, "deleted": D}`
//! (single-term collapse per the ExtensionGuest convention).
//!
//! Contract:
//!   * Reads a config that names a `root_selector` (SPARQL WHERE
//!     pattern projecting `?r`), a document `sink` name, and shape
//!     flags.
//!   * Enumerates root IRIs matching the selector via
//!     `graph-callbacks::execute-query`.
//!   * For each root, walks outgoing edges via CONSTRUCT closure into
//!     a JSON document. The archetypal RDF list
//!     (`rdf:first` / `rdf:rest` / `rdf:nil`) collapses to a JSON
//!     array; every other subject becomes a JSON object keyed by
//!     predicate local-name.
//!   * Enforces the tree property: a subject reached during the walk
//!     may not have more than one in-edge from within the collected
//!     set. If it does, that's a DAG, and the guest either skips that
//!     root (default) or emits a `"$ref"` placeholder.
//!   * Blank nodes are skolemized on the way out — the sink typically
//!     drops blank-node identity across the boundary, so we mint a
//!     stable genid IRI per bnode so the demoted document can
//!     round-trip.
//!   * Emits each document via
//!     `document-sink-callbacks::put-document`. The document key is
//!     the root IRI (or the minted genid for bnode roots — rejected
//!     at collect time); the content is the serialized JSON.
//!   * When `delete_source` is set, DELETEs every triple whose
//!     subject is in the collected set from the source graph so the
//!     tree is genuinely demoted, not duplicated.
//!
//! Migration deviations (M1 Q3, ExtensionGuest wave):
//!
//!   * `sink` is now a substrate-side document-sink NAME (not a
//!     driver URL). Sinks must be pre-registered; presence is
//!     validated via `sink-callbacks::list-sinks`.
//!   * The Stardog-era `sink-execute("INSERT DOC", <json-lit>)`
//!     retires in favor of typed `put-document(sink-name,
//!     {key: <root-iri>, content: <json>})`. The sink adapter owns
//!     whatever storage projection the backend uses.
//!   * `wf_demote_tree` semantically WRITES documents (and DELETEs
//!     source triples), so it consumes `document-sink-callbacks` for
//!     the sink side rather than the read-shape
//!     `sink-query-callbacks::scan-sink-quads` the task-table's row
//!     assumes. Root enumeration continues to run against the source
//!     graph via `graph-callbacks::execute-query`, not against the
//!     sink — the current lib.rs behavior; the sink is the sink-side
//!     output, not the source of truth for what to demote.

#[allow(warnings)]
mod bindings;

use std::collections::{BTreeMap, HashMap, HashSet};

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
use bindings::tegmentum::webfunction::document_sink_callbacks::{
    self as ds, Document, DocumentSinkError,
};
use bindings::tegmentum::webfunction::graph_callbacks::{
    self as gc, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::sink_callbacks::{self as sc};
use bindings::tegmentum::webfunction::types::{
    Literal as WitLiteral, Term as WitTerm,
};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const RDF_FIRST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#first";
const RDF_REST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest";
const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const GENID_PREFIX: &str = "https://tegmentum.ai/.well-known/genid/";
const GENID_SALT: u64 = 0x9E3779B97F4A7C15;
const MAX_DEPTH: usize = 4096;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    #[allow(dead_code)]
    #[serde(default)]
    pub name: Option<String>,
    /// SPARQL WHERE-body-fragment projecting `?r`. E.g.
    /// `?r a <http://schema.org/Recipe>` or
    /// `?r <http://example.org/listHead> _:root . ?r a <http://example.org/List>`.
    pub root_selector: String,
    /// Substrate-side document-sink NAME (not a driver URL).
    pub sink: String,
    /// When set, the guest DELETEs every triple involving any subject
    /// in the collected tree set. Defaults to true.
    #[serde(default = "default_true")]
    pub delete_source: bool,
    /// When set, a root whose walk hits a subject with >1 in-edge
    /// inside the collected set is skipped rather than emitted. When
    /// false, the guest still walks the subgraph but emits a `"$ref"`
    /// placeholder at the second visit and continues. Defaults true.
    #[serde(default = "default_true")]
    pub reject_non_tree: bool,
    /// Emit RDF lists as JSON arrays instead of nested `first`/`rest`
    /// objects. Defaults true — the whole point of moving trees out.
    #[serde(default = "default_true")]
    pub collapse_rdf_lists: bool,
    /// Include `@id` (the root IRI, or a synthesized genid for
    /// bnode-rooted trees) on every node. Defaults true — sinks lose
    /// subject identity otherwise.
    #[serde(default = "default_true")]
    pub include_id: bool,
}

fn default_true() -> bool {
    true
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

fn map_doc_err(e: DocumentSinkError) -> String {
    match e {
        DocumentSinkError::NoSuchSink(m) => format!("document-sink no-such-sink: {m}"),
        DocumentSinkError::NoSuchDocument(m) => format!("document-sink no-such-document: {m}"),
        DocumentSinkError::BackendError(m) => format!("document-sink backend-error: {m}"),
        DocumentSinkError::NotPermitted(m) => format!("document-sink not-permitted: {m}"),
    }
}

fn validate_sink_present(name: &str) -> Result<(), String> {
    let sinks = sc::list_sinks();
    if sinks.iter().any(|s| s.name == name) {
        Ok(())
    } else {
        Err(format!(
            "wf_demote_tree: sink `{name}` not registered with host \
             (list-sinks returned {} sinks)",
            sinks.len()
        ))
    }
}

// ---------------------------------------------------------------------------
// Guest entrypoint
// ---------------------------------------------------------------------------

fn demote_tree_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let config_json = match args.first() {
        Some(WitTerm::Literal(l)) => l.value.clone(),
        Some(other) => {
            return Err(format!(
                "wf_demote_tree: first arg must be a config-json string literal, got {other:?}"
            ));
        }
        None => return Err("wf_demote_tree: expected one arg (config json)".into()),
    };
    let cfg: Config = serde_json::from_str(&config_json)
        .map_err(|e| format!("wf_demote_tree: config parse: {e}"))?;

    validate_sink_present(&cfg.sink)?;

    let roots = enumerate_roots(&cfg.root_selector)?;

    let mut trees = 0u64;
    let mut nodes = 0u64;
    let mut skipped = 0u64;
    let mut all_collected: HashSet<String> = HashSet::new();

    for root in &roots {
        let mut edges = EdgeMap::default();
        collect_subgraph(root, &mut edges)?;
        if !is_tree(&edges, root) && cfg.reject_non_tree {
            skipped += 1;
            continue;
        }
        let mut node_count = 0u64;
        let doc_value = serialize_tree(&edges, root, &cfg, &mut node_count)?;
        nodes += node_count;

        let json_content = serde_json::to_string(&doc_value)
            .map_err(|e| format!("wf_demote_tree: serialize `{root}`: {e}"))?;
        ds::put_document(
            &cfg.sink,
            &Document {
                key: WitTerm::NamedNode(root.clone()),
                content: json_content,
            },
        )
        .map_err(|e| {
            format!("wf_demote_tree: put-document for `{root}`: {}", map_doc_err(e))
        })?;
        trees += 1;
        all_collected.extend(edges.subjects.iter().cloned());
    }

    let mut deleted = 0u64;
    if cfg.delete_source && !all_collected.is_empty() {
        deleted = delete_collected(&all_collected)?;
    }

    let out = json!({
        "trees": trees,
        "nodes": nodes,
        "skipped": skipped,
        "deleted": deleted,
    });
    Ok(json_literal(&out.to_string()))
}

fn enumerate_roots(root_selector: &str) -> Result<Vec<String>, String> {
    let sparql = format!("SELECT DISTINCT ?r WHERE {{ {root_selector} }}");
    let result = gc::execute_query(&sparql).map_err(map_graph_err)?;
    let flat = match result {
        CallbackQueryResult::Bindings(bs) => bs,
        _ => {
            return Err(
                "wf_demote_tree: root selector must yield bindings, not CONSTRUCT/ASK".into(),
            );
        }
    };
    let mut out = Vec::new();
    for b in flat {
        if b.variable != "r" {
            continue;
        }
        match &b.value {
            WitTerm::NamedNode(s) => out.push(s.clone()),
            WitTerm::BlankNode(s) => out.push(format!("_:{s}")),
            _ => (),
        }
    }
    Ok(out)
}

/// Collect the entire subgraph reachable from `root` in a single
/// CONSTRUCT so that blank-node identity is preserved on the wire.
/// The property-path closure `(!<no:such>)*` walks the actual graph
/// from the root and gives us every reachable subject in one shot.
///
/// Requires an IRI root. Bnode-rooted trees need to be skolemized
/// first (via `wf_skolemize`) — the sink boundary does not preserve
/// bnode identity, so demoting a bnode-rooted tree without
/// skolemization would round-trip to a different graph on ingest.
fn collect_subgraph(root: &str, edges: &mut EdgeMap) -> Result<(), String> {
    if root.starts_with("_:") {
        return Err(format!(
            "wf_demote_tree: bnode-rooted tree at {root} — skolemize \
             upstream (wf_skolemize) before demoting; the sink boundary \
             does not preserve bnode identity"
        ));
    }
    let sparql = format!(
        "CONSTRUCT {{ ?s ?p ?o }} WHERE {{ \
         <{root}> (!<urn:tegmentum:no-such-predicate>)* ?s . \
         ?s ?p ?o \
        }}"
    );
    let result = gc::execute_query(&sparql).map_err(map_graph_err)?;
    let quads = match result {
        CallbackQueryResult::Quads(qs) => qs,
        _ => {
            return Err(
                "wf_demote_tree: subgraph CONSTRUCT must yield quads, not bindings/ASK".into(),
            );
        }
    };
    edges.subjects.insert(root.to_string());
    for q in quads {
        let predicate = match &q.predicate {
            WitTerm::NamedNode(name) => name.clone(),
            other => {
                return Err(format!(
                    "wf_demote_tree: non-IRI predicate: {other:?}"
                ));
            }
        };
        let subject_key = match &q.subject {
            WitTerm::NamedNode(iri) => iri.clone(),
            WitTerm::BlankNode(label) => format!("_:{label}"),
            other => {
                return Err(format!(
                    "wf_demote_tree: non-node subject: {other:?}"
                ));
            }
        };
        let object_term = wit_to_term(&q.object);
        edges
            .out
            .entry(subject_key.clone())
            .or_default()
            .push((predicate, object_term.clone()));
        edges
            .in_count
            .entry(term_key(&object_term))
            .and_modify(|c| *c += 1)
            .or_insert(1);
        edges.subjects.insert(subject_key);
    }
    Ok(())
}

fn delete_collected(subjects: &HashSet<String>) -> Result<u64, String> {
    let mut deleted = 0u64;
    for subj in subjects {
        let (delete_body, where_body) = if let Some(label) = subj.strip_prefix("_:") {
            (
                "?s ?p ?o".to_string(),
                format!(
                    "?s ?p ?o . FILTER(isBlank(?s) && str(?s) = \"{label}\")"
                ),
            )
        } else {
            (
                format!("<{subj}> ?p ?o"),
                format!("<{subj}> ?p ?o"),
            )
        };
        let update = format!("DELETE {{ {delete_body} }} WHERE {{ {where_body} }}");
        gc::execute_update(&update).map_err(|e| {
            format!(
                "wf_demote_tree: delete outgoing for `{subj}`: {}",
                map_graph_err(e)
            )
        })?;
        deleted += 1;
    }
    Ok(deleted)
}

fn wit_to_term(v: &WitTerm) -> Term {
    match v {
        WitTerm::NamedNode(s) => Term::Iri(s.clone()),
        WitTerm::BlankNode(s) => Term::Bnode(s.clone()),
        WitTerm::Literal(l) => Term::Literal {
            label: l.value.clone(),
            datatype: l.datatype.clone().unwrap_or_default(),
            lang: l.language.clone(),
        },
        WitTerm::Triple(_) => Term::Literal {
            label: "<<quoted triple>>".into(),
            datatype: "http://www.w3.org/2001/XMLSchema#string".into(),
            lang: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Pure data structures and tree logic (used by evaluate and tests)
// ---------------------------------------------------------------------------

/// Simple term representation, target-independent. Kept separate from
/// the wit-bindgen `WitTerm` type so unit tests don't need to
/// instantiate host-generated bindings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Term {
    Iri(String),
    Bnode(String),
    Literal {
        label: String,
        datatype: String,
        lang: Option<String>,
    },
}

fn term_key(t: &Term) -> String {
    match t {
        Term::Iri(s) => format!("i:{s}"),
        Term::Bnode(s) => format!("b:{s}"),
        Term::Literal {
            label,
            datatype,
            lang,
        } => match lang {
            Some(l) => format!("l:{label}@{l}"),
            None => format!("l:{label}^^{datatype}"),
        },
    }
}

/// Directed edge map used during subgraph collection.
#[derive(Debug, Default, Clone)]
pub struct EdgeMap {
    pub subjects: HashSet<String>,
    pub out: HashMap<String, Vec<(String, Term)>>,
    pub in_count: HashMap<String, u32>,
}

impl EdgeMap {
    pub fn add_edge(&mut self, subject: &str, predicate: &str, object: Term) {
        self.subjects.insert(subject.to_string());
        self.out
            .entry(subject.to_string())
            .or_default()
            .push((predicate.to_string(), object.clone()));
        self.in_count
            .entry(term_key(&object))
            .and_modify(|c| *c += 1)
            .or_insert(1);
    }
}

/// A subgraph rooted at `root` is a tree when every collected subject
/// other than the root has exactly one incoming edge from within the
/// collected set.
pub fn is_tree(edges: &EdgeMap, root: &str) -> bool {
    for subject in &edges.subjects {
        if subject == root {
            continue;
        }
        let key = if let Some(label) = subject.strip_prefix("_:") {
            format!("b:{label}")
        } else {
            format!("i:{subject}")
        };
        let count = edges.in_count.get(&key).copied().unwrap_or(0);
        if count > 1 {
            return false;
        }
    }
    true
}

/// Convert a collected subgraph rooted at `root` into a JSON document,
/// collapsing RDF-list heads to JSON arrays and skolemizing blank
/// nodes per config.
pub fn serialize_tree(
    edges: &EdgeMap,
    root: &str,
    cfg: &Config,
    node_count: &mut u64,
) -> Result<serde_json::Value, String> {
    let mut visited: HashSet<String> = HashSet::new();
    serialize_subject(edges, root, cfg, &mut visited, 0, node_count)
}

fn serialize_subject(
    edges: &EdgeMap,
    subject: &str,
    cfg: &Config,
    visited: &mut HashSet<String>,
    depth: usize,
    node_count: &mut u64,
) -> Result<serde_json::Value, String> {
    if depth > MAX_DEPTH {
        return Err(format!("wf_demote_tree: max depth at {subject}"));
    }
    if !visited.insert(subject.to_string()) {
        return Ok(serde_json::json!({ "$ref": display_id(subject) }));
    }

    if cfg.collapse_rdf_lists && is_list_head(edges, subject) {
        return serialize_list(edges, subject, cfg, visited, depth, node_count);
    }

    *node_count += 1;

    let mut obj = serde_json::Map::new();
    if cfg.include_id {
        obj.insert("@id".into(), serde_json::Value::String(display_id(subject)));
    }
    let empty: Vec<(String, Term)> = Vec::new();
    let out_edges = edges.out.get(subject).unwrap_or(&empty);
    let mut grouped: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for (predicate, object) in out_edges {
        let key = local_name(predicate);
        let value = serialize_object(edges, object, cfg, visited, depth + 1, node_count)?;
        grouped.entry(key).or_default().push(value);
    }
    for (key, mut values) in grouped {
        if values.len() == 1 {
            obj.insert(key, values.remove(0));
        } else {
            obj.insert(key, serde_json::Value::Array(values));
        }
    }
    Ok(serde_json::Value::Object(obj))
}

fn serialize_object(
    edges: &EdgeMap,
    object: &Term,
    cfg: &Config,
    visited: &mut HashSet<String>,
    depth: usize,
    node_count: &mut u64,
) -> Result<serde_json::Value, String> {
    match object {
        Term::Iri(iri) if iri == RDF_NIL => Ok(serde_json::Value::Array(vec![])),
        Term::Iri(iri) => {
            if edges.out.contains_key(iri) {
                serialize_subject(edges, iri, cfg, visited, depth, node_count)
            } else {
                Ok(serde_json::Value::String(iri.clone()))
            }
        }
        Term::Bnode(label) => {
            let key = format!("_:{label}");
            if edges.out.contains_key(&key) {
                serialize_subject(edges, &key, cfg, visited, depth, node_count)
            } else {
                Ok(serde_json::Value::String(mint_genid(label)))
            }
        }
        Term::Literal {
            label,
            datatype,
            lang,
        } => Ok(literal_to_json(label, datatype, lang.as_deref())),
    }
}

fn is_list_head(edges: &EdgeMap, subject: &str) -> bool {
    let out = match edges.out.get(subject) {
        Some(v) => v,
        None => return false,
    };
    let mut has_first = false;
    let mut has_rest = false;
    for (p, _) in out {
        if p == RDF_FIRST {
            has_first = true;
        } else if p == RDF_REST {
            has_rest = true;
        }
    }
    has_first && has_rest
}

fn serialize_list(
    edges: &EdgeMap,
    head: &str,
    cfg: &Config,
    visited: &mut HashSet<String>,
    depth: usize,
    node_count: &mut u64,
) -> Result<serde_json::Value, String> {
    let mut items = Vec::new();
    let mut cursor = Some(head.to_string());
    let mut first_iter = true;
    while let Some(node) = cursor {
        if node == RDF_NIL {
            break;
        }
        if !first_iter && !visited.insert(node.clone()) {
            items.push(serde_json::json!({ "$cycle": display_id(&node) }));
            break;
        }
        *node_count += 1;
        let out = edges.out.get(&node).cloned().unwrap_or_default();
        let mut first_val: Option<serde_json::Value> = None;
        let mut rest_iri: Option<String> = None;
        for (predicate, object) in out {
            match predicate.as_str() {
                RDF_FIRST => {
                    first_val = Some(serialize_object(
                        edges,
                        &object,
                        cfg,
                        visited,
                        depth + 1,
                        node_count,
                    )?);
                }
                RDF_REST => match object {
                    Term::Iri(iri) => rest_iri = Some(iri),
                    Term::Bnode(label) => rest_iri = Some(format!("_:{label}")),
                    Term::Literal { .. } => {
                        return Err(format!(
                            "wf_demote_tree: rdf:rest points to literal at {node}"
                        ));
                    }
                },
                _ => (),
            }
        }
        if let Some(v) = first_val {
            items.push(v);
        }
        cursor = rest_iri;
        first_iter = false;
    }
    Ok(serde_json::Value::Array(items))
}

fn literal_to_json(label: &str, datatype: &str, lang: Option<&str>) -> serde_json::Value {
    if lang.is_some() {
        return serde_json::Value::String(label.to_string());
    }
    match datatype {
        "http://www.w3.org/2001/XMLSchema#integer"
        | "http://www.w3.org/2001/XMLSchema#int"
        | "http://www.w3.org/2001/XMLSchema#long"
        | "http://www.w3.org/2001/XMLSchema#short"
        | "http://www.w3.org/2001/XMLSchema#byte" => label
            .parse::<i64>()
            .map(|n| serde_json::json!(n))
            .unwrap_or_else(|_| serde_json::Value::String(label.into())),
        "http://www.w3.org/2001/XMLSchema#decimal"
        | "http://www.w3.org/2001/XMLSchema#double"
        | "http://www.w3.org/2001/XMLSchema#float" => label
            .parse::<f64>()
            .map(|f| serde_json::json!(f))
            .unwrap_or_else(|_| serde_json::Value::String(label.into())),
        "http://www.w3.org/2001/XMLSchema#boolean" => match label {
            "true" | "1" => serde_json::Value::Bool(true),
            "false" | "0" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(label.into()),
        },
        _ => serde_json::Value::String(label.into()),
    }
}

fn local_name(predicate: &str) -> String {
    if predicate == RDF_TYPE {
        return "@type".into();
    }
    if let Some(pos) = predicate.rfind('#') {
        let s = &predicate[pos + 1..];
        if !s.is_empty() {
            return s.into();
        }
    }
    if let Some(pos) = predicate.rfind('/') {
        let s = &predicate[pos + 1..];
        if !s.is_empty() {
            return s.into();
        }
    }
    predicate.into()
}

fn display_id(subject: &str) -> String {
    if let Some(label) = subject.strip_prefix("_:") {
        mint_genid(label)
    } else {
        subject.to_string()
    }
}

/// Deterministic per-label genid IRI. Matches the encoding used by
/// wf_skolemize so that pre- and post-demote skolemization line up.
fn mint_genid(label: &str) -> String {
    let mut hash: u64 = GENID_SALT;
    for byte in label.bytes() {
        hash = hash.wrapping_mul(0x100000001B3).wrapping_add(byte as u64);
    }
    let mut hash2: u64 = hash.rotate_left(23) ^ 0x428A2F98D728AE22;
    for byte in label.bytes() {
        hash2 = hash2.wrapping_mul(0x100000001B3).wrapping_add(byte as u64);
    }
    format!("{GENID_PREFIX}{hash:016x}{hash2:016x}")
}

// ---------------------------------------------------------------------------
// Guest impls
// ---------------------------------------------------------------------------

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "urn:webfunction:demote-tree".into(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "urn:webfunction:demote-tree" => demote_tree_impl(&args),
            other => Err(format!("wf_demote_tree: unknown function '{other}'")),
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
            "wf_demote_tree: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_demote_tree: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_demote_tree: aggregate state was never constructed".into())
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
            "wf_demote_tree: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

// ---------------------------------------------------------------------------
// Tests (pure-data, target-independent)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_default() -> Config {
        Config {
            name: None,
            root_selector: String::new(),
            sink: String::new(),
            delete_source: false,
            reject_non_tree: true,
            collapse_rdf_lists: true,
            include_id: true,
        }
    }

    #[test]
    fn rdf_list_two_items_collapses_to_json_array() {
        let mut edges = EdgeMap::default();
        edges.add_edge(
            "http://ex/r",
            RDF_FIRST,
            Term::Literal {
                label: "a".into(),
                datatype: "http://www.w3.org/2001/XMLSchema#string".into(),
                lang: None,
            },
        );
        edges.add_edge(
            "http://ex/r",
            RDF_REST,
            Term::Bnode("b1".into()),
        );
        edges.add_edge(
            "_:b1",
            RDF_FIRST,
            Term::Literal {
                label: "b".into(),
                datatype: "http://www.w3.org/2001/XMLSchema#string".into(),
                lang: None,
            },
        );
        edges.add_edge(
            "_:b1",
            RDF_REST,
            Term::Iri(RDF_NIL.into()),
        );

        let cfg = cfg_default();
        let mut nodes = 0u64;
        let doc = serialize_tree(&edges, "http://ex/r", &cfg, &mut nodes).unwrap();
        assert_eq!(doc, serde_json::json!(["a", "b"]));
        assert_eq!(nodes, 2);
    }

    #[test]
    fn nested_object_tree_produces_grouped_json() {
        let mut edges = EdgeMap::default();
        edges.add_edge(
            "http://ex/r",
            "http://schema.org/name",
            Term::Literal {
                label: "Recipe".into(),
                datatype: "http://www.w3.org/2001/XMLSchema#string".into(),
                lang: None,
            },
        );
        edges.add_edge(
            "http://ex/r",
            RDF_TYPE,
            Term::Iri("http://schema.org/Recipe".into()),
        );
        edges.add_edge(
            "http://ex/r",
            "http://schema.org/step",
            Term::Iri("http://ex/s1".into()),
        );
        edges.add_edge(
            "http://ex/s1",
            "http://schema.org/text",
            Term::Literal {
                label: "Chop onions".into(),
                datatype: "http://www.w3.org/2001/XMLSchema#string".into(),
                lang: None,
            },
        );

        let cfg = cfg_default();
        let mut nodes = 0u64;
        let doc = serialize_tree(&edges, "http://ex/r", &cfg, &mut nodes).unwrap();
        assert_eq!(
            doc,
            serde_json::json!({
                "@id": "http://ex/r",
                "@type": "http://schema.org/Recipe",
                "name": "Recipe",
                "step": {
                    "@id": "http://ex/s1",
                    "text": "Chop onions"
                }
            })
        );
        assert_eq!(nodes, 2);
    }

    #[test]
    fn dag_shape_is_rejected_by_is_tree() {
        let mut edges = EdgeMap::default();
        edges.add_edge(
            "http://ex/r",
            "http://ex/left",
            Term::Iri("http://ex/child".into()),
        );
        edges.add_edge(
            "http://ex/r",
            "http://ex/right",
            Term::Iri("http://ex/mid".into()),
        );
        edges.add_edge(
            "http://ex/mid",
            "http://ex/target",
            Term::Iri("http://ex/child".into()),
        );
        edges.subjects.insert("http://ex/child".into());
        assert!(!is_tree(&edges, "http://ex/r"));
    }

    #[test]
    fn empty_rdf_list_collapses_to_empty_array() {
        let mut edges = EdgeMap::default();
        edges.add_edge(
            "http://ex/r",
            "http://ex/items",
            Term::Iri(RDF_NIL.into()),
        );
        let cfg = cfg_default();
        let mut nodes = 0u64;
        let doc = serialize_tree(&edges, "http://ex/r", &cfg, &mut nodes).unwrap();
        assert_eq!(
            doc,
            serde_json::json!({
                "@id": "http://ex/r",
                "items": []
            })
        );
    }

    #[test]
    fn multi_valued_predicate_produces_array() {
        let mut edges = EdgeMap::default();
        for label in ["a", "b", "c"] {
            edges.add_edge(
                "http://ex/r",
                "http://ex/tag",
                Term::Literal {
                    label: label.into(),
                    datatype: "http://www.w3.org/2001/XMLSchema#string".into(),
                    lang: None,
                },
            );
        }
        let cfg = cfg_default();
        let mut nodes = 0u64;
        let doc = serialize_tree(&edges, "http://ex/r", &cfg, &mut nodes).unwrap();
        assert_eq!(
            doc,
            serde_json::json!({
                "@id": "http://ex/r",
                "tag": ["a", "b", "c"]
            })
        );
    }

    #[test]
    fn genid_is_deterministic() {
        let a1 = mint_genid("x1");
        let a2 = mint_genid("x1");
        assert_eq!(a1, a2);
        assert!(a1.starts_with(GENID_PREFIX));
        let suffix = a1.strip_prefix(GENID_PREFIX).unwrap();
        assert_eq!(suffix.len(), 32);
    }
}
