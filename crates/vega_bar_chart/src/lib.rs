//! vega_bar_chart — one-shot Vega-Lite bar chart builder.
//!
//! Migrated (Follow-up E) from the Stardog overlay
//! `stardog:webfunction@0.3.2` world to the base
//! `tegmentum:webfunction/extension-with-host-callbacks@0.1.0` world.
//!
//! Signature: `vega_bar_chart(sparql_query, x_var, y_var
//!            [, title [, y_is_quantitative]])` -> rdf:JSON literal.
//!
//! Executes `sparql_query` via `graph-callbacks::execute-query`,
//! interprets the flat-list bindings as rows keyed on `x_var` / `y_var`,
//! and wraps the array in a complete Vega-Lite spec ready to hand to a
//! Vega-Lite renderer. The returned term is a single rdf:JSON literal.
//!
//! The Stardog-overlay original returned a multi-row binding-set with
//! one `spec` binding; the base sparql-extension surface returns a
//! single `term` from `extension::call`, so the payload collapses to
//! that one literal (equivalent to reading the first row of the old
//! shape). Field types remain the same: `x_var` nominal (categorical
//! bar labels), `y_var` quantitative when inferable, else nominal.

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
use bindings::tegmentum::webfunction::graph_callbacks::{
    self as gc, Binding as WitBinding, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

use serde_json::{json, Value as JsonValue};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const VEGA_LITE_SCHEMA: &str = "https://vega.github.io/schema/vega-lite/v5.json";

fn value_as_string(v: &WitTerm) -> String {
    match v {
        WitTerm::NamedNode(uri) => uri.clone(),
        WitTerm::BlankNode(id) => format!("_:{id}"),
        WitTerm::Literal(l) => l.value.clone(),
        WitTerm::Triple(_) => String::new(),
    }
}

fn value_as_json_scalar(v: &WitTerm, treat_as_number: bool) -> JsonValue {
    let s = value_as_string(v);
    if treat_as_number {
        if let Ok(n) = s.parse::<i64>() {
            return json!(n);
        }
        if let Ok(n) = s.parse::<f64>() {
            if n.is_finite() {
                return json!(n);
            }
        }
    }
    json!(s)
}

fn arg_as_string(v: &WitTerm, arg_ix: usize) -> Result<String, String> {
    match v {
        WitTerm::Literal(l) => Ok(l.value.clone()),
        WitTerm::NamedNode(uri) => Ok(uri.clone()),
        _ => Err(format!(
            "vega_bar_chart: arg {arg_ix} must be a string literal or IRI"
        )),
    }
}

fn execute_query(sparql: &str) -> Result<CallbackQueryResult, String> {
    gc::execute_query(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

/// Group a flat `list<binding>` into rows on the boundary where a
/// variable repeats. Same shape used by wf_infer-extension.
fn group_bindings_into_rows(flat: Vec<WitBinding>) -> Vec<Vec<WitBinding>> {
    let mut rows: Vec<Vec<WitBinding>> = Vec::new();
    let mut current: Vec<WitBinding> = Vec::new();
    for b in flat {
        if current.iter().any(|prior| prior.variable == b.variable) {
            rows.push(std::mem::take(&mut current));
        }
        current.push(b);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

fn evaluate_impl(args: Vec<WitTerm>) -> Result<WitTerm, String> {
    if args.len() < 3 || args.len() > 5 {
        return Err(format!(
            "vega_bar_chart: expected 3..5 args (sparql, x_var, y_var[, title[, y_is_quantitative]]), got {}",
            args.len()
        ));
    }
    let sparql = arg_as_string(&args[0], 0)?;
    let x_var = arg_as_string(&args[1], 1)?;
    let y_var = arg_as_string(&args[2], 2)?;
    let title = if args.len() >= 4 {
        Some(arg_as_string(&args[3], 3)?)
    } else {
        None
    };
    let y_pin = if args.len() >= 5 {
        let s = arg_as_string(&args[4], 4)?;
        match s.to_ascii_lowercase().as_str() {
            "true" | "quantitative" | "q" => Some(true),
            "false" | "nominal" | "n" => Some(false),
            _ => None,
        }
    } else {
        None
    };

    let flat = match execute_query(&sparql)? {
        CallbackQueryResult::Bindings(b) => b,
        CallbackQueryResult::Quads(_) => {
            return Err(
                "vega_bar_chart: SELECT expected but graph-callbacks returned quads".into(),
            );
        }
        CallbackQueryResult::Boolean(_) => {
            return Err(
                "vega_bar_chart: SELECT expected but graph-callbacks returned boolean".into(),
            );
        }
    };

    let rows = group_bindings_into_rows(flat);

    let y_numeric = y_pin.unwrap_or_else(|| {
        rows.first()
            .and_then(|row| row.iter().find(|b| b.variable == y_var))
            .map(|b| {
                let s = value_as_string(&b.value);
                s.parse::<f64>().map(|n| n.is_finite()).unwrap_or(false)
            })
            .unwrap_or(false)
    });

    let mut have_x = false;
    let mut have_y = false;
    let mut values = Vec::with_capacity(rows.len());
    for row in &rows {
        let x_val = row
            .iter()
            .find(|b| b.variable == x_var)
            .map(|b| {
                have_x = true;
                json!(value_as_string(&b.value))
            })
            .unwrap_or(JsonValue::Null);
        let y_val = row
            .iter()
            .find(|b| b.variable == y_var)
            .map(|b| {
                have_y = true;
                value_as_json_scalar(&b.value, y_numeric)
            })
            .unwrap_or(JsonValue::Null);
        values.push(json!({
            x_var.clone(): x_val,
            y_var.clone(): y_val,
        }));
    }

    if !rows.is_empty() && !have_x {
        return Err(format!(
            "vega_bar_chart: x_var `{x_var}` not present in any result row"
        ));
    }
    if !rows.is_empty() && !have_y {
        return Err(format!(
            "vega_bar_chart: y_var `{y_var}` not present in any result row"
        ));
    }

    let mut spec = json!({
        "$schema": VEGA_LITE_SCHEMA,
        "data": { "values": values },
        "mark": "bar",
        "encoding": {
            "x": { "field": &x_var, "type": "nominal", "sort": "-y" },
            "y": {
                "field": &y_var,
                "type": if y_numeric { "quantitative" } else { "nominal" }
            }
        }
    });
    if let Some(t) = title {
        spec["title"] = json!(t);
    }

    Ok(WitTerm::Literal(WitLiteral {
        value: spec.to_string(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    }))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "vega_bar_chart".into(),
            min_arity: 3,
            max_arity: Some(5),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "vega_bar_chart" => evaluate_impl(args),
            other => Err(format!("vega_bar_chart: unknown function '{other}'")),
        }
    }
}

impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "vega_bar_chart: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("vega_bar_chart: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("vega_bar_chart: aggregate state was never constructed".into())
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
            "vega_bar_chart: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);
