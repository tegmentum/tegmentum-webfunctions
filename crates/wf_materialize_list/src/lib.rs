//! wf_materialize_list — RDF-Collection-to-child-table materializer.
//!
//! Signature: `wf:call(<wf_materialize_list.wasm>, "<descriptor-json>")`
//!    → binding-set { rows: xsd:integer }
//!
//! For a shape=list descriptor, walks the `rdf:first`/`rdf:rest` chain
//! starting from every anchor subject's list head, and emits ordered
//! (subject, idx, value) rows into the sink. The sink table is created
//! idempotently as `(subject TEXT NOT NULL, idx INTEGER NOT NULL, value
//! <value_type>, PRIMARY KEY (subject, idx))`.
//!
//! v1 handles RDF Collections (rdf:first/rdf:rest chains). RDF Containers
//! (rdf:_1, rdf:_2, ...) are a straightforward variant — enumerate by
//! predicate name pattern instead of walking — added in a follow-up if
//! real data has them.
//!
//! Uses prepare-query for the chain-walk step so each anchor's chain
//! amortises SPARQL parsing across N recursion steps.

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
const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";

#[derive(Deserialize)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    shape: String,
    anchor: Anchor,
    list_predicate: String,
    #[serde(default = "default_value_type")]
    value_type: String,
    sink: Option<String>,
}

#[derive(Deserialize)]
struct Anchor {
    class: Option<String>,
    predicate_signature: Option<Vec<String>>,
}

fn default_value_type() -> String {
    "string".into()
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let descriptor_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_materialize_list: first arg must be a descriptor json literal"
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
        let sink_url = d
            .sink
            .as_deref()
            .ok_or_else(|| "wf_materialize_list: descriptor has no `sink`".to_string())?;

        // Open sink + create the child table. Fixed shape: (subject TEXT
        // NOT NULL, idx INTEGER NOT NULL, value <T>, PRIMARY KEY).
        let handle = host::sink_open(sink_url)?;
        let table = table_name_from(sink_url);
        let value_sqlite_type = sqlite_type_for(&d.value_type);
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
             subject TEXT NOT NULL, \
             idx INTEGER NOT NULL, \
             value {value_sqlite_type} NOT NULL, \
             PRIMARY KEY (subject, idx))"
        );
        host::sink_execute(handle, &ddl, &[])
            .map_err(|e| format!("wf_materialize_list: create table: {e}"))?;

        // Enumerate anchor subjects + their list heads. One SELECT covers
        // both — the JOIN is trivial and lets us skip subjects that don't
        // have a list.
        let head_query = build_head_query(&d)?;
        let heads = host::execute_query(&head_query, &[], None)?;

        // Prepare the chain-walk step so we amortise SPARQL parse over
        // every N-element chain we traverse.
        let step_prepared = host::prepare_query(
            "SELECT ?value ?rest WHERE { \
             ?head <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> ?value ; \
                   <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest>  ?rest }",
        )?;

        let insert = format!(
            "INSERT OR IGNORE INTO {table} (subject, idx, value) VALUES (?, ?, ?)"
        );

        let mut row_count = 0u64;
        for row in &heads.rows {
            let subject = match binding_iri(row, "subject") {
                Some(s) => s,
                None => continue,
            };
            let head = match binding_iri(row, "head") {
                Some(h) => h,
                None => continue,
            };

            let mut cur = head;
            let mut idx: i64 = 0;
            // Cap the chain walk defensively — a malformed cycle shouldn't
            // hang the materializer. 100k is well above any realistic list.
            const MAX_CHAIN_STEPS: i64 = 100_000;
            while cur != RDF_NIL && idx < MAX_CHAIN_STEPS {
                let step_result = host::run_prepared(
                    step_prepared,
                    &[bnode_binding("head", &cur)],
                    Some(1),
                )?;
                let step_row = match step_result.rows.first() {
                    Some(r) => r,
                    None => break, // dangling chain — end here
                };
                let value = match binding_value(step_row, "value") {
                    Some(v) => v,
                    None => break,
                };
                host::sink_execute(
                    handle,
                    &insert,
                    &[
                        string_lit(&subject),
                        int_lit(idx),
                        value,
                    ],
                )
                .map_err(|e| {
                    format!(
                        "wf_materialize_list: insert row (subject={subject}, idx={idx}): {e}"
                    )
                })?;
                row_count += 1;
                idx += 1;
                cur = match binding_iri_or_nil(step_row, "rest") {
                    Some(n) => n,
                    None => break,
                };
            }
        }

        host::sink_close(handle).ok();

        Ok(BindingSets {
            vars: vec!["rows".into()],
            rows: vec![vec![Binding {
                name: "rows".into(),
                value: int_literal(row_count as i64),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_materialize_list: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_materialize_list: aggregate not applicable".into())
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
                    label:
                        "wf_materialize_list(\"<descriptor-json>\") — walks \
                         rdf:first/rdf:rest chains starting from each anchor \
                         subject's list head, emits ordered (subject, idx, \
                         value) rows to the sink. Returns row count."
                            .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Query construction + helpers
// ---------------------------------------------------------------------------

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
            "wf_materialize_list: anchor missing both `class` and `predicate_signature`"
                .into(),
        );
    };
    Ok(format!(
        "SELECT ?subject ?head WHERE {{ {anchor}?subject <{lp}> ?head }}",
        anchor = anchor_pattern,
        lp = d.list_predicate,
    ))
}

fn table_name_from(url: &str) -> String {
    url.rsplit_once('#')
        .map(|(_, frag)| frag.to_string())
        .unwrap_or_else(|| "list_t".into())
}

fn sqlite_type_for(t: &str) -> &'static str {
    match t {
        "integer" => "INTEGER",
        "decimal" => "REAL",
        "boolean" => "INTEGER",
        _ => "TEXT",
    }
}

// ---------------------------------------------------------------------------
// Binding accessors
// ---------------------------------------------------------------------------

fn binding_value(row: &[Binding], name: &str) -> Option<Value> {
    row.iter().find(|b| b.name == name).map(|b| b.value.clone())
}

fn binding_iri(row: &[Binding], name: &str) -> Option<String> {
    row.iter().find(|b| b.name == name).and_then(|b| match &b.value {
        Value::Iri(s) => Some(s.clone()),
        Value::Bnode(s) => Some(format!("_:{s}")),
        _ => None,
    })
}

/// Like binding_iri but also accepts an rdf:nil marker so chain
/// termination is transparent to the caller.
fn binding_iri_or_nil(row: &[Binding], name: &str) -> Option<String> {
    row.iter().find(|b| b.name == name).and_then(|b| match &b.value {
        Value::Iri(s) => Some(s.clone()),
        Value::Bnode(s) => Some(format!("_:{s}")),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Value constructors
// ---------------------------------------------------------------------------

fn bnode_binding(name: &str, iri_or_bnode: &str) -> Binding {
    // If the value came in as a bnode ("_:x" prefix), re-emit as Bnode;
    // otherwise treat as IRI. Chain heads are usually bnodes but a lifted
    // list (interned URIs) is possible.
    let value = if let Some(rest) = iri_or_bnode.strip_prefix("_:") {
        Value::Bnode(rest.to_string())
    } else {
        Value::Iri(iri_or_bnode.to_string())
    };
    Binding {
        name: name.to_string(),
        value,
    }
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

fn int_literal(n: i64) -> Value {
    int_lit(n)
}

export!(Component);
