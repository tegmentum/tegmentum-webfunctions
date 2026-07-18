//! jmespath_search — evaluate a JMESPath expression against a JSON string.
//!
//! Ports the semantalytics function_json_jmespath/search crate.
//! Argument 0 is the JMESPath expression, argument 1 is the JSON document,
//! both as string literals. Returns the search result as a JSON-encoded
//! xsd:string literal (matching the source crate's shape); callers can
//! then pipe through `parse_json` if they want to unfold rows.

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

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

fn string_of(arg: &Value, which: &str) -> Result<String, String> {
    match arg {
        WitTerm::Literal(l) => Ok(l.value.clone()),
        _ => Err(format!("jmespath_search: {} must be a string literal", which)),
    }
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "jmespath_search".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "jmespath_search" => evaluate_impl(args),
            other => Err(format!("jmespath_search: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
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

        Ok(string_literal(&output))
    }

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "jmespath_search: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("jmespath_search: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("jmespath_search: aggregate state was never constructed".into())
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
            "jmespath_search: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

