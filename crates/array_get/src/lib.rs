//! array_get — return the element at a 0-based index of an array literal.
//!
//! Ports semantalytics/stardog-webfunctions/function_array/get. The source
//! looked the value up in Stardog's mapping dictionary; here the value is
//! carried inside the array literal itself.

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

use serde_json::Value as JsonValue;
struct Component;

const XSD_STRING:      &str = "http://www.w3.org/2001/XMLSchema#string";
const ARRAY_DATATYPE:  &str = "tag:stardog:api:array";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

fn json_to_value(j: &JsonValue) -> Result<Value, String> {
    let obj = j.as_object().ok_or_else(|| "array_get: element is not an object".to_string())?;
    if let Some(iri) = obj.get("iri").and_then(|v| v.as_str()) {
        return Ok(WitTerm::NamedNode(iri.to_string()));
    }
    if let Some(bnode) = obj.get("bnode").and_then(|v| v.as_str()) {
        return Ok(WitTerm::BlankNode(bnode.to_string()));
    }
    if let Some(lit) = obj.get("literal").and_then(|v| v.as_object()) {
        let label = lit.get("label").and_then(|v| v.as_str())
            .ok_or_else(|| "array_get: literal.value missing".to_string())?.to_string();
        let datatype = lit.get("datatype").and_then(|v| v.as_str())
            .ok_or_else(|| "array_get: literal.datatype missing".to_string())?.to_string();
        let lang = lit.get("lang").and_then(|v| v.as_str()).map(String::from);
        return Ok(WitTerm::Literal(WitLiteral { value: label, datatype: Some(datatype), language: lang }));
    }
    Err("array_get: unknown element shape".into())
}

fn decode_array(v: &Value) -> Result<Vec<JsonValue>, String> {
    match v {
        WitTerm::Literal(l) if l.datatype.as_deref().unwrap_or("http://www.w3.org/2001/XMLSchema#string") == ARRAY_DATATYPE => {
            let parsed: JsonValue = serde_json::from_str(&l.value)
                .map_err(|e| format!("array_get: invalid array literal JSON: {}", e))?;
            match parsed {
                JsonValue::Array(a) => Ok(a),
                _ => Err("array_get: array literal is not a JSON array".into()),
            }
        }
        _ => Err(format!("array_get: expected array literal (datatype {})", ARRAY_DATATYPE)),
    }
}

fn index_of(v: &Value) -> Result<usize, String> {
    match v {
        WitTerm::Literal(l) => l.value.parse::<usize>()
            .map_err(|e| format!("array_get: index not a non-negative integer: {}", e)),
        _ => Err("array_get: index must be a numeric literal".into()),
    }
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "array_get".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "array_get" => evaluate_impl(args),
            other => Err(format!("array_get: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 2 {
            return Err(format!("array_get: expected 2 args (array, index), got {}", args.len()));
        }
        let arr = decode_array(&args[0])?;
        let idx = index_of(&args[1])?;
        let elem = arr.get(idx)
            .ok_or_else(|| format!("array_get: index {} out of bounds (len {})", idx, arr.len()))?;
        let value = json_to_value(elem)?;
        Ok(value)
    }

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "array_get: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("array_get: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("array_get: aggregate state was never constructed".into())
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
            "array_get: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

