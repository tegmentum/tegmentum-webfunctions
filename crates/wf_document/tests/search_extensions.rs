//! v1.1 tests — cross-index queries, faceting, MoreLikeThis, and vector
//! search. Kept in a dedicated test file so the manticore_client /
//! search_with_body / sirix_client / manticore_admin_client suites stay
//! focused on their respective v0.2 / v1.0 responsibilities.
//!
//! The MLT test exercises the guest's fetch-from-sirix + query-manticore
//! compose path via the same "call the pure functions in order + swap
//! host::http_post_json for a stdlib TcpStream" idiom the
//! search_with_body suite uses.

use serde_json::{json, Value as JsonValue};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use wf_document::manticore::{
    build_facet_body, build_mlt_body, build_request_body, build_vector_body,
    extract_top_terms, parse_facet_response, parse_response, PlainOpts,
};
use wf_document::sirix::{
    build_fetch_body, build_fetch_sql, parse_fetch_response, parse_sirix_uri, query_url,
};

// ---------------------------------------------------------------------------
// Cross-index — verify request body shape
// ---------------------------------------------------------------------------

/// `opts.indexes` populated with multiple names emits a comma-separated
/// `table` field. `index` positional is ignored — the caller's
/// cross-index list wins.
#[test]
fn cross_index_query_builds_multi_index_request() {
    let opts = PlainOpts {
        indexes: vec!["docs".into(), "manuals".into(), "faq".into()],
        limit: Some(20),
        ..PlainOpts::default()
    };
    let body = build_request_body("ignored-when-indexes-set", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    // Multi-index emitted as comma-separated string, in caller order.
    assert_eq!(parsed["table"], json!("docs,manuals,faq"));
    // Query body itself is unchanged — same match + current-only guard.
    assert_eq!(
        parsed["query"]["bool"]["must"],
        json!([{ "match": { "*": "fox" } }])
    );
}

/// Cross-index with a single entry in the list still emits the union
/// shape (comma-separated table with one name) so backend behaviour is
/// uniform.
#[test]
fn cross_index_single_element_still_emits_from_indexes() {
    let opts = PlainOpts {
        indexes: vec!["docs".into()],
        ..PlainOpts::default()
    };
    let body = build_request_body("ignored", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["table"], json!("docs"));
}

/// `opts.indexes` empty falls back to the positional `index`. This is
/// the v1.0 wire shape and existing callers must see identical output.
#[test]
fn cross_index_none_falls_back_to_positional_index() {
    let body =
        build_request_body("docs", "fox", &PlainOpts::default()).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["table"], json!("docs"));
}

/// Whitespace-only or empty entries in `indexes` are dropped so a
/// malformed list doesn't emit a bare comma to Manticore. If every
/// entry is empty, fall back to the positional `index`.
#[test]
fn cross_index_drops_empty_entries() {
    let opts = PlainOpts {
        indexes: vec!["".into(), "docs".into(), "  ".into(), "manuals".into()],
        ..PlainOpts::default()
    };
    let body = build_request_body("fallback", "fox", &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["table"], json!("docs,manuals"));
}

// ---------------------------------------------------------------------------
// Faceting — verify facet field aggregation shape
// ---------------------------------------------------------------------------

/// Faceting emits one `aggs.<field>.terms.field = <field>` clause per
/// requested field, plus `limit: 0` (facets only, no hits).
#[test]
fn facet_request_builds_correct_aggregation() {
    let opts = PlainOpts::default();
    let body = build_facet_body(
        "docs",
        "waterproof",
        &["lang".to_string(), "_uri".to_string()],
        &opts,
    )
    .unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["table"], json!("docs"));
    assert_eq!(parsed["limit"], json!(0));
    // Underlying search clause is the same shape as build_request_body.
    assert_eq!(
        parsed["query"]["bool"]["must"],
        json!([{ "match": { "*": "waterproof" } }])
    );
    // One aggs entry per requested field.
    assert_eq!(
        parsed["aggs"]["lang"],
        json!({ "terms": { "field": "lang", "size": 100 } })
    );
    assert_eq!(
        parsed["aggs"]["_uri"],
        json!({ "terms": { "field": "_uri", "size": 100 } })
    );
}

