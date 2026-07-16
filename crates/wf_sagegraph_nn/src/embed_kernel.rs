//! `embed` export body — wasi:nn variant of the wf_sagegraph
//! `embed_kernel::embed`. Identical pipeline shape (memo §04 tier 1)
//! from step 1 (`fetch_one_hop` + BFS closure) through step 4
//! (`input8` = self_feats ++ neighbor_pool via graphblas-sparse
//! aggregation). Only step 5 — the ONNX forward pass — diverges.
//!
//! # Forward-pass path (differs from wf_sagegraph)
//!
//!   1. `wasi:nn/graph::load(&[DEMO_MODEL_ONNX], Onnx, Cpu)` — hand
//!      the bundled model bytes to the host-side ONNX Runtime session.
//!      The host builds the graph once per call. `graph::load-by-
//!      name` would let us amortize across calls but that requires a
//!      host-side registry the substrate hasn't wired up yet.
//!   2. `graph.init_execution_context()` — spins a per-call session.
//!   3. `context.compute([("input", tensor)])` — send the 1×8 f32
//!      input tensor and read back the 1×8 f32 output tensor.
//!   4. Decode the returned bytes as little-endian f32 and yield
//!      `Vec<f32>`.
//!
//! # Determinism
//!
//! ONNX Runtime executes the same graph bytes deterministically on
//! the same host — same IEEE-754 f32 arithmetic, same operator
//! implementations. `tract-onnx` and `onnxruntime` differ in
//! implementation but converge on the same numerical result for
//! matmul/relu/tanh at f32 precision; if a small deviation surfaces
//! on the golden `sagegraph_degree_features` case, it is documented
//! in the case's `expected_bindings` note and pinned as the
//! wasi:nn-side authoritative value.
//!
//! # Empty model_url
//!
//! Same short-circuit as the tract sibling: when `model_url` is
//! empty the guest emits the deterministic hash-seeded projection
//! from v0.2 instead of touching the wasi:nn ABI. Cases without a
//! real model URL still see byte-stable output (and the wasi:nn
//! codepath is not exercised, which is helpful for xfail triage).

use serde::Deserialize;

use crate::{EmbedOpts, HostBridge};

/// Structural feature vector per node. Kept at length 4 so the
/// concat `[self, neighbor_pool]` lands at length 8, matching the
/// bundled demo model's input shape.
const STRUCT_FEATURES: usize = 4;

/// Bundled demo ONNX model, byte-identical copy of the sibling
/// crate's file. `include_bytes!` embeds the same digest so the
/// wasi:nn host loads the same graph bytes ORT-side that
/// `tract-onnx` loads guest-side in the sibling — both variants
/// see the same operators + weights.
const DEMO_MODEL_ONNX: &[u8] = include_bytes!("../models/graphsage_demo.onnx");

/// Output dimension of `DEMO_MODEL_ONNX`. When `opts.dimensions`
/// doesn't match, the guest falls back to the hash-seeded stub.
const DEMO_MODEL_OUTPUT_DIM: usize = 8;

