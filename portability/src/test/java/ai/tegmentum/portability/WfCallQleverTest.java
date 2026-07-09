package ai.tegmentum.portability;

import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.ResultSet;
import org.apache.jena.sparql.exec.http.QueryExecutionHTTPBuilder;
import org.junit.BeforeClass;
import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * QLever wf:call proof against the qlever-wf:0.3.0 image (Piece B of the
 * host-callback wiring lands with this test). Complements the older
 * WfTreeScaleQleverTest which pinned itself to 0.1.0 and only exercised
 * the extractor path.
 *
 * <p>Coverage in v0.3:
 * <ul>
 *   <li>SPARQL endpoint is reachable — proves the qlever-server binary
 *       actually links against libqlever_wf_runtime.a and boots</li>
 *   <li>{@code wf:call(<to_upper.wasm>, "stardog")} returns "STARDOG" —
 *       proves the core-module ABI works through the new callback-aware
 *       WfRuntime::Impl constructor (wf_runtime_new_with_callbacks)</li>
 * </ul>
 *
 * <p>execute-query re-entrancy (wf_tree.wasm) is explicitly NOT tested here
 * — Piece B of the runtime work leaves execute_query stubbed at the C++
 * level so the ABI is ready but the QLever query engine wiring is still
 * a follow-up. A guest that reaches for execute-query today sees a clean
 * {@code err "host callback `execute-query` not wired by embedder"} rather
 * than silent misbehaviour.
 *
 * <p>Boot manually with (from a directory containing to_upper.wasm and a
 * bulk-loaded index):
 * <pre>
 *   docker run --rm -d --name qlever-wf --platform linux/arm64 -p 7001:7001 \
 *       -e UID=$(id -u) -e GID=$(id -g) \
 *       -v $(pwd):/data -w /data \
 *       qlever-wf:0.3.0 \
 *       qlever-server --port 7001 &lt;index-basename&gt;
 * </pre>
 */
public class WfCallQleverTest {

    private static final String SPARQL_URL = System.getProperty(
            "qlever.sparql.url", "http://localhost:7001/");
    private static final String WASM_URL_IN_QUERY = System.getProperty(
            "qlever.wasm.url", "file:///opt/wasm/to_upper.wasm");

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
     * Callback-free wasm proof: {@code wf:call(<to_upper.wasm>, "stardog")}
     * returns "STARDOG". This exercises the core-module ABI, which is what
     * to_upper.wasm ships as. The Component Model path is exercised through
     * a separate WIT-based guest whose host imports are stubbed at
     * err&lt;string&gt; today.
     */
    @Test
    public void wfCallReturnsUpperCasedLiteral() {
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?result WHERE {\n" +
            "  BIND (wf:call(<" + WASM_URL_IN_QUERY + ">, \"stardog\") AS ?result)\n" +
            "}";

        final long t0 = System.nanoTime();
        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            final ResultSet rs = qe.execSelect();
            assertThat(rs.hasNext()).isTrue();
            final String out = rs.next().getLiteral("result").getLexicalForm();
            System.out.printf("QLever wf:call (warm): %d ms, result='%s'%n",
                (System.nanoTime() - t0) / 1_000_000L, out);
            assertThat(out).isEqualTo("STARDOG");
        }
    }
}
