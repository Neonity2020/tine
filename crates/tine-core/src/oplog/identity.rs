use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use tine_storage::sealed_accepted_index::AuthenticatedMapKey;
use uuid::Uuid;

macro_rules! opaque_uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Mint an ordinary application identity. Deterministic derivation is
            /// intentionally not the default creation path.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Construct an explicitly supplied application identity.
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

opaque_uuid_id!(
    /// Opaque identity of a managed Tine workspace.
    WorkspaceId
);
opaque_uuid_id!(
    /// Opaque identity of a page, independent of its name or path.
    PageId
);
opaque_uuid_id!(
    /// Opaque identity of a block, independent of any Logseq UUID.
    BlockId
);
opaque_uuid_id!(
    /// Opaque identity of one atomic semantic operation batch.
    BatchId
);
opaque_uuid_id!(
    /// Opaque identity of an authoring device.
    DeviceId
);
opaque_uuid_id!(
    /// Opaque identity of one application session.
    SessionId
);
opaque_uuid_id!(
    /// Device-local identity of one canonical graph projection endpoint.
    ///
    /// Enrollment later binds this identity to one WorkspaceId, DeviceId, and
    /// canonical graph root. It is intentionally not derived from a portable
    /// path because receiver-local roots and formatting are not universal.
    ProjectionEndpointId
);
opaque_uuid_id!(
    /// Sharding-neutral identity of a causal document.
    DocumentId
);
opaque_uuid_id!(
    /// Opaque identity of ONE sequential authoring incarnation of one device.
    ///
    /// This is the product's `DurableBatchContract::CausalPeerKey`: every
    /// `BatchCausalDot` this device publishes names an incarnation, never the
    /// enrolled `DeviceId`. The two are deliberately independent.
    ///
    /// * The enrolled device stays on `OperationBatch::author_device_id` and
    ///   keeps every device/endpoint/enrollment authority it had.
    /// * The incarnation is allocated at random and then SAVED in the durable
    ///   device-private writer-lane record (`oplog::writer_lane`), together
    ///   with the two owned Loro peers. Ordinary, local, external-import and
    ///   seal batches of one incarnation share one sequential Tine causal
    ///   chain.
    ///
    /// Losing that record, or being unable to prove the saved incarnation's own
    /// durable prefix, therefore mints a NEW incarnation before any new
    /// authoring. The unknowable older prefix keeps its own causal identity
    /// instead of having its counters reused, which is exactly what stops a
    /// rebuild from aliasing a `BatchCausalDot` an offline peer still holds.
    /// A derived value could not do that; a random saved one structurally can.
    WriterIncarnationId
);

/// Full identity of one retirable CRDT document. Membership facts are addressed
/// by both entity document UUIDs; they are never truncated into a synthetic UUID.
/// Root Loro container identities are scoped by this key.
///
/// This is the product's `DurableBatchContract::DocumentId`: every manifest
/// descriptor, object envelope, per-document frontier, dependency record and
/// projection claim binds this full address. The inner [`DocumentId`] values
/// remain the *entity birth* identities and are never a document address on
/// their own -- an entity UUID plus a membership pair are different key
/// domains, and a `Membership` address is the exact pair, never a hash of it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentKey {
    Entity(DocumentId),
    Membership {
        block_document_id: DocumentId,
        page_document_id: DocumentId,
    },
}

impl DocumentKey {
    /// Address of one graph/page/block entity document.
    pub const fn entity(document_id: DocumentId) -> Self {
        Self::Entity(document_id)
    }

    /// Address of the membership fact for exactly this (block, page) pair.
    /// Concurrent first visits construct the identical address, so no creator
    /// is elected and no per-visit identity is minted.
    pub const fn membership(block_document_id: DocumentId, page_document_id: DocumentId) -> Self {
        Self::Membership {
            block_document_id,
            page_document_id,
        }
    }

    /// The entity birth ID, when this address names an entity document.
    pub const fn as_entity(self) -> Option<DocumentId> {
        match self {
            Self::Entity(document_id) => Some(document_id),
            Self::Membership { .. } => None,
        }
    }

    /// The block document of a membership address.
    pub const fn membership_block_document_id(self) -> Option<DocumentId> {
        match self {
            Self::Membership {
                block_document_id, ..
            } => Some(block_document_id),
            Self::Entity(_) => None,
        }
    }

