//! wf_validate — check subjects' shape against a descriptor's
//! constraint block.
//!
//! Migrated (Follow-up E) from the Stardog overlay
//! `stardog:webfunction@0.5.0` world to the base
//! `tegmentum:webfunction/extension-with-host-callbacks@0.1.0` world.
//!
//! Signature: `wf_validate("<descriptor-json>" [, <subject-iri>])`
//!   -> rdf:JSON literal `{ "violations": [ ... ] }`
//!
//! The Stardog-era shape returned one row per violation with columns
//! (subject, column, kind, message). The base `extension::call`
//! surface returns a single term, so violations are collapsed into a
//! single rdf:JSON literal whose top-level `violations` array carries
//! one object per violation with the same four keys. Callers that
//! need row-shaped output can `SELECT ... WHERE { BIND(...) }` to
//! parse the JSON per row.
//!
//! Read-only against `graph-callbacks::execute-query` for anchor
//! discovery and per-subject column probes. Does not touch
//! execute-update, sink-*, or invoke-wasm.

#[allow(warnings)]
mod bindings;

use std::collections::HashSet;

use regex_lite::Regex;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use bindings::exports::tegmentum::webfunction::aggregate::{
    AggregateDescriptor, AggregateState, Guest as AggregateGuest, GuestAggregateState,
};
use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::exports::tegmentum::webfunction::property_function::{
    BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
};
use bindings::tegmentum::webfunction::graph_callbacks::{
    self as gc, Binding as WitBinding, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
#[allow(dead_code)]
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    shape: String,
    anchor: Anchor,
    columns: Vec<Column>,
}

#[derive(Deserialize)]
struct Anchor {
    class: Option<String>,
    predicate_signature: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Column {
    name: String,
    role: String,
    predicate: Option<String>,
    #[serde(default = "default_type")]
    r#type: String,
    #[serde(default = "default_cardinality")]
    cardinality: String,
    #[serde(default)]
    constraint: Option<Constraint>,
}

#[derive(Deserialize, Default)]
struct Constraint {
    #[serde(default)]
    regex: Option<String>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    r#enum: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    min_length: Option<usize>,
    #[serde(default)]
    max_length: Option<usize>,
}

fn default_type() -> String {
    "string".into()
}
fn default_cardinality() -> String {
    "0..1".into()
}

// ---------------------------------------------------------------------------
// graph-callbacks helpers
// ---------------------------------------------------------------------------

fn execute_query(sparql: &str) -> Result<CallbackQueryResult, String> {
    gc::execute_query(sparql).map_err(|e| match e {
        gc::GraphCallError::SyntaxError(m) => format!("graph-callbacks syntax-error: {m}"),
        gc::GraphCallError::BackendError(m) => format!("graph-callbacks backend-error: {m}"),
        gc::GraphCallError::NotPermitted(m) => format!("graph-callbacks not-permitted: {m}"),
    })
}

/// Group a flat `list<binding>` from graph-callbacks into per-row
/// vectors — bindings are emitted in row-major order and each row is
/// delimited by re-appearance of any variable name.
fn group_bindings_into_rows(flat: Vec<WitBinding>) -> Vec<Vec<WitBinding>> {
    let mut rows: Vec<Vec<WitBinding>> = Vec::new();
    let mut current: Vec<WitBinding> = Vec::new();
    for b in flat {
        if current.iter().any(|prior| prior.variable == b.variable) {
            rows.push(std::mem::take(&mut current));
        }
        current.push(b);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

fn select_rows(sparql: &str) -> Result<Vec<Vec<WitBinding>>, String> {
    match execute_query(sparql)? {
        CallbackQueryResult::Bindings(bs) => Ok(group_bindings_into_rows(bs)),
        CallbackQueryResult::Quads(_) => {
            Err("wf_validate: SELECT expected but graph-callbacks returned quads".into())
        }
        CallbackQueryResult::Boolean(_) => {
            Err("wf_validate: SELECT expected but graph-callbacks returned boolean".into())
        }
    }
}

// ---------------------------------------------------------------------------
// Guest impls
// ---------------------------------------------------------------------------

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "wf_validate".into(),
            min_arity: 1,
            max_arity: Some(2),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "wf_validate" => wf_validate_impl(&args),
            other => Err(format!("wf_validate: unknown function '{other}'")),
        }
    }
}

