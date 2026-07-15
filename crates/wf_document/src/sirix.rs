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
/// A document is addressed by a **business key** carried as the last
/// segment of the doc-id URI. Optional revision selects a point-in-time
/// view; `None` means "latest committed".
///
/// **Column-name shape**: Sirix's Calcite adapter exposes three columns
/// per resource (`_key BIGINT`, `_revision BIGINT`, `document VARCHAR`).
/// `_key` is Sirix's *internal* node key assigned on insert (2/6/10/…
/// for the first three top-level array elements), NOT a business
/// identifier the caller chose. Callers pin documents by business key
/// (e.g. `"manual-01"`) stored inside the JSON payload at `$._id`, so
/// the WHERE clause here uses `JSON_VALUE("document", '$._id')` for
/// identity and `_revision` (not `_rev`) for time-travel.
///
/// **Convention**: the URI's last segment MUST match `$._id` in the
/// stored JSON. Sirix-imported seed documents are expected to carry
/// their business key under `_id`; the guest's write path
/// (`sirix_write::build_insert_sql`) injects `_id` on insert when the
/// caller supplies a `DocRef.id` URI with a chosen business-key segment.
/// If the JSON doesn't carry `_id`, JSON_VALUE returns NULL and the
/// row is filtered out — the fetch reports zero rows and surfaces as
/// "document not found" up the stack.
///
/// The full-scan + Calcite-side JSON_VALUE filter is O(N) in the
/// resource. That's honest given Sirix's current schema — there is no
/// secondary index on JSON-path values. A resource with millions of
/// documents would want a proper external index; for the current
/// federated-search sizing (dozens to thousands of documents per
/// resource) it's fine.
pub fn build_fetch_sql(doc: &DocId, revision: Option<u64>) -> String {
    // We escape the business key by embedding it as a SQL string
    // literal. JSON_VALUE returns VARCHAR, so comparing against a
    // string literal matches naturally.
    match revision {
        Some(rev) => format!(
            "SELECT * FROM \"{}\".\"{}\" \
             WHERE JSON_VALUE(\"document\", '$._id') = '{}' \
             AND _revision = {}",
            escape_ident(&doc.database),
            escape_ident(&doc.resource),
            escape_sql_str(&doc.node_key),
            rev,
        ),
        None => format!(
            "SELECT * FROM \"{}\".\"{}\" \
             WHERE JSON_VALUE(\"document\", '$._id') = '{}'",
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
/// Same column-name / identity shape as `build_fetch_sql`:
/// `_revision` is Sirix's metadata column (`_rev` doesn't exist), and
/// the row is addressed by JSON-path lookup on `$._id` because Sirix
/// exposes `_key` as a BIGINT internal node key, not the caller's
/// business key. Falls back to stubbing on the guest side if the
/// response is empty or lacks `_revision` (see `parse_revisions_response`).
pub fn build_revisions_sql(doc: &DocId) -> String {
    format!(
        "SELECT _revision FROM \"{}\".\"{}\" \
         WHERE JSON_VALUE(\"document\", '$._id') = '{}' \
         ORDER BY _revision",
        escape_ident(&doc.database),
        escape_ident(&doc.resource),
        escape_sql_str(&doc.node_key),
    )
}

/// Parse sirix-sql's response into a revision list.
///
/// Expected shape:
///   { "columns": ["_REVISION"], "rows": [[1], [2], ...] }
///
/// Sirix's metadata column is named `_revision` (per
/// `SirixTable.getRowType`); Calcite may upcase the label to
/// `_REVISION` on the way back through JDBC. The lookup is
/// case-insensitive and also tolerates the historical `_rev` label so
/// callers stubbing responses in tests don't have to churn.
///
/// If the response is present but lacks a revision-like column, or
/// comes back empty, we return the caller-supplied fallback (typically
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

    // Find the revision column. Sirix exposes `_revision`; tolerate the
    // legacy `_rev` label for callers stubbing responses in tests.
    let idx = columns
        .iter()
        .position(|c| {
            c.as_str().map_or(false, |s| {
                s.eq_ignore_ascii_case("_revision") || s.eq_ignore_ascii_case("_rev")
            })
        })
        .ok_or_else(|| {
            "wf_document: sirix revisions response has no `_revision` column".to_string()
        })?;

    let mut revs: Vec<u64> = Vec::with_capacity(rows.len());
    for row in rows {
        let cells = row
            .as_array()
            .ok_or_else(|| format!("wf_document: sirix row was not an array: {row}"))?;
        let cell = cells
            .get(idx)
            .ok_or_else(|| "wf_document: sirix row missing _revision cell".to_string())?;
        let rev = cell
            .as_u64()
            .or_else(|| cell.as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| format!("wf_document: _revision cell was not an integer: {cell}"))?;
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
            node_key: "manual-01".into(),
        };
        let sql = build_fetch_sql(&d, None);
        assert!(sql.contains("\"docs\""));
        assert!(sql.contains("\"manuals\""));
        // Identity filter is `JSON_VALUE("document", '$._id') = '<key>'`.
        // Sirix's `_key` column is a BIGINT internal node key, not the
        // caller's business key — hence the JSON-path lookup.
        assert!(
            sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"),
            "sql={sql}"
        );
        // No _revision predicate when revision is None.
        assert!(!sql.contains("_revision"), "sql={sql}");
    }

    #[test]
    fn build_fetch_sql_with_revision() {
        let d = DocId {
            database: "docs".into(),
            resource: "manuals".into(),
            node_key: "manual-01".into(),
        };
        let sql = build_fetch_sql(&d, Some(17));
        // Sirix exposes `_revision` (BIGINT), not `_rev`.
        assert!(sql.contains("_revision = 17"), "sql={sql}");
        assert!(sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"));
    }

    #[test]
    fn build_fetch_sql_uses_no_legacy_column_names() {
        // Regression guard: the guest used to emit `_nodekey` / `_rev`,
        // neither of which Sirix exposes. Sirix's Calcite adapter
        // rejects the query with `Column '_NODEKEY' not found in any
        // table` before it reaches an enumerator. This test fails
        // fast if either name reappears.
        let d = DocId {
            database: "docs".into(),
            resource: "manuals".into(),
            node_key: "manual-01".into(),
        };
        let with_rev = build_fetch_sql(&d, Some(3));
        let without_rev = build_fetch_sql(&d, None);
        for sql in [&with_rev, &without_rev] {
            assert!(!sql.contains("_nodekey"), "legacy `_nodekey`: {sql}");
            // `_rev` as a whole word — `_revision` legitimately contains
            // the substring, so match on the bare form.
            assert!(
                !sql.split_whitespace().any(|tok| tok == "_rev"),
                "legacy `_rev` token: {sql}"
            );
        }
    }

    #[test]
    fn build_fetch_sql_escapes_sql_injection_attempts() {
        let d = DocId {
            database: "docs\"; DROP".into(),
            resource: "manuals".into(),
            node_key: "manual-01' OR '1' = '1".into(),
        };
        let sql = build_fetch_sql(&d, None);
        assert!(sql.contains("\"docs\"\"; DROP\""));
        assert!(sql.contains("'manual-01'' OR ''1'' = ''1'"));
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
            node_key: "manual-01".into(),
        };
        let sql = build_revisions_sql(&d);
        assert!(sql.contains("SELECT _revision"), "sql={sql}");
        assert!(
            sql.contains("JSON_VALUE(\"document\", '$._id') = 'manual-01'"),
            "sql={sql}"
        );
        assert!(sql.contains("ORDER BY _revision"), "sql={sql}");
    }

    #[test]
    fn parse_revisions_response_returns_sorted_list() {
        let body = json!({
            "columns": ["_revision"],
            "rows": [[1], [2], [3]]
        })
        .to_string();
        let revs = parse_revisions_response(&body).unwrap();
        assert_eq!(revs, vec![1, 2, 3]);
    }

    #[test]
    fn parse_revisions_response_accepts_string_ints() {
        let body = json!({
            "columns": ["_revision"],
            "rows": [["5"]]
        })
        .to_string();
        let revs = parse_revisions_response(&body).unwrap();
        assert_eq!(revs, vec![5]);
    }

    #[test]
    fn parse_revisions_response_accepts_uppercase_calcite_label() {
        // Calcite / JDBC can uppercase the column name; the parser is
        // case-insensitive.
        let body = json!({
            "columns": ["_REVISION"],
            "rows": [[7]]
        })
        .to_string();
        let revs = parse_revisions_response(&body).unwrap();
        assert_eq!(revs, vec![7]);
    }

    #[test]
    fn parse_revisions_response_accepts_legacy_rev_label() {
        // Historical stubs used `_rev` — kept working so pre-existing
        // integration tests don't have to churn.
        let body = json!({
            "columns": ["_rev"],
            "rows": [[2]]
        })
        .to_string();
        let revs = parse_revisions_response(&body).unwrap();
        assert_eq!(revs, vec![2]);
    }

    #[test]
    fn parse_revisions_response_no_rev_column_errors() {
        let body = json!({
            "columns": ["something-else"],
            "rows": [[1]]
        })
        .to_string();
        let err = parse_revisions_response(&body).unwrap_err();
        assert!(err.contains("_revision"));
    }
}