    /// The page document of a membership address.
    pub const fn membership_page_document_id(self) -> Option<DocumentId> {
        match self {
            Self::Membership {
                page_document_id, ..
            } => Some(page_document_id),
            Self::Entity(_) => None,
        }
    }

    pub const fn is_membership(self) -> bool {
        matches!(self, Self::Membership { .. })
    }

    /// The one lossless authenticated-map key of this document address.
    ///
    /// Every shared authenticated map -- the sealed on-disk document map, the
    /// SQLite frontier treap and the run-local accepted map -- is keyed by
    /// exactly these bytes, so all three compose the same roots for the same
    /// rows. The encoding is:
    ///
    /// * entity: `0x01 || document UUID` (17 bytes)
    /// * membership: `0x02 || block document UUID || page document UUID`
    ///   (33 bytes)
    ///
    /// The tag makes the two domains unambiguous, both fit well inside
    /// [`tine_storage::formats::MAX_AUTHENTICATED_MAP_KEY_BYTES`], and nothing
    /// hashes or truncates a pair into a synthetic UUID.
    pub fn authenticated_map_key(self) -> AuthenticatedMapKey {
        let mut bytes = [0_u8; 1 + 16 + 16];
        let length = match self {
            Self::Entity(document_id) => {
                bytes[0] = DOCUMENT_KEY_ENTITY_KEY_TAG;
                bytes[1..17].copy_from_slice(document_id.as_uuid().as_bytes());
                17
            }
            Self::Membership {
                block_document_id,
                page_document_id,
            } => {
                bytes[0] = DOCUMENT_KEY_MEMBERSHIP_KEY_TAG;
                bytes[1..17].copy_from_slice(block_document_id.as_uuid().as_bytes());
                bytes[17..33].copy_from_slice(page_document_id.as_uuid().as_bytes());
                33
            }
        };
        AuthenticatedMapKey::new(&bytes[..length])
            .expect("a document key is 17 or 33 bytes, inside the shared key bound")
    }

    /// Recover the exact address one authenticated-map key names.
    ///
    /// Readers compare the FULL decoded address with the one they asked for,
    /// so a row can never answer for a different document.
    pub fn from_authenticated_map_key(key: AuthenticatedMapKey) -> Option<Self> {
        let bytes = key.as_slice();
        match (bytes.first().copied()?, bytes.len()) {
            (DOCUMENT_KEY_ENTITY_KEY_TAG, 17) => Some(Self::Entity(DocumentId::from_uuid(
                Uuid::from_slice(&bytes[1..17]).ok()?,
            ))),
            (DOCUMENT_KEY_MEMBERSHIP_KEY_TAG, 33) => Some(Self::Membership {
                block_document_id: DocumentId::from_uuid(Uuid::from_slice(&bytes[1..17]).ok()?),
                page_document_id: DocumentId::from_uuid(Uuid::from_slice(&bytes[17..33]).ok()?),
            }),
            _ => None,
        }
    }
}

/// Key-domain tags. Distinct from the display tags so neither encoding can
/// drift into the other.
const DOCUMENT_KEY_ENTITY_KEY_TAG: u8 = 0x01;
const DOCUMENT_KEY_MEMBERSHIP_KEY_TAG: u8 = 0x02;

const DOCUMENT_KEY_ENTITY_TAG: &str = "entity";
const DOCUMENT_KEY_MEMBERSHIP_TAG: &str = "membership";

/// Precise, reversible rendering of a full document address.
///
/// Tags are explicit so an entity UUID can never be read as a membership pair,
/// and the pair is written out in full so nothing truncates or hashes it.
impl fmt::Display for DocumentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entity(document_id) => {
                write!(f, "{DOCUMENT_KEY_ENTITY_TAG}:{document_id}")
            }
            Self::Membership {
                block_document_id,
                page_document_id,
            } => write!(
                f,
                "{DOCUMENT_KEY_MEMBERSHIP_TAG}:{block_document_id}:{page_document_id}"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentKeyParseError;

impl fmt::Display for DocumentKeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected entity:<uuid> or membership:<block-uuid>:<page-uuid>")
    }
}

