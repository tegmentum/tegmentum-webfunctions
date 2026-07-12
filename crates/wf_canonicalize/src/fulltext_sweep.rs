//! Fulltext literal-index reconciliation.
//!
//! §07 of `wf-conformance/docs/design/wf-fulltext.md` — periodic sweep.
//! The sweep runs as one phase inside `wf_canonicalize::evaluate` and is
//! kept in its own module so the pure state-diff logic can be tested
//! without instantiating the wit-bindgen `Guest` trait.
//!
//! Contract:
//!
//! * Input: a list of `FulltextIndexConfig` entries, a sink handle, and
//!   two "bridge" traits (`GraphBridge` for graph reads, `HttpBridge`
//!   for Manticore admin writes). Bridges are trait objects so tests
//!   can substitute in-memory mocks.
//!
//! * State: per-index known-keys tracker persisted in the same sink
//!   SQLite the alias table lives in. Schema:
//!
//!     `wf_ft_keys_<sanitized-index-name>` (
//!         subject_iri TEXT PRIMARY KEY,
//!         doc_hash    TEXT NOT NULL,
//!         updated_at  INTEGER NOT NULL
//!     )
//!
//!   Rationale ("known-keys tracker shape"): persisted survives
//!   restarts (no full-scan-at-boot cost), doc-hash captures whether
//!   any covered field changed so unchanged docs skip the re-insert.
//!   The alternative (in-memory rebuild-from-Manticore-scan at boot)
//!   is honest but adds startup latency proportional to corpus size;
//!   the SQLite-persisted shape stays O(diff) forever. Both approaches
//!   are v0.1-defensible; we ship the persisted one.
//!
//! * Wire format: `HttpBridge::post_json` is expected to speak the
//!   Manticore `/bulk` NDJSON protocol (§11 of the design memo).
//!   `wf_fulltext::manticore_admin` has the mirror of this on the
//!   guest-export side; wf_canonicalize hits `/bulk` directly rather
//!   than via `invoke-wasm(wf_fulltext.wasm, "insert-batch", ...)`
//!   because `invoke-wasm` only dispatches to the guest's `evaluate`
//!   export — a v0.2 substrate extension will let us route through
//!   the typed guest surface. Until then, the typed exports on
//!   `wf_fulltext` are the honest forward-compatible surface;
//!   wf_canonicalize's sweep is a direct HTTP peer.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

/// Per-entry configuration parsed from the outer wf_canonicalize
/// config JSON. Mirrors the fields on the oxigraph-wf
/// `FulltextIndex` struct that are meaningful for
/// literal-index-mode reconciliation.
#[derive(Debug, Clone, Deserialize)]
pub struct FulltextIndexConfig {
    /// Registry entry name (for logging + known-keys table naming).
    pub name: String,
    /// Bare host[:port] of the Manticore backend (e.g. `http://localhost:9308`).
    pub backend_url: String,
    /// Backend-side index name.
    pub index: String,
    /// Predicate IRIs whose object literals feed this index.
    pub predicates: Vec<String>,
    /// BCP-47 language tag to filter graph reads by. `None` = any lang.
    #[serde(default)]
    pub lang_filter: Option<String>,
    /// How often the sweep should reconcile this entry. `None` = the
    /// canonicalize invocation's cadence (whichever operator drives
    /// wf:call). v0.1 always reconciles on every sweep invocation and
    /// leaves the sweep-scheduling cadence to the caller.
    #[serde(default)]
    pub sweep_interval_secs: Option<u32>,
}

/// Per-sweep counts surfaced back to the caller. Absorbs per-entry
/// errors so an unreachable backend doesn't crash the whole sweep.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepCounts {
    pub inserted: u64,
    pub deleted: u64,
    pub errors: u64,
}

impl SweepCounts {
    fn add(&mut self, other: SweepCounts) {
        self.inserted = self.inserted.saturating_add(other.inserted);
        self.deleted = self.deleted.saturating_add(other.deleted);
        self.errors = self.errors.saturating_add(other.errors);
    }
}

/// A single graph triple's object component — either a literal (with
/// optional lang) or an IRI. Blank-node objects are filtered out
/// upstream because they can't be indexed as fulltext content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiteralOrIri {
    Literal { lex: String, lang: Option<String> },
    Iri(String),
}

