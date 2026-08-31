//! Canonical, inactive evidence for a deterministic multipart bootstrap import.
//!
//! This module deliberately has no object-store, engine, graph-writer, or
//! authority token dependency.  Its only job is to make the v1 bytes and their
//! validation rules unambiguous before a later packet connects them to I/O.
//!
//! Zero source files mean the canonical empty inventory and blob roots and
//! zero pages in both indexes. Zero parts mean the initial frontier is also
//! final, with accepted count zero and no last part; the aggregate commit still
//! exists and binds that generation-zero frontier. A non-empty index always
//! has at least one page and exactly one terminal page.

use std::cmp::Ordering;
use std::fmt;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::identity::BootstrapPartId;
use super::{
    BatchId, CanonicalGraphResourceId, ContentDigest, ImportId, LineageDigest, ManagedPath,
    ManagedTextKind, WorkspaceId,
};

pub(crate) const BOOTSTRAP_IMPORT_SCHEMA_VERSION: u32 = 1;
pub(crate) const BOOTSTRAP_PARTITION_POLICY_VERSION: u32 = 1;
pub(crate) const SOURCE_LEAF_SCHEMA_VERSION: u32 = 1;
pub(crate) const SOURCE_BLOB_SCHEMA_VERSION: u32 = 1;
pub(crate) const SOURCE_SPAN_SCHEMA_VERSION: u32 = 1;
pub(crate) const OPERATION_ROOT_SCHEMA_VERSION: u32 = 1;
pub(crate) const PAYLOAD_OBJECT_ROOT_SCHEMA_VERSION: u32 = 1;
pub(crate) const FULL_OBJECT_ROOT_SCHEMA_VERSION: u32 = 1;
pub(crate) const BOOTSTRAP_FRONTIER_SCHEMA_VERSION: u32 = 1;
pub(crate) const SOURCE_INVENTORY_INDEX_SCHEMA_VERSION: u32 = 1;
pub(crate) const SOURCE_BLOB_INDEX_SCHEMA_VERSION: u32 = 1;
pub(crate) const PART_SPAN_INDEX_SCHEMA_VERSION: u32 = 1;
pub(crate) const BOOTSTRAP_AGGREGATE_COMMIT_SCHEMA_VERSION: u32 = 1;

/// Bootstrap parts are already bounded by semantic bytes, prepared object
/// bytes, encoded manifest bytes, and the ordinary transaction ceiling. Keep
/// this count ceiling aligned with that transaction boundary instead of
/// splitting otherwise-small imports at a second, tighter arbitrary limit.
pub(crate) const MAX_OPERATIONS_PER_BOOTSTRAP_PART: u32 = 100_000;
pub(crate) const MAX_SOURCE_SPANS_PER_BOOTSTRAP_PART: u32 = 100_000;
pub(crate) const MAX_SEMANTIC_EFFECT_BYTES_PER_BOOTSTRAP_PART: u64 = 48 * 1024 * 1024;
pub(crate) const MAX_BATCH_OBJECT_BYTES_PER_BOOTSTRAP_PART: u64 = 192 * 1024 * 1024;
pub(crate) const MAX_BOOTSTRAP_PART_EVIDENCE_BYTES: usize = 768 * 1024;
pub(crate) const MAX_BOOTSTRAP_AGGREGATE_MANIFEST_BYTES: usize = 768 * 1024;
pub(crate) const MAX_SOURCE_BLOB_CHUNK_BYTES: u32 = 1024 * 1024;
pub(crate) const MAX_SOURCE_INDEX_PAGE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SOURCE_INDEX_PAGES: u32 = 4096;
pub(crate) const MAX_PART_SPAN_INDEX_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_BOOTSTRAP_AGGREGATE_COMMIT_BYTES: usize = 1024;
pub(crate) const MAX_SOURCE_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_TOTAL_SOURCE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub(crate) const MAX_PARSED_NODES_PER_SOURCE_FILE: u32 = 1_000_000;

/// Fixed v1 caps that bound allocation in the pure schema.  They are not
/// partition policy knobs; any future change needs a new profile version.
pub(crate) const MAX_BOOTSTRAP_PARTS: u32 = 1024;
pub(crate) const MAX_SOURCE_INVENTORY_LEAVES: u32 = 1_000_000;
pub(crate) const MAX_SOURCE_BLOB_CHUNKS: u32 = 1_000_000;
pub(crate) const MAX_SOURCE_LOCATOR_BYTES: usize = 16 * 1024;
pub(crate) const MAX_CANONICAL_NESTING_DEPTH: u8 = 4;
/// Complete descriptor sets include payload objects, the evidence object, and
/// manifest-defined non-payload objects.  This is deliberately a separately
/// profiled bound from the payload-object cap.
pub(crate) const MAX_FULL_OBJECTS_PER_BOOTSTRAP_PART: u32 = 8_192;
pub(crate) const MAX_FULL_OBJECT_BYTES_PER_BOOTSTRAP_PART: u64 =
    MAX_BATCH_OBJECT_BYTES_PER_BOOTSTRAP_PART + 2 * 768 * 1024;
/// A bootstrap batch owns one semantic-effect object plus one CRDT object for
/// the catalog and each page document. A JSON descriptor occupies at most 192
/// bytes under the durable object bounds; reserve 64 KiB for every fixed field,
/// causal head, and encoding delimiter. The part ceiling is the tighter of
/// that manifest budget and the independent descriptor-count bound.
const MAX_BOOTSTRAP_MANIFEST_DESCRIPTOR_BYTES: usize = 192;
const BOOTSTRAP_MANIFEST_FIXED_HEADROOM_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BOUND_PAGE_DOCUMENTS: u32 =
    ((MAX_PREPARED_MANIFEST_BYTES_PER_BOOTSTRAP_PART - BOOTSTRAP_MANIFEST_FIXED_HEADROOM_BYTES)
        / MAX_BOOTSTRAP_MANIFEST_DESCRIPTOR_BYTES) as u32
        - 2;
pub(crate) const MAX_PAGE_DOCUMENTS_PER_BOOTSTRAP_PART: u32 =
    if MAX_MANIFEST_BOUND_PAGE_DOCUMENTS < MAX_FULL_OBJECTS_PER_BOOTSTRAP_PART - 2 {
        MAX_MANIFEST_BOUND_PAGE_DOCUMENTS
    } else {
        MAX_FULL_OBJECTS_PER_BOOTSTRAP_PART - 2
    };
/// Bootstrap parts are ordinary durable batches. Do not impose a second,
/// tighter manifest policy here: the durable batch codec is the authority for
/// both allocation bounds and accepted bytes.
pub(crate) const MAX_PREPARED_MANIFEST_BYTES_PER_BOOTSTRAP_PART: usize = super::MAX_MANIFEST_BYTES;

const EVIDENCE_MAGIC: &[u8; 8] = b"TINBPE1\0";
const MANIFEST_MAGIC: &[u8; 8] = b"TINBAM1\0";
const INVENTORY_PAGE_MAGIC: &[u8; 8] = b"TINBII1\0";
const BLOB_PAGE_MAGIC: &[u8; 8] = b"TINBBI1\0";
const PART_SPAN_MAGIC: &[u8; 8] = b"TINBSI1\0";
const COMMIT_MAGIC: &[u8; 8] = b"TINBAC1\0";

macro_rules! digest_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub(crate) struct $name([u8; 32]);

        impl $name {
            pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            fn digest(domain: &[u8], fields: &[&[u8]]) -> Self {
                Self(canonical_digest(domain, fields))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), hex(&self.0))
            }
        }
    };
}

digest_type!(BootstrapProfileDigestV1);
digest_type!(SourceLeafDigestV1);
digest_type!(SourceContentDigestV1);
digest_type!(SourceBlobChunkDigestV1);
digest_type!(SourceBlobChunkDescriptorDigestV1);
digest_type!(SourceSpanDigestV1);
digest_type!(OperationDigestV1);
digest_type!(BootstrapEvidenceDigestV1);
digest_type!(BootstrapManifestFingerprintV1);
digest_type!(BootstrapAcceptedEventBindingV1);
digest_type!(BootstrapAggregateDigestV1);
digest_type!(BootstrapFinalFrontierProofV1);
digest_type!(BootstrapPublicationIdV1);

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceInventoryRootV1 {
    digest: [u8; 32],
    source_count: u32,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceBlobChunkRootV1 {
    digest: [u8; 32],
    chunk_count: u32,
    total_bytes: u64,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceSpanRootV1 {
    digest: [u8; 32],
    span_count: u32,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OperationRootV1 {
    digest: [u8; 32],
    operation_count: u32,
    semantic_effect_bytes: u64,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct PayloadObjectRootV1 {
    digest: [u8; 32],
    object_count: u32,
    total_bytes: u64,
}

/// The complete object root is intentionally distinct from the payload root.
/// Its domain covers the canonically ordered complete descriptor set: every
/// payload object, the encoded part-evidence object, then every
/// manifest-defined non-payload object.  Evidence never carries this root, so
/// including the evidence object here cannot form a self-hash cycle.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FullObjectRootV1 {
    digest: [u8; 32],
    object_count: u32,
    total_bytes: u64,
}

macro_rules! root_debug {
    ($name:ident, $count:ident) => {
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("digest", &hex(&self.digest))
                    .field(stringify!($count), &self.$count)
                    .finish()
            }
        }
    };
}

root_debug!(SourceInventoryRootV1, source_count);
root_debug!(SourceSpanRootV1, span_count);

impl fmt::Debug for SourceBlobChunkRootV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceBlobChunkRootV1")
            .field("digest", &hex(&self.digest))
            .field("chunk_count", &self.chunk_count)
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

impl fmt::Debug for OperationRootV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationRootV1")
            .field("digest", &hex(&self.digest))
            .field("operation_count", &self.operation_count)
            .field("semantic_effect_bytes", &self.semantic_effect_bytes)
            .finish()
    }
}

impl fmt::Debug for PayloadObjectRootV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayloadObjectRootV1")
            .field("digest", &hex(&self.digest))
            .field("object_count", &self.object_count)
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

impl fmt::Debug for FullObjectRootV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FullObjectRootV1")
            .field("digest", &hex(&self.digest))
            .field("object_count", &self.object_count)
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

impl SourceInventoryRootV1 {
    pub(crate) fn empty() -> Self {
        SourceInventoryRootBuilderV1::new().finish()
    }

    pub(crate) fn from_leaves(leaves: &[SourceLeafV1]) -> Result<Self, BootstrapImportError> {
        checked_count(
            leaves.len(),
            MAX_SOURCE_INVENTORY_LEAVES,
            "source inventory leaves",
        )?;
        let mut ordered = leaves.iter().collect::<Vec<_>>();
        ordered.sort_unstable_by(|left, right| left.canonical_cmp(right));
        let mut builder = SourceInventoryRootBuilderV1::new();
        for leaf in ordered {
            builder.push(leaf)?;
        }
        Ok(builder.finish())
    }

    pub(crate) const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) const fn source_count(self) -> u32 {
        self.source_count
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        root_bytes(self)
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        if self.source_count > MAX_SOURCE_INVENTORY_LEAVES {
            return Err(BootstrapImportError::CountLimit("source inventory leaves"));
        }
        Ok(())
    }

    fn encode_into(self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.digest);
        bytes.extend_from_slice(&self.source_count.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() != 36 {
            return Err(BootstrapImportError::InvalidFieldLength);
        }
        let value = Self {
            digest: array_32(&bytes[..32])?,
            source_count: read_u32(&bytes[32..])?,
        };
        value.validate()?;
        Ok(value)
    }
}

impl SourceBlobChunkRootV1 {
    /// The empty root is the blob commitment for an empty source inventory.
    /// Inventories containing only zero-length sources produce the same
    /// descriptor root, but remain unambiguous through their separate source
    /// inventory commitment.
    pub(crate) fn empty() -> Self {
        SourceBlobChunkRootBuilderV1::new()
            .finish()
            .expect("empty blob root is continuous")
    }

    pub(crate) fn from_descriptors(
        source_leaves: &[SourceLeafV1],
        descriptors: &[SourceBlobChunkDescriptorV1],
    ) -> Result<Self, BootstrapImportError> {
        checked_count(
            descriptors.len(),
            MAX_SOURCE_BLOB_CHUNKS,
            "source blob chunks",
        )?;
        let mut ordered_sources = source_leaves.iter().collect::<Vec<_>>();
        ordered_sources.sort_unstable_by_key(|source| source.digest());
        let mut ordered_descriptors = descriptors.iter().copied().collect::<Vec<_>>();
        ordered_descriptors.sort_unstable();
        let mut descriptor_index = 0;
        let mut builder = SourceBlobChunkRootBuilderV1::new();
        for source in ordered_sources {
            builder.begin_source(source)?;
            while ordered_descriptors
                .get(descriptor_index)
                .is_some_and(|descriptor| descriptor.source_leaf == source.digest())
            {
                builder.push(ordered_descriptors[descriptor_index])?;
                descriptor_index += 1;
            }
        }
        if descriptor_index != ordered_descriptors.len() {
            return Err(BootstrapImportError::BlobContinuityMismatch);
        }
        builder.finish()
    }

    pub(crate) const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) const fn chunk_count(self) -> u32 {
        self.chunk_count
    }

    pub(crate) const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        root_bytes(self)
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        if self.chunk_count > MAX_SOURCE_BLOB_CHUNKS {
            return Err(BootstrapImportError::CountLimit("source blob chunks"));
        }
        if self.total_bytes > MAX_TOTAL_SOURCE_BYTES {
            return Err(BootstrapImportError::ByteLimit("total source bytes"));
        }
        Ok(())
    }

    fn encode_into(self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.digest);
        bytes.extend_from_slice(&self.chunk_count.to_be_bytes());
        bytes.extend_from_slice(&self.total_bytes.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() != 44 {
            return Err(BootstrapImportError::InvalidFieldLength);
        }
        let value = Self {
            digest: array_32(&bytes[..32])?,
            chunk_count: read_u32(&bytes[32..36])?,
            total_bytes: read_u64(&bytes[36..])?,
        };
        value.validate()?;
        Ok(value)
    }
}

impl SourceSpanRootV1 {
    pub(crate) fn empty() -> Self {
        SourceSpanRootBuilderV1::new().finish()
    }

    pub(crate) fn from_spans(spans: &[SourceSpanV1]) -> Result<Self, BootstrapImportError> {
        checked_count(
            spans.len(),
            MAX_SOURCE_SPANS_PER_BOOTSTRAP_PART,
            "source spans",
        )?;
        let mut ordered = spans.iter().collect::<Vec<_>>();
        ordered.sort_unstable();
        let mut builder = SourceSpanRootBuilderV1::new();
        for span in ordered {
            builder.push(*span)?;
        }
        Ok(builder.finish())
    }

    pub(crate) const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) const fn span_count(self) -> u32 {
        self.span_count
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        root_bytes(self)
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        if self.span_count > MAX_SOURCE_SPANS_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::CountLimit("source spans"));
        }
        Ok(())
    }

    fn encode_into(self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.digest);
        bytes.extend_from_slice(&self.span_count.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() != 36 {
            return Err(BootstrapImportError::InvalidFieldLength);
        }
        let value = Self {
            digest: array_32(&bytes[..32])?,
            span_count: read_u32(&bytes[32..])?,
        };
        value.validate()?;
        Ok(value)
    }
}

impl OperationRootV1 {
    pub(crate) fn empty() -> Self {
        OperationRootBuilderV1::new().finish()
    }

    pub(crate) fn from_operations(
        operations: &[OperationLeafV1],
    ) -> Result<Self, BootstrapImportError> {
        checked_count(
            operations.len(),
            MAX_OPERATIONS_PER_BOOTSTRAP_PART,
            "operations",
        )?;
        let mut ordered = operations.iter().collect::<Vec<_>>();
        ordered.sort_unstable();
        let mut builder = OperationRootBuilderV1::new();
        for operation in ordered {
            builder.push(*operation)?;
        }
        Ok(builder.finish())
    }

    pub(crate) const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) const fn operation_count(self) -> u32 {
        self.operation_count
    }

    pub(crate) const fn semantic_effect_bytes(self) -> u64 {
        self.semantic_effect_bytes
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        root_bytes(self)
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        if self.operation_count > MAX_OPERATIONS_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::CountLimit("operations"));
        }
        if self.semantic_effect_bytes > MAX_SEMANTIC_EFFECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("semantic effect"));
        }
        Ok(())
    }

    fn encode_into(self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.digest);
        bytes.extend_from_slice(&self.operation_count.to_be_bytes());
        bytes.extend_from_slice(&self.semantic_effect_bytes.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() != 44 {
            return Err(BootstrapImportError::InvalidFieldLength);
        }
        let value = Self {
            digest: array_32(&bytes[..32])?,
            operation_count: read_u32(&bytes[32..36])?,
            semantic_effect_bytes: read_u64(&bytes[36..])?,
        };
        value.validate()?;
        Ok(value)
    }
}

