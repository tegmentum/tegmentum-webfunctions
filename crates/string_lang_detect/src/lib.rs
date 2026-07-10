//! string_lang_detect — detect the language of a string.
//!
//! `wf:call(<string_lang_detect.wasm>, text)` returns the detected
//! language's ISO 639-3 code as an xsd:string, or an error if
//! whatlang cannot make a determination (input too short or too mixed).
//!
//! Ports semantalytics function_string_lang/detect. Replaces the
//! lingua-rs dependency (which required a wasm-patched fork at the
//! time) with whatlang, which compiles clean to wasm32-wasip1 and
//! carries its own trigram models so no external data files are
//! needed. Result crate is ~1.5 MB. For higher-accuracy detection on
//! short text, see the parallel string_lang_lingua_* set.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("string_lang_detect: argument must be a string literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!(
                "string_lang_detect: expected 1 arg (text), got {}",
                args.len()
            ));
        }
        let text = string_of(&args[0])?;
        let info = whatlang::detect(text)
            .ok_or_else(|| "string_lang_detect: unable to detect language".to_string())?;
        Ok(BindingSets {
            vars: vec!["lang".into()],
            rows: vec![vec![Binding {
                name: "lang".into(),
                value: string_literal(info.lang().code()),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("string_lang_detect: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("string_lang_detect: aggregate not applicable".into())
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
                    "string_lang_detect(text) -> ISO 639-3 code of the detected language.",
                ),
            }]],
        }
    }
}

export!(Component);
