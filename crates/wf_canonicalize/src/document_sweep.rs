//! Document-mirror reconciliation.
//!
//! §08 of `wf-conformance/docs/design/wf-document.md` — periodic sweep
//! that keeps Manticore mirroring the latest committed state of every
//! Sirix document per `DocumentRegistry` entry (Managed mode only).
//!
//! # Index-only mirroring (memo §08 / v1.0 §03)
//!
//! Manticore holds the **inverted index only** — not document bodies.
//! This is load-bearing: Sirix's whole value proposition is structural-
//! sharing delta storage across revisions. Copying full bodies into
//! Manticore at every sweep (worse: at every revision under
//! `retention_all`) would completely defeat that story — a 100 MB Sirix
//! corpus with 10x average revisions would balloon to a 1 GB Manticore
//! blob store, and Sirix would be doing nothing the substrate observes.
//!
//! So: the sweep sends `_uri`, `_rev`, `_valid_from`, `_valid_to`, plus
//! the tokenized body text under the JSON key `text`. Manticore
//! tokenizes it into the inverted index. Body retrieval (snippets,
//! `include_body: true`) happens via a Sirix round-trip on demand,
//! guest-side.
//!
//! # Operator schema requirement — `text stored='0'`
//!
//! Manticore's `/bulk` API has no per-request "index but don't store"
//! flag — that property is set once at `CREATE TABLE` time on the column.
//! Operators MUST declare the `text` column with the `stored='0'`
//! attribute so Manticore indexes the tokens but does NOT keep the raw
//! text in `_source` (the row payload it returns from `SELECT`). Example:
//!
//! ```sql
//! CREATE TABLE manuals (
//!     _uri        string,
//!     _rev        integer,
//!     _valid_from bigint,
//!     _valid_to   bigint,
//!     text        text stored='0'   -- indexed, NOT stored in _source
//! )
//! ```
//!
//! The sweep sends the same NDJSON `/bulk` body regardless — the schema
//! is what decides whether the raw text lingers in Manticore's `_source`.
//! With `stored='0'`, `SELECT *` returns only metadata; bodies come from
//! Sirix, exactly matching the memo intent.
//!
//! Retention modes: both `latest` and `all` send the same JSON shape
//! (index-only). Under `latest`, `_source` is `_uri`+`_rev` metadata;
//! under `all`, add `_valid_from`+`_valid_to`. Neither carries the body.
//! The v0.2 draft text implied bodies-in-`_source`; the memo correction
//! at wf-conformance commit `dfe456a` retracted that for both modes.
//!
//! # Backwards compat
//!
//! A Manticore corpus populated by an older sweep (pre-rename) will have
//! rows under the `body` column. Those rows remain searchable — Manticore
//! doesn't care which column holds tokens. The sweep re-populates rows
//! under `text` on the next change; the operator drops the old `body`
//! column at their convenience. No wire break; no query break.
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

use std::collections::{BTreeMap, HashMap, HashSet};

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
    /// v0.2 accepted only `"latest"`. v1.0 also accepts `"all"` —
    /// mirror every revision of every document. Any other value falls
    /// back to latest-mode semantics (the outer engine's
    /// `DocumentRegistry` is the front-line validator).
    #[serde(default = "default_retention")]
    pub revision_retention: String,
}

fn default_retention() -> String {
    "latest".into()
}

/// Retention-mode string constants — kept in one place so a rename
/// (unlikely) reaches every branch.
pub const RETENTION_LATEST: &str = "latest";
pub const RETENTION_ALL: &str = "all";
/// v1.0 canonical string prefixes for the object-form retention policies
/// (memo `wf-document-v1.md` §03 retention-policies table). The outer
/// engine's `DocumentRegistry` canonicalizes `{"window": "30d"}` and
/// `{"tail": 10}` object forms into these prefixed string forms before
/// handing config to the sweep; the sweep re-parses them here into
/// `RetentionPolicy`.
pub const RETENTION_WINDOW_PREFIX: &str = "window:";
pub const RETENTION_TAIL_PREFIX: &str = "tail:";

/// Parsed retention policy for a single entry. The wire format between
/// the outer engine and the sweep stays a plain `String` (v0.2 compat);
/// this enum is what the sweep dispatches on internally after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// v0.2 default — one row per URI, current revision only.
    Latest,
    /// v1.0 addition — every revision of every document.
    All,
    /// v1.0 addition — revisions whose `_valid_from` (or, absent a
    /// commit timestamp, `revision` number) is within the last
    /// `window_millis` milliseconds, plus the currently-open revision
    /// of every URI. `window_millis == 0` is rejected at parse time so
    /// the sweep never sees a zero-length window.
    Window { window_millis: i64 },
    /// v1.0 addition — the last N revisions per URI (plus the current).
    Tail { n: u32 },
}

impl RetentionPolicy {
    /// Parse the wire-format string the outer engine's `DocumentRegistry`
    /// canonicalized on the way in.
    ///
    /// Unknown or malformed strings fall back to `Latest` because the
    /// outer engine is the front-line validator — any malformed shape
    /// here means the deployment is misconfigured (already logged at
    /// boot). Latest-mode is the safe conservative fallback: it emits
    /// only current-tip rows, matching v0.2 semantics.
    fn from_wire(retention: &str) -> Self {
        if retention == RETENTION_LATEST {
            return Self::Latest;
        }
        if retention == RETENTION_ALL {
            return Self::All;
        }
        if let Some(dur) = retention.strip_prefix(RETENTION_WINDOW_PREFIX) {
            if let Some(ms) = parse_duration_millis(dur) {
                return Self::Window { window_millis: ms };
            }
        }
        if let Some(n_str) = retention.strip_prefix(RETENTION_TAIL_PREFIX) {
            if let Ok(n) = n_str.parse::<u32>() {
                if n > 0 {
                    return Self::Tail { n };
                }
            }
        }
        Self::Latest
    }
}

/// Parse a duration literal (`30d`, `24h`, `5m`) into milliseconds.
/// Returns `None` when the shape doesn't match — used by
/// `RetentionPolicy::from_wire` to fall back rather than panic. The
/// outer engine's `DocumentRegistry` already rejects malformed shapes
/// at boot, so this defensive fallback is a belt-and-braces layer.
fn parse_duration_millis(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let unit = bytes[bytes.len() - 1];
    let digits = &s[..s.len() - 1];
    let n: i64 = digits.parse().ok()?;
    if n <= 0 {
        return None;
    }
    let ms = match unit {
        b'd' => n.checked_mul(24 * 60 * 60 * 1000)?,
        b'h' => n.checked_mul(60 * 60 * 1000)?,
        b'm' => n.checked_mul(60 * 1000)?,
        _ => return None,
    };
    Some(ms)
}

/// Sweep-wide options, threaded through `run_with_options`. Wraps the
/// v1.0 backfill flag; broken out into a struct so future sweep-scope
/// switches (e.g. per-invocation dry-run) can grow here without another
/// `run_*` variant.
#[derive(Debug, Clone, Copy, Default)]
pub struct SweepOptions {
    /// v1.0 backfill: when `true`, the retention=all branch ignores
    /// the known-keys tracker's `last_seen_rev` history entirely and
    /// re-mirrors every revision it can see from Sirix. Lets an
    /// operator pull the full history on initial `retention: "all"`
    /// enablement without hand-editing the tracker table.
    ///
    /// Latest-mode branch honors this flag too (skip the FNV
    /// unchanged-check) so an operator can force-refresh a stale
    /// mirror without touching the tracker.
    pub full_scan: bool,
    /// Wall-clock reference (millis since epoch) used by
    /// `RetentionPolicy::Window` to compute the "in-window" cutoff.
    /// `None` = read the real system clock (production default);
    /// tests inject a fixed value here so window filtering is
    /// deterministic across timezones and CI machines.
    pub now_millis: Option<i64>,
}

