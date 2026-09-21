//! Live-save conflicts whose one-shot review authority is gone (GH #490),
//! and the research fixture for durable (restart) resolution.
//!
//! Cut out of `model_tests.rs` when that file crossed the 16,000-line test
//! cap (budget B1). A CHILD module of `model_tests`, like
//! `model_alias_admission_tests`, so it keeps that file's fixtures
//! (`scratch`, `gh254_shown`) instead of growing a second copy of them.

use super::*;

/// GH #490: the one-shot authority a live conflict is reviewed through is
/// revoked by any watcher reconcile of the path -- including the echo of the
/// write that caused the conflict -- and, on Windows, by any uncertain delete
/// or rename anywhere in the graph. When that lands between the refusal and
/// the capture, the capture used to fail, the capsule was stored without a
/// disk revision, and after a restart its review failed on every attempt.
#[test]
fn gh490_a_conflict_whose_authority_was_revoked_is_still_captured_and_reviewable() {
    for revoke_everything in [false, true] {
        let root = scratch("gh490-revoked-capture");
        let path = root.join("pages/Note.md");
        fs::write(&path, "- one\n- two\n").unwrap();
        let graph = Graph::open(&root);
        graph.warm_cache();
        let mut page = graph.load_by_path("pages/Note.md").unwrap().unwrap();
        let activation = graph
            .activate_editor(
                "pages/Note.md",
                ActivationIntent::Replace,
                page.rev.as_deref(),
            )
            .unwrap();
        page.activation = Some(activation.activation.as_u64());
        page.blocks[0].raw = "mine one".into();
        fs::write(&path, "- one\n- disk two\n").unwrap();
        let shown = gh254_shown(&graph.save_page(&page, page.rev.as_deref()).unwrap_err());

        // The watcher gets there before the capture does.
        if revoke_everything {
            graph.revoke_all_conflict_authority();
        } else {
            graph.sync_file_checked(&path).unwrap();
        }

        let capture = graph
            .capture_live_save_conflict(&page, page.rev.as_deref(), shown)
            .expect("a revoked authority must not leave the conflict without a capture");
        assert!(
            capture.diff.three_way,
            "the editor's loaded base is still known in this process"
        );
        assert_eq!(capture.disk_rev, content_rev("- one\n- disk two\n"));
        assert!(capture
            .diff
            .rows
            .iter()
            .any(|row| row.kind != crate::sync_diff::RowKind::Unchanged));
        drop(graph);

        // After a restart the capsule reviews and resolves durably.
        let reopened = Graph::open(&root);
        reopened.warm_cache();
        let (diff, authority) = reopened
            .review_live_save_conflict_capsule(
                &page,
                page.rev.as_deref(),
                shown.observation_epoch as i64,
                capture.base_text.as_deref(),
                Some(&capture.disk_rev),
            )
            .unwrap();
        let LiveSaveConflictReviewAuthority::Durable { expected_disk_rev } = authority else {
            panic!("a restored capsule has no live authority: {authority:?}");
        };
        let decisions = diff
            .rows
            .iter()
            .filter(|row| row.kind != crate::sync_diff::RowKind::Unchanged)
            .map(|row| (row.id.clone(), "both".to_owned()))
            .collect::<std::collections::HashMap<_, _>>();
        reopened
            .resolve_durable_live_save_conflict(&page, &expected_disk_rev, &decisions, "union")
            .unwrap();
        let resolved = fs::read_to_string(&path).unwrap();
        assert!(
            resolved.contains("mine one") && resolved.contains("disk two"),
            "{resolved}"
        );
        let _ = fs::remove_dir_all(root);
    }
}

