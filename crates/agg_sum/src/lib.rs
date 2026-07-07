//! agg_sum — integer sum aggregator.
//!
//! Ported from the module-mode ABI (semantalytics/stardog-webfunctions/
//! aggregate/sum) to the Component Model ABI. In the old ABI the aggregate
//! step and finish semantics were folded into a single `aggregate` export
//! that received a JSON payload carrying both the value and the row
//! multiplicity, and each call returned the running total. Under the new
//! ABI those responsibilities are split across `aggregate-step` (state
//! accumulation, no return payload) and `aggregate-finish` (emit result
//! and reset state). Aggregate state is held in a thread_local RefCell —
//! the same pattern used by the canonical `sum_component` reference crate
//! in stardog-webfunction-plugin.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::cell::RefCell;

// State persists across aggregate-step calls until aggregate-finish flushes it.
thread_local! {
    static STATE: RefCell<i64> = const { RefCell::new(0) };
}

struct Component;

const XSD_STRING:  &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

fn integer_of(v: &Value) -> Result<i64, String> {
    match v {
        Value::Literal(lit) => lit
            .label
            .parse::<i64>()
            .map_err(|e| format!("agg_sum: value not parseable as integer: {}", e)),
        _ => Err("agg_sum: argument must be a literal".into()),
    }
}

impl Guest for Component {
    /// `evaluate` is meaningful only inside an aggregate context. The old
    /// ABI never exposed a single-row sum path (the module exported only
    /// `aggregate` / `get_value` / `doc`), so there is nothing sensible to
    /// return here.
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        Err("agg_sum: use via SPARQL aggregate; direct evaluate is not supported".into())
    }

    fn aggregate_step(args: Vec<Value>, mult: u64) -> Result<(), String> {
        let n = match args.first() {
            Some(v) => integer_of(v)?,
            None => return Err("agg_sum: expected at least one argument".into()),
        };
        let mult_i64: i64 = mult
            .try_into()
            .map_err(|_| "agg_sum: multiplicity exceeds i64::MAX".to_string())?;
        STATE.with(|s| *s.borrow_mut() = s.borrow().saturating_add(n.saturating_mul(mult_i64)));
        Ok(())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        let sum = STATE.with(|s| *s.borrow());
        // Reset so that a subsequent aggregation on the same instance starts clean.
        STATE.with(|s| *s.borrow_mut() = 0);
        Ok(BindingSets {
            vars: vec!["value_0".into()],
            rows: vec![vec![Binding {
                name: "value_0".into(),
                value: Value::Literal(Literal {
                    label: sum.to_string(),
                    datatype: XSD_INTEGER.into(),
                    lang: None,
                }),
            }]],
        })
    }

    fn cardinality_estimate(input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        // Sum of N rows produces exactly one row (zero if the input is empty).
        Ok(Cardinality {
            value: 1.0f64.min(input.value),
            accuracy: Accuracy::Accurate,
        })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: Value::Literal(Literal {
                    label: "agg_sum(value_0) -> xsd:integer. Sums the integer value of value_0 across rows, weighted by row multiplicity.".into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

export!(Component);
