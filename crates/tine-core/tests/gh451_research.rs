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

use std::path::PathBuf;

use tine_core::model::PageKind;
use tine_core::Graph;

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

#[test]
fn gh451_namespace_rename_rebinds_identity_and_survives_restart() {
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

    // The moved page's own identity property follows the file move.
    let content = std::fs::read_to_string(&moved).unwrap();
    assert!(
        content.contains("title:: tine-guide2/Feature Showcase"),
        "renamed title:: missing: {content}"
    );
    assert!(!content.contains("title:: tine-guide/Feature Showcase"));

    // The effective identity, routing and link existence all move together.
    let pages = graph.list_pages();
    assert!(
        pages
            .iter()
            .any(|p| p.name == "tine-guide2/Feature Showcase"),
        "new identity missing: {:?}",
        pages.iter().map(|p| p.name.clone()).collect::<Vec<_>>()
    );
    assert!(!pages
        .iter()
        .any(|p| p.name == "tine-guide/Feature Showcase"));
    assert_eq!(
        graph.existing_page_names(&[NEW_NAME.to_string()]),
        vec![NEW_NAME.to_string()]
    );
    assert!(
        graph
            .load_named(NEW_NAME, PageKind::Page)
            .expect("load_named")
            .is_some(),
        "renamed destination must resolve"
    );

    // Reopen recomputes identity from disk and must preserve the corrected route.
    drop(graph);
    let graph = Graph::open(&root);
    assert!(
        graph
            .load_named(NEW_NAME, PageKind::Page)
            .expect("load_named")
            .is_some(),
        "after restart the renamed page must still resolve"
    );

    drop(graph);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn gh451_real_guide_copy_rename_rebinds_identity() {
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

    let guide_file = root.join("pages").join("tine-guide2%2FFeature Showcase.md");
    assert!(
        guide_file.exists(),
        "file must move to the new name: {}",
        guide_file.display()
    );
    let content = std::fs::read_to_string(&guide_file).unwrap();
    assert!(
        content.contains("title:: tine-guide2/Feature Showcase"),
        "guide title:: must follow rename: {content}"
    );
    let pages = graph.list_pages();
    assert!(
        pages
            .iter()
            .any(|p| p.name == "tine-guide2/Feature Showcase"),
        "guide must use its new effective identity"
    );
    assert!(!pages
        .iter()
        .any(|p| p.name == "tine-guide/Feature showcase"));
    assert!(
        graph
            .load_named("tine-guide2/Feature Showcase", PageKind::Page)
            .expect("load_named")
            .is_some(),
        "renamed guide destination must route to its content"
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
