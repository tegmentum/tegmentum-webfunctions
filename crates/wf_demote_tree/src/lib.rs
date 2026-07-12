//! wf_demote_tree — detect tree-shaped RDF subgraphs and shuttle them out
//! as JSON documents.
//!
//! Signature: `wf:call(<wf_demote_tree.wasm>, "<config-json>")`
//!    → binding-set { trees: xsd:integer, nodes: xsd:integer,
//!                    skipped: xsd:integer, deleted: xsd:integer }
//!
//! Contract:
//!   * Reads a config that names a `root_selector` (SPARQL WHERE pattern
//!     projecting `?r`), a `sink` URL, and an optional resource name.
//!   * Enumerates root IRIs matching the selector.
//!   * For each root, walks outgoing edges depth-first into a JSON
//!     document. The archetypal RDF list (`rdf:first` / `rdf:rest` /
//!     `rdf:nil`) collapses to a JSON array; every other subject
//!     becomes a JSON object keyed by predicate local-name.
//!   * Enforces the tree property: a subject reached during the walk
//!     may not have more than one in-edge from within the collected
//!     set. If it does, that's a DAG, and the guest either skips that
//!     root (default) or fails per config.
//!   * Blank nodes are skolemized on the way out — Sirix drops
//!     blank-node identity across the boundary, so we mint a stable
//!     genid IRI per bnode so the demoted document can round-trip.
//!   * Emits each document via the sink's `INSERT DOC` sentinel (v0.5
//!     WIT). Reference sinks: `jsonl://` (write stub), `sirix://` (real
//!     Sirix via companion `sirix-import` CLI reading the JSONL).
//!   * When `delete_source` is set, DELETEs every collected triple
//!     from the source graph so the tree is genuinely demoted, not
//!     duplicated.
//!
//! Rationale: this is the tree counterpart of `wf_demote`, which
//! handles shape-relational demotion into SQLite. Together they
//! implement the "convert relational shapes to SQL, tree shapes to
//! JSON documents" split from the substrate design.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

#[allow(dead_code)]
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
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
    /// Sink URL. Any scheme the host recognizes with an `INSERT DOC`
    /// verb — `jsonl://` (write stub) or `sirix://` (via companion
    /// import).
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
    /// bnode-rooted trees) on every node. Defaults true — Sirix loses
    /// subject identity otherwise.
    #[serde(default = "default_true")]
    pub include_id: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Guest impl
