//! wf_validate — check a subject's shape against a descriptor's
//! constraint block.
//!
//! Signatures:
//!   * `wf:call(<wf_validate.wasm>, "<descriptor-json>", <subject-iri>)`
//!         — validate one subject, return one row per violation.
//!   * `wf:call(<wf_validate.wasm>, "<descriptor-json>")`
//!         — validate every subject matched by the descriptor's anchor,
//!         return one row per violation across all subjects.
//!
//! Return columns: (subject, column, kind, message). Kind is a short
//! machine-readable tag; message is human-readable. Empty result = every
//! subject is valid.
//!
//! Checks performed per column:
//!   * cardinality: 1 requires exactly one value; 1..n requires >=1;
//!     0..1 requires <=1; 0..n is unconstrained.
//!   * type: literal columns whose objects don't match the declared xsd
//!     datatype produce a type violation. IRI columns whose objects
//!     aren't IRIs produce a type violation.
//!   * constraint.regex: literal values (string-typed) must match.
//!   * constraint.min / max: numeric bounds on integer/decimal columns.
//!   * constraint.enum: value must appear in the enumerated set.
//!   * constraint.min_length / max_length: string length bounds.
//!
//! Read-only: uses execute-query for anchor discovery and per-subject
//! column probes. Doesn't touch execute-update, sink-*, or invoke-wasm.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use std::collections::HashSet;

use regex_lite::Regex;
use serde::Deserialize;

use stardog::webfunction::host;
use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
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
// Guest impl
// ---------------------------------------------------------------------------

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        let descriptor_json = match args.first() {
            Some(Value::Literal(l)) => l.label.clone(),
            _ => {
                return Err(
                    "wf_validate: first arg must be a descriptor-json string literal"
                        .into(),
                );
            }
        };
        let d: Descriptor = serde_json::from_str(&descriptor_json)
            .map_err(|e| format!("wf_validate: descriptor parse: {e}"))?;

        // Optional second arg: a specific subject IRI. Absent = validate
        // every subject the anchor matches.
        let subject_iri = args.get(1).and_then(|v| match v {
            Value::Iri(s) => Some(s.clone()),
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

        Ok(BindingSets {
            vars: vec![
                "subject".into(),
                "column".into(),
                "kind".into(),
                "message".into(),
            ],
            rows: violations.into_iter().map(violation_to_row).collect(),
        })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("wf_validate: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("wf_validate: aggregate not applicable".into())
    }
    fn cardinality_estimate(
        _input: Cardinality,
        _args: Vec<Value>,
    ) -> Result<Cardinality, String> {
        Ok(Cardinality {
            value: 100.0,
            accuracy: Accuracy::Injected,
        })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: Value::Literal(Literal {
                    label: "wf_validate(\"<descriptor-json>\" [, <subject-iri>]) \
                            — check cardinality / type / constraint block per \
                            column. Returns (subject, column, kind, message) \
                            per violation."
                        .into(),
                    datatype: XSD_STRING.into(),
                    lang: None,
                }),
            }]],
        }
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
    let bs = host::execute_query(&sparql, &[], None)?;
    let mut out = Vec::with_capacity(bs.rows.len());
    for row in &bs.rows {
        if let Some(iri) = row.first().and_then(|b| match &b.value {
            Value::Iri(s) => Some(s.clone()),
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
    let bs = host::execute_query(&sparql, &[], None)?;
    let values: Vec<Value> = bs
        .rows
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

    // Type + constraint checks per value.
    for v in &values {
        check_type(subject, col, v, violations);
        if let Some(c) = &col.constraint {
            check_constraint(subject, col, v, c, violations);
        }
    }

    // rdf:type is implicit for class-anchored shapes; if the descriptor
    // has a class anchor, verify the subject actually has the type. Only
    // once per subject — record on the anchor class column-key.
    // Not per-column; skip here.

    Ok(())
}

fn check_type(
    subject: &str,
    col: &Column,
    v: &Value,
    violations: &mut Vec<Violation>,
) {
    let expect_iri = col.r#type == "iri";
    match v {
        Value::Iri(_) if expect_iri => {}
        Value::Iri(_) => violations.push(Violation {
            subject: subject.into(),
            column: col.name.clone(),
            kind: "type".into(),
            message: format!("expected literal of type `{}`, got IRI", col.r#type),
        }),
        Value::Literal(l) => {
            if expect_iri {
                violations.push(Violation {
                    subject: subject.into(),
                    column: col.name.clone(),
                    kind: "type".into(),
                    message: "expected IRI, got literal".into(),
                });
            } else {
                let expected_datatype = xsd_iri(&col.r#type);
                if l.datatype != expected_datatype {
                    // xsd:string is often the source's default; permit
                    // it as a soft match for string columns.
                    let permissive =
                        col.r#type == "string" && l.datatype == XSD_STRING;
                    if !permissive {
                        violations.push(Violation {
                            subject: subject.into(),
                            column: col.name.clone(),
                            kind: "type".into(),
                            message: format!(
                                "expected xsd datatype `{}`, got `{}`",
                                expected_datatype, l.datatype
                            ),
                        });
                    }
                }
            }
        }
        Value::Bnode(_) => violations.push(Violation {
            subject: subject.into(),
            column: col.name.clone(),
            kind: "type".into(),
            message: "expected typed value, got bnode".into(),
        }),
    }
}

fn check_constraint(
    subject: &str,
    col: &Column,
    v: &Value,
    c: &Constraint,
    violations: &mut Vec<Violation>,
) {
    let lex = match v {
        Value::Literal(l) => Some(l.label.as_str()),
        Value::Iri(s) => Some(s.as_str()),
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
                message: format!("value has {} chars, min_length {min_len}", lex.chars().count()),
            });
        }
    }
    if let Some(max_len) = c.max_length {
        if lex.chars().count() > max_len {
            violations.push(Violation {
                subject: subject.into(),
                column: col.name.clone(),
                kind: "max_length".into(),
                message: format!("value has {} chars, max_length {max_len}", lex.chars().count()),
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
// Violation → row
// ---------------------------------------------------------------------------

struct Violation {
    subject: String,
    column: String,
    kind: String,
    message: String,
}

fn violation_to_row(v: Violation) -> Vec<Binding> {
    vec![
        Binding {
            name: "subject".into(),
            value: Value::Iri(v.subject),
        },
        Binding {
            name: "column".into(),
            value: string_lit(&v.column),
        },
        Binding {
            name: "kind".into(),
            value: string_lit(&v.kind),
        },
        Binding {
            name: "message".into(),
            value: string_lit(&v.message),
        },
    ]
}

fn string_lit(s: &str) -> Value {
    Value::Literal(Literal {
        label: s.into(),
        datatype: XSD_STRING.into(),
        lang: None,
    })
}

// Suppress unused-import warnings for shapes we don't reach at v1.
#[allow(dead_code)]
fn _touch() {
    let _ = RDF_TYPE;
    let _: Option<&Anchor> = None::<&Anchor>.map(|a| a);
    let _: Option<&Vec<String>> = None;
}

// Ensure Anchor.predicate_signature is considered used by the compiler
// even though enumerate_subjects reads it via pattern match.
#[allow(dead_code)]
fn _use_anchor(a: &Anchor) -> (Option<&String>, Option<&Vec<String>>) {
    (a.class.as_ref(), a.predicate_signature.as_ref())
}

export!(Component);
