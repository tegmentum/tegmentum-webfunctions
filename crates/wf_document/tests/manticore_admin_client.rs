//! Wire round-trip tests for the Manticore admin ops — ported verbatim
//! from `wf_fulltext/tests/manticore_admin_client.rs`, error prefix
//! adjusted for wf_document.

use serde_json::{json, Value as JsonValue};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use wf_document::manticore_admin::{
    build_bulk_body, build_delete_body, parse_bulk_response, PlainDocWrite,
};

#[test]
fn wire_round_trip_insert_batch() {
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

        assert!(headers.starts_with("POST /bulk "), "headers={headers:?}");
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        let response_body = json!({
            "items": [
                { "replace": { "_id": "sirix://docs/manuals/1", "result": "created" } },
                { "replace": { "_id": "sirix://docs/manuals/2", "result": "updated" } }
            ],
            "errors": false
        })
        .to_string();
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

    let docs = vec![
        PlainDocWrite {
            id: "sirix://docs/manuals/1".into(),
            fields: vec![("title".into(), "widget".into())],
            lang: Some("en".into()),
        },
        PlainDocWrite {
            id: "sirix://docs/manuals/2".into(),
            fields: vec![("title".into(), "gadget".into())],
            lang: Some("en".into()),
        },
    ];
    let body = build_bulk_body("manuals", &docs);

    let url = format!("http://{addr}/bulk");
    let response_body = http_post_via_tcp(&url, &body).expect("post");

    let received = rx.recv().unwrap();
    assert_eq!(received, body);

    let count = parse_bulk_response(&response_body).unwrap();
    assert_eq!(count, 2);

    server.join().unwrap();
}

#[test]
fn wire_round_trip_delete_batch() {
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

        assert!(headers.starts_with("POST /bulk "), "headers={headers:?}");
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        let response_body = json!({
            "items": [
                { "delete": { "_id": "sirix://docs/manuals/1", "result": "deleted" } }
            ],
            "errors": false
        })
        .to_string();
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

    let ids = vec!["sirix://docs/manuals/1".to_string()];
    let body = build_delete_body("manuals", &ids);

    let url = format!("http://{addr}/bulk");
    let response_body = http_post_via_tcp(&url, &body).expect("post");

    let received = rx.recv().unwrap();
    assert_eq!(received, body);
    let line: JsonValue = serde_json::from_str(received.trim_end_matches('\n')).unwrap();
    assert_eq!(line["delete"]["index"], "manuals");
    assert_eq!(line["delete"]["id"], "sirix://docs/manuals/1");

    let count = parse_bulk_response(&response_body).unwrap();
    assert_eq!(count, 1);

    server.join().unwrap();
}

// ---------------------------------------------------------------------------
// Minimal stdlib HTTP client.
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
         Content-Type: application/x-ndjson\r\n\
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
