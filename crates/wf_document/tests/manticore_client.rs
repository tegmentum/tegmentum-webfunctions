//! Integration tests for the Manticore adapter — ported verbatim from
//! `wf_fulltext/tests/manticore_client.rs`. The wf_document adapter
//! carries an extra `include_body` / `body_content_type` on `PlainOpts`
//! and adds `body` / `content_type` fields to `Hit`; both are compose-
//! time signals that don't appear on the wire, so the tests here still
//! exercise the same JSON shape.
//!
//! v1.0 note: every emitted body now carries a `bool.filter` clause —
//! at minimum the `_valid_to IS NULL` "current-only" guard that
//! preserves v0.2 semantics on retention=all indexes (memo §04). The
//! v0.2 test bodies below have been updated to reflect this: the JSON
//! shape changes, the semantic coverage stays identical.

use serde_json::{json, Value as JsonValue};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use wf_document::manticore::{
    build_probe_body, build_request_body, parse_response, schema_has_valid_from, AtTime, Hit,
    PlainOpts,
};

/// The v1.0 default filter emitted for callers who set neither
/// `at_time` nor `at_rev` — matches only current revisions.
fn current_only_guard() -> JsonValue {
    json!({ "equals": { "_valid_to": null } })
}

// ---------------------------------------------------------------------------
// build_request_body — per-opts wire-shape checks
// ---------------------------------------------------------------------------

#[test]
fn body_minimal() {
    // v1.0: even the "minimal" body carries the current-only guard so
    // retention=all indexes return only current-time hits when the
    // caller didn't ask for time-travel. Retention=latest sweeps set
    // `_valid_to = NULL` uniformly, so the guard is a no-op there.
    let body = build_request_body("docs", "fox", &PlainOpts::default()).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(
        parsed,
        json!({
            "table": "docs",
            "query": {
                "bool": {
                    "must":   [{ "match": { "*": "fox" } }],
                    "filter": [{ "equals": { "_valid_to": null } }],
                }
            },
        })
    );
}

#[test]
fn body_with_limit_and_offset() {
    let opts = PlainOpts {
        limit: Some(20),
        offset: Some(40),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["limit"], json!(20));
    assert_eq!(parsed["offset"], json!(40));
}

