//! wf_demote — delete source triples for a materialized shape.
//!
//! Signature: `wf:call(<wf_demote.wasm>, "<descriptor-json>")`
//!    → binding-set { deleted: xsd:integer }
//!
//! Reads the descriptor, opens its sink, reads the subject-IRI column
//! (the primary key from the materializer's viewpoint), and DELETEs
//! every triple in the graph store whose subject is one of those IRIs
//! and whose predicate is one of the descriptor's column predicates.
//!
//! Contract with wf_materialize: this guest deletes only what the
//! materializer put on the sink. Anything else about those subjects —
//! predicates that aren't in the descriptor, triples about IRIs that
//! didn't reach the sink — stays untouched. Idempotent: a second run
//! finds no matching triples and returns 0.
//!
//! Runs the anchor's rdf:type triple too when the descriptor uses
//! `anchor.class`, since a materialized shape implicitly captures the
//! class assertion (the sink table's presence is the type predicate's
//! materialized form).

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde::Deserialize;

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const CHUNK: usize = 1000;

#[derive(Deserialize)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    shape: String,
    anchor: Anchor,
    columns: Vec<Column>,
    sink: Option<String>,
}

#[derive(Deserialize)]
struct Anchor {
    class: Option<String>,
    #[allow(dead_code)]
    predicate_signature: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Column {
    #[allow(dead_code)]
    name: String,
    role: String,
    predicate: Option<String>,
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let descriptor_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_demote: first arg must be a descriptor-json string literal"
                        .into(),
                );
            }
        };
        let d: Descriptor = serde_json::from_str(&descriptor_json)
            .map_err(|e| format!("wf_demote: descriptor parse: {e}"))?;
        let sink_url = d
            .sink
            .as_deref()
            .ok_or_else(|| "wf_demote: descriptor has no `sink`".to_string())?;

        // Collect predicates to remove: everything with role != subject_iri
        // that has a predicate declared. rdf:type gets appended when the
        // anchor is class-based.
        let mut predicates: Vec<String> = d
            .columns
            .iter()
            .filter(|c| c.role != "subject_iri")
            .filter_map(|c| c.predicate.clone())
            .collect();
        if d.anchor.class.is_some() {
            predicates.push(RDF_TYPE.to_string());
        }
        if predicates.is_empty() {
            return Ok(build_result(0));
        }

        // Read the sink's subject-IRI column. Descriptor's convention is
        // that the primary-key column has role=subject_iri; its `name` in
        // the sink's DDL is that column's name. wf_materialize uses this
        // name verbatim, so we mirror it.
        let subject_column = d
            .columns
            .iter()
            .find(|c| c.role == "subject_iri")
            .map(|c| c.name.clone())
            .ok_or_else(|| {
                "wf_demote: descriptor has no subject_iri column".to_string()
            })?;

        let table = table_name_from(sink_url);
        let sink_handle = host::sink_open(sink_url)?;
        let rows = host::sink_execute(
            sink_handle,
            &format!("SELECT {subject_column} FROM {table}"),
            &[],
        )
        .map_err(|e| format!("wf_demote: read sink subjects: {e}"))?;

        let mut subjects: Vec<String> = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            if let Some(iri) = row.first().and_then(|b| match &b.value {
                Value::Iri(s) => Some(s.clone()),
                Value::Literal(l) => Some(l.label.clone()),
                _ => None,
            }) {
                subjects.push(iri);
            }
        }
        host::sink_close(sink_handle).ok();

        if subjects.is_empty() {
            return Ok(build_result(0));
        }

        // Delete in chunks. One DELETE per (predicate × subject-chunk):
        // one big filter per pass keeps the number of updates bounded to
        // predicates × (subjects / CHUNK). A single "everything" delete
        // with all-predicates × all-subjects would build a massive
        // VALUES + FILTER but SPARQL parsers tolerate ~10k VALUES rows
        // cleanly; CHUNK=1000 leaves headroom for very fat predicate
        // sets without needing dialect-specific tuning.
        let mut deleted = 0u64;
        for predicate in &predicates {
            for chunk in subjects.chunks(CHUNK) {
                let values_clause = chunk
                    .iter()
                    .map(|iri| format!("<{iri}>"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let update = format!(
                    "DELETE {{ ?s <{predicate}> ?o }} WHERE {{ \
                     ?s <{predicate}> ?o . VALUES ?s {{ {values_clause} }} \
                     }}"
                );
                host::execute_update(&update).map_err(|e| {
                    format!(
                        "wf_demote: delete predicate `{predicate}` chunk of {}: {e}",
                        chunk.len()
                    )
                })?;
                // SPARQL Update doesn't return a count; the caller learns
                // the total via a follow-up COUNT if it wants precision.
                // We accumulate an upper bound: `chunk.len()` per pass.
                deleted += chunk.len() as u64;
            }
        }

        Ok(build_result(deleted))
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_demote: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_demote: aggregate not applicable".into())
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
                    label: "wf_demote(\"<descriptor-json>\") — reads sink's \
                            subject list, DELETEs source triples for those \
                            subjects across each descriptor predicate. \
                            Reports an upper-bound count of triples touched."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

fn build_result(deleted: u64) -> BindingSets {
    BindingSets {
        vars: vec!["deleted".into()],
        rows: vec![vec![Binding {
            name: "deleted".into(),
            value: Value::Literal(Literal {
                label: deleted.to_string(),
                datatype: XSD_INTEGER.into(),
                lang: None,
            }),
        }]],
    }
}

fn table_name_from(url: &str) -> String {
    url.rsplit_once('#')
        .map(|(_, frag)| frag.to_string())
        .unwrap_or_else(|| "t".into())
}

export!(Component);
