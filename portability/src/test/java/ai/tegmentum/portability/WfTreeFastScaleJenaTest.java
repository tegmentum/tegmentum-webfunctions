package ai.tegmentum.portability;

import java.io.File;

import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/** Jena counterpart of {@link WfTreeFastScaleRdf4jTest}. */
public class WfTreeFastScaleJenaTest {

    private static final String WF_TREE_FAST_WASM = System.getProperty("wf.tree.fast.wasm",
            System.getProperty("user.home")
                    + "/git/tegmentum-webfunctions/target/wasm32-wasip1/release/wf_tree_fast.wasm");

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
    public void thousandNodesUnderJena() {
        final File wasm = new File(WF_TREE_FAST_WASM);
        assumeTrue("wf_tree_fast.wasm not built", wasm.exists());

        ai.tegmentum.jena.webfunctions.WebFunctionInit.register();

        final org.apache.jena.rdf.model.Model model =
                org.apache.jena.rdf.model.ModelFactory.createDefaultModel();
        final org.apache.jena.rdf.model.Property has = model.createProperty(HAS_CHILD);
        generateGraph((p, c) -> model.add(model.createResource(p), has, model.createResource(c)));

        final org.apache.jena.query.Dataset ds =
                org.apache.jena.query.DatasetFactory.create(model);
        final org.apache.jena.query.Query q =
                org.apache.jena.query.QueryFactory.create(wfCallSparql(wasm));

        // Warmup.
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
        System.out.printf("Jena  wf_tree_fast over %d nodes (warm): %d ms, JSON %d chars%n",
                TARGET_NODES, elapsedMs, tree.length());

        int missing = 0;
        for (String uri : nodeUris) {
            if (!tree.contains("\"uri\":\"" + uri + "\"")) missing++;
        }
        assertThat(missing).as("Jena fast: %d nodes missing", missing).isZero();
    }

    private static String wfCallSparql(final File wasm) {
        return
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?tree WHERE {\n" +
            "  BIND (wf:call(<" + wasm.toURI() + ">, <" + NS + "0>, <" + HAS_CHILD + ">) AS ?tree)\n" +
            "}";
    }
}
