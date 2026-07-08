//! debug_callback_depth — minimal scratch component to isolate whether the
//! v0.3.0 host-callback ABI works at all.
//!
//! Just calls `host::callback_depth()` (no args, u32 return — simplest
//! possible signature) and returns the result as an xsd:integer.
//!
//! If instantiation + invocation succeed, the linker binding wire-up is fine
//! and the deeper problem is specifically execute-query's compound argument
//! shapes. If they fail, DefaultLinkingContext.addWitHostFunction likely
//! needs the explicit-type-signature constructor form rather than the two-arg
//! version we're using now.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";

impl Guest for Component {
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        let depth = host::callback_depth();
        Ok(BindingSets {
            vars: vec!["depth".into()],
            rows: vec![vec![Binding {
                name: "depth".into(),
                value: Value::Literal(Literal {
                    label: depth.to_string(),
                    datatype: XSD_INTEGER.into(),
                    lang: None,
                }),
            }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("debug_callback_depth: aggregate N/A".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("debug_callback_depth: aggregate N/A".into())
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
                    label: "debug_callback_depth() -> current callback-depth as xsd:integer".into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

export!(Component);
