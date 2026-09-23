use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn unset_watch_mode_prefers_native_events_on_every_platform() {
    assert_eq!(default_watch_mode(), "inotify");
}
use tine_core::model::{BlockDto, Format, PageDto};
#[test]
fn atomic_page_save_temp_events_stay_incremental() {
    use notify::event::{EventKind, ModifyKind, RenameMode};

    let page = PathBuf::from("/graphs/a/pages/one.md");
    // `create_projection_temp` uses this exact suffix. Keeping the producer's
    // spelling here matters: the old test used a retired `.tmp` form and let
    // every real atomic rename fall through to a full-graph scan.
    let temp = PathBuf::from("/graphs/a/pages/.one.md.123.7.projection.tmp");
    let mut pending = Pending::default();
    pending.add_event(notify::Event {
        kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
        paths: vec![temp, page.clone()],
        attrs: Default::default(),
    });

    assert!(
        !pending.need_full,
        "Tine's own temp rename must not request a full scan"
    );
    assert_eq!(pending.paths, HashSet::from([page]));
}

/// GH #366. Windows reports ordinary file writes as `Create(Any)` or
/// `Modify(Any)`. The raw admission classifier already knew these were exact
/// page paths, but the queued classifier sent the same event through a full
/// graph diff. On a large graph that kept creation's external-change barrier
/// raised past every frontend retry.
#[test]
fn windows_exact_page_events_stay_incremental_through_the_pending_queue() {
    use notify::event::{CreateKind, EventKind, ModifyKind};

    let graph_dir = TempGraph::new("windows-pending-exact-page");
    graph_dir.write("pages/TINE版本更新提示词.md", "- 中文内容\n");
    let page = graph_dir.path("pages/TINE版本更新提示词.md");

    for kind in [
        EventKind::Create(CreateKind::Any),
        EventKind::Modify(ModifyKind::Any),
    ] {
        let mut pending = Pending::default();
        pending.add_event(event(kind, vec![page.clone()]));
        assert_eq!(pending.paths, HashSet::from([page.clone()]), "{kind:?}");
        assert!(pending.full_paths.is_empty(), "{kind:?}");
        assert!(!pending.need_full, "{kind:?}");
    }
}

#[test]
fn windows_unicode_event_reconciles_and_releases_new_page_creation() {
    use notify::event::{CreateKind, EventKind};

    let graph_dir = TempGraph::new("windows-unicode-frontier-release");
    graph_dir.write("pages/Anchor.md", "- anchor\n");
    let graph = Graph::open(&graph_dir.root);
    warm_direct_graph(&graph);
    let mut snap = collect_graph_text_files(&graph).files;

    graph_dir.write("pages/TINE版本更新提示词.md", "- 中文内容\n");
    let path = graph_dir.path("pages/TINE版本更新提示词.md");
    let create = event(EventKind::Create(CreateKind::Any), vec![path.clone()]);
    assert!(observe_graph_text_event(
        &graph,
        &graph_dir.root,
        Some(&create),
    ));
    let frontier = graph.graph_text_external_observation_ticket();
    assert_new_page_waits_for_reconciliation(&graph, "Blocked During Windows Event");

    let mut pending = Pending::default();
    pending.add_event(create);
    let (changes, _conflicts, used_full, errors) =
        reconcile_pending(&graph, &mut snap, &pending.paths, false, false);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!used_full, "one exact Windows page event must stay O(page)");
    assert_eq!(changes.len(), 1);
    assert!(graph.acknowledge_graph_text_external_observations(frontier));

    graph
        .save_page(&new_page("Creation After Windows Event"), None)
        .unwrap();
    assert!(graph_dir
        .path("pages/Creation After Windows Event.md")
        .exists());
}

/// GH #374 negative follow-up to #366. Windows reports the atomic
/// publication of Tine's own new journal as an exact graph-text event. That
/// echo must not raise the external-change admission frontier and strand the
/// next new page before the debounced reconciler sees the journal bytes.
#[test]
fn windows_tine_owned_create_echoes_do_not_block_following_pages_or_journals() {
    use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};

    let cases = [
        (
            "journal-page",
            new_journal("Aug 25th, 2026"),
            new_page("20260825100915"),
        ),
        ("page-page-unicode", new_page("第一页"), new_page("第二页")),
        (
            "page-journal",
            new_page("Before Journal"),
            new_journal("Aug 24th, 2026"),
        ),
    ];
    for (case, first, second) in cases {
        let graph_dir = TempGraph::new(&format!("windows-owned-{case}"));
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        warm_direct_graph(&graph);

        graph.save_page(&first, None).unwrap();
        let first_path = graph
            .find_entry(&first.name, first.kind)
            .expect("created first entry")
            .path;
        let before = graph.graph_text_external_observation_ticket();
        // ReadDirectoryChangesW can surface several exact shapes for the
        // one atomic publication before (and occasionally just after) the
        // debounce pass. Every one must validate the same exact receipt.
        for kind in [
            EventKind::Create(CreateKind::Any),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
        ] {
            let paths = if matches!(&kind, EventKind::Modify(ModifyKind::Name(_))) {
                let filename = first_path.file_name().unwrap().to_string_lossy();
                vec![
                    first_path.with_file_name(format!(".{filename}.123.7.projection.tmp")),
                    first_path.clone(),
                ]
            } else {
                vec![first_path.clone()]
            };
            assert!(observe_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(kind, paths)),
            ));
            assert_eq!(
                graph.graph_text_external_observation_ticket(),
                before,
                "{case}: Tine's exact publication echo must not become an external frontier"
            );
        }
        graph
            .sync_file_checked(&first_path)
            .expect("debounced self-write reconciliation");
        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&event(EventKind::Modify(ModifyKind::Any), vec![first_path],)),
        ));
        assert_eq!(
            graph.graph_text_external_observation_ticket(),
            before,
            "{case}: a delayed duplicate matching the admitted cache state remains a no-op"
        );

        graph
            .save_page(&second, None)
            .unwrap_or_else(|error| panic!("{case}: following creation failed: {error}"));
    }
}

#[test]
fn windows_external_replacement_of_tine_publication_keeps_creation_blocked() {
    use notify::event::{EventKind, ModifyKind};

    for same_bytes in [false, true] {
        let graph_dir = TempGraph::new(if same_bytes {
            "windows-external-same-bytes-new-identity"
        } else {
            "windows-external-different-bytes"
        });
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        warm_direct_graph(&graph);
        let first = new_page("First Publication");
        graph.save_page(&first, None).unwrap();
        let first_path = graph
            .find_entry(&first.name, first.kind)
            .expect("created page entry")
            .path;
        if same_bytes {
            let bytes = std::fs::read(&first_path).unwrap();
            let replacement = graph_dir.path("external-winner.tmp");
            std::fs::write(&replacement, bytes).unwrap();
            std::fs::remove_file(&first_path).unwrap();
            std::fs::rename(replacement, &first_path).unwrap();
        } else {
            std::fs::write(&first_path, "- external winner\n").unwrap();
        }

        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&event(EventKind::Modify(ModifyKind::Any), vec![first_path],)),
        ));
        assert_new_page_waits_for_reconciliation(
            &graph,
            if same_bytes {
                "Blocked By New Physical Owner"
            } else {
                "Blocked By Different External Bytes"
            },
        );
    }
}

#[test]
fn unknown_path_event_requests_full_scan_only_for_its_owner() {
    use notify::event::{CreateKind, EventKind};

    let unknown = PathBuf::from("/graphs/a/pages/new-directory");
    let mut pending = Pending::default();
    pending.add_event(notify::Event {
        kind: EventKind::Create(CreateKind::Folder),
        paths: vec![unknown.clone()],
        attrs: Default::default(),
    });

    assert!(!pending.need_full);
    assert_eq!(pending.full_paths, HashSet::from([unknown]));
    assert!(pending.paths.is_empty());
}

#[test]
fn pending_stamps_the_first_event_of_a_batch_once() {
    use notify::event::{CreateKind, EventKind};

    let mut pending = Pending::default();
    assert!(pending.first_event_at.is_none());
    pending.add_event(notify::Event {
        kind: EventKind::Create(CreateKind::File),
        paths: vec![PathBuf::from("/graphs/a/pages/one.md")],
        attrs: Default::default(),
    });
    let first = pending
        .first_event_at
        .expect("first event stamps the batch");
    pending.add_event(notify::Event {
        kind: EventKind::Create(CreateKind::File),
        paths: vec![PathBuf::from("/graphs/a/pages/two.md")],
        attrs: Default::default(),
    });
    assert_eq!(
        pending.first_event_at,
        Some(first),
        "later events in the same batch must not move the batch stamp"
    );
    pending.add_notify_error();
    assert_eq!(pending.first_event_at, Some(first));
}

#[test]
fn latency_receipt_measures_the_three_stages() {
    let first = Instant::now();
    let reconcile_started = first + Duration::from_millis(200);
    let emitted_at = reconcile_started + Duration::from_millis(35);

    let receipt = latency_receipt(
        "graph-a",
        true,
        3,
        4,
        false,
        1,
        Some(first),
        reconcile_started,
        emitted_at,
    );

    assert_eq!(receipt.graph, "graph-a");
    assert_eq!(receipt.mode, "inotify");
    assert_eq!(receipt.pages, 3);
    assert_eq!(receipt.event_paths, 4);
    assert!(!receipt.full_diff);
    assert_eq!(receipt.errors, 1);
    assert_eq!(receipt.event_to_reconcile_ms, Some(200));
    assert_eq!(receipt.reconcile_ms, 35);
    assert_eq!(receipt.event_to_emit_ms, Some(235));
}

#[test]
fn latency_receipt_without_a_callback_stamp_reports_only_reconcile_time() {
    let reconcile_started = Instant::now();
    let emitted_at = reconcile_started + Duration::from_millis(12);

    let receipt = latency_receipt(
        "graph-a",
        false,
        2,
        0,
        true,
        0,
        None,
        reconcile_started,
        emitted_at,
    );

    assert_eq!(receipt.mode, "poll");
    assert!(receipt.full_diff);
    assert_eq!(receipt.event_to_reconcile_ms, None);
    assert_eq!(receipt.event_to_emit_ms, None);
    assert_eq!(receipt.reconcile_ms, 12);
}

#[test]
fn latency_receipt_ring_keeps_the_newest_sixty_four() {
    let now = Instant::now();
    let mut ring = VecDeque::new();
    for seq in 1..=(LATENCY_RECEIPT_CAP as u64 + 6) {
        let mut receipt = latency_receipt("graph-a", true, 1, 1, false, 0, None, now, now);
        receipt.seq = seq;
        push_latency_receipt(&mut ring, receipt);
    }
    assert_eq!(ring.len(), LATENCY_RECEIPT_CAP);
    assert_eq!(ring.front().map(|receipt| receipt.seq), Some(7));
    assert_eq!(
        ring.back().map(|receipt| receipt.seq),
        Some(LATENCY_RECEIPT_CAP as u64 + 6)
    );
}

/// The receipt is a diagnostic wire format a reporter pastes into an issue;
/// its field names are part of that contract.
#[test]
fn latency_receipt_wire_shape_is_stable() {
    let now = Instant::now();
    let receipt = latency_receipt("graph-a", true, 1, 1, false, 0, Some(now), now, now);
    let wire = serde_json::to_value(&receipt).unwrap();
    for key in [
        "seq",
        "at_unix_ms",
        "graph",
        "mode",
        "pages",
        "event_paths",
        "full_diff",
        "errors",
        "event_to_reconcile_ms",
        "reconcile_ms",
        "event_to_emit_ms",
    ] {
        assert!(wire.get(key).is_some(), "missing receipt field {key}");
    }
}

