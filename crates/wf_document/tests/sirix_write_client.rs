//! Wire round-trip tests for the Sirix write-through client. Same
//! hand-rolled TcpListener + HTTP/1.1 idiom as `sirix_client.rs` and
//! `manticore_client.rs` — dependency-free, honest about the actual
//! bytes on the wire.
//!
//! Tests the guest's write path against a mock sirix-sql-server that
//! echoes the received SQL back so we can assert the exact SQL shape.
//! The mock also returns a synthesized `{_rev, _valid_from}` response
//! so we exercise the write-ack parser.
//!
//! Note the sirix-sql-server-side prerequisite: production
//! sirix-sql-server currently only accepts SELECT (its `QueryHandler`
//! calls `Statement.executeQuery`). The mock here simulates what a
//! DML-capable follow-up server would return. See the "Sirix-side gap"
//! notes in `docs/design/wf-document.md`.

use serde_json::{json, Value as JsonValue};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use wf_document::sirix::{build_fetch_body, parse_sirix_uri, query_url, DocId};
use wf_document::sirix_write::{
    build_delete_sql, build_insert_sql, build_update_sql, parse_write_response,
};

// -----------------------------------------------------------------
// Per-op wire round-trips.
// -----------------------------------------------------------------

#[test]
fn insert_returns_revision_1_on_first_commit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (_h, body) = read_http_request(&mut socket);
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        let response_body = json!({
            "columns": ["_rev", "_valid_from", "_nodekey"],
            "rows": [[1, "2026-07-12T18:30:00Z", "42"]]
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    let sql =
        build_insert_sql("docs", "manuals", br#"{"title":"waterproof rig"}"#).unwrap();
    let body = build_fetch_body(&sql);
    let url = query_url(&format!("http://{addr}"));
    assert_eq!(url, format!("http://{addr}/query"));

    let response_body = http_post_via_tcp(&url, &body).expect("post");

    // Verify guest emitted correct SQL.
    let received = rx.recv().unwrap();
    let parsed_received: JsonValue = serde_json::from_str(&received).unwrap();
    let received_sql = parsed_received["sql"].as_str().unwrap();
    assert!(
        received_sql.starts_with("INSERT INTO \"docs\".\"manuals\""),
        "sql={received_sql:?}"
    );
    assert!(received_sql.contains("(document) VALUES ("));

    // Verify guest parsed rev=1 back correctly.
    let ack = parse_write_response(&response_body).unwrap();
    assert_eq!(ack.revision, 1);
    assert_eq!(ack.valid_from, "2026-07-12T18:30:00Z");
    assert_eq!(ack.node_key.as_deref(), Some("42"));

    server.join().unwrap();
}

#[test]
fn update_increments_revision() {
    // Simulate: caller updates a doc they saw at revision 5. Server
    // commits and returns rev=6.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (_h, body) = read_http_request(&mut socket);
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        let response_body = json!({
            "columns": ["_rev", "_valid_from"],
            "rows": [[6, "2026-07-12T19:00:00Z"]]
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    let doc = parse_sirix_uri("sirix://docs/manuals/manual-01").unwrap();
    let sql = build_update_sql(&doc, br#"{"title":"updated"}"#, Some(5)).unwrap();
    let body = build_fetch_body(&sql);
    let url = format!("http://{addr}/query");

    let response_body = http_post_via_tcp(&url, &body).expect("post");

    let received = rx.recv().unwrap();
    let parsed_received: JsonValue = serde_json::from_str(&received).unwrap();
    let received_sql = parsed_received["sql"].as_str().unwrap();
    assert!(
        received_sql.starts_with("UPDATE \"docs\".\"manuals\" SET document = "),
        "sql={received_sql:?}"
    );
    // Identity is by JSON path on `$._id`; Sirix's `_key` is a BIGINT
    // internal node key so it can't stand in for a business key.
    assert!(
        received_sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"),
        "sql={received_sql:?}"
    );
    // Sirix's metadata column is `_revision`, not `_rev`.
    assert!(
        received_sql.contains("AND _revision = 5"),
        "expected OCC predicate, sql={received_sql:?}"
    );

    let ack = parse_write_response(&response_body).unwrap();
    assert_eq!(ack.revision, 6, "revision must increment from 5 to 6");
    server.join().unwrap();
}

#[test]
fn update_without_expected_revision_is_unconditional() {
    // No optimistic-concurrency predicate — the SQL should not
    // contain a `_revision =` filter clause.
    let doc = DocId {
        database: "docs".into(),
        resource: "manuals".into(),
        node_key: "manual-01".into(),
    };
    let sql = build_update_sql(&doc, b"{}", None).unwrap();
    assert!(
        !sql.contains("_revision ="),
        "should be unconditional, sql={sql:?}"
    );
    assert!(
        sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"),
        "sql={sql:?}"
    );
}

#[test]
fn delete_removes_doc() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (_h, body) = read_http_request(&mut socket);
        tx.send(String::from_utf8(body).expect("utf8 body")).unwrap();

        // Sirix's delete is a tombstone commit — it still gets a
        // revision number and valid-from timestamp.
        let response_body = json!({
            "columns": ["_rev", "_valid_from"],
            "rows": [[7, "2026-07-12T19:30:00Z"]]
        })
        .to_string();
        send_ok(&mut socket, &response_body);
    });

    let doc = parse_sirix_uri("sirix://docs/manuals/manual-01").unwrap();
    let sql = build_delete_sql(&doc);
    let body = build_fetch_body(&sql);
    let url = format!("http://{addr}/query");

    let response_body = http_post_via_tcp(&url, &body).expect("post");

    let received = rx.recv().unwrap();
    let parsed_received: JsonValue = serde_json::from_str(&received).unwrap();
    let received_sql = parsed_received["sql"].as_str().unwrap();
    assert_eq!(
        received_sql,
        "DELETE FROM \"docs\".\"manuals\" \
         WHERE JSON_VALUE(\"document\", '$._id') = 'manual-01'"
    );

    let ack = parse_write_response(&response_body).unwrap();
    assert_eq!(ack.revision, 7);

    server.join().unwrap();
}

#[test]
fn write_fails_when_sirix_returns_error() {
    // A 500 with a JSON error body — the parser lifts the error field
    // into an Err() and the guest surfaces it verbatim.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (_h, _b) = read_http_request(&mut socket);
        let body = json!({ "error": "duplicate node-key: 42" }).to_string();
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\n\
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
    let sql = build_insert_sql("docs", "manuals", b"{}").unwrap();
    let body = build_fetch_body(&sql);
    let response_body = http_post_via_tcp(&url, &body).expect("post");
    let err = parse_write_response(&response_body).unwrap_err();
    assert!(
        err.contains("duplicate node-key"),
        "expected surfaced sirix error, got: {err}"
    );

    server.join().unwrap();
}

// -----------------------------------------------------------------
// The sirix-sql-server DML gap: production sirix-sql-server today
// executes JDBC `Statement.executeQuery(sql)` which only accepts
// SELECT. This test documents the failure mode when a real server
// rejects DML — a JDBC exception surfaces through the guest's Err()
// channel. Kept as documentation so the failure mode isn't a mystery
// during the sibling sirix-sql-server DML follow-up.
// -----------------------------------------------------------------

#[test]
fn sirix_sql_server_dml_gap_currently_rejects_writes() {
    // A JDBC SQLException the real server would return today: the
    // shape isn't fully standardized, but the message will contain
    // "executeQuery" and mention that DML isn't allowed.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let (_h, _b) = read_http_request(&mut socket);
        // Simulate the sirix-sql-server today: it catches the JDBC
        // exception in QueryHandler#handleRequest's catch(Exception)
        // and calls respondError(500, "query failed: " + e.getMessage()).
        let body = json!({
            "error": "query failed: java.sql.SQLException: Method executeQuery cannot be used for statement that does not produce a ResultSet"
        }).to_string();
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\n\
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
    let sql = build_insert_sql("docs", "manuals", b"{}").unwrap();
    let body = build_fetch_body(&sql);
    let response_body = http_post_via_tcp(&url, &body).expect("post");
    let err = parse_write_response(&response_body).unwrap_err();
    assert!(
        err.contains("executeQuery"),
        "expected the JDBC SELECT-only error to surface: {err}"
    );
}

// ---------------------------------------------------------------------------
// Minimal stdlib HTTP client + server helpers (identical to
// sirix_client.rs — kept local so the file reads self-contained).
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