/// Facets over `opts.indexes` (cross-index) still work — the union
/// selector layers with `aggs` unchanged.
#[test]
fn facet_cross_index_multi_index_and_aggs() {
    let opts = PlainOpts {
        indexes: vec!["docs".into(), "manuals".into()],
        ..PlainOpts::default()
    };
    let body =
        build_facet_body("ignored", "*", &["lang".to_string()], &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["table"], json!("docs,manuals"));
    assert_eq!(
        parsed["aggs"]["lang"],
        json!({ "terms": { "field": "lang", "size": 100 } })
    );
}

/// Empty fields list is an error — a facet with no fields to aggregate
/// over is a nonsense request and better surfaced loudly than silently
/// returned as an empty facet list.
#[test]
fn facet_empty_fields_errors() {
    let opts = PlainOpts::default();
    let err = build_facet_body("docs", "waterproof", &[], &opts).unwrap_err();
    assert!(err.contains("at least one"), "{err}");
}

/// Parse Manticore's aggregations envelope into ordered facet results.
#[test]
fn parse_facet_response_extracts_buckets_by_field() {
    let response = json!({
        "hits": { "total": 0, "hits": [] },
        "aggregations": {
            "lang": {
                "buckets": [
                    { "key": "en", "doc_count": 42 },
                    { "key": "de", "doc_count": 17 },
                ]
            },
            "_uri": {
                "buckets": [
                    { "key": "sirix://docs/manuals/1", "doc_count": 3 },
                ]
            }
        }
    })
    .to_string();
    let facets = parse_facet_response(
        &response,
        &["lang".to_string(), "_uri".to_string()],
    )
    .unwrap();
    assert_eq!(facets.len(), 2);
    assert_eq!(facets[0].field, "lang");
    assert_eq!(facets[0].values.len(), 2);
    assert_eq!(facets[0].values[0].value, "en");
    assert_eq!(facets[0].values[0].count, 42);
    assert_eq!(facets[1].field, "_uri");
    assert_eq!(facets[1].values.len(), 1);
    assert_eq!(facets[1].values[0].value, "sirix://docs/manuals/1");
}

// ---------------------------------------------------------------------------
// MoreLikeThis — Sirix returns doc, MLT extracts terms, Manticore query
// includes them
// ---------------------------------------------------------------------------

/// Given a seed doc body from Sirix, extract terms honors the "> 3 chars"
/// and "top-K by frequency" rules.
#[test]
fn extract_top_terms_filters_short_and_ranks_by_frequency() {
    let body = b"the fox and the fox and the waterproof waterproof rig";
    let terms = extract_top_terms(body, 3);
    // `the`, `and`, `fox`, `rig` all filtered by the length rule (all <= 3
    // except `waterproof` and `fox` which is 3 -> filtered).
    // `waterproof` occurs twice, `fox` filtered (len 3 not > 3).
    // With len > 3: waterproof(2), rig is 3-char and filtered.
    // So we expect [waterproof] as the only term.
    assert!(terms.contains(&"waterproof".to_string()), "terms={terms:?}");
    // The 3-char words must not slip through.
    assert!(!terms.contains(&"fox".to_string()));
    assert!(!terms.contains(&"the".to_string()));
    assert!(!terms.contains(&"and".to_string()));
    assert!(!terms.contains(&"rig".to_string()));
}

