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
    /// Optional named graph the shape lives in. Absent = default graph.
    /// Present = anchor pattern and column reads scope to this GRAPH
    /// clause, sink schema includes a graph column, subsequent
    /// wf_demote scopes its DELETE to the same graph.
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
    cardinality: String,
    #[serde(default)]
    constraint: Option<Constraint>,
}

#[derive(Deserialize, Default)]
struct Constraint {
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    min_length: Option<usize>,
    #[serde(default)]
    max_length: Option<usize>,
    #[serde(default)]
    r#enum: Option<Vec<serde_json::Value>>,
    // regex is not emitted as SQLite DDL (no native support); wf_validate
    // still checks it at query time.
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
        // Graph identifier stored alongside every row so a sink
        // consumer can distinguish default-graph facts from named-graph
        // ones without re-consulting the descriptor. Empty string ⇔
        // default graph.
        let graph_lit = Value::Literal(Literal {
            label: d.graph.clone().unwrap_or_default(),
            datatype: XSD_STRING.into(),
            lang: None,
        });
        for row in rows.rows {
            let params = align_params(&row, &d.columns, &graph_lit);
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

    // Wrap the pattern in a GRAPH clause when the descriptor scopes to
    // a named graph. Absent = default graph, no wrapping needed.
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

fn build_ddl(table: &str, cols: &[Column]) -> String {
    let mut parts: Vec<String> = Vec::new();
    // Universal graph column — NULL when the source triple lived in the
    // default graph, or the named-graph IRI otherwise. Kept ahead of
    // user columns so the sink schema always starts with it.
    parts.push("_graph TEXT".to_string());
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
        // Emit descriptor constraint block as SQL CHECK where the
        // backend supports it. SQLite supports CHECK on all storage
        // classes; regex is not native and is intentionally skipped
        // (wf_validate still enforces regex at query time). Each check
        // is quoted individually so a mismatch reports column + kind.
        if let Some(c) = &col.constraint {
            let mut checks: Vec<String> = Vec::new();
            if let Some(min) = c.min {
                checks.push(format!("{} >= {}", col.name, min));
            }
            if let Some(max) = c.max {
                checks.push(format!("{} <= {}", col.name, max));
            }
            if let Some(min_len) = c.min_length {
                checks.push(format!("LENGTH({}) >= {}", col.name, min_len));
            }
            if let Some(max_len) = c.max_length {
                checks.push(format!("LENGTH({}) <= {}", col.name, max_len));
            }
            if let Some(enum_set) = &c.r#enum {
                let literal_list = enum_set
                    .iter()
                    .map(sql_literal)
                    .collect::<Vec<_>>()
                    .join(", ");
                checks.push(format!("{} IN ({})", col.name, literal_list));
            }
            for chk in checks {
                piece.push_str(&format!(" CHECK ({chk})"));
            }
        }
        parts.push(piece);
    }
    format!(
        "CREATE TABLE IF NOT EXISTS {} ({})",
        table,
        parts.join(", ")
    )
}

/// Render a JSON enum member as a SQL literal. Numbers pass through as
/// digits; everything else becomes a single-quoted string, doubling any
/// embedded quote per SQL escape rules.
fn sql_literal(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

fn build_insert(table: &str, cols: &[Column]) -> String {
    // _graph is emitted ahead of user columns to match build_ddl's
    // column order; align_params below produces the graph slot first
    // as well.
    let mut names: Vec<&str> = vec!["_graph"];
    names.extend(cols.iter().map(|c| c.name.as_str()));
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

/// Rearrange one row's bindings into (graph, ...descriptor columns...)
/// order. The graph literal is stamped ahead of every user column so
/// the parameter list matches build_insert's placeholder count.
/// Missing OPTIONAL cells become empty-string literals.
fn align_params(row: &[Binding], cols: &[Column], graph_lit: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(cols.len() + 1);
    out.push(graph_lit.clone());
    for col in cols {
        let name = if col.role == "subject_iri" {
            "subject"
        } else {
            col.name.as_str()
        };
        let v = row
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.value.clone())
            .unwrap_or_else(|| {
                Value::Literal(Literal {
                    label: String::new(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                })
            });
        out.push(v);
    }
    out
}

fn integer_literal(n: i64) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
}

export!(Component);
