use super::*;

// Fixed "today" so relative-date tests are deterministic: 2026-06-16.
const TODAY: JournalDate = JournalDate {
    year: 2026,
    month: 6,
    day: 16,
};

/// The block-anchored evaluable filter of an OG `{{query}}` source — what
/// the legacy `run_query` path evaluates.
fn pred(src: &str) -> Filter {
    let (query, _view) = parse_query_source(src, TODAY);
    assert!(
        !query.is_invalid(),
        "{src} parsed with diagnostics: {:?}",
        query.diagnostics
    );
    match query.anchor {
        Anchor::Block => query.evaluable_filter(),
        Anchor::Page => og::rebase_to_block(&query.evaluable_filter()),
    }
}

fn view_of(src: &str) -> ViewSettings {
    parse_query_source(src, TODAY).1
}

/// Where a block sits, for the leaf tests. `EvalCtx` borrows its
/// compiled-pattern table, which is per FILTER rather than per page, so the
/// two are assembled together in [`TestEval::eval`].
struct Place {
    journal: Option<i64>,
    is_journal: bool,
    page_name: String,
    page_props: Vec<(String, String)>,
}

fn ctx_named<'a>() -> Place {
    Place {
        journal: None,
        is_journal: false,
        page_name: "Test".into(),
        page_props: Vec::new(),
    }
}
fn ctx_journal<'a>(key: i64) -> Place {
    Place {
        journal: Some(key),
        is_journal: true,
        page_name: "Journal".into(),
        page_props: Vec::new(),
    }
}
fn ctx_page(name: &str, props: &[(&str, &str)]) -> Place {
    Place {
        journal: None,
        is_journal: false,
        page_name: name.into(),
        page_props: props
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    }
}

trait TestEval {
    fn eval(&self, block: &DocBlock, place: &Place) -> bool;
}

impl TestEval for Filter {
    fn eval(&self, block: &DocBlock, place: &Place) -> bool {
        let compiled = compiled::CompiledLeaves::for_query(self);
        let config = crate::config::ParseConfig::default();
        let registry = registry::Registry::empty(&config);
        let ctx = EvalCtx {
            journal: place.journal,
            is_journal: place.is_journal,
            page_name: &place.page_name,
            page_props: &place.page_props,
            page_roots: std::slice::from_ref(block),
            today: TODAY,
            compiled: &compiled,
            format: crate::query::atom::AtomFormat::Markdown,
            config: &config,
            registry: &registry,
            mode: atom::CompareMode::Both,
        };
        eval::eval_block(self, block, &PathRefCounts::new(), &ctx)
    }
}

// Expected-value constructors, so the assertions below read like the DSL
// they parse rather than like the IR's constructors.
fn e_page_ref(name: &str) -> Filter {
    Filter::page_ref(name)
}
fn e_task(markers: &[&str]) -> Filter {
    Filter::attr(
        Attr::Task,
        CmpOp::In,
        Value::List {
            items: markers.iter().map(|m| Value::text(*m)).collect(),
        },
    )
}
fn e_property(key: &str, value: Option<&str>) -> Filter {
    og::property_leaf(key.to_string(), value.map(str::to_string))
}
fn e_page_property(key: &str, value: Option<&str>) -> Filter {
    Filter::rel(Rel::Page, Quant::Any, e_property(key, value))
}
fn e_content(text: &str) -> Filter {
    og::content_like(text)
}
fn e_journal_between(low: &str, high: &str) -> Filter {
    Filter::rel(
        Rel::Page,
        Quant::Any,
        Filter::attr(
            Attr::Day,
            CmpOp::Between,
            Value::List {
                items: vec![Value::date(low), Value::date(high)],
            },
        ),
    )
}

fn nested_boolean(head: &str, depth: usize, leaf: &str) -> String {
    format!(
        "{}{}{}",
        format!("({head} ").repeat(depth),
        leaf,
        ")".repeat(depth)
    )
}

fn wait_for_backlink_filter_projection(graph: &Graph) {
    let started = std::time::Instant::now();
    while !graph.direct_projection_ready_test() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "projection did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn query_parsers_fail_closed_past_the_shared_depth_and_size_limits() {
    let simple_at_limit = nested_boolean("and", QUERY_NESTING_MAX - 1, "(task TODO)");
    assert!(!parse_query_source(&simple_at_limit, TODAY).0.is_invalid());
    let simple_too_deep = nested_boolean("and", QUERY_NESTING_MAX, "(task TODO)");
    assert!(parse_query_source(&simple_too_deep, TODAY).0.is_invalid());

    let advanced_at_limit = format!(
        "[:find (pull ?b [*]) :where {}]",
        nested_boolean("and", QUERY_NESTING_MAX - 1, "(task ?b #{\"TODO\"})")
    );
    let (accepted, _, rejected) = advanced_pred(&advanced_at_limit, None, TODAY);
    assert!(
        accepted.is_some(),
        "unexpected ignored clauses: {rejected:?}"
    );
    let advanced_too_deep = format!(
        "[:find (pull ?b [*]) :where {}]",
        nested_boolean("and", QUERY_NESTING_MAX, "(task ?b #{\"TODO\"})")
    );
    let (rejected, ran, ignored) = advanced_pred(&advanced_too_deep, None, TODAY);
    assert!(rejected.is_none());
    assert!(ran.is_empty());
    assert!(ignored.iter().any(|item| item == "query-nesting-too-deep"));

    let oversized = "x".repeat(QUERY_SOURCE_MAX_BYTES + 1);
    assert!(!query_source_within_limit(&oversized));
    assert!(parse_query_source(&oversized, TODAY).0.is_invalid());

    // The advanced parser must fail closed on size too, at `advanced_pred`
    // itself. `page_affects_advanced_query` calls it directly, so a ceiling
    // enforced only at the `run_advanced_*` entry points is not the shared
    // ceiling this module's doc comment promises.
    let oversized_advanced = format!(
        "[:find (pull ?b [*]) :where (property ?b :note \"{}\")]",
        "y".repeat(QUERY_SOURCE_MAX_BYTES)
    );
    assert!(!query_source_within_limit(&oversized_advanced));
    let (rejected, ran, ignored) = advanced_pred(&oversized_advanced, None, TODAY);
    assert!(rejected.is_none());
    assert!(ran.is_empty());
    assert!(ignored.iter().any(|item| item == "query-too-large"));

    let harmless = format!("(and (content \"{}\"))", "(".repeat(QUERY_NESTING_MAX + 10));
    assert!(query_nesting_within_limit(&harmless));

    let simple_semicolon = format!(";{}", "(".repeat(QUERY_NESTING_MAX + 1));
    assert!(
        !query_nesting_within_limit(&simple_semicolon),
        "semicolon is ordinary text, not a comment, in the simple DSL"
    );
    let advanced_comment = format!(
        "[:find ?b :where ;; {}\n(task ?b #{{\"TODO\"}})]",
        "(".repeat(QUERY_NESTING_MAX + 1)
    );
    assert!(
        query_nesting_within_limit(&advanced_comment),
        "advanced EDN comments must not count delimiter text"
    );
}

