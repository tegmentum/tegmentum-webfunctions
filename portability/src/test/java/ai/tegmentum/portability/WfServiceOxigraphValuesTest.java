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
 * Regression test for the v0.2 silent-drop bug in oxigraph-wf's SERVICE
 * handler.
 *
 * <p>Prior to the v0.3 query-rewrite pass, {@code SERVICE <wf:call>} with
 * a {@code wf:arg ?var} triple enumerated candidate values for {@code
 * ?var} from the local triple store, then joined the emitted rows against
 * the outer {@code VALUES} clause. Consequence: any VALUES row referencing
 * an IRI that never appears in a quad was silently dropped — even though
 * the wasm would have happily returned rows given that IRI.
 *
 * <p>Post-fix, the query-rewrite pass substitutes each VALUES row's
 * concrete term into the SERVICE block before evaluation, so every VALUES
 * row triggers exactly one wasm call irrespective of the store contents.
 *
 * <p>The test asserts:
 * <ul>
 *   <li>an in-store IRI ({@code urn:eng}) yields the wasm's full walk
 *       (root row + descendants);
 *   <li>an out-of-store IRI ({@code urn:not_in_store}) yields exactly one
 *       row — the depth-0 root with no children — which is the correct
 *       semantic answer for wf_tree_rows given an IRI with no outgoing
 *       {@code hasEmployee} triples.
 * </ul>
 */
public class WfServiceOxigraphValuesTest {

    private static final String SPARQL_URL = System.getProperty(
            "oxigraph.sparql.url", "http://localhost:3137/query");
    private static final String UPDATE_URL = System.getProperty(
            "oxigraph.update.url", "http://localhost:3137/update");
    private static final String WASM_URL_IN_QUERY = System.getProperty(
            "oxigraph.wasm.url",
            "file:///Users/zacharywhitley/git/tegmentum-webfunctions/"
                + "target/wasm32-wasip1/release/wf_tree_rows.wasm");

    private static final String IN_STORE = "urn:eng";
    private static final String OUT_OF_STORE = "urn:not_in_store";
    private static final String CHILD = "urn:alice";

    @BeforeClass
    public static void probeAndLoadGraph() {
        assumeTrue("Oxigraph SPARQL endpoint not reachable at " + SPARQL_URL,
                pingSparql());
        // A single hasEmployee edge: urn:eng -> urn:alice. The wasm walk
        // from urn:eng yields two rows (root + alice); the wasm walk from
        // urn:not_in_store yields exactly one row (root only).
        UpdateExecutionHTTPBuilder.create()
                .endpoint(UPDATE_URL)
                .updateString(
                    "INSERT DATA {\n"
                        + "  <" + IN_STORE + "> a <urn:Dept> ;\n"
                        + "    <urn:hasEmployee> <" + CHILD + "> .\n"
                        + "}")
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
    public void valuesWithOutOfStoreIriStillFiresWasm() {
        // Mix an in-store and an out-of-store IRI. Both must produce
        // rows: the in-store one via its hasEmployee edge, the
        // out-of-store one via the wasm's depth-0 root row.
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n"
            + "SELECT ?dept ?uri WHERE {\n"
            + "  VALUES ?dept { <" + IN_STORE + "> <" + OUT_OF_STORE + "> }\n"
            + "  SERVICE <wf:call> {\n"
            + "    _:c wf:wasm <" + WASM_URL_IN_QUERY + "> ;\n"
            + "        wf:arg  ?dept ;\n"
            + "        wf:arg  \"SELECT ?child WHERE { ?this <urn:hasEmployee> ?child }\" .\n"
            + "    _:o wf:uri ?uri .\n"
            + "  }\n"
            + "}";

        // Group observed ?uri values by their ?dept binding — the outer
        // join guarantees each row carries both.
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

        // In-store root: root + one child = 2 rows.
        assertThat(uriByDept.get(IN_STORE))
            .as("rows for in-store IRI " + IN_STORE)
            .containsExactlyInAnyOrder(IN_STORE, CHILD);

        // Out-of-store root: exactly the root row. Pre-fix this list was
        // empty because the store-enumeration path never tried the IRI.
        assertThat(uriByDept.get(OUT_OF_STORE))
            .as("rows for out-of-store IRI " + OUT_OF_STORE
                + " (pre-fix bug: silently dropped)")
            .containsExactly(OUT_OF_STORE);
    }
}
