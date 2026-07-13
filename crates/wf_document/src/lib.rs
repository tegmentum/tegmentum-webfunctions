//! wf_document — search + storage as one substrate function.
//!
//! One guest, two backends: Manticore for search, SirixDB (via
//! sirix-sql-server) for durable, versioned document storage. Under the
//! index-only design (memo `wf-document.md` §08 and v1.0 §03),
//! **Manticore holds the inverted index only** — document bodies live
//! in Sirix. When `opts.include-body` is true, the guest queries
//! Manticore for hits, then iterates each hit and fetches its body from
//! Sirix inline, returning joined results in a single SERVICE dispatch.
//! When `opts.include-body` is false, bodies are never fetched — the
//! caller sees only IDs / scores / (optional) snippets.
//!
//! Per-hit Sirix fetch failures are **soft-failed**: the offending hit's
//! `body` / `content_type` stay `None` and the search returns the other
//! hits normally. A single bad document never fails the whole SERVICE
//! call.
//!
//! Snippet generation (memo §08) is guest-side: Manticore can't extract
//! snippets from text it doesn't store, so when `highlight: true` AND
//! `include_body: true`, the guest builds a substring snippet from the
//! Sirix body — first query-term hit with ~100 chars of context on each
//! side, matching runs wrapped in `<mark>...</mark>`. Not linguistic
//! like Manticore's built-in SNIPPET() (which stems and inflects), but
//! honest and cheap. When `include_body: false`, `hit.snippet` stays
//! `None` — no body, no snippet.
//!
//! Backwards compat: a corpus mirrored before the design correction may
//! still carry `body` / `content_type` in Manticore's `_source`. The
//! adapter picks them up when present and the compose step skips the
//! Sirix round-trip for those hits.
//!
//! # v1.1 write-through (already shipped) and v1.2 search-side extensions
//!
//! v1.1 landed write-through: `insert-doc`, `update-doc`, `delete-doc`.
//! Writes go into Sirix (source of truth); Manticore catches up on the
//! periodic sweep. See `sirix_write.rs` for the DML SQL builders and
//! the write-ack response parser.
//!
//! v1.2 layers four search-side capabilities on top of that (from the
//! memos' deferred list — `wf-document.md` §12, `wf-document-v1.md`
//! §08). Existing calls behave byte-for-byte the same when the new
//! opts fields are None.
//!
//!   * **Cross-index queries** — `search-opts.indexes` (when Some and
//!     non-empty) unions multiple registered indexes into a single
//!     Manticore request via the comma-separated `table` field. Every
//!     v1.2 export (`search`, `facet`, `more-like-this`, `vector-search`)
//!     honors it; the `index` positional stays the DEFAULT when
//!     `indexes` is None or empty. Search + fetch coexist unchanged.
//!   * **Faceting** — new `facet` export aggregates hits by metadata
//!     field. Same Manticore search backend, extra `aggs` clauses in
//!     the body. Returns `list<facet-result>` in the caller's field
//!     order. Faceting never fetches bodies from Sirix.
//!   * **MoreLikeThis** — new `more-like-this` export. Manticore has no
//!     native MLT; the guest fetches the seed doc's body from Sirix,
//!     extracts up to 10 top-TF alphanumeric terms (length > 3), and
//!     issues an OR-match against Manticore excluding the seed's `_id`.
//!     Same compose surface as `search` — `include_body: true` triggers
//!     a Sirix round-trip for every hit body.
//!   * **Vector search** — new `vector-search` export. Passes the caller's
//!     vector verbatim to Manticore's k-NN operator. The target index
//!     MUST have a `vector` field declared — operator's responsibility.
//!     This substrate never generates embeddings; that's a sweep-side
//!     or separate ingestion concern.
//!
//! Network I/O is a host import (`host::http-post-json`) rather than
//! client-side `ureq`. Same substrate-import idiom as wf_fulltext,
//! wf_sql, wf_fetch — ureq-in-`wasm32-wasip1` isn't reliable across
//! toolchains. The pure request-body construction and response-parsing
//! logic lives in `manticore.rs` / `manticore_admin.rs` / `sirix.rs` /
//! `sirix_write.rs` so tests exercise the wire mapping directly. No new
//! host imports for v1.2: every new capability dispatches through the
//! same `http-post-json` primitive.