impl PayloadObjectRootV1 {
    pub(crate) fn empty() -> Self {
        PayloadObjectRootBuilderV1::new().finish()
    }

    pub(crate) fn from_objects(
        objects: &[PayloadObjectDescriptorV1],
    ) -> Result<Self, BootstrapImportError> {
        checked_count(
            objects.len(),
            MAX_OPERATIONS_PER_BOOTSTRAP_PART,
            "payload objects",
        )?;
        let mut ordered = objects.iter().collect::<Vec<_>>();
        ordered.sort_unstable();
        let mut builder = PayloadObjectRootBuilderV1::new();
        for object in ordered {
            builder.push(*object)?;
        }
        Ok(builder.finish())
    }

    pub(crate) const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) const fn object_count(self) -> u32 {
        self.object_count
    }

    pub(crate) const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        root_bytes(self)
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        if self.object_count > MAX_OPERATIONS_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::CountLimit("payload objects"));
        }
        if self.total_bytes > MAX_BATCH_OBJECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("batch object"));
        }
        Ok(())
    }

    fn encode_into(self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.digest);
        bytes.extend_from_slice(&self.object_count.to_be_bytes());
        bytes.extend_from_slice(&self.total_bytes.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() != 44 {
            return Err(BootstrapImportError::InvalidFieldLength);
        }
        let value = Self {
            digest: array_32(&bytes[..32])?,
            object_count: read_u32(&bytes[32..36])?,
            total_bytes: read_u64(&bytes[36..])?,
        };
        value.validate()?;
        Ok(value)
    }
}

impl FullObjectRootV1 {
    pub(crate) fn empty() -> Self {
        FullObjectRootBuilderV1::new()
            .finish()
            .expect("empty complete-object root is valid")
    }

    pub(crate) fn from_descriptors(
        descriptors: &[FullObjectDescriptorV1],
    ) -> Result<Self, BootstrapImportError> {
        checked_count(
            descriptors.len(),
            MAX_FULL_OBJECTS_PER_BOOTSTRAP_PART,
            "full objects",
        )?;
        let mut ordered = descriptors.iter().copied().collect::<Vec<_>>();
        ordered.sort_unstable();
        let mut builder = FullObjectRootBuilderV1::new();
        for descriptor in ordered {
            builder.push(descriptor)?;
        }
        builder.finish()
    }

    /// Construct the post-evidence root.  The inclusion boundary is exact:
    /// payload descriptors must reproduce `evidence.payload_object_root()`,
    /// the encoded evidence object is inserted once, and `manifest_objects`
    /// must all be manifest-defined non-payload descriptors.  The caller may
    /// then persist this independent root in the aggregate descriptor.
    fn for_part(
        evidence: BootstrapImportPartEvidenceV1,
        payload_objects: &[PayloadObjectDescriptorV1],
        manifest_objects: &[FullObjectDescriptorV1],
    ) -> Result<Self, BootstrapImportError> {
        if PayloadObjectRootV1::from_objects(payload_objects)? != evidence.payload_object_root() {
            return Err(BootstrapImportError::FullObjectRootMismatch);
        }
        let evidence_bytes = evidence.encode()?;
        let mut descriptors = Vec::with_capacity(
            payload_objects
                .len()
                .checked_add(manifest_objects.len())
                .and_then(|count| count.checked_add(1))
                .ok_or(BootstrapImportError::LengthOverflow)?,
        );
        descriptors.extend(payload_objects.iter().map(FullObjectDescriptorV1::payload));
        descriptors.push(FullObjectDescriptorV1::part_evidence(
            evidence.evidence_digest(),
            u64::try_from(evidence_bytes.len())
                .map_err(|_| BootstrapImportError::LengthOverflow)?,
        )?);
        for object in manifest_objects {
            if object.kind != FullObjectKindV1::ManifestDefined {
                return Err(BootstrapImportError::InvalidFullObjectKind);
            }
            descriptors.push(*object);
        }
        Self::from_descriptors(&descriptors)
    }

    pub(crate) const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) const fn object_count(self) -> u32 {
        self.object_count
    }

    pub(crate) const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        if self.object_count > MAX_FULL_OBJECTS_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::CountLimit("full objects"));
        }
        if self.total_bytes > MAX_FULL_OBJECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("full object"));
        }
        Ok(())
    }

    fn encode_into(self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.digest);
        bytes.extend_from_slice(&self.object_count.to_be_bytes());
        bytes.extend_from_slice(&self.total_bytes.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() != 44 {
            return Err(BootstrapImportError::InvalidFieldLength);
        }
        let value = Self {
            digest: array_32(&bytes[..32])?,
            object_count: read_u32(&bytes[32..36])?,
            total_bytes: read_u64(&bytes[36..])?,
        };
        value.validate()?;
        Ok(value)
    }
}

/// The fixed v1 profile input.  The discriminant is included before every
/// big-endian value, so a width change cannot collide with a reordered value.
/// Keep every v1 acceptance-affecting version and bound in this one list.
#[derive(Clone, Copy)]
enum ProfileConstantV1 {
    U8(u8),
    U32(u32),
    U64(u64),
}

