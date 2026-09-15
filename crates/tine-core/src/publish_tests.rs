use super::*;

fn no_refs() -> RefIndex {
    RefIndex::new()
}

#[test]
fn slugify() {
    assert_eq!(slug("Foo Bar"), "foo-bar");
    assert_eq!(slug("n-fold IP"), "n-fold-ip");
}

/// Render a block body the way `render_block` does: one lsdoc parse → canonical
/// skeleton (`render_html`) → export decoration. The unit the decorator tests drive.
fn render_body(raw: &str, refs: &RefIndex) -> String {
    // Graph-less context: ordinary data macros drop; queries fail explicitly.
    let ctx = Ctx {
        refs,
        reverse_refs: None,
        graph: None,
        slugs: None,
        inline_assets: false,
        print_asset_budget: None,
        query_reader: None,
        print_error: None,
        pages: None,
        page_links: None,
        inert_outside_links: false,
        scope: None,
        recorder: None,
        asset_sink: None,
    };
    decorate(&lsdoc::render_html(&body_blocks(raw), &md_opts()), &ctx, 0)
}
fn search_text(raw: &str) -> String {
    ast_plain_text(&body_blocks(raw))
}

#[test]
fn inline_html() {
    let h = render_body("see [[Foo Bar]] and **bold** and `x` and #tag", &no_refs());
    // page ref → `.html` link, brackets dropped (the decorator); #tag → page link.
    assert!(
        h.contains("<a class=\"ref\" href=\"foo-bar.html\">Foo Bar</a>"),
        "{h}"
    );
    assert!(h.contains("<strong>bold</strong>"), "{h}");
    assert!(h.contains("class=\"inline-code\">x</code>"), "{h}");
    assert!(
        h.contains("<a class=\"tag\" href=\"tag.html\">#tag</a>"),
        "{h}"
    );
}

#[test]
fn escapes_html() {
    // lsdoc owns text escaping; the decorator never un-escapes body text.
    assert!(render_body("a < b & c", &no_refs()).contains("a &lt; b &amp; c"));
}

#[test]
fn raw_html_is_sanitized_live() {
    // Raw inline/block HTML now renders LIVE in the export, through the shared
    // sanitizer — allowlisted tags survive, handlers/scripts are stripped.
    let ok = render_body("press <kbd>Ctrl</kbd> and <ins>added</ins>", &no_refs());
    assert!(ok.contains("<kbd>Ctrl</kbd>"), "{ok}");
    assert!(ok.contains("<ins>added</ins>"), "{ok}");
    // NB: mldoc only classifies a SELF-CLOSED `<img/>` as raw HTML; a bare
    // `<img>` is Plain in mldoc/OG too (parity, stays literal). Sanitizer strips
    // the handler, keeps the src.
    let bad = render_body(
        r#"<img src="https://e.com/a.png" onerror="steal()"/>"#,
        &no_refs(),
    );
    assert!(bad.contains("https://e.com/a.png"), "{bad}");
    assert!(!bad.contains("onerror"), "{bad}");
    // A paired <script> IS raw HTML to mldoc; the sanitizer drops it.
    assert!(!render_body("<script>steal()</script>", &no_refs()).contains("steal()"));
}