wit_bindgen::generate!({
    world: "document",
    path: "wit",
});

pub mod manticore;
pub mod manticore_admin;
pub mod sirix;
pub mod sirix_write;

use manticore::{
    build_facet_body, build_mlt_body, build_probe_body, build_request_body,
    build_vector_body, extract_top_terms, parse_facet_response, parse_response,
    schema_has_valid_from, AtTime, Hit as PlainHit, PlainFacetResult, PlainOpts,
};
use manticore_admin::{
    build_bulk_body, build_delete_body, parse_bulk_response, PlainDocWrite,
};
use sirix::{
    build_fetch_body, build_fetch_sql, build_revisions_sql, parse_fetch_response,
    parse_revisions_response, parse_sirix_uri, query_url,
};
use sirix_write::{
    build_delete_sql, build_insert_sql, build_update_sql, parse_write_response,
};
use wf::document::host;

struct Component;

impl Guest for Component {
    fn search(
        search_url: String,
        sirix_url: String,
        index: String,
        query: String,
        opts: SearchOpts,
    ) -> Result<Vec<Hit>, String> {
        if opts.at_time.is_some() && opts.at_rev.is_some() {
            return Err("at_time and at_rev are mutually exclusive".to_string());
        }
        let has_range = opts.after.is_some() || opts.before.is_some();
        if has_range && (opts.at_time.is_some() || opts.at_rev.is_some()) {
            return Err(
                "after/before are mutually exclusive with at_time and at_rev".to_string(),
            );
        }

        let opts_plain = wit_opts_to_plain(&opts)?;

        // v1.2: empty range (after > before after ISO parse) is honest —
        // return the empty hit list rather than round-tripping to
        // Manticore for a guaranteed-empty response.
        if is_empty_range(opts_plain.after.as_ref(), opts_plain.before.as_ref()) {
            return Ok(vec![]);
        }

        let body = build_request_body(&index, &query, &opts_plain)?;
        let url = manticore_url(&search_url);

        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;

        let plain_hits = parse_response(&response_body)?;

        if plain_hits.is_empty() && opts_plain.at_time.is_some() {
            let probe_table = if opts_plain.indexes.is_empty() {
                index.as_str()
            } else {
                ""
            };
            if !probe_table.is_empty() {
                let probe = build_probe_body(probe_table);
                if let Ok(probe_body) = host::http_post_json(&url, &probe) {
                    if !schema_has_valid_from(&probe_body) {
                        return Err(
                            "time-travel search requires retention=all sweep; \
                             index appears to hold latest-only"
                                .to_string(),
                        );
                    }
                }
            }
        }

        let plain_hits = if opts_plain.include_body {
            fetch_bodies_for_hits(&sirix_url, plain_hits, opts_plain.body_content_type.as_deref())
        } else {
            plain_hits
        };

        let plain_hits = if opts_plain.include_body && opts_plain.highlight {
            populate_snippets(plain_hits, &query)
        } else {
            plain_hits
        };

        Ok(plain_hits.into_iter().map(hit_wit).collect())
    }

    fn fetch(sirix_url: String, doc: DocRef) -> Result<Document, String> {
        let plain_doc = parse_sirix_uri(&doc.id)?;
        let sql = build_fetch_sql(&plain_doc, doc.revision);
        let body = build_fetch_body(&sql);
        let url = query_url(&sirix_url);

        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;

        let fetched = parse_fetch_response(&response_body, None)?;

        Ok(Document {
            doc: DocRef {
                id: doc.id,
                revision: doc.revision,
            },
            body: fetched.body,
            content_type: fetched.content_type,
        })
    }

