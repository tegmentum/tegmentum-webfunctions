//! wf_infer — derived facts as materialized SPARQL views.
//!
//! Signature: `wf:call(<wf_infer.wasm>, "<rule-json>")`
//!    → binding-set { rule: xsd:string, inserted: xsd:integer }
//!
//! Runs a user-authored CONSTRUCT query and INSERTs the resulting
//! triples into a target named graph. This is the substrate's answer to
//! OWL — rules are SPARQL you wrote, derived triples live in a graph
//! whose provenance is obvious, delete semantics are honest (drop the
//! graph or delete individual quads).
//!
//! Rule JSON shape — two forms accepted, same semantics:
//!
//! **Explicit CONSTRUCT** (raw SPARQL text):
//!
//! ```json
//! {
//!   "name": "type_from_subclass",
//!   "construct": "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
//!                 CONSTRUCT { ?s a ?super } \
//!                 WHERE { ?s a ?sub . ?sub rdfs:subClassOf+ ?super }",
//!   "graph": "http://tegmentum.ai/graph/derived/type_from_subclass",
//!   "refresh_mode": "replace"
//! }
//! ```
//!
//! **Stardog-SRS-style if/then sugar** (translates to CONSTRUCT
//! automatically; the two triple-pattern strings each go into the
//! WHERE and CONSTRUCT clauses verbatim, with `prefixes` prepended
//! once):
//!
//! ```json
//! {
//!   "name": "type_from_subclass",
//!   "prefixes": "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>",
//!   "if":   "?s a ?sub . ?sub rdfs:subClassOf+ ?super",
//!   "then": "?s a ?super",
//!   "graph": "http://tegmentum.ai/graph/derived/type_from_subclass"
//! }
//! ```
//!
//! Reads like SRS but stays as a CONSTRUCT operationally — same code
//! path, same delete semantics, same predictable cost.
//!
//! `refresh_mode`:
//!   * `"replace"` (default) — CLEAR the target graph before insert.
//!     Full recompute; safe when a rule's dependencies changed.
//!   * `"append"` — no clear; add new derivations, keep old ones. Fast
//!     but stale rows may persist. Useful when the rule strictly
//!     accumulates (e.g. temporal facts that never retract).
//!
//! No profile choice. No reasoner. No mystery costs. The CONSTRUCT
//! you wrote is what runs. Whether it's O(1) or exponential is
//! visible in the query text itself.
//!
//! Complements `wf_reason_pyreason` (planned) — that guest handles the
//! fuzzy/probabilistic/paraconsistent cases via a pyreason microservice
//! and writes annotated triples to the same derived-graph pattern.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use serde::Deserialize;

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

#[derive(Deserialize)]
struct Rule {
    name: String,
    #[serde(default)]
    construct: Option<String>,
    #[serde(default, rename = "if")]
    if_clause: Option<String>,
    #[serde(default, rename = "then")]
    then_clause: Option<String>,
    #[serde(default)]
    prefixes: Option<String>,
    graph: String,
    #[serde(default = "default_refresh")]
    refresh_mode: String,

    /// Loop until fixed point. Each iteration reads from
    /// `default_graph ∪ target_graph` (the user writes their WHERE to
    /// reference the target graph via UNION or GRAPH clauses) and writes
    /// to `target_graph`. INSERT DATA is idempotent at the store level,
    /// so re-inserting already-derived triples is a no-op; we detect
    /// convergence by comparing target-graph size before and after each
    /// pass.
    #[serde(default)]
    iterate: bool,

    /// Safety cap on iteration count. Non-terminating rules or slow
    /// convergence would otherwise loop forever. Default 100; the
    /// transitive closure of anything human-scale converges well
    /// under that.
    #[serde(default = "default_max_iterations")]
    max_iterations: u32,
}

fn default_max_iterations() -> u32 {
    100
}

