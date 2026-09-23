//! GH #543, indexing audit round 11: each finding's class, pinned. The
//! fixtures follow the auditor's probes (evidence `indexing-audit-r11`).

use super::gh543_r10::{
    r10_finish, r10_pages, r10_prebuild, r10_ready_graph, r10_scratch, r10_settle, R10Owner,
};
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

/// R11-07 (class K1, "when does a failure owe the index a new image?"): one
/// worker turn failing -- a transient disk error, a busy database -- on an
/// intact image rolled back and damaged nothing. It owes a validation, which
/// re-lowers the page the failed turn carried, and not a whole fresh build:
/// on 10k pages that was ~17 s of re-indexing with search withdrawn, for a
/// blip.
#[test]
fn gh543_a_failed_turn_on_an_intact_image_is_validated_not_rebuilt() {
    let (root, graph, owner) = r10_ready_graph("k1-turn");
    let projection = graph.direct_projection_test().unwrap();
    let before = projection.fresh_builds_test();
    projection.inject_next_turn_failure_test();
    fs::write(root.join("pages/p1.md"), "- edited k1turn [[p2]]\n").unwrap();
    let _ = graph.sync_file_checked(&root.join("pages/p1.md"));
    let started = std::time::Instant::now();
    // The failed turn's marks are retried at once, so `last_turn_failed`
    // may already be cleared; the injection being taken is the proof.
    while projection.turn_failure_injection_pending_test()
        && started.elapsed() < Duration::from_secs(10)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !projection.turn_failure_injection_pending_test(),
        "precondition: the injected turn failure happened"
    );
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    assert!(
        !graph.search("k1turn", 50).unwrap().is_empty(),
        "the validation did not re-derive the page the failed turn carried"
    );
    assert_eq!(
        projection.fresh_builds_test(),
        before,
        "one failed turn on an intact image rebuilt the whole index"
    );
    r10_finish(root, graph, owner);
}

