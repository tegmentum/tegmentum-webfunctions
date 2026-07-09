package ai.tegmentum.portability;

import ai.tegmentum.rdf4j.webfunctions.WfEvaluationStrategyFactory;

import org.eclipse.rdf4j.model.IRI;
import org.eclipse.rdf4j.model.ValueFactory;
import org.eclipse.rdf4j.model.impl.SimpleValueFactory;
import org.eclipse.rdf4j.query.BindingSet;
import org.eclipse.rdf4j.query.TupleQueryResult;
import org.eclipse.rdf4j.repository.RepositoryConnection;
import org.eclipse.rdf4j.repository.sail.SailRepository;
import org.eclipse.rdf4j.sail.memory.MemoryStore;
import org.junit.Test;

import java.io.File;
import java.util.ArrayList;
import java.util.List;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Property-function counterpart to {@link WfTreeScaleRdf4jTest}. That test
 * exercises the FILTER-shaped {@code BIND(wf:call(...))} form, which projects
 * only a single JSON string. This one exercises the multi-var projection form
 * (SPIN-style magic property in RDF4J) so a query can bind each column of the
 * wasm's {@code binding-sets} return as a first-class SPARQL variable.
 *
 * <p>Companion to {@link WfServiceOxigraphTest} — same 6-node tree, same
 * {@code wf_tree_rows.wasm}, but invoked through RDF4J's
 * {@link org.eclipse.rdf4j.query.algebra.evaluation.function.TupleFunction}
 * SPI rather than Oxigraph's SERVICE hook. The point is portability: identical
 * guest wasm bytes, identical result semantics, different host binding.
 *
 * <p>Query shape:
 * <pre>
 *   PREFIX wf: &lt;http://tegmentum.ai/ns/webfunction/&gt;
 *   SELECT ?uri ?depth ?parent WHERE {
 *     (&lt;file:///…/wf_tree_rows.wasm&gt;
 *      &lt;urn:root&gt;
 *      "SELECT ?child WHERE { ?this &lt;urn:hasChild&gt; ?child }")
 *      wf:call (?uri ?depth ?parent) .
 *   }
 * </pre>
 * Result vars bind positionally in the WIT-declared {@code binding-sets.vars}
 * order — RDF4J's {@link org.eclipse.rdf4j.query.algebra.evaluation.function.TupleFunction}
 * SPI has no way to name columns, so the guest column order and the caller's
 * result-list order have to line up. wf_tree_rows declares (uri, depth, parent).
 */
public class WfPropertyRdf4jTest {

    private static final String WF_TREE_ROWS_WASM = System.getProperty("wf.tree.rows.wasm",
            System.getProperty("user.home")
                    + "/git/tegmentum-webfunctions/target/wasm32-wasip1/release/wf_tree_rows.wasm");

    // Small fixed graph so we can assert exact structure (row count, depth
    // distribution, single root). A 6-node balanced ternary tree gives us
    // n0 -> {n1,n2,n3}, n1 -> {n4,n5}; depths 0/1/1/1/2/2.
    private static final int NODES = 6;
    private static final int BRANCHING = 3;
    private static final String NS = "urn:n";
    private static final String HAS_CHILD = "urn:hasChild";

    @Test
    public void treeRowsProjectAsMultiVar() {
        final File wasm = new File(WF_TREE_ROWS_WASM);
        assumeTrue("wf_tree_rows.wasm not built at " + wasm, wasm.exists());

        final MemoryStore store = new MemoryStore();
        // Without this, the default StrictEvaluationStrategy silently ignores
        // TupleFunctionCall nodes (or blows up in cardinality estimation) and
        // WfCallTupleFunctionOptimizer's rewrite is never inserted into the
        // pipeline.
        store.setEvaluationStrategyFactory(new WfEvaluationStrategyFactory(null));
        final SailRepository repo = new SailRepository(store);
        repo.init();

        try (RepositoryConnection conn = repo.getConnection()) {
            loadTree(conn);

            final String query =
                "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
                "SELECT ?uri ?depth ?parent WHERE {\n" +
                "  (<" + wasm.toURI() + ">\n" +
                "   <" + NS + "0>\n" +
                "   \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\")\n" +
                "  wf:call (?uri ?depth ?parent) .\n" +
                "}";

            final List<BindingSet> rows = collect(conn, query);

            // One row per visited node in the balanced 6-node tree.
            assertThat(rows).as("row count").hasSize(NODES);

            int rootRows = 0;
            int maxDepth = 0;
            final java.util.Set<String> distinctUris = new java.util.HashSet<>();
            for (BindingSet r : rows) {
                // Every row should have an IRI-bound ?uri.
                assertThat(r.getValue("uri"))
                    .as("?uri must be IRI-typed")
                    .isInstanceOf(IRI.class);
                distinctUris.add(r.getValue("uri").stringValue());

                // ?depth is a real typed xsd:integer coming back from the
                // wasm, not a JSON substring. If the marshaller ever
                // regresses to string, .intValue() throws.
                final int depth = ((org.eclipse.rdf4j.model.Literal) r.getValue("depth")).intValue();
                if (depth > maxDepth) maxDepth = depth;

                if (r.getValue("parent") == null) {
                    rootRows++;
                } else {
                    assertThat(r.getValue("parent"))
                        .as("?parent must be IRI when bound")
                        .isInstanceOf(IRI.class);
                }
            }

            assertThat(distinctUris).as("distinct URIs").hasSize(NODES);
            assertThat(rootRows).as("root rows (parent UNDEF)").isEqualTo(1);
            // Depths for the fixed layout are 0,1,1,1,2,2.
            assertThat(maxDepth).as("max depth reached").isEqualTo(2);

            // Composability: FILTER on ?depth proves the column is a real
            // SPARQL binding, not a stringly-typed cell — otherwise the
            // xsd:integer comparison would fail with a type error, or match
            // every row, and this assert wouldn't hold.
            final String filtered =
                "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
                "SELECT ?uri ?depth ?parent WHERE {\n" +
                "  (<" + wasm.toURI() + ">\n" +
                "   <" + NS + "0>\n" +
                "   \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\")\n" +
                "  wf:call (?uri ?depth ?parent) .\n" +
                "  FILTER (?depth > 0)\n" +
                "}";
            assertThat(collect(conn, filtered))
                .as("FILTER(?depth > 0) removes only the single root row")
                .hasSize(NODES - 1);
        } finally {
            repo.shutDown();
        }
    }

    private static void loadTree(final RepositoryConnection conn) {
        final ValueFactory vf = SimpleValueFactory.getInstance();
        final IRI has = vf.createIRI(HAS_CHILD);
        conn.begin();
        for (int i = 1; i < NODES; i++) {
            conn.add(vf.createIRI(NS + ((i - 1) / BRANCHING)),
                     has,
                     vf.createIRI(NS + i));
        }
        conn.commit();
    }

    private static List<BindingSet> collect(final RepositoryConnection conn, final String q) {
        final List<BindingSet> rows = new ArrayList<>();
        try (TupleQueryResult r = conn.prepareTupleQuery(q).evaluate()) {
            while (r.hasNext()) rows.add(r.next());
        }
        return rows;
    }
}
