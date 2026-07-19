//! wf_fetch — HTTP GET + parse RDF + emit quads at a named sink.
//!
//! Signature: `extension.call("fetch", [<url>, <sink-name>])`
//!    → rdf:JSON literal `{"fetched": <bytes>, "emitted": <n>}`.
//!
//! Redesigned from the Stardog-era `wf_fetch` (M1 Q2 + Q3 decisions).
//! The pre-migration crate was a SQL-select-over-sink guest whose whole
//! contract was "SELECT <descriptor cols> FROM <sink table> <user WHERE
//! tail>". That role now belongs to `sink-query-callbacks::execute-sink-select`
//! — a callable at the substrate boundary, not a guest — and `wf_sql`
//! is retired in the sibling commit. This crate becomes something the
//! substrate actually needs a guest for: pulling raw RDF over HTTP and
//! landing it as typed quads at a sink the host has registered.
//!
//! Content parsing: MVP supports Turtle, N-Triples, and N-Quads via
//! `oxttl` (pure-Rust, wasm32-wasip2-clean). Selection is by response
//! Content-Type header — text/turtle → Turtle, application/n-triples
//! → N-Triples, application/n-quads → N-Quads. Anything else (JSON,
//! XML, unknown) is attempted as Turtle first (per the memo's failure
//! path for `application/octet-stream`-shaped bodies) and reported as
//! a parse error if that fails.
//!
//! Downstream SPARQL queries over the fetched data run against the
//! sink via `sink-query-callbacks::execute-sink-select` in a separate
//! SPARQL query — this guest's job stops at "quads are in the sink".
//!
//! API break vs. legacy: the descriptor-json / sql-tail argument shape
//! is dropped. Callers that previously wrote
//!     wf:call(<wf_fetch.wasm>, "<descriptor-json>", "<WHERE tail>")
//! now write two separate SPARQL fragments:
//!     BIND(wf:fetch("<url>", "<sink-name>") AS ?fetch_result)
//!     SERVICE <sink:<sink-name>> { <BGP over fetched quads> }
//! Signalled loudly in the return type: an rdf:JSON summary instead of
//! a projection matching the descriptor's columns.

#[allow(warnings)]
mod bindings;

use serde_json::json;

use bindings::exports::tegmentum::webfunction::aggregate::{
    AggregateDescriptor, AggregateState, Guest as AggregateGuest, GuestAggregateState,
};
use bindings::exports::tegmentum::webfunction::extension::{
    FunctionDescriptor, Guest as ExtensionGuest,
};
use bindings::exports::tegmentum::webfunction::property_function::{
    BindingRow, Guest as PropertyFunctionGuest, PropertyDescriptor,
};
use bindings::tegmentum::webfunction::http_callbacks::{
    self as hc, HttpError, HttpHeader,
};
use bindings::tegmentum::webfunction::sink_callbacks::{self as sc, SinkError};
use bindings::tegmentum::webfunction::types::{
    Literal as WitLiteral, Quad as WitQuad, Term as WitTerm,
};

use oxrdf::{
    BlankNode, GraphName, NamedNode, NamedOrBlankNode, Quad as OxQuad, Term as OxTerm,
};
use oxttl::{NQuadsParser, NTriplesParser, TurtleParser};

struct Component;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";

// ---------------------------------------------------------------------------
// Content-type sniffing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Syntax {
    Turtle,
    NTriples,
    NQuads,
}

