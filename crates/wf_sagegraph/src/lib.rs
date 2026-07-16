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

#[cfg(target_family = "wasm")]
wit_bindgen::generate!({
    world: "wf-sagegraph",
    path: "wit",
});

#[cfg(target_family = "wasm")]
mod wasm_glue {
    //! Wire the wit-bindgen-generated `Guest` trait to the kernel
    //! modules. Only compiled for the wasm build; the host build uses
    //! the plain-Rust types below.
    use super::*;

    struct Component;

    impl Guest for Component {
        fn embed(
            node_iri: String,
            model_url: String,
            k_hops: u32,
            opts: EmbedOpts,
        ) -> Result<Vec<f32>, String> {
            let opts = crate::EmbedOpts {
                dimensions: opts.dimensions,
                pool: opts.pool,
                runtime: opts.runtime,
                fuel_limit: opts.fuel_limit,
            };
            crate::embed_kernel::embed(&node_iri, &model_url, k_hops, &opts, &HostCallbacks)
        }

        fn sweep(
            model_url: String,
            subject_pattern: String,
            target_sink_url: String,
            k_hops: u32,
            opts: SweepOpts,
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
            opts: SearchOpts,
        ) -> Result<Vec<Hit>, String> {
            let opts = crate::SearchOpts {
                metric: opts.metric,
                oversample: opts.oversample,
            };
            let hits =
                crate::search_kernel::search(&index_url, &query_vec, k, &opts, &HostCallbacks)?;
            Ok(hits
                .into_iter()
                .map(|h| Hit {
                    node: h.node,
                    score: h.score,
                })
                .collect())
        }
    }

    pub(crate) struct HostCallbacks;

    impl crate::HostBridge for HostCallbacks {
        fn execute_query(&self, query: &str) -> Result<String, String> {
            wf::sagegraph::host::execute_query(query)
        }
        fn http_post_json(&self, url: &str, body: &str) -> Result<String, String> {
            wf::sagegraph::host::http_post_json(url, body)
        }
    }

    export!(Component);
}

// ---------------------------------------------------------------------------
// Plain-Rust structs mirroring the WIT records.
//
// These are the shapes the pure kernels consume. On wasm, the
// wit-bindgen-generated records are marshalled into these before
// crossing into the kernel; on the host (unit-test) build, tests
// construct these directly. Kept fieldwise-identical to the WIT so a
// mistake in either place shows up as a type error.
// ---------------------------------------------------------------------------

pub mod embed_kernel;
pub mod sweep_kernel;
pub mod search_kernel;

/// Mirror of the WIT `embed-opts` record.
#[derive(Debug, Clone)]
pub struct EmbedOpts {
    pub dimensions: u32,
    pub pool: String,
    pub runtime: Option<String>,
    pub fuel_limit: Option<u64>,
}

/// Mirror of the WIT `sweep-opts` record.
#[derive(Debug, Clone)]
pub struct SweepOpts {
    pub dimensions: u32,
    pub pool: String,
    pub batch_size: u32,
    pub concurrency: u32,
}

/// Mirror of the WIT `search-opts` record.
#[derive(Debug, Clone)]
pub struct SearchOpts {
    pub metric: Option<String>,
    pub oversample: Option<u32>,
}

/// Mirror of the WIT `hit` record. Kernels return this; the wasm
/// glue re-projects it into the generated `Hit` when crossing back.
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
