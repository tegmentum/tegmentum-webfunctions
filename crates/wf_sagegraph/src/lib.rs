//! `wf_sagegraph` — GraphSAGE-family node embeddings as a substrate
//! function. Stub crate landing the WIT world declared in
//! `wit/wf-sagegraph.wit` (memo §04 shape byte-for-byte).
//!
//! # v0.1 status — SUBSTRATE PATH SHIPS, GUEST BODY DEFERRED
//!
//! Per the wf-sagegraph task brief's "honest-failure escape":
//! composing `graphblas-wasm` + `lapack-wasm` + `onnxruntime-wasm` +
//! `fastembed-wasm` into one wasm component with `wasi-http` for the
//! host `execute-query` callback is not a one-session job. The v0.1
//! minimal path substrate ships instead — Oxigraph's
//! `wf_sagegraph_rewrite` folds `SERVICE <wf-sagegraph:<name>?…>` into
//! a VALUES block by computing a degree-based structural embedding at
//! plan time (reads-direct pattern; see `wf-sagegraph.md` §11).
//!
//! This crate keeps the WIT world in the repo so v0.2's guest landing
//! can pick up where v0.1 stopped: composition, not new science
//! (memo §03). The Rust body of the exports intentionally returns
//! unimplemented — a v0.1 dispatch would never reach the guest,
//! because the substrate's rewrite short-circuits `wf-sagegraph:`
//! SERVICE URLs before wasm is invoked.
//!
//! # v0.2 punch list — see `wf-sagegraph.md` §14
//!
//! 1. Compose the ML stack into one component:
//!    - `graphblas-wasm` (neighbor-aggregation semirings, memo §07)
//!    - `lapack-wasm::dgemv` (learned linear transforms per SAGE layer)
//!    - `onnxruntime-wasm` (or `wonnx-wasm`) — ONNX inference
//!    - `fastembed-wasm` — text-attributed feature extraction
//! 2. Wire the `wf:sagegraph/host@0.1.0` execute-query callback
//!    following the wave-8 http-post-json pattern (host-side
//!    registration mirrors the `WfSageGraphHostImports` shape used
//!    by `wf_fulltext`'s host imports).
//! 3. Implement k-hop extraction template — parameterized SPARQL the
//!    guest issues via that callback, bounded by `k-hops` +
//!    `fuel-limit`.
//! 4. Implement `embed` / `sweep` / `search` bodies per memo §14
//!    steps 4, 7, 8.
//! 5. Ship a small pretrained SAGE model (Cora or similar, ~2 MB
//!    ONNX) so the conformance case can exercise the whole pipeline.

// Bindings from wit-bindgen. Once cargo-component builds this crate
// into a wasm component the generated bindings will land here.
// v0.1's honest failure: the tegmentum-webfunctions workspace
// currently has no `cargo-component` builder wired for this crate;
// the WIT world is present as forward-compat scaffolding. See the
// crate-level doc.
#![allow(dead_code, unused_variables)]

// wit-bindgen generation is deferred — v0.1 doesn't invoke the guest.
// The commented block below is the shape v0.2 will land.
//
// wit_bindgen::generate!({
//     path: "wit",
//     world: "wf-sagegraph",
//     exports: {
//         world: SageGraph,
//     },
// });
//
// struct SageGraph;
// impl exports::Guest for SageGraph {
//     fn embed(...) -> Result<Vec<f32>, String> { unimplemented!("v0.2") }
//     fn sweep(...) -> Result<u32, String>     { unimplemented!("v0.2") }
//     fn search(...) -> Result<Vec<Hit>, String> { unimplemented!("v0.2") }
// }
// export!(SageGraph);

/// v0.1 placeholder — the substrate never reaches this crate. Present
/// so `cargo build` on the workspace doesn't reject the `rlib`
/// crate-type as empty.
pub const V0_1_STUB_NOTE: &str =
    "wf_sagegraph v0.1: substrate-side rewrite folds \
     `wf-sagegraph:` SERVICE URLs at plan time; the guest body \
     lands in v0.2. See src/lib.rs module doc.";
