//! wf_embed_sidecar — HTTP sidecar exposing fastembed-rs sentence
//! embeddings for JVM engine substrates that can't link fastembed
//! natively (Jena / RDF4J).
//!
//! # Protocol
//!
//! `POST /embed` with body `{"text":"...","model":"..."}` returns
//! `{"embedding":[f32, ...]}` — the same on-the-wire shape wave-18
//! wired into `HostCallbacks#embedText` on both JVM plugins, gated on
//! `WF_EMBED_SIDECAR_URL`.
//!
//! `GET /health` returns `200 OK` with body `ok` — used by the
//! wf-conformance adapter's `wait_ready` handshake to confirm the
//! subprocess is bound + accepting requests before setting
//! `WF_EMBED_SIDECAR_URL` on the JVM child.
//!
//! Errors surface as HTTP 400 with a plain-text body prefixed
//! `embed-text:` so the JVM caller can render the same message shape
//! it would get from the in-process fastembed path.
//!
//! # Byte parity
//!
//! Dispatches through `fastembed::TextEmbedding` at the pinned
//! `=5.2.0` release — same version, same model catalog, same
//! `TextInitOptions::new(model_enum)` construction as
//! `oxigraph-wf::host::embed_text` and
//! `qlever-wf-runtime::embed_text`. First-call for a given model
//! downloads its files (~130 MB for `bge-small-en`) into
//! `~/.cache/fastembed/`; subsequent calls hit the per-model
//! `TextEmbedding` cache. The resulting f32 lanes are byte-identical
//! to the Rust engines' in-process output, so
//! `sagegraph_text_features.toml`'s pinned `expected_bindings` should
//! hold across (oxigraph, qlever, jena, rdf4j) once the JVM plugins
//! route through this sidecar.
//!
//! # CLI
//!
//! ```text
//! wf_embed_sidecar [--port <port>] [--models <csv>]
//! ```
//!
//! * `--port 0` (default) binds an OS-assigned loopback port.
//! * `--models bge-small-en` (default) is the comma-separated list of
//!   models to eagerly warm the cache on. Empty CSV → warm none; the
//!   first request for a model warms it lazily. Models unknown to
//!   `resolve_embedding_model` are rejected at parse time so the CLI
//!   fails loudly instead of at first request.
//!
//! On startup the process writes `wf_embed_sidecar listening on
//! http://127.0.0.1:<port>` to stderr — the wf-conformance adapter
//! reads that line to discover the ephemeral port. A blank line
//! follows so the parent's line reader terminates cleanly.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Response, Server};

// ---------------------------------------------------------------
// Model catalog + resolver — held in lockstep with
// `oxigraph-wf/src/host.rs::embed_model_catalog` and
// `qlever-wf-runtime/src/lib.rs::resolve_embedding_model`. Adding a
// model here without landing the same name-to-variant on both Rust
// engines would silently break parity, so keep the three tables in
// sync.
// ---------------------------------------------------------------

fn embed_model_catalog() -> &'static [(&'static str, usize)] {
    &[
        ("bge-small-en", 384),
        ("all-MiniLM-L6-v2", 384),
        ("bge-base-en", 768),
        ("nomic-embed-text", 768),
        ("bge-large-en", 1024),
    ]
}

fn resolve_embedding_model(model: &str) -> Result<fastembed::EmbeddingModel, String> {
    use fastembed::EmbeddingModel as M;
    match model.to_ascii_lowercase().as_str() {
        "bge-small-en" => Ok(M::BGESmallENV15),
        "bge-base-en" => Ok(M::BGEBaseENV15),
        "bge-large-en" => Ok(M::BGELargeENV15),
        "all-minilm-l6-v2" => Ok(M::AllMiniLML6V2),
        "nomic-embed-text" => Ok(M::NomicEmbedTextV15),
        _ => Err(format!("embed-text: unknown model: {model}")),
    }
}

