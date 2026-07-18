//! wf_materialize_list — RDF-Collection-to-substrate-sink materializer.
//!
//! Signature: `wf:materialize_list("<descriptor-json>")` returns an
//! rdf:JSON literal shaped as `{"source_rows": N, "quads_accepted": M}`.
//!
//! For a `shape=list` descriptor, walks each anchor subject's
//! `rdf:first`/`rdf:rest` chain via
//! `prepared-query-callbacks::{prepare-query, run-prepared,
//! free-prepared}` and emits typed positional quads at a named sink
//! via `sink-callbacks::emit-quads`. Each list element becomes a
//! quad `(subject, <descriptor.list_element_predicate>, value)`
//! carrying the descriptor's declared value type. Positional ordering
//! is preserved via a companion `(subject, <..#position>, idx)`
//! quad — the substrate sink projects both into whatever ordered
//! storage the backend uses.
//!
//! Migration deviations (Follow-up F, sink-* rewrite):
//!
//!   * Descriptor's `sink` field is a substrate-side sink NAME (not a
//!     driver URL). Sinks must be pre-registered; `list-sinks`
//!     validates presence.
//!   * Guest-side DDL emission is dropped. The Stardog-era shape
//!     `(subject TEXT, idx INTEGER, value <T>, PRIMARY KEY(subject,
//!     idx))` used to be authored by the guest via
//!     `sink-execute(CREATE TABLE ...)`. On the new substrate the
//!     sink adapter owns its schema.
//!   * The chain-walk still amortises SPARQL parse cost via
//!     `prepared-query-callbacks::prepare-query`, unchanged.

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
use bindings::tegmentum::webfunction::prepared_query_callbacks::{
    self as pq, PreparedError,
};
use bindings::tegmentum::webfunction::sink_callbacks::{self as sc, SinkError};
use bindings::tegmentum::webfunction::types::{
    Binding as WitBinding, Literal as WitLiteral, Quad as WitQuad, Term as WitTerm,
};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

// The substrate contract does not fix a positional-order predicate for
// list materialisation; the guest declares its own under the tegmentum
// namespace so the sink adapter can project it without further
// vocabulary coordination.
const TEGMENTUM_LIST_POSITION: &str = "http://tegmentum.ai/ns/list/position";

#[derive(Deserialize)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    shape: String,
    anchor: Anchor,
    list_predicate: String,
    #[serde(default = "default_value_type")]
    value_type: String,
    sink: Option<String>,
    /// The predicate the sink stores the list value under. Defaults to
    /// the descriptor's `list_predicate` when omitted (preserves the
    /// original chain predicate at the sink surface).
    #[serde(default)]
    list_element_predicate: Option<String>,
}

#[derive(Deserialize)]
struct Anchor {
    class: Option<String>,
    predicate_signature: Option<Vec<String>>,
}

fn default_value_type() -> String {
    "string".into()
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

fn xsd_datatype_for(t: &str) -> Option<&'static str> {
    match t {
        "integer" => Some("http://www.w3.org/2001/XMLSchema#integer"),
        "decimal" => Some("http://www.w3.org/2001/XMLSchema#decimal"),
        "boolean" => Some("http://www.w3.org/2001/XMLSchema#boolean"),
        "date" => Some("http://www.w3.org/2001/XMLSchema#date"),
        "datetime" => Some("http://www.w3.org/2001/XMLSchema#dateTime"),
        _ => None,
    }
}

fn coerce_to_value_type(v: &WitTerm, target: &str) -> WitTerm {
    match v {
        WitTerm::Literal(l) => WitTerm::Literal(WitLiteral {
            value: l.value.clone(),
            datatype: xsd_datatype_for(target).map(|s| s.to_string()),
            language: None,
        }),
        other => other.clone(),
    }
}

fn map_graph_err(e: gc::GraphCallError) -> String {
    match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    }
}

