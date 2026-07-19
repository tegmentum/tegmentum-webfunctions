# wf_sql retirement

The `wf_sql` crate is retired.

`wf_sql`'s entire purpose was "arbitrary SQL against a sink, returned as
SPARQL binding-sets" — a thin wrapper around the Stardog-era
`sink-open` / `sink-execute` / `sink-close` host triple, callable from a
`SERVICE <wf:call>` envelope so a SPARQL query could join, filter, or
CONSTRUCT against SQL rows.

The R2 sink-read landing (webfunction-wit commit `463082f`) introduces
`sink-query-callbacks::execute-sink-select` — a substrate-level surface
that takes a SPARQL query, evaluates it against a named sink's virtual
graph, and returns bindings directly. Any consumer that would previously
have written

```sparql
SERVICE <wf:call> {
  wf:target <ipfs://.../wf_sql.wasm> .
  wf:arg "sqlite:///data/mv.db#person" .
  wf:arg "SELECT id, name FROM person WHERE age > 30"
}
```

now writes the same intent as SPARQL against the sink:

```sparql
SERVICE <sink:person-store> {
  ?s :name ?name ; :age ?age . FILTER(?age > 30)
}
```

with `execute-sink-select` dispatching underneath. The sink adapter
owns the SQL translation; the guest layer sees only SPARQL.

Shipping a `wf_sql` analogue on the substrate would just re-wrap a
callback the substrate already exposes — dead weight. External callers
migrate to `execute-sink-select`; no source-compatibility shim ships.

Retired as of the wave that includes this document (commit landing this
file). `wf_fetch` is redesigned in the same wave to fold into the
substrate's ExtensionGuest pattern using `http-callbacks` + `emit-quads`
rather than SQL against a sink.
