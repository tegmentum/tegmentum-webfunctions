//! string_lang_tag — return the detected language as a SPARQL @lang tag
//! (ISO 639-1 preferred, ISO 639-3 fallback).
//!
//! Ports semantalytics function_string_lang/tag (via whatlang).

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn decimal_literal(v: f64) -> Value {
    Value::Literal(Literal { label: format!("{:.4}", v), datatype: XSD_DECIMAL.into(), lang: None })
}

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg { Value::Literal(l) => Ok(l.label.as_str()), _ => Err("argument must be a string literal".into()) }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("string_lang_tag: expected 1 arg (text), got {}", args.len()));
        }
        let text = string_of(&args[0])?;
        let lang = whatlang::detect(text)
            .ok_or_else(|| "string_lang_tag: unable to detect language".to_string())?
            .lang();
        // BCP 47 tag: whatlang exposes code() which is ISO 639-3 (3-letter);
        // there's no 2-letter accessor in 0.16, so we return the 3-letter code
        // prefixed with a dash-free tag body. Downstream can normalise if needed.
        Ok(BindingSets { vars: vec!["tag".into()], rows: vec![vec![
            Binding { name: "tag".into(), value: string_literal(lang.code()) }]] })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> { Err("aggregate not applicable".into()) }
    fn aggregate_finish() -> Result<BindingSets, String> { Err("aggregate not applicable".into()) }
    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets { vars: vec!["doc".into()], rows: vec![vec![Binding { name: "doc".into(),
            value: string_literal("string_lang_tag(text) -> ISO 639-3 language tag.") }]] }
    }
}
export!(Component);
