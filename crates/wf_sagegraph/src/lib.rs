//! wf_sagegraph — GraphSAGE-family node embeddings as a substrate
//! function.
//!
//! # v0.2 status
//!
//! Three exports (`embed` / `sweep` / `search`) implemented end-to-end
//! against the substrate ABI (memo §04). Real wit-bindgen bindings;
//! two host imports (`execute-query`, `http-post-json`) wired.
//!
//! # ONNX honesty — path stubbed
//!
//! The memo §04 stack (`graphblas-wasm` + `lapack-wasm` +
//! `onnxruntime-wasm` + `fastembed-wasm`) is not composable inside a
//! cargo-component `cdylib` in a single session — toolchain walls
//! around component-model, WebGPU, wasi-nn. The v0.2 landing ships a
//! **stubbed ONNX path**: mean-pool over structural neighbor
//! features, then a deterministic linear projection whose weights are
//! derived from a stable hash of `model-url`, then tanh. Vectors are
//! byte-stable per `model-url`; the ABI is honest end-to-end. v0.3
//! swaps the projection for real ONNX behind the same `embed`
//! signature.
//!
//! # k-hop extraction template
//!
//! The guest issues, via `host::execute-query`, one SPARQL query per
//! hop to enumerate the neighborhood. v0.2 uses the simplest
//! honest shape:
//!
//! ```sparql
//! SELECT ?n WHERE {
//!   { <node> ?p ?n }
//!   UNION
//!   { ?n ?p <node> }
//! }
//! ```
//!
//! For k > 1 the guest iterates: each hop expands the frontier by
//! issuing the same query with the previous hop's nodes plugged in as
//! `?n_prev`. This is deliberately naïve — a real k-hop template
//! would push through `PropertyPath` on the substrate side. v0.3
//! concern.

// The `#[cfg(target_family = "wasm")]` gate lets us run the pure-Rust
// kernel unit tests on the host (via `cargo test --lib`) without
// pulling in wit-bindgen's wasm-specific machinery. The types the
// kernels consume are declared as plain Rust structs below, mirroring
// the WIT record shape byte-for-byte, so the wasm and non-wasm builds
// share one kernel implementation.

// wit-bindgen bindings live in a cargo-component-generated
// `bindings.rs`; both are gated on the wasm target so the pure
// kernels remain testable on the native host.
#[cfg(target_family = "wasm")]
#[allow(warnings)]
mod bindings;

#[cfg(target_family = "wasm")]
mod wasm_glue {
    //! Wire the wit-bindgen-generated Guest traits to the kernel
    //! modules. Only compiled for the wasm build; the host build uses
    //! the plain-Rust types below.
    //!
    //! # Well-known-exports migration
    //!
    //! The named `tegmentum:webfunction/embed@0.1.0` interface is
    //! wired to `embed_kernel::embed`. The `embed-request` shape
    //! only carries `text` / `model`; the legacy `node-iri`,
    //! `model-url`, `k-hops`, and `opts` arguments map as:
    //!
    //!   * `request.text` → `node_iri` (semantic shift: the
    //!     well-known interface names the input `text` because
    //!     typical guests run text through an embedder; wf_sagegraph
    //!     runs a graph node through GraphSAGE).
    //!   * `request.model` → `model_url` (defaults from env var
    //!     `WF_SAGEGRAPH_MODEL_URL` when absent, else empty string).
    //!   * `k_hops` from env `WF_SAGEGRAPH_K_HOPS` (default 1).
    //!   * `EmbedOpts` fields from env vars with defaults matching
    //!     the crate's demo model (`dimensions=8`, `pool="mean"`).
    //!
    //! Errors map into `embed-error`:
    //!   * `no-such-model` — kernel returned an error whose message
    //!     mentions "model" (unknown model URL / model load failure).
    //!   * `backend-error` — everything else (SPARQL failure, graph
    //!     shape errors, ONNX runtime errors).
    //!   * `too-large(u32)` — not fired by GraphSAGE; the neighborhood
    //!     bound is `k_hops`, not an input token count.
    //!
    //! Retained bare exports (`sweep`, `search`) stay on the
    //! world-level Guest trait unchanged.

    use super::bindings;
    // Top-level bindings aliases for the world's `use local-types.{...}`
    // — these are the record shapes the world-level Guest trait's
    // sweep / search methods take as parameters and return.
    use bindings::{Hit as WitHit, SearchOpts as WitSearchOpts, SweepOpts as WitSweepOpts};
    use bindings::exports::tegmentum::webfunction::aggregate::{
        AggregateDescriptor, AggregateState, Guest as AggregateGuest, GuestAggregateState,
    };
    use bindings::exports::tegmentum::webfunction::embed::{
        EmbedError, EmbedRequest, Guest as EmbedGuest,
    };
    use bindings::exports::tegmentum::webfunction::extension::{
        FunctionDescriptor, Guest as ExtensionGuest,
    };
    use bindings::exports::tegmentum::webfunction::property_function::{
        BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
    };
    use bindings::stardog::wf_sagegraph::host;
    use bindings::tegmentum::webfunction::types::Term as WitTerm;