#[test]
fn explicit_non_graph_text_file_events_do_not_schedule_graph_scans() {
    use notify::event::{CreateKind, EventKind, RemoveKind};

    for (kind, path, asset_paths) in [
        (
            EventKind::Create(CreateKind::File),
            PathBuf::from("/graphs/a/assets/image.png"),
            1,
        ),
        (
            EventKind::Remove(RemoveKind::File),
            PathBuf::from("/graphs/a/logseq/config.edn"),
            1,
        ),
    ] {
        let mut pending = Pending::default();
        pending.add_event(notify::Event {
            kind,
            paths: vec![path],
            attrs: Default::default(),
        });
        assert!(!pending.need_full);
        assert!(pending.full_paths.is_empty());
        assert!(pending.paths.is_empty());
        assert_eq!(
            pending.asset_paths.len(),
            asset_paths,
            "ordinary exact files must remain available to the separate asset observer"
        );
    }
}

#[test]
fn asset_observation_names_only_paths_inside_the_asset_capability() {
    let root = Path::new("/graphs/a/assets");
    assert_eq!(
        asset_relative_event_path(root, Path::new("/graphs/a/assets/diagrams/flow.svg")),
        Some("diagrams/flow.svg".into())
    );
    assert_eq!(
        asset_relative_event_path(root, Path::new("/graphs/a/pages/flow.svg")),
        None
    );
    assert_eq!(asset_relative_event_path(root, root), None);
}

#[cfg(unix)]
#[test]
fn external_asset_alias_events_refresh_replacement_and_deletion() {
    let graph = TempGraph::new("asset-watch-lexical-alias");
    let canonical = graph.path("external");
    let lexical = graph.path("assets");
    std::fs::create_dir_all(&canonical).unwrap();
    std::os::unix::fs::symlink(&canonical, &lexical).unwrap();
    let image = canonical.join("pixel.png");
    std::fs::write(&image, b"same bytes").unwrap();
    let mut state = AssetWatchState::new(canonical.clone());
    let empty = HashSet::new();
    // notify's recursive graph watch follows this symlink and may assign
    // its lexical path to the same descriptor as the canonical watch.
    let mut exact = HashSet::from([lexical.join("pixel.png")]);
    normalize_asset_event_aliases(
        std::iter::once((graph.path("").as_path(), &state)),
        &mut exact,
        &mut HashSet::new(),
    );
    std::fs::write(canonical.join("replacement"), b"same bytes").unwrap();
    std::fs::rename(canonical.join("replacement"), &image).unwrap();
    assert_eq!(
        reconcile_asset_observation("alias", &mut state, &exact, &empty, false, false),
        vec!["pixel.png"],
    );
    std::fs::remove_file(&image).unwrap();
    let mut exact = HashSet::from([lexical.join("pixel.png")]);
    normalize_asset_event_aliases(
        std::iter::once((graph.path("").as_path(), &state)),
        &mut exact,
        &mut HashSet::new(),
    );
    assert_eq!(
        reconcile_asset_observation("alias", &mut state, &exact, &empty, false, false),
        vec!["pixel.png"],
    );
}

#[test]
fn asset_alias_routing_refreshes_all_shared_root_windows_and_keeps_scope() {
    let graph = TempGraph::new("asset-alias-shared");
    let first_root = graph.path("first");
    let second_root = graph.path("second");
    let inactive_root = graph.path("inactive");
    let canonical = graph.path("external");
    std::fs::create_dir_all(&canonical).unwrap();
    let image = canonical.join("pixel.png");
    std::fs::write(&image, b"old").unwrap();
    let mut first = AssetWatchState::new(canonical.clone());
    let mut second = AssetWatchState::new(canonical.clone());
    let inactive = AssetWatchState::default();
    let lexical_event = second_root.join("assets/pixel.png");
    let mut exact = HashSet::from([
        lexical_event.clone(),
        first_root.join("assets/../outside.png"),
        inactive_root.join("assets/unapproved.png"),
        graph.path("unbound/assets/unknown.png"),
    ]);
    normalize_asset_event_aliases(
        [
            (first_root.as_path(), &first),
            (second_root.as_path(), &second),
            (inactive_root.as_path(), &inactive),
        ]
        .into_iter(),
        &mut exact,
        &mut HashSet::new(),
    );
    assert_eq!(exact.len(), 5, "only the approved pixel alias adds a path");
    assert!(
        exact.contains(&lexical_event),
        "preserve the original event"
    );
    assert!(exact.contains(&image));
    std::fs::write(&image, b"replacement image").unwrap();
    for (label, state) in [("first", &mut first), ("second", &mut second)] {
        assert_eq!(
            reconcile_asset_observation(label, state, &exact, &HashSet::new(), false, false),
            vec!["pixel.png"]
        );
    }
}

#[test]
fn asset_alias_directory_deletion_routes_without_following_missing_paths() {
    let graph = TempGraph::new("asset-alias-directory");
    let root = graph.path("graph");
    let canonical = graph.path("external");
    std::fs::create_dir_all(canonical.join("nested")).unwrap();
    std::fs::write(canonical.join("nested/image.png"), b"image").unwrap();
    let mut state = AssetWatchState::new(canonical.clone());
    std::fs::remove_dir_all(canonical.join("nested")).unwrap();
    let mut full = HashSet::from([root.join("assets/nested")]);
    normalize_asset_event_aliases(
        std::iter::once((root.as_path(), &state)),
        &mut HashSet::new(),
        &mut full,
    );
    assert!(full.contains(&canonical.join("nested")));
    assert_eq!(
        reconcile_asset_observation(
            "directory",
            &mut state,
            &HashSet::new(),
            &full,
            false,
            false
        ),
        vec!["nested/image.png"]
    );
    let mut parent = HashSet::from([root.clone()]);
    normalize_asset_event_aliases(
        std::iter::once((root.as_path(), &state)),
        &mut HashSet::new(),
        &mut parent,
    );
    assert!(
        parent.contains(&canonical),
        "uncertain root events include the approved asset root"
    );
}

#[test]
fn asset_observation_refreshes_peers_but_suppresses_the_originating_self_write() {
    let graph = TempGraph::new("asset-self-write");
    let assets = graph.path("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let mut origin = AssetWatchState::new(assets.clone());
    let mut peer = AssetWatchState::new(assets.clone());
    let empty = HashSet::new();
    assert!(
        reconcile_asset_observation("main", &mut origin, &empty, &empty, true, false,).is_empty()
    );
    assert!(
        reconcile_asset_observation("second", &mut peer, &empty, &empty, true, false,).is_empty()
    );

    let image = assets.join("nested/image.png");
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::write(&image, b"new image bytes").unwrap();
    note_asset_self_write("main", &image);
    let exact = HashSet::from([image]);

    assert_eq!(
        reconcile_asset_observation("second", &mut peer, &exact, &empty, false, false),
        vec!["nested/image.png"]
    );
    assert!(
        reconcile_asset_observation("main", &mut origin, &exact, &empty, false, false,).is_empty()
    );
}

#[test]
fn asset_read_before_watcher_binding_remains_the_refresh_baseline() {
    let graph = TempGraph::new("asset-read-before-watch");
    let assets = graph.path("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let image = assets.join("startup.png");
    std::fs::write(&image, b"bytes rendered before watcher binding").unwrap();
    note_asset_read("main", &image);

    // An external synchronizer replaces the file before AssetWatchState is
    // constructed. A fresh directory snapshot alone would baseline these
    // new bytes and lose the invalidation for the already-rendered image.
    let replacement = assets.join("startup.replacement");
    std::fs::write(&replacement, b"replacement bytes").unwrap();
    std::fs::rename(&replacement, &image).unwrap();
    let mut state = AssetWatchState::new(assets);

    assert_eq!(
        reconcile_asset_observation(
            "main",
            &mut state,
            &HashSet::new(),
            &HashSet::new(),
            false,
            false,
        ),
        vec!["startup.png"]
    );
}

#[test]
fn asset_read_handoff_compares_only_the_assets_that_were_read() {
    let graph = TempGraph::new("asset-read-exact-handoff");
    let assets = graph.path("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let rendered = assets.join("rendered.png");
    let unrelated = assets.join("unrelated.png");
    std::fs::write(&rendered, b"rendered old").unwrap();
    std::fs::write(&unrelated, b"unrelated old").unwrap();
    let mut state = AssetWatchState::new(assets);
    note_asset_read("exact-handoff", &rendered);

    std::fs::write(&rendered, b"rendered replacement").unwrap();
    std::fs::write(&unrelated, b"unrelated replacement").unwrap();

    assert_eq!(
        reconcile_asset_observation(
            "exact-handoff",
            &mut state,
            &HashSet::new(),
            &HashSet::new(),
            false,
            false,
        ),
        vec!["rendered.png"],
        "a WebView read seed must not trigger a whole asset-tree scan"
    );
}

#[cfg(unix)]
#[test]
fn self_write_marker_canonicalizes_an_approved_external_assets_link() {
    use std::os::unix::fs::symlink;

    let graph = TempGraph::new("asset-self-write-external");
    let external = TempGraph::new("asset-self-write-external-target");
    let external_assets = external.path("approved-assets");
    std::fs::create_dir_all(&external_assets).unwrap();
    symlink(&external_assets, graph.path("assets")).unwrap();
    let mut state = AssetWatchState::new(external_assets.clone());
    let image = external_assets.join("linked.png");
    std::fs::write(&image, b"linked image").unwrap();
    note_asset_self_write("main", &graph.path("assets/linked.png"));

    assert!(reconcile_asset_observation(
        "main",
        &mut state,
        &HashSet::from([image]),
        &HashSet::new(),
        false,
        false,
    )
    .is_empty());
}

#[test]
fn uncertain_asset_rescan_is_metadata_only_and_does_not_invent_deletions() {
    let source = crate::test_support::rust_module_production_source("watcher.rs");
    let body = source
        .split_once("fn collect_asset_files(")
        .unwrap()
        .1
        .split_once("fn asset_relative_event_path(")
        .unwrap()
        .0;
    assert!(!body.contains("std::fs::read("));
    assert!(!body.contains("Graph::"));
    assert!(!body.contains("observe_watcher"));

    let graph = TempGraph::new("asset-full-diff");
    let assets = graph.path("assets");
    std::fs::create_dir_all(&assets).unwrap();
    graph.write("assets/one.png", "one");
    let mut state = AssetWatchState::new(assets);
    let empty = HashSet::new();
    assert!(
        reconcile_asset_observation("main", &mut state, &empty, &empty, true, false,).is_empty()
    );
    graph.write("assets/one.png", "changed-length");
    graph.write("assets/two.svg", "two");
    assert_eq!(
        reconcile_asset_observation("main", &mut state, &empty, &empty, true, false),
        vec!["one.png", "two.svg"]
    );
}

#[test]
fn markdown_and_case_variant_text_events_stay_incremental() {
    use notify::event::{CreateKind, EventKind};

    let paths = vec![
        PathBuf::from("/graphs/a/archive/one.markdown"),
        PathBuf::from("/graphs/a/archive/two.MD"),
        PathBuf::from("/graphs/a/archive/three.ORG"),
    ];
    let mut pending = Pending::default();
    pending.add_event(notify::Event {
        kind: EventKind::Create(CreateKind::File),
        paths: paths.clone(),
        attrs: Default::default(),
    });
    assert_eq!(pending.paths, paths.into_iter().collect());
    assert!(pending.full_paths.is_empty());
    assert!(!pending.need_full);
}

/// Concord P5: what the watcher admits from `.git/**` and its equivalents,
/// stated explicitly and tested. A repository parked in the graph tree is
/// the loudest event source a Direct Files user has; none of its churn can
/// describe graph text, so none of it may cost anything.
#[test]
fn vcs_and_tool_churn_never_wakes_the_watcher() {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};

    let roots = HashSet::from([PathBuf::from("/graphs/a")]);
    let noise: &[(EventKind, &str)] = &[
        (
            EventKind::Create(CreateKind::File),
            "/graphs/a/.git/index.lock",
        ),
        (
            EventKind::Remove(RemoveKind::File),
            "/graphs/a/.git/index.lock",
        ),
        (
            EventKind::Modify(ModifyKind::Any),
            "/graphs/a/.git/objects/ab/cdef0123456789",
        ),
        (
            EventKind::Create(CreateKind::Any),
            "/graphs/a/.git/refs/heads/main",
        ),
        (
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            "/graphs/a/.hg/store/data/page.md.i",
        ),
        (
            EventKind::Create(CreateKind::File),
            "/graphs/a/.jj/repo/op_store/x",
        ),
        (EventKind::Create(CreateKind::File), "/graphs/a/.svn/wc.db"),
        (
            EventKind::Create(CreateKind::File),
            "/graphs/a/.stversions/pages/Note~20260818.md",
        ),
        (
            EventKind::Modify(ModifyKind::Any),
            "/graphs/a/.stfolder/marker",
        ),
        (
            EventKind::Create(CreateKind::File),
            "/graphs/a/node_modules/pkg/readme.md",
        ),
    ];
    for (kind, path) in noise {
        let event = notify::Event {
            kind: *kind,
            paths: vec![PathBuf::from(path)],
            attrs: Default::default(),
        };
        assert!(
            watch_event_is_tool_noise(&event, &roots),
            "{path} must not wake the watcher"
        );
    }

    // Everything else still gets through, including the cases a name list
    // is most likely to over-reach on.
    let admitted: &[(EventKind, Vec<&str>)] = &[
        // An ordinary page, and configuration.
        (
            EventKind::Create(CreateKind::File),
            vec!["/graphs/a/pages/Note.md"],
        ),
        (
            EventKind::Modify(ModifyKind::Any),
            vec!["/graphs/a/logseq/config.edn"],
        ),
        // A rename OUT of .git reports both sides: one ordinary path is
        // enough to keep the whole event.
        (
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Both)),
            vec!["/graphs/a/.git/tmp_obj_x", "/graphs/a/pages/Note.md"],
        ),
        // A path under no watched root is not ours to judge.
        (
            EventKind::Create(CreateKind::File),
            vec!["/elsewhere/.git/index"],
        ),
    ];
    for (kind, paths) in admitted {
        let event = notify::Event {
            kind: *kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        };
        assert!(
            !watch_event_is_tool_noise(&event, &roots),
            "{paths:?} must still be seen"
        );
    }

    // A kernel queue overflow says nothing about which paths were lost, so
    // its rescan demand survives even when its paths look like noise.
    let mut overflow = notify::Event {
        kind: EventKind::Create(CreateKind::File),
        paths: vec![PathBuf::from("/graphs/a/.git/objects")],
        attrs: Default::default(),
    };
    overflow = overflow.set_flag(notify::event::Flag::Rescan);
    assert!(!watch_event_is_tool_noise(&overflow, &roots));

    // With no watched root there is nothing to strip a prefix against.
    let event = notify::Event {
        kind: EventKind::Create(CreateKind::File),
        paths: vec![PathBuf::from("/graphs/a/.git/index.lock")],
        attrs: Default::default(),
    };
    assert!(!watch_event_is_tool_noise(&event, &HashSet::new()));
}

