//! wf_infer — derived facts as materialized SPARQL views.
//!
//! Migrated (Follow-up E) from the Stardog overlay
//! `stardog:webfunction@0.5.0` world to the base
//! `tegmentum:webfunction/extension-with-host-callbacks@0.1.0` world.
//! Semantics identical to the sibling `wf_infer-extension` crate; the
//! two artifacts differ only in wasm binary name / exported function
//! name (`wf_infer` here, `run-rule` in the sibling). Both dispatch
//! through `graph-callbacks::{execute-query, execute-update}`.
//!
//! Signature: `wf_infer("<rule-json>")` -> rdf:JSON literal
//!   `{ "rule": ..., "iterations": ..., "emitted_total": ...,
//!      "graph_size": ... }`
//!
//! Runs a user-authored CONSTRUCT (via `graph-callbacks::execute-query`
//! returning `Quads`) and INSERTs the resulting triples into a target
//! named graph (via `graph-callbacks::execute-update`). Optional
//! fixed-point iteration loops the CONSTRUCT / INSERT pair until the
//! target graph's size stops growing, guarded by `max_iterations`.
//!
//! The Stardog-era shape returned one row with four columns; the base
//! `extension::call` surface returns a single term, so the summary
//! collapses into one rdf:JSON literal.

#[allow(warnings)]
mod bindings;

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
use bindings::tegmentum::webfunction::graph_callbacks::{
    self as gc, Binding as WitBinding, Quad as WitQuad, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";

// ---------------------------------------------------------------------------
// Rule descriptor
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Rule {
    name: String,
    #[serde(default)]
    construct: Option<String>,
    #[serde(default, rename = "if")]
    if_clause: Option<String>,
    #[serde(default, rename = "then")]
    then_clause: Option<String>,
    #[serde(default)]
    prefixes: Option<String>,
    graph: String,
    #[serde(default = "default_refresh")]
    refresh_mode: String,

    #[serde(default)]
    iterate: bool,

    #[serde(default = "default_max_iterations")]
    max_iterations: u32,
}

fn default_refresh() -> String {
    "replace".into()
}

fn default_max_iterations() -> u32 {
    100
}

impl Rule {
    fn construct_sparql(&self) -> Result<String, String> {
        match (&self.construct, &self.if_clause, &self.then_clause) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(format!(
                "wf_infer: rule `{}` sets both `construct` and `if`/`then`; pick one",
                self.name
            )),
            (Some(q), None, None) => Ok(q.clone()),
            (None, Some(if_body), Some(then_body)) => {
                let prefix = self
                    .prefixes
                    .as_deref()
                    .map(|p| format!("{p}\n"))
                    .unwrap_or_default();
                Ok(format!(
                    "{prefix}CONSTRUCT {{ {then_body} }} WHERE {{ {if_body} }}"
                ))
            }
            (None, Some(_), None) | (None, None, Some(_)) => Err(format!(
                "wf_infer: rule `{}` uses SRS sugar but is missing one of `if` / `then`",
                self.name
            )),
            (None, None, None) => Err(format!(
                "wf_infer: rule `{}` has neither `construct` nor `if`/`then`",
                self.name
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Guest impls
// ---------------------------------------------------------------------------

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_infer".into(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_infer" => wf_infer_impl(&args),
            other => Err(format!("wf_infer: unknown function '{other}'")),
        }
    }
}

fn wf_infer_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let rule_json = require_literal(args, "wf_infer")?;
    let rule: Rule = serde_json::from_str(&rule_json)
        .map_err(|e| format!("wf_infer: rule parse: {e}"))?;

    if rule.refresh_mode == "replace" {
        let clear = format!("CLEAR SILENT GRAPH <{}>", rule.graph);
        execute_update(&clear)
            .map_err(|e| format!("wf_infer: clear graph `{}`: {e}", rule.graph))?;
    } else if rule.refresh_mode != "append" {
        return Err(format!(
            "wf_infer: unknown refresh_mode `{}` (want replace | append)",
            rule.refresh_mode
        ));
    }

    let construct_sparql = rule.construct_sparql()?;

    let (iterations, emitted_total, final_size) = if rule.iterate {
        iterate_to_fixpoint(&rule, &construct_sparql)?
    } else {
        let quads = query_quads(&construct_sparql)?;
        let inserted = insert_quads(&rule.graph, &quads)?;
        let size = graph_size(&rule.graph)?;
        (1u32, inserted, size)
    };

    let payload = json!({
        "rule": rule.name,
        "iterations": iterations,
        "emitted_total": emitted_total,
        "graph_size": final_size,
    });
    let serialized = serde_json::to_string(&payload)
        .map_err(|e| format!("wf_infer: serializing summary: {e}"))?;
    Ok(WitTerm::Literal(WitLiteral {
        value: serialized,
        datatype: Some(RDF_JSON.into()),
        language: None,
    }))
}

impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "wf_infer: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_infer: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_infer: aggregate state was never constructed".into())
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
            "wf_infer: unknown property function '{name}' (this component provides none)"
        ))
    }
}

// ---------------------------------------------------------------------------
// Fixed-point iteration
// ---------------------------------------------------------------------------

