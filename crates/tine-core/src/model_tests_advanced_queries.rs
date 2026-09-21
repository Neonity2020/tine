//! GH #542: advanced queries written with DataScript attribute patterns, run
//! end to end over a real graph and compared with what Logseq answers.

use super::*;

const TASKS: &str = "\
- TODO plain
- NOW doing it
- DOING in progress
- LATER someday
  SCHEDULED: <2026-06-25 Thu>
- TODO far future
  SCHEDULED: <2099-01-01 Thu>
- TODO with deadline
  DEADLINE: <2026-06-30 Tue>
- DONE finished
- TODO routine chore
  tags:: Routine
  SCHEDULED: <2026-06-24 Wed>
- no marker here
";

fn answer(graph: &Graph, query: &str) -> (Vec<String>, crate::query::AdvancedResult) {
    let result = when_ready(|| graph.run_advanced_query(query, None));
    let mut raws: Vec<String> = result
        .groups
        .iter()
        .flat_map(|group| group.blocks.iter())
        .map(|block| block.raw.lines().next().unwrap_or("").to_owned())
        .collect();
    raws.sort();
    (raws, result)
}

fn sorted(items: &[&str]) -> Vec<String> {
    let mut items: Vec<String> = items.iter().map(|item| (*item).to_owned()).collect();
    items.sort();
    items
}

