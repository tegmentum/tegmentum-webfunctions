//! wf_materialize — descriptor-driven materialization to a sink.
//!
//! Signature: `wf:call(<wf_materialize.wasm>, "<descriptor-json>")`
//!    → binding-set { rows: xsd:integer }
//!
//! Reads the shape descriptor, opens the sink, creates the target table
//! from the descriptor's columns (idempotent — `CREATE TABLE IF NOT
//! EXISTS`), runs the implied SELECT via `execute-query`, and streams
//! each row into the sink via `sink-execute` INSERTs. Returns the number
//! of rows written.
//!
//! v1 scope: shape=attribute descriptors targeting `sqlite://`. tree
//! shapes need a different materializer (subtree assembly, not row
//! streaming) — `wf_materialize_tree.wasm` in a follow-up. foreign_key
//! and child_table are handled here transparently: FKs become an IRI
//! column, child tables get their own descriptor (users invoke
//! wf_materialize once per shape).
//!
//! Targets WIT world v0.5.0 for the `sink-*` host imports.

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

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Descriptor {
    name: String,
    #[allow(dead_code)]
    shape: String,
    anchor: Anchor,
    columns: Vec<Column>,
    sink: Option<String>,
    #[serde(default)]
    registry: Option<String>,
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
    cardinality: String,
}

fn default_type() -> String {
    "string".into()
}

fn default_cardinality() -> String {
    "0..1".into()
}