/// Exact legacy V1 constants. Current policy changes must not silently change
/// the digest accepted for already-authored V1/V2 parts.
const PROFILE_CONSTANTS_V1: &[ProfileConstantV1] = &[
    ProfileConstantV1::U32(BOOTSTRAP_IMPORT_SCHEMA_VERSION),
    ProfileConstantV1::U32(BOOTSTRAP_PARTITION_POLICY_VERSION),
    ProfileConstantV1::U32(SOURCE_LEAF_SCHEMA_VERSION),
    ProfileConstantV1::U32(SOURCE_BLOB_SCHEMA_VERSION),
    ProfileConstantV1::U32(SOURCE_SPAN_SCHEMA_VERSION),
    ProfileConstantV1::U32(OPERATION_ROOT_SCHEMA_VERSION),
    ProfileConstantV1::U32(PAYLOAD_OBJECT_ROOT_SCHEMA_VERSION),
    ProfileConstantV1::U32(FULL_OBJECT_ROOT_SCHEMA_VERSION),
    ProfileConstantV1::U32(BOOTSTRAP_FRONTIER_SCHEMA_VERSION),
    ProfileConstantV1::U32(SOURCE_INVENTORY_INDEX_SCHEMA_VERSION),
    ProfileConstantV1::U32(SOURCE_BLOB_INDEX_SCHEMA_VERSION),
    ProfileConstantV1::U32(PART_SPAN_INDEX_SCHEMA_VERSION),
    ProfileConstantV1::U32(BOOTSTRAP_AGGREGATE_COMMIT_SCHEMA_VERSION),
    ProfileConstantV1::U32(MAX_BOOTSTRAP_PARTS),
    ProfileConstantV1::U32(4_096),
    ProfileConstantV1::U32(4_096),
    ProfileConstantV1::U64(MAX_SEMANTIC_EFFECT_BYTES_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U64(MAX_BATCH_OBJECT_BYTES_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U64(MAX_BOOTSTRAP_PART_EVIDENCE_BYTES as u64),
    ProfileConstantV1::U64(MAX_BOOTSTRAP_AGGREGATE_MANIFEST_BYTES as u64),
    ProfileConstantV1::U32(MAX_SOURCE_BLOB_CHUNK_BYTES),
    ProfileConstantV1::U32(MAX_SOURCE_INVENTORY_LEAVES),
    ProfileConstantV1::U32(MAX_SOURCE_BLOB_CHUNKS),
    ProfileConstantV1::U64(MAX_SOURCE_LOCATOR_BYTES as u64),
    ProfileConstantV1::U64(MAX_SOURCE_INDEX_PAGE_BYTES as u64),
    ProfileConstantV1::U32(MAX_SOURCE_INDEX_PAGES),
    ProfileConstantV1::U64((1024 * 1024) as u64),
    ProfileConstantV1::U64(MAX_BOOTSTRAP_AGGREGATE_COMMIT_BYTES as u64),
    ProfileConstantV1::U64(MAX_SOURCE_FILE_BYTES),
    ProfileConstantV1::U64(MAX_TOTAL_SOURCE_BYTES),
    ProfileConstantV1::U32(MAX_PARSED_NODES_PER_SOURCE_FILE),
    ProfileConstantV1::U8(MAX_CANONICAL_NESTING_DEPTH),
    ProfileConstantV1::U32(MAX_FULL_OBJECTS_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U64(MAX_FULL_OBJECT_BYTES_PER_BOOTSTRAP_PART),
];

const PROFILE_CONSTANTS_CURRENT: &[ProfileConstantV1] = &[
    ProfileConstantV1::U32(BOOTSTRAP_IMPORT_SCHEMA_VERSION),
    ProfileConstantV1::U32(BOOTSTRAP_PARTITION_POLICY_VERSION),
    ProfileConstantV1::U32(SOURCE_LEAF_SCHEMA_VERSION),
    ProfileConstantV1::U32(SOURCE_BLOB_SCHEMA_VERSION),
    ProfileConstantV1::U32(SOURCE_SPAN_SCHEMA_VERSION),
    ProfileConstantV1::U32(OPERATION_ROOT_SCHEMA_VERSION),
    ProfileConstantV1::U32(PAYLOAD_OBJECT_ROOT_SCHEMA_VERSION),
    ProfileConstantV1::U32(FULL_OBJECT_ROOT_SCHEMA_VERSION),
    ProfileConstantV1::U32(BOOTSTRAP_FRONTIER_SCHEMA_VERSION),
    ProfileConstantV1::U32(SOURCE_INVENTORY_INDEX_SCHEMA_VERSION),
    ProfileConstantV1::U32(SOURCE_BLOB_INDEX_SCHEMA_VERSION),
    ProfileConstantV1::U32(PART_SPAN_INDEX_SCHEMA_VERSION),
    ProfileConstantV1::U32(BOOTSTRAP_AGGREGATE_COMMIT_SCHEMA_VERSION),
    ProfileConstantV1::U32(MAX_BOOTSTRAP_PARTS),
    ProfileConstantV1::U32(MAX_OPERATIONS_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U32(MAX_SOURCE_SPANS_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U64(MAX_SEMANTIC_EFFECT_BYTES_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U64(MAX_BATCH_OBJECT_BYTES_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U64(MAX_BOOTSTRAP_PART_EVIDENCE_BYTES as u64),
    ProfileConstantV1::U64(MAX_BOOTSTRAP_AGGREGATE_MANIFEST_BYTES as u64),
    ProfileConstantV1::U32(MAX_SOURCE_BLOB_CHUNK_BYTES),
    ProfileConstantV1::U32(MAX_SOURCE_INVENTORY_LEAVES),
    ProfileConstantV1::U32(MAX_SOURCE_BLOB_CHUNKS),
    ProfileConstantV1::U64(MAX_SOURCE_LOCATOR_BYTES as u64),
    ProfileConstantV1::U64(MAX_SOURCE_INDEX_PAGE_BYTES as u64),
    ProfileConstantV1::U32(MAX_SOURCE_INDEX_PAGES),
    ProfileConstantV1::U64(MAX_PART_SPAN_INDEX_BYTES as u64),
    ProfileConstantV1::U64(MAX_BOOTSTRAP_AGGREGATE_COMMIT_BYTES as u64),
    ProfileConstantV1::U64(MAX_SOURCE_FILE_BYTES),
    ProfileConstantV1::U64(MAX_TOTAL_SOURCE_BYTES),
    ProfileConstantV1::U32(MAX_PARSED_NODES_PER_SOURCE_FILE),
    ProfileConstantV1::U8(MAX_CANONICAL_NESTING_DEPTH),
    ProfileConstantV1::U32(MAX_FULL_OBJECTS_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U64(MAX_FULL_OBJECT_BYTES_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U32(MAX_PAGE_DOCUMENTS_PER_BOOTSTRAP_PART),
    ProfileConstantV1::U64(MAX_PREPARED_MANIFEST_BYTES_PER_BOOTSTRAP_PART as u64),
];

fn profile_digest_from_constants(constants: &[ProfileConstantV1]) -> BootstrapProfileDigestV1 {
    let mut bytes = Vec::with_capacity(constants.len() * 9);
    for constant in constants {
        match constant {
            ProfileConstantV1::U8(value) => {
                bytes.push(1);
                bytes.push(*value);
            }
            ProfileConstantV1::U32(value) => {
                bytes.push(4);
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            ProfileConstantV1::U64(value) => {
                bytes.push(8);
                bytes.extend_from_slice(&value.to_be_bytes());
            }
        }
    }
    BootstrapProfileDigestV1::digest(b"tine/bootstrap-import/partition-profile/v1\0", &[&bytes])
}

/// The fixed v1 partition profile.  Its digest binds every acceptance-affecting
/// byte/count cap and schema/policy version above, so a receiver never infers a
/// profile from ambient configuration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct BootstrapPartitionProfileV1 {
    digest: BootstrapProfileDigestV1,
}

impl BootstrapPartitionProfileV1 {
    pub(crate) fn v1() -> Self {
        Self {
            digest: profile_digest_from_constants(PROFILE_CONSTANTS_V1),
        }
    }

    fn v2() -> Self {
        Self {
            digest: BootstrapProfileDigestV1::digest(
                b"tine/bootstrap-import/partition-profile/v2\0",
                &[
                    Self::v1().digest.as_bytes(),
                    b"terminal-reference-catalog/v1",
                ],
            ),
        }
    }

    fn v3() -> Self {
        let current_constants = profile_digest_from_constants(PROFILE_CONSTANTS_CURRENT);
        Self {
            digest: BootstrapProfileDigestV1::digest(
                b"tine/bootstrap-import/partition-profile/v3\0",
                &[
                    current_constants.as_bytes(),
                    b"terminal-reference-catalog/v1",
                    b"resource-budgeted-parts/v1",
                ],
            ),
        }
    }

    fn v4() -> Self {
        Self {
            digest: BootstrapProfileDigestV1::digest(
                b"tine/bootstrap-import/partition-profile/v4\0",
                &[
                    Self::v3().digest.as_bytes(),
                    b"page-capsule-declarations/v1",
                ],
            ),
        }
    }

    fn v5() -> Self {
        Self {
            digest: BootstrapProfileDigestV1::digest(
                b"tine/bootstrap-import/partition-profile/v5\0",
                &[
                    Self::v4().digest.as_bytes(),
                    b"terminal-current-path-catalog/v1",
                    b"terminal-logseq-claims/v1",
                ],
            ),
        }
    }

    /// Current authoring profile. V4 keeps each page declaration with its
    /// content capsule. This avoids publishing and immediately reopening an
    /// empty home document while retaining every V3 resource boundary. V5
    /// constructs the rebuildable current-path and Logseq-claim indexes once
    /// from terminal authority instead of rebuilding every accumulated prefix.
    /// V6 applies the same rule to the portable-path and page-name indexes;
    /// global source selection has already made both namespaces unique.
    pub(crate) fn current() -> Self {
        Self {
            digest: BootstrapProfileDigestV1::digest(
                b"tine/bootstrap-import/partition-profile/v6\0",
                &[
                    Self::v5().digest.as_bytes(),
                    b"terminal-portable-paths/v1",
                    b"terminal-page-names/v1",
                ],
            ),
        }
    }

    pub(crate) const fn digest(self) -> BootstrapProfileDigestV1 {
        self.digest
    }

    pub(crate) fn uses_terminal_reference_catalog(
        digest: BootstrapProfileDigestV1,
    ) -> Result<bool, BootstrapImportError> {
        Self::validate_digest(digest)?;
        Ok(digest == Self::v2().digest
            || digest == Self::v3().digest
            || digest == Self::v4().digest
            || digest == Self::v5().digest
            || digest == Self::current().digest)
    }

    pub(crate) fn uses_terminal_current_path_catalog(
        digest: BootstrapProfileDigestV1,
    ) -> Result<bool, BootstrapImportError> {
        Self::validate_digest(digest)?;
        Ok(digest == Self::v5().digest || digest == Self::current().digest)
    }

    pub(crate) fn uses_terminal_logseq_claims(
        digest: BootstrapProfileDigestV1,
    ) -> Result<bool, BootstrapImportError> {
        Self::validate_digest(digest)?;
        Ok(digest == Self::v5().digest || digest == Self::current().digest)
    }

    pub(crate) fn uses_terminal_portable_paths(
        digest: BootstrapProfileDigestV1,
    ) -> Result<bool, BootstrapImportError> {
        Self::validate_digest(digest)?;
        Ok(digest == Self::current().digest)
    }

    pub(crate) fn uses_terminal_page_names(
        digest: BootstrapProfileDigestV1,
    ) -> Result<bool, BootstrapImportError> {
        Self::validate_digest(digest)?;
        Ok(digest == Self::current().digest)
    }

    fn validate_digest(digest: BootstrapProfileDigestV1) -> Result<(), BootstrapImportError> {
        if digest != Self::v1().digest
            && digest != Self::v2().digest
            && digest != Self::v3().digest
            && digest != Self::v4().digest
            && digest != Self::v5().digest
            && digest != Self::current().digest
        {
            return Err(BootstrapImportError::ProfileMismatch);
        }
        Ok(())
    }
}

/// One source item in the complete inventory. The managed kind and exact UTF-8
/// graph-relative path bytes come from Graph's configured-root classifier.
/// Neither identity nor decoding reconstructs a conventional pages/journals
/// layout.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceLeafV1 {
    kind: ManagedTextKind,
    path: ManagedPath,
    content_digest: SourceContentDigestV1,
    byte_length: u64,
}

impl SourceLeafV1 {
    pub(crate) fn new(
        kind: ManagedTextKind,
        path: ManagedPath,
        content_digest: SourceContentDigestV1,
        byte_length: u64,
    ) -> Result<Self, BootstrapImportError> {
        if path.as_str().len() > MAX_SOURCE_LOCATOR_BYTES {
            return Err(BootstrapImportError::LocatorLimit);
        }
        if byte_length > MAX_SOURCE_FILE_BYTES {
            return Err(BootstrapImportError::ByteLimit("source file"));
        }
        Ok(Self {
            kind,
            path,
            content_digest,
            byte_length,
        })
    }

    pub(crate) fn digest(&self) -> SourceLeafDigestV1 {
        SourceLeafDigestV1::digest(
            b"tine/bootstrap-import/source-leaf/v1\0",
            &[
                &SOURCE_LEAF_SCHEMA_VERSION.to_be_bytes(),
                &[managed_text_kind_byte(self.kind)],
                self.path.as_str().as_bytes(),
                self.content_digest.as_bytes(),
                &self.byte_length.to_be_bytes(),
            ],
        )
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        canonical_encode(
            b"tine/bootstrap-import/source-leaf/v1\0",
            &[
                &SOURCE_LEAF_SCHEMA_VERSION.to_be_bytes(),
                &[managed_text_kind_byte(self.kind)],
                self.path.as_str().as_bytes(),
                self.content_digest.as_bytes(),
                &self.byte_length.to_be_bytes(),
            ],
        )
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        let fields = decode_canonical_value(bytes, b"tine/bootstrap-import/source-leaf/v1\0", 5)?;
        let version = read_u32(fields[0])?;
        if version != SOURCE_LEAF_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let kind = decode_managed_text_kind(fields[1])?;
        let path = std::str::from_utf8(fields[2])
            .map_err(|_| BootstrapImportError::InvalidUtf8)
            .and_then(|path| {
                ManagedPath::parse(path.to_owned())
                    .map_err(|_| BootstrapImportError::InvalidManagedPath)
            })?;
        let value = Self::new(
            kind,
            path,
            SourceContentDigestV1::from_bytes(array_32(fields[3])?),
            read_u64(fields[4])?,
        )?;
        if value.encode().as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    pub(crate) const fn kind(&self) -> ManagedTextKind {
        self.kind
    }

    pub(crate) fn path(&self) -> &ManagedPath {
        &self.path
    }

    pub(crate) const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    pub(crate) const fn content_digest(&self) -> SourceContentDigestV1 {
        self.content_digest
    }

    fn canonical_cmp(&self, other: &Self) -> Ordering {
        self.path
            .as_str()
            .as_bytes()
            .cmp(other.path.as_str().as_bytes())
            .then_with(|| {
                managed_text_kind_byte(self.kind).cmp(&managed_text_kind_byte(other.kind))
            })
            .then_with(|| self.content_digest.cmp(&other.content_digest))
            .then_with(|| self.byte_length.cmp(&other.byte_length))
    }
}

/// A fixed-size, independently addressable source-blob chunk.  The caller is
/// responsible for choosing the chunks; this schema only binds and bounds them.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceBlobChunkDescriptorV1 {
    source_leaf: SourceLeafDigestV1,
    ordinal: u32,
    count: u32,
    offset: u64,
    byte_length: u32,
    content_digest: SourceBlobChunkDigestV1,
}

impl SourceBlobChunkDescriptorV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        source_leaf: SourceLeafDigestV1,
        ordinal: u32,
        count: u32,
        offset: u64,
        byte_length: u32,
        content_digest: SourceBlobChunkDigestV1,
    ) -> Result<Self, BootstrapImportError> {
        if count == 0 || count > MAX_SOURCE_BLOB_CHUNKS || ordinal >= count {
            return Err(BootstrapImportError::InvalidOrdinal);
        }
        if byte_length == 0 || byte_length > MAX_SOURCE_BLOB_CHUNK_BYTES {
            return Err(BootstrapImportError::ByteLimit("source blob chunk"));
        }
        offset
            .checked_add(u64::from(byte_length))
            .ok_or(BootstrapImportError::LengthOverflow)?;
        Ok(Self {
            source_leaf,
            ordinal,
            count,
            offset,
            byte_length,
            content_digest,
        })
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        canonical_encode(
            b"tine/bootstrap-import/source-blob-chunk/v1\0",
            &[
                &SOURCE_BLOB_SCHEMA_VERSION.to_be_bytes(),
                self.source_leaf.as_bytes(),
                &self.ordinal.to_be_bytes(),
                &self.count.to_be_bytes(),
                &self.offset.to_be_bytes(),
                &self.byte_length.to_be_bytes(),
                self.content_digest.as_bytes(),
            ],
        )
    }

    pub(crate) fn digest(&self) -> SourceBlobChunkDescriptorDigestV1 {
        SourceBlobChunkDescriptorDigestV1::digest(
            b"tine/bootstrap-import/source-blob-chunk-digest/v1\0",
            &[&self.encode()],
        )
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        let fields =
            decode_canonical_value(bytes, b"tine/bootstrap-import/source-blob-chunk/v1\0", 7)?;
        let version = read_u32(fields[0])?;
        if version != SOURCE_BLOB_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let value = Self::new(
            SourceLeafDigestV1::from_bytes(array_32(fields[1])?),
            read_u32(fields[2])?,
            read_u32(fields[3])?,
            read_u64(fields[4])?,
            read_u32(fields[5])?,
            SourceBlobChunkDigestV1::from_bytes(array_32(fields[6])?),
        )?;
        if value.encode().as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    pub(crate) const fn source_leaf(self) -> SourceLeafDigestV1 {
        self.source_leaf
    }

    pub(crate) const fn ordinal(self) -> u32 {
        self.ordinal
    }

    pub(crate) const fn offset(self) -> u64 {
        self.offset
    }

    pub(crate) const fn byte_length(self) -> u32 {
        self.byte_length
    }

    pub(crate) const fn content_digest(self) -> SourceBlobChunkDigestV1 {
        self.content_digest
    }
}

/// One directly loadable canonical inventory page. Empty inventories have no
/// pages. Non-empty indexes have consecutive zero-based page ordinals, and
/// exactly the last page has `terminal = true`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceInventoryIndexPageV1 {
    root: SourceInventoryRootV1,
    page_ordinal: u32,
    first_entry_ordinal: u32,
    terminal: bool,
    entries: Vec<SourceLeafV1>,
}

impl SourceInventoryIndexPageV1 {
    fn new(
        root: SourceInventoryRootV1,
        page_ordinal: u32,
        first_entry_ordinal: u32,
        terminal: bool,
        entries: Vec<SourceLeafV1>,
    ) -> Result<Self, BootstrapImportError> {
        if entries.is_empty() {
            return Err(BootstrapImportError::EmptyIndexPage);
        }
        if page_ordinal >= MAX_SOURCE_INDEX_PAGES {
            return Err(BootstrapImportError::CountLimit(
                "source inventory index pages",
            ));
        }
        let entry_count =
            u32::try_from(entries.len()).map_err(|_| BootstrapImportError::LengthOverflow)?;
        if first_entry_ordinal
            .checked_add(entry_count)
            .ok_or(BootstrapImportError::LengthOverflow)?
            > root.source_count()
        {
            return Err(BootstrapImportError::IndexContinuityMismatch);
        }
        for pair in entries.windows(2) {
            if pair[0].canonical_cmp(&pair[1]) != Ordering::Less {
                return Err(BootstrapImportError::NonCanonicalOrder);
            }
        }
        let value = Self {
            root,
            page_ordinal,
            first_entry_ordinal,
            terminal,
            entries,
        };
        if value.encoded_len()? > MAX_SOURCE_INDEX_PAGE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit(
                "source inventory index page",
            ));
        }
        Ok(value)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, BootstrapImportError> {
        let mut bytes = Vec::with_capacity(self.encoded_len()?);
        bytes.extend_from_slice(INVENTORY_PAGE_MAGIC);
        bytes.extend_from_slice(&SOURCE_INVENTORY_INDEX_SCHEMA_VERSION.to_be_bytes());
        self.root.encode_into(&mut bytes);
        bytes.extend_from_slice(&self.page_ordinal.to_be_bytes());
        bytes.extend_from_slice(&self.first_entry_ordinal.to_be_bytes());
        bytes.push(u8::from(self.terminal));
        bytes.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for entry in &self.entries {
            put_sized(&mut bytes, &entry.encode())?;
        }
        if bytes.len() > MAX_SOURCE_INDEX_PAGE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit(
                "source inventory index page",
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() > MAX_SOURCE_INDEX_PAGE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit(
                "source inventory index page",
            ));
        }
        let mut cursor = FixedReader::with_magic(bytes, INVENTORY_PAGE_MAGIC)?;
        let version = cursor.u32()?;
        if version != SOURCE_INVENTORY_INDEX_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let root = SourceInventoryRootV1::decode(cursor.take(36)?)?;
        let page_ordinal = cursor.u32()?;
        let first_entry_ordinal = cursor.u32()?;
        let terminal = decode_bool(cursor.take(1)?)?;
        let count = cursor.u32()?;
        if count > root.source_count() {
            return Err(BootstrapImportError::IndexContinuityMismatch);
        }
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            entries.push(SourceLeafV1::decode(cursor.sized()?)?);
        }
        cursor.finish()?;
        let value = Self::new(root, page_ordinal, first_entry_ordinal, terminal, entries)?;
        if value.encode()?.as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    fn encoded_len(&self) -> Result<usize, BootstrapImportError> {
        self.entries.iter().try_fold(61_usize, |length, entry| {
            length
                .checked_add(4)
                .and_then(|length| length.checked_add(entry.encode().len()))
                .ok_or(BootstrapImportError::LengthOverflow)
        })
    }

    pub(crate) const fn page_ordinal(&self) -> u32 {
        self.page_ordinal
    }

    pub(crate) const fn terminal(&self) -> bool {
        self.terminal
    }

    pub(crate) fn entries(&self) -> &[SourceLeafV1] {
        &self.entries
    }
}

/// Streaming inventory index author. It retains at most one 1 MiB page and
/// one prior leaf. The caller writes each returned non-terminal page before
/// continuing, then writes the optional terminal page returned by `finish`.
pub(crate) struct SourceInventoryIndexBuilderV1 {
    expected_root: SourceInventoryRootV1,
    root: SourceInventoryRootBuilderV1,
    page_ordinal: u32,
    first_entry_ordinal: u32,
    page_bytes: usize,
    entries: Vec<SourceLeafV1>,
}

impl SourceInventoryIndexBuilderV1 {
    pub(crate) fn new(expected_root: SourceInventoryRootV1) -> Self {
        Self {
            expected_root,
            root: SourceInventoryRootBuilderV1::new(),
            page_ordinal: 0,
            first_entry_ordinal: 0,
            page_bytes: 61,
            entries: Vec::new(),
        }
    }

    pub(crate) fn push(
        &mut self,
        leaf: SourceLeafV1,
    ) -> Result<Option<SourceInventoryIndexPageV1>, BootstrapImportError> {
        let entry_bytes = leaf
            .encode()
            .len()
            .checked_add(4)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if 61 + entry_bytes > MAX_SOURCE_INDEX_PAGE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit(
                "source inventory index entry",
            ));
        }
        let flushed = if !self.entries.is_empty()
            && self
                .page_bytes
                .checked_add(entry_bytes)
                .ok_or(BootstrapImportError::LengthOverflow)?
                > MAX_SOURCE_INDEX_PAGE_BYTES
        {
            Some(self.take_page(false)?)
        } else {
            None
        };
        self.root.push(&leaf)?;
        self.page_bytes = self
            .page_bytes
            .checked_add(entry_bytes)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.entries.push(leaf);
        Ok(flushed)
    }

    pub(crate) fn finish(
        mut self,
    ) -> Result<Option<SourceInventoryIndexPageV1>, BootstrapImportError> {
        let root = std::mem::replace(&mut self.root, SourceInventoryRootBuilderV1::new()).finish();
        if root != self.expected_root {
            return Err(BootstrapImportError::IndexRootMismatch);
        }
        if self.entries.is_empty() {
            if self.expected_root.source_count() != 0 || self.page_ordinal != 0 {
                return Err(BootstrapImportError::IndexContinuityMismatch);
            }
            return Ok(None);
        }
        self.take_page(true).map(Some)
    }

    fn take_page(
        &mut self,
        terminal: bool,
    ) -> Result<SourceInventoryIndexPageV1, BootstrapImportError> {
        let entries = std::mem::take(&mut self.entries);
        let count =
            u32::try_from(entries.len()).map_err(|_| BootstrapImportError::LengthOverflow)?;
        let page = SourceInventoryIndexPageV1::new(
            self.expected_root,
            self.page_ordinal,
            self.first_entry_ordinal,
            terminal,
            entries,
        )?;
        self.page_ordinal = self
            .page_ordinal
            .checked_add(1)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if self.page_ordinal > MAX_SOURCE_INDEX_PAGES {
            return Err(BootstrapImportError::CountLimit(
                "source inventory index pages",
            ));
        }
        self.first_entry_ordinal = self
            .first_entry_ordinal
            .checked_add(count)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.page_bytes = 61;
        Ok(page)
    }
}

pub(crate) struct SourceInventoryIndexValidatorV1 {
    expected_root: SourceInventoryRootV1,
    expected_pages: u32,
    root: SourceInventoryRootBuilderV1,
    next_page: u32,
    next_entry: u32,
    terminal: bool,
}

impl SourceInventoryIndexValidatorV1 {
    pub(crate) fn new(
        expected_root: SourceInventoryRootV1,
        expected_pages: u32,
    ) -> Result<Self, BootstrapImportError> {
        validate_page_count(
            expected_pages,
            expected_root.source_count(),
            "source inventory index pages",
        )?;
        Ok(Self {
            expected_root,
            expected_pages,
            root: SourceInventoryRootBuilderV1::new(),
            next_page: 0,
            next_entry: 0,
            terminal: false,
        })
    }

    pub(crate) fn push_page(
        &mut self,
        page: &SourceInventoryIndexPageV1,
    ) -> Result<(), BootstrapImportError> {
        if self.terminal
            || page.root != self.expected_root
            || page.page_ordinal != self.next_page
            || page.first_entry_ordinal != self.next_entry
        {
            return Err(BootstrapImportError::IndexContinuityMismatch);
        }
        for entry in &page.entries {
            self.root.push(entry)?;
        }
        self.next_entry = self
            .next_entry
            .checked_add(page.entries.len() as u32)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.next_page = self
            .next_page
            .checked_add(1)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.terminal = page.terminal;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<u32, BootstrapImportError> {
        let zero = self.expected_root.source_count() == 0;
        if (zero && (self.next_page != 0 || self.terminal))
            || (!zero && !self.terminal)
            || self.next_page != self.expected_pages
            || self.next_entry != self.expected_root.source_count()
            || self.root.finish() != self.expected_root
        {
            return Err(BootstrapImportError::IndexContinuityMismatch);
        }
        Ok(self.next_page)
    }
}

struct SourceBlobIndexRootAccumulatorV1 {
    root: RootHasherV1,
    last: Option<SourceBlobChunkDescriptorV1>,
    total_bytes: u64,
}

impl SourceBlobIndexRootAccumulatorV1 {
    fn new() -> Self {
        Self {
            root: RootHasherV1::new(b"tine/bootstrap-import/source-blob-root/v1\0"),
            last: None,
            total_bytes: 0,
        }
    }

    fn push(
        &mut self,
        descriptor: SourceBlobChunkDescriptorV1,
    ) -> Result<(), BootstrapImportError> {
        if let Some(last) = self.last {
            if descriptor == last {
                return Err(BootstrapImportError::DuplicateCanonicalItem);
            }
            if descriptor < last {
                return Err(BootstrapImportError::NonCanonicalOrder);
            }
            if descriptor.source_leaf == last.source_leaf && descriptor.ordinal == last.ordinal {
                return Err(BootstrapImportError::ConflictingProtocolIdentity(
                    "source blob chunk ordinal",
                ));
            }
        }
        self.root.push(
            &descriptor.encode(),
            MAX_SOURCE_BLOB_CHUNKS,
            "source blob chunks",
        )?;
        self.total_bytes = self
            .total_bytes
            .checked_add(u64::from(descriptor.byte_length))
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if self.total_bytes > MAX_TOTAL_SOURCE_BYTES {
            return Err(BootstrapImportError::ByteLimit("total source bytes"));
        }
        self.last = Some(descriptor);
        Ok(())
    }

    fn finish(self) -> SourceBlobChunkRootV1 {
        let count = self.root.count;
        SourceBlobChunkRootV1 {
            digest: self.root.finish(),
            chunk_count: count,
            total_bytes: self.total_bytes,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceBlobIndexPageV1 {
    root: SourceBlobChunkRootV1,
    page_ordinal: u32,
    first_entry_ordinal: u32,
    terminal: bool,
    entries: Vec<SourceBlobChunkDescriptorV1>,
}

impl SourceBlobIndexPageV1 {
    fn new(
        root: SourceBlobChunkRootV1,
        page_ordinal: u32,
        first_entry_ordinal: u32,
        terminal: bool,
        entries: Vec<SourceBlobChunkDescriptorV1>,
    ) -> Result<Self, BootstrapImportError> {
        if entries.is_empty() {
            return Err(BootstrapImportError::EmptyIndexPage);
        }
        if page_ordinal >= MAX_SOURCE_INDEX_PAGES {
            return Err(BootstrapImportError::CountLimit("source blob index pages"));
        }
        let entry_count =
            u32::try_from(entries.len()).map_err(|_| BootstrapImportError::LengthOverflow)?;
        if first_entry_ordinal
            .checked_add(entry_count)
            .ok_or(BootstrapImportError::LengthOverflow)?
            > root.chunk_count()
        {
            return Err(BootstrapImportError::IndexContinuityMismatch);
        }
        if entries.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(BootstrapImportError::NonCanonicalOrder);
        }
        let value = Self {
            root,
            page_ordinal,
            first_entry_ordinal,
            terminal,
            entries,
        };
        if value.encoded_len()? > MAX_SOURCE_INDEX_PAGE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit(
                "source blob index page",
            ));
        }
        Ok(value)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, BootstrapImportError> {
        let mut bytes = Vec::with_capacity(self.encoded_len()?);
        bytes.extend_from_slice(BLOB_PAGE_MAGIC);
        bytes.extend_from_slice(&SOURCE_BLOB_INDEX_SCHEMA_VERSION.to_be_bytes());
        self.root.encode_into(&mut bytes);
        bytes.extend_from_slice(&self.page_ordinal.to_be_bytes());
        bytes.extend_from_slice(&self.first_entry_ordinal.to_be_bytes());
        bytes.push(u8::from(self.terminal));
        bytes.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for entry in &self.entries {
            put_sized(&mut bytes, &entry.encode())?;
        }
        if bytes.len() > MAX_SOURCE_INDEX_PAGE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit(
                "source blob index page",
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() > MAX_SOURCE_INDEX_PAGE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit(
                "source blob index page",
            ));
        }
        let mut cursor = FixedReader::with_magic(bytes, BLOB_PAGE_MAGIC)?;
        let version = cursor.u32()?;
        if version != SOURCE_BLOB_INDEX_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let root = SourceBlobChunkRootV1::decode(cursor.take(44)?)?;
        let page_ordinal = cursor.u32()?;
        let first_entry_ordinal = cursor.u32()?;
        let terminal = decode_bool(cursor.take(1)?)?;
        let count = cursor.u32()?;
        if count > root.chunk_count() {
            return Err(BootstrapImportError::IndexContinuityMismatch);
        }
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            entries.push(SourceBlobChunkDescriptorV1::decode(cursor.sized()?)?);
        }
        cursor.finish()?;
        let value = Self::new(root, page_ordinal, first_entry_ordinal, terminal, entries)?;
        if value.encode()?.as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    fn encoded_len(&self) -> Result<usize, BootstrapImportError> {
        self.entries.iter().try_fold(69_usize, |length, entry| {
            length
                .checked_add(4)
                .and_then(|length| length.checked_add(entry.encode().len()))
                .ok_or(BootstrapImportError::LengthOverflow)
        })
    }

    pub(crate) const fn page_ordinal(&self) -> u32 {
        self.page_ordinal
    }

    pub(crate) const fn terminal(&self) -> bool {
        self.terminal
    }

    pub(crate) fn entries(&self) -> &[SourceBlobChunkDescriptorV1] {
        &self.entries
    }
}

pub(crate) struct SourceBlobIndexBuilderV1 {
    expected_root: SourceBlobChunkRootV1,
    root: SourceBlobIndexRootAccumulatorV1,
    page_ordinal: u32,
    first_entry_ordinal: u32,
    page_bytes: usize,
    entries: Vec<SourceBlobChunkDescriptorV1>,
}

impl SourceBlobIndexBuilderV1 {
    pub(crate) fn new(expected_root: SourceBlobChunkRootV1) -> Self {
        Self {
            expected_root,
            root: SourceBlobIndexRootAccumulatorV1::new(),
            page_ordinal: 0,
            first_entry_ordinal: 0,
            page_bytes: 69,
            entries: Vec::new(),
        }
    }

    pub(crate) fn push(
        &mut self,
        descriptor: SourceBlobChunkDescriptorV1,
    ) -> Result<Option<SourceBlobIndexPageV1>, BootstrapImportError> {
        let entry_bytes = descriptor
            .encode()
            .len()
            .checked_add(4)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        let flushed = if !self.entries.is_empty()
            && self
                .page_bytes
                .checked_add(entry_bytes)
                .ok_or(BootstrapImportError::LengthOverflow)?
                > MAX_SOURCE_INDEX_PAGE_BYTES
        {
            Some(self.take_page(false)?)
        } else {
            None
        };
        self.root.push(descriptor)?;
        self.page_bytes = self
            .page_bytes
            .checked_add(entry_bytes)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.entries.push(descriptor);
        Ok(flushed)
    }

    pub(crate) fn finish(mut self) -> Result<Option<SourceBlobIndexPageV1>, BootstrapImportError> {
        let root =
            std::mem::replace(&mut self.root, SourceBlobIndexRootAccumulatorV1::new()).finish();
        if root != self.expected_root {
            return Err(BootstrapImportError::IndexRootMismatch);
        }
        if self.entries.is_empty() {
            if self.expected_root.chunk_count() != 0 || self.page_ordinal != 0 {
                return Err(BootstrapImportError::IndexContinuityMismatch);
            }
            return Ok(None);
        }
        self.take_page(true).map(Some)
    }

    fn take_page(&mut self, terminal: bool) -> Result<SourceBlobIndexPageV1, BootstrapImportError> {
        let entries = std::mem::take(&mut self.entries);
        let count =
            u32::try_from(entries.len()).map_err(|_| BootstrapImportError::LengthOverflow)?;
        let page = SourceBlobIndexPageV1::new(
            self.expected_root,
            self.page_ordinal,
            self.first_entry_ordinal,
            terminal,
            entries,
        )?;
        self.page_ordinal = self
            .page_ordinal
            .checked_add(1)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if self.page_ordinal > MAX_SOURCE_INDEX_PAGES {
            return Err(BootstrapImportError::CountLimit("source blob index pages"));
        }
        self.first_entry_ordinal = self
            .first_entry_ordinal
            .checked_add(count)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.page_bytes = 69;
        Ok(page)
    }
}

pub(crate) struct SourceBlobIndexValidatorV1 {
    expected_root: SourceBlobChunkRootV1,
    expected_pages: u32,
    root: SourceBlobIndexRootAccumulatorV1,
    next_page: u32,
    next_entry: u32,
    terminal: bool,
}

impl SourceBlobIndexValidatorV1 {
    pub(crate) fn new(
        expected_root: SourceBlobChunkRootV1,
        expected_pages: u32,
    ) -> Result<Self, BootstrapImportError> {
        validate_page_count(
            expected_pages,
            expected_root.chunk_count(),
            "source blob index pages",
        )?;
        Ok(Self {
            expected_root,
            expected_pages,
            root: SourceBlobIndexRootAccumulatorV1::new(),
            next_page: 0,
            next_entry: 0,
            terminal: false,
        })
    }

    pub(crate) fn push_page(
        &mut self,
        page: &SourceBlobIndexPageV1,
    ) -> Result<(), BootstrapImportError> {
        if self.terminal
            || page.root != self.expected_root
            || page.page_ordinal != self.next_page
            || page.first_entry_ordinal != self.next_entry
        {
            return Err(BootstrapImportError::IndexContinuityMismatch);
        }
        for entry in &page.entries {
            self.root.push(*entry)?;
        }
        self.next_entry = self
            .next_entry
            .checked_add(page.entries.len() as u32)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.next_page = self
            .next_page
            .checked_add(1)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.terminal = page.terminal;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<u32, BootstrapImportError> {
        let zero = self.expected_root.chunk_count() == 0;
        if (zero && (self.next_page != 0 || self.terminal))
            || (!zero && !self.terminal)
            || self.next_page != self.expected_pages
            || self.next_entry != self.expected_root.chunk_count()
            || self.root.finish() != self.expected_root
        {
            return Err(BootstrapImportError::IndexContinuityMismatch);
        }
        Ok(self.next_page)
    }
}

/// A source range attributed to one part.  Ranges are deliberately source
/// metadata only: they do not carry a filesystem path or a live file handle.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceSpanV1 {
    source_leaf: SourceLeafDigestV1,
    offset: u64,
    byte_length: u64,
}

impl SourceSpanV1 {
    pub(crate) fn new(
        source_leaf: SourceLeafDigestV1,
        offset: u64,
        byte_length: u64,
    ) -> Result<Self, BootstrapImportError> {
        if byte_length == 0 {
            return Err(BootstrapImportError::EmptySourceSpan);
        }
        offset
            .checked_add(byte_length)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        Ok(Self {
            source_leaf,
            offset,
            byte_length,
        })
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        canonical_encode(
            b"tine/bootstrap-import/source-span/v1\0",
            &[
                &SOURCE_SPAN_SCHEMA_VERSION.to_be_bytes(),
                self.source_leaf.as_bytes(),
                &self.offset.to_be_bytes(),
                &self.byte_length.to_be_bytes(),
            ],
        )
    }

    pub(crate) fn digest(&self) -> SourceSpanDigestV1 {
        SourceSpanDigestV1::digest(
            b"tine/bootstrap-import/source-span-digest/v1\0",
            &[&self.encode()],
        )
    }

    fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        let fields = decode_canonical_value(bytes, b"tine/bootstrap-import/source-span/v1\0", 4)?;
        let version = read_u32(fields[0])?;
        if version != SOURCE_SPAN_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let value = Self::new(
            SourceLeafDigestV1::from_bytes(array_32(fields[1])?),
            read_u64(fields[2])?,
            read_u64(fields[3])?,
        )?;
        if value.encode().as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    pub(crate) const fn source_leaf(self) -> SourceLeafDigestV1 {
        self.source_leaf
    }

    pub(crate) const fn offset(self) -> u64 {
        self.offset
    }

    pub(crate) const fn byte_length(self) -> u64 {
        self.byte_length
    }
}

/// The bounded direct `part-spans/<part-id>` object. It is intentionally
/// loadable without walking either graph-sized source index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootstrapPartSpanIndexV1 {
    part_id: BootstrapPartId,
    root: SourceSpanRootV1,
    spans: Vec<SourceSpanV1>,
}

impl BootstrapPartSpanIndexV1 {
    pub(crate) fn new(
        part_id: BootstrapPartId,
        mut spans: Vec<SourceSpanV1>,
    ) -> Result<Self, BootstrapImportError> {
        checked_count(
            spans.len(),
            MAX_SOURCE_SPANS_PER_BOOTSTRAP_PART,
            "source spans",
        )?;
        spans.sort_unstable();
        let root = SourceSpanRootV1::from_spans(&spans)?;
        let value = Self {
            part_id,
            root,
            spans,
        };
        if value.encoded_len()? > MAX_PART_SPAN_INDEX_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("part span index"));
        }
        Ok(value)
    }

    pub(crate) const fn part_id(&self) -> BootstrapPartId {
        self.part_id
    }

    pub(crate) const fn root(&self) -> SourceSpanRootV1 {
        self.root
    }

    pub(crate) fn spans(&self) -> &[SourceSpanV1] {
        &self.spans
    }

    pub(crate) fn validate_part(
        &self,
        evidence: BootstrapImportPartEvidenceV1,
    ) -> Result<(), BootstrapImportError> {
        if self.part_id != evidence.part_id() || self.root != evidence.source_span_root() {
            return Err(BootstrapImportError::PartContextMismatch);
        }
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, BootstrapImportError> {
        let mut bytes = Vec::with_capacity(self.encoded_len()?);
        bytes.extend_from_slice(PART_SPAN_MAGIC);
        bytes.extend_from_slice(&PART_SPAN_INDEX_SCHEMA_VERSION.to_be_bytes());
        bytes.extend_from_slice(self.part_id.as_bytes());
        self.root.encode_into(&mut bytes);
        bytes.extend_from_slice(&(self.spans.len() as u32).to_be_bytes());
        for span in &self.spans {
            put_sized(&mut bytes, &span.encode())?;
        }
        if bytes.len() > MAX_PART_SPAN_INDEX_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("part span index"));
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() > MAX_PART_SPAN_INDEX_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("part span index"));
        }
        let mut cursor = FixedReader::with_magic(bytes, PART_SPAN_MAGIC)?;
        let version = cursor.u32()?;
        if version != PART_SPAN_INDEX_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let part_id = BootstrapPartId::from_digest(cursor.array_32()?);
        let declared_root = SourceSpanRootV1::decode(cursor.take(36)?)?;
        let count = cursor.u32()?;
        if count > MAX_SOURCE_SPANS_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::CountLimit("source spans"));
        }
        let mut spans = Vec::with_capacity(count as usize);
        for _ in 0..count {
            spans.push(SourceSpanV1::decode(cursor.sized()?)?);
        }
        cursor.finish()?;
        let value = Self::new(part_id, spans)?;
        if value.root != declared_root {
            return Err(BootstrapImportError::IndexRootMismatch);
        }
        if value.encode()?.as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    fn encoded_len(&self) -> Result<usize, BootstrapImportError> {
        self.spans.iter().try_fold(84_usize, |length, span| {
            length
                .checked_add(4)
                .and_then(|length| length.checked_add(span.encode().len()))
                .ok_or(BootstrapImportError::LengthOverflow)
        })
    }
}

