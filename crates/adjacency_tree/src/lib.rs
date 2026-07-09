//! adjacency_tree — recursive tree walker that emits an edge list.
//!
//! `wf:adjacency_tree(root, sparql_query [, max_depth])` walks the graph the
//! same way `wf_tree_rows` does — `?this` is re-bound to each newly discovered
//! node, `?child` names the next hop — but instead of one row per visited
//! node, this crate emits one row per parent -> child edge, projected as two
//! typed IRI columns (`source`, `target`).
//!
//! The relationship to `wf_tree_rows` is intentional: same recursion driver,
//! same cycle detection, same fallback child variable, same host-side
//! `prepare-query` amortisation. Only the output projection differs. That
//! makes the pair symmetric — consumers who want node attributes bind
//! `wf_tree_rows`; consumers who want to hand the graph to a downstream edge
//! consumer (Cypher-shaped tools, network-analysis code, path solvers) bind
//! `adjacency_tree`.
//!
//! Order is depth-first, matching `wf_tree_rows`, so a caller that runs both
//! walkers over the same graph and root sees identical traversal order.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::collections::HashSet;

struct Component;

const CHILD_VAR: &str = "child";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

// Soft depth cap: the host defaults to a hard cap of 100 callback frames,
// so 90 leaves room for the guest to unwind cleanly before the host errors.
const DEFAULT_MAX_DEPTH: u32 = 90;

// Per-recursion row limit passed to run-prepared. Matches wf_tree_rows so
// the two crates share the same "at most 1000 children per node" assumption.
const CHILD_ROW_LIMIT: u32 = 1_000;

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn value_as_string(v: &Value) -> String {
    match v {
        Value::Iri(uri) => uri.clone(),
        Value::Bnode(id) => format!("_:{}", id),
        Value::Literal(l) => l.label.clone(),
    }
}

fn string_arg(v: &Value, name: &str) -> Result<String, String> {
    match v {
        Value::Literal(l) => Ok(l.label.clone()),
        _ => Err(format!("wf:adjacency_tree: `{}` argument must be a string literal", name)),
    }
}

fn u32_arg(v: &Value, name: &str) -> Result<u32, String> {
    match v {
        Value::Literal(l) => l
            .label
            .parse::<u32>()
            .map_err(|_| format!("wf:adjacency_tree: `{}` must be a non-negative integer", name)),
        _ => Err(format!("wf:adjacency_tree: `{}` argument must be an integer literal", name)),
    }
}

/// One directed edge (source, target). Both are stored as `Value` so we can
/// carry IRIs, bnodes, or the odd literal-shaped child through without
/// re-marshalling at emission time.
struct Edge {
    source: Value,
    target: Value,
}

/// Depth-first walk. Every time the child-lookup query returns a `?child`
/// binding for the current node, we push (current, child) to `out` and
/// recurse into `child`. The root itself is never emitted as a source with
/// no target — a root that has no children yields an empty edge list.
///
/// Cycles are guarded with a `HashSet<String>` keyed on the node's canonical
/// string form, matching `wf_tree_rows`. When we re-encounter a node, we
/// simply stop descending; the edge that led here is already emitted, so the
/// output retains the discovery-first arc without producing a duplicate walk.
fn walk(
    node: &Value,
    depth: u32,
    query_handle: u32,
    max_depth: u32,
    seen: &mut HashSet<String>,
    out: &mut Vec<Edge>,
) {
    let node_key = value_as_string(node);

    // Cycle: we've walked out of this node before; don't re-descend.
    if !seen.insert(node_key.clone()) {
        return;
    }

    // Bail before recursing if the next level would exceed our cap. The host
    // also enforces its own callback-depth cap; exiting here first keeps the
    // walk cleanly bounded instead of tripping the host error path.
    if depth >= max_depth || host::callback_depth() >= DEFAULT_MAX_DEPTH {
        seen.remove(&node_key);
        return;
    }

    let bindings = vec![Binding {
        name: "this".into(),
        value: node.clone(),
    }];

    let rows = match host::run_prepared(query_handle, &bindings, Some(CHILD_ROW_LIMIT)) {
        Ok(bs) => bs,
        // A query failure at one node shouldn't wipe the whole walk. Drop
        // the descent and let the caller see whatever edges we've already
        // accumulated — same policy as wf_tree_rows.
        Err(_) => {
            seen.remove(&node_key);
            return;
        }
    };

    for row in &rows.rows {
        if let Some(child) = row.iter().find(|b| b.name == CHILD_VAR) {
            // Emit the edge first, then recurse. This ordering guarantees
            // that a parent's out-edges are contiguous in output order and
            // that the root's edges lead the list.
            out.push(Edge {
                source: node.clone(),
                target: child.value.clone(),
            });
            walk(&child.value, depth + 1, query_handle, max_depth, seen, out);
        }
    }

    seen.remove(&node_key);
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        // Args:
        //   0: root node (any Value)
        //   1: SPARQL query as a string literal, binding ?child per row
        //   2 (optional): max recursion depth (integer literal, default 90)
        if args.len() < 2 || args.len() > 3 {
            return Err(format!(
                "wf:adjacency_tree: expected 2 or 3 args (root, query, [max_depth]), got {}",
                args.len()
            ));
        }
        let root = args[0].clone();
        let query = string_arg(&args[1], "query")?;
        let max_depth = if args.len() == 3 {
            u32_arg(&args[2], "max_depth")?
        } else {
            DEFAULT_MAX_DEPTH
        };

        // Amortise SPARQL parse + plan compile across every recursion step
        // (same optimisation as wf_tree_rows).
        let query_handle = host::prepare_query(&query)?;

        let mut seen: HashSet<String> = HashSet::new();
        let mut edges: Vec<Edge> = Vec::new();
        walk(&root, 0, query_handle, max_depth, &mut seen, &mut edges);

        let wire_rows: Vec<Vec<Binding>> = edges
            .into_iter()
            .map(|e| {
                vec![
                    Binding { name: "source".into(), value: e.source },
                    Binding { name: "target".into(), value: e.target },
                ]
            })
            .collect();

        Ok(BindingSets {
            vars: vec!["source".into(), "target".into()],
            rows: wire_rows,
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf:adjacency_tree: aggregate not applicable".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf:adjacency_tree: aggregate not applicable".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        // We can't cheaply predict edge count without walking the tree, so
        // publish a rough hint at `Injected` accuracy — planner treats it as
        // a suggestion, not a promise.
        Ok(Cardinality { value: 100.0, accuracy: Accuracy::Injected })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "wf:adjacency_tree(root, sparql_query [, max_depth=90]) -> binding-sets \
                     { vars: [source, target], rows }. Depth-first recursive walk from \
                     `root`; `sparql_query` must bind `?child` per row and re-uses `?this` \
                     as the current node's IRI at each level. Emits one row per parent-> \
                     child edge discovered. Cycle-safe."),
            }]],
        }
    }
}

export!(Component);
