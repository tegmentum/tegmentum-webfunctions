//! wf_pipeline — declarative composition of substrate steps.
//!
//! Signature: `wf:call(<wf_pipeline.wasm>, "<plan-json>")`
//!    → binding-set { step, kind, name, ok, detail }
//!
//! Reads a JSON plan with an ordered list of steps and runs each one
//! against the substrate's host imports. Returns one row per step so
//! the caller can see exactly what happened and where anything failed.
//!
//! Plan shape:
//!
//! ```json
//! {
//!   "name": "canonicalize_then_materialize",
//!   "steps": [
//!     { "kind": "sparql_update",
//!       "name": "clear_derived",
//!       "update":  "CLEAR SILENT GRAPH <urn:derived:person>" },
//!     { "kind": "sparql_query",
//!       "name":  "sanity_count",
//!       "query": "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
//!       "bind":  { "count": "?n" } },
//!     { "kind": "condition",
//!       "name":  "has_person",
//!       "ask":   "ASK { ?s a <urn:Person> }",
//!       "then_steps": [
//!         { "kind": "wasm",
//!           "name": "materialize_person",
//!           "url":  "file:///.../wf_materialize.wasm",
//!           "arg":  "{\"name\": \"person\", \"limit\": ${count}}",
//!           "on_error": { "retry": 2, "delay_ms": 100,
//!                          "fallback_steps": [
//!                            { "kind": "sparql_update",
//!                              "update": "INSERT DATA { <urn:log> <urn:msg> \"skipped\" }" }
//!                          ] } }
//!       ],
//!       "else_steps": [] }
//!   ]
//! }
//! ```
//!
//! Step kinds:
//!
//! * `sparql_query`    — execute-query; detail = row count. `bind` may
//!                        extract cells from the first row into the
//!                        step context.
//! * `sparql_update`   — execute-update; detail = "ok".
//! * `wasm`            — invoke-wasm with a single-string arg (typically
//!                        a descriptor JSON); detail = the guest's first
//!                        row's first cell. `bind` may extract cells too.
//! * `condition`       — evaluate a SPARQL ASK query, then run either
//!                        `then_steps` or `else_steps`. detail is "true"
//!                        or "false"; the boolean itself can be bound via
//!                        `bind: { "myvar": "_ask" }`.
//!
//! v2 scope (this crate):
//!
//! * Sequential execution with structured branching (`condition`).
//! * Inter-step string variables via `bind:` + `${var}` interpolation.
//!   Substitution happens at pre-dispatch time; the guest still sees a
//!   fully-formed SPARQL / arg string. Only scalar strings flow — no
//!   typed binding sets between steps.
//! * Optional `on_error: { retry, delay_ms, fallback_steps }` per step.
//!   Fixed-count retry with linear backoff, then fallback substeps.
//!
//! v3 scope (this crate, additive over v2):
//!
//! * Typed binding-set propagation between steps. A `sparql_query` /
//!   `wasm` step with `bind_full: true` captures its full binding-set
//!   (vars + rows, typed) into the pipeline context under its step name.
//!   Subsequent `${var}` interpolation notices the referenced var is a
//!   full binding-set (not a scalar string) and expands it as a SPARQL
//!   VALUES clause:
//!
//!   ```sparql
//!   VALUES (?a ?b) { ("x" "y") (<urn:iri> "z") }
//!   ```
//!
//!   IRIs render as `<...>`, literals with their datatype/lang tags
//!   preserved.
//!
//! * The `execute-query-with-bindings` host import (added in
//!   `stardog:webfunction@0.6.0`) is available for guests that want to
//!   feed a captured binding-set as pre-seed bindings on the substrate
//!   side rather than through VALUES text interpolation. wf_pipeline's
//!   own `sparql_query` step keeps to string interpolation for
//!   simplicity — the user-visible knob is `${var}` regardless of shape
//!   — but the host surface is there for other guests.
//!
//! * Backwards compat: `bind_full: false` (default) preserves v2 scalar
//!   semantics exactly. Using a full-bind var where a scalar is
//!   expected (e.g. inside a `wasm` step's arg-JSON) fails the step
//!   with a clear error, so mixed use never silently corrupts a query.
//!
//! On step failure (after retries + fallback exhausted): emit an error
//! row and STOP; later steps don't run. Conditional branches inherit
//! the same stop-on-failure semantic scoped to the outer pipeline.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use std::collections::BTreeMap;

use serde::Deserialize;

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

// ---------------------------------------------------------------------------
// Plan JSON shape
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Plan {
    #[allow(dead_code)]
    #[serde(default)]
    name: String,
    steps: Vec<Step>,
}

#[derive(Deserialize, Clone, Default)]
struct Step {
    kind: String,
    #[serde(default)]
    name: Option<String>,
    // sparql_query / sparql_update
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    update: Option<String>,
    // wasm
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    arg: Option<String>,
    // condition
    #[serde(default)]
    ask: Option<String>,
    #[serde(default)]
    then_steps: Vec<Step>,
    #[serde(default)]
    else_steps: Vec<Step>,
    // v2: inter-step variables
    #[serde(default)]
    bind: BTreeMap<String, String>,
    /// v3: when true, the step's full binding-set (vars + rows, typed)
    /// is captured into the pipeline context under the step's `name`.
    /// Each key in `bind` continues to work as a scalar bind against
    /// the first row, so a step may capture both a scalar (e.g. row
    /// count) and its full grid in one shot. Requires `name` to be set.
    #[serde(default)]
    bind_full: bool,
    // v2: error recovery
    #[serde(default)]
    on_error: Option<OnError>,
}

