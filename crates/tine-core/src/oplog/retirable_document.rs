//! The single current live document layout: fixed-schema retirable Loro
//! documents.
//!
//! One logical fact lives in one document, so the unit the archive can retire
//! matches the fact's lifetime. There is no UUID-keyed catalog document and no
//! per-page map that accumulates every block that ever visited the page.
//!
//! * `Graph`     -- lineage/workspace metadata only.
//! * `Page`      -- immutable page identity, one page-state register, preamble.
//! * `Block`     -- immutable block identity plus birth provenance, one owner
//!                  register, the stable root text, Logseq identity fields.
//! * `Membership`-- addressed by the exact (block document, page document)
//!                  pair, with one optional claim register.
//!
//! Every document also carries a checkpoint register, written only by the
//! seal operation. Immutable identities are supplied by accepted birth
//! evidence; a document's self-declared `identity` register is compared
//! against that evidence and is never authority on its own.
//!
//! Snapshot and merge encoding remain upstream Loro.
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

    /// The page this document's identity is permanently bound to, if any: a
    /// page document's own page, a block document's birth page, or a pair's
    /// page. A graph document has none.
    pub(crate) const fn page_id(&self) -> Option<PageId> {
        match self {
            Self::Graph { .. } => None,
            Self::Page { page_id, .. }
            | Self::Membership { page_id, .. }
            | Self::Block {
                birth_page_id: page_id,
                ..
            } => Some(*page_id),
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
    /// Create one live document from accepted birth evidence.
    pub(crate) fn create(
        identity: DocumentIdentity,
        peer: CrdtPeerId,
        state: DocumentState,
    ) -> Result<Self, String> {
        let document = LoroDoc::new();
        document.set_peer_id(peer.as_u64()).map_err(error)?;
        create_in(&document, &identity, state)?;
        Ok(Self { identity, document })
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
        seal(&self.document, batch_id)
    }
    pub(crate) fn checkpoint(&self) -> Result<Option<BatchId>, String> {
        checkpoint(&self.document)
    }

    pub(crate) fn state(&self) -> Result<DocumentState, String> {
        read_state(&self.identity, &self.document)
    }

    pub(crate) fn set_page_state(&self, state: &PageState) -> Result<(), String> {
        set_page_state(&self.identity, &self.document, state)
    }
    pub(crate) fn set_page_preamble(&self, preamble: Option<&str>) -> Result<(), String> {
        set_page_preamble(&self.identity, &self.document, preamble)
    }
    pub(crate) fn set_owner(&self, owner: BlockOwner) -> Result<(), String> {
        set_owner(&self.identity, &self.document, owner)
    }
    pub(crate) fn replace_text(&self, content: &str) -> Result<(), String> {
        replace_text(&self.identity, &self.document, content)
    }
    pub(crate) fn set_logseq_identity(
        &self,
        uuid: Option<LogseqUuid>,
        origin: Option<LogseqIdentityOrigin>,
    ) -> Result<(), String> {
        set_logseq_identity(&self.identity, &self.document, uuid, origin)
    }
    pub(crate) fn set_membership(&self, claim: Option<&MembershipClaim>) -> Result<(), String> {
        set_membership(&self.identity, &self.document, claim)
    }
}

/// Write the fixed schema of one newly born document into an empty Loro
/// document. The caller owns peer assignment and commit batching.
pub(crate) fn create_in(
    document: &LoroDoc,
    identity: &DocumentIdentity,
    state: DocumentState,
) -> Result<(), String> {
    put(document, "schema", &SCHEMA)?;
    put(document, "identity", identity)?;
    put(document, "checkpoint", &Option::<BatchId>::None)?;
    match state {
        DocumentState::Graph if matches!(identity, DocumentIdentity::Graph { .. }) => (),
        DocumentState::Page { state, preamble } => {
            set_page_state(identity, document, &state)?;
            set_page_preamble(identity, document, preamble.as_deref())?;
        }
        DocumentState::Block(block) => {
            if !matches!(identity, DocumentIdentity::Block { document_id, block_id, .. }
                if *block_id == block.block_id && *document_id == block.home_document_id)
            {
                return Err(
                    "new block state does not match its immutable document identity".into(),
                );
            }
            set_owner(identity, document, block.owner)?;
            set_logseq_identity(
                identity,
                document,
                block.logseq_uuid,
                block.logseq_identity_origin,
            )?;
            replace_text(identity, document, &block.content)?;
        }
        DocumentState::Membership(claim) => set_membership(identity, document, claim.as_ref())?,
        DocumentState::Graph => return Err("graph state requires graph identity".into()),
    }
    document.commit();
    read_state(identity, document)?;
    Ok(())
}

/// The immutable identity a document declares about itself.
///
/// This is inspected so it can be COMPARED with accepted birth evidence. It is
/// never accepted as authority on its own: a foreign snapshot that declares an
/// identity the accepted history did not authorize is malformed.
pub(crate) fn declared_identity(document: &LoroDoc) -> Result<DocumentIdentity, String> {
    if get::<u32>(document, "schema")? != SCHEMA {
        return Err("retirable document schema differs".into());
    }
    get(document, "identity")
}

/// Whether this Loro document has ever been written as a live document.
pub(crate) fn is_initialized(document: &LoroDoc) -> bool {
    document.get_map(META).get("identity").is_some()
}

pub(crate) fn seal(document: &LoroDoc, batch_id: BatchId) -> Result<(), String> {
    put(document, "checkpoint", &Some(batch_id))
}

pub(crate) fn checkpoint(document: &LoroDoc) -> Result<Option<BatchId>, String> {
    get(document, "checkpoint")
}

/// Complete validated state of one live document, against the accepted
/// identity the caller proved independently.
pub(crate) fn read_state(
    identity: &DocumentIdentity,
    document: &LoroDoc,
) -> Result<DocumentState, String> {
    validate_shape(identity, document)?;
    if get::<u32>(document, "schema")? != SCHEMA
        || get::<DocumentIdentity>(document, "identity")? != *identity
    {
        return Err("retirable document schema or accepted birth identity differs".into());
    }
    checkpoint(document)?;
    match identity {
        DocumentIdentity::Graph { .. } => Ok(DocumentState::Graph),
        DocumentIdentity::Page { document_id, .. } => {
            let state: PageState = get(document, "state")?;
            let preamble: Option<String> = get(document, "preamble")?;
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
                owner: get(document, "owner")?,
                logseq_uuid: get(document, "logseq_uuid")?,
                logseq_identity_origin: get(document, "logseq_origin")?,
                content: document.get_text(TEXT).to_string(),
            };
            validate_logseq_identity(block.logseq_uuid, block.logseq_identity_origin)?;
            validate_content(&block.content)?;
            Ok(DocumentState::Block(block))
        }
        DocumentIdentity::Membership {
            block_document_id, ..
        } => {
            let claim: Option<MembershipClaim> = get(document, "claim")?;
            validate_claim(*block_document_id, claim.as_ref())?;
            Ok(DocumentState::Membership(claim))
        }
    }
}

