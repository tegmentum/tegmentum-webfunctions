//! json_path — evaluate a JSONPath expression against a JSON string
//! and return the matched values as rows.
//!
//! Arguments (positional):
//!   0. json_source   — string literal containing a JSON document
//!   1. jsonpath_expr — string literal containing an RFC 9535 JSONPath expression
//!
//! Output: one row per matched value, single variable `value` bound to
//! each match. Scalars are typed exactly as in `parse_json`:
//!
//!   * bool           → xsd:boolean
//!   * number (int)   → xsd:integer
//!   * number (float) → xsd:decimal
//!   * string         → xsd:string
//!   * null           → row omitted (no unbound `value` cell)
//!
//! Structured matches (objects, arrays) are returned as JSON-stringified
//! xsd:string literals; pipe through `parse_json` to unfold further.

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

use serde_json::Value as JsonValue;
use serde_json_path::JsonPath;
struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

fn typed_literal(label: String, datatype: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: label, datatype: Some(datatype.into()), language: None })
}

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

/// Convert a JSON match into a WIT Value using the same typing rules
/// as `parse_json`. Returns `None` for JSON null so the caller can drop
/// the row rather than emit an unbound `value` cell.
fn scalar(v: &JsonValue) -> Option<Value> {
    match v {
        JsonValue::Null => None,
        JsonValue::Bool(b) => Some(typed_literal(b.to_string(), XSD_BOOLEAN)),
        JsonValue::Number(n) => {
            if n.is_i64() || n.is_u64() {
                Some(typed_literal(n.to_string(), XSD_INTEGER))
            } else {
                Some(typed_literal(n.to_string(), XSD_DECIMAL))
            }
        }
        JsonValue::String(s) => Some(typed_literal(s.clone(), XSD_STRING)),
        // Structured: preserve as JSON-stringified xsd:string. Consumer
        // can recurse with parse_json / json_path.
        _ => Some(typed_literal(v.to_string(), XSD_STRING)),
    }
}

fn string_of(arg: &Value, which: &str) -> Result<String, String> {
    match arg {
        WitTerm::Literal(l) => Ok(l.value.clone()),
        _ => Err(format!("json_path: {} must be a string literal", which)),
    }
}

fn evaluate_impl(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!("json_path: expected 2 args, got {}", args.len()));
        }
        let source = string_of(&args[0], "argument 0 (JSON document)")?;
        let expression = string_of(&args[1], "argument 1 (JSONPath expression)")?;

        let document: JsonValue = serde_json::from_str(&source)
            .map_err(|e| format!("json_path: invalid JSON: {}", e))?;
        let path = JsonPath::parse(&expression)
            .map_err(|e| format!("json_path: bad expression: {}", e))?;

        let vars = vec!["value".into()];
        let rows: Vec<Vec<Binding>> = path
            .query(&document)
            .all()
            .into_iter()
            .filter_map(|node| scalar(node).map(|v| vec![Binding {
                name: "value".into(),
                value: v,
            }]))
            .collect();

        Ok(BindingSets { vars, rows })
    }

/// Filter interface stub — property-function-shaped component.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "json_path: unknown filter function '{name}' (use as a property function)"
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
            "json_path: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("json_path: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("json_path: aggregate state was never constructed".into())
    }
}

impl PropertyFunctionGuest for Component {
    fn register_property_functions() -> Vec<PropertyDescriptor> {
        vec![PropertyDescriptor {
            name: "json_path".to_string(),
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
            "json_path" => {
                let mut args = subjects;
                args.extend(objects);
                let bs = evaluate_impl(args)?;
                Ok(to_binding_rows(bs))
            }
            other => Err(format!("json_path: unknown property function '{other}'")),
        }
    }
}

bindings::export!(Component with_types_in bindings);

