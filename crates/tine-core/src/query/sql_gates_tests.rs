//! The two gates SPEC §5 makes this wave's acceptance bar, and the harness both
//! of them run on.
//!
//! * **`walk == SQL`, always.** The lowering and the walk answer the same
//!   question over the same graph, so a difference is a failure of this wave and
//!   never a documented divergence (I-19, I-12). The comparison is at the
//!   PRODUCT level — the block ids a query returns, `tree/filter-top-level-blocks`
//!   included — because that is what a user sees.
//! * **The plan gate (§5.7).** A positively bounded query must reach its anchor
//!   table by `SEARCH`, never `SCAN`, and every bounded relation subquery must
//!   show an index. It runs through `tine-storage`'s `explain_query_plan`
//!   accessor, so it is an ordinary repository test rather than a scratch
//!   harness.
//!
//! **Assert the semantics, not the string.** `blocks` is a rowid table with a
//! BLOB primary key, so SQLite spells the anchor probe `SEARCH b USING
//! [COVERING] INDEX sqlite_autoindex_blocks_1 (block_id=?)` and NEVER `SEARCH b
//! USING PRIMARY KEY` — that spelling is for INTEGER-PK and `WITHOUT ROWID`
//! tables. Same access path, different text.
//!
//! The fast corpus below is the permanent one. `~/research/logseq-anonymized` is
//! an acceptance gate rather than an optional extra (AGENTS §4 tier 2), reached
//! through the `#[ignore]`d twins; when the real graph disagrees with the fast
//! corpus that is a CORPUS DEFECT, and the fix is to extract the minimal shape
//! into the fixture below. No corpus content is read into an assertion message,
//! a receipt or any other artifact — only aggregate counts and plan strings.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tine_storage::sqlite::{PhysicalProjectionQueryReader, PhysicalQueryValue};
use uuid::Uuid;

use crate::date::JournalDate;
use crate::model::Graph;
use crate::query::ir::{Anchor, Bounds, QueryRows};
use crate::query::sql::{lower_query, Lowered, LoweringInputs};
use crate::query::QueryDialect;

/// The Direct Files projection worker is a process-wide singleton per graph and
/// the tests below each start one; serialize them as the neighbouring
/// `direct_projection` tests do.
static GATE_LOCK: Mutex<()> = Mutex::new(());

/// A poisoned lock means a NEIGHBOURING gate failed, which must not turn this
/// gate's own result into a second, misleading failure.
fn serialize() -> std::sync::MutexGuard<'static, ()> {
    GATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn scratch(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tine-query-sql-{tag}-{}", Uuid::new_v4()))
}

/// A graph plus the ready Direct Files projection built from it by the
/// PRODUCTION producer — never by a re-implementation in the test, which would
/// prove only that the test agrees with itself.
struct Corpus {
    graph: Graph,
    reader: PhysicalProjectionQueryReader,
    root: PathBuf,
    owns_root: bool,
}

impl Drop for Corpus {
    fn drop(&mut self) {
        if self.owns_root {
            let _ = std::fs::remove_dir_all(&self.root);
        } else {
            // A real corpus is the user's directory: only the projection this
            // test wrote beside it, in its own temp dir, is removed.
            let _ = std::fs::remove_dir_all(self.projection_dir());
        }
    }
}

