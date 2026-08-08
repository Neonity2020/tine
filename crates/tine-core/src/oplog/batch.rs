use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use tine_storage::DurableBatchContract;

use super::{
    BatchId, DeviceId, DocumentId, FrontierV2, ImportId, SessionId, WorkspaceId,
    MANAGED_ENTITY_SET_VERSION,
};

pub use tine_storage::{ContentDigest, LineageDigest, ObjectKind, SemanticEffectDigest};
// Persistent-format constants have exactly one export path in `tine-storage`.
pub use tine_storage::formats::{
    MANIFEST_ENCODING_VERSION, MAX_MANIFEST_BYTES, MAX_OBJECT_BYTES,
    OBJECT_ENVELOPE_SCHEMA_VERSION, OPLOG_PROTOCOL_VERSION,
};

pub const OPERATION_SCHEMA_VERSION: u32 = 7;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchOrigin {
    LocalMutation,
    ExternalReconciliation { import_id: ImportId },
    BootstrapImport,
}

/// Hidden type-level bridge from the generic durable codec to core policy.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CoreDurableBatchContract;

#[doc(hidden)]
pub struct CoreManifestValidationState {
    crdt_documents: HashSet<DocumentId>,
    semantic_count: usize,
}

impl DurableBatchContract for CoreDurableBatchContract {
    type WorkspaceId = WorkspaceId;
    type DocumentId = DocumentId;
    type BatchId = BatchId;
    type DeviceId = DeviceId;
    type SessionId = SessionId;
    type Origin = BatchOrigin;
    type DependencyFrontier = FrontierV2;
    type ManifestValidationState = CoreManifestValidationState;

    const OPERATION_SCHEMA_VERSION: u32 = OPERATION_SCHEMA_VERSION;
    const MANAGED_ENTITY_SET_VERSION: u32 = MANAGED_ENTITY_SET_VERSION;

    fn begin_manifest_validation() -> Self::ManifestValidationState {
        CoreManifestValidationState {
            crdt_documents: HashSet::new(),
            semantic_count: 0,
        }
    }

    fn validate_descriptor_policy(
        state: &mut Self::ManifestValidationState,
        descriptor: &tine_storage::ObjectDescriptor<Self>,
    ) -> Result<(), tine_storage::BatchError<Self>> {
        match descriptor.kind() {
            ObjectKind::SemanticEffect => state.semantic_count += 1,
            ObjectKind::CrdtUpdate => {
                if !state.crdt_documents.insert(descriptor.document_id()) {
                    return Err(tine_storage::BatchError::DuplicateCrdtDocument(
                        descriptor.document_id(),
                    ));
                }
            }
            ObjectKind::ProjectionIntent
            | ObjectKind::AnnotatedBaseBlob
            | ObjectKind::ExternalImportObservation => {}
        }
        Ok(())
    }

    fn finish_manifest_validation(
        state: Self::ManifestValidationState,
    ) -> Result<(), tine_storage::BatchError<Self>> {
        if state.semantic_count != 1 {
            return Err(tine_storage::BatchError::SemanticEffectCardinality(
                state.semantic_count,
            ));
        }
        Ok(())
    }
}

pub type CausalPeerId = tine_storage::CausalPeerId<CoreDurableBatchContract>;
pub type BatchCausalDot = tine_storage::BatchCausalDot<CoreDurableBatchContract>;
pub type ObjectDescriptor = tine_storage::ObjectDescriptor<CoreDurableBatchContract>;
pub type OperationBatch = tine_storage::OperationBatch<CoreDurableBatchContract>;
pub type OperationObject = tine_storage::OperationObject<CoreDurableBatchContract>;
pub type BatchError = tine_storage::BatchError<CoreDurableBatchContract>;

#[derive(Clone, Debug, Eq, PartialEq)]
/// A complete generic batch object set. This validates object identity, type,
/// cardinality, descriptor equality, and the declared semantic-effect digest.
/// P1A.2 must validate the semantic effect against dependency-frontier state;
/// P1B.1 must prove which projection intents require which annotated base
/// blobs.
pub struct PreparedBatch {
    manifest: OperationBatch,
    objects: Vec<OperationObject>,
}

