//! Alias and physical-resource admission on the Direct save path.
//!
//! Cut out of `model_tests.rs` when that file crossed the 16,000-line test
//! cap (budget B1). These four tests are one behaviour family: what Tine does
//! when two names can reach one page — a hard link inside or outside
//! graph-text scope, and a portable (case/NFC) alias of a leaf or of an
//! ancestor directory. They stay a CHILD module of `model_tests` so they keep
//! its fixtures (`scratch`, `direct_save_bench_new_page`) instead of growing a
//! second copy of them.

use super::*;

#[cfg(unix)]
#[test]
fn unrelated_creation_does_not_mutate_existing_hardlinks() {
    let dir = scratch("creation-hardlink-refusal");
    fs::create_dir_all(dir.join("arbitrary")).unwrap();
    let incumbent = dir.join("pages/Owner.md");
    let alias = dir.join("arbitrary/Alias.md");
    fs::write(&incumbent, b"- incumbent\n").unwrap();
    fs::hard_link(&incumbent, &alias).unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();
    let target = dir.join("pages/Fresh Hardlink Check.md");
    graph
        .save_page(&direct_save_bench_new_page("Fresh Hardlink Check"), None)
        .expect("an unrelated no-replace creation need not rewrite existing aliases");
    assert_eq!(fs::read(&incumbent).unwrap(), b"- incumbent\n");
    assert_eq!(fs::read(&alias).unwrap(), b"- incumbent\n");
    assert!(target.exists());
    let _ = fs::remove_dir_all(&dir);
}

/// GH #571 and GH #555. A second link to a page is only Tine's business when
/// the other name is itself a graph-text path. git-annex in `annex.thin` mode
/// links every page to `.git/annex/objects/...`, which graph-text scope never
/// descends into, and a user may keep a link to a page outside the graph
/// entirely. Refusing on the raw link count made every save on such a graph
/// fail with `precheck.resource_alias`, which is the whole of #571 and the
/// reason Tine was unusable on an annexed graph (#555).
#[cfg(unix)]
#[test]
fn a_hard_link_outside_graph_text_scope_does_not_block_a_save() {
    use std::os::unix::fs::MetadataExt as _;

    for placement in ["annex", "external"] {
        let dir = scratch(&format!("save-hardlink-{placement}"));
        let target = dir.join("pages/Target.md");
        fs::write(&target, "- before\n").unwrap();
        let outside =
            (placement == "external").then(|| scratch(&format!("save-hardlink-{placement}-out")));
        let alias = match &outside {
            Some(root) => root.join("Target.md"),
            None => {
                let objects = dir.join(".git/annex/objects/f4/Target.md");
                fs::create_dir_all(objects.parent().unwrap()).unwrap();
                objects
            }
        };
        fs::hard_link(&target, &alias).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().nlink(), 2, "{placement}");

        let graph = Graph::open(&dir);
        graph.warm_cache();
        let mut page = graph.load_by_path("pages/Target.md").unwrap().unwrap();
        page.blocks[0].raw = "after".to_owned();

        graph
            .save_page(&page, page.rev.as_deref())
            .unwrap_or_else(|error| panic!("{placement}: {error}"));

        assert_eq!(fs::read_to_string(&target).unwrap(), "- after\n");
        // Publication is temp + rename, so the out-of-scope name keeps the
        // bytes it had. That is what every editor writing this way does; it is
        // not something Tine can or should prevent by refusing to save.
        assert_eq!(fs::read_to_string(&alias).unwrap(), "- before\n");
        let _ = fs::remove_dir_all(&dir);
        if let Some(root) = outside {
            let _ = fs::remove_dir_all(root);
        }
    }
}

