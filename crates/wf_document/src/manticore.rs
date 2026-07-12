//! Manticore adapter — pure functions for request-body construction and
//! response parsing. Copied verbatim from `wf_fulltext::manticore` with
//! two additions for wf_document's v0.2 wire shape:
//!
//!   * `PlainOpts` gains `include_body` and `body_content_type` so the
//!     guest can pass the caller's search-opts through unchanged.
//!   * `Hit` gains `body` and `content_type`. Under the index-only
//!     mirroring design (memo `wf-document.md` §08 and v1.0 §03),
//!     Manticore holds the inverted index only — bodies live in Sirix
//!     and are fetched by the compose step. `parse_response` still
//!     opportunistically populates `body` / `content_type` if a
//!     backwards-compat sweep happens to store them in `_source`; a
//!     `None` result means the compose step will round-trip to Sirix.
//!
//! v1.0 additions (memo `wf-document-v1.md` §04, §06):
//!
//!   * `PlainOpts.at_time` / `PlainOpts.at_rev` — time-travel selectors.
//!     `build_request_body` translates them into a `bool.filter` clause
//!     against the sweep-provided `_valid_from` / `_valid_to` / `_rev`
//!     columns.
//!   * Default filter (neither selector set) restricts hits to current
//!     revisions only (`_valid_to IS NULL`) — matches v0.2 semantics
//!     exactly, so a retention=latest sweep still sees every hit.
//!   * `Hit.revision` carries `_rev` back to the caller when present.
//!   * `build_probe_body` + `schema_has_valid_from` — helpers for the
//!     "index doesn't hold history" storage gate in `lib.rs`.
//!
//! v1.1 additions (memos `wf-document.md` §12, `wf-document-v1.md` §08
//! deferred). Four search-side capabilities layered onto the same
//! POST /search backend:
//!
//!   * `PlainOpts.indexes` — cross-index query selector. When non-empty,
//!     `build_*_body` emits the comma-separated `table` field per
//!     Manticore's HTTP JSON API (multi-index queries share the same
//!     backend request shape as single-index).
//!   * `build_facet_body` — adds an `aggs` clause per requested field
//!     alongside the existing search clauses; `parse_facet_response`
//!     lifts Manticore's `aggregations` object into `PlainFacetResult`s.
//!   * `build_mlt_body` — encodes the guest's MoreLikeThis fallback: an
//!     OR-match over the top-K TF terms of the seed doc, excluding the
//!     seed's `_id`.
//!   * `build_vector_body` — Manticore k-NN body. The `vector` field is
//!     required on the target index; this substrate does not generate
//!     embeddings.
//!
//! Kept out of `lib.rs` so the tests can exercise the wire mapping
//! without having to instantiate the wit-bindgen `Guest` trait or stub
//! the host import. Both the guest export and the tests call these
//! functions.

use serde_json::{json, Map, Value as JsonValue};

/// Plain-Rust mirror of the WIT `search-opts` record. The guest converts
/// the WIT record into this shape before calling `build_request_body` so
/// that (a) the mapping code is pure and (b) tests don't need to link
/// against the WIT bindings. Named `PlainOpts` to keep it distinct from
/// the wit-bindgen-generated `SearchOpts` at crate root.
#[derive(Clone, Debug, Default)]
pub struct PlainOpts {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub fields: Vec<String>,
    pub highlight: bool,
    pub lang: Option<String>,
    pub filter: Option<String>,
    pub include_body: bool,
    pub body_content_type: Option<String>,
    /// v1.0 time-travel selector. When set, the built request filters on
    /// the `_valid_from` / `_valid_to` interval columns supplied by a
    /// retention=all sweep. Mutually exclusive with `at_rev`; the guest
    /// enforces exclusivity before calling `build_request_body`.
    pub at_time: Option<AtTime>,
    /// v1.0 time-travel selector by revision. When set, the built
    /// request filters where `_rev = at_rev`.
    pub at_rev: Option<u64>,
    /// v1.1 cross-index selector. Empty = default single-index shape
    /// (the `index` positional wins). Non-empty = emit the comma-
    /// separated `table` field so Manticore unions across the listed
    /// indexes. See `resolve_table` for the small helper.
    pub indexes: Vec<String>,
}

/// A normalized `at_time` value. `Epoch` is emitted as a JSON number so
/// Manticore's numeric range filter compares against the sweep's epoch
/// seconds; `Iso` is forwarded as a string and left to Manticore's
/// datetime parser. See `lib.rs::normalize_at_time`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtTime {
    Epoch(i64),
    Iso(String),
}