/// Pick a parser from the response Content-Type header. Anything the
/// header doesn't identify — including `application/octet-stream`,
/// missing Content-Type, or an unrelated media type — defaults to
/// Turtle. If Turtle can't parse it either, the parse loop surfaces
/// the syntax error to the caller.
fn syntax_from_content_type(headers: &[HttpHeader]) -> Syntax {
    for h in headers {
        // http-callbacks lower-cases header names on receipt, per the
        // interface note in host-callbacks.wit; explicit lower here so
        // a host implementation that forgets to lower doesn't slip a
        // capitalised value past this match.
        if h.name.eq_ignore_ascii_case("content-type") {
            let raw = h.value.to_ascii_lowercase();
            // Content-Type carries parameters after `;` — strip them
            // before matching so `text/turtle; charset=utf-8` still
            // routes to Turtle.
            let media = raw.split(';').next().unwrap_or("").trim();
            return match media {
                "application/n-triples" | "text/plain" => Syntax::NTriples,
                "application/n-quads" => Syntax::NQuads,
                "text/turtle"
                | "application/turtle"
                | "application/x-turtle"
                | "text/n3"
                | "text/rdf+n3"
                | "application/trig" => Syntax::Turtle,
                // Non-RDF (JSON/XML) or unknown — try Turtle. If the
                // body isn't parseable as Turtle the guest returns an
                // error to the caller (memo failure-path §1).
                _ => Syntax::Turtle,
            };
        }
    }
    Syntax::Turtle
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn map_http_err(e: HttpError) -> String {
    match e {
        HttpError::Network(m) => format!("http-callbacks network: {m}"),
        HttpError::Status(code) => format!("http-callbacks non-2xx status: {code}"),
        HttpError::InvalidRequest(m) => format!("http-callbacks invalid-request: {m}"),
        HttpError::NotPermitted(m) => format!("http-callbacks not-permitted: {m}"),
    }
}

fn map_sink_err(e: SinkError) -> String {
    match e {
        SinkError::NoSuchSink(m) => format!("sink-callbacks no-such-sink: {m}"),
        SinkError::SchemaViolation(m) => format!("sink-callbacks schema-violation: {m}"),
        SinkError::BackendError(m) => format!("sink-callbacks backend-error: {m}"),
        SinkError::NotPermitted(m) => format!("sink-callbacks not-permitted: {m}"),
    }
}

// ---------------------------------------------------------------------------
// oxrdf -> WIT translation
// ---------------------------------------------------------------------------

fn wit_from_named(n: &NamedNode) -> WitTerm {
    WitTerm::NamedNode(n.as_str().to_string())
}

fn wit_from_blank(b: &BlankNode) -> WitTerm {
    WitTerm::BlankNode(b.as_str().to_string())
}

fn wit_from_subject(s: &NamedOrBlankNode) -> WitTerm {
    match s {
        NamedOrBlankNode::NamedNode(n) => wit_from_named(n),
        NamedOrBlankNode::BlankNode(b) => wit_from_blank(b),
    }
}

fn wit_from_term(t: &OxTerm) -> WitTerm {
    match t {
        OxTerm::NamedNode(n) => wit_from_named(n),
        OxTerm::BlankNode(b) => wit_from_blank(b),
        OxTerm::Literal(l) => {
            // Language-tagged literals use rdf:langString datatype
            // implicitly per RDF 1.1. The WIT surface carries `datatype`
            // as an optional; we set it explicitly only for typed
            // literals with a non-default datatype. Plain xsd:string
            // literals leave datatype = None so the WIT-side default
            // handling applies.
            let value = l.value().to_string();
            let language = l.language().map(str::to_string);
            let datatype_iri = l.datatype().as_str();
            let datatype = if language.is_some()
                || datatype_iri == "http://www.w3.org/2001/XMLSchema#string"
            {
                None
            } else {
                Some(datatype_iri.to_string())
            };
            WitTerm::Literal(WitLiteral {
                value,
                datatype,
                language,
            })
        }
        #[allow(unreachable_patterns)]
        _ => WitTerm::NamedNode(String::new()),
    }
}

fn graph_iri_from(g: &GraphName) -> Option<String> {
    match g {
        GraphName::NamedNode(n) => Some(n.as_str().to_string()),
        GraphName::BlankNode(b) => Some(format!("_:{}", b.as_str())),
        GraphName::DefaultGraph => None,
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

fn wit_quad_from_ox_quad(q: &OxQuad) -> WitQuad {
    WitQuad {
        subject: wit_from_subject(&q.subject),
        predicate: wit_from_named(&q.predicate),
        object: wit_from_term(&q.object),
        graph: graph_iri_from(&q.graph_name),
    }
}

fn wit_quad_from_triple(
    subject: NamedOrBlankNode,
    predicate: NamedNode,
    object: OxTerm,
) -> WitQuad {
    WitQuad {
        subject: wit_from_subject(&subject),
        predicate: wit_from_named(&predicate),
        object: wit_from_term(&object),
        // Turtle / N-Triples have no graph term — everything lands in
        // the sink's default graph. Callers who need to segregate by
        // graph use N-Quads or pre-scope their sink adapter.
        graph: None,
    }
}

// ---------------------------------------------------------------------------
// Parse dispatch
// ---------------------------------------------------------------------------

fn parse_body(syntax: Syntax, body: &str) -> Result<Vec<WitQuad>, String> {
    let bytes = body.as_bytes();
    match syntax {
        Syntax::Turtle => {
            let mut out = Vec::new();
            for t in TurtleParser::new().for_slice(bytes) {
                let t = t.map_err(|e| format!("wf_fetch: turtle parse: {e}"))?;
                out.push(wit_quad_from_triple(t.subject, t.predicate, t.object));
            }
            Ok(out)
        }
        Syntax::NTriples => {
            let mut out = Vec::new();
            for t in NTriplesParser::new().for_slice(bytes) {
                let t = t.map_err(|e| format!("wf_fetch: n-triples parse: {e}"))?;
                out.push(wit_quad_from_triple(t.subject, t.predicate, t.object));
            }
            Ok(out)
        }
        Syntax::NQuads => {
            let mut out = Vec::new();
            for q in NQuadsParser::new().for_slice(bytes) {
                let q = q.map_err(|e| format!("wf_fetch: n-quads parse: {e}"))?;
                out.push(wit_quad_from_ox_quad(&q));
            }
            Ok(out)
        }
    }
}

// ---------------------------------------------------------------------------
// Guest entrypoint
// ---------------------------------------------------------------------------

fn arg_as_string(term: &WitTerm, name: &str) -> Result<String, String> {
    match term {
        WitTerm::Literal(l) => Ok(l.value.clone()),
        WitTerm::NamedNode(iri) => Ok(iri.clone()),
        other => Err(format!(
            "wf_fetch: expected {name} as string/IRI, got {other:?}"
        )),
    }
}

fn json_literal(s: &str) -> WitTerm {
    WitTerm::Literal(WitLiteral {
        value: s.into(),
        datatype: Some(RDF_JSON.into()),
        language: None,
    })
}

fn fetch_impl(args: &[WitTerm]) -> Result<WitTerm, String> {
    let url = args
        .first()
        .ok_or_else(|| "wf_fetch: expected two args (url, sink-name)".to_string())
        .and_then(|t| arg_as_string(t, "first arg (url)"))?;
    let sink_name = args
        .get(1)
        .ok_or_else(|| "wf_fetch: expected two args (url, sink-name)".to_string())
        .and_then(|t| arg_as_string(t, "second arg (sink-name)"))?;

    // Announce the content types we can parse. Servers that honour
    // Accept-content negotiation will hand us the syntax we understand;
    // servers that ignore it we handle via the Content-Type sniff.
    let headers = vec![HttpHeader {
        name: "accept".into(),
        value: "text/turtle, application/n-triples, application/n-quads;q=0.9".into(),
    }];
    let response = hc::http_get(&url, &headers).map_err(map_http_err)?;
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "wf_fetch: HTTP {} from {}",
            response.status, url
        ));
    }

    let fetched_bytes = response.body.len();
    let syntax = syntax_from_content_type(&response.headers);
    let quads = parse_body(syntax, &response.body)?;

    if quads.is_empty() {
        let out = json!({
            "fetched": fetched_bytes,
            "emitted": 0u32,
        });
        return Ok(json_literal(&out.to_string()));
    }

    // Amortise sink transaction cost with a single batch emit — matches
    // the batch6 (`wf_materialize`, `wf_materialize_list`) convention.
    let accepted = sc::emit_quads(&sink_name, &quads).map_err(map_sink_err)?;

    let out = json!({
        "fetched": fetched_bytes,
        "emitted": accepted,
    });
    Ok(json_literal(&out.to_string()))
}

