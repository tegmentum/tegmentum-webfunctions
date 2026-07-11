//! wf_canonicalize — resolve owl:sameAs at ingest.
//!
//! Signature: `wf:call(<wf_canonicalize.wasm>, "<config-json>")`
//!    → binding-set { classes: xsd:integer, aliased: xsd:integer,
//!                     rewritten: xsd:integer }
//!
//! Config JSON shape (only `sink` is required; `rule` defaults to
//! `mint_genid`):
//!
//! ```json
//! { "sink": "sqlite:///data/mv.db#aliases",
//!   "rule": "mint_genid" }
//! ```
//!
//! Pipeline (five phases):
//!
//! 0. Load any existing alias map from the sink. Seed the union-find
//!    with the (alias → canonical) pairs so previously-assigned
//!    canonicals stay sticky across ingest batches. First run finds no
//!    table and skips seeding; subsequent runs pick up the accumulated
//!    identity decisions and never remint a canonical for a class that
//!    already has one.
//!
//! 1. Enumerate every `?a owl:sameAs ?b` triple in the store, union
//!    them into the DSU. sameAs is transitive per OWL, so A↔B, B↔C
//!    forms one class {A, B, C}. Combined with the seed pairs from
//!    phase 0, this produces the current post-batch equivalence
//!    classes.
//!
//! 2. For each class, pick a canonical. If the class already contains
//!    a canonical from phase 0's seed (sticky path), reuse it — no
//!    remint even if the class grew. Otherwise apply the configured
//!    rule. v1 rules:
//!    * `mint_genid` (default) — mint a deterministic well-known-genid
//!      IRI derived from the sorted class membership. Every source URI
//!      is treated equally as an alias; no arbitrary preference. Matches
//!      wf_skolemize's identity resolution.
//!    * `shortest_uri` — promote the shortest source URI (lex-first
//!      tiebreak). Retained for callers who want to keep a source URI
//!      as canonical (e.g. when one identifier scheme is authoritative).
//!
//! 3. Rewrite the store: for every triple involving any alias (non-
//!    canonical member) in subject or object position, INSERT the
//!    canonicalized version and DELETE the original. Batched as one
//!    INSERT DATA + one filtered DELETE to minimize round trips.
//!
//! 4. Write the (alias → canonical) map into the sink so external
//!    consumers who reach for a non-canonical IRI can be redirected.
//!    Schema: (alias TEXT PRIMARY KEY, canonical TEXT NOT NULL).
//!
//! 5. Delete the owl:sameAs triples themselves — they've served their
//!    purpose and the substrate now maintains the equivalences via
//!    canonical identity + alias map.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use std::collections::HashMap;

use serde::Deserialize;

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const OWL_SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";

#[derive(Deserialize)]
struct Config {
    sink: String,
    #[serde(default = "default_rule")]
    rule: String,
}

