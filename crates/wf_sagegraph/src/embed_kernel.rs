//! `embed` export body — on-the-fly inference per memo §04 tier 1.
//!
//! Pipeline:
//!   1. Issue k-hop SPARQL via `host::execute-query` to enumerate the
//!      neighborhood of `node_iri`.
//!   2. Compute structural features (degree, mean-neighbor-degree,
//!      closure-size) for the node.
//!   3. Aggregate neighbor features via mean-pool (memo §07: real
//!      plus-times semiring, row-normalized).
//!   4. Concatenate [self, aggregated] and run through the stubbed
//!      linear projection standing in for the ONNX forward pass.
//!   5. Return a `list<f32>` of length `opts.dimensions`.

use serde::Deserialize;

use crate::{EmbedOpts, HostBridge};

/// Structural feature vector per node — the input the (stubbed) ONNX
/// model consumes. Kept at fixed length 4 to keep the projection
/// deterministic; grows in v0.3 when real ONNX takes over.
const STRUCT_FEATURES: usize = 4;

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

    // Step 3: per-neighbor structural features + mean-pool.
    let mut sum_neighbor_deg = 0.0f32;
    let mut counted = 0.0f32;
    let mut neighbor_features_sum: [f32; STRUCT_FEATURES] = [0.0; STRUCT_FEATURES];
    for n in &one_hop {
        let n_deg = fetch_degree(n, host).unwrap_or(0.0);
        sum_neighbor_deg += n_deg;
        counted += 1.0;
        let feats = struct_features_for(n_deg, closure.len() as f32);
        for i in 0..STRUCT_FEATURES {
            neighbor_features_sum[i] += feats[i];
        }
    }
    let mean_neighbor_degree = if counted > 0.0 {
        sum_neighbor_deg / counted
    } else {
        0.0
    };
    let neighbor_pool = match opts.pool.as_str() {
        "sum" => neighbor_features_sum,
        "max" => neighbor_features_sum, // v0.2: max stub = sum (real max needs per-neighbor tracking; deferred)
        _ => {
            // mean (default)
            if counted > 0.0 {
                let mut m = neighbor_features_sum;
                for i in 0..STRUCT_FEATURES {
                    m[i] /= counted;
                }
                m
            } else {
                neighbor_features_sum
            }
        }
    };

    // Step 4: self features.
    let self_feats = struct_features_for(degree, closure.len() as f32);
    let count = closure.len() as f32;
    // Cheap scalar summary used to seed the concat vector's tail
    // slot (keeps `[degree, mean_neighbor_degree, count, ...]`
    // observable in the raw pre-projection vector so operators can
    // diff v0.2 against v0.1 arithmetic if they need to).
    let raw_summary: [f32; STRUCT_FEATURES] =
        [degree, mean_neighbor_degree, count, self_feats[3]];

    // Step 5: stubbed ONNX forward pass.
    let out = stubbed_onnx_project(
        &raw_summary,
        &neighbor_pool,
        model_url,
        dimensions,
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// Host-callback wrappers
// ---------------------------------------------------------------------------

/// Return the list of distinct 1-hop neighbors of `node` (both
/// outgoing and incoming). Issues one SPARQL query via
/// `host::execute-query`. Neighbors are returned as their lexical
/// form (IRI or literal-as-string) — the guest doesn't distinguish
/// for the v0.2 stubbed path.
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
            out.push(b.value.clone());
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

// ---------------------------------------------------------------------------
// Stubbed ONNX projection.
// ---------------------------------------------------------------------------

/// Placeholder standing in for the ONNX forward pass. Deterministic
/// per `model_url` (weights derived from a stable FNV-1a hash), so
/// operators can swap "models" and observe vector changes without a
/// real ML runtime. Structure:
///
///   1. Concatenate [self_feats (4), neighbor_pool (4)] → 8-dim
///      input vector.
///   2. Apply `W · x + b` where `W` is a `dimensions × 8` weight
///      matrix synthesized from the model-url hash and `b` a
///      `dimensions`-length bias vector likewise.
///   3. Apply tanh activation.
///
/// v0.3 replaces this whole function with a call into
/// `onnxruntime-wasm` / `wonnx-wasm`. The signature stays constant;
/// callers see byte-stable vectors per (model_url, input).
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
    #[serde(default, rename = "type")]
    pub _kind: String,
    pub value: String,
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
    fn isolated_node_yields_deterministic_vector() {
        // No neighbors — pure self projection. Vector length matches
        // opts.dimensions and every component is finite.
        let host = MockHost::new(vec![empty_bindings()]);
        let v = embed("http://ex/alice", "file:///m.onnx", 1, &opts(8), &host).unwrap();
        assert_eq!(v.len(), 8);
        for x in &v {
            assert!(x.is_finite());
        }
    }

    #[test]
    fn different_model_urls_produce_different_vectors() {
        let host_a = MockHost::new(vec![empty_bindings()]);
        let host_b = MockHost::new(vec![empty_bindings()]);
        let a = embed("http://ex/x", "file:///a.onnx", 1, &opts(4), &host_a).unwrap();
        let b = embed("http://ex/x", "file:///b.onnx", 1, &opts(4), &host_b).unwrap();
        assert_ne!(a, b, "different model URLs must produce different vectors");
    }

    #[test]
    fn same_model_url_produces_same_vector() {
        let ha = MockHost::new(vec![empty_bindings()]);
        let hb = MockHost::new(vec![empty_bindings()]);
        let a = embed("http://ex/x", "file:///m.onnx", 1, &opts(4), &ha).unwrap();
        let b = embed("http://ex/x", "file:///m.onnx", 1, &opts(4), &hb).unwrap();
        assert_eq!(a, b, "same inputs must produce same output");
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
}