impl Rule {
    /// Build the CONSTRUCT SPARQL text. If the rule specifies `construct`
    /// verbatim, use it as-is. If it uses the `if`/`then` sugar,
    /// synthesise a CONSTRUCT that wraps the two triple-pattern strings
    /// into WHERE and CONSTRUCT clauses respectively, prefixing any
    /// declared namespaces once at the top.
    fn construct_sparql(&self) -> Result<String, String> {
        match (&self.construct, &self.if_clause, &self.then_clause) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(format!(
                "wf_infer: rule `{}` sets both `construct` and `if`/`then`; pick one",
                self.name
            )),
            (Some(q), None, None) => Ok(q.clone()),
            (None, Some(if_body), Some(then_body)) => {
                let prefix = self
                    .prefixes
                    .as_deref()
                    .map(|p| format!("{p}\n"))
                    .unwrap_or_default();
                Ok(format!(
                    "{prefix}CONSTRUCT {{ {then_body} }} WHERE {{ {if_body} }}"
                ))
            }
            (None, Some(_), None) | (None, None, Some(_)) => Err(format!(
                "wf_infer: rule `{}` uses SRS sugar but is missing one of `if` / `then`",
                self.name
            )),
            (None, None, None) => Err(format!(
                "wf_infer: rule `{}` has neither `construct` nor `if`/`then`",
                self.name
            )),
        }
    }
}

fn default_refresh() -> String {
    "replace".into()
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let rule_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_infer: first arg must be a rule-json string literal".into(),
                );
            }
        };
        let rule: Rule = serde_json::from_str(&rule_json)
            .map_err(|e| format!("wf_infer: rule parse: {e}"))?;

        // Full-recompute mode: clear the target graph first so stale
        // derivations don't accumulate. "append" mode skips the clear.
        if rule.refresh_mode == "replace" {
            // SILENT so a first-run against a graph that doesn't exist
            // yet is a no-op rather than an error. SPARQL 1.1 Update:
            // absent SILENT, CLEAR GRAPH of a nonexistent graph is an
            // evaluation error.
            let clear = format!("CLEAR SILENT GRAPH <{}>", rule.graph);
            host::execute_update(&clear)
                .map_err(|e| format!("wf_infer: clear graph `{}`: {e}", rule.graph))?;
        } else if rule.refresh_mode != "append" {
            return Err(format!(
                "wf_infer: unknown refresh_mode `{}` (want replace | append)",
                rule.refresh_mode
            ));
        }

        let construct_sparql = rule.construct_sparql()?;

        if rule.iterate {
            let (iterations, emitted_total, final_size) =
                iterate_to_fixpoint(&rule, &construct_sparql)?;
            // Naming discipline: `emitted_total` counts CONSTRUCT rows
            // across every pass — including the ones INSERT DATA set-
            // semantics silently absorbed as duplicates. It's a
            // diagnostic (how much SPARQL work happened) not a fact
            // count. `graph_size` is the honest "how many derived
            // triples exist" number.
            Ok(BindingSets {
                vars: vec![
                    "rule".into(),
                    "iterations".into(),
                    "emitted_total".into(),
                    "graph_size".into(),
                ],
                rows: vec![vec![
                    Binding {
                        name: "rule".into(),
                        value: string_lit(&rule.name),
                    },
                    Binding {
                        name: "iterations".into(),
                        value: int_lit(iterations as i64),
                    },
                    Binding {
                        name: "emitted_total".into(),
                        value: int_lit(emitted_total as i64),
                    },
                    Binding {
                        name: "graph_size".into(),
                        value: int_lit(final_size as i64),
                    },
                ]],
            })
        } else {
            // Single-pass: run the CONSTRUCT once, insert the results.
            let bs = host::execute_query(&construct_sparql, &[], None)?;
            let inserted = bulk_insert(&rule.graph, &bs)?;
            Ok(BindingSets {
                vars: vec!["rule".into(), "inserted".into()],
                rows: vec![vec![
                    Binding {
                        name: "rule".into(),
                        value: string_lit(&rule.name),
                    },
                    Binding {
                        name: "inserted".into(),
                        value: int_lit(inserted as i64),
                    },
                ]],
            })
        }
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_infer: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_infer: aggregate not applicable".into())
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
                    label: "wf_infer(\"<rule-json>\") — runs a CONSTRUCT and \
                            INSERTs the derived triples into a named graph. \
                            replace mode clears the graph first; append mode \
                            accumulates. Returns (rule, inserted) counts."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
    }
}