impl SweepOptions {
    /// Resolve the wall-clock reference used by window retention.
    /// Falls back to the real system clock when the operator (or test
    /// harness) hasn't pinned one.
    fn resolve_now_millis(&self) -> i64 {
        match self.now_millis {
            Some(t) => t,
            None => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        }
    }
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
///
/// `commit_timestamp` is the Sirix commit timestamp for this revision,
/// used by retention=all mode as `_valid_from`. It's `None` when
/// sirix-sql-server didn't include a timestamp column in the response
/// (today's default) — the retention=all branch falls back to using
/// `revision` as the interval marker and logs the fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SirixDocRow {
    pub node_key: String,
    pub revision: u64,
    pub body: String,
    pub content_type: String,
    pub commit_timestamp: Option<i64>,
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
///
/// v1.0 update: keys are `(doc_uri, rev)` composite tuples. Latest-
/// mode rows always use `rev = 0` (a sentinel that matches the
/// tracker table's `DEFAULT 0` column); retention=all rows use the
/// actual Sirix revision. The two modes coexist in the same table
/// without collision.
pub trait DocSinkBridge {
    fn ensure_doc_table(&self, table: &str) -> Result<(), String>;
    fn load_known_docs(
        &self,
        table: &str,
    ) -> Result<HashMap<(String, u64), KnownDoc>, String>;
    fn upsert_doc(
        &self,
        table: &str,
        doc_uri: &str,
        rev: u64,
        entry: &KnownDoc,
    ) -> Result<(), String>;
    fn delete_doc(&self, table: &str, doc_uri: &str, rev: u64) -> Result<(), String>;
}

/// A single document to mirror into Manticore.
///
/// * `uri` is the base `sirix://<db>/<res>/<node-key>` URI. Latest-
///   mode inserts use it verbatim as the Manticore `_id`; retention=all
///   inserts use `<uri>@rev<N>` so multiple revisions coexist under
///   distinct keys.
/// * `valid_from` / `valid_to` are `None` in latest-mode rows.
///   Retention=all fills them in from the Sirix commit timestamp (or,
///   when sirix-sql-server doesn't expose one, from the numeric
///   revision — logged clearly at that point).
///   `valid_to == None` on the current-tip revision (open interval).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocMirror {
    pub uri: String,
    pub revision: u64,
    pub body: String,
    pub content_type: String,
    pub hash: String,
    pub valid_from: Option<i64>,
    pub valid_to: Option<i64>,
}

/// The pure-function view of the sweep — takes bridges and configs,
/// returns aggregated counts. Errors on any single entry are logged
/// and bump `errors`; the sweep never crashes the outer wf_canonicalize
/// pass, so a briefly-unreachable Sirix or Manticore doesn't block the
/// alias-reconcile phase.
///
/// Backwards-compat entry point: uses default `SweepOptions` (no
/// backfill). v1.0 callers that want the backfill flag call
/// `run_with_options` instead.
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
    run_with_options(entries, http, sirix, sink, SweepOptions::default())
}