impl ExtensionGuest for Component {
    fn register() -> Vec<FunctionDescriptor> {
        vec![FunctionDescriptor {
            name: "fetch".into(),
            min_arity: 2,
            max_arity: Some(2),
        }]
    }

    fn call(name: String, args: Vec<WitTerm>) -> Result<WitTerm, String> {
        match name.as_str() {
            "fetch" => fetch_impl(&args),
            other => Err(format!("wf_fetch: unknown function '{other}'")),
        }
    }
}

impl AggregateGuest for Component {
    type AggregateState = UnreachableState;

    fn register_aggregates() -> Vec<AggregateDescriptor> {
        Vec::new()
    }

    fn new_aggregate(name: String) -> Result<AggregateState, String> {
        Err(format!(
            "wf_fetch: unknown aggregate '{name}' (this component provides none)"
        ))
    }
}

pub struct UnreachableState;

impl GuestAggregateState for UnreachableState {
    fn step(&self, _args: Vec<WitTerm>) -> Result<(), String> {
        Err("wf_fetch: aggregate state was never constructed".into())
    }

    fn finish(&self) -> Result<WitTerm, String> {
        Err("wf_fetch: aggregate state was never constructed".into())
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
            "wf_fetch: unknown property function '{name}' (this component provides none)"
        ))
    }
}

bindings::export!(Component with_types_in bindings);

