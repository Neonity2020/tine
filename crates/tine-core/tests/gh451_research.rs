//! GH #451 research fixture — renaming a namespaced page leaves its
//! `title::` property stale, so the page's effective identity does not follow
//! the rename.
//!
//! This is a narrowly scoped RESEARCH fixture for the 2026-09-16 research lane
//! (see tine-agents/opencode/reports/2026-09-16-tine-451-research.md), not a
//! regression test and not a production change:
//!
//! - `gh451_current_state_namespace_rename_sequence` documents TODAY's
//!   behavior end-to-end on this base (it PASSES on the researched base; the
//!   fix should invert or delete it): the file moves and referrers are
//!   rewritten, but the moved file's `title::` keeps the old name, the
//!   effective identity stays the OLD name, the new name routes to nothing
//!   (absent editor → empty page), the first save of the new name meets the
//!   on-disk file as a baseline conflict, and a union "Apply resolution"
//!   merge keeps the stale `title::`, so a reopen repeats the whole loop.
//!
//! - `gh451_renamed_page_identity_must_follow_the_new_name` is the
//!   fail-before proof of the user-visible invariant (it FAILS on this base
//!   and should pass after the fix): after the rename, the page must answer
//!   to its NEW name.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use tine_core::model::direct_save_conflict_epoch;
use tine_core::model::{BlockDto, PageKind};
use tine_core::sync_diff::{DiffRow, RowKind};
use tine_core::{ConflictOverride, EditorActivationHandle, Graph, PageDto};

const OLD_NAME: &str = "tine-guide/Feature Showcase";
const NEW_NAME: &str = "tine-guide2/Feature Showcase";

