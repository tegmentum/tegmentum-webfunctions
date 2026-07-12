//! Document-mirror reconciliation.
//!
//! §08 of `wf-conformance/docs/design/wf-document.md` — periodic sweep
//! that keeps Manticore mirroring the latest committed state of every
//! Sirix document per `DocumentRegistry` entry (Managed mode only).
//!
//! Contract:
//!
//! * Input: a list of `DocumentIndexConfig` entries plus three "bridge"
//!   traits — `HttpBridge` (reused from `fulltext_sweep`; POSTs NDJSON
//!   to Manticore's `/bulk`), `SirixBridge` (executes SQL against
//!   sirix-sql-server), and `DocSinkBridge` (persists the known-keys
//!   tracker). Bridges are trait objects so tests can substitute
//!   in-memory or TcpListener-backed mocks.
//!
//! * State: per-index known-keys tracker persisted in the same sink
//!   SQLite the alias table lives in. Schema:
//!
//!     `wf_doc_keys_<sanitized-index-name>` (
//!         doc_uri       TEXT PRIMARY KEY,
//!         last_seen_rev INTEGER NOT NULL,
//!         doc_hash      TEXT NOT NULL,
//!         updated_at    INTEGER NOT NULL
//!     )
//!
//!   FNV-1a doc_hash on the body lets unchanged docs skip re-insert on
//!   subsequent sweeps. `last_seen_rev` is captured too so a future
//!   sirix-sql `_rev`-filterable endpoint can drive the query
//!   incrementally without changing the tracker schema.
//!
//! * Sirix read: `sirix-sql-server` today exposes only ad-hoc `POST
//!   /query` with a full SQL body — no changes-since endpoint. The
//!   sweep issues a full-resource SELECT and diffs client-side. Noted
//!   as a v0.2 perf gap; a future sirix-sql endpoint that exposes
//!   `_rev` as a filterable column collapses this to O(delta).
//!
//! * Wire format: `HttpBridge::post_json` speaks the Manticore `/bulk`
//!   NDJSON protocol. Same shape as `fulltext_sweep` — one
//!   `{ "replace": { "index": "<name>", "id": "<sirix-uri>", "doc":
//!   {...} } }` line per document, one `{ "delete": { "index":
//!   "<name>", "id": "<sirix-uri>" } }` line per tombstone.
//!
//! * Retention: v0.2 supports only `revision_retention: "latest"` —
//!   Manticore holds one row per URI (its `_id`), historical revisions
//!   stay in Sirix. Time-travel search waits for v1.0 (memo §10).
//!
//! * Rationale for talking to Manticore directly rather than routing
//!   through the wf_document guest's admin exports: at the time the
//!   sweep was implemented wf_document was still being built by a
//!   sibling agent; the admin-export surface (`insert-batch` /
//!   `delete-batch`, same shape as wf_fulltext) is expected to land
//!   later. Once it does, this bridge can route through the guest
//!   surface without a wire-format change — the NDJSON body stays the
//!   same. Revisit when wf_document ships.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value as JsonValue;

pub use crate::fulltext_sweep::{sanitize_index_name, HttpBridge};

/// Per-entry configuration parsed from the outer wf_canonicalize
/// config JSON. Mirrors the Managed-mode fields on the oxigraph-wf
/// `DocumentRegistry` entry (memo §07).
///
/// Federated-mode entries never reach this shape — the outer
/// oxigraph-wf handler filters them out before serializing config for
/// the sweep. Same rationale as fulltext-sweep's exclusion of
/// document-corpus entries: the substrate is a pure client for
/// Federated entries and has nothing to reconcile.
#[derive(Debug, Clone, Deserialize)]
pub struct DocumentIndexConfig {
    /// Registry entry name (for logging + known-keys table naming).
    pub name: String,
    /// Bare host[:port] of the Manticore search backend
    /// (e.g. `http://localhost:9308`).
    pub search_backend: String,
    /// Bare host[:port] of sirix-sql-server
    /// (e.g. `http://localhost:8080`).
    pub storage_backend: String,
    /// Backend-side Manticore index name.
    pub search_index: String,
    /// Sirix database name.
    pub sirix_database: String,
    /// Sirix resource name.
    pub sirix_resource: String,
    /// How often the sweep should reconcile this entry. `None` = the
    /// canonicalize invocation's cadence (whichever operator drives
    /// wf:call). v0.2 always reconciles on every sweep invocation and
    /// leaves the sweep-scheduling cadence to the caller.
    #[serde(default)]
    pub sweep_interval_secs: Option<u32>,
    /// v0.2 only accepts `"latest"`. Anything else is a config error
    /// on the outer oxigraph-wf side; if it slips through we still
    /// mirror only the latest revision.
    #[serde(default = "default_retention")]
    pub revision_retention: String,
}