// ---------------------------------------------------------------------------
// Guest impl
// ---------------------------------------------------------------------------

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let descriptor_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            Some(other) => {
                return Err(format!(
                    "wf_materialize: first arg must be a string literal, got {other:?}"
                ));
            }
            None => {
                return Err(
                    "wf_materialize: expected one arg (descriptor json)".into(),
                );
            }
        };

        let d: Descriptor = serde_json::from_str(&descriptor_json)
            .map_err(|e| format!("wf_materialize: descriptor parse: {e}"))?;

        let sink_url = d
            .sink
            .as_deref()
            .ok_or_else(|| "wf_materialize: descriptor has no `sink`".to_string())?;

        // Build the SPARQL SELECT from the descriptor. Anchor produces the
        // ?subject variable; each column projects one value under its
        // column name. Cardinality 0..1/0..n columns become OPTIONAL
        // patterns so subjects missing that value still surface.
        let sparql = build_select(&d)?;

        // Sink: open once, create the target table, stream rows.
        let handle = host::sink_open(sink_url)?;
        let table_name = table_name_from(sink_url);
        let ddl = build_ddl(&table_name, &d.columns);
        host::sink_execute(handle, &ddl, &[])
            .map_err(|e| format!("wf_materialize: create table: {e}"))?;

        // Read the source rows and insert. execute-query pulls at most
        // 100_000 rows per call by default; for larger shapes the guest
        // should paginate (v2). We accept the cap for v1 and surface it
        // if the source overflows.
        let rows = host::execute_query(&sparql, &[], None)?;

        let mut row_count = 0u64;
        let insert = build_insert(&table_name, &d.columns);
        for row in rows.rows {
            let params = align_params(&row, &d.columns);
            host::sink_execute(handle, &insert, &params)
                .map_err(|e| format!("wf_materialize: insert row {row_count}: {e}"))?;
            row_count += 1;
        }

        host::sink_close(handle).ok(); // best-effort; frame closes on exit anyway

        // Register this shape in the planner's shape registry if the
        // descriptor named one. Stores the descriptor JSON verbatim so
        // the planner can parse it back for column names + sink URL.
        if let Some(registry_url) = &d.registry {
            let reg_handle = host::sink_open(registry_url)?;
            let reg_table = registry_table_from(registry_url);
            host::sink_execute(
                reg_handle,
                &format!(
                    "CREATE TABLE IF NOT EXISTS {reg_table} (\
                     name TEXT PRIMARY KEY, \
                     descriptor TEXT NOT NULL)"
                ),
                &[],
            )
            .map_err(|e| format!("wf_materialize: create registry table: {e}"))?;
            host::sink_execute(
                reg_handle,
                &format!(
                    "INSERT OR REPLACE INTO {reg_table} (name, descriptor) VALUES (?, ?)"
                ),
                &[
                    Value::Literal(Literal {
                        label: d.name.clone(),
                        datatype: XSD_STRING.into(),
                        lang: None,
                    }),
                    Value::Literal(Literal {
                        label: descriptor_json.clone(),
                        datatype: XSD_STRING.into(),
                        lang: None,
                    }),
                ],
            )
            .map_err(|e| format!("wf_materialize: registry insert: {e}"))?;
            host::sink_close(reg_handle).ok();
        }

        Ok(BindingSets {
            vars: vec!["rows".into()],
            rows: vec![vec![Binding {
                name: "rows".into(),
                value: integer_literal(row_count as i64),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_materialize: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_materialize: aggregate not applicable".into())
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
                        "wf_materialize(\"<descriptor-json>\") -> reads descriptor, \
                         opens sink, CREATE TABLE IF NOT EXISTS, runs implied SELECT, \
                         streams rows via sink-execute INSERT. Returns row count."
                            .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Query construction
// ---------------------------------------------------------------------------

fn build_select(d: &Descriptor) -> Result<String, String> {
    let mut projection: Vec<String> = vec!["?subject".into()];
    let mut patterns: Vec<String> = Vec::new();

    // Anchor: rdf:type triple, or presence of every predicate in the
    // signature.
    if let Some(class) = &d.anchor.class {
        patterns.push(format!("?subject a <{class}> ."));
    } else if let Some(sig) = &d.anchor.predicate_signature {
        for (i, p) in sig.iter().enumerate() {
            patterns.push(format!("?subject <{p}> ?_sig{i} ."));
        }
    } else {
        return Err(
            "wf_materialize: anchor missing both `class` and `predicate_signature`"
                .into(),
        );
    }

    for col in &d.columns {
        if col.role == "subject_iri" {
            continue; // captured by ?subject
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

    Ok(format!(
        "SELECT {} WHERE {{ {} }}",
        projection.join(" "),
        patterns.join(" ")
    ))
}

fn build_ddl(table: &str, cols: &[Column]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for col in cols {
        let sqlite_type = sqlite_type_for(&col.r#type);
        let nullable = matches!(col.cardinality.as_str(), "1" | "1..n");
        let mut piece = format!("{} {}", col.name, sqlite_type);
        if nullable {
            piece.push_str(" NOT NULL");
        }
        if col.role == "subject_iri" {
            piece.push_str(" PRIMARY KEY");
        }
        parts.push(piece);
    }
    format!(
        "CREATE TABLE IF NOT EXISTS {} ({})",
        table,
        parts.join(", ")
    )
}

fn build_insert(table: &str, cols: &[Column]) -> String {
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    let placeholders = std::iter::repeat("?")
        .take(names.len())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT OR IGNORE INTO {} ({}) VALUES ({})",
        table,
        names.join(", "),
        placeholders
    )
}

fn sqlite_type_for(t: &str) -> &'static str {
    match t {
        "integer" => "INTEGER",
        "decimal" => "REAL",
        "boolean" => "INTEGER",
        "date" | "datetime" => "TEXT",
        _ => "TEXT",
    }
}

/// Extract the table name from a sink URL fragment (`sqlite:///…#name`).
/// Falls back to `t` for degenerate URLs so the DDL at least succeeds.
fn table_name_from(url: &str) -> String {
    url.rsplit_once('#')
        .map(|(_, frag)| frag.to_string())
        .unwrap_or_else(|| "t".into())
}

fn registry_table_from(url: &str) -> String {
    url.rsplit_once('#')
        .map(|(_, frag)| frag.to_string())
        .unwrap_or_else(|| "shapes".into())
}

/// Rearrange one row's bindings into the descriptor's column order,
/// substituting an empty-string literal for missing OPTIONAL cells.
fn align_params(row: &[Binding], cols: &[Column]) -> Vec<Value> {
    cols.iter()
        .map(|col| {
            let name = if col.role == "subject_iri" {
                "subject"
            } else {
                col.name.as_str()
            };
            row.iter()
                .find(|b| b.name == name)
                .map(|b| b.value.clone())
                .unwrap_or_else(|| {
                    Value::Literal(Literal {
                        label: String::new(),
                        datatype: XSD_STRING.into(),
                        lang: None,
                    })
                })
        })
        .collect()
}

fn integer_literal(n: i64) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
}

export!(Component);
