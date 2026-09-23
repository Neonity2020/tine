//! GH #543: a cold parse survives unrelated page opens, and a listing with
//! one unreadable page does not reparse the healthy ones.

use super::*;

/// GH #543 (audit R4-03): one page failing to parse in a warm SQL session
/// (no parsed cache) does not send the page listing into a whole-graph
/// parse. The watcher's failure moved the generation without telling the
/// index, which then never became ready again, so every indexed read fell
/// back to parsing.
#[test]
fn a_failed_page_in_a_warm_sql_session_does_not_parse_the_graph() {
    let dir = scratch("audit543-r4-warm-failure-list");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let database = dir.join("private/projection.sqlite");
    {
        let graph = Graph::open(&dir);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        graph
            .wait_for_direct_projection_for_test(Duration::from_secs(5))
            .unwrap();
        crate::direct_projection::release_projection(&graph);
    }
    let graph = Graph::open(&dir);
    graph.attach_direct_projection(database).unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    assert!(!graph.has_parsed_cache_test());
    let failed = dir.join("pages/p0.md");
    fs::write(&failed, format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n")).unwrap();
    assert!(graph.sync_file_checked(&failed).unwrap().is_none());
    GRAPH_TEXT_PARSE_ATTEMPTS.with(|count| count.set(0));
    let listed = graph.list_pages().len();
    let parses = GRAPH_TEXT_PARSE_ATTEMPTS.with(Cell::get);
    let ready = graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .is_ok();
    crate::direct_projection::release_projection(&graph);
    eprintln!("R4 warm failure listing: listed={listed} parses={parses} ready={ready}");
    assert!(ready, "a watcher failure left the index not ready");
    assert_eq!(listed, 7);
    assert!(
        parses <= 1,
        "a warm SQL-only session reparsed healthy siblings"
    );
}

/// GH #543 (audit R4-03): repeated listings with one failed page parse
/// that page at most once between changes.
#[test]
fn repeated_listings_with_a_failed_page_parse_it_at_most_once() {
    let dir = scratch("audit543-r4-repeat-failure");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let graph = Graph::open(&dir);
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    let failed = dir.join("pages/p0.md");
    fs::write(&failed, format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n")).unwrap();
    assert!(graph.sync_file_checked(&failed).unwrap().is_none());
    GRAPH_TEXT_PARSE_ATTEMPTS.with(|count| count.set(0));
    for _ in 0..4 {
        assert_eq!(graph.list_pages().len(), 7);
    }
    let parses = GRAPH_TEXT_PARSE_ATTEMPTS.with(Cell::get);
    graph.detach_direct_projection(Duration::from_secs(5));
    eprintln!("R4 repeated failure: four unchanged listings parsed={parses}");
    assert!(parses <= 1, "each request reparsed the same failed bytes");
}

/// GH #543 (audit R3-07): a watcher failure recorded while a complete
/// capture was running is newer than that capture; installing the capture
/// must not erase it and declare the index ready.
#[test]
fn a_capture_that_started_earlier_cannot_erase_a_later_watcher_failure() {
    let dir = scratch("audit543-r3-fast-failure");
    fs::write(dir.join("pages/Good.md"), "- good\n").unwrap();
    let failed = dir.join("pages/Failed.md");
    fs::write(&failed, "- old readable content\n").unwrap();
    let graph = Graph::open(&dir);
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    let permit = graph.admit_retained_graph_text_writer().unwrap();
    let flight = PageBuildFlight::new(
        graph.cache_generation(),
        graph.cache_structural_gen.begin_pass(),
    );
    let built = graph.load_all_pages_with_permit(&permit);
    fs::write(&failed, [0xff, 0xfe, 0xfd]).unwrap();
    assert!(graph.sync_file_checked(&failed).unwrap().is_none());
    let before = graph.page_index_failures();
    let installed = graph.install_reconciled(&flight, &permit, built);
    let after = graph.page_index_failures();
    let ready = graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .is_ok();
    graph.detach_direct_projection(Duration::from_secs(5));
    eprintln!("R3 watcher failure: before={before:?} after={after:?} installed={installed:?} ready={ready}");
    assert_eq!(
        after, before,
        "the old complete capture erased a newer known unreadable path"
    );
}

/// GH #543 (audit R3-06): the favorites page excludes references from
/// answers, so changing it must key a fresh answer, not the memo computed
/// under the old value.
#[test]
fn a_favorites_page_change_is_not_answered_from_the_old_memo() {
    let dir = scratch("audit543-r3-favorites-memo");
    fs::write(dir.join("pages/Target.md"), "- target\n").unwrap();
    fs::write(dir.join("pages/Favorites.md"), "- [[Target]]\n").unwrap();
    fs::write(dir.join("pages/Other.md"), "- [[Target]]\n").unwrap();
    let graph = Graph::open(&dir);
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    let before = graph
        .backlinks_bounded_indexed("Target", 100, 1_000_000)
        .unwrap();
    graph.set_favorites_page("Favorites").unwrap();
    let after = graph
        .backlinks_bounded_indexed("Target", 100, 1_000_000)
        .unwrap();
    let fresh = crate::query::backlinks_bounded_indexed(&graph, "Target", 100, 1_000_000).unwrap();
    graph.detach_direct_projection(Duration::from_secs(5));
    let names = |groups: &[RefGroup]| groups.iter().map(|g| g.page.clone()).collect::<Vec<_>>();
    eprintln!(
        "R3 favorites: before={:?} after={:?} fresh={:?}",
        names(&before.groups),
        names(&after.groups),
        names(&fresh.groups)
    );
    assert_eq!(
        names(&after.groups),
        names(&fresh.groups),
        "settings-only take-in left a cached answer under old reference exclusions"
    );
}

/// GH #543 (audit R3-04): revalidating one readable page that failed to
/// parse re-reads that page only, not every page in the graph.
#[test]
fn a_page_that_fails_to_parse_does_not_reparse_its_healthy_siblings() {
    let dir = scratch("audit543-r3-readable-parse-failure");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let graph = Graph::open(&dir);
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    let failed = dir.join("pages/p0.md");
    fs::write(&failed, format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n")).unwrap();
    assert!(graph.sync_file_checked(&failed).unwrap().is_none());
    GRAPH_TEXT_PARSE_ATTEMPTS.with(|count| count.set(0));
    let listed = graph.list_pages().len();
    let parses = GRAPH_TEXT_PARSE_ATTEMPTS.with(Cell::get);
    graph.detach_direct_projection(Duration::from_secs(5));
    eprintln!("R3 readable parse failure: listed={listed} parses={parses}");
    assert_eq!(listed, 7);
    assert!(
        parses <= 1,
        "a readable failed page reparsed all healthy siblings"
    );
}

/// GH #543 (audit R3-01): a page deleted and recreated while the cold pass
/// ran is re-read on its own; the rest of the pass is kept.
#[test]
fn a_page_deleted_and_recreated_during_a_pass_keeps_the_pass() {
    let dir = scratch("audit543-r3-delete-recreate");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let graph = Arc::new(Graph::open(&dir));
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    let pause = Arc::new(PageBuildTestPause::new());
    *graph.page_build_test.cold_read_done.lock().unwrap() = Some(Arc::clone(&pause));
    let warmer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.warm_cache_cancellable(|| false))
    };
    pause.reached.wait();
    let path = dir.join("pages/p0.md");
    fs::remove_file(&path).unwrap();
    graph.sync_deleted_file(&path).unwrap();
    fs::write(&path, "- recreated\n").unwrap();
    graph.sync_file(&path);
    pause.release.wait();
    let completed = warmer.join().unwrap();
    let first = graph.page_build_parses_test();
    let listed = graph.list_pages().len();
    let total = graph.page_build_parses_test();
    graph.detach_direct_projection(Duration::from_secs(5));
    eprintln!(
        "R3 delete/recreate: completed={completed} first={first} total={total} listed={listed}"
    );
    assert!(
        completed,
        "a named delete/recreate discarded the completed cold pass"
    );
    assert_eq!(first, total, "the next listing reparsed the whole graph");
}

/// GH #543 (audit R3-02): a page opened before the cold pass and edited
/// externally afterwards is not stale forever: the pass's own read is newer
/// than the session publication.
#[test]
fn a_page_edited_after_an_earlier_open_keeps_the_pass() {
    let dir = scratch("audit543-r3-open-external-edit");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let graph = Arc::new(Graph::open(&dir));
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    let pause = Arc::new(PageBuildTestPause::new());
    *graph.page_build_test.cold_read_done.lock().unwrap() = Some(Arc::clone(&pause));
    let warmer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.warm_cache_cancellable(|| false))
    };
    pause.reached.wait();
    let path = dir.join("pages/p0.md");
    let entry = graph.entry_for_path(&path).unwrap();
    graph.load_page(&entry).unwrap();
    // The watcher has not delivered this edit yet. The warm's own baseline
    // check sees it, and must retain that exact newer parse.
    fs::write(&path, "- external edit after page open\n").unwrap();
    pause.release.wait();
    let completed = warmer.join().unwrap();
    let first = graph.page_build_parses_test();
    let listed = graph.list_pages().len();
    let total = graph.page_build_parses_test();
    graph.detach_direct_projection(Duration::from_secs(5));
    eprintln!(
        "R3 open/external edit: completed={completed} first={first} total={total} listed={listed}"
    );
    assert!(
        completed,
        "an obsolete session revision rejected newer disk bytes"
    );
    assert_eq!(first, total, "the next listing reparsed the whole graph");
}

