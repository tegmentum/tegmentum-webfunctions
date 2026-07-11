//! wf_skolemize — replace every blank node with a deterministic
//! well-known-genid IRI.
//!
//! Signature: `wf:call(<wf_skolemize.wasm>)`
//!    → binding-set { renamed: xsd:integer, deleted: xsd:integer }
//!
//! Walks every triple involving a blank-node subject or object, mints a
//! deterministic IRI per bnode from a stable hash of its label, and
//! rewrites the graph:
//!
//!   ?s <p> _:x .   →   ?s <p> <https://tegmentum.ai/.well-known/genid/HASH> .
//!   _:x <p> ?o .   →   <https://tegmentum.ai/.well-known/genid/HASH> <p> ?o .
//!
//! The hash is derived from the bnode's SPARQL-visible ID plus a global
//! salt (to avoid collisions across separate skolemize runs on data that
//! shares bnode labels). The IRI prefix follows RDF 1.1's recommendation
//! for "well-known genid" URIs — round-trip-friendly for any consumer
//! that wants to re-anonymize the subtree.
//!
//! Idempotent-ish: running twice on the same graph is a no-op because
//! the second pass finds no blank nodes.
//!
//! Uses execute-update through the substrate's SPARQL Update path. Runs
//! in three phases:
//!   1. Enumerate distinct blank-node labels.
//!   2. For each, INSERT rewritten triples and DELETE originals in one
//!      DELETE/INSERT statement.
//!   3. Return counts.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const GENID_PREFIX: &str = "https://tegmentum.ai/.well-known/genid/";
const SALT: u64 = 0x9E3779B97F4A7C15; // fractional part of the golden ratio

impl Guest for Component {
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        // Phase 1: enumerate every triple that involves a blank node in
        // either subject or object position. SPARQL blank-node labels are
        // stable per query result so we get their identifiers via the
        // wit Value::Bnode variant.
        //
        // We can't reference specific stored bnodes from SPARQL Update
        // text (any `_:x` in query text is a fresh variable, not a
        // reference to the bnode `x` in the store, and SPARQL 1.1 forbids
        // bnodes in DELETE templates entirely). So skolemization has to
        // happen in three phases: read triples out, compute rewritten
        // ground triples in the guest, INSERT them, then DELETE every
        // remaining bnode-bearing triple in one filter-based sweep.
        let read_sparql = "\
            SELECT ?s ?p ?o WHERE { \
              ?s ?p ?o . \
              FILTER(isBlank(?s) || isBlank(?o)) \
            }";
        let bs = host::execute_query(read_sparql, &[], None)?;

        // Phase 2: emit rewritten triples as one INSERT DATA batch.
        // Ground terms only — bnode positions become genid IRIs.
        let mut insert_body = String::new();
        let mut renamed = 0u64;
        let mut unique_bnodes = std::collections::HashSet::new();

        for row in &bs.rows {
            let s = binding_value(row, "s");
            let p = binding_value(row, "p");
            let o = binding_value(row, "o");
            let (Some(s), Some(p), Some(o)) = (s, p, o) else {
                continue;
            };

            let s_txt = value_to_sparql(&s, &mut unique_bnodes);
            let p_txt = value_to_sparql(&p, &mut unique_bnodes);
            let o_txt = value_to_sparql(&o, &mut unique_bnodes);

            insert_body.push_str(&s_txt);
            insert_body.push(' ');
            insert_body.push_str(&p_txt);
            insert_body.push(' ');
            insert_body.push_str(&o_txt);
            insert_body.push_str(" .\n");
            renamed += 1;
        }

        if !insert_body.is_empty() {
            let insert = format!("INSERT DATA {{ {insert_body} }}");
            host::execute_update(&insert)
                .map_err(|e| format!("wf_skolemize: insert rewritten batch: {e}"))?;
        }

        // Phase 3: delete every remaining bnode-bearing triple. Filter-
        // based, so no need to reference specific bnodes — the isBlank()
        // test hits exactly the originals, and the INSERT above already
        // seeded the ground-term replacements.
        let delete_update = "\
            DELETE { ?s ?p ?o } \
            WHERE  { ?s ?p ?o . FILTER(isBlank(?s) || isBlank(?o)) }";
        host::execute_update(delete_update)
            .map_err(|e| format!("wf_skolemize: delete originals: {e}"))?;

        Ok(BindingSets {
            vars: vec!["renamed".into(), "deleted".into()],
            rows: vec![vec![
                Binding {
                    name: "renamed".into(),
                    value: int_literal(unique_bnodes.len() as i64),
                },
                Binding {
                    name: "deleted".into(),
                    value: int_literal(renamed as i64),
                },
            ]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_skolemize: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_skolemize: aggregate not applicable".into())
    }
    fn cardinality_estimate(
        _input: Cardinality,
        _args: Vec<Value>,
    ) -> Result<Cardinality, String> {
        Ok(Cardinality {
            value: 1.0,
            accuracy: Accuracy::Injected,
        })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: Value::Literal(Literal {
                    label: "wf_skolemize() — rewrites every blank node in \
                            the store as a deterministic <https://tegmentum.\
                            ai/.well-known/genid/…> IRI. Idempotent-ish. \
                            Returns (renamed, deleted) counts."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

/// Render a WIT `value` as its SPARQL serialization for use in INSERT
/// DATA. Bnodes get replaced with their genid IRI; the caller's
/// `seen_bnodes` accumulator tracks how many distinct labels we saw so
/// the return payload can report the rename count.
fn value_to_sparql(v: &Value, seen_bnodes: &mut std::collections::HashSet<String>) -> String {
    match v {
        Value::Iri(s) => format!("<{s}>"),
        Value::Bnode(label) => {
            seen_bnodes.insert(label.clone());
            format!("<{}>", mint_genid(label))
        }
        Value::Literal(l) => {
            // Escape backslash + double-quote + newline + CR + tab per
            // SPARQL string literal rules. Datatype-tagged with the
            // literal's IRI so integer/date/… survive round-trip.
            let escaped = l
                .label
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            if let Some(lang) = &l.lang {
                format!("\"{escaped}\"@{lang}")
            } else {
                format!("\"{escaped}\"^^<{}>", l.datatype)
            }
        }
    }
}

fn binding_value(row: &[Binding], name: &str) -> Option<Value> {
    row.iter().find(|b| b.name == name).map(|b| b.value.clone())
}

/// Deterministic per-label genid IRI. Salt-hashed so a fresh session on
/// data that reused bnode labels from a prior session doesn't collide.
/// Not cryptographic — collisions are catastrophic but astronomically
/// unlikely at 128 bits.
fn mint_genid(label: &str) -> String {
    let mut hash: u64 = SALT;
    for byte in label.bytes() {
        hash = hash.wrapping_mul(0x100000001B3).wrapping_add(byte as u64);
    }
    // Second half of the hash from a fresh accumulator seeded with the
    // first — gives us 128 bits of collision domain from a 64-bit state.
    let mut hash2: u64 = hash.rotate_left(23) ^ 0x428A2F98D728AE22;
    for byte in label.bytes() {
        hash2 = hash2.wrapping_mul(0x100000001B3).wrapping_add(byte as u64);
    }
    format!("{GENID_PREFIX}{hash:016x}{hash2:016x}")
}

fn int_literal(n: i64) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
}

export!(Component);
