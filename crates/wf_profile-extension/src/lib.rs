//! Phase-2 SPARQL extension re-exposing wf_profile's execute-query
//! probes on the standardized `tegmentum:webfunction/graph-callbacks@0.1.0`
//! interface.
//!
//! Two exports:
//!
//!   * `predicate-triple-count(<iri>) -> integer literal`
//!     Runs `SELECT (COUNT(*) AS ?n) WHERE { ?s <iri> ?o }` through
//!     `graph-callbacks::execute-query` and returns the count.
//!     Smallest possible round-trip through the callback surface;
//!     the smoke test asserts on this one.
//!
//!   * `classify-predicate(<iri>) -> string literal (JSON)`
//!     Port of `wf_profile::classify()`. Runs three probes through
//!     `graph-callbacks::execute-query` (subjects+triples count, max
//!     per-subject cardinality, IRI/literal/bnode mix), classifies
//!     the predicate's shape into attribute / foreign-key /
//!     child-table / list / graph, returns a JSON blob mirroring the
//!     row `wf_profile::evaluate()` would emit for this predicate.
//!
//! # Provenance of the algorithm
//!
//! The `classify()`, `is_rdf_list()`, `list_value_type()`,
//! `dominant_datatype()`, and `xsd_to_column_type()` helpers are
//! adapted from
//! `~/git/tegmentum-webfunctions/crates/wf_profile/src/lib.rs`. The
//! shape of every SPARQL string, the classification thresholds
//! (0.95 for pure literal / IRI, 0.85 for RDF-Collection detection,
//! etc.), and the target-type mapping are preserved byte-for-byte so
//! the extension's classify-predicate call agrees with wf_profile's
//! evaluate() on every predicate they both classify.
//!
//! Duplication (rather than a cross-crate `use`) keeps the original
//! wf_profile crate unmodified. wf_profile is a `cdylib` targeting
//! wasm32-wasip1 under `stardog:webfunction@0.5.0`; it cannot be
//! imported as a library. The ~150 lines duplicated here have no
//! dependencies to drift against — the probes are literal strings,
//! the classifier is arithmetic on i64 / f64 counts.
//!
//! # Callback usage
//!
//! Every SPARQL string in this module is dispatched through
//! `bindings::tegmentum::webfunction::graph_callbacks::execute_query`.
//! The Stardog-era `wf_profile` crate paid three round-trips per
//! predicate via `stardog::webfunction::host::execute_query` (with a
//! `bindings: &[Binding]` and `limit: Option<u32>` argument set that
//! graph-callbacks does not carry). The migrated shape drops both:
//! initial-binding substitution is handled by inlining the predicate
//! IRI in the query text; `LIMIT` clauses are inlined into the SPARQL
//! string. Wire-equivalent, one fewer degree of freedom in the WIT.

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
    self as gc, Binding as WitBinding, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{
    Literal as WitLiteral, Term as WitTerm,
};

use serde_json::json;

struct Component;

// XSD datatype IRIs — mirrored from `wf_profile/src/lib.rs`.
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![
            FunctionDescriptor {
                name: "predicate-triple-count".to_string(),
                min_arity: 1,
                max_arity: Some(1),
            },
            FunctionDescriptor {
                name: "classify-predicate".to_string(),
                min_arity: 1,
                max_arity: Some(1),
            },
        ]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "predicate-triple-count" => predicate_triple_count(&args),
            "classify-predicate" => classify_predicate(&args),
            other => Err(format!(
                "wf_profile-extension: unknown function '{other}'"
            )),
        }
    }
}

/// Aggregate interface stub. Every export required by the
/// `extension-with-host-callbacks` world; this component has no
/// aggregates. The unreachable `AggregateState` never materializes
/// because `register_aggregates` returns `[]`.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "wf_profile-extension: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_profile-extension: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_profile-extension: aggregate state was never constructed".into())
    }
}

/// Property-function interface stub. Same reasoning as the aggregate
/// stub — required by the world, unused by this component.
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
            "wf_profile-extension: unknown property function '{name}' (this component provides none)"
        ))
    }
}