/// GH #543 (audit R3-03): more named changes than any bounded log would
/// retain still stay named, so the pass is kept rather than treated as
/// unexplained drift.
#[test]
fn many_named_deletes_during_a_pass_keep_the_pass() {
    let dir = scratch("audit543-r3-many-deletes");
    for index in 0..1030 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let graph = Arc::new(Graph::open(&dir));
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    let pause = Arc::new(PageBuildTestPause::new());
    *graph.page_build_test.cold_read_done.lock().unwrap() = Some(Arc::clone(&pause));
    let warmer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.warm_cache_cancellable(|| false))
    };
    pause.reached.wait();
    for index in 0..1025 {
        let path = dir.join(format!("pages/p{index}.md"));
        fs::remove_file(&path).unwrap();
        graph.sync_deleted_file(&path).unwrap();
    }
    pause.release.wait();
    let completed = warmer.join().unwrap();
    let first = graph.page_build_parses_test();
    let listed = graph.list_pages().len();
    let total = graph.page_build_parses_test();
    graph.detach_direct_projection(Duration::from_secs(5));
    eprintln!("R3 many deletes: completed={completed} first={first} total={total} listed={listed}");
    assert!(
        completed,
        "retention dropped named changes needed by the active pass"
    );
    assert_eq!(first, total, "the next listing reparsed every survivor");
}

