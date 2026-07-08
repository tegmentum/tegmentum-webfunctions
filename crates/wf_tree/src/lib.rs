//! wf_tree — recursive tree walker built on the v0.3.0 host callbacks.
//!
//! `wf:tree(root, sparql_query [, child_var])` returns a plain-JSON tree
//! shaped by recursively re-running `sparql_query` with `?this` re-bound
//! to each discovered child.
//!
//! Semantics:
//!   1. Take the `root` value and any bindings; assemble
//!      `[binding{ name: "this", value: root }]`.
//!   2. Call the host's `execute-query(sparql, bindings, Some(max))`.
//!   3. Each returned row's `child_var` is the next node to recurse on.
//!      Every other variable in the row becomes an attribute on that
//!      child node in the output tree.
//!   4. Recurse until a node has no children, we hit the depth cap, or
//!      we detect a cycle via a URI-set carried through the recursion.
//!
//! Output shape (regular JSON — no `@`-keys, full URLs everywhere):
//!
//! ```json
//! {
//!   "uri": "http://example.org/root",
//!   "children": [
//!     { "uri": "http://example.org/child-a",
//!       "label": "…", "type": "…",
//!       "children": [ … ] },
//!     …
//!   ]
//! }
//! ```

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::{json, Value as JsonValue};
use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::collections::HashSet;

struct Component;

const DEFAULT_CHILD_VAR: &str = "child";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

// Conservative guard below the host's default max-depth of 100. If the host
// admin has lowered the cap, we still exit cleanly before reaching it.
const DEPTH_SOFT_CAP: u32 = 90;

// Per-recursion row limit passed to execute-query. Trees rarely need more
// than a few dozen children per node; higher values just bloat memory.
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

fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Iri(uri) => json!(uri),
        Value::Bnode(id) => json!(format!("_:{}", id)),
        Value::Literal(l) => {
            let dt = l.datatype.as_str();
            if dt == XSD_INTEGER || dt.ends_with("#integer") || dt.ends_with("#long") {
                if let Ok(n) = l.label.parse::<i64>() { return json!(n); }
            }
            if dt == XSD_DECIMAL || dt.ends_with("#decimal") || dt.ends_with("#double") || dt.ends_with("#float") {
                if let Ok(n) = l.label.parse::<f64>() {
                    if n.is_finite() { return json!(n); }
                }
            }
            if dt == XSD_BOOLEAN || dt.ends_with("#boolean") {
                if l.label == "true" { return json!(true); }
                if l.label == "false" { return json!(false); }
            }
            json!(l.label)
        }
    }
}

/// Recursively walk from `node`, running the prepared `query_handle` at each
/// level with `?this` bound to the current node. The parse + precompile cost
/// is paid once by the caller via `host::prepare_query`; each recursion step
/// only pays initial-binding substitution and iteration.
fn walk(node: &Value, query_handle: u32, child_var: &str, seen: &mut HashSet<String>) -> JsonValue {
    let node_key = value_as_string(node);

    let mut obj = serde_json::Map::new();
    obj.insert("uri".into(), json!(node_key));

    // Cycle: emit a stub, don't recurse.
    if !seen.insert(node_key.clone()) {
        obj.insert("cycle".into(), json!(true));
        return JsonValue::Object(obj);
    }

    // Depth guard: emit a stub, don't recurse.
    if host::callback_depth() >= DEPTH_SOFT_CAP {
        obj.insert("depth_bounded".into(), json!(true));
        seen.remove(&node_key);
        return JsonValue::Object(obj);
    }

    let bindings = vec![Binding {
        name: "this".into(),
        value: node.clone(),
    }];

    let rows = match host::run_prepared(query_handle, &bindings, Some(CHILD_ROW_LIMIT)) {
        Ok(bs) => bs,
        Err(e) => {
            obj.insert("error".into(), json!(e));
            seen.remove(&node_key);
            return JsonValue::Object(obj);
        }
    };

    let mut children: Vec<JsonValue> = Vec::new();
    for row in &rows.rows {
        let child_slot = row.iter().find(|b| b.name == child_var);
        let Some(child_binding) = child_slot else { continue };

        // Non-child variables become attributes of this child in the tree.
        let attrs: Vec<(String, JsonValue)> = row.iter()
            .filter(|b| b.name != child_var)
            .map(|b| (b.name.clone(), value_to_json(&b.value)))
            .collect();

        let child_tree = walk(&child_binding.value, query_handle, child_var, seen);
        let mut child_obj = child_tree.as_object().cloned().unwrap_or_default();
        for (k, v) in attrs {
            // Don't clobber existing keys (mainly "uri", "children", "cycle").
            child_obj.entry(k).or_insert(v);
        }
        children.push(JsonValue::Object(child_obj));
    }
    obj.insert("children".into(), JsonValue::Array(children));

    seen.remove(&node_key);
    JsonValue::Object(obj)
}

fn string_arg(v: &Value, name: &str) -> Result<String, String> {
    match v {
        Value::Literal(l) => Ok(l.label.clone()),
        _ => Err(format!("wf:tree: `{}` argument must be a string literal", name)),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        // Args:
        //   0: root node (any Value)
        //   1: SPARQL query as a string literal
        //   2 (optional): child variable name (string literal, defaults to "child")
        if args.len() < 2 || args.len() > 3 {
            return Err(format!(
                "wf:tree: expected 2 or 3 args (root, query, [child_var]), got {}",
                args.len()
            ));
        }
        let root = args[0].clone();
        let query = string_arg(&args[1], "query")?;
        let child_var = if args.len() == 3 {
            string_arg(&args[2], "child_var")?
        } else {
            DEFAULT_CHILD_VAR.into()
        };

        // Parse + precompile the child-lookup query once. Every recursion
        // step reuses the handle, so we don't re-pay ~400µs of parse +
        // strategy.precompile on each of N nodes.
        let query_handle = host::prepare_query(&query)?;

        let mut seen: HashSet<String> = HashSet::new();
        let tree = walk(&root, query_handle, &child_var, &mut seen);

        Ok(BindingSets {
            vars: vec!["tree".into()],
            rows: vec![vec![Binding {
                name: "tree".into(),
                value: string_literal(&tree.to_string()),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf:tree: aggregate not applicable".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf:tree: aggregate not applicable".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Injected })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "wf:tree(root, sparql_query [, child_var='child']) -> JSON tree. \
                     Recursively runs sparql_query with `?this` re-bound to each \
                     discovered child. Any variable named `?child` (or the caller-\
                     supplied child_var) in the query's result becomes the next \
                     node to recurse; all other bound variables become attributes \
                     of that child in the emitted tree. Cycle-safe and depth-bounded. \
                     Output is regular JSON — no @-keys, full URLs everywhere."),
            }]],
        }
    }
}

export!(Component);
