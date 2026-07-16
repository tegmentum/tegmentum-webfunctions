//! `embed` export body — on-the-fly inference per memo §04 tier 1.
//!
//! Pipeline:
//!   1. Issue k-hop SPARQL via `host::execute-query` to enumerate the
//!      neighborhood of `node_iri`.
//!   2. Compute structural features (degree, mean-neighbor-degree,
//!      closure-size) for the node.
//!   3. Aggregate neighbor features via mean-pool (memo §07: real
//!      plus-times semiring, row-normalized).
//!   4. Concatenate `[self, aggregated]` into the 8-dim input the demo
//!      ONNX model expects.
//!   5. Run the model via `tract-onnx` (`Gemm -> Relu -> Gemm -> Tanh`)
//!      and return the output as `list<f32>`.
//!
//! v0.3 replaces the v0.2 "hash-of-model_url weight synth + tanh"
//! placeholder with real ONNX inference using a small bundled demo
//! model (`models/graphsage_demo.onnx`). When `model_url` is empty
//! OR the guest cannot load a model OR the model's output dimension
//! doesn't match `opts.dimensions`, the guest falls back to the v0.2
//! deterministic hash-seeded projection so cases without a real model
//! still get byte-stable output. Which branch fires is logged via
//! `eprintln!` (surfaces on stderr in the wasi-p1 substrate).

use serde::Deserialize;

use crate::{EmbedOpts, HostBridge};

/// Structural feature vector per node — the input the ONNX model
/// consumes. Kept at length 4 so the concat `[self, neighbor_pool]`
/// lands at length 8, which is the input shape of `graphsage_demo.onnx`.
const STRUCT_FEATURES: usize = 4;

/// Bundled demo ONNX model. Two-layer MLP:
///   input(1,8) -> Gemm(W1,b1) -> Relu -> Gemm(W2,b2) -> Tanh -> output(1,8)
/// Weights are hand-crafted, integer-friendly, deterministic. See
/// `scripts/build_demo_onnx.py` in the v0.3 landing PR for the recipe.
///
/// Committing the .onnx bytes into the guest wasm keeps the
/// end-to-end path hermetic: no HTTP fetch, no host-side model
/// registry, no wasi:nn hookup. The model URL is still routed through
/// the guest so a future v0.4 can dispatch on it (bundled vs. fetched
/// vs. wasi:nn-supplied) without a WIT change.
const DEMO_MODEL_ONNX: &[u8] = include_bytes!("../models/graphsage_demo.onnx");