/// A digest-only operation commitment plus the semantic bytes it consumes.
/// The operation bytes remain in the payload object set and are not decoded by
/// this pure validation packet.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OperationLeafV1 {
    operation_digest: OperationDigestV1,
    semantic_effect_bytes: u64,
}

impl OperationLeafV1 {
    pub(crate) fn new(
        operation_digest: OperationDigestV1,
        semantic_effect_bytes: u64,
    ) -> Result<Self, BootstrapImportError> {
        if semantic_effect_bytes > MAX_SEMANTIC_EFFECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("semantic effect"));
        }
        Ok(Self {
            operation_digest,
            semantic_effect_bytes,
        })
    }

    fn encode(&self) -> Vec<u8> {
        canonical_encode(
            b"tine/bootstrap-import/operation-leaf/v1\0",
            &[
                &OPERATION_ROOT_SCHEMA_VERSION.to_be_bytes(),
                self.operation_digest.as_bytes(),
                &self.semantic_effect_bytes.to_be_bytes(),
            ],
        )
    }
}

/// One full encoded object referenced by a bootstrap part.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct PayloadObjectDescriptorV1 {
    content_digest: ContentDigest,
    byte_length: u64,
}

/// Kinds in the full-object root.  Payload identity remains a separate root;
/// these tags make the post-evidence aggregate set domain explicit.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum FullObjectKindV1 {
    Payload = 1,
    PartEvidence = 2,
    ManifestDefined = 3,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FullObjectDescriptorV1 {
    kind: FullObjectKindV1,
    content_digest: [u8; 32],
    byte_length: u64,
}