/// The list is only safe because every name on it is provably outside graph
/// text on its own authority — the core's `GraphTextScope`, which discovery
/// and the full-diff walk also use. If a name ever became page-bearing,
/// dropping its events would hide pages; this fails first.
#[test]
fn vcs_and_tool_noise_dirs_can_never_hold_graph_text() {
    let scope = tine_core::graph_text_scope::GraphTextScope::new(&[], false);
    for name in VCS_AND_TOOL_NOISE_DIRS {
        assert!(
            !scope.should_descend(name),
            "{name} must be outside graph text"
        );
        assert!(
            !scope.is_eligible(&format!("{name}/Page.md")),
            "{name}/Page.md must never be a page"
        );
        assert!(
            !scope.is_eligible(&format!("pages/{name}/Page.md")),
            "pages/{name}/Page.md must never be a page"
        );
    }
    // ...and the one Tine-owned hidden tree that is deliberately NOT here.
    assert!(!VCS_AND_TOOL_NOISE_DIRS.contains(&".tine-sync"));
}

/// A graph that itself lives inside a repository's directory must not have
/// every one of its own events dropped.
#[test]
fn a_graph_under_a_noise_directory_is_judged_relative_to_its_root() {
    use notify::event::{CreateKind, EventKind};

    let roots = HashSet::from([PathBuf::from("/repo/.git/notes")]);
    let event = notify::Event {
        kind: EventKind::Create(CreateKind::File),
        paths: vec![PathBuf::from("/repo/.git/notes/pages/Note.md")],
        attrs: Default::default(),
    };
    assert!(!watch_event_is_tool_noise(&event, &roots));
}

#[test]
fn hidden_sync_events_do_not_schedule_direct_files_reconciliation() {
    use notify::event::{CreateKind, EventKind};

    let chunk = PathBuf::from("/graphs/a/.tine-sync/v1/devices/device/sessions/session/0001.chunk");
    let mut pending = Pending::default();
    pending.add_event(notify::Event {
        kind: EventKind::Create(CreateKind::File),
        paths: vec![chunk.clone()],
        attrs: Default::default(),
    });

    assert!(!pending.need_full);
    assert!(pending.full_paths.is_empty());
    assert!(pending.paths.is_empty());
}

#[test]
fn notify_failures_remain_distinct_from_rescan_obligations() {
    let mut pending = Pending::default();
    pending.add_notify_error();
    assert!(pending.need_full);
    assert!(pending.notify_error);
}

#[test]
fn failed_reconciliation_retries_without_another_filesystem_event() {
    let start = Instant::now();
    let mut retry = RetrySchedule::default();
    retry.failed(start);
    assert!(!retry.take_due(start));
    assert!(retry.take_due(start + RETRY_BACKOFF[0]));

    retry.failed(start + RETRY_BACKOFF[0]);
    assert_eq!(
        retry.remaining(start + RETRY_BACKOFF[0]),
        Some(RETRY_BACKOFF[1])
    );
    retry.succeeded();
    assert_eq!(retry.remaining(start), None);
}

#[test]
fn reconciliation_backoff_is_capped_but_keeps_scheduling() {
    let start = Instant::now();
    let mut retry = RetrySchedule::default();
    for offset in 0..20 {
        retry.failed(start + Duration::from_secs(offset));
    }
    assert_eq!(
        retry.remaining(start + Duration::from_secs(19)),
        Some(*RETRY_BACKOFF.last().unwrap())
    );
}

// Direct Files data-safety audit 2026-08-09, finding 16, in its reachable
// form. The blindness the audit predicted is not permanent and there IS a
// fallback — but only when EVERY root fails, which empties `watched` and
// takes the bounded poll branch. The reachable case is mixed: with two
// graphs open (inotify watch limits are per-user, so a second large graph is
// exactly how one root fails while the other is fine), the inotify branch
// blocked forever on an event from the HEALTHY root, and the failing graph
// stayed invisible until that other graph happened to change.
//
// Two gaps, stated rather than papered over. These cover the wait POLICY,
// not a real kernel `watch()` failure — exhausting fs.inotify.max_user_watches
// needs privileges this box does not have — and not the wiring that feeds
// `desired.difference(&watched)` into it. They are therefore a specification
// of the rule, not fail-before evidence: `inotify_cycle_wait` did not exist
// before this change, so there is no earlier build they could have failed
// against. Do not read them as a regression guard for the loop itself.
#[test]
fn an_unwatched_root_bounds_the_wait_so_its_retry_actually_runs() {
    // Everything watched: block until the kernel says something, as before.
    assert_eq!(inotify_cycle_wait(None, false), None);
    // A root we want and do not have: never block indefinitely.
    assert_eq!(inotify_cycle_wait(None, true), Some(UNWATCHED_ROOT_RETRY));
}

#[test]
fn an_unwatched_root_never_delays_a_sooner_scheduled_retry() {
    let sooner = Duration::from_millis(250);
    assert_eq!(inotify_cycle_wait(Some(sooner), true), Some(sooner));
    assert_eq!(inotify_cycle_wait(Some(sooner), false), Some(sooner));
    let later = UNWATCHED_ROOT_RETRY * 4;
    assert_eq!(
        inotify_cycle_wait(Some(later), true),
        Some(UNWATCHED_ROOT_RETRY)
    );
    assert_eq!(inotify_cycle_wait(Some(later), false), Some(later));
}

#[test]
fn pending_paths_are_dispatched_only_to_the_owning_graph() {
    let a = TempGraph::new("owner-a");
    let b = TempGraph::new("owner-b");
    a.write("pages/one.md", "- one\n");
    b.write("journals/2026_07_10.md", "- two\n");
    let graph_a = Graph::open(&a.root);
    let graph_b = Graph::open(&b.root);
    let paths = HashSet::from([a.path("pages/one.md"), b.path("journals/2026_07_10.md")]);
    assert_eq!(pending_for_graph(&paths, &graph_a).len(), 1);
    assert_eq!(pending_for_graph(&paths, &graph_b).len(), 1);
    assert!(pending_for_graph(&paths, &graph_a)
        .iter()
        .all(|path| path.starts_with(&a.root)));
}

struct TempGraph {
    root: PathBuf,
}

impl TempGraph {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "tine-watch-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("journals")).unwrap();
        std::fs::create_dir_all(root.join("pages")).unwrap();
        Self { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.path(rel)).unwrap();
    }

    fn rename(&self, from: &str, to: &str) {
        let to = self.path(to);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::rename(self.path(from), to).unwrap();
    }
}

impl Drop for TempGraph {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn direct_watch_paths_keep_the_bound_graph_root() {
    let temp = TempGraph::new("direct-authority");
    let slot = GraphSlot::new(Graph::open(&temp.root), temp.root.clone());

    let (graph, root) = direct_watch_paths(&slot).unwrap();
    assert_eq!(graph.root, temp.root);
    assert_eq!(root, temp.root);
}

fn event(kind: notify::event::EventKind, paths: Vec<PathBuf>) -> notify::Event {
    notify::Event {
        kind,
        paths,
        attrs: Default::default(),
    }
}

/// Configuration is deliberately not graph text, so `incremental_page_paths`
/// throws it away — which is exactly why an outside edit to `config.edn` was
/// invisible until the next graph open. It has to be queued separately, for
/// every shape a writer can produce: an in-place write, and the temp+rename
/// that Tine, Logseq and Syncthing all actually use.
#[test]
fn a_config_edn_write_is_queued_even_though_it_is_not_graph_text() {
    use notify::event::{CreateKind, DataChange, EventKind, ModifyKind, RenameMode};
    let config = PathBuf::from("/graph/logseq/config.edn");

    for kind in [
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)),
        EventKind::Create(CreateKind::File),
    ] {
        let mut pending = Pending::default();
        pending.add_event(event(kind, vec![config.clone()]));
        assert!(
            pending.paths.is_empty(),
            "{kind:?}: configuration is not graph text and must not enter the page queue"
        );
        assert!(
            pending.config_paths.contains(&config),
            "{kind:?}: but it must reach the configuration queue"
        );
    }
}

#[test]
fn an_ordinary_page_write_queues_no_configuration_work() {
    use notify::event::{DataChange, EventKind, ModifyKind};
    let mut pending = Pending::default();
    pending.add_event(event(
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        vec![PathBuf::from("/graph/pages/Alpha.md")],
    ));
    assert!(pending.config_paths.is_empty());
    // And an unrelated EDN file is not configuration either. The filename
    // gate is cheap and rough; `is_config_file_path` is the decision.
    let mut pending = Pending::default();
    pending.add_event(event(
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        vec![PathBuf::from("/graph/logseq/pages-metadata.edn")],
    ));
    assert!(pending.config_paths.is_empty());
}

fn new_page(name: &str) -> PageDto {
    PageDto {
        activation: None,
        name: name.to_owned(),
        kind: PageKind::Page,
        title: name.to_owned(),
        pre_block: None,
        blocks: vec![BlockDto {
            id: format!("watcher-{}", name.replace(' ', "-")),
            raw: "local".to_owned(),
            ..BlockDto::default()
        }],
        rev: None,
        format: Format::Md,
        read_only: false,
        path: String::new(),
        guide: false,
    }
}

fn new_journal(name: &str) -> PageDto {
    let mut page = new_page(name);
    page.kind = PageKind::Journal;
    page
}

/// Warm the parsed page cache and exercise one ordinary Direct save. This
/// deliberately does not build the core's optional complete identity index.
fn warm_direct_graph(graph: &Graph) {
    warm_cache(graph);
    let mut anchor = graph.load_by_path("pages/Anchor.md").unwrap().unwrap();
    anchor.blocks[0].raw = "warm guarded identity".to_owned();
    graph
        .save_page(&anchor, anchor.rev.as_deref())
        .expect("warm guarded identity save");
}

