package ai.tegmentum.portability;

import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.QuerySolution;
import org.apache.jena.query.ResultSet;
import org.apache.jena.sparql.exec.http.QueryExecutionHTTPBuilder;
import org.apache.jena.sparql.exec.http.UpdateExecutionHTTPBuilder;
import org.junit.BeforeClass;
import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Proves the SERVICE-shaped return path added to oxigraph-wf: instead of
 * projecting the wasm's first-cell as a single JSON literal, the SERVICE
 * handler expands the wasm's {@code binding-sets { vars, rows }} return
 * into first-class SPARQL variable bindings.
 *
 * <p>Companion to {@link WfTreeScaleOxigraphTest}, which exercises the
 * BIND(wf:call(...)) filter-function path. Boots the same oxigraph-wf
 * server, loads the same 1000-node balanced tree, and asserts that the
 * SERVICE call returns one row per visited node with typed {@code
 * ?depth} and IRI-bound {@code ?uri} / {@code ?parent}.
 *
 * <p>Boot manually with:
 * <pre>
 *   docker run --rm -d --name oxigraph-wf --platform linux/arm64 -p 3030:3030 \
 *       -v &lt;path&gt;/wf_tree_rows.wasm:/opt/wasm/wf_tree_rows.wasm:ro \
 *       oxigraph-wf:0.1.0
 * </pre>
 */
public class WfServiceOxigraphTest {

    private static final int TARGET_NODES = Integer.getInteger("wf.tree.scale.n", 1000);
    private static final int BRANCHING = Integer.getInteger("wf.tree.scale.branching", 3);
    private static final String NS = "http://example.org/n";
    private static final String HAS_CHILD = "http://example.org/hasChild";

    private static final String SPARQL_URL = System.getProperty(
            "oxigraph.sparql.url", "http://localhost:3030/query");
    private static final String UPDATE_URL = System.getProperty(
            "oxigraph.update.url", "http://localhost:3030/update");
    // wf_tree_rows is the new binding-set-shaped variant of wf_tree; the
    // classic wf_tree returns a single JSON string, wf_tree_rows returns
    // (uri, depth, parent) per visited node. See crates/wf_tree_rows.
    private static final String WASM_URL_IN_QUERY = System.getProperty(
            "oxigraph.wasm.url", "file:///opt/wasm/wf_tree_rows.wasm");
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
                + SPARQL_URL + ": " + t.getClass().getName() + " - " + t.getMessage());
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
    public void thousandNodesAsBindingRows() {
        // SERVICE <wf:call> gives us (?uri, ?depth, ?parent) per visited
        // node — one SPARQL solution each, not a JSON blob. FILTER on
        // ?depth exercises that ?depth is a real typed xsd:integer.
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?uri ?depth ?parent WHERE {\n" +
            "  SERVICE <wf:call> {\n" +
            "    _:c wf:wasm  <" + WASM_URL_IN_QUERY + "> ;\n" +
            "        wf:arg   <" + NS + "0> ;\n" +
            "        wf:arg   \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\" .\n" +
            "    _:o wf:uri    ?uri ;\n" +
            "        wf:depth  ?depth ;\n" +
            "        wf:parent ?parent .\n" +
            "  }\n" +
            "}";

        // Warmup so the timed run reflects steady-state (wasm compile +
        // module cache both cold on the first shot).
        try (QueryExecution warm = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            while (warm.execSelect().hasNext()) { warm.execSelect().next(); }
        }

        final long t0 = System.nanoTime();
        int rowCount = 0;
        int rootRows = 0;
        int maxDepthSeen = 0;
        java.util.Set<String> distinctUris = new java.util.HashSet<>();

        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            final ResultSet rs = qe.execSelect();
            while (rs.hasNext()) {
                QuerySolution sol = rs.next();
                rowCount++;
                String uri = sol.getResource("uri").getURI();
                distinctUris.add(uri);
                int depth = sol.getLiteral("depth").getInt();
                if (depth > maxDepthSeen) maxDepthSeen = depth;
                if (sol.get("parent") == null) rootRows++;
            }
        }
        final long elapsedMs = (System.nanoTime() - t0) / 1_000_000L;
        System.out.printf(
                "Oxigraph SERVICE wf_tree_rows over %d nodes (warm): %d ms, %d rows, max depth %d%n",
                TARGET_NODES, elapsedMs, rowCount, maxDepthSeen);

        // Every node in the balanced BRANCHING=3 tree should show up
        // exactly once as a row. The root row is the sole `parent`-
        // unbound one; the rest are children with a valid parent IRI.
        assertThat(rowCount).as("row count").isEqualTo(TARGET_NODES);
        assertThat(distinctUris).as("distinct URIs").hasSize(TARGET_NODES);
        assertThat(rootRows).as("root rows (parent unbound)").isEqualTo(1);
        // With 1000 nodes and branching 3, tree depth = ceil(log3(2001)) = 7.
        // Assert the observed max is at least this — the walk visited to
        // the leaves rather than early-terminating.
        assertThat(maxDepthSeen).as("max depth reached").isGreaterThanOrEqualTo(6);
    }
}
