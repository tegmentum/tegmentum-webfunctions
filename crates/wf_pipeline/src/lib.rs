//! wf_pipeline — declarative composition of substrate steps.
//!
//! Signature: `wf:pipeline("<plan-json>")` returns an rdf:JSON
//! literal shaped as
//!   `{"vars":["step","kind","name","ok","detail"],"rows":[...]}`
//! (mirror of the batch1 / batch2 single-term collapse). Each row is a
//! JSON object with the columns above; one row per executed step.
//!
//! Migration (Follow-up F): moved off the Stardog overlay onto the
//! substrate `tegmentum:webfunction/extension-with-all-host-callbacks
//! @0.1.0` world.
//!
//!   * `sparql_query` / `sparql_update` — `graph-callbacks::
//!     execute-query` / `execute-update`.
//!   * `wasm` step — `wasm-callbacks::invoke-wasm-service` (the R1
//!     property-function-shape sub-invocation, `(url, list<term>) ->
//!     list<binding>`).
//!   * `condition` step — an ASK evaluated by `execute-query`; the
//!     new `graph-callbacks::query-result::boolean` arm exposes the
//!     verdict directly.
//!
//! Migration deviation: the Stardog-era
//! `stardog:webfunction@0.6.0/host::execute-query-with-bindings`
//! primitive (dead-code stub in the pre-migration crate) is not
//! present on the R1 substrate. The v3 `bind_full: true` +
//! `${var}` -> VALUES-clause interpolation path continues to work —
//! it never depended on the substrate primitive, only on textual
//! interpolation into the outgoing SPARQL.
//!
//! Plan shape (unchanged from pre-migration):
//!
//! ```json
//! {
//!   "name": "canonicalize_then_materialize",
//!   "steps": [
//!     { "kind": "sparql_update", "update":  "..." },
//!     { "kind": "sparql_query",  "query":   "...", "bind": { "count": "?n" } },
//!     { "kind": "condition",     "ask":     "...",
//!       "then_steps": [ ... ], "else_steps": [ ... ] },
//!     { "kind": "wasm",          "url":     "...", "arg": "..." }
//!   ]
//! }
//! ```

#[allow(warnings)]
mod bindings;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

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
    self as gc, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{
    Binding as WitBinding, Literal as WitLiteral, Term as WitTerm,
};
use bindings::tegmentum::webfunction::wasm_callbacks::{
    self as wc, WasmCallError,
};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";

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
    // inter-step variables
    #[serde(default)]
    bind: BTreeMap<String, String>,
    /// When true, the step's full binding-set is captured under the
    /// step's `name` (requires `name` to be set).
    #[serde(default)]
    bind_full: bool,
    // error recovery
    #[serde(default)]
    on_error: Option<OnError>,
}

#[derive(Deserialize, Clone, Default)]
struct OnError {
    #[serde(default)]
    retry: u32,
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    fallback_steps: Vec<Step>,
}

// ---------------------------------------------------------------------------
// Substrate abstraction — lets tests inject a mock; the Guest impl uses
// the WIT host imports.
// ---------------------------------------------------------------------------

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
    rows: Vec<Vec<(String, Cell)>>,
}

trait Substrate {
    fn execute_query(&self, sparql: &str) -> Result<QueryOutcome, String>;
    fn execute_update(&self, update: &str) -> Result<(), String>;
    fn invoke_wasm(&self, url: &str, arg: Option<&str>) -> Result<QueryOutcome, String>;
    fn execute_ask(&self, sparql: &str) -> Result<bool, String>;
    fn sleep_ms(&self, _ms: u64) {}
}

// ---------------------------------------------------------------------------
// Interpolation
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum CtxValue {
    Scalar(String),
    Full(QueryOutcome),
}

#[derive(Default, Clone, Debug)]
struct Context {
    vars: BTreeMap<String, CtxValue>,
}

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
    out.push(' ');
    out.push('}');
    out
}