fn default_rule() -> String {
    // Mint a well-known-genid IRI per equivalence class rather than
    // promoting one of the sources — treats every input identifier the
    // same instead of arbitrarily preferring one. Matches wf_skolemize's
    // treatment of blank nodes: identity ambiguity always resolves to a
    // minted IRI + alias map.
    "mint_genid".into()
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let config_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_canonicalize: first arg must be a config-json string literal"
                        .into(),
                );
            }
        };
        let cfg: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("wf_canonicalize: config parse: {e}"))?;

        // Phase 0: prepare the sink and seed the DSU from any existing
        // (alias → canonical) map. Table is created idempotently so a
        // first run finds it empty and the seed loop is a no-op.
        let sink_handle = host::sink_open(&cfg.sink)?;
        let table = table_name_from(&cfg.sink);
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
             alias TEXT PRIMARY KEY, \
             canonical TEXT NOT NULL)"
        );
        host::sink_execute(sink_handle, &ddl, &[])
            .map_err(|e| format!("wf_canonicalize: create alias table: {e}"))?;

        let mut dsu = DisjointSetUnion::new();
        let mut existing_canonicals: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        let existing = host::sink_execute(
            sink_handle,
            &format!("SELECT alias, canonical FROM {table}"),
            &[],
        )
        .map_err(|e| format!("wf_canonicalize: load existing alias map: {e}"))?;
        for row in &existing.rows {
            let alias = binding_literal_str(row, "alias");
            let canonical = binding_literal_str(row, "canonical");
            if let (Some(a), Some(c)) = (alias, canonical) {
                dsu.union(&a, &c);
                existing_canonicals.insert(c);
            }
        }
        let seed_size = existing_canonicals.len();

        // Phase 1: union-find over sameAs in the store on top of the seed.
        let pairs = host::execute_query(
            &format!(
                "SELECT ?a ?b WHERE {{ ?a <{OWL_SAME_AS}> ?b }}"
            ),
            &[],
            None,
        )?;

        for row in &pairs.rows {
            let a = match binding_iri(row, "a") {
                Some(v) => v,
                None => continue,
            };
            let b = match binding_iri(row, "b") {
                Some(v) => v,
                None => continue,
            };
            dsu.union(&a, &b);
        }
        let classes = dsu.classes();

        // Phase 2: pick canonicals. Sticky rule — if the class already
        // contains a previously-assigned canonical (from the phase-0
        // seed), reuse it verbatim. This makes re-runs safe: canonicals
        // never change once assigned, so triples that reference them
        // don't need to be rewritten again. Only newly-added members
        // become fresh aliases in the map.
        //
        // If multiple existing canonicals ended up in the same class
        // (two previously-separate classes merged via a newly-observed
        // sameAs bridge), pick the lex-smallest to keep the choice
        // deterministic; the other becomes an alias, and any triples
        // referencing it get rewritten in phase 3.
        let mut alias_to_canonical: HashMap<String, String> = HashMap::new();
        for class in &classes {
            let mut existing_in_class: Vec<&String> = class
                .iter()
                .filter(|m| existing_canonicals.contains(*m))
                .collect();
            existing_in_class.sort();
            let canonical = match existing_in_class.first() {
                Some(sticky) => (*sticky).clone(),
                None => pick_canonical(class, &cfg.rule)?,
            };
            for member in class {
                if member != &canonical {
                    alias_to_canonical.insert(member.clone(), canonical.clone());
                }
            }
        }

        // Phase 3: rewrite the graph. Fetch every triple that touches an
        // alias, INSERT the canonicalized form, then filter-delete the
        // originals. Deleting last so the fetched originals stay valid
        // through the guest's iteration.
        //
        // For scale we chunk aliases into batches: SPARQL VALUES lists
        // stay under a few thousand IRIs comfortably. v1 dataset sizes
        // (thousands of aliases) fit in one batch — chunk only when we
        // grow past ~5000 aliases.
        let aliases_len = alias_to_canonical.len();
        let mut rewritten = 0u64;

        if !alias_to_canonical.is_empty() {
            let alias_iris: Vec<&String> = alias_to_canonical.keys().collect();

            // Fetch triples where subject or object is any alias.
            let values_clause = alias_iris
                .iter()
                .map(|iri| format!("<{iri}>"))
                .collect::<Vec<_>>()
                .join(" ");
            let fetch = format!(
                "SELECT ?s ?p ?o WHERE {{ \
                 {{ VALUES ?target {{ {vals} }} ?target ?p ?o . BIND(?target AS ?s) }} \
                 UNION \
                 {{ VALUES ?target {{ {vals} }} ?s ?p ?target . BIND(?target AS ?o) }} \
                 FILTER(?p != <{OWL_SAME_AS}>) \
                 }}",
                vals = values_clause,
            );
            let touched = host::execute_query(&fetch, &[], None)?;

            // Build INSERT DATA batch of canonicalized triples.
            let mut insert_body = String::new();
            for row in &touched.rows {
                let s = match binding_value(row, "s") {
                    Some(v) => v,
                    None => continue,
                };
                let p = match binding_value(row, "p") {
                    Some(v) => v,
                    None => continue,
                };
                let o = match binding_value(row, "o") {
                    Some(v) => v,
                    None => continue,
                };
                let s_txt = value_to_sparql(&s, &alias_to_canonical);
                let p_txt = value_to_sparql(&p, &alias_to_canonical);
                let o_txt = value_to_sparql(&o, &alias_to_canonical);
                insert_body.push_str(&s_txt);
                insert_body.push(' ');
                insert_body.push_str(&p_txt);
                insert_body.push(' ');
                insert_body.push_str(&o_txt);
                insert_body.push_str(" .\n");
                rewritten += 1;
            }

            if !insert_body.is_empty() {
                let insert = format!("INSERT DATA {{ {insert_body} }}");
                host::execute_update(&insert).map_err(|e| {
                    format!("wf_canonicalize: insert canonicalized batch: {e}")
                })?;
            }

            // Filter-delete all triples touching an alias in subject or
            // object position. Reuses the same VALUES list.
            let delete = format!(
                "DELETE {{ ?s ?p ?o }} WHERE {{ \
                 ?s ?p ?o . \
                 VALUES ?alias {{ {vals} }} \
                 FILTER(?s = ?alias || ?o = ?alias) \
                 }}",
                vals = values_clause,
            );
            host::execute_update(&delete).map_err(|e| {
                format!("wf_canonicalize: delete alias-bearing triples: {e}")
            })?;
        }

        // Phase 4: append the alias map. INSERT OR REPLACE so re-runs
        // that redirect a previously-seen alias to a merged class's
        // canonical overwrite the old row instead of erroring on the
        // primary key.
        if !alias_to_canonical.is_empty() {
            let insert = format!(
                "INSERT OR REPLACE INTO {table} (alias, canonical) VALUES (?, ?)"
            );
            for (alias, canonical) in &alias_to_canonical {
                host::sink_execute(
                    sink_handle,
                    &insert,
                    &[string_lit(alias), string_lit(canonical)],
                )
                .map_err(|e| {
                    format!("wf_canonicalize: alias table insert `{alias}`: {e}")
                })?;
            }
        }
        host::sink_close(sink_handle).ok();

        // Phase 5: delete the sameAs assertions.
        let delete_sameas = format!(
            "DELETE {{ ?a <{OWL_SAME_AS}> ?b }} WHERE {{ ?a <{OWL_SAME_AS}> ?b }}"
        );
        host::execute_update(&delete_sameas)
            .map_err(|e| format!("wf_canonicalize: delete sameAs assertions: {e}"))?;

        Ok(BindingSets {
            vars: vec![
                "classes".into(),
                "aliased".into(),
                "rewritten".into(),
                "seeded".into(),
            ],
            rows: vec![vec![
                Binding {
                    name: "classes".into(),
                    value: int_literal(classes.len() as i64),
                },
                Binding {
                    name: "aliased".into(),
                    value: int_literal(aliases_len as i64),
                },
                Binding {
                    name: "rewritten".into(),
                    value: int_literal(rewritten as i64),
                },
                Binding {
                    name: "seeded".into(),
                    value: int_literal(seed_size as i64),
                },
            ]],
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_canonicalize: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_canonicalize: aggregate not applicable".into())
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
                    label: "wf_canonicalize(\"{ sink, rule }\") — union-find \
                            over owl:sameAs, pick canonical per equivalence \
                            class, rewrite the store, table the alias→canonical \
                            map into the sink, delete sameAs assertions."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical selection
// ---------------------------------------------------------------------------

fn pick_canonical(class: &[String], rule: &str) -> Result<String, String> {
    match rule {
        // Mint a fresh well-known-genid IRI derived from the equivalence
        // class's sorted membership. No original identifier is promoted;
        // every source is treated as an alias. Deterministic — the same
        // input class always produces the same canonical.
        "mint_genid" => {
            if class.is_empty() {
                return Err("wf_canonicalize: empty equivalence class".into());
            }
            let mut sorted: Vec<&str> = class.iter().map(String::as_str).collect();
            sorted.sort();
            let joined = sorted.join("\0");
            Ok(mint_genid_iri(&joined))
        }
        // Shortest URI wins; lex-first tiebreak. Deterministic and rule-
        // free — no external prefix table to maintain. Retained as an
        // opt-in for callers who prefer promoting a real source URI
        // (useful when one identifier scheme is definitively primary).
        "shortest_uri" => class
            .iter()
            .min_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
            .cloned()
            .ok_or_else(|| "wf_canonicalize: empty equivalence class".into()),
        other => Err(format!(
            "wf_canonicalize: unknown rule `{other}` (v1 supports: mint_genid, shortest_uri)"
        )),
    }
}

/// Mint a deterministic well-known-genid IRI from an input string. Same
/// 128-bit hash pattern used by wf_skolemize — two 64-bit accumulators
/// seeded so collisions across skolemize/canonicalize outputs are
/// astronomically unlikely.
fn mint_genid_iri(input: &str) -> String {
    const GENID_PREFIX: &str = "https://tegmentum.ai/.well-known/genid/";
    const SALT: u64 = 0x9E3779B97F4A7C15;
    let mut h1: u64 = SALT;
    for b in input.bytes() {
        h1 = h1.wrapping_mul(0x100000001B3).wrapping_add(b as u64);
    }
    let mut h2: u64 = h1.rotate_left(23) ^ 0x428A2F98D728AE22;
    for b in input.bytes() {
        h2 = h2.wrapping_mul(0x100000001B3).wrapping_add(b as u64);
    }
    format!("{GENID_PREFIX}{h1:016x}{h2:016x}")
}

// ---------------------------------------------------------------------------
// Disjoint-set union
// ---------------------------------------------------------------------------

struct DisjointSetUnion {
    parent: HashMap<String, String>,
}

impl DisjointSetUnion {
    fn new() -> Self {
        Self {
            parent: HashMap::new(),
        }
    }

    fn find(&mut self, x: &str) -> String {
        let p = self
            .parent
            .entry(x.to_string())
            .or_insert_with(|| x.to_string())
            .clone();
        if p == x {
            return p;
        }
        let root = self.find(&p);
        self.parent.insert(x.to_string(), root.clone());
        root
    }

    fn union(&mut self, a: &str, b: &str) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent.insert(ra, rb);
        }
    }

    /// Emit all equivalence classes as a list of member-lists.
    fn classes(&mut self) -> Vec<Vec<String>> {
        let keys: Vec<String> = self.parent.keys().cloned().collect();
        let mut buckets: HashMap<String, Vec<String>> = HashMap::new();
        for k in keys {
            let root = self.find(&k);
            buckets.entry(root).or_default().push(k);
        }
        buckets.into_values().collect()
    }
}

