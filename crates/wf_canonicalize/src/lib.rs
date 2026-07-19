//! wf_canonicalize — resolve owl:sameAs at ingest, plus keep the
//! fulltext literal-index and document-mirror indexes in sync with
//! the graph.
//!
//! Signature (M1 X4 ExtensionGuest shape):
//!     `<urn:webfunction:canonicalize>("<config-json>")`
//!         → rdf:JSON literal `{"classes": N, "aliased": N,
//!            "rewritten": N, "seeded": N, "ft_inserted": N,
//!            "ft_deleted": N, "ft_errors": N, "doc_inserted": N,
//!            "doc_deleted": N, "doc_unchanged": N, "doc_errors": N}`.
//!
//! Config JSON shape (only `sink` is required; `rule` defaults to
//! `mint_genid`; `fulltext_indexes` and `document_indexes` default to
//! empty and skip their respective reconcile phases entirely):
//!
//! ```json
//! { "sink": "canonicalize",
//!   "rule": "mint_genid",
//!   "fulltext_indexes": [
//!     { "name": "products",
//!       "backend_url": "http://localhost:9308",
//!       "index": "products",
//!       "predicates": ["http://ex/label", "http://ex/description"],
//!       "sweep_interval_secs": 300 }
//!   ],
//!   "document_indexes": [
//!     { "name": "manuals",
//!       "search_backend": "http://localhost:9308",
//!       "storage_backend": "http://localhost:8080",
//!       "search_index": "manuals",
//!       "sirix_database": "docs",
//!       "sirix_resource": "manuals",
//!       "revision_retention": "latest",
//!       "sweep_interval_secs": 300 }
//!   ] }
//! ```
//!
//! Migration deviations (M1 X4, ExtensionGuest + tracker-sink wave):
//!
//!   * The `sink` field is now a substrate-side sink NAME (not a
//!     driver URL). The host owns the underlying SQLite/DuckDB/…
//!     backend; tracker tables are materialized by the host at
//!     `register-tracker-tables` time. The pre-migration form
//!     `sqlite:///data/mv.db#aliases` is dropped; the new form is a
//!     bare identifier the host recognises.
//!   * The alias-map table is fixed at the name `aliases`. Under the
//!     old shape the caller could pick a name via the URL fragment;
//!     the tracker-sink surface does not model per-invocation table
//!     names, so callers who need to segregate alias maps grant
//!     distinct sink names instead.
//!   * `execute_query` / `execute_update` now route through
//!     `graph-callbacks`. The return shape of SELECT (flat
//!     `list<binding>` rather than the old `binding_sets` with a
//!     per-row grouping) is split back into rows via a
//!     variable-repeat heuristic (see `group_bindings_into_rows`),
//!     matching wf_profile's approach.
//!   * Manticore admin HTTP + Sirix SQL HTTP routes through
//!     `http-callbacks::http-post-json` rather than the retired
//!     `wf:fulltext/host@0.1.0` import. Content-Type stays
//!     `application/json` for wire compatibility with the previous
//!     sweep behavior; a future revision can typed-migrate the
//!     Manticore bulk write to `fulltext-callbacks::insert-documents`
//!     but the current Manticore schema (retention `_valid_from` /
//!     `_valid_to` columns + custom `content_type`+`subject` slots)
//!     doesn't fit the typed `fulltext-document { id, fields, lang }`
//!     shape — flagged as a fulltext-callbacks WIT gap in the
//!     migration report.
//!
//! Pipeline (five phases): unchanged in shape; the guest performs
//! phase-0 alias-map load, phase-1 sameAs union-find, phase-2
//! canonical selection, phase-3 graph rewrite, phase-4 alias
//! persistence + fulltext / document sweeps, phase-5 sameAs delete.

#[allow(warnings)]
mod bindings;

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::json;

