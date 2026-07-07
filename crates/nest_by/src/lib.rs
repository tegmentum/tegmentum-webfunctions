//! nest_by — single-level grouping aggregate.
//!
//! Aggregate over rows, each row supplying variadic (name, value) pairs.
//! At `aggregate-finish` we emit a single-row single-var binding-set whose
//! `tree` variable holds a JSON string of shape
//!
//!   [{"<field1>": <v>, "<field2>": <v>, ...}, ...]
//!
//! one JSON object per row. This is essentially `GROUP_CONCAT` but with
//! structured output — every row's fields are preserved as an object,
//! rather than concatenated into a single delimited string.
//!
//! Composed with SPARQL `GROUP BY`, this yields tree-shaped output
//! from an otherwise-flat query:
//!
//! ```sparql
//! SELECT ?parent (nest_by(?child, ?age) AS ?children)
//! WHERE { ?parent bio:hasChild ?child . ?child bio:age ?age }
//! GROUP BY ?parent
//! ```
//!
//! …yields one row per `?parent` with `?children` bound to a JSON list of
//! `{"child": ..., "age": ...}` objects.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::{json, Value as JsonValue};
use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::cell::RefCell;

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Iri(uri) => json!(uri),
        Value::Bnode(id) => json!(format!("_:{}", id)),
        Value::Literal(literal) => {
            // Try to preserve numeric/boolean types when they're obvious
            // from the datatype IRI. Everything else is a JSON string.
            let dt = literal.datatype.as_str();
            if dt.ends_with("#integer") || dt.ends_with("#long") || dt.ends_with("#int") {
                if let Ok(n) = literal.label.parse::<i64>() { return json!(n); }
            }
            if dt.ends_with("#decimal") || dt.ends_with("#double") || dt.ends_with("#float") {
                if let Ok(n) = literal.label.parse::<f64>() {
                    if n.is_finite() { return json!(n); }
                }
            }
            if dt.ends_with("#boolean") {
                if literal.label == "true" { return json!(true); }
                if literal.label == "false" { return json!(false); }
            }
            json!(literal.label)
        }
    }
}

thread_local! {
    /// Accumulated rows. Each row is a Vec of (field-name, JSON-value) pairs.
    static ROWS: RefCell<Vec<Vec<(String, JsonValue)>>> = const { RefCell::new(Vec::new()) };
}

/// Parse variadic `(name, value, name, value, ...)` args into a row.
///
/// `name` must be a string literal (labels are the only sensible thing to
/// key by); `value` can be any WIT `Value` and is converted per the rules
/// in [`value_to_json`].
fn args_to_row(args: &[Value]) -> Result<Vec<(String, JsonValue)>, String> {
    if args.len() % 2 != 0 {
        return Err(format!(
            "nest_by: expected an even number of args (name, value, name, value, …), got {}",
            args.len()
        ));
    }
    let mut row = Vec::with_capacity(args.len() / 2);
    for pair in args.chunks(2) {
        let name = match &pair[0] {
            Value::Literal(l) => l.label.clone(),
            _ => return Err("nest_by: field-name args must be string literals".into()),
        };
        let jv = value_to_json(&pair[1]);
        row.push((name, jv));
    }
    Ok(row)
}

impl Guest for Component {
    /// Not applicable in the single-value BIND form — nest_by only makes sense
    /// as an aggregate over multiple rows. Fail loudly rather than emit a
    /// misleading result.
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        Err("nest_by: use as an aggregate (`nest_by(?a, ?b) AS ?tree`) — \
             not as a filter function".into())
    }

    fn aggregate_step(args: Vec<Value>, _mult: u64) -> Result<(), String> {
        let row = args_to_row(&args)?;
        ROWS.with(|r| r.borrow_mut().push(row));
        Ok(())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        let rows = ROWS.with(|r| std::mem::take(&mut *r.borrow_mut()));
        let arr: Vec<JsonValue> = rows.into_iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (k, v) in row { obj.insert(k, v); }
                JsonValue::Object(obj)
            })
            .collect();
        let json = JsonValue::Array(arr).to_string();
        Ok(BindingSets {
            vars: vec!["tree".into()],
            rows: vec![vec![Binding {
                name: "tree".into(),
                value: string_literal(&json),
            }]],
        })
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
                    "nest_by((name, value)+) -> JSON list of objects (one per row). \
                     Use as an aggregate, typically with GROUP BY. Types are inferred \
                     from the value's xsd:datatype: integer, decimal, boolean become \
                     native JSON; everything else becomes a JSON string."),
            }]],
        }
    }
}

export!(Component);
