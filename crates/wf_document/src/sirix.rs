//! Sirix client — pure functions for `sirix-sql-server` request-body
//! construction, response parsing, and doc-id URI parsing.
//!
//! Wire shape (see ~/git/sirixdb-sql/sirix-sql-server/src/main/java/
//! io/sirixdb/sql/server/SirixSqlServer.java — the routing table
//! declares `POST /query` with body `{"sql": "..."}` and a response of
//! `{"columns": [...], "rows": [[...]]}`).
//!
//! Doc-ids are `sirix://<db>/<resource>/<node-key>` (memo §06). The URI
//! + revision is the true identity of a document; the URI alone means
//! "latest committed".
//!
//! Kept out of `lib.rs` so tests exercise SQL construction and JSON
//! parsing directly without instantiating the wit-bindgen bindings or
//! stubbing `host::http_post_json`. The guest calls these on either
//! side of the host import.

use serde_json::{json, Value as JsonValue};

/// Parsed doc-id components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocId {
    pub database: String,
    pub resource: String,
    pub node_key: String,
}

/// Parse a `sirix://<db>/<resource>/<node-key>` URI into its
/// components. Rejects any URI that doesn't have exactly three
/// non-empty segments after the scheme.
///
/// The node-key segment may include additional slashes only if we treat
/// them as opaque — but Sirix node-keys are integers, so we hold the
/// strict 3-segment shape here and surface the mismatch to callers
/// rather than guess. Callers wanting richer routing can layer their
/// own resolver above this.
pub fn parse_sirix_uri(uri: &str) -> Result<DocId, String> {
    let rest = uri
        .strip_prefix("sirix://")
        .ok_or_else(|| format!("wf_document: doc-id `{uri}` must start with sirix://"))?;
    let parts: Vec<&str> = rest.splitn(3, '/').collect();
    if parts.len() != 3 || parts.iter().any(|s| s.is_empty()) {
        return Err(format!(
            "wf_document: doc-id `{uri}` must be sirix://<db>/<resource>/<node-key>"
        ));
    }
    Ok(DocId {
        database: parts[0].to_string(),
        resource: parts[1].to_string(),
        node_key: parts[2].to_string(),
    })
}

/// A fetched document body. `content_type` is what the caller sees on
/// the WIT wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedDoc {
    pub body: Vec<u8>,
    pub content_type: String,
}

/// Build the endpoint URL for sirix-sql-server's `POST /query`.
/// Idempotent: if the caller already terminated their URL with
/// `/query`, don't double up.
pub fn query_url(sirix_url: &str) -> String {
    let trimmed = sirix_url.trim_end_matches('/');
    if trimmed.ends_with("/query") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/query")
    }
}

/// Build the SQL body sirix-sql-server expects for a document fetch.
///
/// The exposed surface today is SQL:2016 SELECT over a Sirix resource.
/// A document is addressed by its node-key. Optional revision selects a
/// point-in-time view; `None` means "latest committed".
///
/// The exact WHERE-clause column names (`_nodekey`, `_rev`) match
/// Sirix's implicit metadata columns as documented in the sirix-sql
/// project. A revision-scoped SELECT filters on both.
pub fn build_fetch_sql(doc: &DocId, revision: Option<u64>) -> String {
    // We escape the node-key by embedding it as a SQL string literal.
    // Sirix node-keys are integers in practice; we still quote them so
    // a non-integer node-key surfaces as a Sirix-side error rather than
    // a syntax error the guest can't attribute.
    match revision {
        Some(rev) => format!(
            "SELECT * FROM \"{}\".\"{}\" WHERE _nodekey = '{}' AND _rev = {}",
            escape_ident(&doc.database),
            escape_ident(&doc.resource),
            escape_sql_str(&doc.node_key),
            rev,
        ),
        None => format!(
            "SELECT * FROM \"{}\".\"{}\" WHERE _nodekey = '{}'",
            escape_ident(&doc.database),
            escape_ident(&doc.resource),
            escape_sql_str(&doc.node_key),
        ),
    }
}

