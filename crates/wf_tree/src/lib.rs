//! wf_tree — recursive tree walker built on prepared-query callbacks.
//!
//! Signature: `wf:tree(root, sparql_query [, child_var])` returns a
//! single rdf:JSON literal shaped by recursively re-running
//! `sparql_query` with `?this` re-bound to each discovered child.
//!
//! Semantics:
//!   1. Parse + plan `sparql_query` once via
//!      `prepared-query-callbacks::prepare-query`.
//!   2. At each recursion step, call
//!      `prepared-query-callbacks::run-prepared` with a single binding
//!      `{ this = <current-node> }`. The returned flat list<binding> is
//!      split into rows on repeated variable identity (the R1 flat
//!      binding-list convention mirroring
//!      `graph-callbacks::query-result::bindings`).
//!   3. Each row's `child_var` (default: `child`) is the next node to
//!      recurse on; every other bound variable becomes an attribute on
//!      that child in the output tree.
//!   4. Recurse until a node has no children, we hit the depth cap
//!      (`observability-callbacks::callback-depth`), or we detect a
//!      cycle via a URI-set carried through the recursion.
//!
//! Migration deviation (Follow-up F): the legacy `host::run-prepared`
//! took a `max-rows` bound; the R1 shape does not. Callers that need a
//! per-descent bound must inline `LIMIT N` into the SPARQL text. This
//! crate previously passed `Some(1000)` — the same effect is achieved
//! by writing `... LIMIT 1000` in the child-lookup query.
//!
//! Output collapses to a single rdf:JSON literal (mirror of the batch1
//! / batch2 collapse convention).

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

const DEFAULT_CHILD_VAR: &str = "child";
const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

const DEPTH_SOFT_CAP: u32 = 90;

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
        WitTerm::Literal(l) => {
            let dt = l.datatype.as_deref().unwrap_or("");
            if dt == XSD_INTEGER || dt.ends_with("#integer") || dt.ends_with("#long") {
                if let Ok(n) = l.value.parse::<i64>() {
                    return json!(n);
                }
            }
            if dt == XSD_DECIMAL
                || dt.ends_with("#decimal")
                || dt.ends_with("#double")
                || dt.ends_with("#float")
            {
                if let Ok(n) = l.value.parse::<f64>() {
                    if n.is_finite() {
                        return json!(n);
                    }
                }
            }
            if dt == XSD_BOOLEAN || dt.ends_with("#boolean") {
                if l.value == "true" {
                    return json!(true);
                }
                if l.value == "false" {
                    return json!(false);
                }
            }
            json!(l.value)
        }
        WitTerm::Triple(_) => json!("<<quoted-triple>>"),
    }
}

fn string_arg(v: &WitTerm, name: &str) -> Result<String, String> {
    match v {
        WitTerm::Literal(l) => Ok(l.value.clone()),
        _ => Err(format!(
            "wf:tree: `{name}` argument must be a string literal"
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

/// Split a flat `list<binding>` into rows on repeated variable identity.
/// A row ends and a new row begins when we see a variable name we've
/// already seen in the current row (the R1 convention encoded in
/// `graph-callbacks::query-result::bindings` and mirrored here).
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

fn walk(
    node: &WitTerm,
    handle: &PreparedHandle,
    child_var: &str,
    seen: &mut HashSet<String>,
) -> JsonValue {
    let key = term_key(node);
    let mut obj = serde_json::Map::new();
    obj.insert("uri".into(), json!(key));

    if !seen.insert(key.clone()) {
        obj.insert("cycle".into(), json!(true));
        return JsonValue::Object(obj);
    }
    if obs::callback_depth() >= DEPTH_SOFT_CAP {
        obj.insert("depth_bounded".into(), json!(true));
        seen.remove(&key);
        return JsonValue::Object(obj);
    }

    let inputs = vec![WitBinding {
        variable: "this".into(),
        value: node.clone(),
    }];
    let flat = match pq::run_prepared(*handle, &inputs) {
        Ok(v) => v,
        Err(e) => {
            obj.insert("error".into(), json!(map_prepared_err(e)));
            seen.remove(&key);
            return JsonValue::Object(obj);
        }
    };
    let rows = split_rows(flat);

    let mut children: Vec<JsonValue> = Vec::new();
    for row in &rows {
        let Some(child_binding) = row.iter().find(|b| b.variable == child_var) else {
            continue;
        };
        let attrs: Vec<(String, JsonValue)> = row
            .iter()
            .filter(|b| b.variable != child_var)
            .map(|b| (b.variable.clone(), term_to_json(&b.value)))
            .collect();
        let child_tree = walk(&child_binding.value, handle, child_var, seen);
        let mut child_obj = child_tree.as_object().cloned().unwrap_or_default();
        for (k, v) in attrs {
            child_obj.entry(k).or_insert(v);
        }
        children.push(JsonValue::Object(child_obj));
    }
    obj.insert("children".into(), JsonValue::Array(children));

    seen.remove(&key);
    JsonValue::Object(obj)
}

fn tree_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
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

    let handle = pq::prepare_query(&query).map_err(map_prepared_err)?;

    let mut seen: HashSet<String> = HashSet::new();
    let tree = walk(&root, &handle, &child_var, &mut seen);

    pq::free_prepared(handle);

    Ok(json_literal(&tree.to_string()))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_tree".into(),
            min_arity: 2,
            max_arity: Some(3),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_tree" => tree_impl(&args),
            other => Err(format!("wf_tree: unknown function '{other}'")),
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
            "wf_tree: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_tree: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_tree: aggregate state was never constructed".into())
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
            "wf_tree: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
