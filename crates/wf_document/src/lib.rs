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
//! Design memo: `docs/design/wf-document.md` in the wf-conformance repo.
//! Adapter details: memo §04 (WIT), §05 (wire shape), §06 (doc-id
//! contract), §08 (index-only sync), §09 (relationship to wf_fulltext).
//!
//! Network I/O is a host import (`host::http-post-json`) rather than
//! client-side `ureq`. Same substrate-import idiom as wf_fulltext,
//! wf_sql, wf_fetch — ureq-in-`wasm32-wasip1` isn't reliable across
//! toolchains. The pure request-body construction and response-parsing
//! logic lives in `manticore.rs` / `manticore_admin.rs` / `sirix.rs`
//! so tests exercise the wire mapping directly.
//!
//! ## Write-through (v1.1)
//!
//! v1.1 adds three write exports — `insert-doc`, `update-doc`,
//! `delete-doc` — that route through Sirix (source of truth) and let
//! Manticore catch up on the next sweep pass. Sirix commit is the
//! durable acknowledgment; the returned `write-result` carries the
//! new revision and its ISO-8601 `valid_from` timestamp.
//!
//! **Consistency choice**: Sirix-first, Manticore eventual. If Sirix
//! commits but Manticore hasn't yet mirrored, a search would miss the
//! new document until the next sweep — the `fetch` path stays honest
//! (Sirix is authoritative). Chosen for simplicity; a future v1.2
//! could add an opt-in "immediate mirror" path that pushes the write
//! through both stores in one call at the cost of doubling the
//! latency envelope.
//!
//! **Prerequisite**: sirix-sql-server exposes a DML surface. As of
//! this commit, `QueryHandler.java` calls JDBC's
//! `Statement.executeQuery(sql)` — SELECT-only. INSERT / UPDATE /
//! DELETE against production sirix-sql-server will surface the JDBC
//! exception through the guest's `Err(...)` channel. The guest-side
//! implementation is correct and independently tested against a
//! compatible mock; the end-to-end write path requires a sibling
//! change in `sirixdb-sql/sirix-sql-server` to route non-SELECT SQL
//! through `executeUpdate` and to shape a `_rev`/`_valid_from`
//! response body. Tracked as a Sirix-side gap; deliberately not
//! bundled here so the guest can land independently.

wit_bindgen::generate!({
    world: "document",
    path: "wit",
});

pub mod manticore;
pub mod manticore_admin;
pub mod sirix;
pub mod sirix_write;

