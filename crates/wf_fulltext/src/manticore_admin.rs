//! Manticore admin adapter — pure functions for admin request-body
//! construction (bulk insert/replace, delete) and response parsing.
//!
//! Mirrors `manticore.rs`'s pattern: pure functions here so tests exercise
//! the wire mapping without instantiating the wit-bindgen `Guest` trait.
//! The guest calls these on either side of a `host::http_post_json` call.
//!
//! Manticore admin protocol reference — v0.1 assumes the NDJSON bulk API
//! Manticore borrowed from the Elastic family (POST `/bulk`, one JSON
//! command + one payload per line). If Manticore's actual endpoint shape
//! differs from this assumption in your deployment, adjust
//! `build_bulk_body` / `build_delete_body` and keep the guest surface
//! stable. The wire shape used here (each line NDJSON):
//!
//! ```text
//! POST /bulk
//! Content-Type: application/x-ndjson
//! { "replace": { "index": "products", "id": "urn:s:1",
//!                "doc": { "label": "widget", "lang": "en" } } }
//! { "replace": { "index": "products", "id": "urn:s:2", ... } }
//! ```
//!
//! Delete uses the same NDJSON shape with `{ "delete": { "index": ...,
//! "id": ... } }` per line. Manticore's response is a single JSON object
//! `{ "items": [ { "replace": { "_id": "...", "result": "created" } }, ...
//! ], "errors": bool }`. The parser counts successful items — anything
//! that came back as an error string bumps the error surface, and the
//! guest returns Err on any errors: true response.

use serde_json::{json, Map, Value as JsonValue};

/// Plain-Rust mirror of the WIT `doc-write` record. The guest converts
/// the WIT record into this shape before calling `build_bulk_body` so
/// (a) the mapping code is pure and (b) tests don't need to link
/// against the WIT bindings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlainDocWrite {
    pub id: String,
    pub fields: Vec<(String, String)>,
    pub lang: Option<String>,
}

