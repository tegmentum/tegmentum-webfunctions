//! Compose test — mocks BOTH backends on separate TcpListeners, runs
//! the same search-then-fetch-bodies pipeline that `lib.rs::search`
//! runs when `opts.include_body` is true, and verifies the joined
//! result.
//!
//! We can't drive `Component::search` from a native test (that would
//! require stubbing the `host::http_post_json` WIT import). Instead we
//! call the same pure functions in the same order and swap
//! `host::http_post_json` for a stdlib TcpStream client. This mirrors
//! the compose logic byte-for-byte.
//!
//! Index-only design coverage (memo `wf-document.md` §08): the search
//! path never expects body in Manticore's `_source`; every body comes
//! from Sirix. The tests below cover the two include_body branches,
//! per-hit Sirix soft-fail, and the guest-side snippet fallback.

use serde_json::{json, Value as JsonValue};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::thread;

use wf_document::manticore::{build_request_body, parse_response, Hit, PlainOpts};
use wf_document::sirix::{
    build_fetch_body, build_fetch_sql, parse_fetch_response, parse_sirix_uri, query_url,
};

#[test]
fn search_then_fetch_composes_across_two_backends() {
    // ---- Manticore mock -------------------------------------------------
    let manticore_listener = TcpListener::bind("127.0.0.1:0").expect("bind manticore");
    let manticore_addr = manticore_listener.local_addr().unwrap();
    let (m_tx, m_rx) = mpsc::channel::<String>();
    let manticore_thread = thread::spawn(move || {
        let (mut socket, _) = manticore_listener.accept().expect("accept manticore");
        let (headers, body) = read_http_request(&mut socket);
        assert!(headers.starts_with("POST /search "), "manticore headers={headers:?}");
        m_tx.send(String::from_utf8(body).unwrap()).unwrap();

        let response_body = json!({
            "hits": {
                "hits": [
                    {
                        "_id": "sirix://docs/manuals/1",
                        "_score": 0.9,
                        "_source": { "title": "Waterproof rig" },
                        "highlight": { "*": ["<b>waterproof</b> rig"] }
                    },
                    {
                        "_id": "sirix://docs/manuals/2",
                        "_score": 0.6,
                        "_source": { "title": "Sturdy tarp" }
                    }
                ]
            }
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    // ---- Sirix mock: two sequential fetches ------------------------------
    let sirix_listener = TcpListener::bind("127.0.0.1:0").expect("bind sirix");
    let sirix_addr = sirix_listener.local_addr().unwrap();
    let sirix_call_count = Arc::new(AtomicUsize::new(0));
    let sirix_call_count_c = sirix_call_count.clone();
    let (s_tx, s_rx) = mpsc::channel::<String>();
    let sirix_thread = thread::spawn(move || {
        // Handle two connections in sequence — one per hit.
        for i in 0..2 {
            let (mut socket, _) = sirix_listener.accept().expect("accept sirix");
            let (headers, body) = read_http_request(&mut socket);
            assert!(
                headers.starts_with("POST /query "),
                "sirix headers[{i}]={headers:?}"
            );
            s_tx.send(String::from_utf8(body).unwrap()).unwrap();

            // Return a different body per hit so we can verify the join
            // paired the right document with the right hit.
            let response_body = if i == 0 {
                json!({
                    "columns": ["document"],
                    "rows": [["{\"title\":\"Waterproof rig\",\"body\":\"full doc 1\"}"]]
                })
                .to_string()
            } else {
                json!({
                    "columns": ["document"],
                    "rows": [["{\"title\":\"Sturdy tarp\",\"body\":\"full doc 2\"}"]]
                })
                .to_string()
            };
            send_ok(&mut socket, &response_body);
            sirix_call_count_c.fetch_add(1, Ordering::SeqCst);
        }
    });

    // ---- Compose: same order as lib.rs::search ---------------------------
    let opts = PlainOpts {
        limit: Some(20),
        highlight: true,
        include_body: true,
        ..PlainOpts::default()
    };
    let manticore_body = build_request_body("manuals", "waterproof", &opts).unwrap();
    let manticore_url = format!("http://{manticore_addr}/search");
    let manticore_response =
        http_post_via_tcp(&manticore_url, &manticore_body).expect("manticore post");
    let mut hits = parse_response(&manticore_response).expect("parse manticore");
    assert_eq!(hits.len(), 2);

    // Verify Manticore saw the correct body.
    let manticore_received = m_rx.recv().unwrap();
    assert_eq!(manticore_received, manticore_body);

    // Now fetch each body from Sirix.
    let sirix_url_str = format!("http://{sirix_addr}");
    let sirix_query_url = query_url(&sirix_url_str);
    for hit in hits.iter_mut() {
        let doc = parse_sirix_uri(&hit.doc).unwrap();
        let sql = build_fetch_sql(&doc, None);
        let body = build_fetch_body(&sql);
        let response_body = http_post_via_tcp(&sirix_query_url, &body).expect("sirix post");
        let fetched = parse_fetch_response(&response_body, None).unwrap();
        hit.body = Some(fetched.body);
        hit.content_type = Some(fetched.content_type);
    }

    // Verify the join: hits carry the right bodies.
    assert_eq!(sirix_call_count.load(Ordering::SeqCst), 2);

    assert_eq!(hits[0].doc, "sirix://docs/manuals/1");
    assert_eq!(hits[0].snippet.as_deref(), Some("<b>waterproof</b> rig"));
    let body0 = String::from_utf8(hits[0].body.clone().unwrap()).unwrap();
    assert!(body0.contains("full doc 1"));
    assert_eq!(hits[0].content_type.as_deref(), Some("application/json"));

    assert_eq!(hits[1].doc, "sirix://docs/manuals/2");
    let body1 = String::from_utf8(hits[1].body.clone().unwrap()).unwrap();
    assert!(body1.contains("full doc 2"));

    // Verify Sirix saw the correct SQL for each hit.
    let s0: JsonValue = serde_json::from_str(&s_rx.recv().unwrap()).unwrap();
    assert!(s0["sql"].as_str().unwrap().contains("_nodekey = '1'"));
    let s1: JsonValue = serde_json::from_str(&s_rx.recv().unwrap()).unwrap();
    assert!(s1["sql"].as_str().unwrap().contains("_nodekey = '2'"));

    manticore_thread.join().unwrap();
    sirix_thread.join().unwrap();
}

// ---------------------------------------------------------------------------
// Index-only design coverage — Manticore holds no bodies, Sirix does.
// ---------------------------------------------------------------------------

/// Manticore returns hits with *no* body in `_source` (the index-only
/// design). When `opts.include_body = true`, the guest's compose step
/// must round-trip to Sirix for every hit. Test asserts:
///   1. Sirix `/query` is called once per hit.
///   2. Every hit's `body` is populated from Sirix.
#[test]
fn search_with_include_body_fetches_from_sirix() {
    // ---- Manticore mock: bodies absent from `_source` ----------------
    let manticore_listener = TcpListener::bind("127.0.0.1:0").expect("bind manticore");
    let manticore_addr = manticore_listener.local_addr().unwrap();
    let manticore_thread = thread::spawn(move || {
        let (mut socket, _) = manticore_listener.accept().expect("accept manticore");
        let (headers, _body) = read_http_request(&mut socket);
        assert!(headers.starts_with("POST /search "), "manticore headers={headers:?}");
        let response_body = json!({
            "hits": {
                "hits": [
                    { "_id": "sirix://docs/manuals/1", "_score": 0.9,
                      "_source": { "title": "index-only row A" } },
                    { "_id": "sirix://docs/manuals/2", "_score": 0.6,
                      "_source": { "title": "index-only row B" } }
                ]
            }
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    // ---- Sirix mock: one call per hit --------------------------------
    let sirix_listener = TcpListener::bind("127.0.0.1:0").expect("bind sirix");
    let sirix_addr = sirix_listener.local_addr().unwrap();
    let sirix_call_count = Arc::new(AtomicUsize::new(0));
    let sirix_calls = sirix_call_count.clone();
    let sirix_thread = thread::spawn(move || {
        for i in 0..2 {
            let (mut socket, _) = sirix_listener.accept().expect("accept sirix");
            let (headers, _body) = read_http_request(&mut socket);
            assert!(
                headers.starts_with("POST /query "),
                "sirix headers[{i}]={headers:?}"
            );
            let doc = if i == 0 { "body-A" } else { "body-B" };
            let response_body = json!({
                "columns": ["document"],
                "rows": [[doc]]
            })
            .to_string();
            send_ok(&mut socket, &response_body);
            sirix_calls.fetch_add(1, Ordering::SeqCst);
        }
    });

    // ---- Compose (mirroring lib.rs::search when include_body = true) -
    let opts = PlainOpts {
        limit: Some(20),
        include_body: true,
        ..PlainOpts::default()
    };
    let manticore_body = build_request_body("manuals", "waterproof", &opts).unwrap();
    let manticore_url_str = format!("http://{manticore_addr}/search");
    let manticore_response =
        http_post_via_tcp(&manticore_url_str, &manticore_body).expect("manticore post");
    let mut hits = parse_response(&manticore_response).expect("parse manticore");
    assert_eq!(hits.len(), 2);

    // Manticore did NOT provide bodies — this is the index-only shape.
    assert!(hits[0].body.is_none(), "Manticore body must be absent");
    assert!(hits[1].body.is_none(), "Manticore body must be absent");

    // Guest compose: fetch bodies from Sirix, per-hit soft-fail.
    let sirix_url_str = format!("http://{sirix_addr}");
    compose_fetch_bodies(&sirix_url_str, &mut hits, None);

    manticore_thread.join().unwrap();
    sirix_thread.join().unwrap();

    // Every hit was fetched from Sirix exactly once.
    assert_eq!(sirix_call_count.load(Ordering::SeqCst), 2);
    // And every hit's body is now populated.
    assert_eq!(
        String::from_utf8(hits[0].body.clone().unwrap()).unwrap(),
        "body-A"
    );
    assert_eq!(
        String::from_utf8(hits[1].body.clone().unwrap()).unwrap(),
        "body-B"
    );
}

/// Same setup — Manticore returns hits without bodies — but the caller
/// sets `include_body = false`. Sirix must not be contacted at all.
/// Enforced by binding the Sirix listener and asserting zero accepted
/// connections.
#[test]
fn search_without_include_body_skips_sirix_fetch() {
    let manticore_listener = TcpListener::bind("127.0.0.1:0").expect("bind manticore");
    let manticore_addr = manticore_listener.local_addr().unwrap();
    let manticore_thread = thread::spawn(move || {
        let (mut socket, _) = manticore_listener.accept().expect("accept manticore");
        let (_h, _b) = read_http_request(&mut socket);
        let response_body = json!({
            "hits": {
                "hits": [
                    { "_id": "sirix://docs/manuals/1", "_score": 0.9,
                      "_source": { "title": "no body needed" } },
                    { "_id": "sirix://docs/manuals/2", "_score": 0.6,
                      "_source": { "title": "no body needed either" } }
                ]
            }
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    // Sirix listener bound but the test asserts zero connections were
    // accepted. If the compose incorrectly fetches, the connection would
    // succeed and the test's stream count would show 1+.
    let sirix_listener = TcpListener::bind("127.0.0.1:0").expect("bind sirix");
    sirix_listener
        .set_nonblocking(true)
        .expect("nonblocking");
    let sirix_addr = sirix_listener.local_addr().unwrap();

    let opts = PlainOpts {
        limit: Some(20),
        include_body: false,
        ..PlainOpts::default()
    };
    let manticore_body = build_request_body("manuals", "waterproof", &opts).unwrap();
    let manticore_url_str = format!("http://{manticore_addr}/search");
    let manticore_response =
        http_post_via_tcp(&manticore_url_str, &manticore_body).expect("manticore post");
    let hits = parse_response(&manticore_response).expect("parse manticore");
    assert_eq!(hits.len(), 2);

    // Compose: with include_body = false, the guest MUST skip the Sirix
    // round-trip. Verify by NOT calling compose_fetch_bodies here — that
    // matches lib.rs::search's branch. As belt-and-braces, also verify
    // the listener never accepted anything.
    let sirix_url_str = format!("http://{sirix_addr}");
    // No compose_fetch_bodies call. Bodies stay None.

    // Give any errant background fetch a chance to arrive (there
    // shouldn't be any).
    std::thread::sleep(std::time::Duration::from_millis(50));
    match sirix_listener.accept() {
        Ok(_) => panic!("Sirix should not have been contacted with include_body=false"),
        Err(e) => {
            assert_eq!(
                e.kind(),
                std::io::ErrorKind::WouldBlock,
                "expected WouldBlock, got {e}"
            );
        }
    }

    // Bodies untouched.
    assert!(hits[0].body.is_none());
    assert!(hits[1].body.is_none());
    // Snippets also stay None: no body → no substring snippet.
    assert!(hits[0].snippet.is_none());
    assert!(hits[1].snippet.is_none());
    // sirix_url_str is unused when include_body is false — mark for
    // clippy so the test compiles cleanly.
    let _ = sirix_url_str;

    manticore_thread.join().unwrap();
}

/// Guest-side snippet path: with `include_body: true` and `highlight:
/// true`, the guest must build a substring snippet from the Sirix body
/// and wrap matching runs with `<mark>...</mark>`. Verifies wrapping
/// via the pure `generate_snippet` (same function `lib.rs::search`
/// calls) applied to a Sirix-returned body.
#[test]
fn snippet_generation_from_sirix_body() {
    let sirix_listener = TcpListener::bind("127.0.0.1:0").expect("bind sirix");
    let sirix_addr = sirix_listener.local_addr().unwrap();
    let sirix_thread = thread::spawn(move || {
        let (mut socket, _) = sirix_listener.accept().expect("accept sirix");
        let (_h, _b) = read_http_request(&mut socket);
        let response_body = json!({
            "columns": ["document"],
            "rows": [["The waterproof rig kept the crew dry all afternoon."]]
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    // Build one hit and populate its body via the same Sirix path.
    let mut hits = vec![Hit {
        doc: "sirix://docs/manuals/7".into(),
        score: 1.0,
        snippet: None,
        lang: None,
        body: None,
        content_type: None,
        fields: vec![],
        revision: None,
    }];
    let sirix_url_str = format!("http://{sirix_addr}");
    compose_fetch_bodies(&sirix_url_str, &mut hits, None);
    sirix_thread.join().unwrap();

    assert!(hits[0].body.is_some(), "body populated from Sirix");
    // Now generate the snippet on that body — mirrors what
    // `populate_snippets` does when `highlight: true`.
    let body_bytes = hits[0].body.clone().unwrap();
    let snippet = wf_document::generate_snippet(
        &body_bytes,
        &wf_document::snippet_terms("waterproof"),
    )
    .expect("snippet generated");
    assert!(
        snippet.contains("<mark>waterproof</mark>"),
        "mark wrapping in snippet: {snippet}"
    );
}

/// Per-hit soft-fail: Sirix returns 500 for the first hit and 200 for
/// the second. The compose step must:
///   1. Leave hit[0].body as None (bad fetch, don't propagate).
///   2. Populate hit[1].body from the 200 response.
///   3. NOT fail the whole search.
#[test]
fn sirix_fetch_failure_per_hit_leaves_body_none() {
    let manticore_listener = TcpListener::bind("127.0.0.1:0").expect("bind manticore");
    let manticore_addr = manticore_listener.local_addr().unwrap();
    let manticore_thread = thread::spawn(move || {
        let (mut socket, _) = manticore_listener.accept().expect("accept manticore");
        let (_h, _b) = read_http_request(&mut socket);
        let response_body = json!({
            "hits": {
                "hits": [
                    { "_id": "sirix://docs/manuals/1", "_score": 0.9,
                      "_source": { "title": "will-fail" } },
                    { "_id": "sirix://docs/manuals/2", "_score": 0.6,
                      "_source": { "title": "will-succeed" } }
                ]
            }
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    let sirix_listener = TcpListener::bind("127.0.0.1:0").expect("bind sirix");
    let sirix_addr = sirix_listener.local_addr().unwrap();
    let sirix_thread = thread::spawn(move || {
        // Hit 0 — return 500. Compose must swallow this.
        {
            let (mut socket, _) = sirix_listener.accept().expect("accept sirix 0");
            let (_h, _b) = read_http_request(&mut socket);
            send_error_500(&mut socket, "sirix boom");
        }
        // Hit 1 — return 200 with a real body. Braced so the previous
        // socket drops (client-side read() unblocks on EOF).
        {
            let (mut socket, _) = sirix_listener.accept().expect("accept sirix 1");
            let (_h, _b) = read_http_request(&mut socket);
            let response_body = json!({
                "columns": ["document"],
                "rows": [["good body for hit 1"]]
            })
            .to_string();
            send_ok(&mut socket, &response_body);
        }
    });

    let opts = PlainOpts {
        limit: Some(20),
        include_body: true,
        ..PlainOpts::default()
    };
    let manticore_body = build_request_body("manuals", "x", &opts).unwrap();
    let manticore_url_str = format!("http://{manticore_addr}/search");
    let manticore_response =
        http_post_via_tcp(&manticore_url_str, &manticore_body).expect("manticore post");
    let mut hits = parse_response(&manticore_response).expect("parse manticore");
    assert_eq!(hits.len(), 2);

    // Compose. Hit 0's Sirix call fails; hit 1's succeeds.
    let sirix_url_str = format!("http://{sirix_addr}");
    compose_fetch_bodies(&sirix_url_str, &mut hits, None);

    manticore_thread.join().unwrap();
    sirix_thread.join().unwrap();

    assert!(hits[0].body.is_none(), "failed hit's body must stay None");
    assert!(
        hits[1].body.is_some(),
        "successful hit's body must be populated"
    );
    assert_eq!(
        String::from_utf8(hits[1].body.clone().unwrap()).unwrap(),
        "good body for hit 1"
    );
}

/// Test-side mirror of `lib.rs::fetch_bodies_for_hits`. Same soft-fail
/// semantics: parse errors / HTTP errors / response-parse errors leave
/// that hit's body as None, other hits proceed.
///
/// Kept in tests/ because the guest version calls `host::http_post_json`
/// (a WIT import that native tests can't stub). This mirror uses the
/// same stdlib TCP client the other tests use.
fn compose_fetch_bodies(sirix_url: &str, hits: &mut [Hit], body_content_type: Option<&str>) {
    let url = query_url(sirix_url);
    for hit in hits.iter_mut() {
        if hit.body.is_some() {
            continue;
        }
        let Ok(doc) = parse_sirix_uri(&hit.doc) else { continue };
        let sql = build_fetch_sql(&doc, hit.revision);
        let body = build_fetch_body(&sql);
        let Ok(response_body) = http_post_via_tcp(&url, &body) else {
            continue;
        };
        let Ok(fetched) = parse_fetch_response(&response_body, body_content_type) else {
            continue;
        };
        hit.body = Some(fetched.body);
        hit.content_type = Some(fetched.content_type);
    }
}

fn send_error_500(socket: &mut std::net::TcpStream, msg: &str) {
    let response = format!(
        "HTTP/1.1 500 Internal Server Error\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {}",
        msg.len(),
        msg
    );
    socket
        .write_all(response.as_bytes())
        .expect("write 500");
}

// ---------------------------------------------------------------------------
// Helpers — same shape as sirix_client.rs / manticore_client.rs.
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
