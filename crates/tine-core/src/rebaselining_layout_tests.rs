//! Feasibility gate only: no production decoder or alternative live format.
//! The postcard vector is a disposable benchmark transport, not a generation
//! manifest. This isolates upstream Loro per-document cost before changing the
//! accepted transaction protocol. Final packed storage and rendering need their
//! own end-to-end gates.
use super::*;
use crate::oplog::semantic::{BlockState, PagePreambleState, PageState, VisibleMembership};
use loro::{ContainerTrait, ExportMode, LoroDoc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Key {
    Entity(uuid::Uuid),
    Membership(uuid::Uuid, uuid::Uuid),
}

fn put<T: Serialize>(doc: &LoroDoc, key: &str, value: &T) {
    doc.get_map("meta")
        .insert(key, serde_json::to_string(value).unwrap())
        .unwrap();
}

fn get<T: serde::de::DeserializeOwned>(doc: &LoroDoc, key: &str) -> T {
    let value = doc.get_map("meta").get(key).unwrap().get_deep_value();
    serde_json::from_str(value.as_string().unwrap()).unwrap()
}

fn sealed(doc: LoroDoc) -> Vec<u8> {
    put(&doc, "checkpoint", &1u64);
    doc.commit();
    doc.export(ExportMode::shallow_snapshot(&doc.oplog_frontiers()))
        .unwrap()
}

fn memory_kib() -> Option<u64> {
    // Process RSS, not an allocator attribution; report both sides of each phase.
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|line| line.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[test]
#[ignore = "manual release gate: fixed-schema Loro layout on an anonymized corpus copy"]
fn retirable_loro_layout_real_corpus_gate() {
    assert!(!cfg!(debug_assertions), "release-only corpus gate");
    let source = real_graph_copy_source_from_env("TINE_REBASELINING_GRAPH_COPY");
    let seed = 0xc300_0000;
    let initiator = ActivationFixture::copied_graph("entity-layout-initiator", seed, &source);
    let joiner = ActivationFixture::copied_graph("entity-layout-joiner", seed, &source);
    let (_, joiner, handle, descriptor) =
        pending_generation_join_from_fixtures(initiator, joiner, seed);
    handle.join_shared(descriptor).unwrap();
    assert!(matches!(
        handle.clean_shutdown(),
        Ok(SyncShutdownOutcome::Safe(_))
    ));
    drop(handle);
    let resources = open_clean_runtime_resources(&reopen_request(&joiner.request))
        .unwrap()
        .unwrap();
    let before = resources.runtime.engine().canonical_snapshot().unwrap();
    // The corpus is a fresh activation: retired roots are tested independently.
    assert!(before
        .pages
        .iter()
        .all(|(_, state)| matches!(state, PageState::Live { .. })));
    // Independent existing renderer control. These frontiers/claim proofs are
    // fixture authority only; this spike does not validate a new generation.
    let mut projection_controls = BTreeMap::new();
    let engine = resources.runtime.engine();
    let claims = engine
        .clean_transient_projection_claim_snapshot()
        .unwrap()
        .unwrap();
    let materializer = engine
        .clean_projection_bulk_materializer_with_rebuild_claims(
            &engine.accepted_frontier_root().unwrap(),
            crate::oplog::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT,
            claims,
        )
        .unwrap();
    for pages in before.pages.chunks(64) {
        let ids: Vec<_> = pages.iter().map(|(id, _)| *id).collect();
        for state in materializer.materialize_pages_for_projection(&ids).unwrap() {
            let state = state.unwrap();
            let rendered = crate::oplog::projection::plan_projection(
                joiner.request.identities.workspace_id,
                &state,
                None,
            )
            .unwrap();
            projection_controls.insert(state.page.page_id, (state, rendered.target().to_vec()));
        }
    }
    drop(materializer);
    let rss_before = memory_kib();
    let started = Instant::now();
    let mut records = Vec::new();
    let graph = LoroDoc::new();
    put(&graph, "kind", &"graph");
    records.push((Key::Entity(uuid::Uuid::new_v4()), sealed(graph)));
    let mut page_docs = BTreeMap::new();
    let mut block_docs = BTreeMap::new();
    let preambles: BTreeMap<_, _> = before
        .page_preambles
        .iter()
        .map(|preamble| (preamble.page_id, preamble))
        .collect();
    for (id, state) in &before.pages {
        let document_id = uuid::Uuid::new_v4();
        page_docs.insert(*id, document_id);
        let doc = LoroDoc::new();
        put(&doc, "kind", &"page");
        put(&doc, "identity", id);
        put(&doc, "state", state);
        put(&doc, "preamble", &preambles.get(id).copied());
        records.push((Key::Entity(document_id), sealed(doc)));
    }
    for block in &before.blocks {
        let document_id = uuid::Uuid::new_v4();
        block_docs.insert(block.block_id, document_id);
        let doc = LoroDoc::new();
        put(&doc, "kind", &"block");
        put(&doc, "identity", &block.block_id);
        // Existing home is birth provenance in this fixture; the new document
        // key is distinct and is the stable text home in the proposed format.
        put(&doc, "birth", &block.home_document_id);
        put(&doc, "owner", &block.owner);
        put(&doc, "logseq_uuid", &block.logseq_uuid);
        put(&doc, "logseq_origin", &block.logseq_identity_origin);
        doc.get_text("text").insert(0, &block.content).unwrap();
        records.push((Key::Entity(document_id), sealed(doc)));
    }
    for membership in &before.memberships {
        let doc = LoroDoc::new();
        put(&doc, "kind", &"membership");
        put(&doc, "identity", &(membership.block_id, membership.page_id));
        put(&doc, "claim", membership);
        records.push((
            Key::Membership(
                block_docs[&membership.block_id],
                page_docs[&membership.page_id],
            ),
            sealed(doc),
        ));
    }
    let build_ms = started.elapsed().as_millis();
    let document_count = records.len();
    let snapshot_bytes: usize = records.iter().map(|(_, bytes)| bytes.len()).sum();
    let started = Instant::now();
    let packed = postcard::to_allocvec(&records).unwrap();
    let packed_bytes = packed.len();
    let pack = joiner.root.join("disposable-layout-fixture.postcard");
    fs::write(&pack, &packed).unwrap();
    drop(packed);
    drop(records);
    let pack_ms = started.elapsed().as_millis();
    let rss_before_load = memory_kib();
    let started = Instant::now();
    let bytes = fs::read(&pack).unwrap();
    let records: Vec<(Key, Vec<u8>)> = postcard::from_bytes(&bytes).unwrap();
    drop(bytes);
    let read_ms = started.elapsed().as_millis();
    // Packed state is retained; decoded upstream handles are a bounded cache.
    // A control run may explicitly retain all handles to quantify why that
    // tempting implementation is unacceptable on mobile.
    let cache_limit = if std::env::var_os("TINE_LAYOUT_RETAIN_ALL_HANDLES").is_some() {
        document_count
    } else {
        64
    };
    let mut cache = std::collections::VecDeque::new();
    let mut import_elapsed = Duration::ZERO;
    let mut materialize_elapsed = Duration::ZERO;
    let mut after = CanonicalSnapshot::default();
    for (key, snapshot) in records {
        // Evict before allocation, so the bound includes the document in flight.
        if cache.len() == cache_limit {
            cache.pop_front();
        }
        let started = Instant::now();
        let doc = LoroDoc::new();
        assert!(doc.import(&snapshot).unwrap().pending.is_none());
        import_elapsed += started.elapsed();
        let started = Instant::now();
        let key = &key;
        let doc = &doc;
        let kind: String = get(doc, "kind");
        match kind.as_str() {
            "graph" => (),
            "page" => {
                let id = get(doc, "identity");
                assert!(matches!(key, Key::Entity(value) if *value == page_docs[&id]));
                after.pages.push((id, get(doc, "state")));
                if let Some(preamble) = get::<Option<PagePreambleState>>(doc, "preamble") {
                    after.page_preambles.push(preamble);
                }
            }
            "block" => {
                let block_id = get(doc, "identity");
                assert!(matches!(key, Key::Entity(value) if *value == block_docs[&block_id]));
                after.blocks.push(BlockState {
                    block_id,
                    home_document_id: get(doc, "birth"),
                    owner: get(doc, "owner"),
                    logseq_uuid: get(doc, "logseq_uuid"),
                    logseq_identity_origin: get(doc, "logseq_origin"),
                    content: doc.get_text("text").to_string(),
                });
            }
            "membership" => {
                let claim: VisibleMembership = get(doc, "claim");
                assert!(matches!(key, Key::Membership(block, page)
                    if *block == block_docs[&claim.block_id] && *page == page_docs[&claim.page_id]));
                after.memberships.push(claim);
            }
            _ => panic!("unexpected fixture document kind"),
        }

        materialize_elapsed += started.elapsed();
        cache.push_back(doc.clone());
    }
    let import_ms = import_elapsed.as_millis();
    let rss_with_docs = memory_kib();
    let materialize_ms = materialize_elapsed.as_millis();
    // Conflicts are derived from page state in production; fresh corpus has none.
    assert!(before.path_conflicts.is_empty());
    assert_eq!(
        after, before,
        "packed root documents changed canonical state"
    );
    let started = Instant::now();
    let blocks: BTreeMap<_, _> = after
        .blocks
        .iter()
        .map(|block| (block.block_id, block))
        .collect();
    let preambles: BTreeMap<_, _> = after
        .page_preambles
        .iter()
        .map(|preamble| (preamble.page_id, preamble.preamble.clone()))
        .collect();
    let mut members = BTreeMap::<_, Vec<_>>::new();
    for membership in &after.memberships {
        members
            .entry(membership.page_id)
            .or_default()
            .push(membership);
    }
    let mut projected_bytes = 0usize;
    for (page_id, page_state) in &after.pages {
        let PageState::Live {
            name,
            path,
            home_document_id,
            kind,
        } = page_state
        else {
            unreachable!("fresh corpus has only live pages")
        };
        let mut page_blocks = Vec::new();
        for member in members.remove(page_id).unwrap_or_default() {
            let block = blocks[&member.block_id];
            assert_eq!(
                block.owner,
                crate::oplog::semantic::BlockOwner::Page(*page_id)
            );
            page_blocks.push(crate::oplog::hot_engine::MaterializedBlock {
                block_id: block.block_id,
                home_document_id: block.home_document_id,
                parent: member.parent,
                order: member.order.clone(),
                logseq_uuid: block.logseq_uuid,
                logseq_identity_origin: block.logseq_identity_origin,
                content: block.content.clone(),
            });
        }
        page_blocks.sort_unstable_by(|a, b| (&a.order, a.block_id).cmp(&(&b.order, b.block_id)));
        let (mut state, expected_bytes) = projection_controls.remove(page_id).unwrap();
        let reconstructed = crate::oplog::hot_engine::MaterializedPage {
            page_id: *page_id,
            home_document_id: *home_document_id,
            name: name.clone(),
            path: path.clone(),
            kind: *kind,
            preamble: preambles.get(page_id).cloned().flatten(),
            blocks: page_blocks,
            stats: Default::default(),
        };
        state.page.stats = Default::default();
        assert_eq!(reconstructed, state.page);
        state.page = reconstructed;
        let plan = crate::oplog::projection::plan_projection(
            joiner.request.identities.workspace_id,
            &state,
            None,
        )
        .unwrap();
        assert_eq!(plan.target(), expected_bytes);
        projected_bytes += plan.target().len();
    }
    let project_ms = started.elapsed().as_millis();
    let measured_load_ms = read_ms + import_ms + materialize_ms + project_ms;
    eprintln!("retirable_loro_layout pages={} blocks={} memberships={} documents={document_count} snapshot_bytes={snapshot_bytes} packed_bytes={packed_bytes} build_ms={build_ms} pack_ms={pack_ms} physical_pack_reads=1 resident_handle_limit={cache_limit} read_ms={read_ms} import_ms={import_ms} materialize_ms={materialize_ms} project_ms={project_ms} projected_bytes={projected_bytes} measured_load_ms={measured_load_ms} rss_before_kib={rss_before:?} rss_before_load_kib={rss_before_load:?} rss_with_docs_kib={rss_with_docs:?}",
        before.pages.len(), before.blocks.len(), before.memberships.len());
    assert!(
        measured_load_ms < 10_000,
        "layout alone exhausts the full join budget"
    );
}

#[test]
fn fixed_root_text_restore_and_recent_bridge_preserve_identity() {
    let source = LoroDoc::new();
    source.set_peer_id(1).unwrap();
    source.get_text("text").insert(0, "base").unwrap();
    put(&source, "owner", &Some("page-a"));
    put(&source, "checkpoint", &0u64);
    source.commit();
    let floor = source.oplog_frontiers();
    let vv = source.oplog_vv();
    let remote = source.fork();
    remote.set_peer_id(2).unwrap();
    let identity = source.get_text("text").id();
    put(&source, "owner", &Option::<String>::None);
    source.commit();
    put(&source, "owner", &Some("page-a"));
    source.get_text("text").insert(4, " A").unwrap();
    put(&source, "checkpoint", &1u64);
    source.commit();
    let restored = LoroDoc::new();
    restored
        .import(&source.export(ExportMode::shallow_snapshot(&floor)).unwrap())
        .unwrap();
    remote.get_text("text").insert(0, "B ").unwrap();
    put(&remote, "owner", &Some("page-a"));
    remote.commit();
    let update = remote.export(ExportMode::updates(&vv)).unwrap();
    assert!(restored.import(&update).unwrap().pending.is_none());
    assert!(source.import(&update).unwrap().pending.is_none());
    assert_eq!(restored.get_deep_value(), source.get_deep_value());
    assert_eq!(restored.get_text("text").id(), identity);
    assert_eq!(restored.get_text("text").to_string(), "B base A");
}
