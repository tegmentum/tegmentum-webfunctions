//! emit_csv — aggregate rows into a CSV string.
//!
//! Inverse of `parse_csv`. Given a sequence of input rows presented via the
//! aggregate protocol, produce a single CSV document as an xsd:string literal
//! on `aggregate-finish`. Each `aggregate-step` receives the values for one
//! row as separate arguments, all expected to be string literals — cast via
//! `xsd:string(?x)` in the query if the source values aren't strings.
//!
//! Column count is fixed by the first row: subsequent rows with a different
//! arity are rejected so the emitted CSV stays rectangular. Rows are emitted
//! in the order they arrive (SPARQL aggregates see rows in whatever order the
//! plan produces; wrap the SERVICE in a sub-select with ORDER BY if a
//! deterministic ordering matters).
//!
//! Row multiplicity is honored: a row seen with `mult = k` is written k
//! times, matching SPARQL bag semantics.
//!
//! v1 does not accept a header row — feed one in as the first data row if
//! you want headers, or wait for a follow-up revision that takes an
//! explicit header list.
//!
//! `evaluate` is not meaningful for an aggregate; it returns an error.

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};
use std::cell::RefCell;

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

// Per-instance accumulation state.
//   - `columns` is set by the first row and locks arity for the rest of the
//     aggregation.
//   - `rows` holds owned string cells so aggregate_finish can hand them to a
//     csv::Writer without borrowing across the RefCell boundary.
struct State {
    columns: Option<usize>,
    rows: Vec<Vec<String>>,
}

thread_local! {
    static STATE: RefCell<State> = const {
        RefCell::new(State { columns: None, rows: Vec::new() })
    };
}

fn string_literal(s: String) -> Value {
    Value::Literal(Literal { label: s, datatype: XSD_STRING.into(), lang: None })
}

fn string_of(v: &Value) -> Result<&str, String> {
    match v {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("emit_csv: every argument must be a string literal".into()),
    }
}

impl Guest for Component {
    /// `evaluate` is meaningful only inside an aggregate context — a single
    /// row can't be aggregated into anything richer than itself, and the
    /// caller almost certainly wanted the aggregate path.
    fn evaluate(_args: Vec<Value>) -> Result<BindingSets, String> {
        Err("emit_csv: use via SPARQL aggregate; direct evaluate is not supported".into())
    }

    fn aggregate_step(args: Vec<Value>, mult: u64) -> Result<(), String> {
        if args.is_empty() {
            return Err("emit_csv: expected at least one argument per row".into());
        }
        // Validate all cells are string literals before touching state so a
        // partial row can't corrupt the accumulator.
        let mut cells: Vec<String> = Vec::with_capacity(args.len());
        for a in &args {
            cells.push(string_of(a)?.to_string());
        }

        STATE.with(|s| -> Result<(), String> {
            let mut st = s.borrow_mut();
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
            // Honor multiplicity: mult=k means the row appears k times in
            // the bag; k=0 means it's filtered out. Cap at usize::MAX to
            // avoid overflow on absurd inputs.
            let k: usize = mult.try_into().unwrap_or(usize::MAX);
            for i in 0..k {
                if i + 1 == k {
                    // Last copy — move `cells` in instead of cloning again.
                    st.rows.push(cells);
                    return Ok(());
                }
                st.rows.push(cells.clone());
            }
            Ok(())
        })
    }

    fn aggregate_finish() -> Result<BindingSets, String> {
        // Drain state up front so a re-run on the same instance starts clean
        // even if the CSV writer errors below.
        let (columns, rows) = STATE.with(|s| {
            let mut st = s.borrow_mut();
            let columns = st.columns.take();
            let rows = std::mem::take(&mut st.rows);
            (columns, rows)
        });

        let csv = if columns.is_none() {
            // Zero-row aggregation: emit the empty string, mirroring what
            // csv::Writer would produce for no records.
            String::new()
        } else {
            let mut writer = csv::WriterBuilder::new()
                .has_headers(false)
                .from_writer(Vec::<u8>::new());
            for row in &rows {
                writer
                    .write_record(row.iter().map(|s| s.as_str()))
                    .map_err(|e| format!("emit_csv: write error: {}", e))?;
            }
            let bytes = writer
                .into_inner()
                .map_err(|e| format!("emit_csv: flush error: {}", e))?;
            String::from_utf8(bytes)
                .map_err(|e| format!("emit_csv: non-utf8 output: {}", e))?
        };

        Ok(BindingSets {
            vars: vec!["value_0".into()],
            rows: vec![vec![Binding {
                name: "value_0".into(),
                value: string_literal(csv),
            }]],
        })
    }

    fn cardinality_estimate(_input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        // Any non-empty input produces exactly one row; an empty input still
        // produces the single (empty-string) result row.
        Ok(Cardinality { value: 1.0, accuracy: Accuracy::Accurate })
    }

    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "emit_csv(cell_0, cell_1, ...) -> xsd:string. \
                     Aggregate. Concatenates rows into a CSV document; all \
                     cells must be string literals (cast with xsd:string(?x) \
                     if needed). Column count is fixed by the first row. Row \
                     multiplicity is honored. No header row is emitted; \
                     supply one as the first data row if you want one."
                        .to_string(),
                ),
            }]],
        }
    }
}

export!(Component);
