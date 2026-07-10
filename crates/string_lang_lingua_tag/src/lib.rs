//! string_lang_lingua_tag — detected language as an ISO 639-1 tag.
//!
//! Ports semantalytics function_string_lang/tag.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};
use lingua::{Language, LanguageDetectorBuilder};

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

fn all_langs() -> Vec<Language> { Language::all().into_iter().collect() }

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 { return Err(format!("string_lang_lingua_tag: expected 1 arg, got {}", args.len())); }
        let text = string_of(&args[0])?;
        let det = LanguageDetectorBuilder::from_languages(&all_langs()).build();
        let lang = det.detect_language_of(text).ok_or_else(|| "string_lang_lingua_tag: no language detected".to_string())?;
        Ok(BindingSets { vars: vec!["tag".into()], rows: vec![vec![
            Binding { name: "tag".into(), value: string_literal(lang.iso_code_639_1().to_string().to_lowercase().as_str()) }]] })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> { Err("aggregate not applicable".into()) }
    fn aggregate_finish() -> Result<BindingSets, String> { Err("aggregate not applicable".into()) }
    fn cardinality_estimate(_i: Cardinality, _a: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets { BindingSets { vars: vec!["doc".into()], rows: vec![vec![Binding {
        name: "doc".into(), value: string_literal("string_lang_lingua_tag(text) -> ISO 639-1 tag.")}]] } }
}
export!(Component);
