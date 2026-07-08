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
 * Third-engine portability proof: the same wf_tree.wasm binary that runs
 * under RDF4J ({@link WfTreeScaleRdf4jTest}) and Jena
 * ({@link WfTreeScaleJenaTest}) also runs inside Parliament — reached over
 * HTTP through Parliament's SPARQL endpoint.
 *
 * <p>Skipped when Docker isn't available or the
 * {@code parliament-wf:0.1.0-jena6} image hasn't been built yet
 * (from {@code ~/git/parliament/Dockerfile}).
 */
public class WfTreeScaleParliamentTest {

    private static final int TARGET_NODES = Integer.getInteger("wf.tree.scale.n", 1000);
    private static final int BRANCHING = Integer.getInteger("wf.tree.scale.branching", 3);
    private static final String NS = "http://example.org/n";
    private static final String HAS_CHILD = "http://example.org/hasChild";

    // Direct SPARQL endpoint — assumes Parliament is already running (either
    // manually via `docker run` or by an outer harness). This dodges
    // Testcontainers' Docker environment probe which fails under Colima on
    // Apple Silicon with non-default DOCKER_HOST.
    //
    // Boot manually with:
    //   docker run --rm -d --name parliament-wf -p 8089:8089 \
    //     -v <path>/wf_tree.wasm:/opt/wasm/wf_tree.wasm:ro \
    //     parliament-wf:0.1.0-jena6
    private static final String SPARQL_URL = System.getProperty(
            "parliament.sparql.url", "http://localhost:8089/parliament/sparql");
    private static final String UPDATE_URL = System.getProperty(
            "parliament.update.url", "http://localhost:8089/parliament/update");
    private static final String WASM_URL_IN_QUERY = System.getProperty(
            "parliament.wasm.url", "file:///opt/wasm/wf_tree.wasm");
    private static String[] nodeUris;

    @BeforeClass
    public static void probeAndLoadGraph() {
        assumeTrue("Parliament SPARQL endpoint not reachable at " + SPARQL_URL,
                pingSparql());
        loadGraph();
    }

    private static boolean pingSparql() {
        try (org.apache.jena.query.QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString("ASK {}")
                .build()) {
            qe.execAsk();
            return true;
        } catch (Throwable t) {
            System.err.println("[wf-test] Parliament SPARQL probe failed at "
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
    public void thousandNodesUnderParliament() {
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?tree WHERE {\n" +
            "  BIND (wf:call(<" + WASM_URL_IN_QUERY + ">,\n" +
            "        <" + NS + "0>,\n" +
            "        \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\") AS ?tree)\n" +
            "}";

        // Warmup — same isolation the other engine tests use.
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
        System.out.printf("Parliament wf_tree over %d nodes (warm): %d ms, JSON %d chars%n",
                TARGET_NODES, elapsedMs, tree.length());

        int missing = 0;
        for (String uri : nodeUris) {
            if (!tree.contains("\"uri\":\"" + uri + "\"")) missing++;
        }
        assertThat(missing).as("Parliament: %d nodes missing", missing).isZero();
    }

}