/// One page document's accepted page-state register.
pub(crate) fn page_state(
    identity: &DocumentIdentity,
    document: &LoroDoc,
) -> Result<PageState, String> {
    match read_state(identity, document)? {
        DocumentState::Page { state, .. } => Ok(state),
        _ => Err("page state requires a page document".into()),
    }
}

/// One page document's optional preamble register.
pub(crate) fn page_preamble(
    identity: &DocumentIdentity,
    document: &LoroDoc,
) -> Result<Option<String>, String> {
    match read_state(identity, document)? {
        DocumentState::Page { preamble, .. } => Ok(preamble),
        _ => Err("page preamble requires a page document".into()),
    }
}

/// One block document's accepted state.
pub(crate) fn block_state(
    identity: &DocumentIdentity,
    document: &LoroDoc,
) -> Result<BlockState, String> {
    match read_state(identity, document)? {
        DocumentState::Block(state) => Ok(state),
        _ => Err("block state requires a block document".into()),
    }
}

/// One membership document's optional claim register. An absent membership is
/// a null value in this fixed register; no key is added per visit.
pub(crate) fn membership_claim(
    identity: &DocumentIdentity,
    document: &LoroDoc,
) -> Result<Option<MembershipClaim>, String> {
    match read_state(identity, document)? {
        DocumentState::Membership(claim) => Ok(claim),
        _ => Err("membership claim requires a membership document".into()),
    }
}

