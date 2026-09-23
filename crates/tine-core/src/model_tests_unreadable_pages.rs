//! A page Tine cannot read or parse has an unknown effective name. These
//! pin what that failure may refuse (only a name the file could be), how the
//! watcher records it (reconciled state, not a cycle to retry), and how the
//! identity evidence carrying it is installed, replaced and repaired.

use super::*;

#[test]
fn failed_and_stale_effective_identity_evidence_blocks_name_only_creation() {
    // A failed identity can hide a title its text contains, so it blocks
    // name-only creation of that name only. An unrelated exact owner remains
    // writable through its retained bytes/revision/file identity.
    let dir = scratch("failed-effective-identity");
    fs::write(dir.join("pages/Good.md"), "- good\n").unwrap();
    fs::write(
        dir.join("pages/Invalid.md"),
        b"title:: Could Be Hidden\n\xff\xfe\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();
    let mut good = graph.load_by_path("pages/Good.md").unwrap().unwrap();
    good.blocks[0].raw = "good exact save".into();
    graph.save_page(&good, good.rev.as_deref()).unwrap();
    assert_eq!(
        fs::read_to_string(dir.join("pages/Good.md")).unwrap(),
        "- good exact save\n"
    );
    let fresh = markdown_page_dto("Could Be Hidden", "Could Be Hidden", "- no\n").unwrap();
    assert_eq!(
        graph.save_page(&fresh, None).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(!dir.join("pages/Could Be Hidden.md").exists());
    let _ = fs::remove_dir_all(&dir);

    // A graph file arriving after the indexed generation has unknown
    // effective identity until reconciliation advances/rebuilds the cache.
    let dir = scratch("stale-effective-identity");
    fs::write(dir.join("pages/Indexed.md"), "- indexed\n").unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();
    fs::create_dir_all(dir.join("external")).unwrap();
    fs::write(
        dir.join("external/Late.md"),
        "title:: Could Be Hidden\n\n- late\n",
    )
    .unwrap();
    let late = dir.join("external/Late.md");
    graph.note_graph_text_external_observation();
    let observed = graph.graph_text_external_observation_ticket();
    graph.sync_file_checked(&late).unwrap();
    graph.acknowledge_graph_text_external_observations(observed);
    GRAPH_TEXT_CONTENT_READS.with(|reads| reads.set(0));
    GRAPH_TEXT_PARSE_ATTEMPTS.with(|attempts| attempts.set(0));
    let fresh = markdown_page_dto("Could Be Hidden", "Could Be Hidden", "- no\n").unwrap();
    assert_eq!(
        graph.save_page(&fresh, None).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(!dir.join("pages/Could Be Hidden.md").exists());
    assert_eq!(GRAPH_TEXT_CONTENT_READS.with(Cell::get), 0);
    assert_eq!(GRAPH_TEXT_PARSE_ATTEMPTS.with(Cell::get), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cold_malformed_owner_refuses_without_census_or_mutation() {
    let dir = scratch("cold-malformed-owner");
    let malformed = dir.join("pages/Malformed.md");
    let malformed_bytes = b"title:: Must Not Exist\n\xff\xfe\n";
    fs::write(&malformed, malformed_bytes).unwrap();
    let graph = Graph::open(&dir);
    reset_page_build_test_counters(&graph);
    let target = dir.join("pages/Must Not Exist.md");

    let error = graph
        .save_page(
            &markdown_page_dto("Must Not Exist", "Must Not Exist", "- no\n").unwrap(),
            None,
        )
        .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
    assert!(error.to_string().contains("pages/Malformed.md"), "{error}");
    assert_eq!(fs::read(&malformed).unwrap(), malformed_bytes);
    assert!(!target.exists());
    assert_eq!(graph.page_index_failures(), vec!["pages/Malformed.md"]);
    assert_eq!(
        graph
            .page_build_test
            .enumerations
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        graph
            .page_build_test
            .censuses
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    let _ = fs::remove_dir_all(&dir);
}

/// One page Tine cannot read or parse used to refuse every name-only page
/// creation for the session, and failed every watcher cycle that met it, so
/// the watcher retried forever and never acknowledged the observation. Such
/// a page can hide only a name it can carry: its file name, or a name in its
/// text. Anything else is created, and the watcher records the page and goes
/// on.
#[test]
fn one_unreadable_page_blocks_neither_unrelated_creation_nor_the_watcher() {
    for (case, rejected) in [
        ("utf8", b"title:: Hidden Name\n\xff\xfe\n".to_vec()),
        (
            "parser",
            format!("title:: Hidden Name\n\n- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n").into_bytes(),
        ),
    ] {
        let dir = scratch(&format!("one-unreadable-page-{case}"));
        let path = dir.join("pages/Broken.md");
        fs::write(&path, b"- fine\n").unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();
        fs::write(&path, &rejected).unwrap();
        let synced = graph.sync_file_checked(&path);
        assert!(
            matches!(synced, Ok(None)),
            "{case}: the watcher failed on a page it cannot read: {synced:?}"
        );
        assert_eq!(graph.page_index_failures(), vec!["pages/Broken.md"]);

        let unrelated = markdown_page_dto("Unrelated", "Unrelated", "- yes\n").unwrap();
        let created = graph.save_page(&unrelated, None);
        assert!(
            created.is_ok(),
            "{case}: an unreadable page refused an unrelated creation: {created:?}"
        );
        // The title in its text: refused, naming the file.
        let blocked = markdown_page_dto("Hidden Name", "Hidden Name", "- no\n").unwrap();
        let error = graph.save_page(&blocked, None).unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::AlreadyExists,
            "{case}: {error}"
        );
        assert!(
            error.to_string().contains("pages/Broken.md"),
            "{case}: {error}"
        );
        // Its own file name: the file is there, and is never replaced.
        let blocked = markdown_page_dto("Broken", "Broken", "- no\n").unwrap();
        assert!(graph.save_page(&blocked, None).is_err(), "{case}");
        assert_eq!(fs::read(&path).unwrap(), rejected);
        let _ = fs::remove_dir_all(&dir);
    }
}

/// One unreadable page is left out of search, queries and references while
/// the rest of the graph works, so the user is told which page, once per
/// breakage.
#[test]
fn an_unreadable_page_is_announced_once_per_breakage() {
    let dir = scratch("unreadable-page-announcement");
    let path = dir.join("pages/Broken.md");
    fs::write(&path, b"- fine\n").unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();
    assert!(graph.take_unannounced_page_failures().is_empty());

    fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    assert!(graph.sync_file_checked(&path).unwrap().is_none());
    assert_eq!(
        graph.take_unannounced_page_failures(),
        vec!["pages/Broken.md"]
    );
    assert!(graph.take_unannounced_page_failures().is_empty());

    fs::write(&path, b"- repaired\n").unwrap();
    graph.sync_file_checked(&path).unwrap();
    assert!(graph.take_unannounced_page_failures().is_empty());
    fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    assert!(graph.sync_file_checked(&path).unwrap().is_none());
    assert_eq!(
        graph.take_unannounced_page_failures(),
        vec!["pages/Broken.md"]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn warm_install_and_watcher_failures_replace_effective_identity_evidence() {
    // A warm install records the failure set discovered from disk.
    let dir = scratch("empty-cold-then-warm-identity-failure");
    let graph = Graph::open(&dir);
    let invalid = dir.join("pages/Invalid.md");
    fs::write(&invalid, b"title:: Blocked Warm\n\xff\xfe\n").unwrap();
    graph.warm_cache();
    assert_eq!(graph.page_index_failures(), vec!["pages/Invalid.md"]);
    let blocked = markdown_page_dto("Blocked Warm", "Blocked Warm", "- no\n").unwrap();
    assert_eq!(
        graph.save_page(&blocked, None).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(!dir.join("pages/Blocked Warm.md").exists());
    let unrelated = markdown_page_dto("Unrelated Warm", "Unrelated Warm", "- yes\n").unwrap();
    graph.save_page(&unrelated, None).unwrap();
    assert!(dir.join("pages/Unrelated Warm.md").is_file());
    let installed = graph
        .effective_identity_index
        .read()
        .unwrap()
        .as_ref()
        .cloned()
        .unwrap();
    assert_eq!(installed.generation(), graph.cache_generation());
    assert_eq!(installed.failures, vec!["pages/Invalid.md"]);
    let _ = fs::remove_dir_all(&dir);

    // Invalid UTF-8 and parser rejection both become same-generation,
    // per-path failure evidence. A successful watcher reconciliation first
    // installs the repaired cache/identity state and only then clears it.
    for (case, rejected) in [
        ("utf8", b"title:: Blocked Watcher\n\xff\xfe\n".to_vec()),
        (
            "parser",
            format!("title:: Blocked Watcher\n\n- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n").into_bytes(),
        ),
    ] {
        let dir = scratch(&format!("watcher-effective-failure-{case}"));
        let path = dir.join("pages/Mutable.md");
        fs::write(&path, b"- before\n").unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();
        fs::write(&path, rejected).unwrap();
        assert!(graph.sync_file_checked(&path).unwrap().is_none());
        assert_eq!(graph.page_index_failures(), vec!["pages/Mutable.md"]);
        let failed = graph
            .effective_identity_index
            .read()
            .unwrap()
            .as_ref()
            .cloned()
            .unwrap();
        assert_eq!(failed.generation(), graph.cache_generation());
        assert_eq!(failed.failures, vec!["pages/Mutable.md"]);
        let blocked = markdown_page_dto("Blocked Watcher", "Blocked Watcher", "- no\n").unwrap();
        let error = graph.save_page(&blocked, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        let unrelated =
            markdown_page_dto("Unrelated Watcher", "Unrelated Watcher", "- yes\n").unwrap();
        graph.save_page(&unrelated, None).unwrap();

        fs::write(&path, b"title:: Repaired Identity\n\n- after\n").unwrap();
        graph.sync_file_checked(&path).unwrap();
        assert!(graph.page_index_failures().is_empty());
        let repaired = graph
            .effective_identity_index
            .read()
            .unwrap()
            .as_ref()
            .cloned()
            .unwrap();
        assert_eq!(repaired.generation(), graph.cache_generation());
        assert!(repaired.failures.is_empty());
        assert!(repaired
            .owners
            .contains_key(&page_cache_key(PageKind::Page, "Repaired Identity")));
        let allowed =
            markdown_page_dto("Allowed After Repair", "Allowed After Repair", "- yes\n").unwrap();
        graph.save_page(&allowed, None).unwrap();
        assert!(dir.join("pages/Allowed After Repair.md").is_file());
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(unix)]
#[test]
fn watcher_missing_and_changed_identity_record_failure_before_return() {
    for case in ["missing", "changed-identity"] {
        let dir = scratch(&format!("watcher-snapshot-{case}"));
        let path = dir.join("pages/Mutable.md");
        fs::write(&path, b"- before\n").unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();

        BOUNDED_READ_AFTER_METADATA.with(|hook| {
            let path = path.clone();
            *hook.borrow_mut() = Some(Box::new(move || {
                if case == "missing" {
                    fs::remove_file(path)
                } else {
                    let replacement = path.with_extension("replacement");
                    fs::write(&replacement, b"- replacement\n")?;
                    fs::rename(replacement, path)
                }
            }));
        });
        let result = graph.sync_file_checked(&path);
        if case == "missing" {
            // A vanished file owns nothing: no record, no false notice, and
            // its name is free once the watcher's delete lands (audit
            // R15-04).
            assert!(result.unwrap().is_none());
            assert!(graph.page_index_failures().is_empty());
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(graph.page_index_failures(), vec!["pages/Mutable.md"]);
        let failed = graph
            .effective_identity_index
            .read()
            .unwrap()
            .as_ref()
            .cloned()
            .unwrap();
        assert_eq!(failed.generation(), graph.cache_generation());
        assert_eq!(failed.failures, vec!["pages/Mutable.md"]);
        let blocked = markdown_page_dto("Mutable", "Mutable", "- no\n").unwrap();
        assert_eq!(
            graph.save_page(&blocked, None).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let unrelated =
            markdown_page_dto("Unrelated Snapshot", "Unrelated Snapshot", "- yes\n").unwrap();
        graph.save_page(&unrelated, None).unwrap();

        fs::write(&path, b"- repaired\n").unwrap();
        graph.sync_file_checked(&path).unwrap();
        assert!(graph.page_index_failures().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn cold_watcher_failures_publish_generation_bound_identity_evidence_until_repair() {
    for case in ["missing", "invalid-utf8"] {
        let dir = scratch(&format!("cold-watcher-effective-failure-{case}"));
        let path = dir.join("pages/Cold Failure.md");
        fs::write(&path, b"- before\n").unwrap();
        let graph = Graph::open(&dir);
        assert!(graph.cache.read().unwrap().is_none());
        assert!(graph.effective_identity_index.read().unwrap().is_none());

        if case == "missing" {
            BOUNDED_READ_AFTER_METADATA.with(|hook| {
                let path = path.clone();
                *hook.borrow_mut() = Some(Box::new(move || fs::remove_file(path)));
            });
            assert!(graph.sync_file_checked(&path).unwrap().is_none());
        } else {
            fs::write(&path, b"title:: Blocked Cold\n\xff\xfe\n").unwrap();
            assert!(graph.sync_file_checked(&path).unwrap().is_none());
        }

        // A vanished file owns nothing, so it is not recorded as unreadable
        // (audit R15-04); an invalid one is.
        let recorded: Vec<&str> = if case == "missing" {
            Vec::new()
        } else {
            vec!["pages/Cold Failure.md"]
        };
        assert!(graph.cache.read().unwrap().is_none());
        assert_eq!(graph.page_index_failures(), recorded);
        let failed = graph
            .effective_identity_index
            .read()
            .unwrap()
            .as_ref()
            .cloned()
            .expect("cold watcher failure must install effective evidence");
        assert_eq!(failed.generation(), graph.cache_generation());
        assert_eq!(failed.failures, recorded);
        if case != "missing" {
            assert!(failed.physical_paths.contains(&path));
        }

        // The unreadable file can own the title in its text; a vanished
        // file owns nothing. Neither owns an unrelated name.
        if case == "invalid-utf8" {
            let blocked = markdown_page_dto("Blocked Cold", "Blocked Cold", "- no\n").unwrap();
            let error = graph.save_page(&blocked, None).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
            assert!(!dir.join("pages/Blocked Cold.md").exists());
        }
        let unrelated = markdown_page_dto("Unrelated Cold", "Unrelated Cold", "- yes\n").unwrap();
        graph.save_page(&unrelated, None).unwrap();

        fs::write(&path, "title:: Repaired Cold\n\n- after\n").unwrap();
        graph.sync_file_checked(&path).unwrap();
        assert!(graph.page_index_failures().is_empty());
        let repaired = graph
            .effective_identity_index
            .read()
            .unwrap()
            .as_ref()
            .cloned()
            .unwrap();
        assert_eq!(repaired.generation(), graph.cache_generation());
        assert!(repaired.failures.is_empty());
        assert!(repaired
            .owners
            .contains_key(&page_cache_key(PageKind::Page, "Repaired Cold")));
        let exact = graph
            .load_by_path("pages/Cold Failure.md")
            .unwrap()
            .expect("repaired exact owner remains available");
        assert_eq!(exact.blocks[0].raw, "after");

        let allowed = markdown_page_dto("Allowed Cold", "Allowed Cold", "- yes\n").unwrap();
        graph.save_page(&allowed, None).unwrap();
        assert!(dir.join("pages/Allowed Cold.md").is_file());
        let _ = fs::remove_dir_all(&dir);
    }
}
