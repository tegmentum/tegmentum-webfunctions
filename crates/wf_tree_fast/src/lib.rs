//! wf_tree_fast — same tree-walk behavior as wf_tree, but uses the v0.3.3
//! `follow-predicate` host import instead of SPARQL sub-queries.
//!
//! Signature (Rust view): `wf:call(<wasm>, root, predicate [, child_var])`.
//! Compare to wf_tree, which takes a SPARQL string as arg 2.
//!
//! For the walk-one-predicate pattern this cuts:
//!   - SPARQL parse (moot after v0.3.2 prepare, but still gone)
//!   - Initial-binding substitution
//!   - Full binding-set materialisation (this returned a list<value>,
//!     not a record with vars + list<list<binding>>)
//!   - Guest-side row iteration + attribute extraction
//!
//! Expect ~5× vs prepared queries on the tight inner loop.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde_json::{json, Value as JsonValue};
use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::collections::HashSet;

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const DEPTH_SOFT_CAP: u32 = 90;

fn value_as_string(v: &Value) -> String {
    match v {
        Value::Iri(uri) => uri.clone(),
        Value::Bnode(id) => format!("_:{}", id),
        Value::Literal(l) => l.label.clone(),
    }
}

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn walk(node: &Value, predicate: &Value, seen: &mut HashSet<String>) -> JsonValue {
    let node_key = value_as_string(node);

    let mut obj = serde_json::Map::new();
    obj.insert("uri".into(), json!(node_key));

    if !seen.insert(node_key.clone()) {
        obj.insert("cycle".into(), json!(true));
        return JsonValue::Object(obj);
    }

    if host::callback_depth() >= DEPTH_SOFT_CAP {
        obj.insert("depth_bounded".into(), json!(true));
        seen.remove(&node_key);
        return JsonValue::Object(obj);
    }

    let children_values = match host::follow_predicate(node, predicate) {
        Ok(vs) => vs,
        Err(e) => {
            obj.insert("error".into(), json!(e));
            seen.remove(&node_key);
            return JsonValue::Object(obj);
        }
    };

    let children: Vec<JsonValue> = children_values.iter()
        .map(|child_v| walk(child_v, predicate, seen))
        .collect();
    obj.insert("children".into(), JsonValue::Array(children));

    seen.remove(&node_key);
    JsonValue::Object(obj)
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.len() != 2 {
            return Err(format!(
                "wf:tree_fast: expected (root, predicate), got {}",
                args.len()
            ));
        }
        let root = args[0].clone();
        let predicate = args[1].clone();

        let mut seen: HashSet<String> = HashSet::new();
        let tree = walk(&root, &predicate, &mut seen);

        Ok(BindingSets {
            vars: vec!["tree".into()],
            rows: vec![vec![Binding {
                name: "tree".into(),
                value: string_literal(&tree.to_string()),
            }]],
        })
    }

    fn aggregate_step(_a: Vec<Value>, _m: u64) -> Result<(), String> {
        Err("wf:tree_fast: aggregate N/A".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf:tree_fast: aggregate N/A".into())
    }
    fn cardinality_estimate(_i: Cardinality, _a: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "wf:tree_fast(root, predicate) — tree walk using v0.3.3 follow-predicate."),
            }]],
        }
    }
}

export!(Component);
