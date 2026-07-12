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

use serde_json::{json, Value as JsonValue};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::thread;

use wf_document::manticore::{build_request_body, parse_response, PlainOpts};
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
