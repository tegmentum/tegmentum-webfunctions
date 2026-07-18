# wf_embed

Substrate WIT interface for text-embedding host callbacks
(`wf:embed/host@0.1.0`). Not itself a wasm component — this crate
carries only the shared WIT so substrate engines and future guests
reference identical bytes.

## Consumers

- `oxigraph-wf/src/host.rs::add_embed_to_linker` — Rust engine registration.
- `qlever-wf-runtime/src/lib.rs::register_wf_embed_host_import` — same interface, C-ABI substrate.
- `wf_sagegraph` guest (planned v0.5) — text-attributed feature mode
  (memo §06) calls `wf:embed/host.embed-text(node_label, "bge-small-en")`
  to obtain the input feature vector.

## Implementation

v0.1 substrate registrations use a deterministic SHA-256-derived stub
projection: byte-stable per `(text, model)` across engines, no model
download, no network. This matches the wf_sagegraph v0.2
hash-of-model_url pattern that landed structural features first and
kept the ABI honest ahead of the ONNX story.

Follow-up production landing swaps the stub for `fastembed-rs`
(pure-Rust ONNX embedder, on crates.io). The WIT signature does not
change; only the substrate-side registration flips.

## Not yet in the workspace

This crate is intentionally not added to `[workspace.members]` in
`webfunctions/Cargo.toml`. The guest-side wrappers that
would produce a compiled component live in the wf_sagegraph /
wf_sagegraph_nn tree and land in a follow-up sweep once the WIT is
stable. For now this crate is a WIT-only home so the interface
identity is authored once and referenced by every consumer.
