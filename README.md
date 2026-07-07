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

## Current inventory

Each crate is buildable via `cargo component build --release` and yields
a wasm component at
`target/wasm32-wasip1/release/<crate_name>.wasm`.

| Crate | Origin | Function |
|---|---|---|
| `math_sqrt` | port of `function_math/sqrt` | `sqrt(x)` |
| `string_upper` | port of `function_string_case/to_upper` | Unicode uppercase |
| `string_levenshtein` | port of `function_string_similarity/levenshtein` | edit distance via strsim |
| `hash_sha256` | port of `function_crypto_hash/sha2` | hex-encoded SHA-256 |
| `parse_json` | new (XSPARQL interop) | JSON → binding-sets |
| `parse_csv` | new (XSPARQL interop) | CSV → binding-sets |

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
