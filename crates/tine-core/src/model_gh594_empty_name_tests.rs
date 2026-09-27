//! GH #594: a graph whose text names a page that normalizes to the empty key.

use super::gh543_r10::r10_scratch;
use super::*;
use std::time::Duration;

fn builds(files: &[(&str, &str)]) -> Result<(), String> {
    let root = r10_scratch("gh594-empty");
    fs::write(root.join("pages/ordinary.md"), "- ordinary [[Beta]]\n").unwrap();
    for (path, text) in files {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }
    let graph = Graph::open(&root);
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    graph.warm_cache();
    let out = graph
        .wait_for_direct_projection_for_test(Duration::from_secs(10))
        .map(|_| ())
        .map_err(|e| format!("{e:?}"));
    crate::direct_projection::release_projection(&graph);
    out
}

/// Every shape here names a page whose key folds to nothing. Before the fix
/// each one failed the WHOLE graph's index build ("name key and spelling must
/// be non-empty"), three times, then every launch -- the reporters' Linked and
/// Unlinked References never came back (GH #594).
#[test]
fn gh594_a_name_that_folds_to_nothing_does_not_fail_the_index_build() {
    let cases: &[(&str, &str)] = &[
        ("pages/a.md", "- [[ ]]\n"),
        ("pages/a.md", "- [[/]]\n"),
        ("pages/a.md", "- [[//]]\n"),
        ("pages/a.md", "- [[\u{3000}]]\n"),
        ("pages/a.md", "- #/\n"),
        ("pages/a.md", "- #[[ ]]\n"),
        ("pages/a.md", "- #[[/]]\n"),
        ("pages/a.md", "- tags:: /\n"),
        ("pages/a.md", "tags:: a, , b\n\n- x\n"),
        ("pages/a.md", "tags:: /\n\n- x\n"),
        ("pages/a.md", "alias:: /\n\n- x\n"),
        ("pages/a.md", "alias:: a,,b\n\n- x\n"),
        ("pages/a.md", "alias:: [[ ]]\n\n- x\n"),
        ("pages/a.md", "tags:: [[ ]]\n\n- x\n"),
        ("pages/a.md", "tags:: \u{3000}\n\n- x\n"),
        ("pages/a.md", "tags:: a，，b\n\n- x\n"),
        ("pages/a.md", "- \u{3000}:: v\n"),
        ("pages/a.md", "- _:: v\n"),
        ("pages/a.md", "title:: /\n\n- x\n"),
        ("pages/a.md", "title:: \u{3000}\n\n- x\n"),
        ("pages/%2F.md", "- x\n"),
        ("pages/%20.md", "- x\n"),
        ("pages/\u{3000}.md", "- x\n"),
        ("pages/a.md", "- [[a]] [[]]\n"),
        ("pages/a.md", "- #\u{3000}x\n"),
        ("pages/a.md", "- [[[[ ]]]]\n"),
        ("pages/a.md", "- [[ /]]\n"),
        ("pages/a.md", "- [[a/]] [[/a]] [[ / ]]\n"),
        ("pages/a.md", "- tags:: [[/]], #/\n"),
        ("pages/a.md", "- :: v\n"),
        ("pages/a.md", "- id:: \n"),
        ("pages/a.md", "- #\u{200b}\n"),
        ("pages/a.md", "- [[\u{200b}]]\n"),
        ("pages/a.md", "- [[\u{feff}]]\n"),
        ("pages/.md", "- x\n"),
        ("pages/a.md", "title:: \n\n- x\n"),
        (
            "pages/a.md",
            "- ref [[/]] with child\n  - child #/ [[Beta]]\n",
        ),
    ];
    let mut failing = Vec::new();
    for (path, text) in cases {
        if let Err(e) = builds(&[(path, text)]) {
            failing.push(format!("{path} {text:?}: {e}"));
        }
    }
    assert!(
        failing.is_empty(),
        "the index build failed on:\n{}",
        failing.join("\n")
    );
}

/// The class guard: pages of random reference punctuation, blanks (ASCII and
/// ideographic), slashes and property syntax, all in one graph. Any name source
/// -- a ref, tag, alias, property, path ref or page name -- that can hand the
/// index an empty name fails the whole build, and this with it. Deterministic
/// (fixed seeds), so a failure names its seed.
#[test]
fn gh594_random_reference_punctuation_never_fails_the_index_build() {
    const PIECES: &[&str] = &[
        "[[", "]]", "#", "/", " ", "\u{3000}", ",", "，", "::", "tags:: ", "alias:: ", "title:: ",
        "\n- ", "\n  - ", "a", "B", "\u{4e2d}", "\"", "((", "))", "\t",
    ];
    for seed in 0..8_u64 {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut pages = Vec::new();
        for page in 0..60 {
            let mut text = String::from("- ");
            for _ in 0..(next() % 24) {
                text.push_str(PIECES[(next() % PIECES.len() as u64) as usize]);
            }
            text.push('\n');
            pages.push((format!("pages/p{page}.md"), text));
        }
        let files: Vec<(&str, &str)> = pages
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        builds(&files).unwrap_or_else(|error| panic!("seed {seed}: {error}\n{pages:#?}"));
    }
}
