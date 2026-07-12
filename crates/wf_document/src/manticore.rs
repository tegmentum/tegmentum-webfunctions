//! Manticore adapter — pure functions for request-body construction and
//! response parsing. Copied verbatim from `wf_fulltext::manticore` with
//! two additions for wf_document's v0.2 wire shape:
//!
//!   * `PlainOpts` gains `include_body` and `body_content_type` so the
//!     guest can pass the caller's search-opts through unchanged.
//!   * `Hit` gains `body` and `content_type` (populated by the compose
//!     step after Sirix fetches, not by `parse_response` itself).
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
/// start `None` after `parse_response`; the compose step in `lib.rs`
/// fills them from Sirix when `opts.include_body` is true.
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

/// Build the JSON body Manticore expects at `POST /search`.
///
/// Shape (§11 of the wf_fulltext memo, reused here):
///   {
///     "table":   "<index>",
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
pub fn build_request_body(
    index: &str,
    query: &str,
    opts: &PlainOpts,
) -> Result<String, String> {
    let mut body = Map::new();
    body.insert("table".into(), JsonValue::String(index.into()));
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

    // v1.0 wire-shape decision: even in the "no user filter" case, we
    // always wrap in bool.filter when `_valid_from` is a schema column.
    // The trigger is any time-travel field OR the default current-only
    // clause. To preserve v0.2 semantics for callers that hit a
    // retention=latest index (no `_valid_from` column), we only emit the
    // `_valid_to IS NULL` guard when the schema has it — but the guest
    // can't know that here without a probe, so we emit it unconditionally
    // for v1.0. Retention=latest sweeps must index `_valid_to` as NULL
    // for every mirrored row so the guard is a no-op there. See memo §07.

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
            // `_valid_from <= at_time AND (_valid_to IS NULL OR _valid_to > at_time)`
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
            // Revision pin is simpler: exact match on `_rev`.
            filter_list.push(json!({ "equals": { "_rev": rev } }));
        }
        (None, None) => {
            // No time-travel = current revisions only. Matches v0.2
            // semantics (row visible iff _valid_to IS NULL).
            filter_list.push(json!({ "equals": { "_valid_to": null } }));
        }
        (Some(_), Some(_)) => {
            // The guest already rejected this combination in `search`.
            return Err(
                "wf_document: at_time and at_rev are mutually exclusive".to_string()
            );
        }
    }

    // If the only filter was the current-only guard AND we have no
    // lang/filter/time selectors, we still emit bool.filter so the wire
    // shape stays uniform. `has_lang` / `has_filter` / has_at_time /
    // has_at_rev were tracked above only to make future short-circuits
    // easy; here we always emit the bool wrapper because v1.0's default
    // clause needs a filter slot.
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
///
/// Expected shape:
///   { "hits": { "hits": [
///       { "_id": "...", "_score": 0.9,
///         "_source": {...},
///         "highlight": { "*": ["..."] } },
///       ...
///   ] } }
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

    // v1.0: `_rev` populates hit.doc.revision. Accept either a numeric
    // JSON value or a decimal string — Manticore's `_source` layer
    // sometimes stringifies u64s.
    let revision = source_obj
        .and_then(|obj| obj.get("_rev"))
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));

    Ok(Hit {
        doc,
        score,
        snippet,
        lang,
        body: None,
        content_type: None,
        fields,
        revision,
    })
}

/// Build a small "does the index have `_valid_from`?" probe body. Sent
/// to the same `/search` endpoint. The v1.0 guest sends this only when
/// a time-travel search returned zero hits — the empty result is either
/// honest (no matches) or means the sweep isn't retention=all. The
/// probe distinguishes the two by looking at the first document's
/// `_source` fields.
pub fn build_probe_body(index: &str) -> String {
    let body = json!({
        "table": index,
        "query": { "match_all": {} },
        "limit": 1,
    });
    body.to_string()
}

/// Return true iff the probe response's first hit's `_source` contains
/// `_valid_from` — the retention=all sweep's timeline column. False
/// means the sweep hasn't been re-run under retention=all (or the index
/// is empty and we can't tell — err on the side of "no history",
/// callers propagate the loud error).
///
/// A response the guest can't parse (network mangled, unexpected shape)
/// also returns `false` — the storage gate stays best-effort but errs
/// toward surfacing a schema problem.
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
        // Empty index — we can't tell. Callers treat this as "no
        // history"; that's the conservative default for the gate.
        return false;
    };
    first
        .get("_source")
        .and_then(|s| s.as_object())
        .map_or(false, |obj| obj.contains_key("_valid_from"))
}

/// Flatten `_source` into `(name, string)` tuples. Strings pass through;
/// numbers/bools stringified; objects/arrays skipped (would need a lossy
/// JSON-in-string encoding to fit `list<tuple<string,string>>`).
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