impl std::error::Error for DocumentKeyParseError {}

impl FromStr for DocumentKey {
    type Err = DocumentKeyParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = value.strip_prefix(concat!("entity", ":")) {
            return DocumentId::from_str(rest)
                .map(Self::Entity)
                .map_err(|_| DocumentKeyParseError);
        }
        let rest = value
            .strip_prefix(concat!("membership", ":"))
            .ok_or(DocumentKeyParseError)?;
        let (block, page) = rest.split_once(':').ok_or(DocumentKeyParseError)?;
        Ok(Self::Membership {
            block_document_id: DocumentId::from_str(block).map_err(|_| DocumentKeyParseError)?,
            page_document_id: DocumentId::from_str(page).map_err(|_| DocumentKeyParseError)?,
        })
    }
}

impl WriterIncarnationId {
    /// Derive one FIXTURE writer incarnation from a device identity.
    ///
    /// Engine/projection/wire fixtures hold no device-private application
    /// runtime root, so they have nowhere to save a real incarnation. They get
    /// an EXPLICIT stable fixture identity instead, which keeps one fixture's
    /// causal chain stable across its own reopens and keeps two fixture devices
    /// distinct. Production never reaches this: it always reads the saved
    /// random incarnation out of the durable writer-lane record, and a
    /// production fallback that hashed `DeviceId` would reintroduce exactly the
    /// aliasing this type exists to prevent.
    #[cfg(test)]
    pub(crate) fn fixture_for_device(device_id: DeviceId) -> Self {
        Self::from_uuid(derived_uuid(
            b"tine/writer-lane/fixture-incarnation/v1\0",
            &[device_id.as_uuid().as_bytes()],
        ))
    }

    /// Derive one FIXTURE writer incarnation from an arbitrary explicit label.
    /// Same rules as [`Self::fixture_for_device`]: fixtures only.
    #[cfg(test)]
    pub(crate) fn fixture_labelled(label: &[u8]) -> Self {
        Self::from_uuid(derived_uuid(
            b"tine/writer-lane/fixture-incarnation-label/v1\0",
            &[label],
        ))
    }
}

/// Opaque, engine-neutral identity of a CRDT peer within a causal document.
///
/// The numeric representation is an interchange value, not a Loro peer type.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CrdtPeerId(u64);

impl CrdtPeerId {
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Derive one FIXTURE identity for a persistent CRDT writer lane.
    ///
    /// Production lanes are allocated at random and then saved in the durable
    /// device-private writer-lane record (`oplog::writer_lane`), precisely so
    /// that losing that record cannot recreate an identity whose published
    /// prefix nobody can qualify. A derived value is therefore deliberately NOT
    /// the production allocator and never an authority — this exists only for
    /// engine/projection fixtures, which hold no device-private runtime root
    /// and need lanes that stay stable across one fixture's own reopens.
    /// Receiver-side authority is, in every case, the accepted lane-ownership
    /// binding the engine builds from admitted batches.
    #[cfg(test)]
    pub(crate) fn writer_lane_candidate(
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        endpoint_id: ProjectionEndpointId,
        role_tag: u8,
        incarnation: u64,
        attempt: u64,
    ) -> Self {
        Self(derived_u64(
            b"tine/writer-lane/crdt-peer-id/v1\0",
            &[
                workspace_id.as_uuid().as_bytes(),
                device_id.as_uuid().as_bytes(),
                endpoint_id.as_uuid().as_bytes(),
                &[role_tag],
                &incarnation.to_be_bytes(),
                &attempt.to_be_bytes(),
            ],
        ))
    }
}

impl fmt::Display for CrdtPeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A syntactically valid Logseq UUID, kept distinct from Tine's internal BlockId.
///
/// Parsing accepts UUID syntax understood by the UUID library. Serialization is
/// always the canonical lower-case hyphenated representation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LogseqUuid(Uuid);

impl LogseqUuid {
    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }

    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for LogseqUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