// ---------------------------------------------------------------------------
// SPARQL value rendering
// ---------------------------------------------------------------------------

fn value_to_sparql(
    v: &Value,
    alias_to_canonical: &HashMap<String, String>,
) -> String {
    match v {
        Value::Iri(s) => {
            let target = alias_to_canonical.get(s).unwrap_or(s);
            format!("<{target}>")
        }
        Value::Bnode(label) => format!("_:{label}"),
        Value::Literal(l) => {
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

// ---------------------------------------------------------------------------
// Binding + literal helpers
// ---------------------------------------------------------------------------

fn binding_iri(row: &[Binding], name: &str) -> Option<String> {
    row.iter().find(|b| b.name == name).and_then(|b| match &b.value {
        Value::Iri(s) => Some(s.clone()),
        _ => None,
    })
}

fn binding_value(row: &[Binding], name: &str) -> Option<Value> {
    row.iter().find(|b| b.name == name).map(|b| b.value.clone())
}

/// Extract the string form of a binding's value regardless of variant.
/// Used for the sink's alias table — its columns come back as WIT
/// literals per the sink-execute contract, and we only need their
/// lexical form.
fn binding_literal_str(row: &[Binding], name: &str) -> Option<String> {
    row.iter().find(|b| b.name == name).and_then(|b| match &b.value {
        Value::Literal(l) => Some(l.label.clone()),
        Value::Iri(s) => Some(s.clone()),
        _ => None,
    })
}

fn table_name_from(url: &str) -> String {
    url.rsplit_once('#')
        .map(|(_, frag)| frag.to_string())
        .unwrap_or_else(|| "aliases".into())
}

fn string_lit(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

fn int_literal(n: i64) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
}

export!(Component);
