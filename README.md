# webfunctions

Canonical repository of WebAssembly Component Model SPARQL extension
components. Each crate builds to a single `.wasm` binary callable
from any triplestore that speaks the substrate contract — via
`wf:call` under the legacy `stardog:webfunction@0.5.x` plugin path,
or as a native SPARQL custom function / aggregate / property function
under the `tegmentum:webfunction@0.1.0` engine world (see
`~/git/webfunction-wit/`).

Not tied to a single organizational identity — the `tegmentum` in
the WIT namespace stays as an identifier for stability, but this
repository is the canonical extension-components home for the
ecosystem.

Two flavours of crate live here:

1. **Ports** from the semantalytics/stardog-webfunctions suite, converted
   from the old module-mode C-string ABI to the WebAssembly Component
   Model. See "Migration recipe" below.
2. **New** capability that never had a home in the old suite —
   principally the XSPARQL-shape data-interop primitives (`parse_json`,
   `parse_csv`, and friends yet to come).

## Current inventory (126 crates)

Each crate is buildable via `cargo component build --release` and yields
a wasm component at
`target/wasm32-wasip1/release/<crate_name>.wasm`. Average size ≈ 106 KB.

Ports (module-mode → Component Model):

| Prefix | Count | Origin | Notes |
|---|---|---|---|
| `math_*` | 30 | function_math/ | trig, exp/log, arithmetic, stats (mean, median, stddev, covariance, pearson_r), variadic (min/max/sum_arrays). The upstream sources were all identical broken templates — the algorithms in these ports were written from scratch. |
| `math_const_*` | 19 | function_math_constants/ | π, e, τ, √2, ln 2, log₂ 10, φ (from f64::consts), etc. |
| `string_*` | 12 | function_string/ | count / count_graphemes / count_substrings / count_words / count_unique_words / count_where / split_chars / swap_case / title_case / train_case / upper / upper_first |
| `string_case_*` | 13 | function_string_case/ | camel, snake, kebab, pascal, shouty_snake / shouty_kebab, capitalize / decapitalize, lower / lower_first, swap / title / train / upper_first |
| `hash_*` | 21 | function_crypto_hash/ | blake2b / blake2b_256 / blake2b_512 / fsb / gost94 / groestl / k12 / md2 / md4 / md5 / ripemd160 / sha1 / sha256 / sha3_256 / sha384 / sha512 / shabal / sm3 / streebog / tiger / whirlpool |
| `similarity_*` | 8 | function_string_similarity/ | levenshtein / damerau_levenshtein / normalized_levenshtein / normalized_damerau / hamming / jaro / jaro_winkler / osa_distance / sorensen_dice |
| `array_*` | 11 | function_array/ | append / contains / dedupe / equals / fill / get / index / of / reverse / size / unique |
| `base64_*` | 1 | function_base64/ | decode |
| `jmespath_*` | 1 | function_json_jmespath/ | search — JMESPath over JSON |
| `webassembly_*` | 1 | function_webassembly/ | wat — inspect the plugin's Wasm engine version |
| `agg_*` | 1 | aggregate/ | sum — canonical example of aggregate-step / aggregate-finish |

New (XSPARQL-shape data interop, not ports):

| Crate | Purpose |
|---|---|
| `parse_json` | Take a JSON string, return binding-sets. Objects → single row keyed by field; arrays-of-objects → one row per element; scalars typed. Nested values stringified for recursive parse_json. |
| `parse_csv` | Take a CSV string (optional delimiter), return binding-sets. First row is the header, subsequent rows are one Binding per column. |
| `parse_xml` | Take an XML string, return binding-sets. Attributes become columns; direct text goes to `text`; child elements grouped by tag, XML-serialized for recursive parse_xml. |
| `json_path` | Given a JSON string plus a JSONPath expression, return the matched values as rows. |
| `emit_json` | Aggregate-shaped: consume rows via aggregate-step (name-value pairs), emit a JSON string via aggregate-finish. |
| `emit_csv` | Aggregate-shaped mirror of emit_json for CSV. |

