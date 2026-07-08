//! debug_execute_update — minimal scratch component for the v0.3.1
//! `execute-update` host import.
//!
//! Takes three IRI/literal arguments (s, p, o), builds an
//! `INSERT DATA { ?s ?p ?o }` statement, invokes `host::execute_update`,
//! then does a follow-up `SELECT ?o WHERE { ?s ?p ?o }` to confirm the
//! insert actually landed in the transaction the callback saw. Returns a
//! single row with a "confirmed" xsd:boolean binding.
//!
//! The point is a self-contained proof that (a) the wire-up carries the
//! host import through to the guest, (b) the update sees the outer
//! transaction, and (c) the next execute-query in the same wasm frame
//! sees the fresh triple.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

fn iri_of(v: &Value) -> Result<String, String> {
    match v {
        Value::Iri(s) => Ok(s.clone()),
        _ => Err("expected IRI argument".into()),
    }
}

fn as_object_literal(v: &Value) -> Result<String, String> {
    match v {
        Value::Literal(l) => Ok(format!("\"{}\"", l.label.replace('"', "\\\""))),
        Value::Iri(s) => Ok(format!("<{}>", s)),
        _ => Err("expected literal or IRI object".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 3 {
            return Err("debug_execute_update: expected (s, p, o)".into());
        }
        let s = iri_of(&args[0])?;
        let p = iri_of(&args[1])?;
        let o = as_object_literal(&args[2])?;

        // Push the triple.
        let insert = format!("INSERT DATA {{ <{}> <{}> {} }}", s, p, o);
        host::execute_update(&insert, &[])?;

        // Read it back in the same wasm frame.
        let select = format!(
            "SELECT ?o WHERE {{ <{}> <{}> ?o }}",
            s, p
        );
        let bs = host::execute_query(&select, &[], None)?;
        let confirmed = !bs.rows.is_empty();

        Ok(BindingSets {
            vars: vec!["confirmed".into()],
            rows: vec![vec![Binding {
                name: "confirmed".into(),
                value: Value::Literal(Literal {
                    label: confirmed.to_string(),
                    datatype: XSD_BOOLEAN.into(),
                    lang: None,
                }),
            }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("debug_execute_update: aggregate N/A".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("debug_execute_update: aggregate N/A".into())
    }
    fn cardinality_estimate(_i: Cardinality, _a: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: Value::Literal(Literal {
                    label: "debug_execute_update(s,p,o) inserts <s> <p> o and confirms via a follow-up SELECT".into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

export!(Component);