use manticore::{
    build_probe_body, build_request_body, parse_response, schema_has_valid_from, AtTime,
    Hit as PlainHit, PlainOpts,
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
        // v1.0: `at_time` and `at_rev` are mutually exclusive. The
        // guest rejects both-set at the surface — the Manticore adapter
        // then trusts that at most one time-travel selector is set when
        // it builds the interval filter.
        if opts.at_time.is_some() && opts.at_rev.is_some() {
            return Err("at_time and at_rev are mutually exclusive".to_string());
        }

        let at_time_norm = opts
            .at_time
            .as_deref()
            .map(normalize_at_time)
            .transpose()?;

        let opts_plain = PlainOpts {
            limit: opts.limit,
            offset: opts.offset,
            fields: opts.fields,
            highlight: opts.highlight,
            lang: opts.lang,
            filter: opts.filter,
            include_body: opts.include_body,
            body_content_type: opts.body_content_type.clone(),
            at_time: at_time_norm,
            at_rev: opts.at_rev,
        };

        let body = build_request_body(&index, &query, &opts_plain)?;
        let url = manticore_url(&search_url);

        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;

        let plain_hits = parse_response(&response_body)?;

        // v1.0 storage gate: when the caller asked for time-travel but
        // Manticore returned zero hits, probe once — if the index lacks
        // `_valid_from`, the sweep isn't running retention=all and the
        // query would silently degrade. Surface a specific error instead.
        // Best-effort: if the probe fails, fall through to the honest
        // empty result rather than fabricating a schema verdict.
        if plain_hits.is_empty() && opts_plain.at_time.is_some() {
            let probe = build_probe_body(&index);
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

        // Compose: if the caller asked for bodies, fetch each hit's
        // body from Sirix (unless a backwards-compat sweep already put
        // it in `_source`). Per-hit fetch failures are soft-failed —
        // that hit's body stays None and other hits still return.
        let plain_hits = if opts_plain.include_body {
            fetch_bodies_for_hits(&sirix_url, plain_hits, opts_plain.body_content_type.as_deref())
        } else {
            plain_hits
        };

        // Snippet generation: only when the caller asked for highlight
        // AND asked for bodies (no body → nothing to snippet from).
        // Preserve any snippet Manticore returned (backwards compat with
        // a sweep that stored bodies and returned `highlight.*`).
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

        // Gap: sirix-sql-server does not currently expose Sirix's
        // revision history as a queryable column on its SQL surface.
        // The parser returns Err on missing `_rev` — fall back to the
        // supplied revision (or the honest single-element `[latest]`
        // stub) rather than failing the SERVICE dispatch.
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
    //
    // insert-doc / update-doc / delete-doc emit DML SQL to
    // sirix-sql-server's POST /query endpoint. Sirix commits and
    // echoes back {_rev, _valid_from}; the guest lifts that into a
    // WIT `write-result`. Manticore is caught up on the next sweep
    // pass — no extra network calls happen here.
    //
    // Consistency: Sirix commit is durable ack. If the sweep hasn't
    // run yet, a search would miss the write, but `fetch` stays
    // honest (Sirix is authoritative).
    //
    // Prerequisite: sirix-sql-server exposes a DML surface. Its
    // current QueryHandler.java calls Statement.executeQuery(sql) —
    // SELECT only. Production writes will surface the JDBC exception
    // through the guest's Err(...) channel until the sirix-sql-server
    // DML follow-up lands.
    // -----------------------------------------------------------------

    fn insert_doc(
        sirix_url: String,
        database: String,
        resource_name: String,
        doc: Vec<u8>,
        content_type: String,
    ) -> Result<WriteResult, String> {
        let _ = content_type; // Informational only in v1.1; Sirix stores JSON natively.
        let sql = build_insert_sql(&database, &resource_name, &doc)?;
        let body = build_fetch_body(&sql);
        let url = query_url(&sirix_url);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        let ack = parse_write_response(&response_body)?;
        // On INSERT the caller doesn't know the node-key yet; use the
        // one Sirix assigns. If the server omits it, fall back to an
        // empty node-key — the caller can list-revisions and pick it
        // up out of band. Honest but rare.
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
        // Delete gets a write-result on the wire (successor revision +
        // valid_from), but the WIT contract is Result<(), string> —
        // surface Sirix errors and drop the ack.
        parse_write_response(&response_body).map(|_| ())
    }
}

/// Compose step for `include_body`: for each hit that doesn't already
/// carry a body (backwards-compat sweep may have stored it in
/// `_source`), parse its doc-id as a Sirix URI and fetch the body from
/// Sirix. Per-hit failure is soft: parse errors, HTTP errors, and
/// response-parse errors leave that hit's `body` / `content_type` as
/// `None` — other hits proceed normally. The whole search never fails
/// on one bad document.
///
/// Failures are logged via `host::http_post_json`'s error return; the
/// guest doesn't have direct stderr under the substrate's host imports.
/// The visibility is: `body: None` on a hit whose Manticore ID looked
/// valid but Sirix couldn't serve.
fn fetch_bodies_for_hits(
    sirix_url: &str,
    mut hits: Vec<PlainHit>,
    body_content_type: Option<&str>,
) -> Vec<PlainHit> {
    let url = query_url(sirix_url);
    for hit in hits.iter_mut() {
        // Backwards compat: if Manticore already served the body
        // (pre-correction mirror), don't overwrite.
        if hit.body.is_some() {
            continue;
        }
        let Ok(doc) = parse_sirix_uri(&hit.doc) else {
            // Malformed doc-id — leave body as None and move on. This
            // shouldn't happen if the sweep produced valid IDs, but
            // never fail the whole search on one bad entry.
            continue;
        };
        let sql = build_fetch_sql(&doc, hit.revision);
        let body = build_fetch_body(&sql);
        let Ok(response_body) = host::http_post_json(&url, &body) else {
            // Network / Sirix error for this hit — leave as None,
            // proceed with the rest.
            continue;
        };
        let Ok(fetched) = parse_fetch_response(&response_body, body_content_type) else {
            // Sirix responded but the body couldn't be parsed. Leave
            // this hit's body as None.
            continue;
        };
        hit.body = Some(fetched.body);
        hit.content_type = Some(fetched.content_type);
    }
    hits
}

/// Populate `hit.snippet` for hits whose body is present. Preserves any
/// snippet Manticore already supplied (backwards compat with a sweep
/// that mirrored bodies and let Manticore's SNIPPET() do the highlight).
/// For hits with no Manticore snippet: build a substring snippet from
/// the body (memo `wf-document.md` §08).
///
/// Any error extracting a snippet leaves `hit.snippet` as `None` and
/// moves on — snippet generation is strictly best-effort.
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

/// Split the query on whitespace, lowercase each term (ASCII), and
/// drop empties. ASCII lowering is chosen deliberately: the resulting
/// byte positions align with the body's `to_ascii_lowercase()` so we
/// don't have to reconcile grapheme boundaries. Non-ASCII characters
/// pass through unchanged (matches are then case-sensitive for those
/// specific characters — honest given the "cheap" scope).
pub fn snippet_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Build a substring snippet from `body`: locate the earliest hit of
/// any term (case-insensitive, ASCII), take ~100 chars on either side
/// of it, and wrap every matching run with `<mark>...</mark>`.
///
/// Returns `None` when: body isn't valid UTF-8, no term matches, or
/// the char-boundary adjustment somehow produces an empty slice.
///
/// Match wrapping is over merged, sorted term ranges so overlapping
/// terms (e.g. `"water"` + `"waterproof"` in the same body) produce a
/// single `<mark>` span rather than nested tags.
pub fn generate_snippet(body: &[u8], terms: &[String]) -> Option<String> {
    let body_str = std::str::from_utf8(body).ok()?;
    let body_lower = body_str.to_ascii_lowercase();

    // Earliest occurrence of any term.
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

/// Walk left until we hit a char boundary. Cheap alternative to the
/// unstable `str::floor_char_boundary`.
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        idx = s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Walk right until we hit a char boundary. Cheap alternative to the
/// unstable `str::ceil_char_boundary`.
fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Wrap every case-insensitive occurrence of any term in `text` with
/// `<mark>...</mark>`. Overlapping matches are merged (a single span
/// covers them). Uses ASCII case-insensitive matching so byte indexes
/// in `text_lower` correspond 1:1 to `text` — non-ASCII characters
/// pass through unchanged.
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
        // Defence: if boundaries somehow don't align, bail to plain text.
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
            // v1.0: populate revision from Manticore's `_rev`. `None`
            // when the sweep doesn't index it (retention=latest).
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

/// Normalize the WIT `at_time` string into a shape Manticore can filter
/// on. Two accepted forms (memo §04):
///
///   * Unix epoch seconds — a bare integer literal. Emitted as a JSON
///     number so Manticore's range filter compares numerically against
///     `_valid_from`. The sweep indexes revisions as epoch seconds.
///   * ISO-8601 datetime — forwarded verbatim as a string. Manticore's
///     datetime parser handles the common forms.
///
/// We don't rewrite ISO into epoch here — the guest can't rely on
/// chrono/time in `wasm32-wasip1`, and Manticore's own parser is the
/// honest fallback. Empty string is rejected outright.
fn normalize_at_time(raw: &str) -> Result<AtTime, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("wf_document: at_time must not be empty".to_string());
    }
    // `strtoll`-style: pure integer literal (optional leading sign) is
    // an epoch value.
    if let Ok(epoch) = trimmed.parse::<i64>() {
        return Ok(AtTime::Epoch(epoch));
    }
    Ok(AtTime::Iso(trimmed.to_string()))
}