/// An edit saved after the cold parse read every page must cost one page
/// parse, not the whole graph again (GH #543, indexing audit R2-02).
#[test]
fn gh543_an_edit_during_the_cold_parse_costs_one_page() {
    let dir = scratch("gh543-cold-parse-one-edit");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- TODO original\n").unwrap();
    }
    let graph = Arc::new(Graph::open(&dir));
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    let pause = Arc::new(PageBuildTestPause::new());
    *graph.page_build_test.cold_read_done.lock().unwrap() = Some(Arc::clone(&pause));
    let warmer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.warm_cache_cancellable(|| false))
    };
    pause.reached.wait();
    let entry = graph.entry_for_path(&dir.join("pages/p0.md")).unwrap();
    let mut page = graph.load_page(&entry).unwrap();
    page.blocks[0].raw = "TODO edited during cold parse".into();
    graph.save_page(&page, page.rev.as_deref()).unwrap();
    pause.release.wait();
    let completed = warmer.join().unwrap();
    let first = graph.page_build_parses_test();
    let listed = graph.list_pages().len();
    let total = graph.page_build_parses_test();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    graph.detach_direct_projection(Duration::from_secs(5));
    let _ = fs::remove_dir_all(&dir);
    assert!(completed, "the cold parse was discarded for one edit");
    assert_eq!(listed, 8);
    assert!(first <= 9, "the edit cost {first} parses of 8 pages");
    assert_eq!(total, first, "a listing parsed the graph again");
}