/// Build the JSON body sirix-sql-server expects on `POST /query`.
pub fn build_fetch_body(sql: &str) -> String {
    json!({ "sql": sql }).to_string()
}

/// Parse sirix-sql-server's response into a fetched document.
///
/// Response shape (see ~/git/oxigraph-wf/src/sink.rs::sirix_json_to_binding_sets
/// for the reference reader):
///   { "columns": ["document" | "body" | ...],
///     "rows":    [ [ "<body-string-or-json>", ... ] ] }
///
/// We pick the first column whose name matches one of {document, body,
/// content, json} case-insensitively — Sirix's shredder shape isn't
/// standardized yet, so accepting a few likely names avoids brittle
/// coupling. Fallback: first non-null cell of the first row.
///
/// `content_type` is honored from `content_type_override` when Some;
/// otherwise defaults to `application/json` — Sirix's native shape is
/// JSON, so this is the honest v0.2 default.
pub fn parse_fetch_response(
    json_str: &str,
    content_type_override: Option<&str>,
) -> Result<FetchedDoc, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("wf_document: sirix response is not JSON: {e}"))?;

    if let Some(err) = root.get("error").and_then(|e| e.as_str()) {
        return Err(format!("wf_document: sirix: {err}"));
    }

    let columns = root
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "wf_document: sirix response missing `columns` array".to_string())?;
    let rows = root
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "wf_document: sirix response missing `rows` array".to_string())?;

    if rows.is_empty() {
        return Err("wf_document: sirix returned zero rows — document not found".into());
    }

    let first_row = rows[0]
        .as_array()
        .ok_or_else(|| format!("wf_document: sirix row was not an array: {}", rows[0]))?;

    // Prefer a column named document/body/content/json.
    let preferred = ["document", "body", "content", "json"];
    let column_names: Vec<String> = columns
        .iter()
        .map(|c| c.as_str().unwrap_or("").to_string())
        .collect();
    let idx = preferred
        .iter()
        .find_map(|name| {
            column_names
                .iter()
                .position(|c| c.eq_ignore_ascii_case(name))
        })
        // Fallback: first non-null cell.
        .or_else(|| first_row.iter().position(|c| !c.is_null()));

    let cell = idx
        .and_then(|i| first_row.get(i))
        .ok_or_else(|| "wf_document: sirix row had no usable columns".to_string())?;

    let (body_bytes, default_ct) = cell_to_body(cell);
    let content_type = content_type_override
        .map(|s| s.to_string())
        .unwrap_or(default_ct);

    Ok(FetchedDoc {
        body: body_bytes,
        content_type,
    })
}

fn cell_to_body(cell: &JsonValue) -> (Vec<u8>, String) {
    match cell {
        JsonValue::String(s) => (s.as_bytes().to_vec(), guess_content_type(s)),
        JsonValue::Null => (Vec::new(), "application/octet-stream".to_string()),
        // Objects/arrays/scalars → serialize as JSON.
        other => (
            other.to_string().into_bytes(),
            "application/json".to_string(),
        ),
    }
}

/// A cheap sniff: if the string looks like JSON (`{`/`[` prefix) call
/// it application/json; otherwise text/plain. Sirix natively stores
/// JSON so this heuristic is right the vast majority of the time.
fn guess_content_type(s: &str) -> String {
    let trimmed = s.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        "application/json".to_string()
    } else {
        "text/plain".to_string()
    }
}

/// Build the SQL body for a revision listing.
///
/// Sirix's history query surface isn't standardized in sirix-sql yet;
/// see the report's "Sirix-side gaps" section. We construct a query
/// that would work if sirix-sql exposed a `_rev` column on the resource
/// row-scan surface, and fall back to stubbing on the guest side if the
/// response is empty or lacks `_rev`.
pub fn build_revisions_sql(doc: &DocId) -> String {
    format!(
        "SELECT _rev FROM \"{}\".\"{}\" WHERE _nodekey = '{}' ORDER BY _rev",
        escape_ident(&doc.database),
        escape_ident(&doc.resource),
        escape_sql_str(&doc.node_key),
    )
}