impl PreparedBatch {
    pub fn new(
        manifest: OperationBatch,
        objects: Vec<OperationObject>,
    ) -> Result<Self, BatchError> {
        let mut by_digest = BTreeMap::new();
        for object in objects {
            if object.workspace_id() != manifest.workspace_id() {
                return Err(BatchError::WorkspaceMismatch {
                    expected: manifest.workspace_id(),
                    found: object.workspace_id(),
                });
            }
            let descriptor = object.descriptor()?;
            let digest = descriptor.content_digest();
            if by_digest.insert(digest, (descriptor, object)).is_some() {
                return Err(BatchError::DuplicateObjectDigest(digest));
            }
        }

        let mut ordered = Vec::with_capacity(manifest.required_objects().len());
        for expected in manifest.required_objects() {
            let Some((actual, object)) = by_digest.remove(&expected.content_digest()) else {
                return Err(BatchError::MissingObject(expected.clone()));
            };
            if actual != *expected {
                return Err(BatchError::DescriptorMismatch {
                    expected: expected.clone(),
                    actual,
                });
            }
            ordered.push(object);
        }
        if let Some((_, (descriptor, _))) = by_digest.pop_first() {
            return Err(BatchError::UnexpectedObject(descriptor));
        }
        let semantic = ordered
            .iter()
            .find(|object| object.kind() == ObjectKind::SemanticEffect)
            .expect("validated manifests contain exactly one semantic effect");
        let actual_semantic_digest = SemanticEffectDigest::of(semantic.payload());
        if actual_semantic_digest != manifest.semantic_effect_digest() {
            return Err(BatchError::SemanticEffectDigestMismatch {
                expected: manifest.semantic_effect_digest(),
                actual: actual_semantic_digest,
            });
        }
        super::projection_manifest::validate_projection_object_set(&manifest, &ordered)
            .map_err(|error| BatchError::ProjectionObject(error.to_string()))?;
        super::external_import::validate_external_import_object_set(&manifest, &ordered)
            .map_err(|error| BatchError::ExternalImportObject(error.to_string()))?;
        Ok(Self {
            manifest,
            objects: ordered,
        })
    }

    pub fn manifest(&self) -> &OperationBatch {
        &self.manifest
    }

    pub fn objects(&self) -> &[OperationObject] {
        &self.objects
    }

