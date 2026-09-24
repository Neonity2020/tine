//! GH #597: on a filesystem that ignores case (Windows, default macOS), a page
//! file reached through another spelling of its name is not a second page.
//! These tests check something only where the alias spelling resolves; on a
//! case-sensitive filesystem they have nothing to check.

use super::gh543_r10::{r10_finish, r10_prebuild, r10_scratch, r10_settle, R10Owner};
use super::*;
use std::sync::Arc;
use std::time::Duration;

fn resolves_case_variants(root: &Path) -> bool {
    fs::write(root.join("pages/case-probe.md"), "").unwrap();
    let resolves = fs::symlink_metadata(root.join("pages/CASE-PROBE.md")).is_ok();
    fs::remove_file(root.join("pages/case-probe.md")).unwrap();
    resolves
}

/// A file renamed by case only while Tine was closed (on another device, by a
/// sync tool) leaves one page, under the file's new spelling. The stored row
/// under the old spelling still reads through the filesystem, so the launch
/// check kept it and search showed the page twice.
#[test]
fn gh597_a_case_only_rename_outside_tine_leaves_one_page() {
    let root = r10_scratch("gh597-case-rename");
    if !resolves_case_variants(&root) {
        eprintln!("GH #597: case-sensitive filesystem, nothing to check");
        let _ = fs::remove_dir_all(&root);
        return;
    }
    // Enough pages that the launch check repairs the rename in place: a
    // change to a large share of the graph rebuilds the index whole, which
    // would drop the old spelling's row on its own.
    super::gh543_r10::r10_pages(&root, 40);
    fs::write(root.join("pages/Contents.md"), "- body\n").unwrap();
    fs::write(root.join("pages/other.md"), "- see [[contents]]\n").unwrap();
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    fs::rename(
        root.join("pages/Contents.md"),
        root.join("pages/contents.md"),
    )
    .unwrap();

    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    r10_settle(&graph);
    let pages = graph
        .list_pages()
        .into_iter()
        .filter(|entry| crate::refs::page_key(&entry.name) == "contents")
        .map(|entry| entry.rel_path)
        .collect::<Vec<_>>();
    assert_eq!(pages, vec!["pages/contents.md".to_owned()]);
    r10_finish(root, graph, owner);
}

/// A tab, Recent entry or sidebar item saved with the alias spelling
/// `pages/Contents.md` while Tine handed it out opens the file under its disk
/// spelling instead of failing as "no longer available at that path".
#[test]
fn gh597_a_path_saved_under_another_case_spelling_opens_the_file() {
    let root = r10_scratch("gh597-saved-alias-path");
    if !resolves_case_variants(&root) {
        eprintln!("GH #597: case-sensitive filesystem, nothing to check");
        let _ = fs::remove_dir_all(&root);
        return;
    }
    fs::write(root.join("pages/contents.md"), "- body\n").unwrap();
    let graph = Graph::open(&root);
    let dto = graph
        .load_by_path("pages/Contents.md")
        .unwrap()
        .expect("the saved spelling opens the file");
    assert_eq!(dto.path, "pages/contents.md");
    assert!(graph.load_by_path("pages/Contents-2.md").unwrap().is_none());
    drop(graph);
    let _ = fs::remove_dir_all(&root);
}
