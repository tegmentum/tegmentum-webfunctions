//! Reference SPARQL extension proving the Phase 1 host-callback
//! round-trip. Exports one filter function, `expand-neighborhood`,
//! that dispatches through the host's `graph-callbacks::execute-query`
//! to fetch a node's outgoing neighbors and returns them as a
//! comma-separated string literal.
//!
//! Non-goal for this crate: any real graph-analysis heuristic. The
//! function is the smallest thing that can honestly exercise the
//! guest-imports / host-implements direction of the substrate
//! boundary. A `wf_sagegraph`-shape consumer of `graph-callbacks`
//! would fold the returned bindings through structural features +
//! ONNX; we return a string so the smoke test's assertion is
//! trivial.

#[allow(warnings)]
mod bindings;

use bindings::exports::tegmentum::webfunction::aggregate::{
    AggregateDescriptor, AggregateState, Guest as AggregateGuest,
    GuestAggregateState,
};
use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::exports::tegmentum::webfunction::property_function::{
    BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
};
use bindings::tegmentum::webfunction::graph_callbacks::{
    self as gc, QueryResult as CallbackQueryResult,
};
use bindings::tegmentum::webfunction::types::{
    Literal as WitLiteral, Term as WitTerm,
};

struct Component;

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "expand-neighborhood".to_string(),
            min_arity: 1,
            max_arity: Some(1),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "expand-neighborhood" => expand_neighborhood(&args),
            other => Err(format!(
                "example-graph-callback-extension: unknown function '{other}'"
            )),
        }
    }
}

/// Aggregate interface stub. This component has no aggregates.
impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "example-graph-callback-extension: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("example-graph-callback-extension: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("example-graph-callback-extension: aggregate state was never constructed".into())
    }
}

/// Property-function interface stub. This component has no property
/// functions.
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
            "example-graph-callback-extension: unknown property function '{name}' (this component provides none)"
        ))
    }
}

/// `expand-neighborhood(iri)` — issue `SELECT ?neighbor WHERE {
/// <iri> ?p ?neighbor }` through the host's `graph-callbacks::
/// execute-query`, return the bound neighbor IRIs concatenated with
/// commas as a plain string literal.
///
/// Not intended as production graph analysis; the point is the
/// round-trip. A production consumer would return structured data.
fn expand_neighborhood(args: &[WitTerm]) -> Result<WitTerm, String> {
    let [arg] = args else {
        return Err(format!(
            "expand-neighborhood: expected 1 argument, got {}",
            args.len()
        ));
    };
    let iri = match arg {
        WitTerm::NamedNode(i) => i.as_str(),
        WitTerm::BlankNode(_) => {
            return Err("expand-neighborhood: argument must be an IRI, got blank node".into())
        }
        WitTerm::Literal(_) => {
            return Err("expand-neighborhood: argument must be an IRI, got literal".into())
        }
    };

    // Escape angle brackets inside the IRI so a malicious argument
    // cannot break out of the SPARQL literal position. In practice
    // the host's SPARQL parser is what enforces IRI shape; the guest
    // does a naive check here so the callback returns
    // `syntax-error(...)` cleanly rather than dispatching a malformed
    // query.
    if iri.contains('<') || iri.contains('>') || iri.contains('"') {
        return Err(format!(
            "expand-neighborhood: IRI contains angle bracket or quote which cannot be interpolated safely: {iri}"
        ));
    }

    let sparql = format!(
        "SELECT ?neighbor WHERE {{ <{iri}> ?p ?neighbor }} ORDER BY ?neighbor"
    );

    let result = gc::execute_query(&sparql).map_err(|e| {
        // Fold the typed error variant into a string for the outer
        // `call` return. A future revision could preserve the arm.
        match e {
            gc::GraphCallError::SyntaxError(m) => format!("syntax-error: {m}"),
            gc::GraphCallError::BackendError(m) => format!("backend-error: {m}"),
            gc::GraphCallError::NotPermitted(m) => format!("not-permitted: {m}"),
        }
    })?;

    let bindings = match result {
        CallbackQueryResult::Bindings(b) => b,
        CallbackQueryResult::Quads(_) => {
            return Err(
                "expand-neighborhood: SELECT unexpectedly returned quads".into(),
            )
        }
        CallbackQueryResult::Boolean(_) => {
            return Err(
                "expand-neighborhood: SELECT unexpectedly returned boolean".into(),
            )
        }
    };

    // Flat-list layout per host-callbacks.wit's `bindings` shape: the
    // query projects a single variable (`neighbor`), so every
    // binding entry in the returned list corresponds to one solution
    // row. Render each bound term as its N-Triples-flavoured
    // lexical form.
    let mut rendered: Vec<String> = Vec::with_capacity(bindings.len());
    for b in &bindings {
        if b.variable != "neighbor" {
            continue;
        }
        rendered.push(render_term(&b.value));
    }

    Ok(WitTerm::Literal(WitLiteral {
        value: rendered.join(","),
        datatype: None,
        language: None,
    }))
}

/// N-Triples-flavoured rendering for a term. Kept intentionally
/// simple; guests that need proper N-Triples escaping should build
/// their own writer.
fn render_term(t: &WitTerm) -> String {
    match t {
        WitTerm::NamedNode(iri) => format!("<{iri}>"),
        WitTerm::BlankNode(id) => format!("_:{id}"),
        WitTerm::Literal(l) => {
            if let Some(lang) = &l.language {
                format!("\"{}\"@{lang}", l.value)
            } else if let Some(dt) = &l.datatype {
                format!("\"{}\"^^<{dt}>", l.value)
            } else {
                format!("\"{}\"", l.value)
            }
        }
    }
}

bindings::export!(Component with_types_in bindings);
