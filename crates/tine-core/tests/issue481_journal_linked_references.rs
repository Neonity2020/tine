//! Research fixture for GH #481: "Linked References" missing on journal pages.
//!
//! NOT a production change. This builds the smallest synthetic graph that
//! exercises the reported scenario end-to-end at the query layer and compares
//! it against an ordinary-page control, across the journal date-format
//! boundary:
//!
//!   1. an ordinary page target (control);
//!   2. a journal page target whose referrer links it by its display title
//!      ("MMM do, yyyy", the OG/Tine default title format);
//!   3. a journal page target in a graph whose title format IS the ISO shape
//!      (`yyyy-MM-dd`), referrer linking `[[2026-09-20]]`.
//!
//! It asserts both the walk path (`Graph::backlinks`) and the indexed path the
//! Tauri command `get_backlinks` actually calls (`backlinks_bounded_indexed`),
//! and verifies the journal page's PageDto name — the string the frontend
//! passes to `getBacklinks` — equals the name the referrer linked.

use tine_core::model::Graph;

#[path = "support/ready_query.rs"]
mod ready_query;

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tine-481-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("journals")).unwrap();
    std::fs::create_dir_all(dir.join("pages")).unwrap();
    std::fs::create_dir_all(dir.join("logseq")).unwrap();
    dir
}

fn group_pages(groups: &[tine_core::model::RefGroup]) -> Vec<String> {
    let mut pages = groups.iter().map(|g| g.page.clone()).collect::<Vec<_>>();
    pages.sort();
    pages
}

/// (1) Control: an ordinary page target collects its referrer.
#[test]
fn ordinary_page_target_collects_referrer() {
    let dir = scratch("control");
    std::fs::write(dir.join("pages/Target.md"), "- the target page\n").unwrap();
    std::fs::write(dir.join("pages/Notes.md"), "- see [[Target]]\n").unwrap();
    std::fs::write(dir.join("logseq/config.edn"), "{}\n").unwrap();

    let g = Graph::open(&dir);
    assert_eq!(
        group_pages(&g.backlinks("Target")),
        vec!["Notes".to_string()],
        "control: an ordinary page target must collect its referrer"
    );
    let indexed =
        ready_query::when_ready(|| g.backlinks_bounded_indexed("Target", 10_000, 16 * 1024 * 1024));
    assert_eq!(group_pages(&indexed.groups), vec!["Notes".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// (2) The reported case: a journal page target, referenced by its default
/// display title. Referrer is an ordinary page AND another journal.
#[test]
fn journal_page_target_collects_referrers_by_default_title() {
    let dir = scratch("journal-default");
    std::fs::write(
        dir.join("journals/2026_09_20.md"),
        "- journal day under test\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("journals/2026_09_16.md"),
        "- today's entry links the future day [[Sep 20th, 2026]]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("pages/Notes.md"),
        "- plan something for [[Sep 20th, 2026]]\n",
    )
    .unwrap();
    std::fs::write(dir.join("logseq/config.edn"), "{}\n").unwrap();

    let g = Graph::open(&dir);

    // The journal page's own identity: findable under its display title, and
    // the PageDto name the frontend would pass to getBacklinks is exactly that
    // title.
    let entry = g
        .find_entry("Sep 20th, 2026", tine_core::PageKind::Journal)
        .expect("journal day must be findable by its display title");
    let dto = g.load_page(&entry).expect("journal day must load");
    assert_eq!(dto.name, "Sep 20th, 2026");

    let mut want = vec!["Notes".to_string(), "Sep 16th, 2026".to_string()];
    want.sort();
    assert_eq!(
        group_pages(&g.backlinks("Sep 20th, 2026")),
        want,
        "GH #481: a journal page target must collect referrers by its display title"
    );
    let indexed = ready_query::when_ready(|| {
        g.backlinks_bounded_indexed("Sep 20th, 2026", 10_000, 16 * 1024 * 1024)
    });
    assert_eq!(group_pages(&indexed.groups), want);

    let _ = std::fs::remove_dir_all(&dir);
}

/// (3) Date-format boundary: a graph whose journal title format is the ISO
/// shape, referrer linking `[[2026-09-20]]`.
#[test]
fn journal_page_target_collects_referrers_by_iso_title_format() {
    let dir = scratch("journal-iso");
    std::fs::write(
        dir.join("journals/2026_09_20.md"),
        "- journal day under test\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("pages/Notes.md"),
        "- plan something for [[2026-09-20]]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("logseq/config.edn"),
        "{:journal/page-title-format \"yyyy-MM-dd\"}\n",
    )
    .unwrap();

    let g = Graph::open(&dir);
    let entry = g
        .find_entry("2026-09-20", tine_core::PageKind::Journal)
        .expect("journal day must be findable by its ISO title");
    let dto = g.load_page(&entry).expect("journal day must load");
    assert_eq!(dto.name, "2026-09-20");

    assert_eq!(
        group_pages(&g.backlinks("2026-09-20")),
        vec!["Notes".to_string()],
        "a journal page named in the graph's ISO title format must collect referrers"
    );
    let indexed = ready_query::when_ready(|| {
        g.backlinks_bounded_indexed("2026-09-20", 10_000, 16 * 1024 * 1024)
    });
    assert_eq!(group_pages(&indexed.groups), vec!["Notes".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// (3b) The reproduced cross-format gap: with the DEFAULT title format, a link
/// written in the ISO shape is an accepted spelling of the real journal day.
#[test]
fn iso_link_under_default_title_format_reaches_the_journal_page() {
    let dir = scratch("iso-default");
    std::fs::write(
        dir.join("journals/2026_09_20.md"),
        "- journal day under test\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("pages/Notes.md"),
        "- plan something for [[2026-09-20]]\n",
    )
    .unwrap();
    std::fs::write(dir.join("logseq/config.edn"), "{}\n").unwrap();

    let g = Graph::open(&dir);
    // The journal is named by the default title format…
    assert!(
        g.find_entry("Sep 20th, 2026", tine_core::PageKind::Journal)
            .is_some(),
        "journal resolves under its default display title"
    );
    // …but the ISO spelling resolves into the same date-equivalence set.
    assert_eq!(
        group_pages(&g.backlinks("Sep 20th, 2026")),
        vec!["Notes".to_string()],
        "the real journal collects a referrer written with the ISO spelling"
    );
    assert_eq!(
        group_pages(&g.backlinks("2026-09-20")),
        vec!["Notes".to_string()],
        "looking up the ISO spelling resolves the same journal backlinks"
    );

    let indexed = ready_query::when_ready(|| {
        g.backlinks_bounded_indexed("Sep 20th, 2026", 10_000, 16 * 1024 * 1024)
    });
    assert_eq!(group_pages(&indexed.groups), vec!["Notes".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}