pub mod document_sweep;
pub mod fulltext_sweep;

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
    self as gc, Binding as WitBinding, GraphCallError, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::http_callbacks::{
    self as hc, HttpError, HttpHeader,
};
use bindings::tegmentum::webfunction::tracker_sink_callbacks::{
    self as ts, ColumnType, TrackerColumn, TrackerError, TrackerRow, TrackerTableSchema,
    TrackerValue, TrackerWhere,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

use document_sweep::{DocumentIndexConfig, SweepResult as DocSweepResult};
use fulltext_sweep::{FulltextIndexConfig, SweepCounts};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const OWL_SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";
const ALIAS_TABLE: &str = "aliases";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Config {
    /// Substrate-side sink NAME (post-migration). Must resolve in
    /// the host's tracker-sink allowlist; the host owns the backend
    /// path + connection string, not the guest.
    sink: String,
    #[serde(default = "default_rule")]
    rule: String,
    #[serde(default)]
    fulltext_indexes: Vec<FulltextIndexConfig>,
    #[serde(default)]
    document_indexes: Vec<DocumentIndexConfig>,
    #[serde(default)]
    full_scan: bool,
}

fn default_rule() -> String {
    "mint_genid".into()
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn fmt_graph_err(e: GraphCallError) -> String {
    match e {
        GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    }
}

fn fmt_http_err(e: HttpError) -> String {
    match e {
        HttpError::Network(m) => format!("http-callbacks network: {m}"),
        HttpError::Status(c) => format!("http-callbacks non-2xx: {c}"),
        HttpError::InvalidRequest(m) => format!("http-callbacks invalid-request: {m}"),
        HttpError::NotPermitted(m) => format!("http-callbacks not-permitted: {m}"),
    }
}

fn fmt_tracker_err(e: TrackerError) -> String {
    match e {
        TrackerError::NoSuchSink(m) => format!("tracker-sink no-such-sink: {m}"),
        TrackerError::NoSuchTable(m) => format!("tracker-sink no-such-table: {m}"),
        TrackerError::NoSuchRow(m) => format!("tracker-sink no-such-row: {m}"),
        TrackerError::SchemaViolation(m) => format!("tracker-sink schema-violation: {m}"),
        TrackerError::BackendError(m) => format!("tracker-sink backend-error: {m}"),
        TrackerError::NotPermitted(m) => format!("tracker-sink not-permitted: {m}"),
    }
}

// ---------------------------------------------------------------------------
// Binding helpers
// ---------------------------------------------------------------------------

/// Split the flat `list<binding>` graph-callbacks returns into row
/// groups. Same heuristic wf_profile uses: a repeat of a previously-
/// seen variable name in the current row signals the start of a new
/// row.
fn group_bindings_into_rows(flat: Vec<WitBinding>) -> Vec<Vec<WitBinding>> {
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
    rows
}

fn select_rows(sparql: &str) -> Result<Vec<Vec<WitBinding>>, String> {
    let result = gc::execute_query(sparql).map_err(fmt_graph_err)?;
    match result {
        CallbackQueryResult::Bindings(bs) => Ok(group_bindings_into_rows(bs)),
        CallbackQueryResult::Quads(_) => {
            Err("wf_canonicalize: SELECT expected but graph-callbacks returned quads".into())
        }
        CallbackQueryResult::Boolean(_) => {
            Err("wf_canonicalize: SELECT expected but graph-callbacks returned boolean".into())
        }
    }
}

fn binding_iri(row: &[WitBinding], name: &str) -> Option<String> {
    row.iter().find(|b| b.variable == name).and_then(|b| match &b.value {
        WitTerm::NamedNode(s) => Some(s.clone()),
        _ => None,
    })
}

fn binding_term(row: &[WitBinding], name: &str) -> Option<WitTerm> {
    row.iter().find(|b| b.variable == name).map(|b| b.value.clone())
}

// ---------------------------------------------------------------------------
// SPARQL value rendering — WitTerm → SPARQL text with alias rewrite
// ---------------------------------------------------------------------------

fn value_to_sparql(v: &WitTerm, alias_to_canonical: &HashMap<String, String>) -> String {
    match v {
        WitTerm::NamedNode(s) => {
            let target = alias_to_canonical.get(s).unwrap_or(s);
            format!("<{target}>")
        }
        WitTerm::BlankNode(label) => format!("_:{label}"),
        WitTerm::Literal(l) => {
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
                // xsd:string default per RDF 1.1.
                format!("\"{escaped}\"")
            }
        }
        WitTerm::Triple(_) => {
            // RDF-star quoted triples don't appear in the sameAs
            // canonicalization pipeline. Emit a placeholder rather
            // than panic; log at build-diff time if it surfaces.
            "<urn:webfunction:canonicalize:unsupported-triple>".into()
        }
    }
}

// ---------------------------------------------------------------------------
// Literal constructors (for tracker-sink round-trips + SPARQL emits)
// ---------------------------------------------------------------------------

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.into(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    })
}

