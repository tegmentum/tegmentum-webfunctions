//! `sweep` export body — precompute path per memo §04 tier 2.
//!
//! Pipeline:
//!   1. Issue `subject_pattern` (a `SELECT ?s WHERE { … }` fragment)
//!      via `host::execute-query`. Enumerates every subject to embed.
//!   2. For each subject, run the same in-process embed logic as the
//!      `embed` export.
//!   3. POST one JSON row per subject to `target_sink_url` via
//!      `host::http-post-json`. Row shape: `{"node": "<iri>",
//!      "embedding": [f32, ...]}`.
//!   4. Return the count of rows written.
//!
//! Batching / concurrency knobs in `SweepOpts` are honored on the
//! substrate side; the guest issues rows sequentially in v0.2 because
//! `wit_bindgen`'s host imports are not designed for guest-side
//! concurrency. v0.3 concern.

use crate::{HostBridge, SweepOpts, EmbedOpts};

pub fn sweep(
    model_url: &str,
    subject_pattern: &str,
    target_sink_url: &str,
    k_hops: u32,
    opts: &SweepOpts,
    host: &dyn HostBridge,
) -> Result<u32, String> {
    if target_sink_url.is_empty() {
        return Err("wf_sagegraph_nn: sweep target-sink-url must be non-empty".into());
    }
    let dimensions = opts.dimensions;
    if dimensions == 0 {
        return Err("wf_sagegraph_nn: sweep opts.dimensions must be >= 1".into());
    }

    let subjects = enumerate_subjects(subject_pattern, host)?;
    let embed_opts = EmbedOpts {
        dimensions,
        pool: opts.pool.clone(),
        runtime: None,
        fuel_limit: None,
    };

    let mut written: u32 = 0;
    for s in subjects {
        let v = crate::embed_kernel::embed(&s, model_url, k_hops, &embed_opts, host)?;
        let row = serde_json::json!({
            "node": s,
            "embedding": v,
        });
        let body = serde_json::to_string(&row)
            .map_err(|e| format!("wf_sagegraph_nn: encode sweep row: {e}"))?;
        // POST one row per subject. Backend layer is responsible for
        // batching; a JSONL sink would append, a Postgres sink would
        // INSERT ON CONFLICT.
        host.http_post_json(target_sink_url, &body)
            .map_err(|e| format!("wf_sagegraph_nn: sink POST failed for {s}: {e}"))?;
        written += 1;
    }
    Ok(written)
}

/// Run `subject_pattern` (a `SELECT ?s WHERE { … }` fragment) through
/// the host callback and pull the ?s column. The pattern must return
/// a variable named `s` — the guest doesn't inspect the SELECT
/// projection since it just wants IRIs to iterate.
fn enumerate_subjects(
    subject_pattern: &str,
    host: &dyn HostBridge,
) -> Result<Vec<String>, String> {
    let json = host.execute_query(subject_pattern)?;
    let parsed: crate::embed_kernel::SparqlResults = serde_json::from_str(&json)
        .map_err(|e| format!("wf_sagegraph_nn: parse subject SPARQL Results: {e}"))?;
    let mut out = Vec::new();
    for binding in parsed.results.bindings {
        // Prefer `?s`; fall back to the first bound variable so
        // operators aren't locked to one canonical name.
        if let Some(b) = binding.get("s") {
            out.push(b.value.clone());
        } else if let Some((_, b)) = binding.iter().next() {
            out.push(b.value.clone());
        }
    }
    Ok(out)
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
        query_results: RefCell<Vec<String>>,
        posts: RefCell<Vec<(String, String)>>,
    }
    impl MockHost {
        fn new(query_results: Vec<String>) -> Self {
            Self {
                queries: RefCell::new(Vec::new()),
                query_results: RefCell::new(query_results),
                posts: RefCell::new(Vec::new()),
            }
        }
    }
    impl HostBridge for MockHost {
        fn execute_query(&self, q: &str) -> Result<String, String> {
            self.queries.borrow_mut().push(q.to_string());
            if self.query_results.borrow().is_empty() {
                Err("mock: out of canned results".into())
            } else {
                Ok(self.query_results.borrow_mut().remove(0))
            }
        }
        fn http_post_json(&self, url: &str, body: &str) -> Result<String, String> {
            self.posts
                .borrow_mut()
                .push((url.to_string(), body.to_string()));
            Ok("{\"ok\": true}".into())
        }
    }

    fn subj_pattern_result() -> String {
        // Two subjects.
        r#"{"head":{"vars":["s"]},"results":{"bindings":[
            {"s":{"type":"uri","value":"http://ex/alice"}},
            {"s":{"type":"uri","value":"http://ex/bob"}}
        ]}}"#.to_string()
    }
    fn empty_bindings() -> String {
        r#"{"head":{"vars":["n"]},"results":{"bindings":[]}}"#.to_string()
    }

    fn opts(dim: u32) -> SweepOpts {
        SweepOpts {
            dimensions: dim,
            pool: "mean".into(),
            batch_size: 32,
            concurrency: 1,
        }
    }

    #[test]
    fn sweep_iterates_subjects_and_posts_rows() {
        // 1 subj-pattern query + 2 subject one-hop queries + 2 POSTs.
        let host = MockHost::new(vec![
            subj_pattern_result(),
            empty_bindings(),
            empty_bindings(),
        ]);
        let n = sweep(
            "file:///m.onnx",
            "SELECT ?s WHERE { ?s a <http://ex/Person> }",
            "postgres://cache/embeddings",
            1,
            &opts(4),
            &host,
        )
        .expect("sweep");
        assert_eq!(n, 2);
        let posts = host.posts.borrow();
        assert_eq!(posts.len(), 2, "expect one POST per subject");
        assert!(posts[0].0 == "postgres://cache/embeddings");
        assert!(posts[0].1.contains("\"node\":\"http://ex/alice\""));
        assert!(posts[0].1.contains("\"embedding\":["));
        assert!(posts[1].1.contains("\"node\":\"http://ex/bob\""));
    }

    #[test]
    fn empty_sink_url_rejected() {
        let host = MockHost::new(vec![]);
        let err = sweep("file:///m.onnx", "SELECT ?s WHERE { ?s ?p ?o }", "", 1, &opts(4), &host)
            .unwrap_err();
        assert!(err.contains("target-sink-url"));
    }

    #[test]
    fn zero_dimensions_rejected() {
        let host = MockHost::new(vec![]);
        let err = sweep(
            "file:///m.onnx",
            "SELECT ?s WHERE { ?s ?p ?o }",
            "postgres://cache/e",
            1,
            &opts(0),
            &host,
        )
        .unwrap_err();
        assert!(err.contains("dimensions"));
    }
}
