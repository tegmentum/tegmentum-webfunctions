//! wf_sql — arbitrary SQL as a first-class SPARQL SERVICE.
//!
//! Signature: `wf:call(<wf_sql.wasm>, "<sink-url>", "<sql>")`
//!    → binding-set with one binding per SQL projected column.
//!
//! This is the compositional glue that lets SPARQL and SQL live in the
//! same query. A SERVICE-envelope call runs the SQL against the sink
//! and returns its projection as binding-sets; the outer SPARQL query
//! then joins, filters, or CONSTRUCTs against those rows just like any
//! other pattern.
//!
//! Combined with `wf_materialize` (SPARQL → SQL) this closes every
//! cell of the SPARQL×SQL transformation matrix:
//!   * graph → graph:  CONSTRUCT
//!   * graph → table:  SELECT
//!   * graph → state:  UPDATE
//!   * graph → bool:   ASK
//!   * table → graph:  CONSTRUCT + initial bindings (via SERVICE +
//!                     wf_sql, or via SERVICE + wf_fetch)
//!   * table → table:  wf_sql (this guest)
//!   * table → state:  wf_sql (INSERT/UPDATE/DELETE)
//!
//! No new vocabulary — the CONSTRUCT template IS the R2RML-style
//! mapping; wf_sql is just how the row set gets in.
//!
//! The guest is intentionally thin: it opens the sink, runs
//! sink-execute verbatim, and returns whatever comes back. Type
//! inference and column naming are the sink backend's responsibility.
//! For SQLite (v1 backend) that means INTEGER → xsd:integer, REAL →
//! xsd:decimal, TEXT → xsd:string, and column names come from the
//! SELECT's projection.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let sink_url = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            Some(Value::Iri(s)) => s.clone(),
            _ => {
                return Err(
                    "wf_sql: first arg must be a sink URL (string or IRI literal)"
                        .into(),
                );
            }
        };
        let sql = match args.get(1) {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_sql: second arg must be a SQL query string literal".into(),
                );
            }
        };

        let handle = host::sink_open(&sink_url)?;
        let result = host::sink_execute(handle, &sql, &[]);
        host::sink_close(handle).ok();
        result.map_err(|e| format!("wf_sql: {e}"))
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_sql: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_sql: aggregate not applicable".into())
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
                    label: "wf_sql(\"<sink-url>\", \"<sql>\") — arbitrary SQL \
                            against a sink, returned as binding-sets. Callable \
                            from SPARQL as SERVICE <wf:call> so a query can \
                            mix graph patterns and SQL rows in the same WHERE."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

export!(Component);