// ---------------------------------------------------------------------------
// Tracker-sink helpers
// ---------------------------------------------------------------------------

fn tracker_text(values: &[TrackerValue], idx: usize) -> Result<String, String> {
    match values.get(idx) {
        Some(TrackerValue::TextValue(s)) => Ok(s.clone()),
        Some(other) => Err(format!("tracker-select: expected text at column {idx}, got {other:?}")),
        None => Err(format!("tracker-select: missing column {idx}")),
    }
}

fn tracker_int(values: &[TrackerValue], idx: usize) -> Result<i64, String> {
    match values.get(idx) {
        Some(TrackerValue::IntegerValue(i)) => Ok(*i),
        Some(other) => Err(format!("tracker-select: expected integer at column {idx}, got {other:?}")),
        None => Err(format!("tracker-select: missing column {idx}")),
    }
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Materialize the alias-map table on the named sink. Idempotent; the
/// host `register-tracker-tables` call is safe to repeat with the
/// same schema (memo §5).
fn register_alias_table(sink_name: &str) -> Result<(), String> {
    let schema = TrackerTableSchema {
        name: ALIAS_TABLE.into(),
        columns: vec![
            TrackerColumn {
                name: "alias".into(),
                column_type: ColumnType::Text,
                primary_key: true,
                nullable: false,
            },
            TrackerColumn {
                name: "canonical".into(),
                column_type: ColumnType::Text,
                primary_key: false,
                nullable: false,
            },
        ],
        indexes: vec![],
    };
    ts::register_tracker_tables(sink_name, &[schema]).map_err(fmt_tracker_err)
}

// ---------------------------------------------------------------------------
// Guest entry
// ---------------------------------------------------------------------------

fn canonicalize_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let config_json = match args.first() {
        Some(WitTerm::Literal(l)) => l.value.clone(),
        _ => {
            return Err(
                "wf_canonicalize: first arg must be a config-json string literal".into(),
            );
        }
    };
    let cfg: Config = serde_json::from_str(&config_json)
        .map_err(|e| format!("wf_canonicalize: config parse: {e}"))?;

    let sink_name = cfg.sink.clone();

    // Phase 0a: materialize the alias-map table + seed the DSU from
    // any pre-existing (alias → canonical) rows.
    register_alias_table(&sink_name)?;

    let mut dsu = DisjointSetUnion::new();
    let mut existing_canonicals: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    let existing_rows = ts::tracker_select(
        &sink_name,
        ALIAS_TABLE,
        &[],
        &["alias".to_string(), "canonical".to_string()],
    )
    .map_err(fmt_tracker_err)?;
    for row in &existing_rows {
        let alias = tracker_text(&row.values, 0)?;
        let canonical = tracker_text(&row.values, 1)?;
        dsu.union(&alias, &canonical);
        existing_canonicals.insert(canonical);
    }
    let seed_size = existing_canonicals.len();

    // Phase 1: union-find over sameAs in the store on top of the seed.
    let pairs_sparql = format!(
        "SELECT ?a ?b WHERE {{ ?a <{OWL_SAME_AS}> ?b }}"
    );
    let pairs = select_rows(&pairs_sparql)?;

    for row in &pairs {
        let a = match binding_iri(row, "a") {
            Some(v) => v,
            None => continue,
        };
        let b = match binding_iri(row, "b") {
            Some(v) => v,
            None => continue,
        };
        dsu.union(&a, &b);
    }
    let classes = dsu.classes();

    // Phase 2: pick canonicals. Sticky rule — if the class contains a
    // pre-existing canonical, reuse it verbatim; else apply the
    // configured rule.
    let mut alias_to_canonical: HashMap<String, String> = HashMap::new();
    for class in &classes {
        let mut existing_in_class: Vec<&String> = class
            .iter()
            .filter(|m| existing_canonicals.contains(*m))
            .collect();
        existing_in_class.sort();
        let canonical = match existing_in_class.first() {
            Some(sticky) => (*sticky).clone(),
            None => pick_canonical(class, &cfg.rule)?,
        };
        for member in class {
            if member != &canonical {
                alias_to_canonical.insert(member.clone(), canonical.clone());
            }
        }
    }

    // Phase 3: rewrite the graph.
    let aliases_len = alias_to_canonical.len();
    let mut rewritten = 0u64;

    if !alias_to_canonical.is_empty() {
        let alias_iris: Vec<&String> = alias_to_canonical.keys().collect();

        let values_clause = alias_iris
            .iter()
            .map(|iri| format!("<{iri}>"))
            .collect::<Vec<_>>()
            .join(" ");
        let fetch = format!(
            "SELECT ?s ?p ?o WHERE {{ \
             {{ VALUES ?target {{ {vals} }} ?target ?p ?o . BIND(?target AS ?s) }} \
             UNION \
             {{ VALUES ?target {{ {vals} }} ?s ?p ?target . BIND(?target AS ?o) }} \
             FILTER(?p != <{OWL_SAME_AS}>) \
             }}",
            vals = values_clause,
        );
        let touched = select_rows(&fetch)?;

        let mut insert_body = String::new();
        for row in &touched {
            let s = match binding_term(row, "s") {
                Some(v) => v,
                None => continue,
            };
            let p = match binding_term(row, "p") {
                Some(v) => v,
                None => continue,
            };
            let o = match binding_term(row, "o") {
                Some(v) => v,
                None => continue,
            };
            let s_txt = value_to_sparql(&s, &alias_to_canonical);
            let p_txt = value_to_sparql(&p, &alias_to_canonical);
            let o_txt = value_to_sparql(&o, &alias_to_canonical);
            insert_body.push_str(&s_txt);
            insert_body.push(' ');
            insert_body.push_str(&p_txt);
            insert_body.push(' ');
            insert_body.push_str(&o_txt);
            insert_body.push_str(" .\n");
            rewritten += 1;
        }

        if !insert_body.is_empty() {
            let insert = format!("INSERT DATA {{ {insert_body} }}");
            gc::execute_update(&insert).map_err(|e| {
                format!(
                    "wf_canonicalize: insert canonicalized batch: {}",
                    fmt_graph_err(e)
                )
            })?;
        }

        let delete = format!(
            "DELETE {{ ?s ?p ?o }} WHERE {{ \
             ?s ?p ?o . \
             VALUES ?alias {{ {vals} }} \
             FILTER(?s = ?alias || ?o = ?alias) \
             }}",
            vals = values_clause,
        );
        gc::execute_update(&delete).map_err(|e| {
            format!(
                "wf_canonicalize: delete alias-bearing triples: {}",
                fmt_graph_err(e)
            )
        })?;
    }

    // Phase 4: persist the alias map (INSERT OR REPLACE semantics).
    if !alias_to_canonical.is_empty() {
        for (alias, canonical) in &alias_to_canonical {
            let row = TrackerRow {
                values: vec![
                    TrackerValue::TextValue(alias.clone()),
                    TrackerValue::TextValue(canonical.clone()),
                ],
            };
            ts::tracker_upsert(&sink_name, ALIAS_TABLE, &row).map_err(|e| {
                format!(
                    "wf_canonicalize: alias table upsert `{alias}`: {}",
                    fmt_tracker_err(e)
                )
            })?;
        }
    }

    // Phase 4b: fulltext-reconcile. Same fail-soft posture as the
    // pre-migration crate — errors are accumulated per-entry, never
    // propagated up.
    let sweep_counts = if cfg.fulltext_indexes.is_empty() {
        SweepCounts::default()
    } else {
        eprintln!(
            "fulltext sweep: {} literal-index entries, last commit rev={}",
            cfg.fulltext_indexes.len(),
            store_rev()
        );
        fulltext_sweep::run(
            &cfg.fulltext_indexes,
            &FulltextHostBridge,
            &GraphBridge,
            &SinkBridgeImpl {
                sink_name: sink_name.clone(),
            },
        )
    };

    // Phase 4c: document-mirror sweep.
    let doc_sweep = if cfg.document_indexes.is_empty() {
        DocSweepResult::default()
    } else {
        eprintln!(
            "document sweep: {} managed entries, last commit rev={}, full_scan={}",
            cfg.document_indexes.len(),
            store_rev(),
            cfg.full_scan,
        );
        document_sweep::run_with_options(
            &cfg.document_indexes,
            &FulltextHostBridge,
            &SirixHostBridge,
            &DocSinkBridgeImpl {
                sink_name: sink_name.clone(),
            },
            document_sweep::SweepOptions {
                full_scan: cfg.full_scan,
                now_millis: None,
            },
        )
    };

    // Phase 5: delete the sameAs assertions.
    let delete_sameas = format!(
        "DELETE {{ ?a <{OWL_SAME_AS}> ?b }} WHERE {{ ?a <{OWL_SAME_AS}> ?b }}"
    );
    gc::execute_update(&delete_sameas).map_err(|e| {
        format!(
            "wf_canonicalize: delete sameAs assertions: {}",
            fmt_graph_err(e)
        )
    })?;

    let summary = json!({
        "classes": classes.len(),
        "aliased": aliases_len,
        "rewritten": rewritten,
        "seeded": seed_size,
        "ft_inserted": sweep_counts.inserted,
        "ft_deleted": sweep_counts.deleted,
        "ft_errors": sweep_counts.errors,
        "doc_inserted": doc_sweep.inserted,
        "doc_deleted": doc_sweep.deleted,
        "doc_unchanged": doc_sweep.unchanged,
        "doc_errors": doc_sweep.errors,
    });
    Ok(json_literal(&summary.to_string()))
}

