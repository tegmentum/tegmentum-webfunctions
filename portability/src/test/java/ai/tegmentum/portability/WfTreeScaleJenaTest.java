package ai.tegmentum.portability;

import java.io.File;

import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Jena counterpart of {@link WfTreeScaleRdf4jTest}. Split into its own
 * class so surefire's fork-per-class isolation guarantees this test doesn't
 * see wasmtime4j engine state or CallbackContext ThreadLocal residue from
 * the RDF4J run. See {@link WfTreeScaleRdf4jTest}'s javadoc for what we're
 * measuring.
 */
public class WfTreeScaleJenaTest {

    private static final String WF_TREE_WASM = System.getProperty("wf.tree.wasm",
            System.getProperty("user.home")
                    + "/git/tegmentum-webfunctions/target/wasm32-wasip1/release/wf_tree.wasm");

    private static final int TARGET_NODES = Integer.getInteger("wf.tree.scale.n", 1000);
    private static final int BRANCHING = Integer.getInteger("wf.tree.scale.branching", 3);
    private static final String NS = "http://example.org/n";
    private static final String HAS_CHILD = "http://example.org/hasChild";

    private static String[] nodeUris;

    private static void generateGraphOntoConsumer(final EdgeSink sink) {
        nodeUris = new String[TARGET_NODES];
        for (int i = 0; i < TARGET_NODES; i++) nodeUris[i] = NS + i;
        for (int i = 1; i < TARGET_NODES; i++) {
            sink.edge(nodeUris[(i - 1) / BRANCHING], nodeUris[i]);
        }
    }

    @FunctionalInterface
    interface EdgeSink { void edge(String parent, String child); }

    @Test
    public void thousandNodesUnderJena() {
        final File wasm = new File(WF_TREE_WASM);
        assumeTrue("wf_tree.wasm not built", wasm.exists());

        ai.tegmentum.jena.webfunctions.WebFunctionInit.register();

        final org.apache.jena.rdf.model.Model model =
                org.apache.jena.rdf.model.ModelFactory.createDefaultModel();
        final org.apache.jena.rdf.model.Property has = model.createProperty(HAS_CHILD);
        generateGraphOntoConsumer((p, c) ->
                model.add(model.createResource(p), has, model.createResource(c)));

        final org.apache.jena.query.Dataset ds =
                org.apache.jena.query.DatasetFactory.create(model);
        final org.apache.jena.query.Query q =
                org.apache.jena.query.QueryFactory.create(wfCallSparql(wasm));

        // Warmup — see the RDF4J twin for rationale.
        try (org.apache.jena.query.QueryExecution warm =
                org.apache.jena.query.QueryExecutionFactory.create(q, ds)) {
            warm.execSelect().next();
        }

        final long t0 = System.nanoTime();
        final String tree;
        try (org.apache.jena.query.QueryExecution qe =
                org.apache.jena.query.QueryExecutionFactory.create(q, ds)) {
            final org.apache.jena.query.ResultSet rs = qe.execSelect();
            assertThat(rs.hasNext()).isTrue();
            tree = rs.next().getLiteral("tree").getLexicalForm();
        }
        final long elapsedMs = (System.nanoTime() - t0) / 1_000_000L;
        System.out.printf("Jena  wf_tree over %d nodes (warm): %d ms, JSON %d chars%n",
                TARGET_NODES, elapsedMs, tree.length());

        assertTreeContainsAllNodes(tree, "Jena");
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