/// Opening an unchanged page during the cold parse publishes it and moves the
/// cache generation. The parse must still install: discarding it made the
/// next reader parse the whole graph again (GH #543).
#[test]
fn gh543_cold_parse_survives_an_unchanged_page_open() {
    let dir = scratch("gh543-cold-parse-unchanged");
    for index in 0..3 {
        fs::write(
            dir.join("pages").join(format!("Existing{index}.md")),
            "- unchanged\n",
        )
        .unwrap();
    }
    let graph = Arc::new(Graph::open(&dir));
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    let pause = Arc::new(PageBuildTestPause::new());
    *graph.page_build_test.owner_pause.lock().unwrap() = Some(Arc::clone(&pause));
    let warmer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.warm_cache_cancellable(|| false))
    };
    pause.reached.wait();
    let entry = graph
        .entry_for_path(&dir.join("pages/Existing0.md"))
        .unwrap();
    graph.load_page(&entry).unwrap();
    pause.release.wait();
    let completed = warmer.join().unwrap();
    *graph.page_build_test.owner_pause.lock().unwrap() = None;
    let first_parses = graph.page_build_parses_test();
    graph.with_pages(|_| ());
    let total_parses = graph.page_build_parses_test();
    graph
        .wait_for_direct_projection_for_test(std::time::Duration::from_secs(5))
        .unwrap();
    graph.detach_direct_projection(std::time::Duration::from_secs(5));
    let _ = fs::remove_dir_all(&dir);
    assert!(completed, "the cold pass was discarded");
    assert_eq!(total_parses, first_parses, "a second whole-graph parse ran");
}

/// An edit that lands after the cold parse read the page must not be
/// installed over: the parse names the page, and only that page is parsed
/// again before the parse installs (GH #543, R2-02).
#[test]
fn gh543_cold_parse_reparses_a_page_edited_after_it_read_it() {
    let dir = scratch("gh543-cold-parse-edited");
    fs::write(dir.join("pages/Existing.md"), "- unchanged\n").unwrap();
    fs::write(dir.join("pages/Other.md"), "- other\n").unwrap();
    let graph = Graph::open(&dir);
    let permit = graph.admit_retained_graph_text_writer().unwrap();
    let flight = PageBuildFlight::new(
        graph.cache_generation(),
        graph.cache_structural_gen.begin_pass(),
    );
    let built = graph.load_all_pages_with_permit(&permit);
    let path = dir.join("pages/Existing.md");
    let entry = graph.entry_for_path(&path).unwrap();
    let mut page = graph.load_page(&entry).unwrap();
    let base = page.rev.clone().unwrap();
    page.blocks[0].raw = "changed".into();
    graph.save_page(&page, Some(&base)).unwrap();

    let Err((built, stale)) = graph.install_built(&flight, built) else {
        panic!("the parse installed over the edit");
    };
    assert_eq!(stale, std::collections::HashSet::from([path.clone()]));
    let parses = graph.page_build_parses_test();
    assert_eq!(
        graph.install_reconciled(&flight, &permit, built),
        PageCacheInstallOutcome::Installed
    );
    assert_eq!(
        graph.page_build_parses_test(),
        parses + 1,
        "only the edited page is parsed again"
    );
    let cached = graph.with_captured_pages(|pages| {
        pages
            .iter()
            .find(|(entry, _)| entry.path == path)
            .map(|(_, document)| document.roots[0].raw.clone())
    });
    drop(permit);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(cached.flatten().as_deref(), Some("changed"));
}