// ---------------------------------------------------------------------
// predicate-triple-count(<iri>) -> integer literal
// ---------------------------------------------------------------------

/// Run `SELECT (COUNT(*) AS ?n) WHERE { ?s <iri> ?o }` through the
/// host's `graph-callbacks::execute-query` and return the count as an
/// xsd:integer literal. Smallest scalar callback exercise this crate
/// exposes — the smoke test asserts on it.
fn predicate_triple_count(args: &[WitTerm]) -> Result<WitTerm, String> {
    let iri = require_iri(args, "predicate-triple-count")?;
    let sparql =
        format!("SELECT (COUNT(*) AS ?n) WHERE {{ ?s <{iri}> ?o }}");
    let count = query_int_scalar(&sparql, "n")?.unwrap_or(0);
    Ok(int_literal(count))
}

// ---------------------------------------------------------------------
// classify-predicate(<iri>) -> string literal (JSON)
// ---------------------------------------------------------------------

/// Full classification port. Emits one JSON object per predicate with
/// the same fields wf_profile's `evaluate()` returns per row:
/// `predicate`, `shape`, `cardinality`, `target_type`, `subjects`,
/// `triples`, `confidence`.
///
/// Note: some SPARQL constructs wf_profile relies on (`COUNT(DISTINCT
/// ?s)`, `MAX(?c)` over a nested SELECT, `SUM(IF(...))`) are supported
/// by Oxigraph 0.5 but may reduce to zero counts on toy fixtures; the
/// classification is honest in the sense that it drives the same code
/// paths, but the exact confidence/shape depends on what the underlying
/// engine returns for the aggregations.
fn classify_predicate(args: &[WitTerm]) -> Result<WitTerm, String> {
    let iri = require_iri(args, "classify-predicate")?;

    let enum_sparql = format!(
        "SELECT (COUNT(*) AS ?triples) (COUNT(DISTINCT ?s) AS ?subjects) \
         WHERE {{ ?s <{iri}> ?o }}"
    );
    let row = query_first_row(&enum_sparql)?;
    let triples = row_int(&row, "triples").unwrap_or(0);
    let subjects = row_int(&row, "subjects").unwrap_or(0);

    let (shape, cardinality, target_type, confidence) =
        classify(&iri, subjects);

    let payload = json!({
        "predicate": iri,
        "shape": shape,
        "cardinality": cardinality,
        "target_type": target_type,
        "subjects": subjects,
        "triples": triples,
        "confidence": confidence,
    });
    let serialized = serde_json::to_string(&payload).map_err(|e| {
        format!("classify-predicate: serializing classification: {e}")
    })?;
    Ok(json_literal(&serialized))
}

// ---------------------------------------------------------------------
// classify() and helpers — ported from wf_profile/src/lib.rs.
// ---------------------------------------------------------------------