/// Build an NDJSON body for Manticore's `POST /bulk` endpoint from a
/// list of documents to replace/insert.
///
/// Each doc becomes one `{ "replace": { "index": "<index>", "id": "<id>",
/// "doc": { <fields>, "lang": "<lang>" } } }` line. `replace` (not
/// `insert`) is used so re-sending the same id is idempotent — the sweep
/// might re-emit unchanged docs, and Manticore's `insert` would fail on
/// duplicates.
///
/// Returns an empty string when `docs` is empty (the caller can then
/// skip the HTTP round-trip entirely; Manticore accepts an empty body
/// but there's no reason to hit the wire).
pub fn build_bulk_body(index: &str, docs: &[PlainDocWrite]) -> String {
    let mut out = String::new();
    for doc in docs {
        let mut doc_obj = Map::new();
        for (k, v) in &doc.fields {
            doc_obj.insert(k.clone(), JsonValue::String(v.clone()));
        }
        if let Some(lang) = &doc.lang {
            if !lang.is_empty() {
                doc_obj.insert("lang".into(), JsonValue::String(lang.clone()));
            }
        }
        let line = json!({
            "replace": {
                "index": index,
                "id": doc.id,
                "doc": JsonValue::Object(doc_obj),
            }
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// Build an NDJSON body for a batch delete. Same NDJSON shape as
/// `build_bulk_body`, one `{ "delete": { ... } }` line per id.
pub fn build_delete_body(index: &str, ids: &[String]) -> String {
    let mut out = String::new();
    for id in ids {
        let line = json!({
            "delete": {
                "index": index,
                "id": id,
            }
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// Parse Manticore's `/bulk` response.
///
/// Expected shape:
///   { "items": [ { "replace": { "_id": "...", "result": "..." } }, ... ],
///     "errors": bool }
///
/// Returns the count of successful items. If `errors: true` the response
/// is turned into an Err whose message names the first failed item.
pub fn parse_bulk_response(json_str: &str) -> Result<u32, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("wf_fulltext: bulk response is not valid JSON: {e}"))?;

    let items = root
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "wf_fulltext: bulk response missing `items` array".to_string()
        })?;

    let errors_flag = root.get("errors").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut ok_count: u32 = 0;
    let mut first_error: Option<String> = None;
    for item in items {
        // Each item is a map with a single key ("replace" | "delete" |
        // "insert" | "create") whose value carries `_id`, `result` and
        // possibly `error`. Walk any key.
        let (op, payload) = match item.as_object().and_then(|obj| obj.iter().next()) {
            Some((k, v)) => (k.clone(), v),
            None => continue,
        };
        if let Some(err) = payload.get("error") {
            if first_error.is_none() {
                first_error = Some(format!(
                    "wf_fulltext: bulk item `{op}` id={} error: {}",
                    payload
                        .get("_id")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "<unknown>".into()),
                    err
                ));
            }
        } else {
            ok_count += 1;
        }
    }

    if errors_flag {
        return Err(first_error.unwrap_or_else(|| {
            "wf_fulltext: bulk response signalled errors but items contained none".to_string()
        }));
    }
    Ok(ok_count)
}

// ---------------------------------------------------------------------------
// Tests — pure-function checks. Wire-level test lives in tests/.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_bulk_body_flat_doc() {
        let docs = vec![PlainDocWrite {
            id: "urn:s:1".into(),
            fields: vec![
                ("label".into(), "widget".into()),
                ("desc".into(), "a small widget".into()),
            ],
            lang: Some("en".into()),
        }];
        let body = build_bulk_body("products", &docs);
        // One line per doc, terminated with \n.
        assert_eq!(body.matches('\n').count(), 1);
        let line = body.trim_end_matches('\n');
        let parsed: JsonValue = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["replace"]["index"], "products");
        assert_eq!(parsed["replace"]["id"], "urn:s:1");
        assert_eq!(parsed["replace"]["doc"]["label"], "widget");
        assert_eq!(parsed["replace"]["doc"]["desc"], "a small widget");
        assert_eq!(parsed["replace"]["doc"]["lang"], "en");
    }

    #[test]
    fn build_bulk_body_omits_empty_lang() {
        let docs = vec![PlainDocWrite {
            id: "urn:s:1".into(),
            fields: vec![("label".into(), "x".into())],
            lang: None,
        }];
        let body = build_bulk_body("products", &docs);
        let parsed: JsonValue = serde_json::from_str(body.trim_end_matches('\n')).unwrap();
        assert!(!parsed["replace"]["doc"]
            .as_object()
            .unwrap()
            .contains_key("lang"));
    }

    #[test]
    fn build_bulk_body_empty_when_no_docs() {
        let body = build_bulk_body("products", &[]);
        assert!(body.is_empty());
    }

    #[test]
    fn build_delete_body_one_line_per_id() {
        let ids = vec!["urn:s:1".to_string(), "urn:s:2".to_string()];
        let body = build_delete_body("products", &ids);
        assert_eq!(body.matches('\n').count(), 2);
        let mut lines = body.trim_end_matches('\n').lines();
        let first: JsonValue = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first["delete"]["index"], "products");
        assert_eq!(first["delete"]["id"], "urn:s:1");
        let second: JsonValue = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(second["delete"]["id"], "urn:s:2");
    }

    #[test]
    fn parse_bulk_response_counts_successes() {
        let body = json!({
            "items": [
                { "replace": { "_id": "urn:s:1", "result": "created" } },
                { "replace": { "_id": "urn:s:2", "result": "updated" } }
            ],
            "errors": false
        })
        .to_string();
        let n = parse_bulk_response(&body).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn parse_bulk_response_errors_true_surfaces_first_error() {
        let body = json!({
            "items": [
                { "replace": { "_id": "urn:s:1", "result": "created" } },
                { "replace": { "_id": "urn:s:2", "error": "schema mismatch" } }
            ],
            "errors": true
        })
        .to_string();
        let err = parse_bulk_response(&body).unwrap_err();
        assert!(err.contains("urn:s:2") && err.contains("schema mismatch"), "err: {err}");
    }

    #[test]
    fn parse_bulk_response_missing_items_errors() {
        let err = parse_bulk_response(r#"{"errors":false}"#).unwrap_err();
        assert!(err.contains("missing"));
    }

    #[test]
    fn parse_bulk_response_invalid_json_errors() {
        let err = parse_bulk_response("not-json").unwrap_err();
        assert!(err.contains("valid JSON"));
    }
}