// ---------------------------------------------------------------
// Per-model TextEmbedding cache. Same ownership shape as the two
// engine impls: outer Mutex guards the map, inner Mutex per model so
// `embed(...)` can hold `&mut self` without serialising unrelated
// models. Contention on the outer lock is limited to the Arc clone
// (fast); the heavy path — ONNX inference — takes the per-model lock.
// ---------------------------------------------------------------

static EMBED_CACHE: Lazy<Mutex<HashMap<String, Arc<Mutex<fastembed::TextEmbedding>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn get_or_init_embedder(
    model: &str,
) -> Result<Arc<Mutex<fastembed::TextEmbedding>>, String> {
    let cache_key = model.to_ascii_lowercase();
    {
        let map = EMBED_CACHE.lock().expect("EMBED_CACHE poisoned");
        if let Some(existing) = map.get(&cache_key) {
            return Ok(existing.clone());
        }
    }
    let model_enum = resolve_embedding_model(model)?;
    let embedder = fastembed::TextEmbedding::try_new(fastembed::TextInitOptions::new(model_enum))
        .map_err(|e| format!("embed-text: failed to initialise {model}: {e}"))?;
    let arc = Arc::new(Mutex::new(embedder));
    let mut map = EMBED_CACHE.lock().expect("EMBED_CACHE poisoned");
    // Racy insert check — a concurrent request may have already
    // populated the entry while we were building the embedder.
    if let Some(existing) = map.get(&cache_key) {
        return Ok(existing.clone());
    }
    map.insert(cache_key, arc.clone());
    Ok(arc)
}

fn embed_text(text: &str, model: &str) -> Result<Vec<f32>, String> {
    if text.is_empty() {
        return Err("embed-text: text is empty".to_string());
    }
    let embedder = get_or_init_embedder(model)?;
    let mut guard = embedder.lock().expect("per-model embedder mutex poisoned");
    let mut out = guard
        .embed(vec![text], None)
        .map_err(|e| format!("embed-text: inference failed for {model}: {e}"))?;
    if out.is_empty() {
        return Err(format!("embed-text: {model} returned zero rows"));
    }
    Ok(out.remove(0))
}

