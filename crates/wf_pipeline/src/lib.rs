//! wf_pipeline — declarative sequential composition of substrate steps.
//!
//! Signature: `wf:call(<wf_pipeline.wasm>, "<plan-json>")`
//!    → binding-set { step, kind, name, ok, detail }
//!
//! Reads a JSON plan with an ordered list of steps and runs each one
//! against the substrate's host imports. Returns one row per step so
//! the caller can see exactly what happened and where anything failed.
//!
//! Plan shape:
//!
//! ```json
//! {
//!   "name": "canonicalize_then_materialize",
//!   "steps": [
//!     { "kind": "sparql_update",
//!       "name": "clear_derived",
//!       "update":  "CLEAR SILENT GRAPH <urn:derived:person>" },
//!     { "kind": "sparql_query",
//!       "name":  "sanity_count",
//!       "query": "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }" },
//!     { "kind": "wasm",
//!       "name":  "materialize_person",
//!       "url":   "file:///.../wf_materialize.wasm",
//!       "arg":   "{\"name\": \"person\", ...}" }
//!   ]
//! }
//! ```
//!
//! Step kinds:
//!
//! * `sparql_query`    — execute-query, returns row count
//! * `sparql_update`   — execute-update, returns "ok"
//! * `wasm`            — invoke-wasm with a single-string arg (typically
//!                        a descriptor JSON); returns the guest's first
//!                        row's first cell as the detail
//!
//! v1 scope: sequential, no conditionals, no inter-step variables.
//! Steps interact through the graph store — one step writes triples,
//! the next step reads them. That's the "explicit is better than
//! implicit" version of composition; anything richer (branches,
//! variable propagation, error recovery) belongs in a caller that
//! runs multiple wf_pipeline invocations conditionally.
//!
//! On step failure: emit an error row for that step and STOP; later
//! steps don't run. The caller sees exactly which step failed.

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
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

#[derive(Deserialize)]
struct Plan {
    #[allow(dead_code)]
    name: String,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    update: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    arg: Option<String>,
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let plan_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_pipeline: first arg must be a plan-json string literal".into(),
                );
            }
        };
        let plan: Plan = serde_json::from_str(&plan_json)
            .map_err(|e| format!("wf_pipeline: plan parse: {e}"))?;

        let mut rows: Vec<Vec<Binding>> = Vec::with_capacity(plan.steps.len());
        for (idx, step) in plan.steps.iter().enumerate() {
            let step_name = step
                .name
                .clone()
                .unwrap_or_else(|| format!("step_{idx}"));
            let result = run_step(step);
            match result {
                Ok(detail) => {
                    rows.push(build_row(idx as i64, &step.kind, &step_name, true, &detail));
                }
                Err(e) => {
                    rows.push(build_row(idx as i64, &step.kind, &step_name, false, &e));
                    break;
                }
            }
        }

        Ok(BindingSets {
            vars: vec![
                "step".into(),
                "kind".into(),
                "name".into(),
                "ok".into(),
                "detail".into(),
            ],
            rows,
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_pipeline: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_pipeline: aggregate not applicable".into())
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
                    label: "wf_pipeline(\"<plan-json>\") — sequential execution \
                            of substrate steps (sparql_query, sparql_update, \
                            wasm). One row per step. Stops on first failure. \
                            Steps interact through the graph store."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Step execution
// ---------------------------------------------------------------------------

fn run_step(step: &Step) -> Result<String, String> {
    match step.kind.as_str() {
        "sparql_query" => {
            let q = step
                .query
                .as_deref()
                .ok_or_else(|| "sparql_query: missing `query`".to_string())?;
            let bs = host::execute_query(q, &[], None)
                .map_err(|e| format!("sparql_query: {e}"))?;
            Ok(format!("{} rows", bs.rows.len()))
        }
        "sparql_update" => {
            let u = step
                .update
                .as_deref()
                .ok_or_else(|| "sparql_update: missing `update`".to_string())?;
            host::execute_update(u).map_err(|e| format!("sparql_update: {e}"))?;
            Ok("ok".to_string())
        }
        "wasm" => {
            let url = step
                .url
                .as_deref()
                .ok_or_else(|| "wasm: missing `url`".to_string())?;
            let arg_val = step.arg.as_deref().map(|s| {
                Value::Literal(Literal {
                    label: s.to_string(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                })
            });
            let args: Vec<Value> = arg_val.into_iter().collect();
            let bs = host::invoke_wasm(url, &args).map_err(|e| format!("wasm: {e}"))?;
            // Summarize as the first row's first cell — same pattern as
            // wf:call's filter form. Callers wanting the full grid should
            // invoke the guest directly rather than through the pipeline.
            let detail = bs
                .rows
                .first()
                .and_then(|r| r.first())
                .map(|b| match &b.value {
                    Value::Literal(l) => l.label.clone(),
                    Value::Iri(s) => s.clone(),
                    Value::Bnode(s) => format!("_:{s}"),
                })
                .unwrap_or_else(|| "no output rows".to_string());
            Ok(detail)
        }
        other => Err(format!(
            "unknown step kind `{other}` (want: sparql_query | sparql_update | wasm)"
        )),
    }
}

// ---------------------------------------------------------------------------
// Row assembly
// ---------------------------------------------------------------------------

fn build_row(
    idx: i64,
    kind: &str,
    name: &str,
    ok: bool,
    detail: &str,
) -> Vec<Binding> {
    vec![
        Binding {
            name: "step".into(),
            value: int_lit(idx),
        },
        Binding {
            name: "kind".into(),
            value: string_lit(kind),
        },
        Binding {
            name: "name".into(),
            value: string_lit(name),
        },
        Binding {
            name: "ok".into(),
            value: bool_lit(ok),
        },
        Binding {
            name: "detail".into(),
            value: string_lit(detail),
        },
    ]
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

fn bool_lit(b: bool) -> Value {
    Value::Literal(Literal {
        label: if b { "true".into() } else { "false".into() },
        datatype: XSD_BOOLEAN.into(),
        lang: None,
    })
}

export!(Component);