/// Graph-read bridge. The guest-side impl delegates to `execute-query`;
/// tests supply a canned row-set.
pub trait GraphBridge {
    fn select_subject_predicate_object(
        &self,
        predicates: &[String],
    ) -> Result<Vec<(String, String, LiteralOrIri)>, String>;
}

/// HTTP-out bridge. The guest-side impl delegates to
/// `wf:fulltext/host@0.1.0/http-post-json`; tests supply an in-memory
/// mock that records calls.
pub trait HttpBridge {
    fn post_json(&self, url: &str, body: &str) -> Result<String, String>;
}

/// Persistence bridge for the known-keys tracker. Wraps the sink
/// operations `run_one` needs so tests can substitute an in-memory
/// HashMap-backed impl. Wired at the outer guest boundary to the WIT
/// `sink-*` imports.
pub trait SinkBridge {
    fn ensure_table(&self, table: &str) -> Result<(), String>;
    fn load_known(&self, table: &str) -> Result<HashMap<String, String>, String>;
    fn upsert(&self, table: &str, subject: &str, hash: &str) -> Result<(), String>;
    fn delete(&self, table: &str, subject: &str) -> Result<(), String>;
}

/// The v0.1 shape assumed for Manticore's `/bulk` response — see
/// `wf_fulltext::manticore_admin` for the mirror parser. The sweep
/// treats a successful HTTP round-trip with `errors: false` as "the
/// entire batch landed"; anything else bumps `errors`.
///
/// Response check kept minimal: parse-then-look-for-errors. The full
/// parser lives in wf_fulltext; wf_canonicalize doesn't need to be a
/// full Manticore client, just enough to distinguish "batch OK" from
/// "batch had errors".
fn bulk_response_ok(body: &str) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(body)
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

/// Sanitize an index name for use as a SQL table suffix. Anything not
/// `[A-Za-z0-9_]` becomes `_`. Keeps names short enough to stay under
/// SQLite's practical identifier limit.
pub fn sanitize_index_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unnamed".into()
    } else {
        out
    }
}

/// The pure-function view of the sweep — takes bridges and configs,
/// returns aggregated counts. The wf_canonicalize `evaluate` function
/// wires the sink handle into thin sink-execute closures that record
/// known-keys via the outer WIT host imports.
pub fn run(
    entries: &[FulltextIndexConfig],
    http: &impl HttpBridge,
    graph: &impl GraphBridge,
    sink: &impl SinkBridge,
) -> SweepCounts {
    let mut total = SweepCounts::default();
    for entry in entries {
        match run_one(entry, http, graph, sink) {
            Ok(c) => total.add(c),
            Err(msg) => {
                eprintln!(
                    "wf_canonicalize.fulltext_sweep: entry `{}`: {}",
                    entry.name, msg
                );
                total.errors = total.errors.saturating_add(1);
            }
        }
    }
    total
}