fn default_retention() -> String {
    "latest".into()
}

/// Per-sweep counts. `unchanged` reports docs whose FNV hash matched
/// the tracker so the sweep skipped their Manticore round-trip — this
/// is the number the operator watches to gauge whether sweeps are
/// paying off (high `unchanged` == low churn == cheap sweep).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepResult {
    pub inserted: u64,
    pub deleted: u64,
    pub unchanged: u64,
    pub errors: u64,
}

impl SweepResult {
    fn add(&mut self, other: SweepResult) {
        self.inserted = self.inserted.saturating_add(other.inserted);
        self.deleted = self.deleted.saturating_add(other.deleted);
        self.unchanged = self.unchanged.saturating_add(other.unchanged);
        self.errors = self.errors.saturating_add(other.errors);
    }
}

/// One document row read from sirix-sql-server. `body` is the
/// serialized document content — Sirix stores JSON natively so we
/// re-serialize with `serde_json::Value::to_string` for the hash and
/// the mirror body alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SirixDocRow {
    pub node_key: String,
    pub revision: u64,
    pub body: String,
    pub content_type: String,
}

/// Persisted known-keys row for a single doc URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownDoc {
    pub last_seen_rev: u64,
    pub doc_hash: String,
}

/// Sirix-read bridge. Guest-side impl POSTs `{"sql": "SELECT ..."}`
/// to sirix-sql-server via the same `http-post-json` host import
/// Manticore uses. Tests substitute a TcpListener-backed mock (or an
/// in-memory canned response).
///
/// `since_rev` is a forward-compatible hook — v0.2 always passes
/// `None` because sirix-sql-server has no `_rev`-filterable surface
/// today, so we full-scan and diff client-side. When that surface
/// lands the sweep passes `Some(last_seen_rev)` here and Sirix does
/// the filtering server-side. The trait shape stays stable across
/// that migration.
pub trait SirixBridge {
    fn list_documents(
        &self,
        sirix_url: &str,
        database: &str,
        resource: &str,
        since_rev: Option<u64>,
    ) -> Result<Vec<SirixDocRow>, String>;
}

/// Persistence bridge for the doc known-keys tracker. Kept separate
/// from `fulltext_sweep::SinkBridge` because the row shape carries an
/// extra column (`last_seen_rev`) — cleaner to grow via a sibling
/// trait than to widen the fulltext one and force every consumer to
/// carry the doc-specific fields around.
pub trait DocSinkBridge {
    fn ensure_doc_table(&self, table: &str) -> Result<(), String>;
    fn load_known_docs(&self, table: &str) -> Result<HashMap<String, KnownDoc>, String>;
    fn upsert_doc(
        &self,
        table: &str,
        doc_uri: &str,
        entry: &KnownDoc,
    ) -> Result<(), String>;
    fn delete_doc(&self, table: &str, doc_uri: &str) -> Result<(), String>;
}

/// A single document to mirror into Manticore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocMirror {
    pub uri: String,
    pub revision: u64,
    pub body: String,
    pub content_type: String,
    pub hash: String,
}

/// The pure-function view of the sweep — takes bridges and configs,
/// returns aggregated counts. Errors on any single entry are logged
/// and bump `errors`; the sweep never crashes the outer wf_canonicalize
/// pass, so a briefly-unreachable Sirix or Manticore doesn't block the
/// alias-reconcile phase.
pub fn run<H, R, S>(
    entries: &[DocumentIndexConfig],
    http: &H,
    sirix: &R,
    sink: &S,
) -> SweepResult
where
    H: HttpBridge,
    R: SirixBridge,
    S: DocSinkBridge,
{
    let mut total = SweepResult::default();
    for entry in entries {
        match run_one(entry, http, sirix, sink) {
            Ok(c) => total.add(c),
            Err(msg) => {
                eprintln!(
                    "wf_canonicalize.document_sweep: entry `{}`: {}",
                    entry.name, msg
                );
                total.errors = total.errors.saturating_add(1);
            }
        }
    }
    total
}

