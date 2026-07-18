package ai.tegmentum.portability;

import java.io.File;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

import org.eclipse.rdf4j.model.IRI;
import org.eclipse.rdf4j.model.Literal;
import org.eclipse.rdf4j.model.Value;
import org.eclipse.rdf4j.model.ValueFactory;
import org.eclipse.rdf4j.model.impl.SimpleValueFactory;
import org.eclipse.rdf4j.query.BindingSet;
import org.eclipse.rdf4j.query.QueryLanguage;
import org.eclipse.rdf4j.query.TupleQuery;
import org.eclipse.rdf4j.query.TupleQueryResult;
import org.eclipse.rdf4j.query.algebra.evaluation.federation.FederatedServiceResolver;
import org.eclipse.rdf4j.repository.RepositoryConnection;
import org.eclipse.rdf4j.repository.sail.SailRepository;
import org.eclipse.rdf4j.repository.sparql.federation.SPARQLServiceResolver;
import org.eclipse.rdf4j.sail.memory.MemoryStore;
import org.junit.AfterClass;
import org.junit.BeforeClass;
import org.junit.Test;

import ai.tegmentum.rdf4j.webfunctions.WfEvaluationStrategyFactory;
import ai.tegmentum.rdf4j.webfunctions.WfServiceResolver;

import static org.assertj.core.api.Assertions.assertThat;
import static org.junit.Assume.assumeTrue;

/**
 * RDF4J counterpart of {@link WfServiceJenaTest}. Exercises the BGP-envelope
 * {@code SERVICE <wf:call>} handler shipped in the rdf4j-webfunction-plugin
 * against the {@code wf_tree_rows} guest — the recursive walk that emits one
 * row per visited node with a typed integer depth, an IRI uri, and an
 * optional IRI parent.
 *
 * <p>Boots an in-process MemoryStore wired with:
 * <ul>
 *   <li>a {@link WfServiceResolver} intercepting the {@code wf:call} SERVICE
 *       URI, and</li>
 *   <li>a {@link WfEvaluationStrategyFactory} so {@code CallbackContext} is
 *       bound during evaluation — mandatory for {@code wf_tree_rows} to
 *       execute its recursive sub-queries via the {@code execute-query}
 *       host callback.</li>
 * </ul>
 *
 * <p>Data: a hand-built 6-node tree rooted at {@code <urn:root>}. Structure
 * is chosen so we can assert the row set exactly (parent+depth per node) and
 * so a {@code FILTER(?depth > 0)} reduces the six rows to five.
 */
public class WfServiceRdf4jTest {

    private static final String TO_UPPER_WASM = System.getProperty(
            "wf.toUpper.wasm",
            System.getProperty("user.home")
                    + "/git/stardog-webfunction-plugin/src/test/rust/target/wasm32-wasip1/release/to_upper_component.wasm");

    private static final String WF_TREE_ROWS_WASM = System.getProperty(
            "wf.tree.rows.wasm",
            System.getProperty("user.home")
                    + "/git/webfunctions/target/wasm32-wasip1/release/wf_tree_rows.wasm");

    private static final String NS = "urn:";
    private static final String HAS_CHILD = "urn:hasChild";
    private static final String ROOT = NS + "root";
    // Six-node tree:
    //   root
    //   ├── a
    //   │   ├── a1
    //   │   └── a2
    //   └── b
    //       └── b1
    // 1 root at depth 0, 2 children at depth 1, 3 grandchildren at depth 2.
    private static final String A = NS + "a";
    private static final String B = NS + "b";
    private static final String A1 = NS + "a1";
    private static final String A2 = NS + "a2";
    private static final String B1 = NS + "b1";

    private static SailRepository REPO;
    private static FederatedServiceResolver FALLBACK;

    @BeforeClass
    public static void setUp() {
        final MemoryStore store = new MemoryStore();

        // The strategy resolves SERVICE URIs through whatever
        // FederatedServiceResolver the factory was constructed with — the
        // one set on the Sail is only used as a default when no explicit
        // resolver reaches the strategy. Wire our WfServiceResolver here so
        // both the `wf:call` short form and the full IRI reach our handler.
        FALLBACK = new SPARQLServiceResolver();
        final WfServiceResolver resolver = new WfServiceResolver(FALLBACK);
        store.setFederatedServiceResolver(resolver);
        // Bind CallbackContext during query evaluation so wf_tree_rows'
        // execute-query callbacks can re-enter SPARQL. Read-only workflow
        // here, so no Sail is threaded through for execute-update.
        store.setEvaluationStrategyFactory(new WfEvaluationStrategyFactory(resolver));

        REPO = new SailRepository(store);
        REPO.init();

        final ValueFactory vf = SimpleValueFactory.getInstance();
        final IRI has = vf.createIRI(HAS_CHILD);
        try (RepositoryConnection conn = REPO.getConnection()) {
            conn.begin();
            conn.add(vf.createIRI(ROOT), has, vf.createIRI(A));
            conn.add(vf.createIRI(ROOT), has, vf.createIRI(B));
            conn.add(vf.createIRI(A),    has, vf.createIRI(A1));
            conn.add(vf.createIRI(A),    has, vf.createIRI(A2));
            conn.add(vf.createIRI(B),    has, vf.createIRI(B1));
            conn.commit();
        }
    }

    @AfterClass
    public static void tearDown() {
        if (REPO != null) REPO.shutDown();
    }