fn run_one<H: HttpBridge, G: GraphBridge, S: SinkBridge>(
    entry: &FulltextIndexConfig,
    http: &H,
    graph: &G,
    sink: &S,
) -> Result<SweepCounts, String> {
    // 1. Ensure the tracker table exists.
    let table = format!("wf_ft_keys_{}", sanitize_index_name(&entry.name));
    sink.ensure_table(&table)?;

    // 2. Read current graph state for this entry's predicates.
    let rows = graph.select_subject_predicate_object(&entry.predicates)?;

    // 3. Group by subject IRI, producing one document per subject.
    //    Preserves stable field order (sorted by predicate IRI) so the
    //    doc-hash is deterministic across sweeps.
    let (docs, doc_hashes) = build_docs_and_hashes(&rows, entry.lang_filter.as_deref());

    // 4. Load known keys.
    let known = sink.load_known(&table)?;

    // 5. Compute diff.
    let (to_insert, to_delete) = diff(&docs, &doc_hashes, &known);

    // 6. Apply. Errors mid-batch are caught and bumped as `errors`
    //    (each batch is atomic from the backend's POV; a partial failure
    //    still lands the successful rows).
    let mut counts = SweepCounts::default();

    if !to_insert.is_empty() {
        let body = build_bulk_body(&entry.index, &to_insert);
        let url = bulk_url(&entry.backend_url);
        match http.post_json(&url, &body) {
            Ok(response) => match bulk_response_ok(&response) {
                Ok(()) => {
                    counts.inserted = to_insert.len() as u64;
                    // Persist the new known keys.
                    for doc in &to_insert {
                        let hash = doc_hashes.get(&doc.id).cloned().unwrap_or_default();
                        sink.upsert(&table, &doc.id, &hash)?;
                    }
                }
                Err(e) => {
                    counts.errors += 1;
                    eprintln!(
                        "wf_canonicalize.fulltext_sweep: entry `{}`: insert response: {e}",
                        entry.name
                    );
                }
            },
            Err(e) => {
                counts.errors += 1;
                eprintln!(
                    "wf_canonicalize.fulltext_sweep: entry `{}`: insert POST: {e}",
                    entry.name
                );
            }
        }
    }

    if !to_delete.is_empty() {
        let body = build_delete_body(&entry.index, &to_delete);
        let url = bulk_url(&entry.backend_url);
        match http.post_json(&url, &body) {
            Ok(response) => match bulk_response_ok(&response) {
                Ok(()) => {
                    counts.deleted = to_delete.len() as u64;
                    // Purge the tombstoned keys from the tracker.
                    for id in &to_delete {
                        sink.delete(&table, id)?;
                    }
                }
                Err(e) => {
                    counts.errors += 1;
                    eprintln!(
                        "wf_canonicalize.fulltext_sweep: entry `{}`: delete response: {e}",
                        entry.name
                    );
                }
            },
            Err(e) => {
                counts.errors += 1;
                eprintln!(
                    "wf_canonicalize.fulltext_sweep: entry `{}`: delete POST: {e}",
                    entry.name
                );
            }
        }
    }

    Ok(counts)
}

/// One document to send to Manticore. Mirrors
/// `wf_fulltext::manticore_admin::PlainDocWrite` but re-declared here
/// so the sweep module can be tested without pulling wf_fulltext in as
/// a dev-dep (and because the two crates evolve independently).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocWrite {
    pub id: String,
    pub fields: Vec<(String, String)>,
    pub lang: Option<String>,
}

/// Group the flat `?s ?p ?o` row-set into subject-scoped documents.
/// Returns the docs plus a per-subject deterministic doc-hash for the
/// known-keys diff. Blank-node subjects and IRI objects are filtered
/// out — blank subjects can't be fulltext-indexed by a stable key and
/// IRI objects aren't fulltext content.
pub fn build_docs_and_hashes(
    rows: &[(String, String, LiteralOrIri)],
    lang_filter: Option<&str>,
) -> (Vec<DocWrite>, HashMap<String, String>) {
    // Group into: subject -> (predicate -> (lex, lang)).
    let mut by_subject: HashMap<String, HashMap<String, (String, Option<String>)>> =
        HashMap::new();
    for (s, p, o) in rows {
        if let LiteralOrIri::Literal { lex, lang } = o {
            if let Some(target) = lang_filter {
                match lang.as_deref() {
                    Some(l) if l == target => {}
                    Some(l) if l.starts_with(&format!("{target}-")) => {}
                    _ => continue,
                }
            }
            by_subject
                .entry(s.clone())
                .or_default()
                .insert(p.clone(), (lex.clone(), lang.clone()));
        }
    }

    let mut docs = Vec::with_capacity(by_subject.len());
    let mut hashes = HashMap::with_capacity(by_subject.len());
    for (subject, fields) in by_subject {
        // Sorted (predicate, lex) for deterministic hash.
        let mut sorted: Vec<(&String, &(String, Option<String>))> = fields.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));

        let mut hasher_input = String::new();
        let mut out_fields = Vec::with_capacity(sorted.len());
        let mut lang: Option<String> = None;
        for (pred, (lex, l)) in &sorted {
            hasher_input.push_str(pred);
            hasher_input.push('\x1f');
            hasher_input.push_str(lex);
            hasher_input.push('\x1e');
            out_fields.push((pred.to_string(), lex.to_string()));
            // First lang wins — matches how Manticore expects a single
            // per-doc language column.
            if lang.is_none() {
                lang = l.clone();
            }
        }
        let hash = fnv1a64(&hasher_input);
        docs.push(DocWrite {
            id: subject.clone(),
            fields: out_fields,
            lang,
        });
        hashes.insert(subject, format!("{hash:016x}"));
    }
    (docs, hashes)
}

