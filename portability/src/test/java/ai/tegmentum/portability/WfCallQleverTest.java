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
 * QLever wf:call SPARQL-level proof against qlever-wf:0.4.0. Piece C of the
 * wire lands with this image: {@code WfCallExpression.cpp} now marshals args
 * into a WIT {@code list<value>} JSON payload and parses the guest's
 * {@code binding-sets} reply properly, so wf:call finally returns real
 * SPARQL literals instead of always collapsing to UNDEF.
 *
 * <p>0.4.0 differences vs. 0.3.0:
 * <ul>
 *   <li>Argument marshalling: {@code wf:call(<url>, arg1, ...)} now
 *       serialises constants as WIT {@code value} JSON — bare {@code "{}"}
 *       was the 0.3.0 payload.</li>
 *   <li>Result extraction: parses {@code {"vars": ..., "rows": [[{"name":
 *       ..., "value": {...}}, ...], ...]}} via {@code nlohmann::json}
 *       rather than the earlier substring hack that only worked for
 *       xsd:string literals with a single {@code "value":"..."} match.</li>
 * </ul>
 *
 * <p>Test guest: {@code debug_callback_depth.wasm}. It is component-mode,
 * callback-free (the runtime supplies {@code callback-depth} as a fallback
 * that returns 0 outside of nested re-entry), and its {@code evaluate}
 * returns a single {@code xsd:integer} literal with value {@code "0"}.
 * That is enough to prove the whole pipeline end-to-end without needing a
 * bulk-loaded index.
 *
 * <p>Boot manually with (from a directory containing debug_callback_depth.wasm
 * and any minimal index):
 * <pre>
 *   docker run --rm -d --name qlever-wf --platform linux/arm64 -p 7001:7001 \
 *       -e UID=$(id -u) -e GID=$(id -g) \
 *       -v $(pwd):/data -w /data \
 *       qlever-wf:0.4.0 \
 *       qlever-server --port 7001 &lt;index-basename&gt;
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
}