    fn list_revisions(sirix_url: String, doc: DocRef) -> Result<Vec<u64>, String> {
        let plain_doc = parse_sirix_uri(&doc.id)?;
        let sql = build_revisions_sql(&plain_doc);
        let body = build_fetch_body(&sql);
        let url = query_url(&sirix_url);

        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;

        match parse_revisions_response(&response_body) {
            Ok(revs) if !revs.is_empty() => Ok(revs),
            Ok(_) | Err(_) => Ok(vec![doc.revision.unwrap_or(1)]),
        }
    }

    fn insert_batch(
        search_url: String,
        index: String,
        docs: Vec<DocWrite>,
    ) -> Result<u32, String> {
        if docs.is_empty() {
            return Ok(0);
        }
        let plain: Vec<PlainDocWrite> = docs
            .into_iter()
            .map(|d| PlainDocWrite {
                id: d.id,
                fields: d.fields,
                lang: d.lang,
            })
            .collect();
        let body = build_bulk_body(&index, &plain);
        let url = bulk_url(&search_url);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        parse_bulk_response(&response_body)
    }

    fn delete_batch(
        search_url: String,
        index: String,
        ids: Vec<String>,
    ) -> Result<u32, String> {
        if ids.is_empty() {
            return Ok(0);
        }
        let body = build_delete_body(&index, &ids);
        let url = bulk_url(&search_url);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        parse_bulk_response(&response_body)
    }

    // -----------------------------------------------------------------
    // v1.1 write-through — Sirix-first, Manticore eventual
    // -----------------------------------------------------------------

    fn insert_doc(
        sirix_url: String,
        database: String,
        resource_name: String,
        doc: Vec<u8>,
        content_type: String,
    ) -> Result<WriteResult, String> {
        let _ = content_type;
        let sql = build_insert_sql(&database, &resource_name, &doc)?;
        let body = build_fetch_body(&sql);
        let url = query_url(&sirix_url);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        let ack = parse_write_response(&response_body)?;
        let node_key = ack.node_key.clone().unwrap_or_default();
        Ok(WriteResult {
            doc: DocRef {
                id: format!("sirix://{database}/{resource_name}/{node_key}"),
                revision: Some(ack.revision),
            },
            valid_from: ack.valid_from,
        })
    }

