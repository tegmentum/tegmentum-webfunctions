//! vega_bar_chart — one-shot Vega-Lite bar chart builder.
//!
//! Signature: `wf:call(<vega_bar_chart.wasm>, sparql_query, x_var, y_var
//!            [, title [, y_is_quantitative]])` → rdf:JSON literal.
//!
//! Executes `sparql_query` via the host `execute-query` callback,
//! materialises each returned row as `{ x_var: ..., y_var: ... }`, and
//! wraps the array in a complete Vega-Lite spec ready to hand to a
//! Vega-Lite renderer without further processing on the client side.
//!
//! Field types: `x_var` is emitted as `"nominal"` (categorical bar
//! labels), `y_var` as `"quantitative"` when the guest can parse it as
//! a number, else `"nominal"`. Callers can pin `y_is_quantitative` to
//! "true" or "false" via the 5th arg when they know better than the
//! guest's inference.
//!
//! Return type: literal with datatype rdf:JSON so consumers can
//! dispatch on datatype instead of re-parsing an untyped string. The
//! spec is `$schema`-tagged for Vega-Lite v5.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::{json, Value as JsonValue};
use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const VEGA_LITE_SCHEMA: &str = "https://vega.github.io/schema/vega-lite/v5.json";

fn value_as_string(v: &Value) -> String {
    match v {
        Value::Iri(uri) => uri.clone(),
        Value::Bnode(id) => format!("_:{}", id),
        Value::Literal(l) => l.label.clone(),
    }
}

fn value_as_json_scalar(v: &Value, treat_as_number: bool) -> JsonValue {
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

fn arg_as_string(v: &Value, arg_ix: usize) -> Result<String, String> {
    match v {
        Value::Literal(l) => Ok(l.label.clone()),
        Value::Iri(uri) => Ok(uri.clone()),
        _ => Err(format!(
            "vega_bar_chart: arg {} must be a string literal or IRI",
            arg_ix
        )),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
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

        let bs = host::execute_query(&sparql, &[], None)?;

        let x_col = bs
            .vars
            .iter()
            .position(|v| v == &x_var)
            .ok_or_else(|| format!("vega_bar_chart: x_var `{}` not in query result vars", x_var))?;
        let y_col = bs
            .vars
            .iter()
            .position(|v| v == &y_var)
            .ok_or_else(|| format!("vega_bar_chart: y_var `{}` not in query result vars", y_var))?;

        let y_numeric = y_pin.unwrap_or_else(|| {
            bs.rows
                .first()
                .and_then(|row| row.get(y_col))
                .map(|b| {
                    let s = value_as_string(&b.value);
                    s.parse::<f64>().map(|n| n.is_finite()).unwrap_or(false)
                })
                .unwrap_or(false)
        });

        let mut values = Vec::with_capacity(bs.rows.len());
        for row in &bs.rows {
            let x_val = row
                .get(x_col)
                .map(|b| json!(value_as_string(&b.value)))
                .unwrap_or(JsonValue::Null);
            let y_val = row
                .get(y_col)
                .map(|b| value_as_json_scalar(&b.value, y_numeric))
                .unwrap_or(JsonValue::Null);
            values.push(json!({
                x_var.clone(): x_val,
                y_var.clone(): y_val,
            }));
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

        Ok(BindingSets {
            vars: vec!["spec".into()],
            rows: vec![vec![Binding {
                name: "spec".into(),
                value: Value::Literal(Literal {
                    label: spec.to_string(),
                    datatype: RDF_JSON.into(),
                    lang: None,
                }),
            }]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("vega_bar_chart: aggregate not applicable".into())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("vega_bar_chart: aggregate not applicable".into())
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality {
            value: 1.0,
            accuracy: Accuracy::Injected,
        })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: Value::Literal(Literal {
                    label: "vega_bar_chart(sparql, x_var, y_var [, title [, y_is_quantitative]]) \
                            -> rdf:JSON Vega-Lite bar-chart spec ready for a Vega-Lite renderer."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

export!(Component);