fn wf_validate_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let descriptor_json = match args.first() {
        Some(WitTerm::Literal(l)) => l.value.clone(),
        _ => {
            return Err(
                "wf_validate: first arg must be a descriptor-json string literal".into(),
            );
        }
    };
    let d: Descriptor = serde_json::from_str(&descriptor_json)
        .map_err(|e| format!("wf_validate: descriptor parse: {e}"))?;

    // Optional second arg: a specific subject IRI. Absent = validate
    // every subject the anchor matches.
    let subject_iri = args.get(1).and_then(|v| match v {
        WitTerm::NamedNode(s) => Some(s.clone()),
        _ => None,
    });

    let subjects = match subject_iri {
        Some(s) => vec![s],
        None => enumerate_subjects(&d)?,
    };

    let mut violations: Vec<Violation> = Vec::new();
    for subject in &subjects {
        for col in &d.columns {
            check_column(subject, col, &mut violations)?;
        }
    }

    let payload: Vec<JsonValue> = violations.into_iter().map(violation_to_json).collect();
    let out = json!({ "violations": payload });
    let serialized = serde_json::to_string(&out)
        .map_err(|e| format!("wf_validate: serialize summary: {e}"))?;
    Ok(WitTerm::Literal(WitLiteral {
        value: serialized,
        datatype: Some(RDF_JSON.into()),
        language: None,
    }))
}

impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "wf_validate: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_validate: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_validate: aggregate state was never constructed".into())
    }
}

impl PropertyFunctionGuest for Component {
    fn register_property_functions() -> Vec<PropertyDescriptor> {
        Vec::new()
    }

    fn evaluate(
        name: String,
        _subjects: Vec<WitTerm>,
        _objects: Vec<WitTerm>,
    ) -> Result<Vec<BindingRow>, String> {
        Err(format!(
            "wf_validate: unknown property function '{name}' (this component provides none)"
        ))
    }
}

// ---------------------------------------------------------------------------
// Subject enumeration
// ---------------------------------------------------------------------------