impl FromStr for LogseqUuid {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for LogseqUuid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for LogseqUuid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Deterministic identity of one external reconciliation transaction.
///
/// ImportId is a full SHA-256 digest so the inventory identity is not confused
/// with an ordinary randomly minted UUID.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ImportId([u8; 32]);

impl ImportId {
    pub(crate) const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Stable identity of one canonical graph-root filesystem resource.
///
/// The digest is derived only from a retained no-follow directory capability:
/// device/inode on Unix (including Android) and volume/file ID on Windows.
/// Ambient path strings never enter this identity, so moving or renaming the
/// graph preserves enrollment while substituting another directory does not.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalGraphResourceId([u8; 32]);

impl CanonicalGraphResourceId {
    pub(crate) fn from_capability_identity(platform: &[u8], identity: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"tine/canonical-graph-resource/v1\0");
        hasher.update((platform.len() as u64).to_be_bytes());
        hasher.update(platform);
        hasher.update((identity.len() as u64).to_be_bytes());
        hasher.update(identity);
        Self(hasher.finalize().into())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Reconstruct a previously authenticated canonical graph-resource
    /// identity from its fixed digest representation.  Parsing callers still
    /// own the authority decision; this constructor performs no filesystem I/O.
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for CanonicalGraphResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CanonicalGraphResourceId({self})")
    }
}

impl fmt::Display for CanonicalGraphResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl FromStr for CanonicalGraphResourceId {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value).map(Self)
    }
}

impl Serialize for CanonicalGraphResourceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CanonicalGraphResourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Stable identity of one claimed canonical device-local archive directory.
///
/// This remains part of the semantic enrollment records after retirement of
/// the unused archive-claim codec and filesystem protocol.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalArchiveResourceId([u8; 32]);

impl CanonicalArchiveResourceId {
    #[cfg(test)]
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for CanonicalArchiveResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CanonicalArchiveResourceId({self})")
    }
}

impl fmt::Display for CanonicalArchiveResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl FromStr for CanonicalArchiveResourceId {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value).map(Self)
    }
}

impl Serialize for CanonicalArchiveResourceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CanonicalArchiveResourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Stable identity of one projection-receipt directory capability.
///
/// Like the graph resource identity, this is derived from the opened directory
/// resource rather than its ambient pathname. It is also durably recorded in
/// the receipt-store claim, so another directory cannot copy an endpoint tuple
/// and become the engine's enrolled receipt authority.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectionReceiptStoreId([u8; 32]);

impl ProjectionReceiptStoreId {
    pub(crate) fn from_capability_identity(platform: &[u8], identity: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"tine/projection-receipt-store-resource/v1\0");
        hasher.update((platform.len() as u64).to_be_bytes());
        hasher.update(platform);
        hasher.update((identity.len() as u64).to_be_bytes());
        hasher.update(identity);
        Self(hasher.finalize().into())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ProjectionReceiptStoreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProjectionReceiptStoreId({self})")
    }
}

impl fmt::Display for ProjectionReceiptStoreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl FromStr for ProjectionReceiptStoreId {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value).map(Self)
    }
}

impl Serialize for ProjectionReceiptStoreId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ProjectionReceiptStoreId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Debug for ImportId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ImportId({self})")
    }
}

impl fmt::Display for ImportId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl FromStr for ImportId {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value).map(Self)
    }
}

impl Serialize for ImportId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ImportId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl BatchId {
    /// Derive the sole batch identity for a deterministic external import.
    pub fn for_import(import_id: ImportId) -> Self {
        Self(derived_uuid(
            b"tine/import/batch-id/v1\0",
            &[import_id.as_bytes()],
        ))
    }
}

impl DocumentId {
    /// Derive the home document for one unmatched external page from its
    /// workspace and exact managed relative path.
    pub(crate) fn for_unmatched_import_page(
        workspace_id: WorkspaceId,
        managed_relative_path: &[u8],
    ) -> Self {
        Self(derived_uuid(
            b"tine/import/unmatched-page-home-document-id/v1\0",
            &[workspace_id.as_uuid().as_bytes(), managed_relative_path],
        ))
    }

    /// The one authenticated-map key of a live catalog or page-shard document:
    /// exactly its 16 UUID bytes. The run-local accepted map, the SQLite
    /// frontier treap and SQLite point lowering all key rows by these bytes,
    /// so they compose the same roots for the same rows. The retained
    /// retirable-document codec keys its inert rosters by [`DocumentKey`]
    /// instead, and wraps a `DocumentId` only at that codec boundary.
    pub fn authenticated_map_key(self) -> AuthenticatedMapKey {
        AuthenticatedMapKey::from(self.0.into_bytes())
    }

