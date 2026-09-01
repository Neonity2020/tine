//! GH/P-01 regression: the editor's page save and the PDF-highlight writer of
//! the SAME `hls__` page must not deadlock.
//!
//! Before the fix, `save_page` took the graph-global graph-text identity gate
//! and then the per-page lock, while `write_highlights` took the page lock and
//! only reached the gate deep inside the publication helper. Two threads on the
//! same page wedged permanently — and because the saver holds the graph-global
//! gate while it waits, every later graph-text write in the process wedged too.
//!
//! This test fails as a 60-second hang on the unfixed tree in the release
//! profile, and as a `debug_assert` on the owner of the identity gate in the
//! debug profile. It is deliberately a real-thread contention test: the bug is
//! an acquisition-order inversion, so nothing short of two threads observes it.
use tine_core::pdf::{Highlight, Position, Rect};
use tine_core::{Graph, PageKind};

fn highlight(id: &str) -> Highlight {
    let rect = Rect {
        top: 0.0,
        left: 0.0,
        width: 1.0,
        height: 1.0,
        source_width: None,
        source_height: None,
    };
    Highlight {
        id: id.into(),
        page: 1,
        position: Position {
            page: 1,
            bounding: rect.clone(),
            rects: vec![rect],
        },
        color: "yellow".into(),
        text: Some(id.into()),
        image: None,
    }
}

#[test]
fn saving_and_annotating_the_same_hls_page_never_deadlocks() {
    let root = std::env::temp_dir().join(format!("tine-gate-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::create_dir_all(root.join("journals")).unwrap();
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("assets").join("paper.pdf"), b"%PDF-1.4\n").unwrap();

    let graph = std::sync::Arc::new(Graph::open(&root));
    graph.warm_cache();
    // Establish the page so both writers contend for the same existing path.
    graph
        .write_highlights("paper.pdf", "Paper", &[highlight("H0")], &[])
        .unwrap();
    let page_name = tine_core::pdf::hls_page_name(&tine_core::pdf::asset_key("paper.pdf"));
    assert!(
        graph
            .load_named(&page_name, PageKind::Page)
            .unwrap()
            .is_some(),
        "the regression needs the hls page to exist: {page_name}"
    );

    const ROUNDS: usize = 120;
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let saver = {
        let graph = std::sync::Arc::clone(&graph);
        let page_name = page_name.clone();
        let finished = std::sync::Arc::clone(&finished);
        std::thread::spawn(move || {
            for round in 0..ROUNDS {
                let Some(mut page) = graph.load_named(&page_name, PageKind::Page).unwrap() else {
                    continue;
                };
                if let Some(block) = page.blocks.first_mut() {
                    block.raw = format!("{} edit{round}", block.raw.trim_end());
                }
                // Outcome is not the subject: a conflicting concurrent write may
                // legitimately be refused. Only liveness is asserted.
                let _ = graph.save_page(&page, page.rev.as_deref());
            }
            finished.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    };
    let annotator = {
        let graph = std::sync::Arc::clone(&graph);
        let finished = std::sync::Arc::clone(&finished);
        std::thread::spawn(move || {
            for round in 0..ROUNDS {
                let _ = graph.write_highlights(
                    "paper.pdf",
                    "Paper",
                    &[highlight(&format!("H{round}"))],
                    &[],
                );
            }
            finished.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while finished.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "deadlock: the page save and the highlight write acquire the graph-text \
             identity gate and the page lock in opposite orders ({} of 2 writers finished)",
            finished.load(std::sync::atomic::Ordering::SeqCst)
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    saver.join().unwrap();
    annotator.join().unwrap();

    // The gate must be free afterwards: a third writer proves the process is
    // not wedged, which is the user-visible harm the deadlock caused.
    let mut page = graph
        .load_named(&page_name, PageKind::Page)
        .unwrap()
        .expect("hls page survives the contention");
    if let Some(block) = page.blocks.first_mut() {
        block.raw = format!("{} final", block.raw.trim_end());
    }
    graph
        .save_page(&page, page.rev.as_deref())
        .expect("graph-text writes still work after the contention");

    let _ = std::fs::remove_dir_all(&root);
}