/// GH #543 (indexing audit IT-07): with a page that cannot be read and no
/// ready index, listing pages revalidated the failed path by reading and
/// parsing EVERY page, and did so again after every save moved the
/// generation. One unreadable file (a sync mid-delivery, a bad encoding) made
/// each listing a whole-graph parse at 10k pages.
#[test]
fn gh543_listing_with_an_unreadable_page_does_not_reparse_healthy_pages() {
    let dir = scratch("gh543-unreadable-listing");
    for index in 0..6 {
        fs::write(
            dir.join("pages").join(format!("Healthy{index}.md")),
            format!("- healthy {index}\n"),
        )
        .unwrap();
    }
    let database = dir.join("private/projection.sqlite");
    {
        let first = Graph::open(&dir);
        first.attach_direct_projection(database.clone()).unwrap();
        first.warm_cache();
        assert!(first
            .wait_for_direct_projection_for_test(Duration::from_secs(30))
            .is_ok());
        crate::direct_projection::release_projection(&first);
    }
    // Between sessions: one page changes, one becomes unreadable. The reopen
    // keeps the older image and withholds readiness until the unreadable
    // page can join a complete inventory.
    fs::write(
        dir.join("pages/Healthy0.md"),
        "- changed between sessions\n",
    )
    .unwrap();
    fs::write(dir.join("pages/Healthy1.md"), [0xff, 0xfe, 0xfd]).unwrap();
    let graph = Graph::open(&dir);
    graph.attach_direct_projection(database).unwrap();
    graph.warm_cache();
    assert!(graph.direct_projection_test().unwrap().wait_drained_test());

    let first_listing = graph.list_pages();
    assert!(first_listing.iter().all(|entry| entry.name != "healthy1"));
    let entry = graph
        .entry_for_path(&dir.join("pages/Healthy2.md"))
        .unwrap();
    let mut page = graph.load_page(&entry).unwrap();
    page.blocks[0].raw = "edited while a sibling is unreadable".into();
    graph.save_page(&page, page.rev.as_deref()).unwrap();

    // The watcher delivers the unreadable file again, rewritten and still bad:
    // it drops the listing memo so the next listing revalidates that path.
    fs::write(dir.join("pages/Healthy1.md"), [0xff, 0xfe]).unwrap();
    graph.sync_file(&dir.join("pages/Healthy1.md"));
    assert!(!graph.direct_projection_ready_test());
    assert_eq!(
        graph.page_index_failures(),
        vec!["pages/Healthy1.md".to_owned()]
    );
    GRAPH_TEXT_PARSE_ATTEMPTS.with(|count| count.set(0));
    let listed = graph.list_pages();
    let parses = GRAPH_TEXT_PARSE_ATTEMPTS.with(Cell::get);
    let oracle = Graph::open(&dir).list_pages();
    graph.detach_direct_projection(Duration::from_secs(5));
    let names = |entries: &[PageEntry]| {
        let mut names = entries
            .iter()
            .map(|entry| (entry.name.clone(), entry.rel_path.clone()))
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    assert_eq!(
        names(&listed),
        names(&oracle),
        "the exact listing, from the cache"
    );
    assert!(listed.iter().all(|entry| entry.name != "healthy1"));
    assert_eq!(
        parses, 0,
        "one still-unreadable page made the listing reparse {parses} healthy pages"
    );
}

fn model_sources_matching(needle: &str) -> Vec<String> {
    let model = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/model");
    let mut sites = Vec::new();
    for entry in fs::read_dir(&model).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let source: String = fs::read_to_string(&path)
                .unwrap()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            for _ in source.matches(needle) {
                sites.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    sites.sort();
    sites
}

/// GH #543 (I-13, audit R4-03): `cache_gen` moves in one place,
/// `Graph::move_cache_generation` (model/graph_drift.rs), which makes every
/// mover say how the index hears about the move. Two watcher paths moved it
/// on their own and told the index nothing, and it stayed not ready.
#[test]
fn the_cache_generation_moves_through_one_front_door() {
    assert_eq!(
        model_sources_matching("cache_gen.fetch_add("),
        vec!["graph_drift.rs".to_owned()],
        "move cache_gen only through Graph::move_cache_generation (model/graph_drift.rs)"
    );
}

/// GH #543 (audit R4-02): a session id record claims the index was sent
/// those bytes. It is written by a page publication
/// (`Graph::publish_session_page_ids`, model/graph_drift.rs), restored from
/// the index itself (model/derived_reads.rs), and removed with a page
/// (model/projection_lifetime.rs), nowhere else.
#[test]
fn session_page_ids_have_three_writers() {
    assert_eq!(
        model_sources_matching("session_page_ids.write()"),
        vec![
            "derived_reads.rs".to_owned(),
            "graph_drift.rs".to_owned(),
            "projection_lifetime.rs".to_owned(),
            "projection_lifetime.rs".to_owned(),
        ],
        "publish page ids only through Graph::publish_session_page_ids (model/graph_drift.rs)"
    );
}

/// GH #543 (audit R4-P1): the per-path event log keeps what a running pass
/// may still need and drops the rest. It used to keep every path removed or
/// found unreadable for the life of the session.
#[test]
fn the_structural_event_log_keeps_only_what_running_passes_need() {
    let dir = scratch("r4-p1-event-log");
    let graph = Graph::open(&dir);
    for index in 0..300 {
        let path = dir.join(format!("pages/bad{index}.md"));
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(graph.sync_file_checked(&path).unwrap().is_none());
    }
    let idle = graph.cache_structural_gen.logged_paths_test();
    let pass = graph.cache_structural_gen.begin_pass();
    for index in 300..600 {
        let path = dir.join(format!("pages/bad{index}.md"));
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(graph.sync_file_checked(&path).unwrap().is_none());
    }
    let running = graph.cache_structural_gen.logged_paths_test();
    drop(pass);
    eprintln!("R4-P1 event log: idle={idle} during a pass={running}");
    assert!(idle < 256, "with no pass running the log kept {idle} paths");
    assert!(
        running >= 300,
        "a running pass lost events after its start: {running}"
    );
}

/// GH #543 (audit R4-P2): a whole-graph parse a page consumer starts, with no
/// index to report for it, shows on the progress bar while it runs and
/// clears after. Only the paced warm parse used to report itself, so the bar
/// said idle through a consumer's parse.
#[test]
fn a_consumer_whole_graph_parse_shows_on_the_progress_bar() {
    let dir = scratch("r4-p2-fast-parse-progress");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- page\n").unwrap();
    }
    let graph = Arc::new(Graph::open(&dir));
    let pause = graph.pause_next_fast_parse_test();
    let consumer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.orphan_assets().unwrap())
    };
    pause.reached.wait();
    let during = graph.indexing_progress();
    pause.release.wait();
    consumer.join().unwrap();
    let after = graph.indexing_progress();
    eprintln!("R4-P2 fast parse progress: during={during:?} after={after:?}");
    assert!(
        during.is_some_and(|progress| progress.total >= 8),
        "the bar said idle during a whole-graph parse: {during:?}"
    );
    assert_eq!(after, None, "the bar stuck after the parse");
}

