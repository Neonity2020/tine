//! GH #543, indexing audit round 11: each finding's class, pinned. The
//! fixtures follow the auditor's probes (evidence `indexing-audit-r11`).

use super::gh543_r10::{r10_finish, r10_pages, r10_prebuild, r10_scratch, r10_settle, R10Owner};
use super::*;
use std::sync::Arc;
use std::time::Duration;

fn rows_of(database: &Path, sql: &str, path: &str) -> i64 {
    rusqlite::Connection::open(database)
        .unwrap()
        .query_row(sql, rusqlite::params![path], |row| row.get(0))
        .unwrap()
}

/// R11-01 (decision DK4): a parse configuration changed while Tine was closed,
/// over a graph with one page it cannot read now. That page once made the
/// snapshot "incomplete", and an incomplete snapshot was always repaired in
/// place: every page re-lowered in one turn nothing could stop, a close
/// waiting all of it (74 s on 10k pages). It is now built fresh like any
/// config change, and the fresh build keeps the unreadable page -- its text
/// stays searchable, its references stay counted -- and reads it from its
/// file again once it can.
#[test]
fn gh543_a_config_change_over_an_unreadable_page_rebuilds_and_keeps_it() {
    let root = r10_scratch("cfg-unreadable");
    r10_pages(&root, 12);
    let fragile = root.join("pages/fragile.md");
    fs::write(
        &fragile,
        "- fragile carried sentinel [[p0]]\n  - fragile child line\n",
    )
    .unwrap();
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    let postings = "SELECT COUNT(*) FROM reference_postings r JOIN pages p ON p.page_id = r.source_page_id WHERE p.path = ?1";
    let blocks =
        "SELECT COUNT(*) FROM blocks b JOIN pages p ON p.page_id = b.page_id WHERE p.path = ?1";
    let postings_before = rows_of(&database, postings, "pages/fragile.md");
    assert!(postings_before > 0, "the fixture page references nothing");

    fs::write(&fragile, [0x2d, 0x20, 0xff, 0xfe, 0x0a]).unwrap();
    fs::create_dir_all(root.join("logseq")).unwrap();
    fs::write(
        root.join("logseq/config.edn"),
        "{:property/separated-by-commas #{:foo}}\n",
    )
    .unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database.clone()).unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(20)));
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let projection = graph.direct_projection_test().unwrap();
    assert_eq!(
        projection.fresh_builds_test(),
        1,
        "the config change was not built fresh: {}",
        projection.debug_state_test()
    );
    assert!(
        !graph
            .search("fragile carried sentinel", 50)
            .unwrap()
            .is_empty(),
        "the unreadable page left the index"
    );
    assert_eq!(rows_of(&database, blocks, "pages/fragile.md"), 2);
    assert_eq!(
        rows_of(&database, postings, "pages/fragile.md"),
        postings_before
    );
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    drop(graph);

    // Readable again: the stored revision still names the old bytes, so the
    // page is read from its file, not kept as carried.
    fs::write(&fragile, "- fragile restored from its file\n").unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database.clone()).unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(20)));
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    assert!(!graph
        .search("fragile restored from its file", 50)
        .unwrap()
        .is_empty());
    assert!(graph
        .search("fragile carried sentinel", 50)
        .unwrap()
        .is_empty());
    r10_finish(root, graph, owner);
}