impl Corpus {
    fn projection_dir(&self) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tine-query-sql-projection-{}",
            self.root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        ))
    }

    fn open(root: PathBuf, owns_root: bool) -> Corpus {
        let graph = Graph::open(&root);
        graph.warm_cache();
        let projection_dir = std::env::temp_dir().join(format!(
            "tine-query-sql-projection-{}",
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&projection_dir).expect("projection scratch");
        let path = projection_dir.join("projection.sqlite");
        graph
            .attach_direct_projection(path.clone())
            .expect("the projection worker starts");
        let started = Instant::now();
        while !graph.direct_projection_ready_test() {
            assert!(
                started.elapsed() < Duration::from_secs(300),
                "the Direct Files projection did not converge"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let reader = PhysicalProjectionQueryReader::open(&path).expect("the read-only seam opens");
        Corpus {
            graph,
            reader,
            root,
            owns_root,
        }
    }

    fn today(&self) -> JournalDate {
        JournalDate::today()
    }

    /// The walk's answer: the block ids (or page names) one query returns.
    fn walk(&self, source: &str, dialect: QueryDialect) -> BTreeSet<String> {
        let result = crate::query::run_query_result(
            &self.graph,
            source,
            dialect,
            Bounds {
                max_rows: usize::MAX,
                max_bytes: usize::MAX,
            },
        );
        match result.rows {
            QueryRows::Block { groups } => groups
                .into_iter()
                .flat_map(|group| group.blocks.into_iter().map(|block| block.id))
                .collect(),
            QueryRows::Page { pages } => pages
                .into_iter()
                .map(|page| crate::refs::page_key(&page.name))
                .collect(),
        }
    }

    /// The lowering's answer over the same graph, through the D-15 seam.
    fn sql(&self, source: &str, dialect: QueryDialect) -> Result<BTreeSet<String>, &'static str> {
        let today = self.today();
        let (query, _view) = crate::query::parse_query_text(source, dialect, today);
        let registry = self.graph.property_registry();
        let inputs = LoweringInputs {
            today,
            registry: &registry,
            masked_pages: &[],
            cutoff: None,
        };
        let statement = match lower_query(&query, &inputs) {
            Lowered::Statement(statement) => statement,
            Lowered::Unsupported(reason) => return Err(reason),
        };
        let rows = self
            .reader
            .run_projection_query(&statement.sql, &statement.params)
            .unwrap_or_else(|error| {
                panic!("the lowered statement must run: {error}\n{}", statement.sql)
            });
        let anchor = query.anchor;
        Ok(rows
            .into_iter()
            .map(|row| match (anchor, row.first()) {
                (Anchor::Block, Some(PhysicalQueryValue::Blob(id))) => Uuid::from_slice(id)
                    .expect("a 16-byte block id")
                    .to_string(),
                (Anchor::Page, _) => match row.get(1) {
                    Some(PhysicalQueryValue::Text(name)) => crate::refs::page_key(name),
                    other => panic!("a page row selects its name, got {other:?}"),
                },
                (_, other) => panic!("a block row selects its id, got {other:?}"),
            })
            .collect())
    }

    /// `(plan, positively_bounded, matches_nothing)`.
    fn explain(&self, source: &str, dialect: QueryDialect) -> Option<(Vec<String>, bool, bool)> {
        let today = self.today();
        let (query, _view) = crate::query::parse_query_text(source, dialect, today);
        let registry = self.graph.property_registry();
        let inputs = LoweringInputs {
            today,
            registry: &registry,
            masked_pages: &[],
            cutoff: None,
        };
        let Lowered::Statement(statement) = lower_query(&query, &inputs) else {
            return None;
        };
        // The parameters are bound for the EXPLAIN too: with `sqlite_stat4`
        // present the planner may choose differently for a bound value than for
        // an unbound one, and an explain that left them out would measure a
        // statement nobody runs.
        let plan = self
            .reader
            .explain_query_plan(&statement.sql, &statement.params)
            .expect("the plan is available");
        Some((
            plan,
            statement.positively_bounded,
            statement.matches_nothing,
        ))
    }
}

/// The permanent fast corpus. Every shape §5's tables name has a row here, and a
/// disagreement found on the real graph is extracted INTO this function.
fn write_fast_corpus(root: &Path) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    std::fs::create_dir_all(root.join("journals")).expect("journals");

    // Refs (own, ancestor, page), tags, children, and the top-level-root rule:
    // `child under project` matches `[[Project]]` through its ANCESTOR, and is
    // dropped from the result because its parent matched too.
    std::fs::write(
        root.join("pages/refs.md"),
        "- root mentions [[Project]]\n\
         \t- child under project\n\
         \t\t- grandchild under project\n\
         - a #inline-tag line\n\
         - plain line with no reference at all\n",
    )
    .expect("refs page");

    // Tasks, priorities and planning, including the two shapes `tasks` alone
    // cannot answer: a markerless `[#A]` and a markerless `SCHEDULED:`.
    std::fs::write(
        root.join("pages/tasks.md"),
        "- TODO [#A] marked and prioritised\n\
         \t- DONE nested done\n\
         - DOING plain doing\n\
         - [#B] markerless priority\n\
         - markerless schedule\n  SCHEDULED: <2026-06-28 Sun>\n\
         - malformed schedule\n  SCHEDULED: <2026-13-45 Xxx>\n\
         - deadline only\n  DEADLINE: <2026-07-01 Wed>\n\
         - LATER [#a] lowercase priority letter\n",
    )
    .expect("tasks page");

    // Properties: repeated keys, case-varying keys, comma lists, a key-only
    // block, typed values, and an owner with no properties at all (the sparse
    // row that makes a NULL comparison visible).
    std::fs::write(
        root.join("pages/props.md"),
        "type:: Page\n\
         tags:: Genre, Reference\n\
         \n\
         - k:: a\n\
         - k:: a\n\
         - K:: b\n\
         - k:: a, c\n\
         - status:: open\n\
         - priority:: done\n\
         - score:: 12\n\
         - due:: [[Jun 28th, 2026]]\n\
         - blank::\n\
         - a block with no properties\n",
    )
    .expect("props page");

    // Namespaces and page-name ranges.
    std::fs::write(root.join("pages/Proj.md"), "- the namespace parent\n").expect("Proj");
    std::fs::write(
        root.join("pages/Proj%2FAlpha.md"),
        "- inside the namespace\n",
    )
    .expect("Proj/Alpha");
    std::fs::write(
        root.join("pages/Proj%2FAlpha%2FDeep.md"),
        "- two levels down\n",
    )
    .expect("Proj/Alpha/Deep");

    // Content: repeated whitespace and a line break, which `searchable_text`
    // collapses and `query_visible` does not.
    std::fs::write(
        root.join("pages/content.md"),
        "- alpha  beta\n- alpha beta\n- ALPHA BETA gamma\n- 100% literal_underscore\n",
    )
    .expect("content page");

    // Journals: one that parses, and one whose stem does not.
    std::fs::write(
        root.join("journals/2026_06_28.md"),
        "- a journal entry [[Project]]\n",
    )
    .expect("journal");
    std::fs::write(root.join("journals/not_a_date.md"), "- an unparsed stem\n")
        .expect("odd journal");
}