impl FullObjectDescriptorV1 {
    pub(crate) fn manifest_defined(
        content_digest: [u8; 32],
        byte_length: u64,
    ) -> Result<Self, BootstrapImportError> {
        Self::new(
            FullObjectKindV1::ManifestDefined,
            content_digest,
            byte_length,
        )
    }

    fn payload(payload: &PayloadObjectDescriptorV1) -> Self {
        Self {
            kind: FullObjectKindV1::Payload,
            content_digest: *payload.content_digest.as_bytes(),
            byte_length: payload.byte_length,
        }
    }

    fn part_evidence(
        evidence_digest: BootstrapEvidenceDigestV1,
        byte_length: u64,
    ) -> Result<Self, BootstrapImportError> {
        Self::new(
            FullObjectKindV1::PartEvidence,
            *evidence_digest.as_bytes(),
            byte_length,
        )
    }

    fn new(
        kind: FullObjectKindV1,
        content_digest: [u8; 32],
        byte_length: u64,
    ) -> Result<Self, BootstrapImportError> {
        if byte_length == 0 || byte_length > MAX_FULL_OBJECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("full object"));
        }
        Ok(Self {
            kind,
            content_digest,
            byte_length,
        })
    }

    fn encode(&self) -> Vec<u8> {
        canonical_encode(
            b"tine/bootstrap-import/full-object/v1\0",
            &[
                &FULL_OBJECT_ROOT_SCHEMA_VERSION.to_be_bytes(),
                &[self.kind as u8],
                &self.content_digest,
                &self.byte_length.to_be_bytes(),
            ],
        )
    }
}

impl PayloadObjectDescriptorV1 {
    pub(crate) fn new(
        content_digest: ContentDigest,
        byte_length: u64,
    ) -> Result<Self, BootstrapImportError> {
        if byte_length == 0 || byte_length > MAX_BATCH_OBJECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("batch object"));
        }
        Ok(Self {
            content_digest,
            byte_length,
        })
    }

    fn encode(&self) -> Vec<u8> {
        canonical_encode(
            b"tine/bootstrap-import/payload-object/v1\0",
            &[
                &PAYLOAD_OBJECT_ROOT_SCHEMA_VERSION.to_be_bytes(),
                self.content_digest.as_bytes(),
                &self.byte_length.to_be_bytes(),
            ],
        )
    }

    pub(crate) const fn content_digest(self) -> ContentDigest {
        self.content_digest
    }

    pub(crate) const fn byte_length(self) -> u64 {
        self.byte_length
    }
}

struct RootHasherV1 {
    hasher: Sha256,
    count: u32,
}

impl RootHasherV1 {
    fn new(domain: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        Self { hasher, count: 0 }
    }

    fn push(
        &mut self,
        encoded: &[u8],
        limit: u32,
        label: &'static str,
    ) -> Result<(), BootstrapImportError> {
        if self.count >= limit {
            return Err(BootstrapImportError::CountLimit(label));
        }
        self.hasher.update((encoded.len() as u64).to_be_bytes());
        self.hasher.update(encoded);
        self.count += 1;
        Ok(())
    }

    fn finish(mut self) -> [u8; 32] {
        self.hasher.update(4_u64.to_be_bytes());
        self.hasher.update(self.count.to_be_bytes());
        self.hasher.finalize().into()
    }
}

pub(crate) struct SourceInventoryRootBuilderV1 {
    root: RootHasherV1,
    last: Option<SourceLeafV1>,
}

impl SourceInventoryRootBuilderV1 {
    pub(crate) fn new() -> Self {
        Self {
            root: RootHasherV1::new(b"tine/bootstrap-import/source-inventory-root/v1\0"),
            last: None,
        }
    }

    pub(crate) fn push(&mut self, leaf: &SourceLeafV1) -> Result<(), BootstrapImportError> {
        if let Some(last) = &self.last {
            if last.path == leaf.path {
                return if last == leaf {
                    Err(BootstrapImportError::DuplicateCanonicalItem)
                } else {
                    Err(BootstrapImportError::ConflictingProtocolIdentity(
                        "managed source path",
                    ))
                };
            }
            match last.canonical_cmp(leaf) {
                Ordering::Less => {}
                Ordering::Equal => return Err(BootstrapImportError::DuplicateCanonicalItem),
                Ordering::Greater => return Err(BootstrapImportError::NonCanonicalOrder),
            }
        }
        let encoded = leaf.encode();
        self.root.push(
            &encoded,
            MAX_SOURCE_INVENTORY_LEAVES,
            "source inventory leaves",
        )?;
        self.last = Some(leaf.clone());
        Ok(())
    }

    pub(crate) fn finish(self) -> SourceInventoryRootV1 {
        let count = self.root.count;
        SourceInventoryRootV1 {
            digest: self.root.finish(),
            source_count: count,
        }
    }
}

pub(crate) struct SourceBlobChunkRootBuilderV1 {
    root: RootHasherV1,
    last: Option<SourceBlobChunkDescriptorV1>,
    total_bytes: u64,
    current_source: Option<(SourceLeafDigestV1, u64)>,
    last_source: Option<SourceLeafDigestV1>,
}

impl SourceBlobChunkRootBuilderV1 {
    /// Sources are supplied in source-leaf-digest order. Each source is
    /// finished before the next starts, so validation retains at most one
    /// source leaf and one chunk descriptor.
    pub(crate) fn new() -> Self {
        Self {
            root: RootHasherV1::new(b"tine/bootstrap-import/source-blob-root/v1\0"),
            last: None,
            total_bytes: 0,
            current_source: None,
            last_source: None,
        }
    }

    pub(crate) fn begin_source(
        &mut self,
        source: &SourceLeafV1,
    ) -> Result<(), BootstrapImportError> {
        self.finish_source()?;
        let digest = source.digest();
        if self.last_source.is_some_and(|last| digest <= last) {
            return if self.last_source == Some(digest) {
                Err(BootstrapImportError::ConflictingProtocolIdentity(
                    "source leaf digest",
                ))
            } else {
                Err(BootstrapImportError::NonCanonicalOrder)
            };
        }
        self.current_source = Some((digest, source.byte_length));
        self.last_source = Some(digest);
        self.last = None;
        Ok(())
    }

    pub(crate) fn push(
        &mut self,
        descriptor: SourceBlobChunkDescriptorV1,
    ) -> Result<(), BootstrapImportError> {
        let Some((expected_source, _)) = self.current_source else {
            return Err(BootstrapImportError::BlobContinuityMismatch);
        };
        if descriptor.source_leaf != expected_source {
            return Err(BootstrapImportError::BlobContinuityMismatch);
        }
        if let Some(last) = self.last {
            if descriptor == last {
                return Err(BootstrapImportError::DuplicateCanonicalItem);
            }
            if descriptor < last {
                return Err(BootstrapImportError::NonCanonicalOrder);
            }
            if descriptor.ordinal == last.ordinal {
                return Err(BootstrapImportError::ConflictingProtocolIdentity(
                    "source blob chunk ordinal",
                ));
            }
            if descriptor.count != last.count {
                return Err(BootstrapImportError::ConflictingProtocolIdentity(
                    "source blob chunk count",
                ));
            }
            let expected_ordinal = last
                .ordinal
                .checked_add(1)
                .ok_or(BootstrapImportError::LengthOverflow)?;
            let expected_offset = last
                .offset
                .checked_add(u64::from(last.byte_length))
                .ok_or(BootstrapImportError::LengthOverflow)?;
            if descriptor.ordinal != expected_ordinal || descriptor.offset != expected_offset {
                return Err(BootstrapImportError::BlobContinuityMismatch);
            }
        } else {
            self.validate_first_descriptor(descriptor)?;
        }
        let encoded = descriptor.encode();
        self.root
            .push(&encoded, MAX_SOURCE_BLOB_CHUNKS, "source blob chunks")?;
        self.total_bytes = self
            .total_bytes
            .checked_add(u64::from(descriptor.byte_length))
            .ok_or(BootstrapImportError::LengthOverflow)?;
        self.last = Some(descriptor);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<SourceBlobChunkRootV1, BootstrapImportError> {
        self.finish_source()?;
        if self.total_bytes > MAX_TOTAL_SOURCE_BYTES {
            return Err(BootstrapImportError::ByteLimit("total source bytes"));
        }
        let count = self.root.count;
        Ok(SourceBlobChunkRootV1 {
            digest: self.root.finish(),
            chunk_count: count,
            total_bytes: self.total_bytes,
        })
    }

    fn validate_first_descriptor(
        &self,
        descriptor: SourceBlobChunkDescriptorV1,
    ) -> Result<(), BootstrapImportError> {
        let Some((source_leaf, _)) = self.current_source else {
            return Err(BootstrapImportError::BlobContinuityMismatch);
        };
        if descriptor.source_leaf != source_leaf
            || descriptor.ordinal != 0
            || descriptor.offset != 0
        {
            return Err(BootstrapImportError::BlobContinuityMismatch);
        }
        Ok(())
    }

    fn finish_source(&mut self) -> Result<(), BootstrapImportError> {
        let Some((source_leaf, byte_length)) = self.current_source.take() else {
            return Ok(());
        };
        match self.last.take() {
            None if byte_length == 0 => Ok(()),
            Some(last) => {
                let received = last
                    .ordinal
                    .checked_add(1)
                    .ok_or(BootstrapImportError::LengthOverflow)?;
                let terminal_offset = last
                    .offset
                    .checked_add(u64::from(last.byte_length))
                    .ok_or(BootstrapImportError::LengthOverflow)?;
                if last.source_leaf != source_leaf
                    || received != last.count
                    || terminal_offset != byte_length
                {
                    return Err(BootstrapImportError::BlobContinuityMismatch);
                }
                Ok(())
            }
            None => Err(BootstrapImportError::BlobContinuityMismatch),
        }
    }
}

pub(crate) struct SourceSpanRootBuilderV1 {
    root: RootHasherV1,
    last: Option<SourceSpanV1>,
}

impl SourceSpanRootBuilderV1 {
    pub(crate) fn new() -> Self {
        Self {
            root: RootHasherV1::new(b"tine/bootstrap-import/source-span-root/v1\0"),
            last: None,
        }
    }

    pub(crate) fn push(&mut self, span: SourceSpanV1) -> Result<(), BootstrapImportError> {
        if let Some(last) = self.last {
            if span == last {
                return Err(BootstrapImportError::DuplicateCanonicalItem);
            }
            if span < last {
                return Err(BootstrapImportError::NonCanonicalOrder);
            }
        }
        let encoded = span.encode();
        self.root.push(
            &encoded,
            MAX_SOURCE_SPANS_PER_BOOTSTRAP_PART,
            "source spans",
        )?;
        self.last = Some(span);
        Ok(())
    }

    pub(crate) fn finish(self) -> SourceSpanRootV1 {
        let count = self.root.count;
        SourceSpanRootV1 {
            digest: self.root.finish(),
            span_count: count,
        }
    }
}

pub(crate) struct OperationRootBuilderV1 {
    root: RootHasherV1,
    last: Option<OperationLeafV1>,
    semantic_effect_bytes: u64,
}

impl OperationRootBuilderV1 {
    pub(crate) fn new() -> Self {
        Self {
            root: RootHasherV1::new(b"tine/bootstrap-import/operation-root/v1\0"),
            last: None,
            semantic_effect_bytes: 0,
        }
    }

    pub(crate) fn push(&mut self, operation: OperationLeafV1) -> Result<(), BootstrapImportError> {
        if let Some(last) = self.last {
            if operation.operation_digest == last.operation_digest {
                return if operation == last {
                    Err(BootstrapImportError::DuplicateCanonicalItem)
                } else {
                    Err(BootstrapImportError::ConflictingProtocolIdentity(
                        "operation digest",
                    ))
                };
            }
            if operation == last {
                return Err(BootstrapImportError::DuplicateCanonicalItem);
            }
            if operation < last {
                return Err(BootstrapImportError::NonCanonicalOrder);
            }
        }
        self.semantic_effect_bytes = self
            .semantic_effect_bytes
            .checked_add(operation.semantic_effect_bytes)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if self.semantic_effect_bytes > MAX_SEMANTIC_EFFECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("semantic effect"));
        }
        let encoded = operation.encode();
        self.root
            .push(&encoded, MAX_OPERATIONS_PER_BOOTSTRAP_PART, "operations")?;
        self.last = Some(operation);
        Ok(())
    }

    pub(crate) fn finish(self) -> OperationRootV1 {
        let count = self.root.count;
        OperationRootV1 {
            digest: self.root.finish(),
            operation_count: count,
            semantic_effect_bytes: self.semantic_effect_bytes,
        }
    }
}

pub(crate) struct PayloadObjectRootBuilderV1 {
    root: RootHasherV1,
    last: Option<PayloadObjectDescriptorV1>,
    total_bytes: u64,
}

pub(crate) struct FullObjectRootBuilderV1 {
    root: RootHasherV1,
    last: Option<FullObjectDescriptorV1>,
    total_bytes: u64,
}

impl FullObjectRootBuilderV1 {
    pub(crate) fn new() -> Self {
        Self {
            root: RootHasherV1::new(b"tine/bootstrap-import/full-object-root/v1\0"),
            last: None,
            total_bytes: 0,
        }
    }

    pub(crate) fn push(
        &mut self,
        object: FullObjectDescriptorV1,
    ) -> Result<(), BootstrapImportError> {
        if let Some(last) = self.last {
            if object.content_digest == last.content_digest {
                return if object == last {
                    Err(BootstrapImportError::DuplicateCanonicalItem)
                } else {
                    Err(BootstrapImportError::ConflictingProtocolIdentity(
                        "full object digest",
                    ))
                };
            }
            if object == last {
                return Err(BootstrapImportError::DuplicateCanonicalItem);
            }
            if object < last {
                return Err(BootstrapImportError::NonCanonicalOrder);
            }
        }
        self.total_bytes = self
            .total_bytes
            .checked_add(object.byte_length)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if self.total_bytes > MAX_FULL_OBJECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("full object"));
        }
        self.root.push(
            &object.encode(),
            MAX_FULL_OBJECTS_PER_BOOTSTRAP_PART,
            "full objects",
        )?;
        self.last = Some(object);
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<FullObjectRootV1, BootstrapImportError> {
        let count = self.root.count;
        Ok(FullObjectRootV1 {
            digest: self.root.finish(),
            object_count: count,
            total_bytes: self.total_bytes,
        })
    }
}

impl PayloadObjectRootBuilderV1 {
    pub(crate) fn new() -> Self {
        Self {
            root: RootHasherV1::new(b"tine/bootstrap-import/payload-object-root/v1\0"),
            last: None,
            total_bytes: 0,
        }
    }

    pub(crate) fn push(
        &mut self,
        object: PayloadObjectDescriptorV1,
    ) -> Result<(), BootstrapImportError> {
        if let Some(last) = self.last {
            if object.content_digest == last.content_digest {
                return if object == last {
                    Err(BootstrapImportError::DuplicateCanonicalItem)
                } else {
                    Err(BootstrapImportError::ConflictingProtocolIdentity(
                        "payload object digest",
                    ))
                };
            }
            if object == last {
                return Err(BootstrapImportError::DuplicateCanonicalItem);
            }
            if object < last {
                return Err(BootstrapImportError::NonCanonicalOrder);
            }
        }
        self.total_bytes = self
            .total_bytes
            .checked_add(object.byte_length)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if self.total_bytes > MAX_BATCH_OBJECT_BYTES_PER_BOOTSTRAP_PART {
            return Err(BootstrapImportError::ByteLimit("batch object"));
        }
        let encoded = object.encode();
        self.root.push(
            &encoded,
            MAX_OPERATIONS_PER_BOOTSTRAP_PART,
            "payload objects",
        )?;
        self.last = Some(object);
        Ok(())
    }

    pub(crate) fn finish(self) -> PayloadObjectRootV1 {
        let count = self.root.count;
        PayloadObjectRootV1 {
            digest: self.root.finish(),
            object_count: count,
            total_bytes: self.total_bytes,
        }
    }
}

