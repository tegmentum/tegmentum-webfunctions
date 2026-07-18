//! wf_materialize — descriptor-driven materialization to a substrate
//! sink.
//!
//! Signature: `wf:materialize("<descriptor-json>")` returns an
//! rdf:JSON literal shaped as `{"rows": N}` (single-term collapse
//! matching the batch3/batch4 convention).
//!
//! Runs the descriptor's implied SELECT over the graph via
//! `graph-callbacks::execute-query`, then emits one typed quad per
//! (subject, column-predicate, value) pair through
//! `sink-callbacks::emit-quads`. The substrate's sink adapter owns
//! the projection into the backend's storage shape (SQL for
//! SQLite/DuckDB/Postgres; XQuery for SirixDB; native upserts for
//! vector stores).
//!
//! Migration deviations (Follow-up F, sink-* rewrite):
//!
//!   * Descriptor's `sink` field is now a substrate-side sink NAME
//!     (not a driver URL). Sinks must be pre-registered with the host;
//!     `list-sinks` validates presence before the extension proceeds.
//!   * Guest-side DDL emission is dropped. The Stardog-era
//!     `sink-open` / `sink-execute(CREATE TABLE ...)` triple that made
//!     the extension author co-own the sink's schema does not survive
//!     substrate-neutrality review (`sink-callbacks.md` §2); the sink
//!     owns its own schema now.
//!   * Descriptor's `registry` field is ignored on this substrate.
//!     The Stardog-era pattern of writing an extra `(name,
//!     descriptor)` row via arbitrary SQL against a shape-registry
//!     table does not map to `emit-quad`. A follow-on revision may
//!     lift the shape-registry into a substrate-side surface; until
//!     then the guest silently skips registry writes.
//!   * Descriptor's per-column `constraint` (min/max/enum/length) is
//!     also ignored. Constraints belonged in the SQL DDL the guest
//!     used to emit; on the new substrate the sink owns integrity.
//!     `wf_validate` continues to enforce them at query time.

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
    self as gc, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::sink_callbacks::{self as sc, SinkError};
use bindings::tegmentum::webfunction::types::{
    Binding as WitBinding, Literal as WitLiteral, Quad as WitQuad, Term as WitTerm,
};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    shape: String,
    anchor: Anchor,
    columns: Vec<Column>,
    sink: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    registry: Option<String>,
    #[serde(default)]
    graph: Option<String>,
}

#[derive(Deserialize)]
struct Anchor {
    class: Option<String>,
    predicate_signature: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Column {
    name: String,
    role: String,
    predicate: Option<String>,
    #[serde(default = "default_type")]
    r#type: String,
    #[serde(default = "default_cardinality")]
    #[allow(dead_code)]
    cardinality: String,
}

fn default_type() -> String {
    "string".into()
}

fn default_cardinality() -> String {
    "0..1".into()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.into(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    })
}

fn map_graph_err(e: gc::GraphCallError) -> String {
    match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    }
}

fn map_sink_err(e: SinkError) -> String {
    match e {
        SinkError::NoSuchSink(m) => format!("sink-callbacks no-such-sink: {m}"),
        SinkError::SchemaViolation(m) => format!("sink-callbacks schema-violation: {m}"),
        SinkError::BackendError(m) => format!("sink-callbacks backend-error: {m}"),
        SinkError::NotPermitted(m) => format!("sink-callbacks not-permitted: {m}"),
    }
}

fn xsd_datatype_for(t: &str) -> Option<&'static str> {
    match t {
        "integer" => Some("http://www.w3.org/2001/XMLSchema#integer"),
        "decimal" => Some("http://www.w3.org/2001/XMLSchema#decimal"),
        "boolean" => Some("http://www.w3.org/2001/XMLSchema#boolean"),
        "date" => Some("http://www.w3.org/2001/XMLSchema#date"),
        "datetime" => Some("http://www.w3.org/2001/XMLSchema#dateTime"),
        "string" => None, // rdf:PlainLiteral shape (datatype omitted)
        _ => None,
    }
}

fn typed_literal(value: &str, xsd_dt: Option<&str>) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: value.into(),
        datatype: xsd_dt.map(|s| s.to_string()),
        language: None,
    })
}