    fn update_doc(
        sirix_url: String,
        doc: DocRef,
        body: Vec<u8>,
    ) -> Result<WriteResult, String> {
        let plain_doc = parse_sirix_uri(&doc.id)?;
        let sql = build_update_sql(&plain_doc, &body, doc.revision)?;
        let json_body = build_fetch_body(&sql);
        let url = query_url(&sirix_url);
        let response_body = host::http_post_json(&url, &json_body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        let ack = parse_write_response(&response_body)?;
        Ok(WriteResult {
            doc: DocRef {
                id: doc.id,
                revision: Some(ack.revision),
            },
            valid_from: ack.valid_from,
        })
    }

    fn delete_doc(sirix_url: String, doc: DocRef) -> Result<(), String> {
        let plain_doc = parse_sirix_uri(&doc.id)?;
        let sql = build_delete_sql(&plain_doc);
        let json_body = build_fetch_body(&sql);
        let url = query_url(&sirix_url);
        let response_body = host::http_post_json(&url, &json_body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        parse_write_response(&response_body).map(|_| ())
    }

    // -----------------------------------------------------------------
    // v1.2 — faceting
    // -----------------------------------------------------------------
    fn facet(
        search_url: String,
        index: String,
        query: String,
        fields: Vec<String>,
        opts: SearchOpts,
    ) -> Result<Vec<FacetResult>, String> {
        if opts.at_time.is_some() && opts.at_rev.is_some() {
            return Err("at_time and at_rev are mutually exclusive".to_string());
        }
        let opts_plain = wit_opts_to_plain(&opts)?;
        let body = build_facet_body(&index, &query, &fields, &opts_plain)?;
        let url = manticore_url(&search_url);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        let plain = parse_facet_response(&response_body, &fields)?;
        Ok(plain.into_iter().map(facet_result_wit).collect())
    }

    // -----------------------------------------------------------------
    // v1.2 — MoreLikeThis
    // -----------------------------------------------------------------
    fn more_like_this(
        search_url: String,
        sirix_url: String,
        index: String,
        doc: DocRef,
        limit: u32,
        opts: SearchOpts,
    ) -> Result<Vec<Hit>, String> {
        if opts.at_time.is_some() && opts.at_rev.is_some() {
            return Err("at_time and at_rev are mutually exclusive".to_string());
        }
        let opts_plain = wit_opts_to_plain(&opts)?;

        let plain_doc = parse_sirix_uri(&doc.id)?;
        let sql = build_fetch_sql(&plain_doc, doc.revision);
        let body = build_fetch_body(&sql);
        let sirix_query_url = query_url(&sirix_url);
        let response_body = host::http_post_json(&sirix_query_url, &body)
            .map_err(|e| format!("wf_document: POST {sirix_query_url}: {e}"))?;
        let seed = parse_fetch_response(&response_body, None)?;

        let terms = extract_top_terms(&seed.body, 10);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let mlt_body = build_mlt_body(&index, &doc.id, &terms, limit, &opts_plain)?;
        let search_endpoint = manticore_url(&search_url);
        let manticore_response = host::http_post_json(&search_endpoint, &mlt_body)
            .map_err(|e| format!("wf_document: POST {search_endpoint}: {e}"))?;
        let plain_hits = parse_response(&manticore_response)?;

        let plain_hits = if opts_plain.include_body {
            fetch_bodies_for_hits(&sirix_url, plain_hits, opts_plain.body_content_type.as_deref())
        } else {
            plain_hits
        };
        let query_for_snippets = terms.join(" ");
        let plain_hits = if opts_plain.include_body && opts_plain.highlight {
            populate_snippets(plain_hits, &query_for_snippets)
        } else {
            plain_hits
        };

        Ok(plain_hits.into_iter().map(hit_wit).collect())
    }

    // -----------------------------------------------------------------
    // v1.2 — vector search
    // -----------------------------------------------------------------
    fn vector_search(
        search_url: String,
        index: String,
        vector: Vec<f32>,
        k: u32,
        opts: SearchOpts,
    ) -> Result<Vec<Hit>, String> {
        if opts.at_time.is_some() && opts.at_rev.is_some() {
            return Err("at_time and at_rev are mutually exclusive".to_string());
        }
        let opts_plain = wit_opts_to_plain(&opts)?;
        let body = build_vector_body(&index, &vector, k, &opts_plain)?;
        let url = manticore_url(&search_url);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        let plain_hits = parse_response(&response_body)?;
        Ok(plain_hits.into_iter().map(hit_wit).collect())
    }
}

fn fetch_bodies_for_hits(
    sirix_url: &str,
    mut hits: Vec<PlainHit>,
    body_content_type: Option<&str>,
) -> Vec<PlainHit> {
    let url = query_url(sirix_url);
    for hit in hits.iter_mut() {
        if hit.body.is_some() {
            continue;
        }
        let Ok(doc) = parse_sirix_uri(&hit.doc) else {
            continue;
        };
        let sql = build_fetch_sql(&doc, hit.revision);
        let body = build_fetch_body(&sql);
        let Ok(response_body) = host::http_post_json(&url, &body) else {
            continue;
        };
        let Ok(fetched) = parse_fetch_response(&response_body, body_content_type) else {
            continue;
        };
        hit.body = Some(fetched.body);
        hit.content_type = Some(fetched.content_type);
    }
    hits
}

pub fn populate_snippets(mut hits: Vec<PlainHit>, query: &str) -> Vec<PlainHit> {
    let terms = snippet_terms(query);
    if terms.is_empty() {
        return hits;
    }
    for hit in hits.iter_mut() {
        if hit.snippet.is_some() {
            continue;
        }
        let Some(body) = hit.body.as_deref() else {
            continue;
        };
        hit.snippet = generate_snippet(body, &terms);
    }
    hits
}

pub fn snippet_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

pub fn generate_snippet(body: &[u8], terms: &[String]) -> Option<String> {
    let body_str = std::str::from_utf8(body).ok()?;
    let body_lower = body_str.to_ascii_lowercase();

    let first_match = terms
        .iter()
        .filter_map(|t| body_lower.find(t.as_str()))
        .min()?;

    let raw_start = first_match.saturating_sub(100);
    let raw_end = (first_match + 100).min(body_str.len());
    let start = floor_char_boundary(body_str, raw_start);
    let end = ceil_char_boundary(body_str, raw_end);
    if start >= end {
        return None;
    }

    let slice = &body_str[start..end];
    Some(mark_terms(slice, terms))
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        idx = s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn mark_terms(text: &str, terms: &[String]) -> String {
    let text_lower = text.to_ascii_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        if term.is_empty() {
            continue;
        }
        let mut pos = 0;
        while let Some(idx) = text_lower[pos..].find(term.as_str()) {
            let s = pos + idx;
            let e = s + term.len();
            ranges.push((s, e));
            pos = e;
        }
    }
    if ranges.is_empty() {
        return text.to_string();
    }
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r.0 <= last.1 {
                last.1 = last.1.max(r.1);
                continue;
            }
        }
        merged.push(r);
    }
    let mut out = String::with_capacity(text.len() + merged.len() * 13);
    let mut cursor = 0;
    for (s, e) in merged {
        if !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            return text.to_string();
        }
        out.push_str(&text[cursor..s]);
        out.push_str("<mark>");
        out.push_str(&text[s..e]);
        out.push_str("</mark>");
        cursor = e;
    }
    out.push_str(&text[cursor..]);
    out
}