/// Every query shape this wave lowers, in both dialects where both spell it.
/// The two engines must agree on every one of them.
const IDENTITY_SHAPES: &[(&str, QueryDialect)] = &[
    // refs — the flagship leaf, through the ancestor closure and the page
    ("[[Project]]", QueryDialect::Og),
    ("(page-ref Project)", QueryDialect::Og),
    ("#inline-tag", QueryDialect::Og),
    ("ref('Project')", QueryDialect::Tql),
    ("not ref('Project')", QueryDialect::Tql),
    ("tag('inline-tag')", QueryDialect::Tql),
    ("not tag('inline-tag')", QueryDialect::Tql),
    // tasks, priorities, planning
    ("(task TODO)", QueryDialect::Og),
    ("(task TODO DOING)", QueryDialect::Og),
    ("(not (task DONE))", QueryDialect::Og),
    ("(priority A)", QueryDialect::Og),
    ("(priority A B)", QueryDialect::Og),
    ("task = 'todo'", QueryDialect::Tql),
    ("task != 'DONE'", QueryDialect::Tql),
    ("task in ('TODO', 'DOING')", QueryDialect::Tql),
    ("task not in ('DONE')", QueryDialect::Tql),
    ("task is not null", QueryDialect::Tql),
    ("task is null", QueryDialect::Tql),
    ("priority = 'a'", QueryDialect::Tql),
    ("priority != 'A'", QueryDialect::Tql),
    ("priority is not null", QueryDialect::Tql),
    ("priority is null", QueryDialect::Tql),
    ("scheduled is not null", QueryDialect::Tql),
    ("scheduled is null", QueryDialect::Tql),
    ("deadline is not null", QueryDialect::Tql),
    ("scheduled = '2026-06-28'", QueryDialect::Tql),
    ("scheduled >= '2026-06-01'", QueryDialect::Tql),
    ("scheduled < '2026-07-01'", QueryDialect::Tql),
    ("scheduled != '2026-06-28'", QueryDialect::Tql),
    (
        "scheduled between '2026-06-01' and '2026-07-01'",
        QueryDialect::Tql,
    ),
    ("deadline > '2026-06-30'", QueryDialect::Tql),
    // properties — every §3.3 form
    ("(property status open)", QueryDialect::Og),
    ("(property k a)", QueryDialect::Og),
    ("(property k c)", QueryDialect::Og),
    ("(property k b)", QueryDialect::Og),
    ("(property score 12)", QueryDialect::Og),
    ("(property blank)", QueryDialect::Og),
    ("(property type Page)", QueryDialect::Og),
    ("(page-property type Page)", QueryDialect::Og),
    ("(page-tags Genre)", QueryDialect::Og),
    ("(all-page-tags)", QueryDialect::Og),
    (
        "(and (property status open) (property priority done))",
        QueryDialect::Og,
    ),
    ("prop('k') is not null", QueryDialect::Tql),
    ("prop('k') is null", QueryDialect::Tql),
    ("prop('k') = 'a'", QueryDialect::Tql),
    ("prop('k') != 'a'", QueryDialect::Tql),
    ("prop('k') in ('a', 'c')", QueryDialect::Tql),
    ("prop('k') not in ('a')", QueryDialect::Tql),
    ("prop('k') like 'a%'", QueryDialect::Tql),
    ("prop('k') = ''", QueryDialect::Tql),
    ("prop('blank') = ''", QueryDialect::Tql),
    ("prop('score') > 5", QueryDialect::Tql),
    ("prop('score') <= 12", QueryDialect::Tql),
    ("prop('score') = 12", QueryDialect::Tql),
    ("prop('score') between 1 and 20", QueryDialect::Tql),
    ("every(prop('k'), value = 'a')", QueryDialect::Tql),
    ("every(prop('k'), value != 'zzz')", QueryDialect::Tql),
    ("none(prop('k'), value = 'a')", QueryDialect::Tql),
    ("any(prop('k'), value like 'a%')", QueryDialect::Tql),
    ("page_prop('type') = 'page'", QueryDialect::Tql),
    ("page_prop('type') is not null", QueryDialect::Tql),
    ("page_tag('Genre')", QueryDialect::Tql),
    // content
    ("\"alpha beta\"", QueryDialect::Og),
    ("content like '%alpha%'", QueryDialect::Tql),
    ("content = 'alpha  beta'", QueryDialect::Tql),
    ("content != 'alpha beta'", QueryDialect::Tql),
    ("content like 'alpha%'", QueryDialect::Tql),
    ("content like '100\\%%'", QueryDialect::Tql),
    ("content like '%literal\\_underscore%'", QueryDialect::Tql),
    // page attributes, reached from the block anchor through `page`
    ("(page refs)", QueryDialect::Og),
    ("(namespace Proj)", QueryDialect::Og),
    ("(journal)", QueryDialect::Og),
    ("(between '2026-06-01' '2026-07-01')", QueryDialect::Og),
    ("page.name = 'refs'", QueryDialect::Tql),
    ("page.name != 'refs'", QueryDialect::Tql),
    ("page.name in ('refs', 'tasks')", QueryDialect::Tql),
    ("page.name like 'proj/%'", QueryDialect::Tql),
    ("page.name like '%roj%'", QueryDialect::Tql),
    ("page.journal = true", QueryDialect::Tql),
    ("page.journal = false", QueryDialect::Tql),
    ("page.day is not null", QueryDialect::Tql),
    ("page.day is null", QueryDialect::Tql),
    ("page.day >= '2026-01-01'", QueryDialect::Tql),
    ("page.namespace = 'proj'", QueryDialect::Tql),
    ("page.namespace = 'proj/alpha'", QueryDialect::Tql),
    ("page.namespace is not null", QueryDialect::Tql),
    ("page.namespace is null", QueryDialect::Tql),
    ("page.namespace != 'proj'", QueryDialect::Tql),
    // page-anchored rows
    ("@page and journal = true", QueryDialect::Tql),
    ("@page and journal = false", QueryDialect::Tql),
    ("@page and name like 'proj/%'", QueryDialect::Tql),
    ("@page and name = 'props'", QueryDialect::Tql),
    ("@page and day >= '2026-01-01'", QueryDialect::Tql),
    ("@page and day is not null", QueryDialect::Tql),
    ("@page and prop('type') is not null", QueryDialect::Tql),
    ("@page and prop('type') = 'page'", QueryDialect::Tql),
    ("@page and namespace = 'proj'", QueryDialect::Tql),
    ("@page and not name = 'refs'", QueryDialect::Tql),
    // `blocks` is a page relation the walk answers false for; the lowering
    // reproduces that rather than inventing an answer.
    ("@page and any(blocks, task = 'TODO')", QueryDialect::Tql),
    // relations and boolean composition, including the two `every` polarities
    ("any(children, task = 'DONE')", QueryDialect::Tql),
    ("none(children, task = 'DONE')", QueryDialect::Tql),
    ("every(children, task = 'DONE')", QueryDialect::Tql),
    ("every(children, not task = 'DONE')", QueryDialect::Tql),
    ("any(children, content like '%nested%')", QueryDialect::Tql),
    (
        "(or (and (task TODO) (priority A)) (property status open))",
        QueryDialect::Og,
    ),
    ("(and (task TODO) (not [[Project]]))", QueryDialect::Og),
    ("(or [[Project]] (task DOING))", QueryDialect::Og),
];

