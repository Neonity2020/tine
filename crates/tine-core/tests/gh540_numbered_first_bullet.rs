//! GH #540 (DanTremonti's stuck save) — an empty numbered-list item as the first
//! bullet of a new page.
//!
//! The numbered-list command leaves an empty first bullet whose raw is exactly
//! `logseq.order-list-type:: number`. The save path took that for page-header
//! properties and wrote it as the page's unbulleted preamble, so the file became
//! the 32-byte `logseq.order-list-type:: number\n` the reporter found. Once the
//! user typed the item's text, every later save moved that "header" property
//! back into the outline, the GH #163 firewall refused it as `unknown`, and the
//! page stopped saving for the rest of the session.

use std::path::PathBuf;
use tine_core::model::{BlockDto, PageDto, PageKind};
use tine_core::Graph;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "tine-gh540-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("pages")).unwrap();
    root
}

fn block(id: &str, raw: &str, children: Vec<BlockDto>) -> BlockDto {
    BlockDto {
        id: id.into(),
        raw: raw.into(),
        children,
        ..Default::default()
    }
}

fn new_page(name: &str, blocks: Vec<BlockDto>) -> PageDto {
    PageDto {
        name: name.into(),
        kind: PageKind::Page,
        title: name.into(),
        pre_block: None,
        blocks,
        rev: None,
        format: Default::default(),
        read_only: false,
        path: String::new(),
        activation: None,
        guide: false,
    }
}

#[test]
fn an_empty_numbered_first_bullet_stays_a_list_item_and_the_page_keeps_saving() {
    let root = scratch("numbered");
    let graph = Graph::open(&root);
    graph.warm_cache();
    let name = "South Indian Breakfast/Dosa";

    // The reporter's first save: the numbered item exists but has no text yet.
    let first = graph
        .save_page(
            &new_page(
                name,
                vec![block("b1", "logseq.order-list-type:: number", vec![])],
            ),
            None,
        )
        .expect("the first save of a new page");
    let file = std::fs::read_dir(root.join("pages"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("Dosa"))
        .expect("the page file was created");
    let rel = format!("pages/{}", file.file_name().unwrap().to_string_lossy());
    let loaded = graph
        .load_by_path(&rel)
        .unwrap()
        .expect("the new page loads");
    assert_eq!(
        loaded.pre_block,
        None,
        "an empty numbered item must not become page properties: {:?}",
        std::fs::read_to_string(&file).unwrap()
    );
    assert_eq!(
        loaded.blocks.first().map(|b| b.raw.as_str()),
        Some("logseq.order-list-type:: number")
    );

    // Then the item's text and a child, exactly as typed.
    let mut next = new_page(
        name,
        vec![block(
            "b1",
            "Dosa\nlogseq.order-list-type:: number",
            vec![block("c1", "crispy", vec![])],
        )],
    );
    next.path = loaded.path.clone();
    next.activation = loaded.activation;
    graph
        .save_page(&next, Some(first.as_str()))
        .expect("typing into the numbered item must keep saving");
    let reread = graph.load_by_path(&rel).unwrap().unwrap();
    assert_eq!(reread.pre_block, None);
    assert_eq!(
        reread
            .blocks
            .first()
            .map(|b| (b.raw.as_str(), b.children.len())),
        Some(("Dosa\nlogseq.order-list-type:: number", 1)),
        "the text and the child reached disk: {:?}",
        std::fs::read_to_string(&file).unwrap()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_properties_only_first_bullet_of_page_properties_still_becomes_the_header() {
    // GH #198 neighbour: `alias::` typed into the first bullet IS the page
    // header, and keeps being written as the unbulleted preamble.
    let root = scratch("alias");
    let graph = Graph::open(&root);
    graph.warm_cache();
    graph
        .save_page(
            &new_page("Book", vec![block("b1", "alias:: novel", vec![])]),
            None,
        )
        .expect("save");
    assert_eq!(
        std::fs::read_to_string(root.join("pages/Book.md")).unwrap(),
        "alias:: novel\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}
