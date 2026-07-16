//! wf_sagegraph_nn — wasi:nn variant of the wf_sagegraph guest.
//!
//! # v0.4 status
//!
//! Same three exports (`embed` / `sweep` / `search`) as the sibling
//! `wf_sagegraph` crate under `../wf_sagegraph`. Same host callback
//! shape (`wf:sagegraph/host@0.2.0`, keyed by package name so the
//! substrate's linker binding serves both variants). Same bundled
//! demo ONNX model bytes (`models/graphsage_demo.onnx`, symlinked-
//! wise a byte-identical copy so `include_bytes!` produces the same
//! digest and cross-guest parity holds on the raw model).
//!
//! The one difference: the ONNX forward pass in `embed_kernel::embed`
//! calls out to `wasi:nn/{tensor,graph,inference,errors}
//! @0.2.0-rc-2024-10-28` instead of statically linking `tract-onnx`.
//! On engines with `wasmtime-wasi-nn = 46` bound to their linker
//! (oxigraph-wf, qlever-wf-runtime as of v0.4 substrate landing),
//! the host-side ONNX Runtime session executes the model against
//! the guest-supplied input tensor and hands the output back over
//! the ABI. On engines without wasi:nn (wasmtime4j-backed Jena and
//! RDF4J), the guest fails to instantiate — hence the two-wasm-
//! split rollout documented in `Cargo.toml`.

#[cfg(target_family = "wasm")]
wit_bindgen::generate!({
    world: "wf-sagegraph-nn",
    path: "wit",
    generate_all,
});

#[cfg(target_family = "wasm")]
mod wasm_glue {
    //! Wire the wit-bindgen-generated `Guest` trait to the kernel
    //! modules. Mirror of wf_sagegraph's `wasm_glue` — only
    //! difference is the `embed` implementation dispatches to the
    //! wasi:nn-backed `embed_kernel::embed` rather than the tract
    //! sibling.
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
// Plain-Rust structs mirroring the WIT records — see wf_sagegraph::lib
// for the reason these live under `#[cfg(not(target_family = "wasm"))]`.
// Kernels compile against the same field names on wasm and host builds.
// ---------------------------------------------------------------------------

pub mod embed_kernel;
pub mod search_kernel;
pub mod sweep_kernel;

#[cfg(not(target_family = "wasm"))]
#[derive(Debug, Clone)]
pub struct EmbedOpts {
    pub dimensions: u32,
    pub pool: String,
    pub runtime: Option<String>,
    pub fuel_limit: Option<u64>,
}

#[cfg(not(target_family = "wasm"))]
#[derive(Debug, Clone)]
pub struct SweepOpts {
    pub dimensions: u32,
    pub pool: String,
    pub batch_size: u32,
    pub concurrency: u32,
}

#[cfg(not(target_family = "wasm"))]
#[derive(Debug, Clone)]
pub struct SearchOpts {
    pub metric: Option<String>,
    pub oversample: Option<u32>,
}

#[cfg(not(target_family = "wasm"))]
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub node: String,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Host-callback trait — the pure-logic kernels are unit-testable with
// a mock. Mirror of wf_sagegraph::HostBridge, one-for-one; both crates
// share the two `wf:sagegraph/host@0.2.0` imports.
// ---------------------------------------------------------------------------

pub trait HostBridge {
    fn execute_query(&self, query: &str) -> Result<String, String>;
    fn http_post_json(&self, url: &str, body: &str) -> Result<String, String>;
}

/// Version marker for auditability — this is the wasi:nn landing.
pub const V0_4_STATUS: &str =
    "wf_sagegraph_nn v0.4: wasi:nn ONNX inference variant. \
     Same exports and host callbacks as wf_sagegraph; ONNX runtime \
     is host-provided via wasi:nn/{tensor,graph,inference,errors} \
     @0.2.0-rc-2024-10-28. Instantiates only on engines that expose \
     wasi:nn (oxigraph-wf + qlever-wf-runtime as of v0.4 substrate).";
