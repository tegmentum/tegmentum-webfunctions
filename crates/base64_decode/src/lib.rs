//! base64_decode — decode a base64-encoded UTF-8 string literal.
//!
//! Ports the semantalytics function_base64/decode crate. Uses the base64
//! crate's standard alphabet (RFC 4648 §4) with padding. The decoded bytes
//! must be valid UTF-8; if you need to decode arbitrary binary you should
//! not be routing it through a SPARQL string literal.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use base64::engine::{general_purpose::STANDARD, Engine as _};
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("base64_decode: argument must be a string literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("base64_decode: expected 1 arg, got {}", args.len()));
        }
        let encoded = string_of(&args[0])?;
        let bytes = STANDARD
            .decode(encoded.as_bytes())
            .map_err(|e| format!("base64_decode: invalid base64: {}", e))?;
        let decoded = String::from_utf8(bytes)
            .map_err(|e| format!("base64_decode: decoded bytes are not valid UTF-8: {}", e))?;
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: string_literal(&decoded),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("base64_decode: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("base64_decode: aggregate not applicable".into())
    }
    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "base64_decode(s) -> decoded UTF-8 string. Uses the RFC 4648 \
                     standard alphabet with padding. Errors if the input is not \
                     valid base64 or if the decoded bytes are not valid UTF-8."),
            }]],
        }
    }
}

export!(Component);
