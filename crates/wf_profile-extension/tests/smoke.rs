//! Phase-2 host-callback round-trip smoke test.
//!
//! Proves the migration end-to-end:
//!
//!   1. The `wf_profile-extension` cdylib, built by cargo-component
//!      against the shared `extension-with-host-callbacks` world
//!      (imports `tegmentum:webfunction/graph-callbacks@0.1.0`,
//!      exports `extension`), instantiates cleanly under the reference
//!      native host (`host-callbacks-impl` from
//!      `~/git/oxigraph-webfunction-plugin/`).
//!   2. The extension's `predicate-triple-count` filter function
//!      dispatches back through the standardized callback into the
//!      reference oxigraph store and returns the correct COUNT.
//!   3. The extension's `classify-predicate` filter function drives
//!      the multi-query classification and returns a JSON literal.
//!
//! The reference host and its Store are provided by
//! `host-callbacks-impl` — the same crate whose own smoke test
//! (`crates/host-callbacks-impl/tests/smoke.rs`) demonstrates the
//! pattern for `example-graph-callback-extension`. This test proves
//! the sibling migration (wf_profile) loads under exactly that host
//! with no host-side changes.
//!
//! The extension component is built via `cargo component build
//! --release` at test time. Same pattern the reference smoke uses;
//! keeps the build honest (no stale artifact drift) at the cost of
//! adding a build step to `cargo test`.
//!
//! Non-goals (memo §9):
//!
//!   * `http-callbacks` — this extension does not import it, so the
//!     smoke test uses `HttpCallbackImpl::deny_all()`. The linker
//!     wiring is still exercised transitively (instantiation fails
//!     if the world's imports aren't satisfied).
//!   * `wasm-callbacks` — Phase-1 stub, this extension does not
//!     invoke it.

use std::path::{Path, PathBuf};
use std::process::Command;

use host_callbacks_impl::{
    CallbackHostState, ExtensionWithHostCallbacks, GraphCallbackImpl, HttpCallbackImpl,
    WasmCallbackImpl, wire_into_linker,
};

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

// The bindgen! module inside host-callbacks-impl re-exports the
// generated Term type used by the shared `extension.call` signature.
use host_callbacks_impl::bindings::tegmentum::webfunction::types::Term as WitTerm;

/// Walk up from `CARGO_MANIFEST_DIR` to the workspace root so we can
/// point `cargo component build` at the workspace and locate the
/// built artifact under `target/wasm32-wasip2/release/`.
fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // .../webfunctions/crates/wf_profile-extension
    p.pop(); // crates/
    p.pop(); // repo root
    p
}

/// Build the cdylib under `cargo component`. Same recipe the
/// reference host-callbacks-impl smoke test uses — see
/// `~/git/oxigraph-webfunction-plugin/crates/host-callbacks-impl/tests/smoke.rs`.
fn build_extension_component(root: &Path) {
    let status = Command::new("cargo")
        .current_dir(root)
        .args([
            "component",
            "build",
            "--release",
            "-p",
            "wf_profile-extension",
            "--target",
            "wasm32-wasip2",
        ])
        .status()
        .unwrap_or_else(|e| {
            panic!("cargo component build for wf_profile-extension failed to launch: {e}")
        });
    assert!(
        status.success(),
        "cargo component build for wf_profile-extension exited with {status}"
    );
}

fn fixture_turtle() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("profile.ttl");
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

/// Common setup: build the component, wire a linker, seed the graph
/// with the fixture, return the loaded bindings + store.
fn instantiate() -> (
    Engine,
    Component,
    Linker<CallbackHostState>,
    Store<CallbackHostState>,
) {
    let root = workspace_root();
    build_extension_component(&root);
    let component_path = root
        .join("target/wasm32-wasip2/release/wf_profile_extension.wasm");
    assert!(
        component_path.exists(),
        "expected component at {}",
        component_path.display(),
    );

    let graph = GraphCallbackImpl::new_in_memory().expect("graph impl");
    graph.load_turtle(&fixture_turtle()).expect("load turtle");

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("wasmtime engine");

    let mut linker: Linker<CallbackHostState> = Linker::new(&engine);
    wire_into_linker(&mut linker).expect("wire host-callbacks + WASI into linker");

    let state = CallbackHostState::new(
        graph,
        HttpCallbackImpl::deny_all(),
        WasmCallbackImpl::new(),
    );
    let store: Store<CallbackHostState> = Store::new(&engine, state);

    let component = Component::from_file(&engine, &component_path).expect("load component");
    (engine, component, linker, store)
}

#[test]
fn register_reports_two_functions() {
    let (_engine, component, linker, mut store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate extension-with-host-callbacks");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let descriptors = ext_iface.call_register(&mut store).expect("call register()");
    let names: Vec<&str> = descriptors.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"predicate-triple-count"),
        "expected predicate-triple-count in register(), got {names:?}"
    );
    assert!(
        names.contains(&"classify-predicate"),
        "expected classify-predicate in register(), got {names:?}"
    );
    assert_eq!(
        descriptors.len(),
        2,
        "expected exactly 2 descriptors, got {names:?}"
    );
    for d in &descriptors {
        assert_eq!(d.min_arity, 1, "{} min-arity mismatch", d.name);
        assert_eq!(
            d.max_arity,
            Some(1),
            "{} max-arity mismatch",
            d.name,
        );
    }
}