/// Output dimension of `DEMO_MODEL_ONNX`. The demo model is fixed at
/// 8; when `opts.dimensions` doesn't match, the guest falls back to
/// the deterministic hash-seeded stub which honors any dimension.
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
        return Err("wf_sagegraph: embed-opts.dimensions must be >= 1".to_string());
    }

    // Step 1: fetch 1-hop neighbors via host callback.
    let one_hop = fetch_one_hop(node_iri, host)?;
    let degree = one_hop.len() as f32;

    // Step 2: build the k-hop closure by BFS. Bounded by k_hops.
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
    //
    // v0.3.1 lands `graphblas-sparse` as the aggregation backend per
    // memo §07 ("neighbor aggregation is a sparse matrix-vector
    // product; graphblas-wasm covers both directly"). The pool
    // operator picks the semiring / monoid:
    //   * mean  -> PlusZero reduce, then divide by count
    //   * sum   -> PlusZero reduce
    //   * max   -> MaxNegInf reduce  (was a `sum` stub in v0.3)
    // Numeric parity with the prior sequential Rust math holds for
    // mean/sum on the same input order (both accumulate identity
    // then Plus in sequence). `max` now returns a real per-feature
    // maximum instead of the v0.3 sum-stub.
    let mut sum_neighbor_deg = 0.0f32;
    let mut counted = 0.0f32;
    let mut per_neighbor_feats: Vec<[f32; STRUCT_FEATURES]> =
        Vec::with_capacity(one_hop.len());
    for n in &one_hop {
        let n_deg = fetch_degree(n, host).unwrap_or(0.0);
        sum_neighbor_deg += n_deg;
        counted += 1.0;
        per_neighbor_feats.push(struct_features_for(n_deg, closure.len() as f32));
    }
    let mean_neighbor_degree = if counted > 0.0 {
        sum_neighbor_deg / counted
    } else {
        0.0
    };
    let neighbor_pool =
        aggregate_neighbor_features_via_graphblas(&per_neighbor_feats, opts.pool.as_str());

    // Step 4: self features.
    let self_feats = struct_features_for(degree, closure.len() as f32);
    let count = closure.len() as f32;
    // Cheap scalar summary retained so operators can diff the raw
    // pre-projection vector against the v0.1 arithmetic if they need
    // to; unused by the ONNX path, only feeds the fallback stub.
    let raw_summary: [f32; STRUCT_FEATURES] =
        [degree, mean_neighbor_degree, count, self_feats[3]];

    // Step 5: forward pass. Prefer real ONNX inference on the bundled
    // demo model when the caller's opts.dimensions matches the model's
    // output shape AND a model_url was supplied. Fall back to the v0.2
    // deterministic hash-seeded stub otherwise.
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

    if !model_url.is_empty() && dimensions == DEMO_MODEL_OUTPUT_DIM {
        match run_demo_onnx(&input8) {
            Ok(v) => {
                eprintln!(
                    "wf_sagegraph: embed via tract-onnx demo model (model_url={model_url}, dim={dimensions})"
                );
                return Ok(v);
            }
            Err(e) => {
                // Honest failure: emit the reason and drop to the stub
                // so the call still returns SOMETHING deterministic.
                eprintln!(
                    "wf_sagegraph: tract-onnx inference failed ({e}); falling back to hash-seeded stub"
                );
            }
        }
    } else {
        eprintln!(
            "wf_sagegraph: embed via hash-seeded stub (model_url={:?}, dim={dimensions})",
            model_url
        );
    }

    Ok(stubbed_onnx_project(
        &raw_summary,
        &neighbor_pool,
        model_url,
        dimensions,
    ))
}

// ---------------------------------------------------------------------------
// Host-callback wrappers
// ---------------------------------------------------------------------------

