//! wf_apply — call-by-reference higher-order combinator.
//!
//! Signature: `wf:call(<wf_apply.wasm>, <function-iri>, args...)`.
//!
//! The first argument names a resource in the local graph whose
//! `<http://tegmentum.ai/ns/composition/source>` triple carries the wasm
//! URL to invoke. Remaining arguments flow through unchanged as that
//! wasm's positional inputs. Semantically equivalent to
//! `wf:call(?url, args...)` after the dereference — the point is the
//! late-binding through RDF: a function's identity is an IRI you also
//! use elsewhere in the graph.
//!
//! Targets WIT world v0.4.0 for the `invoke-wasm` host import; earlier
//! worlds don't have the capability to call one wasm from inside
//! another.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const COMP_SOURCE_IRI: &str = "http://tegmentum.ai/ns/composition/source";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.is_empty() {
            return Err(
                "wf_apply: need at least a function IRI (first arg)".into(),
            );
        }
        let fn_iri = match &args[0] {
            Value::Iri(iri) => iri.clone(),
            other => {
                return Err(format!(
                    "wf_apply: first arg must be an IRI, got {other:?}"
                ));
            }
        };

        // Dereference the function IRI to a wasm URL via a SPARQL callback
        // into the outer store. The <fn-iri> comp:source ?url triple is
        // the vocabulary contract; the same predicate is what wf:compose
        // and the composition plan format use for the wasm URL of a
        // component, so both use cases share one graph shape.
        let sparql = format!(
            "SELECT ?url WHERE {{ <{fn_iri}> <{COMP_SOURCE_IRI}> ?url }} LIMIT 1"
        );
        let bs = host::execute_query(&sparql, &[], Some(1))?;
        let row = bs
            .rows
            .first()
            .ok_or_else(|| format!("wf_apply: no <{COMP_SOURCE_IRI}> triple for {fn_iri}"))?;
        let url = row
            .first()
            .ok_or_else(|| "wf_apply: dereference query returned an empty row".to_string())
            .and_then(|b| match &b.value {
                Value::Iri(s) => Ok(s.clone()),
                Value::Literal(l) => Ok(l.label.clone()),
                other => Err(format!(
                    "wf_apply: comp:source of {fn_iri} not an IRI or string: {other:?}"
                )),
            })?;

        // Delegate to the invoke-wasm host import. The host's own wf:call
        // pipeline handles fetch + cache + instantiate + execute; we just
        // pass through the caller's remaining args unchanged.
        host::invoke_wasm(&url, &args[1..])
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_apply: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_apply: aggregate not applicable".into())
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
                value: string_literal(
                    "wf_apply(<fn-iri>, args...) -> resolve <fn-iri> comp:source ?url \
                     via SPARQL and invoke the wasm at ?url with args.",
                ),
            }]],
        }
    }
}

export!(Component);