#[test]
fn math_decorates_katex_delimiters() {
    // The decorator turns lsdoc's `data-tex` hook into KaTeX `\(..\)` / `\[..\]`.
    let h = render_body(r"Euler $e^{i\pi}+1=0$ and $$\int_0^1 x\,dx$$", &no_refs());
    assert!(
        h.contains(r#"<span class="math">\(e^{i\pi}+1=0\)</span>"#),
        "{h}"
    );
    assert!(
        h.contains(r#"<span class="math math-display">\[\int_0^1 x\,dx\]</span>"#),
        "{h}"
    );
    // Underscores inside math must NOT become italics.
    assert!(!render_body(r"$a_1 + b_2$", &no_refs()).contains("<em>"));
}

#[test]
fn block_refs_resolve_via_decoration() {
    let mut refs = RefIndex::new();
    refs.insert(
        "5cfb2cc4-2f18-4b6e-b4c0-dcf657179204".into(),
        RefTarget {
            slug: "related-work".into(),
            text: "Related Work section".into(),
        },
    );
    // Labeled block ref → a link to the target block's anchor, showing the label.
    let h = render_body(
        "see [Related Work](((5cfb2cc4-2f18-4b6e-b4c0-dcf657179204)))",
        &refs,
    );
    assert!(
            h.contains(r#"<a class="ref block-ref" href="related-work.html#5cfb2cc4-2f18-4b6e-b4c0-dcf657179204">Related Work</a>"#),
            "{h}"
        );
    // Bare block ref → the target's text, linked.
    let b = render_body("((5cfb2cc4-2f18-4b6e-b4c0-dcf657179204))", &refs);
    assert!(
        b.contains("related-work.html#5cfb2cc4-2f18-4b6e-b4c0-dcf657179204"),
        "{b}"
    );
    assert!(b.contains("Related Work section"), "{b}");
    // Unresolved ref → muted text, no broken link / no stray `))`.
    let u = render_body("[X](((deadbeef-0000-0000-0000-000000000000)))", &refs);
    assert!(u.contains(r#"<span class="block-ref">X</span>"#), "{u}");
    assert!(!u.contains("((deadbeef"), "{u}");
    // A real URL with parentheses is captured whole (no truncation at first ')').
    let w = render_body(
        "[wiki](https://en.wikipedia.org/wiki/Foo_(bar))",
        &no_refs(),
    );
    assert!(
        w.contains(r#"href="https://en.wikipedia.org/wiki/Foo_(bar)""#),
        "{w}"
    );
}

#[test]
fn ordinary_links_and_video_macros_use_the_closed_url_policy() {
    let js = render_body("[click](javascript:alert(1))", &no_refs());
    assert!(!js.contains("href="), "{js}");
    assert!(js.contains("unsafe-link"), "{js}");
    let data = render_body("[click](data:text/html,boom)", &no_refs());
    assert!(!data.contains("href="), "{data}");
    let web = render_body("[safe](https://example.com/x)", &no_refs());
    assert!(web.contains("href=\"https://example.com/x\""), "{web}");
    let local = render_body("[safe](../assets/report.pdf)", &no_refs());
    assert!(local.contains("href=\"../assets/report.pdf\""), "{local}");
    assert!(render_video("javascript:alert(1)").contains("unsafe-link"));
    assert!(!render_video("javascript:alert(1)").contains("href="));
}

#[test]
fn user_block_ids_are_contextualized_for_attributes_and_fragments() {
    let id = "bad\" onmouseover=\"alert(1) #/%";
    let mut refs = RefIndex::new();
    refs.insert(
        id.into(),
        RefTarget {
            slug: "safe".into(),
            text: "target".into(),
        },
    );
    let link = decorate(
        &format!(
            "<span class=\"block-ref\" data-block=\"{}\">label</span>",
            esc_attr(id)
        ),
        &Ctx {
            refs: &refs,
            reverse_refs: None,
            graph: None,
            slugs: None,
            inline_assets: false,
            print_asset_budget: None,
            query_reader: None,
            print_error: None,
            pages: None,
            page_links: None,
            inert_outside_links: false,
            scope: None,
            recorder: None,
            asset_sink: None,
        },
        0,
    );
    assert!(
        link.contains("#bad%22%20onmouseover%3D%22alert%281%29%20%23%2F%25"),
        "{link}"
    );
    assert!(!link.contains(" onmouseover="), "{link}");
}

#[test]
fn decorates_image_and_code_block() {
    // image: `data-asset` → src; the inline-image skeleton survives.
    let h = render_body("![cat](../assets/cat.png)", &no_refs());
    assert!(
        h.contains(r#"<img class="inline-image" src="../assets/cat.png" alt="cat">"#),
        "{h}"
    );
    // fenced code: data-lang → highlight.js `language-X` class, body escaped (not the
    // old per-line `<div class="b">` that leaked the ``` fences).
    let c = render_body("```rust\nlet x = 1 < 2;\n```", &no_refs());
    assert!(
        c.contains(r#"<pre class="code-block"><code class="hljs language-rust">"#),
        "{c}"
    );
    assert!(c.contains("1 &lt; 2"), "code body escaped: {c}");
    assert!(!c.contains("```"), "no raw fence in output: {c}");
}

#[test]
fn search_text_off_the_ast() {
    // headings, emphasis/code, [[wiki]], [label](url), ![alt](url) → readable text.
    assert_eq!(
        search_text("## Heading **bold** _it_ `c`"),
        "Heading bold it c"
    );
    assert_eq!(
        search_text("see [[Foo Bar]] and [lbl](http://x)"),
        "see Foo Bar and lbl"
    );
    assert_eq!(search_text("img ![cat](cat.png) end"), "img cat end");
    // a bare block ref → dropped from the index (an opaque uuid reads as noise).
    assert_eq!(
        search_text("ref ((5cfb2cc4-2f18-4b6e-b4c0-dcf657179204)) gone"),
        "ref gone"
    );
}

#[test]
fn search_text_drops_props_and_scheduling() {
    // Property / SCHEDULED / DEADLINE lines are chrome, not searchable content.
    let raw = "task **important** [[Page]]\nSCHEDULED: <2026-01-01 Thu>\nid:: 1111\nkey:: val\ncontinued bit";
    assert_eq!(search_text(raw), "task important Page continued bit");
    // structural-only block → empty (won't be indexed)
    assert_eq!(search_text("id:: abc\ncollapsed:: true"), "");
}

#[test]
fn publish_emits_sidebar_search_and_block_anchors() {
    let dir = std::env::temp_dir().join(format!("tine-publish-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:favorites [\"Alpha\"]}\n",
    )
    .unwrap();
    fs::write(
            dir.join("pages").join("Alpha.md"),
            "public:: true\n- # Intro to [[Beta]] and **bold** text\n  id:: 11111111-1111-1111-1111-111111111111\n- a unique searchwidget term\n",
        )
        .unwrap();
    fs::write(
        dir.join("pages").join("Beta.md"),
        "public:: true\n- linking back to [[Alpha]]\n",
    )
    .unwrap();
    // A non-public page must NOT be exported.
    fs::write(dir.join("pages").join("Secret.md"), "- private stuff\n").unwrap();

    let g = Graph::open(&dir);
    let _projection = prepare_publication_graph(&g);
    let (outdir, count) = publish_graph(&g).unwrap();
    assert_eq!(count, 2, "only the two public pages");
    let out = std::path::Path::new(&outdir);

    // assets emitted alongside the pages
    assert!(out.join("fuse.min.js").exists(), "vendored Fuse shipped");
    assert!(out.join("app.js").exists(), "app.js shipped");
    assert!(
        out.join("enhance.js").exists(),
        "script-free enhancement shipped"
    );

    // embedded search data: pages + blocks globals, favorite flag, stripped text
    let sidx = fs::read_to_string(out.join("search-index.js")).unwrap();
    assert!(
        sidx.starts_with("window.__tinePages="),
        "{}",
        &sidx[..60.min(sidx.len())]
    );
    let alpha = fs::read_to_string(out.join("alpha.html")).unwrap();
    assert!(alpha.contains("Content-Security-Policy"), "{alpha}");
    // The shell's own KaTeX stylesheet loads its fonts from the same CDN;
    // without a font-src the browser blocks them (observed in the Stage 2
    // published-app journey) and math falls back to system fonts.
    assert!(
        alpha.contains("font-src 'self' data: https://cdn.jsdelivr.net"),
        "{alpha}"
    );
    assert!(!alpha.contains(" onload="), "{alpha}");
    assert!(sidx.contains("window.__tineBlocks="));
    assert!(
        sidx.contains("\"favorite\":true"),
        "Alpha is a favorite: {sidx}"
    );
    assert!(sidx.contains("searchwidget"), "block content indexed");
    assert!(
        sidx.contains("Intro to Beta and bold text"),
        "markup stripped in index: {sidx}"
    );
    assert!(!sidx.contains("[[Beta]]"), "no raw wiki brackets in index");

    // page html: EVERY block carries an anchor (id:: uuid or generated b{n}); the
    // sidebar + scripts are present; no anchorless <li>.
    let alpha = fs::read_to_string(out.join("alpha.html")).unwrap();
    assert!(
        alpha.contains("id=\"11111111-1111-1111-1111-111111111111\""),
        "id:: anchor kept"
    );
    assert!(
        alpha.contains("id=\"b0\""),
        "id-less block got a generated anchor: {alpha}"
    );
    assert!(!alpha.contains("<li>"), "no anchorless <li>");
    assert!(
        alpha.contains("<aside class=\"sidebar\">"),
        "sidebar present"
    );
    assert!(alpha.contains("id=\"tine-search\""), "search box present");
    assert!(alpha.contains("src=\"app.js\""), "app.js linked");

    // index lists public pages, excludes the private one, and uses the shell.
    let index = fs::read_to_string(out.join("index.html")).unwrap();
    assert!(index.contains("alpha.html") && index.contains("beta.html"));
    assert!(!index.contains("secret.html"), "private page excluded");
    assert!(
        index.contains("<aside class=\"sidebar\">"),
        "index uses the sidebar shell"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_fails_closed_when_public_and_private_files_claim_one_page_identity() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tine-publish-ambiguous-{unique}"));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages/Twin.md"),
        "public:: true\n- public sentinel\n- {{query (page Twin)}}\n",
    )
    .unwrap();
    fs::write(dir.join("journals/Twin.md"), "- PRIVATE SENTINEL\n").unwrap();
    fs::write(dir.join("pages/Visible.md"), "public:: true\n- visible\n").unwrap();

    let graph = Graph::open(&dir);
    let (outdir, count) = publish_graph_with_main_reader(&graph).unwrap();
    let out = Path::new(&outdir);
    assert_eq!(count, 1);
    assert!(!out.join("twin.html").exists());
    let all = fs::read_to_string(out.join("search-index.js")).unwrap();
    assert!(!all.contains("PRIVATE SENTINEL"), "{all}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_macros_never_expand_private_graph_content() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-private-macros-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    let private_id = "99999999-9999-4999-8999-999999999999";
    fs::write(
            dir.join("pages/Dashboard.md"),
            format!(
                "public:: true\n- {{{{query (task TODO)}}}}\n- {{{{embed [[Secret]]}}}}\n- {{{{embed (({private_id}))}}}}\n- {{{{namespace PrivateNS}}}}\n"
            ),
        )
        .unwrap();
    fs::write(
        dir.join("pages/Secret.md"),
        format!("- TODO PRIVATE_QUERY_AND_EMBED_TOKEN\n  id:: {private_id}\n"),
    )
    .unwrap();
    fs::write(
        dir.join("pages/PrivateNS___Child.md"),
        "- PRIVATE_NAMESPACE_TOKEN\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/MalformedPrivate.md"),
        "public::true\n- PRIVATE_MALFORMED_PROPERTY_TOKEN\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/SubVisible.md"),
        "\x1apublic:: true\n- SUB_VISIBLE_PROPERTY_TOKEN\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let (outdir, count) = publish_graph(&graph).unwrap();
    assert_eq!(count, 2);
    let dashboard =
        fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();
    assert!(!std::path::Path::new(&outdir)
        .join("malformedprivate.html")
        .exists());
    assert!(std::path::Path::new(&outdir)
        .join("subvisible.html")
        .exists());
    assert!(!dashboard.contains("PRIVATE_QUERY_AND_EMBED_TOKEN"));
    assert!(!dashboard.contains("PRIVATE_NAMESPACE_TOKEN"));
    assert!(!dashboard.contains("PrivateNS/Child"));
    assert!(
        dashboard.contains("No matching blocks")
            || dashboard.contains("Embedded content is not public"),
        "private macro targets should fail closed: {dashboard}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The publish module's production source: publish.rs, then every
/// `publish/*.rs`, as raw text (comments included). A source guard that reads
/// publish.rs alone goes blind when code moves into a child module, so the
/// guards below read the whole module instead (I-11; the K3 lesson).
fn publish_module_production() -> String {
    crate::projection_producer_census::production_rust()
        .iter()
        .filter(|file| {
            file.relative == "crates/tine-core/src/publish.rs"
                || file.relative.starts_with("crates/tine-core/src/publish/")
        })
        .map(|file| file.raw.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn static_query_selection_has_only_the_supplied_main_reader() {
    let production = publish_module_production();
    for retired in [
        "attach_snapshot_query_projection",
        "index_queries(",
        "run_query_bounded(",
        "run_advanced_query_bounded(",
        "run_query_result_ir(",
        "query_cache",
        "query-index.sqlite",
    ] {
        assert!(
            !production.contains(retired),
            "retired publication path: {retired}"
        );
    }
    assert!(production.contains("with_publication_query_reader"));
    let compact = production.split_whitespace().collect::<String>();
    let checked = compact.find("queries.ensure_current()").unwrap();
    let commit = compact
        .find("commit_publish_stage(graph,stage,&target.output)")
        .unwrap();
    assert!(
        checked < commit,
        "cancellation must be checked before output commit"
    );
}

#[test]
fn query_hydration_has_one_shared_source_owner() {
    let production = publish_module_production();
    assert_eq!(
            production.matches("fn with_hydrated_query_groups(").count(),
            1,
            "I-12: query hydration must have exactly one implementation; imitate with_hydrated_query_groups in crates/tine-core/src/publish.rs, the shared owner"
        );
    assert_eq!(
        production.matches("with_hydrated_query_groups(").count(),
        3,
        "one definition plus the flat-query and sheet callers"
    );
    assert_eq!(
            production.matches("let page_by_key =").count(),
            1,
            "I-12: the page-key lookup belongs to the one hydration owner (publish.rs with_hydrated_query_groups)"
        );
    assert_eq!(
            production
                .matches("collect_wanted_doc_blocks(&doc.roots")
                .count(),
            1,
            "I-12: wanted-block collection belongs to the one hydration owner (publish.rs with_hydrated_query_groups)"
        );
    assert_eq!(
        production
            .matches("Never fall back to cached DTO bytes.")
            .count(),
        1,
        "the publication-capability rationale belongs to the shared boundary"
    );
}

#[test]
fn publish_uses_one_fresh_snapshot_after_external_visibility_rewrite() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-external-visibility-snapshot-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    let source_id = "99999999-9999-4999-8999-999999999998";
    fs::write(
            dir.join("pages/Dashboard.md"),
            format!(
                "public:: true\n- {{{{query (task TODO)}}}}\n- {{{{query [:find (pull ?b [*]) :where (task ?b \"TODO\")]}}}}\n- {{{{embed (({source_id}))}}}}\n"
            ),
        )
        .unwrap();
    fs::write(
        dir.join("pages/Source.md"),
        format!("- TODO PRIVATE_STALE_TOKEN\n  id:: {source_id}\n"),
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);

    // Simulate a sync/editor outside Tine changing both visibility and body
    // after the live graph cache was populated. Publication must not combine
    // the fresh public classification with stale cached query/embed DTOs.
    fs::write(
        dir.join("pages/Source.md"),
        format!("public:: true\n- harmless current body\n  id:: {source_id}\n"),
    )
    .unwrap();

    assert_eq!(
        publish_graph(&graph).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    graph
        .sync_file_checked(&dir.join("pages/Source.md"))
        .unwrap();
    graph
        .wait_for_direct_projection_for_test(std::time::Duration::from_secs(30))
        .unwrap();
    let (outdir, count) = publish_graph(&graph).unwrap();
    assert_eq!(count, 2);
    let out = Path::new(&outdir);
    let dashboard = fs::read_to_string(out.join("dashboard.html")).unwrap();
    let source = fs::read_to_string(out.join("source.html")).unwrap();
    let search = fs::read_to_string(out.join("search-index.js")).unwrap();
    let published = format!("{dashboard}\n{source}\n{search}");
    assert!(published.contains("harmless current body"), "{published}");
    assert!(
        !published.contains("PRIVATE_STALE_TOKEN"),
        "a stale live-graph query/embed result crossed the publication snapshot: {published}"
    );
    assert!(
        dashboard.contains("harmless current body"),
        "the block embed must resolve from the same fresh snapshot: {dashboard}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publication_snapshot_uses_live_file_runtime_identity() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-runtime-identity-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(dir.join("pages/Target.md"), "- target\n").unwrap();
    fs::write(dir.join("pages/Source.md"), "- [[Target]] from source\n").unwrap();

    let graph = Graph::open(&dir);
    let live_id = graph.backlinks("Target")[0].blocks[0].id.clone();
    let snapshot = PublicationGraphSnapshot::new(capture_snapshot_pages(&graph)).unwrap();
    assert_eq!(snapshot.graph.backlinks("Target")[0].blocks[0].id, live_id);

    drop(snapshot);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn republish_retires_pages_that_are_no_longer_public() {
    let dir =
        std::env::temp_dir().join(format!("tine-publish-retire-stale-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages/Visible.md"),
        "public:: true\n- visible body\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Secret.md"),
        "public:: true\n- stale private token\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let (outdir, count) = publish_graph(&graph).unwrap();
    assert_eq!(count, 2);
    let out = std::path::Path::new(&outdir);
    assert!(out.join("secret.html").exists());

    fs::write(dir.join("pages/Secret.md"), "- stale private token\n").unwrap();
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let (outdir, count) = publish_graph(&graph).unwrap();
    assert_eq!(count, 1);
    let out = std::path::Path::new(&outdir);
    assert!(out.join("visible.html").exists());
    assert!(
        !out.join("secret.html").exists(),
        "a formerly public page must not remain deployable after republish"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn publish_commit_never_writes_through_a_replaced_output_symlink() {
    use std::os::unix::fs::symlink;

    let dir = std::env::temp_dir().join(format!("tine-publish-output-swap-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!(
        "tine-publish-output-swap-outside-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("index.html"), "outside sentinel").unwrap();
    let graph = Graph::open(&dir);
    let stage = reserve_publish_stage(&graph).unwrap();
    write_publish_stage_file(&stage, "index.html", b"generated site").unwrap();
    symlink(&outside, dir.join("publish")).unwrap();

    assert!(commit_publish_stage(&graph, stage, &PublicationTarget::graph_site().output).is_err());

    assert_eq!(
        fs::read_to_string(outside.join("index.html")).unwrap(),
        "outside sentinel"
    );
    assert!(fs::symlink_metadata(dir.join("publish"))
        .unwrap()
        .file_type()
        .is_symlink());
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn publish_stage_handle_survives_ambient_symlink_swap_without_outside_write() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-stage-capability-{}",
        std::process::id()
    ));
    let outside = std::env::temp_dir().join(format!(
        "tine-publish-stage-capability-outside-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        dir.join("pages/Public.md"),
        "public:: true\n- generated sentinel\n",
    )
    .unwrap();
    fs::write(outside.join("style.css"), "outside sentinel").unwrap();
    PUBLISH_STAGE_WRITE_SWAP.with(|slot| *slot.borrow_mut() = Some(outside.clone()));

    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    assert!(publish_graph(&graph).is_err());
    assert_eq!(
        fs::read_to_string(outside.join("style.css")).unwrap(),
        "outside sentinel"
    );
    assert!(!outside.join("public.html").exists());
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn publish_recovery_handle_survives_ambient_symlink_swap_without_outside_move() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-recovery-capability-{}",
        std::process::id()
    ));
    let outside = std::env::temp_dir().join(format!(
        "tine-publish-recovery-capability-outside-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("publish")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(dir.join("publish/index.html"), "previous site").unwrap();
    fs::write(
        dir.join("pages/Public.md"),
        "public:: true\n- generated sentinel\n",
    )
    .unwrap();
    fs::write(outside.join("previous"), "outside sentinel").unwrap();
    PUBLISH_RECOVERY_SWAP.with(|slot| *slot.borrow_mut() = Some(outside.clone()));

    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let (out, count) = publish_graph(&graph).unwrap();
    assert_eq!(count, 1);
    assert!(Path::new(&out).join("public.html").exists());
    assert_eq!(
        fs::read_to_string(outside.join("previous")).unwrap(),
        "outside sentinel"
    );
    let conflicts = dir.join("logseq/.tine-trash/conflicts");
    assert!(fs::read_dir(conflicts)
        .unwrap()
        .flatten()
        .any(|entry| entry.path().join("previous/index.html").exists()));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn publish_uses_welcome_home_and_public_reverse_block_refs() {
    let dir = std::env::temp_dir().join(format!("tine-publish-welcome-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    let id = "11111111-1111-1111-1111-111111111111";
    fs::write(
        dir.join("pages/Welcome to Tine.md"),
        format!("public:: true\n- Welcome target\n  id:: {id}\n- same page (({id}))\n"),
    )
    .unwrap();
    fs::write(
            dir.join("pages/Other.md"),
            format!("public:: true\n- cross page (({id})) and (({id}))\n- missing ((22222222-2222-2222-2222-222222222222))\n"),
        )
        .unwrap();
    fs::write(
        dir.join("pages/Private.md"),
        format!("- private ref (({id}))\n"),
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let (outdir, count) = publish_graph(&graph).unwrap();
    assert_eq!(count, 2);
    let out = std::path::Path::new(&outdir);
    let entry = fs::read_to_string(out.join("index.html")).unwrap();
    let welcome = fs::read_to_string(out.join("welcome-to-tine.html")).unwrap();
    let pages = fs::read_to_string(out.join("pages.html")).unwrap();

    assert!(entry.contains("<h1 class=\"page\">Welcome to Tine</h1>"));
    assert!(entry.contains("href=\"welcome-to-tine.html\">⌂ Home</a>"));
    assert!(welcome.contains("aria-label=\"2 block references\">2</summary>"));
    assert!(welcome.contains("href=\"welcome-to-tine.html#b0\""));
    assert!(welcome.contains("href=\"other.html#b0\""));
    assert!(
        !welcome.contains("Private"),
        "private referrer must not leak"
    );
    assert!(pages.contains("welcome-to-tine.html") && pages.contains("other.html"));
    assert!(pages.contains("href=\"welcome-to-tine.html\">⌂ Home</a>"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_gives_distinct_nonempty_files_on_slug_collision() {
    // DS#4: two titles that collapse to the same ASCII slug ("foo"), plus a
    // title with NO ASCII-alnum chars (empty slug pre-fix). Pre-fix, `Foo!`
    // and `Foo#` both write `foo.html` (the second silently overwrites the
    // first) and `日本語` writes a degenerate `.html`. Post-fix every page must
    // get a distinct, nonempty file, and every cross-page link must point at
    // the file its target was actually written to.
    let dir = std::env::temp_dir().join(format!("tine-publish-collide-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    // Each page links to the next so we can check the link map matches files.
    fs::write(
        dir.join("pages").join("Foo!.md"),
        "- alpha body linking [[Foo#]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Foo#.md"),
        "- bravo body linking [[日本語]]\n",
    )
    .unwrap();
    fs::write(dir.join("pages").join("日本語.md"), "- charlie body\n").unwrap();

    let g = Graph::open(&dir);
    let _projection = prepare_publication_graph(&g);
    let (outdir, count) = publish_graph(&g).unwrap();
    assert_eq!(count, 3, "all three public pages exported");
    let out = std::path::Path::new(&outdir);

    // The embedded page index (`__tinePages`) is the single source of truth
    // name -> slug; parse it back out.
    let sidx = fs::read_to_string(out.join("search-index.js")).unwrap();
    let after = sidx.strip_prefix("window.__tinePages=").unwrap();
    let json = &after[..after.find(";\n").unwrap()];
    let pages: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
    assert_eq!(pages.len(), 3, "three pages in the index");

    // Every slug is nonempty + distinct, and names an existing, nonempty file.
    let mut seen = std::collections::HashSet::new();
    for p in &pages {
        let s = p["slug"].as_str().unwrap();
        assert!(!s.is_empty(), "slug must be nonempty: {p}");
        assert!(seen.insert(s.to_string()), "slugs must be distinct: {s}");
        let f = out.join(format!("{s}.html"));
        assert!(f.exists(), "file for slug {s} exists");
        assert!(
            fs::metadata(&f).unwrap().len() > 0,
            "file {s}.html nonempty"
        );
    }
    // No page landed in a degenerate empty-slug file.
    assert!(!out.join(".html").exists(), "no empty-slug .html file");

    // Cross-page links point at the file each target was actually written to.
    let name_slug = |name: &str| -> String {
        pages
            .iter()
            .find(|p| p["title"] == name)
            .unwrap_or_else(|| panic!("page {name} in index"))["slug"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let foo_bang = name_slug("Foo!");
    let foo_hash = name_slug("Foo#");
    let cjk = name_slug("日本語");
    let bang_html = fs::read_to_string(out.join(format!("{foo_bang}.html"))).unwrap();
    assert!(
        bang_html.contains(&format!("href=\"{foo_hash}.html\"")),
        "Foo! links to Foo#'s real file ({foo_hash}.html): {bang_html}"
    );
    let hash_html = fs::read_to_string(out.join(format!("{foo_hash}.html"))).unwrap();
    assert!(
        hash_html.contains(&format!("href=\"{cjk}.html\"")),
        "Foo# links to 日本語's real file ({cjk}.html): {hash_html}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn base64_matches_known_vectors() {
    // RFC 4648 test vectors + a binary triple that exercises all 6-bit lanes.
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64_encode(&[0xff, 0xef, 0xbf]), "/++/");
}

#[test]
fn print_asset_inlining_enforces_per_file_and_shared_export_budgets() {
    let dir = std::env::temp_dir().join(format!("tine-print-budget-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("assets")).unwrap();
    fs::write(dir.join("assets/one.png"), b"1234").unwrap();
    fs::write(dir.join("assets/two.png"), b"5678").unwrap();
    fs::write(dir.join("assets/large.png"), b"123456").unwrap();
    let graph = Graph::open(&dir);
    let refs = no_refs();
    let cumulative = RefCell::new(PrintAssetBudget {
        per_asset: 5,
        remaining: 7,
    });
    let cumulative_ctx = Ctx {
        refs: &refs,
        reverse_refs: None,
        graph: Some(&graph),
        slugs: None,
        inline_assets: true,
        print_asset_budget: Some(&cumulative),
        query_reader: None,
        print_error: None,
        pages: None,
        page_links: None,
        inert_outside_links: false,
        scope: None,
        recorder: None,
        asset_sink: None,
    };

    assert!(inline_asset_uri(&cumulative_ctx, "../assets/one.png").is_some());
    assert_eq!(cumulative.borrow().remaining, 3);
    assert!(
        inline_asset_uri(&cumulative_ctx, "../assets/two.png").is_none(),
        "the second valid file must not cross the shared export ceiling"
    );
    assert_eq!(
        cumulative.borrow().remaining,
        3,
        "a rejection consumes no budget"
    );

    let per_file = RefCell::new(PrintAssetBudget {
        per_asset: 5,
        remaining: 20,
    });
    let per_file_ctx = Ctx {
        print_asset_budget: Some(&per_file),
        ..cumulative_ctx
    };
    assert!(
        inline_asset_uri(&per_file_ctx, "../assets/large.png").is_none(),
        "one oversized file must be rejected before it is returned"
    );
    assert_eq!(per_file.borrow().remaining, 20);

    let _ = fs::remove_dir_all(&dir);
}

fn print_test_graph(dir: &Path) -> Graph {
    let graph = Graph::open(dir);
    graph.warm_cache();
    graph
        .attach_direct_projection(dir.join("print-test.sqlite"))
        .unwrap();
    let started = std::time::Instant::now();
    while !graph.direct_projection_ready_test() {
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    graph
}

#[test]
fn page_print_html_is_self_contained_with_inlined_image() {
    let dir = std::env::temp_dir().join(format!("tine-print-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("assets")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    // A 1x1 PNG (real bytes) so the inliner produces a valid data: URI.
    let png: [u8; 67] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    fs::write(dir.join("assets").join("pic.png"), png).unwrap();
    let oversized = fs::File::create(dir.join("assets").join("oversized.png")).unwrap();
    oversized.set_len(PRINT_ASSET_MAX_BYTES + 1).unwrap();
    fs::write(
            dir.join("pages").join("Report.md"),
            "- # Report\n- Some **bold** text and a [[Welcome]] link.\n- ![shot](../assets/pic.png)\n- ![large](../assets/oversized.png)\n",
        )
        .unwrap();

    let g = print_test_graph(&dir);
    let html = g
        .page_print_html("Report", PrintOpts::default())
        .unwrap()
        .expect("page exists");
    // Self-contained: inlined stylesheet + inlined image, no sidebar / app scripts /
    // external style.css.
    assert!(html.contains("<style>"), "stylesheet inlined");
    assert!(
        html.contains("data:image/png;base64,"),
        "image inlined as data URI: {}",
        &html[..0]
    );
    assert!(
        !html.contains("../assets/pic.png"),
        "no relative asset link left"
    );
    assert!(
        html.contains("class=\"print-asset-omitted\""),
        "an oversized image becomes an explicit inert marker"
    );
    assert!(
        !html.contains("oversized.png"),
        "the rejected source path is not retained in the print document"
    );
    assert!(
        !html.contains("<aside class=\"sidebar\">"),
        "no sidebar in print doc"
    );
    assert!(!html.contains("src=\"app.js\""), "no app.js in print doc");
    assert!(!html.contains("<script"), "print doc executes no scripts");
    assert!(
        !html.contains("cdn.jsdelivr.net"),
        "print doc has no CDN resources"
    );
    assert!(
        html.contains("script-src 'none'"),
        "print doc denies script execution even if markup regresses"
    );
    assert!(
        !html.contains("href=\"style.css\""),
        "no external stylesheet link"
    );
    // Content actually rendered.
    assert!(
        html.contains("<h1 class=\"page\">Report</h1>"),
        "page heading"
    );
    assert!(
        html.contains("<strong>bold</strong>"),
        "inline markup rendered"
    );
    assert!(html.contains("@media print"), "print CSS present");

    // Missing page → None (not an error).
    assert!(g
        .page_print_html("No Such Page", PrintOpts::default())
        .unwrap()
        .is_none());

    // Collapsed handling: a collapsed parent's children are expanded by default,
    // and hidden when expand_collapsed is off.
    fs::write(
        dir.join("pages").join("Folded.md"),
        "- Parent\n  collapsed:: true\n\t- hidden child text\n",
    )
    .unwrap();
    drop(g);
    let g2 = print_test_graph(&dir);
    let expanded = g2
        .page_print_html("Folded", PrintOpts::default())
        .unwrap()
        .unwrap();
    assert!(
        expanded.contains("hidden child text"),
        "default expands collapsed"
    );
    let folded = g2
        .page_print_html(
            "Folded",
            PrintOpts {
                expand_collapsed: false,
                ..PrintOpts::default()
            },
        )
        .unwrap()
        .unwrap();
    assert!(
        !folded.contains("hidden child text"),
        "folded hides collapsed children"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// **Y1: a packet may not write a macro name its own tree cannot export.**
///
/// P0-ts's printer writes `{{tine-query …}}` whenever a filter is not
/// OG-expressible (Q3). Before this packet, `expand_macro` matched the
/// literal `"query"` and every other name fell through to the muted
/// `macro-raw` literal — so a saved TQL query published as its own source
/// text. That is the exact failure §7.9 names, and this is its test.
#[test]
fn publish_renders_both_query_macro_names_and_never_leaks_the_source() {
    let dir = std::env::temp_dir().join(format!("tine-publish-tql-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq/config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Tasks.md"),
        "- TODO tql-visible-result [[Alpha]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Dashboard.md"),
        "- {{tine-query @block and [[Alpha]]}}\n- {{query (task TODO)}}\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let (outdir, _) = publish_graph_with_main_reader(&graph).unwrap();
    let dashboard =
        fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();

    // Both macros became query blocks…
    assert_eq!(
        dashboard.matches("class=\"query\"").count(),
        2,
        "both macro names render as query blocks: {dashboard}"
    );
    // …neither leaked its authored source as literal text…
    assert!(
        !dashboard.contains("{{tine-query") && !dashboard.contains("{{query"),
        "a query macro published as literal text: {dashboard}"
    );
    assert!(
        !dashboard.contains("macro-raw"),
        "a query macro fell through to the unknown-macro literal: {dashboard}"
    );
    // …and the TQL one actually RAN, rather than rendering an empty result
    // that would look identical to a working query with no matches.
    assert!(
        dashboard.contains("tql-visible-result"),
        "the TQL query returned its row: {dashboard}"
    );

    let _ = fs::remove_dir_all(&dir);
}

fn query_export_fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tine-publish-query-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(dir.join("logseq/config.edn"), "{}\n").unwrap();
    // Tasks: one matching block, one NOT matching (whole-page export must
    // carry it), a link to a page outside the set and one inside it.
    fs::write(
        dir.join("pages/Tasks.md"),
        "- TODO first-task [[Private Notes]]\n- unmatched-sibling-text [[Also Selected]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Also Selected.md"),
        "- TODO second-task #Private\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Private Notes.md"),
        "- private-sentinel-text\n- DONE not-a-todo\n",
    )
    .unwrap();
    fs::write(dir.join("journals/2026_01_02.md"), "- TODO journal-task\n").unwrap();
    fs::write(dir.join("pages/Dashboard.md"), "- {{query (task TODO)}}\n").unwrap();
    dir
}

fn todo_request(name: &str) -> query_export::QueryPublicationRequest {
    query_export::QueryPublicationRequest {
        query: "(task TODO)".into(),
        advanced: false,
        simple_dialect: None,
        current_page: Some("Dashboard".into()),
        view: None,
        host_block_id: None,
        host_properties: Vec::new(),
        name: name.into(),
        folder: None,
        replace: false,
        asset_budget_bytes: None,
        app_bundle: None,
    }
}

#[test]
fn query_export_publishes_whole_owner_pages_and_nothing_else() {
    let dir = query_export_fixture("owners");
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let request = todo_request("Open tasks");
    let plan = plan_query_publication(&graph, &request).unwrap();
    assert_eq!(plan.anchor, "block");
    assert_eq!(plan.row_count, 3, "{plan:?}");
    assert!(!plan.exists);
    assert_eq!(plan.folder, "open-tasks");
    let names: Vec<&str> = plan.pages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names.contains(&"Tasks") && names.contains(&"Also Selected"));
    assert!(plan.pages.iter().any(|p| p.journal), "journal owner kept");
    assert!(
        !names.contains(&"Private Notes"),
        "DONE block must not select"
    );
    assert!(!names.contains(&"Dashboard"), "host page has no match");

    let outcome = publish_query(&graph, &request, &plan.fingerprint).unwrap();
    assert_eq!(outcome.pages, 3);
    assert!(outcome.retired.is_none());
    let out = PathBuf::from(&outcome.path);
    assert_eq!(out, dir.join("published-queries").join("open-tasks"));
    let tasks = fs::read_to_string(out.join("tasks.html")).unwrap();
    assert!(
        tasks.contains("unmatched-sibling-text"),
        "whole page: {tasks}"
    );
    assert!(
        tasks.contains("also-selected.html"),
        "inside-set link resolves: {tasks}"
    );
    assert!(
        !tasks.contains("private-notes.html") && tasks.contains("Private Notes"),
        "outside-set link is inert text, not a dangling href: {tasks}"
    );
    assert!(tasks.contains("ref-outside"), "{tasks}");
    assert!(!out.join("private-notes.html").exists());
    let also = fs::read_to_string(out.join("also-selected.html")).unwrap();
    assert!(
        !also.contains("private.html") && also.contains("tag-outside"),
        "outside tag inert: {also}"
    );
    let index = fs::read_to_string(out.join("search-index.js")).unwrap();
    assert!(
        !index.contains("private-sentinel-text"),
        "outside content: {index}"
    );
    assert!(
            !index.contains("\"title\":\"Private Notes\""),
            "an outside page is never a page entry (its name may appear as authored text on a selected page): {index}"
        );
    assert!(!dir.join("publish").exists(), "the graph site is untouched");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn query_export_refuses_a_stale_review_and_respects_create_vs_replace() {
    let dir = query_export_fixture("replace");
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let request = todo_request("Open tasks");
    let plan = plan_query_publication(&graph, &request).unwrap();

    // Content of a selected page changes after review: same paths, new
    // revision, so the fingerprint moves and the export refuses.
    fs::write(
        dir.join("pages/Tasks.md"),
        "- TODO first-task edited\n- unmatched-sibling-text\n",
    )
    .unwrap();
    drop(_projection);
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let stale = publish_query(&graph, &request, &plan.fingerprint);
    assert!(
        matches!(stale, Err(query_export::QueryPublicationError::Refused(ref m)) if m.contains("changed")),
        "{stale:?}"
    );

    let plan = plan_query_publication(&graph, &request).unwrap();
    publish_query(&graph, &request, &plan.fingerprint).unwrap();
    let out = dir.join("published-queries").join("open-tasks");
    assert!(out.join("tasks.html").exists());

    // Second export of the same name: the plan reports the collision and
    // a free suffix; create-only refuses; replace retires the old site.
    let plan = plan_query_publication(&graph, &request).unwrap();
    assert!(plan.exists);
    assert_eq!(plan.suggested_folder.as_deref(), Some("open-tasks-2"));
    let refused = publish_query(&graph, &request, &plan.fingerprint);
    assert!(
        matches!(
            refused,
            Err(query_export::QueryPublicationError::Refused(_))
        ),
        "{refused:?}"
    );
    fs::write(out.join("manual-note.txt"), "kept in recovery").unwrap();
    let replacing = query_export::QueryPublicationRequest {
        replace: true,
        folder: Some(plan.folder.clone()),
        ..request.clone()
    };
    let plan = plan_query_publication(&graph, &replacing).unwrap();
    let outcome = publish_query(&graph, &replacing, &plan.fingerprint).unwrap();
    let retired = PathBuf::from(outcome.retired.expect("previous export retired"));
    assert!(retired.starts_with(dir.join("logseq/.tine-trash/conflicts")));
    assert_eq!(
        fs::read_to_string(retired.join("manual-note.txt")).unwrap(),
        "kept in recovery"
    );
    assert!(!out.join("manual-note.txt").exists());
    assert!(out.join("tasks.html").exists());

    // A separate export under the suggested folder lands beside it.
    let separate = query_export::QueryPublicationRequest {
        folder: Some("open-tasks-2".into()),
        ..request.clone()
    };
    let plan = plan_query_publication(&graph, &separate).unwrap();
    assert!(!plan.exists);
    publish_query(&graph, &separate, &plan.fingerprint).unwrap();
    assert!(dir
        .join("published-queries/open-tasks-2/tasks.html")
        .exists());
    assert!(out.join("tasks.html").exists(), "sibling untouched");
    let _ = fs::remove_dir_all(&dir);
}

fn asset_fixture(tag: &str) -> PathBuf {
    let dir = query_export_fixture(tag);
    for sub in ["assets/sub/deep", "assets/a", "assets/b"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    fs::write(dir.join("assets/sub/deep/pic.png"), b"nested-png").unwrap();
    fs::write(dir.join("assets/a/same.png"), b"a-bytes").unwrap();
    fs::write(dir.join("assets/b/same.png"), b"b-bytes").unwrap();
    fs::write(dir.join("assets/report.pdf"), b"%PDF-1.4 pdf-bytes").unwrap();
    fs::write(dir.join("assets/talk.mp3"), b"mp3-bytes").unwrap();
    // The page body references every shape the spec lists: nested path,
    // same basename in two directories, non-image link + audio, a remote
    // link, a missing file, and a repeat reference.
    fs::write(
            dir.join("pages/Tasks.md"),
            "- TODO first-task ![nested](../assets/sub/deep/pic.png) ![a](../assets/a/same.png) ![b](../assets/b/same.png)\n\
             - [report](../assets/report.pdf) [ext](https://example.com/x.png)\n\
             - ![music](../assets/talk.mp3)\n\
             - ![gone](../assets/missing.png) ![again](../assets/a/same.png)\n",
        )
        .unwrap();
    dir
}

#[test]
fn query_export_copies_referenced_assets_into_the_leaf() {
    let dir = asset_fixture("assets-copy");
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let request = todo_request("With assets");
    let plan = plan_query_publication(&graph, &request).unwrap();
    let outcome = publish_query(&graph, &request, &plan.fingerprint).unwrap();
    let out = PathBuf::from(&outcome.path);
    let html = fs::read_to_string(out.join("tasks.html")).unwrap();

    // Copied: nested path preserved; same basename kept apart by directory.
    assert_eq!(
        fs::read(out.join("assets/sub/deep/pic.png")).unwrap(),
        b"nested-png"
    );
    assert_eq!(fs::read(out.join("assets/a/same.png")).unwrap(), b"a-bytes");
    assert_eq!(fs::read(out.join("assets/b/same.png")).unwrap(), b"b-bytes");
    assert_eq!(
        fs::read(out.join("assets/report.pdf")).unwrap(),
        b"%PDF-1.4 pdf-bytes"
    );
    assert_eq!(fs::read(out.join("assets/talk.mp3")).unwrap(), b"mp3-bytes");
    // URLs rewritten to the movable leaf; the original graph paths are gone.
    assert!(html.contains(r#"src="assets/sub/deep/pic.png""#), "{html}");
    assert!(html.contains(r#"src="assets/a/same.png""#), "{html}");
    assert!(html.contains(r#"src="assets/b/same.png""#), "{html}");
    assert!(html.contains(r#"href="assets/report.pdf""#), "{html}");
    assert!(html.contains(r#"src="assets/talk.mp3""#), "{html}");
    assert!(
        !html.contains("../assets/"),
        "no link escapes the leaf: {html}"
    );
    assert!(
        html.contains("https://example.com/x.png"),
        "remote link untouched"
    );
    // Referenced twice, copied once.
    assert_eq!(html.matches(r#"src="assets/a/same.png""#).count(), 2);
    // Missing: a visible marker and one warning, but the export succeeds.
    assert_eq!(html.matches("asset-omitted").count(), 1, "{html}");
    assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
    assert!(outcome.warnings[0].contains("missing.png"));
    // The graph site is untouched by all of this: nothing under publish/.
    assert!(!dir.join("publish").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn query_export_over_the_asset_budget_is_refused_whole() {
    let dir = asset_fixture("assets-budget");
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    // 10 + 7 + 7 + 18 = 42 bytes fit; talk.mp3 (9) does not.
    let request = query_export::QueryPublicationRequest {
        asset_budget_bytes: Some(45),
        ..todo_request("With assets")
    };
    let plan = plan_query_publication(&graph, &request).unwrap();
    let error = publish_query(&graph, &request, &plan.fingerprint).unwrap_err();
    let query_export::QueryPublicationError::AssetBudget(message) = &error else {
        panic!("expected a typed budget refusal, got {error:?}");
    };
    assert!(message.contains("talk.mp3"), "{message}");
    assert!(message.contains("Settings"), "{message}");
    // Nothing reached the destination and no stage was left behind.
    assert!(!dir.join("published-queries").join("with-assets").exists());
    assert!(!dir.join("publish").exists());
    // The same request under a sufficient budget succeeds.
    let request = query_export::QueryPublicationRequest {
        asset_budget_bytes: Some(51),
        ..request
    };
    let plan = plan_query_publication(&graph, &request).unwrap();
    publish_query(&graph, &request, &plan.fingerprint).unwrap();
    assert!(dir
        .join("published-queries/with-assets/assets/talk.mp3")
        .exists());
    let _ = fs::remove_dir_all(&dir);
}

/// A nested query inside an exported page neither counts what it left
/// out nor hides that a sample was drawn over the whole graph.
#[test]
fn query_export_nested_query_is_silent_about_the_outside_but_flags_sampling() {
    let dir = query_export_fixture("nested");
    // Dashboard hosts a query over ALL todos (3 owners) but is only
    // exported because it carries a matching task of its own.
    fs::write(
            dir.join("pages/Dashboard.md"),
            "- TODO dash-task\n- {{query (task TODO)}}\n- {{query (and (task TODO) (page \"Private Notes\"))}}\n",
        )
        .unwrap();
    fs::write(
        dir.join("pages/Tasks.md"),
        "- {{query (and (task TODO) (sample 1))}}\n- TODO first-task\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let request = todo_request("Nested");
    let plan = plan_query_publication(&graph, &request).unwrap();
    let outcome = publish_query(&graph, &request, &plan.fingerprint).unwrap();
    let out = PathBuf::from(&outcome.path);
    let dashboard = fs::read_to_string(out.join("dashboard.html")).unwrap();
    assert!(!dashboard.contains("query-omitted"), "{dashboard}");
    assert!(!dashboard.contains("omitted"), "{dashboard}");
    assert!(!dashboard.contains("private-sentinel-text"), "{dashboard}");
    let tasks = fs::read_to_string(out.join("tasks.html")).unwrap();
    assert!(tasks.contains("query-sampled"), "{tasks}");
    assert!(!dashboard.contains("query-sampled"), "{dashboard}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn query_export_refuses_twins_and_empty_results() {
    let dir = query_export_fixture("twins");
    // A second file claiming "Tasks": the export must refuse, not ship one.
    fs::write(dir.join("journals/Tasks.md"), "- TODO twin-task\n").unwrap();
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let twin = plan_query_publication(&graph, &todo_request("Open tasks"));
    assert!(
        matches!(twin, Err(query_export::QueryPublicationError::Refused(ref m)) if m.contains("Two files")),
        "{twin:?}"
    );
    let mut none = todo_request("Nothing");
    none.query = "(task CANCELED)".into();
    let plan = plan_query_publication(&graph, &none).unwrap();
    assert!(plan.pages.is_empty());
    let refused = publish_query(&graph, &none, &plan.fingerprint);
    assert!(
        matches!(refused, Err(query_export::QueryPublicationError::Refused(ref m)) if m.contains("no results")),
        "{refused:?}"
    );
    assert!(!dir.join("published-queries").join("nothing").exists());
    let _ = fs::remove_dir_all(&dir);
}

/// The exact capture `publish_graph` makes: every page read fresh from
/// disk, parsed, and given its file-owned runtime identity.
///
/// Shared by every fixture that needs a `PublicationGraphSnapshot` of its
/// own, so a test can never disagree with the production capture about
/// which documents, identities or inventory order a snapshot holds.
fn capture_snapshot_pages(graph: &Graph) -> Vec<(crate::model::PageEntry, Arc<doc::Document>)> {
    graph
        .list_pages()
        .into_iter()
        .map(|entry| {
            let content = fs::read_to_string(&entry.path).unwrap();
            let mut parsed = doc::parse(&content);
            crate::model::assign_doc_runtime_ids(&mut parsed.roots, &entry.rel_path);
            (entry, Arc::new(parsed))
        })
        .collect()
}

/// Test-only independent walk oracle. Production publication adapters use
/// the shared SQL reader; local renderer fixtures need only prove that the
/// parsed IR is forwarded and its returned identities hydrate from the
/// exact captured documents.
struct CapturedWalkReader<'a> {
    graph: &'a Graph,
    runs: std::cell::Cell<usize>,
    freshness_checks: std::cell::Cell<usize>,
}

impl PublicationQueryRead for CapturedWalkReader<'_> {
    fn run_subtrees(
        &self,
        _: &crate::query::ir::Query,
        _: &ViewSettings,
        _: Bounds,
        _: &ExecutionContext,
    ) -> Result<crate::query::export_execute::SubtreeQueryResult, QueryExecutionError> {
        Err(QueryExecutionError::Unavailable(
            crate::query::QueryUnavailableReason::UnsupportedRelation,
        ))
    }

    fn run(
        &self,
        query: &crate::query::ir::Query,
        view: &ViewSettings,
        bounds: Bounds,
        context: &ExecutionContext,
    ) -> Result<QueryResult, QueryExecutionError> {
        self.runs.set(self.runs.get() + 1);
        let resolved =
            crate::query::resolve_for_execution(query, context, crate::date::JournalDate::today());
        Ok(crate::query::run_resolved_query_result_over(
            &crate::query::GraphQueryPages(self.graph),
            &resolved,
            view,
            bounds,
        ))
    }

    fn ensure_current(&self) -> Result<(), QueryExecutionError> {
        self.freshness_checks.set(self.freshness_checks.get() + 1);
        Ok(())
    }
}

fn prepare_publication_graph(graph: &Graph) -> tempfile::TempDir {
    let projection = tempfile::tempdir().unwrap();
    graph
        .attach_direct_projection(projection.path().join("direct.sqlite"))
        .unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(std::time::Duration::from_secs(30))
        .unwrap();
    projection
}

fn publish_graph_with_main_reader(graph: &Graph) -> io::Result<(String, usize)> {
    let _projection = prepare_publication_graph(graph);
    publish_graph(graph)
}

/// The renderer routes TQL through its supplied reader and filters only
/// after that reader has answered over the complete capture.
///
/// The two claims are inseparable, so they are one fixture:
///
/// * the reader sees the COMPLETE capture, including the private page, so
///   the public-page capability can subtract honestly. A publication that
///   indexed only public pages would render the same visible row with a
///   silently wrong "omitted" count — a privacy claim stated as a number.
#[test]
fn publish_answers_a_tql_macro_through_its_reader_and_counts_private_matches() {
    let dir =
        std::env::temp_dir().join(format!("tine-publish-tql-omission-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages/Dashboard.md"),
        "public:: true\n- {{tine-query @block and [[Alpha]]}}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Tasks.md"),
        "public:: true\n- TODO tql-public-hit [[Alpha]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Secret.md"),
        "- TODO tql-private-hit [[Alpha]]\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let (outdir, count) = publish_graph_with_main_reader(&graph).unwrap();
    assert_eq!(count, 2, "only the two public pages are published");
    let out = Path::new(&outdir);
    let dashboard = fs::read_to_string(out.join("dashboard.html")).unwrap();
    let search = fs::read_to_string(out.join("search-index.js")).unwrap();

    assert!(
        dashboard.contains("tql-public-hit"),
        "the authorized match must render: {dashboard}"
    );
    assert!(
        dashboard.contains("<span class=\"query-count\">1</span>"),
        "the visible count is the authorized one: {dashboard}"
    );
    assert!(
        !dashboard.contains("tql-private-hit") && !search.contains("tql-private-hit"),
        "a private match crossed the public-page capability: {dashboard}"
    );
    assert!(
        dashboard.contains("1 result on non-public pages omitted."),
        "the private match must be counted before publication filtering: {dashboard}"
    );
    assert!(
        !dashboard.contains("query-unsupported"),
        "the publication refused its own query: {dashboard}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_page_query_emits_real_public_links_and_counts_private_omissions() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-page-query-links-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages/Dashboard.md"),
        "public:: true\n- {{tine-query @page}}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Public Target.md"),
        "public:: true\n- public target\n",
    )
    .unwrap();
    fs::write(dir.join("pages/Secret Target.md"), "- private target\n").unwrap();

    let graph = Graph::open(&dir);
    let (outdir, count) = publish_graph_with_main_reader(&graph).unwrap();
    assert_eq!(count, 2);
    let dashboard = fs::read_to_string(Path::new(&outdir).join("dashboard.html")).unwrap();
    assert!(
        dashboard.contains(
            "class=\"ref query-page-result\" href=\"public-target.html\">Public Target</a>"
        ),
        "the physical public page row must become its real page link: {dashboard}"
    );
    assert!(
        !dashboard.contains("Secret Target") && !dashboard.contains("secret-target.html"),
        "a private page row crossed the captured capability: {dashboard}"
    );
    assert!(
        dashboard.contains("1 result on non-public pages omitted."),
        "the full reader answer must be counted before capability filtering: {dashboard}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// TQL and OG publication share the same main-image correspondence rule.
/// A changed source is refused until its ordinary watcher delta commits;
/// publication never rebuilds a database from the captured documents.
#[test]
fn publish_tql_refuses_stale_main_until_normal_source_update() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-tql-immutable-capture-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq/config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Dashboard.md"),
        "- {{tine-query @block and [[Alpha]]}}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Tasks.md"),
        "- TODO tql-stale-token [[Alpha]]\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let _projection = prepare_publication_graph(&graph);
    // An external editor rewrites the source after the live cache was built.
    fs::write(
        dir.join("pages/Tasks.md"),
        "- TODO tql-current-token [[Alpha]]\n",
    )
    .unwrap();

    assert_eq!(
        publish_graph(&graph).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    graph
        .sync_file_checked(&dir.join("pages/Tasks.md"))
        .unwrap();
    graph
        .wait_for_direct_projection_for_test(std::time::Duration::from_secs(30))
        .unwrap();
    let (outdir, _) = publish_graph(&graph).unwrap();
    let dashboard = fs::read_to_string(Path::new(&outdir).join("dashboard.html")).unwrap();
    assert!(
        dashboard.contains("tql-current-token"),
        "the reader must describe the fresh capture: {dashboard}"
    );
    assert!(
        !dashboard.contains("tql-stale-token"),
        "a stale live-graph revision reached the published result: {dashboard}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The captured-document resolver remains owner-private and operation
/// scoped, but it never creates query storage. Selection belongs wholly to
/// the supplied main reader.
#[test]
fn publication_snapshot_has_no_query_index_and_dies_with_its_root() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-snapshot-index-lifetime-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages/Secret.md"),
        "- TODO private-row-token [[Alpha]]\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let snapshot = PublicationGraphSnapshot::new(capture_snapshot_pages(&graph)).unwrap();
    let root = snapshot._root.0.clone();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700,
            "the private snapshot tree must be owner-only from creation"
        );
    }

    assert!(
        !root.join("query-index.sqlite").exists(),
        "publication must not own transient query storage"
    );

    // Dropping closes the captured resolver and removes its empty root.
    drop(snapshot);
    assert!(
        !root.exists(),
        "the publication snapshot tree outlived its publication: {}",
        root.display()
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn legacy_document_renderer_reports_a_missing_query_reader_explicitly() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-no-query-reader-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq/config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(dir.join("pages/Dashboard.md"), "- {{query (task TODO)}}\n").unwrap();
    let graph = Graph::open(&dir);
    let pages = capture_snapshot_pages(&graph)
        .into_iter()
        .map(|(entry, document)| (entry, document.as_ref().clone()))
        .collect();

    let (outdir, _) = publish_graph_documents(&graph, pages).unwrap();
    let dashboard = fs::read_to_string(Path::new(&outdir).join("dashboard.html")).unwrap();
    assert!(
        dashboard.contains("Query results are unavailable for this render."),
        "a missing reader must not look like an empty successful query: {dashboard}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// §4.3.1: the query transport is the RAW SOURCE SLICE, not lsdoc's
/// comma-split `data-args`.
///
/// mldoc's macro parser splits arguments on commas and stops before the
/// first `}`, so `{{query (task TODO) {:title "T"}}}` reaches the exporter as
/// the argument `(task TODO) {:title "T"` — the options brace MISSING — and a
/// literal comma comes back with a space inserted. Both are silent
/// corruptions of the author's bytes. This asserts the publisher reads the
/// block source instead.
#[test]
fn publish_reads_a_query_macro_from_the_raw_source_not_the_split_args() {
    let dir = std::env::temp_dir().join(format!("tine-publish-rawarg-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq/config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Tasks.md"),
        "- TODO raw-arg-visible-result\n",
    )
    .unwrap();
    // A trailing options map: the AST argument loses its closing brace, so a
    // publisher trusting `data-args` hands the parser an unbalanced form.
    fs::write(
        dir.join("pages/Dashboard.md"),
        "- {{query (task TODO) {:title \"Open, work\"}}}\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let (outdir, _) = publish_graph_with_main_reader(&graph).unwrap();
    let dashboard =
        fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();

    assert!(
        dashboard.contains("class=\"query\""),
        "the query rendered: {dashboard}"
    );
    assert!(
        dashboard.contains("raw-arg-visible-result"),
        "the query with an options map still RAN: {dashboard}"
    );
    // The stray `}` §4.3.1 measures on mldoc must not reach the document.
    assert!(
        !dashboard.contains("}}}") && !dashboard.contains("{{query"),
        "the options brace leaked into the published page: {dashboard}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_begin_query_renders_authored_title_and_results() {
    let dir = std::env::temp_dir().join(format!("tine-publish-begin-query-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq/config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Tasks.md"),
        "- TODO BEGIN_QUERY_PUBLIC_RESULT\n",
    )
    .unwrap();
    fs::write(
            dir.join("pages/Dashboard.md"),
            "- #+BEGIN_QUERY\n  {:title \"Open work\"\n   :query [:find (pull ?b [*]) :where (task ?b \"TODO\")]}\n  #+END_QUERY\n",
        )
        .unwrap();

    let graph = Graph::open(&dir);
    let (outdir, _) = publish_graph_with_main_reader(&graph).unwrap();
    let dashboard =
        fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();

    assert!(
        dashboard.contains("class=\"query-head\">Open work"),
        "{dashboard}"
    );
    assert!(
        dashboard.contains("BEGIN_QUERY_PUBLIC_RESULT"),
        "{dashboard}"
    );
    assert!(
        !dashboard.contains("class=\"query-omitted\""),
        "{dashboard}"
    );
    assert!(!dashboard.contains("#+BEGIN_QUERY"), "{dashboard}");
    assert!(!dashboard.contains("#+END_QUERY"), "{dashboard}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_begin_query_reports_private_rows_without_leaking_them() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-begin-query-private-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
            dir.join("pages/Dashboard.md"),
            "public:: true\n- #+BEGIN_QUERY\n  {:title \"Private-aware work\"\n   :query [:find (pull ?b [*]) :where (task ?b \"TODO\")]}\n  #+END_QUERY\n",
        )
        .unwrap();
    fs::write(
        dir.join("pages/Secret.md"),
        "- TODO PRIVATE_BEGIN_QUERY_RESULT_MUST_NOT_LEAK\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let (outdir, count) = publish_graph_with_main_reader(&graph).unwrap();
    assert_eq!(count, 1);
    let dashboard =
        fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();

    assert!(dashboard.contains("class=\"query-omitted\""), "{dashboard}");
    assert!(
        dashboard.contains("1 result on non-public pages omitted."),
        "{dashboard}"
    );
    assert!(
        !dashboard.contains("PRIVATE_BEGIN_QUERY_RESULT_MUST_NOT_LEAK"),
        "{dashboard}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_malformed_begin_query_is_inert_and_hides_its_payload() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-begin-query-malformed-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
            dir.join("pages/Dashboard.md"),
            "public:: true\n- #+BEGIN_QUERY\n  {:title \"MALFORMED_BEGIN_QUERY_PAYLOAD\" :query (task TODO)}\n  #+END_QUERY\n",
        )
        .unwrap();

    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let (outdir, _) = publish_graph(&graph).unwrap();
    let dashboard =
        fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();

    assert!(
        dashboard.contains("class=\"query-unsupported begin-query-unsupported\""),
        "{dashboard}"
    );
    assert!(
        !dashboard.contains("MALFORMED_BEGIN_QUERY_PAYLOAD"),
        "{dashboard}"
    );
    assert!(!dashboard.contains("#+BEGIN_QUERY"), "{dashboard}");
    assert!(!dashboard.contains("#+END_QUERY"), "{dashboard}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_renders_facets_queries_and_embeds() {
    let dir = std::env::temp_dir().join(format!("tine-publish-facets-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Target.md"),
        "- an embeddable target\n  id:: 22222222-2222-2222-2222-222222222222\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Main.md"),
        "- TODO [#A] do the thing\n  SCHEDULED: <2026-07-10 Fri>\n  notes after the schedule\n\
             - DONE finished it\n\
             - a note\n  status:: open\n\
             - {{query (task TODO)}}\n\
             - {{embed ((22222222-2222-2222-2222-222222222222))}}\n\
             - {{video https://www.youtube.com/watch?v=abc123}}\n",
    )
    .unwrap();

    let g = Graph::open(&dir);
    let (outdir, _) = publish_graph_with_main_reader(&g).unwrap();
    let main = fs::read_to_string(std::path::Path::new(&outdir).join("main.html")).unwrap();

    // task facets: checkbox + marker badge, priority, planning date
    assert!(
        main.contains("class=\"task-checkbox\""),
        "open task checkbox: {main}"
    );
    assert!(
        main.contains("class=\"task-marker m-todo\">TODO"),
        "TODO badge"
    );
    assert!(
        main.contains("class=\"priority p-a\">[#A]"),
        "priority badge"
    );
    assert!(main.contains("SCHEDULED:"), "scheduled line");
    assert!(
        main.contains("notes after the schedule"),
        "body after schedule"
    );
    let scheduled_trailers = main.matches("class=\"planning scheduled\"").count();
    assert!(scheduled_trailers > 0, "scheduled trailer rendered");
    assert_eq!(
        main.matches("2026-07-10 Fri").count(),
        scheduled_trailers,
        "each planning date renders only in trailer chrome, not again in the body"
    );
    assert!(
        main.contains("class=\"task-checkbox checked\"") && main.contains("class=\"b done\""),
        "DONE checked + muted"
    );
    // block property shown
    assert!(
        main.contains("status") && main.contains("open"),
        "block property rendered"
    );
    // query ran and rendered the TODO result (not empty, not the literal macro)
    assert!(main.contains("class=\"query\""), "query block rendered");
    assert!(
        !main.contains("{{query"),
        "query macro expanded, not literal"
    );
    // embed inlined the target block's text
    assert!(main.contains("class=\"embed"), "embed rendered");
    assert!(
        main.contains("class=\"embed block-embed single-root\"><ul class=\"embed-outline\">"),
        "block embed exposes one CSS-scoped root without generic outline connectors: {main}"
    );
    assert!(
        main.contains("an embeddable target"),
        "embed inlined target content: {main}"
    );
    // video → youtube iframe
    assert!(main.contains("youtube.com/embed/abc123"), "video iframe");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_runs_repeated_query_macros_once_per_authored_use() {
    let dir =
        std::env::temp_dir().join(format!("tine-publish-query-repeat-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Tasks.md"),
        "- TODO repeated query target\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Dashboard.md"),
        "- {{query (task TODO)}}\n\
             - {{query (task TODO)}}\n\
             - {{query (task TODO)}}\n\
             - {{query (task TODO)}}\n\
             - {{query (task TODO)}}\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let captured = capture_snapshot_pages(&graph);
    let pages = captured
        .iter()
        .map(|(entry, document)| (entry.clone(), document.as_ref().clone()))
        .collect();
    let mut snapshot = PublicationGraphSnapshot::new(captured).unwrap();
    snapshot.graph.config = graph.config.clone();
    let reader = CapturedWalkReader {
        graph: &snapshot.graph,
        runs: std::cell::Cell::new(0),
        freshness_checks: std::cell::Cell::new(0),
    };
    let (outdir, _) = publish_graph_documents_with_queries(&graph, pages, &reader).unwrap();

    assert_eq!(
        reader.runs.get(),
        5,
        "each macro occurrence must execute through the supplied reader"
    );
    assert_eq!(
        reader.freshness_checks.get(),
        1,
        "the operation is revalidated once before stage publication"
    );
    let dash = fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();
    assert_eq!(
        dash.matches("class=\"query\"").count(),
        5,
        "each macro occurrence still renders independently"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publication_rejects_oversized_sources_before_calling_the_reader() {
    let dir = std::env::temp_dir().join(format!(
        "tine-publish-query-source-bound-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(dir.join("pages").join("P.md"), "- TODO target\n").unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();
    let refs = RefIndex::new();
    let reader = CapturedWalkReader {
        graph: &graph,
        runs: std::cell::Cell::new(0),
        freshness_checks: std::cell::Cell::new(0),
    };
    let ctx = Ctx {
        refs: &refs,
        reverse_refs: None,
        graph: Some(&graph),
        slugs: None,
        inline_assets: false,
        print_asset_budget: None,
        query_reader: Some(&reader),
        print_error: None,
        pages: None,
        page_links: None,
        inert_outside_links: false,
        scope: None,
        recorder: None,
        asset_sink: None,
    };

    let oversized = "x".repeat(crate::query::QUERY_SOURCE_MAX_BYTES + 1);
    assert!(render_query(&graph, &oversized, &ctx, 0).contains("publication limit"));
    let nested = format!("{}(task TODO){}", "(and ".repeat(1_000), ")".repeat(1_000));
    assert!(render_query(&graph, &nested, &ctx, 0).contains("nesting is too deep"));
    assert_eq!(reader.runs.get(), 0, "refused source reached the reader");
    let _ = render_query(&graph, "(task TODO)", &ctx, 0);
    assert_eq!(reader.runs.get(), 1, "a valid source executes normally");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_query_keeps_and_hydrates_a_match_below_a_nonmatching_gap() {
    let dir = std::env::temp_dir().join(format!("tine-publish-query-gap-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(
            dir.join("pages").join("Tasks.md"),
            "- TODO parity ancestor\n  - DONE parity gap\n    - TODO parity grandchild\n      - live child below retained grandchild\n",
        )
        .unwrap();
    fs::write(
        dir.join("pages").join("Dashboard.md"),
        "- {{query (task TODO)}}\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let (outdir, _) = publish_graph_with_main_reader(&graph).unwrap();
    let dashboard =
        fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();

    assert_eq!(
        dashboard.matches("parity grandchild").count(),
        2,
        "{dashboard}"
    );
    assert_eq!(
        dashboard
            .matches("live child below retained grandchild")
            .count(),
        2,
        "{dashboard}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_reuses_pass1_docs_for_repeated_page_embeds() {
    let dir = std::env::temp_dir().join(format!("tine-publish-embed-reuse-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Target.md"),
        "- shared embed target\n  id:: 33333333-3333-3333-3333-333333333333\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Org Page.org"),
        "public:: true\n* org fixture page\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Dashboard.md"),
        "- {{embed [[Target]]}}\n\
             - {{embed [[Target]]}}\n\
             - {{embed [[Target]]}}\n\
             - {{embed [[Target]]}}\n",
    )
    .unwrap();

    let g = Graph::open(&dir);
    let _guard = publish_test_counts::count_for(&dir);
    let _projection = prepare_publication_graph(&g);
    let (outdir, _) = publish_graph(&g).unwrap();

    assert_eq!(
        publish_test_counts::page_doc_loads(),
        0,
        "public page embeds should reuse pass-1 parsed docs, not reload from disk"
    );
    let dash = fs::read_to_string(std::path::Path::new(&outdir).join("dashboard.html")).unwrap();
    assert_eq!(
        dash.matches("shared embed target").count(),
        4,
        "each embed occurrence still renders"
    );

    let _ = fs::remove_dir_all(&dir);
}

// ===== Sheets: a `tine.view::` block must publish as a meaningful read-only
// sheet (table / board / grid), not as bare nested bullets. =====

fn publish_sheet_fixture(name: &str, config: &str, pages: &[(&str, &str)]) -> (PathBuf, String) {
    let dir =
        std::env::temp_dir().join(format!("tine-publish-sheet-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(dir.join("logseq").join("config.edn"), config).unwrap();
    for (file, body) in pages {
        fs::write(dir.join("pages").join(file), body).unwrap();
    }
    let graph = Graph::open(&dir);
    let (outdir, count) = publish_graph_with_main_reader(&graph).unwrap();
    assert_eq!(count, pages.len(), "every fixture page publishes");
    (dir, outdir)
}

fn read_out(outdir: &str, file: &str) -> String {
    fs::read_to_string(std::path::Path::new(outdir).join(file)).unwrap()
}

/// Order-sensitive: the text mentions every sheet facet in prose, so
/// assertions run against the rendered sheet region only.
fn sheet_table_html() -> (PathBuf, String) {
    let (dir, outdir) = publish_sheet_fixture(
            "table",
            "{:preferred-workflow :todo}\n",
            &[(
                "SheetTable.md",
                "public:: true\n\
                 - # Sheet table cases\n\
                 - ## Task table\n  \
                   tine.view:: table\n  \
                   tine.col-aggregates:: prop:estimate=sum\n\
                 \t- TODO [#A] Draft the sheet docs #sheets-demo\n\t  \
                   SCHEDULED: <2026-07-08 Wed>\n\t  \
                   owner:: Martin\n\t  \
                   estimate:: 2\n\
                 \t- DOING Polish cell menus #sheets-demo\n\t  \
                   owner:: Codex\n\t  \
                   estimate:: 5\n\
                 \t- DONE Add aggregate footer #sheets-demo\n\t  \
                   owner:: Codex\n\t  \
                   estimate:: 1\n\
                 - ## Typed reading list\n  \
                   tine.view:: table\n  \
                   tine.fields:: status=enum:todo,reading,done;rating=number;done=checkbox;owner=ref\n  \
                   tine.formula.effort:: rating * 2\n\
                 \t- Bases study\n\t  \
                   status:: reading\n\t  \
                   rating:: 5\n\t  \
                   done:: false\n\t  \
                   owner:: [[Martin]]\n\
                 \t- CSV import notes\n\t  \
                   status:: todo\n\t  \
                   rating:: 3\n\t  \
                   done:: false\n\t  \
                   owner:: [[Codex]]\n\
                 - A plain parent\n\
                 \t- plain child bullet\n",
            )],
        );
    (dir, read_out(&outdir, "sheettable.html"))
}

#[test]
fn publish_sheet_table_columns_aggregate_links() {
    let (dir, html) = sheet_table_html();
    assert_eq!(
        html.matches("<table class=\"sheet-table\">").count(),
        2,
        "each tine.view:: table block publishes one table: {html}"
    );
    // Declared + observed columns surface with their labels.
    for label in [">State<", ">Priority<", ">Tags<", ">owner<", ">estimate<"] {
        assert!(html.contains(label), "column label {label}: {html}");
    }
    // Row content + property values are cells, including page-ref links.
    assert!(html.contains("Draft the sheet docs"), "{html}");
    assert!(
        html.contains("<a class=\"ref\" href=\"martin.html\">Martin</a>"),
        "owner value keeps its [[Martin]] link: {html}"
    );
    // The aggregate footer computes the estimate sum (2 + 5 + 1).
    let foot = html.split("<tfoot>").nth(1).unwrap_or("");
    let foot = foot.split("</tfoot>").next().unwrap_or("");
    assert!(foot.contains("Sum"), "aggregate label: {foot}");
    assert!(
        foot.contains("Sum</span> 8</td>"),
        "aggregate value: {foot}"
    );
    // View configuration is chrome, not published prose.
    assert!(!html.contains("tine.view::"), "view prop hidden: {html}");
    assert!(
        !html.contains("tine.col-aggregates::"),
        "aggregate prop hidden: {html}"
    );
    // Checkbox-typed fields render as checkbox cells.
    assert!(
        html.contains("<td><span class=\"task-checkbox\"></span></td>"),
        "done=false checkbox cell: {html}"
    );
    // Formula column evaluates over each row's rating (5*2, 3*2).
    assert!(html.contains(">effort<"), "formula column label: {html}");
    assert!(html.contains(">10</td>"), "rating 5 * 2: {html}");
    assert!(html.contains(">6</td>"), "rating 3 * 2: {html}");
    // Rows keep stable anchors, and a non-sheet block still nests as an outline.
    assert!(html.contains("<tr id=\""), "row anchor: {html}");
    assert!(html.contains("plain child bullet"), "{html}");
    assert!(!html.contains("<td>plain child bullet"), "{html}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_sheet_table_rows_stay_searchable() {
    let (dir, outdir) = publish_sheet_fixture(
        "table-index",
        "{:preferred-workflow :todo}\n",
        &[(
            "IndexSheet.md",
            "public:: true\n\
                 - ## Indexed table\n  \
                   tine.view:: table\n\
                 \t- uniqueterm zebra\n\t  \
                   owner:: Martin\n",
        )],
    );
    let sidx = read_out(&outdir, "search-index.js");
    assert!(
        sidx.contains("uniqueterm zebra"),
        "sheet rows stay in the block search index: {sidx}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_sheet_boards_group_children() {
    let (dir, outdir) = publish_sheet_fixture(
        "board",
        "{:preferred-workflow :todo}\n",
        &[(
            "Boards.md",
            "public:: true\n\
                 - # Board cases\n\
                 - ## Task board\n  \
                   tine.view:: board\n  \
                   tine.group-by:: state\n\
                 \t- TODO First task\n\
                 \t- DOING Second task\n\
                 \t- DONE Third task\n\
                 \t- Unlabeled card\n\
                 - ## Topic board\n  \
                   tine.view:: board\n  \
                   tine.group-by:: tags\n\
                 \t- Schema menu polish #schema\n\
                 \t- CSV drop walkthrough #interop #schema\n",
        )],
    );
    let html = read_out(&outdir, "boards.html");
    assert_eq!(
        html.matches("<div class=\"sheet-board\">").count(),
        2,
        "each board block publishes one board: {html}"
    );
    for col in [">TODO</h3>", ">DOING</h3>", ">DONE</h3>", ">(none)</h3>"] {
        assert!(html.contains(col), "board column {col}: {html}");
    }
    // State columns follow the workflow order, unlabeled cards last.
    let pos = |needle: &str| html.find(needle).unwrap_or(usize::MAX);
    assert!(pos(">TODO</h3>") < pos(">DOING</h3>"), "{html}");
    assert!(pos(">DOING</h3>") < pos(">DONE</h3>"), "{html}");
    assert!(pos(">DONE</h3>") < pos(">(none)</h3>"), "{html}");
    assert!(pos(">TODO</h3>") < pos("First task"), "{html}");
    assert!(pos("First task") < pos(">DOING</h3>"), "{html}");
    assert!(pos(">(none)</h3>") < pos("Unlabeled card"), "{html}");
    // A multi-tag card sits in every matching column.
    assert!(html.contains(">schema</h3>"), "{html}");
    assert!(html.contains(">interop</h3>"), "{html}");
    assert_eq!(
        html.matches("CSV drop walkthrough").count(),
        2,
        "multi-tag card is duplicated across its columns: {html}"
    );
    assert_eq!(html.matches("Schema menu polish").count(), 1, "{html}");
    assert!(!html.contains("tine.view::"), "view prop hidden: {html}");
    let _ = fs::remove_dir_all(&dir);
}

/// **A query's visible-column choice reaches the published page** (P5A).
///
/// The static publisher is the THIRD consumer of the display-settings model
/// (the §4.1 property merge and the app's query table are the other two).
/// Before this it read only `tine.fields`, `=` required, so a column
/// selection rendered in the app and not in the export — the same note
/// showing two different tables. It now calls the SAME resolver, so the
/// precedence cannot drift.
#[test]
fn publish_query_table_shows_the_selected_columns_in_the_selected_order() {
    let (dir, outdir) = publish_sheet_fixture(
        "query-columns",
        "{:preferred-workflow :todo}\n",
        &[
            (
                "QueryColumns.md",
                "public:: true\n\
                     - # Query column cases\n\
                     - {{query (property owner Avery)}}\n  \
                       tine.view:: table\n  \
                       tine.columns:: owner;shipped;absent\n  \
                       tine.fields:: shipped=checkbox;status=text\n",
            ),
            (
                "Tracker.md",
                "public:: true\n\
                     - Refresh Guide examples\n  \
                       status:: active\n  \
                       owner:: Avery\n  \
                       shipped:: false\n",
            ),
        ],
    );
    let html = read_out(&outdir, "querycolumns.html");
    let head = html.split("<thead>").nth(1).unwrap_or("");
    let head = head.split("</thead>").next().unwrap_or("");
    // Exactly the selected columns, in the selected order — `status` is
    // declared and observed, and is deliberately NOT shown.
    assert_eq!(
        head, "<tr><th></th><th>owner</th><th>shipped</th><th>absent</th></tr>",
        "selected columns, in order, after the implicit title column: {html}"
    );
    // Selection runs AFTER the schema lookup, so the checkbox TYPE declared
    // in `tine.fields` still decides how the cell renders. Losing that is
    // the "columns are not schema" defect in its other direction.
    assert!(
        html.contains("<td><span class=\"task-checkbox\"></span></td>"),
        "shipped=checkbox keeps its declared type rendering: {html}"
    );
    // A selected column no row carries renders an EMPTY cell rather than
    // vanishing, shifting its neighbours, or dropping the whole selection.
    let body = html.split("<tbody>").nth(1).unwrap_or("");
    let body = body.split("</tbody>").next().unwrap_or("");
    let cells: Vec<&str> = body.split("<td>").skip(1).collect();
    assert_eq!(cells.len(), 4, "one row, four cells: {body}");
    let last = cells[3].split("</td>").next().unwrap_or("");
    let text: String = last
        .split('>')
        .skip(1)
        .map(|part| part.split('<').next().unwrap_or(""))
        .collect();
    assert!(
        text.trim().is_empty(),
        "the absent column's cell carries no value: {last:?} in {body}"
    );
    assert!(!html.contains("tine.columns::"), "config is chrome: {html}");
    let _ = fs::remove_dir_all(&dir);
}

/// The legacy branch, in the publisher: a note authored before the split
/// carries its column list in a BARE `tine.fields`, and reading it must not
/// require rewriting the note.
#[test]
fn publish_query_table_reads_a_legacy_bare_field_list_as_columns() {
    let (dir, outdir) = publish_sheet_fixture(
        "query-legacy-columns",
        "{:preferred-workflow :todo}\n",
        &[
            (
                "LegacyColumns.md",
                "public:: true\n\
                     - {{query (property owner Avery)}}\n  \
                       tine.view:: table\n  \
                       tine.fields:: status;owner\n",
            ),
            (
                "Tracker.md",
                "public:: true\n\
                     - Refresh Guide examples\n  \
                       status:: active\n  \
                       owner:: Avery\n  \
                       extra:: noise\n",
            ),
        ],
    );
    let html = read_out(&outdir, "legacycolumns.html");
    let head = html.split("<thead>").nth(1).unwrap_or("");
    let head = head.split("</thead>").next().unwrap_or("");
    assert_eq!(
        head, "<tr><th></th><th>status</th><th>owner</th></tr>",
        "the bare legacy list selects columns, in its own order: {html}"
    );
    // The note is not rewritten to publish it; the bytes on disk still
    // carry the legacy spelling (I-4).
    let source = fs::read_to_string(dir.join("pages").join("LegacyColumns.md")).unwrap();
    assert!(source.contains("tine.fields:: status;owner"), "{source}");
    assert!(!source.contains("tine.columns"), "{source}");
    let _ = fs::remove_dir_all(&dir);
}

/// A PRESENT but empty/invalid `tine.columns` is an explicit statement and
/// there is no legacy list behind it — the published table falls back to
/// the default columns, never to the retired list.
#[test]
fn publish_query_table_treats_a_present_empty_columns_list_as_no_selection() {
    let (dir, outdir) = publish_sheet_fixture(
        "query-cleared-columns",
        "{:preferred-workflow :todo}\n",
        &[
            (
                "ClearedColumns.md",
                "public:: true\n\
                     - {{query (property owner Avery)}}\n  \
                       tine.view:: table\n  \
                       tine.columns::\n  \
                       tine.fields:: status\n",
            ),
            (
                "Tracker.md",
                "public:: true\n\
                     - Refresh Guide examples\n  \
                       status:: active\n  \
                       owner:: Avery\n",
            ),
        ],
    );
    let html = read_out(&outdir, "clearedcolumns.html");
    let head = html.split("<thead>").nth(1).unwrap_or("");
    let head = head.split("</thead>").next().unwrap_or("");
    assert!(
        head.contains("<th>owner</th>") && head.contains("<th>status</th>"),
        "cleared means the DEFAULT columns, not the legacy list: {html}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A children-backed sheet is not a query face: `tine.columns` is not its
/// question, and its published table is unchanged.
#[test]
fn publish_children_sheet_ignores_a_query_column_selection() {
    let (dir, outdir) = publish_sheet_fixture(
        "children-columns",
        "{:preferred-workflow :todo}\n",
        &[(
            "ChildrenColumns.md",
            "public:: true\n\
                 - ## Children table\n  \
                   tine.view:: table\n  \
                   tine.columns:: owner\n\
                 \t- A row\n\t  \
                   owner:: Martin\n\t  \
                   status:: active\n",
        )],
    );
    let html = read_out(&outdir, "childrencolumns.html");
    assert!(html.contains("<th>owner</th>"), "{html}");
    assert!(
        html.contains("<th>status</th>"),
        "an ordinary children sheet keeps every observed column: {html}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_sheet_query_board_groups_results() {
    let (dir, outdir) = publish_sheet_fixture(
        "query-board",
        "{:preferred-workflow :todo}\n",
        &[
            (
                "QueryBoards.md",
                "public:: true\n\
                     - # Query board cases\n\
                     - {{query (property owner Avery)}}\n  \
                       tine.view:: board\n  \
                       tine.group-by:: status\n",
            ),
            (
                "Tracker.md",
                "public:: true\n\
                     - Refresh Guide examples\n  \
                       status:: active\n  \
                       owner:: Avery\n\
                     - Publish the updated demo\n  \
                       status:: planned\n  \
                       owner:: Avery\n\
                     - Other thing\n  \
                       owner:: Jules\n  \
                       status:: active\n",
            ),
        ],
    );
    let html = read_out(&outdir, "queryboards.html");
    assert!(
        html.contains("<div class=\"sheet-board\">"),
        "query results render grouped as a board: {html}"
    );
    assert!(
        !html.contains("<div class=\"query\">"),
        "the flat query list is replaced by the sheet view: {html}"
    );
    for col in [">active</h3>", ">planned</h3>"] {
        assert!(html.contains(col), "status column {col}: {html}");
    }
    assert!(html.contains("Refresh Guide examples"), "{html}");
    assert!(html.contains("Publish the updated demo"), "{html}");
    assert!(
        !html.contains("Other thing"),
        "non-matching blocks stay out of the board: {html}"
    );
    assert!(!html.contains("tine.view::"), "view prop hidden: {html}");
    let _ = fs::remove_dir_all(&dir);
}

/// **The identity pair the new key exists for** (P5B). `tine.group-field::
/// prop:state` is the ordinary property named `state`; bare `state` is the
/// task marker. One published fixture proves both, because a bare token
/// could never have.
#[test]
fn publish_query_board_tells_a_state_property_from_the_task_marker() {
    let pages = |group: &str| {
        vec![
            (
                "GroupField.md",
                format!(
                    "public:: true\n\
                         - {{{{query (property owner Avery)}}}}\n  \
                           tine.view:: board\n  \
                           tine.group-field:: {group}\n"
                ),
            ),
            (
                "Tracker.md",
                "public:: true\n\
                     - TODO Refresh Guide examples\n  \
                       state:: shipped\n  \
                       owner:: Avery\n"
                    .to_string(),
            ),
        ]
    };
    let property = pages("prop:state");
    let (dir, outdir) = publish_sheet_fixture(
        "group-field-property",
        "{:preferred-workflow :todo}\n",
        &property
            .iter()
            .map(|(n, b)| (*n, b.as_str()))
            .collect::<Vec<_>>(),
    );
    let html = read_out(&outdir, "groupfield.html");
    assert!(
        html.contains(">shipped</h3>"),
        "prop:state groups by the ORDINARY property: {html}"
    );
    let _ = fs::remove_dir_all(&dir);

    let marker = pages("state");
    let (dir, outdir) = publish_sheet_fixture(
        "group-field-marker",
        "{:preferred-workflow :todo}\n",
        &marker
            .iter()
            .map(|(n, b)| (*n, b.as_str()))
            .collect::<Vec<_>>(),
    );
    let html = read_out(&outdir, "groupfield.html");
    assert!(
        html.contains(">TODO</h3>") && !html.contains(">shipped</h3>"),
        "bare `state` groups by the TASK MARKER: {html}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A PRESENT but empty `tine.group-field::` is an explicit "no grouping":
/// one ungrouped column, and the Board default never reinstates itself —
/// not even with a legacy `tine.group-by::` still on the block.
#[test]
fn publish_query_board_honours_an_explicit_group_clear() {
    let (dir, outdir) = publish_sheet_fixture(
        "group-field-cleared",
        "{:preferred-workflow :todo}\n",
        &[
            (
                "ClearedGroup.md",
                "public:: true\n\
                     - {{query (property owner Avery)}}\n  \
                       tine.view:: board\n  \
                       tine.group-field::\n  \
                       tine.group-by:: status\n",
            ),
            (
                "Tracker.md",
                "public:: true\n\
                     - TODO Refresh Guide examples\n  \
                       status:: active\n  \
                       owner:: Avery\n\
                     - Publish the updated demo\n  \
                       status:: planned\n  \
                       owner:: Avery\n",
            ),
        ],
    );
    let html = read_out(&outdir, "clearedgroup.html");
    assert_eq!(
        html.matches("<div class=\"sheet-board-col\">").count(),
        1,
        "an explicit clear is ONE ungrouped column: {html}"
    );
    assert!(
        !html.contains(">active</h3>") && !html.contains(">TODO</h3>"),
        "neither the legacy key nor the state default may come back: {html}"
    );
    assert!(html.contains("Refresh Guide examples"), "{html}");
    assert!(html.contains("Publish the updated demo"), "{html}");
    let _ = fs::remove_dir_all(&dir);
}

/// The two readings a query board gives a LEGACY block, both of which the
/// app must agree with (P5B):
///
///  * a bare `tine.group-by:: status` on a board face is now the ORDINARY
///    property — it used to fall through to the task marker and publish a
///    TODO/DONE board for a note whose author wrote `status`;
///  * a board with NO grouping anywhere keeps the task-marker default
///    (ADR 0030). `Unset` and an explicit clear are different answers, and
///    only the clear is one ungrouped column.
#[test]
fn publish_query_board_reads_a_legacy_token_and_keeps_the_default() {
    let rows = "public:: true\n\
                    - TODO Refresh Guide examples\n  \
                      status:: active\n  \
                      owner:: Avery\n";
    let (dir, outdir) = publish_sheet_fixture(
        "legacy-group-token",
        "{:preferred-workflow :todo}\n",
        &[
            (
                "LegacyGroup.md",
                "public:: true\n\
                     - {{query (property owner Avery)}}\n  \
                       tine.view:: board\n  \
                       tine.group-by:: status\n",
            ),
            ("Tracker.md", rows),
        ],
    );
    let html = read_out(&outdir, "legacygroup.html");
    assert!(
        html.contains(">active</h3>") && !html.contains(">TODO</h3>"),
        "a bare legacy token on a board face is an ordinary property: {html}"
    );
    let _ = fs::remove_dir_all(&dir);

    let (dir, outdir) = publish_sheet_fixture(
        "unset-group",
        "{:preferred-workflow :todo}\n",
        &[
            (
                "UnsetGroup.md",
                "public:: true\n\
                     - {{query (property owner Avery)}}\n  \
                       tine.view:: board\n",
            ),
            ("Tracker.md", rows),
        ],
    );
    let html = read_out(&outdir, "unsetgroup.html");
    assert!(
        html.contains(">TODO</h3>"),
        "a grouping nothing states keeps the task-marker default: {html}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// An ordinary children-backed board is NOT a query face: it keeps reading
/// `tine.group-by::` exactly as it did, and ignores the new key.
#[test]
fn publish_children_board_ignores_the_query_grouping_key() {
    let (dir, outdir) = publish_sheet_fixture(
        "children-group-field",
        "{:preferred-workflow :todo}\n",
        &[(
            "ChildrenBoard.md",
            "public:: true\n\
                 - ## Children board\n  \
                   tine.view:: board\n  \
                   tine.group-by:: prop:status\n  \
                   tine.group-field::\n\
                 \t- A row\n\t  \
                   status:: active\n",
        )],
    );
    let html = read_out(&outdir, "childrenboard.html");
    assert!(
        html.contains(">active</h3>"),
        "the children board still groups by its own tine.group-by: {html}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_sheet_grid_positional_nested_ragged() {
    let (dir, outdir) = publish_sheet_fixture(
        "grid",
        "{:preferred-workflow :todo}\n",
        &[(
            "Grids.md",
            "public:: true\n\
                 - # Grid cases\n\
                 - ## Positional grid\n  \
                   tine.view:: grid\n  \
                   tine.header:: true\n\
                 \t-\n\t\t- Area\n\t\t- Owner\n\t\t- Notes\n\
                 \t-\n\t\t- Spec\n\t\t- Martin\n\t\t- Nested grid\n\t\t  \
                   tine.view:: grid\n\t\t\
                   \t-\n\t\t\t\t- Risk\n\t\t\t\t- Mitigation\n\
                 \t-\n\t\t- Build\n\t\t-\n\t\t- Ragged rows are fine\n",
        )],
    );
    let html = read_out(&outdir, "grids.html");
    assert_eq!(
        html.matches("<table class=\"sheet-grid\">").count(),
        2,
        "outer grid plus the grid nested in a cell: {html}"
    );
    for head in [">Area</th>", ">Owner</th>", ">Notes</th>"] {
        assert!(html.contains(head), "header cell {head}: {html}");
    }
    for cell in [">Spec</td>", ">Risk</td>", ">Mitigation</td>"] {
        assert!(html.contains(cell), "cell {cell}: {html}");
    }
    // An empty cell stays an empty cell; ragged rows keep their cells.
    assert!(html.contains("></td>"), "empty middle cell: {html}");
    assert!(html.contains(">Build</td>"), "{html}");
    assert_eq!(
        html.matches(">Ragged rows are fine</td>").count(),
        1,
        "{html}"
    );
    // The nested grid renders inside its host cell.
    let pos = |needle: &str| html.find(needle).unwrap_or(usize::MAX);
    assert!(pos("Nested grid") < pos(">Risk</td>"), "{html}");
    assert!(!html.contains("tine.view::"), "view prop hidden: {html}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_logbook_summary_and_ordered_list_markers() {
    let (dir, outdir) = publish_sheet_fixture(
        "facets",
        "{:preferred-workflow :todo}\n",
        &[(
            "Facets.md",
            "public:: true\n\
                 - DONE A task with a logbook\n  \
                   :LOGBOOK:\n  \
                   CLOCK: [2026-07-01 Wed 10:00:00]--[2026-07-01 Wed 10:30:00] =>  00:30:00\n  \
                   :END:\n\
                 - Numbered parent\n  \
                   logseq.order-list-type:: number\n\
                 \t- First\n\
                 \t- Second\n\
                 - Another numbered\n  \
                   logseq.order-list-type:: number\n\
                 \t- Third\n",
        )],
    );
    let html = read_out(&outdir, "facets.html");
    // The hidden LOGBOOK drawer still yields its elapsed-time badge.
    assert!(html.contains("<div class=\"planning logbook\">"), "{html}");
    assert!(html.contains("00:30:00"), "clock total: {html}");
    assert!(
        !html.contains("CLOCK: [2026-07-01 Wed 10:00:00]"),
        "drawer contents stay hidden: {html}"
    );
    // Own-numbered blocks show their ordinal; the unordered child does not.
    assert!(
        html.contains("<span class=\"ord-marker\">1.</span>"),
        "parent marker: {html}"
    );
    assert!(
        html.contains("<span class=\"ord-marker\">2.</span>"),
        "second numbered parent: {html}"
    );
    assert!(
        !html.contains("<span class=\"ord-marker\">3.</span>"),
        "children are not own-ordered: {html}"
    );
    assert!(
        html.matches("ol-item").count() == 2,
        "exactly two own-ordered blocks: {html}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn publish_sheet_print_renders_table() {
    // The single-page print/PDF export shares render_block with the site
    // export — sheets must appear there too.
    let dir = std::env::temp_dir().join(format!("tine-publish-sheet-print-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:preferred-workflow :todo}\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("PrintSheet.md"),
        "public:: true\n\
             - ## Print table\n  \
               tine.view:: table\n  \
               tine.col-aggregates:: prop:estimate=sum\n\
             \t- TODO Priced row\n\t  \
               estimate:: 4\n",
    )
    .unwrap();
    let graph = print_test_graph(&dir);
    let print = graph
        .page_print_html("PrintSheet", PrintOpts::default())
        .unwrap()
        .unwrap();
    assert!(
        print.contains("<table class=\"sheet-table\">"),
        "the print/PDF export carries the sheet too: {print}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Dev utility (not run by default): materialize a richer sample export at a
/// stable path so the published sidebar + search can be screenshot-verified.
/// `cargo test -p tine-core --lib -- --ignored gen_sample_export --nocapture`
/// then open `file://$TMPDIR/tine-sample-export/publish/index.html`.
#[test]
#[ignore = "manual generator: writes a sample export tree for inspection"]
fn gen_sample_export() {
    let dir = std::env::temp_dir().join("tine-sample-export");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:publishing/all-pages-public? true\n :favorites [\"Welcome\" \"Parameterized IP\"]}\n",
    )
    .unwrap();
    let write = |name: &str, body: &str| fs::write(dir.join("pages").join(name), body).unwrap();
    write(
            "Welcome.md",
            "- # Welcome to the published graph\n  id:: aaaaaaaa-0000-0000-0000-000000000001\n- This is a static export with a **sidebar** and fuzzy [[search]].\n- See [[Parameterized IP]] and the [[ILP Survey]].\n",
        );
    write(
            "Parameterized IP.md",
            "- # Parameterized IP\n- Fixed-parameter tractability of integer programming; **parameterized complexity** of ILPs.\n  id:: aaaaaaaa-0000-0000-0000-000000000002\n- Related to [[ILP Survey]].\n",
        );
    write(
            "ILP Survey.md",
            "- # ILP Survey\n- A survey of integer linear programming techniques and n-fold IP.\n- Back to [[Welcome]].\n",
        );
    write("Search.md", "- Notes on full-text search and ranking.\n");
    write("Project Ideas.md", "- # Project Ideas\n- A grab-bag of ideas, including continuous bribery and opinion diffusion.\n");
    // Exercises every construct the M3 render_html rewrite added to the export:
    // fenced code (highlight.js), tables (data-align), callouts, inline/display math,
    // lists (incl. checkboxes + def-list term), blockquote, org timestamps.
    write(
            "Rendering Showcase.md",
            "- # Rendering Showcase\n\
             - A paragraph with **bold**, *italic*, `inline code`, a [[Welcome]] link and a #demo tag.\n\
             - ```rust\n  fn solve(n: usize) -> usize {\n      (0..n).filter(|x| x & 1 == 0).count()\n  }\n  ```\n\
             - | Method | Time | Note |\n  | :--- | :---: | ---: |\n  | n-fold IP | fast | linear |\n  | brute force | slow | exp |\n\
             - > [!NOTE] Heads up\n  > Callouts now render straight from the AST.\n\
             - > [!WARNING]\n  > Macros are dropped in a static export (they can't run).\n\
             - Inline math $e^{i\\pi}+1=0$ and a display block:\n\
             - $$\\int_0^1 x^2\\,dx = \\tfrac{1}{3}$$\n\
             - Unordered:\n  * first\n  * second\n      * nested\n\
             - Tasks:\n  * [ ] open item\n  * [x] done item\n\
             - Coffee\n  : A hot drink brewed from beans.\n\
             - > A plain blockquote over\n  > two lines.\n\
             - let's try again *(something)* -> --> -- en --- em \\alpha \\Delta\n\
             - A meeting <2026-06-30 Tue 14:00> and a deadline.\n",
        );
    fs::write(
        dir.join("journals").join("2026_06_28.md"),
        "- Worked on the **published export**: sidebar + search.\n- Linked [[Parameterized IP]].\n",
    )
    .unwrap();

    let g = Graph::open(&dir);
    let _projection = prepare_publication_graph(&g);
    let (outdir, count) = publish_graph(&g).unwrap();
    println!("SAMPLE_EXPORT_DIR={outdir} pages={count}");

    // Also emit the single-page print/PDF document for the showcase page, so
    // the print CSS + self-contained render can be eyeballed / screenshotted.
    let print = g
        .page_print_html("Rendering Showcase", PrintOpts::default())
        .unwrap()
        .unwrap();
    let pfile = dir.join("print-sample.html");
    fs::write(&pfile, print).unwrap();
    println!("SAMPLE_PRINT_HTML={}", pfile.display());
}

fn fake_app_bundle() -> Arc<app_export::PublishedAppBundle> {
    Arc::new(app_export::PublishedAppBundle {
            files: vec![
                (
                    "assets/index-abc123.js".into(),
                    b"console.log('bundle');".to_vec(),
                ),
                (
                    "index.html".into(),
                    b"<!doctype html><html><head><meta charset=\"utf-8\"><title>Tine</title>\
<script type=\"module\" src=\"./assets/index-abc123.js\"></script></head><body><div id=\"root\"></div></body></html>"
                        .to_vec(),
                ),
            ],
        })
}

/// Stage 2: a query export with an embedded bundle ships the read-only
/// app beside the static site, over a snapshot computed from the selected
/// pages and nothing else, keyed exactly as the app will ask.
#[test]
fn query_export_ships_the_app_over_a_closed_snapshot() {
    let dir = query_export_fixture("app");
    // Dashboard hosts the export's own query AND a focused-page query;
    // it is exported because its host block does not match but its
    // other block does.
    fs::write(
            dir.join("pages/Dashboard.md"),
            "- TODO dash-task\n- {{query (task TODO)}}\n- {{query (and (task TODO) <% current page %>)}}\n  tine.view:: table\n- {{query (task TODO)}}\n  tine.sample:: 1\n- {{query (task TODO)}}\n  tine.block-display:: 1\n  tine.block-sample:: 2\n",
        )
        .unwrap();
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let mut request = todo_request("Tasks");
    request.app_bundle = Some(fake_app_bundle());
    request.host_properties = vec![
        ("tine.view".into(), "table".into()),
        ("tine.sample".into(), "1".into()),
    ];
    let plan = plan_query_publication(&graph, &request).unwrap();
    let outcome = publish_query(&graph, &request, &plan.fingerprint).unwrap();
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    let out = PathBuf::from(&outcome.path);

    // The bundle, rewritten: marker in <head>, the export's name as title.
    let app_index = fs::read_to_string(out.join("app/index.html")).unwrap();
    assert!(app_index.contains("<head><meta name=\"tine-published\" content=\"snapshot.json\">"));
    assert!(app_index.contains("<title>Tasks</title>"), "{app_index}");
    assert!(!app_index.contains("<title>Tine</title>"));
    assert_eq!(
        fs::read(out.join("app/assets/index-abc123.js")).unwrap(),
        b"console.log('bundle');"
    );
    // The static front door opens the app over HTTP, through a separate
    // script (the shell's CSP forbids inline script), and says so.
    let index = fs::read_to_string(out.join("index.html")).unwrap();
    assert!(
        index.contains("<head><script src=\"app-redirect.js\"></script>"),
        "{index}"
    );
    assert!(index.contains("publish-app-note"), "{index}");
    let redirect = fs::read_to_string(out.join("app-redirect.js")).unwrap();
    assert!(redirect.contains("location.replace(\"app/\""));
    assert!(redirect.contains("static"));

    // The snapshot: exactly the contracted top-level keys.
    let bytes = fs::read(out.join("app/snapshot.json")).unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let mut keys: Vec<&str> = snapshot
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort_unstable();
    let mut expected = app_export::SNAPSHOT_TOP_LEVEL_KEYS.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected);
    assert_eq!(snapshot["schema"], app_export::SNAPSHOT_SCHEMA);
    assert_eq!(snapshot["name"], "Tasks");
    // "Tasks" is a selected page, so the home page steps aside.
    assert_eq!(snapshot["home"], "Tasks (export)");
    let pages = snapshot["pages"].as_array().unwrap();
    assert_eq!(pages[0]["name"], "Tasks (export)");
    assert!(pages.iter().all(|page| page["read_only"] == true));
    let mut names: Vec<&str> = pages.iter().map(|p| p["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "Also Selected",
            "Dashboard",
            "Jan 2nd, 2026",
            "Tasks",
            "Tasks (export)"
        ]
    );
    // Every selected page carries its own graph-relative path; the app
    // opens pages by that path.
    let tasks = pages.iter().find(|p| p["name"] == "Tasks").unwrap();
    assert_eq!(tasks["path"], "pages/Tasks.md");
    let entries = snapshot["entries"].as_array().unwrap();
    assert_eq!(entries[0]["name"], "Tasks (export)");
    assert_eq!(entries.len(), pages.len());
    // The home page's blocks: the macro carrying the host's tine.*
    // properties, then one link per page.
    let home_blocks = pages[0]["blocks"].as_array().unwrap();
    assert_eq!(
        home_blocks[0]["raw"].as_str().unwrap().trim(),
        "{{query (task TODO)}}\ntine.view:: table\ntine.sample:: 1"
    );
    assert_eq!(
        home_blocks[0]["properties"],
        serde_json::json!([["tine.view", "table"], ["tine.sample", "1"]])
    );
    assert_eq!(home_blocks.len(), 1 + 4);
    // Backlinks come from the closed sub-graph only: "Also Selected" is
    // linked from Tasks (selected); Private Notes is nowhere.
    let backlinks = snapshot["backlinks"].as_object().unwrap();
    assert!(backlinks.contains_key("Also Selected"), "{backlinks:?}");
    assert!(!backlinks.contains_key("Private Notes"));

    // Queries: the home query keyed by the home page but executed with
    // the reviewed binding; Dashboard's two macros keyed by Dashboard,
    // the focused one with its substituted execution parse and the
    // host block's tine.* properties.
    let queries = snapshot["queries"].as_array().unwrap();
    let home = queries
        .iter()
        .find(|q| q["host"] == "Tasks (export)")
        .expect("home query recorded");
    assert_eq!(home["argument"], "(task TODO)");
    assert_eq!(home["dialect"], "macro_query");
    assert_eq!(home["context"]["current_page"], "Tasks (export)");
    assert_eq!(home["executed_context"]["current_page"], "Dashboard");
    assert!(home["execution"].is_null());
    assert!(home["result"]["statistics"].is_null());
    assert_eq!(home["result"]["exceeded"], false);
    // The home run is keyed by the host's properties and runs under the
    // view they resolve to: `tine.sample:: 1` bakes one sampled row.
    assert_eq!(
        home["properties"],
        serde_json::json!([["tine.view", "table"], ["tine.sample", "1"]])
    );
    assert_eq!(home["view"]["sample"], 1);
    assert_eq!(home["view"]["view"], "table");
    let home_rows: usize = home["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["blocks"].as_array().unwrap().len())
        .sum();
    assert_eq!(home_rows, 1, "{}", home["result"]);
    let dashboard: Vec<&serde_json::Value> = queries
        .iter()
        .filter(|q| q["host"] == "Dashboard")
        .collect();
    assert_eq!(dashboard.len(), 4, "{queries:?}");
    // Three `(task TODO)` twins on one page are three records, told apart
    // by the view each ran under: unsampled, `tine.sample:: 1`, and the
    // block-scoped `tine.block-sample:: 2` (resolved the way the app
    // resolves it, not the singular merge that would ignore it).
    let twins: Vec<&&serde_json::Value> = dashboard
        .iter()
        .filter(|q| q["argument"] == "(task TODO)")
        .collect();
    assert_eq!(twins.len(), 3);
    let mut samples: Vec<(Option<u64>, usize)> = twins
        .iter()
        .map(|q| {
            let rows: usize = q["result"]["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| g["blocks"].as_array().unwrap().len())
                .sum();
            (q["view"]["sample"].as_u64(), rows)
        })
        .collect();
    samples.sort_unstable();
    assert_eq!(
        samples,
        vec![(None, 4), (Some(1), 1), (Some(2), 2)],
        "{twins:?}"
    );
    let focused = dashboard
        .iter()
        .find(|q| q["argument"].as_str().unwrap().contains("current page"))
        .unwrap();
    assert_eq!(
        focused["execution"]["argument"],
        "(and (task TODO) [[Dashboard]])"
    );
    assert_eq!(focused["properties"][0][0], "tine.view");
    assert_eq!(focused["properties"][0][1], "table");
    assert_eq!(focused["context"]["current_page"], "Dashboard");
    assert_eq!(focused["executed_context"]["current_page"], "Dashboard");
    assert_eq!(focused["result"]["total"], 1);

    // Nothing outside the selection reaches the snapshot: not the
    // unselected page's text, not its path.
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("private-sentinel-text"));
    assert!(!text.contains("Private Notes.md"));
    assert!(!text.contains("not-a-todo"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn query_export_without_an_embedded_frontend_warns_and_ships_no_app() {
    let dir = query_export_fixture("no-app");
    let graph = Graph::open(&dir);
    let _projection = prepare_publication_graph(&graph);
    let mut request = todo_request("Plain");
    request.app_bundle = Some(Arc::new(app_export::PublishedAppBundle::default()));
    let plan = plan_query_publication(&graph, &request).unwrap();
    let outcome = publish_query(&graph, &request, &plan.fingerprint).unwrap();
    assert_eq!(
        outcome.warnings,
        vec![app_export::NO_BUNDLE_WARNING.to_string()]
    );
    let out = PathBuf::from(&outcome.path);
    assert!(!out.join("app").exists());
    assert!(!out.join("app-redirect.js").exists());
    let index = fs::read_to_string(out.join("index.html")).unwrap();
    assert!(!index.contains("app-redirect.js"));
    // A bundle whose shell cannot carry the marker is refused whole.
    let mut request = todo_request("Broken");
    request.app_bundle = Some(Arc::new(app_export::PublishedAppBundle {
        files: vec![("index.html".into(), b"<html><body></body></html>".to_vec())],
    }));
    let plan = plan_query_publication(&graph, &request).unwrap();
    let error = publish_query(&graph, &request, &plan.fingerprint).unwrap_err();
    assert!(error.to_string().contains("not exportable"), "{error}");
    assert!(!dir.join("published-queries/broken").exists());
    let _ = fs::remove_dir_all(&dir);
}
