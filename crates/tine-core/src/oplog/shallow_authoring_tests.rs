//! Authoring must clone the retained shallow state, not just its recent updates.
use super::*;
use loro::ContainerTrait;

#[test]
fn author_clone_preserves_shallow_predecessor_and_concurrent_tail() {
    let original = LoroDoc::new();
    original.set_peer_id(101).unwrap();
    original.get_text("text").insert(0, "base").unwrap();
    original.get_map("meta").insert("owner", "page-a").unwrap();
    original.commit();
    let floor = original.oplog_frontiers();
    let floor_vv = original.oplog_vv();
    let remote = original.fork();
    remote.set_peer_id(103).unwrap();
    original.get_text("text").insert(4, " local").unwrap();
    original.get_map("meta").insert("checkpoint", 1).unwrap();
    original.commit();
    let compact = LoroDoc::new();
    compact
        .import(
            &original
                .export(ExportMode::shallow_snapshot(&floor))
                .unwrap(),
        )
        .unwrap();
    let before = compact.get_deep_value();
    let authored = clone_doc(&compact, 102).unwrap();
    assert_eq!(authored.get_deep_value(), before);
    assert_eq!(authored.oplog_vv(), compact.oplog_vv());
    assert_eq!(authored.oplog_frontiers(), compact.oplog_frontiers());
    assert_eq!(
        authored.get_text("text").id(),
        compact.get_text("text").id()
    );
    authored.get_text("text").insert(0, "new ").unwrap();
    authored.commit();
    assert_eq!(
        compact.get_deep_value(),
        before,
        "clone must be independent"
    );
    remote.get_text("text").insert(0, "remote ").unwrap();
    remote.commit();
    let remote_update = remote.export(ExportMode::updates(&floor_vv)).unwrap();
    assert!(authored.import(&remote_update).unwrap().pending.is_none());
    let authored_update = authored
        .export(ExportMode::updates(&compact.oplog_vv()))
        .unwrap();
    assert!(original.import(&authored_update).unwrap().pending.is_none());
    assert_eq!(original.get_deep_value(), authored.get_deep_value());
}

#[test]
fn author_clone_of_detached_source_keeps_latest_oplog_as_before() {
    let original = LoroDoc::new();
    original.set_peer_id(201).unwrap();
    original.get_text("text").insert(0, "before").unwrap();
    original.commit();
    let earlier = original.oplog_frontiers();
    original.get_text("text").insert(6, " after").unwrap();
    original.commit();
    let latest = original.oplog_frontiers();
    original.checkout(&earlier).unwrap();
    let cloned = clone_doc(&original, 202).unwrap();
    assert_eq!(cloned.get_text("text").to_string(), "before after");
    assert_eq!(cloned.oplog_frontiers(), latest);
    assert_eq!(original.get_text("text").to_string(), "before");
}

#[test]
fn owned_peer_continues_counters_through_repeated_shallow_reopens() {
    let peer = 301;
    let mut document = LoroDoc::new();
    document.set_peer_id(peer).unwrap();
    document
        .get_text("text")
        .insert(0, "unchanged text")
        .unwrap();
    document.commit();
    let text_id = document.get_text("text").id();
    let mut compact_sizes = Vec::new();
    for change in 0..2048 {
        let previous_counter = document.oplog_vv().get(&peer).copied().unwrap();
        document.get_map("meta").insert("owner", change).unwrap();
        document.commit();
        assert!(document.oplog_vv().get(&peer).copied().unwrap() > previous_counter);
        if change % 32 == 31 {
            // Maintenance itself uses the owned lane; no peer-per-seal growth.
            document
                .get_map("meta")
                .insert("checkpoint", change)
                .unwrap();
            document.commit();
            let vv = document.oplog_vv();
            let bytes = document
                .export(ExportMode::shallow_snapshot(&document.oplog_frontiers()))
                .unwrap();
            compact_sizes.push(bytes.len());
            let reopened = LoroDoc::new();
            assert!(reopened.import(&bytes).unwrap().pending.is_none());
            document = clone_doc(&reopened, peer).unwrap();
            assert_eq!(document.oplog_vv(), vv);
            assert_eq!(document.oplog_vv().len(), 1);
            assert_eq!(document.get_text("text").id(), text_id);
            assert_eq!(document.get_text("text").to_string(), "unchanged text");
        }
    }
    let first = compact_sizes[0];
    let largest = *compact_sizes.iter().max().unwrap();
    assert!(
        largest <= first + 64,
        "fixed live state grew: {compact_sizes:?}"
    );
}