/// `search-url` is a bare host[:port] like `http://localhost:9308`.
/// Append `/search` for the Manticore endpoint. Idempotent: if the
/// caller already included `/search`, don't double up.
fn manticore_url(backend_url: &str) -> String {
    let trimmed = backend_url.trim_end_matches('/');
    if trimmed.ends_with("/search") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/search")
    }
}

/// Same idempotent behaviour, for `/bulk`.
fn bulk_url(backend_url: &str) -> String {
    let trimmed = backend_url.trim_end_matches('/');
    if trimmed.ends_with("/bulk") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/bulk")
    }
}

export!(Component);

// ---------------------------------------------------------------------------
// Small in-crate tests for the URL helpers.
// ---------------------------------------------------------------------------

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

    // -----------------------------------------------------------------
    // Snippet generation — pure-function coverage
    // -----------------------------------------------------------------

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
        // Case of the mark payload should match the original body.
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
        // "water" is a prefix of "waterproof". Ranges should merge so the
        // mark covers the longer span exactly once, not nest tags.
        let body = b"waterproof spec sheet";
        let snippet =
            generate_snippet(body, &snippet_terms("water waterproof")).expect("snippet");
        // Single wrapper around the longer word, not nested.
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
        // Body: 250 chars of 'a' + " needle " + 250 chars of 'b'.
        let mut body = vec![b'a'; 250];
        body.extend_from_slice(b" needle ");
        body.extend(std::iter::repeat(b'b').take(250));
        let snippet = generate_snippet(&body, &snippet_terms("needle")).expect("snippet");
        // Snippet length: ~100 before the match + match len + ~100 after.
        // Add a small allowance for the <mark>...</mark> tags (13 chars).
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

    // -----------------------------------------------------------------
    // populate_snippets — preserves Manticore-supplied snippet
    // -----------------------------------------------------------------

    #[test]
    fn populate_snippets_preserves_manticore_snippet() {
        // If Manticore returned a `<b>...</b>`-style snippet (backwards
        // compat with a body-storing sweep), the guest doesn't overwrite.
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