    pub(crate) struct Component;

    // ---------------------------------------------------------------
    // Named-extension `embed` — well-known-exports memo §4.
    // ---------------------------------------------------------------

    impl EmbedGuest for Component {
        fn embed(request: EmbedRequest) -> Result<Vec<f32>, EmbedError> {
            let node_iri = request.text;
            let model_url = request
                .model
                .unwrap_or_else(|| env_or("WF_SAGEGRAPH_MODEL_URL", ""));
            let k_hops = env_u32("WF_SAGEGRAPH_K_HOPS", 1);
            let opts = crate::EmbedOpts {
                dimensions: env_u32("WF_SAGEGRAPH_DIMENSIONS", 8),
                pool: env_or("WF_SAGEGRAPH_POOL", "mean"),
                runtime: std::env::var("WF_SAGEGRAPH_RUNTIME").ok(),
                fuel_limit: std::env::var("WF_SAGEGRAPH_FUEL_LIMIT")
                    .ok()
                    .and_then(|v| v.parse().ok()),
            };
            crate::embed_kernel::embed(&node_iri, &model_url, k_hops, &opts, &HostCallbacks)
                .map_err(classify_embed_error)
        }
    }

    /// Route kernel error strings into the typed `embed-error`
    /// variant. GraphSAGE has no natural `too-large(u32)` arm — the
    /// neighborhood bound is `k_hops`, not a token count — so
    /// everything unclassified falls to `backend-error`.
    fn classify_embed_error(msg: String) -> EmbedError {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("model") && (lower.contains("unknown") || lower.contains("not found") ||
            lower.contains("load") || lower.contains("no such"))
        {
            EmbedError::NoSuchModel(msg)
        } else {
            EmbedError::BackendError(msg)
        }
    }

    fn env_or(name: &str, default: &str) -> String {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    }

    fn env_u32(name: &str, default: u32) -> u32 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    // ---------------------------------------------------------------
    // sparql-extension stubs — no filter / aggregate / property
    // functions.
    // ---------------------------------------------------------------

    impl ExtensionGuest for Component {
        fn register() -> Vec<FunctionDescriptor> {
            Vec::new()
        }

        fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
            Err(format!(
                "wf_sagegraph: no filter function '{name}' registered \
                 (embed is dispatched via the named-extension interface; \
                 sweep / search remain as bare world-level exports)"
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
                "wf_sagegraph: unknown aggregate '{name}' (this component provides none)"
            ))
        }
    }

    pub struct UnreachableAggregateState;

    impl GuestAggregateState for UnreachableAggregateState {
        fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
            Err("wf_sagegraph: aggregate state was never constructed".into())
        }

        fn finish(&self) -> Result<WitTerm, String> {
            Err("wf_sagegraph: aggregate state was never constructed".into())
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
                "wf_sagegraph: unknown property function '{name}' (this component provides none)"
            ))
        }
    }

    // ---------------------------------------------------------------
    // World-level bare exports — retained `sweep` and `search`.
    // ---------------------------------------------------------------

    impl bindings::Guest for Component {
        fn sweep(
            model_url: String,
            subject_pattern: String,
            target_sink_url: String,
            k_hops: u32,
            opts: WitSweepOpts,
        ) -> Result<u32, String> {
            let opts = crate::SweepOpts {
                dimensions: opts.dimensions,
                pool: opts.pool,
                batch_size: opts.batch_size,
                concurrency: opts.concurrency,
            };
            crate::sweep_kernel::sweep(
                &model_url,
                &subject_pattern,
                &target_sink_url,
                k_hops,
                &opts,
                &HostCallbacks,
            )
        }

        fn search(
            index_url: String,
            query_vec: Vec<f32>,
            k: u32,
            opts: WitSearchOpts,
        ) -> Result<Vec<WitHit>, String> {
            let opts = crate::SearchOpts {
                metric: opts.metric,
                oversample: opts.oversample,
            };
            let hits =
                crate::search_kernel::search(&index_url, &query_vec, k, &opts, &HostCallbacks)?;
            Ok(hits
                .into_iter()
                .map(|h| WitHit {
                    node: h.node,
                    score: h.score,
                })
                .collect())
        }
    }

    pub(crate) struct HostCallbacks;

    impl crate::HostBridge for HostCallbacks {
        fn execute_query(&self, query: &str) -> Result<String, String> {
            host::execute_query(query)
        }
        fn http_post_json(&self, url: &str, body: &str) -> Result<String, String> {
            host::http_post_json(url, body)
        }
    }

    bindings::export!(Component with_types_in bindings);
}

