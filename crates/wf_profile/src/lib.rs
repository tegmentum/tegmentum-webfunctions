//! wf_profile — dataset shape profiler.
//!
//! Migrated (Follow-up E) from the Stardog overlay
//! `stardog:webfunction@0.5.0` world to the base
//! `tegmentum:webfunction/extension-with-host-callbacks@0.1.0` world.
//!
//! Signature: `wf_profile()` -> rdf:JSON literal
//!   `{ "predicates": [ { predicate, shape, cardinality, target_type,
//!                        subjects, triples, confidence }, ... ] }`
//!
//! Walks the store's distinct predicates via
//! `graph-callbacks::execute-query`, and for each one runs three probes
//! (max cardinality per subject, object type distribution, and — for
//! literal-valued predicates — dominant datatype) to classify the
//! predicate's shape as attribute / foreign_key / child_table / list /
//! graph. The Stardog-era shape returned one row per predicate; the
//! base `extension::call` surface returns a single term, so the report
//! collapses to one rdf:JSON literal whose top-level `predicates`
//! array carries one object per predicate with the same seven keys.

#[allow(warnings)]
mod bindings;

use serde_json::{json, Value as JsonValue};

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
    self as gc, Binding as WitBinding, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";

// ---------------------------------------------------------------------------
// graph-callbacks helpers
// ---------------------------------------------------------------------------

fn execute_query(sparql: &str) -> Result<CallbackQueryResult, String> {
    gc::execute_query(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

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
    match execute_query(sparql)? {
        CallbackQueryResult::Bindings(bs) => Ok(group_bindings_into_rows(bs)),
        CallbackQueryResult::Quads(_) => {
            Err("wf_profile: SELECT expected but graph-callbacks returned quads".into())
        }
        CallbackQueryResult::Boolean(_) => {
            Err("wf_profile: SELECT expected but graph-callbacks returned boolean".into())
        }
    }
}

// ---------------------------------------------------------------------------
// Guest impls
// ---------------------------------------------------------------------------

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_profile".into(),
            min_arity: 0,
            max_arity: Some(0),
        }]
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_profile" => wf_profile_impl(),
            other => Err(format!("wf_profile: unknown function '{other}'")),
        }
    }
}

fn wf_profile_impl() -> Result<WitTerm, String> {
    // Predicate enumeration.
    let enum_sparql = "\
        SELECT ?p (COUNT(*) AS ?triples) (COUNT(DISTINCT ?s) AS ?subjects) \
        WHERE { ?s ?p ?o } \
        GROUP BY ?p \
        ORDER BY DESC(?triples)";
    let enum_rows = select_rows(enum_sparql)?;

    let mut predicates: Vec<JsonValue> = Vec::with_capacity(enum_rows.len());
    for row in &enum_rows {
        let p = match binding_iri(row, "p") {
            Some(v) => v,
            None => continue,
        };
        let triples = binding_int(row, "triples").unwrap_or(0);
        let subjects = binding_int(row, "subjects").unwrap_or(0);
        let (shape, cardinality, target_type, confidence) = classify(&p, subjects);
        predicates.push(json!({
            "predicate": p,
            "shape": shape,
            "cardinality": cardinality,
            "target_type": target_type,
            "subjects": subjects,
            "triples": triples,
            "confidence": confidence,
        }));
    }

    let out = json!({ "predicates": predicates });
    let serialized = serde_json::to_string(&out)
        .map_err(|e| format!("wf_profile: serialize summary: {e}"))?;
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
            "wf_profile: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_profile: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_profile: aggregate state was never constructed".into())
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
            "wf_profile: unknown property function '{name}' (this component provides none)"
        ))
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

fn classify(predicate: &str, subjects: i64) -> (String, String, String, f64) {
    let card_sparql = format!(
        "SELECT (MAX(?c) AS ?maxc) WHERE {{ \
         SELECT ?s (COUNT(?o) AS ?c) WHERE {{ ?s <{predicate}> ?o }} \
         GROUP BY ?s }}"
    );
    let max_c = select_rows(&card_sparql)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| binding_int(&row, "maxc"))
        .unwrap_or(0);

    let mix_sparql = format!(
        "SELECT \
           (SUM(IF(isIRI(?o), 1, 0)) AS ?iris) \
           (SUM(IF(isLiteral(?o), 1, 0)) AS ?lits) \
           (SUM(IF(isBlank(?o), 1, 0)) AS ?bns) \
         WHERE {{ ?s <{predicate}> ?o }}"
    );
    let mix = select_rows(&mix_sparql)
        .ok()
        .and_then(|rows| rows.into_iter().next());
    let iris = mix.as_ref().and_then(|r| binding_int(r, "iris")).unwrap_or(0);
    let lits = mix.as_ref().and_then(|r| binding_int(r, "lits")).unwrap_or(0);
    let bns = mix.as_ref().and_then(|r| binding_int(r, "bns")).unwrap_or(0);
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
            (
                "child_table".to_string(),
                target,
                (0.75_f64 * lit_pct).min(0.90),
            )
        }
    } else if iri_pct >= 0.95 {
        if max_c <= 1 {
            (
                "foreign_key".to_string(),
                "iri".to_string(),
                0.90_f64.min(iri_pct),
            )
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
    select_rows(&sparql).map(|rows| !rows.is_empty()).unwrap_or(false)
}

fn list_value_type(predicate: &str) -> String {
    let sparql = format!(
        "SELECT ?dt (COUNT(*) AS ?n) (COUNT(IF(isIRI(?item), 1, 1)) AS ?iri_n) WHERE {{ \
         ?s <{predicate}> ?head . \
         ?head <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest>*/\
<http://www.w3.org/1999/02/22-rdf-syntax-ns#first> ?item . \
         FILTER(isLiteral(?item)) BIND(datatype(?item) AS ?dt) \
         }} GROUP BY ?dt ORDER BY DESC(?n) LIMIT 1"
    );
    select_rows(&sparql)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| binding_iri(&row, "dt"))
        .map(|dt| xsd_to_column_type(&dt))
        .unwrap_or_else(|| "iri".into())
}

fn dominant_datatype(predicate: &str) -> String {
    let dt_sparql = format!(
        "SELECT ?dt (COUNT(*) AS ?n) WHERE {{ \
         ?s <{predicate}> ?o . FILTER(isLiteral(?o)) BIND(datatype(?o) AS ?dt) \
         }} GROUP BY ?dt ORDER BY DESC(?n) LIMIT 1"
    );
    select_rows(&dt_sparql)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| binding_iri(&row, "dt"))
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

// ---------------------------------------------------------------------------
// Binding helpers
// ---------------------------------------------------------------------------

fn binding_iri(row: &[WitBinding], name: &str) -> Option<String> {
    row.iter()
        .find(|b| b.variable == name)
        .and_then(|b| match &b.value {
            WitTerm::NamedNode(s) => Some(s.clone()),
            WitTerm::Literal(l) => Some(l.value.clone()),
            _ => None,
        })
}

fn binding_int(row: &[WitBinding], name: &str) -> Option<i64> {
    row.iter()
        .find(|b| b.variable == name)
        .and_then(|b| match &b.value {
            WitTerm::Literal(l) => l.value.parse::<i64>().ok(),
            _ => None,
        })
}

bindings::export!(Component with_types_in bindings);
