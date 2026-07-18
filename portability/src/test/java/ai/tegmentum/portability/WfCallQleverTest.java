package ai.tegmentum.portability;

import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.QuerySolution;
import org.apache.jena.query.ResultSet;
import org.apache.jena.sparql.exec.http.QueryExecutionHTTPBuilder;
import org.junit.BeforeClass;
import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * QLever wf:call SPARQL-level proof against qlever-wf:0.5.0. This tag adds
 * the {@code execute-query} host callback via HTTP loopback back to the
 * same qlever-server: guests that call {@code host::execute_query()} now
 * post their inner SPARQL string against the outer server on
 * {@code QLEVER_LOOPBACK_PORT} and get a real SPARQL 1.1 Results reply.
 *
 * <p>0.5.0 changes vs. 0.4.0:
 * <ul>
 *   <li>{@code WasmRuntime.cpp} wires the runtime's {@code execute_query}
 *       callback pointer instead of leaving it NULL. The callback opens a
 *       plain-socket HTTP/1.1 POST to {@code 127.0.0.1:$QLEVER_LOOPBACK_PORT
 *       /query}, prepends a {@code VALUES (?v) { (&lt;term&gt;) }} clause when
 *       initial bindings are supplied, and converts the SPARQL Results
 *       JSON reply into the WIT binding-sets shape the runtime expects.</li>
 *   <li>Docker: {@code /etc/profile.d/qlever.sh} defaults
 *       {@code QLEVER_LOOPBACK_PORT=7001}; the entrypoint mirrors any
 *       {@code -e QLEVER_LOOPBACK_PORT=X} into a profile fragment so the
 *       login shell inherits it despite {@code su -}'s environment
 *       scrub.</li>
 *   <li>Concurrency: qlever-server MUST run with
 *       {@code --num-simultaneous-queries} &gt;= 2 (recommended 4). With
 *       {@code -j 1} the outer query's worker thread would block inside
 *       the callback while the nested SPARQL request queued behind it.</li>
 * </ul>
 *
 * <p>Not-yet-wired imports: {@code prepare-query}, {@code run-prepared},
 * {@code execute-update}, and {@code follow-predicate} are still stubbed
 * inside the Rust qlever-wf-runtime crate; the C ABI has reserved slots
 * for them but the runtime side hasn't been extended to route through the
 * embedder's callback table. wf_tree.wasm, wf_tree_rows.wasm, and
 * adjacency_tree.wasm therefore still fail cleanly with an
 * err&lt;string&gt; message on QLever until the runtime lands
 * prepare/run-prepared. Track: runtime v0.4+.
 *
 * <p>Test guest: {@code debug_callback_depth.wasm}. It is component-mode,
 * callback-free (the runtime supplies {@code callback-depth} as a fallback
 * that returns 0 outside of nested re-entry), and its {@code evaluate}
 * returns a single {@code xsd:integer} literal with value {@code "0"}.
 * That is enough to prove the whole pipeline end-to-end without needing a
 * bulk-loaded index or the not-yet-wired imports.
 *
 * <p>Boot manually with (from a directory containing debug_callback_depth.wasm
 * and any minimal index):
 * <pre>
 *   docker run --rm -d --name qlever-wf --platform linux/arm64 -p 7001:7001 \
 *       -e UID=$(id -u) -e GID=$(id -g) \
 *       -e QLEVER_LOOPBACK_PORT=7001 \
 *       -v $(pwd):/data -w /data \
 *       qlever-wf:0.5.0 \
 *       qlever-server --port 7001 --num-simultaneous-queries 4 &lt;index-basename&gt;
 * </pre>
 */
public class WfCallQleverTest {

    private static final String SPARQL_URL = System.getProperty(
            "qlever.sparql.url", "http://localhost:7001/");
    // Component-mode guest that returns a single xsd:integer literal "0"
    // and depends on no embedder-provided host callbacks.
    private static final String WASM_URL_IN_QUERY = System.getProperty(
            "qlever.wasm.url", "file:///opt/wasm/debug_callback_depth.wasm");

    private static final String XSD_INTEGER = "http://www.w3.org/2001/XMLSchema#integer";

    @BeforeClass
    public static void probe() {
        assumeTrue("QLever SPARQL endpoint not reachable at " + SPARQL_URL,
                pingSparql());
    }

