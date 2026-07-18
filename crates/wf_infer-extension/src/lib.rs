//! Phase-2 SPARQL extension re-exposing wf_infer's rule engine on
//! the standardized `tegmentum:webfunction/graph-callbacks@0.1.0`
//! interface. Second migration in the `-extension` family (after
//! wf_profile-extension); first consumer of `execute-update` and the
//! `query-result::quads` arm.
//!
//! One export:
//!
//!   * `run-rule(<rule-json>) -> string literal (rdf:JSON)`
//!     Parse a rule descriptor, optionally CLEAR the target graph,
//!     execute the CONSTRUCT via `graph-callbacks::execute-query`
//!     (returning `Quads`), and INSERT the derived triples via
//!     `graph-callbacks::execute-update`. Optional fixed-point
//!     iteration loops the CONSTRUCT / INSERT pair until the target
//!     graph's size stops growing, guarded by a `max_iterations` cap.
//!     Returns an rdf:JSON literal carrying the rule name, iteration
//!     count, per-pass emitted-triple total, and final graph size —
//!     the same fields the original wf_infer returns as
//!     multi-column bindings.
//!
//! # Provenance of the algorithm
//!
//! The `Rule` deserializer, `construct_sparql()` synthesis,
//! CLEAR / INSERT SPARQL templates, and the fixed-point convergence
//! loop are adapted from
//! `~/git/webfunctions/crates/wf_infer/src/lib.rs`. The
//! shape of every SPARQL string, default values, and error messages
//! are preserved so a caller migrating from wf:call(wf_infer.wasm)
//! to run-rule sees byte-equivalent behavior for the same rule JSON.
//!
//! Duplication (rather than a cross-crate `use`) keeps the original
//! wf_infer crate unmodified. wf_infer is a `cdylib` targeting
//! wasm32-wasip1 under `stardog:webfunction@0.5.0`; it cannot be
//! imported as a library. The ~200 lines duplicated here have one
//! non-trivial dependency (`serde` / `serde_json`) which is pinned to
//! the same major version.
//!
//! # Callback usage
//!
//! Every SPARQL string in this module is dispatched through
//! `bindings::tegmentum::webfunction::graph_callbacks::{execute_query,
//! execute_update}`. The Stardog-era `wf_infer` crate paid three
//! round-trips per rule via `stardog::webfunction::host` — CLEAR
//! (update), CONSTRUCT (query with `bindings: &[Binding]` and
//! `limit: Option<u32>`), then INSERT (update). The migrated shape
//! drops the initial-binding / limit degrees of freedom: the
//! `graph-callbacks` API takes a single SPARQL string; anything the
//! Stardog surface expressed as an argument is inlined into the
//! query text. Wire-equivalent, one fewer degree of freedom in the
//! WIT.
//!
//! A material improvement over wf_infer: CONSTRUCT under
//! `graph-callbacks` returns `QueryResult::Quads` — a typed list of
//! `quad { subject, predicate, object, graph }` records. wf_infer's
//! Stardog-era host returned CONSTRUCT rows as a `BindingSets` with
//! `s`/`p`/`o` bindings; this crate reads the Quads arm directly,
//! and the INSERT batch synthesizes triple text from typed terms
//! instead of parsing binding names.

#[allow(warnings)]
mod bindings;

use bindings::exports::tegmentum::webfunction::aggregate::{
    AggregateDescriptor, AggregateState, Guest as AggregateGuest,
    GuestAggregateState,
};
use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::exports::tegmentum::webfunction::property_function::{
    BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
};
use bindings::tegmentum::webfunction::graph_callbacks::{
    self as gc, Binding as WitBinding, Quad as WitQuad,
    QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{
    Literal as WitLiteral, Term as WitTerm,
};

use serde::Deserialize;
use serde_json::json;

struct Component;

// XSD datatype IRIs — mirrored from `wf_infer/src/lib.rs`.
const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";

// ---------------------------------------------------------------------
// Extension surface
// ---------------------------------------------------------------------

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "run-rule".to_string(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "run-rule" => run_rule(&args),
            other => Err(format!(
                "wf_infer-extension: unknown function '{other}'"
            )),
        }
    }
}