fn enumerate_subjects(d: &Descriptor) -> Result<Vec<String>, String> {
    let sparql = if let Some(class) = &d.anchor.class {
        format!("SELECT ?s WHERE {{ ?s a <{class}> }}")
    } else if let Some(sig) = &d.anchor.predicate_signature {
        let mut patterns = String::new();
        for (i, p) in sig.iter().enumerate() {
            patterns.push_str(&format!("?s <{p}> ?_sig{i} . "));
        }
        format!("SELECT DISTINCT ?s WHERE {{ {patterns} }}")
    } else {
        return Err("wf_validate: anchor missing both class and predicate_signature".into());
    };
    let rows = select_rows(&sparql)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Some(iri) = row.first().and_then(|b| match &b.value {
            WitTerm::NamedNode(s) => Some(s.clone()),
            _ => None,
        }) {
            out.push(iri);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Per-column checks
// ---------------------------------------------------------------------------

fn check_column(
    subject: &str,
    col: &Column,
    violations: &mut Vec<Violation>,
) -> Result<(), String> {
    if col.role == "subject_iri" {
        return Ok(());
    }
    let predicate = match &col.predicate {
        Some(p) => p,
        None => return Ok(()),
    };

    let sparql = format!("SELECT ?o WHERE {{ <{subject}> <{predicate}> ?o }}");
    let rows = select_rows(&sparql)?;
    let values: Vec<WitTerm> = rows
        .iter()
        .filter_map(|row| row.first().map(|b| b.value.clone()))
        .collect();

    // Cardinality.
    let n = values.len();
    match col.cardinality.as_str() {
        "1" if n != 1 => violations.push(Violation {
            subject: subject.into(),
            column: col.name.clone(),
            kind: "cardinality".into(),
            message: format!("expected exactly 1 value, found {n}"),
        }),
        "1..n" if n < 1 => violations.push(Violation {
            subject: subject.into(),
            column: col.name.clone(),
            kind: "cardinality".into(),
            message: "expected at least 1 value, found 0".into(),
        }),
        "0..1" if n > 1 => violations.push(Violation {
            subject: subject.into(),
            column: col.name.clone(),
            kind: "cardinality".into(),
            message: format!("expected at most 1 value, found {n}"),
        }),
        _ => {}
    }

    for v in &values {
        check_type(subject, col, v, violations);
        if let Some(c) = &col.constraint {
            check_constraint(subject, col, v, c, violations);
        }
    }

    Ok(())
}

fn check_type(subject: &str, col: &Column, v: &WitTerm, violations: &mut Vec<Violation>) {
    let expect_iri = col.r#type == "iri";
    match v {
        WitTerm::NamedNode(_) if expect_iri => {}
        WitTerm::NamedNode(_) => violations.push(Violation {
            subject: subject.into(),
            column: col.name.clone(),
            kind: "type".into(),
            message: format!("expected literal of type `{}`, got IRI", col.r#type),
        }),
        WitTerm::Literal(l) => {
            if expect_iri {
                violations.push(Violation {
                    subject: subject.into(),
                    column: col.name.clone(),
                    kind: "type".into(),
                    message: "expected IRI, got literal".into(),
                });
            } else {
                let expected_datatype = xsd_iri(&col.r#type);
                let actual_datatype = l
                    .datatype
                    .as_deref()
                    .unwrap_or(XSD_STRING);
                if actual_datatype != expected_datatype {
                    let permissive = col.r#type == "string" && actual_datatype == XSD_STRING;
                    if !permissive {
                        violations.push(Violation {
                            subject: subject.into(),
                            column: col.name.clone(),
                            kind: "type".into(),
                            message: format!(
                                "expected xsd datatype `{expected_datatype}`, got `{actual_datatype}`"
                            ),
                        });
                    }
                }
            }
        }
        WitTerm::BlankNode(_) => violations.push(Violation {
            subject: subject.into(),
            column: col.name.clone(),
            kind: "type".into(),
            message: "expected typed value, got bnode".into(),
        }),
        WitTerm::Triple(_) => violations.push(Violation {
            subject: subject.into(),
            column: col.name.clone(),
            kind: "type".into(),
            message: "expected typed value, got quoted triple".into(),
        }),
    }
}

fn check_constraint(
    subject: &str,
    col: &Column,
    v: &WitTerm,
    c: &Constraint,
    violations: &mut Vec<Violation>,
) {
    let lex = match v {
        WitTerm::Literal(l) => Some(l.value.as_str()),
        WitTerm::NamedNode(s) => Some(s.as_str()),
        _ => None,
    };
    let Some(lex) = lex else { return };

    if let Some(re) = &c.regex {
        match Regex::new(re) {
            Ok(compiled) => {
                if !compiled.is_match(lex) {
                    violations.push(Violation {
                        subject: subject.into(),
                        column: col.name.clone(),
                        kind: "regex".into(),
                        message: format!("value `{lex}` did not match /{re}/"),
                    });
                }
            }
            Err(e) => violations.push(Violation {
                subject: subject.into(),
                column: col.name.clone(),
                kind: "regex-invalid".into(),
                message: format!("descriptor regex `{re}` did not compile: {e}"),
            }),
        }
    }

    if let Some(min) = c.min {
        if let Ok(n) = lex.parse::<f64>() {
            if n < min {
                violations.push(Violation {
                    subject: subject.into(),
                    column: col.name.clone(),
                    kind: "min".into(),
                    message: format!("value {n} < min {min}"),
                });
            }
        }
    }
    if let Some(max) = c.max {
        if let Ok(n) = lex.parse::<f64>() {
            if n > max {
                violations.push(Violation {
                    subject: subject.into(),
                    column: col.name.clone(),
                    kind: "max".into(),
                    message: format!("value {n} > max {max}"),
                });
            }
        }
    }
    if let Some(min_len) = c.min_length {
        if lex.chars().count() < min_len {
            violations.push(Violation {
                subject: subject.into(),
                column: col.name.clone(),
                kind: "min_length".into(),
                message: format!(
                    "value has {} chars, min_length {min_len}",
                    lex.chars().count()
                ),
            });
        }
    }
    if let Some(max_len) = c.max_length {
        if lex.chars().count() > max_len {
            violations.push(Violation {
                subject: subject.into(),
                column: col.name.clone(),
                kind: "max_length".into(),
                message: format!(
                    "value has {} chars, max_length {max_len}",
                    lex.chars().count()
                ),
            });
        }
    }
    if let Some(enum_set) = &c.r#enum {
        let allowed: HashSet<String> = enum_set
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        if !allowed.contains(lex) {
            violations.push(Violation {
                subject: subject.into(),
                column: col.name.clone(),
                kind: "enum".into(),
                message: format!("value `{lex}` not in enum"),
            });
        }
    }
}

fn xsd_iri(t: &str) -> &'static str {
    match t {
        "integer" => "http://www.w3.org/2001/XMLSchema#integer",
        "decimal" => "http://www.w3.org/2001/XMLSchema#decimal",
        "boolean" => "http://www.w3.org/2001/XMLSchema#boolean",
        "date" => "http://www.w3.org/2001/XMLSchema#date",
        "datetime" => "http://www.w3.org/2001/XMLSchema#dateTime",
        _ => XSD_STRING,
    }
}

// ---------------------------------------------------------------------------
// Violation → JSON
// ---------------------------------------------------------------------------

struct Violation {
    subject: String,
    column: String,
    kind: String,
    message: String,
}

fn violation_to_json(v: Violation) -> JsonValue {
    json!({
        "subject": v.subject,
        "column": v.column,
        "kind": v.kind,
        "message": v.message,
    })
}

// Silence unused-import checks that fell out of the migration.
#[allow(dead_code)]
fn _use_anchor(a: &Anchor) -> (Option<&String>, Option<&Vec<String>>) {
    (a.class.as_ref(), a.predicate_signature.as_ref())
}

bindings::export!(Component with_types_in bindings);