/// v1.0 sweep entry point with the operator-controlled backfill flag.
/// `run` delegates here with defaults so the wire compat for existing
/// callers is preserved.
pub fn run_with_options<H, R, S>(
    entries: &[DocumentIndexConfig],
    http: &H,
    sirix: &R,
    sink: &S,
    options: SweepOptions,
) -> SweepResult
where
    H: HttpBridge,
    R: SirixBridge,
    S: DocSinkBridge,
{
    let mut total = SweepResult::default();
    for entry in entries {
        let policy = RetentionPolicy::from_wire(&entry.revision_retention);
        let result = match policy {
            RetentionPolicy::All => run_one_all(entry, http, sirix, sink, options),
            RetentionPolicy::Window { window_millis } => {
                run_one_window(entry, http, sirix, sink, options, window_millis)
            }
            RetentionPolicy::Tail { n } => run_one_tail(entry, http, sirix, sink, options, n),
            RetentionPolicy::Latest => run_one(entry, http, sirix, sink, options),
        };
        match result {
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
    options: SweepOptions,
) -> Result<SweepResult, String> {
    // 1. Ensure the tracker table exists (idempotent).
    let table = format!("wf_doc_keys_{}", sanitize_index_name(&entry.name));
    sink.ensure_doc_table(&table)?;

    // 2. Load the previously-known docs. Latest-mode keys are
    //    `(uri, 0)` — projected below to the uri-only view `build_diff`
    //    expects. `full_scan` empties the known-map so every doc
    //    gets re-mirrored regardless of hash.
    let known_composite = if options.full_scan {
        HashMap::new()
    } else {
        sink.load_known_docs(&table)?
    };
    let known: HashMap<String, KnownDoc> = known_composite
        .iter()
        .filter_map(|((u, r), k)| if *r == 0 { Some((u.clone(), k.clone())) } else { None })
        .collect();

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
                            0,
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
                        sink.delete_doc(&table, uri, 0)?;
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

/// Retention=all branch — mirror every revision of every document
/// in the registered Sirix resource. Row IDs on Manticore are
/// `<sirix-uri>@rev<N>` so multiple revisions coexist without
/// collision; each carries an `_valid_from`/`_valid_to` interval so
/// point-in-time search can filter.
///
/// **Sirix-side gap.** sirix-sql-server's SQL surface today doesn't
/// expose per-revision history — it returns a single row per node key
/// (the current-tip). When the response shape signals that (no group
/// has more than one row), this branch logs the gap and returns
/// without inserting anything, matching the memo §11 honesty
/// invariant. When the response DOES carry multi-revision rows (some
/// adapter forks do), we use them.
///
/// **Deletes**: retention=all is append-only by construction. We do
/// not delete individual historical revisions — Sirix's model
/// guarantees history sticks around, and tombstoning happens as a
/// new revision. The delete path is intentionally omitted here.
fn run_one_all<H: HttpBridge, R: SirixBridge, S: DocSinkBridge>(
    entry: &DocumentIndexConfig,
    http: &H,
    sirix: &R,
    sink: &S,
    options: SweepOptions,
) -> Result<SweepResult, String> {
    let table = format!("wf_doc_keys_{}", sanitize_index_name(&entry.name));
    sink.ensure_doc_table(&table)?;

    let known: HashMap<(String, u64), KnownDoc> = if options.full_scan {
        // Backfill mode — pretend we've seen nothing so every rev
        // that comes back from Sirix gets mirrored.
        HashMap::new()
    } else {
        sink.load_known_docs(&table)?
    };

    // Same SQL surface as latest-mode. If sirix-sql-server ever grows
    // history-including semantics for the standard SELECT, the sweep
    // benefits transparently.
    let rows = sirix
        .list_documents(
            &entry.storage_backend,
            &entry.sirix_database,
            &entry.sirix_resource,
            None,
        )
        .map_err(|e| format!("sirix list_documents: {e}"))?;

    let by_key = group_rows_by_key(&rows);
    let has_history = by_key.values().any(|v| v.len() > 1);
    if !by_key.is_empty() && !has_history {
        // Honesty invariant (memo §11). If Sirix only gave us the
        // current-tip revision for every doc, we can't index history —
        // log clearly and bail out without touching Manticore. A
        // healthy retention=all invocation should never hit this on a
        // corpus that actually has multiple revisions per doc.
        eprintln!(
            "wf_canonicalize.document_sweep: entry `{}`: revision_retention=\"all\" \
             requested, but sirix-sql-server returned a single row per _nodekey \
             (no per-revision history exposed via SQL). This is a known Sirix-side \
             gap — see wf-document-v1.md §11. Sweep returning without inserting; \
             `errors=0` because this is a config/deployment issue, not a runtime error.",
            entry.name
        );
        return Ok(SweepResult::default());
    }

    let (to_insert, unchanged, timestamp_fallback) =
        build_diff_all(entry, &by_key, &known);
    if timestamp_fallback {
        eprintln!(
            "wf_canonicalize.document_sweep: entry `{}`: revision_retention=\"all\" \
             using `_rev` as the `_valid_from` marker because sirix-sql-server did \
             not include a `_commit_timestamp` column. Point-in-time search will \
             compare integers, not timestamps, until Sirix exposes commit times.",
            entry.name
        );
    }

    let mut counts = SweepResult {
        inserted: 0,
        deleted: 0,
        unchanged,
        errors: 0,
    };

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
                            m.revision,
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

    Ok(counts)
}

/// Retention=window branch (memo `wf-document-v1.md` §03).
///
/// Enumerate all revisions from Sirix, keep the ones whose
/// `_valid_from` is within `[now - window_millis, now]` OR whose
/// `_valid_to` is null (the current-tip revision, always in the
/// mirror). Bounded by activity.
///
/// **Cleanup pass**: composite `(uri, rev)` tracker rows that were
/// previously mirrored but no longer fall in-window get emitted as
/// Manticore deletes so operators don't pay for indefinite historical
/// index bloat.
fn run_one_window<H: HttpBridge, R: SirixBridge, S: DocSinkBridge>(
    entry: &DocumentIndexConfig,
    http: &H,
    sirix: &R,
    sink: &S,
    options: SweepOptions,
    window_millis: i64,
) -> Result<SweepResult, String> {
    let table = format!("wf_doc_keys_{}", sanitize_index_name(&entry.name));
    sink.ensure_doc_table(&table)?;

    let known: HashMap<(String, u64), KnownDoc> = if options.full_scan {
        HashMap::new()
    } else {
        sink.load_known_docs(&table)?
    };

    let rows = sirix
        .list_documents(
            &entry.storage_backend,
            &entry.sirix_database,
            &entry.sirix_resource,
            None,
        )
        .map_err(|e| format!("sirix list_documents: {e}"))?;

    let by_key = group_rows_by_key(&rows);
    let has_history = by_key.values().any(|v| v.len() > 1);
    if !by_key.is_empty() && !has_history {
        // Same honesty invariant as retention=all: without per-revision
        // history from Sirix we can't compute a "was in window at t?"
        // interval, so log clearly and bail without touching Manticore.
        eprintln!(
            "wf_canonicalize.document_sweep: entry `{}`: revision_retention=window \
             requested, but sirix-sql-server returned a single row per _nodekey \
             (no per-revision history exposed via SQL). See wf-document-v1.md §11. \
             Sweep returning without inserting; `errors=0` because this is a \
             config/deployment issue, not a runtime error.",
            entry.name
        );
        return Ok(SweepResult::default());
    }

    let now_millis = options.resolve_now_millis();
    let cutoff = now_millis.saturating_sub(window_millis);

    let (to_insert, unchanged, timestamp_fallback, kept_keys) =
        build_diff_windowed(entry, &by_key, &known, cutoff);
    if timestamp_fallback {
        eprintln!(
            "wf_canonicalize.document_sweep: entry `{}`: revision_retention=window \
             using `_rev` as the interval marker because sirix-sql-server did not \
             include a `_commit_timestamp` column. Window filtering compares \
             integers, not timestamps, until Sirix exposes commit times.",
            entry.name
        );
    }

    // Cleanup: any previously-mirrored `(uri, rev)` pair that's not in
    // `kept_keys` has aged out of the window and needs a Manticore delete
    // plus a tracker row removal.
    let to_delete = compute_composite_deletes(&known, &kept_keys);

    emit_all_mode_bulk(entry, http, sink, &table, &to_insert, &to_delete, unchanged)
}

/// Retention=tail branch (memo `wf-document-v1.md` §03).
///
/// Enumerate all revisions from Sirix, group by URI, keep the last
/// `n` revisions per URI (plus the current tip — the two overlap when
/// `n >= 1`, and `n == 0` is impossible by parse-time validation).
///
/// **Cleanup pass**: composite `(uri, rev)` tracker rows for revisions
/// that fell out of the tail window get emitted as Manticore deletes.
fn run_one_tail<H: HttpBridge, R: SirixBridge, S: DocSinkBridge>(
    entry: &DocumentIndexConfig,
    http: &H,
    sirix: &R,
    sink: &S,
    options: SweepOptions,
    n: u32,
) -> Result<SweepResult, String> {
    let table = format!("wf_doc_keys_{}", sanitize_index_name(&entry.name));
    sink.ensure_doc_table(&table)?;

    let known: HashMap<(String, u64), KnownDoc> = if options.full_scan {
        HashMap::new()
    } else {
        sink.load_known_docs(&table)?
    };

    let rows = sirix
        .list_documents(
            &entry.storage_backend,
            &entry.sirix_database,
            &entry.sirix_resource,
            None,
        )
        .map_err(|e| format!("sirix list_documents: {e}"))?;

    let by_key = group_rows_by_key(&rows);
    let has_history = by_key.values().any(|v| v.len() > 1);
    if !by_key.is_empty() && !has_history {
        eprintln!(
            "wf_canonicalize.document_sweep: entry `{}`: revision_retention=tail \
             requested, but sirix-sql-server returned a single row per _nodekey \
             (no per-revision history exposed via SQL). See wf-document-v1.md §11. \
             Sweep returning without inserting; `errors=0` because this is a \
             config/deployment issue, not a runtime error.",
            entry.name
        );
        return Ok(SweepResult::default());
    }

    let (to_insert, unchanged, timestamp_fallback, kept_keys) =
        build_diff_tail(entry, &by_key, &known, n);
    if timestamp_fallback {
        eprintln!(
            "wf_canonicalize.document_sweep: entry `{}`: revision_retention=tail \
             using `_rev` as the interval marker because sirix-sql-server did not \
             include a `_commit_timestamp` column.",
            entry.name
        );
    }

    let to_delete = compute_composite_deletes(&known, &kept_keys);

    emit_all_mode_bulk(entry, http, sink, &table, &to_insert, &to_delete, unchanged)
}

/// Shared insert+delete emission path for the retention modes that use
/// the composite `<uri>@rev<N>` id scheme (all, window, tail). Latest-
/// mode keeps its own `run_one` because its id shape and delete
/// semantics differ (rev=0 sentinel, uri-only delete keys).
fn emit_all_mode_bulk<H: HttpBridge, S: DocSinkBridge>(
    entry: &DocumentIndexConfig,
    http: &H,
    sink: &S,
    table: &str,
    to_insert: &[DocMirror],
    to_delete: &[(String, u64)],
    unchanged: u64,
) -> Result<SweepResult, String> {
    let mut counts = SweepResult {
        inserted: 0,
        deleted: 0,
        unchanged,
        errors: 0,
    };

    if !to_insert.is_empty() {
        let body = build_bulk_body(&entry.search_index, to_insert);
        let url = bulk_url(&entry.search_backend);
        match http.post_json(&url, &body) {
            Ok(response) => match bulk_response_ok(&response) {
                Ok(()) => {
                    counts.inserted = to_insert.len() as u64;
                    for m in to_insert {
                        sink.upsert_doc(
                            table,
                            &m.uri,
                            m.revision,
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

    if !to_delete.is_empty() {
        let delete_ids: Vec<String> = to_delete
            .iter()
            .map(|(uri, rev)| format!("{uri}@rev{rev}"))
            .collect();
        let body = build_delete_body(&entry.search_index, &delete_ids);
        let url = bulk_url(&entry.search_backend);
        match http.post_json(&url, &body) {
            Ok(response) => match bulk_response_ok(&response) {
                Ok(()) => {
                    counts.deleted = to_delete.len() as u64;
                    for (uri, rev) in to_delete {
                        sink.delete_doc(table, uri, *rev)?;
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

/// Retention=window diff. Returns
/// `(to_insert, unchanged, timestamp_fallback, kept_composite_keys)`.
///
/// A revision is "in-window" if its `_valid_from` (or `_rev` fallback)
/// is `>= cutoff`, OR it's the current-tip revision of its URI. The
/// current-tip is always kept so the corpus never loses its
/// searchable "live now" surface even during a quiet period longer
/// than the window.
fn build_diff_windowed(
    entry: &DocumentIndexConfig,
    by_key: &BTreeMap<String, Vec<SirixDocRow>>,
    known: &HashMap<(String, u64), KnownDoc>,
    cutoff: i64,
) -> (Vec<DocMirror>, u64, bool, HashSet<(String, u64)>) {
    let mut to_insert: Vec<DocMirror> = Vec::new();
    let mut unchanged: u64 = 0;
    let mut timestamp_fallback = false;
    let mut kept_keys: HashSet<(String, u64)> = HashSet::new();

    for (node_key, group) in by_key {
        let base_uri =
            build_doc_uri(&entry.sirix_database, &entry.sirix_resource, node_key);

        let mut sorted: Vec<&SirixDocRow> = group.iter().collect();
        sorted.sort_by_key(|r| r.revision);
        let n = sorted.len();

        for (i, row) in sorted.iter().enumerate() {
            let (valid_from_val, fell_back_here) = match row.commit_timestamp {
                Some(ts) => (ts, false),
                None => (row.revision as i64, true),
            };
            timestamp_fallback |= fell_back_here;

            let is_current_tip = i + 1 == n;
            let in_window = valid_from_val >= cutoff;
            if !in_window && !is_current_tip {
                // Aged out of the window and not the tip — skip entirely.
                continue;
            }

            let valid_to = if !is_current_tip {
                match sorted[i + 1].commit_timestamp {
                    Some(ts) => Some(ts),
                    None => Some(sorted[i + 1].revision as i64),
                }
            } else {
                None
            };

            let composite_key = (base_uri.clone(), row.revision);
            kept_keys.insert(composite_key.clone());

            let hash = fnv1a64_hex(&row.body);
            match known.get(&composite_key) {
                Some(prev) if prev.doc_hash == hash => {
                    unchanged += 1;
                }
                _ => {
                    to_insert.push(DocMirror {
                        uri: base_uri.clone(),
                        revision: row.revision,
                        body: row.body.clone(),
                        content_type: row.content_type.clone(),
                        hash,
                        valid_from: Some(valid_from_val),
                        valid_to,
                    });
                }
            }
        }
    }
    to_insert.sort_by(|a, b| a.uri.cmp(&b.uri).then(a.revision.cmp(&b.revision)));
    (to_insert, unchanged, timestamp_fallback, kept_keys)
}

/// Retention=tail diff. Returns
/// `(to_insert, unchanged, timestamp_fallback, kept_composite_keys)`.
///
/// Groups by URI, sorts revisions ascending, keeps the last `n`
/// revisions (which naturally includes the current tip). When a URI
/// has fewer than `n` revisions we keep them all — the memo phrasing
/// "last N per URI plus current" means "at most N, always including
/// the current tip."
fn build_diff_tail(
    entry: &DocumentIndexConfig,
    by_key: &BTreeMap<String, Vec<SirixDocRow>>,
    known: &HashMap<(String, u64), KnownDoc>,
    tail_n: u32,
) -> (Vec<DocMirror>, u64, bool, HashSet<(String, u64)>) {
    let mut to_insert: Vec<DocMirror> = Vec::new();
    let mut unchanged: u64 = 0;
    let mut timestamp_fallback = false;
    let mut kept_keys: HashSet<(String, u64)> = HashSet::new();

    for (node_key, group) in by_key {
        let base_uri =
            build_doc_uri(&entry.sirix_database, &entry.sirix_resource, node_key);

        let mut sorted: Vec<&SirixDocRow> = group.iter().collect();
        sorted.sort_by_key(|r| r.revision);
        let group_len = sorted.len();
        let keep_from = group_len.saturating_sub(tail_n as usize);

        for (i, row) in sorted.iter().enumerate() {
            if i < keep_from {
                continue;
            }
            let (valid_from_val, fell_back_here) = match row.commit_timestamp {
                Some(ts) => (ts, false),
                None => (row.revision as i64, true),
            };
            timestamp_fallback |= fell_back_here;

            let is_current_tip = i + 1 == group_len;
            let valid_to = if !is_current_tip {
                match sorted[i + 1].commit_timestamp {
                    Some(ts) => Some(ts),
                    None => Some(sorted[i + 1].revision as i64),
                }
            } else {
                None
            };

            let composite_key = (base_uri.clone(), row.revision);
            kept_keys.insert(composite_key.clone());

            let hash = fnv1a64_hex(&row.body);
            match known.get(&composite_key) {
                Some(prev) if prev.doc_hash == hash => {
                    unchanged += 1;
                }
                _ => {
                    to_insert.push(DocMirror {
                        uri: base_uri.clone(),
                        revision: row.revision,
                        body: row.body.clone(),
                        content_type: row.content_type.clone(),
                        hash,
                        valid_from: Some(valid_from_val),
                        valid_to,
                    });
                }
            }
        }
    }
    to_insert.sort_by(|a, b| a.uri.cmp(&b.uri).then(a.revision.cmp(&b.revision)));
    (to_insert, unchanged, timestamp_fallback, kept_keys)
}

/// Compute composite `(uri, rev)` pairs that were in the previous
/// tracker snapshot but are not in the current retention decision.
/// These rows have aged out (window) or fallen past the tail cutoff.
/// The caller emits Manticore deletes keyed as `<uri>@rev<N>` and
/// clears the corresponding tracker rows.
///
/// Deterministic order (uri asc, rev asc) so wire-body assertions are
/// stable across test runs.
fn compute_composite_deletes(
    known: &HashMap<(String, u64), KnownDoc>,
    kept_keys: &HashSet<(String, u64)>,
) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = known
        .keys()
        .filter(|k| !kept_keys.contains(*k))
        .cloned()
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    out
}

/// Group Sirix rows by node_key, preserving the caller-supplied
/// order within each key so the "history exposed?" heuristic
/// downstream can trust it.
fn group_rows_by_key(rows: &[SirixDocRow]) -> BTreeMap<String, Vec<SirixDocRow>> {
    let mut by_key: BTreeMap<String, Vec<SirixDocRow>> = BTreeMap::new();
    for row in rows {
        by_key.entry(row.node_key.clone()).or_default().push(row.clone());
    }
    by_key
}

/// Retention=all diff. Returns
/// `(to_insert, unchanged, timestamp_fallback)`.
///
/// `timestamp_fallback` = `true` when at least one revision lacked a
/// `commit_timestamp` and we substituted the numeric `revision` as
/// the interval marker. Surfaced to the caller so the sweep can log
/// the fallback once per entry rather than once per row.
fn build_diff_all(
    entry: &DocumentIndexConfig,
    by_key: &BTreeMap<String, Vec<SirixDocRow>>,
    known: &HashMap<(String, u64), KnownDoc>,
) -> (Vec<DocMirror>, u64, bool) {
    let mut to_insert: Vec<DocMirror> = Vec::new();
    let mut unchanged: u64 = 0;
    let mut timestamp_fallback = false;
    for (node_key, group) in by_key {
        let base_uri =
            build_doc_uri(&entry.sirix_database, &entry.sirix_resource, node_key);

        // Sort ascending by revision so the look-ahead computes
        // valid_to as "commit timestamp of the NEXT revision of the
        // same URI" per the memo §03.
        let mut sorted: Vec<&SirixDocRow> = group.iter().collect();
        sorted.sort_by_key(|r| r.revision);
        let n = sorted.len();

        for (i, row) in sorted.iter().enumerate() {
            let (valid_from_val, fell_back_here) = match row.commit_timestamp {
                Some(ts) => (ts, false),
                None => (row.revision as i64, true),
            };
            let valid_to = if i + 1 < n {
                match sorted[i + 1].commit_timestamp {
                    Some(ts) => Some(ts),
                    None => Some(sorted[i + 1].revision as i64),
                }
            } else {
                // Current-tip revision — open interval.
                None
            };
            timestamp_fallback |= fell_back_here;

            let hash = fnv1a64_hex(&row.body);
            let composite_key = (base_uri.clone(), row.revision);
            match known.get(&composite_key) {
                Some(prev) if prev.doc_hash == hash => {
                    unchanged += 1;
                }
                _ => {
                    to_insert.push(DocMirror {
                        uri: base_uri.clone(),
                        revision: row.revision,
                        body: row.body.clone(),
                        content_type: row.content_type.clone(),
                        hash,
                        valid_from: Some(valid_from_val),
                        valid_to,
                    });
                }
            }
        }
    }
    to_insert.sort_by(|a, b| a.uri.cmp(&b.uri).then(a.revision.cmp(&b.revision)));
    (to_insert, unchanged, timestamp_fallback)
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
/// deterministic tests. Latest-mode only — emits `DocMirror`s with
/// `valid_from`/`valid_to` set to `None` (the bulk-body builder then
/// omits the interval columns for these rows, preserving the v0.2
/// wire shape byte-for-byte).
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
                    valid_from: None,
                    valid_to: None,
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
/// Doc payload carries `_uri`, `_rev`, `text`, and `content_type` — an
/// index schema that mirrors the memo §06 doc-ref surface. The `_id`
/// on the outer `replace` envelope is the same `sirix://` URI so
/// re-sending the same doc is idempotent.
///
/// The body text goes on the wire under the JSON key `text` (memo §08
/// index-only correction, wf-conformance commit `dfe456a`). Operators
/// declare the `text` column at `CREATE TABLE` time with the
/// `stored='0'` attribute so Manticore tokenizes it into the inverted
/// index but does NOT retain the raw bytes in `_source`. See the
/// crate-level doc comment for the exact DDL and rationale (Sirix's
/// structural sharing must not be defeated by a duplicated blob store
/// in Manticore).
///
/// v1.0 addition: retention=all `DocMirror`s carry `valid_from` /
/// `valid_to` and are keyed as `<uri>@rev<N>` on the wire so multiple
/// revisions of one URI coexist. When the mirror carries validity
/// intervals we emit `_valid_from` / `_valid_to` columns alongside;
/// latest-mode rows omit them entirely so `_source` shrinks to just
/// `_uri` + `_rev` metadata.
pub fn build_bulk_body(index: &str, docs: &[DocMirror]) -> String {
    use serde_json::{json, Map, Value as J};
    let mut out = String::new();
    for doc in docs {
        let mut doc_obj = Map::new();
        doc_obj.insert("_uri".into(), J::String(doc.uri.clone()));
        doc_obj.insert("_rev".into(), J::Number(doc.revision.into()));
        // Body text on the wire under `text`, per memo §08 index-only
        // correction. Operator DDL declares this column as
        // `text stored='0'` — indexed for full-text search, not kept
        // in `_source`. Bodies are fetched from Sirix on demand.
        doc_obj.insert("text".into(), J::String(doc.body.clone()));
        doc_obj.insert(
            "content_type".into(),
            J::String(doc.content_type.clone()),
        );
        // Retention=all rows carry validity intervals + a
        // rev-qualified id. Latest-mode rows keep the bare URI as
        // `_id`, matching v0.2 exactly.
        let (id, is_all_mode) = match doc.valid_from {
            Some(_) => (format!("{}@rev{}", doc.uri, doc.revision), true),
            None => (doc.uri.clone(), false),
        };
        if is_all_mode {
            if let Some(vf) = doc.valid_from {
                doc_obj.insert("_valid_from".into(), J::Number(vf.into()));
            }
            // `null` when this is the current-tip revision (open
            // interval). Explicit null so the guest can filter with
            // `_valid_to IS NULL OR _valid_to > at_time`.
            match doc.valid_to {
                Some(vt) => {
                    doc_obj.insert("_valid_to".into(), J::Number(vt.into()));
                }
                None => {
                    doc_obj.insert("_valid_to".into(), J::Null);
                }
            }
        }
        let line = json!({
            "replace": {
                "index": index,
                "id": id,
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
    // v1.0 retention=all uses this when present. Optional — the
    // retention=all branch logs a fallback and uses `_rev` as the
    // interval marker when it's absent.
    let ts_idx = idx_of(&[
        "_commit_timestamp",
        "commit_timestamp",
        "_commit_ts",
        "committimestamp",
    ]);

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
        let commit_timestamp = ts_idx.and_then(|i| cells.get(i)).and_then(|c| {
            c.as_i64().or_else(|| c.as_str().and_then(|s| s.parse().ok()))
        });
        // Doc content: prefer the named column, else first non-metadata cell.
        let (body, content_type) = match doc_idx.and_then(|i| cells.get(i)) {
            Some(cell) => cell_body_and_ct(cell),
            None => {
                // Fallback: first cell that's not the nodekey / rev / ts slot.
                let fallback = cells.iter().enumerate().find_map(|(i, c)| {
                    if i == nodekey_idx || i == rev_idx || ts_idx == Some(i) {
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
            commit_timestamp,
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
        // (table, uri, rev) -> KnownDoc. rev=0 for latest-mode rows,
        // real Sirix revision for retention=all.
        rows: RefCell<HashMap<(String, String, u64), KnownDoc>>,
    }
    impl DocSinkBridge for MockDocSink {
        fn ensure_doc_table(&self, _table: &str) -> Result<(), String> {
            Ok(())
        }
        fn load_known_docs(
            &self,
            table: &str,
        ) -> Result<HashMap<(String, u64), KnownDoc>, String> {
            Ok(self
                .rows
                .borrow()
                .iter()
                .filter_map(|((t, uri, rev), k)| {
                    if t == table {
                        Some(((uri.clone(), *rev), k.clone()))
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
            rev: u64,
            entry: &KnownDoc,
        ) -> Result<(), String> {
            self.rows.borrow_mut().insert(
                (table.to_string(), doc_uri.to_string(), rev),
                entry.clone(),
            );
            Ok(())
        }
        fn delete_doc(
            &self,
            table: &str,
            doc_uri: &str,
            rev: u64,
        ) -> Result<(), String> {
            self.rows
                .borrow_mut()
                .remove(&(table.to_string(), doc_uri.to_string(), rev));
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
            commit_timestamp: None,
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
                commit_timestamp: None,
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
                    commit_timestamp: None,
                },
                SirixDocRow {
                    node_key: "43".into(),
                    revision: 1,
                    body: "{\"title\":\"gadget\"}".into(),
                    content_type: "application/json".into(),
                    commit_timestamp: None,
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
                commit_timestamp: None,
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
            commit_timestamp: None,
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

    // -----------------------------------------------------------------
    // v1.0 retention=all coverage
    // -----------------------------------------------------------------

    fn cfg_all(name: &str) -> DocumentIndexConfig {
        let mut c = cfg(name);
        c.revision_retention = RETENTION_ALL.into();
        c
    }

    /// A three-revision history for `_nodekey=42` from Sirix with
    /// commit timestamps. Mimics what a history-aware sirix-sql
    /// response would look like.
    fn three_rev_history_with_ts() -> Vec<SirixDocRow> {
        vec![
            SirixDocRow {
                node_key: "42".into(),
                revision: 1,
                body: "{\"title\":\"widget-v1\"}".into(),
                content_type: "application/json".into(),
                commit_timestamp: Some(1_700_000_000),
            },
            SirixDocRow {
                node_key: "42".into(),
                revision: 2,
                body: "{\"title\":\"widget-v2\"}".into(),
                content_type: "application/json".into(),
                commit_timestamp: Some(1_700_005_000),
            },
            SirixDocRow {
                node_key: "42".into(),
                revision: 3,
                body: "{\"title\":\"widget-v3\"}".into(),
                content_type: "application/json".into(),
                commit_timestamp: Some(1_700_010_000),
            },
        ]
    }

    #[test]
    fn retention_all_mirrors_every_revision() {
        let entry = cfg_all("manuals");
        let sirix = MockSirix {
            rows: RefCell::new(three_rev_history_with_ts()),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();

        let r = run(&[entry], &http, &sirix, &sink);
        assert_eq!(r.inserted, 3, "one insert per revision");
        assert_eq!(r.unchanged, 0);
        assert_eq!(r.deleted, 0);
        assert_eq!(r.errors, 0);

        // Inspect the bulk body: three lines, one per rev, each with
        // the composite `<uri>@rev<N>` id and the interval columns.
        let posts = http.posts.borrow();
        assert_eq!(posts.len(), 1, "single POST for the batch of 3");
        let ndjson = &posts[0].1;
        let lines: Vec<&str> = ndjson.lines().collect();
        assert_eq!(lines.len(), 3);

        let ids: Vec<String> = lines
            .iter()
            .map(|l| {
                let v: JsonValue = serde_json::from_str(l).unwrap();
                v["replace"]["id"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                "sirix://docs/manuals/42@rev1",
                "sirix://docs/manuals/42@rev2",
                "sirix://docs/manuals/42@rev3",
            ]
        );

        // Interval columns: valid_from = commit ts, valid_to = next rev's
        // commit ts, except the tip which is null.
        let docs: Vec<JsonValue> = lines
            .iter()
            .map(|l| serde_json::from_str::<JsonValue>(l).unwrap()["replace"]["doc"].clone())
            .collect();
        assert_eq!(docs[0]["_valid_from"], JsonValue::from(1_700_000_000_i64));
        assert_eq!(docs[0]["_valid_to"], JsonValue::from(1_700_005_000_i64));
        assert_eq!(docs[1]["_valid_from"], JsonValue::from(1_700_005_000_i64));
        assert_eq!(docs[1]["_valid_to"], JsonValue::from(1_700_010_000_i64));
        assert_eq!(docs[2]["_valid_from"], JsonValue::from(1_700_010_000_i64));
        assert_eq!(docs[2]["_valid_to"], JsonValue::Null);
    }

    #[test]
    fn retention_all_current_revision_has_null_valid_to() {
        let entry = cfg_all("manuals");
        let sirix = MockSirix {
            rows: RefCell::new(three_rev_history_with_ts()),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();

        run(&[entry], &http, &sirix, &sink);
        let posts = http.posts.borrow();
        let last_line = posts[0].1.lines().last().unwrap();
        let v: JsonValue = serde_json::from_str(last_line).unwrap();
        assert_eq!(v["replace"]["id"], "sirix://docs/manuals/42@rev3");
        // Explicit `null` for the current-tip revision — open
        // interval, per memo §03.
        assert_eq!(v["replace"]["doc"]["_valid_to"], JsonValue::Null);
    }

    #[test]
    fn retention_all_gracefully_reports_when_sirix_has_no_history() {
        // sirix-sql-server today returns a single row per node_key.
        // Retention=all must recognize that gap, log it clearly, and
        // return without inserting.
        let entry = cfg_all("manuals");
        let sirix = MockSirix {
            rows: RefCell::new(vec![
                SirixDocRow {
                    node_key: "42".into(),
                    revision: 1,
                    body: "{\"title\":\"widget\"}".into(),
                    content_type: "application/json".into(),
                    commit_timestamp: Some(1_700_000_000),
                },
                SirixDocRow {
                    node_key: "43".into(),
                    revision: 1,
                    body: "{\"title\":\"gadget\"}".into(),
                    content_type: "application/json".into(),
                    commit_timestamp: Some(1_700_000_000),
                },
            ]),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();

        let r = run(&[entry], &http, &sirix, &sink);
        assert_eq!(r.inserted, 0);
        assert_eq!(r.unchanged, 0);
        assert_eq!(r.deleted, 0);
        assert_eq!(r.errors, 0, "the gap is a config issue, not a runtime error");
        assert!(
            http.posts.borrow().is_empty(),
            "no POSTs go to Manticore when we can't distinguish revisions"
        );
    }

    #[test]
    fn retention_all_falls_back_to_rev_marker_without_timestamp() {
        // When sirix-sql-server exposes history but no commit_timestamp
        // column, we still index all revs but log the fallback and use
        // `_rev` as the interval marker.
        let entry = cfg_all("manuals");
        let rows_no_ts: Vec<SirixDocRow> = three_rev_history_with_ts()
            .into_iter()
            .map(|mut r| {
                r.commit_timestamp = None;
                r
            })
            .collect();
        let sirix = MockSirix {
            rows: RefCell::new(rows_no_ts),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();

        let r = run(&[entry], &http, &sirix, &sink);
        assert_eq!(r.inserted, 3);
        let posts = http.posts.borrow();
        let first_line = posts[0].1.lines().next().unwrap();
        let v: JsonValue = serde_json::from_str(first_line).unwrap();
        // Fallback: `_valid_from` uses the revision number.
        assert_eq!(v["replace"]["doc"]["_valid_from"], JsonValue::from(1));
        assert_eq!(v["replace"]["doc"]["_valid_to"], JsonValue::from(2));
    }

    #[test]
    fn full_scan_ignores_tracker() {
        // Pre-populate the tracker with a known rev for `42`; a normal
        // (non-full_scan) sweep would then report `unchanged`. With
        // full_scan=true, every rev re-emits regardless.
        let entry = cfg_all("manuals");
        let sirix = MockSirix {
            rows: RefCell::new(three_rev_history_with_ts()),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();
        let table = format!(
            "wf_doc_keys_{}",
            sanitize_index_name(&entry.name)
        );
        for row in three_rev_history_with_ts() {
            sink.upsert_doc(
                &table,
                "sirix://docs/manuals/42",
                row.revision,
                &KnownDoc {
                    last_seen_rev: row.revision,
                    doc_hash: fnv1a64_hex(&row.body),
                },
            )
            .unwrap();
        }

        // Baseline: without full_scan, everything is unchanged.
        let r_default = run(&[entry.clone()], &http, &sirix, &sink);
        assert_eq!(r_default.inserted, 0);
        assert_eq!(r_default.unchanged, 3);

        // With full_scan=true, tracker is ignored.
        let http2 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let r_backfill = run_with_options(
            &[entry],
            &http2,
            &sirix,
            &sink,
            SweepOptions { full_scan: true, now_millis: None },
        );
        assert_eq!(r_backfill.inserted, 3, "full_scan re-inserts every rev");
        assert_eq!(r_backfill.unchanged, 0);
    }

    #[test]
    fn latest_mode_still_produces_v0_2_wire_shape() {
        // v0.2 regression guard: latest-mode emits the bare
        // `sirix://...` `_id`, no interval columns.
        let entry = cfg("manuals");
        let sirix = MockSirix {
            rows: RefCell::new(vec![SirixDocRow {
                node_key: "42".into(),
                revision: 1,
                body: "{\"title\":\"widget\"}".into(),
                content_type: "application/json".into(),
                commit_timestamp: None,
            }]),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();
        run(&[entry], &http, &sirix, &sink);
        let posts = http.posts.borrow();
        let line = posts[0].1.trim_end_matches('\n');
        let v: JsonValue = serde_json::from_str(line).unwrap();
        assert_eq!(v["replace"]["id"], "sirix://docs/manuals/42");
        assert!(v["replace"]["doc"].get("_valid_from").is_none());
        assert!(v["replace"]["doc"].get("_valid_to").is_none());
    }

    // -----------------------------------------------------------------
    // Index-only wire shape (memo §08 correction, dfe456a)
    // -----------------------------------------------------------------

    /// The sweep emits the body text under the JSON key `text`, not
    /// `body`. Operators declare that column as `text stored='0'` so
    /// Manticore indexes but doesn't keep the raw text in `_source`.
    #[test]
    fn latest_mode_bulk_body_uses_text_key_not_body() {
        let entry = cfg("manuals");
        let sirix = MockSirix {
            rows: RefCell::new(vec![SirixDocRow {
                node_key: "42".into(),
                revision: 1,
                body: "widget spec sheet".into(),
                content_type: "text/plain".into(),
                commit_timestamp: None,
            }]),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();
        run(&[entry], &http, &sirix, &sink);

        let posts = http.posts.borrow();
        let line = posts[0].1.trim_end_matches('\n');
        let v: JsonValue = serde_json::from_str(line).unwrap();
        let doc = &v["replace"]["doc"];
        // Body text goes on the wire under `text` — Manticore indexes
        // it, DDL `stored='0'` keeps it out of `_source`.
        assert_eq!(doc["text"], JsonValue::String("widget spec sheet".into()));
        // The pre-memo-correction key `body` is no longer emitted.
        assert!(
            doc.get("body").is_none(),
            "sweep must not emit `body` field: {doc}"
        );
    }

    /// Retention=all also uses `text` for every revision — the memo
    /// correction covers both latest and all modes.
    #[test]
    fn retention_all_bulk_body_uses_text_key_not_body() {
        let entry = cfg_all("manuals");
        let sirix = MockSirix {
            rows: RefCell::new(three_rev_history_with_ts()),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();
        run(&[entry], &http, &sirix, &sink);

        let posts = http.posts.borrow();
        let ndjson = &posts[0].1;
        for line in ndjson.lines() {
            let v: JsonValue = serde_json::from_str(line).unwrap();
            let doc = &v["replace"]["doc"];
            assert!(
                doc.get("text").is_some(),
                "every retention_all row must carry `text`: {doc}"
            );
            assert!(
                doc.get("body").is_none(),
                "retention_all rows must not emit `body`: {doc}"
            );
        }
    }

    #[test]
    fn commit_timestamp_column_parsed_when_present() {
        let body = json!({
            "columns": ["_nodekey", "_rev", "_commit_timestamp", "document"],
            "rows": [
                ["42", 1, 1_700_000_000_i64, "{\"title\":\"widget\"}"],
                ["42", 2, 1_700_005_000_i64, "{\"title\":\"widget-v2\"}"],
            ]
        })
        .to_string();
        let rows = parse_scan_response(&body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].commit_timestamp, Some(1_700_000_000));
        assert_eq!(rows[1].commit_timestamp, Some(1_700_005_000));
        assert_eq!(rows[0].body, "{\"title\":\"widget\"}");
    }

    // -----------------------------------------------------------------
    // v1.0 window / tail retention policies (memo `wf-document-v1.md` §03)
    // -----------------------------------------------------------------

    /// Convert days to milliseconds for the pinned test clock.
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    fn cfg_window(name: &str, wire: &str) -> DocumentIndexConfig {
        let mut c = cfg(name);
        c.revision_retention = wire.into();
        c
    }

    fn cfg_tail(name: &str, wire: &str) -> DocumentIndexConfig {
        let mut c = cfg(name);
        c.revision_retention = wire.into();
        c
    }

    /// Five revisions of the same doc, spaced 20 days apart so a
    /// 30-day window keeps the most recent two (revs 4, 5) and rev3
    /// (20 days ago). Revs 1 and 2 (80d, 60d ago) age out.
    fn five_revs_spaced_20d(now_ms: i64) -> Vec<SirixDocRow> {
        (0..5)
            .map(|i| {
                let rev = (i + 1) as u64;
                let periods_ago = 4 - i as i64; // rev1 -> 4 periods ago
                SirixDocRow {
                    node_key: "42".into(),
                    revision: rev,
                    body: format!("{{\"v\":{rev}}}"),
                    content_type: "application/json".into(),
                    commit_timestamp: Some(now_ms - periods_ago * 20 * DAY_MS),
                }
            })
            .collect()
    }

    #[test]
    fn retention_policy_from_wire_parses_all_forms() {
        assert_eq!(RetentionPolicy::from_wire("latest"), RetentionPolicy::Latest);
        assert_eq!(RetentionPolicy::from_wire("all"), RetentionPolicy::All);
        assert_eq!(
            RetentionPolicy::from_wire("window:30d"),
            RetentionPolicy::Window { window_millis: 30 * DAY_MS }
        );
        assert_eq!(
            RetentionPolicy::from_wire("window:24h"),
            RetentionPolicy::Window { window_millis: 24 * 60 * 60 * 1000 }
        );
        assert_eq!(
            RetentionPolicy::from_wire("window:5m"),
            RetentionPolicy::Window { window_millis: 5 * 60 * 1000 }
        );
        assert_eq!(
            RetentionPolicy::from_wire("tail:10"),
            RetentionPolicy::Tail { n: 10 }
        );
        // Malformed shapes fall back to Latest (belt-and-braces; the
        // outer engine's DocumentRegistry rejects them at boot).
        assert_eq!(RetentionPolicy::from_wire("bogus"), RetentionPolicy::Latest);
        assert_eq!(RetentionPolicy::from_wire("window:30x"), RetentionPolicy::Latest);
        assert_eq!(RetentionPolicy::from_wire("tail:0"), RetentionPolicy::Latest);
    }

    #[test]
    fn window_policy_mirrors_only_recent_revisions() {
        // 5 revs spaced 20 days apart (80d, 60d, 40d, 20d, 0d ago).
        // window=30d keeps rev4 (20d, in-window) and rev5 (current
        // tip, always kept). Revs 1, 2, 3 (80d, 60d, 40d) age out.
        let now = 1_700_000_000_000_i64;
        let entry = cfg_window("manuals", "window:30d");
        let sirix = MockSirix {
            rows: RefCell::new(five_revs_spaced_20d(now)),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();

        let r = run_with_options(
            &[entry],
            &http,
            &sirix,
            &sink,
            SweepOptions { full_scan: false, now_millis: Some(now) },
        );
        assert_eq!(r.inserted, 2, "rev4 (20d in-window) + rev5 (current tip)");
        assert_eq!(r.deleted, 0);
        assert_eq!(r.unchanged, 0);
        assert_eq!(r.errors, 0);

        // The wire body should carry rev4 and rev5 only — not rev1/2/3.
        let posts = http.posts.borrow();
        assert_eq!(posts.len(), 1);
        let ids: Vec<String> = posts[0]
            .1
            .lines()
            .map(|l| {
                let v: JsonValue = serde_json::from_str(l).unwrap();
                v["replace"]["id"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                "sirix://docs/manuals/42@rev4".to_string(),
                "sirix://docs/manuals/42@rev5".to_string(),
            ]
        );
    }

    #[test]
    fn tail_policy_mirrors_only_last_n() {
        // 10 revs, tail=5 keeps the last 5 (revs 6..10).
        let entry = cfg_tail("manuals", "tail:5");
        let rows: Vec<SirixDocRow> = (1..=10)
            .map(|rev| SirixDocRow {
                node_key: "42".into(),
                revision: rev,
                body: format!("{{\"v\":{rev}}}"),
                content_type: "application/json".into(),
                commit_timestamp: Some(1_700_000_000 + rev as i64 * 1000),
            })
            .collect();
        let sirix = MockSirix {
            rows: RefCell::new(rows),
        };
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let sink = MockDocSink::default();

        let r = run(&[entry], &http, &sirix, &sink);
        assert_eq!(r.inserted, 5, "tail=5 keeps the last 5 revs");
        assert_eq!(r.deleted, 0);
        assert_eq!(r.errors, 0);

        let posts = http.posts.borrow();
        assert_eq!(posts.len(), 1);
        let ids: Vec<String> = posts[0]
            .1
            .lines()
            .map(|l| {
                let v: JsonValue = serde_json::from_str(l).unwrap();
                v["replace"]["id"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                "sirix://docs/manuals/42@rev6".to_string(),
                "sirix://docs/manuals/42@rev7".to_string(),
                "sirix://docs/manuals/42@rev8".to_string(),
                "sirix://docs/manuals/42@rev9".to_string(),
                "sirix://docs/manuals/42@rev10".to_string(),
            ]
        );
    }

    #[test]
    fn window_policy_cleans_up_aged_revisions() {
        // Sweep gen 0: rev at t=now-25d is in-window (30d).
        //  Manticore + tracker gain the row.
        // Sweep gen 1: 20 days later, that rev is now t=now-45d — out
        //  of window. The sweep must emit a delete for it.
        let now_gen0 = 1_700_000_000_000_i64;
        let now_gen1 = now_gen0 + 20 * DAY_MS;
        let entry = cfg_window("manuals", "window:30d");

        // A single doc with two revs: rev1 is 25 days old at gen0,
        // rev2 is the current tip. Rev1 will still be in-window at gen0
        // (25d < 30d) but out-of-window by gen1 (45d > 30d).
        let rev1_ts = now_gen0 - 25 * DAY_MS;
        let rev2_ts = now_gen0; // current tip at gen0
        let rows = vec![
            SirixDocRow {
                node_key: "42".into(),
                revision: 1,
                body: "{\"v\":1}".into(),
                content_type: "application/json".into(),
                commit_timestamp: Some(rev1_ts),
            },
            SirixDocRow {
                node_key: "42".into(),
                revision: 2,
                body: "{\"v\":2}".into(),
                content_type: "application/json".into(),
                commit_timestamp: Some(rev2_ts),
            },
        ];
        let sirix = MockSirix { rows: RefCell::new(rows) };
        let sink = MockDocSink::default();

        // Gen 0: both revs in-window, both mirror.
        let http0 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let r0 = run_with_options(
            &[entry.clone()],
            &http0,
            &sirix,
            &sink,
            SweepOptions { full_scan: false, now_millis: Some(now_gen0) },
        );
        assert_eq!(r0.inserted, 2, "gen0 mirrors both in-window revs");
        assert_eq!(r0.deleted, 0);

        // Gen 1: rev1 is now 45 days old — out of window. It's not the
        // current tip either. Expect exactly one delete keyed as
        // <uri>@rev1.
        let http1 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let r1 = run_with_options(
            &[entry],
            &http1,
            &sirix,
            &sink,
            SweepOptions { full_scan: false, now_millis: Some(now_gen1) },
        );
        assert_eq!(r1.deleted, 1, "rev1 aged out; sweep must delete it from Manticore");
        assert_eq!(r1.inserted, 0);
        // The wire body of the delete carries the composite id.
        let posts = http1.posts.borrow();
        let delete_line = posts
            .iter()
            .flat_map(|(_, body)| body.lines())
            .find(|line| line.contains("\"delete\""))
            .expect("delete line present");
        assert!(
            delete_line.contains("sirix://docs/manuals/42@rev1"),
            "delete wire body missing composite id: {delete_line}"
        );
        // And the tracker row for rev1 is gone.
        let tracker_rows = sink.rows.borrow();
        assert!(
            !tracker_rows
                .keys()
                .any(|(_, uri, rev)| uri == "sirix://docs/manuals/42" && *rev == 1),
            "tracker still contains rev1 after aged-out delete"
        );
    }

    #[test]
    fn tail_policy_deletes_older_revisions_that_fall_out_of_window() {
        // Baseline mirror: 3 revs of one doc under tail=3 (everything
        // fits). Then Sirix commits 2 more revs; tail=3 now keeps the
        // last 3 (revs 3, 4, 5), and revs 1 & 2 must get deleted from
        // Manticore.
        let entry = cfg_tail("manuals", "tail:3");

        let gen0_rows: Vec<SirixDocRow> = (1..=3)
            .map(|rev| SirixDocRow {
                node_key: "42".into(),
                revision: rev,
                body: format!("{{\"v\":{rev}}}"),
                content_type: "application/json".into(),
                commit_timestamp: Some(1_700_000_000 + rev as i64 * 1000),
            })
            .collect();
        let sirix = MockSirix {
            rows: RefCell::new(gen0_rows),
        };
        let sink = MockDocSink::default();
        let http0 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let r0 = run(&[entry.clone()], &http0, &sirix, &sink);
        assert_eq!(r0.inserted, 3, "gen0 mirrors all three revs");
        assert_eq!(r0.deleted, 0);

        // Commit revs 4 and 5.
        for rev in 4..=5u64 {
            sirix.rows.borrow_mut().push(SirixDocRow {
                node_key: "42".into(),
                revision: rev,
                body: format!("{{\"v\":{rev}}}"),
                content_type: "application/json".into(),
                commit_timestamp: Some(1_700_000_000 + rev as i64 * 1000),
            });
        }
        let http1 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let r1 = run(&[entry], &http1, &sirix, &sink);
        assert_eq!(r1.inserted, 2, "revs 4 and 5 are new to the tail window");
        assert_eq!(r1.unchanged, 1, "rev3 was already in the tail window");
        assert_eq!(r1.deleted, 2, "revs 1 and 2 fell out of the tail window");

        // Wire body carries deletes for rev1 and rev2 specifically.
        let posts = http1.posts.borrow();
        let combined: String = posts
            .iter()
            .flat_map(|(_, body)| body.lines())
            .filter(|line| line.contains("\"delete\""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            combined.contains("sirix://docs/manuals/42@rev1"),
            "expected rev1 delete: {combined}"
        );
        assert!(
            combined.contains("sirix://docs/manuals/42@rev2"),
            "expected rev2 delete: {combined}"
        );
    }
}