/// Aggregate interface stub. Every export required by the
/// `extension-with-host-callbacks` world; this component has no
/// aggregates. Same shape wf_profile-extension uses.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "wf_infer-extension: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_infer-extension: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_infer-extension: aggregate state was never constructed".into())
    }
}

/// Property-function interface stub. Required by the world, unused
/// by this component.
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
            "wf_infer-extension: unknown property function '{name}' (this component provides none)"
        ))
    }
}

// ---------------------------------------------------------------------
// Rule descriptor — port of wf_infer::Rule.
// ---------------------------------------------------------------------

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

    /// Fixed-point iteration flag — mirror of wf_infer::Rule::iterate.
    #[serde(default)]
    iterate: bool,

    /// Safety cap on iteration count — mirror of
    /// wf_infer::Rule::max_iterations.
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
    /// Build the CONSTRUCT SPARQL text. If the rule specifies
    /// `construct` verbatim, use it as-is. If it uses the `if`/`then`
    /// sugar, synthesise a CONSTRUCT wrapping the two triple-pattern
    /// strings into WHERE and CONSTRUCT clauses respectively,
    /// prefixing any declared namespaces once at the top. Byte-for-
    /// byte mirror of wf_infer::Rule::construct_sparql.
    fn construct_sparql(&self) -> Result<String, String> {
        match (&self.construct, &self.if_clause, &self.then_clause) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(format!(
                "wf_infer-extension: rule `{}` sets both `construct` and `if`/`then`; pick one",
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
                "wf_infer-extension: rule `{}` uses SRS sugar but is missing one of `if` / `then`",
                self.name
            )),
            (None, None, None) => Err(format!(
                "wf_infer-extension: rule `{}` has neither `construct` nor `if`/`then`",
                self.name
            )),
        }
    }
}

// ---------------------------------------------------------------------
// run-rule(<rule-json>) — the one exported function.
// ---------------------------------------------------------------------

