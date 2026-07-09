//! wf_tree_rows — recursive tree walker that emits flat binding-set rows
//! rather than the nested JSON string that `wf_tree` produces.
//!
//! `wf:tree_rows(root, sparql_query [, max_depth])` runs the child-lookup
//! query the same way `wf_tree` does — `?this` is re-bound to each newly
//! discovered node — but instead of assembling a JSON tree, every visited
//! node is projected as one row in a `binding-sets { vars, rows }` result
//! with columns `("uri", "depth", "parent")`. Emission order is depth-first,
//! so consumers can render the tree with a stack; cycle detection matches
//! `wf_tree`'s HashSet<String> guard.
//!
//! The `child` variable of the child-lookup query is always what advances
//! the recursion, matching wf_tree's `DEFAULT_CHILD_VAR`. Sibling columns
//! (labels, types, etc.) are ignored — this crate is deliberately narrow so
//! consumers can bind the three columns as typed SPARQL variables through
//! the new SERVICE handler in oxigraph-wf. If you need labels or other
//! attributes, use `wf_tree` and unpack the JSON.

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
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

// Same soft depth cap as wf_tree: the host defaults to a hard cap of 100
// callback frames, so 90 leaves room to unwind cleanly.
const DEFAULT_MAX_DEPTH: u32 = 90;

// Per-recursion row limit passed to run-prepared. Matches wf_tree so the
// two crates share the same "at most 1000 children per node" assumption.
const CHILD_ROW_LIMIT: u32 = 1_000;

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn integer_literal(n: u32) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
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
        _ => Err(format!("wf:tree_rows: `{}` argument must be a string literal", name)),
    }
}

fn u32_arg(v: &Value, name: &str) -> Result<u32, String> {
    match v {
        Value::Literal(l) => l
            .label
            .parse::<u32>()
            .map_err(|_| format!("wf:tree_rows: `{}` must be a non-negative integer", name)),
        _ => Err(format!("wf:tree_rows: `{}` argument must be an integer literal", name)),
    }
}

/// One row of output: (uri, depth, optional parent). The SERVICE handler
/// (or any direct binding-set consumer) receives `parent` as an
/// `Option<Value>` — represented in the wire binding-sets as either a
/// present binding or an omitted one. wf_tree_rows always emits the
/// binding for `parent` at depth > 0; at depth 0 (the root) it omits the
/// binding entirely, matching SPARQL semantics for an unbound column.
struct Row {
    uri: Value,
    depth: u32,
    parent: Option<Value>,
}

/// Depth-first walk. `parent` is `None` only at the root; every recursive
/// descent carries its caller's URI down so we can emit the parent column
/// without a second lookup. `seen` guards against cycles; a node already
/// in `seen` is skipped entirely (matches wf_tree, but without emitting a
/// stub row — this variant is trying to feed a relational binding-set, not
/// a self-describing tree object).
fn walk(
    node: &Value,
    depth: u32,
    parent: Option<&Value>,
    query_handle: u32,
    max_depth: u32,
    seen: &mut HashSet<String>,
    out: &mut Vec<Row>,
) {
    let node_key = value_as_string(node);

    // Cycle: emit nothing and unwind.
    if !seen.insert(node_key.clone()) {
        return;
    }

    // Emit the current node first — depth-first, parent-before-children.
    out.push(Row {
        uri: node.clone(),
        depth,
        parent: parent.cloned(),
    });

    // Bail before recursing if the next level would exceed our cap. The
    // host also enforces its own callback-depth cap; we exit first so the
    // walk terminates cleanly instead of tripping the host error path.
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
        // A query failure at one node shouldn't wipe the whole walk. Just
        // stop descending here and let the caller see whatever we've
        // accumulated. (An earlier design surfaced errors via a stub row;
        // dropping them keeps the schema uniform for the SERVICE binding.)
        Err(_) => {
            seen.remove(&node_key);
            return;
        }
    };

    for row in &rows.rows {
        if let Some(child) = row.iter().find(|b| b.name == CHILD_VAR) {
            walk(&child.value, depth + 1, Some(node), query_handle, max_depth, seen, out);
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
                "wf:tree_rows: expected 2 or 3 args (root, query, [max_depth]), got {}",
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
        // (same optimisation as wf_tree — cuts a 1000-node walk from ~800ms
        // to ~150ms in the reference impl).
        let query_handle = host::prepare_query(&query)?;

        let mut seen: HashSet<String> = HashSet::new();
        let mut rows: Vec<Row> = Vec::new();
        walk(&root, 0, None, query_handle, max_depth, &mut seen, &mut rows);

        let wire_rows: Vec<Vec<Binding>> = rows
            .into_iter()
            .map(|r| {
                let mut bs = Vec::with_capacity(3);
                bs.push(Binding { name: "uri".into(), value: r.uri });
                bs.push(Binding { name: "depth".into(), value: integer_literal(r.depth) });
                // Omit the parent binding for the root row so consumers
                // observe SPARQL-native "unbound" semantics rather than an
                // empty-string sentinel.
                if let Some(p) = r.parent {
                    bs.push(Binding { name: "parent".into(), value: p });
                }
                bs
            })
            .collect();

        Ok(BindingSets {
            vars: vec!["uri".into(), "depth".into(), "parent".into()],
            rows: wire_rows,
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf:tree_rows: aggregate not applicable".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf:tree_rows: aggregate not applicable".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        // We can't cheaply predict how many rows a walk will yield; report
        // a rough placeholder with `Injected` accuracy so the planner
        // treats us as a hint, not a promise.
        Ok(Cardinality { value: 100.0, accuracy: Accuracy::Injected })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "wf:tree_rows(root, sparql_query [, max_depth=90]) -> binding-sets \
                     { vars: [uri, depth, parent], rows }. Depth-first recursive walk \
                     from `root`; `sparql_query` must bind `?child` per row and \
                     re-uses `?this` as the current node's IRI at each level. \
                     Emits one row per visited node — `parent` is unbound at depth 0 \
                     and bound to the caller node otherwise. Cycle-safe."),
            }]],
        }
    }
}

export!(Component);