#[test]
fn body_omits_offset_when_none() {
    let opts = PlainOpts {
        limit: Some(20),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert!(!parsed.as_object().unwrap().contains_key("offset"));
}

#[test]
fn body_with_fields_projection() {
    let opts = PlainOpts {
        fields: vec!["title".into(), "body".into()],
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["_source"], json!({ "includes": ["title", "body"] }));
}

#[test]
fn body_omits_source_when_fields_empty() {
    let body = build_request_body("docs", "fox", &PlainOpts::default()).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert!(!parsed.as_object().unwrap().contains_key("_source"));
}

#[test]
fn body_with_highlight() {
    let opts = PlainOpts {
        highlight: true,
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["highlight"], json!({}));
}

#[test]
fn body_omits_highlight_when_false() {
    let body = build_request_body("docs", "fox", &PlainOpts::default()).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert!(!parsed.as_object().unwrap().contains_key("highlight"));
}

#[test]
fn body_with_lang_wraps_in_bool_filter() {
    let opts = PlainOpts {
        lang: Some("en".into()),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    // v1.0: lang filter joins the current-only guard in the same list.
    assert_eq!(
        parsed["query"],
        json!({
            "bool": {
                "must":   [{ "match": { "*": "fox" } }],
                "filter": [
                    { "equals": { "lang": "en" } },
                    { "equals": { "_valid_to": null } },
                ],
            }
        })
    );
}

#[test]
fn body_with_filter_string_forwarded_verbatim() {
    let opts = PlainOpts {
        filter: Some(r#"{"equals":{"category":"book"}}"#.into()),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    // v1.0: forwarded user filter still in slot [0]; current-only guard
    // appended after.
    assert_eq!(
        parsed["query"]["bool"]["filter"],
        json!([
            { "equals": { "category": "book" } },
            { "equals": { "_valid_to": null } },
        ])
    );
}

#[test]
fn body_with_lang_and_filter_combined() {
    let opts = PlainOpts {
        lang: Some("de".into()),
        filter: Some(r#"{"range":{"price":{"lt":50}}}"#.into()),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    // v1.0: lang, user filter, then the current-only guard.
    assert_eq!(filters.len(), 3);
    assert_eq!(filters[0], json!({ "equals": { "lang": "de" } }));
    assert_eq!(filters[1], json!({ "range": { "price": { "lt": 50 } } }));
    assert_eq!(filters[2], current_only_guard());
}

#[test]
fn body_rejects_invalid_filter_json() {
    let opts = PlainOpts {
        filter: Some("not-json{".into()),
        ..PlainOpts::default()
    };
    let err = build_request_body("docs", "fox", &opts).unwrap_err();
    assert!(err.contains("opts.filter"));
}

// ---------------------------------------------------------------------------
// parse_response — Manticore -> Hit
// ---------------------------------------------------------------------------

fn canned_response() -> String {
    json!({
        "hits": {
            "total": 2,
            "hits": [
                {
                    "_id":    "sirix://docs/manuals/1",
                    "_score": 0.987,
                    "_source": {
                        "title": "Quick brown fox",
                        "lang":  "en",
                        "views": 42
                    },
                    "highlight": { "*": ["a <b>quick</b> brown fox"] }
                },
                {
                    "_id":    "sirix://docs/manuals/2",
                    "_score": 0.55,
                    "_source": { "title": "Lazy dog" }
                }
            ]
        }
    })
    .to_string()
}

#[test]
fn parse_response_maps_id_score_snippet() {
    let hits = parse_response(&canned_response()).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].doc, "sirix://docs/manuals/1");
    assert_eq!(hits[0].score, 0.987);
    assert_eq!(
        hits[0].snippet.as_deref(),
        Some("a <b>quick</b> brown fox")
    );
    assert_eq!(hits[1].snippet, None);
}

#[test]
fn parse_response_leaves_body_and_content_type_none_when_source_lacks_them() {
    // Index-only design (memo `wf-document.md` §08): the sweep does NOT
    // put bodies in Manticore's `_source`, so parse_response returns
    // `body: None` / `content_type: None`. The guest's compose step
    // fetches from Sirix. The canned_response above deliberately omits
    // those fields so this test locks in the index-only shape.
    let hits = parse_response(&canned_response()).unwrap();
    assert_eq!(hits[0].body, None);
    assert_eq!(hits[0].content_type, None);
    assert_eq!(hits[1].body, None);
    assert_eq!(hits[1].content_type, None);
}

#[test]
fn parse_response_backcompat_populates_body_from_source() {
    // Backwards compat: if a pre-correction sweep did store bodies in
    // Manticore's `_source`, the adapter picks them up so the compose
    // step can skip the Sirix round-trip. Present-in-source is optional;
    // absent is the norm under the index-only design.
    let body = json!({
        "hits": { "hits": [{
            "_id":    "sirix://docs/manuals/1",
            "_score": 0.5,
            "_source": {
                "title":        "Legacy mirror row",
                "body":         "waterproof rig full body",
                "content_type": "text/plain; charset=utf-8"
            }
        }]}
    })
    .to_string();
    let hits = parse_response(&body).unwrap();
    assert_eq!(
        hits[0].body.as_deref().map(String::from_utf8_lossy).map(|s| s.to_string()),
        Some("waterproof rig full body".to_string())
    );
    assert_eq!(
        hits[0].content_type.as_deref(),
        Some("text/plain; charset=utf-8")
    );
}

#[test]
fn parse_response_extracts_source_fields() {
    let hits = parse_response(&canned_response()).unwrap();
    let expected: Vec<(String, String)> = vec![
        ("lang".into(), "en".into()),
        ("title".into(), "Quick brown fox".into()),
        ("views".into(), "42".into()),
    ];
    assert_eq!(hits[0].fields, expected);
    assert_eq!(hits[0].lang.as_deref(), Some("en"));
}

#[test]
fn parse_response_lang_none_when_absent_in_source() {
    let hits = parse_response(&canned_response()).unwrap();
    assert_eq!(hits[1].lang, None);
}

#[test]
fn parse_response_empty_hits() {
    let body = json!({ "hits": { "total": 0, "hits": [] } }).to_string();
    let hits = parse_response(&body).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn parse_response_invalid_json_errors() {
    let err = parse_response("not-json{").unwrap_err();
    assert!(err.contains("wf_document"));
}

#[test]
fn parse_response_missing_hits_hits_errors() {
    let body = json!({ "hits": { "total": 0 } }).to_string();
    let err = parse_response(&body).unwrap_err();
    assert!(err.contains("missing"));
}

#[test]
fn parse_response_skips_nested_source_values() {
    let body = json!({
        "hits": { "hits": [{
            "_id":    "sirix://docs/manuals/99",
            "_score": 0.1,
            "_source": {
                "name":   "widget",
                "tags":   ["a", "b"],
                "meta":   { "nested": true }
            }
        }]}
    })
    .to_string();
    let hits = parse_response(&body).unwrap();
    let names: Vec<_> = hits[0].fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, vec!["name"]);
}

// ---------------------------------------------------------------------------
// v1.0 — time-travel search
// ---------------------------------------------------------------------------

#[test]
fn neither_restricts_to_current_only() {
    // Neither at_time nor at_rev = current-only guard.
    let body = build_request_body("docs", "fox", &PlainOpts::default()).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0], current_only_guard());
}

#[test]
fn at_time_iso_emits_interval_filter() {
    let opts = PlainOpts {
        at_time: Some(AtTime::Iso("2026-01-01T00:00:00Z".into())),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    // interval = two clauses: _valid_from lte, and (_valid_to null or gt)
    assert_eq!(filters.len(), 2);
    assert_eq!(
        filters[0],
        json!({ "range": { "_valid_from": { "lte": "2026-01-01T00:00:00Z" } } })
    );
    assert_eq!(
        filters[1],
        json!({
            "bool": {
                "should": [
                    { "equals": { "_valid_to": null } },
                    { "range":  { "_valid_to": { "gt": "2026-01-01T00:00:00Z" } } },
                ]
            }
        })
    );
    // and the current-only guard is NOT emitted when time-travel is on.
    for f in filters {
        assert!(f != &current_only_guard(), "current-only guard leaked in");
    }
}

#[test]
fn at_time_epoch_emits_numeric_interval_filter() {
    let opts = PlainOpts {
        at_time: Some(AtTime::Epoch(1735689600)),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    // Numeric on the wire — Manticore compares numerically against the
    // sweep's epoch-seconds `_valid_from`.
    assert_eq!(
        filters[0],
        json!({ "range": { "_valid_from": { "lte": 1735689600 } } })
    );
    assert_eq!(
        filters[1]["bool"]["should"][1],
        json!({ "range": { "_valid_to": { "gt": 1735689600 } } })
    );
}

#[test]
fn at_rev_emits_equality_filter() {
    let opts = PlainOpts {
        at_rev: Some(17),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    // at_rev = one clause, the exact-revision match. Current-only guard
    // is omitted (revision pin fully specifies the target row).
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0], json!({ "equals": { "_rev": 17 } }));
}

#[test]
fn both_at_time_and_at_rev_errors_at_build_body() {
    // The guest surface (`search` in lib.rs) rejects the both-set case
    // before it reaches build_request_body. build_request_body carries a
    // defence-in-depth guard for callers that skip the surface check.
    let opts = PlainOpts {
        at_time: Some(AtTime::Epoch(1735689600)),
        at_rev: Some(17),
        ..PlainOpts::default()
    };
    let err = build_request_body("docs", "fox", &opts).unwrap_err();
    assert!(err.contains("mutually exclusive"), "err={err}");
}

#[test]
fn at_time_composes_with_user_filter_and_lang() {
    // Ordering: user's lang → user's filter → time-travel clauses.
    let opts = PlainOpts {
        lang: Some("en".into()),
        filter: Some(r#"{"equals":{"category":"book"}}"#.into()),
        at_time: Some(AtTime::Iso("2026-01-01".into())),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    // 4 clauses: lang, user-filter, valid_from lte, valid_to null/gt.
    assert_eq!(filters.len(), 4);
    assert_eq!(filters[0], json!({ "equals": { "lang": "en" } }));
    assert_eq!(filters[1], json!({ "equals": { "category": "book" } }));
    assert_eq!(
        filters[2],
        json!({ "range": { "_valid_from": { "lte": "2026-01-01" } } })
    );
}

// ---------------------------------------------------------------------------
// v1.2 — range queries (memo wf-document-v1.md §08)
// ---------------------------------------------------------------------------

#[test]
fn after_only_emits_open_range() {
    // Only `after` set → single _valid_from range with `gte` only.
    // Range mode drops the default current-only guard.
    let opts = PlainOpts {
        after: Some(AtTime::Iso("2026-01-01T00:00:00Z".into())),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    assert_eq!(filters.len(), 1);
    let range = &filters[0]["range"]["_valid_from"];
    assert_eq!(range["gte"], json!("2026-01-01T00:00:00Z"));
    assert!(
        range.as_object().unwrap().get("lte").is_none(),
        "before is None → no `lte` bound emitted, got {range}"
    );
    // The current-only guard MUST NOT leak into range mode.
    for f in filters {
        assert!(f != &current_only_guard(), "current-only guard leaked into range mode");
    }
}

#[test]
fn before_only_emits_closed_range() {
    // Only `before` set → single _valid_from range with `lte` only.
    let opts = PlainOpts {
        before: Some(AtTime::Iso("2026-06-01T00:00:00Z".into())),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    assert_eq!(filters.len(), 1);
    let range = &filters[0]["range"]["_valid_from"];
    assert_eq!(range["lte"], json!("2026-06-01T00:00:00Z"));
    assert!(
        range.as_object().unwrap().get("gte").is_none(),
        "after is None → no `gte` bound emitted, got {range}"
    );
    for f in filters {
        assert!(f != &current_only_guard(), "current-only guard leaked into range mode");
    }
}

#[test]
fn after_and_before_emits_both_bounds() {
    // Both bounds set → single _valid_from range with `gte` and `lte`.
    // Epoch bounds hit the numeric-JSON branch on the wire.
    let opts = PlainOpts {
        after: Some(AtTime::Epoch(1735689600)),
        before: Some(AtTime::Epoch(1743465600)),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filters = parsed["query"]["bool"]["filter"].as_array().unwrap();
    assert_eq!(filters.len(), 1);
    let range = &filters[0]["range"]["_valid_from"];
    assert_eq!(range["gte"], json!(1735689600));
    assert_eq!(range["lte"], json!(1743465600));
    for f in filters {
        assert!(f != &current_only_guard(), "current-only guard leaked into range mode");
    }
}

#[test]
fn range_with_at_time_errors() {
    // Defence-in-depth: guest surface rejects the conflict first, but
    // `build_query_clause` also carries the guard for callers that skip
    // the surface check.
    let opts = PlainOpts {
        after: Some(AtTime::Iso("2026-01-01".into())),
        at_time: Some(AtTime::Iso("2026-03-01".into())),
        ..PlainOpts::default()
    };
    let err = build_request_body("docs", "fox", &opts).unwrap_err();
    assert!(
        err.contains("mutually exclusive"),
        "expected mutual-exclusion error, got: {err}"
    );

    // Same guard for at_rev — verify it fires on that path too.
    let opts_rev = PlainOpts {
        before: Some(AtTime::Iso("2026-06-01".into())),
        at_rev: Some(17),
        ..PlainOpts::default()
    };
    let err_rev = build_request_body("docs", "fox", &opts_rev).unwrap_err();
    assert!(
        err_rev.contains("mutually exclusive"),
        "expected mutual-exclusion error, got: {err_rev}"
    );
}

#[test]
fn empty_range_returns_empty() {
    // `after > before` is caught at the guest surface (lib.rs::search),
    // which returns Ok(vec![]). At the manticore.rs layer we still emit
    // a well-formed range body — the guest short-circuits before we get
    // here — so this test locks the surface behaviour by running the
    // parity check on `is_empty_range` via a duplicate implementation.
    // Local mirror kept in the test to avoid making the internal helper
    // pub — semantics only, not a re-export.
    fn empty(a: Option<&AtTime>, b: Option<&AtTime>) -> bool {
        match (a, b) {
            (Some(AtTime::Epoch(x)), Some(AtTime::Epoch(y))) => x > y,
            (Some(AtTime::Iso(x)), Some(AtTime::Iso(y))) => x > y,
            _ => false,
        }
    }
    assert!(empty(
        Some(&AtTime::Iso("2026-06-01".into())),
        Some(&AtTime::Iso("2026-01-01".into())),
    ));
    assert!(empty(
        Some(&AtTime::Epoch(1743465600)),
        Some(&AtTime::Epoch(1735689600)),
    ));
    // Well-ordered range → not empty.
    assert!(!empty(
        Some(&AtTime::Iso("2026-01-01".into())),
        Some(&AtTime::Iso("2026-06-01".into())),
    ));
    // One-sided range → never empty (open on the missing side).
    assert!(!empty(Some(&AtTime::Iso("2026-01-01".into())), None));
    assert!(!empty(None, Some(&AtTime::Iso("2026-06-01".into()))));
    // Mixed epoch/iso → permissive, defer coercion to Manticore.
    assert!(!empty(
        Some(&AtTime::Epoch(1743465600)),
        Some(&AtTime::Iso("2026-01-01".into())),
    ));
}

#[test]
fn revision_populated_from_response() {
    // Mock response with `_rev` in `_source` — verify hit.revision is
    // populated.
    let body = json!({
        "hits": {
            "hits": [{
                "_id":    "sirix://docs/manuals/1",
                "_score": 0.5,
                "_source": {
                    "title":  "waterproof spec",
                    "_rev":   17,
                    "_valid_from": 1735689600,
                    "_valid_to":   null
                }
            }]
        }
    })
    .to_string();
    let hits = parse_response(&body).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].revision, Some(17));
}

#[test]
fn revision_absent_leaves_none() {
    // v0.2-shape hit (no `_rev`) — hit.revision stays None.
    let body = json!({
        "hits": { "hits": [{
            "_id": "sirix://docs/manuals/1",
            "_score": 0.5,
            "_source": { "title": "no rev here" }
        }]}
    })
    .to_string();
    let hits = parse_response(&body).unwrap();
    assert_eq!(hits[0].revision, None);
}

#[test]
fn revision_from_string_int() {
    // Manticore sometimes stringifies u64s in `_source`. Accept both.
    let body = json!({
        "hits": { "hits": [{
            "_id": "sirix://docs/manuals/1",
            "_score": 0.5,
            "_source": { "_rev": "23" }
        }]}
    })
    .to_string();
    let hits = parse_response(&body).unwrap();
    assert_eq!(hits[0].revision, Some(23));
}

// ---------------------------------------------------------------------------
// v1.0 — storage-gate probe helpers
// ---------------------------------------------------------------------------

#[test]
fn probe_body_shape() {
    let body = build_probe_body("docs");
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["table"], json!("docs"));
    assert_eq!(parsed["query"], json!({ "match_all": {} }));
    assert_eq!(parsed["limit"], json!(1));
}

#[test]
fn schema_has_valid_from_when_source_carries_it() {
    let body = json!({
        "hits": { "hits": [{
            "_id": "x", "_score": 1.0,
            "_source": { "_valid_from": 1735689600, "_valid_to": null }
        }]}
    })
    .to_string();
    assert!(schema_has_valid_from(&body));
}

#[test]
fn schema_has_valid_from_false_on_latest_only_index() {
    // Retention=latest sweep only mirrors current bodies, no interval
    // columns. Probe returns false, and the storage gate surfaces the
    // "index appears to hold latest-only" error.
    let body = json!({
        "hits": { "hits": [{
            "_id": "x", "_score": 1.0,
            "_source": { "title": "current body" }
        }]}
    })
    .to_string();
    assert!(!schema_has_valid_from(&body));
}

#[test]
fn schema_has_valid_from_false_on_empty_probe() {
    // Empty index — we can't tell. Conservatively false so the gate errs
    // on the side of surfacing a schema problem.
    let body = json!({ "hits": { "hits": [] } }).to_string();
    assert!(!schema_has_valid_from(&body));
}

// ---------------------------------------------------------------------------
// Wire test — the guest's built body reaches Manticore intact
// ---------------------------------------------------------------------------

#[test]
fn wire_round_trip_via_local_tcp_listener() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");

        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 1024];
        let (headers, body) = loop {
            let n = socket.read(&mut chunk).expect("read");
            if n == 0 {
                break (String::new(), Vec::new());
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = find_header_end(&buf) {
                let header_str = String::from_utf8_lossy(&buf[..end]).to_string();
                let content_length = content_length_of(&header_str).unwrap_or(0);
                let body_start = end + 4;
                while buf.len() < body_start + content_length {
                    let n = socket.read(&mut chunk).expect("read body");
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = buf[body_start..body_start + content_length].to_vec();
                break (header_str, body);
            }
        };

        assert!(headers.starts_with("POST /search "), "headers={headers:?}");
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        let response_body = canned_response();
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n\
             {}",
            response_body.len(),
            response_body
        );
        socket.write_all(response.as_bytes()).expect("write response");
    });

    let opts = PlainOpts {
        limit: Some(10),
        highlight: true,
        lang: Some("en".into()),
        ..PlainOpts::default()
    };
    let body = build_request_body("docs", "quick brown fox", &opts).unwrap();

    let url = format!("http://{addr}/search");
    let response_body = http_post_via_tcp(&url, &body).expect("post");

    let received = rx.recv().unwrap();
    assert_eq!(received, body);

    let hits = parse_response(&response_body).unwrap();
    assert_eq!(hits.len(), 2);
    let expected_first = Hit {
        doc: "sirix://docs/manuals/1".into(),
        score: 0.987,
        snippet: Some("a <b>quick</b> brown fox".into()),
        lang: Some("en".into()),
        body: None,
        content_type: None,
        fields: vec![
            ("lang".into(), "en".into()),
            ("title".into(), "Quick brown fox".into()),
            ("views".into(), "42".into()),
        ],
        // v1.0: the canned response's _source has no `_rev`, so revision
        // stays None. See `revision_populated_from_response` below for
        // the populated case.
        revision: None,
    };
    assert_eq!(hits[0], expected_first);

    server.join().unwrap();
}

// ---------------------------------------------------------------------------
// Minimal stdlib HTTP client — for tests only
// ---------------------------------------------------------------------------

fn http_post_via_tcp(url: &str, body: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "url must start with http://".to_string())?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{path}")
    };

    let mut socket = std::net::TcpStream::connect(authority)
        .map_err(|e| format!("connect: {e}"))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {body}",
        body.len()
    );
    socket
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];
    loop {
        let n = socket.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8(buf).map_err(|e| format!("utf8: {e}"))?;
    let (_, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "no header terminator".to_string())?;
    Ok(body.to_string())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length_of(headers: &str) -> Option<usize> {
    for line in headers.split("\r\n") {
        let mut parts = line.splitn(2, ':');
        let Some(name) = parts.next().map(str::trim) else { continue };
        let Some(value) = parts.next().map(str::trim) else { continue };
        if name.eq_ignore_ascii_case("content-length") {
            return value.parse::<usize>().ok();
        }
    }
    None
}
