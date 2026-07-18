package ai.tegmentum.portability;

import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.QuerySolution;
import org.apache.jena.query.ResultSet;
import org.apache.jena.sparql.exec.http.QueryExecutionHTTPBuilder;
import org.apache.jena.sparql.exec.http.UpdateExecutionHTTPBuilder;
import org.junit.BeforeClass;
import org.junit.Test;

import java.util.HashMap;
import java.util.Map;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Regression test for the v0.4 BGP-materialisation path in oxigraph-wf's
 * query-rewrite pass.
 *
 * <p>Companion to {@link WfServiceOxigraphValuesTest}, which covers the
 * static {@code VALUES + SERVICE} shape. Here the input to {@code wf:arg
 * ?dept} is not a VALUES clause but a BGP ({@code ?dept a <urn:Dept>})
 * sitting next to the SERVICE block. Pre-v0.4 the SERVICE handler fell
 * through to its store-enumeration path and logged a WARN; v0.4
 * pre-evaluates the BGP against the store, materialises the bindings as
 * a synthetic Values, and lets the existing per-row Union rewrite fire.
 *
 * <p>Assertions:
 * <ul>
 *   <li>Every dept known to the store shows up in the results with both
 *       its own IRI (the root row emitted by wf_tree_rows) and its
 *       associated employee (one hasEmployee edge per dept).</li>
 *   <li>Row count equals the equivalent VALUES-driven query's row count
 *       (6 = 3 depts * (root + 1 child)).</li>
 * </ul>
 *
 * <p>Boot manually with:
 * <pre>
 *   cd ~/git/oxigraph-wf &amp;&amp; cargo build --release --bin oxigraph-wf
 *   ~/git/oxigraph-wf/target/release/oxigraph-wf --port 3139 \
 *       --wasm-cache-dir /tmp/oxwf-bgp &gt; /tmp/oxwf-bgp.log 2&gt;&amp;1 &amp;
 * </pre>
 */
public class WfServiceOxigraphBgpTest {

    private static final String SPARQL_URL = System.getProperty(
            "oxigraph.sparql.url", "http://localhost:3139/query");
    private static final String UPDATE_URL = System.getProperty(
            "oxigraph.update.url", "http://localhost:3139/update");
    private static final String WASM_URL_IN_QUERY = System.getProperty(
            "oxigraph.wasm.url",
            "file:///Users/zacharywhitley/git/webfunctions/"
                + "target/wasm32-wasip1/release/wf_tree_rows.wasm");

    // Three depts, one employee each — matches the WfServiceOxigraphValuesTest
    // shape but drops the VALUES clause so the BGP-materialisation path is
    // what feeds ?dept into SERVICE.
    private static final String[][] EDGES = new String[][] {
            {"urn:d1", "urn:e1"},
            {"urn:d2", "urn:e2"},
            {"urn:d3", "urn:e3"},
    };

    @BeforeClass
    public static void probeAndLoadGraph() {
        assumeTrue("Oxigraph SPARQL endpoint not reachable at " + SPARQL_URL,
                pingSparql());
        final StringBuilder u = new StringBuilder("INSERT DATA {\n");
        for (String[] e : EDGES) {
            u.append("  <").append(e[0])
                .append("> a <urn:Dept> ; <urn:hasEmployee> <")
                .append(e[1]).append("> .\n");
        }
        u.append("}");
        UpdateExecutionHTTPBuilder.create()
                .endpoint(UPDATE_URL)
                .updateString(u.toString())
                .build()
                .execute();
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

    @Test
    public void bgpBoundDeptStillFiresWasmPerRow() {
        // ?dept is bound by a BGP triple (no VALUES). Pre-v0.4 this took
        // the store-enumeration path and logged a WARN; v0.4 pre-evaluates
        // the BGP so each dept gets one wasm call, same as VALUES.
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n"
            + "SELECT ?dept ?uri WHERE {\n"
            + "  ?dept a <urn:Dept> .\n"
            + "  SERVICE <wf:call> {\n"
            + "    _:c wf:wasm <" + WASM_URL_IN_QUERY + "> ;\n"
            + "        wf:arg  ?dept ;\n"
            + "        wf:arg  \"SELECT ?child WHERE { ?this <urn:hasEmployee> ?child }\" .\n"
            + "    _:o wf:uri ?uri .\n"
            + "  }\n"
            + "}";

        Map<String, java.util.List<String>> uriByDept = new HashMap<>();
        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            final ResultSet rs = qe.execSelect();
            while (rs.hasNext()) {
                QuerySolution sol = rs.next();
                String dept = sol.getResource("dept").getURI();
                String uri = sol.getResource("uri").getURI();
                uriByDept.computeIfAbsent(dept, d -> new java.util.ArrayList<>()).add(uri);
            }
        }

        // Every dept must produce its root row (?uri = ?dept) and its
        // one hasEmployee child — same expectation as the VALUES-driven
        // test, since the semantics of the rewrite should match.
        for (String[] e : EDGES) {
            String dept = e[0];
            String employee = e[1];
            assertThat(uriByDept.get(dept))
                .as("rows for BGP-bound dept " + dept)
                .containsExactlyInAnyOrder(dept, employee);
        }

        // Row count matches the analogous VALUES-driven test: 3 depts * 2
        // rows each = 6. This is the "same as VALUES" contract the v0.4
        // rewrite is supposed to preserve.
        int total = uriByDept.values().stream().mapToInt(java.util.List::size).sum();
        assertThat(total).as("total rows across all depts").isEqualTo(6);
    }
}
