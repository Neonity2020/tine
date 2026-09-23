//! Build the public Guide from Tine's onboarding demo graph as the read-only
//! published app, with Tine's static HTML export retained as its fallback.
//!
//! Scaffolds the demo graph in a temp dir, publishes ALL its pages, and writes a
//! self-contained site into the given output dir (e.g. `website/guide`). The demo
//! pages carry no `public::` markers, so all-pages-public is forced **in memory
//! for this export only** — the shipped onboarding config stays
//! `all-pages-public=false`, so a real user's new graph never silently publishes.
//!
//! `publish_graph` emits asset embeds as `../assets/<file>` (it assumes the site
//! is served from `<graph>/publish` next to `<graph>/assets`). To keep the hosted
//! Guide self-contained under one directory, the emitted HTML is rewritten to
//! `assets/<file>` and the graph's `assets/` is copied in alongside the pages.
//!
//! Three sample journal days (`guide-journals/`) are added for this site only.
//!
//! Usage: cargo run -q -p tine-core --example build-guide-site -- website/guide dist
//! (Re-run after changing the demo templates in src/templates/.)

use std::fs;
use std::path::{Path, PathBuf};

use std::sync::Arc;
use tine_core::onboarding::create_demo_graph;
use tine_core::publish::app_export::PublishedAppBundle;
use tine_core::publish::publish_graph_app;
use tine_core::Graph;

const GUIDE_JOURNALS: [(&str, &str); 3] = [
    (
        "2026_09_21.md",
        include_str!("guide-journals/2026_09_21.md"),
    ),
    (
        "2026_09_22.md",
        include_str!("guide-journals/2026_09_22.md"),
    ),
    (
        "2026_09_23.md",
        include_str!("guide-journals/2026_09_23.md"),
    ),
];

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn collect_app_bundle(root: &Path, dir: &Path, files: &mut Vec<(String, Vec<u8>)>) {
    for entry in fs::read_dir(dir).expect("read frontend bundle") {
        let entry = entry.expect("frontend bundle entry");
        let path = entry.path();
        if entry
            .file_type()
            .expect("frontend bundle entry type")
            .is_dir()
        {
            collect_app_bundle(root, &path, files);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("frontend asset under bundle root")
            .to_string_lossy()
            .replace('\\', "/");
        if PublishedAppBundle::ships(&relative) {
            files.push((relative, fs::read(&path).expect("read frontend asset")));
        }
    }
}

fn app_bundle(root: &Path) -> PublishedAppBundle {
    let mut files = Vec::new();
    collect_app_bundle(root, root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    PublishedAppBundle { files }
}

fn main() {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: build-guide-site <out_dir> <frontend_dist>"),
    );
    let frontend = PathBuf::from(
        std::env::args()
            .nth(2)
            .expect("usage: build-guide-site <out_dir> <frontend_dist>"),
    );

    let tmp = std::env::temp_dir().join("tine-guide-site-build");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).expect("create temp graph dir");

    create_demo_graph(&tmp).expect("scaffold demo graph");
    // A few sample days so the Guide's Journals view is not empty. They live
    // only in this public site, never in a user's demo graph or Guide copy.
    for (name, text) in GUIDE_JOURNALS {
        fs::write(tmp.join("journals").join(name), text).expect("write sample journal");
    }

    let mut graph = Graph::open(&tmp);
    graph.config_mut().all_pages_public = true;
    let projection_dir = tempfile::tempdir().expect("create derived-state directory");
    graph
        .attach_direct_projection(projection_dir.path().join("direct.sqlite"))
        .expect("open main projection");
    graph.warm_cache();
    let outcome = publish_graph_app(
        &graph,
        Arc::new(app_bundle(&frontend)),
        "Tine Guide",
        "Welcome to Tine",
    )
    .expect("publish live demo graph");
    let publish_dir = PathBuf::from(&outcome.path);

    // Fresh output dir = the published pages, with self-contained asset paths.
    let _ = fs::remove_dir_all(&out);
    copy_dir(&publish_dir, &out).expect("copy publish output");
    for entry in fs::read_dir(&out).expect("read out dir") {
        let p = entry.expect("dir entry").path();
        if p.extension().map(|e| e == "html").unwrap_or(false) {
            let html = fs::read_to_string(&p).expect("read html");
            fs::write(&p, html.replace("\"../assets/", "\"assets/")).expect("write html");
        }
    }

    // Copy the demo graph's assets in next to the pages so `assets/<file>` resolves.
    let assets = tmp.join("assets");
    if assets.is_dir() {
        copy_dir(&assets, &out.join("assets")).expect("copy assets");
    }

    let _ = fs::remove_dir_all(&tmp);
    println!(
        "published {} pages as the live Guide -> {}",
        outcome.pages,
        out.display()
    );
}
