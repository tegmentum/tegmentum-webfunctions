//! nest_by — single-level grouping aggregate.
//!
//! Aggregate over rows, each row supplying variadic (name, value) pairs.
//! At `finish` emits a JSON array of objects — one JSON object per row.
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

#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

use serde_json::{json, Value as JsonValue};

use bindings::exports::tegmentum::webfunction::aggregate::{
    AggregateDescriptor, AggregateState, Guest as AggregateGuest, GuestAggregateState,
};
use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::exports::tegmentum::webfunction::property_function::{
    BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const AGGREGATE_NAME: &str = "nest_by";

struct Component;

/// Filter interface stub.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "nest_by: unknown filter function '{name}' (use via SPARQL aggregate)"
        ))
    }
}

/// Aggregate interface.
impl AggregateGuest for Component {
    type AggregateState = NestAccumulator;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        vec![AggregateDescriptor {
            name: AGGREGATE_NAME.to_string(),
            min_arity: 2,
            max_arity: None,
        }]
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        match name.as_str() {
            AGGREGATE_NAME => Ok(AggregateState::new(NestAccumulator::new())),
            other => Err(format!("nest_by: unknown aggregate '{other}'")),
        }
    }
}

/// Property-function interface stub.
impl PropertyFunctionGuest for Component {
    fn register_property_functions() -> Vec<PropertyDescriptor> {
        Vec::new()
    }

    fn evaluate(
        name: String,
        _subjects: Vec<WitTerm>,
        _objects: Vec<WitTerm>,
    ) -> Result<Vec<BindingRow>, String> {
        Err(format!(
            "nest_by: unknown property function '{name}' (this component provides none)"
        ))
    }
}

pub struct NestAccumulator {
    rows: RefCell<Vec<Vec<(String, JsonValue)>>>,
}

impl NestAccumulator {
    fn new() -> Self {
        Self { rows: RefCell::new(Vec::new()) }
    }
}

fn value_to_json(v: &WitTerm) -> JsonValue {
    match v {
        WitTerm::NamedNode(uri) => json!(uri),
        WitTerm::BlankNode(id) => json!(format!("_:{id}")),
        WitTerm::Literal(literal) => {
            let dt = literal
                .datatype
                .as_deref()
                .unwrap_or("http://www.w3.org/2001/XMLSchema#string");
            if dt.ends_with("#integer") || dt.ends_with("#long") || dt.ends_with("#int") {
                if let Ok(n) = literal.value.parse::<i64>() {
                    return json!(n);
                }
            }
            if dt.ends_with("#decimal") || dt.ends_with("#double") || dt.ends_with("#float") {
                if let Ok(n) = literal.value.parse::<f64>() {
                    if n.is_finite() {
                        return json!(n);
                    }
                }
            }
            if dt.ends_with("#boolean") {
                if literal.value == "true" {
                    return json!(true);
                }
                if literal.value == "false" {
                    return json!(false);
                }
            }
            json!(literal.value)
        }
        WitTerm::Triple(_) => json!("<<quoted triple>>"),
    }
}

fn args_to_row(args: &[WitTerm]) -> Result<Vec<(String, JsonValue)>, String> {
    if args.len() % 2 != 0 {
        return Err(format!(
            "nest_by: expected an even number of args (name, value, name, value, …), got {}",
            args.len()
        ));
    }
    let mut row = Vec::with_capacity(args.len() / 2);
    for pair in args.chunks(2) {
        let name = match &pair[0] {
            WitTerm::Literal(l) => l.value.clone(),
            _ => return Err("nest_by: field-name args must be string literals".into()),
        };
        let jv = value_to_json(&pair[1]);
        row.push((name, jv));
    }
    Ok(row)
}

impl GuestAggregateState for NestAccumulator {
    fn step(&self, args: Vec<WitTerm>) -> Result<(), String> {
        let row = args_to_row(&args)?;
        self.rows.borrow_mut().push(row);
        Ok(())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        let rows = std::mem::take(&mut *self.rows.borrow_mut());
        let arr: Vec<JsonValue> = rows
            .into_iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (k, v) in row {
                    obj.insert(k, v);
                }
                JsonValue::Object(obj)
            })
            .collect();
        let json = JsonValue::Array(arr).to_string();
        Ok(WitTerm::Literal(WitLiteral {
            value: json,
            datatype: Some(XSD_STRING.to_string()),
            language: None,
        }))
    }
}

bindings::export!(Component with_types_in bindings);
