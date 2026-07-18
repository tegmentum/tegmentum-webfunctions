//! parse_xml — turn an XML string into rows.
//!
//! The XSPARQL problem for XML, done as a composable primitive rather than
//! as a language extension. Given an XML document as a string literal, this
//! component returns binding-sets shaped as:
//!
//!   * With one arg: iterate the root element's direct element children.
//!     One row per child.
//!   * With two args (source, element_name): iterate every element in the
//!     document whose local tag name equals `element_name` (descendants of
//!     the root, at any depth). One row per match.
//!
//! For each matched element:
//!   * Every attribute becomes a column named after the attribute's local
//!     name; the value is the attribute string as an xsd:string literal.
//!   * Direct text content (concatenation of text nodes that are immediate
//!     children of the element, trimmed) becomes column `text` — omitted
//!     if the trimmed text is empty.
//!   * Every nested child element becomes a column named after its tag;
//!     the value is the exact XML source of that child (opening tag,
//!     content, closing tag), as a string literal. Use `wf:call(parse_xml,
//!     ?nested)` recursively to unfold — matching the parse_json pattern.
//!     If a child tag repeats, the XML fragments are concatenated with a
//!     newline separator in encounter order.
//!
//! The `vars` header is the union of column names encountered across all
//! rows, in first-seen order. Rows omit bindings for columns they lack
//! (unbound in output), matching parse_json's behavior.

#[allow(warnings)]
mod bindings;

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

/// Legacy names kept as aliases so the ported property-function body
/// reads with minimum diff against the flat-world original.
type Value = WitTerm;
type Literal = WitLiteral;

/// Local shim mirroring the old `Binding` shape (`name`, `value`) so the
/// port keeps the original construction sites unchanged. Column names
/// are dropped when converting to the base world's `BindingRow`, which
/// carries only positional values.
struct Binding {
    #[allow(dead_code)]
    name: String,
    value: WitTerm,
}

/// Local shim mirroring the old `BindingSets` shape (`vars`, `rows`).
struct BindingSets {
    #[allow(dead_code)]
    vars: Vec<String>,
    rows: Vec<Vec<Binding>>,
}

fn to_binding_rows(bs: BindingSets) -> Vec<BindingRow> {
    bs.rows
        .into_iter()
        .map(|row| BindingRow {
            values: row.into_iter().map(|b| b.value).collect(),
        })
        .collect()
}

use roxmltree::{Document, Node};
use std::collections::BTreeSet;

struct Component;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const TEXT_COL: &str = "text";

fn string_literal(s: &str) -> Value {
    WitTerm::Literal(WitLiteral { value: s.into(), datatype: Some(XSD_STRING.into()), language: None })
}

fn string_of(arg: &Value) -> Result<&str, String> {
    match arg {
        WitTerm::Literal(l) => Ok(l.value.as_str()),
        _ => Err("parse_xml: argument must be a string literal".into()),
    }
}

/// Direct-child text content of an element, concatenated and trimmed.
/// Returns None if the trimmed text is empty.
fn direct_text(node: Node) -> Option<String> {
    let mut buf = String::new();
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                buf.push_str(t);
            }
        }
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// A minimal serialisation of `node` suitable for handing back to a
/// recursive parse_xml call. roxmltree 0.20 dropped the built-in source-range
/// accessor, and the crate doesn't ship a full serialiser, so we roll a
/// small one that covers the shape parse_xml itself understands:
/// opening tag with attributes, direct text, and children rendered
/// recursively. Namespaces are elided (we don't consume them on parse-in
/// either).
fn xml_source(_source: &str, node: Node) -> String {
    let tag = node.tag_name().name();
    let mut out = String::new();
    out.push('<');
    out.push_str(tag);
    for attr in node.attributes() {
        out.push(' ');
        out.push_str(attr.name());
        out.push_str("=\"");
        for c in attr.value().chars() {
            match c {
                '"' => out.push_str("&quot;"),
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                _ => out.push(c),
            }
        }
        out.push('"');
    }
    let has_children = node.children().any(|c| c.is_element() || c.is_text());
    if !has_children {
        out.push_str("/>");
        return out;
    }
    out.push('>');
    for child in node.children() {
        if child.is_element() {
            out.push_str(&xml_source(_source, child));
        } else if let Some(t) = child.text() {
            for c in t.chars() {
                match c {
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '&' => out.push_str("&amp;"),
                    _ => out.push(c),
                }
            }
        }
    }
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
    out
}