// ---------------------------------------------------------------------------
// Fixed-point iteration
// ---------------------------------------------------------------------------

/// Loop the CONSTRUCT until the target graph stops growing. Returns
/// (iterations_run, total_triples_inserted, final_graph_size).
fn iterate_to_fixpoint(rule: &Rule, construct_sparql: &str) -> Result<(u32, u64, u64), String> {
    let mut prev_size = graph_size(&rule.graph)?;
    let mut total_inserted = 0u64;
    let mut iterations = 0u32;

    while iterations < rule.max_iterations {
        let bs = host::execute_query(construct_sparql, &[], None)?;
        let inserted = bulk_insert(&rule.graph, &bs)?;
        total_inserted += inserted;
        iterations += 1;

        let new_size = graph_size(&rule.graph)?;
        if new_size == prev_size {
            // Fixed point: nothing new was added this pass.
            return Ok((iterations, total_inserted, new_size));
        }
        prev_size = new_size;
    }
    // Hit the cap without converging. Return what we have; the caller
    // sees iterations == max_iterations as the signal that the loop was
    // truncated. In practice this means the rule diverges or is very
    // slow to converge — inspect and consider raising max_iterations
    // or restructuring the rule.
    Ok((iterations, total_inserted, prev_size))
}

/// Count triples currently in the target graph. Returns 0 if the graph
/// doesn't exist yet.
fn graph_size(graph: &str) -> Result<u64, String> {
    let sparql = format!(
        "SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"
    );
    let bs = host::execute_query(&sparql, &[], Some(1))?;
    let row = bs
        .rows
        .first()
        .ok_or_else(|| "wf_infer: graph_size returned no row".to_string())?;
    let n = row
        .iter()
        .find(|b| b.name == "n")
        .and_then(|b| match &b.value {
            Value::Literal(l) => l.label.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(0);
    Ok(n)
}

// ---------------------------------------------------------------------------
// INSERT batching
// ---------------------------------------------------------------------------

const BATCH_SIZE: usize = 500;

fn bulk_insert(graph: &str, bs: &BindingSets) -> Result<u64, String> {
    let mut total = 0u64;
    let mut buffer = String::new();
    let mut in_batch = 0usize;

    for row in &bs.rows {
        let mut s: Option<&Value> = None;
        let mut p: Option<&Value> = None;
        let mut o: Option<&Value> = None;
        for b in row {
            match b.name.as_str() {
                "s" => s = Some(&b.value),
                "p" => p = Some(&b.value),
                "o" => o = Some(&b.value),
                _ => {}
            }
        }
        let (Some(s), Some(p), Some(o)) = (s, p, o) else {
            continue;
        };
        buffer.push_str(&value_to_sparql(s));
        buffer.push(' ');
        buffer.push_str(&value_to_sparql(p));
        buffer.push(' ');
        buffer.push_str(&value_to_sparql(o));
        buffer.push_str(" .\n");
        in_batch += 1;
        total += 1;

        if in_batch >= BATCH_SIZE {
            flush(graph, &buffer)?;
            buffer.clear();
            in_batch = 0;
        }
    }
    if !buffer.is_empty() {
        flush(graph, &buffer)?;
    }
    Ok(total)
}

fn flush(graph: &str, triples: &str) -> Result<(), String> {
    let insert = format!(
        "INSERT DATA {{ GRAPH <{graph}> {{ {triples} }} }}"
    );
    host::execute_update(&insert)
        .map_err(|e| format!("wf_infer: insert batch: {e}"))
}

fn value_to_sparql(v: &Value) -> String {
    match v {
        Value::Iri(s) => format!("<{s}>"),
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

fn string_lit(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

fn int_lit(n: i64) -> Value {
    Value::Literal(Literal {
        label: n.to_string(),
        datatype: XSD_INTEGER.into(),
        lang: None,
    })
}

export!(Component);