#[test]
fn backlink_filter_context_indexes_visible_descendants_and_parser_owned_facets() {
    use std::fs;

    const ROOT: &str = "12345678-1234-4234-8234-123456789abc";
    let dir = std::env::temp_dir().join(format!(
        "tine-backlink-filter-context-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::write(
            dir.join("pages/Source.md"),
            format!(
                "- Parent [[Target]]\n  id:: {ROOT}\n  - A descendant carries the exact needle [[Other]] #tag\n    tags:: Team\n  - TODO parser-owned task state\n  - ```\n    [[CodeOnly]]\n    ```\n"
            ),
        )
        .unwrap();

    let graph = Graph::open(&dir);
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    graph.warm_cache();
    wait_for_backlink_filter_projection(&graph);
    let runtime_id = graph.backlinks("Target")[0].blocks[0].id.clone();
    let targets = [
        BacklinkFilterTarget {
            page: "Source".into(),
            kind: PageKind::Page,
            block_id: runtime_id.clone(),
        },
        // Defensive duplicate input must not make a complete response
        // look truncated or duplicate its payload.
        BacklinkFilterTarget {
            page: "Source".into(),
            kind: PageKind::Page,
            block_id: runtime_id,
        },
    ];
    let context = backlink_filter_context(&graph, "Target", &targets, "\"exact needle\"").unwrap();

    assert!(!context.truncated);
    assert_eq!(context.entries.len(), 1);
    let entry = &context.entries[0];
    assert!(
        entry.text_matches,
        "visible descendant text must match natively"
    );
    let facets = entry
        .facets
        .iter()
        .map(|facet| refs::normalize(facet))
        .collect::<std::collections::HashSet<_>>();
    for expected in ["other", "tag", "team", "todo"] {
        assert!(
            facets.contains(expected),
            "missing {expected}: {:?}",
            entry.facets
        );
    }
    assert!(!facets.contains("target"));
    assert!(
        !facets.contains("codeonly"),
        "code-fence text is not a reference facet"
    );

    let matches = |search: &str| {
        backlink_filter_context(&graph, "Target", &targets, search)
            .unwrap()
            .entries[0]
            .text_matches
    };
    for search in [
        "exact needle",
        "needle exact",
        "missing OR needle",
        "\"exact needle\"",
        "/exact needle/",
    ] {
        assert!(matches(search), "shared Matcher should accept {search:?}");
    }
    for search in [
        "needle missing",
        "needle -descendant",
        "\"needle exact\"",
        "/Exact needle/",
        "id::",
    ] {
        assert!(!matches(search), "shared Matcher should reject {search:?}");
    }

    for search in ["", "   ", "-needle"] {
        let empty = backlink_filter_context(&graph, "Target", &targets, search).unwrap();
        assert!(empty.search_error.is_none());
        assert!(
            empty.entries[0].text_matches,
            "empty search remains unfiltered"
        );
    }
    let invalid = backlink_filter_context(&graph, "Target", &targets, "/[/").unwrap();
    assert!(invalid.search_error.is_some());
    assert!(
        invalid.entries[0].text_matches,
        "invalid search reports its parse error without hiding roots"
    );
    let mut missing_targets = targets.to_vec();
    missing_targets.push(BacklinkFilterTarget {
        page: "Source".into(),
        kind: PageKind::Page,
        block_id: "missing-or-stale-root".into(),
    });
    let missing = backlink_filter_context(&graph, "Target", &missing_targets, "").unwrap();
    assert_eq!(missing.entries.len(), 1);
    assert!(
        missing.truncated,
        "a missing requested root stays visible through the frontend fallback"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn backlink_filter_context_scopes_alias_cycles_collisions_and_hydration() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-backlink-filter-alias-scope-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(dir.join("pages/A.md"), "alias:: B\n\n- A\n").unwrap();
    fs::write(dir.join("pages/B.md"), "alias:: C\n\n- B\n").unwrap();
    fs::write(dir.join("pages/C.md"), "alias:: A\n\n- C\n").unwrap();
    fs::write(dir.join("pages/Z.md"), "alias:: B\n\n- Z\n").unwrap();
    fs::write(
        dir.join("pages/SourceOne.md"),
        "- exact needle [[A]] [[B]] [[b]] [[Outside]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/SourceTwo.md"),
        "- exact needle [[C]] [[Z]]\n",
    )
    .unwrap();
    for index in 0..32 {
        fs::write(
            dir.join("pages").join(format!("Unrelated{index}.md")),
            "- unrelated\n",
        )
        .unwrap();
    }

    let graph = Graph::open(&dir);
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    graph.warm_cache();
    wait_for_backlink_filter_projection(&graph);
    let targets = graph.with_pages(|pages| {
        pages
            .iter()
            .filter(|(entry, _)| entry.name.starts_with("Source"))
            .map(|(entry, document)| BacklinkFilterTarget {
                page: entry.name.clone(),
                kind: entry.kind,
                block_id: document.roots[0].uuid.clone(),
            })
            .collect::<Vec<_>>()
    });
    graph.reset_direct_projection_candidate_probe_test();

    let context = backlink_filter_context(&graph, "B", &targets, "exact needle").expect(
        "a differently cased dictionary spelling must not produce a null-path/text-name row",
    );
    assert_eq!(context.entries.len(), 2);
    assert!(context.entries.iter().all(|entry| entry.text_matches));
    let facets = context
        .entries
        .iter()
        .flat_map(|entry| entry.facets.iter())
        .map(|facet| refs::page_key(facet))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        facets,
        std::collections::HashSet::from(["outside".to_string()])
    );
    let mut hydrated = graph.direct_projection_hydrated_pages_test();
    hydrated.sort();
    assert_eq!(
        hydrated,
        vec![
            std::path::PathBuf::from("pages/SourceOne.md"),
            std::path::PathBuf::from("pages/SourceTwo.md"),
        ]
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn backlink_filter_context_hydrates_duplicate_physical_names_and_journal_spellings() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-backlink-filter-duplicates-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::write(dir.join("pages/Same.md"), "- duplicate needle\n").unwrap();
    fs::write(dir.join("pages/same.markdown"), "- duplicate needle\n").unwrap();
    fs::write(dir.join("journals/2026_09_20.md"), "- journal\n").unwrap();
    let day = JournalDate {
        year: 2026,
        month: 9,
        day: 20,
    };
    let journal_title = JournalFormat::new(None, None).title(day);
    fs::write(
        dir.join("pages/JournalSource.md"),
        format!("- journal needle [[{journal_title}]] [[Outside]]\n"),
    )
    .unwrap();

    let graph = Graph::open(&dir);
    graph
        .attach_direct_projection(dir.join("private/projection.sqlite"))
        .unwrap();
    graph.warm_cache();
    wait_for_backlink_filter_projection(&graph);
    let (duplicate_targets, journal_target) = graph.with_pages(|pages| {
        let duplicates = pages
            .iter()
            .filter(|(entry, _)| refs::page_key(&entry.name) == "same")
            .map(|(entry, document)| BacklinkFilterTarget {
                page: "same".into(),
                kind: entry.kind,
                block_id: document.roots[0].uuid.clone(),
            })
            .collect::<Vec<_>>();
        let journal = pages
            .iter()
            .find(|(entry, _)| entry.name == "JournalSource")
            .map(|(entry, document)| BacklinkFilterTarget {
                page: entry.name.clone(),
                kind: entry.kind,
                block_id: document.roots[0].uuid.clone(),
            })
            .unwrap();
        (duplicates, journal)
    });
    assert_eq!(
        duplicate_targets.len(),
        2,
        "fixture needs duplicate physical names"
    );

    graph.reset_direct_projection_candidate_probe_test();
    let duplicates =
        backlink_filter_context(&graph, "Target", &duplicate_targets, "duplicate needle").unwrap();
    assert_eq!(duplicates.entries.len(), 2);
    let mut hydrated = graph.direct_projection_hydrated_pages_test();
    hydrated.sort();
    assert_eq!(
        hydrated,
        vec![
            std::path::PathBuf::from("pages/Same.md"),
            std::path::PathBuf::from("pages/same.markdown"),
        ]
    );

    let journal =
        backlink_filter_context(&graph, "2026-09-20", &[journal_target], "journal needle").unwrap();
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(
        journal.entries[0]
            .facets
            .iter()
            .map(|facet| refs::page_key(facet))
            .collect::<Vec<_>>(),
        vec!["outside"]
    );

    graph.direct_projection_set_source_revision_test(
        &dir.join("pages/JournalSource.md"),
        "stale-test-revision",
    );
    assert!(matches!(
        backlink_filter_context(
            &graph,
            "2026-09-20",
            &[BacklinkFilterTarget {
                page: "JournalSource".into(),
                kind: PageKind::Page,
                block_id: journal.entries[0].block_id.clone(),
            }],
            "journal needle",
        ),
        Err(QueryExecutionError::NotReady(
            QueryReadinessReason::PendingEdits
        ))
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn backlink_filter_scoped_journal_names_match_effective_title_resolver() {
    use std::fs;

    let day_20 = JournalDate {
        year: 2026,
        month: 9,
        day: 20,
    };
    let day_21 = JournalDate {
        year: 2026,
        month: 9,
        day: 21,
    };

    for (case, config, title_format) in [
        ("default", None, None),
        (
            "custom",
            Some(
                "{:journal/file-name-format \"yyyy_MM_dd\"\n\
                  :journal/page-title-format \"dd-MM-yyyy\"}\n",
            ),
            Some("dd-MM-yyyy"),
        ),
    ] {
        let dir = std::env::temp_dir().join(format!(
            "tine-backlink-filter-journal-title-{case}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("journals")).unwrap();
        if let Some(config) = config {
            fs::create_dir_all(dir.join("logseq")).unwrap();
            fs::write(dir.join("logseq/config.edn"), config).unwrap();
        }
        let format = JournalFormat::new(Some("yyyy_MM_dd"), title_format);
        let effective_title = format.title(day_21);
        fs::write(
            dir.join("journals/2026_09_20.md"),
            format!("title:: {effective_title}\n\n- first journal\n"),
        )
        .unwrap();
        fs::write(
            dir.join("journals/2026_09_19.md"),
            format!("title:: {effective_title}\n\n- duplicate effective journal\n"),
        )
        .unwrap();

        let graph = Graph::open(&dir);
        graph
            .attach_direct_projection(dir.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_for_backlink_filter_projection(&graph);
        assert_eq!(
            graph
                .list_pages()
                .iter()
                .filter(|entry| entry.kind == PageKind::Journal && entry.name == effective_title)
                .count(),
            2,
            "fixture must expose duplicate effective journal names"
        );

        let aliases = graph.page_aliases();
        for target in [format.title(day_20), format.title(day_21)] {
            let established = graph_equivalent_page_names(&graph, &aliases, &target).1;
            let scoped = graph
                .backlink_filter_scope(&target, &[])
                .unwrap()
                .names_norm;
            assert_eq!(
                scoped, established,
                "scoped journal equivalence drifted for {case} target {target}"
            );
        }

        let day_20_names = graph
            .backlink_filter_scope(&format.title(day_20), &[])
            .unwrap()
            .names_norm;
        assert_eq!(
            day_20_names,
            vec![refs::page_key(&format.title(day_20))],
            "the physical Sep 20 filename must not override the effective Sep 21 title"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn backlink_filter_context_source_keeps_projection_setup_scoped() {
    let query_source = crate::test_support::rust_module_production_files("query.rs")
        .into_iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let start = query_source.find("pub fn backlink_filter_context").unwrap();
    let body = &query_source[start
        ..query_source[start..]
            .find("\n}\n")
            .map(|end| start + end + 3)
            .unwrap()];
    for forbidden in [
        ".page_aliases(",
        "graph_equivalent_page_names(",
        "reference_candidate_pages_indexed(",
        ".with_pages(",
        ".list_pages(",
    ] {
        assert!(
            !body.contains(forbidden),
            "per-search filter setup must not use the graph-wide route: {forbidden}"
        );
    }

    let direct_source = include_str!("model/direct_query.rs");
    for required in [
        "INDEXED BY pages_name_idx",
        "WHERE page_name.key = ?1 AND page.text_kind = 1",
        "INDEXED BY reference_alias_declarations_source_idx",
        "INDEXED BY reference_alias_declarations_name_idx",
        "WITH requested(text_kind, name_key) AS (VALUES",
        "direct_projection_pages_for_sources",
    ] {
        assert!(
            direct_source.contains(required),
            "scoped projection route lost {required}"
        );
    }
}

#[test]
fn parse_pageref_and_tag() {
    assert_eq!(pred("[[Foo]]"), e_page_ref("Foo"));
    assert_eq!(pred("#bar"), e_page_ref("bar"));
    assert_eq!(pred("(tag Foo)"), e_page_ref("Foo"));
}

#[test]
fn parse_boolean() {
    assert_eq!(
        pred("(and [[A]] [[B]])"),
        Filter::and(vec![e_page_ref("A"), e_page_ref("B")])
    );
    assert_eq!(pred("(not [[A]])"), Filter::not(e_page_ref("A")));
}

#[test]
fn parse_task_and_property() {
    assert_eq!(pred("(task TODO DOING)"), e_task(&["TODO", "DOING"]));
    assert_eq!(
        pred("(property type book)"),
        e_property("type", Some("book"))
    );
    assert_eq!(pred("(property public)"), e_property("public", None));
}

#[test]
fn property_key_and_ref_value_match_logseq() {
    // Leading `:` on the key is stripped (keyword form == symbol form).
    assert_eq!(
        pred("(property :type book)"),
        e_property("type", Some("book"))
    );
    // `_` → `-` (Logseq stores `my_key` as `my-key`).
    assert_eq!(pred("(property my_key v)"), e_property("my-key", Some("v")));
    // A `[[page]]` value is captured (was dropped, leaking a stray page-ref).
    assert_eq!(
        pred("(property :fach [[Foo Bar]])"),
        e_property("fach", Some("Foo Bar"))
    );
    // A `#tag` value is captured.
    assert_eq!(
        pred("(property :type #assignment)"),
        e_property("type", Some("assignment"))
    );
    // page-property mirrors the same normalization + value capture.
    assert_eq!(
        pred("(page-property :fach [[Foo]])"),
        e_page_property("fach", Some("Foo"))
    );
}

#[test]
fn reported_and_of_colon_properties_parses_both_clauses() {
    // GH: `(and (property :fach [[X]]) (property :type "#assignment"))` used to
    // parse to And[Property(":fach", None), PageRef(X)] — the colon key never
    // matched, the ref leaked, and the second clause was dropped → "No results".
    let p = pred(
        r##"(and (property :fach [[Management der digitalen Transformation]]) (property :type "#assignment"))"##,
    );
    assert_eq!(
        p,
        Filter::and(vec![
            e_property("fach", Some("Management der digitalen Transformation")),
            // OG's `parse-property-value` strips the leading `#` of a tag
            // spelling, quoted or not.
            e_property("type", Some("assignment")),
        ])
    );
}

#[test]
fn eval_colon_property_and_query_matches_block() {
    let none = ctx_named();
    let mut b = DocBlock::new("assignment one");
    b.raw
        .push_str("\nfach:: [[Management der digitalen Transformation]]\ntype:: #assignment");
    // The reported query now matches a block carrying both properties.
    assert!(pred(
            r##"(and (property :fach [[Management der digitalen Transformation]]) (property :type "#assignment"))"##
        )
        .eval(&b, &none));
    // A different course value does not match.
    assert!(!pred("(property :fach [[Other Course]])").eval(&b, &none));
    // Colon-less form still works (unchanged behavior).
    assert!(pred("(property type assignment)").eval(&b, &none));
}

#[test]
fn parse_escaped_string_content() {
    // `\"`/`\\` inside a quoted full-text term are unescaped (mirrors the
    // query-builder serializer's quoteStr), so a quote in the term doesn't
    // end the string early and silently truncate the query.
    assert_eq!(pred("\"foo \\\"bar\\\"\""), e_content("foo \"bar\""));
    assert_eq!(pred("\"a\\\\b\""), e_content("a\\b"));
    // Only `\"`/`\\` are escapes: a hand-authored backslash before another
    // char is literal, so `"C:\tmp"` stays `C:\tmp` (not `C:tmp`). The term
    // is case-folded at parse time (the content match is case-insensitive).
    assert_eq!(pred("\"a\\q\""), e_content("a\\q"));
    // The IR keeps the user's spelling; the case fold happens at evaluation
    // so the printer can re-emit the term byte-for-byte.
    assert_eq!(pred("\"C:\\tmp\""), e_content("C:\\tmp"));
    // End-to-end: the term still matches a block whose text contains the quote.
    let none = ctx_named();
    let b = DocBlock::new("note: foo \"bar\" baz");
    assert!(pred("\"foo \\\"bar\\\"\"").eval(&b, &none));
}

#[test]
fn search_predicate_preserves_escaped_friendly_source_and_evaluates_it() {
    let parsed = pred(r#"(search "foo \"exact phrase\" -draft OR C:\\tmp")"#);
    assert_eq!(
        parsed,
        Filter::attr(
            Attr::Content,
            CmpOp::Match,
            Value::text(r#"foo "exact phrase" -draft OR C:\tmp"#)
        )
    );

    let none = ctx_named();
    assert!(parsed.eval(&DocBlock::new("foo and an exact phrase, ready"), &none));
    assert!(!parsed.eval(&DocBlock::new("foo and an exact phrase, but draft"), &none));
    // The decoded backslash is passed losslessly to the friendly parser;
    // the second OR branch can therefore match a Windows-style path.
    assert!(parsed.eval(&DocBlock::new(r"open C:\tmp\notes"), &none));

    // The predicate remains an ordinary composable query-DSL clause.
    let task_search = pred(r#"(and (task TODO) (search "foo -draft"))"#);
    assert!(task_search.eval(&DocBlock::new("TODO foo ready"), &none));
    assert!(!task_search.eval(&DocBlock::new("DONE foo ready"), &none));
}

#[test]
fn content_regex_preserves_escapes_and_invalid_patterns_match_nothing() {
    let parsed = pred(r#"(content-regex "ID:\\s+[A-Z]{3}\\d+\\s+\"quoted\"")"#);
    assert_eq!(
        parsed,
        Filter::attr(
            Attr::Content,
            CmpOp::Regex,
            Value::text(r#"ID:\s+[A-Z]{3}\d+\s+"quoted""#)
        )
    );

    let none = ctx_named();
    assert!(parsed.eval(&DocBlock::new(r#"prefix ID: ABC42 "quoted" suffix"#), &none));
    // Rust regex matching is intentionally case-sensitive.
    assert!(!parsed.eval(&DocBlock::new(r#"prefix ID: abc42 "quoted" suffix"#), &none));

    let invalid = pred(r#"(content-regex "[unclosed")"#);
    assert!(matches!(
        invalid,
        Filter::Leaf {
            leaf: Leaf::Attr {
                op: CmpOp::Regex,
                ..
            }
        }
    ));
    assert!(!invalid.eval(&DocBlock::new("[unclosed"), &none));
}

#[test]
fn aggregate_and_group_by_parse_as_noop_filters() {
    // 1a: the aggregation/grouping directives ride in the DSL (D2) so the
    // builder round-trips and run_query succeeds; they never filter (eval→true).
    use ir::{AggFn, Field};
    // D2: the four directives are LIFTED out of the filter into the view;
    // what stays in the tree is the neutral `True`.
    assert_eq!(pred("(aggregate count)"), Filter::True);
    assert_eq!(
        view_of("(aggregate count)").aggregates,
        vec![(Field::new(""), AggFn::Count)]
    );
    assert_eq!(
        view_of("(aggregate sum hours)").aggregates,
        vec![(Field::new("hours"), AggFn::Sum)]
    );
    assert_eq!(
        view_of("(aggregate avg score)").aggregates,
        vec![(Field::new("score"), AggFn::Avg)]
    );
    assert_eq!(
        view_of("(group-by page)").group_by,
        Some(Field::new("page"))
    );
    assert_eq!(
        view_of("(group-by status)").group_by,
        Some(Field::new("status"))
    );

    // No-op filter: a block passes regardless.
    let none = ctx_named();
    let b = DocBlock::new("just a note");
    assert!(pred("(aggregate count)").eval(&b, &none));
    assert!(pred("(group-by page)").eval(&b, &none));
    // Combined with a real filter, the aggregate doesn't restrict the matches.
    let task = DocBlock::new("TODO ship it");
    assert!(pred("(and (task TODO) (aggregate count))").eval(&task, &none));
    assert!(!pred("(and (task DONE) (aggregate count))").eval(&task, &none));
}

#[test]
fn advanced_datalog_is_unsupported() {
    assert!(is_advanced(
        "[:find (pull ?b [*]) :where [?b :block/marker]]"
    ));
    assert!(parse_query_source("[:find ?b :where ...]", TODAY)
        .0
        .is_invalid());
}

#[test]
fn advanced_exact_page_property_pair_matches_page_property_predicate() {
    let source = r#"[:find (pull ?p [*])
                         :where
                         [?p :block/properties ?props]
                         [(get ?props :class)]]"#;
    let (lowered, ran, ignored) = advanced_pred(source, None, TODAY);

    assert_eq!(
        lowered.map(|query| query.filter),
        Some(pred("(page-property :class)"))
    );
    assert_eq!(ran, vec!["page-property"]);
    assert!(ignored.is_empty());
}

#[test]
fn advanced_current_page_input_lowers_the_standard_page_relationship() {
    // Logseq graph-parser revision 6e7afa8eb040686ff057156ee877193b581dd369
    // resolves the typed :current-page keyword positionally through
    // current-page-fn and lowercases it before DataScript execution.
    let refs = r#"{:query [:find (pull ?b [*])
                              :in $ ?current-page
                              :where
                              [?p :block/name ?current-page]
                              [?b :block/refs ?p]]
                      :inputs [:current-page]}"#;
    let (lowered, ran, ignored) = advanced_pred(refs, Some("Focus A"), TODAY);

    assert_eq!(
        lowered.map(|query| query.filter),
        Some(e_page_ref("focus a"))
    );
    assert_eq!(ran, vec!["current-page-ref"]);
    assert!(ignored.is_empty());

    let physical = refs.replace(":block/refs", ":block/page");
    let (lowered, ran, ignored) = advanced_pred(&physical, Some("Focus A"), TODAY);
    assert_eq!(
        lowered.map(|query| query.filter),
        Some(Filter::rel(
            Rel::Page,
            Quant::Any,
            Filter::attr(Attr::Name, CmpOp::Eq, Value::text("focus a"))
        ))
    );
    assert_eq!(ran, vec!["current-page"]);
    assert!(ignored.is_empty());
}

#[test]
fn advanced_typed_inputs_keep_date_bounds_numeric() {
    let source = r#"[:find (pull ?b [*])
                         :in $ ?start ?end
                         :where (between ?b ?start ?end)]
                        :inputs [2026-06-01 2026-06-30]"#;
    let (lowered, ran, ignored) = advanced_pred(source, Some("Not a date"), TODAY);

    assert_eq!(
        lowered.map(|query| query.filter),
        Some(Filter::rel(
            Rel::Page,
            Quant::Any,
            adv_range(Attr::Day, Some(20260601), Some(20260630))
        ))
    );
    assert_eq!(ran, vec!["between"]);
    assert!(ignored.is_empty());
}

#[test]
fn advanced_unrelated_bracket_pattern_stays_unsupported() {
    let source = r#"[:find (pull ?p [*])
                         :where
                         [?p :block/name ?name]
                         [(get ?name :class)]]"#;
    let (lowered, ran, ignored) = advanced_pred(source, None, TODAY);

    assert!(lowered.is_none());
    assert!(ran.is_empty());
    assert_eq!(ignored, vec!["pattern", "pattern"]);
}

#[test]
fn eval_against_blocks() {
    let none = ctx_named();
    let task = DocBlock::new("TODO buy milk for [[Home]]");
    assert!(pred("(task TODO)").eval(&task, &none));
    assert!(pred("[[Home]]").eval(&task, &none));
    assert!(pred("(and (task TODO) [[Home]])").eval(&task, &none));
    assert!(!pred("(and (task DONE) [[Home]])").eval(&task, &none));
    assert!(pred("(not [[Work]])").eval(&task, &none));

    let mut withprop = DocBlock::new("a book");
    withprop.raw.push_str("\ntype:: book");
    assert!(pred("(property type book)").eval(&withprop, &none));
    assert!(pred("(property type)").eval(&withprop, &none));
    assert!(!pred("(property type article)").eval(&withprop, &none));
}

#[test]
fn eval_between_journal_titles() {
    let on_2022 = ctx_journal(20220615);
    let on_2019 = ctx_journal(20190101);
    let b = DocBlock::new("TODO something");
    let q = pred("(between [[Jan 1st, 2021]] [[Jan 1st, 2100]])");
    assert!(q.eval(&b, &on_2022));
    assert!(!q.eval(&b, &on_2019));
    let sched = DocBlock::new("TODO x\nSCHEDULED: <2022-03-03 Thu>");
    assert!(!q.eval(&sched, &ctx_named()));
    assert!(pred("(between any [[Jan 1st, 2021]] [[Jan 1st, 2100]])").eval(&sched, &ctx_named()));
}

/// The unqualified two-bound form is OG's journal-page range. Scheduled and
/// deadline ranges remain available through their explicit field selectors;
/// Tine's former permissive union is retained only as explicit `any`.
#[test]
fn og_unqualified_between_is_bounded_to_journal_pages() {
    use std::fs;

    const DEC_5_A: &str = "44444444-4444-4444-8444-444444444441";
    const DEC_5_B: &str = "44444444-4444-4444-8444-444444444442";
    const DEC_7_A: &str = "44444444-4444-4444-8444-444444444443";
    const DEC_7_B: &str = "44444444-4444-4444-8444-444444444444";
    const OUTSIDE_JOURNAL: &str = "55555555-5555-4555-8555-555555555555";
    const NAMED_SCHEDULED: &str = "66666666-6666-4666-8666-666666666666";
    let dir = std::env::temp_dir().join(format!(
        "tine-og-between-journal-bounds-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::write(
        dir.join("journals/2020_12_05.md"),
        format!("- first in range\n  id:: {DEC_5_A}\n- second in range\n  id:: {DEC_5_B}\n"),
    )
    .unwrap();
    fs::write(
        dir.join("journals/2020_12_07.md"),
        format!("- third in range\n  id:: {DEC_7_A}\n- fourth in range\n  id:: {DEC_7_B}\n"),
    )
    .unwrap();
    // Both rows have an in-range planning timestamp but live outside the
    // requested journal-page interval. The old Any default leaked them.
    fs::write(
        dir.join("journals/2021_07_01.md"),
        format!("- outside journal\n  SCHEDULED: <2020-12-06 Sun>\n  id:: {OUTSIDE_JOURNAL}\n"),
    )
    .unwrap();
    fs::write(
        dir.join("pages/Named.md"),
        format!("- named scheduled\n  DEADLINE: <2020-12-06 Sun>\n  id:: {NAMED_SCHEDULED}\n"),
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let ids = run_query(&graph, "(between [[Dec 5th, 2020]] [[Dec 7th, 2020]])")
        .into_iter()
        .flat_map(|group| group.blocks.into_iter().map(persisted_dto_id))
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            DEC_5_A.to_string(),
            DEC_5_B.to_string(),
            DEC_7_A.to_string(),
            DEC_7_B.to_string(),
        ]
    );

    // Reversed bounds are normalized by OG's build-between-two-arg.
    let reversed = run_query(&graph, "(between [[Dec 7th, 2020]] [[Dec 5th, 2020]])");
    assert_eq!(
        reversed
            .iter()
            .map(|group| group.blocks.len())
            .sum::<usize>(),
        4
    );
    // Tine's union remains explicitly requestable.
    let any = run_query(&graph, "(between any [[Dec 5th, 2020]] [[Dec 7th, 2020]])");
    assert_eq!(any.iter().map(|group| group.blocks.len()).sum::<usize>(), 6);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn eval_between_relative_dates() {
    // TODAY = 2026-06-16. (between -7d +7d) => [2026-06-09, 2026-06-23].
    let q = pred("(between -7d +7d)");
    // The IR keeps the bounds UNRESOLVED; `today` is applied at evaluation.
    assert_eq!(q, e_journal_between("-7d", "+7d"));
    let b = DocBlock::new("x");
    assert!(q.eval(&b, &ctx_journal(20260616)));
    assert!(q.eval(&b, &ctx_journal(20260609)));
    assert!(!q.eval(&b, &ctx_journal(20260601)));
    // keyword bounds + month/year units
    assert_eq!(
        pred("(between today tomorrow)"),
        e_journal_between("today", "tomorrow")
    );
    let b = DocBlock::new("x");
    assert!(pred("(between today tomorrow)").eval(&b, &ctx_journal(20260617)));
    assert!(!pred("(between today tomorrow)").eval(&b, &ctx_journal(20260618)));
    assert_eq!(pred("(between -1m +1y)"), e_journal_between("-1m", "+1y"));
    assert!(pred("(between -1m +1y)").eval(&b, &ctx_journal(20260516)));
    assert!(!pred("(between -1m +1y)").eval(&b, &ctx_journal(20260515)));
}

#[test]
fn between_field_selector_and_journal_only() {
    // Field keyword parses into the right variant.
    assert_eq!(
        pred("(between journal -30d today)"),
        e_journal_between("-30d", "today")
    );
    assert_eq!(
        pred("(between scheduled -7d +7d)"),
        Filter::attr(
            Attr::Scheduled,
            CmpOp::Between,
            Value::List {
                items: vec![Value::date("-7d"), Value::date("+7d")]
            }
        )
    );

    // `between journal` restricts to journal pages: a block with an in-range
    // SCHEDULED date on a *named* page must NOT match.
    let q = pred("(between journal -30d today)");
    let sched = DocBlock::new("TODO x\nSCHEDULED: <2026-06-10 Wed>");
    assert!(!q.eval(&sched, &ctx_named())); // named page, journal=None
    assert!(q.eval(&DocBlock::new("TODO y"), &ctx_journal(20260610))); // journal page in range
    assert!(!q.eval(&DocBlock::new("TODO z"), &ctx_journal(20260101))); // journal page out of range

    // `between scheduled` ignores the page's journal date entirely.
    let qs = pred("(between scheduled -30d today)");
    assert!(qs.eval(&sched, &ctx_named()));
    assert!(!qs.eval(&DocBlock::new("TODO y"), &ctx_journal(20260610)));

    // `between deadline` only looks at DEADLINE lines.
    let qd = pred("(between deadline -30d today)");
    let dead = DocBlock::new("TODO x\nDEADLINE: <2026-06-10 Wed>");
    assert!(qd.eval(&dead, &ctx_named()));
    assert!(!qd.eval(&sched, &ctx_named()));
}

#[test]
fn agenda_query_keys_off_scheduled_deadline_not_journal_date() {
    // The journal-agenda DSL the app inserts (window = ±7d around TODAY).
    // It must match on the SCHEDULED/DEADLINE date itself, NOT the journal
    // day the block happens to live on — otherwise a stale-deadline item
    // carried onto a recent day shows up forever (the reported bug).
    let q = pred("(or (between scheduled -7d +7d) (between deadline -7d +7d))");

    // Ancient deadline, sitting on TODAY's journal page: must NOT match.
    let stale = DocBlock::new("TODO old thing\nDEADLINE: <2025-01-01 Wed>");
    assert!(!q.eval(&stale, &ctx_journal(20260616)));

    // Deadline today (on any page): matches.
    let due = DocBlock::new("TODO pay\nDEADLINE: <2026-06-16 Tue>");
    assert!(q.eval(&due, &ctx_named()));

    // Scheduled in range but on an OLD journal page: still matches (the scan
    // is whole-graph; the journal day is irrelevant to the window).
    let sched = DocBlock::new("TODO meet\nSCHEDULED: <2026-06-18 Thu>");
    assert!(q.eval(&sched, &ctx_journal(20200101)));

    // No scheduled/deadline at all: never in the agenda, even on today.
    assert!(!q.eval(&DocBlock::new("just a note"), &ctx_journal(20260616)));
}

#[test]
fn journal_predicate_and_target_query() {
    let b = DocBlock::new("TODO buy milk");
    assert_eq!(
        pred("(journal)"),
        Filter::rel(
            Rel::Page,
            Quant::Any,
            Filter::attr(Attr::Journal, CmpOp::Eq, Value::Bool { value: true })
        )
    );
    assert!(pred("(journal)").eval(&b, &ctx_journal(20260616)));
    assert!(!pred("(journal)").eval(&b, &ctx_named()));

    // The motivating query: TODOs on journal pages dated in the last 30 days.
    let q = pred("(and (task TODO) (between journal -30d today))");
    assert!(q.eval(&b, &ctx_journal(20260601)));
    assert!(!q.eval(&b, &ctx_journal(20260101))); // too old
    assert!(!q.eval(&DocBlock::new("DONE buy milk"), &ctx_journal(20260601))); // not TODO
    assert!(!q.eval(&b, &ctx_named())); // not a journal page
}

#[test]
fn eval_page_and_namespace() {
    let b = DocBlock::new("hi");
    let ctx = ctx_page("Project/Alpha", &[]);
    assert!(pred("(page Project/Alpha)").eval(&b, &ctx));
    assert!(!pred("(page Project/Beta)").eval(&b, &ctx));
    assert!(pred("(namespace Project)").eval(&b, &ctx));
    assert!(!pred("(namespace Other)").eval(&b, &ctx));
}

#[test]
fn eval_page_property_and_tags() {
    let b = DocBlock::new("hi");
    let ctx = ctx_page("P", &[("type", "project"), ("tags", "research, active")]);
    assert!(pred("(page-property type project)").eval(&b, &ctx));
    assert!(pred("(page-property type)").eval(&b, &ctx));
    assert!(!pred("(page-property type book)").eval(&b, &ctx));
    assert!(pred("(page-tags research)").eval(&b, &ctx));
    assert!(!pred("(page-tags archived)").eval(&b, &ctx));
}

#[test]
fn eval_content_and_multivalue_property() {
    let none = ctx_named();
    let b = DocBlock::new("the quick brown fox");
    assert!(pred("\"quick brown\"").eval(&b, &none));
    assert!(!pred("\"slow\"").eval(&b, &none));
    // multi-value + page-ref property value matching
    let mut mv = DocBlock::new("x");
    mv.raw.push_str("\ntags:: [[research]], optimization");
    assert!(pred("(property tags research)").eval(&mv, &none));
    assert!(pred("(property tags optimization)").eval(&mv, &none));
    assert!(!pred("(property tags cooking)").eval(&mv, &none));
}

#[test]
fn property_query_matches_folded_source_key() {
    let none = ctx_named();
    let mut block = DocBlock::new("shipped task");
    block.raw.push_str("\ndone_at:: 2026-07-19");

    assert!(pred("(property done-at 2026-07-19)").eval(&block, &none));
    assert!(pred("(property DONE_AT)").eval(&block, &none));
}

#[test]
fn property_facets_group_folded_keys() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-property-key-norm-facets-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::write(
        dir.join("pages/Properties.md"),
        "- first\n  done_at:: one\n- second\n  done-at:: two\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    assert_eq!(
        property_facets(&graph),
        vec![(
            "done-at".to_string(),
            vec!["one".to_string(), "two".to_string()]
        )]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn autocomplete_property_facets_follow_og_visibility_sources_and_budget() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-property-autocomplete-facets-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq/config.edn"),
        "{:block-hidden-properties #{:hidden_config}}",
    )
    .unwrap();
    fs::write(
            dir.join("pages/Properties.md"),
            "Page_Only:: preamble\nTitle:: Page title\nhidden_config:: secret\n\n- first\n  alpha:: one\n  Alpha_Value:: two\n  template:: My template\n  id:: hidden\n  background_color:: hidden too\n  hidden_config:: block secret\n",
        )
        .unwrap();

    let graph = Graph::open(&dir);
    assert_eq!(
        autocomplete_property_facets_bounded(&graph, usize::MAX, usize::MAX),
        (
            vec![
                ("alpha".to_string(), vec!["one".to_string()]),
                ("alpha-value".to_string(), vec!["two".to_string()]),
                ("page-only".to_string(), vec!["preamble".to_string()]),
                ("template".to_string(), vec!["My template".to_string()]),
                ("title".to_string(), vec!["Page title".to_string()]),
            ],
            false,
        )
    );

    let (bounded, exceeded) = autocomplete_property_facets_bounded(&graph, 3, usize::MAX);
    assert!(exceeded);
    assert!(
        bounded.len()
            + bounded
                .iter()
                .map(|(_, values)| values.len())
                .sum::<usize>()
            <= 3
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn content_predicate_uses_the_a6_search_fold() {
    let none = ctx_named();
    let block = DocBlock::new("Re\u{301}sume\u{301}");
    assert!(pred("\"Résumé\"").eval(&block, &none));
    assert!(pred("\"Resume\"").eval(&block, &none));
}

#[test]
fn parse_extracts_options() {
    // The directives no longer ride in the filter: they are lifted into the
    // view on parse and the walk reads them through the one adapter.
    let view = view_of("(and (task TODO) (sample 5) (sort-by priority desc))");
    let opts = QueryOpts::from_view(&view);
    assert_eq!(opts.sample, Some(5));
    assert_eq!(opts.sort, vec![("priority".to_string(), false)]);
}

#[test]
fn block_sort_special_fields_and_visible_fallback_keep_their_exact_meanings() {
    let block = BlockDto {
        raw: "VISIBLE Ä\nignored".into(),
        priority: Some("b".into()),
        scheduled: Some("2026-09-08".into()),
        deadline: None,
        properties: vec![
            ("priority".into(), "property priority".into()),
            ("page".into(), "property page".into()),
            ("scheduled".into(), "property scheduled".into()),
            ("deadline".into(), "property deadline".into()),
        ],
        ..BlockDto::default()
    };
    assert_eq!(sort_key(&block, "MIXED Page", "priority"), "B");
    assert_eq!(sort_key(&block, "MIXED Page", "page"), "mixed page");
    assert_eq!(sort_key(&block, "MIXED Page", "scheduled"), "2026-09-08");
    assert_eq!(sort_key(&block, "MIXED Page", "deadline"), "~");
    assert_eq!(sort_key(&block, "MIXED Page", "missing"), "visible ä");
}

#[test]
fn extracted_comparator_keeps_base_order_for_equal_recency_keys() {
    let group = |page: &str, id: &str| ResultViewGroup {
        page: page.into(),
        kind: PageKind::Page,
        blocks: vec![BlockDto {
            id: id.into(),
            raw: id.into(),
            ..BlockDto::default()
        }],
        evidence: Vec::new(),
    };
    let groups = vec![
        group("first", "a"),
        group("newest", "b"),
        group("second", "c"),
    ];
    let recency = HashMap::from([
        ("first".to_string(), 10),
        ("newest".to_string(), 20),
        ("second".to_string(), 10),
    ]);
    let opts = QueryOpts::from_view(&ViewSettings {
        sort: vec![(ir::Field::new("modified"), SortDir::Desc)],
        ..ViewSettings::default()
    });
    let output = apply_result_view_directives(groups, &recency, &opts);
    let ids: Vec<_> = output
        .iter()
        .flat_map(|group| group.blocks.iter().map(|block| block.id.as_str()))
        .collect();
    assert_eq!(ids, ["b", "a", "c"]);
}

#[test]
fn ordered_view_sort_uses_secondary_direction_before_sample_and_keeps_ties() {
    use ir::Field;
    let blocks = [("a", "Ada"), ("b", "Bo"), ("c", "Bo")]
        .into_iter()
        .map(|(id, owner)| BlockDto {
            id: id.into(),
            raw: id.into(),
            priority: Some("A".into()),
            properties: vec![("owner".into(), owner.into())],
            ..BlockDto::default()
        })
        .collect();
    let groups = vec![RefGroup {
        page: "Page".into(),
        kind: PageKind::Page,
        blocks,
        evidence: Vec::new(),
    }];
    let view = ViewSettings {
        sort: vec![
            (Field::new("priority"), SortDir::Asc),
            (Field::new("owner"), SortDir::Desc),
        ],
        sample: Some(2),
        ..ViewSettings::default()
    };
    let output = apply_view(
        PreViewGroups {
            matched_total: None,
            ordered: false,
            statistics: None,
            groups,
            recency_by_page: HashMap::new(),
            total: 3,
            exceeded: false,
        },
        &view,
    );
    let ids: Vec<_> = output
        .groups
        .iter()
        .flat_map(|g| g.blocks.iter().map(|b| b.id.as_str()))
        .collect();
    assert_eq!(ids, ["b", "c"]);
    assert_eq!(output.total, 3);
    assert!(!output.exceeded);
}

#[test]
fn ordered_view_sort_secondary_recency_requires_construction_axis() {
    use ir::Field;
    let view = ViewSettings {
        sort: vec![
            (Field::new("priority"), SortDir::Asc),
            (Field::new("modified"), SortDir::Desc),
        ],
        sample: Some(2),
        ..ViewSettings::default()
    };
    let profile = ConstructionProfile::from_view(&view);
    assert!(profile.want_recency);
    assert_eq!(profile.sample_admission_cap, None);
}

#[test]
fn located_view_keeps_distinct_physical_roots_through_sort_coalescing_and_sample() {
    // No Clone/Eq implementation: sorting carries ownership, never joins
    // back by exposed IDs, which are intentionally identical here.
    struct Locator(u8);
    let block = |priority: &str, locator| {
        (
            BlockDto {
                id: "same-public-id".into(),
                raw: "same visible text".into(),
                priority: Some(priority.into()),
                properties: vec![("owner".into(), "Bo".into())],
                ..BlockDto::default()
            },
            Locator(locator),
        )
    };
    let groups = vec![
        ResultViewGroup {
            page: "Same".into(),
            kind: PageKind::Page,
            blocks: vec![block("B", 11), block("A", 12)],
            evidence: vec![],
        },
        ResultViewGroup {
            page: "Same".into(),
            kind: PageKind::Page,
            blocks: vec![block("A", 21), block("C", 22)],
            evidence: vec![],
        },
    ];
    let opts = QueryOpts::from_view(&ViewSettings {
        sort: vec![
            (ir::Field::new("priority"), SortDir::Asc),
            (ir::Field::new("owner"), SortDir::Desc),
        ],
        sample: Some(3),
        ..ViewSettings::default()
    });
    let output = apply_result_view_directives(groups, &HashMap::new(), &opts);
    assert_eq!(output.len(), 1, "adjacent display headings still coalesce");
    let physical: Vec<_> = output
        .into_iter()
        .flat_map(|g| g.blocks)
        .map(|(_, locator)| locator.0)
        .collect();
    assert_eq!(
        physical,
        [12, 21, 11],
        "ties retain physical input order before sampling"
    );
}

#[test]
fn located_view_base_order_and_unsorted_sampling_preserve_physical_groups() {
    let group = |name: &str, kind, locator| ResultViewGroup {
        page: name.into(),
        kind,
        blocks: vec![(
            BlockDto {
                id: "same".into(),
                ..BlockDto::default()
            },
            locator,
        )],
        evidence: vec![],
    };
    let mut groups = vec![
        group("Z", PageKind::Page, 4),
        group("Same", PageKind::Page, 1),
        group("Same", PageKind::Page, 2),
        group("Same", PageKind::Journal, 3),
    ];
    base_order_result_view_groups(&mut groups);
    let opts = QueryOpts::from_view(&ViewSettings {
        sample: Some(3),
        ..ViewSettings::default()
    });
    let output = apply_result_view_directives(groups, &HashMap::new(), &opts);
    assert_eq!(output.len(), 3, "unsorted physical groups remain separate");
    let physical: Vec<_> = output
        .into_iter()
        .flat_map(|g| g.blocks)
        .map(|(_, locator)| locator)
        .collect();
    assert_eq!(physical, [3, 1, 2]);
}

#[test]
/// The walk's planning leaves now read the PROJECTED timestamp rather than
/// rescanning raw text (SPEC §3.2 G2). The `SCHEDULED:`-only query still
/// ignores a `DEADLINE:` line and vice versa.
fn planning_leaves_read_the_projected_timestamp_not_raw_text() {
    let none = ctx_named();
    let scheduled = pred("(between scheduled [[Jul 6th, 2026]] [[Jul 6th, 2026]])");
    let deadline = pred("(between deadline [[Jul 6th, 2026]] [[Jul 6th, 2026]])");

    let own_line = DocBlock::new("TODO x\nSCHEDULED: <2026-07-06 Mon>");
    assert!(scheduled.eval(&own_line, &none));
    assert!(!deadline.eval(&own_line, &none));

    let trailing = DocBlock::new(" SCHEDULED: <2026-07-06 Mon> #email students");
    assert!(scheduled.eval(&trailing, &none));

    let dead = DocBlock::new("DEADLINE: <2026-07-06 Mon>");
    assert!(!scheduled.eval(&dead, &none));
    assert!(deadline.eval(&dead, &none));
}

fn quick_switch_fingerprint(entries: Vec<PageEntry>) -> Vec<(String, PageKind, String)> {
    entries
        .into_iter()
        .map(|e| (e.name, e.kind, e.rel_path))
        .collect()
}

fn graph_from_page_snapshot(pages: &[(&str, &str, &str)]) -> Graph {
    let pages = pages
        .iter()
        .map(|(name, rel_path, source)| {
            (
                PageEntry {
                    name: (*name).into(),
                    kind: PageKind::Page,
                    date_key: None,
                    rel_path: (*rel_path).into(),
                    path: (*rel_path).into(),
                },
                std::sync::Arc::new(crate::doc::parse(source)),
            )
        })
        .collect();
    Graph::from_page_snapshot("", pages)
}

fn persisted_dto_id(block: BlockDto) -> String {
    block
        .properties
        .into_iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("id"))
        .map(|(_, value)| value)
        .expect("fixture block has persisted id::")
}

fn search_block_texts(graph: &Graph, query: &str, limit: usize) -> Vec<String> {
    search_cancellable(graph, query, limit, || false)
        .into_iter()
        .flat_map(|group| group.blocks.into_iter().map(|block| block.raw))
        .collect()
}

fn graph_search_block_texts(execution: crate::query_plan::QueryExecution) -> Vec<String> {
    execution
        .hits
        .into_iter()
        .filter_map(|hit| match hit {
            crate::query_plan::QueryHit::Block { display_text, .. } => Some(display_text),
            crate::query_plan::QueryHit::Page { .. } => None,
        })
        .collect()
}

#[test]
fn autocomplete_page_or_token_is_literal() {
    let graph = graph_from_page_snapshot(&[
        ("A", "pages/a.md", "- filler\n"),
        ("B", "pages/b.md", "- filler\n"),
        ("ORbit", "pages/orbit.md", "- target\n"),
        (
            "a OR b notes",
            "pages/a-or-b.md",
            "- literal multi-word target\n",
        ),
    ]);

    assert_eq!(
        quick_switch(&graph, "OR", 1)
            .into_iter()
            .map(|page| page.name)
            .collect::<Vec<_>>(),
        ["ORbit"]
    );
    assert_eq!(quick_switch(&graph, "a OR b", 1)[0].name, "a OR b notes");
}

#[test]
fn autocomplete_block_or_token_is_literal() {
    let graph =
        graph_from_page_snapshot(&[("Logic", "pages/logic.md", "- logic OR gate\n- unrelated\n")]);

    assert_eq!(search_block_texts(&graph, "OR", 8), ["logic OR gate"]);
}

#[test]
fn autocomplete_negation_token_is_literal() {
    let graph = graph_from_page_snapshot(&[
        ("A", "pages/a.md", "- filler\n"),
        (
            "-foo page",
            "pages/minus-foo.md",
            "- block contains -foo literally\n",
        ),
    ]);

    assert_eq!(
        quick_switch(&graph, "-foo", 1)
            .into_iter()
            .map(|page| page.name)
            .collect::<Vec<_>>(),
        ["-foo page"]
    );
    assert_eq!(
        search_block_texts(&graph, "-foo", 8),
        ["block contains -foo literally"]
    );
}

#[test]
fn autocomplete_no_present_absent_present_ladder() {
    let graph = graph_from_page_snapshot(&[
        ("A", "pages/a.md", "- filler\n"),
        ("B", "pages/b.md", "- filler\n"),
        ("ORbit", "pages/orbit.md", "- target\n"),
    ]);

    for query in ["O", "OR", "ORb"] {
        assert!(
            quick_switch(&graph, query, 1)
                .iter()
                .any(|page| page.name == "ORbit"),
            "ORbit disappeared for autocomplete query {query:?}"
        );
    }
}

#[test]
fn ctrlk_dsl_still_active() {
    let graph = graph_from_page_snapshot(&[(
        "Search",
        "pages/search.md",
        "- foo safe\n- foo x excluded\n- bar safe\n- unrelated\n",
    )]);

    let or_hits = graph_search_block_texts(
        crate::query_plan::QueryPlan::friendly("foo OR bar", 8, 8).execute_with_explain(
            &graph,
            || false,
            false,
        ),
    );
    assert!(or_hits.iter().any(|text| text == "foo safe"));
    assert!(or_hits.iter().any(|text| text == "bar safe"));
    assert!(!or_hits.iter().any(|text| text == "unrelated"));

    let excluded = graph_search_block_texts(
        crate::query_plan::QueryPlan::friendly("foo -x", 8, 8).execute_with_explain(
            &graph,
            || false,
            false,
        ),
    );
    assert_eq!(excluded, ["foo safe"]);
    assert!(crate::query_plan::QueryPlan::friendly("-x", 8, 8)
        .execute_with_explain(&graph, || false, false)
        .hits
        .is_empty());

    let scoped = graph_search_block_texts(
        crate::query_plan::QueryPlan::friendly_for_page(
            "foo -x",
            8,
            crate::query_plan::QueryPageScope {
                name: "Search".into(),
                page_kind: PageKind::Page,
                path: Some("pages/search.md".into()),
            },
        )
        .execute_with_explain(&graph, || false, false),
    );
    assert_eq!(scoped, ["foo safe"]);
}

#[test]
fn page_topk_ties_are_input_order_independent() {
    let file_forward = graph_from_page_snapshot(&[
        ("alx", "pages/alx.md", "- file page\n"),
        ("aly", "pages/aly.md", "- file page\n"),
    ]);
    let file_reversed = graph_from_page_snapshot(&[
        ("aly", "pages/aly.md", "- file page\n"),
        ("alx", "pages/alx.md", "- file page\n"),
    ]);
    for graph in [&file_forward, &file_reversed] {
        assert_eq!(quick_switch(graph, "al", 1)[0].name, "alx");
    }

    let refs_forward =
        graph_from_page_snapshot(&[("Source", "pages/source.md", "- [[alx]] [[aly]]\n")]);
    let refs_reversed =
        graph_from_page_snapshot(&[("Source", "pages/source.md", "- [[aly]] [[alx]]\n")]);
    for graph in [&refs_forward, &refs_reversed] {
        let result = quick_switch(graph, "al", 1);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "alx");
        assert!(
            result[0].rel_path.is_empty(),
            "winner must be reference-only"
        );
    }
}

fn quick_switch_reference_full_sort(graph: &Graph, query: &str, limit: usize) -> Vec<PageEntry> {
    let plan = crate::query_plan::QueryPlan::legacy_page_search(query, usize::MAX);
    crate::query_plan::page_hits_to_entries(plan.execute(graph, || false).hits)
        .into_iter()
        .take(limit)
        .collect()
}

#[test]
fn quick_switch_topk_matches_stable_full_sort_with_ties() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-quick-switch-topk-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();

    for i in 0..220 {
        fs::write(
            dir.join("pages").join(format!("aa{i:03}.md")),
            "- tied page\n",
        )
        .unwrap();
    }
    let refs = (0..40)
        .map(|i| format!("[[aa-ref-{i:03}]]"))
        .collect::<Vec<_>>()
        .join(" ");
    fs::write(dir.join("pages").join("zzsource.md"), format!("- {refs}\n")).unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();

    for query in [
        "",
        "aa",
        "000",
        "\"aa\"",
        "/^aa/",
        "aa -zzz",
        "aa OR zzsource",
        "-draft",
        "/(unclosed/",
    ] {
        for limit in [1, 7, 12, 64, 199, 240, 300] {
            let got = quick_switch_fingerprint(quick_switch(&graph, query, limit));
            let expected =
                quick_switch_fingerprint(quick_switch_reference_full_sort(&graph, query, limit));
            assert_eq!(got, expected, "query={query:?} limit={limit}");
        }
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn block_search_topk_keeps_late_best_match_and_ranks_it_first() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-block-search-topk-best-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(
        dir.join("pages/aa-weak.md"),
        "- a long weak interior needle match\n- another long weak interior needle match\n",
    )
    .unwrap();
    fs::write(dir.join("pages/zz-best.md"), "- needle\n").unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let ranked = search_cancellable(&graph, "needle", 2, || false)
        .into_iter()
        .flat_map(|group| {
            group
                .blocks
                .into_iter()
                .map(move |block| (group.page.clone(), block.raw))
        })
        .collect::<Vec<_>>();

    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0], ("zz-best".into(), "needle".into()));
    assert!(ranked.iter().any(|(page, _)| page == "zz-best"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn block_search_topk_uses_stable_traversal_ties() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-block-search-topk-ties-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    for name in ["aa", "bb", "cc", "dd"] {
        fs::write(
            dir.join("pages").join(format!("{name}.md")),
            "- tied needle\n",
        )
        .unwrap();
    }

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let pages = search_cancellable(&graph, "needle", 3, || false)
        .into_iter()
        .flat_map(|group| std::iter::repeat_n(group.page, group.blocks.len()))
        .collect::<Vec<_>>();
    assert_eq!(pages, ["aa", "bb", "cc"]);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn block_search_topk_ties_sort_rel_path_with_reversed_page_snapshot() {
    let pages = ["dd", "cc", "bb", "aa"]
        .into_iter()
        .map(|name| {
            let rel_path = format!("pages/{name}.md");
            (
                PageEntry {
                    name: name.into(),
                    kind: PageKind::Page,
                    date_key: None,
                    rel_path: rel_path.clone(),
                    path: rel_path.into(),
                },
                std::sync::Arc::new(crate::doc::parse("- tied needle\n")),
            )
        })
        .collect();
    let graph = Graph::from_page_snapshot("", pages);

    let pages = search_cancellable(&graph, "needle", 3, || false)
        .into_iter()
        .flat_map(|group| std::iter::repeat_n(group.page, group.blocks.len()))
        .collect::<Vec<_>>();

    assert_eq!(pages, ["aa", "bb", "cc"]);
}

#[test]
fn block_search_groups_preserve_interleaved_global_rank() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-block-search-ranked-groups-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(dir.join("pages/aa.md"), "- needle\n- xneedle\n").unwrap();
    fs::write(dir.join("pages/bb.md"), "- needle plus\n").unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let groups = search_cancellable(&graph, "needle", 3, || false);
    assert_eq!(
        groups
            .iter()
            .map(|group| group.page.as_str())
            .collect::<Vec<_>>(),
        ["aa", "bb", "aa"]
    );
    assert_eq!(
        groups
            .into_iter()
            .flat_map(|group| group.blocks.into_iter().map(|block| block.raw))
            .collect::<Vec<_>>(),
        ["needle", "needle plus", "xneedle"]
    );

    let _ = fs::remove_dir_all(&dir);
}

/// OG 1.0.0 (`query_dsl.cljs` + the `:page-ref` rule) evaluates a bare
/// `[[Page]]` simple-query clause against `:block/path-refs`. That relation
/// includes both explicit references and the page the block physically
/// belongs to. Keep the explicit `(page …)` operator narrower: it means
/// physical membership only.
#[test]
fn og_bare_page_token_unions_physical_membership_and_explicit_refs() {
    use std::fs;

    const ON_PAGE: &str = "11111111-1111-4111-8111-111111111111";
    const EXPLICIT_REF: &str = "22222222-2222-4222-8222-222222222222";
    const UNRELATED: &str = "33333333-3333-4333-8333-333333333333";
    let dir = std::env::temp_dir().join(format!("tine-og-bare-page-union-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::write(
        dir.join("pages/Parity Target.md"),
        format!("- TODO physically on target\n  id:: {ON_PAGE}\n"),
    )
    .unwrap();
    fs::write(
        dir.join("pages/Parity Workflows.md"),
        format!("- TODO explicit [[Parity Target]] witness\n  id:: {EXPLICIT_REF}\n"),
    )
    .unwrap();
    fs::write(
        dir.join("pages/Other.md"),
        format!("- TODO unrelated witness\n  id:: {UNRELATED}\n"),
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let ids = |query: &str| {
        run_query(&graph, query)
            .into_iter()
            .flat_map(|group| group.blocks.into_iter().map(persisted_dto_id))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids("(and (task TODO) [[Parity Target]])"),
        vec![ON_PAGE.to_string(), EXPLICIT_REF.to_string()]
    );
    assert_eq!(
        ids("(and (task TODO) (page \"Parity Target\"))"),
        vec![ON_PAGE.to_string()]
    );

    let _ = fs::remove_dir_all(&dir);
}

/// OG's graph parser materializes `:block/path-refs` from every ancestor's
/// explicit refs (`with-path-refs`), so a bare page-ref query also matches a
/// descendant whose own text does not repeat the reference. The explicit
/// `(page ...)` operator remains physical page membership only.
#[test]
fn og_bare_page_token_inherits_ancestor_path_refs() {
    use std::fs;

    const ON_PAGE: &str = "44444444-4444-4444-8444-444444444444";
    const INHERITED_CHILD: &str = "55555555-5555-4555-8555-555555555555";
    const INHERITED_GRANDCHILD: &str = "66666666-6666-4666-8666-666666666666";
    const DIRECT_REF: &str = "77777777-7777-4777-8777-777777777777";
    const UNRELATED_CHILD: &str = "88888888-8888-4888-8888-888888888888";
    const INVALIDATION_WITNESS: &str = "99999999-9999-4999-8999-999999999999";
    let dir = std::env::temp_dir().join(format!(
        "tine-og-bare-page-path-refs-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::write(
        dir.join("pages/Target.md"),
        format!("- TODO physically on target\n  id:: {ON_PAGE}\n"),
    )
    .unwrap();
    fs::write(
            dir.join("pages/Workflows.md"),
            format!(
                "- Parent [[Target]]\n  - TODO inherited child\n    id:: {INHERITED_CHILD}\n    - TODO inherited grandchild\n      id:: {INHERITED_GRANDCHILD}\n- Other parent\n  - TODO unrelated child\n    id:: {UNRELATED_CHILD}\n- TODO direct [[Target]]\n  id:: {DIRECT_REF}\n"
            ),
        )
        .unwrap();
    fs::write(
            dir.join("pages/Inherited Only.md"),
            format!(
                "- Cache context [[Target]]\n  - TODO inherited invalidation witness\n    id:: {INVALIDATION_WITNESS}\n"
            ),
        )
        .unwrap();

    let graph = Graph::open(&dir);
    let ids = |query: &str| {
        run_query(&graph, query)
            .into_iter()
            .flat_map(|group| group.blocks.into_iter().map(persisted_dto_id))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids("(and (task TODO) [[Target]])")
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        [ON_PAGE, INHERITED_CHILD, INVALIDATION_WITNESS, DIRECT_REF]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    // OG query presentation suppresses a matching block only when its
    // immediate parent also matched, so the matching grandchild is not a
    // second top-level result.
    assert!(!ids("(and (task TODO) [[Target]])").contains(&INHERITED_GRANDCHILD.to_string()));
    assert_eq!(
        ids("(and (task TODO) (not [[Target]]))"),
        vec![UNRELATED_CHILD.to_string()]
    );
    assert_eq!(
        ids("(and (task TODO) (page \"Target\"))"),
        vec![ON_PAGE.to_string()]
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Macro arguments arrive without their source quotes after the parser has
/// expanded `$1`. OG's simple query reader treats that bare value as a
/// block-content term; Tine must not silently drop it from an `and` form.
#[test]
fn og_bare_word_is_a_content_term() {
    let parsed = pred("(and (task DONE) changelog)");
    assert_eq!(
        parsed,
        Filter::and(vec![e_task(&["DONE"]), e_content("changelog")])
    );
    assert!(parsed.eval(
        &DocBlock::new("DONE Write changelog for v0.0.9"),
        &ctx_named()
    ));
    assert!(!parsed.eval(&DocBlock::new("DONE Publish release notes"), &ctx_named()));
}

#[test]
fn quick_switch_topk_sorts_only_survivors() {
    let limit = 12;
    let total = 240;
    let mut heap = std::collections::BinaryHeap::with_capacity(limit);
    let mut reference = Vec::with_capacity(total);
    for index in 0..total {
        let score = (index % 6) as i32;
        reference.push((score, index));
        push_quick_switch_top(&mut heap, limit, ScoredQuickSwitchCand { score, index });
    }

    let top = finish_quick_switch_top(heap);
    assert_eq!(
        top.len(),
        limit,
        "survivor sort must be bounded by limit, not total candidates"
    );

    reference.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    reference.truncate(limit);
    let got: Vec<(i32, usize)> = top.into_iter().map(|c| (c.score, c.index)).collect();
    assert_eq!(got, reference);
}

/// Issue #9: linked references are grouped by referring page, ordered by the
/// referrer's journal day DESCENDING (newest journal first), with non-journal
/// referrers last — matching OG (`components/block.cljs` `sort-by :block/journal-day >`).
// Tine's Favorites layout page holds `[[links]]` so that renames follow it
// for free — but those links are a sidebar arrangement, not a mention, so
// the page must never appear in anyone's Linked References. Identity comes
// from `:tine/favorites-page` in config.edn, NOT from a reserved page name:
// a user's own page called "Favorites" must keep behaving like any page.
#[test]
fn favorites_layout_page_is_never_a_reference_source() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-fav-exclude-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages").join("Notes.md"),
        "- a real mention [[Target]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Favorites.md"),
        "tine/favorites:: true\n\n- [[Target]]\n- Work\n\t- [[Target]]\n",
    )
    .unwrap();

    // Without the config key the page is an ORDINARY page and still counts.
    fs::write(dir.join("logseq").join("config.edn"), "{}\n").unwrap();
    let plain = crate::model::Graph::open(&dir);
    let names = |groups: &[crate::model::RefGroup]| {
        let mut names = groups.iter().map(|g| g.page.clone()).collect::<Vec<_>>();
        names.sort();
        names
    };
    assert_eq!(
        names(&plain.backlinks("Target")),
        vec!["Favorites".to_string(), "Notes".to_string()],
        "an unmarked page named Favorites is just a page"
    );

    // With it, the layout page drops out and nothing else does.
    fs::write(
        dir.join("logseq").join("config.edn"),
        "{:tine/favorites-page \"Favorites\"}\n",
    )
    .unwrap();
    let marked = crate::model::Graph::open(&dir);
    assert_eq!(
        names(&marked.backlinks("Target")),
        vec!["Notes".to_string()]
    );
    // The target page itself is still excluded from its own references.
    assert!(!names(&marked.backlinks("Notes")).contains(&"Notes".to_string()));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn backlinks_ordered_by_referrer_journal_date_desc() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-backlinks-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    // Three journals referencing [[Common]], written OUT of date order; two plain pages.
    fs::write(
        dir.join("journals").join("1897_07_24.md"),
        "- oldestref [[Common]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("journals").join("2026_06_29.md"),
        "- newestref [[Common]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("journals").join("1927_07_02.md"),
        "- middleref [[Common]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Notes.md"),
        "- plainref [[Common]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Alpha.md"),
        "- alpharef [[Common]]\n",
    )
    .unwrap();

    let g = crate::model::Graph::open(&dir);
    let groups = g.backlinks("Common");
    // Identify each group by its block text (robust to the journal title format).
    let tags: Vec<&str> = groups
        .iter()
        .map(|gr| {
            let raw = gr.blocks[0].raw.as_str();
            [
                "newestref",
                "middleref",
                "oldestref",
                "alpharef",
                "plainref",
            ]
            .into_iter()
            .find(|t| raw.contains(t))
            .unwrap_or("?")
        })
        .collect();
    assert_eq!(
        tags,
        vec![
            "newestref",
            "middleref",
            "oldestref",
            "alpharef",
            "plainref"
        ],
        "{tags:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn canonical_reference_evidence_keeps_mixed_alias_occurrences_and_properties() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-reference-evidence-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(
        dir.join("pages").join("Target.md"),
        "alias:: Alias\n\n- canonical page\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Source.md"),
        "- [[Alias]] then Alias and Target and `Target`\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Props.md"),
        "related:: [[Alias]]\n\n- ordinary\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let linked = backlinks(&graph, "Target");
    let source = linked.iter().find(|group| group.page == "Source").unwrap();
    assert_eq!(source.blocks.len(), 1);
    assert_eq!(source.evidence.len(), 1);
    assert_eq!(source.evidence[0].occurrences.len(), 1);
    assert_eq!(
        source.evidence[0].occurrences[0].kind,
        ReferenceKind::Explicit
    );
    let props = linked.iter().find(|group| group.page == "Props").unwrap();
    assert!(props.blocks[0].page_property);
    assert_eq!(
        props.evidence[0].occurrences[0].kind,
        ReferenceKind::Explicit
    );

    let unlinked = unlinked_refs(&graph, "Target");
    let source = unlinked
        .iter()
        .find(|group| group.page == "Source")
        .unwrap();
    assert_eq!(
        source.blocks.len(),
        1,
        "one block row, not one row per mention"
    );
    assert_eq!(
        source.evidence[0].occurrences.len(),
        3,
        "alias + title + the mention inside inline code, which Logseq also \
             reports as unlinked (GH #270)"
    );
    assert!(source.evidence[0]
        .occurrences
        .iter()
        .all(|occurrence| occurrence.kind == ReferenceKind::Plain));
    let diagnostics = reference_diagnostics(&graph, "Target");
    assert_eq!(diagnostics.engine_version, "reference-evidence/v1");
    let source_trace = diagnostics
        .traces
        .iter()
        .find(|trace| trace.page == "Source")
        .unwrap();
    assert!(source_trace.included_linked && source_trace.included_unlinked);
    // One explicit `[[Alias]]` plus the three plain mentions above.
    assert_eq!(source_trace.occurrences.len(), 4);
    assert!(!serde_json::to_string(&diagnostics)
        .unwrap()
        .contains("launcher-ranking"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn property_keys_create_backlink_membership_with_key_evidence() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-property-key-backlinks-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(
        dir.join("pages/url.md"),
        "url:: https://self.example\n\n- target\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Key Referrer.md"),
        "url:: https://referrer.example\n\n- body\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Block Referrer.md"),
        "- body\n  url:: https://block.example\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Value Referrer.md"),
        "author:: [[url]]\n\n- body\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let refs = backlinks_bounded(&graph, "url", 100, usize::MAX);
    let pages = refs
        .groups
        .iter()
        .map(|group| group.page.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert!(pages.contains("Key Referrer"), "{pages:?}");
    assert!(pages.contains("Block Referrer"), "{pages:?}");
    assert!(pages.contains("Value Referrer"), "{pages:?}");
    assert!(!pages.contains("url"), "self page must remain excluded");

    let key_group = refs
        .groups
        .iter()
        .find(|group| group.page == "Key Referrer")
        .unwrap();
    let occurrence = &key_group.evidence[0].occurrences[0];
    assert_eq!(occurrence.rule, "explicit_property_key");
    assert_eq!(
        occurrence.span,
        crate::model::ReferenceSpan { start: 0, end: 3 }
    );
    assert_eq!(
        &key_group.blocks[0].raw[occurrence.span.start..occurrence.span.end],
        "url"
    );
    let block_group = refs
        .groups
        .iter()
        .find(|group| group.page == "Block Referrer")
        .unwrap();
    let block_occurrence = block_group.evidence[0]
        .occurrences
        .iter()
        .find(|occurrence| occurrence.rule == "explicit_property_key")
        .unwrap();
    assert_eq!(
        &block_group.blocks[0].raw[block_occurrence.span.start..block_occurrence.span.end],
        "url"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn property_key_membership_uses_canonical_key_fold_and_og_eligibility() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!(
        "tine-property-key-eligibility-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq/config.edn"),
        "{:property-pages/excludelist #{:private_key}}",
    )
    .unwrap();
    fs::write(
            dir.join("pages/Source.md"),
            "- keys\n  Done_At:: today\n  id:: not-a-reference\n  background-color:: red\n  private-key:: hidden\n",
        )
        .unwrap();

    let graph = Graph::open(&dir);
    assert_eq!(backlinks(&graph, "done-at")[0].page, "Source");
    assert!(backlinks(&graph, "id").is_empty());
    assert!(backlinks(&graph, "background-color").is_empty());
    assert!(backlinks(&graph, "private-key").is_empty());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disabled_property_pages_suppress_only_key_membership() {
    use std::fs;

    let dir =
        std::env::temp_dir().join(format!("tine-property-key-disabled-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("logseq/config.edn"),
        "{:property-pages/enabled? false}",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Key Referrer.md"),
        "url:: https://referrer.example\n\n- body\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages/Value Referrer.md"),
        "author:: [[url]]\n\n- body\n",
    )
    .unwrap();

    let graph = Graph::open(&dir);
    let refs = backlinks(&graph, "url");
    assert_eq!(
        refs.iter()
            .map(|group| group.page.as_str())
            .collect::<Vec<_>>(),
        vec!["Value Referrer"]
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn og_page_identity_and_reference_grouping_use_nfc_without_accent_folding() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-ref-nfc-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(dir.join("pages/Café.md"), "- target\n").unwrap();
    fs::write(
        dir.join("pages/Source.md"),
        "- [[Cafe\u{301}]] and plain Cafe\u{301}\n",
    )
    .unwrap();
    fs::write(dir.join("pages/Ascii.md"), "- [[cafe]]\n").unwrap();
    let graph = Graph::open(&dir);
    let linked = backlinks(&graph, "Café");
    assert_eq!(
        linked.iter().filter(|group| group.page == "Source").count(),
        1
    );
    assert!(!linked.iter().any(|group| group.page == "Ascii"));
    let unlinked = unlinked_refs(&graph, "Café");
    assert_eq!(
        unlinked
            .iter()
            .filter(|group| group.page == "Source")
            .count(),
        1
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn plain_page_property_is_unlinked_and_diagnostics_agree() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-page-prop-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(dir.join("pages/Target.md"), "- target\n").unwrap();
    fs::write(dir.join("pages/PageProps.md"), "note:: Target\n\n- body\n").unwrap();
    fs::write(dir.join("pages/BlockProps.md"), "- note:: Target\n").unwrap();
    let graph = Graph::open(&dir);
    let groups = unlinked_refs(&graph, "Target");
    for page in ["PageProps", "BlockProps"] {
        assert_eq!(
            groups
                .iter()
                .find(|group| group.page == page)
                .unwrap()
                .blocks
                .len(),
            1
        );
    }
    assert!(
        groups
            .iter()
            .find(|group| group.page == "PageProps")
            .unwrap()
            .blocks[0]
            .page_property
    );
    let diagnostics = reference_diagnostics(&graph, "Target");
    assert!(
        diagnostics
            .traces
            .iter()
            .find(|trace| trace.page == "PageProps")
            .unwrap()
            .included_unlinked
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn duplicate_source_page_names_merge_into_one_reference_group() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-ref-groups-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages/a")).unwrap();
    fs::create_dir_all(dir.join("pages/b")).unwrap();
    fs::write(dir.join("pages/a/Note.md"), "- first [[Target]]\n").unwrap();
    fs::write(dir.join("pages/b/Note.md"), "- second [[Target]]\n").unwrap();
    let graph = Graph::open(&dir);
    let groups = backlinks(&graph, "Target");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].page, "Note");
    assert_eq!(groups[0].blocks.len(), 2);
    assert_eq!(groups[0].evidence.len(), 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn structural_id_value_never_creates_an_unlinked_group() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-ref-id-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(dir.join("pages/6a55b643.md"), "- target\n").unwrap();
    fs::write(
        dir.join("pages/Source.md"),
        "- id:: 6a55b643-1234-5678-9abc-def012345678\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    assert!(unlinked_refs(&graph, "6a55b643").is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bounded_occurrence_evidence_reaches_reference_results() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-ref-total-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(dir.join("pages/Target.md"), "- target\n").unwrap();
    fs::write(
        dir.join("pages/Source.md"),
        format!("- {}\n", "Target ".repeat(70)),
    )
    .unwrap();
    let graph = Graph::open(&dir);
    let groups = unlinked_refs(&graph, "Target");
    let evidence = &groups[0].evidence[0];
    assert_eq!(evidence.occurrences.len(), 64);
    assert_eq!(evidence.total, 70);
    assert!(evidence.truncated);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn real_title_beats_colliding_alias() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!(
        "tine-real-page-before-alias-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(dir.join("pages/X.md"), "- real title\n").unwrap();
    fs::write(dir.join("pages/Y.md"), "alias:: X\n\n- [[X]]\n").unwrap();
    let graph = Graph::open(&dir);
    assert!(backlinks(&graph, "X").iter().any(|group| group.page == "Y"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn duplicate_alias_component_keeps_all_edges_and_uses_lexical_canonical() {
    let owned = vec![
        (
            std::path::PathBuf::from("pages/a/B.md"),
            "z".to_string(),
            "B".to_string(),
        ),
        (
            std::path::PathBuf::from("pages/z/A.md"),
            "z".to_string(),
            "A".to_string(),
        ),
    ];
    let aliases = sorted_alias_owners(owned);
    assert_eq!(
        aliases,
        vec![
            ("z".to_string(), "B".to_string()),
            ("z".to_string(), "A".to_string()),
        ],
        "every path-sorted alias edge must reach component resolution"
    );
    assert_eq!(
        equivalent_page_names(&RealPageNames::new(), &aliases, "Z").0,
        "A"
    );
}

/// Regression for the pre-0.6 performance audit: recursive `block_to_dto`
/// used to clone a nested suffix for every matching/query/reference id,
/// producing N(N+1)/2 wire nodes (and ~1.8 GiB RSS at N=2,000). OG query
/// presentation suppresses a result whose direct parent is also a result;
/// references retain every occurrence. All wire rows stay shallow, and an
/// explicit preview is bounded before allocation.
#[test]
fn nested_result_contract_is_non_overlapping_and_preview_is_bounded() {
    use std::fs;

    fn collect_ids(blocks: &[BlockDto], out: &mut Vec<String>) {
        for block in blocks {
            out.push(block.id.clone());
            collect_ids(&block.children, out);
        }
    }
    fn dto_nodes(blocks: &[BlockDto]) -> usize {
        blocks
            .iter()
            .map(|block| 1 + dto_nodes(&block.children))
            .sum()
    }

    const DEPTH: usize = 128;
    let dir = std::env::temp_dir().join(format!("tine-non-overlap-results-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    let nested = (0..DEPTH)
        .map(|depth| format!("{}- TODO [[Target]] node {depth}\n", "  ".repeat(depth)))
        .collect::<String>();
    fs::write(dir.join("pages").join("Nested.md"), nested).unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let entry = graph
        .list_pages()
        .into_iter()
        .find(|entry| entry.name == "Nested")
        .unwrap();
    let page = graph.load_page(&entry).unwrap();
    let mut ids = Vec::new();
    collect_ids(&page.blocks, &mut ids);
    assert_eq!(ids.len(), DEPTH);

    let query = run_query(&graph, "(task TODO)");
    assert_eq!(query.iter().map(|g| g.blocks.len()).sum::<usize>(), 1);
    assert_eq!(
        dto_nodes(&query[0].blocks),
        1,
        "query membership DTOs stay shallow"
    );

    let linked = backlinks(&graph, "Target");
    assert_eq!(linked.iter().map(|g| g.blocks.len()).sum::<usize>(), DEPTH);
    assert_eq!(
        linked
            .iter()
            .flat_map(|group| &group.blocks)
            .map(|block| dto_nodes(std::slice::from_ref(block)))
            .sum::<usize>(),
        DEPTH,
        "every reference occurrence remains independently countable but shallow"
    );

    let resolved = resolve_blocks(&graph, &ids);
    assert_eq!(resolved.len(), DEPTH);
    assert_eq!(
        resolved
            .iter()
            .flatten()
            .map(|group| dto_nodes(&group.blocks))
            .sum::<usize>(),
        DEPTH,
        "N requested nested ids must produce N DTO nodes, not N(N+1)/2"
    );

    let preview = preview_block(&graph, &ids[0], 50).unwrap();
    assert_eq!(dto_nodes(&preview.group.blocks), 50);
    assert_eq!(preview.truncated, DEPTH - 50);

    let byte_bounded = preview_block_with_budget(&graph, &ids[0], DEPTH, 512).unwrap();
    assert!(
        byte_bounded
            .group
            .blocks
            .iter()
            .map(crate::model::block_dto_estimated_bytes)
            .sum::<usize>()
            <= 512
    );
    assert!(byte_bounded.truncated > 0);

    let root_too_large = preview_block_with_budget(&graph, &ids[0], DEPTH, 64).unwrap();
    assert!(root_too_large.group.blocks.is_empty());
    assert_eq!(root_too_large.truncated, DEPTH);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn og_query_roots_and_reference_occurrences_cover_matching_descendants_below_a_gap() {
    use std::fs;

    const TARGET_ID: &str = "11111111-1111-4111-8111-111111111111";
    let dir = std::env::temp_dir().join(format!("tine-og-query-root-gap-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::write(
            dir.join("pages").join("Nested.md"),
            format!(
                "- TODO [[Target]] (({TARGET_ID})) PlainName ancestor\n  - DONE non-matching gap\n    - TODO [[Target]] (({TARGET_ID})) PlainName grandchild\n"
            ),
        )
        .unwrap();
    fs::write(
        dir.join("pages").join("Target.md"),
        format!("- target\n  id:: {TARGET_ID}\n"),
    )
    .unwrap();
    fs::write(dir.join("pages").join("PlainName.md"), "- target\n").unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let raws = |groups: &[RefGroup]| {
        groups
            .iter()
            .flat_map(|group| group.blocks.iter().map(|block| block.raw.clone()))
            .collect::<Vec<_>>()
    };

    let simple = raws(&run_query(&graph, "(task TODO)"));
    assert_eq!(simple.len(), 2);
    assert!(simple.iter().any(|raw| raw.contains("ancestor")));
    assert!(simple.iter().any(|raw| raw.contains("grandchild")));

    let advanced = run_advanced_query(
        &graph,
        "[:find (pull ?b [*]) :where (task ?b \"TODO\")]",
        None,
    );
    assert!(advanced.supported);
    assert_eq!(raws(&advanced.groups).len(), 2);

    let linked = backlinks(&graph, "Target");
    assert_eq!(raws(&linked).len(), 2);
    assert_eq!(
        linked
            .iter()
            .map(|group| group.evidence.len())
            .sum::<usize>(),
        2
    );

    let unlinked = unlinked_refs(&graph, "PlainName");
    assert_eq!(raws(&unlinked).len(), 2);
    assert_eq!(
        unlinked
            .iter()
            .map(|group| group.evidence.len())
            .sum::<usize>(),
        2
    );

    assert_eq!(raws(&block_referrers(&graph, TARGET_ID)).len(), 2);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn query_export_spec_defaults_a_missing_simple_dialect_to_og() {
    let legacy = r#"{
            "key": "legacy",
            "query": "(task TODO)",
            "advanced": false,
            "current_page": null
        }"#;
    let spec: QueryExportSpec = serde_json::from_str(legacy).expect("legacy export spec");
    assert_eq!(spec.simple_dialect, None);
    assert_eq!(spec.simple_dialect(), QueryDialect::Og);

    let declared: QueryExportSpec = serde_json::from_str(
        r#"{
                "key": "tql",
                "query": "task = 'TODO'",
                "advanced": false,
                "simple_dialect": "tql"
            }"#,
    )
    .expect("declared TQL export spec");
    assert_eq!(declared.simple_dialect(), QueryDialect::Tql);
}

#[test]
fn query_export_hydrates_only_selected_subtrees_under_one_session_budget() {
    use std::fs;

    let dir = std::env::temp_dir().join(format!("tine-query-export-budget-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();

    let wide_children = |prefix: &str| {
        (0..5_000)
            .map(|index| format!("  - {prefix} child {index}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // Each matching root has 5,000 descendants. Page A also has a 5,000-node
    // unrelated branch: whole-page hydration would clone/index all 10,002
    // nodes before noticing the export cap.
    fs::write(
        dir.join("pages").join("A.md"),
        format!(
            "- TODO selected A\n{}\n- unrelated branch\n{}\n",
            wide_children("selected-a"),
            wide_children("unrelated-a"),
        ),
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("B.md"),
        format!("- DONE selected B\n{}\n", wide_children("selected-b")),
    )
    .unwrap();

    let graph = Graph::open(&dir);
    graph.warm_cache();
    let batch = export_query_subtrees(
        &graph,
        &[
            QueryExportSpec {
                key: "todo".into(),
                query: "(task TODO)".into(),
                advanced: false,
                simple_dialect: None,
                current_page: None,
            },
            QueryExportSpec {
                key: "done".into(),
                query: "(task DONE)".into(),
                advanced: false,
                simple_dialect: None,
                current_page: None,
            },
        ],
        64,
        50,
        3,
        1024 * 1024,
    );

    assert_eq!(batch.results.len(), 2);
    assert_eq!(batch.results[0].total, 1);
    assert_eq!(batch.results[0].shown, 1);
    assert_eq!(batch.results[0].groups[0].blocks[0].children.len(), 2);
    assert_eq!(batch.results[0].omitted_nodes, 4_998);
    assert_eq!(batch.results[1].total, 1);
    assert_eq!(batch.results[1].shown, 0);
    assert_eq!(batch.results[1].omitted_nodes, 5_001);
    let emitted = batch
        .results
        .iter()
        .flat_map(|result| result.groups.iter())
        .flat_map(|group| group.blocks.iter())
        .map(crate::model::block_dto_estimated_bytes)
        .sum::<usize>();
    assert!(emitted <= 1024 * 1024);
    assert!(batch.results.iter().all(|result| {
        result
            .groups
            .iter()
            .flat_map(|group| group.blocks.iter())
            .all(|block| !block.raw.contains("unrelated branch"))
    }));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn interactive_search_stops_inside_a_page_when_superseded() {
    use std::cell::Cell;
    use std::fs;
    let dir = std::env::temp_dir().join(format!("tine-search-cancel-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    let content = (0..1000)
        .map(|i| format!("- ordinary block {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.join("pages").join("Large.md"), content).unwrap();
    let graph = Graph::open(&dir);
    let checks = Cell::new(0usize);
    let result = search_cancellable(&graph, "never-matches", 10, || {
        checks.set(checks.get() + 1);
        checks.get() > 12
    });
    assert!(result.is_empty());
    assert!(checks.get() < 40, "cancellation checks: {}", checks.get());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn result_families_stop_constructing_at_row_and_byte_budgets() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!(
        "tine-result-construction-budget-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    let content = (0..12)
        .map(|i| {
            format!(
                "- TODO [[Target]] item {i}\n  field-{i}:: {}",
                "x".repeat(100)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.join("pages/Source.md"), content).unwrap();
    fs::write(dir.join("pages/Target.md"), "- target\n").unwrap();
    let graph = Graph::open(&dir);

    RESULT_DTO_CONSTRUCTIONS.with(|count| count.set(0));
    let query = run_query_bounded(&graph, "(task TODO)", 3, usize::MAX);
    assert!(query.exceeded);
    assert_eq!(query.total, 12);
    assert_eq!(
        query
            .groups
            .iter()
            .map(|group| group.blocks.len())
            .sum::<usize>(),
        3
    );
    assert_eq!(RESULT_DTO_CONSTRUCTIONS.with(std::cell::Cell::get), 3);

    RESULT_DTO_CONSTRUCTIONS.with(|count| count.set(0));
    crate::reference_evidence::reset_occurrence_constructions();
    let refs = backlinks_bounded(&graph, "Target", 2, usize::MAX);
    assert!(refs.exceeded);
    assert_eq!(refs.total, 12);
    assert_eq!(
        refs.groups
            .iter()
            .map(|group| group.blocks.len())
            .sum::<usize>(),
        2
    );
    assert_eq!(RESULT_DTO_CONSTRUCTIONS.with(std::cell::Cell::get), 2);
    assert_eq!(crate::reference_evidence::occurrence_constructions(), 2);

    RESULT_DTO_CONSTRUCTIONS.with(|count| count.set(0));
    let sample = run_query_bounded(&graph, "(and (task TODO) (sample 1))", 20, usize::MAX);
    assert!(!sample.exceeded);
    assert_eq!(sample.total, 1);
    assert_eq!(RESULT_DTO_CONSTRUCTIONS.with(std::cell::Cell::get), 1);

    let (facets, facets_exceeded) = property_facets_bounded(&graph, 2, usize::MAX);
    assert!(facets_exceeded);
    assert!(facets.iter().map(|(_, values)| values.len()).sum::<usize>() <= 2);
    let _ = fs::remove_dir_all(&dir);
}

/// GH #542: an attribute pattern lowers only when it is a filter on the
/// returned block alone.
#[test]
fn gh542_attribute_patterns_lower_only_block_local_meaning() {
    let lower = |src: &str| advanced_pred(src, None, TODAY);

    // A flipped comparison is the same bound.
    let (a, _, ignored_a) =
        lower("[:find (pull ?b [*]) :where [?b :block/scheduled ?d] [(<= ?d 20260630)]]");
    let (b, _, ignored_b) =
        lower("[:find (pull ?b [*]) :where [?b :block/scheduled ?d] [(>= 20260630 ?d)]]");
    assert!(ignored_a.is_empty() && ignored_b.is_empty());
    assert_eq!(a.unwrap().filter, b.unwrap().filter);

    // A value variable shared by two patterns is a join (scheduled == deadline):
    // neither pattern may lower to "has a schedule".
    let (_, ran, ignored) =
        lower("[:find (pull ?b [*]) :where [?b :block/scheduled ?d] [?b :block/deadline ?d]]");
    assert!(ran.is_empty(), "{ran:?}");
    assert_eq!(ignored, vec!["pattern", "pattern"]);

    // A `not` correlated with an outer binding is not "no deadline".
    let (_, ran, ignored) = lower(
        "[:find (pull ?b [*]) :where (task ?b #{\"TODO\"}) [?b :block/scheduled ?d] (not [?b :block/deadline ?d])]",
    );
    assert_eq!(ran, vec!["task"]);
    assert!(ignored.contains(&"not".to_string()), "{ignored:?}");

    // A literal of the wrong type never matches in Logseq; it is not guessed.
    let (lowered, _, ignored) = lower("[:find (pull ?b [*]) :where [?b :block/marker 3]]");
    assert!(lowered.is_none());
    assert_eq!(ignored, vec!["pattern"]);

    // The pulled variable must be the one the clauses constrain.
    let (lowered, _, _) = lower("[:find (pull ?x [*]) :where [?b :block/marker \"TODO\"]]");
    assert!(lowered.is_none());
}
