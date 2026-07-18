//! wf_fulltext — full-text search as a substrate function.
//!
//! # Well-known-exports migration
//!
//! This crate is a prototype landing for the well-known-exports
//! memo (~/git/webfunction-wit/docs/design/well-known-exports.md).
//! The query-facing dispatch surface migrates from a bare
//! `search(backend-url, index, query, opts)` export at the world
//! root to the named `tegmentum:webfunction/search@0.1.0` interface
//! — hosts that recognize the named interface dispatch through it
//! natively; hosts on the runtime-bridging path keep using the
//! extension.call ladder.
//!
//! # Shape adaptations
//!
//! * `backend-url` and `index` moved from positional guest args
//!   into environment variables (`WF_FULLTEXT_BACKEND_URL`,
//!   `WF_FULLTEXT_INDEX`). The well-known `search-request` only
//!   carries `query` / `limit` / `offset`; substrate operators set
//!   the backend location at wasm-runtime configuration time.
//!   Defaults: `http://localhost:9308` and `wf_fulltext`.
//! * Extended query-opts fields (`fields`, `highlight`, `lang`,
//!   `filter`) are dropped from the named-interface surface. The
//!   crate always requests a snippet from Manticore per the
//!   always-emit-a-snippet policy documented in `manticore.rs`.
//! * `search-hit.subject` receives the legacy `hit.doc` wrapped as
//!   a named-node IRI. Legacy `hit.lang` / `hit.fields` drop off
//!   the named-interface return; downstream consumers that need
//!   them can call through the crate's original wire shape via the
//!   admin bare exports.
//!
//! # Retained bare exports
//!
//! `insert-batch` and `delete-batch` stay as world-level bare
//! exports (the well-known-exports memo covers only search /
//! execute / embed today). A future admin-facing well-known
//! interface folds these in without a source change here.
//!
//! # Non-search stubs
//!
//! The world includes `tegmentum:webfunction/sparql-extension` for
//! substrate consistency — the `extension` / `aggregate` /
//! `property-function` interfaces are exported with empty
//! registries so host-side dispatch (through the extension.call
//! ladder OR the bindgen-side lookup) sees a consistent surface.

pub mod manticore;
pub mod manticore_admin;

// wit-bindgen-generated bindings + Guest impls are gated on the wasm
// target so `cargo test` on the native host (rlib crate-type) can
// exercise the pure adapter modules without pulling in wit-bindgen's
// wasm-only cabi_post exports (which use `:` / `/` / `@` in symbol
// names — invalid for the native linker). Matches the pattern
// wf_sagegraph established.
#[cfg(target_family = "wasm")]
#[allow(warnings)]
mod bindings;

#[cfg(target_family = "wasm")]
mod wasm_glue {
    use super::manticore::{build_request_body, parse_response, PlainOpts};
    use super::manticore_admin::{
        build_bulk_body, build_delete_body, parse_bulk_response, PlainDocWrite,
    };

    use super::bindings;
    use bindings::exports::tegmentum::webfunction::aggregate::{
        AggregateDescriptor, AggregateState, Guest as AggregateGuest, GuestAggregateState,
    };
    use bindings::exports::tegmentum::webfunction::extension::{
        FunctionDescriptor, Guest as ExtensionGuest,
    };
    use bindings::exports::tegmentum::webfunction::property_function::{
        BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
    };
    use bindings::exports::tegmentum::webfunction::search::{
        Guest as SearchGuest, SearchError, SearchHit, SearchRequest,
    };
    use bindings::stardog::wf_fulltext::host;
    use bindings::tegmentum::webfunction::types::Term as WitTerm;
    use bindings::DocWrite;

    pub(crate) struct Component;

    // ---------------------------------------------------------------
    // Named-extension `search` — well-known-exports memo §4.
    // ---------------------------------------------------------------