/// The `walk == SQL` acceptance gate, over the permanent fast corpus.
#[test]
fn the_walk_and_the_lowering_answer_every_shape_identically() {
    let _serial = serialize();
    let root = scratch("identity");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let differences = compare_every_shape(&corpus);
    assert!(
        differences.is_empty(),
        "the walk and the lowering disagree:\n{}",
        differences.join("\n")
    );
}

/// The same gate over the anonymized graph (AGENTS §4 tier 2). A disagreement
/// here is a CORPUS DEFECT in the fixture above: extract the minimal shape into
/// `write_fast_corpus`, never weaken the gate. Only shape sources and counts are
/// printed — no page name, block text or property value.
#[test]
#[ignore = "acceptance gate over a real corpus: set TINE_QUERY_IDENTITY_GRAPH"]
fn the_walk_and_the_lowering_agree_over_a_real_corpus() {
    let _serial = serialize();
    let Some(root) = std::env::var_os("TINE_QUERY_IDENTITY_GRAPH") else {
        eprintln!("skipped: set TINE_QUERY_IDENTITY_GRAPH to a corpus directory");
        return;
    };
    let corpus = Corpus::open(PathBuf::from(&root), false);
    let mut answered = 0usize;
    let mut declined = 0usize;
    let mut rows = 0usize;
    for (source, dialect) in IDENTITY_SHAPES {
        match corpus.sql(source, *dialect) {
            Ok(sql) => {
                answered += 1;
                rows += sql.len();
            }
            Err(_) => declined += 1,
        }
    }
    let differences = compare_every_shape(&corpus);
    eprintln!(
        "walk_sql_identity_over_a_real_corpus shapes={} answered={answered} declined={declined} \
         matched_rows={rows} disagreements={}",
        IDENTITY_SHAPES.len(),
        differences.len()
    );
    assert!(
        differences.is_empty(),
        "the walk and the lowering disagree on a real graph (shape sources only):\n{}",
        differences.join("\n")
    );
}

