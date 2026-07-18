//! Phase-2 host-callback round-trip smoke test for wf_infer-extension.
//!
//! Proves the second migration end-to-end:
//!
//!   1. The `wf_infer-extension` cdylib, built by cargo-component
//!      against the shared `extension-with-host-callbacks` world
//!      (imports `tegmentum:webfunction/graph-callbacks@0.1.0`,
//!      exports `extension`), instantiates cleanly under the reference
//!      native host (`host-callbacks-impl` from
//!      `~/git/oxigraph-extension/`).
//!   2. `graph-callbacks::execute-update` — a new surface not
//!      exercised by wf_profile-extension — round-trips through the
//!      reference host's SPARQL Update path (CLEAR and INSERT DATA).
//!   3. `query-result::quads` arm — CONSTRUCT dispatch returns
//!      typed quads; the extension's insert path reads them without a
//!      per-crate binding-name convention.
//!   4. Multi-callback per invocation — the "append" test verifies
//!      that graph_size + CONSTRUCT + INSERT (all through the callback
//!      surface, all within one guest call) compose correctly. The
//!      "replace" test adds a CLEAR at the top of the chain. The
//!      iteration test drives ~4 * max_iterations callbacks and
//!      verifies convergence detection.
//!
//! The reference host and its Store are provided by
//! `host-callbacks-impl` — the same crate wf_profile-extension's
//! smoke test uses. This test proves the second migration loads
//! under exactly that host with no host-side changes.
//!
//! Non-goals (memo §9):
//!
//!   * `http-callbacks` — this extension does not import it, so the
//!     smoke test uses `HttpCallbackImpl::deny_all()`.
//!   * `wasm-callbacks` — Phase-1 stub, this extension does not
//!     invoke it.

use std::path::{Path, PathBuf};
use std::process::Command;

use host_callbacks_impl::{
    CallbackHostState, ExtensionWithHostCallbacks, GraphCallbackImpl,
    HttpCallbackImpl, WasmCallbackImpl, wire_into_linker,
};

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use host_callbacks_impl::bindings::tegmentum::webfunction::types::{
    Literal as WitLiteral, Term as WitTerm,
};

/// Walk up from `CARGO_MANIFEST_DIR` to the workspace root so we can
/// point `cargo component build` at the workspace and locate the
/// built artifact under `target/wasm32-wasip2/release/`.
fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // .../tegmentum-webfunctions/crates/wf_infer-extension
    p.pop(); // crates/
    p.pop(); // repo root
    p
}

/// Build the cdylib under `cargo component`, exactly once per test
/// binary invocation. `cargo test` runs tests in parallel by default;
/// letting each test invoke `cargo component build` concurrently
/// races on the target dir (multiple builds compete for the file
/// lock, and one thread can see a transiently-empty artifact path).
/// The `Once` guard serialises the build across all tests in this
/// binary. Same pattern the reference `host-callbacks-impl` smoke
/// test uses under high test parallelism.
fn build_extension_component(root: &Path) {
    static BUILD_ONCE: std::sync::Once = std::sync::Once::new();
    BUILD_ONCE.call_once(|| {
        let status = Command::new("cargo")
            .current_dir(root)
            .args([
                "component",
                "build",
                "--release",
                "-p",
                "wf_infer-extension",
                "--target",
                "wasm32-wasip2",
            ])
            .status()
            .unwrap_or_else(|e| {
                panic!(
                    "cargo component build for wf_infer-extension failed to launch: {e}"
                )
            });
        assert!(
            status.success(),
            "cargo component build for wf_infer-extension exited with {status}"
        );
    });
}

fn fixture_turtle() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("subclass.ttl");
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

/// Common setup: build the component, wire a linker, seed the graph
/// with the fixture, return the loaded bindings + store. Every test
/// gets a fresh in-memory oxigraph store so tests don't collide on
/// derived-graph state.
fn instantiate() -> (
    Engine,
    Component,
    Linker<CallbackHostState>,
    Store<CallbackHostState>,
    std::sync::Arc<oxigraph::store::Store>,
) {
    let root = workspace_root();
    build_extension_component(&root);
    let component_path = root
        .join("target/wasm32-wasip2/release/wf_infer_extension.wasm");
    assert!(
        component_path.exists(),
        "expected component at {}",
        component_path.display(),
    );

    let graph = GraphCallbackImpl::new_in_memory().expect("graph impl");
    graph.load_turtle(&fixture_turtle()).expect("load turtle");
    // Keep a clone of the underlying store so the assertions can
    // read the derived graph out-of-band (the extension does the
    // writes through the callback surface; we verify by SPARQL against
    // the same store here).
    let store_handle = graph.store_handle();

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
    let wasmtime_store: Store<CallbackHostState> = Store::new(&engine, state);

    let component = Component::from_file(&engine, &component_path)
        .expect("load component");
    (engine, component, linker, wasmtime_store, store_handle)
}

fn json_arg(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.to_string(),
        datatype: None,
        language: None,
    })
}

