//! wf_map — map-over-rows higher-order combinator.
//!
//! Signature: `wf:call(<wf_map.wasm>, <wasm-url>, "SELECT ?x WHERE {...}")`.
//!
//! Runs the second-argument SELECT against the local store via the
//! `execute-query` host callback, invokes the first-argument wasm on
//! each row's first cell via `invoke-wasm`, and packs the resulting
//! values into an rdf:JSON array — one output element per input row,
//! in the query's solution order. Semantics match Rust's
//! `Iterator::map`. The rdf:JSON datatype flags the return as
//! structured content so downstream consumers can dispatch on datatype
//! instead of guessing at an untyped string.
//!
//! Targets WIT world v0.4.0 for the `invoke-wasm` host import.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::{Value as JsonValue, json};
use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn xsd_string_literal(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

fn json_literal(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: RDF_JSON.into(),
        lang: None,
    })
}

fn value_to_json_scalar(v: &Value) -> JsonValue {
    match v {
        Value::Iri(uri) => json!(uri),
        Value::Bnode(id) => json!(format!("_:{id}")),
        Value::Literal(l) => {
            // Best-effort typed-numeric promotion — matches what the
            // host-native wf:map did before the port, so downstream
            // consumers see the same shape.
            let dt = l.datatype.as_str();
            if dt.ends_with("integer") || dt.ends_with("long") || dt.ends_with("int") {
                if let Ok(n) = l.label.parse::<i64>() {
                    return json!(n);
                }
            }
            if dt.ends_with("decimal") || dt.ends_with("double") || dt.ends_with("float") {
                if let Ok(n) = l.label.parse::<f64>() {
                    if n.is_finite() {
                        return json!(n);
                    }
                }
            }
            if dt.ends_with("boolean") {
                match l.label.as_str() {
                    "true" | "1" => return json!(true),
                    "false" | "0" => return json!(false),
                    _ => {}
                }
            }
            json!(l.label)
        }
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!(
                "wf_map: expected 2 args (wasm URL, inner SPARQL), got {}",
                args.len()
            ));
        }
        let url = match &args[0] {
            Value::Iri(s) => s.clone(),
            Value::Literal(l) => l.label.clone(),
            other => {
                return Err(format!(
                    "wf_map: first arg must be an IRI or string, got {other:?}"
                ));
            }
        };
        let inner_sparql = match &args[1] {
            Value::Literal(l) => l.label.clone(),
            Value::Iri(s) => s.clone(),
            other => {
                return Err(format!(
                    "wf_map: second arg must be a SPARQL string, got {other:?}"
                ));
            }
        };

        // Fetch the rows we're going to map over. `execute-query` is the
        // v0.3.x import; still present and unchanged in v0.4.
        let bs = host::execute_query(&inner_sparql, &[], None)?;

        let mut mapped: Vec<JsonValue> = Vec::with_capacity(bs.rows.len());
        for row in &bs.rows {
            // First column of the row is the input to the mapper. Rows
            // that project no bound values become nulls in the output
            // — a valid stand-in for a missing SPARQL binding.
            let Some(first) = row.first() else {
                mapped.push(JsonValue::Null);
                continue;
            };
            let inner_args = vec![first.value.clone()];
            let inner_bs = host::invoke_wasm(&url, &inner_args)?;
            let output_value = inner_bs
                .rows
                .first()
                .and_then(|r| r.first())
                .map(|b| value_to_json_scalar(&b.value))
                .unwrap_or(JsonValue::Null);
            mapped.push(output_value);
        }

        let payload = serde_json::to_string(&JsonValue::Array(mapped))
            .map_err(|e| format!("wf_map: serializing output: {e}"))?;

        Ok(BindingSets {
            vars: vec!["mapped".into()],
            rows: vec![vec![Binding {
                name: "mapped".into(),
                value: json_literal(&payload),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_map: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_map: aggregate not applicable".into())
    }
    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
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
                value: xsd_string_literal(
                    "wf_map(<wasm-url>, \"SELECT ...\") -> rdf:JSON array of the wasm's \
                     output on each row's first cell, in the query's solution order.",
                ),
            }]],
        }
    }
}

export!(Component);