fn map_prepared_err(e: PreparedError) -> String {
    match e {
        PreparedError::SyntaxError(m) => format!("prepared-query syntax-error: {m}"),
        PreparedError::BackendError(m) => format!("prepared-query backend-error: {m}"),
        PreparedError::UnknownHandle => "prepared-query unknown-handle".into(),
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

fn validate_sink_present(name: &str) -> Result<(), String> {
    let sinks = sc::list_sinks();
    if sinks.iter().any(|s| s.name == name) {
        Ok(())
    } else {
        Err(format!(
            "wf_materialize_list: sink `{name}` not registered with host \
             (list-sinks returned {} sinks)",
            sinks.len()
        ))
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

fn build_head_query(d: &Descriptor) -> Result<String, String> {
    let anchor_pattern = if let Some(class) = &d.anchor.class {
        format!("?subject a <{class}> . ")
    } else if let Some(sig) = &d.anchor.predicate_signature {
        let mut s = String::new();
        for (i, p) in sig.iter().enumerate() {
            s.push_str(&format!("?subject <{p}> ?_sig{i} . "));
        }
        s
    } else {
        return Err(
            "wf_materialize_list: anchor missing both `class` and `predicate_signature`".into(),
        );
    };
    Ok(format!(
        "SELECT ?subject ?head WHERE {{ {}?subject <{}> ?head }}",
        anchor_pattern, d.list_predicate,
    ))
}

fn term_iri(t: &WitTerm) -> Option<String> {
    match t {
        WitTerm::NamedNode(s) => Some(s.clone()),
        WitTerm::BlankNode(s) => Some(format!("_:{s}")),
        _ => None,
    }
}

fn head_binding_for(node_iri: &str) -> WitBinding {
    let value = if let Some(rest) = node_iri.strip_prefix("_:") {
        WitTerm::BlankNode(rest.to_string())
    } else {
        WitTerm::NamedNode(node_iri.to_string())
    };
    WitBinding {
        variable: "head".into(),
        value,
    }
}

fn integer_literal(n: i64) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: n.to_string(),
        datatype: Some(XSD_INTEGER.into()),
        language: None,
    })
}

fn subject_term_from_iri(iri: &str) -> WitTerm {
    if let Some(rest) = iri.strip_prefix("_:") {
        WitTerm::BlankNode(rest.to_string())
    } else {
        WitTerm::NamedNode(iri.to_string())
    }
}

// ---------------------------------------------------------------------------
// Guest entrypoint
// ---------------------------------------------------------------------------

fn materialize_list_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let descriptor_json = match args.first() {
        Some(WitTerm::Literal(l)) => l.value.clone(),
        _ => {
            return Err(
                "wf_materialize_list: first arg must be a descriptor-json string literal"
                    .into(),
            );
        }
    };

    let d: Descriptor = serde_json::from_str(&descriptor_json)
        .map_err(|e| format!("wf_materialize_list: descriptor parse: {e}"))?;
    if d.shape != "list" {
        return Err(format!(
            "wf_materialize_list: descriptor shape must be `list`, got `{}`",
            d.shape
        ));
    }
    let sink_name = d
        .sink
        .as_deref()
        .ok_or_else(|| "wf_materialize_list: descriptor has no `sink`".to_string())?;
    validate_sink_present(sink_name)?;

    let element_predicate = d
        .list_element_predicate
        .clone()
        .unwrap_or_else(|| d.list_predicate.clone());

    let head_query = build_head_query(&d)?;
    let head_result = gc::execute_query(&head_query).map_err(map_graph_err)?;
    let flat = match head_result {
        CallbackQueryResult::Bindings(bs) => bs,
        _ => {
            return Err(
                "wf_materialize_list: head query must yield bindings, not CONSTRUCT/ASK".into(),
            );
        }
    };
    let head_rows = split_rows(flat);

    let step_handle = pq::prepare_query(
        "SELECT ?value ?rest WHERE { \
         ?head <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> ?value ; \
               <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest>  ?rest }",
    )
    .map_err(map_prepared_err)?;

    let mut all_quads: Vec<WitQuad> = Vec::new();
    let mut source_row_count: u64 = 0;
    const MAX_CHAIN_STEPS: i64 = 100_000;

    for row in &head_rows {
        let subject_iri = match row
            .iter()
            .find(|b| b.variable == "subject")
            .and_then(|b| term_iri(&b.value))
        {
            Some(s) => s,
            None => continue,
        };
        let head_iri = match row
            .iter()
            .find(|b| b.variable == "head")
            .and_then(|b| term_iri(&b.value))
        {
            Some(h) => h,
            None => continue,
        };

        let subject_term = subject_term_from_iri(&subject_iri);
        let mut cur = head_iri;
        let mut idx: i64 = 0;
        while cur != RDF_NIL && idx < MAX_CHAIN_STEPS {
            let inputs = vec![head_binding_for(&cur)];
            let step_flat = pq::run_prepared(step_handle, &inputs).map_err(map_prepared_err)?;
            let step_rows = split_rows(step_flat);
            let step_row = match step_rows.first() {
                Some(r) => r,
                None => break,
            };
            let value = match step_row.iter().find(|b| b.variable == "value") {
                Some(b) => coerce_to_value_type(&b.value, &d.value_type),
                None => break,
            };
            all_quads.push(WitQuad {
                subject: subject_term.clone(),
                predicate: WitTerm::NamedNode(element_predicate.clone()),
                object: value,
                graph: None,
            });
            all_quads.push(WitQuad {
                subject: subject_term.clone(),
                predicate: WitTerm::NamedNode(TEGMENTUM_LIST_POSITION.into()),
                object: integer_literal(idx),
                graph: None,
            });
            source_row_count += 1;
            idx += 1;
            cur = match step_row
                .iter()
                .find(|b| b.variable == "rest")
                .and_then(|b| term_iri(&b.value))
            {
                Some(n) => n,
                None => break,
            };
        }
    }

    pq::free_prepared(step_handle);

    if all_quads.is_empty() {
        let out = json!({ "source_rows": source_row_count, "quads_accepted": 0 });
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
            name: "wf_materialize_list".into(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_materialize_list" => materialize_list_impl(&args),
            other => Err(format!("wf_materialize_list: unknown function '{other}'")),
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
            "wf_materialize_list: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_materialize_list: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_materialize_list: aggregate state was never constructed".into())
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
            "wf_materialize_list: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
