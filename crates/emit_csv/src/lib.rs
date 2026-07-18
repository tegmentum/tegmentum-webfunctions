//! emit_csv — aggregate rows into a CSV string.
//!
//! Inverse of `parse_csv`. Given a sequence of input rows presented via
//! the aggregate protocol, produce a single CSV document as an xsd:string
//! literal on `finish`. Each `step` call receives the values for one row
//! as separate arguments, all expected to be string literals — cast via
//! `xsd:string(?x)` in the query if the source values aren't strings.
//!
//! Column count is fixed by the first row: subsequent rows with a
//! different arity are rejected so the emitted CSV stays rectangular.
//! Rows are emitted in the order they arrive.
//!
//! **Multiplicity note.** Under the old flat world, `aggregate-step`
//! received a per-row `mult: u64`; the base sparql-extension world
//! folds that into a single call per row (Stardog historically passed
//! `mult = 1`). Callers that relied on non-unit multiplicity semantics
//! need a per-row repeat at the host side.

#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

use bindings::exports::tegmentum::webfunction::aggregate::{
    AggregateDescriptor, AggregateState, Guest as AggregateGuest, GuestAggregateState,
};
use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::exports::tegmentum::webfunction::property_function::{
    BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
};
use bindings::tegmentum::webfunction::types::{Literal as WitLiteral, Term as WitTerm};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const AGGREGATE_NAME: &str = "emit_csv";

struct Component;

/// Filter interface stub.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "emit_csv: unknown filter function '{name}' (use via SPARQL aggregate)"
        ))
    }
}

/// Aggregate interface: one aggregate, `emit_csv`.
impl AggregateGuest for Component {
    type AggregateState = CsvAccumulator;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        vec![AggregateDescriptor {
            name: AGGREGATE_NAME.to_string(),
            min_arity: 1,
            max_arity: None,
        }]
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        match name.as_str() {
            AGGREGATE_NAME => Ok(AggregateState::new(CsvAccumulator::new())),
            other => Err(format!("emit_csv: unknown aggregate '{other}'")),
        }
    }
}

/// Property-function interface stub.
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
            "emit_csv: unknown property function '{name}' (this component provides none)"
        ))
    }
}

/// Per-instance accumulation state.
/// - `columns` is set by the first row and locks arity for the rest.
/// - `rows` holds owned string cells so `finish` can hand them to a
///   csv::Writer without borrowing across the RefCell boundary.
pub struct CsvAccumulator {
    state: RefCell<CsvState>,
}

struct CsvState {
    columns: Option<usize>,
    rows: Vec<Vec<String>>,
}

impl CsvAccumulator {
    fn new() -> Self {
        Self {
            state: RefCell::new(CsvState {
                columns: None,
                rows: Vec::new(),
            }),
        }
    }
}

fn string_of(v: &WitTerm) -> Result<&str, String> {
    match v {
        WitTerm::Literal(l) => Ok(l.value.as_str()),
        _ => Err("emit_csv: every argument must be a string literal".into()),
    }
}

impl GuestAggregateState for CsvAccumulator {
    fn step(&self, args: Vec<WitTerm>) -> Result<(), String> {
        if args.is_empty() {
            return Err("emit_csv: expected at least one argument per row".into());
        }
        // Validate all cells are string literals before touching state so a
        // partial row can't corrupt the accumulator.
        let mut cells: Vec<String> = Vec::with_capacity(args.len());
        for a in &args {
            cells.push(string_of(a)?.to_string());
        }
        let mut st = self.state.borrow_mut();
        match st.columns {
            None => st.columns = Some(cells.len()),
            Some(n) if n == cells.len() => {}
            Some(n) => {
                return Err(format!(
                    "emit_csv: row arity {} does not match first row arity {}",
                    cells.len(),
                    n
                ));
            }
        }
        st.rows.push(cells);
        Ok(())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        let (columns, rows) = {
            let mut st = self.state.borrow_mut();
            (st.columns.take(), std::mem::take(&mut st.rows))
        };
        let csv = if columns.is_none() {
            String::new()
        } else {
            let mut writer = csv::WriterBuilder::new()
                .has_headers(false)
                .from_writer(Vec::<u8>::new());
            for row in &rows {
                writer
                    .write_record(row.iter().map(|s| s.as_str()))
                    .map_err(|e| format!("emit_csv: write error: {e}"))?;
            }
            let bytes = writer
                .into_inner()
                .map_err(|e| format!("emit_csv: flush error: {e}"))?;
            String::from_utf8(bytes)
                .map_err(|e| format!("emit_csv: non-utf8 output: {e}"))?
        };
        Ok(WitTerm::Literal(WitLiteral {
            value: csv,
            datatype: Some(XSD_STRING.to_string()),
            language: None,
        }))
    }
}

bindings::export!(Component with_types_in bindings);
