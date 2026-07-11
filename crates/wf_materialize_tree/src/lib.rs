//! wf_materialize_tree — subtree-assembling materializer for shape=tree.
//!
//! Signature: `wf:call(<wf_materialize_tree.wasm>, "<descriptor-json>")`
//!    → binding-set { trees: xsd:integer, nodes: xsd:integer }
//!
//! For a `shape=tree` descriptor with `parent_link`/`child_link`
//! columns and attribute columns (label, etc), walks each anchor
//! subject's tree and posts a JSON document per root to the sink. The
//! document mirrors the tree shape:
//!
//! ```json
//! { "id": "<iri>", "label": "…", "children": [ { ... }, … ] }
//! ```
//!
//! Attribute-role columns other than the anchor's parent/child links
//! become top-level fields on each node. Cycles are cut off with a
//! visited-set (worst case: dedup with the ancestor's IRI).
//!
//! Sink: any URL with an `INSERT DOC` capable backend. The reference
//! plugin ships `jsonl://` and `sirix://` schemes (the latter is a
//! JSONL stub in v1; real SirixDB HTTP+XQuery drops in later).

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use std::collections::HashSet;

use serde::Deserialize;

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
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
// Guest impl
// ---------------------------------------------------------------------------

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let descriptor_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_materialize_tree: first arg must be a descriptor-json string literal"
                        .into(),
                );
            }
        };
        let d: Descriptor = serde_json::from_str(&descriptor_json)
            .map_err(|e| format!("wf_materialize_tree: descriptor parse: {e}"))?;
        if d.shape != "tree" {
            return Err(format!(
                "wf_materialize_tree: descriptor shape must be `tree`, got `{}`",
                d.shape
            ));
        }
        let sink_url = d
            .sink
            .as_deref()
            .ok_or_else(|| "wf_materialize_tree: descriptor has no `sink`".to_string())?;

        let parent_predicate = column_predicate(&d.columns, "parent_link");
        let child_predicate = column_predicate(&d.columns, "child_link");
        let attribute_columns: Vec<Column> = d
            .columns
            .iter()
            .filter(|c| c.role == "attribute")
            .cloned()
            .collect();

        // Enumerate anchor subjects. If we have parent_link, roots are
        // anchor subjects with no parent value. Otherwise every anchor
        // subject is treated as its own root — the descriptor lied
        // about being a tree, but at least the materializer terminates.
        let roots = enumerate_roots(&d.anchor, parent_predicate.as_deref())?;

        let handle = host::sink_open(sink_url)?;

        let mut trees = 0u64;
        let mut nodes = 0u64;
        for root in &roots {
            let mut visited: HashSet<String> = HashSet::new();
            let doc = build_subtree(
                root,
                &attribute_columns,
                child_predicate.as_deref(),
                parent_predicate.as_deref(),
                &mut visited,
                0,
                &mut nodes,
            )?;
            let json_line = doc.to_string();
            host::sink_execute(
                handle,
                "INSERT DOC",
                &[string_lit(&json_line)],
            )
            .map_err(|e| format!("wf_materialize_tree: sink write for root `{root}`: {e}"))?;
            trees += 1;
        }
        host::sink_close(handle).ok();

        Ok(BindingSets {
            vars: vec!["trees".into(), "nodes".into()],
            rows: vec![vec![
                Binding {
                    name: "trees".into(),
                    value: int_lit(trees as i64),
                },
                Binding {
                    name: "nodes".into(),
                    value: int_lit(nodes as i64),
                },
            ]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_materialize_tree: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_materialize_tree: aggregate not applicable".into())
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
                    label: "wf_materialize_tree(\"<descriptor-json>\") — walk \
                            each root's subtree via child_link, emit one JSON \
                            document per root to the sink."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

fn column_predicate(columns: &[Column], role: &str) -> Option<String> {
    columns.iter().find(|c| c.role == role).and_then(|c| c.predicate.clone())
}

fn enumerate_roots(anchor: &Anchor, parent_predicate: Option<&str>) -> Result<Vec<String>, String> {
    let class = anchor
        .class
        .as_deref()
        .ok_or_else(|| "wf_materialize_tree: anchor.class required (predicate_signature not yet supported)".to_string())?;
    let sparql = match parent_predicate {
        Some(p) => format!(
            "SELECT DISTINCT ?s WHERE {{ ?s a <{class}> FILTER NOT EXISTS {{ ?s <{p}> ?anyparent }} }}"
        ),
        None => format!("SELECT DISTINCT ?s WHERE {{ ?s a <{class}> }}"),
    };
    let bs = host::execute_query(&sparql, &[], None)?;
    let mut out = Vec::with_capacity(bs.rows.len());
    for row in &bs.rows {
        if let Some(iri) = row.first().and_then(|b| match &b.value {
            Value::Iri(s) => Some(s.clone()),
            _ => None,
        }) {
            out.push(iri);
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
        // Cycle — emit a self-reference stub. Cheaper than aborting;
        // the tree wasn't strictly a tree, but the demote still lands.
        return Ok(serde_json::json!({ "id": subject, "cycle": true }));
    }
    *nodes_counter += 1;

    let mut node = serde_json::Map::new();
    node.insert("id".into(), serde_json::Value::String(subject.to_string()));

    // Fetch attribute columns for this subject.
    for col in attribute_columns {
        let predicate = match &col.predicate {
            Some(p) => p,
            None => continue,
        };
        let value_sparql = format!(
            "SELECT ?o WHERE {{ <{subject}> <{predicate}> ?o }} LIMIT 1"
        );
        let bs = host::execute_query(&value_sparql, &[], Some(1))?;
        if let Some(v) = bs.rows.first().and_then(|r| r.first()).map(|b| &b.value) {
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
        let bs = host::execute_query(&children_query, &[], None)?;
        let mut children: Vec<serde_json::Value> = Vec::with_capacity(bs.rows.len());
        for row in &bs.rows {
            if let Some(child_iri) = row.first().and_then(|b| match &b.value {
                Value::Iri(s) => Some(s.clone()),
                _ => None,
            }) {
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

fn value_to_json(v: &Value, ty: &str) -> serde_json::Value {
    match v {
        Value::Iri(s) => serde_json::Value::String(s.clone()),
        Value::Bnode(s) => serde_json::Value::String(format!("_:{s}")),
        Value::Literal(l) => match ty {
            "integer" => l
                .label
                .parse::<i64>()
                .map(|n| serde_json::json!(n))
                .unwrap_or_else(|_| serde_json::Value::String(l.label.clone())),
            "decimal" => l
                .label
                .parse::<f64>()
                .and_then(|f| Ok(serde_json::json!(f)))
                .unwrap_or_else(|_| serde_json::Value::String(l.label.clone())),
            "boolean" => match l.label.as_str() {
                "true" | "1" => serde_json::Value::Bool(true),
                "false" | "0" => serde_json::Value::Bool(false),
                other => serde_json::Value::String(other.into()),
            },
            _ => serde_json::Value::String(l.label.clone()),
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn string_lit(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

fn int_lit(n: i64) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
}

export!(Component);
