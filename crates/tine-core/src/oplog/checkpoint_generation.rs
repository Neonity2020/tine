//! Versioned accepted-frontier codecs for checkpoint generations.
//!
//! R1a is deliberately reader-only at the production boundary. These codecs
//! freeze and test V2 bytes while every live recovery path continues to accept
//! only V1 authority. The generation builder and marker authoring arrive in
//! later independently releasable cuts.

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::hot_engine::{
    AcceptedBatchEvidence, AcceptedFrontierRoot, ACCEPTED_EVIDENCE_SCHEMA_VERSION,
    ACCEPTED_FRONTIER_ROOT_SCHEMA_VERSION,
};
use super::{BatchId, ContentDigest, DocumentDependencies, EngineError};

pub(crate) const ACCEPTED_FRONTIER_ROOT_V2_SCHEMA_VERSION: u32 = 2;
pub(crate) const ACCEPTED_EVIDENCE_V2_SCHEMA_VERSION: u32 = 9;
const ACCEPTED_FRONTIER_V2_STATE_DOMAIN: &[u8] = b"tine/oplog/accepted-frontier/v2/state\0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CheckpointGenerationBindingV2 {
    pub(crate) generation_id: [u8; 16],
    pub(crate) predecessor_generation_id: Option<[u8; 16]>,
    pub(crate) full_anchor_generation_id: [u8; 16],
    pub(crate) covered_count: u64,
    pub(crate) covered_document_count: u64,
    pub(crate) covered_block_count: u64,
    pub(crate) covered_retained_bytes_total: u64,
    pub(crate) covered_semantic_capsules_root_digest: ContentDigest,
    pub(crate) covered_batch_root_key: Option<[u8; 16]>,
    pub(crate) covered_batch_root_digest: ContentDigest,
    pub(crate) covered_status_root_key: Option<[u8; 16]>,
    pub(crate) covered_status_root_digest: ContentDigest,
    pub(crate) covered_sequence_root_digest: Option<ContentDigest>,
    pub(crate) covered_sequence_height: u8,
    pub(crate) covered_causal_tip_root_key: Option<[u8; 16]>,
    pub(crate) covered_causal_tip_root_digest: ContentDigest,
    pub(crate) covered_head_facts_root_digest: ContentDigest,
    pub(crate) current_projection_payload_pins_root_digest: ContentDigest,
    pub(crate) nonlinear_state_root_digest: ContentDigest,
    pub(crate) retention_pins_root_digest: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptedFrontierRootV2 {
    pub(crate) acceptance_sequence: u64,
    pub(crate) document_count: u64,
    pub(crate) document_overlay_count: u64,
    pub(crate) retained_bytes_total: u64,
    pub(crate) document_map_root_key: Option<[u8; 16]>,
    pub(crate) document_map_root_digest: ContentDigest,
    pub(crate) batch_map_root_key: Option<[u8; 16]>,
    pub(crate) batch_map_root_digest: ContentDigest,
    pub(crate) batch_map_count: u64,
    pub(crate) status_map_root_key: Option<[u8; 16]>,
    pub(crate) status_map_root_digest: ContentDigest,
    pub(crate) status_map_count: u64,
    pub(crate) sequence_root_digest: Option<ContentDigest>,
    pub(crate) sequence_height: u8,
    pub(crate) sequence_count: u64,
    pub(crate) generation: CheckpointGenerationBindingV2,
    pub(crate) state_digest: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptedBatchEvidenceV2 {
    pub(crate) batch_id: BatchId,
    pub(crate) manifest_fingerprint: ContentDigest,
    pub(crate) event_binding_digest: ContentDigest,
    pub(crate) acceptance_sequence: u64,
    pub(crate) prior_frontier_root: AcceptedFrontierRootV2,
    pub(crate) post_frontier_root: AcceptedFrontierRootV2,
    pub(crate) affected_documents: Vec<DocumentDependencies>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum VersionedAcceptedFrontierRoot {
    V1(AcceptedFrontierRoot),
    V2(AcceptedFrontierRootV2),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum VersionedAcceptedBatchEvidence {
    V1(AcceptedBatchEvidence),
    V2(AcceptedBatchEvidenceV2),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct GenerationWireV2 {
    generation_id: [u8; 16],
    predecessor_generation_id: Option<[u8; 16]>,
    full_anchor_generation_id: [u8; 16],
    covered_count: u64,
    covered_document_count: u64,
    covered_block_count: u64,
    covered_retained_bytes_total: u64,
    covered_semantic_capsules_root_digest: [u8; 32],
    covered_batch_root_key: Option<[u8; 16]>,
    covered_batch_root_digest: [u8; 32],
    covered_status_root_key: Option<[u8; 16]>,
    covered_status_root_digest: [u8; 32],
    covered_sequence_root_digest: Option<[u8; 32]>,
    covered_sequence_height: u8,
    covered_causal_tip_root_key: Option<[u8; 16]>,
    covered_causal_tip_root_digest: [u8; 32],
    covered_head_facts_root_digest: [u8; 32],
    current_projection_payload_pins_root_digest: [u8; 32],
    nonlinear_state_root_digest: [u8; 32],
    retention_pins_root_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct FrontierIdentityWireV2 {
    schema: u32,
    acceptance_sequence: u64,
    document_count: u64,
    document_overlay_count: u64,
    retained_bytes_total: u64,
    document_map_root_key: Option<[u8; 16]>,
    document_map_root_digest: [u8; 32],
    batch_map_root_key: Option<[u8; 16]>,
    batch_map_root_digest: [u8; 32],
    batch_map_count: u64,
    status_map_root_key: Option<[u8; 16]>,
    status_map_root_digest: [u8; 32],
    status_map_count: u64,
    sequence_root_digest: Option<[u8; 32]>,
    sequence_height: u8,
    sequence_count: u64,
    generation: GenerationWireV2,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct FrontierWireV2 {
    identity: FrontierIdentityWireV2,
    state_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct EvidenceWireV2 {
    schema: u32,
    batch_id: [u8; 16],
    manifest_fingerprint: [u8; 32],
    event_binding_digest: [u8; 32],
    acceptance_sequence: u64,
    prior_frontier_root: FrontierWireV2,
    post_frontier_root: FrontierWireV2,
    affected_documents: Vec<DocumentDependencies>,
}

impl From<&CheckpointGenerationBindingV2> for GenerationWireV2 {
    fn from(value: &CheckpointGenerationBindingV2) -> Self {
        Self {
            generation_id: value.generation_id,
            predecessor_generation_id: value.predecessor_generation_id,
            full_anchor_generation_id: value.full_anchor_generation_id,
            covered_count: value.covered_count,
            covered_document_count: value.covered_document_count,
            covered_block_count: value.covered_block_count,
            covered_retained_bytes_total: value.covered_retained_bytes_total,
            covered_semantic_capsules_root_digest: *value
                .covered_semantic_capsules_root_digest
                .as_bytes(),
            covered_batch_root_key: value.covered_batch_root_key,
            covered_batch_root_digest: *value.covered_batch_root_digest.as_bytes(),
            covered_status_root_key: value.covered_status_root_key,
            covered_status_root_digest: *value.covered_status_root_digest.as_bytes(),
            covered_sequence_root_digest: value
                .covered_sequence_root_digest
                .map(|digest| *digest.as_bytes()),
            covered_sequence_height: value.covered_sequence_height,
            covered_causal_tip_root_key: value.covered_causal_tip_root_key,
            covered_causal_tip_root_digest: *value.covered_causal_tip_root_digest.as_bytes(),
            covered_head_facts_root_digest: *value.covered_head_facts_root_digest.as_bytes(),
            current_projection_payload_pins_root_digest: *value
                .current_projection_payload_pins_root_digest
                .as_bytes(),
            nonlinear_state_root_digest: *value.nonlinear_state_root_digest.as_bytes(),
            retention_pins_root_digest: *value.retention_pins_root_digest.as_bytes(),
        }
    }
}

impl From<GenerationWireV2> for CheckpointGenerationBindingV2 {
    fn from(value: GenerationWireV2) -> Self {
        Self {
            generation_id: value.generation_id,
            predecessor_generation_id: value.predecessor_generation_id,
            full_anchor_generation_id: value.full_anchor_generation_id,
            covered_count: value.covered_count,
            covered_document_count: value.covered_document_count,
            covered_block_count: value.covered_block_count,
            covered_retained_bytes_total: value.covered_retained_bytes_total,
            covered_semantic_capsules_root_digest: ContentDigest::from_bytes(
                value.covered_semantic_capsules_root_digest,
            ),
            covered_batch_root_key: value.covered_batch_root_key,
            covered_batch_root_digest: ContentDigest::from_bytes(value.covered_batch_root_digest),
            covered_status_root_key: value.covered_status_root_key,
            covered_status_root_digest: ContentDigest::from_bytes(value.covered_status_root_digest),
            covered_sequence_root_digest: value
                .covered_sequence_root_digest
                .map(ContentDigest::from_bytes),
            covered_sequence_height: value.covered_sequence_height,
            covered_causal_tip_root_key: value.covered_causal_tip_root_key,
            covered_causal_tip_root_digest: ContentDigest::from_bytes(
                value.covered_causal_tip_root_digest,
            ),
            covered_head_facts_root_digest: ContentDigest::from_bytes(
                value.covered_head_facts_root_digest,
            ),
            current_projection_payload_pins_root_digest: ContentDigest::from_bytes(
                value.current_projection_payload_pins_root_digest,
            ),
            nonlinear_state_root_digest: ContentDigest::from_bytes(
                value.nonlinear_state_root_digest,
            ),
            retention_pins_root_digest: ContentDigest::from_bytes(value.retention_pins_root_digest),
        }
    }
}

impl AcceptedFrontierRootV2 {
    fn identity_wire(&self) -> FrontierIdentityWireV2 {
        FrontierIdentityWireV2 {
            schema: ACCEPTED_FRONTIER_ROOT_V2_SCHEMA_VERSION,
            acceptance_sequence: self.acceptance_sequence,
            document_count: self.document_count,
            document_overlay_count: self.document_overlay_count,
            retained_bytes_total: self.retained_bytes_total,
            document_map_root_key: self.document_map_root_key,
            document_map_root_digest: *self.document_map_root_digest.as_bytes(),
            batch_map_root_key: self.batch_map_root_key,
            batch_map_root_digest: *self.batch_map_root_digest.as_bytes(),
            batch_map_count: self.batch_map_count,
            status_map_root_key: self.status_map_root_key,
            status_map_root_digest: *self.status_map_root_digest.as_bytes(),
            status_map_count: self.status_map_count,
            sequence_root_digest: self.sequence_root_digest.map(|digest| *digest.as_bytes()),
            sequence_height: self.sequence_height,
            sequence_count: self.sequence_count,
            generation: (&self.generation).into(),
        }
    }

    pub(crate) fn expected_state_digest(&self) -> Result<ContentDigest, EngineError> {
        let identity = encode(&self.identity_wire())?;
        let mut bytes = ACCEPTED_FRONTIER_V2_STATE_DOMAIN.to_vec();
        bytes.extend_from_slice(&identity);
        Ok(ContentDigest::of(&bytes))
    }

    pub(crate) fn with_computed_state_digest(mut self) -> Result<Self, EngineError> {
        self.state_digest = self.expected_state_digest()?;
        Ok(self)
    }

    pub(crate) fn encode_canonical(&self) -> Result<Vec<u8>, EngineError> {
        self.validate()?;
        encode(&FrontierWireV2 {
            identity: self.identity_wire(),
            state_digest: *self.state_digest.as_bytes(),
        })
    }

    fn from_wire(wire: FrontierWireV2) -> Result<Self, EngineError> {
        let identity = wire.identity;
        let root = Self {
            acceptance_sequence: identity.acceptance_sequence,
            document_count: identity.document_count,
            document_overlay_count: identity.document_overlay_count,
            retained_bytes_total: identity.retained_bytes_total,
            document_map_root_key: identity.document_map_root_key,
            document_map_root_digest: ContentDigest::from_bytes(identity.document_map_root_digest),
            batch_map_root_key: identity.batch_map_root_key,
            batch_map_root_digest: ContentDigest::from_bytes(identity.batch_map_root_digest),
            batch_map_count: identity.batch_map_count,
            status_map_root_key: identity.status_map_root_key,
            status_map_root_digest: ContentDigest::from_bytes(identity.status_map_root_digest),
            status_map_count: identity.status_map_count,
            sequence_root_digest: identity.sequence_root_digest.map(ContentDigest::from_bytes),
            sequence_height: identity.sequence_height,
            sequence_count: identity.sequence_count,
            generation: identity.generation.into(),
            state_digest: ContentDigest::from_bytes(wire.state_digest),
        };
        if identity.schema != ACCEPTED_FRONTIER_ROOT_V2_SCHEMA_VERSION {
            return Err(archive("unknown accepted-frontier V2 schema"));
        }
        root.validate()?;
        Ok(root)
    }

    pub(crate) fn validate(&self) -> Result<(), EngineError> {
        let empty = tine_storage::sealed_accepted_index::authenticated_map_empty_digest();
        if self.document_overlay_count > self.document_count
            || self.batch_map_count != self.acceptance_sequence
            || self.status_map_count != self.acceptance_sequence
            || self.sequence_count != self.acceptance_sequence
            || self.generation.covered_count > self.acceptance_sequence
            || self.generation.covered_document_count > self.document_count
            || self.generation.covered_retained_bytes_total > self.retained_bytes_total
            || self.generation.generation_id == [0; 16]
            || self.generation.full_anchor_generation_id == [0; 16]
        {
            return Err(archive(
                "malformed accepted-frontier V2 counts or generation",
            ));
        }
        validate_map_binding(
            self.document_overlay_count,
            self.document_map_root_key,
            self.document_map_root_digest,
            empty,
        )?;
        validate_map_binding(
            self.batch_map_count,
            self.batch_map_root_key,
            self.batch_map_root_digest,
            empty,
        )?;
        validate_map_binding(
            self.status_map_count,
            self.status_map_root_key,
            self.status_map_root_digest,
            empty,
        )?;
        validate_sequence_binding(
            self.sequence_count,
            self.sequence_height,
            self.sequence_root_digest,
        )?;
        validate_map_binding(
            self.generation.covered_count,
            self.generation.covered_batch_root_key,
            self.generation.covered_batch_root_digest,
            empty,
        )?;
        validate_map_binding(
            self.generation.covered_count,
            self.generation.covered_status_root_key,
            self.generation.covered_status_root_digest,
            empty,
        )?;
        validate_sequence_binding(
            self.generation.covered_count,
            self.generation.covered_sequence_height,
            self.generation.covered_sequence_root_digest,
        )?;
        if self.expected_state_digest()? != self.state_digest {
            return Err(archive("accepted-frontier V2 state digest mismatch"));
        }
        Ok(())
    }
}

impl AcceptedBatchEvidenceV2 {
    pub(crate) fn encode_canonical(&self) -> Result<Vec<u8>, EngineError> {
        self.validate()?;
        encode(&EvidenceWireV2 {
            schema: ACCEPTED_EVIDENCE_V2_SCHEMA_VERSION,
            batch_id: self.batch_id.as_uuid().into_bytes(),
            manifest_fingerprint: *self.manifest_fingerprint.as_bytes(),
            event_binding_digest: *self.event_binding_digest.as_bytes(),
            acceptance_sequence: self.acceptance_sequence,
            prior_frontier_root: frontier_wire(&self.prior_frontier_root),
            post_frontier_root: frontier_wire(&self.post_frontier_root),
            affected_documents: self.affected_documents.clone(),
        })
    }

    fn from_wire(wire: EvidenceWireV2) -> Result<Self, EngineError> {
        if wire.schema != ACCEPTED_EVIDENCE_V2_SCHEMA_VERSION {
            return Err(archive("unknown accepted-evidence V2 schema"));
        }
        let evidence = Self {
            batch_id: BatchId::from_uuid(uuid::Uuid::from_bytes(wire.batch_id)),
            manifest_fingerprint: ContentDigest::from_bytes(wire.manifest_fingerprint),
            event_binding_digest: ContentDigest::from_bytes(wire.event_binding_digest),
            acceptance_sequence: wire.acceptance_sequence,
            prior_frontier_root: AcceptedFrontierRootV2::from_wire(wire.prior_frontier_root)?,
            post_frontier_root: AcceptedFrontierRootV2::from_wire(wire.post_frontier_root)?,
            affected_documents: wire.affected_documents,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    fn validate(&self) -> Result<(), EngineError> {
        self.prior_frontier_root.validate()?;
        self.post_frontier_root.validate()?;
        if self.acceptance_sequence != self.post_frontier_root.acceptance_sequence
            || self.acceptance_sequence
                != self
                    .prior_frontier_root
                    .acceptance_sequence
                    .saturating_add(1)
            || self.prior_frontier_root.generation != self.post_frontier_root.generation
            || self
                .affected_documents
                .windows(2)
                .any(|pair| pair[0].document_id() >= pair[1].document_id())
        {
            return Err(archive("non-canonical accepted-evidence V2 transition"));
        }
        Ok(())
    }
}

pub(crate) fn decode_versioned_frontier_root(
    bytes: &[u8],
) -> Result<VersionedAcceptedFrontierRoot, EngineError> {
    match leading_schema(bytes, "accepted-frontier root")? {
        ACCEPTED_FRONTIER_ROOT_SCHEMA_VERSION => Ok(VersionedAcceptedFrontierRoot::V1(
            AcceptedFrontierRoot::decode_canonical_v1(bytes)?,
        )),
        ACCEPTED_FRONTIER_ROOT_V2_SCHEMA_VERSION => {
            let wire: FrontierWireV2 = decode(bytes, "accepted-frontier V2")?;
            Ok(VersionedAcceptedFrontierRoot::V2(
                AcceptedFrontierRootV2::from_wire(wire)?,
            ))
        }
        schema => Err(archive(format!(
            "unknown accepted-frontier schema {schema}"
        ))),
    }
}

pub(crate) fn decode_versioned_evidence(
    bytes: &[u8],
) -> Result<VersionedAcceptedBatchEvidence, EngineError> {
    match leading_schema(bytes, "accepted evidence")? {
        ACCEPTED_EVIDENCE_SCHEMA_VERSION => Ok(VersionedAcceptedBatchEvidence::V1(
            AcceptedBatchEvidence::decode_canonical_v1(bytes)?,
        )),
        ACCEPTED_EVIDENCE_V2_SCHEMA_VERSION => {
            let wire: EvidenceWireV2 = decode(bytes, "accepted-evidence V2")?;
            Ok(VersionedAcceptedBatchEvidence::V2(
                AcceptedBatchEvidenceV2::from_wire(wire)?,
            ))
        }
        schema => Err(archive(format!(
            "unknown accepted-evidence schema {schema}"
        ))),
    }
}

fn leading_schema(bytes: &[u8], what: &str) -> Result<u32, EngineError> {
    postcard::take_from_bytes::<u32>(bytes)
        .map(|(schema, _)| schema)
        .map_err(|error| archive(format!("invalid {what} schema: {error}")))
}

pub(crate) struct TineAcceptedEvidenceDecoder;

impl tine_storage::sealed_accepted_index::SealedAcceptedEvidenceDecoder
    for TineAcceptedEvidenceDecoder
{
    fn decode_accepted_evidence(
        &self,
        evidence_schema: u32,
        exact_evidence_bytes: &[u8],
    ) -> Result<
        tine_storage::sealed_accepted_index::AcceptedEvidenceBindingV2,
        tine_storage::sealed_accepted_index::SealedAcceptedIndexError,
    > {
        let found = leading_schema(exact_evidence_bytes, "accepted evidence").map_err(|error| {
            tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Corrupt(
                error.to_string(),
            )
        })?;
        if found != evidence_schema {
            return Err(
                tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Corrupt(format!(
                    "accepted-status evidence schema {evidence_schema} != encoded schema {found}"
                )),
            );
        }
        let decoded = decode_versioned_evidence(exact_evidence_bytes).map_err(|error| {
            tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Corrupt(
                error.to_string(),
            )
        })?;
        let (batch_id, manifest_fingerprint, event_binding_digest, acceptance_sequence) =
            match decoded {
                VersionedAcceptedBatchEvidence::V1(evidence) => (
                    evidence.batch_id(),
                    evidence.manifest_fingerprint(),
                    evidence.event_binding_digest(),
                    evidence.acceptance_sequence(),
                ),
                VersionedAcceptedBatchEvidence::V2(evidence) => (
                    evidence.batch_id,
                    evidence.manifest_fingerprint,
                    evidence.event_binding_digest,
                    evidence.acceptance_sequence,
                ),
            };
        Ok(
            tine_storage::sealed_accepted_index::AcceptedEvidenceBindingV2 {
                batch_id: batch_id.as_uuid().into_bytes(),
                manifest_fingerprint,
                event_binding_digest,
                acceptance_sequence,
            },
        )
    }
}

fn frontier_wire(root: &AcceptedFrontierRootV2) -> FrontierWireV2 {
    FrontierWireV2 {
        identity: root.identity_wire(),
        state_digest: *root.state_digest.as_bytes(),
    }
}

fn validate_map_binding(
    count: u64,
    key: Option<[u8; 16]>,
    digest: ContentDigest,
    empty: ContentDigest,
) -> Result<(), EngineError> {
    if (count == 0) != key.is_none() || (count == 0) != (digest == empty) {
        return Err(archive("authenticated-map V2 count/root mismatch"));
    }
    Ok(())
}

fn validate_sequence_binding(
    count: u64,
    height: u8,
    digest: Option<ContentDigest>,
) -> Result<(), EngineError> {
    if (count == 0) != digest.is_none() || (count == 0 && height != 0) {
        return Err(archive("accepted-sequence V2 count/root mismatch"));
    }
    if count > 0 {
        let mut capacity = 1_u64;
        for _ in 0..height {
            capacity = capacity
                .checked_mul(tine_storage::formats::SEALED_ACCEPTED_SEQUENCE_FANOUT as u64)
                .ok_or_else(|| archive("accepted-sequence V2 height overflow"))?;
        }
        let lower = if height == 0 {
            0
        } else {
            capacity / tine_storage::formats::SEALED_ACCEPTED_SEQUENCE_FANOUT as u64
        };
        if count > capacity || count <= lower {
            return Err(archive("accepted-sequence V2 height is not minimal"));
        }
    }
    Ok(())
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, EngineError> {
    postcard::to_allocvec(value).map_err(|error| archive(error.to_string()))
}

fn decode<T: DeserializeOwned + Serialize>(bytes: &[u8], what: &str) -> Result<T, EngineError> {
    let (value, trailing): (T, &[u8]) = postcard::take_from_bytes(bytes)
        .map_err(|error| archive(format!("invalid {what}: {error}")))?;
    if !trailing.is_empty() || encode(&value)? != bytes {
        return Err(archive(format!("non-canonical {what}")));
    }
    Ok(value)
}

fn archive(message: impl Into<String>) -> EngineError {
    EngineError::Archive(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct SealedMemoryStore {
        objects: Vec<(
            tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
            ContentDigest,
            Vec<u8>,
        )>,
    }

    impl tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore for SealedMemoryStore {
        fn read_sealed_accepted_object(
            &self,
            kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
            address: ContentDigest,
        ) -> Result<Option<Vec<u8>>, tine_storage::sealed_accepted_index::SealedAcceptedIndexError>
        {
            Ok(self
                .objects
                .iter()
                .find(|(stored_kind, stored_address, _)| {
                    *stored_kind == kind && *stored_address == address
                })
                .map(|(_, _, bytes)| bytes.clone()))
        }

        fn publish_sealed_accepted_object(
            &mut self,
            kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
            address: ContentDigest,
            bytes: &[u8],
        ) -> Result<(), tine_storage::sealed_accepted_index::SealedAcceptedIndexError> {
            if let Some((_, _, existing)) =
                self.objects
                    .iter()
                    .find(|(stored_kind, stored_address, _)| {
                        *stored_kind == kind && *stored_address == address
                    })
            {
                if existing != bytes {
                    return Err(
                        tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Corrupt(
                            "same address has different test bytes".into(),
                        ),
                    );
                }
                return Ok(());
            }
            self.objects.push((kind, address, bytes.to_vec()));
            Ok(())
        }
    }

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::from_bytes([byte; 32])
    }

    fn generation(covered: u64) -> CheckpointGenerationBindingV2 {
        let empty = tine_storage::sealed_accepted_index::authenticated_map_empty_digest();
        CheckpointGenerationBindingV2 {
            generation_id: [1; 16],
            predecessor_generation_id: None,
            full_anchor_generation_id: [1; 16],
            covered_count: covered,
            covered_document_count: 0,
            covered_block_count: 0,
            covered_retained_bytes_total: 0,
            covered_semantic_capsules_root_digest: digest(2),
            covered_batch_root_key: (covered > 0).then_some([3; 16]),
            covered_batch_root_digest: if covered > 0 { digest(4) } else { empty },
            covered_status_root_key: (covered > 0).then_some([5; 16]),
            covered_status_root_digest: if covered > 0 { digest(6) } else { empty },
            covered_sequence_root_digest: (covered > 0).then_some(digest(7)),
            covered_sequence_height: 0,
            covered_causal_tip_root_key: None,
            covered_causal_tip_root_digest: empty,
            covered_head_facts_root_digest: digest(8),
            current_projection_payload_pins_root_digest: digest(9),
            nonlinear_state_root_digest: digest(10),
            retention_pins_root_digest: digest(11),
        }
    }

    fn frontier(sequence: u64) -> AcceptedFrontierRootV2 {
        let empty = tine_storage::sealed_accepted_index::authenticated_map_empty_digest();
        AcceptedFrontierRootV2 {
            acceptance_sequence: sequence,
            document_count: 0,
            document_overlay_count: 0,
            retained_bytes_total: 0,
            document_map_root_key: None,
            document_map_root_digest: empty,
            batch_map_root_key: (sequence > 0).then_some([3; 16]),
            batch_map_root_digest: if sequence > 0 { digest(4) } else { empty },
            batch_map_count: sequence,
            status_map_root_key: (sequence > 0).then_some([5; 16]),
            status_map_root_digest: if sequence > 0 { digest(6) } else { empty },
            status_map_count: sequence,
            sequence_root_digest: (sequence > 0).then_some(digest(7)),
            sequence_height: 0,
            sequence_count: sequence,
            generation: generation(0),
            state_digest: digest(0),
        }
        .with_computed_state_digest()
        .unwrap()
    }

    #[test]
    fn v1_empty_frontier_bytes_are_unchanged_and_decode_through_the_versioned_path() {
        let root = AcceptedFrontierRoot::empty();
        let bytes = root.encode_canonical_v1().unwrap();
        assert_eq!(
            ContentDigest::of(&bytes).to_string(),
            "dfd057e3e7ed32e4a39159ba70f81844edb3627dcabbe93dfb4e61e1ee50d418"
        );
        assert_eq!(
            decode_versioned_frontier_root(&bytes).unwrap(),
            VersionedAcceptedFrontierRoot::V1(root)
        );
    }

    #[test]
    fn v2_frontier_is_canonical_and_state_digest_bound() {
        let root = frontier(1);
        let bytes = root.encode_canonical().unwrap();
        assert_eq!(
            ContentDigest::of(&bytes).to_string(),
            "7e854a63b62ca6dabef3e136dac2e6a1547989c47a6547ad40b31ca1715403b4"
        );
        assert_eq!(
            decode_versioned_frontier_root(&bytes).unwrap(),
            VersionedAcceptedFrontierRoot::V2(root.clone())
        );
        let mut corrupt = bytes.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(decode_versioned_frontier_root(&corrupt).is_err());
    }

    #[test]
    fn v2_evidence_round_trips_but_is_not_a_v1_value() {
        let evidence = AcceptedBatchEvidenceV2 {
            batch_id: BatchId::from_uuid(uuid::Uuid::from_bytes([0x51; 16])),
            manifest_fingerprint: digest(0x61),
            event_binding_digest: digest(0x71),
            acceptance_sequence: 1,
            prior_frontier_root: frontier(0),
            post_frontier_root: frontier(1),
            affected_documents: Vec::new(),
        };
        let bytes = evidence.encode_canonical().unwrap();
        assert_eq!(
            ContentDigest::of(&bytes).to_string(),
            "e4943c6a64e5b1d11f5d1db1019132e1875b39c7b0af60e6a4c9acb577e0527a"
        );
        assert_eq!(
            decode_versioned_evidence(&bytes).unwrap(),
            VersionedAcceptedBatchEvidence::V2(evidence.clone())
        );
        let binding = tine_storage::sealed_accepted_index::SealedAcceptedEvidenceDecoder::decode_accepted_evidence(
            &TineAcceptedEvidenceDecoder,
            ACCEPTED_EVIDENCE_V2_SCHEMA_VERSION,
            &bytes,
        )
        .unwrap();
        assert_eq!(binding.batch_id, [0x51; 16]);
        assert_eq!(binding.manifest_fingerprint, digest(0x61));
        assert_eq!(binding.event_binding_digest, digest(0x71));
        assert_eq!(binding.acceptance_sequence, 1);
        assert!(tine_storage::sealed_accepted_index::SealedAcceptedEvidenceDecoder::decode_accepted_evidence(
            &TineAcceptedEvidenceDecoder,
            ACCEPTED_EVIDENCE_SCHEMA_VERSION,
            &bytes,
        )
        .is_err());
        assert!(AcceptedBatchEvidence::decode_canonical_v1(&bytes).is_err());

        let mut changed_generation = evidence.clone();
        changed_generation.post_frontier_root.generation = generation(1);
        changed_generation.post_frontier_root = changed_generation
            .post_frontier_root
            .with_computed_state_digest()
            .unwrap();
        assert!(changed_generation.encode_canonical().is_err());
    }

    #[test]
    fn version_dispatch_rejects_unknown_and_mixed_schema_bytes() {
        let v1_root = AcceptedFrontierRoot::empty().encode_canonical_v1().unwrap();
        let v2_root = frontier(1).encode_canonical().unwrap();
        for (mut bytes, replacement) in [
            (
                v1_root.clone(),
                ACCEPTED_FRONTIER_ROOT_V2_SCHEMA_VERSION as u8,
            ),
            (v2_root.clone(), ACCEPTED_FRONTIER_ROOT_SCHEMA_VERSION as u8),
            (v2_root, 0x7f),
        ] {
            bytes[0] = replacement;
            assert!(decode_versioned_frontier_root(&bytes).is_err());
        }

        let batch_id = BatchId::from_uuid(uuid::Uuid::from_bytes([0x51; 16]));
        let v1_evidence = AcceptedBatchEvidence::for_test(
            batch_id,
            digest(0x61),
            digest(0x71),
            AcceptedFrontierRoot::empty(),
            Vec::new(),
            Vec::new(),
            vec![(batch_id, digest(0x81))],
            0,
        )
        .encode_canonical_v1()
        .unwrap();
        let v2_evidence = AcceptedBatchEvidenceV2 {
            batch_id,
            manifest_fingerprint: digest(0x61),
            event_binding_digest: digest(0x71),
            acceptance_sequence: 1,
            prior_frontier_root: frontier(0),
            post_frontier_root: frontier(1),
            affected_documents: Vec::new(),
        }
        .encode_canonical()
        .unwrap();
        for (mut bytes, replacement) in [
            (v1_evidence, ACCEPTED_EVIDENCE_V2_SCHEMA_VERSION as u8),
            (v2_evidence.clone(), ACCEPTED_EVIDENCE_SCHEMA_VERSION as u8),
            (v2_evidence, 0x7f),
        ] {
            bytes[0] = replacement;
            assert!(decode_versioned_evidence(&bytes).is_err());
        }
    }

    #[test]
    fn tine_and_storage_share_the_exact_causal_record_address() {
        use super::super::hot_engine::{
            accepted_causal_record_digest, authenticated_causal_clock_root,
        };
        use super::super::{BatchCausalDot, CausalPeerId, DeviceId};

        let low =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x11; 16])));
        let author =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x44; 16])));
        let (root_key, root_digest) =
            authenticated_causal_clock_root(&[(low, 3), (author, 7)]).unwrap();
        assert_eq!(root_key, Some([0x44; 16]));
        assert_eq!(
            root_digest.to_string(),
            "effa60f99d8c9c9560c9f6d176ce457070460af5152e813d6affdd6cb48496d2"
        );
        let engine_address = accepted_causal_record_digest(
            BatchId::from_uuid(uuid::Uuid::from_bytes([0x51; 16])),
            digest(0x22),
            digest(0x33),
            BatchCausalDot::new(author, 7).unwrap(),
            root_key,
            root_digest,
        );
        let storage_record = tine_storage::sealed_accepted_index::SealedAcceptedCausalRecordV2 {
            batch_id: [0x51; 16],
            manifest_fingerprint: digest(0x22),
            event_binding_digest: digest(0x33),
            causal_peer_id: [0x44; 16],
            causal_counter: 7,
            canonical_causal_clock: vec![
                tine_storage::sealed_accepted_index::SealedAcceptedCausalClockEntryV2 {
                    peer_id: [0x11; 16],
                    counter: 3,
                },
                tine_storage::sealed_accepted_index::SealedAcceptedCausalClockEntryV2 {
                    peer_id: [0x44; 16],
                    counter: 7,
                },
            ],
        };
        assert_eq!(storage_record.address().unwrap(), engine_address);
        assert_eq!(
            engine_address.to_string(),
            "7f4986b2491f46879adadfd66a4f7c3f516006c123868ad7ecff6d5791b80756"
        );
    }

    #[test]
    fn tine_decoder_completes_the_shared_membership_proof() {
        use tine_storage::sealed_accepted_index::{
            AcceptedSequenceEntryV2, AcceptedSequenceRootV2, AcceptedStatusRecordV2,
            AuthenticatedMapRootV1, SealedAcceptedCausalClockEntryV2, SealedAcceptedCausalRecordV2,
            SealedAcceptedIndexReader, SealedAcceptedIndexRootsV2, SealedAcceptedIndexWriter,
        };

        let batch_id = BatchId::from_uuid(uuid::Uuid::from_bytes([0x51; 16]));
        let evidence = AcceptedBatchEvidenceV2 {
            batch_id,
            manifest_fingerprint: digest(0x61),
            event_binding_digest: digest(0x71),
            acceptance_sequence: 1,
            prior_frontier_root: frontier(0),
            post_frontier_root: frontier(1),
            affected_documents: Vec::new(),
        };
        let causal = SealedAcceptedCausalRecordV2 {
            batch_id: [0x51; 16],
            manifest_fingerprint: digest(0x61),
            event_binding_digest: digest(0x71),
            causal_peer_id: [0x44; 16],
            causal_counter: 7,
            canonical_causal_clock: vec![SealedAcceptedCausalClockEntryV2 {
                peer_id: [0x44; 16],
                counter: 7,
            }],
        };
        let mut store = SealedMemoryStore::default();
        let (batch_map, status_map, sequence);
        {
            let mut writer = SealedAcceptedIndexWriter::new(&mut store);
            let causal_address = writer.publish_causal(&causal).unwrap();
            let status = AcceptedStatusRecordV2 {
                batch_id: [0x51; 16],
                no_op: false,
                evidence_schema: ACCEPTED_EVIDENCE_V2_SCHEMA_VERSION,
                exact_evidence_bytes: evidence.encode_canonical().unwrap(),
                accepted_causal_record_digest: causal_address,
            };
            let status_address = writer.publish_status(&status).unwrap();
            batch_map = writer
                .upsert_map(AuthenticatedMapRootV1::empty(), [0x51; 16], causal_address)
                .unwrap();
            status_map = writer
                .upsert_map(AuthenticatedMapRootV1::empty(), [0x51; 16], status_address)
                .unwrap();
            sequence = writer
                .append_sequence(
                    AcceptedSequenceRootV2::empty(),
                    AcceptedSequenceEntryV2 {
                        sequence: 1,
                        batch_id: [0x51; 16],
                        accepted_status_value_digest: status_address,
                    },
                )
                .unwrap();
        }
        let proof = SealedAcceptedIndexReader::new(&store)
            .prove_membership(
                SealedAcceptedIndexRootsV2 {
                    batch_map,
                    status_map,
                    sequence,
                },
                1,
                [0x51; 16],
                &TineAcceptedEvidenceDecoder,
            )
            .unwrap()
            .unwrap();
        assert_eq!(proof.sequence.sequence, 1);
        assert_eq!(
            proof.status.exact_evidence_bytes,
            evidence.encode_canonical().unwrap()
        );
        assert_eq!(proof.causal, causal);
    }

    #[test]
    fn r1a_v2_types_are_codec_only_and_no_production_marker_can_author_them() {
        let oplog = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/oplog");
        let mut outside_mentions = Vec::new();
        for entry in std::fs::read_dir(&oplog).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) != Some("rs")
                || path.file_name().and_then(|value| value.to_str())
                    == Some("checkpoint_generation.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let production = source.split("\n#[cfg(test)]").next().unwrap_or(&source);
            if production.contains("AcceptedFrontierRootV2")
                || production.contains("CheckpointGenerationBindingV2")
                || production.contains("AcceptedBatchEvidenceV2")
            {
                outside_mentions.push(path);
            }
        }
        assert!(
            outside_mentions.is_empty(),
            "R1a V2 authority escaped the codec module: {outside_mentions:?}"
        );

        let own = include_str!("checkpoint_generation.rs")
            .split("\n#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "std::fs",
            "cap_std",
            "ObjectStore",
            "DurableDirectoryPublication",
            "publish_immutable",
            "write_all",
        ] {
            assert!(
                !own.contains(forbidden),
                "R1a codec gained a production authoring primitive: {forbidden}"
            );
        }

        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        for required in [
            "tine-storage::sealed_accepted_index",
            "production recovery continues to accept only V1",
            "module may name its V2 authority types",
        ] {
            assert!(
                contract.contains(required),
                "storage contract omitted the tested R1a boundary: {required}"
            );
        }
    }
}