Skipped and why:

- **function_python** — a Python interpreter can't run in a wasm
  component. Consider a `pyodide.wasm`-shaped bridge later if needed.
- **function_image**, **function_object_detection**, **function_ocr**,
  **function_nlp** — depend on native / large-model deps that either
  don't compile to wasm32-wasip1 or would bloat the .wasm by 10-100 MB.
  Defer per-component until needed.
- **function_bio**, **function_bio_alphabet** — the upstream 60 crates
  were all empty templates copied from function_math, not real bio
  implementations. Bio work is happening separately in
  [scry-webfunctions-demo](https://github.com/tegmentum/scry-webfunctions-demo)
  (blastp, protparam) — those will move here eventually as `bio_*`.
- **aggregate_stats**, **aggregate_stats_distribution** — empty upstream.
  agg_sum is the canonical aggregate example; stats aggregators will
  follow the same pattern once needed.

## Deferred / to-port

The upstream suite has ~200 additional functions across math, strings,
crypto, bio, image, NLP, arrays, and JSON that haven't been migrated
yet. See `~/git/stardog-webfunctions/` for the source. Rough grouping:

- **function_math** — 30 (sin, cos, log, mean, median, stddev, …)
- **function_math_constants** — 19 (π, e, √2, φ, …)
- **function_string** — 16 (count, split, …)
- **function_string_case** — 15 (camel, snake, kebab, pascal, …)
- **function_string_similarity** — 9 (jaro, jaro-winkler, hamming, …)
- **function_string_lang** — 9 (language detection, …)
- **function_array** — 12 (min, max, distinct, …)
- **function_crypto_hash** — 19 (blake2, md5, sha1/2/3, ripemd, …)
- **function_bio** — 30 (contents pending — sequence utilities)
- **function_nlp**, **function_ocr**, **function_image**,
  **function_object_detection** — larger dependencies; port when needed

## Migration recipe (old ABI → Component Model)

Every crate in `~/git/stardog-webfunctions/` follows a template. Porting
one to the new ABI is mechanical:

### Cargo.toml

**Old:**
```toml
[dependencies]
serde = { version = "1.0", features = [ "derive" ] }
serde_json = { version = "1.0" }
stardog_function = { git = "https://github.com/semantalytics/stardog-webfunctions" }
```

**New:**
```toml
[dependencies]
wit-bindgen.workspace = true

[package.metadata.component]
package = "tegmentum:<name>"

[package.metadata.component.target]
path = "wit"
world = "webfunction"
```

Domain-specific deps (strsim, sha2, csv, serde_json for JSON parsing)
stay. The `stardog_function` shim is dropped entirely.

### src/lib.rs

**Old:**
```rust
#[no_mangle]
pub extern "C" fn evaluate(arg: *mut c_char) -> *mut c_char {
    let args_str = unsafe { CStr::from_ptr(arg).to_str().unwrap() };
    let values: Value = serde_json::from_str(args_str).unwrap();
    let x = values["results"]["bindings"][0]["value_0"]["value"].as_str().unwrap();
    let result = /* ... compute ... */;
    let json = json!({"head":{"vars":["result"]},"results":{"bindings":[{"result":{"type":"literal","value":result}}]}}).to_string();
    unsafe { CString::from_vec_unchecked(json.into_bytes()) }.into_raw()
}
```

**New:**
```rust
wit_bindgen::generate!({ world: "webfunction", path: "wit" });
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let x = match &args[0] { Value::Literal(l) => &l.label, _ => return Err("...".into()) };
        let result = /* ... compute ... */;
        Ok(BindingSets {
            vars: vec!["result".into()],
            rows: vec![vec![Binding {
                name: "result".into(),
                value: Value::Literal(Literal {
                    label: result.to_string(),
                    datatype: "http://www.w3.org/2001/XMLSchema#decimal".into(),
                    lang: None,
                }),
            }]],
        })
    }
    fn aggregate_step(_: Vec<Value>, _: u64) -> Result<(), String> { Err("N/A".into()) }
    fn aggregate_finish() -> Result<BindingSets, String> { Err("N/A".into()) }
    fn cardinality_estimate(_: Cardinality, _: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }
    fn doc() -> BindingSets { /* … */ }
}

export!(Component);
```

The key wins:
- No manual JSON serialization of the SPARQL result envelope.
- No raw pointers, no `CString::from_vec_unchecked`, no `unwrap()`s in
  the ABI boundary — `wit-bindgen` handles it.
- Real return types with real error messages (`Result<_, String>`).
- Value types are typed: `iri`, `literal { label, datatype, lang }`,
  `bnode`. No more `value_0` positional keying.

### wit/webfunction.wit

Each legacy `stardog:webfunction@0.5.x` crate carries its own copy of
the WIT world in its `wit/` directory (self-contained per crate).
Newer extension crates that target the substrate WIT
(`tegmentum:webfunction@0.1.0`) point at the webfunction-wit
submodule at repo-root `wit/`.
This is the contract every plugin talks to. Keep it byte-identical
across crates or components won't be portable.

## Building

Each crate individually:
```bash
cd crates/<name>
cargo component build --release
```

Or all at once from the workspace root:
```bash
for c in crates/*; do (cd "$c" && cargo component build --release); done
```

Wasm outputs land in `target/wasm32-wasip1/release/<name>.wasm`.

## Migrated components (`*-extension` crates)

Sibling crates that re-expose primitives from the Stardog-era `wf_*`
family on the standardized `tegmentum:webfunction@0.1.0` package
declared in `~/git/oxigraph-webfunction-plugin/wit/`. Each sibling is a
`cdylib` targeting `wasm32-wasip2`, loads under any host implementing
the standardized extension world, and does not modify the original
`wf_*` crate. See `~/git/wf-conformance/docs/design/wf-host-callbacks.md`
§7 for the migration path.

| Sibling | Upstream contract | Imported callbacks | Original crate | Callback surface validated |
|---|---|---|---|---|
| `wf_skolemize-extension` | `tegmentum:webfunction/extension@0.1.0` | (none — pure filter) | `wf_skolemize` | scalar filter export only |
| `wf_profile-extension` | `tegmentum:webfunction/extension-with-host-callbacks@0.1.0` | `tegmentum:webfunction/graph-callbacks@0.1.0` | `wf_profile` | `execute-query` (Bindings arm) |
| `wf_infer-extension` | `tegmentum:webfunction/extension-with-host-callbacks@0.1.0` | `tegmentum:webfunction/graph-callbacks@0.1.0` | `wf_infer` | `execute-query` (Quads arm from CONSTRUCT) + `execute-update` (CLEAR + INSERT DATA); multi-callback per invocation |

The `-extension` siblings target the shared world under
`tegmentum:webfunction@0.1.0`; the originals keep their per-crate
`stardog:webfunction@0.5.0` (or peer) worlds intact for existing
Stardog-plugin consumers.

Between them the three migrated siblings exercise the full Phase-1
`graph-callbacks` surface (both `execute-query` result arms plus
`execute-update`) plus reentrancy under the reference
`host-callbacks-impl` — the iteration path in `wf_infer-extension`
issues on the order of `4 * max_iterations` callbacks per invocation,
proving the callback boundary tolerates high-frequency dispatch
against a live oxigraph store.

Build one sibling and run its round-trip:
```bash
cargo component build --release -p wf_profile-extension --target wasm32-wasip2
cargo test -p wf_profile-extension --test smoke

cargo component build --release -p wf_infer-extension --target wasm32-wasip2
cargo test -p wf_infer-extension --test smoke
```

The smoke test uses the reference `host-callbacks-impl` from
`~/git/oxigraph-webfunction-plugin/crates/host-callbacks-impl/` (path
dev-dep) to prove the migrated component loads against the shared
world's host side. Downstream hosts implementing the same three
`Host` traits load the migrated component with no adapter layer.

## Overlay-crate migration status

The 22-crate overlay wave (Follow-up E + F + M1) moves each Stardog-era
`stardog:webfunction@0.3.x`–`0.6.x` crate onto the substrate
contract at `tegmentum:webfunction@0.1.0` (submodule `wit/`).

Progress: 22 / 22 overlay crates migrated, 1 retired (`wf_sql`).
`wf_sql` is retired as of Follow-up F — the R2
`sink-query-callbacks::execute-sink-select` surface subsumes its
"arbitrary SQL against a sink" role, so shipping a wf_sql analogue
on the substrate would be dead weight. See
`docs/wf_sql-retirement.md`. `wf_fetch` is redesigned in the same
wave as an HTTP-fetch + `emit-quads` guest (still a substrate
crate — the "retired" label is on its old descriptor / sql-tail
contract, not on the crate itself).

| Wave / batch | Crates | World | Callbacks used |
|---|---|---|---|
| Follow-up E batch1 (`4252647`) | `debug_execute_update`, `vega_bar_chart`, `wf_validate` | `extension-with-host-callbacks` | `graph-callbacks` |
| Follow-up E batch2 (`0104cda`) | `wf_infer`, `wf_profile`, `wf_skolemize` | `extension-with-host-callbacks` | `graph-callbacks` |
| Follow-up F batch3 (`7ec4eda`) | `debug_callback_depth`, `wf_tree_fast` | `extension-with-all-host-callbacks` | `observability-callbacks` (+ `graph-callbacks` for wf_tree_fast) |
| Follow-up F batch4 (`69584ce`) | `adjacency_tree`, `wf_tree`, `wf_tree_rows` | `extension-with-all-host-callbacks` | `prepared-query-callbacks` + `observability-callbacks` |
| Follow-up F batch5 (`f04fd51`) | `wf_apply`, `wf_map`, `wf_pipeline` | `extension-with-all-host-callbacks` | `wasm-callbacks` + `graph-callbacks` |
| Follow-up F batch6 (`3beeb6a`) | `wf_materialize`, `wf_materialize_list` | `extension-with-all-host-callbacks` | `sink-callbacks` (write-only) + `graph-callbacks` (+ `prepared-query-callbacks` for the list variant) |
| M1 Q3 batch7 | `wf_demote`, `wf_demote_tree`, `wf_materialize_tree` | `extension-with-all-host-callbacks` | `sink-query-callbacks::scan-sink-quads` (demote) + `document-sink-callbacks::put-document` (demote_tree, materialize_tree) + `graph-callbacks` |
| M1 Q2 wf_fetch | `wf_fetch` | `extension-with-all-host-callbacks` | `http-callbacks::http-get` + `sink-callbacks::emit-quads` — HTTP GET + Turtle/N-Triples/N-Quads parse + batched emit (see `feat(wf_fetch)` commit) |
| M1 X4 wf_canonicalize | `wf_canonicalize` | `extension-with-all-host-callbacks` | `tracker-sink-callbacks` (alias-map + fulltext-sweep + document-sweep scratch tables) + `graph-callbacks` + `http-callbacks::http-post-json` (Manticore admin + Sirix SQL) |

### Retired

| Crate | Reason retired |
|---|---|
| `wf_sql` | Subsumed by `sink-query-callbacks::execute-sink-select` (R2 sink-read landing). The whole point of `wf_sql` was "arbitrary SQL against a sink returned as binding-sets" — the substrate now exposes that shape as a first-class callback, so a guest crate that re-wraps it is dead weight. See `docs/wf_sql-retirement.md`. |

These crates continue to build against their per-crate legacy
`stardog:webfunction@0.5.0/host` world and are unchanged by the
overlay migration.

## Publishing as a wasm-in-database bundle

The long game (see the tegmentum roadmap): publish the whole suite as a
single Turtle file where each function is an RDF resource keyed by URL
with the wasm bytes attached. A consumer runs `LOAD` and the whole
library is available in their triplestore, no admin required.

That's what turns this repo from "grab a `.wasm`" into "the PostGIS
install of a SPARQL-native world."

## License

Apache-2.0.
