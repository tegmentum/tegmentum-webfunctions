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
 * Sixth-engine portability proof: wf_tree.wasm on Oxigraph.
 *
 * <p>Oxigraph exposes {@code SparqlEvaluator::with_custom_function} — a
 * public builder hook that binds a Rust closure to a URI. The oxigraph-wf
 * server wires our {@code wf:call} handler through that hook, so the same
 * WIT world drives Oxigraph without a fork.
 *
 * <p>Boot manually with:
 * <pre>
 *   docker run --rm -d --name oxigraph-wf --platform linux/arm64 -p 3030:3030 \
 *       -v &lt;path&gt;/wf_tree.wasm:/opt/wasm/wf_tree.wasm:ro \
 *       oxigraph-wf:0.1.0
 * </pre>
 */
public class WfTreeScaleOxigraphTest {

    private static final int TARGET_NODES = Integer.getInteger("wf.tree.scale.n", 1000);
    private static final int BRANCHING = Integer.getInteger("wf.tree.scale.branching", 3);
    private static final String NS = "http://example.org/n";
    private static final String HAS_CHILD = "http://example.org/hasChild";

    private static final String SPARQL_URL = System.getProperty(
            "oxigraph.sparql.url", "http://localhost:3030/query");
    private static final String UPDATE_URL = System.getProperty(
            "oxigraph.update.url", "http://localhost:3030/update");
    private static final String WASM_URL_IN_QUERY = System.getProperty(
            "oxigraph.wasm.url", "file:///opt/wasm/wf_tree.wasm");
    private static String[] nodeUris;

    @BeforeClass
    public static void probeAndLoadGraph() {
        assumeTrue("Oxigraph SPARQL endpoint not reachable at " + SPARQL_URL,
                pingSparql());
        loadGraph();
    }

    private static boolean pingSparql() {
        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString("ASK {}")
                .build()) {
            qe.execAsk();
            return true;
        } catch (Throwable t) {
            System.err.println("[wf-test] Oxigraph SPARQL probe failed at "
                + SPARQL_URL + ": " + t.getClass().getName() + " — " + t.getMessage());
            return false;
        }
    }

    private static void loadGraph() {
        nodeUris = new String[TARGET_NODES];
        for (int i = 0; i < TARGET_NODES; i++) nodeUris[i] = NS + i;

        final StringBuilder u = new StringBuilder("INSERT DATA {\n");
        for (int i = 1; i < TARGET_NODES; i++) {
            u.append("  <").append(nodeUris[(i - 1) / BRANCHING])
                .append("> <").append(HAS_CHILD)
                .append("> <").append(nodeUris[i]).append("> .\n");
        }
        u.append("}");

        UpdateExecutionHTTPBuilder.create()
                .endpoint(UPDATE_URL)
                .updateString(u.toString())
                .build()
                .execute();
    }

    @Test
    public void thousandNodesUnderOxigraph() {
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?tree WHERE {\n" +
            "  BIND (wf:call(<" + WASM_URL_IN_QUERY + ">,\n" +
            "        <" + NS + "0>,\n" +
            "        \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\") AS ?tree)\n" +
            "}";

        // Warmup
        try (QueryExecution warm = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            warm.execSelect().next();
        }

        final long t0 = System.nanoTime();
        final String tree;
        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            final ResultSet rs = qe.execSelect();
            assertThat(rs.hasNext()).isTrue();
            tree = rs.next().getLiteral("tree").getLexicalForm();
        }
        final long elapsedMs = (System.nanoTime() - t0) / 1_000_000L;
        System.out.printf("Oxigraph wf_tree over %d nodes (warm): %d ms, JSON %d chars%n",
                TARGET_NODES, elapsedMs, tree.length());

        int missing = 0;
        for (String uri : nodeUris) {
            if (!tree.contains("\"uri\":\"" + uri + "\"")) missing++;
        }
        assertThat(missing).as("Oxigraph: %d nodes missing", missing).isZero();
    }
}