/// Compares both engines on every shape, returning ONE line per disagreement.
/// The line names the query source and the two cardinalities and nothing else,
/// so it is safe to print for a real corpus.
fn compare_every_shape(corpus: &Corpus) -> Vec<String> {
    let mut differences = Vec::new();
    for (source, dialect) in IDENTITY_SHAPES {
        let Ok(sql) = corpus.sql(source, *dialect) else {
            continue;
        };
        let walk = corpus.walk(source, *dialect);
        if walk != sql {
            differences.push(format!(
                "{source}: walk={} sql={} only_in_walk={} only_in_sql={}",
                walk.len(),
                sql.len(),
                walk.difference(&sql).count(),
                sql.difference(&walk).count()
            ));
        }
    }
    differences
}

/// A gate that cannot fail is not a gate: the fast corpus must actually produce
/// matches, or `walk == SQL` would be `{} == {}` for every shape.
#[test]
fn the_fast_corpus_answers_every_shape_it_can_and_matches_something() {
    let _serial = serialize();
    let root = scratch("coverage");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let mut nonempty = 0usize;
    let mut declined: Vec<&str> = Vec::new();
    for (source, dialect) in IDENTITY_SHAPES {
        match corpus.sql(source, *dialect) {
            Ok(rows) if !rows.is_empty() => nonempty += 1,
            Ok(_) => {}
            Err(_) => declined.push(source),
        }
    }
    assert!(
        declined.is_empty(),
        "no shape in the identity corpus may be declined: {declined:?}"
    );
    assert!(
        nonempty * 2 >= IDENTITY_SHAPES.len(),
        "only {nonempty} of {} shapes match anything; the corpus is too thin to prove identity",
        IDENTITY_SHAPES.len()
    );
}

