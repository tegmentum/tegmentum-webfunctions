//! Sirix write-through — pure builders for INSERT / UPDATE / DELETE
//! SQL against sirix-sql-server's `POST /query` endpoint, plus the
//! DML response parser.
//!
//! v0.2 deferred write-through; v1.1 lands it in the guest. Kept in
//! its own module so `sirix.rs` stays focused on the SELECT path and
//! so tests exercise the SQL construction / response parsing directly
//! without touching the read side.
//!
//! **Design**: Sirix is the source of truth for identity and revision
//! numbers. The guest emits DML SQL; sirix-sql-server (once its DML
//! path exists — see the "sirix-sql-server DML gap" section in
//! `docs/design/wf-document.md`) executes it against Sirix and echoes
//! back the committed revision + valid-from timestamp. Manticore is
//! caught up on the periodic sweep — the `write-result` returned to
//! callers is durable once Sirix commits; the search index converges
//! later.
//!
//! **Prerequisite**: sirix-sql-server's `QueryHandler.java` currently
//! calls JDBC's `Statement.executeQuery(sql)` — SELECT-only. DML
//! support requires a sibling change to route non-SELECT statements
//! through `executeUpdate`, plus a shape decision for the DML response
//! body. Until that lands, production Sirix will reject the DML with a
//! JDBC exception the guest surfaces as `Err(...)`. This module is
//! implemented and tested against a compatible mock so the guest can
//! land independently of the sirix-sql-server-side work.
//!
//! **Response shape** (agreed with sirix-sql-server DML follow-up):
//!   { "columns": ["_rev", "_valid_from"],
//!     "rows":    [[<u64>, "<ISO-8601>"]] }
//! The columns are named consistently with the read-side; the parser
//! tolerates case variations (a JDBC driver may uppercase labels) and
//! also accepts a flat `{"revision": N, "valid_from": "..."}` shape
//! for a simpler DML wrapper.

use serde_json::Value as JsonValue;

use crate::sirix::DocId;

/// Build the SQL body for an INSERT. The document body is embedded as
/// a SQL string literal — Sirix's native shape is JSON, so we pass the
/// bytes through as UTF-8 text and let Sirix reject anything it can't
/// parse. Non-UTF-8 payloads are rejected here rather than corrupted
/// on the wire.
///
/// Shape: `INSERT INTO "<db>"."<resource>" (document) VALUES ('<escaped>')`.
/// The DML surface for sirix-sql-server hasn't been standardized yet;
/// this shape mirrors what a straightforward JDBC-compatible driver
/// would accept.
pub fn build_insert_sql(
    database: &str,
    resource: &str,
    body: &[u8],
) -> Result<String, String> {
    let body_str = std::str::from_utf8(body)
        .map_err(|e| format!("wf_document: insert body is not valid UTF-8: {e}"))?;
    Ok(format!(
        "INSERT INTO \"{}\".\"{}\" (document) VALUES ('{}')",
        escape_ident(database),
        escape_ident(resource),
        escape_sql_str(body_str),
    ))
}

/// Build the SQL body for an UPDATE addressed by the caller's
/// business key. When `expected_revision` is supplied the caller
/// expresses "I saw revision N; write on top of that" as an
/// optimistic-concurrency predicate. Sirix's MVCC commits at the
/// current head regardless — the predicate is a coarse guard, not a
/// compare-and-swap, and its enforcement lives in sirix-sql-server's
/// DML path (a sibling deliverable).
///
/// **Column-name / identity shape**: matches `sirix::build_fetch_sql`.
/// Sirix exposes `_key BIGINT` (internal node key, not a business
/// identifier) and `_revision BIGINT`. Rows are addressed by JSON path
/// on `$._id` so the URI's last segment resolves to the same document
/// the read path sees.
pub fn build_update_sql(
    doc: &DocId,
    body: &[u8],
    expected_revision: Option<u64>,
) -> Result<String, String> {
    let body_str = std::str::from_utf8(body)
        .map_err(|e| format!("wf_document: update body is not valid UTF-8: {e}"))?;
    let base = format!(
        "UPDATE \"{}\".\"{}\" SET document = '{}' \
         WHERE JSON_VALUE(\"document\", '$._id') = '{}'",
        escape_ident(&doc.database),
        escape_ident(&doc.resource),
        escape_sql_str(body_str),
        escape_sql_str(&doc.node_key),
    );
    Ok(match expected_revision {
        Some(rev) => format!("{base} AND _revision = {rev}"),
        None => base,
    })
}