// ---------------------------------------------------------------------------
// ExtensionGuest / AggregateGuest / PropertyFunctionGuest surfaces
// ---------------------------------------------------------------------------

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "urn:webfunction:canonicalize".into(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "urn:webfunction:canonicalize" => canonicalize_impl(&args),
            other => Err(format!("wf_canonicalize: unknown function '{other}'")),
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
            "wf_canonicalize: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_canonicalize: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_canonicalize: aggregate state was never constructed".into())
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
            "wf_canonicalize: unknown property function '{name}' \
             (this component provides none)"
        ))
    }
}

// ---------------------------------------------------------------------------
// Canonical selection
// ---------------------------------------------------------------------------

fn pick_canonical(class: &[String], rule: &str) -> Result<String, String> {
    match rule {
        "mint_genid" => {
            if class.is_empty() {
                return Err("wf_canonicalize: empty equivalence class".into());
            }
            let mut sorted: Vec<&str> = class.iter().map(String::as_str).collect();
            sorted.sort();
            let joined = sorted.join("\0");
            Ok(mint_genid_iri(&joined))
        }
        "shortest_uri" => class
            .iter()
            .min_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
            .cloned()
            .ok_or_else(|| "wf_canonicalize: empty equivalence class".into()),
        other => Err(format!(
            "wf_canonicalize: unknown rule `{other}` (v1 supports: mint_genid, shortest_uri)"
        )),
    }
}