/// GH #571, and the half of the old rule Martin dropped on 2026-09-21: two
/// GRAPH-TEXT paths on one inode no longer block an ordinary resave. Naming
/// the sibling requires the complete identity index, and an existing save must
/// never build one (GH #267) — that is what keeps a save O(1) rather than
/// O(graph) — so the only local stand-in was the raw link count, which cannot
/// tell a graph sibling from git-annex's `.git/annex/objects` link and so made
/// annexed graphs unsaveable. The precise refusal still fires where the index
/// is already in hand: the `precheck.resource_alias` arm of
/// `validate_current_graph_text_collision_strict`, on page creation.
///
/// The accepted consequence is asserted here rather than left implicit:
/// publication is temp + no-clobber rename, so the other page keeps the bytes
/// it had instead of following the edit.
#[cfg(unix)]
#[test]
fn an_in_graph_alias_no_longer_blocks_a_resave() {
    let dir = scratch("save-in-graph-alias");
    let target = dir.join("pages/Target.md");
    let alias = dir.join("pages/Alias.md");
    fs::write(&target, "- before\n").unwrap();
    fs::hard_link(&target, &alias).unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let mut page = graph.load_by_path("pages/Target.md").unwrap().unwrap();
    page.blocks[0].raw = "after".to_owned();

    graph
        .save_page(&page, page.rev.as_deref())
        .expect("an in-graph alias no longer refuses the save");

    assert_eq!(fs::read_to_string(&target).unwrap(), "- after\n");
    assert_eq!(
        fs::read_to_string(&alias).unwrap(),
        "- before\n",
        "temp + rename publishes a new inode, so the alias keeps its bytes"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn creation_refuses_portable_leaf_and_ancestor_aliases_without_mutation() {
    for (label, configured, alias) in [
        ("case", "Pages", "pages"),
        ("nfc", "Caf\u{e9}Pages", "Cafe\u{301}Pages"),
    ] {
        let dir = scratch(&format!("creation-portable-ancestor-{label}"));
        fs::create_dir_all(dir.join("logseq")).unwrap();
        fs::write(
            dir.join("logseq/config.edn"),
            format!("{{:pages-directory \"{configured}\"}}\n"),
        )
        .unwrap();
        fs::create_dir_all(dir.join(alias)).unwrap();
        let incumbent = dir.join(alias).join("Incumbent.md");
        fs::write(&incumbent, b"- incumbent\n").unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();
        let target = dir.join(configured).join("Fresh.md");
        let error = graph
            .save_page(&direct_save_bench_new_page("Fresh"), None)
            .unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::AlreadyExists,
            "{label}: {error}"
        );
        assert_eq!(fs::read(&incumbent).unwrap(), b"- incumbent\n");
        assert!(!target.exists());
        assert!(!dir.join(configured).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    for (label, incumbent_name, requested) in [
        ("case", "leaf.md", "Leaf"),
        ("nfc", "Caf\u{e9}.md", "Cafe\u{301}"),
    ] {
        let dir = scratch(&format!("creation-portable-leaf-{label}"));
        let incumbent = dir.join("pages").join(incumbent_name);
        fs::write(&incumbent, b"- incumbent\n").unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();
        let error = graph
            .save_page(&direct_save_bench_new_page(requested), None)
            .unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::AlreadyExists,
            "{label}: {error}"
        );
        assert_eq!(fs::read(&incumbent).unwrap(), b"- incumbent\n");
        let _ = fs::remove_dir_all(&dir);
    }
}

/// GH #571, GH #555: the rename half. A rename MOVES a directory entry; the
/// inode, and so every other name linked to it, keeps exactly the bytes it had.
/// The divergence `MS-REF-GRAPH-TEXT-ALIAS` describes comes from temp + rename
/// PUBLICATION giving one name a new inode, which a move never does, so the
/// raw link-count refusal the move source kept after the save fix defended
/// nothing and made every page on an annexed or deduplicated graph unrenamable.
/// The referrer rewrites the rename also performs are ordinary publications,
/// already allowed on a hard-linked file. Two GRAPH pages sharing one inode
/// stay refused, by the rename's inventory, which can name the sibling.
#[cfg(unix)]
#[test]
fn a_hard_linked_page_can_be_renamed() {
    for placement in ["annex", "external", "in-graph"] {
        let dir = scratch(&format!("rename-hardlink-{placement}"));
        let target = dir.join("pages/Old.md");
        fs::write(&target, "- body\n").unwrap();
        fs::write(dir.join("pages/Referrer.md"), "- see [[Old]]\n").unwrap();
        let outside =
            (placement == "external").then(|| scratch(&format!("rename-hardlink-{placement}-out")));
        let alias = match (placement, &outside) {
            (_, Some(root)) => root.join("Old.md"),
            ("annex", None) => {
                let objects = dir.join(".git/annex/objects/f4/Old.md");
                fs::create_dir_all(objects.parent().unwrap()).unwrap();
                objects
            }
            _ => dir.join("pages/Twin.md"),
        };
        fs::hard_link(&target, &alias).unwrap();
        // The referrer is linked too, so its rewrite is exercised on a
        // multiply-linked file as well.
        let referrer_link = dir.join(format!("referrer-link-{placement}"));
        fs::hard_link(dir.join("pages/Referrer.md"), &referrer_link).unwrap();

        let graph = Graph::open(&dir);
        graph.warm_cache();
        let renamed = graph.rename_page("Old", "New");
        if placement == "in-graph" {
            // Two GRAPH pages on one inode is the one case Martin kept refusing
            // (2026-09-20). The rename's whole-graph inventory names both files,
            // which is the precise check; nothing moves or is rewritten.
            let error = renamed.expect_err("an in-graph twin is refused precisely");
            assert!(error.to_string().contains("pages/Twin.md"), "{error}");
            assert_eq!(fs::read_to_string(&target).unwrap(), "- body\n");
            assert_eq!(
                fs::read_to_string(dir.join("pages/Referrer.md")).unwrap(),
                "- see [[Old]]\n"
            );
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        renamed.unwrap_or_else(|error| panic!("{placement}: {error}"));

        assert!(!target.exists(), "{placement}: the old name is gone");
        assert_eq!(
            fs::read_to_string(dir.join("pages/New.md")).unwrap(),
            "- body\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("pages/Referrer.md")).unwrap(),
            "- see [[New]]\n",
            "{placement}: the referrer follows the rename"
        );
        assert_eq!(
            fs::read_to_string(&alias).unwrap(),
            "- body\n",
            "{placement}: the other link keeps the bytes it had"
        );
        let _ = fs::remove_dir_all(&dir);
        if let Some(root) = outside {
            let _ = fs::remove_dir_all(root);
        }
    }
}

/// GH #571, GH #555: the external-edit half. On an annexed graph every page
/// has a second link in `.git/annex/objects`, so the watcher's exact feed met
/// a raw link count of 2 on every external edit (git pull, Syncthing, Logseq
/// itself) and threw the warm identity index away; the next create or rename
/// then paid a whole-graph rebuild. The exact feed must treat such a file like
/// any other.
#[cfg(unix)]
#[test]
fn an_external_edit_to_a_hard_linked_page_keeps_the_identity_index_warm() {
    let dir = scratch("exact-feed-hardlink");
    let target = dir.join("pages/Target.md");
    fs::write(&target, "- before\n").unwrap();
    let objects = dir.join(".git/annex/objects/f4/Target.md");
    fs::create_dir_all(objects.parent().unwrap()).unwrap();
    fs::hard_link(&target, &objects).unwrap();
    let graph = Graph::open(&dir);
    guarded_test_prime_identity(&graph);
    let before = graph.guarded_graph_text_identity_report();

    // An in-place external write keeps the inode, and with it the second link.
    fs::write(&target, "- edited outside Tine\n").unwrap();
    graph
        .observe_graph_text_external_paths(std::iter::once(target.as_path()), false)
        .unwrap();

    let after = graph.guarded_graph_text_identity_report();
    assert!(!after.invalidated, "{after:?}");
    assert_eq!(after.complete_builds, before.complete_builds);
    assert_eq!(after.exact_updates, before.exact_updates + 1);
    let _ = fs::remove_dir_all(&dir);
}