/// End-to-end MLT: Sirix mock returns the seed body, the guest's
/// tokenizer extracts terms, and the built MLT body includes them in the
/// match clause and excludes the seed's `_id`.
#[test]
fn more_like_this_extracts_terms_and_queries() {
    // ---- Sirix mock: returns a seed doc body -----------------------
    let sirix_listener = TcpListener::bind("127.0.0.1:0").expect("bind sirix");
    let sirix_addr = sirix_listener.local_addr().unwrap();
    let (s_tx, s_rx) = mpsc::channel::<String>();
    let sirix_thread = thread::spawn(move || {
        let (mut socket, _) = sirix_listener.accept().expect("accept sirix");
        let (headers, body) = read_http_request(&mut socket);
        assert!(headers.starts_with("POST /query "), "sirix headers={headers:?}");
        s_tx.send(String::from_utf8(body).unwrap()).unwrap();
        // Seed body: "waterproof waterproof tarp grommets" —
        // "waterproof" wins the TF race, "tarp" and "grommets" also
        // qualify (both > 3 chars).
        let response_body = json!({
            "columns": ["document"],
            "rows": [["the waterproof waterproof tarp shelter with sturdy grommets"]]
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    // ---- Guest compose (mirror of lib.rs::more_like_this) -----------
    let seed_id = "sirix://docs/manuals/1";
    let seed_doc = parse_sirix_uri(seed_id).unwrap();
    let sirix_url = format!("http://{sirix_addr}");
    let sirix_query_url = query_url(&sirix_url);

    let sql = build_fetch_sql(&seed_doc, None);
    let fetch_body = build_fetch_body(&sql);
    let fetch_response = http_post_via_tcp(&sirix_query_url, &fetch_body).expect("sirix");
    let fetched = parse_fetch_response(&fetch_response, None).unwrap();

    // Verify Sirix saw a fetch for the seed doc. Sirix exposes
    // `_key` as a BIGINT internal node key, not the caller's business
    // key, so identity goes through JSON_VALUE on `$._id`.
    let received_sirix = s_rx.recv().unwrap();
    let sirix_body: JsonValue = serde_json::from_str(&received_sirix).unwrap();
    assert!(
        sirix_body["sql"]
            .as_str()
            .unwrap()
            .contains("JSON_VALUE(\"document\", '$._id') = '1'"),
        "sirix saw sql: {}",
        sirix_body["sql"]
    );

    // Extract terms — should include "waterproof", "tarp", "grommets",
    // "sturdy", "shelter" (all > 3 chars). "waterproof" ranks first
    // (freq 2). "the" and "with" are filtered.
    let terms = extract_top_terms(&fetched.body, 10);
    assert!(!terms.is_empty(), "some terms should have been extracted");
    assert_eq!(
        terms[0], "waterproof",
        "highest-TF term wins the first slot: {terms:?}"
    );
    assert!(
        terms.contains(&"tarp".to_string()),
        "tarp present: {terms:?}"
    );
    assert!(
        terms.contains(&"grommets".to_string()),
        "grommets present: {terms:?}"
    );

    // Build the MLT Manticore body — verify it (a) OR-matches the terms
    // and (b) excludes the seed's `_id`.
    let opts = PlainOpts::default();
    let mlt_body = build_mlt_body("docs", seed_id, &terms, 20, &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&mlt_body).unwrap();
    assert_eq!(parsed["table"], json!("docs"));
    assert_eq!(parsed["limit"], json!(20));
    // The `must` clause is the joined terms as a single match query —
    // Manticore treats each whitespace-separated word as an OR term.
    let must_str = parsed["query"]["bool"]["must"][0]["match"]["*"]
        .as_str()
        .unwrap();
    assert!(
        must_str.contains("waterproof"),
        "must includes seed's top term: {must_str}"
    );
    // The `must_not` clause excludes the seed's `_id`.
    assert_eq!(
        parsed["query"]["bool"]["must_not"][0],
        json!({ "equals": { "_id": "sirix://docs/manuals/1" } })
    );

    sirix_thread.join().unwrap();
}

/// Sanity check: build_mlt_body errors when the terms list is empty —
/// the guest should short-circuit before calling it, but the builder
/// carries a defence-in-depth guard.
#[test]
fn mlt_empty_terms_errors() {
    let opts = PlainOpts::default();
    let err = build_mlt_body("docs", "sirix://x/y/1", &[], 10, &opts).unwrap_err();
    assert!(err.contains("no usable terms"), "{err}");
}

// ---------------------------------------------------------------------------
// Vector search — vector forwarded verbatim
// ---------------------------------------------------------------------------

/// The vector is emitted as a JSON array of numbers under
/// `knn.query_vector` with `field = "vector"` and the caller's `k`.
#[test]
fn vector_search_forwards_vector_verbatim() {
    let vector: Vec<f32> = vec![0.10, 0.25, 0.75, -0.50];
    let opts = PlainOpts::default();
    let body = build_vector_body("products", &vector, 10, &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["table"], json!("products"));
    assert_eq!(parsed["knn"]["field"], json!("vector"));
    assert_eq!(parsed["knn"]["k"], json!(10));
    let qv = parsed["knn"]["query_vector"].as_array().unwrap();
    assert_eq!(qv.len(), 4);
    // Verify each vector component made it onto the wire as-is (float
    // widening f32 -> f64 is expected — components should compare equal
    // to the widened original).
    let expected: Vec<f64> = vector.iter().map(|&f| f as f64).collect();
    let actual: Vec<f64> = qv.iter().map(|v| v.as_f64().unwrap()).collect();
    assert_eq!(actual, expected);
    // limit defaults to k when opts.limit is None.
    assert_eq!(parsed["limit"], json!(10));
}

/// Vector search over `opts.indexes` (cross-index) forwards the union
/// selector to `table` while keeping the k-NN clause unchanged.
#[test]
fn vector_search_cross_index_forwards_indexes() {
    let vector: Vec<f32> = vec![1.0, 0.0];
    let opts = PlainOpts {
        indexes: vec!["docs".into(), "manuals".into()],
        limit: Some(20),
        ..PlainOpts::default()
    };
    let body = build_vector_body("ignored", &vector, 5, &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["table"], json!("docs,manuals"));
    assert_eq!(parsed["knn"]["k"], json!(5));
    // Caller's explicit limit wins over `k`.
    assert_eq!(parsed["limit"], json!(20));
}

/// Empty vector or k=0 error at the guest surface — Manticore would
/// otherwise reject with an opaque backend error.
#[test]
fn vector_search_empty_vector_errors() {
    let opts = PlainOpts::default();
    let empty: Vec<f32> = Vec::new();
    let err = build_vector_body("products", &empty, 10, &opts).unwrap_err();
    assert!(err.contains("non-empty vector"), "{err}");
}

#[test]
fn vector_search_zero_k_errors() {
    let opts = PlainOpts::default();
    let v: Vec<f32> = vec![1.0];
    let err = build_vector_body("products", &v, 0, &opts).unwrap_err();
    assert!(err.contains("k > 0"), "{err}");
}

/// Vector search with `lang` filter layers it as a `knn.filter` sibling
/// — verify that clause reaches the wire under knn.
#[test]
fn vector_search_with_lang_filter_layers_under_knn() {
    let vector: Vec<f32> = vec![0.1];
    let opts = PlainOpts {
        lang: Some("en".into()),
        ..PlainOpts::default()
    };
    let body = build_vector_body("products", &vector, 3, &opts).unwrap();
    let parsed: JsonValue = serde_json::from_str(&body).unwrap();
    let filter = parsed["knn"]["filter"].as_array().unwrap();
    // filter contains lang equals + current-only guard
    assert!(filter.iter().any(|c| c == &json!({ "equals": { "lang": "en" } })));
}

// ---------------------------------------------------------------------------
// Minimal stdlib HTTP client (shared idiom) — mirror of manticore_client.rs
// ---------------------------------------------------------------------------

fn read_http_request(socket: &mut std::net::TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];
    loop {
        let n = socket.read(&mut chunk).expect("read");
        if n == 0 {
            return (String::new(), Vec::new());
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
            return (header_str, body);
        }
    }
}

fn send_ok(socket: &mut std::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {}",
        body.len(),
        body
    );
    socket.write_all(response.as_bytes()).expect("write response");
}

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

// Silence unused warnings for the parse_response import — it's here so
// the search-with-body test scaffolding can be extended in follow-ups
// without another import shuffle.
#[allow(dead_code)]
fn _unused() {
    let _ = parse_response("");
}