// ---------------------------------------------------------------
// HTTP request/response envelopes.
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EmbedRequest {
    text: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

// ---------------------------------------------------------------
// CLI arg parsing — deliberately hand-rolled (two flags) instead of
// pulling clap in. `--port <u16>` and `--models <csv>` are the only
// knobs; anything else exits with a usage line so a stale caller
// notices immediately.
// ---------------------------------------------------------------

struct CliArgs {
    port: u16,
    warm_models: Vec<String>,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut port: u16 = 0;
    let mut warm_models: Vec<String> = vec!["bge-small-en".to_string()];
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => {
                let raw = iter.next().ok_or("--port requires a value")?;
                port = raw
                    .parse::<u16>()
                    .map_err(|e| format!("--port {raw}: {e}"))?;
            }
            "--models" => {
                let raw = iter.next().ok_or("--models requires a value")?;
                warm_models = raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--help" | "-h" => {
                return Err("usage: wf_embed_sidecar [--port <port>] [--models <csv>]".into());
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    for m in &warm_models {
        resolve_embedding_model(m)
            .map_err(|e| format!("--models rejected `{m}`: {e}"))?;
    }
    Ok(CliArgs { port, warm_models })
}

// ---------------------------------------------------------------
// Main loop — bind, print banner, dispatch requests inline. tiny_http
// spawns internal accept threads; we serve one request per
// `incoming_requests()` iteration inline. fastembed's TextEmbedding
// is `Sync + Send`-safe under the per-model Mutex, so a caller that
// wants concurrent requests can move the loop to a thread pool later.
// For the conformance suite's serial JVM-invocation shape, inline is
// simplest and avoids surprising thread ordering in the byte-parity
// path.
// ---------------------------------------------------------------

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("wf_embed_sidecar: {msg}");
            std::process::exit(2);
        }
    };

    let bind_addr = format!("127.0.0.1:{}", args.port);
    let server = match Server::http(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wf_embed_sidecar: failed to bind {bind_addr}: {e}");
            std::process::exit(1);
        }
    };
    let actual_port = match server.server_addr().to_ip() {
        Some(addr) => addr.port(),
        None => {
            eprintln!("wf_embed_sidecar: bound server had no IP address");
            std::process::exit(1);
        }
    };

    // Banner on stderr — the wf-conformance adapter's `wait_ready`
    // parses this line to discover the ephemeral port when
    // `--port 0` was used. Format is stable across releases.
    eprintln!("wf_embed_sidecar listening on http://127.0.0.1:{actual_port}");
    eprintln!();

    // Optional eager warm — pulls the model bytes into
    // `~/.cache/fastembed/` before the first `/embed` hit so the
    // conformance case doesn't see a multi-second first-call latency
    // inside its test budget. Failures here are logged but non-fatal
    // — the model will re-attempt on demand.
    for m in &args.warm_models {
        match get_or_init_embedder(m) {
            Ok(_) => eprintln!("wf_embed_sidecar: warmed model `{m}`"),
            Err(e) => eprintln!("wf_embed_sidecar: warm `{m}` failed (will retry on demand): {e}"),
        }
    }

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        match (&method, url.as_str()) {
            (Method::Get, "/health") => {
                let resp = Response::from_string("ok")
                    .with_status_code(200)
                    .with_header(text_plain_header());
                let _ = request.respond(resp);
            }
            (Method::Get, "/models") => {
                // Diagnostic-only — mirrors `list-models` from the WIT.
                let names: Vec<&&str> =
                    embed_model_catalog().iter().map(|(n, _)| n).collect();
                let body = serde_json::to_string(&names).unwrap_or_else(|_| "[]".into());
                let resp = Response::from_string(body)
                    .with_status_code(200)
                    .with_header(json_header());
                let _ = request.respond(resp);
            }
            (Method::Post, "/embed") => {
                let mut body = String::new();
                if let Err(e) = request.as_reader().read_to_string(&mut body) {
                    let msg = format!("embed-text: failed to read body: {e}");
                    let resp = Response::from_string(msg)
                        .with_status_code(400)
                        .with_header(text_plain_header());
                    let _ = request.respond(resp);
                    continue;
                }
                let req: EmbedRequest = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        let msg = format!("embed-text: bad json body: {e}");
                        let resp = Response::from_string(msg)
                            .with_status_code(400)
                            .with_header(text_plain_header());
                        let _ = request.respond(resp);
                        continue;
                    }
                };
                match embed_text(&req.text, &req.model) {
                    Ok(vec) => {
                        let out = EmbedResponse { embedding: vec };
                        let body = match serde_json::to_string(&out) {
                            Ok(s) => s,
                            Err(e) => {
                                let msg = format!("embed-text: response serialise: {e}");
                                let resp = Response::from_string(msg)
                                    .with_status_code(500)
                                    .with_header(text_plain_header());
                                let _ = request.respond(resp);
                                continue;
                            }
                        };
                        let len = body.len();
                        let resp = Response::new(
                            tiny_http::StatusCode(200),
                            vec![json_header()],
                            Cursor::new(body.into_bytes()),
                            Some(len),
                            None,
                        );
                        let _ = request.respond(resp);
                    }
                    Err(msg) => {
                        let resp = Response::from_string(msg)
                            .with_status_code(400)
                            .with_header(text_plain_header());
                        let _ = request.respond(resp);
                    }
                }
            }
            _ => {
                let resp = Response::from_string(format!(
                    "wf_embed_sidecar: no route for {method:?} {url}"
                ))
                .with_status_code(404)
                .with_header(text_plain_header());
                let _ = request.respond(resp);
            }
        }
    }
}

fn text_plain_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
        .expect("static header always parses")
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header always parses")
}