/// The block document's stable root text handle. Delete and Restore never
/// remove or recreate it, so its ContainerID is stable for the document's life.
pub(crate) fn root_text(document: &LoroDoc) -> loro::LoroText {
    document.get_text(TEXT)
}

fn validate_shape(identity: &DocumentIdentity, document: &LoroDoc) -> Result<(), String> {
    let LoroValue::Map(roots) = document.get_value() else {
        return Err("retirable document root directory is malformed".into());
    };
    let is_block = matches!(identity, DocumentIdentity::Block { .. });
    if !roots.contains_key(META) || roots.len() > if is_block { 2 } else { 1 } {
        return Err("retirable document has a missing or extra root".into());
    }
    for (name, value) in roots.iter() {
        let expected = if name.as_str() == META {
            document.get_map(META).id()
        } else if is_block && name.as_str() == TEXT {
            document.get_text(TEXT).id()
        } else {
            return Err("retirable document has an unknown root".into());
        };
        if value != &LoroValue::Container(expected) {
            return Err("retirable root container type differs".into());
        }
    }
    let map = document.get_map(META).get_value();
    let actual: BTreeSet<_> = map
        .as_map()
        .ok_or("retirable metadata is not a map")?
        .keys()
        .map(|name| name.as_str())
        .collect();
    let expected: BTreeSet<_> = identity.fields().iter().copied().collect();
    if actual != expected {
        return Err("retirable document does not have its fixed register schema".into());
    }
    Ok(())
}

pub(crate) fn set_page_state(
    identity: &DocumentIdentity,
    document: &LoroDoc,
    state: &PageState,
) -> Result<(), String> {
    if !matches!(identity, DocumentIdentity::Page { document_id, .. }
        if state.home_document_id() == *document_id)
    {
        return Err("page state requires its original page document".into());
    }
    put(document, "state", state)
}

pub(crate) fn set_page_preamble(
    identity: &DocumentIdentity,
    document: &LoroDoc,
    preamble: Option<&str>,
) -> Result<(), String> {
    if !matches!(identity, DocumentIdentity::Page { .. }) {
        return Err("preamble requires a page document".into());
    }
    validate_preamble(preamble)?;
    put(document, "preamble", &preamble)
}

pub(crate) fn set_owner(
    identity: &DocumentIdentity,
    document: &LoroDoc,
    owner: BlockOwner,
) -> Result<(), String> {
    if !matches!(identity, DocumentIdentity::Block { .. }) {
        return Err("owner requires a block document".into());
    }
    put(document, "owner", &owner)
}

pub(crate) fn replace_text(
    identity: &DocumentIdentity,
    document: &LoroDoc,
    content: &str,
) -> Result<(), String> {
    if !matches!(identity, DocumentIdentity::Block { .. }) {
        return Err("text requires a block document".into());
    }
    validate_content(content)?;
    document
        .get_text(TEXT)
        .update(content, UpdateOptions::default())
        .map_err(error)
}

pub(crate) fn set_logseq_identity(
    identity: &DocumentIdentity,
    document: &LoroDoc,
    uuid: Option<LogseqUuid>,
    origin: Option<LogseqIdentityOrigin>,
) -> Result<(), String> {
    if !matches!(identity, DocumentIdentity::Block { .. }) {
        return Err("Logseq identity requires a block document".into());
    }
    validate_logseq_identity(uuid, origin)?;
    put(document, "logseq_uuid", &uuid)?;
    put(document, "logseq_origin", &origin)
}

pub(crate) fn set_membership(
    identity: &DocumentIdentity,
    document: &LoroDoc,
    claim: Option<&MembershipClaim>,
) -> Result<(), String> {
    let DocumentIdentity::Membership {
        block_document_id, ..
    } = identity
    else {
        return Err("membership claim requires a membership document".into());
    };
    validate_claim(*block_document_id, claim)?;
    put(document, "claim", &claim)
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
