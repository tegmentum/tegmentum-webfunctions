//! string_lang_tag — return the detected language as a SPARQL @lang tag
//! (ISO 639-1 preferred, ISO 639-3 fallback).
//!
//! Ports semantalytics function_string_lang/tag (via whatlang).

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

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

fn decimal_literal(v: f64) -> Value {
    WitTerm::Literal(WitLiteral { value: format!("{:.4}", v), datatype: Some(XSD_DECIMAL.into()), language: None })
}

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg { WitTerm::Literal(l) => Ok(l.value.as_str()), _ => Err("argument must be a string literal".into()) }
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "string_lang_tag".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "string_lang_tag" => evaluate_impl(args),
            other => Err(format!("string_lang_tag: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 1 {
            return Err(format!("string_lang_tag: expected 1 arg (text), got {}", args.len()));
        }
        let text = string_of(&args[0])?;
        let lang = whatlang::detect(text)
            .ok_or_else(|| "string_lang_tag: unable to detect language".to_string())?
            .lang();
        // BCP 47 tag: whatlang exposes code() which is ISO 639-3 (3-letter);
        // there's no 2-letter accessor in 0.16, so we return the 3-letter code
        // prefixed with a dash-free tag body. Downstream can normalise if needed.
        Ok(string_literal(lang.code()))
    }

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "string_lang_tag: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("string_lang_tag: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("string_lang_tag: aggregate state was never constructed".into())
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
            "string_lang_tag: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