/// Ordered list of (column-name, string-value) pairs for one element row.
///
/// Column order per row:
///   1. Attributes in document order.
///   2. `text` (if the direct text content is non-empty).
///   3. Child-element tags in first-encounter order; repeated tags are
///      concatenated (their XML fragments joined by `\n`).
fn element_columns(source: &str, elem: Node) -> Vec<(String, String)> {
    let mut cols: Vec<(String, String)> = Vec::new();

    for attr in elem.attributes() {
        cols.push((attr.name().to_string(), attr.value().to_string()));
    }

    if let Some(t) = direct_text(elem) {
        cols.push((TEXT_COL.to_string(), t));
    }

    // Group children by tag, preserving first-encounter order.
    let mut children_grouped: Vec<(String, String)> = Vec::new();
    for child in elem.children().filter(|n| n.is_element()) {
        let tag = child.tag_name().name().to_string();
        let xml = xml_source(source, child);
        if let Some(existing) = children_grouped.iter_mut().find(|(t, _)| t == &tag) {
            existing.1.push('\n');
            existing.1.push_str(&xml);
        } else {
            children_grouped.push((tag, xml));
        }
    }
    cols.extend(children_grouped);

    cols
}

fn evaluate_impl(args: Vec<Value>) -> Result<BindingSets, String> {
        if args.is_empty() || args.len() > 2 {
            return Err(format!(
                "parse_xml: expected 1 or 2 args (source, [element_name]), got {}",
                args.len()
            ));
        }
        let source = string_of(&args[0])?;
        let doc = Document::parse(source)
            .map_err(|e| format!("parse_xml: invalid XML: {}", e))?;
        let root = doc.root_element();

        let matches: Vec<Node> = if args.len() == 2 {
            let name = string_of(&args[1])?;
            doc.descendants()
                .filter(|n| n.is_element() && n.tag_name().name() == name)
                .collect()
        } else {
            root.children().filter(|n| n.is_element()).collect()
        };

        // Collect column names in first-seen order across the whole result.
        let mut vars_seen: BTreeSet<String> = BTreeSet::new();
        let mut vars: Vec<String> = Vec::new();
        let mut row_cols: Vec<Vec<(String, String)>> = Vec::with_capacity(matches.len());
        for node in matches {
            let cols = element_columns(source, node);
            for (k, _) in &cols {
                if vars_seen.insert(k.clone()) {
                    vars.push(k.clone());
                }
            }
            row_cols.push(cols);
        }

        let rows: Vec<Vec<Binding>> = row_cols
            .into_iter()
            .map(|cols| {
                cols.into_iter()
                    .map(|(k, v)| Binding { name: k, value: string_literal(&v) })
                    .collect()
            })
            .collect();

        Ok(BindingSets { vars, rows })
    }

/// Filter interface stub — property-function-shaped component.
impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        Vec::new()
    }

    fn call(name: String, _args: Vec<WitTerm>) -> Result<WitTerm, String> {
        Err(format!(
            "parse_xml: unknown filter function '{name}' (use as a property function)"
        ))
    }
}

/// Aggregate interface stub.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "parse_xml: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("parse_xml: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("parse_xml: aggregate state was never constructed".into())
    }
}

impl PropertyFunctionGuest for Component {
    fn register_property_functions() -> Vec<PropertyDescriptor> {
        vec![PropertyDescriptor {
            name: "parse_xml".to_string(),
            subject_arity: 0,
            object_arity: 0,
        }]
    }

    fn evaluate(
        name: String,
        subjects: Vec<WitTerm>,
        objects: Vec<WitTerm>,
    ) -> Result<Vec<BindingRow>, String> {
        match name.as_str() {
            "parse_xml" => {
                let mut args = subjects;
                args.extend(objects);
                let bs = evaluate_impl(args)?;
                Ok(to_binding_rows(bs))
            }
            other => Err(format!("parse_xml: unknown property function '{other}'")),
        }
    }
}

bindings::export!(Component with_types_in bindings);