    private static boolean pingSparql() {
        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString("ASK {}")
                .build()) {
            qe.execAsk();
            return true;
        } catch (Throwable t) {
            System.err.println("[wf-test] QLever SPARQL probe failed at "
                + SPARQL_URL + ": " + t.getClass().getName() + " — " + t.getMessage());
            return false;
        }
    }

    /**
     * BIND (wf:call(&lt;debug_callback_depth.wasm&gt;) AS ?depth) → "0"^^xsd:integer.
     *
     * <p>Regression: v0.3.0 would return UNDEF because the C++ WfCallExpression
     * always emitted an empty JSON payload AND used a naive
     * {@code "value":"..."} substring extractor, which cannot match a nested
     * {@code "value":{"literal":{"label":"0","datatype":...}}} shape at all.
     */
    @Test
    public void wfCallReturnsRealBoundValue() {
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?result WHERE {\n" +
            "  BIND (wf:call(<" + WASM_URL_IN_QUERY + ">) AS ?result)\n" +
            "}";

        final long t0 = System.nanoTime();
        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            final ResultSet rs = qe.execSelect();
            assertThat(rs.hasNext()).isTrue();
            final QuerySolution row = rs.next();
            assertThat(row.contains("result"))
                .as("wf:call must bind a non-UNDEF value")
                .isTrue();
            final String lex = row.getLiteral("result").getLexicalForm();
            final String dt = row.getLiteral("result").getDatatypeURI();
            System.out.printf("QLever wf:call (warm): %d ms, result='%s'^^<%s>%n",
                (System.nanoTime() - t0) / 1_000_000L, lex, dt);
            assertThat(lex).isEqualTo("0");
            assertThat(dt).isEqualTo(XSD_INTEGER);
        }
    }

    // ---- 0.5.0 execute-query loopback proof ------------------------------

    /**
     * URL of a guest that calls {@code host::execute_query()} exactly once
     * and returns the first row's first cell as an xsd:string. Not shipped
     * from webfunctions/crates/ — the crates dir was carved off
     * from this task's edit scope, so we build the fixture out-of-tree at
     * {@code scratchpad/debug_execute_query/} and mount its wasm into the
     * container. See the 0.5.0 boot-recipe comment on the class Javadoc.
     *
     * <p>The default value assumes the caller has mounted
     * {@code debug_execute_query.wasm} at {@code /opt/wasm/}. Override
     * via {@code -Dqlever.wasm.execute_query.url=...}.
     */
    private static final String WASM_URL_EXECUTE_QUERY = System.getProperty(
            "qlever.wasm.execute_query.url",
            "file:///opt/wasm/debug_execute_query.wasm");

    /**
     * End-to-end callback proof for qlever-wf:0.5.0. The outer query calls
     * {@code wf:call(<debug_execute_query.wasm>, "SELECT ...")}; the guest
     * loops back to qlever-server via HTTP, reads the first cell, and
     * projects it as an xsd:string. If the string it returns is the
     * subject of a triple actually stored in the index, the callback
     * demonstrably traversed the whole path:
     * outer query → WfCallExpression → wasm evaluate →
     * host::execute_query → C++ callback → HTTP POST → qlever-server →
     * SPARQL results → back through the runtime → outer BIND.
     *
     * <p>Prerequisite: the container is booted against a tree.ttl containing
     * at least one triple {@code <urn:root> <urn:hasChild> <urn:a>} and
     * qlever-server is running with {@code --num-simultaneous-queries >= 2}
     * so the callback thread and the nested request can coexist.
     * Skipped (via assumeTrue) if
     * {@code -Dqlever.wf.callback.enabled=true} is not set — the test
     * cannot know whether the container was booted with a suitable
     * QLEVER_LOOPBACK_PORT.
     */
    @Test
    public void executeQueryCallbackLoopsBackToIndex() {
        assumeTrue(
            "Enable with -Dqlever.wf.callback.enabled=true when the "
            + "container was booted with QLEVER_LOOPBACK_PORT set and "
            + "debug_execute_query.wasm mounted at /opt/wasm/",
            Boolean.getBoolean("qlever.wf.callback.enabled"));

        // Inner SPARQL: find any child of urn:root. The guest projects the
        // first cell as a string, so if any triple loaded matches, the
        // outer BIND resolves to a non-empty string literal.
        final String innerSparql =
            "SELECT ?child WHERE { <urn:root> <urn:hasChild> ?child }";
        final String outer =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?result WHERE {\n" +
            "  BIND (wf:call(<" + WASM_URL_EXECUTE_QUERY + ">, \"" +
                    innerSparql.replace("\"", "\\\"") + "\") AS ?result)\n" +
            "}";

        final long t0 = System.nanoTime();
        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(outer)
                .build()) {
            final ResultSet rs = qe.execSelect();
            assertThat(rs.hasNext())
                .as("qlever must project a row for wf:call(execute-query)")
                .isTrue();
            final QuerySolution row = rs.next();
            assertThat(row.contains("result"))
                .as("wf:call → execute-query round trip must bind a value")
                .isTrue();
            final String lex = row.getLiteral("result").getLexicalForm();
            System.out.printf("QLever wf:call → execute-query round trip: "
                + "%d ms, result='%s'%n",
                (System.nanoTime() - t0) / 1_000_000L, lex);
            // Any non-empty lexical proves the callback path resolved to
            // real index data; the exact value depends on load order.
            assertThat(lex)
                .as("execute-query returned an empty first cell — either the "
                    + "index has no <urn:root> <urn:hasChild> ?x triple or the "
                    + "callback silently failed to loopback")
                .isNotEmpty();
        }
    }
}