/// The cycle-free evidence carried by one candidate part.  It intentionally
/// omits its own digest, batch-manifest fingerprint, and full descriptor root.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct BootstrapImportPartEvidenceV1 {
    import_id: ImportId,
    profile_digest: BootstrapProfileDigestV1,
    ordinal: u32,
    part_count: u32,
    source_span_root: SourceSpanRootV1,
    operation_root: OperationRootV1,
    payload_object_root: PayloadObjectRootV1,
    predecessor: Option<BootstrapPartId>,
}

impl BootstrapImportPartEvidenceV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        import_id: ImportId,
        profile_digest: BootstrapProfileDigestV1,
        ordinal: u32,
        part_count: u32,
        source_span_root: SourceSpanRootV1,
        operation_root: OperationRootV1,
        payload_object_root: PayloadObjectRootV1,
        predecessor: Option<BootstrapPartId>,
    ) -> Result<Self, BootstrapImportError> {
        let value = Self {
            import_id,
            profile_digest,
            ordinal,
            part_count,
            source_span_root,
            operation_root,
            payload_object_root,
            predecessor,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, BootstrapImportError> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(320);
        bytes.extend_from_slice(EVIDENCE_MAGIC);
        put_field(
            &mut bytes,
            1,
            &BOOTSTRAP_IMPORT_SCHEMA_VERSION.to_be_bytes(),
        )?;
        put_field(&mut bytes, 2, self.import_id.as_bytes())?;
        put_field(&mut bytes, 3, self.profile_digest.as_bytes())?;
        put_field(&mut bytes, 4, &self.ordinal.to_be_bytes())?;
        put_field(&mut bytes, 5, &self.part_count.to_be_bytes())?;
        put_field(&mut bytes, 6, &root_bytes(self.source_span_root))?;
        put_field(&mut bytes, 7, &root_bytes(self.operation_root))?;
        put_field(&mut bytes, 8, &root_bytes(self.payload_object_root))?;
        put_field(&mut bytes, 9, &part_id_option_bytes(self.predecessor))?;
        if bytes.len() > MAX_BOOTSTRAP_PART_EVIDENCE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("part evidence"));
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() > MAX_BOOTSTRAP_PART_EVIDENCE_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("part evidence"));
        }
        let fields = CanonicalFieldsV1::parse(bytes, EVIDENCE_MAGIC, 9, 1)?;
        let version = read_u32(fields.required(1)?)?;
        if version != BOOTSTRAP_IMPORT_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let value = Self::new(
            ImportId::from_digest(array_32(fields.required(2)?)?),
            BootstrapProfileDigestV1::from_bytes(array_32(fields.required(3)?)?),
            read_u32(fields.required(4)?)?,
            read_u32(fields.required(5)?)?,
            SourceSpanRootV1::decode(fields.required(6)?)?,
            OperationRootV1::decode(fields.required(7)?)?,
            PayloadObjectRootV1::decode(fields.required(8)?)?,
            decode_part_id_option(fields.required(9)?)?,
        )?;
        if value.encode()?.as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    pub(crate) fn part_id(&self) -> BootstrapPartId {
        BootstrapPartId::derive(
            self.import_id,
            self.profile_digest.as_bytes(),
            self.ordinal,
            self.source_span_root.digest(),
            self.operation_root.digest(),
        )
    }

    pub(crate) fn batch_id(&self) -> BatchId {
        BatchId::for_bootstrap_part(self.part_id())
    }

    pub(crate) fn evidence_digest(&self) -> BootstrapEvidenceDigestV1 {
        let bytes = self
            .encode()
            .expect("validated fixed-size bootstrap evidence encodes");
        BootstrapEvidenceDigestV1::digest(
            b"tine/bootstrap-import/part-evidence-digest/v1\0",
            &[&bytes],
        )
    }

    pub(crate) const fn import_id(self) -> ImportId {
        self.import_id
    }

    pub(crate) const fn profile_digest(self) -> BootstrapProfileDigestV1 {
        self.profile_digest
    }

    pub(crate) const fn ordinal(self) -> u32 {
        self.ordinal
    }

    pub(crate) const fn part_count(self) -> u32 {
        self.part_count
    }

    pub(crate) const fn source_span_root(self) -> SourceSpanRootV1 {
        self.source_span_root
    }

    pub(crate) const fn operation_root(self) -> OperationRootV1 {
        self.operation_root
    }

    pub(crate) const fn payload_object_root(self) -> PayloadObjectRootV1 {
        self.payload_object_root
    }

    pub(crate) const fn predecessor(self) -> Option<BootstrapPartId> {
        self.predecessor
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        BootstrapPartitionProfileV1::validate_digest(self.profile_digest)?;
        if self.part_count == 0 || self.part_count > MAX_BOOTSTRAP_PARTS {
            return Err(BootstrapImportError::InvalidPartCount);
        }
        if self.ordinal >= self.part_count {
            return Err(BootstrapImportError::InvalidOrdinal);
        }
        match (self.ordinal, self.part_count, self.predecessor) {
            (0, _, None) => {}
            (0, _, Some(_)) => return Err(BootstrapImportError::UnexpectedPredecessor),
            (_, 1, Some(_)) => return Err(BootstrapImportError::UnexpectedPredecessor),
            (_, _, None) => return Err(BootstrapImportError::MissingPredecessor),
            _ => {}
        }
        self.source_span_root.validate()?;
        self.operation_root.validate()?;
        self.payload_object_root.validate()?;
        if self.operation_root.operation_count == 0 {
            return Err(BootstrapImportError::EmptyOperationTransaction);
        }
        Ok(())
    }
}

/// An archive-local frontier commitment.  It has no dependency on
/// `AcceptedFrontierRoot` or any engine type, and therefore cannot accidentally
/// become a serialization of a live acceptance structure.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ArchiveLocalFrontierBindingV1 {
    digest: [u8; 32],
    accepted_count: u32,
    last_part: Option<BootstrapPartId>,
}

impl ArchiveLocalFrontierBindingV1 {
    pub(crate) fn initial(import_id: ImportId, profile_digest: BootstrapProfileDigestV1) -> Self {
        Self {
            digest: canonical_digest(
                b"tine/bootstrap-import/archive-frontier-initial/v1\0",
                &[import_id.as_bytes(), profile_digest.as_bytes()],
            ),
            accepted_count: 0,
            last_part: None,
        }
    }

    pub(crate) fn advance(
        self,
        part_id: BootstrapPartId,
        accepted_event: BootstrapAcceptedEventBindingV1,
    ) -> Result<Self, BootstrapImportError> {
        let accepted_count = self
            .accepted_count
            .checked_add(1)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if accepted_count > MAX_BOOTSTRAP_PARTS {
            return Err(BootstrapImportError::CountLimit("bootstrap parts"));
        }
        Ok(Self {
            digest: canonical_digest(
                b"tine/bootstrap-import/archive-frontier-step/v1\0",
                &[
                    &BOOTSTRAP_FRONTIER_SCHEMA_VERSION.to_be_bytes(),
                    &self.digest,
                    &accepted_count.to_be_bytes(),
                    part_id.as_bytes(),
                    accepted_event.as_bytes(),
                ],
            ),
            accepted_count,
            last_part: Some(part_id),
        })
    }

    pub(crate) const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) const fn accepted_count(self) -> u32 {
        self.accepted_count
    }

    pub(crate) const fn last_part(self) -> Option<BootstrapPartId> {
        self.last_part
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        archive_frontier_bytes(self)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        decode_archive_frontier(bytes)
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        if self.accepted_count > MAX_BOOTSTRAP_PARTS {
            return Err(BootstrapImportError::CountLimit("bootstrap parts"));
        }
        if (self.accepted_count == 0) != self.last_part.is_none() {
            return Err(BootstrapImportError::InvalidArchiveFrontier);
        }
        Ok(())
    }
}

/// The descriptor carried in the aggregate manifest for one accepted part.
/// Its evidence is reconstructed from the fixed fields below; the evidence
/// digest is therefore checkable without fetching a separate object.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct BootstrapPartDescriptorV1 {
    evidence: BootstrapImportPartEvidenceV1,
    part_id: BootstrapPartId,
    batch_id: BatchId,
    evidence_digest: BootstrapEvidenceDigestV1,
    manifest_fingerprint: BootstrapManifestFingerprintV1,
    full_object_root: FullObjectRootV1,
    accepted_event: BootstrapAcceptedEventBindingV1,
    acceptance_sequence: u32,
    prior_frontier: ArchiveLocalFrontierBindingV1,
    post_frontier: ArchiveLocalFrontierBindingV1,
}

impl BootstrapPartDescriptorV1 {
    /// Build the next archive-local accepted descriptor without consulting an
    /// engine.  The aggregate validator still verifies ordinal and predecessor
    /// continuity once every descriptor is present.
    pub(crate) fn accepted(
        evidence: BootstrapImportPartEvidenceV1,
        manifest_fingerprint: BootstrapManifestFingerprintV1,
        payload_objects: &[PayloadObjectDescriptorV1],
        manifest_objects: &[FullObjectDescriptorV1],
        prior_frontier: ArchiveLocalFrontierBindingV1,
    ) -> Result<Self, BootstrapImportError> {
        let acceptance_sequence = prior_frontier
            .accepted_count
            .checked_add(1)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        let part_id = evidence.part_id();
        let evidence_digest = evidence.evidence_digest();
        let full_object_root =
            FullObjectRootV1::for_part(evidence, payload_objects, manifest_objects)?;
        let accepted_event = accepted_event_binding(
            part_id,
            evidence_digest,
            manifest_fingerprint,
            full_object_root,
            acceptance_sequence,
        );
        let post_frontier = prior_frontier.advance(part_id, accepted_event)?;
        Self::new(
            evidence,
            manifest_fingerprint,
            full_object_root,
            acceptance_sequence,
            prior_frontier,
            post_frontier,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        evidence: BootstrapImportPartEvidenceV1,
        manifest_fingerprint: BootstrapManifestFingerprintV1,
        full_object_root: FullObjectRootV1,
        acceptance_sequence: u32,
        prior_frontier: ArchiveLocalFrontierBindingV1,
        post_frontier: ArchiveLocalFrontierBindingV1,
    ) -> Result<Self, BootstrapImportError> {
        let part_id = evidence.part_id();
        let evidence_digest = evidence.evidence_digest();
        let accepted_event = accepted_event_binding(
            part_id,
            evidence_digest,
            manifest_fingerprint,
            full_object_root,
            acceptance_sequence,
        );
        let value = Self {
            evidence,
            part_id,
            batch_id: evidence.batch_id(),
            evidence_digest,
            manifest_fingerprint,
            full_object_root,
            accepted_event,
            acceptance_sequence,
            prior_frontier,
            post_frontier,
        };
        value.validate_self()?;
        Ok(value)
    }

    pub(crate) const fn evidence(self) -> BootstrapImportPartEvidenceV1 {
        self.evidence
    }

    pub(crate) const fn part_id(self) -> BootstrapPartId {
        self.part_id
    }

    pub(crate) const fn batch_id(self) -> BatchId {
        self.batch_id
    }

    pub(crate) const fn acceptance_sequence(self) -> u32 {
        self.acceptance_sequence
    }

    pub(crate) const fn post_frontier(self) -> ArchiveLocalFrontierBindingV1 {
        self.post_frontier
    }

    /// Verify the directly loaded replay artifacts against this aggregate-bound
    /// part descriptor without exposing either committed value or its canonical
    /// full-object encoding.
    pub(crate) fn validate_loaded_artifacts(
        self,
        manifest_fingerprint: BootstrapManifestFingerprintV1,
        payload_objects: &[PayloadObjectDescriptorV1],
        manifest_objects: &[FullObjectDescriptorV1],
    ) -> Result<(), BootstrapImportError> {
        self.validate_self()?;
        if manifest_fingerprint != self.manifest_fingerprint {
            return Err(BootstrapImportError::FullObjectRootMismatch);
        }
        if FullObjectRootV1::for_part(self.evidence, payload_objects, manifest_objects)?
            != self.full_object_root
        {
            return Err(BootstrapImportError::FullObjectRootMismatch);
        }
        Ok(())
    }

    fn validate_self(self) -> Result<(), BootstrapImportError> {
        self.evidence.validate()?;
        if self.part_id != self.evidence.part_id() {
            return Err(BootstrapImportError::PartIdMismatch);
        }
        if self.batch_id != self.evidence.batch_id() {
            return Err(BootstrapImportError::BatchIdMismatch);
        }
        if self.evidence_digest != self.evidence.evidence_digest() {
            return Err(BootstrapImportError::EvidenceDigestMismatch);
        }
        self.full_object_root.validate()?;
        if self.accepted_event
            != accepted_event_binding(
                self.part_id,
                self.evidence_digest,
                self.manifest_fingerprint,
                self.full_object_root,
                self.acceptance_sequence,
            )
        {
            return Err(BootstrapImportError::AcceptedEventBindingMismatch);
        }
        self.prior_frontier.validate()?;
        self.post_frontier.validate()?;
        Ok(())
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PART_DESCRIPTOR_BYTES);
        bytes.extend_from_slice(self.part_id.as_bytes());
        bytes.extend_from_slice(self.batch_id.as_uuid().as_bytes());
        bytes.extend_from_slice(&self.evidence.ordinal().to_be_bytes());
        bytes.extend_from_slice(&self.evidence.part_count().to_be_bytes());
        self.evidence.source_span_root().encode_into(&mut bytes);
        self.evidence.operation_root().encode_into(&mut bytes);
        self.evidence.payload_object_root().encode_into(&mut bytes);
        bytes.extend_from_slice(&part_id_option_bytes(self.evidence.predecessor()));
        bytes.extend_from_slice(self.evidence_digest.as_bytes());
        bytes.extend_from_slice(self.manifest_fingerprint.as_bytes());
        self.full_object_root.encode_into(&mut bytes);
        bytes.extend_from_slice(self.accepted_event.as_bytes());
        bytes.extend_from_slice(&self.acceptance_sequence.to_be_bytes());
        bytes.extend_from_slice(&self.prior_frontier.encode());
        bytes.extend_from_slice(&self.post_frontier.encode());
        debug_assert_eq!(bytes.len(), PART_DESCRIPTOR_BYTES);
        bytes
    }

    fn decode(
        bytes: &[u8],
        import_id: ImportId,
        profile_digest: BootstrapProfileDigestV1,
    ) -> Result<Self, BootstrapImportError> {
        if bytes.len() != PART_DESCRIPTOR_BYTES {
            return Err(BootstrapImportError::InvalidFieldLength);
        }
        let mut cursor = FixedReader::new(bytes);
        let part_id = BootstrapPartId::from_digest(cursor.array_32()?);
        let batch_id = BatchId::from_uuid(Uuid::from_bytes(cursor.array_16()?));
        let ordinal = cursor.u32()?;
        let part_count = cursor.u32()?;
        let source_span_root = SourceSpanRootV1::decode(cursor.take(36)?)?;
        let operation_root = OperationRootV1::decode(cursor.take(44)?)?;
        let payload_object_root = PayloadObjectRootV1::decode(cursor.take(44)?)?;
        let predecessor = decode_part_id_option(cursor.take(33)?)?;
        let evidence_digest = BootstrapEvidenceDigestV1::from_bytes(cursor.array_32()?);
        let manifest_fingerprint = BootstrapManifestFingerprintV1::from_bytes(cursor.array_32()?);
        let full_object_root = FullObjectRootV1::decode(cursor.take(44)?)?;
        let accepted_event = BootstrapAcceptedEventBindingV1::from_bytes(cursor.array_32()?);
        let acceptance_sequence = cursor.u32()?;
        let prior_frontier = ArchiveLocalFrontierBindingV1::decode(cursor.take(69)?)?;
        let post_frontier = ArchiveLocalFrontierBindingV1::decode(cursor.take(69)?)?;
        cursor.finish()?;
        let evidence = BootstrapImportPartEvidenceV1::new(
            import_id,
            profile_digest,
            ordinal,
            part_count,
            source_span_root,
            operation_root,
            payload_object_root,
            predecessor,
        )?;
        let value = Self {
            evidence,
            part_id,
            batch_id,
            evidence_digest,
            manifest_fingerprint,
            full_object_root,
            accepted_event,
            acceptance_sequence,
            prior_frontier,
            post_frontier,
        };
        value.validate_self()?;
        if value.encode().as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }
}

