package ai.tegmentum.portability;

import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.QuerySolution;
import org.apache.jena.query.ResultSet;
import org.apache.jena.sparql.exec.http.QueryExecutionHTTPBuilder;
import org.junit.BeforeClass;
import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * QLever portability proof for the v0.4 {@code invoke-wasm} host import.
 * Mirror of {@link WfApplyJenaTest} / {@link WfApplyRdf4jTest} but against
 * a running {@code qlever-server} at {@code qlever-wf:0.6.0} — the image
 * built off the qlever-wf-runtime commit that registers the
 * {@code stardog:webfunction/host@0.4.0} interface and implements
 * {@code invoke-wasm} internally.
 *
 * <p>0.6.0 changes vs. 0.5.0:
 * <ul>
 *   <li>Rust runtime: same C ABI, but now registers the six-import v0.3
 *       host set on both @0.3.2 and @0.4.0 interface names via a shared
 *       helper, and adds {@code invoke-wasm} on @0.4.0.</li>
 *   <li>invoke-wasm dispatches internally: fetch bytes (file:// / http(s)://
 *       / bare path), compile once and cache in a static
 *       Mutex&lt;HashMap&lt;String, Component&gt;&gt;, then instantiate into a
 *       fresh Store that inherits the outer HostBridge — so nested
 *       execute-query callbacks continue to reach qlever-server via
 *       loopback exactly as they would at the top level.</li>
 *   <li>C++ WfCallExpression / WasmRuntime.cpp unchanged. The
 *       version-agnostic C ABI (wf_runtime_new_with_callbacks +
 *       wf_runtime_invoke) needed no C++ code changes for the v0.4.0
 *       upgrade.</li>
 * </ul>
 *
 * <p>The test relies on the outer wf_apply guest doing:
 * <pre>
 *   evaluate([&lt;urn:demo:detect_lang&gt;, "text"]):
 *     -&gt; execute_query("SELECT ?url WHERE { &lt;urn:demo:detect_lang&gt;
 *                          &lt;http://tegmentum.ai/ns/composition/source&gt; ?url }")
 *     -&gt; invoke_wasm(?url, ["text"])
 *     -&gt; return binding-sets
 * </pre>
 *
 * <p>To satisfy the execute-query dereference the container needs the
 * triple {@code &lt;urn:demo:detect_lang&gt; comp:source
 * &lt;file:///opt/wasm/string_lang_detect.wasm&gt;} loaded in the outer index.
 * The recipe below builds a tiny .ttl fixture and points qlever-server at
 * it so both wf_apply.wasm and the target detect_lang wasm sit at
 * {@code /opt/wasm/} inside the container.
 *
 * <p>Boot manually (assumes the qlever-wf-runtime commit registering
 * @0.4.0 has been baked into a fresh qlever-wf:0.6.0 image):
 * <pre>
 *   # Prepare a fixture graph and wasm dir on the host.
 *   mkdir -p /tmp/wf-apply-qlever && cd /tmp/wf-apply-qlever
 *   cp ~/git/webfunctions/target/wasm32-wasip1/release/wf_apply.wasm .
 *   cp ~/git/webfunctions/target/wasm32-wasip1/release/string_lang_detect.wasm .
 *   cat &gt; graph.ttl &lt;&lt;'EOF'
 *   @prefix comp: &lt;http://tegmentum.ai/ns/composition/&gt; .
 *   @prefix :    &lt;urn:demo:&gt; .
 *   :detect_lang comp:source &lt;file:///opt/wasm/string_lang_detect.wasm&gt; .
 *   EOF
 *   # Build a QLever index over graph.ttl (single-file recipe).
 *   docker run --rm -v $(pwd):/data -w /data qlever-wf:0.6.0 \
 *       IndexBuilderMain -F ttl -f graph.ttl -i idx --stxxl-memory-gb 1
 *   # Boot qlever-server with loopback + at least 2 workers.
 *   docker run --rm -d --name qlever-wf --platform linux/arm64 -p 7001:7001 \
 *       -e UID=$(id -u) -e GID=$(id -g) \
 *       -e QLEVER_LOOPBACK_PORT=7001 \
 *       -v $(pwd):/data -w /data \
 *       -v $(pwd):/opt/wasm \
 *       qlever-wf:0.6.0 \
 *       qlever-server --port 7001 --num-simultaneous-queries 4 idx
 * </pre>
 */
public class WfApplyQleverTest {

    private static final String SPARQL_URL = System.getProperty(
            "qlever.sparql.url", "http://localhost:7001/");

    // Default assumes both wasm blobs are bind-mounted at /opt/wasm/ inside
    // the container. Override via -Dqlever.wf_apply.url / -Dqlever.detect.url
    // when boot recipe puts them elsewhere.
    private static final String WF_APPLY_URL = System.getProperty(
            "qlever.wf_apply.url", "file:///opt/wasm/wf_apply.wasm");

    private static final String DETECT_FN_IRI = System.getProperty(
            "qlever.detect.iri", "urn:demo:detect_lang");

    @BeforeClass
    public static void probe() {
        assumeTrue("QLever SPARQL endpoint not reachable at " + SPARQL_URL,
                pingSparql());
    }

    private static boolean pingSparql() {
        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString("ASK {}")
                .build()) {
            qe.execAsk();
            return true;
        } catch (Throwable t) {
            System.err.println("[wf-apply-qlever] probe failed at "
                + SPARQL_URL + ": " + t.getClass().getName() + " — " + t.getMessage());
            return false;
        }
    }

    /**
     * Proves the full deref-and-invoke path on QLever:
     * <ol>
     *   <li>Outer BIND calls wf_apply.wasm with (&lt;detect_lang&gt;, "text").</li>
     *   <li>wf_apply's evaluate reads the outer graph via execute-query to
     *       resolve the {@code comp:source} triple.</li>
     *   <li>wf_apply then calls invoke-wasm with the resolved URL — the
     *       v0.6.0 runtime instantiates the nested guest and returns its
     *       binding-sets verbatim.</li>
     *   <li>The outer BIND surfaces the detected ISO 639-3 code as ?lang.</li>
     * </ol>
     * If any link in that chain isn't wired at @0.4.0, wf_apply either
     * fails the execute-query call (missing v0.4.0 interface) or the
     * invoke-wasm call (missing import registration). Success = "eng".
     */
    @Test
    public void applyDereferencesAndInvokes() {
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?lang WHERE {\n" +
            "  BIND (wf:call(<" + WF_APPLY_URL + ">,\n" +
            "                <" + DETECT_FN_IRI + ">,\n" +
            "                \"The quick brown fox jumps over the lazy dog\") AS ?lang)\n" +
            "}";

        try (QueryExecution qe = QueryExecutionHTTPBuilder.create()
                .endpoint(SPARQL_URL)
                .queryString(sparql)
                .build()) {
            final ResultSet rs = qe.execSelect();
            assertThat(rs.hasNext()).isTrue();
            final QuerySolution row = rs.next();
            assertThat(row.contains("lang"))
                .as("wf_apply must bind a non-UNDEF value")
                .isTrue();
            assertThat(row.getLiteral("lang").getLexicalForm()).isEqualTo("eng");
        }
    }
}
