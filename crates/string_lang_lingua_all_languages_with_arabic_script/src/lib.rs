//! string_lang_lingua_all_languages_with_arabic_script — enumerate compiled-in lingua Arabic-script languages.
//!
//! Delegates to Language::all_with_arabic_script() and emits one ISO 639-1 code per row.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};
use lingua::Language;

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

impl Guest for Component {
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        let mut langs: Vec<Language> = Language::all_with_arabic_script().into_iter().collect();
        langs.sort_by_key(|l| l.iso_code_639_1() as u32);
        let rows = langs.into_iter().map(|l| vec![Binding {
            name: "lang".into(),
            value: string_literal(l.iso_code_639_1().to_string().to_lowercase().as_str())
        }]).collect();
        Ok(BindingSets { vars: vec!["lang".into()], rows })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> { Err("aggregate not applicable".into()) }
    fn aggregate_finish() -> Result<BindingSets, String> { Err("aggregate not applicable".into()) }
    fn cardinality_estimate(_i: Cardinality, _a: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets { vars: vec!["doc".into()], rows: vec![vec![Binding {
            name: "doc".into(),
            value: string_literal("string_lang_lingua_all_languages_with_arabic_script() -> ISO 639-1 codes of the compiled-in Arabic-script languages.")
        }]] }
    }
}
export!(Component);