/// Port of `wf_profile::classify`. Signature and thresholds preserved
/// verbatim so the sibling and the original agree per predicate.
fn classify(
    predicate: &str,
    subjects: i64,
) -> (String, String, String, f64) {
    let card_sparql = format!(
        "SELECT (MAX(?c) AS ?maxc) WHERE {{ \
         SELECT ?s (COUNT(?o) AS ?c) WHERE {{ ?s <{predicate}> ?o }} \
         GROUP BY ?s }}"
    );
    let max_c = query_first_row(&card_sparql)
        .ok()
        .and_then(|row| row_int(&row, "maxc"))
        .unwrap_or(0);

    let mix_sparql = format!(
        "SELECT \
           (SUM(IF(isIRI(?o), 1, 0)) AS ?iris) \
           (SUM(IF(isLiteral(?o), 1, 0)) AS ?lits) \
           (SUM(IF(isBlank(?o), 1, 0)) AS ?bns) \
         WHERE {{ ?s <{predicate}> ?o }}"
    );
    let mix = query_first_row(&mix_sparql).ok();
    let iris = mix.as_ref().and_then(|r| row_int(r, "iris")).unwrap_or(0);
    let lits = mix.as_ref().and_then(|r| row_int(r, "lits")).unwrap_or(0);
    let bns = mix.as_ref().and_then(|r| row_int(r, "bns")).unwrap_or(0);
    let total = (iris + lits + bns).max(1);
    let iri_pct = iris as f64 / total as f64;
    let lit_pct = lits as f64 / total as f64;

    let cardinality = if max_c <= 1 {
        "0..1"
    } else if subjects > 0 {
        "0..n"
    } else {
        "0..1"
    };

    let (shape, target_type, confidence) = if max_c <= 1 && is_rdf_list(predicate) {
        let value_type = list_value_type(predicate);
        ("list".to_string(), value_type, 0.85)
    } else if lit_pct >= 0.95 {
        let dt = dominant_datatype(predicate);
        let target = xsd_to_column_type(&dt);
        if max_c <= 1 {
            ("attribute".to_string(), target, 0.95_f64.min(lit_pct))
        } else {
            ("child_table".to_string(), target, (0.75_f64 * lit_pct).min(0.90))
        }
    } else if iri_pct >= 0.95 {
        if max_c <= 1 {
            ("foreign_key".to_string(), "iri".to_string(), 0.90_f64.min(iri_pct))
        } else {
            ("graph".to_string(), "iri".to_string(), 0.50_f64.min(iri_pct))
        }
    } else if bns > 0 {
        ("graph".to_string(), "bnode".to_string(), 0.60)
    } else {
        ("graph".to_string(), "mixed".to_string(), 0.30)
    };

    (shape, cardinality.to_string(), target_type, confidence)
}

fn is_rdf_list(predicate: &str) -> bool {
    let sparql = format!(
        "SELECT ?head WHERE {{ \
         ?s <{predicate}> ?head . \
         ?head <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> ?_f ; \
               <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest>  ?_r . \
         }} LIMIT 1"
    );
    query_bindings(&sparql).map(|b| !b.is_empty()).unwrap_or(false)
}

fn list_value_type(predicate: &str) -> String {
    let sparql = format!(
        "SELECT ?dt (COUNT(*) AS ?n) WHERE {{ \
         ?s <{predicate}> ?head . \
         ?head <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest>*/\
<http://www.w3.org/1999/02/22-rdf-syntax-ns#first> ?item . \
         FILTER(isLiteral(?item)) BIND(datatype(?item) AS ?dt) \
         }} GROUP BY ?dt ORDER BY DESC(?n) LIMIT 1"
    );
    query_first_row(&sparql)
        .ok()
        .and_then(|row| row_iri(&row, "dt"))
        .map(|dt| xsd_to_column_type(&dt))
        .unwrap_or_else(|| "iri".into())
}

fn dominant_datatype(predicate: &str) -> String {
    let dt_sparql = format!(
        "SELECT ?dt (COUNT(*) AS ?n) WHERE {{ \
         ?s <{predicate}> ?o . FILTER(isLiteral(?o)) BIND(datatype(?o) AS ?dt) \
         }} GROUP BY ?dt ORDER BY DESC(?n) LIMIT 1"
    );
    query_first_row(&dt_sparql)
        .ok()
        .and_then(|row| row_iri(&row, "dt"))
        .unwrap_or_default()
}

fn xsd_to_column_type(datatype: &str) -> String {
    match datatype {
        "http://www.w3.org/2001/XMLSchema#integer"
        | "http://www.w3.org/2001/XMLSchema#int"
        | "http://www.w3.org/2001/XMLSchema#long"
        | "http://www.w3.org/2001/XMLSchema#short"
        | "http://www.w3.org/2001/XMLSchema#byte"
        | "http://www.w3.org/2001/XMLSchema#nonNegativeInteger"
        | "http://www.w3.org/2001/XMLSchema#positiveInteger" => "integer".into(),
        "http://www.w3.org/2001/XMLSchema#decimal"
        | "http://www.w3.org/2001/XMLSchema#double"
        | "http://www.w3.org/2001/XMLSchema#float" => "decimal".into(),
        "http://www.w3.org/2001/XMLSchema#boolean" => "boolean".into(),
        "http://www.w3.org/2001/XMLSchema#date"
        | "http://www.w3.org/2001/XMLSchema#gYear"
        | "http://www.w3.org/2001/XMLSchema#gYearMonth" => "date".into(),
        "http://www.w3.org/2001/XMLSchema#dateTime" => "datetime".into(),
        _ => "string".into(),
    }
}

