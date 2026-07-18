//! string_lang_detect — detect the language of a string.
//!
//! `wf:call(<string_lang_detect.wasm>, text)` returns the detected
//! language's ISO 639-3 code as an xsd:string, or an error if
//! whatlang cannot make a determination (input too short or too mixed).
//!
//! Ports semantalytics function_string_lang/detect. Replaces the
//! lingua-rs dependency (which required a wasm-patched fork at the
//! time) with whatlang, which compiles clean to wasm32-wasip1 and
//! carries its own trigram models so no external data files are
//! needed. Result crate is ~1.5 MB. For higher-accuracy detection on
//! short text, see the parallel string_lang_lingua_* set.

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

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg {
        WitTerm::Literal(l) => Ok(l.value.as_str()),
        _ => Err("string_lang_detect: argument must be a string literal".into()),
    }
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "string_lang_detect".to_string(),
            min_arity: 0,
            max_arity: None,
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "string_lang_detect" => evaluate_impl(args),
            other => Err(format!("string_lang_detect: unknown function '{other}'")),
        }
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 1 {
            return Err(format!(
                "string_lang_detect: expected 1 arg (text), got {}",
                args.len()
            ));
        }
        let text = string_of(&args[0])?;
        let info = whatlang::detect(text)
            .ok_or_else(|| "string_lang_detect: unable to detect language".to_string())?;
        Ok(string_literal(info.lang().code()))
    }

/// Aggregate interface stub — this component provides none.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "string_lang_detect: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("string_lang_detect: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("string_lang_detect: aggregate state was never constructed".into())
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
            "string_lang_detect: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

