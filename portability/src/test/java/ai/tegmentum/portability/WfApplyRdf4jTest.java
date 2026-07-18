package ai.tegmentum.portability;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;

import org.eclipse.rdf4j.query.BindingSet;
import org.eclipse.rdf4j.query.QueryLanguage;
import org.eclipse.rdf4j.query.TupleQueryResult;
import org.eclipse.rdf4j.repository.RepositoryConnection;
import org.eclipse.rdf4j.repository.sail.SailRepository;
import org.eclipse.rdf4j.repository.sparql.federation.SPARQLServiceResolver;
import org.eclipse.rdf4j.rio.RDFFormat;
import org.eclipse.rdf4j.sail.memory.MemoryStore;
import org.junit.AfterClass;
import org.junit.BeforeClass;
import org.junit.Test;

import ai.tegmentum.rdf4j.webfunctions.WfEvaluationStrategyFactory;
import ai.tegmentum.rdf4j.webfunctions.WfServiceResolver;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Portability proof for the v0.4 {@code invoke-wasm} host import on
 * RDF4J. Mirror of {@link WfApplyJenaTest} — declares a
 * {@code <urn:demo:detect_lang> comp:source <file://…/string_lang_detect.wasm>}
 * triple in a MemoryStore, then invokes {@code wf_apply.wasm} through
 * the SERVICE envelope. The portable guest dereferences the function
 * IRI via {@code execute-query} and invokes the resolved wasm via
 * {@code invoke-wasm} — both host imports must be wired in the RDF4J
 * plugin's v0.4 registration block.
 */
public class WfApplyRdf4jTest {

    private static final String WF_APPLY_WASM = System.getProperty(
            "wf.apply.wasm",
            System.getProperty("user.home")
                    + "/git/webfunctions/target/wasm32-wasip1/release/wf_apply.wasm");

    private static final String DETECT_WASM = System.getProperty(
            "wf.detect.wasm",
            System.getProperty("user.home")
                    + "/git/webfunctions/target/wasm32-wasip1/release/string_lang_detect.wasm");

    private static SailRepository REPO;

    @BeforeClass
    public static void setUp() throws java.io.IOException {
        assumeTrue("wf_apply.wasm not built",
                new java.io.File(WF_APPLY_WASM).isFile());
        assumeTrue("string_lang_detect.wasm not built",
                new java.io.File(DETECT_WASM).isFile());

        final MemoryStore store = new MemoryStore();
        final SPARQLServiceResolver fallback = new SPARQLServiceResolver();
        final WfServiceResolver resolver = new WfServiceResolver(fallback);
        store.setFederatedServiceResolver(resolver);
        // Strategy factory binds CallbackContext during evaluation — needed
        // so wf_apply's execute-query + invoke-wasm callbacks reach a live
        // TripleSource / ValueFactory.
        store.setEvaluationStrategyFactory(new WfEvaluationStrategyFactory(resolver));

        REPO = new SailRepository(store);
        REPO.init();

        final String turtle =
                "@prefix comp: <http://tegmentum.ai/ns/composition/> .\n" +
                "@prefix :    <urn:demo:> .\n" +
                ":detect_lang comp:source <file://" + DETECT_WASM + "> .\n";
        try (RepositoryConnection conn = REPO.getConnection()) {
            conn.begin();
            conn.add(new ByteArrayInputStream(turtle.getBytes(StandardCharsets.UTF_8)),
                    "urn:demo:", RDFFormat.TURTLE);
            conn.commit();
        }
    }

    @AfterClass
    public static void tearDown() {
        if (REPO != null) REPO.shutDown();
    }

    /**
     * Exercises the full deref-and-invoke path: outer wasm reads the
     * source triple through execute-query, then invokes the inner
     * wasm through invoke-wasm. Success = ISO 639-3 "eng" returned.
     */
    @Test
    public void applyDereferencesAndInvokes() {
        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?lang WHERE {\n" +
            "  BIND (wf:call(<file://" + WF_APPLY_WASM + ">,\n" +
            "                <urn:demo:detect_lang>,\n" +
            "                \"The quick brown fox jumps over the lazy dog\") AS ?lang)\n" +
            "}";

        try (RepositoryConnection conn = REPO.getConnection();
             TupleQueryResult rs = conn.prepareTupleQuery(QueryLanguage.SPARQL, sparql).evaluate()) {
            assertThat(rs.hasNext()).isTrue();
            final BindingSet row = rs.next();
            assertThat(row.getValue("lang").stringValue()).isEqualTo("eng");
        }
    }
}
