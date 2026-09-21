//! GH #546 — the backend half of the reporter's mid-edit save-failure toast.
//!
//! Page-header properties authored as the flagless properties-only FIRST BULLET
//! are folded into `pre_block` by the frontend projection (GH #198) — but only
//! while that bullet is still exactly properties. The fold rewrites disk and is
//! never reconciled back into the store, so from that save onward the store
//! holds the header as a root while disk holds it as a preamble. The next
//! keystroke that leaves the bullet transiently NOT properties-only (a second
//! property whose `::` has not been typed yet) therefore ships
//! `pre_block: None` plus a property-bearing outline block — exactly the shape
//! the GH #163 data-preservation firewall refuses.
//!
//! The refusal is correct and must stay: it is what keeps the preamble on disk.
//! What this test pins is the part the user actually sees. The refusal used to
//! carry no typed code, so `direct_save_failure_code` fell through to its
//! `unknown` default — which the frontend retries at 100 ms and 300 ms before
//! the third failure pushes `Couldn't save "…" after 3 tries … (reason code:
//! unknown)` while the user is still typing in the block. It now carries
//! `refused.data_preservation`, which the frontend does not retry and reports
//! once with a way to the draft (GH #535).

use std::path::PathBuf;
use tine_core::model::{direct_save_failure_code, BlockDto};
use tine_core::{ActivationIntent, Graph, PageDto};

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "tine-gh546-{tag}-{}-{}",
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

/// Make `dto` an EDITOR's DTO, the way the frontend does: since GH #254
/// increment 3 a read alone mints no identity, so a save needs an activation.
fn as_editor(graph: &Graph, dto: &mut PageDto) {
    let handle = graph
        .activate_editor(&dto.path, ActivationIntent::Replace, dto.rev.as_deref())
        .expect("the target is inside the graph");
    dto.activation = Some(handle.activation.as_u64());
}

#[test]
fn a_mid_edit_page_header_bullet_is_refused_with_the_typed_data_preservation_code() {
    let root = scratch("mid-edit");
    // What the GH #198 fold has already written on the previous save: the page
    // header lives on disk as a PREAMBLE, with no outline blocks at all.
    std::fs::write(root.join("pages/Note.md"), "alias:: book\n").unwrap();
    let graph = Graph::open(&root);
    graph.warm_cache();
    let mut page = graph.load_by_path("pages/Note.md").unwrap().unwrap();
    let base = page.rev.clone();
    assert_eq!(
        page.pre_block.as_deref(),
        Some("alias:: book"),
        "precondition: disk carries the header as a preamble"
    );

    // The store never learned about that fold, so it still presents the header
    // as the first root with an empty pre_block. One keystroke into a second
    // property — `tag`, whose `::` is not typed yet — the bullet is no longer
    // exactly properties, so the frontend fold no longer applies and the header
    // property ships as outline content.
    page.pre_block = None;
    page.blocks = vec![BlockDto {
        raw: "alias:: book\ntag".into(),
        ..Default::default()
    }];
    as_editor(&graph, &mut page);

    let error = graph
        .save_page(&page, base.as_deref())
        .expect_err("the firewall must refuse a DTO that moves the header into the outline");
    assert_eq!(
        direct_save_failure_code(&error),
        "refused.data_preservation",
        "the refusal is a verdict on the draft's content, so it must carry the \
         no-retry code rather than the retried `unknown` fallthrough in the \
         reporter's screenshot"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("pages/Note.md")).unwrap(),
        "alias:: book\n",
        "refusing must leave the existing preamble byte-identical on disk"
    );

    let _ = std::fs::remove_dir_all(&root);
}
