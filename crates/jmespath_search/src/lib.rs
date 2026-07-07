//! jmespath_search — evaluate a JMESPath expression against a JSON string.
//!
//! Ports the semantalytics function_json_jmespath/search crate.
//! Argument 0 is the JMESPath expression, argument 1 is the JSON document,
//! both as string literals. Returns the search result as a JSON-encoded
//! xsd:string literal (matching the source crate's shape); callers can
//! then pipe through `parse_json` if they want to unfold rows.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn string_of(arg: &Value, which: &str) -> Result<String, String> {
    match arg {
        Value::Literal(l) => Ok(l.label.clone()),
        _ => Err(format!("jmespath_search: {} must be a string literal", which)),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!("jmespath_search: expected 2 args, got {}", args.len()));
        }
        let expression = string_of(&args[0], "argument 0 (expression)")?;
        let document = string_of(&args[1], "argument 1 (JSON document)")?;

        let expr = jmespath::compile(&expression)
            .map_err(|e| format!("jmespath_search: bad expression: {}", e))?;
        let data = jmespath::Variable::from_json(&document)
            .map_err(|e| format!("jmespath_search: invalid JSON: {}", e))?;
        let result = expr
            .search(data)
            .map_err(|e| format!("jmespath_search: search failed: {}", e))?;

        // Variable's Display impl emits JSON — matches the source crate's
        // `result.to_string()` behaviour.
        let output = result.to_string();

        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: string_literal(&output),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("jmespath_search: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("jmespath_search: aggregate not applicable".into())
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
                    "jmespath_search(expression, json_document) -> JSON-encoded \
                     result of applying the JMESPath expression to the document. \
                     Pipe through parse_json to unfold structured results into rows."),
            }]],
        }
    }
}

export!(Component);
