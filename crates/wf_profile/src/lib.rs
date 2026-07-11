//! wf_profile — dataset shape profiler.
//!
//! Signature: `wf:call(<wf_profile.wasm>)`
//!    → binding-set { predicate, shape, cardinality, target_type,
//!                    subjects, triples, confidence }
//!
//! Walks the store's distinct predicates, and for each one runs three
//! probes (max cardinality per subject, object type distribution, and —
//! for literal-valued predicates — dominant datatype) to classify the
//! predicate's shape as attribute / foreign_key / child_table / tree /
//! graph. Emits one row per predicate; the user consumes the report and
//! hand-crafts (or auto-generates in a follow-up tool) shape descriptors
//! from the rows that look worth materializing.
//!
//! Tree detection is skipped in v1 — it needs a full second SPARQL pass
//! per candidate parent predicate to check the single-parent + acyclic
//! invariants. v1 marks tree-shape candidates as graph and lets the user
//! promote them by hand. A follow-up `wf_profile_trees.wasm` guest can
//! do the deep check for a specific predicate on demand.
//!
//! Targets WIT world v0.5.0 (needs only execute-query; the sink-* imports
//! stay unused but the guest links against the same world as its
//! materializer sibling).

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

// ---------------------------------------------------------------------------
// Guest impl
// ---------------------------------------------------------------------------

impl Guest for Component {
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        // Predicate enumeration. Pull triple counts and distinct-subject
        // counts so downstream shape decisions can weigh whether a
        // predicate carries enough data to bother demoting.
        let enum_sparql = "\
            SELECT ?p (COUNT(*) AS ?triples) (COUNT(DISTINCT ?s) AS ?subjects) \
            WHERE { ?s ?p ?o } \
            GROUP BY ?p \
            ORDER BY DESC(?triples)";
        let enum_bs = host::execute_query(enum_sparql, &[], None)?;

        let mut rows: Vec<Vec<Binding>> = Vec::with_capacity(enum_bs.rows.len());
        for row in &enum_bs.rows {
            let p = match binding_iri(row, "p") {
                Some(v) => v,
                None => continue,
            };
            let triples = binding_int(row, "triples").unwrap_or(0);
            let subjects = binding_int(row, "subjects").unwrap_or(0);
            let (shape, cardinality, target_type, confidence) =
                classify(&p, subjects);
            rows.push(build_row(
                &p,
                &shape,
                &cardinality,
                &target_type,
                subjects,
                triples,
                confidence,
            ));
        }

