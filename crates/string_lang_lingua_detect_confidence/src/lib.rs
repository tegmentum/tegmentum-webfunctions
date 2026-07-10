//! string_lang_lingua_detect_confidence — one row per candidate language,
//! ordered by descending relative confidence.
//!
//! Ports semantalytics function_string_lang/detect_confidence.

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
        if args.len() != 1 { return Err(format!("string_lang_lingua_detect_confidence: expected 1 arg, got {}", args.len())); }
        let text = string_of(&args[0])?;
        let det = LanguageDetectorBuilder::from_languages(&all_langs()).build();
        let values = det.compute_language_confidence_values(text);
        let rows = values.into_iter().map(|(l, c)| vec![
            Binding { name: "lang".into(),       value: string_literal(l.iso_code_639_1().to_string().to_lowercase().as_str()) },
            Binding { name: "confidence".into(), value: decimal_literal(c) },
        ]).collect();
        Ok(BindingSets { vars: vec!["lang".into(), "confidence".into()], rows })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> { Err("aggregate not applicable".into()) }
    fn aggregate_finish() -> Result<BindingSets, String> { Err("aggregate not applicable".into()) }
    fn cardinality_estimate(_i: Cardinality, _a: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets { BindingSets { vars: vec!["doc".into()], rows: vec![vec![Binding {
        name: "doc".into(), value: string_literal("string_lang_lingua_detect_confidence(text) -> (lang, confidence) rows.")}]] } }
}
export!(Component);
