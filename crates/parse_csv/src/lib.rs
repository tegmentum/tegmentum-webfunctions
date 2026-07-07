//! parse_csv — turn a CSV string into rows.
//!
//! The XSPARQL problem for CSV, done as a composable primitive rather than
//! as a language extension. Given a CSV document as a string literal, this
//! component returns binding-sets shaped as:
//!
//!   * First row is treated as the header; each column becomes a variable.
//!   * Subsequent rows are one Binding per column, all as xsd:string
//!     literals. Consumers can `xsd:integer(?x)` etc. if they want typed
//!     values — CSV has no native type information.
//!
//! Optional second arg: a single-character literal for the delimiter
//! (defaults to ","). Third arg reserved for future use (quote char, etc.).

wit_bindgen::generate!({
    world: "webfunction",
    path: "wit",
});

use stardog::webfunction::types::{Accuracy, Binding, Literal};

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

fn string_literal(s: &str) -> Value {
    Value::Literal(Literal { label: s.into(), datatype: XSD_STRING.into(), lang: None })
}

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg {
        Value::Literal(l) => Ok(l.label.as_str()),
        _ => Err("parse_csv: argument must be a string literal".into()),
    }
}

impl Guest for Component {
    fn evaluate(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.is_empty() || args.len() > 2 {
            return Err(format!(
                "parse_csv: expected 1 or 2 args (source, [delimiter]), got {}",
                args.len()
            ));
        }
        let source = string_of(&args[0])?;
        let delimiter = if args.len() == 2 {
            let d = string_of(&args[1])?;
            match d.as_bytes() {
                [b] => *b,
                _ => return Err("parse_csv: delimiter must be a single-byte character".into()),
            }
        } else {
            b','
        };

        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(true)
            .from_reader(source.as_bytes());

        let vars: Vec<String> = reader
            .headers()
            .map_err(|e| format!("parse_csv: cannot read headers: {}", e))?
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut rows: Vec<Vec<Binding>> = Vec::new();
        for record in reader.records() {
            let record = record.map_err(|e| format!("parse_csv: record error: {}", e))?;
            let mut bindings = Vec::with_capacity(vars.len());
            for (i, field) in record.iter().enumerate() {
                if i >= vars.len() { break; }
                bindings.push(Binding {
                    name: vars[i].clone(),
                    value: string_literal(field),
                });
            }
            rows.push(bindings);
        }
        Ok(BindingSets { vars, rows })
    }

    fn aggregate_step(_args: Vec<Value>, _mult: u64) -> Result<(), String> {
        Err("parse_csv: aggregate not applicable".into())
    }
    fn aggregate_finish() -> Result<BindingSets, String> {
        Err("parse_csv: aggregate not applicable".into())
    }
    fn cardinality_estimate(input: Cardinality, _args: Vec<Value>) -> Result<Cardinality, String> {
        Ok(Cardinality { value: input.value.max(1.0), accuracy: Accuracy::Injected })
    }
    fn doc() -> BindingSets {
        BindingSets {
            vars: vec!["doc".into()],
            rows: vec![vec![Binding {
                name: "doc".into(),
                value: string_literal(
                    "parse_csv(source, [delimiter=',']) -> binding-sets. \
                     First row is the header; each column becomes a variable. \
                     Values are xsd:string literals — cast to numeric types \
                     with xsd:integer(?x) etc. in the query if needed."),
            }]],
        }
    }
}

export!(Component);
