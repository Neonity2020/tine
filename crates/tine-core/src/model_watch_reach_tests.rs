//! GH #543 (audit R9-05/R9-06): how far a watch event on one path reaches --
//! nothing, the file, or the subtree under it -- tracks the same scope the
//! discovery walk uses, and answers a gone path from the identity index.

use super::*;

/// GH #543 (audit R9-06): the old name of a rename no longer exists, so the
/// watcher cannot ask the filesystem whether it was a file or a directory of
/// pages. With a current guarded identity index the answer is exact: a
/// subtree only if the index holds a file under that name. Without one, every
/// gone path stays a subtree, so a removed directory of pages is never
/// mistaken for a file.
#[test]
fn a_gone_path_is_a_subtree_only_if_the_index_holds_a_file_under_it() {
    let dir = scratch("watch-reach-index");
    fs::create_dir_all(dir.join("pages/Folder")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(dir.join("pages/Anchor.md"), b"- a\n").unwrap();
    fs::write(dir.join("pages/Gone.md"), b"- g\n").unwrap();
    fs::write(dir.join("pages/Folder/Inner.md"), b"- i\n").unwrap();
    fs::write(dir.join("pages/picture.png"), b"png").unwrap();
    fs::write(dir.join("logseq/config.edn"), b"{}\n").unwrap();
    let graph = Graph::open(&dir);
    guarded_test_prime_identity(&graph);
    for relative in ["pages/Gone.md", "pages/picture.png"] {
        fs::remove_file(dir.join(relative)).unwrap();
    }
    fs::remove_dir_all(dir.join("pages/Folder")).unwrap();
    fs::remove_file(dir.join("logseq/config.edn")).unwrap();

    let reach = |relative: &str| graph.graph_text_watch_reach(&dir.join(relative));
    assert_eq!(reach("pages/Gone.md"), GraphTextWatchReach::File);
    assert_eq!(reach("pages/Folder"), GraphTextWatchReach::Subtree);
    assert_eq!(reach("pages/picture.png"), GraphTextWatchReach::Nothing);
    assert_eq!(reach("logseq/config.edn"), GraphTextWatchReach::Nothing);
    assert_eq!(reach("pages/Anchor.md"), GraphTextWatchReach::File);
    assert!(!graph.guarded_graph_text_identity_report().invalidated);

    graph
        .observe_graph_text_external_paths(std::iter::empty::<&Path>(), true)
        .unwrap();
    for relative in ["pages/Gone.md", "pages/picture.png", "pages/Folder"] {
        assert_eq!(
            reach(relative),
            GraphTextWatchReach::Subtree,
            "{relative}: without a current index a gone path may have been a directory"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The watcher's routing predicates must admit exactly what discovery
/// admits, plus conflict copies (which are never cached as pages but must
/// still refresh the conflicts panel). GH #268 was the gap between the two.
#[test]
fn watch_predicates_track_the_same_scope_discovery_walks() {
    let dir = scratch("watch-predicate-scope");
    fs::create_dir_all(dir.join("Archive/Deep")).unwrap();
    fs::create_dir_all(dir.join("assets")).unwrap();
    fs::create_dir_all(dir.join(".hidden")).unwrap();
    fs::write(dir.join("pages/Page.md"), b"- p\n").unwrap();
    fs::write(dir.join("top.md"), b"- t\n").unwrap();
    fs::write(dir.join("Archive/Deep/deep.org"), b"* d\n").unwrap();
    let graph = Graph::open(&dir);

    for relative in [
        "pages/Page.md",
        "journals/2026_08_06.md",
        "top.md",
        "Archive/Deep/deep.org",
        // Not present on disk: the predicate is lexical on purpose, so a
        // deletion routes through exactly the same test as a creation.
        "Archive/Gone.md",
        // A Syncthing conflict copy is not eligible text, but its arrival
        // still has to reach the conflicts panel.
        "pages/Page.sync-conflict-20260806-101500-ABCDEFG.md",
    ] {
        assert!(
            graph.graph_text_watch_relevant(&dir.join(relative)),
            "{relative} must be routed to its graph"
        );
    }

    for relative in [
        "assets/image.md",
        ".hidden/skip.md",
        "logseq/bak/old.md",
        "pages/notes.txt",
        "pages",
    ] {
        assert!(
            !graph.graph_text_watch_relevant(&dir.join(relative)),
            "{relative} must not be routed as graph text"
        );
    }
    assert!(
        !graph.graph_text_watch_relevant(Path::new("/elsewhere/pages/Other.md")),
        "a path outside the graph root belongs to another graph, or none"
    );

    // Unclassified paths (directory moves) force a full scan, but only where
    // eligible text could live. An existing file is exactly itself.
    for relative in ["pages/Moved", "Archive"] {
        assert_eq!(
            graph.graph_text_watch_reach(&dir.join(relative)),
            GraphTextWatchReach::Subtree,
            "{relative} could contain graph text"
        );
    }
    assert_eq!(
        graph.graph_text_watch_reach(&dir.join("top.md")),
        GraphTextWatchReach::File
    );
    // Configuration has its own watcher queue; it is not graph text.
    for relative in [
        "assets",
        "assets/pictures",
        ".git/objects",
        "logseq/bak",
        "logseq/config.edn",
    ] {
        assert_eq!(
            graph.graph_text_watch_reach(&dir.join(relative)),
            GraphTextWatchReach::Nothing,
            "{relative} is excluded -- a move there must not rescan the graph"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}
