//! wf_materialize_tree — subtree-assembling materializer for
//! shape=tree, writing to a document sink.
//!
//! Signature: `<urn:webfunction:materialize-tree>("<descriptor-json>")`
//! returns an rdf:JSON literal shaped as `{"trees": T, "nodes": N}`
//! (single-term collapse per the ExtensionGuest convention).
//!
//! For a `shape=tree` descriptor with `parent_link` / `child_link`
//! columns and attribute columns (label, etc), walks each anchor
//! subject's tree and posts a JSON document per root to a document
//! sink. The document mirrors the tree shape:
//!
//! ```json
//! { "id": "<iri>", "label": "…", "children": [ { ... }, … ] }
//! ```
//!
//! Attribute-role columns other than the anchor's parent/child links
//! become top-level fields on each node. Cycles are cut off with a
//! visited-set; a self-reference marker is emitted at the second
//! visit.
//!
//! Migration deviations (M1 Q3, ExtensionGuest wave):
//!
//!   * Descriptor's `sink` field is now a substrate-side document-sink
//!     NAME (not a driver URL). Sinks must be pre-registered;
//!     `sink-callbacks::list-sinks` validates presence.
//!   * The Stardog-era `sink-execute("INSERT DOC", <json-lit>)` shape
//!     retires in favor of typed
//!     `document-sink-callbacks::put-document(sink-name,
//!     {key: <root-iri>, content: <json>})`. The document key is the
//!     root IRI; the content is the serialized JSON. The sink adapter
//!     owns whatever storage projection the backend uses (Sirix
//!     collection, SQLite blob column, object-store bucket).

#[allow(warnings)]
mod bindings;

use std::collections::HashSet;

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
    Binding as WitBinding, Literal as WitLiteral, Term as WitTerm,
};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const MAX_DEPTH: usize = 4096;

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    shape: String,
    anchor: Anchor,
    columns: Vec<Column>,
    sink: Option<String>,
}

#[derive(Deserialize)]
struct Anchor {
    class: Option<String>,
    #[allow(dead_code)]
    predicate_signature: Option<Vec<String>>,
}

#[derive(Deserialize, Clone)]
struct Column {
    name: String,
    role: String,
    predicate: Option<String>,
    #[serde(default = "default_type")]
    r#type: String,
    #[allow(dead_code)]
    #[serde(default)]
    cardinality: Option<String>,
}

fn default_type() -> String {
    "string".into()
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
            "wf_materialize_tree: sink `{name}` not registered with host \
             (list-sinks returned {} sinks)",
            sinks.len()
        ))
    }
}

fn split_rows(flat: Vec<WitBinding>) -> Vec<Vec<WitBinding>> {
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
    rows
}

fn column_predicate(columns: &[Column], role: &str) -> Option<String> {
    columns
        .iter()
        .find(|c| c.role == role)
        .and_then(|c| c.predicate.clone())
}

fn term_iri(t: &WitTerm) -> Option<String> {
    match t {
        WitTerm::NamedNode(s) => Some(s.clone()),
        WitTerm::BlankNode(s) => Some(format!("_:{s}")),
        _ => None,
    }
}

