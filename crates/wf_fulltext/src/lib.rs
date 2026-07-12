//! wf_fulltext — full-text search as a substrate function.
//!
//! Signature (matching the substrate's `wf:partial` arg model):
//!   `wf:partial(<wf_fulltext.wasm>, "<backend-url>", "<index>",
//!               "<query>", <limit>)`
//!
//! The guest posts a JSON body to `<backend-url>/search` (Manticore's
//! HTTP+JSON endpoint), parses the response into typed `hit`s, and returns
//! them to the substrate as a `list<hit>` for the SERVICE-envelope adapter
//! to surface as SPARQL binding-sets.
//!
//! Design memo: `docs/design/wf-fulltext.md` in the wf-conformance repo.
//! Adapter details: memo §11 (Manticore) and §12 (OpenSearch swap).
//!
//! Network I/O is a host import (`host::http-post-json`) rather than
//! client-side `ureq`. Every other guest that talks out-of-sandbox
//! (`wf_sql`, `wf_fetch`) uses the same substrate-import idiom because
//! ureq-in-`wasm32-wasip1` isn't reliable across toolchains. The
//! request-body construction and response-parsing logic lives in
//! `manticore.rs` so tests can exercise the wire mapping directly.

wit_bindgen::generate!({
    world: "fulltext",
    path: "wit",
});

pub mod manticore;
pub mod manticore_admin;

use manticore::{build_request_body, parse_response, PlainOpts};
use manticore_admin::{
    build_bulk_body, build_delete_body, parse_bulk_response, PlainDocWrite,
};
use wf::fulltext::host;

struct Component;

impl Guest for Component {
    fn search(
        backend_url: String,
        index: String,
        query: String,
        opts: QueryOpts,
    ) -> Result<Vec<Hit>, String> {
        // WIT record -> plain-Rust QueryOpts for the pure mapping module.
        let opts_plain = PlainOpts {
            limit: opts.limit,
            offset: opts.offset,
            fields: opts.fields,
            highlight: opts.highlight,
            lang: opts.lang,
            filter: opts.filter,
        };

        let body = build_request_body(&index, &query, &opts_plain)?;
        let url = manticore_url(&backend_url);

        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_fulltext: POST {url}: {e}"))?;

        let plain_hits = parse_response(&response_body)?;

        // Plain-Rust Hit -> WIT record.
        Ok(plain_hits
            .into_iter()
            .map(|h| Hit {
                doc: h.doc,
                score: h.score,
                snippet: h.snippet,
                lang: h.lang,
                fields: h.fields,
            })
            .collect())
    }

    fn insert_batch(
        backend_url: String,
        index: String,
        docs: Vec<DocWrite>,
    ) -> Result<u32, String> {
        if docs.is_empty() {
            return Ok(0);
        }
        // WIT -> plain-Rust for the pure mapping module.
        let plain: Vec<PlainDocWrite> = docs
            .into_iter()
            .map(|d| PlainDocWrite {
                id: d.id,
                fields: d.fields,
                lang: d.lang,
            })
            .collect();
        let body = build_bulk_body(&index, &plain);
        let url = bulk_url(&backend_url);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_fulltext: POST {url}: {e}"))?;
        parse_bulk_response(&response_body)
    }

    fn delete_batch(
        backend_url: String,
        index: String,
        ids: Vec<String>,
    ) -> Result<u32, String> {
        if ids.is_empty() {
            return Ok(0);
        }
        let body = build_delete_body(&index, &ids);
        let url = bulk_url(&backend_url);
        let response_body = host::http_post_json(&url, &body)
            .map_err(|e| format!("wf_fulltext: POST {url}: {e}"))?;
        parse_bulk_response(&response_body)
    }
}

/// `backend-url` is a bare host[:port] like `http://localhost:9308`. Append
/// `/search` for the Manticore endpoint. Idempotent: if the caller already
/// included `/search`, don't double up.
fn manticore_url(backend_url: &str) -> String {
    let trimmed = backend_url.trim_end_matches('/');
    if trimmed.ends_with("/search") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/search")
    }
}

/// Same idempotent behaviour as [`manticore_url`], but for the `/bulk`
/// admin endpoint that `insert-batch` / `delete-batch` POST to. If the
/// caller already terminated their backend URL with `/bulk`, don't
/// double up.
fn bulk_url(backend_url: &str) -> String {
    let trimmed = backend_url.trim_end_matches('/');
    if trimmed.ends_with("/bulk") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/bulk")
    }
}

export!(Component);