/// GH #490: a capsule that was stored WITHOUT a capture (by a build before the
/// fallback above, or a Managed-era capsule with no live epoch at all) carries
/// only a session-scoped epoch, dead after a restart. Its review must fall back
/// to the disk as it is now instead of failing forever.
#[test]
fn gh490_a_restored_captureless_capsule_is_reviewed_against_the_disk() {
    let root = scratch("gh490-captureless-restore");
    let path = root.join("pages/Note.md");
    fs::write(&path, "- one\n- two\n").unwrap();
    let graph = Graph::open(&root);
    graph.warm_cache();
    let mut page = graph.load_by_path("pages/Note.md").unwrap().unwrap();
    let activation = graph
        .activate_editor(
            "pages/Note.md",
            ActivationIntent::Replace,
            page.rev.as_deref(),
        )
        .unwrap();
    page.activation = Some(activation.activation.as_u64());
    page.blocks[0].raw = "mine one".into();
    fs::write(&path, "- one\n- disk two\n").unwrap();
    let shown = gh254_shown(&graph.save_page(&page, page.rev.as_deref()).unwrap_err());
    drop(graph);

    let reopened = Graph::open(&root);
    reopened.warm_cache();
    // The route the review used to take for such a capsule: dead forever.
    let dead = reopened
        .live_save_conflict_diff(&page, page.rev.as_deref(), shown)
        .unwrap_err();
    assert_eq!(dead.kind(), io::ErrorKind::PermissionDenied);

    for epoch in [shown.observation_epoch as i64, -1] {
        let (diff, authority) = reopened
            .review_live_save_conflict_capsule(&page, page.rev.as_deref(), epoch, None, None)
            .unwrap();
        assert_eq!(
            authority,
            LiveSaveConflictReviewAuthority::Durable {
                expected_disk_rev: content_rev("- one\n- disk two\n")
            }
        );
        assert!(diff
            .rows
            .iter()
            .any(|row| row.kind != crate::sync_diff::RowKind::Unchanged));
    }

    let _ = fs::remove_dir_all(root);
}

/// RESEARCH FIXTURE (2026-09-16, private live-save conflict card; see
/// tine-agents/opencode/reports/2026-09-16-live-save-conflict-research.md).
///
/// Mirrors `scripts/e2e-concord-live-save.mjs`'s quarantined Direct leg at
/// every decisive difference from the green sibling test above: a ONE-block
/// page, external replacements that swap the inode (rename, the way
/// Syncthing/Dropbox and the journey's `atomicReplace` do — twice), and an
/// all-"mine" decision sweep applied through the durable (restart) capsule
/// authority. If Apply resolution fails in the app with
/// `Direct Files could not save (reason code: unknown)`, this fixture is the
/// narrowest layer that can still see the typed error before the command
/// boundary flattens it.
#[test]
fn research_concord_live_save_all_mine_after_rename_restart() {
    let root = scratch("research-live-save-all-mine");
    let path = root.join("pages/B3EKeepDraft.md");
    let staged = root.join("pages/B3EKeepDraft.md.external");
    fs::write(&path, "- common mine base\n").unwrap();
    let graph = Graph::open(&root);
    graph.warm_cache();
    let mut page = graph
        .load_by_path("pages/B3EKeepDraft.md")
        .unwrap()
        .unwrap();
    let activation = graph
        .activate_editor(
            "pages/B3EKeepDraft.md",
            ActivationIntent::Replace,
            page.rev.as_deref(),
        )
        .unwrap();
    page.activation = Some(activation.activation.as_u64());
    page.blocks[0].raw = "direct retained laptop draft".into();
    // The journey's `atomicReplace`: a new inode lands under the same name.
    fs::write(&staged, "- direct current phone body\n").unwrap();
    fs::rename(&staged, &path).unwrap();
    let shown = gh254_shown(&graph.save_page(&page, page.rev.as_deref()).unwrap_err());
    let capture = graph
        .capture_live_save_conflict(&page, page.rev.as_deref(), shown)
        .unwrap();
    drop(graph);

    // The outage write — again a replacement on a fresh inode.
    fs::write(&staged, "- direct newer phone body during outage\n").unwrap();
    fs::rename(&staged, &path).unwrap();

    let reopened = Graph::open(&root);
    reopened.warm_cache();
    let diff = reopened
        .durable_live_save_conflict_diff(&page, capture.base_text.as_deref())
        .unwrap();
    let decisions = diff
        .rows
        .iter()
        .filter(|row| row.kind != crate::sync_diff::RowKind::Unchanged)
        .map(|row| (row.id.clone(), "mine".to_owned()))
        .collect::<std::collections::HashMap<_, _>>();
    let result =
        reopened.resolve_durable_live_save_conflict(&page, &diff.conflict_rev, &decisions, "union");
    match result {
        Ok(_) => {
            let resolved = fs::read_to_string(&path).unwrap();
            assert!(
                resolved.contains("direct retained laptop draft"),
                "keep-mine must write the retained draft: {resolved}"
            );
        }
        Err(error) => panic!(
            "research fixture: durable all-mine resolve failed before the \
             command boundary: code={} kind={:?} display={error:?}",
            direct_save_failure_code(&error),
            error.kind(),
        ),
    }
    let _ = fs::remove_dir_all(root);
}
