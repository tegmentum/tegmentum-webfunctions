//! hash_blake2b_256 — hex-encoded BLAKE2b with 256-bit (32-byte) output of a UTF-8 string literal.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use blake2::{Blake2b, Digest};
use digest::consts::U32;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

type Blake2b256 = Blake2b<U32>;

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("hash_blake2b_256: argument must be a string literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("hash_blake2b_256: expected 1 arg, got {}", args.len()));
        }
        let mut hasher = Blake2b256::new();
        hasher.update(string_of(&args[0])?.as_bytes());
        let digest = hasher.finalize();
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: literal(&hex::encode(digest)),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("hash_blake2b_256: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("hash_blake2b_256: aggregate not applicable".into())
    }
    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: literal("hash_blake2b_256(s) -> lowercase hex-encoded BLAKE2b-256 of s."),
            }]],
        }
    }
}

export!(Component);