#[test]
fn predicate_triple_count_knows_returns_four() {
    // Fixture has four `:knows` triples. The extension's
    // predicate-triple-count invokes graph-callbacks::execute-query
    // with `SELECT (COUNT(*) AS ?n) WHERE { ?s <:knows> ?o }` and the
    // reference impl routes the query through oxigraph. Expected: 4.
    let (_engine, component, linker, mut store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let arg = WitTerm::NamedNode("http://example.org/knows".to_string());
    let result = ext_iface
        .call_call(&mut store, "predicate-triple-count", &[arg])
        .expect("wasm call")
        .expect("predicate-triple-count returned Err");

    match result {
        WitTerm::Literal(l) => {
            assert_eq!(l.value, "4", "unexpected count: {:?}", l.value);
            assert_eq!(
                l.datatype.as_deref(),
                Some("http://www.w3.org/2001/XMLSchema#integer"),
                "expected xsd:integer datatype, got {:?}",
                l.datatype,
            );
            assert!(l.language.is_none());
        }
        other => panic!("expected literal return, got {other:?}"),
    }
}

#[test]
fn predicate_triple_count_name_returns_three() {
    // Sanity-check a different predicate against the same fixture.
    // Three `:name` triples in profile.ttl.
    let (_engine, component, linker, mut store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let arg = WitTerm::NamedNode("http://example.org/name".to_string());
    let result = ext_iface
        .call_call(&mut store, "predicate-triple-count", &[arg])
        .expect("wasm call")
        .expect("predicate-triple-count returned Err");

    match result {
        WitTerm::Literal(l) => assert_eq!(l.value, "3"),
        other => panic!("expected literal, got {other:?}"),
    }
}

#[test]
fn predicate_triple_count_absent_predicate_returns_zero() {
    // Boundary: a predicate absent from the fixture returns 0. Proves
    // the extension does not crash on an empty result.
    let (_engine, component, linker, mut store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let arg = WitTerm::NamedNode(
        "http://example.org/never-heard-of-it".to_string(),
    );
    let result = ext_iface
        .call_call(&mut store, "predicate-triple-count", &[arg])
        .expect("wasm call")
        .expect("predicate-triple-count returned Err");

    match result {
        WitTerm::Literal(l) => assert_eq!(l.value, "0"),
        other => panic!("expected literal, got {other:?}"),
    }
}

#[test]
fn classify_predicate_emits_json_payload() {
    // Multi-query classification. This drives four SPARQL queries
    // through graph-callbacks::execute-query (enum, cardinality, mix,
    // and — for functional single-object predicates — the is_rdf_list
    // probe). The return is a JSON literal carrying the classification
    // fields.
    //
    // We don't over-assert on the exact classification because Oxigraph
    // 0.5's aggregations on toy fixtures behave differently from
    // Stardog (SUM(IF(...)) is not universally supported, MAX over a
    // subselect reduces oddly on empty groups, etc.); the honest
    // assertions are that (a) the callback round-trip completes without
    // errors, (b) the return is an RDF-JSON-typed literal, and (c) the
    // payload parses as JSON and carries the predicate we asked about.
    let (_engine, component, linker, mut store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let arg = WitTerm::NamedNode("http://example.org/name".to_string());
    let result = ext_iface
        .call_call(&mut store, "classify-predicate", &[arg])
        .expect("wasm call")
        .expect("classify-predicate returned Err");

    match result {
        WitTerm::Literal(l) => {
            assert_eq!(
                l.datatype.as_deref(),
                Some("http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON"),
                "expected rdf:JSON datatype, got {:?}",
                l.datatype,
            );
            let payload: serde_json::Value = serde_json::from_str(&l.value)
                .unwrap_or_else(|e| panic!("classify payload not JSON: {e}: {}", l.value));
            assert_eq!(
                payload.get("predicate").and_then(|v| v.as_str()),
                Some("http://example.org/name"),
                "predicate field mismatch: {payload}",
            );
            assert!(
                payload.get("shape").is_some(),
                "shape field missing: {payload}"
            );
            assert!(
                payload.get("cardinality").is_some(),
                "cardinality field missing: {payload}"
            );
            assert!(
                payload.get("triples").is_some(),
                "triples field missing: {payload}"
            );
        }
        other => panic!("expected literal, got {other:?}"),
    }
}

#[test]
fn unknown_function_returns_typed_err() {
    // The extension's `call` dispatcher rejects an unknown function
    // name with an Err(String) rather than crashing.
    let (_engine, component, linker, mut store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let arg = WitTerm::NamedNode("http://example.org/knows".to_string());
    let result = ext_iface
        .call_call(&mut store, "not-a-real-function", &[arg])
        .expect("wasm call");

    match result {
        Ok(term) => panic!("expected Err, got Ok({term:?})"),
        Err(msg) => {
            assert!(
                msg.contains("unknown function"),
                "unexpected error message: {msg}"
            );
        }
    }
}

#[test]
fn non_iri_argument_is_rejected() {
    // Validates the argument-shape guard: a literal argument to
    // predicate-triple-count is rejected before any callback is
    // dispatched.
    let (_engine, component, linker, mut store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let literal = WitTerm::Literal(
        host_callbacks_impl::bindings::tegmentum::webfunction::types::Literal {
            value: "not-an-iri".to_string(),
            datatype: None,
            language: None,
        },
    );
    let result = ext_iface
        .call_call(&mut store, "predicate-triple-count", &[literal])
        .expect("wasm call");
    match result {
        Ok(term) => panic!("expected Err, got Ok({term:?})"),
        Err(msg) => assert!(
            msg.contains("must be an IRI"),
            "unexpected error message: {msg}"
        ),
    }
}