fn iterate_to_fixpoint(
    rule: &Rule,
    construct_sparql: &str,
) -> Result<(u32, u64, u64), String> {
    let mut prev_size = graph_size(&rule.graph)?;
    let mut total_emitted: u64 = 0;
    let mut iterations: u32 = 0;

    while iterations < rule.max_iterations {
        let quads = query_quads(construct_sparql)?;
        let emitted = insert_quads(&rule.graph, &quads)?;
        total_emitted += emitted;
        iterations += 1;

        let new_size = graph_size(&rule.graph)?;
        if new_size == prev_size {
            return Ok((iterations, total_emitted, new_size));
        }
        prev_size = new_size;
    }
    Ok((iterations, total_emitted, prev_size))
}

// ---------------------------------------------------------------------------
// graph-callbacks helpers
// ---------------------------------------------------------------------------

fn require_literal(args: &[WitTerm], func: &str) -> Result<String, String> {
    let [arg] = args else {
        return Err(format!(
            "{func}: expected 1 argument (rule-json string literal), got {}",
            args.len()
        ));
    };
    match arg {
        WitTerm::Literal(l) => Ok(l.value.clone()),
        WitTerm::NamedNode(_) => Err(format!("{func}: argument must be a string literal, got IRI")),
        WitTerm::BlankNode(_) => Err(format!(
            "{func}: argument must be a string literal, got blank node"
        )),
        WitTerm::Triple(_) => Err(format!(
            "{func}: argument must be a string literal, got quoted triple"
        )),
    }
}

fn query_quads(sparql: &str) -> Result<Vec<WitQuad>, String> {
    match execute_query(sparql)? {
        CallbackQueryResult::Quads(q) => Ok(q),
        CallbackQueryResult::Bindings(bs) => Ok(bindings_to_quads(bs)),
        CallbackQueryResult::Boolean(_) => {
            Err("wf_infer: CONSTRUCT unexpectedly returned boolean".into())
        }
    }
}

fn bindings_to_quads(flat: Vec<WitBinding>) -> Vec<WitQuad> {
    let mut rows: Vec<Vec<WitBinding>> = Vec::new();
    let mut current: Vec<WitBinding> = Vec::new();
    for b in flat {
        if current.iter().any(|prior| prior.variable == b.variable) {
            rows.push(std::mem::take(&mut current));
        }
        current.push(b);
    }
    if !current.is_empty() {
        rows.push(current);
    }

    let mut quads: Vec<WitQuad> = Vec::new();
    for row in rows {
        let mut s: Option<WitTerm> = None;
        let mut p: Option<WitTerm> = None;
        let mut o: Option<WitTerm> = None;
        for b in row {
            match b.variable.as_str() {
                "s" | "subject" => s = Some(b.value),
                "p" | "predicate" => p = Some(b.value),
                "o" | "object" => o = Some(b.value),
                _ => {}
            }
        }
        if let (Some(s), Some(p), Some(o)) = (s, p, o) {
            quads.push(WitQuad {
                subject: s,
                predicate: p,
                object: o,
                graph: None,
            });
        }
    }
    quads
}

fn graph_size(graph: &str) -> Result<u64, String> {
    let sparql = format!(
        "SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"
    );
    match execute_query(&sparql)? {
        CallbackQueryResult::Bindings(bs) => Ok(read_first_int(&bs, "n")),
        _ => Ok(0),
    }
}

fn read_first_int(bs: &[WitBinding], var: &str) -> u64 {
    bs.iter()
        .find(|b| b.variable == var)
        .and_then(|b| match &b.value {
            WitTerm::Literal(l) => l.value.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn execute_query(sparql: &str) -> Result<CallbackQueryResult, String> {
    gc::execute_query(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

fn execute_update(sparql: &str) -> Result<(), String> {
    gc::execute_update(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

// ---------------------------------------------------------------------------
// INSERT batching
// ---------------------------------------------------------------------------

const BATCH_SIZE: usize = 500;

fn insert_quads(graph: &str, quads: &[WitQuad]) -> Result<u64, String> {
    let mut total: u64 = 0;
    let mut buffer = String::new();
    let mut in_batch = 0usize;

    for q in quads {
        buffer.push_str(&term_to_sparql(&q.subject));
        buffer.push(' ');
        buffer.push_str(&term_to_sparql(&q.predicate));
        buffer.push(' ');
        buffer.push_str(&term_to_sparql(&q.object));
        buffer.push_str(" .\n");
        in_batch += 1;
        total += 1;

        if in_batch >= BATCH_SIZE {
            flush(graph, &buffer)?;
            buffer.clear();
            in_batch = 0;
        }
    }
    if !buffer.is_empty() {
        flush(graph, &buffer)?;
    }
    Ok(total)
}

fn flush(graph: &str, triples: &str) -> Result<(), String> {
    let insert = format!("INSERT DATA {{ GRAPH <{graph}> {{ {triples} }} }}");
    execute_update(&insert).map_err(|e| format!("wf_infer: insert batch: {e}"))
}

fn term_to_sparql(t: &WitTerm) -> String {
    match t {
        WitTerm::NamedNode(iri) => format!("<{iri}>"),
        WitTerm::BlankNode(label) => format!("_:{label}"),
        WitTerm::Literal(l) => literal_to_sparql(l),
        WitTerm::Triple(_) => String::new(),
    }
}

fn literal_to_sparql(l: &WitLiteral) -> String {
    let escaped = l
        .value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    if let Some(lang) = &l.language {
        format!("\"{escaped}\"@{lang}")
    } else if let Some(dt) = &l.datatype {
        format!("\"{escaped}\"^^<{dt}>")
    } else {
        format!("\"{escaped}\"")
    }
}

bindings::export!(Component with_types_in bindings);
