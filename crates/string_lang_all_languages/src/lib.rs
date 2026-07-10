//! string_lang_all_languages — enumerate the languages whatlang can detect.
//!
//! Ports semantalytics function_string_lang/all_languages. Rows: one ISO 639-3
//! code per language.

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

use whatlang::Lang;

impl Guest for Component {
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        let langs: Vec<Lang> = Lang::all().iter().copied().filter(|l| true).collect();
        let rows = langs.into_iter().map(|l| vec![
            Binding { name: "lang".into(), value: string_literal(l.code()) }]).collect();
        Ok(BindingSets { vars: vec!["lang".into()], rows })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> { Err("aggregate not applicable".into()) }
    fn aggregate_finish() -> Result<BindingSets, String> { Err("aggregate not applicable".into()) }
    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets { vars: vec!["doc".into()], rows: vec![vec![Binding { name: "doc".into(),
            value: string_literal("string_lang_all_languages() -> rows of ISO 639-3 language codes.") }]] }
    }
}
export!(Component);