/// Return the list of distinct 1-hop **resource** neighbors of
/// `node` (IRI or blank-node, both outgoing and incoming). Issues
/// one SPARQL query via `host::execute-query`.
///
/// v0.3.1 (memo §07 literal-filter fix): literal-shaped ?n
/// bindings are dropped here. Prior versions returned every ?n and
/// then let the degree lookup on `"Alice"@en` etc. either fail
/// SPARQL parse or silently fold a zero into the mean. That was
/// cross-engine byte-identical but semantically wrong — literal
/// values aren't graph neighbors, they're attribute-value payloads
/// on the node's own triples. The filter now happens up-front so
/// BFS expansion, closure sizing, and mean-degree folds all see the
/// same resource-only view.
pub(crate) fn fetch_one_hop(
    node: &str,
    host: &dyn HostBridge,
) -> Result<Vec<String>, String> {
    let query = format!(
        "SELECT DISTINCT ?n WHERE {{ \
            {{ <{node}> ?p ?n }} UNION {{ ?n ?p <{node}> }} \
        }}"
    );
    let json = host.execute_query(&query)?;
    let parsed: SparqlResults = serde_json::from_str(&json)
        .map_err(|e| format!("wf_sagegraph: parse SPARQL Results: {e}"))?;
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

/// Undirected degree of `node`: number of triples where `node`
/// appears as subject OR object. Issues one COUNT query via
/// `host::execute-query`.
pub(crate) fn fetch_degree(
    node: &str,
    host: &dyn HostBridge,
) -> Result<f32, String> {
    let query = format!(
        "SELECT (COUNT(*) AS ?c) WHERE {{ \
            {{ <{node}> ?p ?o }} UNION {{ ?s ?p <{node}> }} \
        }}"
    );
    let json = host.execute_query(&query)?;
    let parsed: SparqlResults = serde_json::from_str(&json)
        .map_err(|e| format!("wf_sagegraph: parse SPARQL Results: {e}"))?;
    let row = parsed
        .results
        .bindings
        .first()
        .ok_or_else(|| "wf_sagegraph: COUNT query returned no rows".to_string())?;
    let c = row
        .get("c")
        .ok_or_else(|| "wf_sagegraph: COUNT query missing ?c".to_string())?;
    c.value
        .parse::<f32>()
        .map_err(|e| format!("wf_sagegraph: parse COUNT: {e}"))
}

// ---------------------------------------------------------------------------
// Feature extraction — v0.2 structural only.
// ---------------------------------------------------------------------------

/// Fixed 4-dim structural feature vector for a node given (degree,
/// closure-size). v0.2 only. v0.3 replaces with real GraphBLAS-backed
/// clustering / PageRank per memo §06.
fn struct_features_for(degree: f32, closure_size: f32) -> [f32; STRUCT_FEATURES] {
    let normalized_degree = if closure_size > 0.0 {
        degree / (1.0 + closure_size)
    } else {
        0.0
    };
    [degree, degree.ln_1p(), normalized_degree, 1.0]
}

/// Aggregate per-neighbor structural feature vectors into a single
/// pooled `[f32; STRUCT_FEATURES]` via `graphblas-sparse` (memo §07).
///
/// We lay out the neighbor features as a `STRUCT_FEATURES × N` CSR
/// where row `i` holds the values of feature `i` across all `N`
/// neighbors, then call `reduce_to_vector` with the pool-selected
/// monoid. Row-wise reduction gives one pooled value per feature.
///
/// Pool semantics:
///   * `"mean"` (default when unset or unknown) — PlusZero reduce,
///     divide by count. Numerically identical to the prior sequential
///     Rust accumulator on the same input order.
///   * `"sum"` — PlusZero reduce, no divide.
///   * `"max"` — MaxNegInf reduce. This is the real per-feature max;
///     v0.3 shipped a stub that returned the sum here.
///
/// Empty-neighbor case: returns `[0.0; STRUCT_FEATURES]` regardless
/// of monoid, so `max` on empty stays at zero instead of leaking
/// `f32::NEG_INFINITY` into the ONNX input. Caller relies on that
/// (see the `input8` build in `embed`).
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

    // Build STRUCT_FEATURES × N CSR. Row i, col j = per_neighbor[j][i].
    // Order across cols matches the caller's iteration order so f32
    // Plus-reduce lands the same summation tree as the pre-v0.3.1
    // sequential accumulator — critical for byte-identical parity on
    // the default `mean` pool.
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
        // sum and mean both start from PlusZero; mean divides after.
        _ => MonoidId::PlusZero,
    };

    let (indices, reduced) = match reduce_to_vector(monoid, &csr) {
        Ok(t) => t,
        // Reduce only fails on dtype mismatch — we built the CSR
        // ourselves as all-Float32, so this branch is defensively
        // impossible. Fall back to zeros to preserve the honest-shape
        // contract in the impossible-but-not-crashing case.
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

    // Mean = plus-reduce then row-normalize by degree (memo §07).
    // `sum` and `max` skip the normalize.
    if pool != "sum" && pool != "max" {
        let count_f = n as f32;
        for x in out.iter_mut() {
            *x /= count_f;
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Real ONNX inference — v0.3 landing.
// ---------------------------------------------------------------------------

/// Run the bundled demo ONNX model against `input`. Returns the
/// flattened output tensor as a Vec<f32>. Errors surface as
/// human-readable strings for the caller to log-and-fall-back.
///
/// tract-onnx is pure Rust, no wgpu, no wasi-nn, no libonnxruntime;
/// it compiles cleanly to `wasm32-wasip1`. Every engine that
/// instantiates this guest sees byte-identical output for the same
/// `input`, because IEEE-754 f32 arithmetic is deterministic and the
/// model bytes + tract runtime version are pinned.
fn run_demo_onnx(input: &[f32; 8]) -> Result<Vec<f32>, String> {
    use tract_onnx::prelude::*;
    let mut cur = std::io::Cursor::new(DEMO_MODEL_ONNX);
    let model = tract_onnx::onnx()
        .model_for_read(&mut cur)
        .map_err(|e| format!("model_for_read: {e}"))?
        .into_optimized()
        .map_err(|e| format!("into_optimized: {e}"))?
        .into_runnable()
        .map_err(|e| format!("into_runnable: {e}"))?;
    let tensor = tract_ndarray::Array2::from_shape_vec((1, 8), input.to_vec())
        .map_err(|e| format!("shape_vec: {e}"))?
        .into_tensor();
    let outputs = model
        .run(tvec![tensor.into()])
        .map_err(|e| format!("run: {e}"))?;
    let view = outputs[0]
        .to_array_view::<f32>()
        .map_err(|e| format!("to_array_view: {e}"))?;
    Ok(view.iter().copied().collect())
}

// ---------------------------------------------------------------------------
// Deterministic hash-seeded projection — the honest fallback.
// ---------------------------------------------------------------------------

/// Placeholder standing in for the ONNX forward pass when the demo
/// model can't service the request (empty model_url, mismatched
/// output dim, tract failure). Deterministic per `model_url` (weights
/// derived from a stable FNV-1a hash), so operators can swap
/// "models" and observe vector changes even without a real ONNX file.
///
/// Structure:
///   1. Concatenate `[self_feats (4), neighbor_pool (4)]` -> 8-dim
///      input vector.
///   2. Apply `W · x + b` where `W` is a `dimensions × 8` weight
///      matrix synthesized from the model-url hash and `b` a
///      `dimensions`-length bias vector likewise.
///   3. Apply tanh activation.
///
/// Kept as a first-class path so cases without a real model URL still
/// exercise the substrate ABI end-to-end.
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

/// FNV-1a 64-bit hash — deterministic, no dependencies.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Deterministic [-0.5, 0.5] weight from a 64-bit seed. Just enough
/// dispersion to make different `model_url`s produce visibly different
/// projections without pretending to be principled ML.
fn seed_to_weight(seed: u64) -> f32 {
    let n = (seed >> 11) as u32;
    let frac = (n as f32) / (u32::MAX as f32);
    frac - 0.5
}

// ---------------------------------------------------------------------------
// SPARQL Results JSON — minimal deserializer.
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
    /// `"uri"`, `"bnode"`, `"literal"`, or `"typed-literal"` per
    /// SPARQL Results JSON. Load-bearing for the literal-filter fix
    /// (v0.3.1): degree lookups only make sense for IRI/bnode
    /// terms, so literal-shaped bindings are dropped before neighbor
    /// aggregation. See `fetch_one_hop`.
    #[serde(default, rename = "type")]
    pub kind: String,
    pub value: String,
}

impl SparqlBinding {
    /// True when this binding names a graph resource (IRI or blank
    /// node) that can plausibly have its own outgoing/incoming
    /// triples. False for literal-shaped bindings (`"literal"`,
    /// `"typed-literal"`) which are attribute values, not neighbors.
    ///
    /// v0.3.1 (memo §07) literal-filter fix: `fetch_one_hop` used to
    /// return every ?n binding regardless of kind, and the degree
    /// lookup on a literal-shaped ?n would either fail SPARQL parse
    /// (silently swallowed by `unwrap_or(0.0)`) or fold a spurious
    /// zero into the mean. Filtering here removes the semantic
    /// imprecision.
    pub(crate) fn is_resource(&self) -> bool {
        matches!(self.kind.as_str(), "uri" | "bnode" | "" )
    }
}

// ---------------------------------------------------------------------------
// Tests
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
    fn one_neighbor(iri: &str) -> String {
        format!(
            r#"{{"head":{{"vars":["n"]}},"results":{{"bindings":[{{"n":{{"type":"uri","value":"{iri}"}}}}]}}}}"#
        )
    }
    fn count(n: usize) -> String {
        format!(
            r#"{{"head":{{"vars":["c"]}},"results":{{"bindings":[{{"c":{{"type":"literal","value":"{n}"}}}}]}}}}"#
        )
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
        // No neighbors. dim=8 hits the ONNX path (matches DEMO model
        // output dim); every component must be finite regardless.
        let host = MockHost::new(vec![empty_bindings()]);
        let v = embed("http://ex/alice", "file:///m.onnx", 1, &opts(8), &host).unwrap();
        assert_eq!(v.len(), 8);
        for x in &v {
            assert!(x.is_finite());
        }
    }

    #[test]
    fn onnx_path_deterministic_same_input() {
        // Same input, same output — real inference must be
        // byte-identical across calls.
        let ha = MockHost::new(vec![empty_bindings()]);
        let hb = MockHost::new(vec![empty_bindings()]);
        let a = embed("http://ex/x", "file:///m.onnx", 1, &opts(8), &ha).unwrap();
        let b = embed("http://ex/x", "file:///m.onnx", 1, &opts(8), &hb).unwrap();
        assert_eq!(a, b, "same inputs must produce same ONNX output");
    }

    #[test]
    fn onnx_path_independent_of_model_url() {
        // Because the demo model is bundled and dispatched on
        // dimension match, model_url doesn't tint the output for the
        // ONNX path. Different model_urls yield the SAME vector at
        // dim=8. (v0.4 will honor model_url for real per-URL model
        // fetch.)
        let ha = MockHost::new(vec![empty_bindings()]);
        let hb = MockHost::new(vec![empty_bindings()]);
        let a = embed("http://ex/x", "file:///a.onnx", 1, &opts(8), &ha).unwrap();
        let b = embed("http://ex/x", "file:///b.onnx", 1, &opts(8), &hb).unwrap();
        assert_eq!(a, b, "demo model ignores model_url in v0.3 minimal");
    }

    #[test]
    fn stub_path_different_urls_different_vectors() {
        // Dim=4 skips the demo model (mismatched output dim) and
        // falls back to the hash-seeded stub — where model_url DOES
        // tint the output.
        let ha = MockHost::new(vec![empty_bindings()]);
        let hb = MockHost::new(vec![empty_bindings()]);
        let a = embed("http://ex/x", "file:///a.onnx", 1, &opts(4), &ha).unwrap();
        let b = embed("http://ex/x", "file:///b.onnx", 1, &opts(4), &hb).unwrap();
        assert_ne!(a, b, "stub path must remain model_url-sensitive");
    }

    #[test]
    fn empty_model_url_uses_stub_path() {
        // Empty model_url short-circuits to the stub even at dim=8,
        // so the caller retains a way to force the deterministic
        // hash-seeded output for regression testing.
        let host = MockHost::new(vec![empty_bindings()]);
        let v = embed("http://ex/x", "", 1, &opts(8), &host).unwrap();
        assert_eq!(v.len(), 8);
        // The all-zero-features stub vector for the empty-url seed is
        // the bias tanh; every component finite and non-NaN.
        for x in &v {
            assert!(x.is_finite());
        }
    }

    #[test]
    fn zero_dimensions_rejected() {
        let host = MockHost::new(vec![empty_bindings()]);
        let err = embed("http://ex/x", "file:///m.onnx", 1, &opts(0), &host)
            .unwrap_err();
        assert!(err.contains("dimensions"));
    }

    #[test]
    fn one_hop_bfs_calls_expected_queries() {
        // node with one neighbor. one_hop query, then degree query for
        // that neighbor.
        let host = MockHost::new(vec![one_neighbor("http://ex/bob"), count(3)]);
        let v = embed("http://ex/alice", "file:///m.onnx", 1, &opts(4), &host).unwrap();
        assert_eq!(v.len(), 4);
        let qs = host.queries.borrow();
        assert_eq!(qs.len(), 2, "expect one-hop + one degree call");
        assert!(qs[0].contains("<http://ex/alice>"), "first query pulls neighbors");
        assert!(qs[1].contains("<http://ex/bob>"), "second query is degree of neighbor");
    }

    /// v0.3.1 regression — literal-shaped one-hop bindings must NOT
    /// be treated as graph neighbors. Before the filter went in, the
    /// guest would fire a `SELECT (COUNT(*)) WHERE { { <Alice> ?p ?o } ... }`
    /// degree probe against `"Alice"@en`, silently swallow the
    /// SPARQL parse error via `unwrap_or(0.0)`, and fold a spurious
    /// zero into the neighbor-degree mean. This test pins the fixed
    /// behavior: mixed-kind bindings drop the literals, so only the
    /// resource neighbor's degree probe fires.
    #[test]
    fn fetch_one_hop_filters_literals() {
        // Mixed-kind bindings: two IRIs, one literal, one bnode.
        // Only the IRIs and the bnode should count as neighbors.
        let mixed = r#"{"head":{"vars":["n"]},"results":{"bindings":[
            {"n":{"type":"uri","value":"http://ex/bob"}},
            {"n":{"type":"literal","value":"Alice"}},
            {"n":{"type":"typed-literal","value":"165","datatype":"http://www.w3.org/2001/XMLSchema#integer"}},
            {"n":{"type":"uri","value":"http://ex/carol"}},
            {"n":{"type":"bnode","value":"b0"}}
        ]}}"#.to_string();
        // Three neighbors survive the filter (bob, carol, b0) so we
        // need three degree canned results.
        let host = MockHost::new(vec![mixed, count(2), count(3), count(1)]);
        let v = embed("http://ex/alice", "file:///m.onnx", 1, &opts(4), &host).unwrap();
        assert_eq!(v.len(), 4);
        let qs = host.queries.borrow();
        assert_eq!(
            qs.len(),
            4,
            "one one-hop + three degree probes (literals filtered out)"
        );
        // First probe hits alice's one-hop query.
        assert!(qs[0].contains("<http://ex/alice>"));
        // The three follow-ups probe the resource neighbors' degrees;
        // no probe fires against the literal values.
        let follow_ups: String = qs[1..].join("\n");
        assert!(follow_ups.contains("<http://ex/bob>"), "bob degree probed");
        assert!(follow_ups.contains("<http://ex/carol>"), "carol degree probed");
        assert!(
            !follow_ups.contains("\"Alice\"") && !follow_ups.contains("\"165\""),
            "literal-shaped bindings must NOT trigger degree probes"
        );
    }

    /// v0.3.1 sanity — the graphblas-sparse-backed mean-pool must
    /// land the same summation tree as the pre-v0.3.1 sequential
    /// Rust accumulator on the same input order. Any drift here
    /// would break byte-identical cross-engine parity on the
    /// existing sagegraph cases. Pin the aggregation as a pure
    /// function of the neighbor feature vectors.
    #[test]
    fn graphblas_mean_pool_matches_sequential_sum() {
        let feats: Vec<[f32; STRUCT_FEATURES]> = vec![
            [1.0, 2.0, 3.0, 4.0],
            [0.5, 1.5, 2.5, 3.5],
            [10.0, 0.0, -1.0, 0.25],
        ];
        let pooled = aggregate_neighbor_features_via_graphblas(&feats, "mean");
        // Reference: sequential accumulate then divide by n. Same
        // order = same summation tree = byte-identical result.
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
        assert_eq!(pooled, expected, "graphblas mean must byte-equal sequential mean");
    }

    /// v0.3.1 — `max` was a `sum` stub in v0.3. The graphblas MaxNegInf
    /// monoid gives us a real per-feature max. Pin that behavior.
    #[test]
    fn graphblas_max_pool_returns_true_maximum() {
        let feats: Vec<[f32; STRUCT_FEATURES]> = vec![
            [1.0, 5.0, -3.0, 0.0],
            [4.0, 2.0, -1.0, 7.0],
            [2.0, 3.0, -10.0, 6.5],
        ];
        let pooled = aggregate_neighbor_features_via_graphblas(&feats, "max");
        assert_eq!(pooled, [4.0, 5.0, -1.0, 7.0]);
    }

    /// v0.3.1 — sum pool skips the divide step and matches a plain
    /// sequential accumulator.
    #[test]
    fn graphblas_sum_pool_matches_sequential_sum() {
        let feats: Vec<[f32; STRUCT_FEATURES]> = vec![
            [1.0, 2.0, 3.0, 4.0],
            [5.0, 6.0, 7.0, 8.0],
        ];
        let pooled = aggregate_neighbor_features_via_graphblas(&feats, "sum");
        assert_eq!(pooled, [6.0, 8.0, 10.0, 12.0]);
    }

    /// v0.3.1 — empty neighbor input must return all zeros regardless
    /// of pool, so `max` on empty doesn't leak `f32::NEG_INFINITY` into
    /// the downstream ONNX input.
    #[test]
    fn graphblas_empty_input_returns_zeros_all_pools() {
        for pool in ["mean", "sum", "max", "weird-unknown-pool"] {
            let out = aggregate_neighbor_features_via_graphblas(&[], pool);
            assert_eq!(
                out,
                [0.0; STRUCT_FEATURES],
                "pool {pool:?} on empty must be zeros"
            );
        }
    }
}