// ---------------------------------------------------------------------------
// Plain-Rust structs — the shapes the pure kernels consume in BOTH
// the native (unit-test) build and the wasm build. Kept
// fieldwise-identical to the WIT record layouts so the wasm_glue
// module can translate WIT records to these with a per-field
// assignment. No cfg gate: the wit-bindgen-generated aliases live
// inside `mod bindings`, so declaring the same identifiers at crate
// root does not collide.
// ---------------------------------------------------------------------------

pub mod embed_kernel;
pub mod sweep_kernel;
pub mod search_kernel;

/// Kernel-side mirror of the historical WIT `embed-opts` record.
/// The well-known-exports migration dropped `embed-opts` from the
/// WIT (the named `tegmentum:webfunction/embed@0.1.0` interface's
/// `embed-request` only carries `text` / `model`). The struct
/// remains here because the kernel still needs the tuning knobs;
/// wasm_glue populates it from env vars.
#[derive(Debug, Clone)]
pub struct EmbedOpts {
    pub dimensions: u32,
    pub pool: String,
    pub runtime: Option<String>,
    pub fuel_limit: Option<u64>,
}

/// Kernel-side mirror of the WIT `sweep-opts` record.
#[derive(Debug, Clone)]
pub struct SweepOpts {
    pub dimensions: u32,
    pub pool: String,
    pub batch_size: u32,
    pub concurrency: u32,
}

/// Kernel-side mirror of the WIT `search-opts` record.
#[derive(Debug, Clone)]
pub struct SearchOpts {
    pub metric: Option<String>,
    pub oversample: Option<u32>,
}

/// Kernel-side mirror of the WIT `hit` record. Kernels return this;
/// wasm_glue re-projects it into `bindings::Hit` when crossing back.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub node: String,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Host-callback trait — so kernels are testable without a wasm runtime.
// ---------------------------------------------------------------------------

/// Thin trait over the two host imports so the pure-logic kernels can
/// be unit-tested with a mock. The wasm build wires `HostCallbacks`
/// (in `wasm_glue`) which forwards to `wf::sagegraph::host::*`.
pub trait HostBridge {
    fn execute_query(&self, query: &str) -> Result<String, String>;
    fn http_post_json(&self, url: &str, body: &str) -> Result<String, String>;
}

// ---------------------------------------------------------------------------
// Version marker — kept so an accidental workspace-wide rebuild
// notices this crate changed shape from v0.1's stub-only lib.
// ---------------------------------------------------------------------------

/// Version-note constant, referenced from tests only. v0.1 shipped a
/// substrate-side rewrite in place of the guest; v0.2 lands the guest
/// with the ONNX path stubbed (see crate-level doc for the honesty
/// note).
pub const V0_2_STATUS: &str =
    "wf_sagegraph v0.2: guest body landed with three exports; \
     ONNX forward pass stubbed with a deterministic model-url-hashed \
     linear projection (see crate-level docs).";

/// v0.3.1 delta note. Kept next to `V0_2_STATUS` so a reader tracing
/// the version story sees every landing in one place.
///
///   * **graphblas-sparse aggregation** — the pure-Rust core of
///     ~/git/graphblas-wasm is now a normal cargo dep. Neighbor
///     aggregation lands through `reduce_to_vector` (PlusZero for
///     mean/sum, MaxNegInf for max). Byte-identical to the prior
///     sequential accumulator for mean pool because the accumulation
///     order matches. `max` upgrades from a sum-stub to a real
///     per-feature maximum.
///
///   * **literal-filter fix (memo §07)** — `fetch_one_hop` now drops
///     literal-shaped ?n bindings so degree probes / BFS expansion /
///     closure sizing all see the same resource-only view. Prior
///     behavior silently folded a zero into the mean for every
///     literal neighbor via `unwrap_or(0.0)` on the parse-failing
///     degree query. Byte parity across engines held before and
///     holds after; the numbers moved because the semantics moved.
///
///   * **fastembed-wasm text-attributed features — v0.4 blocked**.
///     `~/git/fastembed-wasm` is a wit-bindgen `cdylib` component
///     that imports `wasi::nn` (wasi-nn). There is no pure-Rust
///     `fastembed-sparse`-style sibling to consume as a normal
///     cargo dep. Integrating it into `wf_sagegraph` needs either
///     `wasm-tools compose` in the build graph (same toolchain wall
///     the v0.2 ONNX composition attempt hit — see the crate-level
///     doc above) OR a substrate-wide `wasmtime-wasi-nn` linker
///     registration on every engine's Store (out of scope for this
///     crate's fence). Text-attributed features stay unimplemented
///     for v0.3.1; structural is still the honest default per memo
///     §06 last paragraph. Revisit in v0.4 once either wall is
///     scaled.
pub const V0_3_1_STATUS: &str =
    "wf_sagegraph v0.3.1: graphblas-sparse aggregation backend + \
     literal-shaped one-hop filter. fastembed-wasm text-attributed \
     features deferred to v0.4 (composition wall).";