fn hit_wit(h: PlainHit) -> Hit {
    Hit {
        doc: DocRef {
            id: h.doc,
            revision: h.revision,
        },
        score: h.score,
        snippet: h.snippet,
        lang: h.lang,
        body: h.body,
        content_type: h.content_type,
        fields: h.fields,
    }
}

/// v1.2: convert a `PlainFacetResult` into the WIT-side `FacetResult`.
fn facet_result_wit(r: PlainFacetResult) -> FacetResult {
    use wf::document::types::FacetValue;
    FacetResult {
        field: r.field,
        values: r
            .values
            .into_iter()
            .map(|v| FacetValue {
                value: v.value,
                count: v.count,
            })
            .collect(),
    }
}

/// Fold a WIT `search-opts` record into the plain-Rust `PlainOpts` the
/// pure request-body builders in `manticore.rs` consume. Normalizes
/// `at_time` in the process. Broken out so every v1.2 export shares one
/// conversion path.
fn wit_opts_to_plain(opts: &SearchOpts) -> Result<PlainOpts, String> {
    let at_time_norm = opts
        .at_time
        .as_deref()
        .map(normalize_at_time)
        .transpose()?;
    let after_norm = opts
        .after
        .as_deref()
        .map(normalize_at_time)
        .transpose()?;
    let before_norm = opts
        .before
        .as_deref()
        .map(normalize_at_time)
        .transpose()?;
    Ok(PlainOpts {
        limit: opts.limit,
        offset: opts.offset,
        fields: opts.fields.clone(),
        highlight: opts.highlight,
        lang: opts.lang.clone(),
        filter: opts.filter.clone(),
        include_body: opts.include_body,
        body_content_type: opts.body_content_type.clone(),
        at_time: at_time_norm,
        at_rev: opts.at_rev,
        indexes: opts.indexes.clone().unwrap_or_default(),
        after: after_norm,
        before: before_norm,
    })
}

