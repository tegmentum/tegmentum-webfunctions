//! webassembly_wat — disassemble a WebAssembly module to WAT text.
//!
//! Ports the semantalytics function_webassembly/wasm_2_wat crate.
//! Argument 0 is the WebAssembly binary as a base64-encoded string literal
//! (base64 keeps the bytes SPARQL-safe). Returns the pretty-printed WAT
//! representation as an xsd:string literal.

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
        _ => Err("webassembly_wat: argument must be a base64-encoded string literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("webassembly_wat: expected 1 arg, got {}", args.len()));
        }
        let encoded = string_of(&args[0])?;
        let wasm_bytes = STANDARD
            .decode(encoded.as_bytes())
            .map_err(|e| format!("webassembly_wat: invalid base64: {}", e))?;
        let wat = wasmprinter::print_bytes(&wasm_bytes)
            .map_err(|e| format!("webassembly_wat: failed to disassemble module: {}", e))?;

        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: string_literal(&wat),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("webassembly_wat: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("webassembly_wat: aggregate not applicable".into())
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
                    "webassembly_wat(base64_wasm) -> WAT text disassembly of the module. \
                     Input must be a base64-encoded WebAssembly binary (magic \"\\0asm\")."),
            }]],
        }
    }
}

export!(Component);
