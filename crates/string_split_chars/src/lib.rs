//! string_split_chars — split a string into its constituent characters.
//!
//! Ports the semantalytics function_string/split_chars crate. The original
//! stashed each character in Stardog's MappingDictionary and returned a
//! `tag:stardog:api:array` literal. Under the Component Model that
//! side-channel is gone — instead we return one row per character with the
//! `result` variable bound to a `xsd:string` literal for that character.

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

/// Legacy names kept as aliases so the ported property-function body
/// reads with minimum diff against the flat-world original.
type Value = WitTerm;
type Literal = WitLiteral;

/// Local shim mirroring the old `Binding` shape (`name`, `value`) so the
/// port keeps the original construction sites unchanged. Column names
/// are dropped when converting to the base world's `BindingRow`, which
/// carries only positional values.
struct Binding {
    #[allow(dead_code)]
    name: String,
    value: WitTerm,
}

/// Local shim mirroring the old `BindingSets` shape (`vars`, `rows`).
struct BindingSets {
    #[allow(dead_code)]
    vars: Vec<String>,
    rows: Vec<Vec<Binding>>,
}

fn to_binding_rows(bs: BindingSets) -> Vec<BindingRow> {
    bs.rows
        .into_iter()
        .map(|row| BindingRow {
            values: row.into_iter().map(|b| b.value).collect(),
        })
        .collect()
}

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}
fn string_of(a: &Value) -> Result<&str, String> {
    match a {
        WitTerm::Literal(l) => Ok(l.value.as_str()),
        _ => Err("string_split_chars: argument must be a string literal".into()),
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 1 {
            return Err(format!("string_split_chars: expected 1 arg, got {}", args.len()));
        }
        let rows: Vec<Vec<Binding>> = voca_rs::split::chars(string_of(&args[0])?)
            .into_iter()
            .map(|c| vec![Binding { name: "result".into(), value: string_literal(c) }])
            .collect();
        Ok(BindingSets { vars: vec!["result".into()], rows })
    }

/// Filter interface stub — property-function-shaped component.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "string_split_chars: unknown filter function '{name}' (use as a property function)"
        ))
    }
}

/// Aggregate interface stub.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "string_split_chars: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("string_split_chars: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("string_split_chars: aggregate state was never constructed".into())
    }
}

impl PropertyFunctionGuest for Component {
    fn register_property_functions() -> Vec<PropertyDescriptor> {
        vec![PropertyDescriptor {
            name: "string_split_chars".to_string(),
            subject_arity: 0,
            object_arity: 0,
        }]
    }

    fn evaluate(
        name: String,
        subjects: Vec<WitTerm>,
        objects: Vec<WitTerm>,
    ) -> Result<Vec<BindingRow>, String> {
        match name.as_str() {
            "string_split_chars" => {
                let mut args = subjects;
                args.extend(objects);
                let bs = evaluate_impl(args)?;
                Ok(to_binding_rows(bs))
            }
            other => Err(format!("string_split_chars: unknown property function '{other}'")),
        }
    }
}

bindings::export!(Component with_types_in bindings);