/// v1.2 range mode: returns true when `after > before` after normalization.
/// Empty range is honest: the guest returns `Ok(vec![])` rather than an
/// error — the caller asked for a well-defined empty slice.
///
/// The comparison keeps epoch<->epoch and iso<->iso paths separate; the
/// mixed case is left permissive (returns false) so the request goes to
/// Manticore, which will do the string-vs-number coercion itself. This
/// preserves parity with `at_time`'s Manticore-side normalization.
fn is_empty_range(after: Option<&AtTime>, before: Option<&AtTime>) -> bool {
    match (after, before) {
        (Some(AtTime::Epoch(a)), Some(AtTime::Epoch(b))) => a > b,
        (Some(AtTime::Iso(a)), Some(AtTime::Iso(b))) => a > b,
        _ => false,
    }
}

fn normalize_at_time(raw: &str) -> Result<AtTime, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("wf_document: at_time must not be empty".to_string());
    }
    if let Ok(epoch) = trimmed.parse::<i64>() {
        return Ok(AtTime::Epoch(epoch));
    }
    Ok(AtTime::Iso(trimmed.to_string()))
}

fn manticore_url(backend_url: &str) -> String {
    let trimmed = backend_url.trim_end_matches('/');
    if trimmed.ends_with("/search") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/search")
    }
}

fn bulk_url(backend_url: &str) -> String {
    let trimmed = backend_url.trim_end_matches('/');
    if trimmed.ends_with("/bulk") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/bulk")
    }
}

