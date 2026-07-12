//! wf_document — search + storage as one substrate function.
//!
//! One guest, two backends: Manticore for search, SirixDB (via
//! sirix-sql-server) for durable, versioned document storage. When
//! `opts.include-body` is true, `search` composes both backends: first
//! it queries Manticore, then it iterates hits and fetches each body
//! from Sirix inline, returning joined results in a single SERVICE
//! dispatch.
//!
//! Design memo: `docs/design/wf-document.md` in the wf-conformance repo.
//! Adapter details: memo §04 (WIT), §05 (wire shape), §06 (doc-id
//! contract), §09 (relationship to wf_fulltext).
//!
//! Network I/O is a host import (`host::http-post-json`) rather than
//! client-side `ureq`. Same substrate-import idiom as wf_fulltext,
//! wf_sql, wf_fetch — ureq-in-`wasm32-wasip1` isn't reliable across
//! toolchains. The pure request-body construction and response-parsing
//! logic lives in `manticore.rs` / `manticore_admin.rs` / `sirix.rs`
//! so tests exercise the wire mapping directly.

wit_bindgen::generate!({
    world: "document",
    path: "wit",
});

pub mod manticore;
pub mod manticore_admin;
pub mod sirix;

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

        // Compose: if the caller asked for bodies, fetch each hit from
        // Sirix and populate `body` + `content_type` before returning.
        let plain_hits = if opts_plain.include_body {
            fetch_bodies_for_hits(&sirix_url, plain_hits, opts_plain.body_content_type.as_deref())?
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
}

/// Compose step for `include_body`: for each hit, parse its doc-id as a
/// Sirix URI, run a fetch, and populate `body` + `content_type`. Fails
/// the whole search only if the parse fails — a per-hit Sirix fetch
/// failure is elevated to a full error too, so the caller can't
/// silently miss bodies. (An alternative would be per-hit soft-fail;
/// v0.2 opts for the loud choice.)
fn fetch_bodies_for_hits(
    sirix_url: &str,
    mut hits: Vec<PlainHit>,
    body_content_type: Option<&str>,
) -> Result<Vec<PlainHit>, String> {
    let url = query_url(sirix_url);
    for hit in hits.iter_mut() {
        let doc = parse_sirix_uri(&hit.doc)?;
        let sql = build_fetch_sql(&doc, None);
        let body = build_fetch_body(&sql);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_document: POST {url}: {e}"))?;
        let fetched = parse_fetch_response(&response_body, body_content_type)?;
        hit.body = Some(fetched.body);
        hit.content_type = Some(fetched.content_type);
    }
    Ok(hits)
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
}