#[derive(Deserialize, Clone, Default)]
struct OnError {
    /// Number of retries *after* the first attempt. `retry: 2` means up
    /// to 3 total attempts.
    #[serde(default)]
    retry: u32,
    /// Linear backoff base in milliseconds. Attempt N (1-indexed after
    /// the first) sleeps `delay_ms * N` before firing.
    #[serde(default)]
    delay_ms: u64,
    /// Steps to run if all retries fail. If the fallback substeps all
    /// succeed the pipeline continues; if any fallback substep fails
    /// the pipeline halts as usual.
    #[serde(default)]
    fallback_steps: Vec<Step>,
}

// ---------------------------------------------------------------------------
// Substrate abstraction — lets tests inject a mock; the Guest impl uses
// the WIT host imports.
// ---------------------------------------------------------------------------

/// A typed cell in a query outcome. Preserves SPARQL term shape so v3's
/// VALUES-clause expansion can render IRIs as `<...>` and literals with
/// datatype/lang decoration. `as_str` yields the raw lexical form for
/// v2's scalar bind path so old behaviour is preserved.
#[derive(Clone, Debug)]
enum Cell {
    Iri(String),
    Literal {
        label: String,
        datatype: String,
        lang: Option<String>,
    },
    Bnode(String),
}

impl Cell {
    fn as_str(&self) -> String {
        match self {
            Cell::Iri(s) => s.clone(),
            Cell::Literal { label, .. } => label.clone(),
            Cell::Bnode(s) => format!("_:{s}"),
        }
    }

    /// SPARQL term form as it appears inside a VALUES clause. Blank
    /// nodes aren't valid in VALUES; we emit UNDEF (per SPARQL 1.1) so
    /// downstream queries at least parse.
    fn as_sparql(&self) -> String {
        match self {
            Cell::Iri(s) => format!("<{s}>"),
            Cell::Literal {
                label,
                datatype,
                lang,
            } => {
                let escaped = escape_sparql_string(label);
                if let Some(tag) = lang {
                    format!("\"{escaped}\"@{tag}")
                } else if datatype == "http://www.w3.org/2001/XMLSchema#string"
                    || datatype.is_empty()
                {
                    format!("\"{escaped}\"")
                } else {
                    format!("\"{escaped}\"^^<{datatype}>")
                }
            }
            Cell::Bnode(_) => "UNDEF".to_string(),
        }
    }
}

fn escape_sparql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
struct QueryOutcome {
    vars: Vec<String>,
    /// One row per result; each cell is (var-name, typed-value).
    rows: Vec<Vec<(String, Cell)>>,
}

trait Substrate {
    fn execute_query(&self, sparql: &str) -> Result<QueryOutcome, String>;
    fn execute_update(&self, update: &str) -> Result<(), String>;
    fn invoke_wasm(&self, url: &str, arg: Option<&str>) -> Result<QueryOutcome, String>;
    fn execute_ask(&self, sparql: &str) -> Result<bool, String>;
    /// Backoff hook. Real Guest sleeps via std; the mock counts calls.
    fn sleep_ms(&self, _ms: u64) {}
}

// ---------------------------------------------------------------------------
// Interpolation
// ---------------------------------------------------------------------------

/// A pipeline-context value. Scalars are v2's shape — a single string
/// substituted into `${var}` at pre-dispatch time. Full binding-sets
/// are v3 — captured via `bind_full: true`, expanded as a SPARQL
/// `VALUES` clause when interpolated.
#[derive(Clone, Debug)]
enum CtxValue {
    Scalar(String),
    Full(QueryOutcome),
}

#[derive(Default, Clone, Debug)]
struct Context {
    vars: BTreeMap<String, CtxValue>,
}

/// Render a binding-set as a SPARQL `VALUES (?a ?b) { ("x" "y") ... }`
/// clause. An empty rows list produces `VALUES (?a ?b) { }` — a valid
/// SPARQL construct that constrains to no rows, which stops downstream
/// queries from returning anything, but at least parses.
fn render_values_clause(oc: &QueryOutcome) -> String {
    let mut out = String::from("VALUES (");
    for (i, v) in oc.vars.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('?');
        out.push_str(v);
    }
    out.push_str(") {");
    for row in &oc.rows {
        out.push_str(" (");
        for (vi, var) in oc.vars.iter().enumerate() {
            if vi > 0 {
                out.push(' ');
            }
            match row.iter().find(|(n, _)| n == var).map(|(_, c)| c) {
                Some(cell) => out.push_str(&cell.as_sparql()),
                None => out.push_str("UNDEF"),
            }
        }
        out.push(')');
    }
    if !oc.rows.is_empty() {
        out.push(' ');
    } else {
        out.push(' ');
    }
    out.push('}');
    out
}

/// Where the interpolated string is going. SPARQL-mode expansion allows
/// full binding-sets to become VALUES clauses; scalar-mode (used by the
/// `wasm` step's `arg` JSON blob and by `condition`'s ASK query when
/// splicing outside a SPARQL grammar position) rejects them with a
/// clear error — mixed use never silently corrupts downstream input.
#[derive(Copy, Clone, Debug)]
enum InterpMode {
    Sparql,
    Scalar,
}

/// Replace `${var}` occurrences with values from `ctx`. Returns an error
/// on an unbound variable or an unterminated `${...}`. Escaping is not
/// supported in v2 — if you need a literal `${...}` in a query, bind a
/// var whose value is that literal.
///
/// v3: full binding-set vars expand as a SPARQL `VALUES (...) { ... }`
/// clause when interpolated in Sparql mode. Using a full-bind var in
/// Scalar mode (e.g. inside a `wasm` step's arg-JSON) fails the step
/// with a clear "mixed use" error.
fn interpolate(s: &str, ctx: &Context) -> Result<String, String> {
    interpolate_mode(s, ctx, InterpMode::Sparql)
}