// ---------------------------------------------------------------------
// graph-callbacks helpers.
// ---------------------------------------------------------------------

/// Assert `args` is one IRI and return its lexical form. Same shape
/// checks `wf_profile` uses for its per-predicate probes.
fn require_iri(args: &[WitTerm], func: &str) -> Result<String, String> {
    let [arg] = args else {
        return Err(format!(
            "{func}: expected 1 argument (predicate IRI), got {}",
            args.len()
        ));
    };
    match arg {
        WitTerm::NamedNode(i) => Ok(i.clone()),
        WitTerm::BlankNode(_) => {
            Err(format!("{func}: argument must be an IRI, got blank node"))
        }
        WitTerm::Literal(_) => {
            Err(format!("{func}: argument must be an IRI, got literal"))
        }
    }
}

/// Dispatch a SPARQL string through `graph-callbacks::execute-query`
/// and unwrap the bindings arm. Non-bindings arms are reported as an
/// error string — every query this crate issues is a `SELECT`.
fn query_bindings(sparql: &str) -> Result<Vec<WitBinding>, String> {
    let result = gc::execute_query(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => {
            format!("graph-callbacks syntax-error: {m}")
        }
        gc::GraphCallError::BackendError(m) => {
            format!("graph-callbacks backend-error: {m}")
        }
        gc::GraphCallError::NotPermitted(m) => {
            format!("graph-callbacks not-permitted: {m}")
        }
    })?;
    match result {
        CallbackQueryResult::Bindings(b) => Ok(b),
        CallbackQueryResult::Quads(_) => {
            Err("graph-callbacks: SELECT unexpectedly returned quads".into())
        }
        CallbackQueryResult::Boolean(_) => {
            Err("graph-callbacks: SELECT unexpectedly returned boolean".into())
        }
    }
}

/// Group a flat `list<binding>` (per the Phase-1 host-callbacks WIT
/// shape) into rows keyed by the sequence of variable names the SELECT
/// projects. The reference `host-callbacks-impl` emits every variable
/// of every solution back-to-back in solution order; we detect the
/// row boundary by variable-name reset — the first time a variable we
/// have already seen reappears, a new row has started.
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

fn query_first_row(sparql: &str) -> Result<Vec<WitBinding>, String> {
    let flat = query_bindings(sparql)?;
    Ok(group_bindings_into_rows(flat).into_iter().next().unwrap_or_default())
}

/// Convenience: run a SELECT expected to yield one row with one
/// integer-typed variable, return the value as i64 or None if the
/// row / variable is absent.
fn query_int_scalar(sparql: &str, var: &str) -> Result<Option<i64>, String> {
    let row = query_first_row(sparql)?;
    Ok(row_int(&row, var))
}

fn row_int(row: &[WitBinding], var: &str) -> Option<i64> {
    row.iter().find(|b| b.variable == var).and_then(|b| match &b.value {
        WitTerm::Literal(l) => l.value.parse::<i64>().ok(),
        _ => None,
    })
}

fn row_iri(row: &[WitBinding], var: &str) -> Option<String> {
    row.iter().find(|b| b.variable == var).and_then(|b| match &b.value {
        WitTerm::NamedNode(s) => Some(s.clone()),
        WitTerm::Literal(l) => Some(l.value.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Literal constructors for return terms.
// ---------------------------------------------------------------------

fn int_literal(n: i64) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: n.to_string(),
        datatype: Some(XSD_INTEGER.to_string()),
        language: None,
    })
}

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.to_string(),
        datatype: Some(RDF_JSON.to_string()),
        language: None,
    })
}

#[allow(dead_code)]
fn string_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.to_string(),
        datatype: Some(XSD_STRING.to_string()),
        language: None,
    })
}

bindings::export!(Component with_types_in bindings);