    /**
     * Sanity: the plain non-recursive to_upper guest goes through the
     * SERVICE envelope handler. Proves the parse/resolve/marshal round-trip
     * before we tackle the recursive tree walk.
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

        try (RepositoryConnection conn = REPO.getConnection();
             TupleQueryResult rs = conn.prepareTupleQuery(QueryLanguage.SPARQL, sparql).evaluate()) {
            assertThat(rs.hasNext()).isTrue();
            final BindingSet row = rs.next();
            assertThat(row.getValue("upper").stringValue()).isEqualTo("STARDOG");
            assertThat(rs.hasNext()).isFalse();
        }
    }

    /**
     * Variable-arg substitution: an outer VALUES clause feeds different
     * inputs into the same SERVICE body via {@code wf:arg ?input}. The
     * FederatedService must resolve {@code ?input} from each outer binding,
     * pass it to the wasm, and preserve the outer binding on the way out.
     */
    @Test
    public void variableArgSubstitutionFromValuesClause() {
        final File wasm = new File(TO_UPPER_WASM);
        assumeTrue("to_upper_component.wasm not built", wasm.exists());

        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?input ?upper WHERE {\n" +
            "  VALUES ?input { \"stardog\" \"rdf4j\" }\n" +
            "  SERVICE <wf:call> {\n" +
            "    _:c wf:wasm    <" + wasm.toURI() + "> ;\n" +
            "        wf:arg     ?input .\n" +
            "    _:o wf:value_0 ?upper .\n" +
            "  }\n" +
            "}";

        final Set<String> pairs = new HashSet<>();
        try (RepositoryConnection conn = REPO.getConnection();
             TupleQueryResult rs = conn.prepareTupleQuery(QueryLanguage.SPARQL, sparql).evaluate()) {
            while (rs.hasNext()) {
                final BindingSet row = rs.next();
                pairs.add(row.getValue("input").stringValue()
                        + "->" + row.getValue("upper").stringValue());
            }
        }
        assertThat(pairs).containsExactlyInAnyOrder(
                "stardog->STARDOG", "rdf4j->RDF4J");
    }

    /**
     * Full six-node tree via the recursive walker. Uses the fully-qualified
     * SERVICE IRI (not the short {@code wf:call}) to prove the resolver
     * routes both forms into the same handler.
     */
    @Test
    public void treeRowsSixNodes() {
        final File wasm = new File(WF_TREE_ROWS_WASM);
        assumeTrue("wf_tree_rows.wasm not built", wasm.exists());

        final String sparql =
            "PREFIX wf: <http://tegmentum.ai/ns/webfunction/>\n" +
            "SELECT ?uri ?depth ?parent WHERE {\n" +
            "  SERVICE <http://tegmentum.ai/ns/webfunction/call> {\n" +
            "    _:c wf:wasm  <" + wasm.toURI() + "> ;\n" +
            "        wf:arg   <" + ROOT + "> ;\n" +
            "        wf:arg   \"SELECT ?child WHERE { ?this <" + HAS_CHILD + "> ?child }\" .\n" +
            "    _:o wf:uri    ?uri ;\n" +
            "        wf:depth  ?depth ;\n" +
            "        wf:parent ?parent .\n" +
            "  }\n" +
            "}";

        try (RepositoryConnection conn = REPO.getConnection()) {
            final List<BindingSet> rows = collect(conn, sparql);
            assertThat(rows).as("six-node tree").hasSize(6);

            // Every returned URI is an IRI; every depth is a typed
            // xsd:integer. Wasm returns them via the WIT literal type; the
            // marshaller should surface real Literal/IRI shapes on the way
            // out (not string blobs).
            int rootRows = 0;
            final Set<String> seenUris = new HashSet<>();
            for (BindingSet r : rows) {
                assertThat(r.getValue("uri")).isInstanceOf(IRI.class);
                seenUris.add(r.getValue("uri").stringValue());

                final Value depth = r.getValue("depth");
                assertThat(depth).isInstanceOf(Literal.class);
                final Literal depthLit = (Literal) depth;
                assertThat(depthLit.getDatatype().stringValue())
                        .isEqualTo("http://www.w3.org/2001/XMLSchema#integer");

                if (depthLit.intValue() == 0) {
                    rootRows++;
                    // Root has no parent — should be UNDEF (unbound).
                    assertThat(r.getValue("parent"))
                            .as("root row parent must be UNDEF")
                            .isNull();
                } else {
                    assertThat(r.getValue("parent"))
                            .as("non-root row parent must be IRI")
                            .isInstanceOf(IRI.class);
                }
            }
            assertThat(seenUris).as("distinct URIs across rows")
                    .containsExactlyInAnyOrder(ROOT, A, B, A1, A2, B1);
            assertThat(rootRows).as("exactly one row at depth 0").isEqualTo(1);
        }
    }

    /**
     * Post-SERVICE FILTER on the typed integer depth reduces the six-row
     * result to five. Proves ?depth is a real xsd:integer that survives
     * comparison against a numeric literal — not just a string masquerading
     * as a number.
     */
    @Test
    public void filterOnDepthDropsRoot() {
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

        try (RepositoryConnection conn = REPO.getConnection()) {
            final List<BindingSet> rows = collect(conn, sparql);
            assertThat(rows).as("six-node tree minus root").hasSize(5);
            for (BindingSet r : rows) {
                final Literal d = (Literal) r.getValue("depth");
                assertThat(d.intValue()).isGreaterThan(0);
                assertThat(r.getValue("parent"))
                        .as("non-root row always has a parent IRI")
                        .isInstanceOf(IRI.class);
            }
        }
    }

    private static List<BindingSet> collect(final RepositoryConnection conn, final String sparql) {
        final TupleQuery q = conn.prepareTupleQuery(QueryLanguage.SPARQL, sparql);
        final java.util.ArrayList<BindingSet> out = new java.util.ArrayList<>();
        try (TupleQueryResult rs = q.evaluate()) {
            while (rs.hasNext()) out.add(rs.next());
        }
        return out;
    }
}
