# tegmentum-webfunctions

WebAssembly Component Model library of reusable SPARQL procedures. Each
crate builds to a single `.wasm` binary callable via `wf:call` from any
triplestore with the tegmentum plugins installed (Stardog, Jena, RDF4J).

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

Every crate copies the same WIT world from `shared/wit/webfunction.wit`.
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

## Publishing as a wasm-in-database bundle

The long game (see the tegmentum roadmap): publish the whole suite as a
single Turtle file where each function is an RDF resource keyed by URL
with the wasm bytes attached. A consumer runs `LOAD` and the whole
library is available in their triplestore, no admin required.

That's what turns this repo from "grab a `.wasm`" into "the PostGIS
install of a SPARQL-native world."

## License

Apache-2.0.