/// Given current-graph state (docs + per-subject hash) and the
/// known-keys map, return (subjects to insert/update, subjects to
/// delete).
pub fn diff(
    docs: &[DocWrite],
    hashes: &HashMap<String, String>,
    known: &HashMap<String, String>,
) -> (Vec<DocWrite>, Vec<String>) {
    let mut to_insert = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for doc in docs {
        seen.insert(doc.id.as_str());
        let this_hash = hashes.get(&doc.id).cloned().unwrap_or_default();
        match known.get(&doc.id) {
            Some(prev) if *prev == this_hash => {
                // Unchanged — skip.
            }
            _ => to_insert.push(doc.clone()),
        }
    }
    let mut to_delete = Vec::new();
    for id in known.keys() {
        if !seen.contains(id.as_str()) {
            to_delete.push(id.clone());
        }
    }
    // Stable ordering for deterministic tests.
    to_insert.sort_by(|a, b| a.id.cmp(&b.id));
    to_delete.sort();
    (to_insert, to_delete)
}

/// Mirror of `wf_fulltext::manticore_admin::build_bulk_body`. Kept
/// re-implemented here (rather than depending on the wf_fulltext
/// crate) so this crate can be built as a standalone wasm component
/// with no cross-crate build coupling. The two implementations MUST
/// agree on the wire shape.
pub fn build_bulk_body(index: &str, docs: &[DocWrite]) -> String {
    use serde_json::{json, Map, Value as J};
    let mut out = String::new();
    for doc in docs {
        let mut doc_obj = Map::new();
        for (k, v) in &doc.fields {
            doc_obj.insert(k.clone(), J::String(v.clone()));
        }
        if let Some(lang) = &doc.lang {
            if !lang.is_empty() {
                doc_obj.insert("lang".into(), J::String(lang.clone()));
            }
        }
        let line = json!({
            "replace": {
                "index": index,
                "id": doc.id,
                "doc": J::Object(doc_obj),
            }
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// Mirror of `wf_fulltext::manticore_admin::build_delete_body`. Same
/// justification as `build_bulk_body`.
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

fn bulk_url(backend_url: &str) -> String {
    let trimmed = backend_url.trim_end_matches('/');
    if trimmed.ends_with("/bulk") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/bulk")
    }
}

/// FNV-1a 64-bit hash — small, deterministic, good enough for the
/// change-detection use case. Same rationale as elsewhere in the
/// substrate: we're not fighting adversaries here, just detecting
/// whether the document body changed since last sweep.
fn fnv1a64(s: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // Mock bridges.
    #[allow(dead_code)]
    struct MockGraph {
        rows: Vec<(String, String, LiteralOrIri)>,
    }
    impl GraphBridge for MockGraph {
        fn select_subject_predicate_object(
            &self,
            _predicates: &[String],
        ) -> Result<Vec<(String, String, LiteralOrIri)>, String> {
            Ok(self.rows.clone())
        }
    }

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

    /// In-memory HashMap-backed SinkBridge. Table names are prefixed
    /// into keys so multiple entries don't collide.
    #[derive(Default)]
    struct MockSink {
        rows: RefCell<HashMap<(String, String), String>>, // (table, subject) -> hash
    }
    impl SinkBridge for MockSink {
        fn ensure_table(&self, _table: &str) -> Result<(), String> {
            Ok(())
        }
        fn load_known(&self, table: &str) -> Result<HashMap<String, String>, String> {
            Ok(self
                .rows
                .borrow()
                .iter()
                .filter_map(|((t, s), h)| {
                    if t == table {
                        Some((s.clone(), h.clone()))
                    } else {
                        None
                    }
                })
                .collect())
        }
        fn upsert(&self, table: &str, subject: &str, hash: &str) -> Result<(), String> {
            self.rows
                .borrow_mut()
                .insert((table.to_string(), subject.to_string()), hash.to_string());
            Ok(())
        }
        fn delete(&self, table: &str, subject: &str) -> Result<(), String> {
            self.rows
                .borrow_mut()
                .remove(&(table.to_string(), subject.to_string()));
            Ok(())
        }
    }

    fn ok_response() -> String {
        r#"{"items":[{"replace":{"_id":"x","result":"created"}}],"errors":false}"#.to_string()
    }

    #[test]
    fn build_docs_groups_by_subject_and_sorts_fields() {
        let rows = vec![
            (
                "urn:s:1".into(),
                "http://ex/label".into(),
                LiteralOrIri::Literal {
                    lex: "widget".into(),
                    lang: Some("en".into()),
                },
            ),
            (
                "urn:s:1".into(),
                "http://ex/desc".into(),
                LiteralOrIri::Literal {
                    lex: "a widget".into(),
                    lang: Some("en".into()),
                },
            ),
            (
                "urn:s:2".into(),
                "http://ex/label".into(),
                LiteralOrIri::Literal {
                    lex: "gadget".into(),
                    lang: Some("en".into()),
                },
            ),
        ];
        let (docs, _hashes) = build_docs_and_hashes(&rows, None);
        let by_id: HashMap<&str, &DocWrite> =
            docs.iter().map(|d| (d.id.as_str(), d)).collect();

        let d1 = by_id["urn:s:1"];
        assert_eq!(
            d1.fields,
            vec![
                ("http://ex/desc".into(), "a widget".into()),
                ("http://ex/label".into(), "widget".into()),
            ]
        );
        assert_eq!(d1.lang.as_deref(), Some("en"));

        let d2 = by_id["urn:s:2"];
        assert_eq!(
            d2.fields,
            vec![("http://ex/label".into(), "gadget".into())]
        );
    }

    #[test]
    fn build_docs_filters_by_lang() {
        let rows = vec![
            (
                "urn:s:1".into(),
                "http://ex/label".into(),
                LiteralOrIri::Literal {
                    lex: "widget".into(),
                    lang: Some("en".into()),
                },
            ),
            (
                "urn:s:1".into(),
                "http://ex/label-de".into(),
                LiteralOrIri::Literal {
                    lex: "Bauteil".into(),
                    lang: Some("de".into()),
                },
            ),
        ];
        let (docs, _) = build_docs_and_hashes(&rows, Some("en"));
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].fields.len(), 1);
        assert_eq!(docs[0].fields[0].0, "http://ex/label");
    }

    #[test]
    fn diff_detects_new_updated_and_removed() {
        let docs = vec![
            DocWrite {
                id: "urn:new".into(),
                fields: vec![],
                lang: None,
            },
            DocWrite {
                id: "urn:updated".into(),
                fields: vec![],
                lang: None,
            },
            DocWrite {
                id: "urn:unchanged".into(),
                fields: vec![],
                lang: None,
            },
        ];
        let mut hashes = HashMap::new();
        hashes.insert("urn:new".to_string(), "AAAA".to_string());
        hashes.insert("urn:updated".to_string(), "BBBB".to_string());
        hashes.insert("urn:unchanged".to_string(), "CCCC".to_string());

        let mut known = HashMap::new();
        known.insert("urn:updated".to_string(), "PRIOR".to_string());
        known.insert("urn:unchanged".to_string(), "CCCC".to_string());
        known.insert("urn:removed".to_string(), "PRIOR".to_string());

        let (to_ins, to_del) = diff(&docs, &hashes, &known);
        assert_eq!(
            to_ins.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["urn:new", "urn:updated"]
        );
        assert_eq!(to_del, vec!["urn:removed".to_string()]);
    }

    #[test]
    fn build_bulk_body_matches_wf_fulltext_shape() {
        let docs = vec![DocWrite {
            id: "urn:s:1".into(),
            fields: vec![("label".into(), "widget".into())],
            lang: Some("en".into()),
        }];
        let body = build_bulk_body("products", &docs);
        let line = body.trim_end_matches('\n');
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["replace"]["index"], "products");
        assert_eq!(parsed["replace"]["id"], "urn:s:1");
        assert_eq!(parsed["replace"]["doc"]["label"], "widget");
        assert_eq!(parsed["replace"]["doc"]["lang"], "en");
    }

    #[test]
    fn build_delete_body_one_line_per_id() {
        let body = build_delete_body("products", &["urn:s:1".into(), "urn:s:2".into()]);
        assert_eq!(body.matches('\n').count(), 2);
    }

    #[test]
    fn sanitize_index_name_replaces_specials() {
        assert_eq!(sanitize_index_name("products"), "products");
        assert_eq!(sanitize_index_name("my-index/2"), "my_index_2");
        assert_eq!(sanitize_index_name(""), "unnamed");
    }

    /// Table-level test — feed the pure diff path a state change and
    /// verify the correct requests get POSTed. Sink writes are stubbed
    /// out; the test focuses on the wire behavior end-to-end.
    #[test]
    fn sweep_emits_insert_for_new_doc_and_delete_for_removed() {
        // v0 corpus: {urn:s:1 -> "widget"}, then delta: add urn:s:2,
        // remove urn:s:1.
        //
        // We simulate this by calling `diff()` directly since the
        // full `run_one` needs the sink bridges. The wire-post path is
        // covered by wire_round_trip_insert_batch in wf_fulltext.

        let docs_v1 = vec![DocWrite {
            id: "urn:s:2".into(),
            fields: vec![("label".into(), "gadget".into())],
            lang: None,
        }];
        let mut hashes_v1 = HashMap::new();
        hashes_v1.insert("urn:s:2".to_string(), "H2".to_string());

        let mut known = HashMap::new();
        known.insert("urn:s:1".to_string(), "H1".to_string());

        let (ins, del) = diff(&docs_v1, &hashes_v1, &known);
        assert_eq!(ins.len(), 1);
        assert_eq!(ins[0].id, "urn:s:2");
        assert_eq!(del, vec!["urn:s:1".to_string()]);

        // And the mock HTTP bridge sees the right shape when we POST.
        let http = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let ins_body = build_bulk_body("products", &ins);
        let _ = http.post_json("http://localhost:9308/bulk", &ins_body);

        let del_body = build_delete_body("products", &del);
        let _ = http.post_json("http://localhost:9308/bulk", &del_body);

        let posts = http.posts.borrow();
        assert_eq!(posts.len(), 2);
        assert!(posts[0].1.contains("\"urn:s:2\""));
        assert!(posts[0].1.contains("gadget"));
        assert!(posts[1].1.contains("\"urn:s:1\""));
        assert!(posts[1].1.contains("delete"));
    }

    #[test]
    fn bulk_response_ok_accepts_errors_false() {
        assert!(bulk_response_ok(&ok_response()).is_ok());
    }

    #[test]
    fn bulk_response_ok_errs_on_errors_true() {
        let body = r#"{"items":[{"replace":{"_id":"x","error":"schema"}}],"errors":true}"#;
        assert!(bulk_response_ok(body).is_err());
    }

    #[test]
    fn deterministic_hash_of_identical_doc() {
        let rows = vec![
            (
                "s".into(),
                "p1".into(),
                LiteralOrIri::Literal {
                    lex: "x".into(),
                    lang: None,
                },
            ),
            (
                "s".into(),
                "p2".into(),
                LiteralOrIri::Literal {
                    lex: "y".into(),
                    lang: None,
                },
            ),
        ];
        let (_, h1) = build_docs_and_hashes(&rows, None);
        let (_, h2) = build_docs_and_hashes(&rows, None);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_changes_when_field_changes() {
        let base = vec![(
            "s".into(),
            "p1".into(),
            LiteralOrIri::Literal {
                lex: "x".into(),
                lang: None,
            },
        )];
        let modified = vec![(
            "s".into(),
            "p1".into(),
            LiteralOrIri::Literal {
                lex: "y".into(),
                lang: None,
            },
        )];
        let (_, h_base) = build_docs_and_hashes(&base, None);
        let (_, h_mod) = build_docs_and_hashes(&modified, None);
        assert_ne!(h_base.get("s"), h_mod.get("s"));
    }

    /// Ignoring the sink-execute path (which requires the WIT host
    /// bridge), verify `run` collates counts across multiple entries
    /// and surfaces errors per-entry without crashing.
    #[test]
    fn run_returns_error_count_but_never_crashes() {
        struct FailingGraph;
        impl GraphBridge for FailingGraph {
            fn select_subject_predicate_object(
                &self,
                _predicates: &[String],
            ) -> Result<Vec<(String, String, LiteralOrIri)>, String> {
                Err("mock: graph unavailable".into())
            }
        }
        let http = MockHttp::default();
        let sink = MockSink::default();
        let entries = vec![FulltextIndexConfig {
            name: "products".into(),
            backend_url: "http://localhost:9308".into(),
            index: "products".into(),
            predicates: vec!["http://ex/label".into()],
            lang_filter: None,
            sweep_interval_secs: None,
        }];
        let counts = run(&entries, &http, &FailingGraph, &sink);
        assert_eq!(counts.errors, 1);
        assert_eq!(counts.inserted, 0);
        assert_eq!(counts.deleted, 0);
    }

    /// End-to-end mocked sweep test: v0 corpus with one subject, then
    /// a graph delta that adds one and removes the original. Verifies
    /// the sweep emits the correct insert + delete POSTs, and that
    /// the tracker state persists across sweeps so a subsequent run
    /// with the same graph state emits nothing.
    #[test]
    fn full_sweep_two_generations_against_mock_backend() {
        let entry = FulltextIndexConfig {
            name: "products".into(),
            backend_url: "http://localhost:9308".into(),
            index: "products".into(),
            predicates: vec!["http://ex/label".into()],
            lang_filter: None,
            sweep_interval_secs: None,
        };
        let sink = MockSink::default();

        // Gen 0: {urn:s:1 -> "widget"}
        let graph_v0 = MockGraph {
            rows: vec![(
                "urn:s:1".into(),
                "http://ex/label".into(),
                LiteralOrIri::Literal {
                    lex: "widget".into(),
                    lang: None,
                },
            )],
        };
        let http_v0 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let counts_v0 = run(&[entry.clone()], &http_v0, &graph_v0, &sink);
        assert_eq!(counts_v0.inserted, 1);
        assert_eq!(counts_v0.deleted, 0);
        assert_eq!(counts_v0.errors, 0);
        assert_eq!(http_v0.posts.borrow().len(), 1);
        assert!(http_v0.posts.borrow()[0].1.contains("urn:s:1"));
        assert!(http_v0.posts.borrow()[0].1.contains("widget"));

        // Gen 1: {urn:s:2 -> "gadget"} (urn:s:1 removed)
        let graph_v1 = MockGraph {
            rows: vec![(
                "urn:s:2".into(),
                "http://ex/label".into(),
                LiteralOrIri::Literal {
                    lex: "gadget".into(),
                    lang: None,
                },
            )],
        };
        let http_v1 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let counts_v1 = run(&[entry.clone()], &http_v1, &graph_v1, &sink);
        assert_eq!(counts_v1.inserted, 1);
        assert_eq!(counts_v1.deleted, 1);
        assert_eq!(counts_v1.errors, 0);
        let posts_v1 = http_v1.posts.borrow();
        assert_eq!(posts_v1.len(), 2);
        // First is insert (urn:s:2), second is delete (urn:s:1).
        assert!(posts_v1[0].1.contains("replace"), "post 0: {}", posts_v1[0].1);
        assert!(posts_v1[0].1.contains("urn:s:2"));
        assert!(posts_v1[1].1.contains("delete"), "post 1: {}", posts_v1[1].1);
        assert!(posts_v1[1].1.contains("urn:s:1"));

        // Gen 2: same as gen 1. Sweep should be a no-op — no POSTs.
        let http_v2 = MockHttp {
            posts: RefCell::new(Vec::new()),
            response: ok_response(),
        };
        let counts_v2 = run(&[entry], &http_v2, &graph_v1, &sink);
        assert_eq!(counts_v2.inserted, 0);
        assert_eq!(counts_v2.deleted, 0);
        assert_eq!(counts_v2.errors, 0);
        assert!(http_v2.posts.borrow().is_empty());
    }
}