fn run_one<H: HttpBridge, R: SirixBridge, S: DocSinkBridge>(
    entry: &DocumentIndexConfig,
    http: &H,
    sirix: &R,
    sink: &S,
) -> Result<SweepResult, String> {
    // 1. Ensure the tracker table exists (idempotent).
    let table = format!("wf_doc_keys_{}", sanitize_index_name(&entry.name));
    sink.ensure_doc_table(&table)?;

    // 2. Load the previously-known docs first — we need them both for
    //    the diff and to decide the `since_rev` hint we could pass to
    //    Sirix (unused in v0.2 because sirix-sql lacks _rev filtering).
    let known = sink.load_known_docs(&table)?;

    // 3. Full-scan of the resource from Sirix (v0.2 gap: no
    //    changes-since endpoint on sirix-sql-server; noted in the memo
    //    §08 sync semantics). `since_rev: None` = "everything".
    let rows = sirix
        .list_documents(
            &entry.storage_backend,
            &entry.sirix_database,
            &entry.sirix_resource,
            None,
        )
        .map_err(|e| format!("sirix list_documents: {e}"))?;

    // 4. Build the set of URIs currently in Sirix + their mirror
    //    payloads, and diff against `known`.
    let (to_insert, unchanged) = build_diff(entry, &rows, &known);
    let mut to_delete = compute_deletes(&rows, &known, entry);
    to_delete.sort();

    let mut counts = SweepResult {
        inserted: 0,
        deleted: 0,
        unchanged,
        errors: 0,
    };

    // 5. Emit inserts. Same NDJSON `/bulk` shape as fulltext_sweep.
    if !to_insert.is_empty() {
        let body = build_bulk_body(&entry.search_index, &to_insert);
        let url = bulk_url(&entry.search_backend);
        match http.post_json(&url, &body) {
            Ok(response) => match bulk_response_ok(&response) {
                Ok(()) => {
                    counts.inserted = to_insert.len() as u64;
                    for m in &to_insert {
                        sink.upsert_doc(
                            &table,
                            &m.uri,
                            &KnownDoc {
                                last_seen_rev: m.revision,
                                doc_hash: m.hash.clone(),
                            },
                        )?;
                    }
                }
                Err(e) => {
                    counts.errors += 1;
                    eprintln!(
                        "wf_canonicalize.document_sweep: entry `{}`: insert response: {e}",
                        entry.name
                    );
                }
            },
            Err(e) => {
                counts.errors += 1;
                eprintln!(
                    "wf_canonicalize.document_sweep: entry `{}`: insert POST: {e}",
                    entry.name
                );
            }
        }
    }

    // 6. Emit deletes.
    if !to_delete.is_empty() {
        let body = build_delete_body(&entry.search_index, &to_delete);
        let url = bulk_url(&entry.search_backend);
        match http.post_json(&url, &body) {
            Ok(response) => match bulk_response_ok(&response) {
                Ok(()) => {
                    counts.deleted = to_delete.len() as u64;
                    for uri in &to_delete {
                        sink.delete_doc(&table, uri)?;
                    }
                }
                Err(e) => {
                    counts.errors += 1;
                    eprintln!(
                        "wf_canonicalize.document_sweep: entry `{}`: delete response: {e}",
                        entry.name
                    );
                }
            },
            Err(e) => {
                counts.errors += 1;
                eprintln!(
                    "wf_canonicalize.document_sweep: entry `{}`: delete POST: {e}",
                    entry.name
                );
            }
        }
    }

    Ok(counts)
}

/// Construct the doc URI in the memo §06 shape:
/// `sirix://<db>/<resource>/<node-key>`. Broken out so
/// `document_urls_use_sirix_scheme` can assert the exact shape without
/// re-deriving it.
pub fn build_doc_uri(database: &str, resource: &str, node_key: &str) -> String {
    format!("sirix://{database}/{resource}/{node_key}")
}

/// Compute (docs-to-insert-or-update, unchanged-count) from the Sirix
/// full-scan against the known-keys tracker. Sorted output for
/// deterministic tests.
fn build_diff(
    entry: &DocumentIndexConfig,
    rows: &[SirixDocRow],
    known: &HashMap<String, KnownDoc>,
) -> (Vec<DocMirror>, u64) {
    let mut to_insert: Vec<DocMirror> = Vec::new();
    let mut unchanged: u64 = 0;
    for row in rows {
        let uri = build_doc_uri(&entry.sirix_database, &entry.sirix_resource, &row.node_key);
        let hash = fnv1a64_hex(&row.body);
        match known.get(&uri) {
            Some(prev) if prev.doc_hash == hash => {
                unchanged += 1;
            }
            _ => {
                to_insert.push(DocMirror {
                    uri,
                    revision: row.revision,
                    body: row.body.clone(),
                    content_type: row.content_type.clone(),
                    hash,
                });
            }
        }
    }
    to_insert.sort_by(|a, b| a.uri.cmp(&b.uri));
    (to_insert, unchanged)
}

/// Compute the list of URIs to delete from Manticore: URIs the sweep
/// has seen before but that no longer appear in the current Sirix
/// scan. Since v0.2 does a full scan, "not in current scan" is exactly
/// "not in the current-committed state of the resource".
fn compute_deletes(
    rows: &[SirixDocRow],
    known: &HashMap<String, KnownDoc>,
    entry: &DocumentIndexConfig,
) -> Vec<String> {
    let seen: HashSet<String> = rows
        .iter()
        .map(|r| build_doc_uri(&entry.sirix_database, &entry.sirix_resource, &r.node_key))
        .collect();
    known
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect()
}