fn mint_genid_iri(input: &str) -> String {
    const GENID_PREFIX: &str = "https://tegmentum.ai/.well-known/genid/";
    const SALT: u64 = 0x9E3779B97F4A7C15;
    let mut h1: u64 = SALT;
    for b in input.bytes() {
        h1 = h1.wrapping_mul(0x100000001B3).wrapping_add(b as u64);
    }
    let mut h2: u64 = h1.rotate_left(23) ^ 0x428A2F98D728AE22;
    for b in input.bytes() {
        h2 = h2.wrapping_mul(0x100000001B3).wrapping_add(b as u64);
    }
    format!("{GENID_PREFIX}{h1:016x}{h2:016x}")
}

// ---------------------------------------------------------------------------
// Disjoint-set union
// ---------------------------------------------------------------------------

struct DisjointSetUnion {
    parent: HashMap<String, String>,
}

impl DisjointSetUnion {
    fn new() -> Self {
        Self {
            parent: HashMap::new(),
        }
    }

    fn find(&mut self, x: &str) -> String {
        let p = self
            .parent
            .entry(x.to_string())
            .or_insert_with(|| x.to_string())
            .clone();
        if p == x {
            return p;
        }
        let root = self.find(&p);
        self.parent.insert(x.to_string(), root.clone());
        root
    }