#[test]
fn gh542_reporter_advanced_queries_answer_like_logseq() {
    let dir = scratch("gh542-advanced");
    fs::write(dir.join("journals/2026_06_20.md"), TASKS).unwrap();
    fs::write(
        dir.join("pages/Note.md"),
        "- TODO not a journal\n- NOW page now\n",
    )
    .unwrap();
    let graph = ready_graph(&dir);

    // The reporter's first query (also Tine's own screenshot fixture).
    let (raws, result) = answer(
        &graph,
        r#"{:title "Recent tasks"
            :query [:find (pull ?b [*])
                    :where [?b :block/marker "TODO"]]}"#,
    );
    assert!(result.supported, "{:?}", result.ignored);
    assert_eq!(
        raws,
        sorted(&[
            "TODO plain",
            "TODO far future",
            "TODO with deadline",
            "TODO routine chore",
            "TODO not a journal",
        ])
    );

    // Any marker at all.
    let (raws, result) = answer(
        &graph,
        r#"{:title "Open work" :query [:find (pull ?b [*]) :where [?b :block/marker ?m]]}"#,
    );
    assert!(result.supported, "{:?}", result.ignored);
    assert_eq!(raws.len(), 10, "{raws:?}");
    assert!(!raws.contains(&"no marker here".to_owned()));

    // `not=` keeps every other marker, and still only blocks that have one.
    let (raws, result) = answer(
        &graph,
        r#"[:find (pull ?b [*]) :where [?b :block/marker ?m] [(not= ?m "DONE")]]"#,
    );
    assert!(result.ignored.is_empty(), "{:?}", result.ignored);
    assert_eq!(raws.len(), 9, "{raws:?}");
    assert!(!raws.contains(&"DONE finished".to_owned()));

    // NOW: a marker variable narrowed by `contains?`. The result transform
    // is a Clojure function Tine does not run; it only orders, and it is
    // reported rather than silently dropped.
    let (raws, result) = answer(
        &graph,
        r#"{:title "NOW"
            :query [:find (pull ?h [*])
                    :where
                    [?h :block/marker ?marker]
                    [(contains? #{"NOW" "DOING"} ?marker)]]
            :result-transform (fn [result] (sort-by (fn [h] (get h :block/priority "Z")) result))}"#,
    );
    assert!(result.supported, "{:?}", result.ignored);
    assert_eq!(
        raws,
        sorted(&["NOW doing it", "DOING in progress", "NOW page now"])
    );
    assert_eq!(result.ignored, vec!["result-transform".to_owned()]);

    // NEXT, with its result variable matching its clauses: tasks on journal
    // pages that have a journal day.
    let (raws, result) = answer(
        &graph,
        r#"{:query [:find (pull ?h [*])
                    :where
                    [?h :block/marker ?marker]
                    [(contains? #{"NOW" "LATER" "TODO"} ?marker)]
                    [?h :block/page ?p]
                    [?p :block/journal? true]
                    [?p :block/journal-day ?d]]}"#,
    );
    assert!(result.supported, "{:?}", result.ignored);
    assert!(result.ignored.is_empty(), "{:?}", result.ignored);
    assert_eq!(
        raws,
        sorted(&[
            "TODO plain",
            "NOW doing it",
            "LATER someday",
            "TODO far future",
            "TODO with deadline",
            "TODO routine chore",
        ])
    );

    // As the reporter wrote NEXT, it pulls `?b` while every clause binds `?h`,
    // which Logseq refuses too (`?b` is never bound). It must not answer.
    let (raws, result) = answer(
        &graph,
        r#"{:query [:find (pull ?b [*])
                    :where
                    [?h :block/marker ?marker]
                    [(contains? #{"NOW" "LATER" "TODO"} ?marker)]]}"#,
    );
    assert!(!result.supported);
    assert!(raws.is_empty());

    // Scheduled: scheduled on or before today, excluding Routine.
    let (raws, result) = answer(
        &graph,
        r#"{:query [:find (pull ?b [*])
                    :in $ ?next
                    :where
                    (task ?b #{"NOW" "LATER" "TODO" "DOING" "WAIT" "WAITING"})
                    (not (property ?b :tags "Routine"))
                    (or-join [?b ?d]
                             [?b :block/scheduled ?d])
                    [(<= ?d ?next)]]
            :inputs [:today]}"#,
    );
    assert!(result.supported, "{:?}", result.ignored);
    assert!(result.ignored.is_empty(), "{:?}", result.ignored);
    assert_eq!(raws, sorted(&["LATER someday"]));

    // OVERDUE: NOW/TODO with neither a schedule nor a deadline.
    let (raws, result) = answer(
        &graph,
        r#"{:query [:find (pull ?b [*])
                    :where
                    (task ?b #{"NOW" "TODO"})
                    (not [?b :block/scheduled ?d])
                    (not [?b :block/deadline ?d])]}"#,
    );
    assert!(result.supported, "{:?}", result.ignored);
    assert!(result.ignored.is_empty(), "{:?}", result.ignored);
    assert_eq!(
        raws,
        sorted(&[
            "TODO plain",
            "NOW doing it",
            "TODO not a journal",
            "NOW page now"
        ])
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A `not` or `or` Tine only partly understands must not run as the part it
/// understood: `not` of a narrower clause removes blocks the query keeps, and
/// `or` of fewer branches drops blocks the query returns.
#[test]
fn gh542_a_partly_understood_not_or_or_is_not_narrowed() {
    let dir = scratch("gh542-partial-negation");
    fs::write(dir.join("journals/2026_06_20.md"), TASKS).unwrap();
    let graph = ready_graph(&dir);

    // `(not (and (task ?b #{"TODO"}) (bogus ?b)))` keeps every TODO for which
    // `bogus` fails. Running only `(not (task ?b #{"TODO"}))` would drop them.
    let (raws, result) = answer(
        &graph,
        r#"[:find (pull ?b [*])
            :where (task ?b #{"TODO"}) (not (and (task ?b #{"TODO"}) (bogus ?b)))]"#,
    );
    assert!(
        raws.iter().filter(|raw| raw.starts_with("TODO")).count() == 4,
        "{raws:?} ignored={:?}",
        result.ignored
    );
    assert!(!result.ignored.is_empty());

    // `(or (task ?b #{"DONE"}) (bogus ?b))` may return more than DONE blocks.
    let (raws, result) = answer(
        &graph,
        r#"[:find (pull ?b [*]) :where (task ?b #{"TODO" "DONE"}) (or (task ?b #{"DONE"}) (bogus ?b))]"#,
    );
    assert!(
        raws.iter().filter(|raw| raw.starts_with("TODO")).count() == 4,
        "{raws:?} ignored={:?}",
        result.ignored
    );
    assert!(!result.ignored.is_empty());
    let _ = fs::remove_dir_all(&dir);
}
