//! GH #550: opening a journal day resolves it from its date instead of
//! walking the whole graph inventory, and answers exactly what the
//! inventory lookup would.

use super::*;

fn day(year: i32, month: u32, day: u32) -> JournalDate {
    JournalDate { year, month, day }
}

/// The answer the inventory path gives: `find_entry`, then load.
fn inventory_answer(graph: &Graph, title: &str) -> Option<(String, String, Vec<String>)> {
    let entry = graph.find_entry(title, PageKind::Journal)?;
    let page = graph.load_page(&entry).ok()?;
    Some((
        page.name,
        entry.rel_path,
        page.blocks.into_iter().map(|b| b.raw).collect(),
    ))
}

fn fast_answer(graph: &Graph, title: &str) -> Option<(String, Vec<String>)> {
    graph
        .load_named(title, PageKind::Journal)
        .unwrap()
        .map(|page| (page.name, page.blocks.into_iter().map(|b| b.raw).collect()))
}

#[test]
fn gh550_todays_journal_opens_without_walking_the_graph() {
    let root = scratch("gh550-journal-by-date");
    for index in 0..40 {
        fs::write(root.join(format!("pages/p{index:02}.md")), "- page\n").unwrap();
    }
    fs::write(root.join("journals/2026_09_21.md"), "- today\n").unwrap();
    let graph = Graph::open(&root);
    let title = graph.journal_format.title(day(2026, 9, 21));

    GRAPH_TEXT_INVENTORY_ENTRY_VISITS.with(|visits| visits.set(0));
    let page = graph
        .load_named(&title, PageKind::Journal)
        .unwrap()
        .expect("today's journal exists");
    let visits = GRAPH_TEXT_INVENTORY_ENTRY_VISITS.with(Cell::get);

    assert_eq!(page.kind, PageKind::Journal);
    assert_eq!(page.blocks[0].raw, "today");
    assert!(
        visits < 5,
        "a journal open must not enumerate the graph's 41 files: visits={visits}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn gh550_journal_by_date_answers_what_the_inventory_lookup_answers() {
    let root = scratch("gh550-journal-by-date-siblings");
    fs::write(root.join("pages/other.md"), "- other\n").unwrap();
    // Plain date-stem day.
    fs::write(root.join("journals/2026_09_21.md"), "- plain\n").unwrap();
    // A `title::` that renames the day away from its date.
    fs::write(
        root.join("journals/2026_09_20.md"),
        "title:: Renamed day\n\n- renamed\n",
    )
    .unwrap();
    // Two date-stem files for one day (md + org).
    fs::write(root.join("journals/2026_09_19.md"), "- md twin\n").unwrap();
    fs::write(root.join("journals/2026_09_19.org"), "* org twin\n").unwrap();
    // A title-named stray with no date-stem file.
    let graph = Graph::open(&root);
    let stray_title = graph.journal_format.title(day(2026, 9, 18));
    fs::write(root.join(format!("journals/{stray_title}.md")), "- stray\n").unwrap();
    // A title-named stray beside its date-stem file.
    let both_title = graph.journal_format.title(day(2026, 9, 17));
    fs::write(root.join("journals/2026_09_17.md"), "- stem\n").unwrap();
    fs::write(
        root.join(format!("journals/{both_title}.md")),
        "- stray twin\n",
    )
    .unwrap();
    let graph = Graph::open(&root);

    for date in [
        day(2026, 9, 21),
        day(2026, 9, 20),
        day(2026, 9, 19),
        day(2026, 9, 18),
        day(2026, 9, 17),
        day(2026, 9, 16), // absent
    ] {
        let title = graph.journal_format.title(date);
        let expected = inventory_answer(&graph, &title).map(|(name, _, raw)| (name, raw));
        assert_eq!(fast_answer(&graph, &title), expected, "{title}");
    }
    assert_eq!(
        fast_answer(&graph, &both_title).map(|(_, raw)| raw),
        Some(vec!["stem".to_owned()]),
        "the date-stem file represents the day, as in the feed"
    );
    let _ = fs::remove_dir_all(&root);
}
