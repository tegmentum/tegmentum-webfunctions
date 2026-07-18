//! Phase-1 SPARQL extension re-exposing the wf_skolemize primitive
//! as a scalar filter function.
//!
//! Exports one function:
//!
//!   `<urn:webfunction:skolemize>(?bnode) -> IRI`
//!
//! The argument must be a blank node; the return is a deterministic
//! well-known-genid IRI derived from a stable 128-bit hash of the
//! bnode's SPARQL-visible label. Two calls with the same input yield
//! the same output; two calls with different inputs yield different
//! outputs modulo an astronomical collision probability.
//!
//! Non-bnode arguments are rejected — SPARQL semantics for the caller
//! are that the function raises an evaluation error at that call
//! site, which the oxigraph-extension host surfaces as `None`.
//!
//! # Provenance of the algorithm
//!
//! The mint_genid function, IRI prefix, and salt are duplicated from
//! `~/git/tegmentum-webfunctions/crates/wf_skolemize/src/lib.rs`
//! (see mint_genid there). Duplication rather than a cross-crate
//! import keeps the original wf_skolemize crate unmodified while the
//! Phase-1 extension WIT surface stabilizes. Both crates converge
//! on identical output for the same bnode label — verified by
//! inspection; the ~15 lines have no dependencies to drift against.

#[allow(warnings)]
mod bindings;

use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::tegmentum::webfunction::types::Term as WitTerm;

struct Component;

/// IRI prefix for minted canonicals. RDF 1.1 recommends
/// `.well-known/genid/` as the URI-space for skolem constants; the
/// tegmentum.ai host keeps them addressable while remaining
/// substitute-friendly for consumers that re-anonymize the subtree.
const GENID_PREFIX: &str = "https://tegmentum.ai/.well-known/genid/";

/// Salt for the 128-bit hash mixer. Fractional part of the golden
/// ratio — same constant wf_skolemize uses so both crates hash
/// identically for the same bnode label.
const SALT: u64 = 0x9E3779B97F4A7C15;

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "skolemize".to_string(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "skolemize" => skolemize(&args),
            other => Err(format!(
                "wf_skolemize-extension: unknown function '{other}'"
            )),
        }
    }
}

/// `skolemize(_:x)` — mint the well-known-genid IRI for the given
/// blank node. Arity is enforced by the descriptor and re-verified
/// here to keep the error path honest.
fn skolemize(args: &[WitTerm]) -> Result<WitTerm, String> {
    let [arg] = args else {
        return Err(format!(
            "skolemize: expected 1 argument, got {}",
            args.len()
        ));
    };
    let label = match arg {
        WitTerm::BlankNode(label) => label,
        WitTerm::NamedNode(_) => {
            return Err(
                "skolemize: argument must be a blank node, got IRI".into(),
            );
        }
        WitTerm::Literal(_) => {
            return Err(
                "skolemize: argument must be a blank node, got literal".into(),
            );
        }
    };
    Ok(WitTerm::NamedNode(mint_genid(label)))
}

/// Deterministic per-label genid IRI. Salt-hashed so a fresh session
/// on data that reused bnode labels from a prior session doesn't
/// collide. Not cryptographic — collisions are catastrophic but
/// astronomically unlikely at 128 bits.
///
/// Algorithm mirror of wf_skolemize::mint_genid. Kept byte-for-byte
/// identical so `skolemize(_:x)` in a SPARQL filter and the batch
/// rewrite done by `wf:call(<wf_skolemize.wasm>)` produce the same
/// canonical IRI for the same bnode label.
fn mint_genid(label: &str) -> String {
    let mut hash: u64 = SALT;
    for byte in label.bytes() {
        hash = hash.wrapping_mul(0x100000001B3).wrapping_add(byte as u64);
    }
    // Second half of the hash from a fresh accumulator seeded with
    // the first — gives us 128 bits of collision domain from a
    // 64-bit state.
    let mut hash2: u64 = hash.rotate_left(23) ^ 0x428A2F98D728AE22;
    for byte in label.bytes() {
        hash2 = hash2.wrapping_mul(0x100000001B3).wrapping_add(byte as u64);
    }
    format!("{GENID_PREFIX}{hash:016x}{hash2:016x}")
}

bindings::export!(Component with_types_in bindings);