/// Build the NDJSON body for a batch of `replace` ops into Manticore.
/// Doc payload carries `_uri`, `_rev`, `body`, and `content_type` — an
/// index schema that mirrors the memo §06 doc-ref surface. The `_id`
/// on the outer `replace` envelope is the same `sirix://` URI so
/// re-sending the same doc is idempotent.
pub fn build_bulk_body(index: &str, docs: &[DocMirror]) -> String {
    use serde_json::{json, Map, Value as J};
    let mut out = String::new();
    for doc in docs {
        let mut doc_obj = Map::new();
        doc_obj.insert("_uri".into(), J::String(doc.uri.clone()));
        doc_obj.insert("_rev".into(), J::Number(doc.revision.into()));
        doc_obj.insert("body".into(), J::String(doc.body.clone()));
        doc_obj.insert(
            "content_type".into(),
            J::String(doc.content_type.clone()),
        );
        let line = json!({
            "replace": {
                "index": index,
                "id": doc.uri,
                "doc": J::Object(doc_obj),
            }
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// Build the NDJSON body for a batch of `delete` ops. One line per URI.
pub fn build_delete_body(index: &str, ids: &[String]) -> String {
    use serde_json::json;
    let mut out = String::new();
    for id in ids {
        let line = json!({
            "delete": {
                "index": index,
                "id": id,
            }
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// Idempotent Manticore `/bulk` URL construction. If the operator
/// already supplied a `/bulk`-suffixed URL don't double it up.
pub fn bulk_url(backend_url: &str) -> String {
    let trimmed = backend_url.trim_end_matches('/');
    if trimmed.ends_with("/bulk") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/bulk")
    }
}

/// Sirix-sql-server `/query` URL construction. Idempotent, matching
/// `bulk_url`'s shape. Kept adjacent so the sweep's URL handling for
/// both backends reads as one paragraph.
pub fn sirix_query_url(sirix_url: &str) -> String {
    let trimmed = sirix_url.trim_end_matches('/');
    if trimmed.ends_with("/query") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/query")
    }
}

/// Minimal Manticore `/bulk` response check. Same rationale as in
/// `fulltext_sweep::bulk_response_ok`: the sweep just needs "batch
/// landed" vs. "batch had errors"; the full parser lives in
/// wf_fulltext.
fn bulk_response_ok(body: &str) -> Result<(), String> {
    let v: JsonValue = serde_json::from_str(body)
        .map_err(|e| format!("bulk response not JSON: {e}"))?;
    if v.get("errors").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Err(format!(
            "backend reported errors: {}",
            v.get("items")
                .map(|it| it.to_string())
                .unwrap_or_else(|| "<no items>".into())
        ));
    }
    Ok(())
}

/// SirixBridge helper — build the `POST /query` body for the
/// full-resource scan. Kept adjacent so the guest bridge impl and the
/// test mocks can both call it (avoids drift between "what the sweep
/// asks for" and "what the mock knows how to answer").
pub fn build_scan_sql(database: &str, resource: &str, since_rev: Option<u64>) -> String {
    // Sirix-sql exposes the resource as a virtual table under
    // "<db>"."<resource>". `_nodekey`, `_rev`, and `document` are the
    // implicit metadata columns (per wf_document::sirix::build_fetch_sql
    // — same three columns we lean on for a fetch by node-key).
    match since_rev {
        Some(rev) => format!(
            "SELECT _nodekey, _rev, document FROM \"{db}\".\"{res}\" \
             WHERE _rev > {rev}",
            db = escape_ident(database),
            res = escape_ident(resource),
        ),
        None => format!(
            "SELECT _nodekey, _rev, document FROM \"{db}\".\"{res}\"",
            db = escape_ident(database),
            res = escape_ident(resource),
        ),
    }
}

/// Build the JSON body sirix-sql-server expects on `POST /query`.
pub fn build_query_body(sql: &str) -> String {
    serde_json::json!({ "sql": sql }).to_string()
}

/// Parse sirix-sql-server's `{"columns":[...],"rows":[[...],...]}`
/// response into a `Vec<SirixDocRow>`. Case-insensitive column-name
/// lookup so a Sirix-side rename between `_NODEKEY` / `_nodekey`
/// doesn't break the sweep.
pub fn parse_scan_response(json_str: &str) -> Result<Vec<SirixDocRow>, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("sirix response is not JSON: {e}"))?;
    if let Some(err) = root.get("error").and_then(|e| e.as_str()) {
        return Err(format!("sirix: {err}"));
    }
    let columns = root
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "sirix response missing `columns` array".to_string())?;
    let rows = root
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "sirix response missing `rows` array".to_string())?;

    let column_names: Vec<String> = columns
        .iter()
        .map(|c| c.as_str().unwrap_or("").to_string())
        .collect();
    let idx_of = |candidates: &[&str]| -> Option<usize> {
        candidates.iter().find_map(|want| {
            column_names
                .iter()
                .position(|c| c.eq_ignore_ascii_case(want))
        })
    };
    let nodekey_idx = idx_of(&["_nodekey", "nodekey", "_key"])
        .ok_or_else(|| "sirix response missing _nodekey column".to_string())?;
    let rev_idx = idx_of(&["_rev", "rev", "_revision", "revision"])
        .ok_or_else(|| "sirix response missing _rev column".to_string())?;
    let doc_idx = idx_of(&["document", "body", "content", "json"]);

    let mut out: Vec<SirixDocRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let cells = row
            .as_array()
            .ok_or_else(|| format!("sirix row was not an array: {row}"))?;
        let nk = cell_to_string(cells.get(nodekey_idx))?;
        let rev = cells
            .get(rev_idx)
            .and_then(|c| c.as_u64().or_else(|| c.as_str().and_then(|s| s.parse().ok())))
            .ok_or_else(|| "sirix _rev cell was not an integer".to_string())?;
        // Doc content: prefer the named column, else first non-metadata cell.
        let (body, content_type) = match doc_idx.and_then(|i| cells.get(i)) {
            Some(cell) => cell_body_and_ct(cell),
            None => {
                // Fallback: first cell that's not the nodekey / rev slot.
                let fallback = cells.iter().enumerate().find_map(|(i, c)| {
                    if i == nodekey_idx || i == rev_idx {
                        None
                    } else if c.is_null() {
                        None
                    } else {
                        Some(c)
                    }
                });
                match fallback {
                    Some(c) => cell_body_and_ct(c),
                    None => (String::new(), "application/octet-stream".to_string()),
                }
            }
        };
        out.push(SirixDocRow {
            node_key: nk,
            revision: rev,
            body,
            content_type,
        });
    }
    Ok(out)
}