/// Parse the rule descriptor, run it, return a JSON summary literal.
///
/// Non-goal for the extension: multi-argument variants of wf_infer's
/// evaluate() are collapsed into the single rule-json argument.
/// Everything wf_infer's Stardog-era caller expressed as extra
/// arguments is expressible in the rule descriptor already.
fn run_rule(args: &[WitTerm]) -> Result<WitTerm, String> {
    let rule_json = require_literal(args, "run-rule")?;
    let rule: Rule = serde_json::from_str(&rule_json)
        .map_err(|e| format!("wf_infer-extension: rule parse: {e}"))?;

    // Full-recompute mode: clear the target graph first so stale
    // derivations don't accumulate. "append" mode skips the clear.
    // Mirrors wf_infer's refresh-mode dispatch — `SILENT` on CLEAR
    // so a first run against a nonexistent graph is a no-op.
    if rule.refresh_mode == "replace" {
        let clear = format!("CLEAR SILENT GRAPH <{}>", rule.graph);
        execute_update(&clear).map_err(|e| {
            format!("wf_infer-extension: clear graph `{}`: {e}", rule.graph)
        })?;
    } else if rule.refresh_mode != "append" {
        return Err(format!(
            "wf_infer-extension: unknown refresh_mode `{}` (want replace | append)",
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
    let serialized = serde_json::to_string(&payload).map_err(|e| {
        format!("wf_infer-extension: serializing summary: {e}")
    })?;
    Ok(json_literal(&serialized))
}

// ---------------------------------------------------------------------
// Fixed-point iteration — port of wf_infer::iterate_to_fixpoint.
// ---------------------------------------------------------------------

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

// ---------------------------------------------------------------------
// graph-callbacks helpers.
// ---------------------------------------------------------------------

/// Assert `args` is one string literal and return its lexical form.
fn require_literal(args: &[WitTerm], func: &str) -> Result<String, String> {
    let [arg] = args else {
        return Err(format!(
            "{func}: expected 1 argument (rule-json string literal), got {}",
            args.len()
        ));
    };
    match arg {
        WitTerm::Literal(l) => Ok(l.value.clone()),
        WitTerm::NamedNode(_) => {
            Err(format!("{func}: argument must be a string literal, got IRI"))
        }
        WitTerm::BlankNode(_) => Err(format!(
            "{func}: argument must be a string literal, got blank node"
        )),
    }
}

/// Dispatch a SPARQL CONSTRUCT through `graph-callbacks::execute-query`
/// and unwrap the `Quads` arm. Bindings / Boolean arms are reported
/// as an error string — every CONSTRUCT this crate issues should
/// return quads. Falls back to interpreting a `Bindings` arm keyed on
/// s/p/o for engines whose CONSTRUCT-through-execute-query path
/// (deprecated on oxigraph 0.5) returns bindings instead of a graph.
fn query_quads(sparql: &str) -> Result<Vec<WitQuad>, String> {
    match execute_query(sparql)? {
        CallbackQueryResult::Quads(q) => Ok(q),
        // Some engines / callback impls may serialise CONSTRUCT via a
        // synthetic bindings shape (`?s ?p ?o`); accept that too so
        // the migration doesn't couple to a single arm at the
        // graph-callbacks boundary.
        CallbackQueryResult::Bindings(bs) => Ok(bindings_to_quads(bs)),
        CallbackQueryResult::Boolean(_) => Err(
            "wf_infer-extension: CONSTRUCT unexpectedly returned boolean"
                .into(),
        ),
    }
}

/// Group a flat `list<binding>` into rows on the boundary where a
/// variable repeats — mirror of wf_profile-extension's
/// `group_bindings_into_rows`. Then map each row to a Quad keyed on
/// `?s / ?p / ?o`. Any row missing one of the three is skipped.
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

/// Count triples currently in the target graph via a SELECT COUNT.
/// Mirror of wf_infer::graph_size.
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
        gc::GraphCallError::SyntaxError(m) => {
            format!("graph-callbacks syntax-error: {m}")
        }
        gc::GraphCallError::BackendError(m) => {
            format!("graph-callbacks backend-error: {m}")
        }
        gc::GraphCallError::NotPermitted(m) => {
            format!("graph-callbacks not-permitted: {m}")
        }
    })
}

fn execute_update(sparql: &str) -> Result<(), String> {
    gc::execute_update(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => {
            format!("graph-callbacks syntax-error: {m}")
        }
        gc::GraphCallError::BackendError(m) => {
            format!("graph-callbacks backend-error: {m}")
        }
        gc::GraphCallError::NotPermitted(m) => {
            format!("graph-callbacks not-permitted: {m}")
        }
    })
}

// ---------------------------------------------------------------------
// INSERT batching — port of wf_infer::bulk_insert.
// ---------------------------------------------------------------------

/// Chunk size chosen to mirror wf_infer::BATCH_SIZE. Larger batches
/// win network round-trips; smaller batches keep the SPARQL string
/// bounded. 500 is the arbitrary value both crates agree on.
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
    let insert = format!(
        "INSERT DATA {{ GRAPH <{graph}> {{ {triples} }} }}"
    );
    execute_update(&insert)
        .map_err(|e| format!("wf_infer-extension: insert batch: {e}"))
}

/// Serialize a term as its SPARQL surface form. Mirror of
/// wf_infer::value_to_sparql — literal escaping identical.
fn term_to_sparql(t: &WitTerm) -> String {
    match t {
        WitTerm::NamedNode(iri) => format!("<{iri}>"),
        WitTerm::BlankNode(label) => format!("_:{label}"),
        WitTerm::Literal(l) => literal_to_sparql(l),
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
        // WIT `datatype = none` denotes xsd:string per host-callbacks
        // convention — emit as a plain quoted literal so the SPARQL
        // parser applies the RDF 1.1 default.
        format!("\"{escaped}\"")
    }
}

// ---------------------------------------------------------------------
// Return-literal constructors.
// ---------------------------------------------------------------------

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.to_string(),
        datatype: Some(RDF_JSON.to_string()),
        language: None,
    })
}

bindings::export!(Component with_types_in bindings);