/// Cast a value from the source graph to the descriptor's column type.
/// Non-literal terms (IRIs, bnodes) pass through unchanged so a
/// foreign-key column carries the referenced IRI.
fn coerce_to_column_type(term: &WitTerm, target_type: &str) -> WitTerm {
    match term {
        WitTerm::Literal(l) => typed_literal(&l.value, xsd_datatype_for(target_type)),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// SPARQL construction
// ---------------------------------------------------------------------------

fn build_select(d: &Descriptor) -> Result<String, String> {
    let mut projection: Vec<String> = vec!["?subject".into()];
    let mut patterns: Vec<String> = Vec::new();

    if let Some(class) = &d.anchor.class {
        patterns.push(format!("?subject a <{class}> ."));
    } else if let Some(sig) = &d.anchor.predicate_signature {
        for (i, p) in sig.iter().enumerate() {
            patterns.push(format!("?subject <{p}> ?_sig{i} ."));
        }
    } else {
        return Err(
            "wf_materialize: anchor missing both `class` and `predicate_signature`".into(),
        );
    }

    for col in &d.columns {
        if col.role == "subject_iri" {
            continue;
        }
        let pred = col
            .predicate
            .as_deref()
            .ok_or_else(|| format!("column `{}` needs `predicate`", col.name))?;
        let var = format!("?{}", col.name);
        projection.push(var.clone());
        let triple = format!("?subject <{pred}> {var} .");
        match col.cardinality.as_str() {
            "1" | "1..n" => patterns.push(triple),
            _ => patterns.push(format!("OPTIONAL {{ {triple} }}")),
        }
    }

    let where_body = if let Some(g) = &d.graph {
        format!("GRAPH <{g}> {{ {} }}", patterns.join(" "))
    } else {
        patterns.join(" ")
    };
    Ok(format!(
        "SELECT {} WHERE {{ {} }}",
        projection.join(" "),
        where_body
    ))
}

// ---------------------------------------------------------------------------
// Sink discovery + row -> quads
// ---------------------------------------------------------------------------

fn validate_sink_present(name: &str) -> Result<(), String> {
    let sinks = sc::list_sinks();
    if sinks.iter().any(|s| s.name == name) {
        Ok(())
    } else {
        Err(format!(
            "wf_materialize: sink `{name}` not registered with host (list-sinks returned {} sinks)",
            sinks.len()
        ))
    }
}

/// Reconstruct rows from a flat `list<binding>` by splitting on repeated
/// variable identity — the R1 shape returned by
/// `graph-callbacks::query-result::bindings`.
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

/// Extract the subject term from a materializer row. The SELECT
/// projects `?subject` at position 0.
fn row_subject(row: &[WitBinding]) -> Option<WitTerm> {
    row.iter()
        .find(|b| b.variable == "subject")
        .map(|b| b.value.clone())
}

fn build_row_quads(row: &[WitBinding], d: &Descriptor) -> Option<Vec<WitQuad>> {
    let subject = row_subject(row)?;
    let mut quads: Vec<WitQuad> = Vec::new();
    let graph_iri = d.graph.clone();

    if let Some(class) = &d.anchor.class {
        quads.push(WitQuad {
            subject: subject.clone(),
            predicate: WitTerm::NamedNode(RDF_TYPE.into()),
            object: WitTerm::NamedNode(class.clone()),
            graph: graph_iri.clone(),
        });
    }

    for col in &d.columns {
        if col.role == "subject_iri" {
            continue;
        }
        let Some(pred) = col.predicate.as_deref() else {
            continue;
        };
        let Some(binding) = row.iter().find(|b| b.variable == col.name) else {
            continue; // OPTIONAL absent — skip quad
        };
        let object = coerce_to_column_type(&binding.value, &col.r#type);
        quads.push(WitQuad {
            subject: subject.clone(),
            predicate: WitTerm::NamedNode(pred.into()),
            object,
            graph: graph_iri.clone(),
        });
    }

    Some(quads)
}

// ---------------------------------------------------------------------------
// Guest entrypoint
// ---------------------------------------------------------------------------

fn materialize_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let descriptor_json = match args.first() {
        Some(WitTerm::Literal(l)) => l.value.clone(),
        Some(other) => {
            return Err(format!(
                "wf_materialize: first arg must be a string literal, got {other:?}"
            ));
        }
        None => return Err("wf_materialize: expected one arg (descriptor json)".into()),
    };
    let d: Descriptor = serde_json::from_str(&descriptor_json)
        .map_err(|e| format!("wf_materialize: descriptor parse: {e}"))?;

    let sink_name = d
        .sink
        .as_deref()
        .ok_or_else(|| "wf_materialize: descriptor has no `sink`".to_string())?;
    validate_sink_present(sink_name)?;

    let sparql = build_select(&d)?;
    let result = gc::execute_query(&sparql).map_err(map_graph_err)?;
    let flat = match result {
        CallbackQueryResult::Bindings(bs) => bs,
        _ => {
            return Err(
                "wf_materialize: descriptor SELECT must yield bindings, not CONSTRUCT/ASK".into(),
            );
        }
    };
    let rows = split_rows(flat);

    // Accumulate a flat quad list across rows so we make a single
    // emit-quads call per materialization — matches the R1 batch
    // discipline of amortising sink transaction costs.
    let mut all_quads: Vec<WitQuad> = Vec::new();
    let mut source_row_count: u64 = 0;
    for row in &rows {
        let Some(row_quads) = build_row_quads(row, &d) else {
            continue;
        };
        all_quads.extend(row_quads);
        source_row_count += 1;
    }

    if all_quads.is_empty() {
        let out = json!({ "rows": source_row_count });
        return Ok(json_literal(&out.to_string()));
    }

    let accepted = sc::emit_quads(sink_name, &all_quads).map_err(map_sink_err)?;

    let out = json!({
        "source_rows": source_row_count,
        "quads_accepted": accepted,
    });
    Ok(json_literal(&out.to_string()))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_materialize".into(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_materialize" => materialize_impl(&args),
            other => Err(format!("wf_materialize: unknown function '{other}'")),
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
            "wf_materialize: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_materialize: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_materialize: aggregate state was never constructed".into())
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
            "wf_materialize: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
