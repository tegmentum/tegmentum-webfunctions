//! Wire round-trip tests for the Sirix client. Same idiom as the
//! Manticore tests — a stdlib TcpListener + hand-rolled HTTP/1.1 keeps
//! the tests dependency-free and honest about the actual bytes on the
//! wire.

use serde_json::{json, Value as JsonValue};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use wf_document::sirix::{
    build_fetch_body, build_fetch_sql, build_revisions_sql, parse_fetch_response,
    parse_revisions_response, parse_sirix_uri, query_url, DocId,
};

#[test]
fn wire_round_trip_fetch_document_no_revision() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (headers, body) = read_http_request(&mut socket);

        assert!(headers.starts_with("POST /query "), "headers={headers:?}");
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        let response_body = json!({
            "columns": ["document"],
            "rows": [["{\"title\":\"waterproof rig\"}"]]
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    let doc = parse_sirix_uri("sirix://docs/manuals/manual-01").unwrap();
    let sql = build_fetch_sql(&doc, None);
    let body = build_fetch_body(&sql);

    let url = query_url(&format!("http://{addr}"));
    assert_eq!(url, format!("http://{addr}/query"));

    let response_body = http_post_via_tcp(&url, &body).expect("post");

    let received = rx.recv().unwrap();
    let parsed_received: JsonValue = serde_json::from_str(&received).unwrap();
    let received_sql = parsed_received["sql"].as_str().unwrap();
    // Identity is by JSON path on `$._id` — Sirix's `_key` is a BIGINT
    // internal node key, not a business key.
    assert!(
        received_sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"),
        "sql={received_sql}"
    );
    // No revision predicate when revision is None.
    assert!(!received_sql.contains("_revision"), "sql={received_sql}");

    let fetched = parse_fetch_response(&response_body, None).unwrap();
    assert_eq!(fetched.body, b"{\"title\":\"waterproof rig\"}");
    assert_eq!(fetched.content_type, "application/json");

    server.join().unwrap();
}

#[test]
fn wire_round_trip_fetch_document_with_revision() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (_headers, body) = read_http_request(&mut socket);
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        let response_body = json!({
            "columns": ["document"],
            "rows": [["{\"title\":\"older revision\"}"]]
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    let doc = DocId {
        database: "docs".into(),
        resource: "manuals".into(),
        node_key: "manual-01".into(),
    };
    let sql = build_fetch_sql(&doc, Some(17));
    let body = build_fetch_body(&sql);

    let url = format!("http://{addr}/query");
    let response_body = http_post_via_tcp(&url, &body).expect("post");

    let received = rx.recv().unwrap();
    let parsed_received: JsonValue = serde_json::from_str(&received).unwrap();
    let received_sql = parsed_received["sql"].as_str().unwrap();
    // Sirix's metadata column is `_revision`, not `_rev`.
    assert!(received_sql.contains("_revision = 17"), "sql={received_sql}");
    assert!(
        received_sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"),
        "sql={received_sql}"
    );

    let fetched = parse_fetch_response(&response_body, None).unwrap();
    assert_eq!(fetched.body, b"{\"title\":\"older revision\"}");

    server.join().unwrap();
}

#[test]
fn wire_round_trip_list_revisions() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (_headers, body) = read_http_request(&mut socket);
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        let response_body = json!({
            "columns": ["_revision"],
            "rows": [[1], [2], [3]]
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    let doc = parse_sirix_uri("sirix://docs/manuals/manual-01").unwrap();
    let sql = build_revisions_sql(&doc);
    let body = build_fetch_body(&sql);

    let url = format!("http://{addr}/query");
    let response_body = http_post_via_tcp(&url, &body).expect("post");

    let received = rx.recv().unwrap();
    let parsed_received: JsonValue = serde_json::from_str(&received).unwrap();
    let received_sql = parsed_received["sql"].as_str().unwrap();
    assert!(received_sql.contains("SELECT _revision"), "sql={received_sql}");
    assert!(
        received_sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"),
        "sql={received_sql}"
    );

    let revs = parse_revisions_response(&response_body).unwrap();
    assert_eq!(revs, vec![1, 2, 3]);

    server.join().unwrap();
}

#[test]
fn wire_returns_error_on_non_200() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (_h, _b) = read_http_request(&mut socket);
        let body = json!({ "error": "resource does not exist" }).to_string();
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n\
             {}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).expect("write response");
    });

    let url = format!("http://{addr}/query");
    let body = build_fetch_body("SELECT anything");
    // http_post_via_tcp returns the body regardless of status; the
    // host::http_post_json wrapper is what would surface non-2xx as Err
    // in production. Here we still verify the parser surfaces the
    // sirix-side `error` field even when the response body is JSON.
    let response_body = http_post_via_tcp(&url, &body).expect("post");
    let err = parse_fetch_response(&response_body, None).unwrap_err();
    assert!(err.contains("resource does not exist"));

    server.join().unwrap();
}

// ---------------------------------------------------------------------------
// URI parsing tests (also covered as unit tests inside sirix.rs; keep a
// wire-adjacent copy here so this file reads self-contained).
// ---------------------------------------------------------------------------

#[test]
fn uri_parse_ok() {
    let d = parse_sirix_uri("sirix://docs/manuals/42").unwrap();
    assert_eq!(d.database, "docs");
    assert_eq!(d.resource, "manuals");
    assert_eq!(d.node_key, "42");
}

#[test]
fn uri_parse_rejects_bad_scheme() {
    assert!(parse_sirix_uri("http://a/b/c").is_err());
    assert!(parse_sirix_uri("sirix:/a/b/c").is_err());
}

#[test]
fn uri_parse_rejects_bad_shape() {
    assert!(parse_sirix_uri("sirix://a/b").is_err());
    assert!(parse_sirix_uri("sirix://a//c").is_err());
}

// ---------------------------------------------------------------------------
// Minimal stdlib HTTP client + server helpers.
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