/// R11-06 (class K1): rows that contradict each other -- here blocks whose
/// page row is gone, damage only a Tine defect writes and `quick_check`
/// cannot see (foreign keys are not part of it) --
/// make a read answer `InvalidSnapshot`. That answer is the reader's own
/// evidence of damage and owes a new image; ignoring it in favour of the
/// structural check left task and reference queries failing for the rest of
/// the session. One rebuild per session: a contradiction on a freshly built
/// image is a lowering defect no rebuild can fix, and rebuilding for it would
/// loop.
#[test]
fn gh543_contradictory_rows_rebuild_the_index_once() {
    let (root, graph, owner) = r10_ready_graph("k1-contradiction");
    let projection = graph.direct_projection_test().unwrap();
    {
        let connection =
            rusqlite::Connection::open(root.join("private/projection.sqlite")).unwrap();
        connection.busy_timeout(Duration::from_secs(5)).unwrap();
        connection
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        assert!(
            connection
                .execute("UPDATE pages SET page_id = page_id + 1000000", [])
                .unwrap()
                > 0
        );
    }
    let before = projection.fresh_builds_test();
    let first = graph.run_query_bounded("(task TODO)", 20_000, 32 * 1024 * 1024);
    // `InvalidSnapshot` before the fix; the read now also asks for the
    // rebuild and answers that it is recovering.
    assert!(
        first.is_err(),
        "precondition: the damage is visible to the read: {first:?}"
    );
    let started = std::time::Instant::now();
    let answer = loop {
        let answer = graph.run_query_bounded("(task TODO)", 20_000, 32 * 1024 * 1024);
        if answer.is_ok() || started.elapsed() > Duration::from_secs(20) {
            break answer;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        answer.map(|answer| answer.groups.len()).ok(),
        Some(12),
        "the contradictory image was never replaced"
    );
    assert_eq!(projection.fresh_builds_test(), before + 1);

    // The same contradiction again, now on the rebuilt image: no second
    // rebuild.
    r10_settle(&graph);
    {
        let connection =
            rusqlite::Connection::open(root.join("private/projection.sqlite")).unwrap();
        connection.busy_timeout(Duration::from_secs(5)).unwrap();
        connection
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        connection
            .execute("UPDATE pages SET page_id = page_id + 1000000", [])
            .unwrap();
    }
    for _ in 0..3 {
        let _ = graph.run_query_bounded("(task TODO)", 20_000, 32 * 1024 * 1024);
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = projection.wait_drained_test();
    assert_eq!(
        projection.fresh_builds_test(),
        before + 1,
        "a contradiction on a freshly built image rebuilt it again"
    );
    r10_finish(root, graph, owner);
}

/// K1 and R11-08, pinned in the source: whether a failure owes the index a new
/// image has one answer, `failure_owes_new_image`, which both the worker and a
/// failed read's repair ask; and the index owes a validation for one
/// production reason, a turn that failed on an intact image. A second
/// producer of "rebuild after a failure" is how a failed turn came to rebuild
/// unconditionally beside the decider the reads used (audit R11-07); a
/// "stale" producer only fixtures reached kept a public entry point and a
/// false doc for months (R11-08). Exemplar: `direct_projection_owner.rs`.
#[test]
fn a_failure_owes_a_new_image_by_one_decider() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let read = |file: &str| fs::read_to_string(src.join(file)).unwrap();
    let production = |text: &str| -> String {
        // Drop `#[cfg(test)]` items: they set state fixtures need.
        let mut kept = String::new();
        let mut skip_depth: Option<i32> = None;
        let mut pending_cfg_test = false;
        let mut depth = 0i32;
        for line in text.lines() {
            let opens = line.matches('{').count() as i32;
            let closes = line.matches('}').count() as i32;
            if skip_depth.is_none() && line.trim() == "#[cfg(test)]" {
                pending_cfg_test = true;
                continue;
            }
            if pending_cfg_test && skip_depth.is_none() {
                pending_cfg_test = false;
                if opens > closes {
                    skip_depth = Some(depth);
                }
                depth += opens - closes;
                continue;
            }
            depth += opens - closes;
            if let Some(at) = skip_depth {
                if depth <= at {
                    skip_depth = None;
                }
                continue;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        kept
    };
    let worker = production(&read("direct_projection.rs"));
    let repair = production(&read("model/direct_query.rs"));
    let owner = read("direct_projection_owner.rs");
    assert!(
        owner.contains("pub(super) fn failure_owes_new_image("),
        "the K1 decider moved; update this guard"
    );
    assert_eq!(
        worker.matches("worker_failed.store(true").count(),
        1,
        "a second place marks the worker failed (K1, I-24)"
    );
    assert!(
        worker.contains("owner::IndexFailure::of_turn(message)")
            && worker.contains("owner::failure_owes_new_image(")
            && worker.contains("if owes_new_image {"),
        "a failed turn must ask the K1 decider before it latches a fresh build"
    );
    assert_eq!(
        worker.matches("validated.store(false").count(),
        0,
        "the index owes a validation again after it has one; a failed turn \
         returns its marks instead (R11-08, reconciler design §5)"
    );
    assert!(
        repair
            .contains("failure.is_some_and(|failure| projection.failure_owes_new_image(failure))"),
        "a failed read's repair must ask the K1 decider"
    );
    for (file, text) in [
        ("direct_projection.rs", &worker),
        ("model/direct_query.rs", &repair),
    ] {
        for gone in [
            "failed_read_found_damage",
            "fn mark_stale",
            "projection.worker_failed() ||",
        ] {
            assert!(
                !text.contains(gone),
                "{file}: `{gone}` came back (K1/R11-08)"
            );
        }
    }
    let page_cache = production(&read("model/page_cache.rs"));
    assert!(
        !page_cache.contains("fn invalidate_cache"),
        "a production whole-graph invalidation came back; every change reaches \
         the graph by path (R11-08)"
    );
}
