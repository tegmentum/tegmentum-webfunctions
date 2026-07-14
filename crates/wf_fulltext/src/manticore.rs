//! Manticore adapter — pure functions for request-body construction and
//! response parsing.
//!
//! Kept out of `lib.rs` so the tests can exercise the wire mapping without
//! having to instantiate the wit-bindgen `Guest` trait or stub the host
//! import. Both the guest export and the tests call these functions.

use serde_json::{json, Map, Value as JsonValue};

/// Plain-Rust mirror of the WIT `query-opts` record. The guest converts the
/// WIT record into this shape before calling `build_request_body` so that
/// (a) the mapping code is pure and (b) tests don't need to link against
/// the WIT bindings. Named `PlainOpts` to keep it distinct from the
/// wit-bindgen-generated `QueryOpts` at crate root.
#[derive(Clone, Debug, Default)]
pub struct PlainOpts {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub fields: Vec<String>,
    pub highlight: bool,
    pub lang: Option<String>,
    pub filter: Option<String>,
}

/// Plain-Rust mirror of the WIT `hit` record.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub doc: String,
    pub score: f64,
    pub snippet: Option<String>,
    pub lang: Option<String>,
    pub fields: Vec<(String, String)>,
}

/// Build the JSON body Manticore expects at `POST /search`.
///
/// Shape (§11 of the design memo):
///   {
///     "table":   "<index>",
///     "query":   { "match": { "*": "<query>" } }
///                or wrapped in a `bool.filter` when lang/filter present,
///     "limit":   <limit>,
///     "offset":  <offset>,     -- only if opts.offset is Some
///     "_source": { "includes": [...] },  -- only if opts.fields non-empty
///     "highlight": { "pre_tags": "…", "post_tags": "…" }
///   }
///
/// `opts.lang` becomes `bool.filter.equals.lang = "<lang>"`. This assumes
/// the index was created with a `lang` column (memo §05); indexes without
/// one silently return zero hits for a language filter, which is the
/// honest "no rows match" answer.
///
/// `opts.filter` is a caller-supplied JSON string forwarded verbatim into
/// the `bool.filter` slot. The guest doesn't parse it — if the caller
/// passed nonsense, Manticore rejects the request and the guest surfaces
/// the error.
///
/// # `opts.highlight` and the always-emit-a-snippet policy
///
/// Snippets are always requested from Manticore so `hit.snippet` gets
/// populated on every match. This closes the design gap in memo §10 —
/// the substrate is meant to smart-set `highlight: true` whenever the
/// SPARQL SERVICE body projects `?snippet`, but the `wf_search_rewrite`
/// pass doesn't inspect the body for that today (the URL sugar has no
/// visibility into which columns the caller will bind). Rather than
/// hand the decoder a systematically-missing column, the guest always
/// asks Manticore for the fragment and lets the SPARQL side drop it
/// silently when not projected. Manticore's SNIPPET cost is negligible
/// for a query it's already scoring.
///
/// `opts.highlight` picks the wrapping style, not whether a snippet
/// comes back:
///   * `true`  → Manticore's default `<b>…</b>` wrapping (UI mode).
///   * `false` → empty pre/post tags → plain-text fragment (data mode).
/// The design memo's `snippet: option<string>` return shape is
/// preserved — `None` still happens when Manticore has no matching
/// text to highlight (rare in practice; the response `hits.hits` array
/// wouldn't contain the doc at all in that case).
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
    // Always request a snippet. See the "always-emit-a-snippet policy"
    // note above for the rationale. `opts.highlight` selects tags:
    // `true` = Manticore default (`<b>…</b>`), `false` = plain fragment.
    let highlight_body = if opts.highlight {
        // Empty object = default `<b>` wrapping across all fields; per
        // §11 of the memo.
        JsonValue::Object(Map::new())
    } else {
        json!({ "pre_tags": "", "post_tags": "" })
    };
    body.insert("highlight".into(), highlight_body);

    serde_json::to_string(&JsonValue::Object(body))
        .map_err(|e| format!("wf_fulltext: serialize request body: {e}"))
}