/// R11-01's other half: an in-place repair runs through the same batched
/// loop, so a close stops it between batches instead of waiting for every
/// page, and the next open finishes it from what it wrote.
#[test]
fn gh543_a_close_stops_an_in_place_repair_between_batches() {
    let root = r10_scratch("repair-stops");
    r10_pages(&root, 280);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    // 70 of 280 pages: inside the repair bound, and more than two batches.
    for index in 0..70 {
        fs::write(
            root.join("pages").join(format!("p{index}.md")),
            format!("- repaired r{index}\n"),
        )
        .unwrap();
    }
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database.clone()).unwrap();
    crate::direct_projection::count_page_lowerings_test(&root);
    let closer = Arc::clone(&graph);
    let (closed, reached) = std::sync::mpsc::channel();
    graph
        .direct_projection_test()
        .unwrap()
        .after_next_lowering_batch_test(Box::new(move || {
            closer.direct_projection_test().unwrap().close_test();
            let _ = closed.send(());
        }));
    let owner = R10Owner::start(&graph);
    assert!(
        reached.recv_timeout(Duration::from_secs(20)).is_ok(),
        "the repair never reached a batch boundary a close could stop at \
         (lowered {} pages)",
        crate::direct_projection::page_lowerings_test()
    );
    assert!(graph
        .direct_projection_test()
        .unwrap()
        .close_and_wait_for_worker(Duration::from_secs(20)));
    let lowered = crate::direct_projection::page_lowerings_test();
    assert!(
        (1..70).contains(&lowered),
        "a close during the repair lowered {lowered} of its 70 pages"
    );
    assert_eq!(
        graph.direct_projection_test().unwrap().fresh_builds_test(),
        0,
        "the fixture's change was not a repair"
    );
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    drop(graph);

    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(20)));
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    for index in [0, 69] {
        assert!(
            !graph
                .search(&format!("repaired r{index}"), 50)
                .unwrap()
                .is_empty(),
            "the resumed repair lost page p{index}"
        );
    }
    r10_finish(root, graph, owner);
}

/// R11-01 (decision DK4), the premise of the carry: a page a fresh build
/// carries from the old image, rebuilt from the text and tree the image
/// stored, lowers to exactly the rows a parse of its file gives. If it did
/// not, an unreadable page would come back from a rebuild with different
/// search text, references or properties than it had. The corpus spans the
/// shapes the rows must carry: a preamble, properties and ids, nesting by tab
/// and by space, scheduling, a code fence holding bullets, headings, CRLF
/// endings, Org, a journal and a namespaced page.
#[test]
fn gh543_a_carried_page_lowers_as_its_file_parses() {
    let root = r10_scratch("carried-round-trip");
    let files: [(&str, &str); 9] = [
        (
            "pages/Alpha Book.md",
            "type:: book\ntags:: reading, fiction\n\n- TODO alpha parent #reading [[Beta]]\n  id:: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n  SCHEDULED: <2026-09-20 Sun>\n\t- alpha child\n\t  prop:: value\n\t\t- alpha grandchild [[Gamma]]\n\t- alpha sibling\n- DONE done task\n",
        ),
        (
            "pages/Beta.md",
            "alias:: bee\n\n- beta refers to [[Alpha Book]] ((aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee))\n- another #fiction line\n",
        ),
        (
            "pages/Code.md",
            "- fenced\n  ```\n  - not a block\n    - nor this\n  ```\n- # A heading block\n-\n- after an empty block\n",
        ),
        ("pages/Crlf.md", "- first line\r\n  - nested line\r\n- second\r\n"),
        ("pages/Alpha%2FChild.md", "- namespaced [[Alpha Book]]\n"),
        ("pages/Preamble only.md", "title:: Preamble only\nicon:: x\n"),
        ("pages/Empty.md", ""),
        (
            "pages/Orgish.org",
            "#+title: Orgish\n* TODO org heading [[Beta]]\n  :PROPERTIES:\n  :id: 11111111-2222-3333-4444-555555555555\n  :END:\n** org child\n* second heading\n",
        ),
        ("journals/2026_09_18.md", "- journal entry [[Beta]]\n- LATER journal task\n"),
    ];
    for (path, text) in files {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);

    let graph = Graph::open(&root);
    let config = Arc::new(crate::config::ParseConfig::default());
    let carried: std::collections::BTreeMap<_, _> =
        crate::direct_projection::carried_physical_pages_test(&database, &config)
            .expect("the image's pages are rebuilt from their rows")
            .into_iter()
            .collect();
    let entries = graph.list_pages();
    let mut compared = 0;
    for entry in entries {
        let text = fs::read_to_string(root.join(&entry.rel_path)).unwrap();
        let (document, _) = super::page_parse::parse_page_content(&entry, &text);
        let parsed = crate::direct_projection::physical_page_for_test(&entry, &document, &config)
            .expect("the parse lowers");
        let stored = carried
            .get(&entry.rel_path)
            .unwrap_or_else(|| panic!("{} was not stored", entry.rel_path));
        assert_eq!(
            stored, &parsed,
            "{} lowers differently from its stored rows than from its file",
            entry.rel_path
        );
        compared += 1;
    }
    assert_eq!(compared, files.len(), "every fixture page is compared");
    assert_eq!(carried.len(), files.len());
    fs::remove_dir_all(root).unwrap();
}