#[test]
fn drained_frontier_routes_to_a_same_root_replacement_instance() {
    let graph_dir = TempGraph::new("watcher-drained-frontier-refresh");
    graph_dir.write("pages/Anchor.md", "- anchor\n");

    let old_slot = GraphSlot::new(Graph::open(&graph_dir.root), graph_dir.root.clone());
    let old_graph = old_slot.graph();
    warm_direct_graph(&old_graph);
    let old_ticket = old_graph.note_graph_text_external_observation();
    let mut graphs = HashMap::from([(
        "main".to_owned(),
        WatchedGraph {
            assets: AssetWatchState::new(old_graph.assets_path()),
            graph: old_graph,
            root: graph_dir.root.clone(),
            snap: HashMap::new(),
            baseline: true,
            last_reconcile_error: Some("retired retry".to_owned()),
            retry: RetrySchedule::default(),
            pending_observation_epoch: Some(old_ticket),
            transition_skipped: false,
        },
    )]);

    let replacement_slot = Arc::new(GraphSlot::new(
        Graph::open(&graph_dir.root),
        graph_dir.root.clone(),
    ));
    let replacement = replacement_slot.graph();
    warm_direct_graph(&replacement);
    graph_dir.write(
        "pages/Replacement Event.md",
        "- external replacement event\n",
    );
    let external = graph_dir.path("pages/Replacement Event.md");
    let replacement_ticket = replacement.note_graph_text_external_observation();
    let drained = HashMap::from([(graph_dir.root.clone(), replacement_ticket)]);

    route_drained_direct_frontiers(
        &mut graphs,
        vec![("main".to_owned(), replacement_slot)],
        &drained,
        |slot| Some(slot.graph().assets_path()),
    );

    let routed = graphs.get_mut("main").unwrap();
    assert!(routed
        .graph
        .owns_graph_text_external_observation_ticket(replacement_ticket));
    assert!(!routed.baseline);
    assert!(routed.last_reconcile_error.is_none());
    assert!(routed.pending_observation_epoch.is_none());

    routed.pending_observation_epoch = Some(replacement_ticket);
    routed.graph.sync_file_checked(&external).unwrap();
    let reconciled = routed.pending_observation_epoch.take().unwrap();
    assert!(routed
        .graph
        .acknowledge_graph_text_external_observations(reconciled));
    routed
        .graph
        .save_page(&new_page("Creation After Refresh"), None)
        .unwrap();
    assert!(graph_dir.path("pages/Creation After Refresh.md").exists());
}

/// A quiet poll must publish an exact empty observation rather than an
/// uncertain one. Whether that preserves a live guarded index belongs to
/// `tine-core` and is tested beside the private index builder there.
#[test]
fn quiet_poll_cycles_publish_an_exact_empty_observation() {
    let tg = TempGraph::new("poll-warm");
    tg.write("pages/Anchor.md", "- anchor\n");
    tg.write("Root note.md", "- root\n");
    let graph = Graph::open(&tg.root);
    let snap = collect_graph_text_files(&graph).files;
    for _ in 0..3 {
        let current = collect_graph_text_files(&graph);
        let (changed, uncertain) = poll_observation(&snap, &current);
        assert!(changed.is_empty());
        assert!(!uncertain);
    }
}

/// The other half of the same policy: a poll cycle that did observe an
/// external change must publish its exact path. Applying that path to a
/// live guarded index is tested inside `tine-core`.
#[test]
fn a_poll_cycle_reports_what_it_actually_observed() {
    let tg = TempGraph::new("poll-observe");
    tg.write("pages/Anchor.md", "- anchor\n");
    let graph = Graph::open(&tg.root);
    let snap = collect_graph_text_files(&graph).files;
    tg.write("Root note.md", "title:: Root note\n\n- external\n");
    let current = collect_graph_text_files(&graph);
    let (changed, uncertain) = poll_observation(&snap, &current);
    assert!(!uncertain);
    assert_eq!(changed, vec![tg.path("Root note.md")]);
}

/// The fallback half. If the rescan could not read part of the graph, what
/// it found is NOT a complete account of what changed, and the index must be
/// invalidated rather than exactly updated -- otherwise a save would trust
/// an index that is missing whatever lives behind the unreadable directory.
#[test]
fn an_incomplete_poll_scan_publishes_uncertainty() {
    let tg = TempGraph::new("poll-incomplete");
    tg.write("pages/Anchor.md", "- anchor\n");
    let graph = Graph::open(&tg.root);
    let snap = collect_graph_text_files(&graph).files;
    let mut incomplete = collect_graph_text_files(&graph);
    incomplete.complete = false;
    let (changed, uncertain) = poll_observation(&snap, &incomplete);
    assert!(changed.is_empty());
    assert!(uncertain);
}

/// The same thing end to end, where the walk itself decides. Root ignores
/// directory permissions, so this can only be demonstrated when the test
/// user is not root; it self-skips rather than passing vacuously.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_makes_the_scan_report_itself_incomplete() {
    use std::os::unix::fs::PermissionsExt;

    let tg = TempGraph::new("poll-unreadable");
    tg.write("Archive/Filed.md", "- filed\n");
    tg.write("pages/Anchor.md", "- anchor\n");
    let graph = Graph::open(&tg.root);
    assert!(collect_graph_text_files(&graph).complete);

    let blocked = tg.path("Archive");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable_anyway = std::fs::read_dir(&blocked).is_ok();
    let observed = collect_graph_text_files(&graph);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

    if readable_anyway {
        return; // running as root; permissions prove nothing here
    }
    assert!(
        !observed.complete,
        "a directory the scan could not read must make the scan report itself incomplete"
    );
}

fn assert_new_page_refused(graph: &Graph, name: &str) {
    assert_eq!(
        graph.save_page(&new_page(name), None).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists,
        "{name} must remain owned by the observed external graph text"
    );
}

fn assert_new_page_waits_for_reconciliation(graph: &Graph, name: &str) {
    assert_eq!(
        graph.save_page(&new_page(name), None).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "{name} creation must not race the watcher debounce window"
    );
}

fn reconcile_external_path(graph: &Graph, path: &Path) {
    let epoch = graph.graph_text_external_observation_ticket();
    graph
        .sync_file_checked(path)
        .expect("debounced exact-path reconciliation");
    graph.acknowledge_graph_text_external_observations(epoch);
}

#[test]
fn graph_root_text_create_delete_rename_and_semantics_reach_guarded_identity() {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    for extension in ["md", "org"] {
        let graph_dir = TempGraph::new(&format!("root-text-{extension}"));
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        warm_direct_graph(&graph);

        let created_rel = format!("nonstandard/deep/Physical Name.{extension}");
        graph_dir.write(
            &created_rel,
            &format!("title:: Created {extension}\n\n- external\n"),
        );
        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&event(
                EventKind::Create(CreateKind::File),
                vec![graph_dir.path(&created_rel)],
            )),
        ));
        assert_new_page_waits_for_reconciliation(&graph, &format!("Created {extension}"));
        reconcile_external_path(&graph, &graph_dir.path(&created_rel));
        assert_new_page_refused(&graph, &format!("Created {extension}"));

        let deleted_rel = format!("nonstandard/deep/Delete {extension}.{extension}");
        graph_dir.write(&deleted_rel, "- external\n");
        observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&event(
                EventKind::Create(CreateKind::File),
                vec![graph_dir.path(&deleted_rel)],
            )),
        );
        assert_new_page_waits_for_reconciliation(&graph, &format!("Delete {extension}"));
        reconcile_external_path(&graph, &graph_dir.path(&deleted_rel));
        assert_new_page_refused(&graph, &format!("Delete {extension}"));
        graph_dir.remove(&deleted_rel);
        let delete_event = event(
            EventKind::Remove(RemoveKind::File),
            vec![graph_dir.path(&deleted_rel)],
        );
        let deletion = graph_text_observation(&graph, &graph_dir.root, Some(&delete_event));
        assert!(!deletion.uncertain);
        assert_eq!(deletion.exact_paths, vec![graph_dir.path(&deleted_rel)]);
        observe_graph_text_event(&graph, &graph_dir.root, Some(&delete_event));
        assert_new_page_waits_for_reconciliation(&graph, &format!("Delete {extension}"));
        let delete_epoch = graph.graph_text_external_observation_ticket();
        graph
            .sync_deleted_file(&graph_dir.path(&deleted_rel))
            .expect("debounced deletion reconciliation");
        graph.acknowledge_graph_text_external_observations(delete_epoch);

        let old_rel = format!("nonstandard/deep/Old {extension}.{extension}");
        let new_rel = format!("nonstandard/deep/New {extension}.{extension}");
        graph_dir.write(&old_rel, "- external\n");
        observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&event(
                EventKind::Create(CreateKind::File),
                vec![graph_dir.path(&old_rel)],
            )),
        );
        assert_new_page_waits_for_reconciliation(&graph, &format!("Old {extension}"));
        reconcile_external_path(&graph, &graph_dir.path(&old_rel));
        assert_new_page_refused(&graph, &format!("Old {extension}"));
        graph_dir.rename(&old_rel, &new_rel);
        let rename_event = event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![graph_dir.path(&old_rel), graph_dir.path(&new_rel)],
        );
        let rename = graph_text_observation(&graph, &graph_dir.root, Some(&rename_event));
        assert!(!rename.uncertain);
        assert_eq!(
            rename.exact_paths,
            vec![graph_dir.path(&old_rel), graph_dir.path(&new_rel)]
        );
        observe_graph_text_event(&graph, &graph_dir.root, Some(&rename_event));
        assert_new_page_waits_for_reconciliation(&graph, &format!("New {extension}"));
        let rename_epoch = graph.graph_text_external_observation_ticket();
        graph
            .sync_deleted_file(&graph_dir.path(&old_rel))
            .expect("debounced rename source reconciliation");
        graph
            .sync_file_checked(&graph_dir.path(&new_rel))
            .expect("debounced rename destination reconciliation");
        graph.acknowledge_graph_text_external_observations(rename_epoch);
        assert_new_page_refused(&graph, &format!("New {extension}"));
    }
}

/// A callback arriving after batch A was drained belongs to batch B. Batch
/// A must acknowledge only its captured frontier, never the graph's newer
/// global epoch, or creation could race B before B is reconciled.
#[test]
fn drained_batch_cannot_acknowledge_a_callback_in_the_next_batch() {
    use notify::event::{CreateKind, EventKind};

    let graph_dir = TempGraph::new("watcher-drain-frontier-race");
    graph_dir.write("pages/Anchor.md", "- anchor\n");
    let graph = Graph::open(&graph_dir.root);
    warm_direct_graph(&graph);
    let mut pending = Pending::default();

    graph_dir.write("pages/External A.md", "- external A\n");
    let path_a = graph_dir.path("pages/External A.md");
    assert!(observe_graph_text_event(
        &graph,
        &graph_dir.root,
        Some(&event(
            EventKind::Create(CreateKind::File),
            vec![path_a.clone()],
        )),
    ));
    pending.add_legacy_observations(vec![(
        graph_dir.root.clone(),
        graph.graph_text_external_observation_ticket(),
    )]);
    let batch_a = pending.take_legacy_observation_epochs();

    graph_dir.write("pages/External B.md", "- external B\n");
    let path_b = graph_dir.path("pages/External B.md");
    assert!(observe_graph_text_event(
        &graph,
        &graph_dir.root,
        Some(&event(
            EventKind::Create(CreateKind::File),
            vec![path_b.clone()],
        )),
    ));
    pending.add_legacy_observations(vec![(
        graph_dir.root.clone(),
        graph.graph_text_external_observation_ticket(),
    )]);

    graph.sync_file_checked(&path_a).unwrap();
    graph.acknowledge_graph_text_external_observations(batch_a[&graph_dir.root]);
    assert_new_page_waits_for_reconciliation(&graph, "Still Blocked By B");

    let batch_b = pending.take_legacy_observation_epochs();
    graph.sync_file_checked(&path_b).unwrap();
    graph.acknowledge_graph_text_external_observations(batch_b[&graph_dir.root]);
    graph.save_page(&new_page("Now Reconciled"), None).unwrap();
}