impl AtTime {
    fn to_json(&self) -> JsonValue {
        match self {
            AtTime::Epoch(n) => JsonValue::Number((*n).into()),
            AtTime::Iso(s) => JsonValue::String(s.clone()),
        }
    }
}

/// Plain-Rust mirror of the WIT `hit` record. `body` / `content_type`
/// start `None` after `parse_response` under the index-only design —
/// bodies are held in Sirix and the compose step in `lib.rs` fetches
/// them per-hit when `opts.include_body` is true. If a backwards-compat
/// sweep happens to store them in Manticore's `_source`, `parse_response`
/// picks them up so the compose step can skip the Sirix round-trip.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub doc: String,
    pub score: f64,
    pub snippet: Option<String>,
    pub lang: Option<String>,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
    pub fields: Vec<(String, String)>,
    /// v1.0: `_rev` from Manticore's `_source` when present. `None`
    /// means the sweep didn't index a revision — retention=latest, or
    /// the sweep pre-dates the v1.0 column.
    pub revision: Option<u64>,
}

/// v1.1 faceting bucket — one aggregation entry. `value` is stringified
/// so the WIT type stays uniform across string/numeric bucket keys.
#[derive(Clone, Debug, PartialEq)]
pub struct PlainFacetValue {
    pub value: String,
    pub count: u64,
}

/// v1.1 faceting result — one aggregation over one field.
#[derive(Clone, Debug, PartialEq)]
pub struct PlainFacetResult {
    pub field: String,
    pub values: Vec<PlainFacetValue>,
}

/// Resolve the effective `table` value used across every builder in
/// this module. When `opts.indexes` is empty, fall back to the single
/// positional `index`. When non-empty, emit a comma-separated list —
/// Manticore's HTTP JSON API treats the string as "any of these tables"
/// which is exactly the cross-index semantic the guest wants. Empty
/// entries in `indexes` are dropped so a malformed opts list doesn't
/// smuggle a bare comma onto the wire.
pub fn resolve_table(index: &str, opts: &PlainOpts) -> String {
    if opts.indexes.is_empty() {
        return index.to_string();
    }
    let joined: Vec<&str> = opts
        .indexes
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if joined.is_empty() {
        index.to_string()
    } else {
        joined.join(",")
    }
}

/// Build the JSON body Manticore expects at `POST /search`.
///
/// Shape (§11 of the wf_fulltext memo, reused here):
///   {
///     "table":   "<index>"  |  "<idx1>,<idx2>,...",
///     "query":   { "match": { "*": "<query>" } }
///                or wrapped in a `bool.filter` when lang/filter present,
///     "limit":   <limit>,
///     "offset":  <offset>,     -- only if opts.offset is Some
///     "_source": { "includes": [...] },  -- only if opts.fields non-empty
///     "highlight": {},         -- only if opts.highlight
///   }
///
/// `opts.lang` becomes `bool.filter.equals.lang = "<lang>"`.
/// `opts.filter` is forwarded verbatim into `bool.filter`.
///
/// `include_body` / `body_content_type` are compose-time signals for
/// the guest's post-search fetch step and do not appear on the wire.
///
/// v1.1: `opts.indexes` (when non-empty) drives the `table` field; the
/// positional `index` is the DEFAULT.
pub fn build_request_body(
    index: &str,
    query: &str,
    opts: &PlainOpts,
) -> Result<String, String> {
    let mut body = Map::new();
    body.insert("table".into(), JsonValue::String(resolve_table(index, opts)));
    body.insert("query".into(), build_query_clause(query, opts)?);

    if let Some(limit) = opts.limit {
        body.insert("limit".into(), JsonValue::Number(limit.into()));
    }
    if let Some(offset) = opts.offset {
        body.insert("offset".into(), JsonValue::Number(offset.into()));
    }
    if !opts.fields.is_empty() {
        body.insert(
            "_source".into(),
            json!({ "includes": opts.fields.clone() }),
        );
    }
    if opts.highlight {
        body.insert("highlight".into(), JsonValue::Object(Map::new()));
    }

    serde_json::to_string(&JsonValue::Object(body))
        .map_err(|e| format!("wf_document: serialize request body: {e}"))
}