/// Build the SQL body for a DELETE addressed by the caller's business
/// key. Same identity shape as `build_update_sql` — see that doc-
/// comment for the column-name rationale.
pub fn build_delete_sql(doc: &DocId) -> String {
    format!(
        "DELETE FROM \"{}\".\"{}\" \
         WHERE JSON_VALUE(\"document\", '$._id') = '{}'",
        escape_ident(&doc.database),
        escape_ident(&doc.resource),
        escape_sql_str(&doc.node_key),
    )
}

/// A committed write, as returned by sirix-sql-server after a
/// successful DML. The guest lifts these into the WIT `write-result`
/// on the way out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteAck {
    pub revision: u64,
    pub valid_from: String,
    /// Present on INSERT, where the caller doesn't yet know the
    /// node-key Sirix assigned. `None` on UPDATE / DELETE because the
    /// caller supplied the node-key already.
    pub node_key: Option<String>,
}

/// Parse a DML response from sirix-sql-server into a `WriteAck`.
///
/// Accepts two shapes:
///   * `{"columns": [...], "rows": [[...]]}` with columns `_rev`,
///     `_valid_from`, and (optionally, on INSERT) `_nodekey`.
///     Preferred: shares the read-side JSON envelope.
///   * A flat single-object shape (`{"revision": N, "valid_from": "..."}`)
///     for a simpler DML wrapper.
///
/// Column matching is case-insensitive: a JDBC driver may uppercase
/// labels on the way back through Undertow.
pub fn parse_write_response(json_str: &str) -> Result<WriteAck, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("wf_document: sirix write response is not JSON: {e}"))?;

    if let Some(err) = root.get("error").and_then(|e| e.as_str()) {
        return Err(format!("wf_document: sirix: {err}"));
    }

    // Flat shape first — a DML wrapper that returns a bare object.
    let flat_rev = root
        .get("revision")
        .or_else(|| root.get("rev"))
        .or_else(|| root.get("_rev"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        });
    let flat_vf = root
        .get("valid_from")
        .or_else(|| root.get("_valid_from"))
        .and_then(|v| v.as_str());
    if let (Some(rev), Some(vf)) = (flat_rev, flat_vf) {
        return Ok(WriteAck {
            revision: rev,
            valid_from: vf.to_string(),
            node_key: root
                .get("node_key")
                .or_else(|| root.get("_nodekey"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        });
    }

    // Columns/rows shape.
    let columns = root
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or_else(|| {
            "wf_document: sirix write response missing `columns` array".to_string()
        })?;
    let rows = root
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or_else(|| {
            "wf_document: sirix write response missing `rows` array".to_string()
        })?;

    if rows.is_empty() {
        return Err("wf_document: sirix write returned zero rows".into());
    }

    let first_row = rows[0].as_array().ok_or_else(|| {
        format!("wf_document: sirix write row was not an array: {}", rows[0])
    })?;

    let idx_of = |name: &str| -> Option<usize> {
        columns.iter().position(|c| {
            c.as_str()
                .map_or(false, |s| s.eq_ignore_ascii_case(name))
        })
    };

    let rev_idx = idx_of("_rev")
        .or_else(|| idx_of("rev"))
        .or_else(|| idx_of("revision"))
        .ok_or_else(|| {
            "wf_document: sirix write response has no `_rev` column".to_string()
        })?;
    let rev_cell = first_row.get(rev_idx).ok_or_else(|| {
        "wf_document: sirix write row missing _rev cell".to_string()
    })?;
    let revision = rev_cell
        .as_u64()
        .or_else(|| rev_cell.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| {
            format!("wf_document: _rev cell was not an integer: {rev_cell}")
        })?;

    let vf_idx = idx_of("_valid_from")
        .or_else(|| idx_of("valid_from"))
        .ok_or_else(|| {
            "wf_document: sirix write response has no `_valid_from` column"
                .to_string()
        })?;
    let vf_cell = first_row.get(vf_idx).ok_or_else(|| {
        "wf_document: sirix write row missing _valid_from cell".to_string()
    })?;
    let valid_from = vf_cell
        .as_str()
        .ok_or_else(|| {
            format!("wf_document: _valid_from cell was not a string: {vf_cell}")
        })?
        .to_string();

    let node_key = idx_of("_nodekey")
        .or_else(|| idx_of("nodekey"))
        .and_then(|i| first_row.get(i))
        .and_then(|c| c.as_str().map(|s| s.to_string()));

    Ok(WriteAck {
        revision,
        valid_from,
        node_key,
    })
}

fn escape_ident(s: &str) -> String {
    s.replace('"', "\"\"")
}

fn escape_sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_insert_sql_shape() {
        let sql = build_insert_sql("docs", "manuals", br#"{"title":"waterproof rig"}"#)
            .unwrap();
        assert!(sql.starts_with("INSERT INTO \"docs\".\"manuals\""), "{sql}");
        assert!(
            sql.contains("(document) VALUES ('{\"title\":\"waterproof rig\"}')"),
            "{sql}"
        );
    }

    #[test]
    fn build_insert_sql_escapes_single_quotes() {
        let sql = build_insert_sql("d", "r", b"o'reilly").unwrap();
        assert!(sql.contains("VALUES ('o''reilly')"), "{sql}");
    }

    #[test]
    fn build_insert_sql_escapes_ident_double_quotes() {
        let sql = build_insert_sql("do\"cs", "r", b"{}").unwrap();
        assert!(sql.contains("\"do\"\"cs\""), "{sql}");
    }

    #[test]
    fn build_insert_sql_rejects_non_utf8_body() {
        let bad: &[u8] = &[0xff, 0xfe, 0xfd];
        assert!(build_insert_sql("d", "r", bad).is_err());
    }

    #[test]
    fn build_update_sql_no_revision() {
        let doc = DocId {
            database: "docs".into(),
            resource: "manuals".into(),
            node_key: "manual-01".into(),
        };
        let sql = build_update_sql(&doc, b"{\"x\":1}", None).unwrap();
        assert!(
            sql.starts_with("UPDATE \"docs\".\"manuals\" SET document = "),
            "{sql}"
        );
        assert!(
            sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"),
            "{sql}"
        );
        assert!(!sql.contains("_revision"), "{sql}");
        assert!(!sql.contains("_nodekey"), "{sql}");
    }

    #[test]
    fn build_update_sql_with_expected_revision() {
        let doc = DocId {
            database: "docs".into(),
            resource: "manuals".into(),
            node_key: "manual-01".into(),
        };
        let sql = build_update_sql(&doc, b"{\"x\":2}", Some(5)).unwrap();
        assert!(sql.contains("AND _revision = 5"), "{sql}");
        assert!(!sql.contains("_rev = 5"), "{sql}");
    }

    #[test]
    fn build_delete_sql_shape() {
        let doc = DocId {
            database: "docs".into(),
            resource: "manuals".into(),
            node_key: "manual-01".into(),
        };
        let sql = build_delete_sql(&doc);
        assert_eq!(
            sql,
            "DELETE FROM \"docs\".\"manuals\" \
             WHERE JSON_VALUE(\"document\", '$._id') = 'manual-01'"
        );
    }

    #[test]
    fn parse_write_response_columns_rows_shape() {
        let body = json!({
            "columns": ["_rev", "_valid_from", "_nodekey"],
            "rows": [[1, "2026-07-12T18:30:00Z", "42"]]
        })
        .to_string();
        let ack = parse_write_response(&body).unwrap();
        assert_eq!(ack.revision, 1);
        assert_eq!(ack.valid_from, "2026-07-12T18:30:00Z");
        assert_eq!(ack.node_key.as_deref(), Some("42"));
    }

    #[test]
    fn parse_write_response_flat_shape() {
        let body = json!({
            "revision": 3,
            "valid_from": "2026-07-12T19:00:00Z"
        })
        .to_string();
        let ack = parse_write_response(&body).unwrap();
        assert_eq!(ack.revision, 3);
        assert_eq!(ack.valid_from, "2026-07-12T19:00:00Z");
        assert_eq!(ack.node_key, None);
    }

    #[test]
    fn parse_write_response_case_insensitive_columns() {
        let body = json!({
            "columns": ["_REV", "_VALID_FROM"],
            "rows": [[7, "2026-07-12T20:00:00Z"]]
        })
        .to_string();
        let ack = parse_write_response(&body).unwrap();
        assert_eq!(ack.revision, 7);
    }

    #[test]
    fn parse_write_response_surface_sirix_error() {
        let body = json!({ "error": "constraint violated" }).to_string();
        let err = parse_write_response(&body).unwrap_err();
        assert!(err.contains("constraint violated"), "{err}");
    }

    #[test]
    fn parse_write_response_missing_rev_errors() {
        let body = json!({
            "columns": ["_valid_from"],
            "rows": [["2026-07-12T20:00:00Z"]]
        })
        .to_string();
        assert!(parse_write_response(&body).is_err());
    }
}