    fn union(&mut self, a: &str, b: &str) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent.insert(ra, rb);
        }
    }

    fn classes(&mut self) -> Vec<Vec<String>> {
        let keys: Vec<String> = self.parent.keys().cloned().collect();
        let mut buckets: HashMap<String, Vec<String>> = HashMap::new();
        for k in keys {
            let root = self.find(&k);
            buckets.entry(root).or_default().push(k);
        }
        buckets.into_values().collect()
    }
}

// ---------------------------------------------------------------------------
// Store-rev marker (opaque monotonic wall-clock stamp for logs)
// ---------------------------------------------------------------------------

fn store_rev() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

// ---------------------------------------------------------------------------
// Fulltext-sweep / document-sweep bridges
// ---------------------------------------------------------------------------

/// Wire the sweep modules' `HttpBridge` through to
/// `http-callbacks::http-post-json`. Content-Type stays
/// `application/json` for wire compatibility with the pre-migration
/// behavior (which routed through `wf:fulltext/host@0.1.0` that
/// hardcoded the same value).
struct FulltextHostBridge;

impl fulltext_sweep::HttpBridge for FulltextHostBridge {
    fn post_json(&self, url: &str, body: &str) -> Result<String, String> {
        let headers = vec![HttpHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }];
        let resp = hc::http_post_json(url, body, &headers).map_err(fmt_http_err)?;
        if !(200..300).contains(&resp.status) {
            return Err(format!(
                "http-callbacks: non-2xx status {} from {}",
                resp.status, url
            ));
        }
        Ok(resp.body)
    }
}

/// Wire the fulltext-sweep module's `GraphBridge` through to
/// `graph-callbacks::execute-query`.
struct GraphBridge;

impl fulltext_sweep::GraphBridge for GraphBridge {
    fn select_subject_predicate_object(
        &self,
        predicates: &[String],
    ) -> Result<Vec<(String, String, fulltext_sweep::LiteralOrIri)>, String> {
        let values = predicates
            .iter()
            .map(|p| format!("<{p}>"))
            .collect::<Vec<_>>()
            .join(" ");
        let sparql = format!(
            "SELECT ?s ?p ?o WHERE {{ ?s ?p ?o . VALUES ?p {{ {values} }} }}"
        );
        let rows = select_rows(&sparql)?;
        let mut out: Vec<(String, String, fulltext_sweep::LiteralOrIri)> =
            Vec::with_capacity(rows.len());
        for row in &rows {
            let s = match binding_iri(row, "s") {
                Some(v) => v,
                None => continue,
            };
            let p = match binding_iri(row, "p") {
                Some(v) => v,
                None => continue,
            };
            let o_binding = row.iter().find(|b| b.variable == "o");
            let o = match o_binding {
                Some(b) => match &b.value {
                    WitTerm::Literal(l) => fulltext_sweep::LiteralOrIri::Literal {
                        lex: l.value.clone(),
                        lang: l.language.clone(),
                    },
                    WitTerm::NamedNode(i) => fulltext_sweep::LiteralOrIri::Iri(i.clone()),
                    // Blank-node and quoted-triple objects are unindexable — skip.
                    WitTerm::BlankNode(_) | WitTerm::Triple(_) => continue,
                },
                None => continue,
            };
            out.push((s, p, o));
        }
        Ok(out)
    }
}