// ---------------------------------------------------------------------------
// Unit tests — native-mode syntax dispatch + parse
// ---------------------------------------------------------------------------
//
// Only the pieces that don't cross the WIT boundary are testable in
// native mode: `syntax_from_content_type`, `parse_body`, and the
// oxrdf → WIT translation shape. The `fetch_impl` end-to-end path
// requires HTTP + sink callbacks the host wires in at instantiation
// time; smoke coverage for it lives with the host-callbacks-impl
// crate, not in this guest.
#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(name: &str, value: &str) -> HttpHeader {
        HttpHeader {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn content_type_sniff_turtle() {
        let s = syntax_from_content_type(&[hdr("content-type", "text/turtle")]);
        assert!(matches!(s, Syntax::Turtle));
    }

    #[test]
    fn content_type_sniff_turtle_with_charset() {
        let s = syntax_from_content_type(&[hdr("content-type", "text/turtle; charset=utf-8")]);
        assert!(matches!(s, Syntax::Turtle));
    }

    #[test]
    fn content_type_sniff_ntriples() {
        let s = syntax_from_content_type(&[hdr("content-type", "application/n-triples")]);
        assert!(matches!(s, Syntax::NTriples));
    }

    #[test]
    fn content_type_sniff_nquads() {
        let s = syntax_from_content_type(&[hdr("content-type", "application/n-quads")]);
        assert!(matches!(s, Syntax::NQuads));
    }

    #[test]
    fn content_type_sniff_missing_defaults_turtle() {
        let s = syntax_from_content_type(&[]);
        assert!(matches!(s, Syntax::Turtle));
    }

    #[test]
    fn content_type_sniff_octet_stream_defaults_turtle() {
        let s = syntax_from_content_type(&[hdr("content-type", "application/octet-stream")]);
        assert!(matches!(s, Syntax::Turtle));
    }

    #[test]
    fn parse_ntriples_two_triples() {
        let body = r#"<http://ex/s1> <http://ex/p> "v1" .
<http://ex/s2> <http://ex/p> "v2" .
"#;
        let quads = parse_body(Syntax::NTriples, body).expect("parses");
        assert_eq!(quads.len(), 2);
        match &quads[0].subject {
            WitTerm::NamedNode(iri) => assert_eq!(iri, "http://ex/s1"),
            other => panic!("expected named-node subject, got {other:?}"),
        }
        assert!(quads[0].graph.is_none());
    }

    #[test]
    fn parse_turtle_one_triple() {
        let body = r#"@prefix ex: <http://ex/> .
ex:s ex:p "v" .
"#;
        let quads = parse_body(Syntax::Turtle, body).expect("parses");
        assert_eq!(quads.len(), 1);
        match &quads[0].predicate {
            WitTerm::NamedNode(iri) => assert_eq!(iri, "http://ex/p"),
            other => panic!("expected named-node predicate, got {other:?}"),
        }
    }

    #[test]
    fn parse_nquads_carries_graph() {
        let body = r#"<http://ex/s> <http://ex/p> "v" <http://ex/g> .
"#;
        let quads = parse_body(Syntax::NQuads, body).expect("parses");
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].graph.as_deref(), Some("http://ex/g"));
    }

    #[test]
    fn parse_syntax_error_surfaces() {
        let body = "this is not turtle";
        let result = parse_body(Syntax::Turtle, body);
        assert!(result.is_err(), "expected parse error, got {result:?}");
    }

    #[test]
    fn typed_literal_carries_datatype() {
        let body = r#"<http://ex/s> <http://ex/p> "42"^^<http://www.w3.org/2001/XMLSchema#integer> .
"#;
        let quads = parse_body(Syntax::NTriples, body).expect("parses");
        assert_eq!(quads.len(), 1);
        match &quads[0].object {
            WitTerm::Literal(l) => {
                assert_eq!(l.value, "42");
                assert_eq!(
                    l.datatype.as_deref(),
                    Some("http://www.w3.org/2001/XMLSchema#integer")
                );
                assert!(l.language.is_none());
            }
            other => panic!("expected literal object, got {other:?}"),
        }
    }

    #[test]
    fn plain_string_literal_omits_datatype() {
        let body = r#"<http://ex/s> <http://ex/p> "hello" .
"#;
        let quads = parse_body(Syntax::NTriples, body).expect("parses");
        match &quads[0].object {
            WitTerm::Literal(l) => {
                assert_eq!(l.value, "hello");
                // xsd:string is the RDF 1.1 default; suppressed at the
                // WIT boundary per types.wit's `datatype = none ==
                // xsd:string` convention.
                assert!(l.datatype.is_none());
            }
            other => panic!("expected literal, got {other:?}"),
        }
    }
}
