//! array_append — append a value to an array literal.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/append.

#[allow(warnings)]
mod bindings;

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

/// Legacy names — kept as type aliases so the ported business logic
/// below reads with minimum diff against the flat-world original. The
/// `Term::Triple` arm added by the R2 types consolidation is handled
/// in each `match` inside this file.
type Value = WitTerm;
type Literal = WitLiteral;

use serde_json::{Map as JsonMap, Value as JsonValue};
struct Component;

const XSD_STRING:      &str = "http://www.w3.org/2001/XMLSchema#string";
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

fn value_to_json(v: &Value) -> JsonValue {
    match v {
        WitTerm::NamedNode(s) => {
            let mut m = JsonMap::new();
            m.insert("iri".into(), JsonValue::String(s.clone()));
            JsonValue::Object(m)
        }
        WitTerm::Literal(l) => {
            let mut inner = JsonMap::new();
            inner.insert("label".into(), JsonValue::String(l.value.clone()));
            inner.insert("datatype".into(), JsonValue::String(l.datatype.clone().unwrap_or_default()));
            inner.insert("lang".into(), match &l.language {
                Some(s) => JsonValue::String(s.clone()),
                None => JsonValue::Null,
            });
            let mut m = JsonMap::new();
            m.insert("literal".into(), JsonValue::Object(inner));
            JsonValue::Object(m)
        }
        WitTerm::BlankNode(s) => {
            let mut m = JsonMap::new();
            m.insert("bnode".into(), JsonValue::String(s.clone()));
            JsonValue::Object(m)
        }
            WitTerm::Triple(_) => panic!("quoted triples are not supported by this component"),
}
}

fn decode_array(v: &Value) -> Result<Vec<JsonValue>, String> {
    match v {
        WitTerm::Literal(l) if l.datatype.as_deref().unwrap_or("http://www.w3.org/2001/XMLSchema#string") == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.value)
                .map_err(|e| format!("array_append: invalid array literal JSON: {}", e))?;
            match parsed {
                JsonValue::Array(a) => Ok(a),
                _ => Err("array_append: array literal is not a JSON array".into()),
            }
        }
        _ => Err(format!("array_append: expected array literal (datatype {})", ARRAY_DATATYPE)),
    }
}

fn encode_array_json(items: Vec<JsonValue>) -> Value {
    WitTerm::Literal(WitLiteral { value: JsonValue::Array(items).to_string(), datatype: Some(ARRAY_DATATYPE.into()), language: None })
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "array_append".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "array_append" => evaluate_impl(args),
            other => Err(format!("array_append: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 2 {
            return Err(format!("array_append: expected 2 args (array, value), got {}", args.len()));
        }
        let mut arr = decode_array(&args[0])?;
        arr.push(value_to_json(&args[1]));
        Ok(encode_array_json(arr))
    }

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "array_append: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("array_append: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("array_append: aggregate state was never constructed".into())
    }
}

/// Property-function interface stub — this component provides none.
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
            "array_append: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