/// GH #543 (audit R6-05): a query-export plan asked for while the index is
/// being built answers with the typed not-ready error, which the dialog
/// retries, and it asks before it captures the graph, so a retry does not
/// parse every page. It used to parse the whole graph and then fail with
/// untyped prose the dialog showed as a permanent refusal.
#[test]
fn a_query_export_plan_during_indexing_is_typed_not_ready_and_parses_nothing() {
    let dir = scratch("audit543-r6-export-plan");
    for index in 0..6 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- TODO task\n").unwrap();
    }
    let database = dir.join("private/projection.sqlite");
    {
        let graph = Graph::open(&dir);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        graph
            .wait_for_direct_projection_for_test(Duration::from_secs(5))
            .unwrap();
        crate::direct_projection::release_projection(&graph);
    }
    let graph = Arc::new(Graph::open(&dir));
    graph.attach_direct_projection(database).unwrap();
    let pause = graph.pause_next_warm_after_read_test();
    let warmer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.warm_cache())
    };
    pause.reached.wait();
    let request = crate::publish::query_export::QueryPublicationRequest {
        query: "(task TODO)".into(),
        advanced: false,
        simple_dialect: None,
        current_page: None,
        view: None,
        host_block_id: None,
        host_properties: Vec::new(),
        name: "todo".into(),
        folder: None,
        replace: false,
        asset_budget_bytes: None,
        app_bundle: None,
    };
    let planner = {
        let graph = Arc::clone(&graph);
        let request = request.clone();
        std::thread::spawn(move || {
            GRAPH_TEXT_PARSE_ATTEMPTS.with(|count| count.set(0));
            let during = crate::publish::plan_query_publication(&graph, &request);
            (during, GRAPH_TEXT_PARSE_ATTEMPTS.with(Cell::get))
        })
    };
    let started = std::time::Instant::now();
    while !planner.is_finished() && started.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let answered_during_pass = planner.is_finished();
    pause.release.wait();
    warmer.join().unwrap();
    let (during, parses) = planner.join().unwrap();
    let after = crate::publish::plan_query_publication(&graph, &request);
    crate::direct_projection::release_projection(&graph);
    let typed = match &during {
        Err(crate::publish::query_export::QueryPublicationError::Io(error)) => error
            .get_ref()
            .and_then(|source| source.downcast_ref::<crate::query::QueryExecutionError>())
            .cloned(),
        _ => None,
    };
    eprintln!(
        "R6-05 plan during indexing: answered_during_pass={answered_during_pass} typed={typed:?} parses={parses} after_ok={}",
        after.is_ok()
    );
    assert!(
        answered_during_pass,
        "a plan during indexing waited for the whole pass"
    );
    assert!(
        matches!(typed, Some(crate::query::QueryExecutionError::NotReady(_))),
        "a plan during indexing was not typed not-ready: {during:?}"
    );
    assert_eq!(
        parses, 0,
        "a plan refused for indexing parsed the graph first"
    );
    assert!(
        after.is_ok(),
        "the plan failed after indexing finished: {after:?}"
    );
}