fn build_query_clause(query: &str, opts: &PlainOpts) -> Result<JsonValue, String> {
    let match_clause = json!({ "match": { "*": query } });

    let has_lang = opts.lang.as_deref().map_or(false, |s| !s.is_empty());
    let has_filter = opts.filter.as_deref().map_or(false, |s| !s.is_empty());
    let has_at_time = opts.at_time.is_some();
    let has_at_rev = opts.at_rev.is_some();

    let mut filter_list: Vec<JsonValue> = Vec::new();

    if let Some(lang) = opts.lang.as_deref().filter(|s| !s.is_empty()) {
        filter_list.push(json!({ "equals": { "lang": lang } }));
    }
    if let Some(filter_json) = opts.filter.as_deref().filter(|s| !s.is_empty()) {
        let parsed: JsonValue = serde_json::from_str(filter_json).map_err(|e| {
            format!("wf_document: opts.filter must be valid JSON: {e}")
        })?;
        filter_list.push(parsed);
    }

    // Time-travel selectors — mutually exclusive at the guest surface.
    match (opts.at_time.as_ref(), opts.at_rev) {
        (Some(at), None) => {
            let at_json = at.to_json();
            filter_list.push(json!({
                "range": { "_valid_from": { "lte": at_json.clone() } }
            }));
            filter_list.push(json!({
                "bool": {
                    "should": [
                        { "equals": { "_valid_to": null } },
                        { "range":  { "_valid_to": { "gt": at_json } } },
                    ]
                }
            }));
        }
        (None, Some(rev)) => {
            filter_list.push(json!({ "equals": { "_rev": rev } }));
        }
        (None, None) => {
            filter_list.push(json!({ "equals": { "_valid_to": null } }));
        }
        (Some(_), Some(_)) => {
            return Err(
                "wf_document: at_time and at_rev are mutually exclusive".to_string()
            );
        }
    }

    let _ = (has_lang, has_filter, has_at_time, has_at_rev);

    Ok(json!({
        "bool": {
            "must":   [match_clause],
            "filter": filter_list,
        }
    }))
}

/// Parse Manticore's `POST /search` response into `Hit`s. `body` and
/// `content_type` are left `None`; the compose step in `lib.rs` fills
/// them from Sirix when the caller asked for `include_body`.
pub fn parse_response(json_str: &str) -> Result<Vec<Hit>, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("wf_document: response is not valid JSON: {e}"))?;

    let hits_array = root
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(|h| h.as_array())
        .ok_or_else(|| {
            "wf_document: response missing `hits.hits` array".to_string()
        })?;

    let mut out = Vec::with_capacity(hits_array.len());
    for hit_json in hits_array {
        out.push(parse_hit(hit_json)?);
    }
    Ok(out)
}

fn parse_hit(hit: &JsonValue) -> Result<Hit, String> {
    let doc = hit
        .get("_id")
        .map(|v| match v {
            JsonValue::String(s) => s.clone(),
            other => other.to_string(),
        })
        .ok_or_else(|| "wf_document: hit missing `_id`".to_string())?;

    let score = hit
        .get("_score")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| "wf_document: hit missing numeric `_score`".to_string())?;

    let snippet = hit
        .get("highlight")
        .and_then(|h| h.as_object())
        .and_then(|obj| {
            obj.values()
                .filter_map(|v| v.as_array())
                .find_map(|arr| arr.first())
                .and_then(|first| first.as_str())
                .map(|s| s.to_string())
        });

    let source_obj = hit.get("_source").and_then(|s| s.as_object());
    let lang = source_obj
        .and_then(|obj| obj.get("lang"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let fields = source_obj
        .map(|obj| flatten_source(obj))
        .unwrap_or_default();

    let revision = source_obj
        .and_then(|obj| obj.get("_rev"))
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));

    let body = source_obj
        .and_then(|obj| obj.get("body"))
        .and_then(|v| v.as_str())
        .map(|s| s.as_bytes().to_vec());
    let content_type = source_obj
        .and_then(|obj| obj.get("content_type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(Hit {
        doc,
        score,
        snippet,
        lang,
        body,
        content_type,
        fields,
        revision,
    })
}

pub fn build_probe_body(index: &str) -> String {
    let body = json!({
        "table": index,
        "query": { "match_all": {} },
        "limit": 1,
    });
    body.to_string()
}