    impl SearchGuest for Component {
        fn search(request: SearchRequest) -> Result<Vec<SearchHit>, SearchError> {
            let backend_url = env_or("WF_FULLTEXT_BACKEND_URL", "http://localhost:9308");
            let index = env_or("WF_FULLTEXT_INDEX", "wf_fulltext");

            // Extended opts (fields / highlight / lang / filter) drop
            // off the named-interface surface. `highlight = true` keeps
            // the always-emit-a-snippet policy documented in
            // `manticore.rs`; the wrapping style follows the default.
            let opts_plain = PlainOpts {
                limit: request.limit,
                offset: request.offset,
                fields: Vec::new(),
                highlight: true,
                lang: None,
                filter: None,
            };

            let body = build_request_body(&index, &request.query, &opts_plain)
                .map_err(SearchError::InvalidQuery)?;
            let url = manticore_url(&backend_url);

            let response_body = host::http_post_json(&url, &body)
                .map_err(|e| SearchError::BackendError(format!("POST {url}: {e}")))?;

            let plain_hits =
                parse_response(&response_body).map_err(SearchError::BackendError)?;

            // Legacy `hit.doc: String` (typically the subject IRI) →
            // `search-hit.subject: term` as a named-node. Legacy
            // `hit.lang` / `hit.fields` are not carried by the named
            // interface's return shape; consumers that need them run
            // through the crate's original wire shape.
            Ok(plain_hits
                .into_iter()
                .map(|h| SearchHit {
                    subject: WitTerm::NamedNode(h.doc),
                    score: h.score,
                    snippet: h.snippet,
                })
                .collect())
        }
    }

    /// Read an env var; fall back to `default` when the var is unset
    /// or empty. Wasi-preview2 hosts pass env vars into the guest by
    /// the runtime's normal configuration mechanism (wasmtime4j
    /// `WasiCtxBuilder::env`, wasmtime `Store` config).
    fn env_or(name: &str, default: &str) -> String {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    }

    // ---------------------------------------------------------------
    // sparql-extension stubs — no filter / aggregate / property
    // functions. wf_fulltext's dispatch happens through the named
    // `search` interface above; the sparql-extension include is here
    // for substrate consistency (see the world declaration in
    // wit/world.wit).
    // ---------------------------------------------------------------

    impl ExtensionGuest for Component {
        fn register() -> Vec<FunctionDescriptor> {
            Vec::new()
        }

        fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
            Err(format!(
                "wf_fulltext: no filter function '{name}' registered \
                 (search is dispatched via the named-extension interface)"
            ))
        }
    }

    impl AggregateGuest for Component {
        type AggregateState = UnreachableAggregateState;

        fn register_aggregates() -> Vec<AggregateDescriptor> {
            Vec::new()
        }

        fn new_aggregate(name: String) -> Result<AggregateState, String> {
            Err(format!(
                "wf_fulltext: unknown aggregate '{name}' (this component provides none)"
            ))
        }
    }

    pub struct UnreachableAggregateState;

    impl GuestAggregateState for UnreachableAggregateState {
        fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
            Err("wf_fulltext: aggregate state was never constructed".into())
        }

        fn finish(&self) -> Result<WitTerm, String> {
            Err("wf_fulltext: aggregate state was never constructed".into())
        }
    }

    impl PropertyFunctionGuest for Component {
        fn register_property_functions() -> Vec<PropertyDescriptor> {
            Vec::new()
        }

        fn evaluate(
            name: String,
            _subjects: Vec<WitTerm>,
            _objects: Vec<WitTerm>,
        ) -> Result<Vec<BindingRow>, String> {
            Err(format!(
                "wf_fulltext: unknown property function '{name}' (this component provides none)"
            ))
        }
    }

    // ---------------------------------------------------------------
    // World-level bare exports — retained admin surface.
    // ---------------------------------------------------------------

    impl bindings::Guest for Component {
        fn insert_batch(
            backend_url: String,
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

    // ---------------------------------------------------------------
    // URL helpers — idempotent path append.
    // ---------------------------------------------------------------

    /// `backend-url` is a bare host[:port] like
    /// `http://localhost:9308`. Append `/search` for the Manticore
    /// endpoint. Idempotent: if the caller already included
    /// `/search`, don't double up.
    fn manticore_url(backend_url: &str) -> String {
        let trimmed = backend_url.trim_end_matches('/');
        if trimmed.ends_with("/search") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/search")
        }
    }

    /// Same idempotent behaviour as [`manticore_url`], but for the
    /// `/bulk` admin endpoint that `insert-batch` / `delete-batch`
    /// POST to. If the caller already terminated their backend URL
    /// with `/bulk`, don't double up.
    fn bulk_url(backend_url: &str) -> String {
        let trimmed = backend_url.trim_end_matches('/');
        if trimmed.ends_with("/bulk") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/bulk")
        }
    }

    bindings::export!(Component with_types_in bindings);
}
