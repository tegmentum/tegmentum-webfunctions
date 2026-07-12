//! Manticore adapter — pure functions for request-body construction and
//! response parsing. Copied verbatim from `wf_fulltext::manticore` with
//! two additions for wf_document's v0.2 wire shape:
//!
//!   * `PlainOpts` gains `include_body` and `body_content_type` so the
//!     guest can pass the caller's search-opts through unchanged.
//!   * `Hit` gains `body` and `content_type` (populated by the compose
//!     step after Sirix fetches, not by `parse_response` itself).
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

    if !has_lang && !has_filter {
        return Ok(match_clause);
    }

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

    Ok(Hit {
        doc,
        score,
        snippet,
        lang,
        body: None,
        content_type: None,
        fields,
    })
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
