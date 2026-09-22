//! GH #543: a cold parse survives unrelated page opens, and a listing with
//! one unreadable page does not reparse the healthy ones.

use super::*;

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
    let flight = PageBuildFlight::new(graph.cache_generation(), graph.cache_structural_gen.load());
    let built = graph.load_all_pages_with_permit(&permit);
    fs::write(&failed, [0xff, 0xfe, 0xfd]).unwrap();
    assert!(graph.sync_file_checked(&failed).is_err());
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
    let flight = PageBuildFlight::new(graph.cache_generation(), graph.cache_structural_gen.load());
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
    graph.direct_projection_test().unwrap().wait_drained_test();

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