/// Wire the fulltext-sweep module's `SinkBridge` through to the
/// tracker-sink WIT imports. Carries only the sink NAME — the host
/// owns the underlying connection.
struct SinkBridgeImpl {
    sink_name: String,
}

impl fulltext_sweep::SinkBridge for SinkBridgeImpl {
    fn ensure_table(&self, table: &str) -> Result<(), String> {
        let schema = TrackerTableSchema {
            name: table.into(),
            columns: vec![
                TrackerColumn {
                    name: "subject_iri".into(),
                    column_type: ColumnType::Text,
                    primary_key: true,
                    nullable: false,
                },
                TrackerColumn {
                    name: "doc_hash".into(),
                    column_type: ColumnType::Text,
                    primary_key: false,
                    nullable: false,
                },
                TrackerColumn {
                    name: "updated_at".into(),
                    column_type: ColumnType::Integer,
                    primary_key: false,
                    nullable: false,
                },
            ],
            indexes: vec![],
        };
        ts::register_tracker_tables(&self.sink_name, &[schema]).map_err(fmt_tracker_err)
    }

    fn load_known(&self, table: &str) -> Result<HashMap<String, String>, String> {
        let rows = ts::tracker_select(
            &self.sink_name,
            table,
            &[],
            &["subject_iri".to_string(), "doc_hash".to_string()],
        )
        .map_err(fmt_tracker_err)?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in &rows {
            let subj = tracker_text(&r.values, 0)?;
            let hash = tracker_text(&r.values, 1)?;
            out.insert(subj, hash);
        }
        Ok(out)
    }

    fn upsert(&self, table: &str, subject: &str, hash: &str) -> Result<(), String> {
        let row = TrackerRow {
            values: vec![
                TrackerValue::TextValue(subject.into()),
                TrackerValue::TextValue(hash.into()),
                TrackerValue::IntegerValue(now_secs()),
            ],
        };
        ts::tracker_upsert(&self.sink_name, table, &row).map_err(fmt_tracker_err)
    }

    fn delete(&self, table: &str, subject: &str) -> Result<(), String> {
        let clauses = vec![TrackerWhere {
            column: "subject_iri".into(),
            operator: "=".into(),
            value: Some(TrackerValue::TextValue(subject.into())),
        }];
        ts::tracker_delete(&self.sink_name, table, &clauses)
            .map(|_| ())
            .map_err(fmt_tracker_err)
    }
}

// ---------------------------------------------------------------------------
// Document-sweep bridges
// ---------------------------------------------------------------------------

/// Wire the document-sweep `SirixBridge` through to
/// `http-callbacks::http-post-json`. Sirix-sql-server accepts a plain
/// `POST /query` with `{"sql": "..."}` — same wire shape wf_document
/// uses.
struct SirixHostBridge;

impl document_sweep::SirixBridge for SirixHostBridge {
    fn list_documents(
        &self,
        sirix_url: &str,
        database: &str,
        resource: &str,
        since_rev: Option<u64>,
        wants_history: bool,
    ) -> Result<Vec<document_sweep::SirixDocRow>, String> {
        let sql =
            document_sweep::build_scan_sql(database, resource, since_rev, wants_history);
        let body = document_sweep::build_query_body(&sql);
        let url = document_sweep::sirix_query_url(sirix_url);
        let headers = vec![HttpHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }];
        let resp = hc::http_post_json(&url, &body, &headers).map_err(fmt_http_err)?;
        if !(200..300).contains(&resp.status) {
            return Err(format!(
                "sirix POST /query: non-2xx status {} from {}",
                resp.status, url
            ));
        }
        document_sweep::parse_scan_response(&resp.body)
    }
}