/// GH #543: a rename marked the index stale, and the mark outlived the
/// rename's own delta. Once the rename had converged, a page failing to
/// parse no longer carried the index to the new generation, so every
/// indexed read after it parsed the whole graph.
#[test]
fn a_failed_page_after_a_rename_does_not_parse_the_graph() {
    let dir = scratch("gh543-stale-after-rename");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let database = dir.join("private/projection.sqlite");
    {
        let graph = Graph::open(&dir);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        graph
            .wait_for_direct_projection_for_test(Duration::from_secs(5))
            .unwrap();
        crate::direct_projection::release_projection(&graph);
    }
    let graph = Graph::open(&dir);
    graph.attach_direct_projection(database).unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    graph.rename_page("p7", "p9").unwrap();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    assert!(!graph.has_parsed_cache_test());
    let failed = dir.join("pages/p0.md");
    fs::write(&failed, format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n")).unwrap();
    assert!(graph.sync_file_checked(&failed).unwrap().is_none());
    GRAPH_TEXT_PARSE_ATTEMPTS.with(|count| count.set(0));
    let listed = graph.list_pages().len();
    let parses = GRAPH_TEXT_PARSE_ATTEMPTS.with(Cell::get);
    let ready = graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .is_ok();
    crate::direct_projection::release_projection(&graph);
    assert!(ready, "a failure after a rename left the index not ready");
    assert_eq!(listed, 7);
    assert!(
        parses <= 1,
        "a listing after a rename parsed {parses} pages"
    );
}

/// GH #543: a rename moved the generation and queued its delta only after
/// it had re-read the renamed pages. A listing in between found the index
/// behind with nothing queued, and parsed the whole graph.
#[test]
fn a_listing_during_a_rename_waits_for_the_rename_instead_of_parsing() {
    let dir = scratch("gh543-listing-during-rename");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let database = dir.join("private/projection.sqlite");
    {
        let graph = Graph::open(&dir);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        graph
            .wait_for_direct_projection_for_test(Duration::from_secs(5))
            .unwrap();
        crate::direct_projection::release_projection(&graph);
    }
    let graph = Arc::new(Graph::open(&dir));
    graph.attach_direct_projection(database).unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    assert!(!graph.has_parsed_cache_test());
    let parses_before = graph.page_build_parses_test();
    let pause = graph.pause_after_next_parsed_cache_discard_test();
    let renamer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.rename_page("p7", "p9").map(|_| ()))
    };
    pause.reached.wait();
    let lister = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.list_pages())
    };
    // Long enough for the listing to reach its decision while the rename is
    // held between its generation move and its delta.
    std::thread::sleep(Duration::from_millis(300));
    pause.release.wait();
    renamer.join().unwrap().unwrap();
    let listed = lister.join().unwrap();
    let parses = graph.page_build_parses_test() - parses_before;
    crate::direct_projection::release_projection(&graph);
    assert_eq!(parses, 0, "a listing during a rename parsed {parses} pages");
    let names = listed
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"p9") && !names.contains(&"p7"), "{names:?}");
}

