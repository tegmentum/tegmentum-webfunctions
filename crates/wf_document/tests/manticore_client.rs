//! Integration tests for the Manticore adapter — ported verbatim from
//! `wf_fulltext/tests/manticore_client.rs`. The wf_document adapter
//! carries an extra `include_body` / `body_content_type` on `PlainOpts`
//! and adds `body` / `content_type` fields to `Hit`; both are compose-
//! time signals that don't appear on the wire, so the tests here still
//! exercise the same JSON shape.

use serde_json::{json, Value as JsonValue};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use wf_document::manticore::{build_request_body, parse_response, Hit, PlainOpts};

// ---------------------------------------------------------------------------
// build_request_body — per-opts wire-shape checks
// ---------------------------------------------------------------------------

#[test]
fn body_minimal() {
    let body = build_request_body("docs", "fox", &PlainOpts::default()).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(
        parsed,
        json!({
            "table": "docs",
            "query": { "match": { "*": "fox" } },
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
    assert_eq!(
        parsed["query"],
        json!({
            "bool": {
                "must":   [{ "match": { "*": "fox" } }],
                "filter": [{ "equals": { "lang": "en" } }],
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
    assert_eq!(
        parsed["query"]["bool"]["filter"],
        json!([{ "equals": { "category": "book" } }])
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
    assert_eq!(filters.len(), 2);
    assert_eq!(filters[0], json!({ "equals": { "lang": "de" } }));
    assert_eq!(filters[1], json!({ "range": { "price": { "lt": 50 } } }));
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
fn parse_response_leaves_body_and_content_type_none() {
    // Manticore never populates body/content_type — that's the compose
    // step's job. Verify parse_response returns None for both.
    let hits = parse_response(&canned_response()).unwrap();
    assert_eq!(hits[0].body, None);
    assert_eq!(hits[0].content_type, None);
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
