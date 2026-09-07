//! Fixed-schema retirable Loro documents. This defines the proposed current
//! logical units; runtime admission/cutover must still bind them to accepted
//! entity births and writer lanes. Snapshot and merge encoding remain upstream.
use loro::{ContainerTrait, ExportMode, Frontiers, LoroDoc, LoroValue, UpdateOptions};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeSet;

use super::semantic::{
    BlockOwner, BlockState, LogseqIdentityOrigin, MembershipClaim, PageState,
    MAX_BLOCK_CONTENT_BYTES, MAX_PAGE_PREAMBLE_BYTES,
};
use super::{
    BatchId, BlockId, CrdtPeerId, DocumentId, DocumentKey, LineageDigest, LogseqUuid, PageId,
    WorkspaceId,
};

const SCHEMA: u32 = 1;
const META: &str = "meta";
const TEXT: &str = "text";

/// Immutable identities must be supplied by the accepted birth/identity index
/// when opening foreign snapshots. Self-declared metadata is not authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DocumentIdentity {
    Graph {
        document_id: DocumentId,
        workspace_id: WorkspaceId,
        lineage: LineageDigest,
    },
    Page {
        document_id: DocumentId,
        page_id: PageId,
    },
    Block {
        document_id: DocumentId,
        block_id: BlockId,
        birth_page_id: PageId,
        birth_page_document_id: DocumentId,
    },
    Membership {
        block_document_id: DocumentId,
        block_id: BlockId,
        page_document_id: DocumentId,
        page_id: PageId,
    },
}

impl DocumentIdentity {
    pub(crate) fn key(&self) -> DocumentKey {
        match self {
            Self::Graph { document_id, .. }
            | Self::Page { document_id, .. }
            | Self::Block { document_id, .. } => DocumentKey::Entity(*document_id),
            Self::Membership {
                block_document_id,
                page_document_id,
                ..
            } => DocumentKey::Membership {
                block_document_id: *block_document_id,
                page_document_id: *page_document_id,
            },
        }
    }