fn cell_to_string(cell: Option<&JsonValue>) -> Result<String, String> {
    match cell {
        Some(JsonValue::String(s)) => Ok(s.clone()),
        Some(JsonValue::Number(n)) => Ok(n.to_string()),
        Some(JsonValue::Null) | None => {
            Err("sirix cell was null when a value was required".into())
        }
        Some(other) => Ok(other.to_string()),
    }
}

fn cell_body_and_ct(cell: &JsonValue) -> (String, String) {
    match cell {
        JsonValue::String(s) => {
            let trimmed = s.trim_start();
            let ct = if trimmed.starts_with('{') || trimmed.starts_with('[') {
                "application/json"
            } else {
                "text/plain"
            };
            (s.clone(), ct.to_string())
        }
        JsonValue::Null => (String::new(), "application/octet-stream".to_string()),
        other => (other.to_string(), "application/json".to_string()),
    }
}

fn escape_ident(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// FNV-1a 64-bit hash rendered as a lowercase 16-char hex string. Same
/// primitive `fulltext_sweep` uses — we're just detecting change, not
/// resisting adversaries.
fn fnv1a64_hex(s: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    // -----------------------------------------------------------------
    // In-memory mocks
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct MockHttp {
        posts: RefCell<Vec<(String, String)>>, // (url, body)
        response: String,
    }
    impl HttpBridge for MockHttp {
        fn post_json(&self, url: &str, body: &str) -> Result<String, String> {
            self.posts
                .borrow_mut()
                .push((url.to_string(), body.to_string()));
            Ok(self.response.clone())
        }
    }

    struct MockSirix {
        rows: RefCell<Vec<SirixDocRow>>,
    }
    impl SirixBridge for MockSirix {
        fn list_documents(
            &self,
            _sirix_url: &str,
            _database: &str,
            _resource: &str,
            _since_rev: Option<u64>,
        ) -> Result<Vec<SirixDocRow>, String> {
            Ok(self.rows.borrow().clone())
        }
    }

    #[derive(Default)]
    struct MockDocSink {
        rows: RefCell<HashMap<(String, String), KnownDoc>>, // (table, uri) -> KnownDoc
    }
    impl DocSinkBridge for MockDocSink {
        fn ensure_doc_table(&self, _table: &str) -> Result<(), String> {
            Ok(())
        }
        fn load_known_docs(
            &self,
            table: &str,
        ) -> Result<HashMap<String, KnownDoc>, String> {
            Ok(self
                .rows
                .borrow()
                .iter()
                .filter_map(|((t, uri), k)| {
                    if t == table {
                        Some((uri.clone(), k.clone()))
                    } else {
                        None
                    }
                })
                .collect())
        }
        fn upsert_doc(
            &self,
            table: &str,
            doc_uri: &str,
            entry: &KnownDoc,
        ) -> Result<(), String> {
            self.rows
                .borrow_mut()
                .insert((table.to_string(), doc_uri.to_string()), entry.clone());
            Ok(())
        }
        fn delete_doc(&self, table: &str, doc_uri: &str) -> Result<(), String> {
            self.rows
                .borrow_mut()
                .remove(&(table.to_string(), doc_uri.to_string()));
            Ok(())
        }
    }

    fn ok_response() -> String {
        json!({
            "items": [{ "replace": { "_id": "x", "result": "created" } }],
            "errors": false
        })
        .to_string()
    }

    fn cfg(name: &str) -> DocumentIndexConfig {
        DocumentIndexConfig {
            name: name.into(),
            search_backend: "http://localhost:9308".into(),
            storage_backend: "http://localhost:8080".into(),
            search_index: name.into(),
            sirix_database: "docs".into(),
            sirix_resource: name.into(),
            sweep_interval_secs: None,
            revision_retention: "latest".into(),
        }
    }

    // -----------------------------------------------------------------
    // URL shape
    // -----------------------------------------------------------------

    #[test]
    fn document_urls_use_sirix_scheme() {
        let uri = build_doc_uri("docs", "manuals", "42");
        assert_eq!(uri, "sirix://docs/manuals/42");
        // And through the diff path — the URI shape reaches Manticore.
        let entry = cfg("manuals");
        let rows = vec![SirixDocRow {
            node_key: "42".into(),
            revision: 1,
            body: "{\"title\":\"widget\"}".into(),
            content_type: "application/json".into(),
        }];
        let (to_insert, _) = build_diff(&entry, &rows, &HashMap::new());
        assert_eq!(to_insert.len(), 1);
        assert_eq!(to_insert[0].uri, "sirix://docs/manuals/42");
        // Bulk body carries it through as both _id and doc._uri.
        let body = build_bulk_body("manuals", &to_insert);
        let line = body.trim_end_matches('\n');
        let parsed: JsonValue = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["replace"]["id"], "sirix://docs/manuals/42");
        assert_eq!(parsed["replace"]["doc"]["_uri"], "sirix://docs/manuals/42");
    }

    // -----------------------------------------------------------------
    // Diff logic
    // -----------------------------------------------------------------

    #[test]
    fn hash_prevents_reinsert_of_unchanged_doc() {
        let entry = cfg("manuals");
        let sirix = MockSirix {
            rows: RefCell::new(vec![SirixDocRow {
                node_key: "42".into(),
                revision: 3,
                body: "{\"title\":\"widget\"}".into(),
                content_type: "application/json".into(),
            }]),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();

        let r1 = run(&[entry.clone()], &http, &sirix, &sink);
        assert_eq!(r1.inserted, 1, "gen0 should insert");
        assert_eq!(r1.unchanged, 0);
        assert_eq!(r1.deleted, 0);
        assert_eq!(r1.errors, 0);

        // Reset the http bridge (posts vec) but keep the same sink /
        // sirix state, then rerun.
        let http2 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let r2 = run(&[entry], &http2, &sirix, &sink);
        assert_eq!(r2.inserted, 0);
        assert_eq!(r2.unchanged, 1);
        assert_eq!(r2.deleted, 0);
        assert_eq!(r2.errors, 0);
        assert!(
            http2.posts.borrow().is_empty(),
            "no POSTs when everything is unchanged"
        );
    }

    #[test]
    fn sweep_emits_delete_for_removed_doc() {
        let entry = cfg("manuals");
        // Gen 0: two docs.
        let sirix_v0 = MockSirix {
            rows: RefCell::new(vec![
                SirixDocRow {
                    node_key: "42".into(),
                    revision: 1,
                    body: "{\"title\":\"widget\"}".into(),
                    content_type: "application/json".into(),
                },
                SirixDocRow {
                    node_key: "43".into(),
                    revision: 1,
                    body: "{\"title\":\"gadget\"}".into(),
                    content_type: "application/json".into(),
                },
            ]),
        };
        let http_v0 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();
        let r0 = run(&[entry.clone()], &http_v0, &sirix_v0, &sink);
        assert_eq!(r0.inserted, 2);
        assert_eq!(r0.deleted, 0);

        // Gen 1: doc 43 is gone from Sirix.
        let sirix_v1 = MockSirix {
            rows: RefCell::new(vec![SirixDocRow {
                node_key: "42".into(),
                revision: 1,
                body: "{\"title\":\"widget\"}".into(),
                content_type: "application/json".into(),
            }]),
        };
        let http_v1 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let r1 = run(&[entry], &http_v1, &sirix_v1, &sink);
        assert_eq!(r1.inserted, 0);
        assert_eq!(r1.deleted, 1);
        assert_eq!(r1.unchanged, 1);
        // The delete request went out with the right URI shape.
        let posts = http_v1.posts.borrow();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].1.contains("\"delete\""));
        assert!(posts[0].1.contains("sirix://docs/manuals/43"));
    }

    #[test]
    fn diff_detects_updated_doc_via_hash_change() {
        let entry = cfg("manuals");
        let mut known = HashMap::new();
        known.insert(
            "sirix://docs/manuals/42".to_string(),
            KnownDoc {
                last_seen_rev: 1,
                doc_hash: fnv1a64_hex("{\"title\":\"widget\"}"),
            },
        );
        let rows = vec![SirixDocRow {
            node_key: "42".into(),
            revision: 2,
            body: "{\"title\":\"widget-v2\"}".into(),
            content_type: "application/json".into(),
        }];
        let (to_insert, unchanged) = build_diff(&entry, &rows, &known);
        assert_eq!(unchanged, 0);
        assert_eq!(to_insert.len(), 1);
        assert_eq!(to_insert[0].revision, 2);
    }

    // -----------------------------------------------------------------
    // SQL construction + response parsing
    // -----------------------------------------------------------------

    #[test]
    fn scan_sql_full_and_since_rev() {
        let full = build_scan_sql("docs", "manuals", None);
        assert_eq!(
            full,
            "SELECT _nodekey, _rev, document FROM \"docs\".\"manuals\""
        );
        let since = build_scan_sql("docs", "manuals", Some(7));
        assert!(since.ends_with("WHERE _rev > 7"));
    }

    #[test]
    fn scan_response_parser_pulls_columns() {
        let body = json!({
            "columns": ["_nodekey", "_rev", "document"],
            "rows": [
                ["42", 1, "{\"title\":\"widget\"}"],
                ["43", 2, {"title": "gadget"}]
            ]
        })
        .to_string();
        let rows = parse_scan_response(&body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].node_key, "42");
        assert_eq!(rows[0].revision, 1);
        assert_eq!(rows[0].body, "{\"title\":\"widget\"}");
        assert_eq!(rows[1].node_key, "43");
        assert_eq!(rows[1].revision, 2);
        // Object cells get serialized back to JSON with application/json.
        assert!(rows[1].body.contains("gadget"));
        assert_eq!(rows[1].content_type, "application/json");
    }

    #[test]
    fn scan_response_parser_case_insensitive_columns() {
        let body = json!({
            "columns": ["_NODEKEY", "_REV", "DOCUMENT"],
            "rows": [["1", 1, "hello"]]
        })
        .to_string();
        let rows = parse_scan_response(&body).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node_key, "1");
        assert_eq!(rows[0].revision, 1);
        assert_eq!(rows[0].body, "hello");
    }

    // -----------------------------------------------------------------
    // End-to-end sweep against TcpListener-backed Sirix + Manticore
    // -----------------------------------------------------------------

    /// Real-HTTP sweep — spins two TcpListeners (one for Manticore's
    /// `/bulk` and one for sirix-sql-server's `/query`), runs the sweep
    /// twice with a delta between the two runs (a new doc committed to
    /// Sirix), and asserts:
    ///   * gen0 inserts both docs
    ///   * gen1 inserts only the new doc; hash-match skips the existing
    ///     one and no delete goes out
    ///   * the doc URI shape hits the wire as `sirix://<db>/<res>/<key>`
    #[test]
    fn full_sweep_two_generations_against_mock_backend() {
        let entry = cfg("manuals");

        // Manticore mock: accept a single POST /bulk, capture the body,
        // reply with a canned success. Keep a channel of received bodies.
        let manticore_bodies: Arc<Mutex<Vec<String>>> =
            Arc::new(Mutex::new(Vec::new()));
        let manticore_url = spawn_tcp_echo_server(
            Arc::clone(&manticore_bodies),
            move |_body| {
                json!({
                    "items": [{ "replace": { "_id": "x", "result": "created" } }],
                    "errors": false
                })
                .to_string()
            },
        );

        // Sirix mock: a slot for the currently-committed rows. Each
        // sweep POSTs a SQL SELECT and we reply with the current slot.
        let sirix_rows: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![
            json!(["42", 1, "{\"title\":\"widget\"}"]),
        ]));
        let sirix_url = spawn_sirix_server(Arc::clone(&sirix_rows));

        // Bridges.
        let http = TcpHttpBridge;
        let sirix = TcpSirixBridge;
        let sink = MockDocSink::default();

        let mut entry_v0 = entry.clone();
        entry_v0.search_backend = manticore_url.clone();
        entry_v0.storage_backend = sirix_url.clone();

        let r0 = run(&[entry_v0.clone()], &http, &sirix, &sink);
        assert_eq!(r0.inserted, 1, "gen0 inserts");
        assert_eq!(r0.deleted, 0);
        assert_eq!(r0.errors, 0);
        {
            let bodies = manticore_bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1);
            assert!(
                bodies[0].contains("sirix://docs/manuals/42"),
                "wire body missing sirix URI: {}",
                bodies[0]
            );
            assert!(bodies[0].contains("\"replace\""));
        }

        // Commit a new doc to the "Sirix" mock.
        {
            let mut rows = sirix_rows.lock().unwrap();
            rows.push(json!(["99", 5, "{\"title\":\"gadget\"}"]));
        }

        let r1 = run(&[entry_v0], &http, &sirix, &sink);
        assert_eq!(r1.inserted, 1, "gen1 inserts new doc");
        assert_eq!(r1.unchanged, 1, "gen1 keeps the existing doc");
        assert_eq!(r1.deleted, 0);
        {
            let bodies = manticore_bodies.lock().unwrap();
            assert_eq!(bodies.len(), 2);
            assert!(
                bodies[1].contains("sirix://docs/manuals/99"),
                "gen1 body missing new sirix URI: {}",
                bodies[1]
            );
            // Only the new doc, not the existing one.
            assert!(
                !bodies[1].contains("sirix://docs/manuals/42"),
                "gen1 body should not re-insert existing doc: {}",
                bodies[1]
            );
        }
    }

    // -----------------------------------------------------------------
    // TcpListener-backed bridges for the wire test
    // -----------------------------------------------------------------

    struct TcpHttpBridge;
    impl HttpBridge for TcpHttpBridge {
        fn post_json(&self, url: &str, body: &str) -> Result<String, String> {
            http_post_via_tcp(url, body)
        }
    }

    struct TcpSirixBridge;
    impl SirixBridge for TcpSirixBridge {
        fn list_documents(
            &self,
            sirix_url: &str,
            database: &str,
            resource: &str,
            since_rev: Option<u64>,
        ) -> Result<Vec<SirixDocRow>, String> {
            let sql = build_scan_sql(database, resource, since_rev);
            let body = build_query_body(&sql);
            let url = sirix_query_url(sirix_url);
            let response = http_post_via_tcp(&url, &body)?;
            parse_scan_response(&response)
        }
    }

    /// Spin a single-connection-at-a-time HTTP echo server that
    /// records every request body and lets the caller compute the
    /// response. Runs on a background thread; the returned URL is the
    /// listener's ephemeral port.
    fn spawn_tcp_echo_server<F>(
        received: Arc<Mutex<Vec<String>>>,
        respond: F,
    ) -> String
    where
        F: Fn(&str) -> String + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut socket = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let body = match read_http_body(&mut socket) {
                    Some(b) => b,
                    None => continue,
                };
                received.lock().unwrap().push(body.clone());
                let response_body = respond(&body);
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n\
                     {}",
                    response_body.len(),
                    response_body
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    /// Spin a mock sirix-sql-server that answers `POST /query` with
    /// the shared row slot wrapped in the columns/rows envelope.
    fn spawn_sirix_server(rows: Arc<Mutex<Vec<serde_json::Value>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut socket = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                // Just drain the request; we don't validate the SQL body
                // beyond the endpoint being reachable.
                let _ = read_http_body(&mut socket);
                let response_body = json!({
                    "columns": ["_nodekey", "_rev", "document"],
                    "rows": &*rows.lock().unwrap(),
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
                let _ = socket.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    /// Read one HTTP/1.1 request from `socket` and return its body as
    /// a UTF-8 string. Uses Content-Length for framing; matches the
    /// wf_fulltext tests' idiom.
    fn read_http_body(socket: &mut std::net::TcpStream) -> Option<String> {
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 1024];
        loop {
            let n = socket.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header_str = String::from_utf8_lossy(&buf[..end]).to_string();
                let content_length = header_str
                    .split("\r\n")
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.trim().eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let body_start = end + 4;
                while buf.len() < body_start + content_length {
                    let n = socket.read(&mut chunk).ok()?;
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = &buf[body_start..body_start + content_length];
                return Some(String::from_utf8_lossy(body).to_string());
            }
        }
    }

    /// Minimal stdlib HTTP client — mirrors the helper in
    /// `wf_fulltext/tests/manticore_admin_client.rs`. Kept here so the
    /// document_sweep tests don't take a dep on wf_fulltext's test
    /// utilities.
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
}