/// A complete portable aggregate. `graph_resource` is source provenance only:
/// publication identity and final-frontier authority never use or compare it
/// with a receiving graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootstrapAggregateManifestV1 {
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    graph_resource: CanonicalGraphResourceId,
    import_id: ImportId,
    complete_source_count: u32,
    source_inventory_root: SourceInventoryRootV1,
    source_inventory_page_count: u32,
    source_blob_root: SourceBlobChunkRootV1,
    source_blob_page_count: u32,
    profile_digest: BootstrapProfileDigestV1,
    parts: Vec<BootstrapPartDescriptorV1>,
    initial_frontier: ArchiveLocalFrontierBindingV1,
    final_frontier: ArchiveLocalFrontierBindingV1,
    final_frontier_proof: BootstrapFinalFrontierProofV1,
}

impl BootstrapAggregateManifestV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        graph_resource: CanonicalGraphResourceId,
        import_id: ImportId,
        complete_source_count: u32,
        source_inventory_root: SourceInventoryRootV1,
        source_inventory_page_count: u32,
        source_blob_root: SourceBlobChunkRootV1,
        source_blob_page_count: u32,
        profile_digest: BootstrapProfileDigestV1,
        parts: Vec<BootstrapPartDescriptorV1>,
        initial_frontier: ArchiveLocalFrontierBindingV1,
        final_frontier: ArchiveLocalFrontierBindingV1,
        final_frontier_proof: BootstrapFinalFrontierProofV1,
    ) -> Result<Self, BootstrapImportError> {
        checked_count(parts.len(), MAX_BOOTSTRAP_PARTS, "bootstrap parts")?;
        let value = Self {
            workspace_id,
            lineage_digest,
            graph_resource,
            import_id,
            complete_source_count,
            source_inventory_root,
            source_inventory_page_count,
            source_blob_root,
            source_blob_page_count,
            profile_digest,
            parts,
            initial_frontier,
            final_frontier,
            final_frontier_proof,
        };
        value.validate()?;
        Ok(value)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_for_import(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        graph_resource: CanonicalGraphResourceId,
        import_id: ImportId,
        complete_source_count: u32,
        source_inventory_root: SourceInventoryRootV1,
        source_inventory_page_count: u32,
        source_blob_root: SourceBlobChunkRootV1,
        source_blob_page_count: u32,
        profile_digest: BootstrapProfileDigestV1,
        parts: Vec<BootstrapPartDescriptorV1>,
        initial_frontier: ArchiveLocalFrontierBindingV1,
        final_frontier: ArchiveLocalFrontierBindingV1,
    ) -> Result<Self, BootstrapImportError> {
        let final_frontier_proof = final_frontier_proof(
            workspace_id,
            lineage_digest,
            import_id,
            profile_digest,
            final_frontier,
        );
        Self::new(
            workspace_id,
            lineage_digest,
            graph_resource,
            import_id,
            complete_source_count,
            source_inventory_root,
            source_inventory_page_count,
            source_blob_root,
            source_blob_page_count,
            profile_digest,
            parts,
            initial_frontier,
            final_frontier,
            final_frontier_proof,
        )
    }

    pub(crate) fn empty(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        graph_resource: CanonicalGraphResourceId,
        import_id: ImportId,
    ) -> Result<Self, BootstrapImportError> {
        let profile_digest = BootstrapPartitionProfileV1::v1().digest();
        let frontier = ArchiveLocalFrontierBindingV1::initial(import_id, profile_digest);
        Self::new_for_import(
            workspace_id,
            lineage_digest,
            graph_resource,
            import_id,
            0,
            SourceInventoryRootV1::empty(),
            0,
            SourceBlobChunkRootV1::empty(),
            0,
            profile_digest,
            Vec::new(),
            frontier,
            frontier,
        )
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, BootstrapImportError> {
        self.validate()?;
        let parts = self.encode_parts()?;
        let mut bytes = Vec::with_capacity(512 + parts.len());
        bytes.extend_from_slice(MANIFEST_MAGIC);
        put_field(
            &mut bytes,
            1,
            &BOOTSTRAP_IMPORT_SCHEMA_VERSION.to_be_bytes(),
        )?;
        put_field(&mut bytes, 2, self.workspace_id.as_uuid().as_bytes())?;
        put_field(&mut bytes, 3, self.lineage_digest.as_bytes())?;
        put_field(&mut bytes, 4, self.graph_resource.as_bytes())?;
        put_field(&mut bytes, 5, self.import_id.as_bytes())?;
        put_field(&mut bytes, 6, &self.complete_source_count.to_be_bytes())?;
        put_field(&mut bytes, 7, &root_bytes(self.source_inventory_root))?;
        put_field(
            &mut bytes,
            8,
            &self.source_inventory_page_count.to_be_bytes(),
        )?;
        put_field(&mut bytes, 9, &root_bytes(self.source_blob_root))?;
        put_field(&mut bytes, 10, &self.source_blob_page_count.to_be_bytes())?;
        put_field(&mut bytes, 11, self.profile_digest.as_bytes())?;
        put_field(&mut bytes, 12, &parts)?;
        put_field(&mut bytes, 13, &self.initial_frontier.encode())?;
        put_field(&mut bytes, 14, &self.final_frontier.encode())?;
        put_field(&mut bytes, 15, self.final_frontier_proof.as_bytes())?;
        if bytes.len() > MAX_BOOTSTRAP_AGGREGATE_MANIFEST_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("aggregate manifest"));
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() > MAX_BOOTSTRAP_AGGREGATE_MANIFEST_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("aggregate manifest"));
        }
        let fields = CanonicalFieldsV1::parse(bytes, MANIFEST_MAGIC, 15, 1)?;
        let version = read_u32(fields.required(1)?)?;
        if version != BOOTSTRAP_IMPORT_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let workspace_id = WorkspaceId::from_uuid(Uuid::from_bytes(array_16(fields.required(2)?)?));
        let lineage_digest = LineageDigest::from_bytes(array_32(fields.required(3)?)?);
        let graph_resource = CanonicalGraphResourceId::from_bytes(array_32(fields.required(4)?)?);
        let import_id = ImportId::from_digest(array_32(fields.required(5)?)?);
        let complete_source_count = read_u32(fields.required(6)?)?;
        let source_inventory_root = SourceInventoryRootV1::decode(fields.required(7)?)?;
        let source_inventory_page_count = read_u32(fields.required(8)?)?;
        let source_blob_root = SourceBlobChunkRootV1::decode(fields.required(9)?)?;
        let source_blob_page_count = read_u32(fields.required(10)?)?;
        let profile_digest = BootstrapProfileDigestV1::from_bytes(array_32(fields.required(11)?)?);
        let parts = decode_parts(fields.required(12)?, import_id, profile_digest)?;
        let initial_frontier = ArchiveLocalFrontierBindingV1::decode(fields.required(13)?)?;
        let final_frontier = ArchiveLocalFrontierBindingV1::decode(fields.required(14)?)?;
        let final_frontier_proof =
            BootstrapFinalFrontierProofV1::from_bytes(array_32(fields.required(15)?)?);
        let value = Self::new(
            workspace_id,
            lineage_digest,
            graph_resource,
            import_id,
            complete_source_count,
            source_inventory_root,
            source_inventory_page_count,
            source_blob_root,
            source_blob_page_count,
            profile_digest,
            parts,
            initial_frontier,
            final_frontier,
            final_frontier_proof,
        )?;
        if value.encode()?.as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    pub(crate) fn aggregate_digest(&self) -> BootstrapAggregateDigestV1 {
        let bytes = self
            .encode()
            .expect("validated bounded aggregate manifest encodes");
        BootstrapAggregateDigestV1::digest(
            b"tine/bootstrap-import/aggregate-digest/v1\0",
            &[&bytes],
        )
    }

    pub(crate) fn publication_id(&self) -> BootstrapPublicationIdV1 {
        BootstrapPublicationIdV1::derive(
            self.workspace_id,
            self.lineage_digest,
            self.import_id,
            self.profile_digest,
            self.source_inventory_root,
            self.source_blob_root,
        )
    }

    pub(crate) fn parts(&self) -> &[BootstrapPartDescriptorV1] {
        &self.parts
    }

    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn lineage_digest(&self) -> LineageDigest {
        self.lineage_digest
    }

    pub(crate) const fn graph_resource(&self) -> CanonicalGraphResourceId {
        self.graph_resource
    }

    pub(crate) const fn import_id(&self) -> ImportId {
        self.import_id
    }

    pub(crate) const fn profile_digest(&self) -> BootstrapProfileDigestV1 {
        self.profile_digest
    }

    pub(crate) const fn source_inventory_root(&self) -> SourceInventoryRootV1 {
        self.source_inventory_root
    }

    pub(crate) const fn source_inventory_page_count(&self) -> u32 {
        self.source_inventory_page_count
    }

    pub(crate) const fn source_blob_root(&self) -> SourceBlobChunkRootV1 {
        self.source_blob_root
    }

    pub(crate) const fn source_blob_page_count(&self) -> u32 {
        self.source_blob_page_count
    }

    pub(crate) fn final_frontier(&self) -> ArchiveLocalFrontierBindingV1 {
        self.final_frontier
    }

    fn encode_parts(&self) -> Result<Vec<u8>, BootstrapImportError> {
        checked_count(self.parts.len(), MAX_BOOTSTRAP_PARTS, "bootstrap parts")?;
        let mut bytes = Vec::with_capacity(4 + self.parts.len() * PART_DESCRIPTOR_BYTES);
        bytes.extend_from_slice(&(self.parts.len() as u32).to_be_bytes());
        for part in &self.parts {
            bytes.extend_from_slice(&part.encode());
        }
        Ok(bytes)
    }

    fn validate(&self) -> Result<(), BootstrapImportError> {
        BootstrapPartitionProfileV1::validate_digest(self.profile_digest)?;
        self.source_inventory_root.validate()?;
        self.source_blob_root.validate()?;
        if self.complete_source_count != self.source_inventory_root.source_count() {
            return Err(BootstrapImportError::SourceCountMismatch);
        }
        validate_page_count(
            self.source_inventory_page_count,
            self.source_inventory_root.source_count(),
            "source inventory index pages",
        )?;
        validate_page_count(
            self.source_blob_page_count,
            self.source_blob_root.chunk_count(),
            "source blob index pages",
        )?;
        checked_count(self.parts.len(), MAX_BOOTSTRAP_PARTS, "bootstrap parts")?;
        self.initial_frontier.validate()?;
        self.final_frontier.validate()?;
        let expected_initial =
            ArchiveLocalFrontierBindingV1::initial(self.import_id, self.profile_digest);
        if self.initial_frontier != expected_initial {
            return Err(BootstrapImportError::InitialFrontierMismatch);
        }

        let part_count = self.parts.len() as u32;
        let mut predecessor = None;
        let mut frontier = self.initial_frontier;
        for (index, part) in self.parts.iter().enumerate() {
            let ordinal = index as u32;
            part.validate_self()?;
            if part.evidence.import_id() != self.import_id
                || part.evidence.profile_digest() != self.profile_digest
                || part.evidence.ordinal() != ordinal
                || part.evidence.part_count() != part_count
            {
                return Err(BootstrapImportError::PartContextMismatch);
            }
            if part.evidence.predecessor() != predecessor {
                return Err(BootstrapImportError::PredecessorMismatch);
            }
            let expected_sequence = ordinal
                .checked_add(1)
                .ok_or(BootstrapImportError::LengthOverflow)?;
            if part.acceptance_sequence != expected_sequence {
                return Err(BootstrapImportError::AcceptanceSequenceMismatch);
            }
            if part.prior_frontier != frontier {
                return Err(BootstrapImportError::FrontierChainMismatch);
            }
            let expected_post = frontier.advance(part.part_id, part.accepted_event)?;
            if part.post_frontier != expected_post {
                return Err(BootstrapImportError::FrontierChainMismatch);
            }
            predecessor = Some(part.part_id);
            frontier = expected_post;
        }
        if self.final_frontier != frontier {
            return Err(BootstrapImportError::FinalFrontierMismatch);
        }
        if self.final_frontier_proof
            != final_frontier_proof(
                self.workspace_id,
                self.lineage_digest,
                self.import_id,
                self.profile_digest,
                self.final_frontier,
            )
        {
            return Err(BootstrapImportError::FinalFrontierProofMismatch);
        }
        Ok(())
    }
}

impl BootstrapPublicationIdV1 {
    /// Exact portable identity:
    /// SHA-256(domain || workspace || lineage || import || profile ||
    /// inventory-root-digest || blob-root-digest). No field lengths or local
    /// graph/source-provenance identity are inserted.
    pub(crate) fn derive(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        import_id: ImportId,
        profile_digest: BootstrapProfileDigestV1,
        source_inventory_root: SourceInventoryRootV1,
        source_blob_root: SourceBlobChunkRootV1,
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"tine/bootstrap-publication/v1\0");
        hasher.update(workspace_id.as_uuid().as_bytes());
        hasher.update(lineage_digest.as_bytes());
        hasher.update(import_id.as_bytes());
        hasher.update(profile_digest.as_bytes());
        hasher.update(source_inventory_root.digest());
        hasher.update(source_blob_root.digest());
        Self(hasher.finalize().into())
    }
}

/// The bounded direct `commits/<publication-id>` marker. Prefix artifacts are
/// not visible authority until these bytes validate against the named
/// aggregate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct BootstrapAggregateCommitV1 {
    publication_id: BootstrapPublicationIdV1,
    aggregate_digest: BootstrapAggregateDigestV1,
    aggregate_byte_length: u64,
    part_count: u32,
    final_frontier: ArchiveLocalFrontierBindingV1,
}

impl BootstrapAggregateCommitV1 {
    pub(crate) fn for_aggregate(
        aggregate: &BootstrapAggregateManifestV1,
    ) -> Result<Self, BootstrapImportError> {
        let aggregate_bytes = aggregate.encode()?;
        Self::new(
            aggregate.publication_id(),
            aggregate.aggregate_digest(),
            u64::try_from(aggregate_bytes.len())
                .map_err(|_| BootstrapImportError::LengthOverflow)?,
            aggregate.parts.len() as u32,
            aggregate.final_frontier,
        )
    }

    pub(crate) fn new(
        publication_id: BootstrapPublicationIdV1,
        aggregate_digest: BootstrapAggregateDigestV1,
        aggregate_byte_length: u64,
        part_count: u32,
        final_frontier: ArchiveLocalFrontierBindingV1,
    ) -> Result<Self, BootstrapImportError> {
        let value = Self {
            publication_id,
            aggregate_digest,
            aggregate_byte_length,
            part_count,
            final_frontier,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) const fn publication_id(self) -> BootstrapPublicationIdV1 {
        self.publication_id
    }

    pub(crate) const fn aggregate_digest(self) -> BootstrapAggregateDigestV1 {
        self.aggregate_digest
    }

    pub(crate) const fn aggregate_byte_length(self) -> u64 {
        self.aggregate_byte_length
    }

    pub(crate) const fn part_count(self) -> u32 {
        self.part_count
    }

    pub(crate) const fn final_frontier(self) -> ArchiveLocalFrontierBindingV1 {
        self.final_frontier
    }

    pub(crate) fn encode(self) -> Result<Vec<u8>, BootstrapImportError> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(220);
        bytes.extend_from_slice(COMMIT_MAGIC);
        put_field(
            &mut bytes,
            1,
            &BOOTSTRAP_AGGREGATE_COMMIT_SCHEMA_VERSION.to_be_bytes(),
        )?;
        put_field(&mut bytes, 2, self.publication_id.as_bytes())?;
        put_field(&mut bytes, 3, self.aggregate_digest.as_bytes())?;
        put_field(&mut bytes, 4, &self.aggregate_byte_length.to_be_bytes())?;
        put_field(&mut bytes, 5, &self.part_count.to_be_bytes())?;
        put_field(&mut bytes, 6, &self.final_frontier.encode())?;
        if bytes.len() > MAX_BOOTSTRAP_AGGREGATE_COMMIT_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("aggregate commit"));
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BootstrapImportError> {
        if bytes.len() > MAX_BOOTSTRAP_AGGREGATE_COMMIT_BYTES {
            return Err(BootstrapImportError::EncodedSizeLimit("aggregate commit"));
        }
        let fields = CanonicalFieldsV1::parse(bytes, COMMIT_MAGIC, 6, 1)?;
        let version = read_u32(fields.required(1)?)?;
        if version != BOOTSTRAP_AGGREGATE_COMMIT_SCHEMA_VERSION {
            return Err(BootstrapImportError::UnsupportedVersion(version));
        }
        let value = Self::new(
            BootstrapPublicationIdV1::from_bytes(array_32(fields.required(2)?)?),
            BootstrapAggregateDigestV1::from_bytes(array_32(fields.required(3)?)?),
            read_u64(fields.required(4)?)?,
            read_u32(fields.required(5)?)?,
            ArchiveLocalFrontierBindingV1::decode(fields.required(6)?)?,
        )?;
        if value.encode()?.as_slice() != bytes {
            return Err(BootstrapImportError::NonCanonicalBytes);
        }
        Ok(value)
    }