fn value_to_json(v: &WitTerm, ty: &str) -> serde_json::Value {
    match v {
        WitTerm::NamedNode(s) => serde_json::Value::String(s.clone()),
        WitTerm::BlankNode(s) => serde_json::Value::String(format!("_:{s}")),
        WitTerm::Literal(l) => match ty {
            "integer" => l
                .value
                .parse::<i64>()
                .map(|n| serde_json::json!(n))
                .unwrap_or_else(|_| serde_json::Value::String(l.value.clone())),
            "decimal" => l
                .value
                .parse::<f64>()
                .map(|f| serde_json::json!(f))
                .unwrap_or_else(|_| serde_json::Value::String(l.value.clone())),
            "boolean" => match l.value.as_str() {
                "true" | "1" => serde_json::Value::Bool(true),
                "false" | "0" => serde_json::Value::Bool(false),
                other => serde_json::Value::String(other.into()),
            },
            _ => serde_json::Value::String(l.value.clone()),
        },
        WitTerm::Triple(_) => serde_json::Value::String("<<quoted triple>>".into()),
    }
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

fn enumerate_roots(
    anchor: &Anchor,
    parent_predicate: Option<&str>,
) -> Result<Vec<String>, String> {
    let class = anchor
        .class
        .as_deref()
        .ok_or_else(|| {
            "wf_materialize_tree: anchor.class required (predicate_signature not yet supported)"
                .to_string()
        })?;
    let sparql = match parent_predicate {
        Some(p) => format!(
            "SELECT DISTINCT ?s WHERE {{ ?s a <{class}> FILTER NOT EXISTS {{ ?s <{p}> ?anyparent }} }}"
        ),
        None => format!("SELECT DISTINCT ?s WHERE {{ ?s a <{class}> }}"),
    };
    let result = gc::execute_query(&sparql).map_err(map_graph_err)?;
    let flat = match result {
        CallbackQueryResult::Bindings(bs) => bs,
        _ => {
            return Err(
                "wf_materialize_tree: root query must yield bindings, not CONSTRUCT/ASK".into(),
            );
        }
    };
    let mut out = Vec::new();
    for b in flat {
        if b.variable != "s" {
            continue;
        }
        if let WitTerm::NamedNode(iri) = &b.value {
            out.push(iri.clone());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Subtree assembly
// ---------------------------------------------------------------------------

fn build_subtree(
    subject: &str,
    attribute_columns: &[Column],
    child_predicate: Option<&str>,
    parent_predicate: Option<&str>,
    visited: &mut HashSet<String>,
    depth: usize,
    nodes_counter: &mut u64,
) -> Result<serde_json::Value, String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "wf_materialize_tree: max recursion depth exceeded at {subject}"
        ));
    }
    if !visited.insert(subject.to_string()) {
        return Ok(serde_json::json!({ "id": subject, "cycle": true }));
    }
    *nodes_counter += 1;

    let mut node = serde_json::Map::new();
    node.insert("id".into(), serde_json::Value::String(subject.to_string()));

    // Attribute columns: one SELECT ... LIMIT 1 per column per subject.
    for col in attribute_columns {
        let predicate = match &col.predicate {
            Some(p) => p,
            None => continue,
        };
        let value_sparql = format!(
            "SELECT ?o WHERE {{ <{subject}> <{predicate}> ?o }} LIMIT 1"
        );
        let result = gc::execute_query(&value_sparql).map_err(map_graph_err)?;
        let flat = match result {
            CallbackQueryResult::Bindings(bs) => bs,
            _ => continue,
        };
        if let Some(v) = flat.first().map(|b| &b.value) {
            node.insert(col.name.clone(), value_to_json(v, &col.r#type));
        }
    }

    // Walk children. Prefer explicit child_predicate; fall back to
    // inverse of parent_predicate if only that direction is asserted.
    let children_query = match (child_predicate, parent_predicate) {
        (Some(cp), _) => format!("SELECT ?c WHERE {{ <{subject}> <{cp}> ?c }}"),
        (None, Some(pp)) => format!("SELECT ?c WHERE {{ ?c <{pp}> <{subject}> }}"),
        (None, None) => String::new(),
    };
    if !children_query.is_empty() {
        let result = gc::execute_query(&children_query).map_err(map_graph_err)?;
        let flat = match result {
            CallbackQueryResult::Bindings(bs) => bs,
            _ => Vec::new(),
        };
        let child_rows = split_rows(flat);
        let mut children: Vec<serde_json::Value> = Vec::with_capacity(child_rows.len());
        for row in child_rows {
            if let Some(child_iri) = row
                .iter()
                .find(|b| b.variable == "c")
                .and_then(|b| term_iri(&b.value))
            {
                let sub = build_subtree(
                    &child_iri,
                    attribute_columns,
                    child_predicate,
                    parent_predicate,
                    visited,
                    depth + 1,
                    nodes_counter,
                )?;
                children.push(sub);
            }
        }
        if !children.is_empty() {
            node.insert("children".into(), serde_json::Value::Array(children));
        }
    }

    Ok(serde_json::Value::Object(node))
}

// ---------------------------------------------------------------------------
// Guest entrypoint
// ---------------------------------------------------------------------------

fn materialize_tree_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let descriptor_json = match args.first() {
        Some(WitTerm::Literal(l)) => l.value.clone(),
        Some(other) => {
            return Err(format!(
                "wf_materialize_tree: first arg must be a descriptor-json string literal, got {other:?}"
            ));
        }
        None => return Err("wf_materialize_tree: expected one arg (descriptor json)".into()),
    };
    let d: Descriptor = serde_json::from_str(&descriptor_json)
        .map_err(|e| format!("wf_materialize_tree: descriptor parse: {e}"))?;
    if d.shape != "tree" {
        return Err(format!(
            "wf_materialize_tree: descriptor shape must be `tree`, got `{}`",
            d.shape
        ));
    }
    let sink_name = d
        .sink
        .as_deref()
        .ok_or_else(|| "wf_materialize_tree: descriptor has no `sink`".to_string())?;
    validate_sink_present(sink_name)?;

    let parent_predicate = column_predicate(&d.columns, "parent_link");
    let child_predicate = column_predicate(&d.columns, "child_link");
    let attribute_columns: Vec<Column> = d
        .columns
        .iter()
        .filter(|c| c.role == "attribute")
        .cloned()
        .collect();

    let roots = enumerate_roots(&d.anchor, parent_predicate.as_deref())?;

    let mut trees = 0u64;
    let mut nodes = 0u64;
    for root in &roots {
        let mut visited: HashSet<String> = HashSet::new();
        let doc_value = build_subtree(
            root,
            &attribute_columns,
            child_predicate.as_deref(),
            parent_predicate.as_deref(),
            &mut visited,
            0,
            &mut nodes,
        )?;
        let json_content = doc_value.to_string();
        ds::put_document(
            sink_name,
            &Document {
                key: WitTerm::NamedNode(root.clone()),
                content: json_content,
            },
        )
        .map_err(|e| {
            format!(
                "wf_materialize_tree: put-document for root `{root}`: {}",
                map_doc_err(e)
            )
        })?;
        trees += 1;
    }

    let out = json!({
        "trees": trees,
        "nodes": nodes,
    });
    Ok(json_literal(&out.to_string()))
}

// ---------------------------------------------------------------------------
// Guest impls
// ---------------------------------------------------------------------------

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "urn:webfunction:materialize-tree".into(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "urn:webfunction:materialize-tree" => materialize_tree_impl(&args),
            other => Err(format!("wf_materialize_tree: unknown function '{other}'")),
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
            "wf_materialize_tree: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_materialize_tree: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_materialize_tree: aggregate state was never constructed".into())
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
            "wf_materialize_tree: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
