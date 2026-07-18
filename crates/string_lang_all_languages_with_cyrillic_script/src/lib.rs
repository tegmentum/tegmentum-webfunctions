//! string_lang_all_languages_with_cyrillic_script — enumerate whatlang languages canonically written in the Cyrillic script.
//!
//! whatlang does not expose a Lang -> Script mapping (a Lang can be written
//! in multiple scripts; detection classifies scripts from text, not from
//! Lang variants), so this crate carries a hardcoded per-script language
//! table matching the corresponding semantalytics original's semantics.

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

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

const SCRIPT_LANGS: &[&str] = &["rus", "ukr", "bel", "bul", "mkd", "srp"];

fn evaluate_impl(_args: Vec<Value>) -> Result<BindingSets, String> {
        let rows = SCRIPT_LANGS.iter().map(|c| vec![Binding {
            name: "lang".into(), value: string_literal(c)
        }]).collect();
        Ok(BindingSets { vars: vec!["lang".into()], rows })
    }

/// Filter interface stub — property-function-shaped component.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "string_lang_all_languages_with_cyrillic_script: unknown filter function '{name}' (use as a property function)"
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
            "string_lang_all_languages_with_cyrillic_script: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("string_lang_all_languages_with_cyrillic_script: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("string_lang_all_languages_with_cyrillic_script: aggregate state was never constructed".into())
    }
}

impl PropertyFunctionGuest for Component {
    fn register_property_functions() -> Vec<PropertyDescriptor> {
        vec![PropertyDescriptor {
            name: "string_lang_all_languages_with_cyrillic_script".to_string(),
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
            "string_lang_all_languages_with_cyrillic_script" => {
                let mut args = subjects;
                args.extend(objects);
                let bs = evaluate_impl(args)?;
                Ok(to_binding_rows(bs))
            }
            other => Err(format!("string_lang_all_languages_with_cyrillic_script: unknown property function '{other}'")),
        }
    }
}

bindings::export!(Component with_types_in bindings);