#[test]
fn legacy_uncertain_graph_root_events_block_creation_until_reconciliation() {
    use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};
    use notify::event::{EventAttributes, Flag};

    // Not `config.edn`: configuration is not graph text and has its own
    // queue (GH #543, audit R9-05;
    // `a_config_write_reaches_only_the_configuration_queue`).
    for case in ["root-create", "directory-rename", "rescan", "notify-error"] {
        let graph_dir = TempGraph::new(&format!("uncertain-{case}"));
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        warm_direct_graph(&graph);
        graph_dir.write(
            "nonstandard/deep/Physical.md",
            &format!("title:: Epoch {case}\n\n- external\n"),
        );

        let event = match case {
            "root-create" => Some(event(
                EventKind::Create(CreateKind::Folder),
                vec![graph_dir.root.clone()],
            )),
            "directory-rename" => {
                std::fs::create_dir_all(graph_dir.path("nonstandard/from.md")).unwrap();
                graph_dir.rename("nonstandard/from.md", "nonstandard/to.md");
                Some(event(
                    EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                    vec![
                        graph_dir.path("nonstandard/from.md"),
                        graph_dir.path("nonstandard/to.md"),
                    ],
                ))
            }
            "rescan" => {
                let mut attrs = EventAttributes::new();
                attrs.set_flag(Flag::Rescan);
                Some(notify::Event {
                    kind: EventKind::Other,
                    paths: Vec::new(),
                    attrs,
                })
            }
            "notify-error" => None,
            _ => unreachable!(),
        };
        let observation = graph_text_observation(&graph, &graph_dir.root, event.as_ref());
        assert!(observation.relevant, "{case}");
        assert!(observation.uncertain, "{case}");
        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            event.as_ref(),
        ));
        assert_new_page_waits_for_reconciliation(&graph, &format!("Epoch {case}"));
    }
}

/// Windows ReadDirectoryChangesW reports no sub-kind, so notify emits
/// `Create(Any)` / `Modify(Any)` / `Remove(Any)` for everything except a
/// rename. Those used to fall to the catch-all and mark the observation
/// uncertain, which invalidates the whole guarded identity index -- meaning
/// every external file event on Windows made the next save rebuild the
/// entire graph. With an active sync client that is every save.
///
/// Create/Modify are resolvable (the path still exists) and must now take
/// the exact-path arm. For `Remove(Any)` the entry is gone, so the graph's
/// own inventory answers whether it held pages: a removed page file is one
/// exact path, and only a removed directory of pages is uncertain (GH #543,
/// audit R10-08).
#[test]
fn ambiguous_windows_event_kinds_take_the_exact_path_arm_unless_pages_went() {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};

    let graph_dir = TempGraph::new("windows-any-kinds");
    graph_dir.write("pages/Anchor.md", "- anchor\n");
    graph_dir.write("pages/Folder/Inner.md", "- inner\n");
    let graph = Graph::open(&graph_dir.root);
    warm_direct_graph(&graph);

    graph_dir.write("pages/External.md", "- external\n");
    let path = graph_dir.path("pages/External.md");

    for kind in [
        EventKind::Create(CreateKind::Any),
        EventKind::Modify(ModifyKind::Any),
    ] {
        let observation = graph_text_observation(
            &graph,
            &graph_dir.root,
            Some(&event(kind, vec![path.clone()])),
        );
        assert!(observation.relevant, "{kind:?}");
        assert!(
            !observation.uncertain,
            "{kind:?} carries an exact path and must not poison the whole index"
        );
        assert_eq!(observation.exact_paths, vec![path.clone()], "{kind:?}");
    }

    // A directory event still cannot be treated as a page file, even when
    // the sub-kind is missing -- the arm discriminates against the live
    // filesystem, not against the event kind.
    std::fs::create_dir_all(graph_dir.path("pages/sub")).unwrap();
    let directory = graph_text_observation(
        &graph,
        &graph_dir.root,
        Some(&event(
            EventKind::Create(CreateKind::Any),
            vec![graph_dir.path("pages/sub")],
        )),
    );
    assert!(
        directory.uncertain,
        "an ambiguous event on a directory is not provably a page file"
    );

    // A removed page file is one exact path: the graph held no pages under it.
    std::fs::remove_file(&path).unwrap();
    let removed = graph_text_observation(
        &graph,
        &graph_dir.root,
        Some(&event(
            EventKind::Remove(RemoveKind::Any),
            vec![path.clone()],
        )),
    );
    assert!(
        !removed.uncertain,
        "a removed page file must not poison the whole index"
    );
    assert_eq!(removed.exact_paths, vec![path]);

    // A removed directory of pages stays uncertain: every page under it went.
    std::fs::remove_dir_all(graph_dir.path("pages/Folder")).unwrap();
    let folder = graph_text_observation(
        &graph,
        &graph_dir.root,
        Some(&event(
            EventKind::Remove(RemoveKind::Any),
            vec![graph_dir.path("pages/Folder")],
        )),
    );
    assert!(
        folder.uncertain,
        "Remove(Any) of a directory of pages must not be mistaken for one path"
    );
}

/// GH #543 (audit R9-05, R9-06): a rename, or a removal that did not say
/// file or directory, names a path that may no longer exist. Both watcher
/// deciders used to read every such path as "maybe a directory of pages":
/// the batch queue ran a full diff of the graph and the callback
/// invalidated the guarded identity index. Tine writes `config.edn` by two
/// renames, so every settings change cost a whole-graph stat walk. Both
/// deciders now ask `graph_text_watch_reach`, so they agree, and the
/// configuration file reaches only the configuration queue. (What the
/// reach answers from a current identity index is pinned in the core:
/// `a_gone_path_is_a_subtree_only_if_the_index_holds_a_file_under_it`.)
#[test]
fn a_config_write_reaches_only_the_configuration_queue() {
    use notify::event::{EventKind, ModifyKind, RemoveKind, RenameMode};

    let graph_dir = TempGraph::new("watch-reach");
    graph_dir.write("pages/Anchor.md", "- anchor\n");
    graph_dir.write("pages/Gone.md", "- gone\n");
    graph_dir.write("pages/Folder/Inner.md", "- inner\n");
    graph_dir.write("logseq/config.edn", "{}\n");
    let graph = Graph::open(&graph_dir.root);
    warm_direct_graph(&graph);

    let rename =
        |mode, paths: Vec<PathBuf>| event(EventKind::Modify(ModifyKind::Name(mode)), paths);
    let queued = |events: &[notify::Event]| {
        let mut pending = Pending::default();
        for event in events {
            pending.add_event(event.clone());
        }
        let (subtrees, files) = unclassified_paths_for_graph(&pending.full_paths, &graph);
        (pending, subtrees, files)
    };

    // Tine's own config write: the live file renamed aside, the staged
    // file renamed into place (`atomic_replace_expected_with_mover`).
    let config = graph_dir.path("logseq/config.edn");
    let retired = graph_dir.path("logseq/.config.edn.1.1.retired");
    let staged = graph_dir.path("logseq/.config.edn.1.1.publish.tmp");
    graph_dir.rename("logseq/config.edn", "logseq/.config.edn.1.1.retired");
    graph_dir.write("logseq/.config.edn.1.1.publish.tmp", "{:x 1}\n");
    graph_dir.rename("logseq/.config.edn.1.1.publish.tmp", "logseq/config.edn");
    let config_events = [
        rename(RenameMode::From, vec![config.clone()]),
        rename(RenameMode::To, vec![retired.clone()]),
        rename(RenameMode::Both, vec![config.clone(), retired.clone()]),
        rename(RenameMode::From, vec![staged.clone()]),
        rename(RenameMode::To, vec![config.clone()]),
        rename(RenameMode::Both, vec![staged, config.clone()]),
        event(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            vec![config.clone()],
        ),
    ];
    for event in &config_events {
        let observation = graph_text_observation(&graph, &graph_dir.root, Some(event));
        assert!(
            !observation.uncertain && observation.exact_paths.is_empty(),
            "{event:?}: configuration is not graph text"
        );
    }
    let (pending, subtrees, files) = queued(&config_events);
    assert!(
        subtrees.is_empty(),
        "a config write must not diff the whole graph"
    );
    assert!(files.is_empty() && pending.paths.is_empty());
    assert!(
        pending.config_paths.contains(&config),
        "it reaches the config queue"
    );
    let before = graph.guarded_graph_text_identity_report();
    for event in &config_events {
        observe_graph_text_event(&graph, &graph_dir.root, Some(event));
    }
    let after = graph.guarded_graph_text_identity_report();
    assert_eq!(
        (after.invalidated, after.generation),
        (before.invalidated, before.generation),
        "a config write leaves the guarded identity index alone"
    );
    graph
        .save_page(&new_page("Created After Settings"), None)
        .expect(
            "a config write publishes no graph-text epoch, so creation does not wait on a \
         reconciliation that a config-only batch never runs",
        );

    // A page renamed away, or removed with no kind, and a folder of pages
    // renamed away: both deciders ask the same reach.
    graph_dir.remove("pages/Gone.md");
    let folder = graph_dir.path("pages/Folder");
    std::fs::remove_dir_all(&folder).unwrap();
    for (path, event) in [
        (
            graph_dir.path("pages/Gone.md"),
            rename(RenameMode::From, vec![graph_dir.path("pages/Gone.md")]),
        ),
        (
            graph_dir.path("pages/Gone.md"),
            event(
                EventKind::Remove(RemoveKind::Any),
                vec![graph_dir.path("pages/Gone.md")],
            ),
        ),
        (
            folder.clone(),
            rename(RenameMode::From, vec![folder.clone()]),
        ),
    ] {
        let subtree = graph.graph_text_watch_reach(&path) == GraphTextWatchReach::Subtree;
        let observation = graph_text_observation(&graph, &graph_dir.root, Some(&event));
        assert_eq!(observation.uncertain, subtree, "{event:?}");
        let (pending, subtrees, files) = queued(std::slice::from_ref(&event));
        // The queue takes a renamed page name as that page without asking
        // (`incremental_page_paths`); it may be less conservative than the
        // callback, never more.
        assert!(!subtrees.contains(&path) || subtree, "{event:?}");
        assert!(
            subtrees.contains(&path) || files.contains(&path) || pending.paths.contains(&path),
            "{event:?} is still reconciled"
        );
    }
}

#[test]
fn graph_root_observation_routes_only_to_the_owning_graph() {
    use notify::event::{CreateKind, EventKind};

    let graph_a_dir = TempGraph::new("root-owner-a");
    let graph_b_dir = TempGraph::new("root-owner-b");
    graph_a_dir.write("pages/Anchor.md", "- anchor A\n");
    graph_b_dir.write("pages/Anchor.md", "- anchor B\n");
    let graph_a = Graph::open(&graph_a_dir.root);
    let graph_b = Graph::open(&graph_b_dir.root);

    graph_b_dir.write(
        "nonstandard/deep/Stale.md",
        "title:: Must Stay Stale\n\n- external without a B callback\n",
    );
    graph_a_dir.write("nonstandard/deep/A.md", "- observed A\n");
    let event_a = event(
        EventKind::Create(CreateKind::File),
        vec![graph_a_dir.path("nonstandard/deep/A.md")],
    );
    assert!(observe_graph_text_event(
        &graph_a,
        &graph_a_dir.root,
        Some(&event_a),
    ));
    let graph_b_observation = graph_text_observation(&graph_b, &graph_b_dir.root, Some(&event_a));
    assert!(!graph_b_observation.relevant);
    assert!(!graph_b_observation.uncertain);
    assert!(graph_b_observation.exact_paths.is_empty());
    assert!(!observe_graph_text_event(
        &graph_b,
        &graph_b_dir.root,
        Some(&event_a),
    ));
}