/// The shapes §5.7's table calls positively bounded, plus the two presence
/// probes the dossier names as a hard stop, plus the two controls §5.7 predicts
/// will NOT be bounded.
const PLAN_SHAPES: &[(&str, QueryDialect)] = &[
    ("[[Project]]", QueryDialect::Og),
    ("#inline-tag", QueryDialect::Og),
    ("tag('inline-tag')", QueryDialect::Tql),
    ("(task TODO)", QueryDialect::Og),
    ("(property status open)", QueryDialect::Og),
    (
        "(and (property status open) (property priority done))",
        QueryDialect::Og,
    ),
    ("(priority A)", QueryDialect::Og),
    ("scheduled is not null", QueryDialect::Tql),
    ("deadline is not null", QueryDialect::Tql),
    ("scheduled >= '2026-06-01'", QueryDialect::Tql),
    ("deadline < '2026-07-01'", QueryDialect::Tql),
    ("prop('score') > 5", QueryDialect::Tql),
    ("prop('k') = 'a'", QueryDialect::Tql),
    ("every(prop('k'), value = 'a')", QueryDialect::Tql),
    ("any(children, task = 'DONE')", QueryDialect::Tql),
    ("page.name = 'refs'", QueryDialect::Tql),
    ("@page and name like 'proj/%'", QueryDialect::Tql),
    ("@page and day >= '2026-01-01'", QueryDialect::Tql),
    ("@page and prop('type') is not null", QueryDialect::Tql),
];

/// §5.7's plan gate. **A failing plan gate is information, not an obstacle:**
/// nothing here reclassifies a leaf or relaxes an assertion to make a plan pass.
#[test]
fn a_positively_bounded_query_searches_its_anchor_and_indexes_its_subqueries() {
    let _serial = serialize();
    let root = scratch("plan");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let (failures, vacuous) = measure_plans(&corpus);
    assert!(
        failures.is_empty(),
        "§5.7 plan gate failures:\n{}",
        failures.join("\n")
    );
    // The fast corpus is written so that EVERY plan shape has rows to find. If
    // one folds to the empty statement here, the gate has stopped measuring it
    // and the fixture — not the assertion — is what has to change.
    assert!(
        vacuous.is_empty(),
        "the fast corpus no longer makes these plan shapes satisfiable, so the gate \
         is not measuring them:\n{}",
        vacuous.join("\n")
    );
}

/// The same gate on the anonymized graph, where the row counts are real and the
/// planner's choices are the ones that matter (AGENTS §4 tier 2).
#[test]
#[ignore = "plan gate over a real corpus: set TINE_QUERY_IDENTITY_GRAPH"]
fn the_plan_gate_holds_over_a_real_corpus() {
    let _serial = serialize();
    let Some(root) = std::env::var_os("TINE_QUERY_IDENTITY_GRAPH") else {
        eprintln!("skipped: set TINE_QUERY_IDENTITY_GRAPH to a corpus directory");
        return;
    };
    let corpus = Corpus::open(PathBuf::from(&root), false);
    let (failures, vacuous) = measure_plans(&corpus);
    for (source, dialect) in PLAN_SHAPES {
        if let Some((plan, bounded, nothing)) = corpus.explain(source, *dialect) {
            let tag = if nothing {
                "vacuous"
            } else if bounded {
                "bounded"
            } else {
                "unbounded"
            };
            eprintln!("plan[{tag}] {source} :: {}", plan.join(" | "));
        }
    }
    // A shape whose predicate is unsatisfiable ON THIS CORPUS is reported, not
    // asserted: the key does not exist here with the type the operator needs, so
    // the statement reads no row and there is no index for SQLite to choose.
    // Every such shape is measured for real on the fast corpus, where it does
    // have rows — this is a property of the graph, not a relaxed gate.
    for line in &vacuous {
        eprintln!("plan[skipped] {line}");
    }
    assert!(
        failures.is_empty(),
        "§5.7 plan gate failures on a real graph:\n{}",
        failures.join("\n")
    );
}