    /// Recover the exact document one live authenticated-map key names.
    /// A key that is not exactly 128 bits wide names no live document; it is
    /// refused rather than truncated.
    pub fn from_authenticated_map_key(key: AuthenticatedMapKey) -> Option<Self> {
        <[u8; 16]>::try_from(key.as_slice())
            .ok()
            .map(|bytes| Self(Uuid::from_bytes(bytes)))
    }

    /// A path released by an accepted deletion cannot reuse the path-stable
    /// home document of its prior page. Bind the replacement shard to the
    /// authenticated release dependency so replicas derive the same fresh
    /// document without colliding with the deleted owner's shard.
    pub(crate) fn for_released_import_page(
        workspace_id: WorkspaceId,
        managed_relative_path: &[u8],
        release: super::LogicalCompletionId,
    ) -> Self {
        Self(derived_uuid(
            b"tine/import/released-page-home-document-id/v1\0",
            &[
                workspace_id.as_uuid().as_bytes(),
                managed_relative_path,
                release.as_bytes(),
            ],
        ))
    }

    /// Derive the external-observation document for one import transaction.
    pub(crate) fn for_external_import_observation(
        workspace_id: WorkspaceId,
        import_id: ImportId,
    ) -> Self {
        Self(derived_uuid(
            b"tine/import/external-observation-document-id/v1\0",
            &[workspace_id.as_uuid().as_bytes(), import_id.as_bytes()],
        ))
    }
}

impl SessionId {
    /// Derive the synthetic external author session for one import transaction.
    pub(crate) fn for_external_import_author(
        workspace_id: WorkspaceId,
        import_id: ImportId,
    ) -> Self {
        Self(derived_uuid(
            b"tine/import/external-author-session-id/v1\0",
            &[workspace_id.as_uuid().as_bytes(), import_id.as_bytes()],
        ))
    }
}

impl PageId {
    /// Derive the identity of an unmatched imported page.
    pub fn for_unmatched_import(import_id: ImportId, locator: &[u8]) -> Self {
        Self(derived_uuid(
            b"tine/import/unmatched-page-id/v1\0",
            &[import_id.as_bytes(), locator],
        ))
    }
}

impl BlockId {
    /// Derive the identity of an unmatched imported block.
    pub fn for_unmatched_import(import_id: ImportId, locator: &[u8]) -> Self {
        Self(derived_uuid(
            b"tine/import/unmatched-block-id/v1\0",
            &[import_id.as_bytes(), locator],
        ))
    }

    /// Derive the sibling created to preserve the second authored text of one
    /// concurrent edit pair. Stable identity lets a later causal descendant
    /// retire that machine-created sibling when both branches converge.
    pub(crate) fn for_conflict_sibling(
        original: BlockId,
        min_batch: BatchId,
        max_batch: BatchId,
    ) -> Self {
        debug_assert!(min_batch < max_batch);
        Self(derived_uuid(
            b"tine/conflict-resolution/sibling-block-id/v1\0",
            &[
                original.as_uuid().as_bytes(),
                min_batch.as_uuid().as_bytes(),
                max_batch.as_uuid().as_bytes(),
            ],
        ))
    }
}

fn derived_uuid(domain: &[u8], parts: &[&[u8]]) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Mark deterministic application IDs as RFC 9562 UUIDv8 values.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn derived_u64(domain: &[u8], parts: &[&[u8]]) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DigestParseError;

impl fmt::Display for DigestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected exactly 64 lower-case hexadecimal characters")
    }
}

impl std::error::Error for DigestParseError {}

pub(crate) fn parse_digest(value: &str) -> Result<[u8; 32], DigestParseError> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(DigestParseError);
    }
    let mut result = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        result[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(result)
}

fn hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => unreachable!("validated hexadecimal nibble"),
    }
}

