package ai.tegmentum.portability;

import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.QuerySolution;
import org.apache.jena.query.ResultSet;
import org.apache.jena.sparql.exec.http.QueryExecutionHTTPBuilder;
import org.apache.jena.sparql.exec.http.UpdateExecutionHTTPBuilder;
import org.junit.BeforeClass;
import org.junit.Test;

import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.Set;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Exercises the {@code adjacency_tree} wasm component through the
 * oxigraph-wf SERVICE handler.
 *
 * <p>Companion to {@link WfServiceOxigraphTest}: same server, same
 * SERVICE-shaped binding-set fan-out. Where {@code wf_tree_rows} projects
 * one row per visited node with (uri, depth, parent), {@code adjacency_tree}
 * projects one row per parent-&gt;child edge with (source, target). This
 * test loads a hand-shaped 6-node tree — 5 edges, small enough that we can
 * assert every edge the walker returns is one we inserted.
 *
 * <p>Boot manually with:
 * <pre>
 *   docker run --rm -d --name oxigraph-wf --platform linux/arm64 -p 3030:3030 \
 *       -v &lt;path&gt;/adjacency_tree.wasm:/opt/wasm/adjacency_tree.wasm:ro \
 *       oxigraph-wf:0.1.0
 * </pre>
 */
public class WfAdjacencyOxigraphTest {

    private static final String NS = "http://example.org/n";
    private static final String HAS_CHILD = "http://example.org/hasChild";

    private static final String SPARQL_URL = System.getProperty(
            "oxigraph.sparql.url", "http://localhost:3030/query");
    private static final String UPDATE_URL = System.getProperty(
            "oxigraph.update.url", "http://localhost:3030/update");
    private static final String WASM_URL_IN_QUERY = System.getProperty(
            "oxigraph.wasm.url", "file:///opt/wasm/adjacency_tree.wasm");

    // Hand-shaped 6-node tree:
    //          n0
    //         /  \
    //        n1   n2
    //       / \    \
    //      n3  n4   n5
    // Deliberately unbalanced so the DFS walk order isn't accidentally
    // preserved by any implicit level-order in the store.
    private static final String[][] EDGES = new String[][] {
            {NS + "0", NS + "1"},
            {NS + "0", NS + "2"},
            {NS + "1", NS + "3"},
            {NS + "1", NS + "4"},
            {NS + "2", NS + "5"},
    };

    // Ground-truth edge set built at load time; the SERVICE walker's output
    // is checked as a subset of this. Using a LinkedHashSet only for stable
    // failure messages — semantics are set-equality.
    private static final Set<String> EDGE_KEYS = new LinkedHashSet<>();

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
        final StringBuilder u = new StringBuilder("INSERT DATA {\n");
        for (String[] e : EDGES) {
            u.append("  <").append(e[0])
                .append("> <").append(HAS_CHILD)
                .append("> <").append(e[1]).append("> .\n");
            EDGE_KEYS.add(edgeKey(e[0], e[1]));
        }
        u.append("}");

        UpdateExecutionHTTPBuilder.create()
                .endpoint(UPDATE_URL)
                .updateString(u.toString())
                .build()
                .execute();
    }

    private static String edgeKey(String source, String target) {
        return source + "|" + target;
    }

    @Test
    public void sixNodeTreeAsEdgeRows() {
        // SERVICE <wf:call> gives us (?s, ?t) per parent->child edge — one
        // SPARQL solution each. The child-lookup pattern re-binds ?child
        // per hop and lets the walker recurse depth-first from n0.
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?s ?t WHERE {\n" +
            "  SERVICE <wf:call> {\n" +
            "    _:c wf:wasm  <" + WASM_URL_IN_QUERY + "> ;\n" +
            "        wf:arg   <" + NS + "0> ;\n" +
            "        wf:arg   \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\" .\n" +
            "    _:o wf:source ?s ;\n" +
            "        wf:target ?t .\n" +
            "  }\n" +
            "}";

        int rowCount = 0;
        Set<String> observedEdges = new HashSet<>();

        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            final ResultSet rs = qe.execSelect();
            while (rs.hasNext()) {
                QuerySolution sol = rs.next();
                rowCount++;
                String source = sol.getResource("s").getURI();
                String target = sol.getResource("t").getURI();
                String key = edgeKey(source, target);
                // Every walker-produced edge must correspond to an actual
                // hasChild triple we inserted. Anything else means the
                // walker is fabricating edges (or the SERVICE handler is
                // scrambling column bindings).
                assertThat(EDGE_KEYS)
                        .as("edge (%s, %s) present in input graph", source, target)
                        .contains(key);
                observedEdges.add(key);
            }
        }

        // A 6-node tree has exactly 5 edges; the walker must emit each one
        // exactly once (no duplicates, no cycles, no misses).
        assertThat(rowCount).as("row count").isEqualTo(EDGES.length);
        assertThat(observedEdges).as("distinct edges").hasSize(EDGES.length);
        assertThat(observedEdges).as("edge set equality").isEqualTo(EDGE_KEYS);
    }
}