pub fn schema_has_valid_from(json_str: &str) -> bool {
    let root: JsonValue = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let hits = root
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(|h| h.as_array());
    let Some(hits) = hits else { return false };
    let Some(first) = hits.first() else {
        return false;
    };
    first
        .get("_source")
        .and_then(|s| s.as_object())
        .map_or(false, |obj| obj.contains_key("_valid_from"))
}

fn flatten_source(obj: &Map<String, JsonValue>) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        match v {
            JsonValue::String(s) => out.push((k.clone(), s.clone())),
            JsonValue::Number(n) => out.push((k.clone(), n.to_string())),
            JsonValue::Bool(b) => out.push((k.clone(), b.to_string())),
            JsonValue::Null => out.push((k.clone(), String::new())),
            _ => {}
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// v1.1 — faceting
// ---------------------------------------------------------------------------

/// Build the JSON body for a facet query. Reuses the same `POST /search`
/// endpoint (Manticore hangs aggregations off the search response), then
/// adds one `aggs` entry per requested field.
///
/// `limit` is forced to 0 in the body — the caller only wants the
/// aggregations, not per-hit rows. All other search-opts (`lang`,
/// `filter`, `at_time` / `at_rev`, `indexes`) apply to the query the
/// aggregations are computed over.
pub fn build_facet_body(
    index: &str,
    query: &str,
    fields: &[String],
    opts: &PlainOpts,
) -> Result<String, String> {
    if fields.is_empty() {
        return Err("wf_document: facet requires at least one field".to_string());
    }
    let mut body = Map::new();
    body.insert("table".into(), JsonValue::String(resolve_table(index, opts)));
    body.insert("query".into(), build_query_clause(query, opts)?);
    body.insert("limit".into(), JsonValue::Number(0.into()));

    let mut aggs = Map::new();
    for field in fields {
        if field.is_empty() {
            continue;
        }
        aggs.insert(
            field.clone(),
            json!({ "terms": { "field": field, "size": 100 } }),
        );
    }
    if aggs.is_empty() {
        return Err("wf_document: facet fields were all empty".to_string());
    }
    body.insert("aggs".into(), JsonValue::Object(aggs));

    serde_json::to_string(&JsonValue::Object(body))
        .map_err(|e| format!("wf_document: serialize facet body: {e}"))
}

/// Parse Manticore's aggregations envelope into `PlainFacetResult`s.
///
/// Missing `aggregations` is not an error — the caller sees an empty
/// list. Field order in the result follows `fields` (the caller's
/// request order) so downstream consumers can zip by index.
pub fn parse_facet_response(
    json_str: &str,
    fields: &[String],
) -> Result<Vec<PlainFacetResult>, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("wf_document: facet response is not valid JSON: {e}"))?;

    let aggs = match root.get("aggregations").and_then(|v| v.as_object()) {
        Some(obj) => obj,
        None => return Ok(Vec::new()),
    };

    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        let Some(agg) = aggs.get(field) else {
            out.push(PlainFacetResult {
                field: field.clone(),
                values: Vec::new(),
            });
            continue;
        };
        let buckets = agg
            .get("buckets")
            .and_then(|b| b.as_array())
            .cloned()
            .unwrap_or_default();
        let mut values = Vec::with_capacity(buckets.len());
        for bucket in buckets {
            let key = bucket
                .get("key")
                .map(|v| match v {
                    JsonValue::String(s) => s.clone(),
                    JsonValue::Number(n) => n.to_string(),
                    JsonValue::Bool(b) => b.to_string(),
                    JsonValue::Null => String::new(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let count = bucket
                .get("doc_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            values.push(PlainFacetValue { value: key, count });
        }
        out.push(PlainFacetResult {
            field: field.clone(),
            values,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// v1.1 — MoreLikeThis
// ---------------------------------------------------------------------------

/// Extract the top-K most frequent alphanumeric tokens of length > 3
/// from `body`. Non-UTF-8 bytes are dropped as they scan past; ASCII
/// lowercasing normalizes case for TF counting. Deterministic ordering:
/// on ties, terms are emitted in insertion order (first-seen wins).
pub fn extract_top_terms(body: &[u8], k: usize) -> Vec<String> {
    let text = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut counts: Vec<(String, u64, usize)> = Vec::new();
    let mut current = String::new();
    let mut idx: usize = 0;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            let taken = std::mem::take(&mut current);
            if taken.len() > 3 {
                bump_or_insert(&mut counts, taken, idx);
                idx += 1;
            }
        }
    }
    if !current.is_empty() && current.len() > 3 {
        bump_or_insert(&mut counts, current, idx);
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    counts.into_iter().take(k).map(|(t, _, _)| t).collect()
}

fn bump_or_insert(counts: &mut Vec<(String, u64, usize)>, term: String, first_seen: usize) {
    for entry in counts.iter_mut() {
        if entry.0 == term {
            entry.1 += 1;
            return;
        }
    }
    counts.push((term, 1, first_seen));
}

/// Build the JSON body for a MoreLikeThis query. Manticore has no
/// native MLT — the guest instead issues an OR-match over the top TF
/// terms of the seed doc, excluding the seed's own `_id`.
pub fn build_mlt_body(
    index: &str,
    seed_doc_id: &str,
    terms: &[String],
    limit: u32,
    opts: &PlainOpts,
) -> Result<String, String> {
    if terms.is_empty() {
        return Err("wf_document: more-like-this seed had no usable terms".to_string());
    }
    let terms_str = terms.join(" ");

    let base = build_query_clause(&terms_str, opts)?;
    let base_bool = base
        .get("bool")
        .and_then(|b| b.as_object())
        .ok_or_else(|| "wf_document: MLT base query missing bool wrapper".to_string())?;
    let must = base_bool.get("must").cloned().unwrap_or_else(|| json!([]));
    let filter = base_bool
        .get("filter")
        .cloned()
        .unwrap_or_else(|| json!([]));

    let query_clause = json!({
        "bool": {
            "must":     must,
            "must_not": [ { "equals": { "_id": seed_doc_id } } ],
            "filter":   filter,
        }
    });

    let mut body = Map::new();
    body.insert("table".into(), JsonValue::String(resolve_table(index, opts)));
    body.insert("query".into(), query_clause);
    body.insert("limit".into(), JsonValue::Number(limit.into()));
    if let Some(offset) = opts.offset {
        body.insert("offset".into(), JsonValue::Number(offset.into()));
    }
    if !opts.fields.is_empty() {
        body.insert(
            "_source".into(),
            json!({ "includes": opts.fields.clone() }),
        );
    }
    if opts.highlight {
        body.insert("highlight".into(), JsonValue::Object(Map::new()));
    }
    serde_json::to_string(&JsonValue::Object(body))
        .map_err(|e| format!("wf_document: serialize MLT body: {e}"))
}

// ---------------------------------------------------------------------------
// v1.1 — vector search
// ---------------------------------------------------------------------------

/// Build the JSON body for a vector (k-NN) query. Vector is forwarded
/// verbatim to Manticore; the guest never generates embeddings. The
/// target index MUST declare a `vector` field — operator's
/// responsibility.
pub fn build_vector_body(
    index: &str,
    vector: &[f32],
    k: u32,
    opts: &PlainOpts,
) -> Result<String, String> {
    if vector.is_empty() {
        return Err("wf_document: vector-search requires a non-empty vector".to_string());
    }
    if k == 0 {
        return Err("wf_document: vector-search requires k > 0".to_string());
    }
    let vec_json: Vec<JsonValue> = vector
        .iter()
        .map(|f| {
            serde_json::Number::from_f64(*f as f64)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null)
        })
        .collect();

    let mut knn = Map::new();
    knn.insert("field".into(), JsonValue::String("vector".into()));
    knn.insert("query_vector".into(), JsonValue::Array(vec_json));
    knn.insert("k".into(), JsonValue::Number(k.into()));

    let clause = build_query_clause("", opts)?;
    if let Some(filter) = clause
        .get("bool")
        .and_then(|b| b.get("filter"))
        .and_then(|f| f.as_array())
    {
        if !filter.is_empty() {
            knn.insert("filter".into(), JsonValue::Array(filter.clone()));
        }
    }

    let mut body = Map::new();
    body.insert("table".into(), JsonValue::String(resolve_table(index, opts)));
    body.insert("knn".into(), JsonValue::Object(knn));
    let limit = opts.limit.unwrap_or(k);
    body.insert("limit".into(), JsonValue::Number(limit.into()));
    if let Some(offset) = opts.offset {
        body.insert("offset".into(), JsonValue::Number(offset.into()));
    }
    if !opts.fields.is_empty() {
        body.insert(
            "_source".into(),
            json!({ "includes": opts.fields.clone() }),
        );
    }

    serde_json::to_string(&JsonValue::Object(body))
        .map_err(|e| format!("wf_document: serialize vector body: {e}"))
}
