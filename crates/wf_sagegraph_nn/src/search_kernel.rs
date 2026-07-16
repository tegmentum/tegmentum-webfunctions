//! `search` export body — KNN over cached embeddings per memo §04.
//!
//! Sugar over `wf-vector:search`. The guest POSTs a JSON body to the
//! substrate's vector-search endpoint at `index_url` via
//! `host::http-post-json` and returns the parsed hit list.
//!
//! Body shape:
//! ```json
//! {"query": [f32...], "k": N, "metric": "cosine" | "l2",
//!  "oversample": N?}
//! ```
//!
//! Expected response shape — matches what the substrate's
//! `wf-vector:` endpoint already returns:
//! ```json
//! {"hits": [{"node": "<iri>", "score": 0.87}, ...]}
//! ```
//!
//! # Honesty note
//!
//! `index_url` in v0.2 is a plain HTTP URL — a wf_sagegraph client
//! passes the URL of the substrate's vector-index admin endpoint
//! directly. The memo §04 sketch had `search` internally translating
//! to `wf-vector:search`, but a WASM guest cannot itself invoke a
//! `wf-vector:` SERVICE (the substrate URL scheme is a rewrite-time
//! sugar, not a runtime resolver from inside a guest). The `index_url`
//! passed to `search` is what the substrate side resolved from a
//! registered `wf-vector` index — same information, honestly
//! stripped down to something the guest can actually call.

use serde::Deserialize;

use crate::{Hit, HostBridge, SearchOpts};

pub fn search(
    index_url: &str,
    query_vec: &[f32],
    k: u32,
    opts: &SearchOpts,
    host: &dyn HostBridge,
) -> Result<Vec<Hit>, String> {
    if index_url.is_empty() {
        return Err("wf_sagegraph_nn: search index-url must be non-empty".into());
    }
    if k == 0 {
        return Err("wf_sagegraph_nn: search k must be >= 1".into());
    }
    let body = serde_json::json!({
        "query": query_vec,
        "k": k,
        "metric": opts.metric.clone().unwrap_or_else(|| "cosine".to_string()),
        "oversample": opts.oversample,
    });
    let body_str = serde_json::to_string(&body)
        .map_err(|e| format!("wf_sagegraph_nn: encode search body: {e}"))?;
    let resp = host.http_post_json(index_url, &body_str)?;
    let parsed: SearchResponse = serde_json::from_str(&resp)
        .map_err(|e| format!("wf_sagegraph_nn: parse vector-index response: {e}"))?;
    Ok(parsed
        .hits
        .into_iter()
        .map(|h| Hit {
            node: h.node,
            score: h.score,
        })
        .collect())
}

#[derive(Deserialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
}

#[derive(Deserialize)]
struct SearchHit {
    node: String,
    score: f64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct MockHost {
        posts: RefCell<Vec<(String, String)>>,
        response: RefCell<String>,
    }
    impl MockHost {
        fn new(response: &str) -> Self {
            Self {
                posts: RefCell::new(Vec::new()),
                response: RefCell::new(response.to_string()),
            }
        }
    }
    impl HostBridge for MockHost {
        fn execute_query(&self, _q: &str) -> Result<String, String> {
            unreachable!("search does not execute SPARQL")
        }
        fn http_post_json(&self, url: &str, body: &str) -> Result<String, String> {
            self.posts.borrow_mut().push((url.to_string(), body.to_string()));
            Ok(self.response.borrow().clone())
        }
        fn embed_text(&self, _text: &str, _model: &str) -> Result<Vec<f32>, String> {
            unreachable!("search does not dispatch through wf:embed")
        }
    }

    fn opts() -> SearchOpts {
        SearchOpts { metric: None, oversample: None }
    }

    #[test]
    fn posts_query_and_parses_hits() {
        let resp = r#"{"hits":[
            {"node":"http://ex/a","score":0.91},
            {"node":"http://ex/b","score":0.78}
        ]}"#;
        let host = MockHost::new(resp);
        let hits = search("http://vec/idx", &[0.1, 0.2], 5, &opts(), &host).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].node, "http://ex/a");
        assert!((hits[0].score - 0.91).abs() < 1e-9);
        let posts = host.posts.borrow();
        assert_eq!(posts[0].0, "http://vec/idx");
        // f32 → serde_json Value routes through f64, so 0.1 lands as
        // its double-precision extension (e.g. 0.10000000149011612).
        // Just check the field is present rather than exact-match the
        // widened lexical form.
        assert!(posts[0].1.contains("\"query\":["));
        assert!(posts[0].1.contains("\"k\":5"));
        assert!(posts[0].1.contains("\"metric\":\"cosine\""));
    }

    #[test]
    fn empty_url_rejected() {
        let host = MockHost::new("");
        let e = search("", &[0.0], 3, &opts(), &host).unwrap_err();
        assert!(e.contains("index-url"));
    }

    #[test]
    fn zero_k_rejected() {
        let host = MockHost::new("");
        let e = search("http://x/y", &[0.0], 0, &opts(), &host).unwrap_err();
        assert!(e.contains("k"));
    }

    #[test]
    fn parse_error_surfaces_message() {
        let host = MockHost::new("not-json");
        let e = search("http://x/y", &[0.0], 3, &opts(), &host).unwrap_err();
        assert!(e.contains("parse vector-index response"));
    }
}
