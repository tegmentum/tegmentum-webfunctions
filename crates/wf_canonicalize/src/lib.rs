//! wf_canonicalize — resolve owl:sameAs at ingest.
//!
//! Signature: `wf:call(<wf_canonicalize.wasm>, "<config-json>")`
//!    → binding-set { classes: xsd:integer, aliased: xsd:integer,
//!                     rewritten: xsd:integer }
//!
//! Config JSON shape (only `sink` is required; `rule` defaults to
//! `shortest_uri`):
//!
//! ```json
//! { "sink": "sqlite:///data/mv.db#aliases",
//!   "rule": "shortest_uri" }
//! ```
//!
//! Pipeline (four phases):
//!
//! 1. Enumerate every `?a owl:sameAs ?b` triple, build a union-find over
//!    IRI identities. sameAs is transitive per OWL, so A↔B, B↔C forms
//!    one class {A, B, C}. Equivalence classes drop out of the DSU.
//!
//! 2. For each class, pick a canonical member by the configured rule.
//!    v1 rules: `shortest_uri` (shortest lexicographic form wins,
//!    lex-first tiebreak). Extensible later with `prefix_priority` +
//!    an ordered list of prefixes.
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
    "shortest_uri".into()
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

        // Phase 1: union-find over sameAs.
        let pairs = host::execute_query(
            &format!(
                "SELECT ?a ?b WHERE {{ ?a <{OWL_SAME_AS}> ?b }}"
            ),
            &[],
            None,
        )?;

        let mut dsu = DisjointSetUnion::new();
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

        // Phase 2: pick canonicals. Alias map: non-canonical → canonical.
        let mut alias_to_canonical: HashMap<String, String> = HashMap::new();
        for class in &classes {
            let canonical = pick_canonical(class, &cfg.rule)?;
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

        // Phase 4: write the alias map to the sink.
        if !alias_to_canonical.is_empty() {
            let handle = host::sink_open(&cfg.sink)?;
            let table = table_name_from(&cfg.sink);
            let ddl = format!(
                "CREATE TABLE IF NOT EXISTS {table} (\
                 alias TEXT PRIMARY KEY, \
                 canonical TEXT NOT NULL)"
            );
            host::sink_execute(handle, &ddl, &[])
                .map_err(|e| format!("wf_canonicalize: create alias table: {e}"))?;
            let insert = format!(
                "INSERT OR REPLACE INTO {table} (alias, canonical) VALUES (?, ?)"
            );
            for (alias, canonical) in &alias_to_canonical {
                host::sink_execute(
                    handle,
                    &insert,
                    &[string_lit(alias), string_lit(canonical)],
                )
                .map_err(|e| {
                    format!("wf_canonicalize: alias table insert `{alias}`: {e}")
                })?;
            }
            host::sink_close(handle).ok();
        }

        // Phase 5: delete the sameAs assertions.
        let delete_sameas = format!(
            "DELETE {{ ?a <{OWL_SAME_AS}> ?b }} WHERE {{ ?a <{OWL_SAME_AS}> ?b }}"
        );
        host::execute_update(&delete_sameas)
            .map_err(|e| format!("wf_canonicalize: delete sameAs assertions: {e}"))?;

        Ok(BindingSets {
            vars: vec!["classes".into(), "aliased".into(), "rewritten".into()],
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
        // Shortest URI wins; lex-first tiebreak. Deterministic and rule-
        // free — no external prefix table to maintain.
        "shortest_uri" => class
            .iter()
            .min_by(|a, b| {
                a.len()
                    .cmp(&b.len())
                    .then_with(|| a.cmp(b))
            })
            .cloned()
            .ok_or_else(|| "wf_canonicalize: empty equivalence class".into()),
        other => Err(format!(
            "wf_canonicalize: unknown rule `{other}` (v1 supports: shortest_uri)"
        )),
    }
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
