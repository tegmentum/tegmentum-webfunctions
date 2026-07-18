package ai.tegmentum.portability;

import org.apache.jena.query.Dataset;
import org.apache.jena.query.DatasetFactory;
import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.QueryExecutionFactory;
import org.apache.jena.query.ResultSet;
import org.apache.jena.rdf.model.Model;
import org.apache.jena.rdf.model.ModelFactory;
import org.apache.jena.riot.Lang;
import org.apache.jena.riot.RDFDataMgr;
import org.junit.BeforeClass;
import org.junit.Test;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Portability proof for the v0.4 {@code invoke-wasm} host import on
 * Jena. Loads a small graph that declares a "function-by-reference"
 * triple ({@code <urn:demo:detect_lang> comp:source
 * <file://…/string_lang_detect.wasm>}), then invokes
 * {@code wf:call(<wf_apply.wasm>, <urn:demo:detect_lang>, "text")}.
 *
 * <p>The portable {@code wf_apply.wasm} guest dereferences the
 * function IRI through the outer graph via {@code execute-query} and
 * calls the resolved wasm via {@code invoke-wasm}. Both host imports
 * live at {@code stardog:webfunction/host@0.4.0} — this test proves
 * the Jena plugin's v0.4 registration is complete end-to-end.
 */
public class WfApplyJenaTest {

    private static final String WF_APPLY_WASM = System.getProperty(
            "wf.apply.wasm",
            System.getProperty("user.home")
                    + "/git/webfunctions/target/wasm32-wasip1/release/wf_apply.wasm");

    private static final String DETECT_WASM = System.getProperty(
            "wf.detect.wasm",
            System.getProperty("user.home")
                    + "/git/webfunctions/target/wasm32-wasip1/release/string_lang_detect.wasm");

    private static Dataset dataset;

    @BeforeClass
    public static void setUp() {
        assumeTrue("wf_apply.wasm not built",
                new java.io.File(WF_APPLY_WASM).isFile());
        assumeTrue("string_lang_detect.wasm not built",
                new java.io.File(DETECT_WASM).isFile());
        ai.tegmentum.jena.webfunctions.WebFunctionInit.register();

        final String turtle =
                "@prefix comp: <http://tegmentum.ai/ns/composition/> .\n" +
                "@prefix :    <urn:demo:> .\n" +
                ":detect_lang comp:source <file://" + DETECT_WASM + "> .\n";
        final Model model = ModelFactory.createDefaultModel();
        RDFDataMgr.read(model,
                new ByteArrayInputStream(turtle.getBytes(StandardCharsets.UTF_8)),
                Lang.TURTLE);
        dataset = DatasetFactory.create(model);
    }

    @Test
    public void applyDereferencesAndInvokes() {
        // The trailing " AS ?lang" pattern is idiomatic; each row's ?lang
        // will carry the detected ISO 639-3 code returned by string_lang_detect.
        final String query =
                "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
                "SELECT ?lang WHERE {\n" +
                "  BIND (wf:call(<file://" + WF_APPLY_WASM + ">,\n" +
                "                <urn:demo:detect_lang>,\n" +
                "                \"The quick brown fox jumps over the lazy dog\") AS ?lang)\n" +
                "}";
        try (QueryExecution qe = QueryExecutionFactory.create(query, dataset)) {
            final ResultSet rs = qe.execSelect();
            assertThat(rs.hasNext()).isTrue();
            final String detected = rs.next().getLiteral("lang").getLexicalForm();
            assertThat(detected).isEqualTo("eng");
        }
    }
}
