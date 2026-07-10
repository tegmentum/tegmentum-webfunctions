//! string_lang_all_languages_with_devanagari_script — enumerate whatlang languages canonically written in the Devanagari script.
//!
//! whatlang does not expose a Lang -> Script mapping (a Lang can be written
//! in multiple scripts; detection classifies scripts from text, not from
//! Lang variants), so this crate carries a hardcoded per-script language
//! table matching the corresponding semantalytics original's semantics.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

const SCRIPT_LANGS: &[&str] = &["hin", "mar", "nep"];

impl Guest for Component {
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        let rows = SCRIPT_LANGS.iter().map(|c| vec![Binding {
            name: "lang".into(), value: string_literal(c)
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
            value: string_literal("string_lang_all_languages_with_devanagari_script() -> ISO 639-3 codes of whatlang languages canonically in the Devanagari script.")
        }]] }
    }
}
export!(Component);