/// `(failures, shapes that provably read nothing on this corpus)`.
fn measure_plans(corpus: &Corpus) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    let mut vacuous = Vec::new();
    for (source, dialect) in PLAN_SHAPES {
        let Some((plan, bounded, nothing)) = corpus.explain(source, *dialect) else {
            failures.push(format!("{source}: the lowering declined a plan-gate shape"));
            continue;
        };
        if nothing {
            vacuous.push(format!(
                "{source}: unsatisfiable on this corpus (the key's effective type \
                 rejects the operator), so the statement reads no row"
            ));
            continue;
        }
        if !bounded {
            failures.push(format!(
                "{source}: §5.7's table calls this bounded and the classifier does not"
            ));
            continue;
        }
        // The anchor alias is `b` for a block row and `p` for a page row; the
        // statement's own FROM decides which. Assert SEARCH on it, never SCAN —
        // the SPELLING of the probe is SQLite's business (a BLOB primary key is
        // reached through `sqlite_autoindex_blocks_1`, never through the words
        // "PRIMARY KEY").
        let anchor = if plan.iter().any(|step| step.contains(" blocks AS b ")) {
            "b"
        } else {
            "p"
        };
        if plan.iter().any(|step| {
            step.starts_with(&format!("SCAN {anchor} ")) || *step == format!("SCAN {anchor}")
        }) {
            failures.push(format!(
                "{source}: the anchor is SCANned: {}",
                plan.join(" | ")
            ));
            continue;
        }
        if !plan
            .iter()
            .any(|step| step.starts_with(&format!("SEARCH {anchor} ")))
        {
            failures.push(format!(
                "{source}: no SEARCH on the anchor alias {anchor}: {}",
                plan.join(" | ")
            ));
            continue;
        }
        // Every relation subquery of a bounded query must reach its own table by
        // an index — a facet-table scan is accepted only where §5.7 says so
        // (`task != 'DONE'` scans the small `tasks` table), and none of the
        // shapes above is one.
        let scans: Vec<&String> = plan
            .iter()
            .filter(|step| {
                step.starts_with("SCAN ") && !step.starts_with(&format!("SCAN {anchor}"))
            })
            .collect();
        if !scans.is_empty() {
            failures.push(format!(
                "{source}: a bounded relation subquery scans: {}",
                plan.join(" | ")
            ));
        }
    }
    (failures, vacuous)
}

/// The paired-base performance receipt (AGENTS §4): the SAME queries, on the
/// SAME machine, in one session, over the anonymized graph — the walk and the
/// SQL path side by side. A synthetic page cannot answer "is this faster on my
/// graph", so this reports only what it measured and on which corpus.
#[test]
#[ignore = "paired-base performance receipt: set TINE_QUERY_IDENTITY_GRAPH"]
fn the_walk_and_the_lowering_are_timed_against_each_other_on_a_real_corpus() {
    let _serial = serialize();
    let Some(root) = std::env::var_os("TINE_QUERY_IDENTITY_GRAPH") else {
        eprintln!("skipped: set TINE_QUERY_IDENTITY_GRAPH to a corpus directory");
        return;
    };
    let corpus = Corpus::open(PathBuf::from(&root), false);
    const REPEATS: u32 = 5;
    eprintln!("paired_base_query_receipt corpus=real repeats={REPEATS}");
    for (source, dialect) in PLAN_SHAPES {
        // Warm both sides once so neither pays for the other's first-touch cost.
        let Ok(first) = corpus.sql(source, *dialect) else {
            continue;
        };
        let _ = corpus.walk(source, *dialect);
        let walk_start = Instant::now();
        for _ in 0..REPEATS {
            let _ = corpus.walk(source, *dialect);
        }
        let walk = walk_start.elapsed() / REPEATS;
        let sql_start = Instant::now();
        for _ in 0..REPEATS {
            let _ = corpus.sql(source, *dialect);
        }
        let sql = sql_start.elapsed() / REPEATS;
        eprintln!(
            "paired_base shape={source:?} rows={} walk_us={} sql_us={}",
            first.len(),
            walk.as_micros(),
            sql.as_micros()
        );
    }
}