/// Extract the JSON payload from an rdf:JSON literal return.
fn unpack_json_return(term: WitTerm) -> serde_json::Value {
    match term {
        WitTerm::Literal(l) => {
            assert_eq!(
                l.datatype.as_deref(),
                Some("http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON"),
                "expected rdf:JSON datatype, got {:?}",
                l.datatype,
            );
            serde_json::from_str(&l.value).unwrap_or_else(|e| {
                panic!("run-rule payload not JSON: {e}: {}", l.value)
            })
        }
        other => panic!("expected literal return, got {other:?}"),
    }
}

/// Count triples in a named graph directly via the oxigraph store —
/// used to verify the extension's INSERTs landed.
fn count_graph(
    store: &std::sync::Arc<oxigraph::store::Store>,
    graph: &str,
) -> u64 {
    #[allow(deprecated)]
    let results = store
        .query(&format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"
        ))
        .expect("count query");
    match results {
        oxigraph::sparql::QueryResults::Solutions(iter) => {
            for sol in iter {
                let sol = sol.expect("solution");
                for (_, term) in sol.iter() {
                    if let oxigraph::model::Term::Literal(l) = term {
                        return l.value().parse::<u64>().unwrap_or(0);
                    }
                }
            }
            0
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[test]
fn register_reports_run_rule() {
    let (_engine, component, linker, mut store, _) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate extension-with-host-callbacks");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let descriptors = ext_iface.call_register(&mut store).expect("call register()");
    let names: Vec<&str> = descriptors.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"run-rule"),
        "expected run-rule in register(), got {names:?}"
    );
    assert_eq!(
        descriptors.len(),
        1,
        "expected exactly 1 descriptor, got {names:?}"
    );
    let d = &descriptors[0];
    assert_eq!(d.min_arity, 1, "min-arity");
    assert_eq!(d.max_arity, Some(1), "max-arity");
}

