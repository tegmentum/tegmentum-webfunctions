package ai.tegmentum.portability;

import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.ResultSet;
import org.apache.jena.sparql.exec.http.QueryExecutionHTTPBuilder;
import org.apache.jena.sparql.exec.http.UpdateExecutionHTTPBuilder;
import org.junit.BeforeClass;
import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Fourth-engine portability proof: the same wf_tree.wasm binary that runs
 * under RDF4J, Jena, and Parliament also runs inside QLever — via the
 * native {@code wf:call} SPARQL expression added to the QLever fork.
 *
 * <p>QLever v0.1 of wf:call only supports the core-module wasm ABI
 * (malloc/free/evaluate). wf_tree.wasm is Component Model with host
 * callbacks, so this test intentionally uses a different, simpler wasm
 * component built for the module ABI — currently to_upper. When wf_tree_fast
 * gets a module-ABI variant, swap in here.
 *
 * <p>Boot manually with:
 * <pre>
 *   docker run --rm -d --name qlever-wf --platform linux/arm64 -p 7001:7001 \
 *       -v &lt;path&gt;/to_upper.wasm:/opt/wasm/to_upper.wasm:ro \
 *       qlever-wf:0.1.0 qlever-server --port 7001 &lt;index-path&gt;
 * </pre>
 */
public class WfTreeScaleQleverTest {

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
     * Minimum viable proof: {@code wf:call(<to_upper.wasm>, "stardog")}
     * returns "STARDOG". Same interface, different wasm — confirms the C++
     * SparqlExpression wiring works end-to-end.
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