        Ok(BindingSets {
            vars: vec![
                "predicate".into(),
                "shape".into(),
                "cardinality".into(),
                "target_type".into(),
                "subjects".into(),
                "triples".into(),
                "confidence".into(),
            ],
            rows,
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_profile: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_profile: aggregate not applicable".into())
    }
    fn cardinality_estimate(
        _input: Cardinality,
        _args: Vec<Value>,
    ) -> Result<Cardinality, String> {
        Ok(Cardinality {
            value: 100.0,
            accuracy: Accuracy::Injected,
        })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: Value::Literal(Literal {
                    label: "wf_profile() -> one row per predicate with \
                            classification (attribute/foreign_key/\
                            child_table/tree/graph), cardinality, dominant \
                            object type, coverage counts, and a confidence \
                            score."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

fn classify(
    predicate: &str,
    subjects: i64,
) -> (String, String, String, f64) {
    let card_sparql = format!(
        "SELECT (MAX(?c) AS ?maxc) WHERE {{ \
         SELECT ?s (COUNT(?o) AS ?c) WHERE {{ ?s <{predicate}> ?o }} \
         GROUP BY ?s }}"
    );
    let max_c = host::execute_query(&card_sparql, &[], Some(1))
        .ok()
        .and_then(|bs| bs.rows.into_iter().next())
        .and_then(|row| binding_int(&row, "maxc"))
        .unwrap_or(0);

    let mix_sparql = format!(
        "SELECT \
           (SUM(IF(isIRI(?o), 1, 0)) AS ?iris) \
           (SUM(IF(isLiteral(?o), 1, 0)) AS ?lits) \
           (SUM(IF(isBlank(?o), 1, 0)) AS ?bns) \
         WHERE {{ ?s <{predicate}> ?o }}"
    );
    let mix = host::execute_query(&mix_sparql, &[], Some(1))
        .ok()
        .and_then(|bs| bs.rows.into_iter().next());
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

    // Check for RDF Collection / rdf:list shape BEFORE dispatching on
    // iri_pct vs lit_pct, because a list head may be either a bnode
    // (pre-skolemize) or an IRI (post-skolemize) but the shape is the
    // same. Any predicate whose object typically has rdf:first + rdf:rest
    // out-edges is a list head, regardless of whether the head is a
    // bnode or a genid IRI. Only reachable for functional predicates —
    // real lists have one head per subject.
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
            // Could be a tree; v1 doesn't check invariants, so emit as
            // graph and let the user promote manually.
            ("graph".to_string(), "iri".to_string(), 0.50_f64.min(iri_pct))
        }
    } else if bns > 0 {
        // Blank-node objects that aren't RDF Collection heads — OWL
        // Restrictions, reified statements, ad-hoc structures. Not
        // demotion candidates.
        ("graph".to_string(), "bnode".to_string(), 0.60)
    } else {
        ("graph".to_string(), "mixed".to_string(), 0.30)
    };

    (shape, cardinality.to_string(), target_type, confidence)
}

/// Is the predicate's object typically the head of an RDF Collection?
/// Detection: probe whether the object carries both `rdf:first` and
/// `rdf:rest` out-edges. Works pre-skolemize (head is a bnode) and
/// post-skolemize (head is a genid IRI) — the shape lives in the
/// out-edges, not the head's identity. One SELECT with LIMIT 1 is
/// enough; a mid-chain surgery that hides the head shape is a corner
/// case we accept as a false-negative rather than a false-positive.
fn is_rdf_list(predicate: &str) -> bool {
    let sparql = format!(
        "SELECT ?head WHERE {{ \
         ?s <{predicate}> ?head . \
         ?head <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> ?_f ; \
               <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest>  ?_r . \
         }}"
    );
    host::execute_query(&sparql, &[], Some(1))
        .map(|bs| !bs.rows.is_empty())
        .unwrap_or(false)
}

/// For a predicate whose object is an RDF Collection head, discover the
/// dominant datatype of the chain's leaf values. Samples up to 100 first
/// items via `rdf:first` — we only need enough to pick one xsd datatype
/// or notice that the leaves are IRIs.
fn list_value_type(predicate: &str) -> String {
    let sparql = format!(
        "SELECT ?dt (COUNT(*) AS ?n) (COUNT(IF(isIRI(?item), 1, 1)) AS ?iri_n) WHERE {{ \
         ?s <{predicate}> ?head . \
         ?head <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest>*/\
<http://www.w3.org/1999/02/22-rdf-syntax-ns#first> ?item . \
         FILTER(isLiteral(?item)) BIND(datatype(?item) AS ?dt) \
         }} GROUP BY ?dt ORDER BY DESC(?n)"
    );
    host::execute_query(&sparql, &[], Some(1))
        .ok()
        .and_then(|bs| bs.rows.into_iter().next())
        .and_then(|row| binding_iri(&row, "dt"))
        .map(|dt| xsd_to_column_type(&dt))
        .unwrap_or_else(|| "iri".into())
}

fn dominant_datatype(predicate: &str) -> String {
    let dt_sparql = format!(
        "SELECT ?dt (COUNT(*) AS ?n) WHERE {{ \
         ?s <{predicate}> ?o . FILTER(isLiteral(?o)) BIND(datatype(?o) AS ?dt) \
         }} GROUP BY ?dt ORDER BY DESC(?n)"
    );
    host::execute_query(&dt_sparql, &[], Some(1))
        .ok()
        .and_then(|bs| bs.rows.into_iter().next())
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

fn binding_iri(row: &[Binding], name: &str) -> Option<String> {
    row.iter().find(|b| b.name == name).and_then(|b| match &b.value {
        Value::Iri(s) => Some(s.clone()),
        Value::Literal(l) => Some(l.label.clone()),
        _ => None,
    })
}

fn binding_int(row: &[Binding], name: &str) -> Option<i64> {
    row.iter().find(|b| b.name == name).and_then(|b| match &b.value {
        Value::Literal(l) => l.label.parse::<i64>().ok(),
        _ => None,
    })
}

fn build_row(
    predicate: &str,
    shape: &str,
    cardinality: &str,
    target_type: &str,
    subjects: i64,
    triples: i64,
    confidence: f64,
) -> Vec<Binding> {
    vec![
        Binding { name: "predicate".into(), value: Value::Iri(predicate.into()) },
        Binding { name: "shape".into(), value: string_literal(shape) },
        Binding { name: "cardinality".into(), value: string_literal(cardinality) },
        Binding { name: "target_type".into(), value: string_literal(target_type) },
        Binding { name: "subjects".into(), value: int_literal(subjects) },
        Binding { name: "triples".into(), value: int_literal(triples) },
        Binding { name: "confidence".into(), value: decimal_literal(confidence) },
    ]
}

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

fn int_literal(n: i64) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
}

fn decimal_literal(x: f64) -> Value {
    Value::Literal(Literal {
        label: format!("{x:.2}"),
        datatype: XSD_DECIMAL.into(),
        lang: None,
    })
}

export!(Component);