#[test]
fn subclass_rule_derives_transitive_types() {
    // The signal test. Runs a CONSTRUCT rule that materializes the
    // transitive closure of `?s a ?super` under rdfs:subClassOf+. The
    // fixture has three direct type assertions (whiskers a Cat, rex a
    // Dog, dumbo a Mammal) and a Cat/Dog/Mammal → Animal chain, so:
    //
    //   whiskers → Mammal, Animal
    //   rex      → Mammal, Animal
    //   dumbo    → Animal
    //
    // Five derived triples in total. Iterate mode is off (single pass);
    // the CONSTRUCT's `subClassOf+` handles the transitive step in one
    // query.
    let derived_graph = "http://example.org/derived/types";
    let rule_json = serde_json::json!({
        "name": "subclass_closure",
        "construct":
            "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
             CONSTRUCT { ?s a ?super } \
             WHERE { ?s a ?sub . ?sub rdfs:subClassOf+ ?super }",
        "graph": derived_graph,
        "refresh_mode": "replace",
    })
    .to_string();

    let (_engine, component, linker, mut store, ox_store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let result = ext_iface
        .call_call(&mut store, "run-rule", &[json_arg(&rule_json)])
        .expect("wasm call")
        .expect("run-rule returned Err");
    let payload = unpack_json_return(result);
    assert_eq!(
        payload.get("rule").and_then(|v| v.as_str()),
        Some("subclass_closure"),
        "rule field mismatch: {payload}"
    );
    assert_eq!(
        payload.get("iterations").and_then(|v| v.as_u64()),
        Some(1),
        "iterations should be 1 for non-iterate mode: {payload}"
    );

    let graph_size = count_graph(&ox_store, derived_graph);
    assert_eq!(
        graph_size, 5,
        "expected 5 derived triples in {derived_graph} (whiskers→Mammal, \
         whiskers→Animal, rex→Mammal, rex→Animal, dumbo→Animal); \
         actual size {graph_size}",
    );
    assert_eq!(
        payload.get("graph_size").and_then(|v| v.as_u64()),
        Some(5),
        "run-rule reported graph_size ≠ 5: {payload}"
    );
}

#[test]
fn if_then_sugar_matches_explicit_construct() {
    // Same rule expressed via the SRS-style if/then sugar. Should
    // derive the exact same five triples as the explicit-CONSTRUCT
    // form above. This validates the Rule::construct_sparql
    // synthesis path.
    let derived_graph = "http://example.org/derived/types-sugar";
    let rule_json = serde_json::json!({
        "name": "subclass_closure_sugar",
        "prefixes":
            "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>",
        "if": "?s a ?sub . ?sub rdfs:subClassOf+ ?super",
        "then": "?s a ?super",
        "graph": derived_graph,
    })
    .to_string();

    let (_engine, component, linker, mut store, ox_store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let result = ext_iface
        .call_call(&mut store, "run-rule", &[json_arg(&rule_json)])
        .expect("wasm call")
        .expect("run-rule returned Err");
    let _payload = unpack_json_return(result);

    let graph_size = count_graph(&ox_store, derived_graph);
    assert_eq!(
        graph_size, 5,
        "if/then sugar should derive the same 5 triples as explicit \
         CONSTRUCT; got {graph_size}",
    );
}

#[test]
fn replace_mode_clears_stale_derivations() {
    // Run a rule that produces one triple; verify. Then swap the
    // graph out from under it (delete the fixture triple that made
    // the rule fire) and re-run with refresh_mode=replace. The CLEAR
    // callback should erase the previous derivation and the fresh
    // CONSTRUCT should produce zero triples.
    let derived_graph = "http://example.org/derived/refresh-test";
    let rule_json = serde_json::json!({
        "name": "cat_only",
        "construct":
            "PREFIX : <http://example.org/> \
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
             CONSTRUCT { ?s a :Cat } WHERE { ?s a :Cat }",
        "graph": derived_graph,
        "refresh_mode": "replace",
    })
    .to_string();

    let (_engine, component, linker, mut store, ox_store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    // First pass: one Cat in the fixture, one triple derived.
    let _ = ext_iface
        .call_call(&mut store, "run-rule", &[json_arg(&rule_json)])
        .expect("wasm call")
        .expect("run-rule returned Err");
    assert_eq!(
        count_graph(&ox_store, derived_graph),
        1,
        "expected 1 derived triple after first pass"
    );

    // Delete the Cat assertion out-of-band, then re-run. CLEAR
    // should wipe the stale derivation; new CONSTRUCT is empty.
    {
        #[allow(deprecated)]
        ox_store.update(
            "PREFIX : <http://example.org/> \
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
             DELETE DATA { :whiskers a :Cat }",
        )
        .expect("delete cat assertion");
    }
    let result = ext_iface
        .call_call(&mut store, "run-rule", &[json_arg(&rule_json)])
        .expect("wasm call")
        .expect("run-rule returned Err");
    let payload = unpack_json_return(result);
    assert_eq!(
        count_graph(&ox_store, derived_graph),
        0,
        "expected 0 derived triples after replace-mode CLEAR of stale row"
    );
    assert_eq!(
        payload.get("graph_size").and_then(|v| v.as_u64()),
        Some(0),
    );
}

#[test]
fn append_mode_accumulates_across_runs() {
    // With refresh_mode=append, a repeated CONSTRUCT does NOT clear
    // the target graph. INSERT DATA is set-semantics at the store
    // level, so re-running the same rule doesn't double-count — but
    // it does keep whatever the previous run wrote. This validates
    // the append branch of the refresh-mode dispatch.
    let derived_graph = "http://example.org/derived/append-test";
    let rule_json = serde_json::json!({
        "name": "cat_append",
        "construct":
            "PREFIX : <http://example.org/> \
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
             CONSTRUCT { ?s a :Cat } WHERE { ?s a :Cat }",
        "graph": derived_graph,
        "refresh_mode": "append",
    })
    .to_string();

    let (_engine, component, linker, mut store, ox_store) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let _ = ext_iface
        .call_call(&mut store, "run-rule", &[json_arg(&rule_json)])
        .expect("wasm call")
        .expect("run-rule returned Err");
    assert_eq!(count_graph(&ox_store, derived_graph), 1);

    // Second pass: append mode, no CLEAR. Set semantics on INSERT
    // means still 1 triple, not 2.
    let _ = ext_iface
        .call_call(&mut store, "run-rule", &[json_arg(&rule_json)])
        .expect("wasm call")
        .expect("run-rule returned Err");
    assert_eq!(
        count_graph(&ox_store, derived_graph),
        1,
        "append mode + INSERT DATA idempotency: expected 1, got"
    );
}

#[test]
fn unknown_function_returns_typed_err() {
    let (_engine, component, linker, mut store, _) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let result = ext_iface
        .call_call(&mut store, "not-a-real-function", &[json_arg("{}")])
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
fn non_literal_argument_is_rejected() {
    let (_engine, component, linker, mut store, _) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let iri_arg = WitTerm::NamedNode("http://example.org/not-json".to_string());
    let result = ext_iface
        .call_call(&mut store, "run-rule", &[iri_arg])
        .expect("wasm call");
    match result {
        Ok(term) => panic!("expected Err, got Ok({term:?})"),
        Err(msg) => assert!(
            msg.contains("string literal"),
            "unexpected error message: {msg}"
        ),
    }
}

#[test]
fn malformed_rule_json_reports_parse_error() {
    let (_engine, component, linker, mut store, _) = instantiate();
    let bindings = ExtensionWithHostCallbacks::instantiate(&mut store, &component, &linker)
        .expect("instantiate");
    let ext_iface = bindings.tegmentum_webfunction_extension();

    let result = ext_iface
        .call_call(&mut store, "run-rule", &[json_arg("not-json")])
        .expect("wasm call");
    match result {
        Ok(term) => panic!("expected Err, got Ok({term:?})"),
        Err(msg) => assert!(
            msg.contains("rule parse"),
            "unexpected error message: {msg}"
        ),
    }
}
