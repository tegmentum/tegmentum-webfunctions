//! wf_fetch — read a materialized shape back through its sink.
//!
//! Signatures:
//!   * `wf:call(<wf_fetch.wasm>, "<descriptor-json>")`
//!         — return all rows.
//!   * `wf:call(<wf_fetch.wasm>, "<descriptor-json>", "<sql-tail>")`
//!         — append the SQL tail (typically `WHERE col op literal
//!         [ORDER BY …] [LIMIT n]`) to the projection.
//!
//! Return: binding-set with one binding per descriptor column, using
//! the column name as the binding variable name. Callers project the
//! same names in their outer SELECT and get the shape's rows directly.
//!
//! Uses the descriptor's column list to build a projection so the guest
//! doesn't have to introspect the sink's schema (avoids a round-trip),
//! and so the WIT-side variable names come out in the same order every
//! run. The subject-IRI column comes back as a WIT iri variant; every
//! other column as an xsd-typed literal.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde::Deserialize;

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";
const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";

#[derive(Deserialize)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    shape: String,
    columns: Vec<Column>,
    sink: Option<String>,
    /// wf-relational v0.1 (design memo `wf-relational.md` §04): when
    /// `false`, `_graph` is omitted from the SELECT projection. Real
    /// Postgres tables don't carry a `_graph` column, so a Postgres-
    /// backed shape must set this false or the SQL will fail with
    /// `column "_graph" does not exist`. Defaults to `true` to keep
    /// backward compatibility with the SQLite sink path where the
    /// materializer always adds `_graph` at seed time.
    #[serde(default = "default_include_graph")]
    include_graph: bool,
    /// wf-relational v0.1 (memo §04): informational field naming the
    /// sink family (`"sqlite"`, `"duckdb"`, `"postgres"`, `"sirix"`).
    /// v0.1 does not branch on this — the sink URL scheme is what
    /// picks the host backend at `sink_open` time — but the descriptor
    /// carries it so a v0.2+ SQL-dialect selector can lift it without
    /// a shape migration.
    #[allow(dead_code)]
    #[serde(default)]
    sink_kind: Option<String>,
}

fn default_include_graph() -> bool {
    true
}

#[derive(Deserialize)]
struct Column {
    name: String,
    role: String,
    #[serde(default = "default_type")]
    r#type: String,
    #[allow(dead_code)]
    #[serde(default)]
    predicate: Option<String>,
}

