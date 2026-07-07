//! emit_json — aggregate rows into a JSON string.
//!
//! Dual to `parse_json`: where parse_json turns a JSON document into
//! binding-sets, emit_json turns binding-sets into a JSON document.
//! Because the input width and column set are only known at call-time
//! (they are the aggregate arguments), the caller passes name-value
//! pairs as pairs of arguments:
//!
//!   (agg wf:call ?k1 ?v1 ?k2 ?v2 ...)
//!
//! Even-indexed arguments (0, 2, 4, …) are keys and must be string
//! literals. Odd-indexed arguments are values and may be any WIT Value
//! variant — IRIs stringify to their IRI, literals to their label,
//! bnodes to their id. All emitted JSON values are strings; that keeps
//! the mapping simple and reversible via parse_json for the string case
//! and avoids ambiguity for the datatyped-literal case.
//!
//! Each `aggregate-step` call produces one JSON object and appends it
//! (repeated by `mult`) to a thread_local Vec. `aggregate-finish`
//! serialises the vec as a JSON array via serde_json and resets state.
//! `evaluate` errors — there is nothing to aggregate over a single row.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::{Map as JsonMap, Value as JsonValue};
use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::cell::RefCell;

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

// Accumulator for aggregate-step. Each step pushes one JSON object; a
// step with multiplicity N pushes N copies (matching Stardog's row-
// multiplicity semantics used by agg_sum et al.). aggregate-finish
// drains and resets so a subsequent aggregation on the same instance
// starts clean.
thread_local! {
    static ROWS: RefCell<Vec<JsonValue>> = const { RefCell::new(Vec::new()) };
}

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

/// Extract a key string from an even-indexed argument. Keys must be
/// string literals; IRIs and bnodes are rejected because they are not
/// user-authored column names and using them silently would mask a
/// programming error at the query site.
fn key_of(v: &Value, index: usize) -> Result<String, String> {
    match v {
        Value::Literal(lit) => Ok(lit.label.clone()),
        Value::Iri(_) => Err(format!(
            "emit_json: key at argument index {} must be a string literal, got IRI",
            index
        )),
        Value::Bnode(_) => Err(format!(
            "emit_json: key at argument index {} must be a string literal, got blank node",
            index
        )),
    }
}

/// Stringify any WIT Value into the JSON string used as the object's
/// field value. IRIs render as the IRI itself, literals as their
/// lexical label (datatype and language tag are dropped — callers who
/// need round-tripping can compose emit_json with an explicit STR()
/// / DATATYPE() shape at the SPARQL layer), bnodes as their id.
fn value_as_json_string(v: &Value) -> String {
    match v {
        Value::Iri(s) => s.clone(),
        Value::Literal(lit) => lit.label.clone(),
        Value::Bnode(s) => s.clone(),
    }
}

impl Guest for Component {
    /// A single-row emit_json is meaningless: the whole point of the
    /// function is to fold a stream of rows into one JSON document.
    /// Fail loudly rather than degrade to a one-element array, which
    /// would hide a query-shape mistake.
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        Err("emit_json: use via SPARQL aggregate; direct evaluate is not supported".into())
    }

    fn aggregate_step(args: Vec<Value>, mult: u64) -> Result<(), String> {
        if args.len() % 2 != 0 {
            return Err(format!(
                "emit_json: expected an even number of arguments (key/value pairs), got {}",
                args.len()
            ));
        }
        // preserve_order is enabled in Cargo.toml, so the resulting map
        // keeps insertion order — the object's field order matches the
        // argument order at the call site, which is what the caller
        // reads at their end.
        let mut obj: JsonMap<String, JsonValue> = JsonMap::new();
        let mut i = 0;
        while i < args.len() {
            let key = key_of(&args[i], i)?;
            let value = value_as_json_string(&args[i + 1]);
            obj.insert(key, JsonValue::String(value));
            i += 2;
        }
        let row = JsonValue::Object(obj);
        ROWS.with(|rows| {
            let mut rows = rows.borrow_mut();
            // Push `mult` copies to honour aggregate multiplicity.
            // `mult` is u64 but wasm memory is bounded — a pathological
            // multiplicity would OOM long before this loop misbehaves.
            for _ in 0..mult {
                rows.push(row.clone());
            }
        });
        Ok(())
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        let rows = ROWS.with(|r| std::mem::take(&mut *r.borrow_mut()));
        let json = serde_json::to_string(&JsonValue::Array(rows))
            .map_err(|e| format!("emit_json: serialisation failed: {}", e))?;
        Ok(BindingSets {
            vars: vec!["json".into()],
            rows: vec![vec![Binding {
                name: "json".into(),
                value: string_literal(&json),
            }]],
        })
    }

    fn cardinality_estimate(input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        // N input rows collapse to exactly one JSON string (zero if the
        // input group is empty, but Stardog only invokes an aggregate
        // finish when the group is non-empty, so 1 is the honest
        // estimate).
        Ok(Cardinality {
            value: 1.0f64.min(input.value.max(0.0)),
            accuracy: Accuracy::Accurate,
        })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "emit_json(k1, v1, k2, v2, ...) -> xsd:string. \
                     Aggregate: builds one JSON object per input row from \
                     (key, value) argument pairs and returns the array of \
                     objects as a JSON string in variable 'json'. Keys must \
                     be string literals; values of any kind are stringified \
                     (IRIs to their IRI, literals to their label, bnodes to \
                     their id).",
                ),
            }]],
        }
    }
}

export!(Component);