#[test]
fn excluded_private_text_and_exact_non_text_events_are_not_published() {
    use notify::event::{CreateKind, EventKind};

    let graph_dir = TempGraph::new("excluded-private");
    graph_dir.write("pages/Anchor.md", "- anchor\n");
    let graph = Graph::open(&graph_dir.root);
    for (relative, claimed) in [
        (".tine-sync/private/Sync.md", "Excluded Sync"),
        ("assets/private/Asset.org", "Excluded Asset"),
        ("logseq/bak/recovery.md", "Excluded Recovery"),
        (".hidden/private.md", "Excluded Hidden"),
    ] {
        graph_dir.write(relative, &format!("title:: {claimed}\n\n- private\n"));
        let event = event(
            EventKind::Create(CreateKind::File),
            vec![graph_dir.path(relative)],
        );
        let observation = graph_text_observation(&graph, &graph_dir.root, Some(&event));
        assert!(observation.relevant, "{relative}");
        assert!(!observation.uncertain, "{relative}");
        assert!(observation.exact_paths.is_empty(), "{relative}");
        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&event)
        ));
    }

    graph_dir.write(
        "nonstandard/deep/Stale.md",
        "title:: Exact Non Text Is Harmless\n\n- unobserved\n",
    );
    graph_dir.write("nonstandard/deep/image.png", "not graph text\n");
    let non_text = event(
        EventKind::Create(CreateKind::File),
        vec![graph_dir.path("nonstandard/deep/image.png")],
    );
    let observation = graph_text_observation(&graph, &graph_dir.root, Some(&non_text));
    assert!(!observation.uncertain);
    assert!(observation.exact_paths.is_empty());
    assert!(observe_graph_text_event(
        &graph,
        &graph_dir.root,
        Some(&non_text)
    ));

    for relative in [".tine-sync", "assets", "logseq/bak", ".hidden"] {
        let private_directory = event(
            EventKind::Create(CreateKind::Folder),
            vec![graph_dir.path(relative)],
        );
        let observation = graph_text_observation(&graph, &graph_dir.root, Some(&private_directory));
        assert!(!observation.uncertain, "{relative}");
        assert!(observation.exact_paths.is_empty(), "{relative}");
    }
}

fn warm_cache(graph: &Graph) {
    let _ = graph.search("__watcher_warm_cache__", 1);
}

fn sorted_changes(mut changes: Vec<GraphChange>) -> Vec<GraphChange> {
    fn kind_key(kind: PageKind) -> &'static str {
        match kind {
            PageKind::Journal => "journal",
            PageKind::Page => "page",
        }
    }
    changes.sort_by(|a, b| {
        (a.removed, kind_key(a.kind), a.name.as_str()).cmp(&(
            b.removed,
            kind_key(b.kind),
            b.name.as_str(),
        ))
    });
    changes
}

fn rel_paths(tg: &TempGraph, rels: &[&str]) -> HashSet<PathBuf> {
    rels.iter().map(|rel| tg.path(rel)).collect()
}

fn assert_incremental_matches_full(
    name: &str,
    setup: impl FnOnce(&TempGraph),
    mutate: impl FnOnce(&TempGraph) -> HashSet<PathBuf>,
) {
    let tg = TempGraph::new(name);
    setup(&tg);

    let inc_graph = Graph::open(&tg.root);
    let full_graph = Graph::open(&tg.root);
    warm_cache(&inc_graph);
    warm_cache(&full_graph);

    let mut inc_snap = collect_graph_text_files(&inc_graph).files;
    let mut full_snap = inc_snap.clone();

    let paths = mutate(&tg);

    let (inc_changes, inc_conflicts_dirty, inc_errors) =
        incremental_reconcile(&inc_graph, &mut inc_snap, &paths);
    let fresh = collect_graph_text_files(&inc_graph).files;
    let (full_changes, full_conflicts_dirty, full_errors) =
        full_diff_reconcile(&full_graph, &mut full_snap, fresh.clone());

    assert_eq!(inc_snap, fresh, "incremental snap must match full scan");
    assert_eq!(full_snap, fresh, "full snap must match fresh scan");
    assert_eq!(inc_conflicts_dirty, full_conflicts_dirty);
    assert!(inc_errors.is_empty());
    assert!(full_errors.is_empty());
    assert_eq!(
        sorted_changes(inc_changes),
        sorted_changes(full_changes),
        "incremental changes must match full-diff changes"
    );
}