fn default_type() -> String {
    "string".into()
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let descriptor_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_fetch: first arg must be a descriptor-json string literal"
                        .into(),
                );
            }
        };
        let sql_tail: String = args
            .get(1)
            .map(|v| match v {
                Value::Literal(l) => l.label.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();

        let d: Descriptor = serde_json::from_str(&descriptor_json)
            .map_err(|e| format!("wf_fetch: descriptor parse: {e}"))?;
        let sink_url = d
            .sink
            .as_deref()
            .ok_or_else(|| "wf_fetch: descriptor has no `sink`".to_string())?;

        let table = table_name_from(sink_url);
        // _graph goes ahead of user columns in the projection so
        // downstream binding lookup finds it under a stable name. Sink
        // consumers that don't project ?_graph in their outer SELECT
        // just ignore it. wf-relational sources (Postgres tables that
        // never grew a _graph column) suppress this by setting
        // `include_graph: false` in the descriptor — the projection
        // then contains only the declared shape columns.
        let mut projection: Vec<String> = Vec::with_capacity(d.columns.len() + 1);
        if d.include_graph {
            projection.push("_graph".to_string());
        }
        projection.extend(d.columns.iter().map(|c| c.name.clone()));
        let column_list = projection.join(", ");
        let sql = if sql_tail.is_empty() {
            format!("SELECT {column_list} FROM {table}")
        } else {
            format!("SELECT {column_list} FROM {table} {sql_tail}")
        };

        let handle = host::sink_open(sink_url)?;
        let raw = host::sink_execute(handle, &sql, &[])
            .map_err(|e| format!("wf_fetch: sink query `{sql}`: {e}"))?;
        host::sink_close(handle).ok();

        // The sink returns rows with column-name bindings already. We
        // remap them into WIT value shapes matched to the descriptor's
        // declared column types (the sink's own type inference is looser
        // — TEXT → xsd:string always — so we tighten it here).
        let mut vars: Vec<String> = Vec::with_capacity(d.columns.len() + 1);
        if d.include_graph {
            vars.push("_graph".into());
        }
        vars.extend(d.columns.iter().map(|c| c.name.clone()));

        let mut rows: Vec<Vec<Binding>> = Vec::with_capacity(raw.rows.len());
        for row in &raw.rows {
            let mut out = Vec::with_capacity(d.columns.len() + 1);
            // Surface the graph column as an iri when non-empty; skip
            // the binding entirely for default-graph rows so consumers
            // see it as UNDEF rather than as an empty-string literal
            // (matches SPARQL's convention that default-graph facts
            // have no graph identifier at all). Suppressed entirely
            // for wf-relational-style sinks whose table has no
            // `_graph` column (memo §04).
            let graph_val = if d.include_graph {
                row.iter().find(|b| b.name == "_graph").map(|b| &b.value)
            } else {
                None
            };
            if let Some(gv) = graph_val {
                let non_empty = match gv {
                    Value::Literal(l) if l.label.is_empty() => false,
                    Value::Literal(_) | Value::Iri(_) => true,
                    Value::Bnode(_) => false,
                };
                if non_empty {
                    let iri_val = match gv {
                        Value::Iri(s) => Value::Iri(s.clone()),
                        Value::Literal(l) => Value::Iri(l.label.clone()),
                        other => other.clone(),
                    };
                    out.push(Binding {
                        name: "_graph".into(),
                        value: iri_val,
                    });
                }
            }
            for col in &d.columns {
                let raw_val = row.iter().find(|b| b.name == col.name).map(|b| &b.value);
                if let Some(v) = raw_val {
                    let typed = retype(v, &col.role, &col.r#type);
                    out.push(Binding {
                        name: col.name.clone(),
                        value: typed,
                    });
                }
            }
            rows.push(out);
        }

        Ok(BindingSets { vars, rows })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_fetch: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_fetch: aggregate not applicable".into())
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
                    label: "wf_fetch(\"<descriptor-json>\", \"[<sql-tail>]\") — \
                            SELECT the descriptor's columns from its sink, \
                            optionally appending an SQL tail. Returns rows \
                            as WIT binding-sets keyed by column name, with \
                            xsd datatypes drawn from the descriptor."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Type coercion — descriptor is the source of truth
// ---------------------------------------------------------------------------

fn retype(v: &Value, role: &str, ty: &str) -> Value {
    // Subject-IRI column always surfaces as an iri, regardless of what
    // the sink stored it as. The descriptor guarantees this is an IRI.
    if role == "subject_iri" {
        return match v {
            Value::Iri(s) => Value::Iri(s.clone()),
            Value::Literal(l) => Value::Iri(l.label.clone()),
            other => other.clone(),
        };
    }
    // Non-subject columns: tag literals with the descriptor's declared
    // xsd datatype. IRIs pass through unchanged (the `type: "iri"` case
    // covers foreign-key columns that reference other shapes).
    match v {
        Value::Iri(s) if ty == "iri" => Value::Iri(s.clone()),
        Value::Iri(s) => Value::Iri(s.clone()),
        Value::Literal(l) => Value::Literal(Literal {
            label: l.label.clone(),
            datatype: xsd_iri(ty).into(),
            lang: None,
        }),
        Value::Bnode(b) => Value::Bnode(b.clone()),
    }
}

fn xsd_iri(t: &str) -> &'static str {
    match t {
        "integer" => XSD_INTEGER,
        "decimal" => XSD_DECIMAL,
        "boolean" => XSD_BOOLEAN,
        "date" => XSD_DATE,
        "datetime" => XSD_DATETIME,
        _ => XSD_STRING,
    }
}

fn table_name_from(url: &str) -> String {
    url.rsplit_once('#')
        .map(|(_, frag)| frag.to_string())
        .unwrap_or_else(|| "t".into())
}

export!(Component);
