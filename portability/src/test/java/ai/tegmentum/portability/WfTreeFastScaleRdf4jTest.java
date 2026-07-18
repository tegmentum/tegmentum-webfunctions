package ai.tegmentum.portability;

import java.io.File;

import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Ceiling benchmark: the same tree walk as {@link WfTreeScaleRdf4jTest} but
 * using {@code wf_tree_fast.wasm} — which drives the v0.3.3
 * {@code follow-predicate} host import instead of SPARQL sub-queries.
 * Direct comparison to the wf_tree numbers shows what SPARQL parsing +
 * binding-set marshalling actually cost per call.
 */
public class WfTreeFastScaleRdf4jTest {

    private static final String WF_TREE_FAST_WASM = System.getProperty("wf.tree.fast.wasm",
            System.getProperty("user.home")
                    + "/git/webfunctions/target/wasm32-wasip1/release/wf_tree_fast.wasm");

    private static final int TARGET_NODES = Integer.getInteger("wf.tree.scale.n", 1000);
    private static final int BRANCHING = Integer.getInteger("wf.tree.scale.branching", 3);
    private static final String NS = "http://example.org/n";
    private static final String HAS_CHILD = "http://example.org/hasChild";

    private static String[] nodeUris;

    private static void generateGraph(final EdgeSink sink) {
        nodeUris = new String[TARGET_NODES];
        for (int i = 0; i < TARGET_NODES; i++) nodeUris[i] = NS + i;
        for (int i = 1; i < TARGET_NODES; i++) {
            sink.edge(nodeUris[(i - 1) / BRANCHING], nodeUris[i]);
        }
    }

    @FunctionalInterface interface EdgeSink { void edge(String p, String c); }

    @Test
    public void thousandNodesUnderRdf4j() {
        final File wasm = new File(WF_TREE_FAST_WASM);
        assumeTrue("wf_tree_fast.wasm not built", wasm.exists());

        final org.eclipse.rdf4j.sail.memory.MemoryStore store =
                new org.eclipse.rdf4j.sail.memory.MemoryStore();
        store.setEvaluationStrategyFactory(
            new ai.tegmentum.rdf4j.webfunctions.WfEvaluationStrategyFactory(null));
        final org.eclipse.rdf4j.repository.sail.SailRepository repo =
                new org.eclipse.rdf4j.repository.sail.SailRepository(store);
        repo.init();

        try (org.eclipse.rdf4j.repository.RepositoryConnection conn = repo.getConnection()) {
            final org.eclipse.rdf4j.model.ValueFactory vf =
                    org.eclipse.rdf4j.model.impl.SimpleValueFactory.getInstance();
            final org.eclipse.rdf4j.model.IRI has = vf.createIRI(HAS_CHILD);
            conn.begin();
            generateGraph((p, c) -> conn.add(vf.createIRI(p), has, vf.createIRI(c)));
            conn.commit();

            final String sparql = wfCallSparql(wasm);

            // Warmup.
            try (org.eclipse.rdf4j.query.TupleQueryResult warm =
                    conn.prepareTupleQuery(sparql).evaluate()) {
                warm.next();
            }

            final long t0 = System.nanoTime();
            final String tree;
            try (org.eclipse.rdf4j.query.TupleQueryResult r =
                    conn.prepareTupleQuery(sparql).evaluate()) {
                assertThat(r.hasNext()).isTrue();
                tree = r.next().getValue("tree").stringValue();
            }
            final long elapsedMs = (System.nanoTime() - t0) / 1_000_000L;
            System.out.printf("RDF4J wf_tree_fast over %d nodes (warm): %d ms, JSON %d chars%n",
                    TARGET_NODES, elapsedMs, tree.length());

            int missing = 0;
            for (String uri : nodeUris) {
                if (!tree.contains("\"uri\":\"" + uri + "\"")) missing++;
            }
            assertThat(missing).as("RDF4J fast: %d nodes missing", missing).isZero();
        } finally {
            repo.shutDown();
        }
    }

    private static String wfCallSparql(final File wasm) {
        // Two args now: root and predicate. No SPARQL sub-query string.
        return
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?tree WHERE {\n" +
            "  BIND (wf:call(<" + wasm.toURI() + ">, <" + NS + "0>, <" + HAS_CHILD + ">) AS ?tree)\n" +
            "}";
    }
}