#[derive(Copy, Clone, Debug)]
enum InterpMode {
    Sparql,
    Scalar,
}

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
    rows: Vec<Vec<(String, JsonValue)>>,
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

        self.rows
            .push(build_row(idx, &step.kind, &step_name, false, &last_err));

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
                let url = interpolate_mode(url, &self.ctx, InterpMode::Scalar)?;
                let arg_string = match step.arg.as_deref() {
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
// Row assembly (each row -> BTreeMap<String, JsonValue>)
// ---------------------------------------------------------------------------

fn build_row(idx: i64, kind: &str, name: &str, ok: bool, detail: &str) -> Vec<(String, JsonValue)> {
    vec![
        ("step".to_string(), json!(idx)),
        ("kind".to_string(), json!(kind)),
        ("name".to_string(), json!(name)),
        ("ok".to_string(), json!(ok)),
        ("detail".to_string(), json!(detail)),
    ]
}

fn rows_to_json_literal(rows: Vec<Vec<(String, JsonValue)>>) -> WitTerm {
    let json_rows: Vec<JsonValue> = rows
        .into_iter()
        .map(|r| {
            let mut obj = serde_json::Map::new();
            for (k, v) in r {
                obj.insert(k, v);
            }
            JsonValue::Object(obj)
        })
        .collect();
    let out = json!({
        "vars": ["step", "kind", "name", "ok", "detail"],
        "rows": json_rows,
    });
    WitTerm::Literal(WitLiteral {
        value: out.to_string(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    })
}

// ---------------------------------------------------------------------------
// Guest impl — wires the real WIT host imports into a Substrate.
// ---------------------------------------------------------------------------

struct HostSubstrate;

fn map_graph_err(e: gc::GraphCallError) -> String {
    match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    }
}

fn map_wasm_err(e: WasmCallError) -> String {
    match e {
        WasmCallError::NotFound(m) => format!("wasm-callbacks not-found: {m}"),
        WasmCallError::InvocationError(m) => format!("wasm-callbacks invocation-error: {m}"),
        WasmCallError::NotPermitted(m) => format!("wasm-callbacks not-permitted: {m}"),
    }
}

fn value_to_cell(v: &WitTerm) -> Cell {
    match v {
        WitTerm::NamedNode(s) => Cell::Iri(s.clone()),
        WitTerm::Literal(l) => Cell::Literal {
            label: l.value.clone(),
            datatype: l.datatype.clone().unwrap_or_default(),
            lang: l.language.clone(),
        },
        WitTerm::BlankNode(s) => Cell::Bnode(s.clone()),
        WitTerm::Triple(_) => Cell::Literal {
            label: "<<quoted-triple>>".into(),
            datatype: XSD_STRING.into(),
            lang: None,
        },
    }
}

/// Reconstruct rows from a flat `list<binding>` by splitting on
/// repeated variable identity (the R1 shape returned by
/// `graph-callbacks::query-result::bindings` and
/// `wasm-callbacks::invoke-wasm-service`).
fn flat_to_outcome(flat: Vec<WitBinding>) -> QueryOutcome {
    let mut vars: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<(String, Cell)>> = Vec::new();
    let mut current: Vec<(String, Cell)> = Vec::new();
    for b in flat {
        if current.iter().any(|(n, _)| n == &b.variable) {
            rows.push(std::mem::take(&mut current));
        }
        if !vars.contains(&b.variable) {
            vars.push(b.variable.clone());
        }
        current.push((b.variable, value_to_cell(&b.value)));
    }
    if !current.is_empty() {
        rows.push(current);
    }
    QueryOutcome { vars, rows }
}

impl Substrate for HostSubstrate {
    fn execute_query(&self, sparql: &str) -> Result<QueryOutcome, String> {
        let result = gc::execute_query(sparql).map_err(map_graph_err)?;
        match result {
            CallbackQueryResult::Bindings(bs) => Ok(flat_to_outcome(bs)),
            CallbackQueryResult::Quads(qs) => {
                // Project CONSTRUCT / DESCRIBE results into (?s ?p ?o)
                // rows so downstream bind/interpolate paths behave.
                let vars = vec!["s".into(), "p".into(), "o".into()];
                let rows: Vec<Vec<(String, Cell)>> = qs
                    .into_iter()
                    .map(|q| {
                        vec![
                            ("s".to_string(), value_to_cell(&q.subject)),
                            ("p".to_string(), value_to_cell(&q.predicate)),
                            ("o".to_string(), value_to_cell(&q.object)),
                        ]
                    })
                    .collect();
                Ok(QueryOutcome { vars, rows })
            }
            CallbackQueryResult::Boolean(_) => Err(
                "sparql_query: ASK result — use a `condition` step instead".into(),
            ),
        }
    }
    fn execute_update(&self, update: &str) -> Result<(), String> {
        gc::execute_update(update).map_err(map_graph_err)
    }
    fn invoke_wasm(&self, url: &str, arg: Option<&str>) -> Result<QueryOutcome, String> {
        let args: Vec<WitTerm> = arg
            .into_iter()
            .map(|s| {
                WitTerm::Literal(WitLiteral {
                    value: s.to_string(),
                    datatype: Some(XSD_STRING.into()),
                    language: None,
                })
            })
            .collect();
        let bs = wc::invoke_wasm_service(url, &args).map_err(map_wasm_err)?;
        Ok(flat_to_outcome(bs))
    }
    fn execute_ask(&self, sparql: &str) -> Result<bool, String> {
        let result = gc::execute_query(sparql).map_err(map_graph_err)?;
        match result {
            CallbackQueryResult::Boolean(b) => Ok(b),
            CallbackQueryResult::Bindings(bs) => {
                // Fallback: an ASK routed through the bindings arm would
                // land here. Treat any bound row as `true`, no rows as
                // `false` — matches the pre-migration mock semantics.
                Ok(!bs.is_empty())
            }
            CallbackQueryResult::Quads(qs) => Ok(!qs.is_empty()),
        }
    }
    fn sleep_ms(&self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_pipeline".into(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_pipeline" => {
                let plan_json = match args.first() {
                    Some(WitTerm::Literal(l)) => l.value.clone(),
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
                Ok(rows_to_json_literal(runner.rows))
            }
            other => Err(format!("wf_pipeline: unknown function '{other}'")),
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
            "wf_pipeline: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_pipeline: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_pipeline: aggregate state was never constructed".into())
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
            "wf_pipeline: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

// ---------------------------------------------------------------------------
// Tests — kept intact via the Substrate trait, decoupled from the WIT
// host imports.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

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
            self.query_results.borrow_mut().remove(0)
        }
        fn execute_update(&self, update: &str) -> Result<(), String> {
            self.updates_seen.borrow_mut().push(update.to_string());
            self.update_results.borrow_mut().remove(0)
        }
        fn invoke_wasm(&self, url: &str, arg: Option<&str>) -> Result<QueryOutcome, String> {
            self.wasm_calls_seen
                .borrow_mut()
                .push((url.to_string(), arg.map(|s| s.to_string())));
            self.wasm_results.borrow_mut().remove(0)
        }
        fn execute_ask(&self, sparql: &str) -> Result<bool, String> {
            self.asks_seen.borrow_mut().push(sparql.to_string());
            self.ask_results.borrow_mut().remove(0)
        }
        fn sleep_ms(&self, ms: u64) {
            self.sleeps.borrow_mut().push(ms);
        }
    }

    fn row_get(row: &[(String, JsonValue)], name: &str) -> String {
        row.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| match v {
                JsonValue::String(s) => s.clone(),
                JsonValue::Bool(b) => b.to_string(),
                JsonValue::Number(n) => n.to_string(),
                other => other.to_string(),
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
                        .map(|(n, v)| {
                            (
                                n.to_string(),
                                Cell::Literal {
                                    label: v.to_string(),
                                    datatype: XSD_STRING.into(),
                                    lang: None,
                                },
                            )
                        })
                        .collect()
                })
                .collect(),
        }
    }

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

    #[test]
    fn on_error_retry_succeeds_on_last_attempt() {
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
        sub.stage_update(Ok(()));
        sub.stage_update(Ok(()));

        let mut r = Runner::new(&sub);
        r.run_steps(&plan.steps);

        assert_eq!(r.rows.len(), 3, "rows: {:#?}", r.rows);
        assert_eq!(row_get(&r.rows[0], "name"), "will_fail");
        assert_eq!(row_get(&r.rows[0], "ok"), "false");
        assert_eq!(row_get(&r.rows[1], "name"), "fb");
        assert_eq!(row_get(&r.rows[1], "ok"), "true");
        assert_eq!(row_get(&r.rows[2], "name"), "after");
        assert_eq!(row_get(&r.rows[2], "ok"), "true");
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

    #[test]
    fn bind_full_captures_binding_set() {
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

        match r.ctx.vars.get("count") {
            Some(CtxValue::Scalar(s)) => assert_eq!(s, "7"),
            other => panic!("expected Scalar count, got {other:?}"),
        }
        let seen = sub.queries_seen.borrow();
        assert!(seen[1].ends_with("LIMIT 7"), "got: {}", seen[1]);
        assert!(!seen[1].contains("VALUES"), "got: {}", seen[1]);
    }

    #[test]
    fn mixed_use_errors_clearly() {
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

        assert_eq!(r.rows.len(), 2);
        assert_eq!(row_get(&r.rows[0], "ok"), "true");
        assert_eq!(row_get(&r.rows[1], "ok"), "false");
        let detail = row_get(&r.rows[1], "detail");
        assert!(detail.contains("grid"), "detail was: {detail}");
        assert!(
            detail.contains("bind_full") || detail.contains("full binding-set"),
            "detail was: {detail}"
        );
        assert!(sub.wasm_calls_seen.borrow().is_empty());
    }
}
