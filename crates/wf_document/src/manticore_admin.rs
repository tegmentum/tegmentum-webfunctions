//! Manticore admin adapter — pure functions for admin request-body
//! construction (bulk insert/replace, delete) and response parsing.
//! Copied verbatim from `wf_fulltext::manticore_admin`; only the error
//! prefix changes to `wf_document` so log lines identify the guest.
//!
//! Mirrors `manticore.rs`'s pattern: pure functions here so tests
//! exercise the wire mapping without instantiating the wit-bindgen
//! `Guest` trait. The guest calls these on either side of a
//! `host::http_post_json` call.
//!
//! Manticore admin protocol — POST `/bulk` NDJSON, one JSON command +
//! one payload per line:
//!
//! ```text
//! POST /bulk
//! Content-Type: application/x-ndjson
//! { "replace": { "index": "manuals", "id": "sirix://docs/manuals/42",
//!                "doc": { "title": "waterproof rig", "lang": "en" } } }
//! ```
//!
//! Delete uses the same NDJSON shape with `{ "delete": { "index": ...,
//! "id": ... } }` per line. Response: `{ "items": [ ... ], "errors":
//! bool }`.

use serde_json::{json, Map, Value as JsonValue};

/// Plain-Rust mirror of the WIT `doc-write` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlainDocWrite {
    pub id: String,
    pub fields: Vec<(String, String)>,
    pub lang: Option<String>,
}

/// Build an NDJSON body for Manticore's `POST /bulk` endpoint from a
/// list of documents to replace/insert. Each doc becomes one `{
/// "replace": { ... } }` line. `replace` (not `insert`) so re-sending
/// the same id is idempotent — the sweep might re-emit unchanged docs.
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

/// Parse Manticore's `/bulk` response. Returns the count of successful
/// items. If `errors: true` the response is turned into an Err whose
/// message names the first failed item.
pub fn parse_bulk_response(json_str: &str) -> Result<u32, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("wf_document: bulk response is not valid JSON: {e}"))?;

    let items = root
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "wf_document: bulk response missing `items` array".to_string()
        })?;

    let errors_flag = root.get("errors").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut ok_count: u32 = 0;
    let mut first_error: Option<String> = None;
    for item in items {
        let (op, payload) = match item.as_object().and_then(|obj| obj.iter().next()) {
            Some((k, v)) => (k.clone(), v),
            None => continue,
        };
        if let Some(err) = payload.get("error") {
            if first_error.is_none() {
                first_error = Some(format!(
                    "wf_document: bulk item `{op}` id={} error: {}",
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
            "wf_document: bulk response signalled errors but items contained none".to_string()
        }));
    }
    Ok(ok_count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_bulk_body_flat_doc() {
        let docs = vec![PlainDocWrite {
            id: "sirix://docs/manuals/1".into(),
            fields: vec![
                ("title".into(), "widget".into()),
                ("desc".into(), "a small widget".into()),
            ],
            lang: Some("en".into()),
        }];
        let body = build_bulk_body("manuals", &docs);
        assert_eq!(body.matches('\n').count(), 1);
        let line = body.trim_end_matches('\n');
        let parsed: JsonValue = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["replace"]["index"], "manuals");
        assert_eq!(parsed["replace"]["id"], "sirix://docs/manuals/1");
        assert_eq!(parsed["replace"]["doc"]["title"], "widget");
        assert_eq!(parsed["replace"]["doc"]["desc"], "a small widget");
        assert_eq!(parsed["replace"]["doc"]["lang"], "en");
    }

    #[test]
    fn build_bulk_body_omits_empty_lang() {
        let docs = vec![PlainDocWrite {
            id: "sirix://docs/manuals/1".into(),
            fields: vec![("title".into(), "x".into())],
            lang: None,
        }];
        let body = build_bulk_body("manuals", &docs);
        let parsed: JsonValue = serde_json::from_str(body.trim_end_matches('\n')).unwrap();
        assert!(!parsed["replace"]["doc"]
            .as_object()
            .unwrap()
            .contains_key("lang"));
    }

    #[test]
    fn build_bulk_body_empty_when_no_docs() {
        let body = build_bulk_body("manuals", &[]);
        assert!(body.is_empty());
    }

    #[test]
    fn build_delete_body_one_line_per_id() {
        let ids = vec![
            "sirix://docs/manuals/1".to_string(),
            "sirix://docs/manuals/2".to_string(),
        ];
        let body = build_delete_body("manuals", &ids);
        assert_eq!(body.matches('\n').count(), 2);
        let mut lines = body.trim_end_matches('\n').lines();
        let first: JsonValue = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first["delete"]["index"], "manuals");
        assert_eq!(first["delete"]["id"], "sirix://docs/manuals/1");
        let second: JsonValue = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(second["delete"]["id"], "sirix://docs/manuals/2");
    }

    #[test]
    fn parse_bulk_response_counts_successes() {
        let body = json!({
            "items": [
                { "replace": { "_id": "sirix://docs/manuals/1", "result": "created" } },
                { "replace": { "_id": "sirix://docs/manuals/2", "result": "updated" } }
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
                { "replace": { "_id": "sirix://docs/manuals/1", "result": "created" } },
                { "replace": { "_id": "sirix://docs/manuals/2", "error": "schema mismatch" } }
            ],
            "errors": true
        })
        .to_string();
        let err = parse_bulk_response(&body).unwrap_err();
        assert!(err.contains("sirix://docs/manuals/2") && err.contains("schema mismatch"), "err: {err}");
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