pub fn embed(
    node_iri: &str,
    model_url: &str,
    k_hops: u32,
    opts: &EmbedOpts,
    host: &dyn HostBridge,
) -> Result<Vec<f32>, String> {
    let dimensions = opts.dimensions as usize;
    if dimensions == 0 {
        return Err("wf_sagegraph_nn: embed-opts.dimensions must be >= 1".to_string());
    }

    // Step 1: fetch 1-hop neighbors via host callback.
    let one_hop = fetch_one_hop(node_iri, host)?;
    let degree = one_hop.len() as f32;

    // Step 2: k-hop BFS closure (bounded by k_hops).
    let mut closure: Vec<String> = one_hop.clone();
    let mut frontier = one_hop.clone();
    for _hop in 2..=k_hops.max(1) {
        let mut next: Vec<String> = Vec::new();
        for f in &frontier {
            let more = fetch_one_hop(f, host).unwrap_or_default();
            for m in more {
                if !closure.iter().any(|c| c == &m) {
                    closure.push(m.clone());
                    next.push(m);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    // Step 3: per-neighbor structural features + pooled aggregation.
    // graphblas-sparse-backed, same summation tree as the sibling
    // guest's mean pool — byte-identical output on shared inputs.
    let mut sum_neighbor_deg = 0.0f32;
    let mut counted = 0.0f32;
    let mut per_neighbor_feats: Vec<[f32; STRUCT_FEATURES]> = Vec::with_capacity(one_hop.len());
    for n in &one_hop {
        let n_deg = fetch_degree(n, host).unwrap_or(0.0);
        sum_neighbor_deg += n_deg;
        counted += 1.0;
        per_neighbor_feats.push(struct_features_for(n_deg, closure.len() as f32));
    }
    let _mean_neighbor_degree = if counted > 0.0 {
        sum_neighbor_deg / counted
    } else {
        0.0
    };
    let neighbor_pool =
        aggregate_neighbor_features_via_graphblas(&per_neighbor_feats, opts.pool.as_str());

    // Step 4: self features + input concatenation.
    let self_feats = struct_features_for(degree, closure.len() as f32);
    let input8: [f32; 8] = [
        self_feats[0],
        self_feats[1],
        self_feats[2],
        self_feats[3],
        neighbor_pool[0],
        neighbor_pool[1],
        neighbor_pool[2],
        neighbor_pool[3],
    ];

    // Step 5: forward pass. wasi:nn on the wasm build; fall back to
    // the hash-seeded stub when we're outside the wasm build or the
    // caller's dimensions mismatch the demo model's output shape.
    if !model_url.is_empty() && dimensions == DEMO_MODEL_OUTPUT_DIM {
        match run_demo_onnx_via_wasi_nn(&input8) {
            Ok(v) => {
                eprintln!(
                    "wf_sagegraph_nn: embed via wasi:nn ORT (model_url={model_url}, dim={dimensions})"
                );
                return Ok(v);
            }
            Err(e) => {
                eprintln!(
                    "wf_sagegraph_nn: wasi:nn inference failed ({e}); falling back to hash-seeded stub"
                );
            }
        }
    } else {
        eprintln!(
            "wf_sagegraph_nn: embed via hash-seeded stub (model_url={:?}, dim={dimensions})",
            model_url
        );
    }

    let count = closure.len() as f32;
    let raw_summary: [f32; STRUCT_FEATURES] = [
        degree,
        _mean_neighbor_degree,
        count,
        self_feats[3],
    ];
    Ok(stubbed_onnx_project(
        &raw_summary,
        &neighbor_pool,
        model_url,
        dimensions,
    ))
}

// ---------------------------------------------------------------------------
// wasi:nn forward pass (wasm build).
// ---------------------------------------------------------------------------

/// wasm-build: shovel the demo model bytes + input tensor across
/// the wasi:nn ABI. The host (`wasmtime-wasi-nn` on oxigraph-wf and
/// qlever-wf-runtime) owns the ONNX Runtime session; the guest just
/// packs LE f32 bytes on the way in and unpacks them on the way out.
#[cfg(target_family = "wasm")]
fn run_demo_onnx_via_wasi_nn(input: &[f32; 8]) -> Result<Vec<f32>, String> {
    use crate::wasi::nn::errors::Error as NnError;
    use crate::wasi::nn::graph::{load, ExecutionTarget, GraphEncoding};
    use crate::wasi::nn::tensor::{Tensor, TensorType};

    fn map_err(context: &str, err: NnError) -> String {
        // NnError is a resource — code() + data() give us both the
        // enum discriminant and a backend-specific string. Stringify
        // both so xfail triage can see which side of the ABI failed.
        format!("{context}: code={:?} data={}", err.code(), err.data())
    }

    // Step 5.1: load graph.
    let graph = load(
        &[DEMO_MODEL_ONNX.to_vec()],
        GraphEncoding::Onnx,
        ExecutionTarget::Cpu,
    )
    .map_err(|e| map_err("wasi:nn graph::load", e))?;

    // Step 5.2: init execution context.
    let ctx = graph
        .init_execution_context()
        .map_err(|e| map_err("wasi:nn init_execution_context", e))?;

    // Step 5.3: build the 1x8 f32 input tensor. Little-endian bytes
    // are the wasi:nn convention (mirrors the fastembed guest's
    // packing for its BERT input_ids tensor).
    let dims = vec![1u32, 8u32];
    let mut bytes = Vec::with_capacity(8 * 4);
    for f in input {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    let tensor = Tensor::new(&dims, TensorType::Fp32, &bytes);

    // Step 5.4: run inference. The demo model's input node name in
    // `graphsage_demo.onnx` is "input" (the Python script that built
    // it in the sibling PR named it explicitly). Onnxruntime binds
    // by name, so this MUST match the graph's declared input name.
    let outputs = ctx
        .compute(vec![("input".to_string(), tensor)])
        .map_err(|e| map_err("wasi:nn compute", e))?;

    // Step 5.5: decode the first output tensor. The demo model
    // exposes a single named output ("output" per the build script)
    // of shape [1,8]. We take the first tensor unconditionally to
    // tolerate a name mismatch on hosts that don't preserve it.
    let (_name, out_tensor) = outputs
        .into_iter()
        .next()
        .ok_or_else(|| "wasi:nn compute returned no outputs".to_string())?;
    let out_bytes = out_tensor.data();
    if out_bytes.len() != DEMO_MODEL_OUTPUT_DIM * 4 {
        return Err(format!(
            "wasi:nn output tensor size mismatch: got {} bytes, expected {}",
            out_bytes.len(),
            DEMO_MODEL_OUTPUT_DIM * 4
        ));
    }
    let mut out = Vec::with_capacity(DEMO_MODEL_OUTPUT_DIM);
    for chunk in out_bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

/// Host-build (unit tests): wasi:nn isn't available off-wasm, so
/// fall back to the deterministic hash-seeded stub. Every unit test
/// against this kernel exercises the fallback path — for real
/// end-to-end validation of wasi:nn, run the two sagegraph cases
/// against Oxigraph / QLever via the wf-conformance harness (which
/// selects `wf_sagegraph_nn.wasm` on those engines through
/// `locate_wf_sagegraph_nn_wasm_url`).
#[cfg(not(target_family = "wasm"))]
fn run_demo_onnx_via_wasi_nn(_input: &[f32; 8]) -> Result<Vec<f32>, String> {
    Err("wasi:nn unavailable on host build; fall through to stub".to_string())
}

// ---------------------------------------------------------------------------
// Host-callback wrappers — verbatim from wf_sagegraph::embed_kernel.
// The literal-filter fix (v0.3.1, memo §07) is preserved so both
// variants drop the same one-hop bindings and neighbor aggregation
// stays byte-identical across the wf_sagegraph / wf_sagegraph_nn
// pair on the same fixture data.
// ---------------------------------------------------------------------------

pub(crate) fn fetch_one_hop(node: &str, host: &dyn HostBridge) -> Result<Vec<String>, String> {
    let query = format!(
        "SELECT DISTINCT ?n WHERE {{ \
            {{ <{node}> ?p ?n }} UNION {{ ?n ?p <{node}> }} \
        }}"
    );
    let json = host.execute_query(&query)?;
    let parsed: SparqlResults = serde_json::from_str(&json)
        .map_err(|e| format!("wf_sagegraph_nn: parse SPARQL Results: {e}"))?;
    let mut out = Vec::new();
    for binding in parsed.results.bindings {
        if let Some(b) = binding.get("n") {
            if b.is_resource() {
                out.push(b.value.clone());
            }
        }
    }
    Ok(out)
}

pub(crate) fn fetch_degree(node: &str, host: &dyn HostBridge) -> Result<f32, String> {
    let query = format!(
        "SELECT (COUNT(*) AS ?c) WHERE {{ \
            {{ <{node}> ?p ?o }} UNION {{ ?s ?p <{node}> }} \
        }}"
    );
    let json = host.execute_query(&query)?;
    let parsed: SparqlResults = serde_json::from_str(&json)
        .map_err(|e| format!("wf_sagegraph_nn: parse SPARQL Results: {e}"))?;
    let row = parsed
        .results
        .bindings
        .first()
        .ok_or_else(|| "wf_sagegraph_nn: COUNT query returned no rows".to_string())?;
    let c = row
        .get("c")
        .ok_or_else(|| "wf_sagegraph_nn: COUNT query missing ?c".to_string())?;
    c.value
        .parse::<f32>()
        .map_err(|e| format!("wf_sagegraph_nn: parse COUNT: {e}"))
}

// ---------------------------------------------------------------------------
// Feature extraction — same formulas as the sibling; the input to
// the ONNX runtime must be bit-for-bit identical across the two
// guests so any deviation in the wasi:nn output surfaces as a
// runtime-side issue, not a data-side one.
// ---------------------------------------------------------------------------

fn struct_features_for(degree: f32, closure_size: f32) -> [f32; STRUCT_FEATURES] {
    let normalized_degree = if closure_size > 0.0 {
        degree / (1.0 + closure_size)
    } else {
        0.0
    };
    [degree, degree.ln_1p(), normalized_degree, 1.0]
}

fn aggregate_neighbor_features_via_graphblas(
    per_neighbor: &[[f32; STRUCT_FEATURES]],
    pool: &str,
) -> [f32; STRUCT_FEATURES] {
    use graphblas_sparse::dtype::DynScalar;
    use graphblas_sparse::ops::reduce::reduce_to_vector;
    use graphblas_sparse::semiring::MonoidId;
    use graphblas_sparse::storage::CsrStorage;

    let n = per_neighbor.len();
    if n == 0 {
        return [0.0; STRUCT_FEATURES];
    }

    let mut row_ptrs = vec![0usize; STRUCT_FEATURES + 1];
    let mut col_indices = Vec::with_capacity(STRUCT_FEATURES * n);
    let mut values: Vec<DynScalar> = Vec::with_capacity(STRUCT_FEATURES * n);
    for i in 0..STRUCT_FEATURES {
        for j in 0..n {
            col_indices.push(j);
            values.push(DynScalar::Float32(per_neighbor[j][i]));
        }
        row_ptrs[i + 1] = col_indices.len();
    }
    let csr = CsrStorage::<DynScalar> {
        rows: STRUCT_FEATURES,
        cols: n,
        row_ptrs,
        col_indices,
        values,
    };

    let monoid = match pool {
        "max" => MonoidId::MaxNegInf,
        _ => MonoidId::PlusZero,
    };

    let (indices, reduced) = match reduce_to_vector(monoid, &csr) {
        Ok(t) => t,
        Err(_) => return [0.0; STRUCT_FEATURES],
    };

    let mut out = [0.0f32; STRUCT_FEATURES];
    for (idx, v) in indices.iter().zip(reduced.iter()) {
        if let DynScalar::Float32(f) = v {
            if *idx < STRUCT_FEATURES {
                out[*idx] = *f;
            }
        }
    }

    if pool != "sum" && pool != "max" {
        let count_f = n as f32;
        for x in out.iter_mut() {
            *x /= count_f;
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Deterministic hash-seeded projection — same body as the sibling.
// Kept identical so an operator swapping between the two guests
// sees the same output when the ONNX path is short-circuited.
// ---------------------------------------------------------------------------

fn stubbed_onnx_project(
    self_feats: &[f32; STRUCT_FEATURES],
    neighbor_pool: &[f32; STRUCT_FEATURES],
    model_url: &str,
    dimensions: usize,
) -> Vec<f32> {
    let input: [f32; 8] = [
        self_feats[0],
        self_feats[1],
        self_feats[2],
        self_feats[3],
        neighbor_pool[0],
        neighbor_pool[1],
        neighbor_pool[2],
        neighbor_pool[3],
    ];
    let base_seed = fnv1a(model_url.as_bytes());
    let mut out = Vec::with_capacity(dimensions);
    for d in 0..dimensions {
        let row_seed = base_seed.wrapping_add((d as u64).wrapping_mul(0x9E3779B97F4A7C15));
        let mut acc: f32 = 0.0;
        for (i, x) in input.iter().enumerate() {
            let w_seed = row_seed.wrapping_add((i as u64).wrapping_mul(0xBF58476D1CE4E5B9));
            let w = seed_to_weight(w_seed);
            acc += w * x;
        }
        let b_seed = row_seed.wrapping_mul(0x94D049BB133111EB);
        let b = seed_to_weight(b_seed);
        out.push((acc + b).tanh());
    }
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn seed_to_weight(seed: u64) -> f32 {
    let n = (seed >> 11) as u32;
    let frac = (n as f32) / (u32::MAX as f32);
    frac - 0.5
}

// ---------------------------------------------------------------------------
// SPARQL Results JSON — minimal deserializer, verbatim from sibling.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct SparqlResults {
    pub results: SparqlResultsBody,
}

#[derive(Deserialize)]
pub(crate) struct SparqlResultsBody {
    pub bindings: Vec<std::collections::HashMap<String, SparqlBinding>>,
}

#[derive(Deserialize, Clone)]
pub(crate) struct SparqlBinding {
    #[serde(default, rename = "type")]
    pub kind: String,
    pub value: String,
}

impl SparqlBinding {
    pub(crate) fn is_resource(&self) -> bool {
        matches!(self.kind.as_str(), "uri" | "bnode" | "")
    }
}

// ---------------------------------------------------------------------------
// Tests — host build only. wasi:nn is short-circuited to the stub
// on the host, so these tests do NOT exercise the wasi:nn path.
// For that, run the sagegraph cases against Oxigraph / QLever.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct MockHost {
        queries: RefCell<Vec<String>>,
        results: RefCell<Vec<String>>,
    }

    impl MockHost {
        fn new(results: Vec<String>) -> Self {
            Self {
                queries: RefCell::new(Vec::new()),
                results: RefCell::new(results),
            }
        }
    }

    impl HostBridge for MockHost {
        fn execute_query(&self, query: &str) -> Result<String, String> {
            self.queries.borrow_mut().push(query.to_string());
            if self.results.borrow().is_empty() {
                Err("mock: out of canned results".into())
            } else {
                Ok(self.results.borrow_mut().remove(0))
            }
        }
        fn http_post_json(&self, _u: &str, _b: &str) -> Result<String, String> {
            unreachable!("embed does not POST")
        }
    }

    fn empty_bindings() -> String {
        r#"{"head":{"vars":["n"]},"results":{"bindings":[]}}"#.to_string()
    }

    fn opts(dim: u32) -> EmbedOpts {
        EmbedOpts {
            dimensions: dim,
            pool: "mean".into(),
            runtime: None,
            fuel_limit: None,
        }
    }

    #[test]
    fn isolated_node_yields_finite_vector() {
        let host = MockHost::new(vec![empty_bindings()]);
        let v = embed("http://ex/alice", "file:///m.onnx", 1, &opts(8), &host).unwrap();
        assert_eq!(v.len(), 8);
        for x in &v {
            assert!(x.is_finite());
        }
    }

    #[test]
    fn zero_dimensions_rejected() {
        let host = MockHost::new(vec![empty_bindings()]);
        let err = embed("http://ex/x", "file:///m.onnx", 1, &opts(0), &host).unwrap_err();
        assert!(err.contains("dimensions"));
    }

    #[test]
    fn empty_model_url_uses_stub_path() {
        let host = MockHost::new(vec![empty_bindings()]);
        let v = embed("http://ex/x", "", 1, &opts(8), &host).unwrap();
        assert_eq!(v.len(), 8);
        for x in &v {
            assert!(x.is_finite());
        }
    }

    #[test]
    fn graphblas_mean_pool_matches_sequential_sum() {
        let feats: Vec<[f32; STRUCT_FEATURES]> = vec![
            [1.0, 2.0, 3.0, 4.0],
            [0.5, 1.5, 2.5, 3.5],
            [10.0, 0.0, -1.0, 0.25],
        ];
        let pooled = aggregate_neighbor_features_via_graphblas(&feats, "mean");
        let n = feats.len() as f32;
        let mut expected = [0.0f32; STRUCT_FEATURES];
        for row in &feats {
            for i in 0..STRUCT_FEATURES {
                expected[i] += row[i];
            }
        }
        for x in expected.iter_mut() {
            *x /= n;
        }
        assert_eq!(pooled, expected);
    }
}
