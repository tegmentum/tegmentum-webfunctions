package ai.tegmentum.portability;

import java.io.File;

import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Scale-out counterpart to {@link WfTreePortabilityTest}. Generates a
 * balanced N-ary tree of ~1000 nodes and runs {@code wf_tree.wasm} against
 * it under both RDF4J and Jena. Prints wall-clock timings and asserts every
 * generated URI appears in the produced JSON — a soft check that the
 * recursive execute-query pipeline stays correct at three-digit node counts,
 * not just the five-node toy graph.
 *
 * <p>What we're guarding against:
 * <ul>
 *   <li>Any O(n²) blowup in the marshalling (per-row list construction,
 *       Map lookups, etc.).</li>
 *   <li>Depth-limit trips: with branching factor 3, a 1000-node balanced
 *       tree is ~7 deep — well below the 100 default.</li>
 *   <li>Memory pressure — every call materialises the entire tree JSON as
 *       a single string; if that starts costing seconds we want to know.</li>
 * </ul>
 *
 * <p>Configurable via {@code -Dwf.tree.scale.n=<int>}. Defaults to 1023
 * (a perfectly-full ternary tree of depth 6: 1 + 3 + 9 + 27 + 81 + 243 + 729,
 * whose sum is 1093 — close enough).
 */
public class WfTreeScaleRdf4jTest {

    private static final String WF_TREE_WASM = System.getProperty("wf.tree.wasm",
            System.getProperty("user.home")
                    + "/git/webfunctions/target/wasm32-wasip1/release/wf_tree.wasm");

    private static final int TARGET_NODES = Integer.getInteger("wf.tree.scale.n", 1000);
    private static final int BRANCHING = Integer.getInteger("wf.tree.scale.branching", 3);
    private static final String NS = "http://example.org/n";
    private static final String HAS_CHILD = "http://example.org/hasChild";

    private static String[] nodeUris;

    private static void generateGraphOntoConsumer(final EdgeSink sink) {
        nodeUris = new String[TARGET_NODES];
        for (int i = 0; i < TARGET_NODES; i++) nodeUris[i] = NS + i;
        // Complete B-ary tree: parent of i is (i-1)/BRANCHING.
        for (int i = 1; i < TARGET_NODES; i++) {
            sink.edge(nodeUris[(i - 1) / BRANCHING], nodeUris[i]);
        }
    }

    @FunctionalInterface
    interface EdgeSink { void edge(String parent, String child); }

    @Test
    public void thousandNodesUnderRdf4j() {
        final File wasm = new File(WF_TREE_WASM);
        assumeTrue("wf_tree.wasm not built", wasm.exists());

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
            generateGraphOntoConsumer((p, c) ->
                    conn.add(vf.createIRI(p), has, vf.createIRI(c)));
            conn.commit();

            // Warmup: pay engine init + component instantiate + JVM warmup once,
            // discard result. The "warm" number below is what a hot invocation
            // costs — the number end-users see on the second and every
            // subsequent wf:call in a running server.
            try (org.eclipse.rdf4j.query.TupleQueryResult warm =
                    conn.prepareTupleQuery(wfCallSparql(wasm)).evaluate()) {
                warm.next();
            }

            final long t0 = System.nanoTime();
            final String tree;
            try (org.eclipse.rdf4j.query.TupleQueryResult r =
                    conn.prepareTupleQuery(wfCallSparql(wasm)).evaluate()) {
                assertThat(r.hasNext()).isTrue();
                tree = r.next().getValue("tree").stringValue();
            }
            final long elapsedMs = (System.nanoTime() - t0) / 1_000_000L;
            System.out.printf("RDF4J wf_tree over %d nodes (warm): %d ms, JSON %d chars%n",
                    TARGET_NODES, elapsedMs, tree.length());

            assertTreeContainsAllNodes(tree, "RDF4J");
        } finally {
            repo.shutDown();
        }
    }

    private static String wfCallSparql(final File wasm) {
        return
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?tree WHERE {\n" +
            "  BIND (wf:call(\n" +
            "        <" + wasm.toURI() + ">,\n" +
            "        <" + NS + "0>,\n" +
            "        \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\"" +
            "  ) AS ?tree)\n" +
            "}";
    }

    private static void assertTreeContainsAllNodes(final String tree, final String engine) {
        int missing = 0;
        String firstMissing = null;
        for (String uri : nodeUris) {
            if (!tree.contains("\"uri\":\"" + uri + "\"")) {
                if (firstMissing == null) firstMissing = uri;
                missing++;
            }
        }
        assertThat(missing)
            .as("%s: %d/%d nodes missing from wf_tree JSON (first missing: %s)",
                engine, missing, nodeUris.length, firstMissing)
            .isZero();
    }
}