pub(crate) fn write_hex(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        f.write_str(
            std::str::from_utf8(&[HEX[(byte >> 4) as usize], HEX[(byte & 0x0f) as usize]])
                .expect("hexadecimal is UTF-8"),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> WorkspaceId {
        WorkspaceId::from_uuid(Uuid::from_u128(0x1020_3040_5060_7080_90a0_b0c0_d0e0_f001))
    }

    fn import_id() -> ImportId {
        ImportId::from_digest([0x5a; 32])
    }

    #[test]
    fn publisher_p1_external_import_derivations_are_deterministic_and_domain_separated() {
        let workspace = workspace();
        let import = import_id();
        let home = DocumentId::for_unmatched_import_page(workspace, b"pages/nested/naive.md");
        let session = SessionId::for_external_import_author(workspace, import);
        let observation = DocumentId::for_external_import_observation(workspace, import);

        assert_eq!(
            home,
            DocumentId::for_unmatched_import_page(workspace, b"pages/nested/naive.md")
        );
        assert_eq!(
            session,
            SessionId::for_external_import_author(workspace, import)
        );
        assert_eq!(
            observation,
            DocumentId::for_external_import_observation(workspace, import)
        );

        let rendered = [
            home.to_string(),
            session.to_string(),
            observation.to_string(),
        ];
        for (left_index, left) in rendered.iter().enumerate() {
            for right in rendered.iter().skip(left_index + 1) {
                assert_ne!(left, right, "derivation domains must remain separate");
            }
        }

        assert_eq!(home.to_string(), "737b3bff-157d-8cfe-a3e8-be0ca069e2d6");
        assert_eq!(session.to_string(), "5e69f6b5-0b83-8916-904c-36f09da566e1");
        assert_eq!(
            observation.to_string(),
            "54588e2e-938c-8f75-bc5c-f9ddbcf4ddb7"
        );
    }

    /// The candidate is a per-(workspace, device, endpoint, role, incarnation)
    /// value, not a per-batch or per-import one: that is precisely what bounds
    /// P to writer incarnations instead of history. `attempt` only feeds the
    /// bounded collision probe, and `incarnation` is what a coverage-losing
    /// rebuild advances.
    #[test]
    fn writer_lane_candidates_separate_role_incarnation_and_endpoint() {
        let workspace = workspace();
        let device = DeviceId::from_uuid(Uuid::from_u128(0x11));
        let endpoint = ProjectionEndpointId::from_uuid(Uuid::from_u128(0x22));
        let other_endpoint = ProjectionEndpointId::from_uuid(Uuid::from_u128(0x23));
        let base = CrdtPeerId::writer_lane_candidate(workspace, device, endpoint, 0, 0, 0);

        assert_eq!(
            base,
            CrdtPeerId::writer_lane_candidate(workspace, device, endpoint, 0, 0, 0)
        );
        for divergent in [
            CrdtPeerId::writer_lane_candidate(workspace, device, endpoint, 1, 0, 0),
            CrdtPeerId::writer_lane_candidate(workspace, device, endpoint, 0, 1, 0),
            CrdtPeerId::writer_lane_candidate(workspace, device, endpoint, 0, 0, 1),
            CrdtPeerId::writer_lane_candidate(workspace, device, other_endpoint, 0, 0, 0),
            CrdtPeerId::writer_lane_candidate(
                workspace,
                DeviceId::from_uuid(Uuid::from_u128(0x12)),
                endpoint,
                0,
                0,
                0,
            ),
        ] {
            assert_ne!(base, divergent);
        }
    }

    #[test]
    fn canonical_archive_resource_identity_is_persistable() {
        let archive = CanonicalArchiveResourceId::from_bytes([0xaa; 32]);
        let graph = CanonicalGraphResourceId::from_capability_identity(
            b"test-platform",
            b"same-retained-directory-identity",
        );
        let receipt = ProjectionReceiptStoreId::from_capability_identity(
            b"test-platform",
            b"same-retained-directory-identity",
        );

        assert_ne!(archive.as_bytes(), graph.as_bytes());
        assert_ne!(archive.as_bytes(), receipt.as_bytes());
        assert_eq!(
            archive
                .to_string()
                .parse::<CanonicalArchiveResourceId>()
                .unwrap(),
            archive
        );
        assert_eq!(
            serde_json::from_slice::<CanonicalArchiveResourceId>(
                &serde_json::to_vec(&archive).unwrap()
            )
            .unwrap(),
            archive
        );
    }
}