/// The reporter's exact shape: a guide-copy page whose file carries
/// `title::` bound to its namespaced name (onboarding binds it at creation
/// because the encoded filename is not the name), plus one referrer.
fn gh451_graph(tag: &str) -> (PathBuf, Graph) {
    let root = std::env::temp_dir().join(format!("tine-gh451-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::create_dir_all(root.join("journals")).unwrap();
    let graph = Graph::open(&root);
    // Mirrors onboarding::copy_guide_into_graph → create_markdown_page_if_absent:
    // the file gets `title::` bound to the namespaced page name.
    let created = graph
        .create_markdown_page_if_absent(
            OLD_NAME,
            "title:: tine-guide/Feature Showcase\n\n- showcase body\n",
        )
        .expect("guide-shaped page creation");
    assert!(created, "fixture setup: page must be created, not skipped");
    // A referrer, as the real guide graph has, so the rename also rewrites refs.
    std::fs::write(
        root.join("pages/Other.md"),
        "- see [[tine-guide/Feature Showcase]]\n",
    )
    .unwrap();
    // Reopen so the raw-written referrer joins the inventory like a real graph.
    drop(graph);
    let graph = Graph::open(&root);
    // Setup sanity: before the rename the page answers to its namespaced name.
    assert!(
        graph
            .load_named(OLD_NAME, PageKind::Page)
            .expect("load_named")
            .is_some(),
        "fixture setup: the page must initially be routable by name"
    );
    (root, graph)
}

fn moved_path(root: &std::path::Path) -> PathBuf {
    // Legacy (default) filename encoding of `tine-guide2/Feature Showcase`.
    root.join("pages").join("tine-guide2%2FFeature Showcase.md")
}

fn old_path(root: &std::path::Path) -> PathBuf {
    root.join("pages").join("tine-guide%2FFeature Showcase.md")
}

/// An absent-editor DTO exactly the way the frontend builds one for the empty
/// route (`emptyPage` + `activateAbsentEditor`): no path, no revision, an
/// activation minted against the prospective target.
fn first_edit_dto(graph: &Graph) -> (PageDto, EditorActivationHandle) {
    let handle = graph
        .activate_absent_editor(NEW_NAME, PageKind::Page)
        .expect("absent-editor activation for the renamed destination");
    let dto = PageDto {
        name: NEW_NAME.into(),
        kind: PageKind::Page,
        title: NEW_NAME.into(),
        pre_block: None,
        blocks: vec![BlockDto {
            // A real UUID: the projection refuses a placeholder id.
            id: "4510a2e-6f3a-4b1e-9a11-2c3d4e5f6a7b".into(),
            raw: "first edit on the empty route".into(),
            ..Default::default()
        }],
        rev: None,
        format: Default::default(),
        read_only: false,
        path: String::new(),
        activation: Some(handle.activation.as_u64()),
        guide: false,
    };
    (dto, handle)
}

fn accept_suggestions(rows: &[DiffRow], out: &mut HashMap<String, String>) {
    for row in rows {
        if row.kind != RowKind::Unchanged {
            out.insert(
                row.id.clone(),
                row.suggestion.clone().unwrap_or_else(|| "both".to_owned()),
            );
        }
        accept_suggestions(&row.children, out);
    }
}

#[test]
fn gh451_current_state_namespace_rename_sequence() {
    let (root, graph) = gh451_graph("current");

    // The reporter's rename: only the namespace prefix changes.
    graph
        .rename_page(OLD_NAME, NEW_NAME)
        .expect("rename succeeds");

    // (a) The FILE moved and referrers were rewritten — the rename machinery
    // itself works.
    assert!(!old_path(&root).exists(), "old file must be gone");
    let moved = moved_path(&root);
    assert!(moved.exists(), "file must exist at the new path");
    let referrer = std::fs::read_to_string(root.join("pages/Other.md")).unwrap();
    assert!(
        referrer.contains("[[tine-guide2/Feature Showcase]]"),
        "referrer must be rewritten: {referrer}"
    );

    // (b) CURRENT BEHAVIOR: the moved file's `title::` is STALE — it still
    // names the OLD page.
    let content = std::fs::read_to_string(&moved).unwrap();
    assert!(
        content.contains("title:: tine-guide/Feature Showcase"),
        "current state: stale title:: after rename: {content}"
    );
    assert!(!content.contains("title:: tine-guide2/Feature Showcase"));

    // (c) CURRENT BEHAVIOR: the effective (title::-aware) identity is still
    // the OLD name; no page answers to the new name.
    let pages = graph.list_pages();
    assert!(
        pages
            .iter()
            .any(|p| p.name == "tine-guide/Feature Showcase"),
        "current state: page list still carries the old effective name: {:?}",
        pages.iter().map(|p| p.name.clone()).collect::<Vec<_>>()
    );
    assert!(!pages
        .iter()
        .any(|p| p.name == "tine-guide2/Feature Showcase"));
    assert!(
        graph
            .existing_page_names(&[NEW_NAME.to_string()])
            .is_empty(),
        "current state: the new name is a dead link"
    );

    // (d) CURRENT BEHAVIOR: routing to the new name finds nothing → the app
    // opens an absent editor → the user sees an EMPTY page.
    assert!(
        graph
            .load_named(NEW_NAME, PageKind::Page)
            .expect("load_named")
            .is_none(),
        "current state: the renamed destination does not resolve"
    );

    // (e) CURRENT BEHAVIOR: the first edit on that empty route cannot save —
    // the moved file is on disk under the new name's target path, so the save
    // meets a baseline it never loaded (SaveBaselinePresent) and raises the
    // conflict capsule.
    let (first_edit, _handle) = first_edit_dto(&graph);
    let refusal = graph
        .save_page(&first_edit, None)
        .expect_err("current state: first edit on the empty route must conflict");
    assert_eq!(refusal.kind(), io::ErrorKind::AlreadyExists, "{refusal}");
    let epoch = direct_save_conflict_epoch(&refusal)
        .expect("the refusal must be a banner-class conflict carrying its observation epoch");
    let shown = ConflictOverride {
        observation_epoch: epoch,
    };

    // (f) The user merges ("Apply resolution" with the default union
    // pre-block choice) — this is the app's live-save conflict resolution.
    let diff = graph
        .live_save_conflict_diff(&first_edit, None, shown)
        .expect("live-save conflict diff");
    let mut decisions = HashMap::new();
    accept_suggestions(&diff.rows, &mut decisions);
    let resolved = graph
        .resolve_live_save_conflict(&first_edit, None, shown, &decisions, "union")
        .expect("apply resolution");
    // In-session the resolved DTO carries the new name, so the page LOOKS
    // right ("Tine shows the page with the title changed")…
    assert_eq!(resolved.name, NEW_NAME);

    // (g) CURRENT BEHAVIOR: …but the merged file on disk KEEPS the stale
    // `title::` (union_pre takes the on-disk side's pre-block; an existing
    // file never re-binds its title at save).
    let merged = std::fs::read_to_string(&moved).unwrap();
    assert!(
        merged.contains("title:: tine-guide/Feature Showcase"),
        "current state: the merge must not repair the stale title:: — it does not: {merged}"
    );
    assert!(merged.contains("first edit on the empty route"), "{merged}");

    // (h) CURRENT BEHAVIOR: restart — the merged page still does not answer to
    // its new name, so the loop repeats from (d).
    drop(graph);
    let graph = Graph::open(&root);
    assert!(
        graph
            .load_named(NEW_NAME, PageKind::Page)
            .expect("load_named")
            .is_none(),
        "current state: after restart the merged page still routes empty"
    );
    assert!(
        graph
            .list_pages()
            .iter()
            .any(|p| p.name == "tine-guide/Feature Showcase"),
        "current state: after restart the effective identity is still the old name"
    );

    drop(graph);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn gh451_current_state_real_guide_copy() {
    // Fidelity variant: the reporter's literal first step — "Copy the guide
    // inside your graph (pressing the button to do it)" — through the real
    // onboarding code path, then the same rename.
    let root = std::env::temp_dir().join(format!(
        "tine-gh451-guide-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::create_dir_all(root.join("journals")).unwrap();
    let graph = Graph::open(&root);
    let result = tine_core::onboarding::copy_guide_into_graph(&graph, "Feature showcase")
        .expect("guide copy");
    assert!(result.created, "guide copy must create pages");
    // The bundled showcase page is titled "Feature showcase" (template casing).
    assert!(
        result
            .created_pages
            .iter()
            .any(|name| name == "tine-guide/Feature showcase"),
        "guide copy must include the showcase page: {:?}",
        result.created_pages
    );

    // The reporter's rename (casing of the destination follows the issue).
    graph
        .rename_page(
            "tine-guide/Feature showcase",
            "tine-guide2/Feature Showcase",
        )
        .expect("rename succeeds");

    // CURRENT BEHAVIOR: the moved guide page keeps its stale `title::`, the
    // effective identity stays the old name, and the new name routes nowhere.
    let guide_file = root.join("pages").join("tine-guide2%2FFeature Showcase.md");
    assert!(
        guide_file.exists(),
        "file must move to the new name: {}",
        guide_file.display()
    );
    let content = std::fs::read_to_string(&guide_file).unwrap();
    assert!(
        content.contains("title:: tine-guide/Feature showcase"),
        "current state: stale guide title:: after rename: {content}"
    );
    let pages = graph.list_pages();
    assert!(
        pages
            .iter()
            .any(|p| p.name == "tine-guide/Feature showcase"),
        "current state: effective identity is still the old name"
    );
    assert!(!pages
        .iter()
        .any(|p| p.name == "tine-guide2/Feature Showcase"));
    assert!(
        graph
            .load_named("tine-guide2/Feature Showcase", PageKind::Page)
            .expect("load_named")
            .is_none(),
        "current state: the renamed destination routes to an empty page"
    );

    drop(graph);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn gh451_renamed_page_identity_must_follow_the_new_name() {
    // FAIL-BEFORE proof of the user-visible invariant (GH #451): after
    // renaming a page, the page must answer to its NEW name — the effective
    // (title::-aware) identity moves with the file. Fails on the researched
    // base because `title::` is left stale by the rename transaction.
    let (root, graph) = gh451_graph("invariant");

    graph
        .rename_page(OLD_NAME, NEW_NAME)
        .expect("rename succeeds");

    let pages = graph.list_pages();
    assert!(
        pages
            .iter()
            .any(|p| p.name == "tine-guide2/Feature Showcase"),
        "invariant: the renamed page must be listed under its NEW name; got {:?}",
        pages.iter().map(|p| p.name.clone()).collect::<Vec<_>>()
    );
    assert!(
        !pages
            .iter()
            .any(|p| p.name == "tine-guide/Feature Showcase"),
        "invariant: no page may keep answering to the OLD name"
    );
    assert!(
        graph
            .load_named(NEW_NAME, PageKind::Page)
            .expect("load_named")
            .is_some(),
        "invariant: routing to the new name must load the renamed page"
    );
    assert_eq!(
        graph.existing_page_names(&[NEW_NAME.to_string()]),
        vec![NEW_NAME.to_string()],
        "invariant: links to the new name must resolve"
    );

    drop(graph);
    let _ = std::fs::remove_dir_all(&root);
}