fn build_query_clause(query: &str, opts: &PlainOpts) -> Result<JsonValue, String> {
    let match_clause = json!({ "match": { "*": query } });

    let has_lang = opts.lang.as_deref().map_or(false, |s| !s.is_empty());
    let has_filter = opts.filter.as_deref().map_or(false, |s| !s.is_empty());

    if !has_lang && !has_filter {
        return Ok(match_clause);
    }

    // Wrap in bool { must: [match], filter: [...] }.
    let mut filter_list: Vec<JsonValue> = Vec::new();
    if let Some(lang) = opts.lang.as_deref().filter(|s| !s.is_empty()) {
        filter_list.push(json!({ "equals": { "lang": lang } }));
    }
    if let Some(filter_json) = opts.filter.as_deref().filter(|s| !s.is_empty()) {
        let parsed: JsonValue = serde_json::from_str(filter_json).map_err(|e| {
            format!("wf_fulltext: opts.filter must be valid JSON: {e}")
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

/// Parse Manticore's `POST /search` response into `Hit`s.
///
/// Expected shape:
///   { "hits": { "hits": [
///       { "_id": "...", "_score": 0.9,
///         "_source": {...},
///         "highlight": { "*": ["..."] } },
///       ...
///   ] } }
///
/// Mapping (§11 of the memo):
///   _id                   -> hit.doc
///   _score                -> hit.score           (already f64)
///   highlight.*.[0]       -> hit.snippet         (first snippet if any)
///   _source               -> hit.fields          (string-valued keys only;
///                                                 non-strings are stringified,
///                                                 non-scalar values skipped)
///   _source.lang          -> hit.lang            (if present; else None)
pub fn parse_response(json_str: &str) -> Result<Vec<Hit>, String> {
    let root: JsonValue = serde_json::from_str(json_str)
        .map_err(|e| format!("wf_fulltext: response is not valid JSON: {e}"))?;

    let hits_array = root
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(|h| h.as_array())
        .ok_or_else(|| {
            "wf_fulltext: response missing `hits.hits` array".to_string()
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
        .ok_or_else(|| "wf_fulltext: hit missing `_id`".to_string())?;

    let score = hit
        .get("_score")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| "wf_fulltext: hit missing numeric `_score`".to_string())?;

    let snippet = hit
        .get("highlight")
        .and_then(|h| h.as_object())
        .and_then(|obj| {
            // Take the first non-empty snippet array we find; Manticore's
            // "*" key covers "all fields" but callers requesting a
            // specific field may see a keyed entry instead.
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
        fields,
    })
}

/// Flatten `_source` into `(name, string)` tuples.
///
/// Strings pass through as-is. Numbers and bools are stringified (they're
/// still scalar; the caller may want to see them). Objects and arrays are
/// skipped — the WIT return shape is `list<tuple<string, string>>`, not a
/// nested value, and inventing a JSON-in-string encoding for the exotic
/// cases would hide the shape mismatch from callers. Prefer honest gap.
fn flatten_source(obj: &Map<String, JsonValue>) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        match v {
            JsonValue::String(s) => out.push((k.clone(), s.clone())),
            JsonValue::Number(n) => out.push((k.clone(), n.to_string())),
            JsonValue::Bool(b) => out.push((k.clone(), b.to_string())),
            JsonValue::Null => out.push((k.clone(), String::new())),
            // Object / Array — skip. Non-scalar; not representable in the
            // hit.fields shape without a lossy JSON-in-string encoding.
            _ => {}
        }
    }
    // Stable order for deterministic tests. serde_json's Map preserves
    // insertion order by default, but iteration order of the flattened
    // list should still be predictable if a caller reorders keys.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