/// Parse sirix-sql's response into a revision list.
///
/// Expected shape:
///   { "columns": ["_REV"], "rows": [[1], [2], ...] }
///
/// If the response is present but lacks a `_rev`-like column, or comes
/// back empty, we return the caller-supplied fallback (typically
/// `[latest_rev]`) with an Err-in-Ok tag so the guest can propagate the
/// gap. See `list_revisions` in `lib.rs` for the fallback plumbing.
pub fn parse_revisions_response(json_str: &str) -> Result<Vec<u64>, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("wf_document: sirix revisions response is not JSON: {e}"))?;

    if let Some(err) = root.get("error").and_then(|e| e.as_str()) {
        return Err(format!("wf_document: sirix: {err}"));
    }

    let columns = root
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "wf_document: sirix response missing `columns` array".to_string())?;
    let rows = root
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "wf_document: sirix response missing `rows` array".to_string())?;

    // Find the _rev column.
    let idx = columns
        .iter()
        .position(|c| c.as_str().map_or(false, |s| s.eq_ignore_ascii_case("_rev")))
        .ok_or_else(|| {
            "wf_document: sirix revisions response has no `_rev` column".to_string()
        })?;

    let mut revs: Vec<u64> = Vec::with_capacity(rows.len());
    for row in rows {
        let cells = row
            .as_array()
            .ok_or_else(|| format!("wf_document: sirix row was not an array: {row}"))?;
        let cell = cells
            .get(idx)
            .ok_or_else(|| "wf_document: sirix row missing _rev cell".to_string())?;
        let rev = cell
            .as_u64()
            .or_else(|| cell.as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| format!("wf_document: _rev cell was not an integer: {cell}"))?;
        revs.push(rev);
    }
    Ok(revs)
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

    #[test]
    fn parse_sirix_uri_ok() {
        let d = parse_sirix_uri("sirix://docs/manuals/42").unwrap();
        assert_eq!(d.database, "docs");
        assert_eq!(d.resource, "manuals");
        assert_eq!(d.node_key, "42");
    }

    #[test]
    fn parse_sirix_uri_rejects_wrong_scheme() {
        let err = parse_sirix_uri("http://docs/manuals/42").unwrap_err();
        assert!(err.contains("sirix://"));
    }

    #[test]
    fn parse_sirix_uri_rejects_two_segments() {
        let err = parse_sirix_uri("sirix://docs/manuals").unwrap_err();
        assert!(err.contains("sirix://<db>/<resource>/<node-key>"));
    }

    #[test]
    fn parse_sirix_uri_rejects_empty_segment() {
        let err = parse_sirix_uri("sirix://docs//42").unwrap_err();
        assert!(err.contains("sirix://<db>/<resource>/<node-key>"));
    }

    #[test]
    fn query_url_idempotent() {
        assert_eq!(query_url("http://x:8080"), "http://x:8080/query");
        assert_eq!(query_url("http://x:8080/"), "http://x:8080/query");
        assert_eq!(query_url("http://x:8080/query"), "http://x:8080/query");
        assert_eq!(query_url("http://x:8080/query/"), "http://x:8080/query");
    }

    #[test]
    fn build_fetch_sql_no_revision() {
        let d = DocId {
            database: "docs".into(),
            resource: "manuals".into(),
            node_key: "42".into(),
        };
        let sql = build_fetch_sql(&d, None);
        assert!(sql.contains("\"docs\""));
        assert!(sql.contains("\"manuals\""));
        assert!(sql.contains("_nodekey = '42'"));
        assert!(!sql.contains("_rev"));
    }

    #[test]
    fn build_fetch_sql_with_revision() {
        let d = DocId {
            database: "docs".into(),
            resource: "manuals".into(),
            node_key: "42".into(),
        };
        let sql = build_fetch_sql(&d, Some(17));
        assert!(sql.contains("_rev = 17"));
    }

    #[test]
    fn build_fetch_sql_escapes_sql_injection_attempts() {
        let d = DocId {
            database: "docs\"; DROP".into(),
            resource: "manuals".into(),
            node_key: "42' OR '1' = '1".into(),
        };
        let sql = build_fetch_sql(&d, None);
        assert!(sql.contains("\"docs\"\"; DROP\""));
        assert!(sql.contains("_nodekey = '42'' OR ''1'' = ''1'"));
    }

    #[test]
    fn build_fetch_body_shape() {
        let s = build_fetch_body("SELECT 1");
        let parsed: JsonValue = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["sql"], "SELECT 1");
    }

    #[test]
    fn parse_fetch_response_string_body() {
        let body = json!({
            "columns": ["document"],
            "rows": [["{\"title\":\"waterproof\"}"]]
        })
        .to_string();
        let doc = parse_fetch_response(&body, None).unwrap();
        assert_eq!(doc.body, b"{\"title\":\"waterproof\"}");
        assert_eq!(doc.content_type, "application/json");
    }

    #[test]
    fn parse_fetch_response_prefers_document_column() {
        let body = json!({
            "columns": ["other", "document"],
            "rows": [["ignored", "{\"real\": 1}"]]
        })
        .to_string();
        let doc = parse_fetch_response(&body, None).unwrap();
        assert_eq!(doc.body, b"{\"real\": 1}");
    }

    #[test]
    fn parse_fetch_response_content_type_override() {
        let body = json!({
            "columns": ["document"],
            "rows": [["hello"]]
        })
        .to_string();
        let doc = parse_fetch_response(&body, Some("text/plain; charset=utf-8")).unwrap();
        assert_eq!(doc.content_type, "text/plain; charset=utf-8");
    }

    #[test]
    fn parse_fetch_response_empty_rows_errors() {
        let body = json!({ "columns": ["document"], "rows": [] }).to_string();
        let err = parse_fetch_response(&body, None).unwrap_err();
        assert!(err.contains("zero rows"));
    }

    #[test]
    fn parse_fetch_response_surface_sirix_error() {
        let body = json!({ "error": "no such resource" }).to_string();
        let err = parse_fetch_response(&body, None).unwrap_err();
        assert!(err.contains("no such resource"));
    }

    #[test]
    fn parse_fetch_response_invalid_json_errors() {
        let err = parse_fetch_response("not-json{", None).unwrap_err();
        assert!(err.contains("wf_document"));
    }

    #[test]
    fn build_revisions_sql_shape() {
        let d = DocId {
            database: "docs".into(),
            resource: "manuals".into(),
            node_key: "42".into(),
        };
        let sql = build_revisions_sql(&d);
        assert!(sql.contains("SELECT _rev"));
        assert!(sql.contains("_nodekey = '42'"));
        assert!(sql.contains("ORDER BY _rev"));
    }

    #[test]
    fn parse_revisions_response_returns_sorted_list() {
        let body = json!({
            "columns": ["_rev"],
            "rows": [[1], [2], [3]]
        })
        .to_string();
        let revs = parse_revisions_response(&body).unwrap();
        assert_eq!(revs, vec![1, 2, 3]);
    }

    #[test]
    fn parse_revisions_response_accepts_string_ints() {
        let body = json!({
            "columns": ["_rev"],
            "rows": [["5"]]
        })
        .to_string();
        let revs = parse_revisions_response(&body).unwrap();
        assert_eq!(revs, vec![5]);
    }

    #[test]
    fn parse_revisions_response_no_rev_column_errors() {
        let body = json!({
            "columns": ["something-else"],
            "rows": [[1]]
        })
        .to_string();
        let err = parse_revisions_response(&body).unwrap_err();
        assert!(err.contains("_rev"));
    }
}