    /// Exact bytes retained by the durable batch: the canonical manifest plus
    /// every canonical object envelope. Tail capacity must be reserved from
    /// this value before the manifest can make a local mutation authoritative.
    pub fn retained_bytes(&self) -> Result<usize, BatchError> {
        let manifest_bytes = self.manifest.encode()?.len();
        let total = self
            .objects
            .iter()
            .try_fold(manifest_bytes, |total, object| {
                total
                    .checked_add(object.encoded_len()?)
                    .ok_or(BatchError::LengthOverflow)
            })?;
        // A batch that exceeds TAIL_MAX_BYTES is rejected permanently, and the
        // refusal reports only the total -- which cannot distinguish "many
        // objects" from "a verbose envelope" from "one huge object". Those need
        // different fixes. Report the composition by object kind so the
        // dominant contributor is measured rather than guessed.
        if std::env::var_os("TINE_BATCH_TRACE").is_some() {
            use std::collections::BTreeMap;
            let mut by_kind: BTreeMap<String, (usize, usize)> = BTreeMap::new();
            for object in &self.objects {
                let entry = by_kind.entry(format!("{:?}", object.kind())).or_default();
                entry.0 += 1;
                entry.1 += object.encoded_len().unwrap_or(0);
            }
            let summary = by_kind
                .iter()
                .map(|(kind, (count, bytes))| format!("{kind}={count}objs/{bytes}B"))
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!(
                "BATCH COMPOSITION: total={total}B manifest={manifest_bytes}B \
                 objects={} | {summary}",
                self.objects.len()
            );
        }
        Ok(total)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedBatch(PreparedBatch);

impl ValidatedBatch {
    pub(crate) fn new(batch: PreparedBatch) -> Self {
        Self(batch)
    }

    pub fn manifest(&self) -> &OperationBatch {
        self.0.manifest()
    }

    pub fn objects(&self) -> &[OperationObject] {
        self.0.objects()
    }

    pub(crate) fn into_prepared(self) -> PreparedBatch {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;

    fn workspace(value: u128) -> WorkspaceId {
        WorkspaceId::from_uuid(Uuid::from_u128(value))
    }

    fn document(value: u128) -> DocumentId {
        DocumentId::from_uuid(Uuid::from_u128(value))
    }

    fn batch(value: u128) -> BatchId {
        BatchId::from_uuid(Uuid::from_u128(value))
    }

    fn manifest_with(
        required_objects: Vec<ObjectDescriptor>,
    ) -> Result<OperationBatch, BatchError> {
        let device = DeviceId::from_uuid(Uuid::from_u128(4));
        OperationBatch::new_with_causality(
            workspace(1),
            LineageDigest::from_bytes([0x11; 32]),
            batch(3),
            device,
            SessionId::from_uuid(Uuid::from_u128(5)),
            BatchOrigin::LocalMutation,
            BatchCausalDot::new(CausalPeerId::from_device_id(device), 1).unwrap(),
            Vec::new(),
            FrontierV2::default(),
            SemanticEffectDigest::of(b"semantic"),
            required_objects,
        )
    }

    #[test]
    fn core_policy_hook_rejects_construction_and_decode_at_manifest_ingress() {
        assert_eq!(
            manifest_with(Vec::new()).unwrap_err(),
            BatchError::SemanticEffectCardinality(0)
        );

        let object = OperationObject::new(
            workspace(1),
            document(1),
            ObjectKind::SemanticEffect,
            b"semantic".to_vec(),
        )
        .unwrap();
        let semantic_descriptor = object.descriptor().unwrap();
        let update_a = OperationObject::new(
            workspace(1),
            document(2),
            ObjectKind::CrdtUpdate,
            b"update-a".to_vec(),
        )
        .unwrap()
        .descriptor()
        .unwrap();
        let update_b = OperationObject::new(
            workspace(1),
            document(2),
            ObjectKind::CrdtUpdate,
            b"update-b".to_vec(),
        )
        .unwrap()
        .descriptor()
        .unwrap();
        assert!(matches!(
            manifest_with(vec![
                semantic_descriptor.clone(),
                update_a.clone(),
                update_b.clone(),
            ]),
            Err(BatchError::DuplicateCrdtDocument(found)) if found == document(2)
        ));

        let manifest = manifest_with(vec![semantic_descriptor, update_a]).unwrap();
        let mut wire: Value = serde_json::from_slice(&manifest.encode().unwrap()).unwrap();
        wire["required_objects"] = Value::Array(Vec::new());
        assert_eq!(
            OperationBatch::decode(&serde_json::to_vec(&wire).unwrap()).unwrap_err(),
            BatchError::SemanticEffectCardinality(0)
        );

        let mut wire: Value = serde_json::from_slice(&manifest.encode().unwrap()).unwrap();
        wire["required_objects"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(update_b).unwrap());
        let mut descriptors: Vec<ObjectDescriptor> =
            serde_json::from_value(wire["required_objects"].clone()).unwrap();
        descriptors.sort_unstable();
        wire["required_objects"] = serde_json::to_value(descriptors).unwrap();
        let error = OperationBatch::decode(&serde_json::to_vec(&wire).unwrap()).unwrap_err();
        assert!(
            matches!(error, BatchError::DuplicateCrdtDocument(found) if found == document(2)),
            "{error:?}"
        );
    }

    #[test]
    fn manifest_validation_preserves_per_descriptor_error_precedence() {
        let update_a = OperationObject::new(
            workspace(1),
            document(10),
            ObjectKind::CrdtUpdate,
            b"update-a".to_vec(),
        )
        .unwrap()
        .descriptor()
        .unwrap();
        let update_b = OperationObject::new(
            workspace(1),
            document(10),
            ObjectKind::CrdtUpdate,
            b"update-b".to_vec(),
        )
        .unwrap()
        .descriptor()
        .unwrap();
        let semantic = OperationObject::new(
            workspace(1),
            document(20),
            ObjectKind::SemanticEffect,
            b"semantic".to_vec(),
        )
        .unwrap()
        .descriptor()
        .unwrap();
        let reused_digest = ObjectDescriptor::new(
            document(30),
            ObjectKind::ProjectionIntent,
            semantic.content_digest(),
            semantic.encoded_byte_length(),
        )
        .unwrap();
        let mut descriptors = vec![update_a, update_b, semantic.clone(), reused_digest];
        descriptors.sort_unstable();

        assert!(matches!(
            manifest_with(descriptors.clone()),
            Err(BatchError::DuplicateCrdtDocument(found)) if found == document(10)
        ));

        let manifest = manifest_with(vec![semantic]).unwrap();
        let mut wire: Value = serde_json::from_slice(&manifest.encode().unwrap()).unwrap();
        wire["required_objects"] = serde_json::to_value(descriptors).unwrap();
        let error = OperationBatch::decode(&serde_json::to_vec(&wire).unwrap()).unwrap_err();
        assert!(
            matches!(error, BatchError::DuplicateCrdtDocument(found) if found == document(10)),
            "{error:?}"
        );
    }
}
