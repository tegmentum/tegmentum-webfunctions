//! sirix_udf_hello — reference SQL UDF for the sirix:plugin@0.1.0 world.
//!
//! Signature: `sirix_udf_hello(name: text) -> text`
//!   → returns `"hello, {name}"`.
//!
//! Deliberately trivial. The purpose is to exercise the plugin loader's
//! end-to-end path (load component → instantiate → invoke `sql-udf#evaluate`
//! → decode the returned `value`) — not to demonstrate anything about
//! Calcite integration, which lands in phase 2.

wit_bindgen::generate!({
    world: "sql-udf-plugin",
    path: "wit",
});

use exports::sirix::plugin::sql_udf::Guest;
use sirix::plugin::types::Value;

struct Component;

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 1 {
            return Err(format!(
                "sirix_udf_hello: expected 1 arg, got {}",
                args.len()
            ));
        }
        let name = match &args[0] {
            Value::Text(s) => s.clone(),
            Value::NullValue => return Ok(Value::NullValue),
            other => {
                return Err(format!(
                    "sirix_udf_hello: expected text arg, got {other:?}"
                ));
            }
        };
        Ok(Value::Text(format!("hello, {name}")))
    }
}

export!(Component);
