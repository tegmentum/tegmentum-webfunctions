package ai.tegmentum.portability;

import java.io.File;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

import org.apache.jena.query.Dataset;
import org.apache.jena.query.DatasetFactory;
import org.apache.jena.query.QueryExecution;
import org.apache.jena.query.QueryExecutionFactory;
import org.apache.jena.query.QueryFactory;
import org.apache.jena.query.QuerySolution;
import org.apache.jena.query.ResultSet;
import org.apache.jena.rdf.model.Model;
import org.apache.jena.rdf.model.ModelFactory;
import org.junit.BeforeClass;
import org.junit.Ignore;
import org.junit.Test;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * Jena counterpart of {@link WfServiceOxigraphTest}. Proves that the
 * {@code SERVICE <wf:call>} envelope handler in the Jena plugin surfaces
 * the wasm's {@code binding-sets { vars, rows }} return as typed SPARQL
 * variable bindings (not a JSON blob) and correctly substitutes outer
 * variable bindings into {@code wf:arg} references before invoking the
 * wasm.
 *
 * <p>The reference guest for the mixed-variant tree walk case
 * ({@code wf_tree_rows.wasm}) is currently blocked by a wasmtime4j
 * 46.0.1-1.2.0 deserializer limitation — see
 * {@link #treeRowsGuestBlockedByWasmtime4jDeserializer} — so this class
 * exercises the SERVICE-handler contract with two uniform-variant guests
 * (multi-row output, and one-in-one-out with variable-arg substitution).
 */
public class WfServiceJenaTest {

    private static final String TO_UPPER_WASM = System.getProperty(
            "wf.toUpper.wasm",
            System.getProperty("user.home")
                    + "/git/stardog-webfunction-plugin/src/test/rust/target/wasm32-wasip1/release/to_upper_component.wasm");

    private static final String MULTI_VAR_WASM = System.getProperty(
            "wf.multiVar.wasm",
            System.getProperty("user.home")
                    + "/git/stardog-webfunction-plugin/src/test/rust/target/wasm32-wasip1/release/multi_var_component.wasm");

    private static final String WF_TREE_ROWS_WASM = System.getProperty(
            "wf.tree.rows.wasm",
            System.getProperty("user.home")
                    + "/git/webfunctions/target/wasm32-wasip1/release/wf_tree_rows.wasm");

    private static final String NS = "urn:";
    private static final String HAS_CHILD = "urn:hasChild";
    private static final String ROOT = NS + "root";
    private static final String A = NS + "a";
    private static final String B = NS + "b";

    private static Dataset dataset;

    @BeforeClass
    public static void setUp() {
        ai.tegmentum.jena.webfunctions.WebFunctionInit.register();

        final Model model = ModelFactory.createDefaultModel();
        model.add(model.createResource(ROOT),
                model.createProperty(HAS_CHILD),
                model.createResource(A));
        model.add(model.createResource(ROOT),
                model.createProperty(HAS_CHILD),
                model.createResource(B));
        dataset = DatasetFactory.create(model);
    }

    /**
     * BGP-envelope parsing: {@code wf:wasm} names the URL, {@code wf:arg}
     * feeds a positional arg (a constant literal here), {@code wf:value_0}
     * projects the wasm's returned column onto a SPARQL variable. Proves
     * the executor decodes the envelope and hands off the args in order.
     */
    @Test
    public void constArgAndSingleColumnProjection() {
        final File wasm = new File(TO_UPPER_WASM);
        assumeTrue("to_upper_component.wasm not built", wasm.exists());

        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?upper WHERE {\n" +
            "  SERVICE <wf:call> {\n" +
            "    _:c wf:wasm    <" + wasm.toURI() + "> ;\n" +
            "        wf:arg     \"stardog\" .\n" +
            "    _:o wf:value_0 ?upper .\n" +
            "  }\n" +
            "}";

        try (QueryExecution qe = QueryExecutionFactory.create(QueryFactory.create(sparql), dataset)) {
            final ResultSet rs = qe.execSelect();
            assertThat(rs.hasNext()).isTrue();
            assertThat(rs.next().getLiteral("upper").getLexicalForm()).isEqualTo("STARDOG");
            assertThat(rs.hasNext()).isFalse();
        }
    }

    /**
     * Load-bearing new capability: {@code wf:arg} may reference an
     * outer-bound variable. The outer {@code VALUES ?input} feeds one row
     * into the SERVICE per input binding; the executor must resolve
     * {@code ?input} from each input binding, thread it into the wasm's
     * positional args, and re-extend the input binding with the output
     * column. Also asserts the outer {@code ?input} still flows through
     * so the executor isn't clobbering the incoming binding.
     */
    @Test
    public void variableArgSubstitutionFromValuesClause() {
        final File wasm = new File(TO_UPPER_WASM);
        assumeTrue("to_upper_component.wasm not built", wasm.exists());

        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?input ?upper WHERE {\n" +
            "  VALUES ?input { \"stardog\" \"jena\" }\n" +
            "  SERVICE <wf:call> {\n" +
            "    _:c wf:wasm    <" + wasm.toURI() + "> ;\n" +
            "        wf:arg     ?input .\n" +
            "    _:o wf:value_0 ?upper .\n" +
            "  }\n" +
            "}";

        final List<String[]> pairs = new ArrayList<>();
        try (QueryExecution qe = QueryExecutionFactory.create(QueryFactory.create(sparql), dataset)) {
            final ResultSet rs = qe.execSelect();
            while (rs.hasNext()) {
                final QuerySolution sol = rs.next();
                pairs.add(new String[] {
                        sol.getLiteral("input").getLexicalForm(),
                        sol.getLiteral("upper").getLexicalForm()});
            }
        }
        // Two inputs → two outputs, each preserving the outer ?input
        // binding and adding an upper-cased ?upper.
        final Set<String> got = new HashSet<>();
        for (String[] p : pairs) got.add(p[0] + "->" + p[1]);
        assertThat(got).containsExactlyInAnyOrder("stardog->STARDOG", "jena->JENA");
    }

    /**
     * Multi-row multi-column projection: the {@code multi_var_component}
     * guest returns two rows across three columns
     * ({@code label}, {@code upper}, {@code length}); the BGP envelope
     * projects all three onto SPARQL variables in one pass. Confirms
     * {@code wf:length}'s integer datatype survives the executor's
     * marshalling (proves the executor doesn't stringify the wasm's
     * typed literals on the way out).
     */
    @Test
    public void multiRowMultiColumnProjection() {
        final File wasm = new File(MULTI_VAR_WASM);
        assumeTrue("multi_var_component.wasm not built", wasm.exists());

        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?label ?upper ?length WHERE {\n" +
            "  SERVICE <wf:call> {\n" +
            "    _:c wf:wasm   <" + wasm.toURI() + "> .\n" +
            "    _:o wf:label  ?label ;\n" +
            "        wf:upper  ?upper ;\n" +
            "        wf:length ?length .\n" +
            "  }\n" +
            "}";

        int rowCount = 0;
        try (QueryExecution qe = QueryExecutionFactory.create(QueryFactory.create(sparql), dataset)) {
            final ResultSet rs = qe.execSelect();
            while (rs.hasNext()) {
                final QuerySolution sol = rs.next();
                rowCount++;
                if ("stardog".equals(sol.getLiteral("label").getLexicalForm())) {
                    assertThat(sol.getLiteral("upper").getLexicalForm()).isEqualTo("STARDOG");
                    assertThat(sol.getLiteral("length").getInt()).isEqualTo(7);
                    assertThat(sol.getLiteral("length").getDatatypeURI())
                            .isEqualTo("http://www.w3.org/2001/XMLSchema#integer");
                }
            }
        }
        assertThat(rowCount).isEqualTo(2);
    }

    /**
     * Reference test for the wf_tree_rows recursive tree-walker guest.
     * Currently disabled: wasmtime4j 46.0.1-1.2.0's WitValue deserializer
     * infers each variant instance's type from its observed case only, so
     * a row containing a binding with {@code value=iri} alongside another
     * with {@code value=literal} fails the deserializer's uniform-list
     * validation ({@code WitList.of} rejects the second binding record
     * because its structurally-equivalent-but-not-{@link Object#equals()}
     * value-field type doesn't match the first). Fix belongs upstream in
     * wasmtime4j — deserialization should build variant types with all
     * cases known, or {@code WitList} should use {@code isCompatibleWith}
     * instead of strict {@code equals}. Re-enable once the fix ships.
     */
    @Test
    public void treeRowsGuestBlockedByWasmtime4jDeserializer() {
        final File wasm = new File(WF_TREE_ROWS_WASM);
        assumeTrue("wf_tree_rows.wasm not built", wasm.exists());

        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?uri ?depth ?parent WHERE {\n" +
            "  SERVICE <wf:call> {\n" +
            "    _:c wf:wasm  <" + wasm.toURI() + "> ;\n" +
            "        wf:arg   <" + ROOT + "> ;\n" +
            "        wf:arg   \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\" .\n" +
            "    _:o wf:uri    ?uri ;\n" +
            "        wf:depth  ?depth ;\n" +
            "        wf:parent ?parent .\n" +
            "  }\n" +
            "  FILTER(?depth > 0)\n" +
            "}";

        try (QueryExecution qe = QueryExecutionFactory.create(QueryFactory.create(sparql), dataset)) {
            int rowCount = 0;
            for (final ResultSet rs = qe.execSelect(); rs.hasNext(); rs.next()) rowCount++;
            assertThat(rowCount).isEqualTo(2);
        }
    }
}
