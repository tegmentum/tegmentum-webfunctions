//! nest_hierarchy — multi-level nesting of flat rows.
//!
//! Aggregate-shaped. Each row supplies variadic (name, value) pairs;
//! the first row's first argument is a JSON-array string listing the
//! field names in nesting order.

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
const AGGREGATE_NAME: &str = "nest_hierarchy";

struct Component;

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }
    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "nest_hierarchy: unknown filter function '{name}' (use via SPARQL aggregate)"
        ))
    }
}

impl AggregateGuest for Component {
    type AggregateState = HierarchyAccumulator;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        vec![AggregateDescriptor {
            name: AGGREGATE_NAME.to_string(),
            min_arity: 3,
            max_arity: None,
        }]
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        match name.as_str() {
            AGGREGATE_NAME => Ok(AggregateState::new(HierarchyAccumulator::new())),
            other => Err(format!("nest_hierarchy: unknown aggregate '{other}'")),
        }
    }
}

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
            "nest_hierarchy: unknown property function '{name}' (this component provides none)"
        ))
    }
}

struct HierarchyState {
    hierarchy: Option<Vec<String>>,
    rows: Vec<Vec<(String, JsonValue)>>,
}

pub struct HierarchyAccumulator {
    state: RefCell<HierarchyState>,
}

impl HierarchyAccumulator {
    fn new() -> Self {
        Self {
            state: RefCell::new(HierarchyState {
                hierarchy: None,
                rows: Vec::new(),
            }),
        }
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

fn json_key(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn args_to_row(args: &[WitTerm]) -> Result<Vec<(String, JsonValue)>, String> {
    if args.len() % 2 != 0 {
        return Err(format!(
            "nest_hierarchy: expected an even number of field/value args, got {}",
            args.len()
        ));
    }
    let mut row = Vec::with_capacity(args.len() / 2);
    for pair in args.chunks(2) {
        let name = match &pair[0] {
            WitTerm::Literal(l) => l.value.clone(),
            _ => return Err("nest_hierarchy: field-name args must be string literals".into()),
        };
        row.push((name, value_to_json(&pair[1])));
    }
    Ok(row)
}

fn nest(rows: &[Vec<(String, JsonValue)>], keys: &[String]) -> JsonValue {
    if keys.is_empty() {
        let arr: Vec<JsonValue> = rows
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (k, v) in row {
                    obj.insert(k.clone(), v.clone());
                }
                JsonValue::Object(obj)
            })
            .collect();
        return JsonValue::Array(arr);
    }
    let key = &keys[0];
    let rest = &keys[1..];
    let mut buckets: std::collections::BTreeMap<
        String,
        (Option<JsonValue>, Vec<Vec<(String, JsonValue)>>),
    > = std::collections::BTreeMap::new();
    for row in rows {
        let key_val = row.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let bucket_key = match &key_val {
            Some(v) => json_key(v),
            None => "<null>".into(),
        };
        let stripped: Vec<(String, JsonValue)> =
            row.iter().filter(|(k, _)| k != key).cloned().collect();
        let entry = buckets.entry(bucket_key).or_insert((key_val, Vec::new()));
        entry.1.push(stripped);
    }
    let mut obj = serde_json::Map::new();
    for (label, (_original, sub_rows)) in buckets {
        obj.insert(label, nest(&sub_rows, rest));
    }
    JsonValue::Object(obj)
}

impl GuestAggregateState for HierarchyAccumulator {
    fn step(&self, args: Vec<WitTerm>) -> Result<(), String> {
        if args.is_empty() {
            return Err("nest_hierarchy: first arg must be a JSON array of key names \
                        (['key1','key2',...])"
                .into());
        }
        let (first, rest) = args.split_first().unwrap();
        let spec_str = match first {
            WitTerm::Literal(l) => l.value.as_str(),
            _ => return Err("nest_hierarchy: first arg must be a string literal".into()),
        };
        let spec: Vec<String> = serde_json::from_str(spec_str)
            .map_err(|e| format!("nest_hierarchy: bad hierarchy spec {spec_str}: {e}"))?;

        let mut st = self.state.borrow_mut();
        if let Some(existing) = &st.hierarchy {
            if existing != &spec {
                return Err("nest_hierarchy: hierarchy spec changed mid-aggregate".to_string());
            }
        } else {
            st.hierarchy = Some(spec);
        }
        let row = args_to_row(rest)?;
        st.rows.push(row);
        Ok(())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        let (hierarchy, rows) = {
            let mut st = self.state.borrow_mut();
            (
                st.hierarchy.take().unwrap_or_default(),
                std::mem::take(&mut st.rows),
            )
        };
        let tree = nest(&rows, &hierarchy);
        Ok(WitTerm::Literal(WitLiteral {
            value: tree.to_string(),
            datatype: Some(XSD_STRING.to_string()),
            language: None,
        }))
    }
}

bindings::export!(Component with_types_in bindings);