fn snapshot_relative_names(tg: &TempGraph, graph: &Graph) -> Vec<String> {
    let mut names: Vec<String> = collect_graph_text_files(graph)
        .files
        .keys()
        .map(|path| {
            path.strip_prefix(&tg.root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    names.sort();
    names
}

#[test]
fn snapshot_covers_exactly_what_discovery_covers() {
    // #21: page files in sub-folders must be in the snapshot, so an edit
    // there is reconciled rather than invisible until a graph reopen.
    // GH #268: so must a page at the graph ROOT or in a custom folder --
    // `graph_text_inventory` walks graph-wide through `GraphTextScope`, and
    // a snapshot narrower than discovery makes those pages permanently
    // unreconcilable. Excluded trees stay excluded, by the same authority.
    let tg = TempGraph::new("snapshot-scope");
    tg.write("top.md", "- t\n");
    tg.write("pages/Page.md", "- p\n");
    tg.write("journals/2026_08_06.md", "- j\n");
    tg.write("Archive/mid.org", "* m\n");
    tg.write("Archive/Deep/Deeper/deep.md", "- d\n");
    tg.write("Archive/notes.txt", "ignored\n");
    tg.write(".hidden/skip.md", "- s\n");
    tg.write("assets/embedded.md", "- a\n");
    tg.write("logseq/bak/old.md", "- b\n");

    assert_eq!(
        snapshot_relative_names(&tg, &Graph::open(&tg.root)),
        vec![
            "Archive/Deep/Deeper/deep.md",
            "Archive/mid.org",
            "journals/2026_08_06.md",
            "pages/Page.md",
            "top.md",
        ]
    );
}

#[cfg(unix)]
#[test]
fn snapshot_does_not_follow_page_symlinks() {
    use std::os::unix::fs::symlink;

    let tg = TempGraph::new("snapshot-symlink");
    let outside =
        std::env::temp_dir().join(format!("tine-watch-outside-{}.md", std::process::id()));
    let _ = std::fs::remove_file(&outside);
    std::fs::write(&outside, "- outside\n").unwrap();
    symlink(&outside, tg.path("pages/secret.md")).unwrap();

    assert!(snapshot_relative_names(&tg, &Graph::open(&tg.root)).is_empty());

    std::fs::remove_file(&outside).ok();
}

#[test]
fn graph_wide_external_paths_are_routed_like_discovery_routes_them() {
    // GH #268, the event-routing half. The watch is installed recursively on
    // the graph ROOT, so these events all arrive; the reconcile lane used to
    // filter them against `journals/` + `pages/` and silently drop the rest.
    let tg = TempGraph::new("event-scope");
    tg.write("top.md", "- t\n");
    tg.write("Archive/mid.md", "- m\n");
    tg.write("pages/Page.md", "- p\n");
    tg.write("assets/embedded.md", "- a\n");
    tg.write(".hidden/skip.md", "- s\n");
    let graph = Graph::open(&tg.root);

    use notify::event::{DataChange, EventKind, ModifyKind};
    let mut pending = Pending::default();
    for relative in [
        "top.md",
        "Archive/mid.md",
        "pages/Page.md",
        "assets/embedded.md",
        ".hidden/skip.md",
    ] {
        pending.add_event(event(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            vec![tg.path(relative)],
        ));
    }

    let mut routed: Vec<String> = pending_for_graph(&pending.paths, &graph)
        .iter()
        .map(|path| {
            path.strip_prefix(&tg.root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    routed.sort();
    assert_eq!(routed, vec!["Archive/mid.md", "pages/Page.md", "top.md"]);
}

#[test]
fn unclassified_paths_force_a_full_scan_only_where_pages_could_live() {
    // A directory move reports a path whose nature we cannot know, so it
    // forces a full diff. Excluded trees must not: dropping an image into
    // `assets/` cannot change the text inventory, and rescanning the graph
    // for it is the amplification this program exists to remove.
    let tg = TempGraph::new("full-scan-owner");
    let graph = Graph::open(&tg.root);
    let (owned, files) = unclassified_paths_for_graph(
        &HashSet::from([
            tg.path("pages/Moved"),
            tg.path("Archive"),
            tg.path("assets"),
            tg.path("assets/pictures"),
            tg.path(".git/objects"),
            tg.path("logseq/config.edn"),
            PathBuf::from("/somewhere/else/pages"),
        ]),
        &graph,
    );
    assert!(files.is_empty());
    let mut owned: Vec<String> = owned
        .iter()
        .map(|path| {
            path.strip_prefix(&tg.root)
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().into_owned())
        })
        .collect();
    owned.sort();
    assert_eq!(owned, vec!["Archive", "pages/Moved"]);
}

#[test]
fn incremental_create_top_level_file_matches_full_diff() {
    assert_incremental_matches_full(
        "create-top",
        |tg| tg.write("pages/Seed.md", "- seed\n"),
        |tg| {
            tg.write("pages/New.md", "- new\n");
            rel_paths(tg, &["pages/New.md"])
        },
    );
}

#[test]
fn incremental_create_is_identified_as_inventory_change() {
    let tg = TempGraph::new("create-inventory");
    tg.write("pages/Seed.md", "- seed\n");
    let graph = Graph::open(&tg.root);
    warm_cache(&graph);
    let mut snap = collect_graph_text_files(&graph).files;
    let path = tg.path("pages/New.md");
    tg.write("pages/New.md", "- new\n");

    let (changes, conflicts_dirty, errors) =
        incremental_reconcile(&graph, &mut snap, &HashSet::from([path]));

    assert!(!conflicts_dirty);
    assert!(errors.is_empty());
    assert_eq!(changes.len(), 1);
    assert!(changes[0].created);
    assert!(!changes[0].removed);
}

#[test]
fn incremental_create_nested_file_matches_full_diff() {
    assert_incremental_matches_full(
        "create-nested",
        |tg| tg.write("pages/Seed.md", "- seed\n"),
        |tg| {
            tg.write("pages/sub/New.md", "- nested\n");
            rel_paths(tg, &["pages/sub/New.md"])
        },
    );
}

#[test]
fn incremental_modify_len_change_matches_full_diff() {
    assert_incremental_matches_full(
        "modify-len",
        |tg| tg.write("pages/Edit.md", "- one\n"),
        |tg| {
            std::thread::sleep(Duration::from_millis(20));
            tg.write("pages/Edit.md", "- one\n- two\n");
            rel_paths(tg, &["pages/Edit.md"])
        },
    );
}

#[test]
fn incremental_modify_same_len_mtime_change_matches_full_diff() {
    assert_incremental_matches_full(
        "modify-same-len",
        |tg| tg.write("pages/Edit.md", "- alpha\n"),
        |tg| {
            std::thread::sleep(Duration::from_millis(20));
            tg.write("pages/Edit.md", "- beta!\n");
            rel_paths(tg, &["pages/Edit.md"])
        },
    );
}

#[test]
fn explicit_event_reconciles_even_when_snapshot_metadata_is_equal() {
    let tg = TempGraph::new("explicit-same-metadata");
    tg.write("pages/Edit.md", "- alpha\n");
    let graph = Graph::open(&tg.root);
    warm_cache(&graph);
    let path = tg.path("pages/Edit.md");
    tg.write("pages/Edit.md", "- bravo\n"); // equal byte length
    let stamp = file_snapshot(&path).unwrap();
    // Simulate a sync tool preserving every snapshot field: the explicit
    // notify path must still reach Graph::sync_file's content comparison.
    let mut snap = HashMap::from([(path.clone(), stamp)]);
    let (changes, conflicts_dirty, errors) =
        incremental_reconcile(&graph, &mut snap, &HashSet::from([path]));
    assert!(!conflicts_dirty);
    assert!(errors.is_empty());
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].name, "Edit");
    assert_eq!(changes[0].kind, PageKind::Page);
    assert!(!changes[0].created);
    assert!(!changes[0].removed);
}

#[test]
fn incremental_remove_top_level_file_matches_full_diff() {
    assert_incremental_matches_full(
        "remove-top",
        |tg| {
            tg.write("pages/Keep.md", "- keep\n");
            tg.write("pages/Delete.md", "- delete\n");
        },
        |tg| {
            tg.remove("pages/Delete.md");
            rel_paths(tg, &["pages/Delete.md"])
        },
    );
}

#[test]
fn incremental_remove_nested_file_matches_full_diff() {
    assert_incremental_matches_full(
        "remove-nested",
        |tg| {
            tg.write("pages/Keep.md", "- keep\n");
            tg.write("pages/sub/Delete.md", "- delete\n");
        },
        |tg| {
            tg.remove("pages/sub/Delete.md");
            rel_paths(tg, &["pages/sub/Delete.md"])
        },
    );
}

#[test]
fn incremental_rename_within_pages_matches_full_diff() {
    assert_incremental_matches_full(
        "rename-within-pages",
        |tg| tg.write("pages/Old.md", "- renamed\n"),
        |tg| {
            tg.rename("pages/Old.md", "pages/New.md");
            rel_paths(tg, &["pages/Old.md", "pages/New.md"])
        },
    );
}

#[test]
fn incremental_rename_across_tree_matches_full_diff() {
    assert_incremental_matches_full(
        "rename-across-tree",
        |tg| tg.write("pages/JournalMove.md", "- moved\n"),
        |tg| {
            tg.rename("pages/JournalMove.md", "journals/2026_07_10.md");
            rel_paths(tg, &["pages/JournalMove.md", "journals/2026_07_10.md"])
        },
    );
}

#[test]
fn incremental_burst_union_matches_full_diff() {
    assert_incremental_matches_full(
        "burst-union",
        |tg| {
            tg.write("pages/Edit.md", "- edit before\n");
            tg.write("pages/Delete.md", "- delete\n");
            tg.write("pages/Keep.md", "- keep\n");
        },
        |tg| {
            std::thread::sleep(Duration::from_millis(20));
            tg.write("pages/Edit.md", "- edit after\n");
            tg.remove("pages/Delete.md");
            tg.write("pages/Create.md", "- create\n");
            tg.write("pages/sub/Nested.md", "- nested\n");
            rel_paths(
                tg,
                &[
                    "pages/Edit.md",
                    "pages/Delete.md",
                    "pages/Create.md",
                    "pages/sub/Nested.md",
                ],
            )
        },
    );
}

#[test]
fn bulk_threshold_boundary_is_exclusive_on_both_sides() {
    assert!(!burst_escalates(0));
    assert!(!burst_escalates(BULK_CHANGE_THRESHOLD));
    assert!(burst_escalates(BULK_CHANGE_THRESHOLD + 1));
    assert!(!emit_as_bulk(BULK_CHANGE_THRESHOLD));
    assert!(emit_as_bulk(BULK_CHANGE_THRESHOLD + 1));
}

/// `graph-changed-bulk` is a frontend wire contract: the aggregate carries
/// the same per-page change shape the `graph-changed` event carries.
#[test]
fn graph_changed_bulk_wire_shape_is_stable() {
    let wire = serde_json::to_value(GraphChangedBulk {
        changes: vec![GraphChange {
            name: "Page".to_owned(),
            kind: PageKind::Page,
            created: true,
            removed: false,
        }],
    })
    .unwrap();
    let changes = wire.get("changes").and_then(|value| value.as_array());
    let first = changes.and_then(|list| list.first()).expect("one change");
    for key in ["name", "kind", "created", "removed"] {
        assert!(first.get(key).is_some(), "missing bulk change field {key}");
    }
}

#[test]
fn a_drained_batch_above_the_threshold_escalates_to_the_full_branch() {
    // Concord P2 (GH #337 / spec L6): a VCS checkout or first big sync dumps
    // N file events; processing them per-file costs two reads + a parse per
    // path with deliberately no stat shortcut. Above the threshold the batch
    // must take the stat-diff full branch instead.
    let tg = TempGraph::new("burst-escalation");
    tg.write("pages/Seed.md", "- seed\n");
    let graph = Graph::open(&tg.root);
    warm_cache(&graph);
    let mut snap = collect_graph_text_files(&graph).files;

    let mut paths = HashSet::new();
    for index in 0..(BULK_CHANGE_THRESHOLD + 1) {
        let rel = format!("pages/Bulk {index}.md");
        tg.write(&rel, &format!("- bulk {index}\n"));
        paths.insert(tg.path(&rel));
    }

    let (changes, _, used_full, errors) =
        reconcile_pending(&graph, &mut snap, &paths, false, false);
    assert!(
        used_full,
        "a batch of {} paths (> {BULK_CHANGE_THRESHOLD}) must escalate to the full stat-diff branch",
        paths.len()
    );
    assert!(errors.is_empty());
    assert_eq!(changes.len(), BULK_CHANGE_THRESHOLD + 1);
}

#[test]
fn a_drained_batch_at_the_threshold_stays_incremental() {
    // The complement: ordinary bursts (a save, a small sync delta) keep the
    // per-file branch, whose explicit-event semantics deliberately bypass
    // the stat shortcut (see `explicit_event_reconciles_even_when_snapshot_
    // metadata_is_equal`).
    let tg = TempGraph::new("burst-no-escalation");
    tg.write("pages/Seed.md", "- seed\n");
    let graph = Graph::open(&tg.root);
    warm_cache(&graph);
    let mut snap = collect_graph_text_files(&graph).files;

    let mut paths = HashSet::new();
    for index in 0..BULK_CHANGE_THRESHOLD {
        let rel = format!("pages/Bulk {index}.md");
        tg.write(&rel, &format!("- bulk {index}\n"));
        paths.insert(tg.path(&rel));
    }

    let (changes, _, used_full, errors) =
        reconcile_pending(&graph, &mut snap, &paths, false, false);
    assert!(
        !used_full,
        "a batch of exactly {BULK_CHANGE_THRESHOLD} paths must keep the incremental branch"
    );
    assert!(errors.is_empty());
    assert_eq!(changes.len(), BULK_CHANGE_THRESHOLD);
}

/// The correctness invariant behind the escalation: whichever branch a burst
/// takes, the result is the same. Same family as
/// `incremental_burst_union_matches_full_diff`, sized across the threshold.
#[test]
fn incremental_burst_above_threshold_union_matches_full_diff() {
    assert_incremental_matches_full(
        "burst-union-above-threshold",
        |tg| {
            for index in 0..BULK_CHANGE_THRESHOLD {
                tg.write(&format!("pages/Edit {index}.md"), "- before\n");
            }
            tg.write("pages/Delete.md", "- delete\n");
            tg.write("pages/Keep.md", "- keep\n");
        },
        |tg| {
            std::thread::sleep(Duration::from_millis(20));
            let mut rels: Vec<String> = Vec::new();
            for index in 0..BULK_CHANGE_THRESHOLD {
                let rel = format!("pages/Edit {index}.md");
                tg.write(&rel, "- after\n");
                rels.push(rel);
            }
            tg.remove("pages/Delete.md");
            rels.push("pages/Delete.md".to_owned());
            for index in 0..4 {
                let rel = format!("pages/sub/Created {index}.md");
                tg.write(&rel, "- created\n");
                rels.push(rel);
            }
            rels.iter().map(|rel| tg.path(rel)).collect()
        },
    );
}

/// Bulk-change measurement + generous regression gate (Concord P2).
///
/// Ignored: the fixture is a few hundred generated files — deliberately NOT
/// part of the fast unit corpus. Run explicitly:
///   cargo nextest run -p tine --run-ignored ignored-only -E 'test(bulk_reconcile)'
///
/// Measures a checkout-shaped change (many files replaced at once under a
/// running watcher) through both reconcile branches, prints the numbers, and
/// asserts only an order-of-magnitude ceiling — never a tight timing bound.
#[test]
#[ignore = "bulk fixture (hundreds of generated files); run explicitly"]
fn bulk_reconcile_bench_and_gate() {
    const TOTAL: usize = 800;
    const CHANGED: usize = 400;

    let tg = TempGraph::new("bulk-bench");
    for index in 0..TOTAL {
        tg.write(
            &format!("pages/Bulk {index}.md"),
            &format!("- bulk page {index}\n- second line {index}\n"),
        );
    }
    let inc_graph = Graph::open(&tg.root);
    let full_graph = Graph::open(&tg.root);
    warm_cache(&inc_graph);
    warm_cache(&full_graph);
    let mut inc_snap = collect_graph_text_files(&inc_graph).files;
    let mut full_snap = inc_snap.clone();

    // The external revision: a checkout replaces CHANGED files' contents.
    std::thread::sleep(Duration::from_millis(20));
    let mut paths = HashSet::new();
    for index in 0..CHANGED {
        let rel = format!("pages/Bulk {index}.md");
        tg.write(&rel, &format!("- bulk page {index} switched\n"));
        paths.insert(tg.path(&rel));
    }

    let incremental_started = Instant::now();
    let (inc_changes, _, inc_errors) = incremental_reconcile(&inc_graph, &mut inc_snap, &paths);
    let incremental_elapsed = incremental_started.elapsed();

    let full_started = Instant::now();
    let snapshot = collect_graph_text_files(&full_graph);
    let (full_changes, _, full_errors) =
        full_diff_reconcile(&full_graph, &mut full_snap, snapshot.files);
    let full_elapsed = full_started.elapsed();

    assert!(inc_errors.is_empty());
    assert!(full_errors.is_empty());
    assert_eq!(inc_changes.len(), CHANGED);
    assert_eq!(full_changes.len(), CHANGED);
    println!(
        "bulk-reconcile bench: {CHANGED} changed of {TOTAL} files — \
         incremental branch {}ms, full stat-diff branch {}ms",
        incremental_elapsed.as_millis(),
        full_elapsed.as_millis(),
    );

    // Generous gate: the escalated (full) branch reconciling a 400-file
    // change over an 800-file graph measured 95 ms on the 2026-08 dev box
    // (incremental branch: 94 ms — the branches cost the same for genuinely
    // changed files; escalation buys one consistent snapshot and one
    // aggregate emit, not reconcile speed). 10 s ≈ 100× measured: it catches
    // an order-of-magnitude regression (e.g. an accidental whole-graph
    // reparse per changed file) without ever flaking under load.
    let ceiling = Duration::from_secs(10);
    assert!(
        full_elapsed < ceiling,
        "escalated bulk reconcile took {}ms (ceiling {}ms)",
        full_elapsed.as_millis(),
        ceiling.as_millis(),
    );
}

#[test]
fn reconcile_pending_need_full_uses_full_scan_branch() {
    let tg = TempGraph::new("need-full");
    tg.write("pages/Seed.md", "- seed\n");

    let inc_graph = Graph::open(&tg.root);
    let full_graph = Graph::open(&tg.root);
    warm_cache(&inc_graph);
    warm_cache(&full_graph);

    let mut inc_snap = collect_graph_text_files(&inc_graph).files;
    let mut full_snap = inc_snap.clone();

    tg.write(
        "pages/sub/CreatedByDirEvent.md",
        "- created through dir op\n",
    );
    let incomplete_paths = rel_paths(&tg, &["pages/Seed.md"]);
    let (inc_changes, inc_conflicts_dirty, used_full, inc_errors) =
        reconcile_pending(&inc_graph, &mut inc_snap, &incomplete_paths, true, false);
    let fresh = collect_graph_text_files(&inc_graph).files;
    let (full_changes, full_conflicts_dirty, full_errors) =
        full_diff_reconcile(&full_graph, &mut full_snap, fresh.clone());

    assert!(used_full, "need_full must bypass incremental reconcile");
    assert!(inc_errors.is_empty());
    assert!(full_errors.is_empty());
    assert_eq!(inc_snap, fresh);
    assert_eq!(full_snap, fresh);
    assert_eq!(inc_conflicts_dirty, full_conflicts_dirty);
    assert_eq!(sorted_changes(inc_changes), sorted_changes(full_changes));
}

/// DUP-5: every temp shape a Tine writer can rename INTO the live graph
/// must be recognized here, or the rename event that publishes the real
/// page is dropped. The restore shape was invisible until 2026-08-25.
#[test]
fn recognizes_every_tine_temp_shape_that_lands_in_the_live_graph() {
    for recognized in [
        ".Foo.md.1234.7.tmp",
        ".Foo.md.1234.7.new.tmp",
        ".Foo.md.1234.7.projection.tmp",
        ".tine-restore-1234-7.tmp",
    ] {
        assert!(
            is_tine_atomic_page_temp_path(Path::new(recognized)),
            "{recognized} must be recognized as a Tine atomic temp"
        );
    }
    for foreign in [
        ".tine-restore-x.tmp",
        ".tine-restore-12.tmp",
        "tine-restore-1234-7.tmp",
        ".Foo.md.restore.tmp",
        "Foo.md",
    ] {
        assert!(
            !is_tine_atomic_page_temp_path(Path::new(foreign)),
            "{foreign} must NOT read as a Tine atomic temp"
        );
    }
}

/// GH #543, audit R10-13: the watcher reconciles a graph only while it holds
/// that root's storage transition lane, and never waits for it. A restore
/// rewrites the tree under the lane; reconciling those files into the graph
/// the restore is about to retire doubled the work and kept its projection
/// worker busy past the detach bound. Waiting would stall every graph.
#[test]
fn the_watcher_reconciles_a_graph_only_under_its_transition_lane() {
    let runtime = include_str!("runtime.rs");
    let lane = runtime
        .find("transition_lane(&graph.root)")
        .expect("the reconcile loop takes the root's transition lane");
    let reconcile = runtime
        .find("reconcile_pending(\n")
        .or_else(|| runtime.find("reconcile_pending("))
        .expect("the reconcile loop reconciles");
    assert!(lane < reconcile, "the lane is taken before the reconcile");
    let between = &runtime[lane..reconcile];
    assert!(
        between.contains("lane.try_lock()") && !between.contains("lane.lock()"),
        "the watcher must try the lane, never wait on it"
    );
    assert!(
        between.contains("graph.transition_skipped = true;"),
        "a skipped graph must carry a full diff to the next cycle"
    );
}

/// GH #543, audit R11-05: a reopen replaces the window's `Graph`, and the new
/// graph read the files itself. Diffing it against the old graph's stamps
/// re-synced every file a restore rewrote, byte-identical ones included (200
/// of 200 here); against its own baseline there is nothing to sync. The
/// runtime takes that baseline when the slot's graph changes (guard below).
#[test]
fn a_reopened_graph_is_diffed_against_its_own_baseline() {
    let temp = TempGraph::new("r11-reopen-baseline");
    for i in 0..200 {
        temp.write(&format!("pages/p{i}.md"), &format!("- a{i}\n"));
    }
    let old = Graph::open(&temp.root);
    let mut old_snap = collect_graph_text_files(&old).files;
    std::thread::sleep(std::time::Duration::from_millis(20));
    for i in 0..200 {
        // A restore copies the backup in: new file, same bytes, new mtime.
        let path = temp.path(&format!("pages/p{i}.md"));
        let copy = path.with_extension("md.restore");
        std::fs::write(&copy, format!("- a{i}\n")).unwrap();
        std::fs::rename(&copy, &path).unwrap();
    }
    let reopened = Graph::open(&temp.root);
    let (stale, _, _) = full_diff_reconcile(
        &reopened,
        &mut old_snap,
        collect_graph_text_files(&reopened).files,
    );
    assert_eq!(
        stale.len(),
        200,
        "precondition: the old baseline sees every copy"
    );
    let mut own_snap = collect_graph_text_files(&reopened).files;
    let (changes, _, errors) = full_diff_reconcile(
        &reopened,
        &mut own_snap,
        collect_graph_text_files(&reopened).files,
    );
    assert!(
        changes.is_empty() && errors.is_empty(),
        "{changes:?} {errors:?}"
    );
}

/// GH #543, audit R11-05: when the slot hands the watcher a different `Graph`
/// for the same root, the watcher takes that graph's baseline.
#[test]
fn the_watcher_rebaselines_a_replaced_graph() {
    let graph_dir = TempGraph::new("watcher-rebind-replaced-graph");
    graph_dir.write("pages/Anchor.md", "- anchor\n");
    let old = GraphSlot::new(Graph::open(&graph_dir.root), graph_dir.root.clone()).graph();
    let replacement = GraphSlot::new(Graph::open(&graph_dir.root), graph_dir.root.clone()).graph();
    let stale = old.note_graph_text_external_observation();
    let mut watched = WatchedGraph::new(
        Arc::clone(&old),
        graph_dir.root.clone(),
        Some(old.assets_path()),
    );
    watched.baseline = true;
    watched.snap.insert(
        graph_dir.root.join("pages/Anchor.md"),
        FileStamp {
            modified: std::time::SystemTime::UNIX_EPOCH,
            len: 1,
            identity: 1,
            changed: 1,
        },
    );
    watched.last_reconcile_error = Some("the old graph's failure".to_owned());
    watched.retry.failed(Instant::now());
    watched.pending_observation_epoch = Some(stale);
    watched.transition_skipped = true;

    watched.take_on(Arc::clone(&replacement), None);

    assert!(Arc::ptr_eq(&watched.graph, &replacement));
    assert_eq!(watched.root, graph_dir.root);
    assert!(
        !watched.baseline && watched.snap.is_empty() && !watched.transition_skipped,
        "a replaced graph must be diffed against its own baseline, not the old graph's \
         (GH #543, audit R11-05)"
    );
    assert!(
        watched.last_reconcile_error.is_none()
            && watched.retry.remaining(Instant::now()).is_none()
            && watched.pending_observation_epoch.is_none()
            && watched.assets.active_root().is_none(),
        "no part of the old graph's watch outlives its replacement (GH #543, audit R12-03)"
    );
}

/// GH #543, audit R13-02: the asset half of a watch belongs to the asset
/// folder, not to the graph. A reopen of the same root that replaced the
/// graph re-snapshotted the folder over an asset change still queued for that
/// cycle, and the old image stayed on screen.
#[test]
fn a_replaced_graph_keeps_a_queued_asset_change() {
    let graph_dir = TempGraph::new("watcher-rebind-keeps-asset-change");
    graph_dir.write("pages/Anchor.md", "- anchor\n");
    graph_dir.write("assets/a.png", "one");
    let old = GraphSlot::new(Graph::open(&graph_dir.root), graph_dir.root.clone()).graph();
    let replacement = GraphSlot::new(Graph::open(&graph_dir.root), graph_dir.root.clone()).graph();
    let mut watched = WatchedGraph::new(
        Arc::clone(&old),
        graph_dir.root.clone(),
        Some(old.assets_path()),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    graph_dir.write("assets/a.png", "two, longer");
    watched.take_on(Arc::clone(&replacement), Some(replacement.assets_path()));
    assert!(Arc::ptr_eq(&watched.graph, &replacement));
    let exact: HashSet<PathBuf> = [graph_dir.root.join("assets/a.png")].into_iter().collect();
    let changed = reconcile_asset_observation(
        "main",
        &mut watched.assets,
        &exact,
        &HashSet::new(),
        false,
        false,
    );
    assert_eq!(
        changed,
        vec!["a.png".to_owned()],
        "the rebind took a queued asset change into its baseline (GH #543, audit R13-02)"
    );
}

/// The other half of R13-02's rule: a new asset folder, on the same graph or
/// a replaced one, is watched from its own snapshot.
#[test]
fn a_moved_asset_folder_is_watched_afresh() {
    let graph_dir = TempGraph::new("watcher-moved-asset-folder");
    graph_dir.write("pages/Anchor.md", "- anchor\n");
    graph_dir.write("assets/a.png", "one");
    graph_dir.write("elsewhere/b.png", "two");
    let graph = GraphSlot::new(Graph::open(&graph_dir.root), graph_dir.root.clone()).graph();
    let mut watched = WatchedGraph::new(
        Arc::clone(&graph),
        graph_dir.root.clone(),
        Some(graph.assets_path()),
    );
    let elsewhere = graph_dir.root.join("elsewhere");
    watched.take_on(Arc::clone(&graph), Some(elsewhere.clone()));
    assert_eq!(watched.assets.active_root(), Some(&elsewhere));
    watched.take_on(Arc::clone(&graph), None);
    assert!(watched.assets.active_root().is_none());
}

/// R12-03's shape: the same-root arms of the watcher cycle and of frontier
/// routing each took a replaced graph on by hand, and reset different
/// fields. Every same-root hand-over now goes through `WatchedGraph::take_on`,
/// and every new watch through `WatchedGraph::new`. What each half keeps is
/// pinned by the behavioural tests above.
#[test]
fn a_replaced_graph_is_taken_on_in_one_place() {
    let runtime = include_str!("runtime.rs");
    assert_eq!(
        runtime.matches(".take_on(").count(),
        2,
        "both same-root arms hand the slot's graph to take_on"
    );
    assert_eq!(
        runtime.matches("WatchedGraph::new(").count(),
        2,
        "both inserts construct"
    );
    let literals = runtime
        .lines()
        .filter(|line| line.contains("WatchedGraph {"))
        .filter(|line| !line.contains("struct WatchedGraph") && !line.contains("impl WatchedGraph"))
        .count();
    assert_eq!(
        literals, 0,
        "a struct literal names the watch's fields by hand; construct through WatchedGraph::new"
    );
    for assignment in [
        ".graph = ",
        ".snap.clear()",
        ".baseline = false",
        ".assets = ",
    ] {
        assert!(
            !runtime.contains(&format!("current{assignment}")),
            "a same-root arm edits the watch by hand ({assignment}); use WatchedGraph::take_on \
             (GH #543, audits R12-03, R13-02)"
        );
    }
}

/// GH #543, audit R11-04: "who notices changes to a graph root the OS is not
/// watching?" The cycle does: a root outside `watched` is polled like poll
/// mode (a full diff, a poll observation and a configuration check) until its
/// watch succeeds, and the cycle it succeeds diffs it once more. A per-root
/// `watch()` failure used to emit an error and nothing else, so changes to
/// that graph, and to its `config.edn`, stayed unseen.
#[test]
fn an_unwatched_root_is_polled_and_diffed_when_watched() {
    let runtime = include_str!("runtime.rs");
    let rule = &runtime[runtime
        .find("let unwatched =")
        .expect("the per-root unwatched rule")..];
    let rule = &rule[..rule.find(';').unwrap()];
    assert!(
        rule.contains("!inotify")
            && rule.contains("!watched")
            && rule.contains("root.starts_with(dir)"),
        "a root is unwatched when no watched directory holds it"
    );
    let need_full = &runtime[runtime
        .find("let need_full = event_need_full")
        .expect("need_full")..];
    let need_full = &need_full[..need_full.find(';').unwrap()];
    assert!(
        need_full.contains("rewalk"),
        "an unwatched root must diff in full, and a newly watched one once"
    );
    assert!(
        runtime.contains(
            "let rewalk = rewalks_root(initial_cycle, polled, handed_over(&graph.root));"
        ),
        "the cycle asks rewalks_root whether to walk a root in full"
    );
    // GH #543, audits R12-07 and R13-08: the rule itself, by behaviour.
    for (initial_cycle, polled, handed_over, walks) in [
        (true, false, false, false),
        (true, true, false, false),
        (true, false, true, false),
        (true, true, true, false),
        (false, false, false, false),
        (false, true, false, true),
        (false, false, true, true),
        (false, true, true, true),
    ] {
        assert_eq!(
            rewalks_root(initial_cycle, polled, handed_over),
            walks,
            "initial_cycle={initial_cycle} polled={polled} handed_over={handed_over}: an \
             unwatched root diffs in full and a newly watched one once, but a first cycle \
             already walked the root for its baseline (GH #543, audit R12-07)"
        );
    }
    assert!(
        runtime.contains("&owned, need_full, polled)"),
        "an unwatched root publishes a poll observation"
    );
    let recheck = &runtime[runtime
        .find("if unwatched(&graph.root) || handed_over(&graph.root) {")
        .expect("unwatched roots are rechecked")..];
    assert!(
        recheck[..recheck.find('}').unwrap()].contains("config_recheck.insert(label.clone());"),
        "an unwatched root's configuration is checked every cycle"
    );
}