fn interpolate_mode(s: &str, ctx: &Context, mode: InterpMode) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'}' {
                end += 1;
            }
            if end == bytes.len() {
                return Err(format!("unterminated ${{...}} in `{s}`"));
            }
            let var = std::str::from_utf8(&bytes[start..end])
                .map_err(|e| format!("interpolation: {e}"))?;
            match ctx.vars.get(var) {
                Some(CtxValue::Scalar(v)) => out.push_str(v),
                Some(CtxValue::Full(oc)) => match mode {
                    InterpMode::Sparql => out.push_str(&render_values_clause(oc)),
                    InterpMode::Scalar => {
                        return Err(format!(
                            "variable `${{{var}}}` is a full binding-set                              (captured via bind_full) but was used in a                              non-SPARQL scalar context; use it inside a                              sparql_query or sparql_update step's text                              instead"
                        ));
                    }
                },
                None => return Err(format!("unbound variable `${{{var}}}`")),
            }
            i = end + 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

struct Runner<'s, S: Substrate> {
    substrate: &'s S,
    rows: Vec<Vec<Binding>>,
    ctx: Context,
    counter: usize,
}

impl<'s, S: Substrate> Runner<'s, S> {
    fn new(substrate: &'s S) -> Self {
        Self {
            substrate,
            rows: Vec::new(),
            ctx: Context::default(),
            counter: 0,
        }
    }

    /// Returns true if the pipeline should continue past `steps`.
    fn run_steps(&mut self, steps: &[Step]) -> bool {
        for step in steps {
            if !self.run_step(step) {
                return false;
            }
        }
        true
    }

    fn run_step(&mut self, step: &Step) -> bool {
        let idx = self.counter as i64;
        self.counter += 1;
        let step_name = step
            .name
            .clone()
            .unwrap_or_else(|| format!("step_{idx}"));

        if step.kind == "condition" {
            return self.run_condition(idx, &step_name, step);
        }

        let retries = step.on_error.as_ref().map(|o| o.retry).unwrap_or(0);
        let delay_ms = step.on_error.as_ref().map(|o| o.delay_ms).unwrap_or(0);

        let mut last_err = String::new();
        for attempt in 0..=retries {
            if attempt > 0 && delay_ms > 0 {
                self.substrate.sleep_ms(delay_ms * attempt as u64);
            }
            match self.dispatch(step) {
                Ok((detail, binds)) => {
                    for (k, v) in binds {
                        self.ctx.vars.insert(k, v);
                    }
                    self.rows
                        .push(build_row(idx, &step.kind, &step_name, true, &detail));
                    return true;
                }
                Err(e) => {
                    last_err = e;
                }
            }
        }

        // Retries exhausted. Emit the failure row.
        self.rows
            .push(build_row(idx, &step.kind, &step_name, false, &last_err));

        // Fallback?
        if let Some(oe) = step.on_error.as_ref() {
            if !oe.fallback_steps.is_empty() {
                let fb = oe.fallback_steps.clone();
                return self.run_steps(&fb);
            }
        }
        false
    }

    fn run_condition(&mut self, idx: i64, step_name: &str, step: &Step) -> bool {
        let ask = match step.ask.as_deref() {
            Some(a) => a,
            None => {
                self.rows.push(build_row(
                    idx,
                    "condition",
                    step_name,
                    false,
                    "condition: missing `ask`",
                ));
                return false;
            }
        };
        let ask = match interpolate(ask, &self.ctx) {
            Ok(s) => s,
            Err(e) => {
                self.rows
                    .push(build_row(idx, "condition", step_name, false, &e));
                return false;
            }
        };
        let b = match self.substrate.execute_ask(&ask) {
            Ok(v) => v,
            Err(e) => {
                self.rows.push(build_row(
                    idx,
                    "condition",
                    step_name,
                    false,
                    &format!("condition: {e}"),
                ));
                return false;
            }
        };
        let detail = if b { "true" } else { "false" };
        self.rows
            .push(build_row(idx, "condition", step_name, true, detail));

        // Only `_ask` is meaningful for condition binds.
        for (name, source) in &step.bind {
            if source == "_ask" || source == "?_ask" {
                self.ctx
                    .vars
                    .insert(name.clone(), CtxValue::Scalar(detail.to_string()));
            }
        }

        let branch = if b { &step.then_steps } else { &step.else_steps };
        self.run_steps(branch)
    }

    /// One attempt at a non-condition step. Returns (detail, binds).
    /// v3: when `step.bind_full` is set, the full binding-set is also
    /// captured, keyed by the step's name.
    fn dispatch(&self, step: &Step) -> Result<(String, Vec<(String, CtxValue)>), String> {
        match step.kind.as_str() {
            "sparql_query" => {
                let q = step
                    .query
                    .as_deref()
                    .ok_or_else(|| "sparql_query: missing `query`".to_string())?;
                let q = interpolate(q, &self.ctx)?;
                let bs = self
                    .substrate
                    .execute_query(&q)
                    .map_err(|e| format!("sparql_query: {e}"))?;
                let detail = format!("{} rows", bs.rows.len());
                let mut binds = extract_binds(&step.bind, &bs);
                capture_full(step, &bs, &mut binds)?;
                Ok((detail, binds))
            }
            "sparql_update" => {
                let u = step
                    .update
                    .as_deref()
                    .ok_or_else(|| "sparql_update: missing `update`".to_string())?;
                let u = interpolate(u, &self.ctx)?;
                self.substrate
                    .execute_update(&u)
                    .map_err(|e| format!("sparql_update: {e}"))?;
                Ok(("ok".to_string(), Vec::new()))
            }
            "wasm" => {
                let url = step
                    .url
                    .as_deref()
                    .ok_or_else(|| "wasm: missing `url`".to_string())?;
                // URL is scalar text, not a SPARQL grammar position.
                let url = interpolate_mode(url, &self.ctx, InterpMode::Scalar)?;
                let arg_string = match step.arg.as_deref() {
                    // The wasm arg is an opaque scalar payload (usually
                    // a JSON blob). Splicing a VALUES clause into it is
                    // a category error — reject with a clear message.
                    Some(a) => Some(interpolate_mode(a, &self.ctx, InterpMode::Scalar)?),
                    None => None,
                };
                let bs = self
                    .substrate
                    .invoke_wasm(&url, arg_string.as_deref())
                    .map_err(|e| format!("wasm: {e}"))?;
                let detail = bs
                    .rows
                    .first()
                    .and_then(|r| r.first())
                    .map(|(_, c)| c.as_str())
                    .unwrap_or_else(|| "no output rows".to_string());
                let mut binds = extract_binds(&step.bind, &bs);
                capture_full(step, &bs, &mut binds)?;
                Ok((detail, binds))
            }
            other => Err(format!(
                "unknown step kind `{other}` (want: sparql_query | \
                 sparql_update | wasm | condition)"
            )),
        }
    }
}