/// Wire the document-sweep `DocSinkBridge` through to the tracker-sink
/// WIT imports. Separate from `SinkBridgeImpl` because the doc
/// tracker schema carries an extra `last_seen_rev` column and its PK
/// is composite `(doc_uri, rev)`.
struct DocSinkBridgeImpl {
    sink_name: String,
}

impl document_sweep::DocSinkBridge for DocSinkBridgeImpl {
    fn ensure_doc_table(&self, table: &str) -> Result<(), String> {
        let schema = TrackerTableSchema {
            name: table.into(),
            columns: vec![
                TrackerColumn {
                    name: "doc_uri".into(),
                    column_type: ColumnType::Text,
                    primary_key: true,
                    nullable: false,
                },
                TrackerColumn {
                    name: "rev".into(),
                    column_type: ColumnType::Integer,
                    primary_key: true,
                    nullable: false,
                },
                TrackerColumn {
                    name: "last_seen_rev".into(),
                    column_type: ColumnType::Integer,
                    primary_key: false,
                    nullable: false,
                },
                TrackerColumn {
                    name: "doc_hash".into(),
                    column_type: ColumnType::Text,
                    primary_key: false,
                    nullable: false,
                },
                TrackerColumn {
                    name: "updated_at".into(),
                    column_type: ColumnType::Integer,
                    primary_key: false,
                    nullable: false,
                },
            ],
            indexes: vec![],
        };
        ts::register_tracker_tables(&self.sink_name, &[schema]).map_err(fmt_tracker_err)
    }

    fn load_known_docs(
        &self,
        table: &str,
    ) -> Result<HashMap<(String, u64), document_sweep::KnownDoc>, String> {
        let rows = ts::tracker_select(
            &self.sink_name,
            table,
            &[],
            &[
                "doc_uri".to_string(),
                "rev".to_string(),
                "last_seen_rev".to_string(),
                "doc_hash".to_string(),
            ],
        )
        .map_err(fmt_tracker_err)?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in &rows {
            let uri = tracker_text(&r.values, 0)?;
            let rev = tracker_int(&r.values, 1)? as u64;
            let last_seen_rev = tracker_int(&r.values, 2)? as u64;
            let doc_hash = tracker_text(&r.values, 3)?;
            out.insert(
                (uri, rev),
                document_sweep::KnownDoc {
                    last_seen_rev,
                    doc_hash,
                },
            );
        }
        Ok(out)
    }

    fn upsert_doc(
        &self,
        table: &str,
        doc_uri: &str,
        rev: u64,
        entry: &document_sweep::KnownDoc,
    ) -> Result<(), String> {
        let row = TrackerRow {
            values: vec![
                TrackerValue::TextValue(doc_uri.into()),
                TrackerValue::IntegerValue(rev as i64),
                TrackerValue::IntegerValue(entry.last_seen_rev as i64),
                TrackerValue::TextValue(entry.doc_hash.clone()),
                TrackerValue::IntegerValue(now_secs()),
            ],
        };
        ts::tracker_upsert(&self.sink_name, table, &row).map_err(fmt_tracker_err)
    }

    fn delete_doc(&self, table: &str, doc_uri: &str, rev: u64) -> Result<(), String> {
        let clauses = vec![
            TrackerWhere {
                column: "doc_uri".into(),
                operator: "=".into(),
                value: Some(TrackerValue::TextValue(doc_uri.into())),
            },
            TrackerWhere {
                column: "rev".into(),
                operator: "=".into(),
                value: Some(TrackerValue::IntegerValue(rev as i64)),
            },
        ];
        ts::tracker_delete(&self.sink_name, table, &clauses)
            .map(|_| ())
            .map_err(fmt_tracker_err)
    }
}

bindings::export!(Component with_types_in bindings);
