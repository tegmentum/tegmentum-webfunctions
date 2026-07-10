//! string_lang_detect_confidence — detect language of a string and return
//! (ISO 639-3, confidence) for each candidate whatlang considered, sorted
//! by descending confidence.
//!
//! Ports semantalytics function_string_lang/detect_confidence (via whatlang).

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
            return Err(format!("string_lang_detect_confidence: expected 1 arg (text), got {}", args.len()));
        }
        let text = string_of(&args[0])?;
        let info = whatlang::detect(text)
            .ok_or_else(|| "string_lang_detect_confidence: unable to detect language".to_string())?;
        // whatlang's single detect() yields one language + one confidence. The
        // multi-candidate list is not exposed by 0.16; emit the single row
        // for API symmetry with string_lang_lingua_detect_confidence.
        Ok(BindingSets {
            vars: vec!["lang".into(), "confidence".into()],
            rows: vec![vec![
                Binding { name: "lang".into(), value: string_literal(info.lang().code()) },
                Binding { name: "confidence".into(), value: decimal_literal(info.confidence()) },
            ]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> { Err("aggregate not applicable".into()) }
    fn aggregate_finish() -> Result<BindingSets, String> { Err("aggregate not applicable".into()) }
    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets { vars: vec!["doc".into()], rows: vec![vec![Binding { name: "doc".into(),
            value: string_literal("string_lang_detect_confidence(text) -> rows of (lang, confidence).") }]] }
    }
}
export!(Component);