/// v3 helper: if `bind_full: true`, capture the full binding-set into
/// context under the step's `name`. Requires `name` to be set — the
/// binding-set has no other stable handle to key under.
fn capture_full(
    step: &Step,
    bs: &QueryOutcome,
    binds: &mut Vec<(String, CtxValue)>,
) -> Result<(), String> {
    if !step.bind_full {
        return Ok(());
    }
    let Some(name) = step.name.as_ref() else {
        return Err(
            "bind_full: true requires the step to have a `name` (the \
             captured binding-set is keyed by step name)"
                .to_string(),
        );
    };
    binds.push((name.clone(), CtxValue::Full(bs.clone())));
    Ok(())
}

/// Given a `bind` map (context-name -> source-var-name) and a query
/// outcome, produce the (context-name, ctx-value) pairs. Missing source
/// vars are simply skipped — the pipeline shouldn't fail because a
/// `SELECT (COUNT(*) AS ?n)` came back with zero rows, but any bind
/// referencing `?n` in that case just doesn't fire. Always emits
/// `CtxValue::Scalar` (lexical form) — v3's full-binding-set capture is
/// separate, driven by `bind_full` on the step.
fn extract_binds(
    bind_map: &BTreeMap<String, String>,
    bs: &QueryOutcome,
) -> Vec<(String, CtxValue)> {
    if bind_map.is_empty() {
        return Vec::new();
    }
    let first = match bs.rows.first() {
        Some(r) => r,
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(bind_map.len());
    for (ctx_name, source) in bind_map {
        let stripped = source.strip_prefix('?').unwrap_or(source);
        if let Some((_, cell)) = first.iter().find(|(n, _)| n == stripped) {
            out.push((ctx_name.clone(), CtxValue::Scalar(cell.as_str())));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Guest impl — wires the real WIT host imports into a Substrate.
// ---------------------------------------------------------------------------

struct HostSubstrate;

impl Substrate for HostSubstrate {
    fn execute_query(&self, sparql: &str) -> Result<QueryOutcome, String> {
        let bs = host::execute_query(sparql, &[], None).map_err(|e| e.to_string())?;
        Ok(bs_to_outcome(bs))
    }
    fn execute_update(&self, update: &str) -> Result<(), String> {
        host::execute_update(update).map_err(|e| e.to_string())
    }
    fn invoke_wasm(&self, url: &str, arg: Option<&str>) -> Result<QueryOutcome, String> {
        let arg_val = arg.map(|s| {
            Value::Literal(Literal {
                label: s.to_string(),
                datatype: XSD_STRING.into(),
                lang: None,
            })
        });
        let args: Vec<Value> = arg_val.into_iter().collect();
        let bs = host::invoke_wasm(url, &args).map_err(|e| e.to_string())?;
        Ok(bs_to_outcome(bs))
    }
    fn execute_ask(&self, sparql: &str) -> Result<bool, String> {
        let bs = host::execute_query(sparql, &[], None).map_err(|e| e.to_string())?;
        // ASK results come back as vars=["_ask"] with one row whose sole
        // binding is a boolean literal labeled "true" or "false".
        let cell = bs
            .rows
            .first()
            .and_then(|r| r.first())
            .ok_or_else(|| "ASK returned no rows".to_string())?;
        match &cell.value {
            Value::Literal(l) => match l.label.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                other => Err(format!("ASK returned non-boolean literal `{other}`")),
            },
            _ => Err("ASK returned non-literal".into()),
        }
    }
    fn sleep_ms(&self, ms: u64) {
        // Best-effort backoff. In a wasi-p2 component this maps to the
        // clocks import; if the host doesn't provide it, sleep is a
        // no-op and the retry fires immediately.
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

fn bs_to_outcome(bs: BindingSets) -> QueryOutcome {
    let vars = bs.vars.clone();
    let rows = bs
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|b| (b.name.clone(), value_to_cell(&b.value)))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    QueryOutcome { vars, rows }
}

/// WIT `value` variant → the typed `Cell` used inside a QueryOutcome.
/// Preserves the SPARQL term shape so v3's VALUES-clause rendering can
/// emit IRIs as `<...>` and literals with the right decoration.
fn value_to_cell(v: &Value) -> Cell {
    match v {
        Value::Iri(s) => Cell::Iri(s.clone()),
        Value::Literal(l) => Cell::Literal {
            label: l.label.clone(),
            datatype: l.datatype.clone(),
            lang: l.lang.clone(),
        },
        Value::Bnode(s) => Cell::Bnode(s.clone()),
    }
}

/// The v3 host import: execute a SPARQL query with a full pre-seed
/// binding-sets matrix. Currently unused by wf_pipeline itself (the
/// `sparql_query` step interpolates VALUES via string substitution for
/// simplicity), but exposed so other guests in the workspace can call
/// through wf_pipeline's WIT world without re-declaring imports.
#[allow(dead_code)]
fn execute_query_with_bindings(
    sparql: &str,
    seed: &QueryOutcome,
    max_rows: Option<u32>,
) -> Result<QueryOutcome, String> {
    let seed_wit = outcome_to_wit_binding_sets(seed);
    let bs = host::execute_query_with_bindings(sparql, &seed_wit, max_rows)
        .map_err(|e| e.to_string())?;
    Ok(bs_to_outcome(bs))
}

#[allow(dead_code)]
fn outcome_to_wit_binding_sets(oc: &QueryOutcome) -> BindingSets {
    let rows = oc
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|(name, cell)| Binding {
                    name: name.clone(),
                    value: cell_to_wit_value(cell),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    BindingSets {
        vars: oc.vars.clone(),
        rows,
    }
}

#[allow(dead_code)]
fn cell_to_wit_value(cell: &Cell) -> Value {
    match cell {
        Cell::Iri(s) => Value::Iri(s.clone()),
        Cell::Literal {
            label,
            datatype,
            lang,
        } => Value::Literal(Literal {
            label: label.clone(),
            datatype: datatype.clone(),
            lang: lang.clone(),
        }),
        Cell::Bnode(s) => Value::Bnode(s.clone()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let plan_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_pipeline: first arg must be a plan-json string literal".into(),
                );
            }
        };
        let plan: Plan = serde_json::from_str(&plan_json)
            .map_err(|e| format!("wf_pipeline: plan parse: {e}"))?;

        let substrate = HostSubstrate;
        let mut runner = Runner::new(&substrate);
        runner.run_steps(&plan.steps);

        Ok(BindingSets {
            vars: vec![
                "step".into(),
                "kind".into(),
                "name".into(),
                "ok".into(),
                "detail".into(),
            ],
            rows: runner.rows,
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_pipeline: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_pipeline: aggregate not applicable".into())
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
                    label: "wf_pipeline(\"<plan-json>\") — composition of \
                            substrate steps (sparql_query, sparql_update, \
                            wasm, condition). v2 adds condition branches, \
                            ${var} interpolation via `bind`, and \
                            on_error retry/fallback. v3 adds typed \
                            binding-set propagation via `bind_full`: a \
                            step's full row grid captured under its name, \
                            expanded as a SPARQL VALUES clause when \
                            interpolated downstream. One row per step. \
                            Stops on first unrecovered failure."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Row assembly
// ---------------------------------------------------------------------------

fn build_row(idx: i64, kind: &str, name: &str, ok: bool, detail: &str) -> Vec<Binding> {
    vec![
        Binding {
            name: "step".into(),
            value: int_lit(idx),
        },
        Binding {
            name: "kind".into(),
            value: string_lit(kind),
        },
        Binding {
            name: "name".into(),
            value: string_lit(name),
        },
        Binding {
            name: "ok".into(),
            value: bool_lit(ok),
        },
        Binding {
            name: "detail".into(),
            value: string_lit(detail),
        },
    ]
}

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

fn bool_lit(b: bool) -> Value {
    Value::Literal(Literal {
        label: if b { "true".into() } else { "false".into() },
        datatype: XSD_BOOLEAN.into(),
        lang: None,
    })
}

export!(Component);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Mock substrate. Each call kind pops from a scripted queue so tests
    /// can precisely stage per-attempt outcomes (needed for retry tests).
    #[derive(Default)]
    struct MockSubstrate {
        query_results: RefCell<Vec<Result<QueryOutcome, String>>>,
        update_results: RefCell<Vec<Result<(), String>>>,
        wasm_results: RefCell<Vec<Result<QueryOutcome, String>>>,
        ask_results: RefCell<Vec<Result<bool, String>>>,
        queries_seen: RefCell<Vec<String>>,
        updates_seen: RefCell<Vec<String>>,
        wasm_calls_seen: RefCell<Vec<(String, Option<String>)>>,
        asks_seen: RefCell<Vec<String>>,
        sleeps: RefCell<Vec<u64>>,
    }

    impl MockSubstrate {
        fn stage_query(&self, r: Result<QueryOutcome, String>) {
            self.query_results.borrow_mut().push(r);
        }
        fn stage_update(&self, r: Result<(), String>) {
            self.update_results.borrow_mut().push(r);
        }
        fn stage_wasm(&self, r: Result<QueryOutcome, String>) {
            self.wasm_results.borrow_mut().push(r);
        }
        fn stage_ask(&self, r: Result<bool, String>) {
            self.ask_results.borrow_mut().push(r);
        }
    }

    impl Substrate for MockSubstrate {
        fn execute_query(&self, sparql: &str) -> Result<QueryOutcome, String> {
            self.queries_seen.borrow_mut().push(sparql.to_string());
            self.query_results
                .borrow_mut()
                .remove(0)
        }
        fn execute_update(&self, update: &str) -> Result<(), String> {
            self.updates_seen.borrow_mut().push(update.to_string());
            self.update_results
                .borrow_mut()
                .remove(0)
        }
        fn invoke_wasm(&self, url: &str, arg: Option<&str>) -> Result<QueryOutcome, String> {
            self.wasm_calls_seen
                .borrow_mut()
                .push((url.to_string(), arg.map(|s| s.to_string())));
            self.wasm_results
                .borrow_mut()
                .remove(0)
        }
        fn execute_ask(&self, sparql: &str) -> Result<bool, String> {
            self.asks_seen.borrow_mut().push(sparql.to_string());
            self.ask_results
                .borrow_mut()
                .remove(0)
        }
        fn sleep_ms(&self, ms: u64) {
            self.sleeps.borrow_mut().push(ms);
        }
    }

    fn row_get(row: &[Binding], name: &str) -> String {
        row.iter()
            .find(|b| b.name == name)
            .map(|b| match &b.value {
                Value::Literal(l) => l.label.clone(),
                Value::Iri(s) => s.clone(),
                Value::Bnode(s) => format!("_:{s}"),
            })
            .unwrap_or_default()
    }

    fn outcome(vars: &[&str], rows: Vec<Vec<(&str, &str)>>) -> QueryOutcome {
        QueryOutcome {
            vars: vars.iter().map(|s| s.to_string()).collect(),
            rows: rows
                .into_iter()
                .map(|r| {
                    r.into_iter()
                        .map(|(n, v)| (n.to_string(), Cell::Literal {
                            label: v.to_string(),
                            datatype: XSD_STRING.into(),
                            lang: None,
                        }))
                        .collect()
                })
                .collect(),
        }
    }

    /// Test-only helper: build a QueryOutcome whose cells are typed IRIs.
    /// Useful for the v3 VALUES-expansion tests, which need `<...>` in
    /// the rendered clause.
    fn outcome_iri(vars: &[&str], rows: Vec<Vec<(&str, &str)>>) -> QueryOutcome {
        QueryOutcome {
            vars: vars.iter().map(|s| s.to_string()).collect(),
            rows: rows
                .into_iter()
                .map(|r| {
                    r.into_iter()
                        .map(|(n, v)| (n.to_string(), Cell::Iri(v.to_string())))
                        .collect()
                })
                .collect(),
        }
    }

    // ----- interpolation -----

    #[test]
    fn interpolate_basic() {
        let mut ctx = Context::default();
        ctx.vars
            .insert("count".into(), CtxValue::Scalar("3".into()));
        ctx.vars
            .insert("name".into(), CtxValue::Scalar("person".into()));
        let out =
            interpolate("SELECT * WHERE { ?s a :${name} } LIMIT ${count}", &ctx).unwrap();
        assert_eq!(out, "SELECT * WHERE { ?s a :person } LIMIT 3");
    }

    #[test]
    fn interpolate_unbound_var_errors_clearly() {
        let ctx = Context::default();
        let err = interpolate("LIMIT ${missing}", &ctx).unwrap_err();
        assert!(err.contains("unbound"), "err was: {err}");
        assert!(err.contains("missing"), "err was: {err}");
    }

    #[test]
    fn interpolate_unterminated_errors() {
        let ctx = Context::default();
        let err = interpolate("LIMIT ${oops", &ctx).unwrap_err();
        assert!(err.contains("unterminated"), "err was: {err}");
    }

    // ----- condition -----

    #[test]
    fn condition_true_runs_then_branch() {
        let plan_json = r#"{
            "name": "c",
            "steps": [
              { "kind": "condition",
                "name": "chk",
                "ask": "ASK { ?s a :T }",
                "then_steps": [
                  { "kind": "sparql_update",
                    "name": "yes",
                    "update": "INSERT DATA { <a> <b> \"then\" }" }
                ],
                "else_steps": [
                  { "kind": "sparql_update",
                    "name": "no",
                    "update": "INSERT DATA { <a> <b> \"else\" }" }
                ] }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_ask(Ok(true));
        sub.stage_update(Ok(()));
        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        // condition row + then-branch update row
        assert_eq!(r.rows.len(), 2);
        assert_eq!(row_get(&r.rows[0], "kind"), "condition");
        assert_eq!(row_get(&r.rows[0], "detail"), "true");
        assert_eq!(row_get(&r.rows[1], "name"), "yes");
        assert_eq!(row_get(&r.rows[1], "ok"), "true");
        assert_eq!(sub.updates_seen.borrow().len(), 1);
        assert!(sub.updates_seen.borrow()[0].contains("then"));
    }

    #[test]
    fn condition_false_runs_else_branch() {
        let plan_json = r#"{
            "name": "c",
            "steps": [
              { "kind": "condition",
                "ask": "ASK { ?s a :T }",
                "then_steps": [
                  { "kind": "sparql_update",
                    "update": "INSERT DATA { <a> <b> \"then\" }" }
                ],
                "else_steps": [
                  { "kind": "sparql_update",
                    "update": "INSERT DATA { <a> <b> \"else\" }" }
                ] }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_ask(Ok(false));
        sub.stage_update(Ok(()));
        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        assert_eq!(r.rows.len(), 2);
        assert_eq!(row_get(&r.rows[0], "detail"), "false");
        assert!(sub.updates_seen.borrow()[0].contains("else"));
    }

    // ----- inter-step bind -----

    #[test]
    fn bind_from_step_a_interpolates_into_step_b() {
        let plan_json = r#"{
            "name": "b",
            "steps": [
              { "kind": "sparql_query",
                "name": "count",
                "query": "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
                "bind": { "count": "?n" } },
              { "kind": "sparql_query",
                "name": "limited",
                "query": "SELECT * WHERE { ?s ?p ?o } LIMIT ${count}" }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_query(Ok(outcome(&["n"], vec![vec![("n", "3")]])));
        sub.stage_query(Ok(outcome(
            &["s", "p", "o"],
            vec![
                vec![("s", "a"), ("p", "b"), ("o", "c")],
                vec![("s", "d"), ("p", "e"), ("o", "f")],
                vec![("s", "g"), ("p", "h"), ("o", "i")],
            ],
        )));

        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        assert_eq!(r.rows.len(), 2);
        assert_eq!(row_get(&r.rows[0], "ok"), "true");
        assert_eq!(row_get(&r.rows[1], "ok"), "true");
        assert_eq!(row_get(&r.rows[1], "detail"), "3 rows");
        // The second query must have been interpolated with count=3.
        assert!(
            sub.queries_seen.borrow()[1].ends_with("LIMIT 3"),
            "second query was: {}",
            sub.queries_seen.borrow()[1]
        );
        match r.ctx.vars.get("count").cloned() {
            Some(CtxValue::Scalar(s)) => assert_eq!(s, "3"),
            other => panic!("expected Scalar(\"3\"), got {other:?}"),
        }
    }

    #[test]
    fn unbound_interpolation_fails_step() {
        let plan_json = r#"{
            "name": "u",
            "steps": [
              { "kind": "sparql_query",
                "query": "SELECT * WHERE { ?s ?p ?o } LIMIT ${nope}" }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        assert_eq!(r.rows.len(), 1);
        assert_eq!(row_get(&r.rows[0], "ok"), "false");
        assert!(row_get(&r.rows[0], "detail").contains("unbound"));
    }

    // ----- on_error retry -----

    #[test]
    fn on_error_retry_succeeds_on_last_attempt() {
        // retry: 2 → up to 3 total attempts. Fail twice then succeed.
        let plan_json = r#"{
            "name": "r",
            "steps": [
              { "kind": "sparql_update",
                "name": "flaky",
                "update": "INSERT DATA { <a> <b> \"c\" }",
                "on_error": { "retry": 2, "delay_ms": 10 } }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_update(Err("transient 1".into()));
        sub.stage_update(Err("transient 2".into()));
        sub.stage_update(Ok(()));

        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        assert_eq!(r.rows.len(), 1);
        assert_eq!(row_get(&r.rows[0], "ok"), "true");
        assert_eq!(row_get(&r.rows[0], "detail"), "ok");
        assert_eq!(sub.updates_seen.borrow().len(), 3);
        // Linear backoff: sleeps before attempts 1 and 2 → 10, 20.
        assert_eq!(sub.sleeps.borrow().as_slice(), &[10u64, 20u64]);
    }

    #[test]
    fn on_error_all_retries_exhaust_then_run_fallback() {
        let plan_json = r#"{
            "name": "f",
            "steps": [
              { "kind": "sparql_update",
                "name": "will_fail",
                "update": "INSERT DATA { <a> <b> \"c\" }",
                "on_error": {
                  "retry": 1,
                  "delay_ms": 0,
                  "fallback_steps": [
                    { "kind": "sparql_update",
                      "name": "fb",
                      "update": "INSERT DATA { <urn:log> <urn:msg> \"skipped\" }" }
                  ]
                }
              },
              { "kind": "sparql_update",
                "name": "after",
                "update": "INSERT DATA { <x> <y> \"z\" }" }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_update(Err("perm".into()));
        sub.stage_update(Err("perm".into()));
        // fallback update:
        sub.stage_update(Ok(()));
        // "after" step:
        sub.stage_update(Ok(()));

        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        // Rows: failure row, fallback row, after row.
        assert_eq!(r.rows.len(), 3, "rows: {:#?}", r.rows);
        assert_eq!(row_get(&r.rows[0], "name"), "will_fail");
        assert_eq!(row_get(&r.rows[0], "ok"), "false");
        assert_eq!(row_get(&r.rows[1], "name"), "fb");
        assert_eq!(row_get(&r.rows[1], "ok"), "true");
        assert_eq!(row_get(&r.rows[2], "name"), "after");
        assert_eq!(row_get(&r.rows[2], "ok"), "true");
        // Attempted 2 times (retry=1 → 2 total) + 1 fallback + 1 after = 4.
        assert_eq!(sub.updates_seen.borrow().len(), 4);
    }

    #[test]
    fn wasm_bind_and_interpolate() {
        let plan_json = r#"{
            "name": "w",
            "steps": [
              { "kind": "wasm",
                "name": "produce",
                "url": "file:///produce.wasm",
                "arg": "{\"n\": 5}",
                "bind": { "answer": "?out" } },
              { "kind": "wasm",
                "name": "consume",
                "url": "file:///consume.wasm",
                "arg": "{\"prev\": \"${answer}\"}" }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_wasm(Ok(outcome(&["out"], vec![vec![("out", "42")]])));
        sub.stage_wasm(Ok(outcome(&["ok"], vec![vec![("ok", "done")]])));

        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        assert_eq!(r.rows.len(), 2);
        assert_eq!(row_get(&r.rows[0], "detail"), "42");
        assert_eq!(row_get(&r.rows[1], "detail"), "done");
        let seen = sub.wasm_calls_seen.borrow();
        assert_eq!(seen[1].1.as_deref(), Some("{\"prev\": \"42\"}"));
    }

    // ----- v3: bind_full / VALUES expansion / mixed use -----

    #[test]
    fn bind_full_captures_binding_set() {
        // A sparql_query step with bind_full: true captures the entire
        // binding-set (vars + rows) under its name, not just a scalar.
        let plan_json = r#"{
            "name": "cap",
            "steps": [
              { "kind": "sparql_query",
                "name": "people",
                "query": "SELECT ?p ?age WHERE { ?p a :Person ; :age ?age }",
                "bind_full": true }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_query(Ok(outcome(
            &["p", "age"],
            vec![
                vec![("p", "alice"), ("age", "30")],
                vec![("p", "bob"), ("age", "25")],
            ],
        )));

        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        assert_eq!(r.rows.len(), 1);
        assert_eq!(row_get(&r.rows[0], "ok"), "true");
        // The ctx should now hold a Full binding-set under "people".
        match r.ctx.vars.get("people") {
            Some(CtxValue::Full(oc)) => {
                assert_eq!(oc.vars, vec!["p".to_string(), "age".to_string()]);
                assert_eq!(oc.rows.len(), 2);
            }
            other => panic!("expected Full binding-set under `people`, got {other:?}"),
        }
    }

    #[test]
    fn interpolation_expands_binding_set_as_values() {
        // A captured binding-set interpolated into a downstream SPARQL
        // query expands as `VALUES (?p ?age) { ("alice" "30") ... }`.
        let plan_json = r#"{
            "name": "expand",
            "steps": [
              { "kind": "sparql_query",
                "name": "people",
                "query": "SELECT ?p ?age WHERE { ?p a :Person ; :age ?age }",
                "bind_full": true },
              { "kind": "sparql_query",
                "name": "join",
                "query": "SELECT * WHERE { ${people} ?p :city ?c }" }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_query(Ok(outcome(
            &["p", "age"],
            vec![
                vec![("p", "alice"), ("age", "30")],
                vec![("p", "bob"), ("age", "25")],
            ],
        )));
        sub.stage_query(Ok(outcome(&["p", "c"], vec![])));

        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        assert_eq!(r.rows.len(), 2, "rows: {:#?}", r.rows);
        assert_eq!(row_get(&r.rows[0], "ok"), "true");
        assert_eq!(row_get(&r.rows[1], "ok"), "true");

        let seen = sub.queries_seen.borrow();
        let second = &seen[1];
        assert!(
            second.contains("VALUES (?p ?age)"),
            "expected VALUES header in second query, got: {second}"
        );
        assert!(
            second.contains("(\"alice\" \"30\")"),
            "expected alice row in second query, got: {second}"
        );
        assert!(
            second.contains("(\"bob\" \"25\")"),
            "expected bob row in second query, got: {second}"
        );
    }

    #[test]
    fn bind_full_iri_expansion_uses_angle_brackets() {
        // IRIs in a captured binding-set render as `<...>` in the VALUES
        // clause, not as string literals — otherwise downstream joins
        // against IRI predicates would silently miss.
        let plan_json = r#"{
            "name": "iri",
            "steps": [
              { "kind": "sparql_query",
                "name": "who",
                "query": "SELECT ?p WHERE { ?p a :Person }",
                "bind_full": true },
              { "kind": "sparql_query",
                "name": "join",
                "query": "SELECT * WHERE { ${who} ?p :name ?n }" }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_query(Ok(outcome_iri(
            &["p"],
            vec![
                vec![("p", "http://ex/alice")],
                vec![("p", "http://ex/bob")],
            ],
        )));
        sub.stage_query(Ok(outcome(&["p", "n"], vec![])));

        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        let seen = sub.queries_seen.borrow();
        let second = &seen[1];
        assert!(
            second.contains("(<http://ex/alice>)"),
            "expected angle-bracketed alice, got: {second}"
        );
        assert!(
            second.contains("(<http://ex/bob>)"),
            "expected angle-bracketed bob, got: {second}"
        );
    }

    #[test]
    fn scalar_bind_still_works() {
        // Explicit backwards-compat check: with bind_full: false (the
        // default) a scalar bind from v2 flows unchanged as a plain
        // string substitution.
        let plan_json = r#"{
            "name": "compat",
            "steps": [
              { "kind": "sparql_query",
                "name": "cnt",
                "query": "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
                "bind": { "count": "?n" } },
              { "kind": "sparql_query",
                "name": "limited",
                "query": "SELECT * WHERE { ?s ?p ?o } LIMIT ${count}" }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_query(Ok(outcome(&["n"], vec![vec![("n", "7")]])));
        sub.stage_query(Ok(outcome(&["s", "p", "o"], vec![])));
        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        // Ctx var is a Scalar, not a Full.
        match r.ctx.vars.get("count") {
            Some(CtxValue::Scalar(s)) => assert_eq!(s, "7"),
            other => panic!("expected Scalar count, got {other:?}"),
        }
        // Second query has plain "LIMIT 7", no VALUES injection.
        let seen = sub.queries_seen.borrow();
        assert!(seen[1].ends_with("LIMIT 7"), "got: {}", seen[1]);
        assert!(!seen[1].contains("VALUES"), "got: {}", seen[1]);
    }

    #[test]
    fn mixed_use_errors_clearly() {
        // Using a full-bind var inside a `wasm` step's arg-JSON is a
        // category error — the arg is opaque scalar text, not a SPARQL
        // position. Fail the step with a message that names the
        // variable and points at the mistake.
        let plan_json = r#"{
            "name": "mix",
            "steps": [
              { "kind": "sparql_query",
                "name": "grid",
                "query": "SELECT ?a WHERE { ?a a :T }",
                "bind_full": true },
              { "kind": "wasm",
                "name": "consume",
                "url": "file:///consume.wasm",
                "arg": "{\"prev\": ${grid}}" }
            ]
        }"#;
        let plan: Plan = serde_json::from_str(plan_json).unwrap();
        let sub = MockSubstrate::default();
        sub.stage_query(Ok(outcome(&["a"], vec![vec![("a", "x")]])));
        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        // First step succeeds; the wasm step fails cleanly on the
        // interpolation of `${grid}` into a scalar context.
        assert_eq!(r.rows.len(), 2);
        assert_eq!(row_get(&r.rows[0], "ok"), "true");
        assert_eq!(row_get(&r.rows[1], "ok"), "false");
        let detail = row_get(&r.rows[1], "detail");
        assert!(detail.contains("grid"), "detail was: {detail}");
        assert!(
            detail.contains("bind_full") || detail.contains("full binding-set"),
            "detail was: {detail}"
        );
        // The wasm was never actually invoked — the failure happened
        // during pre-dispatch interpolation.
        assert!(sub.wasm_calls_seen.borrow().is_empty());
    }
}
