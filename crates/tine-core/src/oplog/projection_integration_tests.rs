use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::oplog::{
    plan_projection, BlobDescription, BlockId, CrdtPeerCounter, CrdtPeerId, DeviceId,
    DocumentDependencies, DocumentId, FrontierV2, LogseqIdentityOrigin, LogseqUuid, ManagedPath,
    ManagedTextKind, MaterializationStats, MaterializedBlock, MaterializedPage, PageId,
    PolicyGeneratedAnchorReason, ProjectionClaimEvidence, ProjectionClaimParticipant,
    ProjectionCompletion, ProjectionEndpointBinding, ProjectionEndpointId, ProjectionError,
    ProjectionIntent, ProjectionPageState, ProjectionPrecondition, ProjectionReceiptStore,
    ProjectionStoreError, StoreError, WorkspaceId,
};
use crate::Graph;
use uuid::Uuid;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("tine-projection-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn workspace(value: u128) -> WorkspaceId {
    WorkspaceId::from_uuid(uuid(value))
}

fn logseq(value: u128) -> LogseqUuid {
    LogseqUuid::from_uuid(uuid(value))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn copy_directory_tree(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn block(
    value: u128,
    parent: Option<u128>,
    order: &str,
    content: impl Into<String>,
    logseq_uuid: Option<LogseqUuid>,
) -> MaterializedBlock {
    MaterializedBlock {
        block_id: BlockId::from_uuid(uuid(value)),
        home_document_id: DocumentId::from_uuid(uuid(10_000 + value)),
        parent: parent.map(|value| BlockId::from_uuid(uuid(value))),
        order: order.into(),
        logseq_uuid,
        logseq_identity_origin: logseq_uuid.map(|_| LogseqIdentityOrigin::ExternalImported),
        content: content.into(),
    }
}

fn generated_block(
    value: u128,
    parent: Option<u128>,
    order: &str,
    content: impl Into<String>,
    logseq_uuid: LogseqUuid,
    reason: PolicyGeneratedAnchorReason,
) -> MaterializedBlock {
    let mut block = block(value, parent, order, content, Some(logseq_uuid));
    block.logseq_identity_origin = Some(LogseqIdentityOrigin::PolicyGenerated { reason });
    block
}

fn page(path: &str, blocks: Vec<MaterializedBlock>) -> ProjectionPageState {
    let mut participants = BTreeMap::<LogseqUuid, Vec<ProjectionClaimParticipant>>::new();
    for block in &blocks {
        if let Some(logseq_uuid) = block.logseq_uuid {
            participants
                .entry(logseq_uuid)
                .or_default()
                .push(ProjectionClaimParticipant::new(
                    block.block_id,
                    block.home_document_id,
                ));
        }
    }
    let frontier = FrontierV2::new(
        participants
            .values()
            .flatten()
            .map(|participant| participant.home_document_id())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .enumerate()
            .map(|(index, document_id)| {
                DocumentDependencies::new(
                    document_id,
                    vec![CrdtPeerCounter::new(
                        CrdtPeerId::from_u64(index as u64 + 1),
                        0,
                    )],
                    vec![],
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap();
    ProjectionPageState {
        page: MaterializedPage {
            page_id: crate::oplog::PageId::from_uuid(uuid(500)),
            home_document_id: DocumentId::from_uuid(uuid(501)),
            name: crate::oplog::LogicalPageName::parse("Projection Page").unwrap(),
            path: ManagedPath::parse(path).unwrap(),
            kind: ManagedTextKind::Page,
            preamble: None,
            blocks,
            stats: MaterializationStats::default(),
        },
        frontier,
        claim_evidence: participants
            .into_iter()
            .map(|(uuid, participants)| ProjectionClaimEvidence::new(uuid, participants).unwrap())
            .collect(),
    }
}

fn plan(state: &ProjectionPageState, base: Option<&[u8]>) -> crate::oplog::ProjectionPlan {
    plan_projection(workspace(1), state, base).unwrap()
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

#[test]
fn hierarchy_order_annotations_and_bytes_are_deterministic() {
    let blocks = vec![
        block(4, Some(1), "b", "child b", None),
        block(2, None, "b", "root b", None),
        block(1, None, "a", "root a", None),
        block(3, Some(1), "a", "child a", None),
    ];
    let expected = "- root a\n\t- child a\n\t- child b\n- root b\n";
    let first = plan(&page("pages/tree.md", blocks.clone()), None);
    let mut reversed = blocks;
    reversed.reverse();
    let second = plan(&page("pages/tree.md", reversed), None);

    assert_eq!(text(first.target()), expected);
    assert_eq!(first.target(), second.target());
    assert_eq!(
        first.intent().encode().unwrap(),
        second.intent().encode().unwrap()
    );

    let annotations = first.intent().annotations();
    assert_eq!(annotations.len(), 4);
    assert_eq!(annotations[0].locator().components(), &[0]);
    assert_eq!(annotations[1].locator().components(), &[0, 0]);
    assert_eq!(annotations[2].locator().components(), &[0, 1]);
    assert_eq!(annotations[3].locator().components(), &[1]);
    for annotation in annotations {
        let span = annotation.span();
        let rendered = &first.target()[span.start() as usize..span.end() as usize];
        assert!(rendered.starts_with(b"- ") || rendered.windows(2).any(|pair| pair == b"- "));
        assert!(!rendered.is_empty());
    }
}

#[test]
fn imported_markdown_structural_trivia_is_span_instrumentation_transparent() {
    let sources = [
        "- a\n\n",
        "- a\n\n- b\n",
        "- a\r\n\r\n- b\r\n\r\n",
        "- a",
        "title:: Page\n\n\n- a\n- b\n",
        "- fenced\n  ```text\n\n  - literal\n  ```\n\n- task\n  :LOGBOOK:\n  CLOCK: [2026-07-29 Wed]\n  :END:\n\n- final\n",
    ];

    for (case, source) in sources.into_iter().enumerate() {
        let parsed = crate::doc::parse(source);
        let blocks = parsed
            .roots
            .iter()
            .enumerate()
            .map(|(index, parsed)| {
                block(
                    case as u128 * 100 + index as u128 + 1,
                    None,
                    &format!("{index:04}"),
                    parsed.raw.clone(),
                    None,
                )
            })
            .collect();
        let mut state = page("pages/structural-trivia.md", blocks);
        state.page.preamble = parsed.pre_block;
        let projected = plan_projection(workspace(1), &state, Some(source.as_bytes()))
            .unwrap_or_else(|error| panic!("case {case} rejected span instrumentation: {error}"));
        assert_eq!(
            projected.target(),
            source.as_bytes(),
            "case {case} did not reproduce exact source bytes"
        );
    }
}

#[test]
fn imported_org_structural_trivia_is_span_instrumentation_transparent() {
    let sources = [
        "* a\n\n",
        "* a\n\n* b\n",
        "* a\r\n\r\n* b\r\n\r\n",
        "* a",
        "#+TITLE: Page\n\n\n* a\n* b\n",
        "* source\n#+BEGIN_SRC text\n* literal headline\n#+END_SRC\n\n* task\n:LOGBOOK:\nCLOCK: [2026-07-29 Wed]\n:END:\n\n* final\n",
    ];

    for (case, source) in sources.into_iter().enumerate() {
        let parsed = crate::org::parse_org(source);
        let blocks = parsed
            .roots
            .iter()
            .enumerate()
            .map(|(index, parsed)| {
                block(
                    case as u128 * 100 + index as u128 + 1,
                    None,
                    &format!("{index:04}"),
                    parsed.raw.clone(),
                    None,
                )
            })
            .collect();
        let mut state = page("pages/structural-trivia.org", blocks);
        state.page.preamble = parsed.pre_block;
        let projected = plan_projection(workspace(1), &state, Some(source.as_bytes()))
            .unwrap_or_else(|error| panic!("case {case} rejected span instrumentation: {error}"));
        assert_eq!(
            projected.target(),
            source.as_bytes(),
            "case {case} did not reproduce exact source bytes"
        );
    }
}

#[test]
fn malformed_hierarchies_fail_closed() {
    let missing = page(
        "pages/missing.md",
        vec![block(1, Some(99), "a", "orphan", None)],
    );
    assert!(matches!(
        plan_projection(workspace(1), &missing, None,),
        Err(ProjectionError::MissingParent { .. })
    ));

    let cycle = page(
        "pages/cycle.md",
        vec![
            block(1, Some(2), "a", "one", None),
            block(2, Some(1), "a", "two", None),
        ],
    );
    assert!(matches!(
        plan_projection(workspace(1), &cycle, None,),
        Err(ProjectionError::CyclicTree(_))
    ));
}

#[test]
fn concurrent_equal_sibling_orders_use_block_identity_as_a_stable_tie_breaker() {
    let duplicate_order = page(
        "pages/order.md",
        vec![
            block(1, None, "same", "one", None),
            block(2, None, "same", "two", None),
        ],
    );
    let forward = plan_projection(workspace(1), &duplicate_order, None).unwrap();
    let mut reversed = duplicate_order.clone();
    reversed.page.blocks.reverse();
    let reversed = plan_projection(workspace(1), &reversed, None).unwrap();
    assert_eq!(text(forward.target()), "- one\n- two\n");
    assert_eq!(forward.target(), reversed.target());
}

#[test]
fn sparse_address_state_covers_reference_embed_and_export_deep_link() {
    let reasons = [
        (1, "reference", PolicyGeneratedAnchorReason::BlockReference),
        (2, "embed", PolicyGeneratedAnchorReason::BlockEmbed),
        (3, "export deep link", PolicyGeneratedAnchorReason::Export),
        (
            4,
            "copied deep link",
            PolicyGeneratedAnchorReason::CopiedDeepLink,
        ),
    ];
    let blocks = reasons
        .iter()
        .enumerate()
        .map(|(index, (value, label, reason))| {
            generated_block(
                *value,
                None,
                &format!("{index}"),
                *label,
                logseq(100 + value),
                *reason,
            )
        })
        .collect();
    let result = plan(&page("pages/addressable.md", blocks), None);
    for (value, label, _) in reasons {
        assert!(text(result.target()).contains(label));
        assert!(
            text(result.target()).contains(&format!("id:: {}", logseq(100 + value))),
            "{label} did not receive its addressable UUID"
        );
    }
    assert_eq!(result.generated_anchors().len(), 4);
}

#[test]
fn id_removal_change_existing_invalid_and_duplicate_raw_text_are_preserved() {
    let changed = logseq(200);
    let duplicate = logseq(201);
    let state = page(
        "pages/raw-ids.md",
        vec![
            block(1, None, "a", "removed", None),
            block(
                2,
                None,
                "b",
                format!("changed\nid:: {changed}"),
                Some(changed),
            ),
            block(3, None, "c", "invalid\nid:: definitely-not-a-uuid", None),
            block(
                4,
                None,
                "d",
                format!("duplicate one\nid:: {duplicate}"),
                None,
            ),
            block(
                5,
                None,
                "e",
                format!("duplicate two\nid:: {duplicate}"),
                None,
            ),
        ],
    );
    let result = plan(&state, None);
    let rendered = text(result.target());
    assert_eq!(rendered.matches("id::").count(), 4);
    assert!(rendered.contains("id:: definitely-not-a-uuid"));
    assert_eq!(rendered.matches(&duplicate.to_string()).count(), 2);
    assert!(result.generated_anchors().is_empty());

    let annotations = result.intent().annotations();
    assert_eq!(annotations[0].logseq_uuid(), None);
    assert_eq!(annotations[1].logseq_uuid(), Some(changed));
    assert_eq!(annotations[2].logseq_uuid(), None);
    assert_eq!(annotations[3].logseq_uuid(), None);
    assert_eq!(annotations[4].logseq_uuid(), None);
}

#[test]
fn inconsistent_duplicate_logseq_authority_is_rejected_without_cleaning_bytes() {
    let duplicate = logseq(300);
    let state = page(
        "pages/ambiguous.md",
        vec![
            block(
                1,
                None,
                "a",
                format!("one\nid:: {duplicate}"),
                Some(duplicate),
            ),
            block(2, None, "b", format!("two\nid:: {duplicate}"), None),
        ],
    );
    assert!(matches!(
        plan_projection(
            workspace(1),
            &state,
            None,
        ),
        Err(ProjectionError::AmbiguousRawLogseqId(id)) if id == duplicate
    ));
}

#[test]
fn external_identity_without_parser_confirmed_raw_property_fails_closed() {
    let external = logseq(450);
    for content in [
        format!("outside\n```\nid:: {external}\n```"),
        format!("outside `{external}`\n`id:: {external}`"),
    ] {
        let state = page(
            "pages/external.md",
            vec![block(1, None, "a", content, Some(external))],
        );
        assert!(matches!(
            plan_projection(
                workspace(1),
                &state,
                None,
            ),
            Err(ProjectionError::MissingExternalRawLogseqId {
                block: _,
                logseq_uuid
            }) if logseq_uuid == external
        ));
    }
}

#[test]
fn authoritative_preamble_and_structure_do_not_come_from_base() {
    let mut state = page(
        "pages/preamble.md",
        vec![
            block(1, None, "a", "root\nbody", None),
            block(2, Some(1), "a", "child", None),
        ],
    );
    state.page.preamble = Some("title:: Authoritative\nfree text".into());
    let result = plan(&state, Some(b"title:: stale base\n\n- old\n"));
    assert_eq!(
        text(result.target()),
        "title:: Authoritative\nfree text\n\n- root\n  body\n\t- child\n"
    );
    let parsed = crate::doc::parse(text(result.target()));
    assert_eq!(
        parsed.pre_block.as_deref(),
        Some("title:: Authoritative\nfree text")
    );
    assert_eq!(parsed.roots[0].raw, "root\nbody");
    assert_eq!(parsed.roots[0].children[0].raw, "child");
}

#[test]
fn untouched_supported_projection_remains_byte_identical() {
    let base = b"title:: Exact\r\n\r\n- root\r\n  body\r\n  - child\r\n";
    let mut state = page(
        "pages/untouched.md",
        vec![
            block(1, None, "a", "root\nbody", None),
            block(2, Some(1), "a", "child", None),
        ],
    );
    state.page.preamble = Some("title:: Exact".into());
    let result = plan(&state, Some(base));
    assert_eq!(result.target(), base);
}

#[test]
fn org_parser_distinguishes_real_mixed_case_id_from_source_and_example_blocks() {
    let existing = logseq(460);
    let generated = logseq(461);
    let state = page(
        "pages/org-parser.org",
        vec![
            block(
                1,
                None,
                "a",
                format!("real\n:properties:\n:Id: {existing}\n:end:"),
                Some(existing),
            ),
            generated_block(
                2,
                None,
                "b",
                format!(
                    "literal\n#+BEGIN_SRC text\n:PROPERTIES:\n:ID: {generated}\n:END:\n#+END_SRC\n#+BEGIN_EXAMPLE\n:ID: {generated}\n#+END_EXAMPLE"
                ),
                generated,
                PolicyGeneratedAnchorReason::BlockReference,
            ),
        ],
    );
    let result = plan(&state, None);
    let parsed = crate::org::parse_org(text(result.target()));
    let existing_text = existing.to_string();
    let generated_text = generated.to_string();
    assert_eq!(
        parsed.roots[0].property("id").as_deref(),
        Some(existing_text.as_str())
    );
    assert_eq!(
        parsed.roots[1].property("id").as_deref(),
        Some(generated_text.as_str())
    );
    assert_eq!(text(result.target()).matches(&generated_text).count(), 3);
}

#[test]
fn markdown_org_and_crlf_use_one_projection_path() {
    let id = logseq(500);
    let markdown = plan(
        &page(
            "pages/format.md",
            vec![
                block(1, None, "a", "root", None),
                generated_block(
                    2,
                    Some(1),
                    "a",
                    "child",
                    id,
                    PolicyGeneratedAnchorReason::BlockEmbed,
                ),
            ],
        ),
        Some(b"- old\r\n  - child\r\n"),
    );
    assert_eq!(
        text(markdown.target()),
        format!("- root\r\n  - child\r\n    id:: {id}\r\n")
    );

    let org = plan(
        &page(
            "pages/format.org",
            vec![generated_block(
                1,
                None,
                "a",
                "TODO title\nSCHEDULED: <2026-07-23 Thu>\nbody",
                id,
                PolicyGeneratedAnchorReason::Export,
            )],
        ),
        Some(b"* old\r\n"),
    );
    assert_eq!(
        text(org.target()),
        format!(
            "* TODO title\r\nSCHEDULED: <2026-07-23 Thu>\r\n:PROPERTIES:\r\n:id: {id}\r\n:END:\r\nbody\r\n"
        )
    );
}

#[test]
fn org_existing_and_invalid_id_drawer_text_is_preserved() {
    let existing = logseq(600);
    let generated = logseq(601);
    let state = page(
        "pages/org-ids.org",
        vec![
            block(
                1,
                None,
                "a",
                format!("existing\n:PROPERTIES:\n:id: {existing}\n:custom: keep\n:END:"),
                Some(existing),
            ),
            generated_block(
                2,
                None,
                "b",
                "invalid\n:PROPERTIES:\n:id: invalid-raw\n:custom: keep-too\n:END:",
                generated,
                PolicyGeneratedAnchorReason::CopiedDeepLink,
            ),
        ],
    );
    let result = plan(&state, None);
    let rendered = text(result.target());
    assert_eq!(rendered.matches(&existing.to_string()).count(), 1);
    assert!(rendered.contains(":id: invalid-raw"));
    assert!(rendered.contains(&format!(":id: {generated}")));
    assert!(rendered.contains(":id: invalid-raw\n:custom: keep-too"));
}

#[test]
fn receipt_store_orders_base_before_intent_and_enumerates_incomplete() {
    let dir = TestDir::new("store-order");
    let state = page("pages/store.md", vec![block(1, None, "a", "target", None)]);
    let projection = plan(&state, Some(b"- base\n"));
    let store = ProjectionReceiptStore::open(dir.path(), workspace(1)).unwrap();
    let intent_id = store
        .publish_intent(projection.intent(), Some(b"- base\n"))
        .unwrap();
    let base = match projection.intent().precondition() {
        ProjectionPrecondition::Base(description) => description,
        ProjectionPrecondition::Absent => panic!("expected base"),
    };
    assert!(dir
        .path()
        .join("bases")
        .join(format!("{}.base", hex(base.sha256())))
        .is_file());
    assert!(dir
        .path()
        .join("intents")
        .join(format!("{}.intent", hex(intent_id.as_bytes())))
        .is_file());
    let completion_path = dir
        .path()
        .join("completions")
        .join(format!("{}.completion", hex(intent_id.as_bytes())));
    assert!(!completion_path.exists());
    assert_eq!(
        store.incomplete_intents().unwrap(),
        vec![projection.intent().clone()]
    );
}

#[test]
fn receipt_store_requires_exact_base_presence_and_descriptor_match() {
    let dir = TestDir::new("base-consistency");
    let store = ProjectionReceiptStore::open(dir.path(), workspace(1)).unwrap();
    let absent = plan(
        &page(
            "archive/pages/absent.md",
            vec![block(1, None, "a", "target", None)],
        ),
        None,
    );
    assert!(matches!(
        store.publish_intent(absent.intent(), Some(b"unexpected")),
        Err(ProjectionStoreError::UnexpectedBase)
    ));

    let based = plan(
        &page(
            "archive/pages/based.md",
            vec![block(1, None, "a", "target", None)],
        ),
        Some(b"- exact base\n"),
    );
    let description = match based.intent().precondition() {
        ProjectionPrecondition::Base(description) => *description,
        ProjectionPrecondition::Absent => panic!("expected base"),
    };
    assert!(matches!(
        store.publish_intent(based.intent(), None),
        Err(ProjectionStoreError::MissingBase(found)) if found == description
    ));
    assert!(matches!(
        store.publish_intent(based.intent(), Some(b"- wrong base\n")),
        Err(ProjectionStoreError::BaseEvidenceMismatch(found)) if found == description
    ));
    assert!(fs::read_dir(dir.path().join("bases"))
        .unwrap()
        .next()
        .is_none());
    assert!(fs::read_dir(dir.path().join("intents"))
        .unwrap()
        .next()
        .is_none());

    store
        .publish_intent(based.intent(), Some(b"- exact base\n"))
        .unwrap();
    assert_eq!(
        store.load_base(based.intent()).unwrap().unwrap().bytes(),
        b"- exact base\n"
    );
    assert!(store.load_base(absent.intent()).unwrap().is_none());
}

#[test]
fn declared_oversized_target_is_rejected_before_any_evidence_publication() {
    const LIMIT: u64 = 64 * 1024 * 1024;

    let dir = TestDir::new("declared-oversized-target");
    let store = ProjectionReceiptStore::open(dir.path(), workspace(1)).unwrap();
    let intent = ProjectionIntent::new(
        workspace(1),
        PageId::from_uuid(uuid(900)),
        ManagedPath::parse("pages/oversized.md").unwrap(),
        FrontierV2::default(),
        Vec::new(),
        ProjectionPrecondition::Absent,
        crate::oplog::ProjectionTargetKind::Present,
        BlobDescription::from_parts([0; 32], LIMIT + 1),
        Vec::new(),
    )
    .unwrap();

    assert!(matches!(
        store.publish_intent(&intent, None),
        Err(ProjectionStoreError::EvidenceTooLarge {
            kind: "projection target",
            declared,
            limit: LIMIT,
        }) if declared == LIMIT + 1
    ));
    assert!(fs::read_dir(dir.path().join("bases"))
        .unwrap()
        .next()
        .is_none());
    assert!(fs::read_dir(dir.path().join("intents"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn incomplete_enumeration_skips_only_canonical_publication_temps() {
    let dir = TestDir::new("enumeration-names");
    let store = ProjectionReceiptStore::open(dir.path(), workspace(1)).unwrap();
    let state = page(
        "pages/enumeration.md",
        vec![block(1, None, "a", "target", None)],
    );
    let projection = plan(&state, None);
    store.publish_intent(projection.intent(), None).unwrap();
    fs::write(
        dir.path()
            .join("intents")
            .join(format!(".tmp-{}", Uuid::new_v4())),
        b"crash residue",
    )
    .unwrap();
    assert_eq!(store.incomplete_intents().unwrap().len(), 1);

    fs::write(
        dir.path().join("intents").join(".tmp-not-canonical"),
        b"malformed residue",
    )
    .unwrap();
    assert!(matches!(
        store.incomplete_intents(),
        Err(ProjectionStoreError::MalformedEvidenceName(name))
            if name == ".tmp-not-canonical"
    ));
}

#[cfg(unix)]
#[test]
fn base_survives_when_intent_namespace_fails_before_publication() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("base-first");
    let projection = plan(
        &page(
            "pages/base-first.md",
            vec![block(1, None, "a", "target", None)],
        ),
        Some(b"- base\n"),
    );
    let store = ProjectionReceiptStore::open(dir.path(), workspace(1)).unwrap();
    fs::remove_dir(dir.path().join("intents")).unwrap();
    symlink(dir.path().join("bases"), dir.path().join("intents")).unwrap();
    assert!(store
        .publish_intent(projection.intent(), Some(b"- base\n"))
        .is_err());

    let base = match projection.intent().precondition() {
        ProjectionPrecondition::Base(description) => description,
        ProjectionPrecondition::Absent => panic!("expected base"),
    };
    assert!(dir
        .path()
        .join("bases")
        .join(format!("{}.base", hex(base.sha256())))
        .is_file());
    assert!(!dir
        .path()
        .join("bases")
        .join(format!(
            "{}.intent",
            hex(projection.intent().id().unwrap().as_bytes())
        ))
        .exists());
}

#[test]
fn corrupt_missing_noncanonical_and_unknown_evidence_fail_closed() {
    let dir = TestDir::new("corrupt");
    let projection = plan(
        &page(
            "pages/corrupt.md",
            vec![block(1, None, "a", "target", None)],
        ),
        Some(b"- base\n"),
    );
    let store = ProjectionReceiptStore::open(dir.path(), workspace(1)).unwrap();
    let intent_id = store
        .publish_intent(projection.intent(), Some(b"- base\n"))
        .unwrap();
    let base = match projection.intent().precondition() {
        ProjectionPrecondition::Base(description) => description,
        ProjectionPrecondition::Absent => panic!("expected base"),
    };
    let base_path = dir
        .path()
        .join("bases")
        .join(format!("{}.base", hex(base.sha256())));
    fs::remove_file(&base_path).unwrap();
    assert!(matches!(
        store.load_intent(intent_id),
        Err(ProjectionStoreError::MissingBase(_))
    ));
    fs::write(&base_path, b"- base\n").unwrap();
    fs::write(&base_path, b"- evil\n").unwrap();
    assert!(matches!(
        store.load_intent(intent_id),
        Err(ProjectionStoreError::BaseEvidenceMismatch(_))
    ));
    fs::write(&base_path, b"- base\n").unwrap();

    let intent_path = dir
        .path()
        .join("intents")
        .join(format!("{}.intent", hex(intent_id.as_bytes())));
    let canonical = fs::read(&intent_path).unwrap();
    let future = String::from_utf8(canonical.clone()).unwrap().replacen(
        "\"receipt_schema_version\":5",
        "\"receipt_schema_version\":99",
        1,
    );
    fs::write(&intent_path, future).unwrap();
    assert!(matches!(
        store.load_intent(intent_id),
        Err(ProjectionStoreError::Receipt(error))
            if error.to_string().contains("unknown receipt schema 99")
    ));

    let mut noncanonical = canonical.clone();
    noncanonical.push(b'\n');
    fs::write(&intent_path, noncanonical).unwrap();
    assert!(matches!(
        store.load_intent(intent_id),
        Err(ProjectionStoreError::NonCanonical("projection intent"))
    ));
    let collision = store
        .publish_intent(projection.intent(), Some(b"- base\n"))
        .unwrap_err();
    assert!(matches!(
        collision,
        ProjectionStoreError::Store(error)
            if matches!(
                error.as_ref(),
                StoreError::ImmutableCollision("projection intent")
            )
    ));
    fs::write(&intent_path, b"{").unwrap();
    assert!(store.load_intent(intent_id).is_err());

    let claim_dir = TestDir::new("future-claim");
    let mut claim = Vec::new();
    // The CURRENT magic naming a version this build does not know. It moves
    // with the store format: the point of the case is "current family, future
    // version", not the literal bytes.
    claim.extend_from_slice(b"TINEPR6\0");
    claim.extend_from_slice(&99_u32.to_be_bytes());
    claim.extend_from_slice(&[0_u8; 32]);
    claim.extend_from_slice(workspace(1).as_uuid().as_bytes());
    claim.extend_from_slice(&[0_u8; 1 + 16 + 16 + 32]);
    fs::write(claim_dir.path().join("projection-receipts.claim"), claim).unwrap();
    assert!(matches!(
        ProjectionReceiptStore::open(claim_dir.path(), workspace(1)),
        Err(ProjectionStoreError::Operation { operation, source })
            if operation == "initialize private receipt store"
                && matches!(*source, ProjectionStoreError::UnknownStoreVersion(99))
    ));
}

/// The living contract states the private receipt-store claim version and
/// every prior magic it refuses. If a change moves one without the other, this
/// fails.
#[test]
fn the_contract_states_the_receipt_store_claim_version() {
    let contract = include_str!("../../../../docs/storage-sync-contract.md");
    assert!(
        contract.contains("The private receipt-store claim, and when it is checked"),
        "the storage contract must carry the receipt-store claim section"
    );
    assert!(
        contract.contains(&format!(
            "`{}`, `STORE_CLAIM_VERSION` = {}",
            crate::oplog::projection_store::store_claim_magic_display(),
            crate::oplog::projection_store::store_claim_version()
        )),
        "the storage contract must state the current claim magic and version"
    );
    for magic in crate::oplog::projection_store::prior_store_claim_magics_display() {
        assert!(
            contract.contains(&format!("`{magic}`")),
            "the storage contract must list refused prior claim magic {magic}"
        );
    }
    assert!(
        contract.contains("`STORE_CLAIM_LEN`"),
        "the storage contract must name the exact-length rule the precheck enforces"
    );
    assert!(
        contract.contains("`target_kind`"),
        "the storage contract must describe the explicit target-kind field"
    );
}

#[test]
fn intent_and_completion_records_carry_an_explicit_target_kind() {
    let dir = TestDir::new("explicit-target-kind");
    let store = ProjectionReceiptStore::open(dir.path(), workspace(1)).unwrap();

    // R16-C1: an absent target and a page that renders to zero bytes flatten to
    // the SAME blob description. Only the explicit discriminant separates them,
    // so a later consumer must never read byte length to decide.
    let absent = ProjectionIntent::new(
        workspace(1),
        PageId::from_uuid(uuid(940)),
        ManagedPath::parse("pages/target-kind.md").unwrap(),
        FrontierV2::default(),
        Vec::new(),
        ProjectionPrecondition::Base(BlobDescription::of(b"- base\n")),
        crate::oplog::ProjectionTargetKind::Absent,
        BlobDescription::of(&[]),
        Vec::new(),
    )
    .unwrap();
    let empty_present = ProjectionIntent::new(
        workspace(1),
        PageId::from_uuid(uuid(940)),
        ManagedPath::parse("pages/target-kind.md").unwrap(),
        FrontierV2::default(),
        Vec::new(),
        ProjectionPrecondition::Base(BlobDescription::of(b"- base\n")),
        crate::oplog::ProjectionTargetKind::Present,
        BlobDescription::of(&[]),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(absent.target(), empty_present.target());
    assert_eq!(
        absent.target_kind(),
        crate::oplog::ProjectionTargetKind::Absent
    );
    assert_eq!(
        empty_present.target_kind(),
        crate::oplog::ProjectionTargetKind::Present
    );
    assert!(absent.target_kind().is_absent());
    assert!(!empty_present.target_kind().is_absent());
    // The intent id derivation is deliberately unchanged by (c): the kind is a
    // stored field, not an identity input.
    assert_eq!(absent.id().unwrap(), empty_present.id().unwrap());
    // ... and a replay that differs only in kind is NOT the same projection.
    assert!(!absent.matches_replay_except_frontier(&empty_present));

    // It survives the on-disk round trip, and a record whose declared kind
    // contradicts its target bytes is refused.
    for intent in [&absent, &empty_present] {
        let bytes = intent.encode().unwrap();
        assert!(
            std::str::from_utf8(&bytes).unwrap().contains("target_kind"),
            "the discriminant must be part of the canonical encoding"
        );
        assert_eq!(&ProjectionIntent::decode(&bytes).unwrap(), intent);
    }
    let mut contradictory = absent.encode().unwrap();
    let text = String::from_utf8(std::mem::take(&mut contradictory)).unwrap();
    let text = text.replace("\"byte_length\":0", "\"byte_length\":7");
    assert!(
        ProjectionIntent::decode(text.as_bytes()).is_err(),
        "an absent target that declares bytes must be refused"
    );

    // Completions record the kind their intent declared.
    store
        .publish_intent(&empty_present, Some(b"- base\n"))
        .unwrap();
    let completion = ProjectionCompletion::for_intent(&empty_present, &[]).unwrap();
    assert_eq!(
        completion.target_kind(),
        crate::oplog::ProjectionTargetKind::Present
    );
    let encoded = completion.encode().unwrap();
    assert!(std::str::from_utf8(&encoded)
        .unwrap()
        .contains("target_kind"));
    assert_eq!(
        ProjectionCompletion::decode_bound(&encoded, &empty_present).unwrap(),
        completion
    );
    // A completion carrying the other kind is not bound to this intent.
    let absent_completion = ProjectionCompletion::for_intent(&absent, &[]).unwrap();
    assert_eq!(
        absent_completion.target_kind(),
        crate::oplog::ProjectionTargetKind::Absent
    );
    assert!(absent_completion.validate_against(&empty_present).is_err());
}

#[test]
fn claimless_nonempty_and_prior_version_receipt_roots_fail_without_mutation() {
    let claimless = TestDir::new("claimless-nonempty");
    fs::write(claimless.path().join("copied.completion"), b"evidence").unwrap();
    let before = fs::read_dir(claimless.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let claimless_error = ProjectionReceiptStore::open(claimless.path(), workspace(1))
        .expect_err("claimless nonempty receipt root must be rejected");
    assert!(matches!(
        claimless_error,
        ProjectionStoreError::Operation { operation, source }
            if operation == "initialize private receipt store"
                && matches!(*source, ProjectionStoreError::ClaimlessNonemptyStore)
    ));
    let after = fs::read_dir(claimless.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(after, before);

    // Every prior magic, including the pre-(c) TINEPR5, is recognized only
    // enough to refuse without mutation. There is no migration and no dual
    // acceptance; the outer graph-open lifecycle owns backup and rebuild.
    for (magic, found) in [(&b"TINEPR5\0"[..], 5_u32), (&b"TINEPR4\0"[..], 4)] {
        let prior = TestDir::new("prior-receipt-claim");
        let mut claim = Vec::new();
        claim.extend_from_slice(magic);
        claim.extend_from_slice(&found.to_be_bytes());
        claim.extend_from_slice(workspace(1).as_uuid().as_bytes());
        claim.extend_from_slice(&[0_u8; 1 + 16 + 16 + 32]);
        fs::write(prior.path().join("projection-receipts.claim"), &claim).unwrap();
        let error = ProjectionReceiptStore::open(prior.path(), workspace(1))
            .expect_err("a prior store claim must be refused");
        assert!(
            matches!(
                &error,
                ProjectionStoreError::Operation { operation, source }
                    if operation == "initialize private receipt store"
                        && matches!(
                            **source,
                            ProjectionStoreError::UpgradeRequired {
                                found: refused,
                                current: 6
                            } if refused == found
                        )
            ),
            "unexpected error: {error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("backed up")
                && message.contains("rebuilt")
                && message.contains("Markdown/Org")
                && !message.contains("re-activate"),
            "the low-level refusal must name automatic blank-slate recovery, not manual re-activation: {message}"
        );
        assert_eq!(
            fs::read(prior.path().join("projection-receipts.claim")).unwrap(),
            claim
        );
        assert_eq!(fs::read_dir(prior.path()).unwrap().count(), 1);
    }
}

#[test]
fn receipt_resource_identity_survives_move_and_rejects_a_simultaneous_copy() {
    let parent = TestDir::new("receipt-resource");
    let original = parent.path().join("original");
    let moved = parent.path().join("moved");
    let copied = parent.path().join("copied");
    let opened = ProjectionReceiptStore::open(&original, workspace(1)).unwrap();
    let store_id = opened.store_id();
    let projection = plan(
        &page(
            "pages/moved-receipts.md",
            vec![block(1, None, "a", "moved", None)],
        ),
        None,
    );
    opened.publish_intent(projection.intent(), None).unwrap();
    drop(opened);
    fs::rename(&original, &moved).unwrap();
    let reopened = ProjectionReceiptStore::open(&moved, workspace(1)).unwrap();
    assert_eq!(reopened.store_id(), store_id);
    assert!(reopened
        .load_attempt_reservations(projection.intent())
        .unwrap()
        .is_empty());
    drop(reopened);

    copy_directory_tree(&moved, &copied);
    assert!(matches!(
        ProjectionReceiptStore::open(&copied, workspace(1)),
        Err(ProjectionStoreError::Operation { operation, source })
            if operation == "initialize private receipt store"
                && matches!(*source, ProjectionStoreError::EndpointBindingMismatch)
    ));
}

#[test]
fn every_top_level_receipt_namespace_fails_closed_after_deletion_or_copy_replacement() {
    let base = b"- before\n";
    for namespace in ["bases", "intents", "completions", "attempts", "forensics"] {
        let root = TestDir::new(&format!("top-level-authority-{namespace}"));
        let store = ProjectionReceiptStore::open(root.path(), workspace(1)).unwrap();
        let projection = plan(
            &page(
                "pages/authority.md",
                vec![block(1, None, "a", "after", None)],
            ),
            Some(base),
        );
        let intent = projection.intent();
        let intent_id = store.publish_intent(intent, Some(base)).unwrap();
        let live = root.path().join(namespace);
        let retained = root.path().join(format!("{namespace}-retained"));
        fs::rename(&live, &retained).unwrap();

        let missing = match namespace {
            "bases" => store.load_base(intent).map(|_| ()),
            "intents" => store.load_intent(intent_id).map(|_| ()),
            "completions" => store.load_completion(intent).map(|_| ()),
            "attempts" => store.load_attempt_reservations(intent).map(|_| ()),
            "forensics" => store.local_forensic_evidence(intent).map(|_| ()),
            _ => unreachable!(),
        };
        assert!(
            matches!(missing, Err(ProjectionStoreError::NamespaceSubstitution(_))),
            "{namespace}: {missing:?}"
        );

        copy_directory_tree(&retained, &live);
        let replaced = match namespace {
            "bases" => store.load_base(intent).map(|_| ()),
            "intents" => store.load_intent(intent_id).map(|_| ()),
            "completions" => store.load_completion(intent).map(|_| ()),
            "attempts" => store.load_attempt_reservations(intent).map(|_| ()),
            "forensics" => store.local_forensic_evidence(intent).map(|_| ()),
            _ => unreachable!(),
        };
        assert!(
            matches!(
                replaced,
                Err(ProjectionStoreError::NamespaceSubstitution(_))
            ),
            "{namespace}: {replaced:?}"
        );
    }
}

/// A per-intent recovery namespace that is missing on reopen is **recreated**,
/// not refused.
///
/// Refusal census 2026-08-26 (P-census). This test replaces
/// `established_per_intent_namespaces_cannot_be_deleted_replaced_or_recreated_after_reopen`,
/// which asserted the opposite: that a per-intent namespace bound by its
/// reservation/authority artifact pair could never be recreated by name. That
/// refusal defended only an actor able to rename directories inside Tine's
/// app-private receipt store — out of scope per
/// `specs/notes/2026-08-07-trust-model-and-threat-model-decision.md` — while
/// making a lost or torn 1 KB binding artifact wedge the page's projection
/// permanently. The artifacts and the refusals are gone; absence is recovery.
#[test]
fn a_missing_per_intent_recovery_namespace_is_recreated_instead_of_wedging_projection() {
    for namespace in ["attempts", "forensics"] {
        let root = TestDir::new(&format!("per-intent-recovery-{namespace}"));
        let store = ProjectionReceiptStore::open(root.path(), workspace(1)).unwrap();
        let projection = plan(
            &page(
                "pages/per-intent.md",
                vec![block(1, None, "a", "after", None)],
            ),
            None,
        );
        let intent = projection.intent();
        let intent_id = store.publish_intent(intent, None).unwrap();
        let name = intent_id.to_string();
        let live = root.path().join(namespace).join(&name);
        assert!(live.is_dir());
        drop(store);

        // The state a crash between `mkdir` and the next barrier can leave, and
        // the state the deleted binding artifacts used to make unrecoverable.
        fs::remove_dir_all(&live).unwrap();

        let reopened = ProjectionReceiptStore::open(root.path(), workspace(1)).unwrap();
        let reservation = reopened
            .reserve_attempt(intent)
            .expect("a missing per-intent namespace is recreated, not refused");
        // `begin_mutation` needs BOTH per-intent namespaces, so it is what
        // proves the forensics namespace recovers too.
        let authority = reopened
            .begin_mutation(intent, Some(&reservation))
            .expect("a missing per-intent namespace is recreated, not refused");
        drop(authority);
        assert!(
            live.is_dir(),
            "the {namespace} namespace is recreated on demand"
        );
        assert_eq!(
            reopened.load_attempt_reservations(intent).unwrap(),
            vec![reservation]
        );
    }
}