    pub(crate) fn validate_aggregate(
        self,
        aggregate: &BootstrapAggregateManifestV1,
    ) -> Result<(), BootstrapImportError> {
        let expected = Self::for_aggregate(aggregate)?;
        if self != expected {
            return Err(BootstrapImportError::AggregateCommitMismatch);
        }
        Ok(())
    }

    fn validate(self) -> Result<(), BootstrapImportError> {
        if self.aggregate_byte_length == 0
            || self.aggregate_byte_length > MAX_BOOTSTRAP_AGGREGATE_MANIFEST_BYTES as u64
        {
            return Err(BootstrapImportError::ByteLimit("aggregate manifest"));
        }
        if self.part_count > MAX_BOOTSTRAP_PARTS {
            return Err(BootstrapImportError::CountLimit("bootstrap parts"));
        }
        self.final_frontier.validate()?;
        if self.final_frontier.accepted_count != self.part_count {
            return Err(BootstrapImportError::FinalFrontierMismatch);
        }
        Ok(())
    }
}

const PART_DESCRIPTOR_BYTES: usize = 495;

fn accepted_event_binding(
    part_id: BootstrapPartId,
    evidence_digest: BootstrapEvidenceDigestV1,
    manifest_fingerprint: BootstrapManifestFingerprintV1,
    full_object_root: FullObjectRootV1,
    acceptance_sequence: u32,
) -> BootstrapAcceptedEventBindingV1 {
    BootstrapAcceptedEventBindingV1::digest(
        b"tine/bootstrap-import/accepted-event-binding/v1\0",
        &[
            part_id.as_bytes(),
            evidence_digest.as_bytes(),
            manifest_fingerprint.as_bytes(),
            &root_bytes(full_object_root),
            &acceptance_sequence.to_be_bytes(),
        ],
    )
}

fn final_frontier_proof(
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    import_id: ImportId,
    profile_digest: BootstrapProfileDigestV1,
    final_frontier: ArchiveLocalFrontierBindingV1,
) -> BootstrapFinalFrontierProofV1 {
    BootstrapFinalFrontierProofV1::digest(
        b"tine/bootstrap-import/final-frontier-proof/v1\0",
        &[
            workspace_id.as_uuid().as_bytes(),
            lineage_digest.as_bytes(),
            import_id.as_bytes(),
            profile_digest.as_bytes(),
            &final_frontier.encode(),
        ],
    )
}

fn decode_parts(
    bytes: &[u8],
    import_id: ImportId,
    profile_digest: BootstrapProfileDigestV1,
) -> Result<Vec<BootstrapPartDescriptorV1>, BootstrapImportError> {
    if bytes.len() < 4 {
        return Err(BootstrapImportError::Truncated);
    }
    let count = read_u32(&bytes[..4])?;
    if count > MAX_BOOTSTRAP_PARTS {
        return Err(BootstrapImportError::CountLimit("bootstrap parts"));
    }
    let expected = 4_usize
        .checked_add(
            (count as usize)
                .checked_mul(PART_DESCRIPTOR_BYTES)
                .ok_or(BootstrapImportError::LengthOverflow)?,
        )
        .ok_or(BootstrapImportError::LengthOverflow)?;
    if expected != bytes.len() {
        return Err(BootstrapImportError::InvalidFieldLength);
    }
    let mut parts = Vec::with_capacity(count as usize);
    for descriptor in bytes[4..].chunks_exact(PART_DESCRIPTOR_BYTES) {
        parts.push(BootstrapPartDescriptorV1::decode(
            descriptor,
            import_id,
            profile_digest,
        )?);
    }
    Ok(parts)
}

trait RootEncodeV1 {
    fn encode_root(self, bytes: &mut Vec<u8>);
}

impl RootEncodeV1 for SourceInventoryRootV1 {
    fn encode_root(self, bytes: &mut Vec<u8>) {
        self.encode_into(bytes);
    }
}

impl RootEncodeV1 for SourceBlobChunkRootV1 {
    fn encode_root(self, bytes: &mut Vec<u8>) {
        self.encode_into(bytes);
    }
}

impl RootEncodeV1 for SourceSpanRootV1 {
    fn encode_root(self, bytes: &mut Vec<u8>) {
        self.encode_into(bytes);
    }
}

impl RootEncodeV1 for OperationRootV1 {
    fn encode_root(self, bytes: &mut Vec<u8>) {
        self.encode_into(bytes);
    }
}

impl RootEncodeV1 for PayloadObjectRootV1 {
    fn encode_root(self, bytes: &mut Vec<u8>) {
        self.encode_into(bytes);
    }
}

impl RootEncodeV1 for FullObjectRootV1 {
    fn encode_root(self, bytes: &mut Vec<u8>) {
        self.encode_into(bytes);
    }
}

fn root_bytes<T: RootEncodeV1>(root: T) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(44);
    root.encode_root(&mut bytes);
    bytes
}

fn archive_frontier_bytes(frontier: ArchiveLocalFrontierBindingV1) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(69);
    bytes.extend_from_slice(&frontier.digest);
    bytes.extend_from_slice(&frontier.accepted_count.to_be_bytes());
    match frontier.last_part {
        None => {
            bytes.push(0);
            bytes.extend_from_slice(&[0; 32]);
        }
        Some(part_id) => {
            bytes.push(1);
            bytes.extend_from_slice(part_id.as_bytes());
        }
    }
    bytes
}

fn decode_archive_frontier(
    bytes: &[u8],
) -> Result<ArchiveLocalFrontierBindingV1, BootstrapImportError> {
    if bytes.len() != 69 {
        return Err(BootstrapImportError::InvalidFieldLength);
    }
    let last_part = match bytes[36] {
        0 if bytes[37..].iter().all(|byte| *byte == 0) => None,
        1 => Some(BootstrapPartId::from_digest(array_32(&bytes[37..])?)),
        _ => return Err(BootstrapImportError::NonCanonicalBytes),
    };
    let value = ArchiveLocalFrontierBindingV1 {
        digest: array_32(&bytes[..32])?,
        accepted_count: read_u32(&bytes[32..36])?,
        last_part,
    };
    value.validate()?;
    Ok(value)
}

fn part_id_option_bytes(part_id: Option<BootstrapPartId>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(33);
    match part_id {
        None => {
            bytes.push(0);
            bytes.extend_from_slice(&[0; 32]);
        }
        Some(part_id) => {
            bytes.push(1);
            bytes.extend_from_slice(part_id.as_bytes());
        }
    }
    bytes
}

fn decode_part_id_option(bytes: &[u8]) -> Result<Option<BootstrapPartId>, BootstrapImportError> {
    if bytes.len() != 33 {
        return Err(BootstrapImportError::InvalidFieldLength);
    }
    match bytes[0] {
        0 if bytes[1..].iter().all(|byte| *byte == 0) => Ok(None),
        1 => Ok(Some(BootstrapPartId::from_digest(array_32(&bytes[1..])?))),
        _ => Err(BootstrapImportError::NonCanonicalBytes),
    }
}

fn canonical_encode(domain: &[u8], fields: &[&[u8]]) -> Vec<u8> {
    let capacity = domain.len() + fields.iter().map(|field| 8 + field.len()).sum::<usize>();
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(domain);
    for field in fields {
        bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
        bytes.extend_from_slice(field);
    }
    bytes
}

fn canonical_digest(domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    hasher.finalize().into()
}

fn decode_canonical_value<'a>(
    bytes: &'a [u8],
    domain: &[u8],
    field_count: usize,
) -> Result<Vec<&'a [u8]>, BootstrapImportError> {
    if !bytes.starts_with(domain) {
        return Err(BootstrapImportError::InvalidMagic);
    }
    let mut cursor = domain.len();
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        if bytes.len().saturating_sub(cursor) < 8 {
            return Err(BootstrapImportError::Truncated);
        }
        let length = usize::try_from(read_u64(&bytes[cursor..cursor + 8])?)
            .map_err(|_| BootstrapImportError::LengthOverflow)?;
        cursor += 8;
        let end = cursor
            .checked_add(length)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if end > bytes.len() {
            return Err(BootstrapImportError::Truncated);
        }
        fields.push(&bytes[cursor..end]);
        cursor = end;
    }
    if cursor != bytes.len() {
        return Err(BootstrapImportError::TrailingBytes);
    }
    Ok(fields)
}

fn put_sized(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), BootstrapImportError> {
    let length = u32::try_from(value.len()).map_err(|_| BootstrapImportError::LengthOverflow)?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

fn put_field(bytes: &mut Vec<u8>, tag: u8, value: &[u8]) -> Result<(), BootstrapImportError> {
    let length = u32::try_from(value.len()).map_err(|_| BootstrapImportError::LengthOverflow)?;
    let projected = bytes
        .len()
        .checked_add(5)
        .and_then(|length| length.checked_add(value.len()))
        .ok_or(BootstrapImportError::LengthOverflow)?;
    if projected > MAX_BOOTSTRAP_AGGREGATE_MANIFEST_BYTES {
        return Err(BootstrapImportError::EncodedSizeLimit("canonical value"));
    }
    bytes.push(tag);
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

struct CanonicalFieldsV1<'a> {
    fields: Vec<Option<&'a [u8]>>,
}

impl<'a> CanonicalFieldsV1<'a> {
    fn parse(
        bytes: &'a [u8],
        magic: &[u8; 8],
        max_tag: u8,
        depth: u8,
    ) -> Result<Self, BootstrapImportError> {
        if depth > MAX_CANONICAL_NESTING_DEPTH {
            return Err(BootstrapImportError::DepthLimit);
        }
        if bytes.len() < magic.len() || &bytes[..magic.len()] != magic {
            return Err(BootstrapImportError::InvalidMagic);
        }
        let mut fields = vec![None; usize::from(max_tag) + 1];
        let mut cursor = magic.len();
        let mut last_tag = 0;
        while cursor < bytes.len() {
            if bytes.len() - cursor < 5 {
                return Err(BootstrapImportError::Truncated);
            }
            let tag = bytes[cursor];
            cursor += 1;
            if tag == 0 || tag > max_tag {
                return Err(BootstrapImportError::UnknownField(tag));
            }
            if tag == last_tag {
                return Err(BootstrapImportError::DuplicateField(tag));
            }
            if tag < last_tag {
                return Err(BootstrapImportError::NonCanonicalFieldOrder);
            }
            last_tag = tag;
            let length = u32::from_be_bytes(
                bytes[cursor..cursor + 4]
                    .try_into()
                    .expect("four-byte field length"),
            ) as usize;
            cursor += 4;
            let end = cursor
                .checked_add(length)
                .ok_or(BootstrapImportError::LengthOverflow)?;
            if end > bytes.len() {
                return Err(BootstrapImportError::Truncated);
            }
            fields[usize::from(tag)] = Some(&bytes[cursor..end]);
            cursor = end;
        }
        Ok(Self { fields })
    }

    fn required(&self, tag: u8) -> Result<&'a [u8], BootstrapImportError> {
        self.fields
            .get(usize::from(tag))
            .and_then(|field| *field)
            .ok_or(BootstrapImportError::MissingField(tag))
    }
}

struct FixedReader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> FixedReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn with_magic(bytes: &'a [u8], magic: &[u8; 8]) -> Result<Self, BootstrapImportError> {
        if !bytes.starts_with(magic) {
            return Err(BootstrapImportError::InvalidMagic);
        }
        Ok(Self {
            bytes,
            cursor: magic.len(),
        })
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], BootstrapImportError> {
        let end = self
            .cursor
            .checked_add(length)
            .ok_or(BootstrapImportError::LengthOverflow)?;
        if end > self.bytes.len() {
            return Err(BootstrapImportError::Truncated);
        }
        let value = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(value)
    }

    fn array_16(&mut self) -> Result<[u8; 16], BootstrapImportError> {
        array_16(self.take(16)?)
    }

    fn array_32(&mut self) -> Result<[u8; 32], BootstrapImportError> {
        array_32(self.take(32)?)
    }

    fn u32(&mut self) -> Result<u32, BootstrapImportError> {
        read_u32(self.take(4)?)
    }

    fn sized(&mut self) -> Result<&'a [u8], BootstrapImportError> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    fn finish(self) -> Result<(), BootstrapImportError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(BootstrapImportError::TrailingBytes)
        }
    }
}

fn array_16(bytes: &[u8]) -> Result<[u8; 16], BootstrapImportError> {
    bytes
        .try_into()
        .map_err(|_| BootstrapImportError::InvalidFieldLength)
}

fn array_32(bytes: &[u8]) -> Result<[u8; 32], BootstrapImportError> {
    bytes
        .try_into()
        .map_err(|_| BootstrapImportError::InvalidFieldLength)
}

fn read_u32(bytes: &[u8]) -> Result<u32, BootstrapImportError> {
    bytes
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| BootstrapImportError::InvalidFieldLength)
}

fn read_u64(bytes: &[u8]) -> Result<u64, BootstrapImportError> {
    bytes
        .try_into()
        .map(u64::from_be_bytes)
        .map_err(|_| BootstrapImportError::InvalidFieldLength)
}

fn decode_bool(bytes: &[u8]) -> Result<bool, BootstrapImportError> {
    match bytes {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(BootstrapImportError::NonCanonicalBytes),
    }
}

const fn managed_text_kind_byte(kind: ManagedTextKind) -> u8 {
    match kind {
        ManagedTextKind::Page => 1,
        ManagedTextKind::Journal => 2,
    }
}

fn decode_managed_text_kind(bytes: &[u8]) -> Result<ManagedTextKind, BootstrapImportError> {
    match bytes {
        [1] => Ok(ManagedTextKind::Page),
        [2] => Ok(ManagedTextKind::Journal),
        _ => Err(BootstrapImportError::InvalidManagedTextKind),
    }
}

fn validate_page_count(
    page_count: u32,
    entry_count: u32,
    label: &'static str,
) -> Result<(), BootstrapImportError> {
    if page_count > MAX_SOURCE_INDEX_PAGES {
        return Err(BootstrapImportError::CountLimit(label));
    }
    if (page_count == 0) != (entry_count == 0) {
        return Err(BootstrapImportError::IndexContinuityMismatch);
    }
    Ok(())
}

fn checked_count(length: usize, max: u32, label: &'static str) -> Result<(), BootstrapImportError> {
    if length > max as usize {
        return Err(BootstrapImportError::CountLimit(label));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BootstrapImportError {
    AcceptedEventBindingMismatch,
    AcceptanceSequenceMismatch,
    AggregateCommitMismatch,
    BatchIdMismatch,
    BlobContinuityMismatch,
    ByteLimit(&'static str),
    CountLimit(&'static str),
    ConflictingProtocolIdentity(&'static str),
    DepthLimit,
    DuplicateCanonicalItem,
    DuplicateField(u8),
    EmptyIndexPage,
    EmptyOperationTransaction,
    EmptySourceSpan,
    EncodedSizeLimit(&'static str),
    EvidenceDigestMismatch,
    FinalFrontierMismatch,
    FinalFrontierProofMismatch,
    FrontierChainMismatch,
    FullObjectRootMismatch,
    InitialFrontierMismatch,
    IndexContinuityMismatch,
    IndexRootMismatch,
    InvalidArchiveFrontier,
    InvalidFieldLength,
    InvalidMagic,
    InvalidFullObjectKind,
    InvalidManagedPath,
    InvalidManagedTextKind,
    InvalidOrdinal,
    InvalidPartCount,
    LengthOverflow,
    LocatorLimit,
    MissingField(u8),
    MissingPredecessor,
    NonCanonicalBytes,
    NonCanonicalFieldOrder,
    NonCanonicalOrder,
    PartContextMismatch,
    PartIdMismatch,
    PredecessorMismatch,
    ProfileMismatch,
    SourceCountMismatch,
    TrailingBytes,
    Truncated,
    InvalidUtf8,
    UnexpectedPredecessor,
    UnknownField(u8),
    UnsupportedVersion(u32),
}

impl fmt::Display for BootstrapImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bootstrap-import canonical validation failed: {self:?}")
    }
}

impl std::error::Error for BootstrapImportError {}