export!(Component);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manticore_url_idempotent() {
        assert_eq!(manticore_url("http://x:9308"), "http://x:9308/search");
        assert_eq!(manticore_url("http://x:9308/"), "http://x:9308/search");
        assert_eq!(manticore_url("http://x:9308/search"), "http://x:9308/search");
    }

    #[test]
    fn bulk_url_idempotent() {
        assert_eq!(bulk_url("http://x:9308"), "http://x:9308/bulk");
        assert_eq!(bulk_url("http://x:9308/bulk"), "http://x:9308/bulk");
    }

    #[test]
    fn normalize_at_time_epoch_seconds() {
        assert_eq!(normalize_at_time("1735689600").unwrap(), AtTime::Epoch(1735689600));
        assert_eq!(normalize_at_time("0").unwrap(), AtTime::Epoch(0));
    }

    #[test]
    fn normalize_at_time_iso_8601_verbatim() {
        assert_eq!(
            normalize_at_time("2026-01-01T00:00:00Z").unwrap(),
            AtTime::Iso("2026-01-01T00:00:00Z".into())
        );
    }

    #[test]
    fn normalize_at_time_trims_whitespace() {
        assert_eq!(normalize_at_time("  2026-01-01  ").unwrap(), AtTime::Iso("2026-01-01".into()));
    }

    #[test]
    fn normalize_at_time_empty_errors() {
        assert!(normalize_at_time("").is_err());
        assert!(normalize_at_time("   ").is_err());
    }

    #[test]
    fn snippet_wraps_matching_term_with_mark() {
        let body = b"The quick brown fox jumps over the lazy dog.";
        let snippet =
            generate_snippet(body, &snippet_terms("fox")).expect("snippet from body");
        assert!(
            snippet.contains("<mark>fox</mark>"),
            "expected <mark>fox</mark>, got: {snippet}"
        );
    }

    #[test]
    fn snippet_case_insensitive_ascii() {
        let body = b"Waterproof rig for outdoor use";
        let snippet =
            generate_snippet(body, &snippet_terms("WATERPROOF")).expect("snippet");
        assert!(
            snippet.contains("<mark>Waterproof</mark>"),
            "case preserved in mark payload: {snippet}"
        );
    }

    #[test]
    fn snippet_multi_term_all_wrapped() {
        let body = b"waterproof tarp with sturdy grommets";
        let snippet =
            generate_snippet(body, &snippet_terms("tarp grommets")).expect("snippet");
        assert!(snippet.contains("<mark>tarp</mark>"), "tarp wrapped: {snippet}");
        assert!(
            snippet.contains("<mark>grommets</mark>"),
            "grommets wrapped: {snippet}"
        );
    }

    #[test]
    fn snippet_overlapping_terms_merge_into_one_mark() {
        let body = b"waterproof spec sheet";
        let snippet =
            generate_snippet(body, &snippet_terms("water waterproof")).expect("snippet");
        assert!(
            snippet.contains("<mark>waterproof</mark>"),
            "merged mark: {snippet}"
        );
        assert!(
            !snippet.contains("<mark><mark>"),
            "no nested marks: {snippet}"
        );
    }

    #[test]
    fn snippet_no_match_returns_none() {
        let body = b"The quick brown fox jumps over the lazy dog";
        assert!(generate_snippet(body, &snippet_terms("zebra")).is_none());
    }

    #[test]
    fn snippet_empty_body_returns_none() {
        assert!(generate_snippet(b"", &snippet_terms("fox")).is_none());
    }

    #[test]
    fn snippet_non_utf8_body_returns_none() {
        let bad = &[0xff, 0xfe, 0xfd];
        assert!(generate_snippet(bad, &snippet_terms("fox")).is_none());
    }

    #[test]
    fn snippet_takes_roughly_100_chars_each_side() {
        let mut body = vec![b'a'; 250];
        body.extend_from_slice(b" needle ");
        body.extend(std::iter::repeat(b'b').take(250));
        let snippet = generate_snippet(&body, &snippet_terms("needle")).expect("snippet");
        assert!(
            snippet.len() < 250,
            "snippet should be windowed, got {} chars",
            snippet.len()
        );
        assert!(snippet.contains("<mark>needle</mark>"));
    }

    #[test]
    fn snippet_terms_split_and_lowercase() {
        assert_eq!(
            snippet_terms("Waterproof  TARP"),
            vec!["waterproof".to_string(), "tarp".to_string()]
        );
        assert!(snippet_terms("   ").is_empty());
    }

    #[test]
    fn populate_snippets_preserves_manticore_snippet() {
        let hit = PlainHit {
            doc: "sirix://docs/x/1".into(),
            score: 1.0,
            snippet: Some("<b>fox</b> from Manticore".into()),
            lang: None,
            body: Some(b"the quick brown fox".to_vec()),
            content_type: Some("text/plain".into()),
            fields: vec![],
            revision: None,
        };
        let out = populate_snippets(vec![hit], "fox");
        assert_eq!(
            out[0].snippet.as_deref(),
            Some("<b>fox</b> from Manticore")
        );
    }

    #[test]
    fn populate_snippets_generates_when_missing() {
        let hit = PlainHit {
            doc: "sirix://docs/x/1".into(),
            score: 1.0,
            snippet: None,
            lang: None,
            body: Some(b"the quick brown fox".to_vec()),
            content_type: Some("text/plain".into()),
            fields: vec![],
            revision: None,
        };
        let out = populate_snippets(vec![hit], "fox");
        assert!(out[0].snippet.as_deref().unwrap().contains("<mark>fox</mark>"));
    }

    #[test]
    fn populate_snippets_body_none_leaves_snippet_none() {
        let hit = PlainHit {
            doc: "sirix://docs/x/1".into(),
            score: 1.0,
            snippet: None,
            lang: None,
            body: None,
            content_type: None,
            fields: vec![],
            revision: None,
        };
        let out = populate_snippets(vec![hit], "fox");
        assert_eq!(out[0].snippet, None);
    }
}
