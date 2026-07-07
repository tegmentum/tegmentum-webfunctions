//! nest_hierarchy — multi-level nesting of flat rows.
//!
//! Aggregate-shaped. Each row supplies variadic (name, value) pairs; the
//! first row's *first* argument is a JSON-array string listing the field
//! names in nesting order.
//!
//! Given rows like
//!
//!     nest_hierarchy('["family","subunit"]',
//!                    'family', ?family, 'subunit', ?subunit, 'seq', ?seq)
//!
//! and rows:
//!
//!     ("hemoglobin", "alpha",  "MVL...")
//!     ("hemoglobin", "beta",   "MVH...")
//!     ("hemoglobin", "delta",  "MVH...")
//!     ("myoglobin",  "single", "MGL...")
//!
//! …emit a nested JSON object:
//!
//!     {"hemoglobin": {"alpha": [...], "beta": [...], "delta": [...]},
//!      "myoglobin": {"single": [...]}}
//!
//! Leaves are lists of objects containing the remaining (non-hierarchy)
//! fields. This is the *post-hoc* nesting operator — it groups already-
//! materialised flat rows by successive keys. It DOES NOT run per-level
//! sub-queries with parent bindings; that requires wasm callback into
//! SPARQL and is intentionally a separate operator (see paper 2 sketch).

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::{json, Value as JsonValue};
use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::cell::RefCell;

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Iri(uri) => json!(uri),
        Value::Bnode(id) => json!(format!("_:{}", id)),
        Value::Literal(literal) => {
            let dt = literal.datatype.as_str();
            if dt.ends_with("#integer") || dt.ends_with("#long") || dt.ends_with("#int") {
                if let Ok(n) = literal.label.parse::<i64>() { return json!(n); }
            }
            if dt.ends_with("#decimal") || dt.ends_with("#double") || dt.ends_with("#float") {
                if let Ok(n) = literal.label.parse::<f64>() {
                    if n.is_finite() { return json!(n); }
                }
            }
            if dt.ends_with("#boolean") {
                if literal.label == "true" { return json!(true); }
                if literal.label == "false" { return json!(false); }
            }
            json!(literal.label)
        }
    }
}

fn json_key(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => s.clone(),
        other => other.to_string(),
    }
}

struct State {
    hierarchy: Option<Vec<String>>,
    rows: Vec<Vec<(String, JsonValue)>>,
}

thread_local! {
    static STATE: RefCell<State> = const {
        RefCell::new(State { hierarchy: None, rows: Vec::new() })
    };
}

fn args_to_row(args: &[Value]) -> Result<Vec<(String, JsonValue)>, String> {
    if args.len() % 2 != 0 {
        return Err(format!(
            "nest_hierarchy: expected an even number of field/value args, got {}",
            args.len()
        ));
    }
    let mut row = Vec::with_capacity(args.len() / 2);
    for pair in args.chunks(2) {
        let name = match &pair[0] {
            Value::Literal(l) => l.label.clone(),
            _ => return Err("nest_hierarchy: field-name args must be string literals".into()),
        };
        row.push((name, value_to_json(&pair[1])));
    }
    Ok(row)
}

/// Recursively nest a set of rows under successive keys.
fn nest(rows: &[Vec<(String, JsonValue)>], keys: &[String]) -> JsonValue {
    if keys.is_empty() {
        // Leaf level: emit the remaining fields as an array of objects.
        let arr: Vec<JsonValue> = rows.iter().map(|row| {
            let mut obj = serde_json::Map::new();
            for (k, v) in row { obj.insert(k.clone(), v.clone()); }
            JsonValue::Object(obj)
        }).collect();
        return JsonValue::Array(arr);
    }
    let key = &keys[0];
    let rest = &keys[1..];
    // Bucket rows by the value at `key`.
    let mut buckets: std::collections::BTreeMap<String, (Option<JsonValue>, Vec<Vec<(String, JsonValue)>>)>
        = std::collections::BTreeMap::new();
    for row in rows {
        let key_val = row.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let bucket_key = match &key_val {
            Some(v) => json_key(v),
            None => "<null>".into(),
        };
        let stripped: Vec<(String, JsonValue)> =
            row.iter().filter(|(k, _)| k != key).cloned().collect();
        let entry = buckets.entry(bucket_key).or_insert((key_val, Vec::new()));
        entry.1.push(stripped);
    }
    let mut obj = serde_json::Map::new();
    for (label, (_original, sub_rows)) in buckets {
        obj.insert(label, nest(&sub_rows, rest));
    }
    JsonValue::Object(obj)
}

impl Guest for Component {
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        Err("nest_hierarchy: use as an aggregate — not as a filter function".into())
    }

    fn aggregate_step(args: Vec<Value>, _mult: u64) -> Result<(), String> {
        if args.is_empty() {
            return Err("nest_hierarchy: first arg must be a JSON array of key names \
                        (['key1','key2',...])".into());
        }
        // First arg on every call is the hierarchy spec. We accept it once
        // (the first row) and require identical subsequent specs so callers
        // aren't tempted to change the hierarchy mid-aggregate.
        let (first, rest) = args.split_first().unwrap();
        let spec_str = match first {
            Value::Literal(l) => l.label.as_str(),
            _ => return Err("nest_hierarchy: first arg must be a string literal".into()),
        };
        let spec: Vec<String> = serde_json::from_str(spec_str)
            .map_err(|e| format!("nest_hierarchy: bad hierarchy spec {}: {}", spec_str, e))?;

        STATE.with(|s| {
            let mut st = s.borrow_mut();
            if let Some(existing) = &st.hierarchy {
                if existing != &spec {
                    return Err("nest_hierarchy: hierarchy spec changed mid-aggregate".to_string());
                }
            } else {
                st.hierarchy = Some(spec);
            }
            let row = args_to_row(rest)?;
            st.rows.push(row);
            Ok(())
        })
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        let (hierarchy, rows) = STATE.with(|s| {
            let mut st = s.borrow_mut();
            let h = st.hierarchy.take().unwrap_or_default();
            let r = std::mem::take(&mut st.rows);
            (h, r)
        });
        let tree = nest(&rows, &hierarchy);
        Ok(BindingSets {
            vars: vec!["tree".into()],
            rows: vec![vec![Binding {
                name: "tree".into(),
                value: string_literal(&tree.to_string()),
            }]],
        })
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "nest_hierarchy(hierarchy_spec, (name, value)+) -> nested JSON object. \
                     hierarchy_spec is a JSON array literal like '[\"a\",\"b\"]' listing \
                     the fields in nesting order. Aggregate over rows, then output is a JSON \
                     object grouped by each successive key. Leaves hold arrays of objects \
                     containing the non-hierarchy fields. Post-hoc grouping; does NOT run \
                     per-level sub-queries with parent bindings."),
            }]],
        }
    }
}

export!(Component);