    fn fields(&self) -> &'static [&'static str] {
        match self {
            Self::Graph { .. } => &["schema", "identity", "checkpoint"],
            Self::Page { .. } => &["schema", "identity", "checkpoint", "state", "preamble"],
            Self::Block { .. } => &[
                "schema",
                "identity",
                "checkpoint",
                "owner",
                "logseq_uuid",
                "logseq_origin",
            ],
            Self::Membership { .. } => &["schema", "identity", "checkpoint", "claim"],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DocumentState {
    Graph,
    Page {
        state: PageState,
        preamble: Option<String>,
    },
    Block(BlockState),
    Membership(Option<MembershipClaim>),
}

pub(crate) struct RetirableDocument {
    identity: DocumentIdentity,
    document: LoroDoc,
}

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}
fn put<T: Serialize>(document: &LoroDoc, field: &str, value: &T) -> Result<(), String> {
    document
        .get_map(META)
        .insert(field, serde_json::to_string(value).map_err(error)?)
        .map_err(error)
}
fn get<T: DeserializeOwned>(document: &LoroDoc, field: &str) -> Result<T, String> {
    let value = document
        .get_map(META)
        .get(field)
        .ok_or_else(|| format!("retirable document omits {field}"))?
        .get_deep_value();
    let string = value
        .as_string()
        .ok_or_else(|| format!("retirable {field} is not a register value"))?;
    serde_json::from_str(string).map_err(error)
}

impl RetirableDocument {
    pub(crate) fn create(
        identity: DocumentIdentity,
        peer: CrdtPeerId,
        state: DocumentState,
    ) -> Result<Self, String> {
        let document = LoroDoc::new();
        document.set_peer_id(peer.as_u64()).map_err(error)?;
        put(&document, "schema", &SCHEMA)?;
        put(&document, "identity", &identity)?;
        put(&document, "checkpoint", &Option::<BatchId>::None)?;
        let result = Self { identity, document };
        match state {
            DocumentState::Graph if matches!(result.identity, DocumentIdentity::Graph { .. }) => (),
            DocumentState::Page { state, preamble } => {
                result.set_page_state(&state)?;
                result.set_page_preamble(preamble.as_deref())?;
            }
            DocumentState::Block(block) => {
                if !matches!(result.identity, DocumentIdentity::Block { document_id, block_id, .. }
                    if block_id == block.block_id && document_id == block.home_document_id)
                {
                    return Err(
                        "new block state does not match its immutable document identity".into(),
                    );
                }
                result.set_owner(block.owner)?;
                result.set_logseq_identity(block.logseq_uuid, block.logseq_identity_origin)?;
                result.replace_text(&block.content)?;
            }
            DocumentState::Membership(claim) => result.set_membership(claim.as_ref())?,
            DocumentState::Graph => return Err("graph state requires graph identity".into()),
        }
        result.document.commit();
        result.state()?;
        Ok(result)
    }

    pub(crate) fn open(expected: DocumentIdentity, snapshot: &[u8]) -> Result<Self, String> {
        let document = LoroDoc::new();
        let imported = document.import(snapshot).map_err(error)?;
        if imported.pending.is_some() {
            return Err("retirable snapshot has missing dependencies".into());
        }
        let result = Self {
            identity: expected,
            document,
        };
        result.state()?;
        Ok(result)
    }

    pub(crate) fn key(&self) -> DocumentKey {
        self.identity.key()
    }
    pub(crate) fn document(&self) -> &LoroDoc {
        &self.document
    }
    pub(crate) fn snapshot(&self, floor: &Frontiers) -> Result<Vec<u8>, String> {
        self.state()?;
        self.document
            .export(ExportMode::shallow_snapshot(floor))
            .map_err(error)
    }
    pub(crate) fn seal(&self, batch_id: BatchId) -> Result<(), String> {
        put(&self.document, "checkpoint", &Some(batch_id))
    }
    pub(crate) fn checkpoint(&self) -> Result<Option<BatchId>, String> {
        get(&self.document, "checkpoint")
    }

    pub(crate) fn state(&self) -> Result<DocumentState, String> {
        self.validate_shape()?;
        if get::<u32>(&self.document, "schema")? != SCHEMA
            || get::<DocumentIdentity>(&self.document, "identity")? != self.identity
        {
            return Err("retirable document schema or accepted birth identity differs".into());
        }
        self.checkpoint()?;
        match &self.identity {
            DocumentIdentity::Graph { .. } => Ok(DocumentState::Graph),
            DocumentIdentity::Page { document_id, .. } => {
                let state: PageState = get(&self.document, "state")?;
                let preamble: Option<String> = get(&self.document, "preamble")?;
                if state.home_document_id() != *document_id {
                    return Err("page state changed its document identity".into());
                }
                validate_preamble(preamble.as_deref())?;
                Ok(DocumentState::Page { state, preamble })
            }
            DocumentIdentity::Block {
                document_id,
                block_id,
                ..
            } => {
                let block = BlockState {
                    block_id: *block_id,
                    home_document_id: *document_id,
                    owner: get(&self.document, "owner")?,
                    logseq_uuid: get(&self.document, "logseq_uuid")?,
                    logseq_identity_origin: get(&self.document, "logseq_origin")?,
                    content: self.document.get_text(TEXT).to_string(),
                };
                validate_logseq_identity(block.logseq_uuid, block.logseq_identity_origin)?;
                validate_content(&block.content)?;
                Ok(DocumentState::Block(block))
            }
            DocumentIdentity::Membership {
                block_document_id, ..
            } => {
                let claim: Option<MembershipClaim> = get(&self.document, "claim")?;
                validate_claim(*block_document_id, claim.as_ref())?;
                Ok(DocumentState::Membership(claim))
            }
        }
    }

    fn validate_shape(&self) -> Result<(), String> {
        let LoroValue::Map(roots) = self.document.get_value() else {
            return Err("retirable document root directory is malformed".into());
        };
        let is_block = matches!(self.identity, DocumentIdentity::Block { .. });
        if !roots.contains_key(META) || roots.len() > if is_block { 2 } else { 1 } {
            return Err("retirable document has a missing or extra root".into());
        }
        for (name, value) in roots.iter() {
            let expected = if name.as_str() == META {
                self.document.get_map(META).id()
            } else if is_block && name.as_str() == TEXT {
                self.document.get_text(TEXT).id()
            } else {
                return Err("retirable document has an unknown root".into());
            };
            if value != &LoroValue::Container(expected) {
                return Err("retirable root container type differs".into());
            }
        }
        let map = self.document.get_map(META).get_value();
        let actual: BTreeSet<_> = map
            .as_map()
            .ok_or("retirable metadata is not a map")?
            .keys()
            .map(|name| name.as_str())
            .collect();
        let expected: BTreeSet<_> = self.identity.fields().iter().copied().collect();
        if actual != expected {
            return Err("retirable document does not have its fixed register schema".into());
        }
        Ok(())
    }

    pub(crate) fn set_page_state(&self, state: &PageState) -> Result<(), String> {
        if !matches!(self.identity, DocumentIdentity::Page { document_id, .. }
            if state.home_document_id() == document_id)
        {
            return Err("page state requires its original page document".into());
        }
        put(&self.document, "state", state)
    }
    pub(crate) fn set_page_preamble(&self, preamble: Option<&str>) -> Result<(), String> {
        if !matches!(self.identity, DocumentIdentity::Page { .. }) {
            return Err("preamble requires a page document".into());
        }
        validate_preamble(preamble)?;
        put(&self.document, "preamble", &preamble)
    }
    pub(crate) fn set_owner(&self, owner: BlockOwner) -> Result<(), String> {
        if !matches!(self.identity, DocumentIdentity::Block { .. }) {
            return Err("owner requires a block document".into());
        }
        put(&self.document, "owner", &owner)
    }
    pub(crate) fn replace_text(&self, content: &str) -> Result<(), String> {
        if !matches!(self.identity, DocumentIdentity::Block { .. }) {
            return Err("text requires a block document".into());
        }
        validate_content(content)?;
        self.document
            .get_text(TEXT)
            .update(content, UpdateOptions::default())
            .map_err(error)
    }
    pub(crate) fn set_logseq_identity(
        &self,
        uuid: Option<LogseqUuid>,
        origin: Option<LogseqIdentityOrigin>,
    ) -> Result<(), String> {
        if !matches!(self.identity, DocumentIdentity::Block { .. }) {
            return Err("Logseq identity requires a block document".into());
        }
        validate_logseq_identity(uuid, origin)?;
        put(&self.document, "logseq_uuid", &uuid)?;
        put(&self.document, "logseq_origin", &origin)
    }
    pub(crate) fn set_membership(&self, claim: Option<&MembershipClaim>) -> Result<(), String> {
        let DocumentIdentity::Membership {
            block_document_id, ..
        } = self.identity
        else {
            return Err("membership claim requires a membership document".into());
        };
        validate_claim(block_document_id, claim)?;
        put(&self.document, "claim", &claim)
    }
}

fn validate_claim(
    block_document_id: DocumentId,
    claim: Option<&MembershipClaim>,
) -> Result<(), String> {
    if let Some(claim) = claim {
        claim.validate().map_err(error)?;
        if claim.home_document_id != block_document_id {
            return Err("membership names a different block document".into());
        }
    }
    Ok(())
}
fn validate_logseq_identity(
    uuid: Option<LogseqUuid>,
    origin: Option<LogseqIdentityOrigin>,
) -> Result<(), String> {
    if uuid.is_some() != origin.is_some() {
        return Err("Logseq UUID and origin must be present together".into());
    }
    Ok(())
}
fn validate_content(content: &str) -> Result<(), String> {
    if content.len() > MAX_BLOCK_CONTENT_BYTES {
        return Err("block content exceeds the semantic bound".into());
    }
    Ok(())
}
fn validate_preamble(preamble: Option<&str>) -> Result<(), String> {
    if preamble.is_some_and(|text| text.len() > MAX_PAGE_PREAMBLE_BYTES) {
        return Err("page preamble exceeds the semantic bound".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc_id(value: u128) -> DocumentId {
        DocumentId::from_uuid(uuid::Uuid::from_u128(value))
    }
    fn page_id(value: u128) -> PageId {
        PageId::from_uuid(uuid::Uuid::from_u128(value))
    }
    fn block_identity() -> DocumentIdentity {
        DocumentIdentity::Block {
            document_id: doc_id(101),
            block_id: BlockId::from_uuid(uuid::Uuid::from_u128(1)),
            birth_page_id: page_id(2),
            birth_page_document_id: doc_id(202),
        }
    }
    fn block(content: &str) -> RetirableDocument {
        RetirableDocument::create(
            block_identity(),
            CrdtPeerId::from_u64(1),
            DocumentState::Block(BlockState {
                block_id: BlockId::from_uuid(uuid::Uuid::from_u128(1)),
                home_document_id: doc_id(101),
                owner: BlockOwner::Page(page_id(2)),
                logseq_uuid: None,
                logseq_identity_origin: None,
                content: content.into(),
            }),
        )
        .unwrap()
    }
    fn membership_identity(page: u128, page_document: u128) -> DocumentIdentity {
        DocumentIdentity::Membership {
            block_document_id: doc_id(101),
            block_id: BlockId::from_uuid(uuid::Uuid::from_u128(1)),
            page_document_id: doc_id(page_document),
            page_id: page_id(page),
        }
    }
    fn copy(source: &RetirableDocument, peer: u64) -> RetirableDocument {
        let snapshot = source.document.export(ExportMode::Snapshot).unwrap();
        let copied = RetirableDocument::open(source.identity.clone(), &snapshot).unwrap();
        copied.document.set_peer_id(peer).unwrap();
        copied
    }
    fn exchange(left: &RetirableDocument, right: &RetirableDocument) {
        let left_snapshot = left.document.export(ExportMode::Snapshot).unwrap();
        let right_snapshot = right.document.export(ExportMode::Snapshot).unwrap();
        assert!(left
            .document
            .import(&right_snapshot)
            .unwrap()
            .pending
            .is_none());
        assert!(right
            .document
            .import(&left_snapshot)
            .unwrap()
            .pending
            .is_none());
        assert_eq!(left.state().unwrap(), right.state().unwrap());
    }

    #[test]
    fn block_retirement_and_concurrent_restore_preserve_root_text() {
        for content in ["", "retained original text"] {
            let source = block(content);
            let text_id = source.document.get_text(TEXT).id();
            source.set_owner(BlockOwner::Tombstone).unwrap();
            source
                .seal(BatchId::from_uuid(uuid::Uuid::from_u128(10)))
                .unwrap();
            source.document.commit();
            let bytes = source.snapshot(&source.document.oplog_frontiers()).unwrap();
            let reopened = RetirableDocument::open(block_identity(), &bytes).unwrap();
            assert_eq!(source.state().unwrap(), reopened.state().unwrap());
            let left = copy(&reopened, 2);
            let right = copy(&reopened, 3);
            left.set_owner(BlockOwner::Page(page_id(2))).unwrap();
            right.set_owner(BlockOwner::Page(page_id(2))).unwrap();
            exchange(&left, &right);
            assert_eq!(left.document.get_text(TEXT).id(), text_id);
            assert_eq!(left.document.get_text(TEXT).to_string(), content);
            assert_eq!(left.key(), DocumentKey::Entity(doc_id(101)));
            assert_eq!(left.checkpoint().unwrap(), source.checkpoint().unwrap());
        }
    }

    #[test]
    fn concurrent_first_membership_creation_is_one_fact_with_a_full_pair_key() {
        let identity = membership_identity(2, 202);
        let claim = MembershipClaim::new(doc_id(101), None, "a").unwrap();
        let left = RetirableDocument::create(
            identity.clone(),
            CrdtPeerId::from_u64(10),
            DocumentState::Membership(Some(claim.clone())),
        )
        .unwrap();
        let right = RetirableDocument::create(
            identity,
            CrdtPeerId::from_u64(20),
            DocumentState::Membership(Some(claim.clone())),
        )
        .unwrap();
        assert_eq!(left.key(), right.key());
        exchange(&left, &right);
        assert_eq!(
            left.state().unwrap(),
            DocumentState::Membership(Some(claim))
        );
        assert_eq!(left.document.get_map(META).len(), 4);
    }

    #[test]
    fn old_page_reorder_cannot_undo_a_move_and_later_move_back_uses_current_fact() {
        let block = block("original");
        let a = RetirableDocument::create(
            membership_identity(2, 202),
            CrdtPeerId::from_u64(1),
            DocumentState::Membership(Some(MembershipClaim::new(doc_id(101), None, "a").unwrap())),
        )
        .unwrap();
        let b = RetirableDocument::create(
            membership_identity(3, 303),
            CrdtPeerId::from_u64(2),
            DocumentState::Membership(Some(MembershipClaim::new(doc_id(101), None, "b").unwrap())),
        )
        .unwrap();
        let late_reorder = copy(&a, 3);
        block.set_owner(BlockOwner::Page(page_id(3))).unwrap();
        for step in 0..64 {
            late_reorder
                .set_membership(Some(
                    &MembershipClaim::new(doc_id(101), None, format!("z-{step}")).unwrap(),
                ))
                .unwrap();
            late_reorder.document.commit();
        }
        exchange(&a, &late_reorder);
        assert!(matches!(block.state().unwrap(), DocumentState::Block(state)
            if state.owner == BlockOwner::Page(page_id(3))));
        assert!(matches!(
            b.state().unwrap(),
            DocumentState::Membership(Some(_))
        ));
        let current_a = copy(&a, 2);
        let moved_back_claim = MembershipClaim::new(doc_id(101), None, "new-position").unwrap();
        current_a.set_membership(Some(&moved_back_claim)).unwrap();
        block.set_owner(BlockOwner::Page(page_id(2))).unwrap();
        exchange(&a, &current_a);
        assert_eq!(
            a.state().unwrap(),
            DocumentState::Membership(Some(moved_back_claim))
        );
        assert!(matches!(block.state().unwrap(), DocumentState::Block(state)
            if state.owner == BlockOwner::Page(page_id(2)) && state.content == "original"));
    }

    #[test]
    fn page_lifecycle_and_graph_metadata_roundtrip_without_a_catalog() {
        let identity = DocumentIdentity::Page {
            document_id: doc_id(202),
            page_id: page_id(2),
        };
        let live = PageState::Live {
            name: super::super::semantic::LogicalPageName::parse("Page").unwrap(),
            path: super::super::ManagedPath::parse("pages/Page.md").unwrap(),
            home_document_id: doc_id(202),
            kind: super::super::ManagedTextKind::Page,
        };
        let page = RetirableDocument::create(
            identity.clone(),
            CrdtPeerId::from_u64(1),
            DocumentState::Page {
                state: live.clone(),
                preamble: Some("title:: Page".into()),
            },
        )
        .unwrap();
        let tombstone = PageState::Tombstone {
            name: live.name().clone(),
            home_document_id: doc_id(202),
            kind: live.kind(),
        };
        page.set_page_state(&tombstone).unwrap();
        page.document.commit();
        let bytes = page.snapshot(&page.document.oplog_frontiers()).unwrap();
        let reopened = RetirableDocument::open(identity, &bytes).unwrap();
        assert_eq!(reopened.state().unwrap(), page.state().unwrap());
        reopened.set_page_state(&live).unwrap();
        assert!(matches!(
            reopened.state().unwrap(),
            DocumentState::Page {
                state: PageState::Live { .. },
                preamble: Some(_)
            }
        ));
        let graph_identity = DocumentIdentity::Graph {
            document_id: doc_id(999),
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::from_u128(99)),
            lineage: LineageDigest::from_bytes([9; 32]),
        };
        let graph = RetirableDocument::create(
            graph_identity.clone(),
            CrdtPeerId::from_u64(1),
            DocumentState::Graph,
        )
        .unwrap();
        assert_eq!(graph.document.get_map(META).len(), 3);
        let reopened = RetirableDocument::open(
            graph_identity,
            &graph.snapshot(&graph.document.oplog_frontiers()).unwrap(),
        )
        .unwrap();
        assert_eq!(reopened.state().unwrap(), DocumentState::Graph);
    }

    #[test]
    fn foreign_snapshot_must_match_accepted_identity_and_fixed_schema() {
        let source = block("content");
        let snapshot = source.document.export(ExportMode::Snapshot).unwrap();
        let mut wrong = block_identity();
        if let DocumentIdentity::Block { document_id, .. } = &mut wrong {
            *document_id = doc_id(999);
        }
        assert!(RetirableDocument::open(wrong, &snapshot).is_err());
        let before = source.document.oplog_vv();
        source.state().unwrap();
        assert_eq!(
            source.document.oplog_vv(),
            before,
            "validation must not author operations"
        );
        source
            .document
            .get_map(META)
            .insert("historical-block-key", "forbidden")
            .unwrap();
        assert!(source.state().is_err());
        let extra_root = block("content");
        extra_root
            .document
            .get_map("catalog")
            .insert("old-page", "forbidden")
            .unwrap();
        assert!(extra_root.state().is_err());
    }
}