/// GH #543: a listing took the graph's generation, and an edit moved it
/// while the listing waited. The index became ready at the new generation,
/// but readiness is exact, so the old generation's was not coming, and the
/// listing parsed the whole graph instead of reading the index.
#[test]
fn a_listing_overtaken_by_an_edit_reads_the_index_instead_of_parsing() {
    let dir = scratch("gh543-listing-overtaken");
    for index in 0..8 {
        fs::write(dir.join(format!("pages/p{index}.md")), "- original\n").unwrap();
    }
    let database = dir.join("private/projection.sqlite");
    {
        let graph = Graph::open(&dir);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        graph
            .wait_for_direct_projection_for_test(Duration::from_secs(5))
            .unwrap();
        crate::direct_projection::release_projection(&graph);
    }
    let graph = Arc::new(Graph::open(&dir));
    graph.attach_direct_projection(database).unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    assert!(!graph.has_parsed_cache_test());
    let parses_before = graph.page_build_parses_test();
    let pause = graph.pause_next_derived_read_test();
    let lister = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.list_pages())
    };
    pause.reached.wait();
    fs::write(dir.join("pages/p1.md"), "- saved\n").unwrap();
    graph.sync_file_checked(&dir.join("pages/p1.md")).unwrap();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(5))
        .unwrap();
    pause.release.wait();
    let listed = lister.join().unwrap();
    let parses = graph.page_build_parses_test() - parses_before;
    crate::direct_projection::release_projection(&graph);
    assert_eq!(listed.len(), 8);
    assert_eq!(parses, 0, "an overtaken listing parsed {parses} pages");
}

/// An edit may lower onto the image a session reopened, but that image does
/// not answer for the graph until a warm or full inventory validates it:
/// readiness waits for validation (`index_need` reports `Validate`). This is
/// why a pending edit on an unvalidated image is not by itself "coming"
/// (`projection_lifetime.rs`, derived-read wait; GH #543 audit R9-14).
#[test]
fn gh543_an_edit_on_a_reopened_image_does_not_make_it_ready_before_validation() {
    let dir = scratch("gh543-edit-before-validation");
    for index in 0..3 {
        fs::write(
            dir.join("pages").join(format!("Page{index}.md")),
            format!("- page {index}\n"),
        )
        .unwrap();
    }
    let database = dir.join("private/projection.sqlite");
    {
        let first = Graph::open(&dir);
        first.attach_direct_projection(database.clone()).unwrap();
        first.warm_cache();
        assert!(first
            .wait_for_direct_projection_for_test(Duration::from_secs(30))
            .is_ok());
        crate::direct_projection::release_projection(&first);
    }
    let graph = Graph::open(&dir);
    graph.attach_direct_projection(database).unwrap();
    let entry = graph.entry_for_path(&dir.join("pages/Page0.md")).unwrap();
    let mut page = graph.load_page(&entry).unwrap();
    page.blocks[0].raw = "edited before the warm".into();
    graph.save_page(&page, page.rev.as_deref()).unwrap();
    let projection = graph.direct_projection_test().unwrap();
    assert!(projection.wait_drained_test(), "the edit's turn ran");
    let (need, _) = projection.index_need_now();
    let ready_before = graph.direct_projection_ready_test();
    graph.warm_cache();
    let ready_after = graph
        .wait_for_direct_projection_for_test(Duration::from_secs(30))
        .is_ok();
    graph.detach_direct_projection(Duration::from_secs(5));
    assert_eq!(need, crate::direct_projection::IndexNeed::Validate);
    assert!(!ready_before, "an unvalidated image was published as ready");
    assert!(
        ready_after,
        "the warm validates the image and readiness follows"
    );
}
