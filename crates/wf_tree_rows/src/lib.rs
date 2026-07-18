//! wf_tree_rows — recursive tree walker projecting one row per visited
//! node, collapsed to a single rdf:JSON literal for the base-substrate
//! filter export.
//!
//! Signature: `wf:tree_rows(root, sparql_query [, max_depth])` returns
//! an rdf:JSON literal shaped as
//!   `{"vars": ["uri","depth","parent"], "rows": [ ... ]}`
//! matching the batch1 / batch2 collapse convention. Each row is a JSON
//! object with `uri`, `depth`, and (except at depth 0) `parent`.
//!
//! Recursion mechanics:
//!   * `?this` is rebound to each newly discovered node at every
//!     descent through
//!     `prepared-query-callbacks::{prepare-query, run-prepared,
//!     free-prepared}`.
//!   * `?child` (from the caller-supplied SPARQL) advances the walk.
//!   * Cycle detection uses a `HashSet<String>` guard on canonical
//!     term strings, matching the original.
//!   * Depth cap tracked via
//!     `observability-callbacks::callback-depth`.
//!
//! Migration deviation (Follow-up F): the legacy `host::run-prepared`
//! accepted a `max-rows` bound. The R1 shape does not; callers wanting
//! a per-descent bound must inline `LIMIT N` into their SPARQL text.
//! The original crate defaulted to `Some(1000)`; the same effect is
//! achieved by writing `... LIMIT 1000`.

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
use bindings::tegmentum::webfunction::observability_callbacks as obs;
use bindings::tegmentum::webfunction::prepared_query_callbacks::{
    self as pq, PreparedError, PreparedHandle,
};
use bindings::tegmentum::webfunction::types::{
    Binding as WitBinding, Literal as WitLiteral, Term as WitTerm,
};

struct Component;

const CHILD_VAR: &str = "child";
const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const DEFAULT_MAX_DEPTH: u32 = 90;

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.into(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    })
}

fn term_key(v: &WitTerm) -> String {
    match v {
        WitTerm::NamedNode(uri) => uri.clone(),
        WitTerm::BlankNode(id) => format!("_:{id}"),
        WitTerm::Literal(l) => l.value.clone(),
        WitTerm::Triple(_) => "<<quoted-triple>>".into(),
    }
}

fn term_to_json(v: &WitTerm) -> JsonValue {
    match v {
        WitTerm::NamedNode(uri) => json!(uri),
        WitTerm::BlankNode(id) => json!(format!("_:{id}")),
        WitTerm::Literal(l) => json!(l.value),
        WitTerm::Triple(_) => json!("<<quoted-triple>>"),
    }
}

fn string_arg(v: &WitTerm, name: &str) -> Result<String, String> {
    match v {
        WitTerm::Literal(l) => Ok(l.value.clone()),
        _ => Err(format!(
            "wf:tree_rows: `{name}` argument must be a string literal"
        )),
    }
}

fn u32_arg(v: &WitTerm, name: &str) -> Result<u32, String> {
    match v {
        WitTerm::Literal(l) => l.value.parse::<u32>().map_err(|_| {
            format!("wf:tree_rows: `{name}` must be a non-negative integer literal")
        }),
        _ => Err(format!(
            "wf:tree_rows: `{name}` argument must be an integer literal"
        )),
    }
}

fn map_prepared_err(e: PreparedError) -> String {
    match e {
        PreparedError::SyntaxError(m) => format!("prepared-query syntax-error: {m}"),
        PreparedError::BackendError(m) => format!("prepared-query backend-error: {m}"),
        PreparedError::UnknownHandle => "prepared-query unknown-handle".into(),
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

struct Row {
    uri: WitTerm,
    depth: u32,
    parent: Option<WitTerm>,
}

fn walk(
    node: &WitTerm,
    depth: u32,
    parent: Option<&WitTerm>,
    handle: &PreparedHandle,
    max_depth: u32,
    seen: &mut HashSet<String>,
    out: &mut Vec<Row>,
) {
    let key = term_key(node);
    if !seen.insert(key.clone()) {
        return;
    }
    out.push(Row {
        uri: node.clone(),
        depth,
        parent: parent.cloned(),
    });
    if depth >= max_depth || obs::callback_depth() >= DEFAULT_MAX_DEPTH {
        seen.remove(&key);
        return;
    }

    let inputs = vec![WitBinding {
        variable: "this".into(),
        value: node.clone(),
    }];
    let flat = match pq::run_prepared(*handle, &inputs) {
        Ok(v) => v,
        Err(_) => {
            seen.remove(&key);
            return;
        }
    };
    let rows = split_rows(flat);
    for row in &rows {
        if let Some(child) = row.iter().find(|b| b.variable == CHILD_VAR) {
            walk(
                &child.value,
                depth + 1,
                Some(node),
                handle,
                max_depth,
                seen,
                out,
            );
        }
    }

    seen.remove(&key);
}

fn tree_rows_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
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

    let handle = pq::prepare_query(&query).map_err(map_prepared_err)?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut rows: Vec<Row> = Vec::new();
    walk(&root, 0, None, &handle, max_depth, &mut seen, &mut rows);

    pq::free_prepared(handle);

    let json_rows: Vec<JsonValue> = rows
        .into_iter()
        .map(|r| {
            let mut obj = serde_json::Map::new();
            obj.insert("uri".into(), term_to_json(&r.uri));
            obj.insert("depth".into(), json!(r.depth));
            if let Some(p) = r.parent {
                obj.insert("parent".into(), term_to_json(&p));
            }
            JsonValue::Object(obj)
        })
        .collect();
    let out = json!({
        "vars": ["uri", "depth", "parent"],
        "rows": json_rows,
    });
    Ok(json_literal(&out.to_string()))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_tree_rows".into(),
            min_arity: 2,
            max_arity: Some(3),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_tree_rows" => tree_rows_impl(&args),
            other => Err(format!("wf_tree_rows: unknown function '{other}'")),
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
            "wf_tree_rows: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_tree_rows: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_tree_rows: aggregate state was never constructed".into())
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
            "wf_tree_rows: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
