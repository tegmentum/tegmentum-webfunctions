//! sirix_shredder_uppercase — reference JSON shredder plugin for the
//! sirix:plugin@0.1.0 world.
//!
//! Uppercases every string value in the incoming JSON document, leaving
//! keys, structure, numbers, booleans, and nulls untouched. Walks the
//! tree with serde_json rather than doing a byte-level rewrite so nested
//! objects/arrays are handled correctly.

wit_bindgen::generate!({
    world: "json-shredder-plugin",
    path: "wit",
});

use exports::sirix::plugin::json_shredder::Guest;
use serde_json::Value;

struct Component;

fn upper_strings(v: &mut Value) {
    match v {
        Value::String(s) => *s = s.to_uppercase(),
        Value::Array(items) => items.iter_mut().for_each(upper_strings),
        Value::Object(map) => map.values_mut().for_each(upper_strings),
        _ => {}
    }
}

impl Guest for Component {
    fn transform(input: Vec<u8>) -> Result<Vec<u8>, String> {
        let mut value: Value = serde_json::from_slice(&input)
            .map_err(|e| format!("sirix_shredder_uppercase: parse failed: {e}"))?;
        upper_strings(&mut value);
        serde_json::to_vec(&value)
            .map_err(|e| format!("sirix_shredder_uppercase: serialize failed: {e}"))
    }
}

export!(Component);