// ---------------------------------------------------------------------------

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let config_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_demote_tree: first arg must be a config-json string literal".into(),
                );
            }
        };
        let cfg: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("wf_demote_tree: config parse: {e}"))?;

        #[cfg(target_arch = "wasm32")]
        {
            evaluate_impl(cfg)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = cfg;
            Err("wf_demote_tree: host imports are not available on the native target".into())
        }
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_demote_tree: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_demote_tree: aggregate not applicable".into())
    }
    fn cardinality_estimate(
        _input: Cardinality,
        _args: Vec<Value>,
    ) -> Result<Cardinality, String> {
        Ok(Cardinality {
            value: 1.0,
            accuracy: Accuracy::Injected,
        })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: Value::Literal(Literal {
                    label: "wf_demote_tree(\"<config-json>\") — walk each \
                            root's tree, collapse rdf:List into JSON arrays, \
                            reject non-tree DAGs, emit JSON per root to the \
                            sink, optionally DELETE source triples."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Wasm-only evaluation path
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
fn evaluate_impl(cfg: Config) -> Result<BindingSets, String> {
    let roots = enumerate_roots(&cfg.root_selector)?;

    let sink_handle = host::sink_open(&cfg.sink)?;
    let mut trees = 0u64;
    let mut nodes = 0u64;
    let mut skipped = 0u64;
    let mut all_collected: HashSet<String> = HashSet::new();

    for root in &roots {
        let mut edges = EdgeMap::default();
        // CONSTRUCT the entire reachable subgraph in one round-trip so
        // we can decide tree-ness holistically before serializing.
        collect_subgraph_wasm(root, &mut edges)?;
        if !is_tree(&edges, root) {
            if cfg.reject_non_tree {
                skipped += 1;
                continue;
            }
        }
        let mut node_count = 0u64;
        let doc = serialize_tree(&edges, root, &cfg, &mut node_count)?;
        nodes += node_count;

        let json_line = serde_json::to_string(&doc)
            .map_err(|e| format!("wf_demote_tree: serialize `{root}`: {e}"))?;
        host::sink_execute(
            sink_handle,
            "INSERT DOC",
            &[Value::Literal(Literal {
                label: json_line,
                datatype: XSD_STRING.into(),
                lang: None,
            })],
        )
        .map_err(|e| format!("wf_demote_tree: sink write for `{root}`: {e}"))?;
        trees += 1;
        all_collected.extend(edges.subjects.iter().cloned());
    }
    host::sink_close(sink_handle).ok();

    let mut deleted = 0u64;
    if cfg.delete_source && !all_collected.is_empty() {
        deleted = delete_collected(&all_collected)?;
    }

    Ok(BindingSets {
        vars: vec![
            "trees".into(),
            "nodes".into(),
            "skipped".into(),
            "deleted".into(),
        ],
        rows: vec![vec![
            Binding {
                name: "trees".into(),
                value: int_lit(trees as i64),
            },
            Binding {
                name: "nodes".into(),
                value: int_lit(nodes as i64),
            },
            Binding {
                name: "skipped".into(),
                value: int_lit(skipped as i64),
            },
            Binding {
                name: "deleted".into(),
                value: int_lit(deleted as i64),
            },
        ]],
    })
}

#[cfg(target_arch = "wasm32")]
fn enumerate_roots(root_selector: &str) -> Result<Vec<String>, String> {
    let sparql = format!("SELECT DISTINCT ?r WHERE {{ {root_selector} }}");
    let bs = host::execute_query(&sparql, &[], None)?;
    let mut out = Vec::with_capacity(bs.rows.len());
    for row in &bs.rows {
        if let Some(iri) = row.first().and_then(|b| match &b.value {
            Value::Iri(s) => Some(s.clone()),
            Value::Bnode(b) => Some(format!("_:{b}")),
            _ => None,
        }) {
            out.push(iri);
        }
    }
    Ok(out)
}

/// Collect the entire subgraph reachable from `root` in a single CONSTRUCT
/// so that blank-node identity is preserved on the wire (SPARQL Update
/// text can't reference specific bnodes, and re-querying by label
/// doesn't survive engines that canonicalize labels per query — the
/// property-path closure walks the actual graph from the root and gives
/// us every triple in one shot).
///
/// Requires an IRI root. Bnode-rooted trees need to be skolemized first
/// (via `wf_skolemize`) — Sirix drops bnode identity across the sink
/// boundary anyway, so demoting a bnode-rooted tree without
/// skolemization would round-trip to a different graph on ingest.
#[cfg(target_arch = "wasm32")]
fn collect_subgraph_wasm(
    root: &str,
    edges: &mut EdgeMap,
) -> Result<(), String> {
    if root.starts_with("_:") {
        return Err(format!(
            "wf_demote_tree: bnode-rooted tree at {root} — skolemize \
             upstream (wf_skolemize) before demoting; Sirix does not \
             preserve bnode identity across the sink"
        ));
    }
    // Property-path closure `(!<no:such>)*` matches any predicate, zero
    // or more times, so `?s` ranges over every node reachable outward
    // from `<root>`. `?s ?p ?o` then materializes every outgoing edge
    // of every reachable subject.
    let sparql = format!(
        "CONSTRUCT {{ ?s ?p ?o }} WHERE {{ \
         <{root}> (!<urn:tegmentum:no-such-predicate>)* ?s . \
         ?s ?p ?o \
        }}"
    );
    let bs = host::execute_query(&sparql, &[], None)?;
    edges.subjects.insert(root.to_string());
    for row in &bs.rows {
        let s = row.iter().find(|b| b.name == "s").map(|b| b.value.clone());
        let p = row.iter().find(|b| b.name == "p").map(|b| b.value.clone());
        let o = row.iter().find(|b| b.name == "o").map(|b| b.value.clone());
        let (Some(s), Some(p), Some(o)) = (s, p, o) else {
            continue;
        };
        let predicate = match &p {
            Value::Iri(name) => name.clone(),
            other => {
                return Err(format!("wf_demote_tree: non-IRI predicate: {other:?}"));
            }
        };
        let subject_key = match &s {
            Value::Iri(iri) => iri.clone(),
            Value::Bnode(label) => format!("_:{label}"),
            other => {
                return Err(format!("wf_demote_tree: non-node subject: {other:?}"));
            }
        };
        let object_term = wit_to_term(&o);
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

#[cfg(target_arch = "wasm32")]
fn delete_collected(subjects: &HashSet<String>) -> Result<u64, String> {
    // Two passes: outgoing (subject is the collected set) and incoming
    // (object is the collected set). We can't reference blank-node
    // labels directly in SPARQL Update text (SPARQL 1.1 disallows that),
    // so anything blank is filtered via `str(?s) = "..."`.
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
        host::execute_update(&update).map_err(|e| {
            format!("wf_demote_tree: delete outgoing for `{subj}`: {e}")
        })?;
        deleted += 1;
    }
    Ok(deleted)
}

#[cfg(target_arch = "wasm32")]
fn wit_to_term(v: &Value) -> Term {
    match v {
        Value::Iri(s) => Term::Iri(s.clone()),
        Value::Bnode(s) => Term::Bnode(s.clone()),
        Value::Literal(l) => Term::Literal {
            label: l.label.clone(),
            datatype: l.datatype.clone(),
            lang: l.lang.clone(),
        },
    }
}

#[cfg(target_arch = "wasm32")]
fn int_lit(n: i64) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
}

// ---------------------------------------------------------------------------
// Pure data structures and tree logic (used by wasm evaluate and tests)
// ---------------------------------------------------------------------------

/// Simple term representation, target-independent. Kept separate from the
/// wit-bindgen `Value` type so unit tests don't need to instantiate host-
/// generated bindings.
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

/// Directed edge map used during subgraph collection. `subjects` is the
/// full set of collected subjects; `out` is subject → list of (predicate,
/// object); `in_count` counts incoming references per term key so we can
/// enforce the tree property.
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
/// collected set. The root itself is allowed zero incoming edges (or
/// one, if the caller elected to include the parent stub — we ignore
/// the root's in-count).
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
        // Second visit — the tree check should have rejected this, but
        // if reject_non_tree=false the caller wants us to keep going.
        // Emit a reference stub so the doc is still well-formed.
        return Ok(serde_json::json!({ "$ref": display_id(subject) }));
    }

    // RDF-list collapse: a subject is a list head when it has rdf:first
    // + rdf:rest edges (and no other edges except rdf:type=rdf:List
    // decoration, which we tolerate). Node counting is delegated to
    // `serialize_list` so we don't double-count the head cons cell.
    if cfg.collapse_rdf_lists && is_list_head(edges, subject) {
        return serialize_list(edges, subject, cfg, visited, depth, node_count);
    }

    *node_count += 1;

    let mut obj = serde_json::Map::new();
    if cfg.include_id {
        obj.insert("@id".into(), serde_json::Value::String(display_id(subject)));
    }
    // Predicate grouping: RDF predicates can repeat; collapse into JSON
    // arrays keyed by predicate local-name. Sorted for determinism —
    // Sirix shredding order matters for revision keys.
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
                // Bnode with no outgoing edges we know about. Skolemize
                // into a genid IRI so the reference survives Sirix.
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
    // Head was already inserted into `visited` by the caller. Each
    // subsequent cons cell we visit needs to be recorded so a cycle
    // terminates the walk instead of stack-overflowing.
    while let Some(node) = cursor {
        if node == RDF_NIL {
            break;
        }
        // Mark visited on second and later cons cells; head was inserted
        // by the caller.
        if !first_iter && !visited.insert(node.clone()) {
            // Cycle in the rdf:rest chain — abort with a `$cycle`
            // marker for this list, matching the reject_non_tree path
            // in the outer walker.
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
                _ => (), // ignore rdf:type=rdf:List decoration and any noise.
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
    // Prefer last '#', else last '/'. rdf:type is a special case — expose
    // it as `@type` so the JSON reads like JSON-LD.
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

export!(Component);

// ---------------------------------------------------------------------------
// Tests
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

    /// The archetypal test:
    ///
    ///   <r> rdf:first "a" ; rdf:rest [ rdf:first "b" ; rdf:rest rdf:nil ]
    ///
    /// demotes to the JSON array `["a", "b"]`.
    #[test]
    fn rdf_list_two_items_collapses_to_json_array() {
        let mut edges = EdgeMap::default();
        // <r> rdf:first "a" ; rdf:rest _:b1
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
        // _:b1 rdf:first "b" ; rdf:rest rdf:nil
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
        // Two cons cells traversed.
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
        // Two subjects both point at a shared child — that's a DAG, not
        // a tree.
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
    fn literal_types_project_to_typed_json() {
        let cases = [
            (
                Term::Literal {
                    label: "42".into(),
                    datatype: "http://www.w3.org/2001/XMLSchema#integer".into(),
                    lang: None,
                },
                serde_json::json!(42),
            ),
            (
                Term::Literal {
                    label: "true".into(),
                    datatype: "http://www.w3.org/2001/XMLSchema#boolean".into(),
                    lang: None,
                },
                serde_json::json!(true),
            ),
            (
                Term::Literal {
                    label: "hi".into(),
                    datatype: "http://www.w3.org/2001/XMLSchema#string".into(),
                    lang: None,
                },
                serde_json::json!("hi"),
            ),
        ];
        for (term, expected) in cases {
            let mut edges = EdgeMap::default();
            edges.add_edge("http://ex/r", "http://ex/p", term);
            let cfg = cfg_default();
            let mut nodes = 0u64;
            let doc = serialize_tree(&edges, "http://ex/r", &cfg, &mut nodes).unwrap();
            let p = doc.get("p").expect("p field");
            assert_eq!(p, &expected);
        }
    }

    #[test]
    fn genid_is_deterministic_and_matches_wf_skolemize_shape() {
        let a1 = mint_genid("x1");
        let a2 = mint_genid("x1");
        assert_eq!(a1, a2);
        assert!(a1.starts_with(GENID_PREFIX));
        // The suffix should be two 16-hex-char halves = 32 chars.
        let suffix = a1.strip_prefix(GENID_PREFIX).unwrap();
        assert_eq!(suffix.len(), 32);
    }
}
