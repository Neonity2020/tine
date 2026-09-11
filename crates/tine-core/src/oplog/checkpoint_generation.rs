//! Adapter between Tine's one current accepted-evidence format and the shared
//! sealed accepted-history index.
//!
//! A5 completes the former R1a reader-only boundary with one disposable
//! generation publisher. Managed Storage is pre-0.7, so this module still
//! contains no legacy decoder, version dispatch, or migration bridge.

#[path = "sealed_document_map.rs"]
mod sealed_document_map;

use sealed_document_map::SealedDocumentMap;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use serde::{Deserialize, Serialize};

use super::hot_engine::{
    AcceptedBatchEvidence, AcceptedFrontierRoot, CleanCheckpointAcceptedRow,
    CleanCheckpointCapture, CompactAcceptedDocument, ACCEPTED_EVIDENCE_SCHEMA_VERSION,
};
use super::object_store::ObjectStore;
use super::{
    BatchCausalDot, BatchId, BlobDescription, CausalPeerId, ContentDigest, DocumentDependencies,
    DocumentId, WriterIncarnationId,
};
use tine_storage::sealed_accepted_index::AuthenticatedMapKey;

const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const CHECKPOINT_DIRECTORY: &str = "clean-open-checkpoint-v1";
const CHECKPOINT_POINTER: &str = "current";
const CHECKPOINT_PAYLOAD_NAMES: [&str; 2] = ["payload-a", "payload-b"];
const CHECKPOINT_GENERATION_NAMES: [&str; 2] = ["generation-a", "generation-b"];
const MAX_CHECKPOINT_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const CLEAN_CHECKPOINT_LAG_MAX: u64 = 64;

#[cfg(test)]
static FAIL_CHECKPOINT_WRITE_ROOTS: Mutex<BTreeSet<std::path::PathBuf>> =
    Mutex::new(BTreeSet::new());

#[cfg(test)]
pub(crate) fn fail_checkpoint_writes_for_test(store_root: &std::path::Path, fail: bool) {
    let root = std::fs::canonicalize(store_root)
        .expect("the checkpoint failure fixture has opened its archive root");
    let mut roots = FAIL_CHECKPOINT_WRITE_ROOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if fail {
        roots.insert(root);
    } else {
        roots.remove(&root);
    }
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
        use tine_storage::sealed_accepted_index::SealedAcceptedIndexError;

        if evidence_schema != ACCEPTED_EVIDENCE_SCHEMA_VERSION {
            return Err(SealedAcceptedIndexError::Corrupt(format!(
                "accepted-status evidence schema {evidence_schema} != current schema {ACCEPTED_EVIDENCE_SCHEMA_VERSION}"
            )));
        }
        let evidence = AcceptedBatchEvidence::decode_canonical(exact_evidence_bytes)
            .map_err(|error| SealedAcceptedIndexError::Corrupt(error.to_string()))?;
        Ok(
            tine_storage::sealed_accepted_index::AcceptedEvidenceBindingV2 {
                batch_id: evidence.batch_id().as_uuid().into_bytes(),
                manifest_fingerprint: evidence.manifest_fingerprint(),
                event_binding_digest: evidence.event_binding_digest(),
                acceptance_sequence: evidence.acceptance_sequence(),
            },
        )
    }
}

#[derive(Default)]
pub(crate) struct CheckpointSealedStore {
    objects: BTreeMap<(u8, ContentDigest), Vec<u8>>,
}

fn sealed_kind_code(kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind) -> u8 {
    use tine_storage::sealed_accepted_index::SealedAcceptedObjectKind;
    match kind {
        SealedAcceptedObjectKind::MapNode => 1,
        SealedAcceptedObjectKind::StatusRecord => 2,
        SealedAcceptedObjectKind::SequenceLeaf => 3,
        SealedAcceptedObjectKind::SequenceNode => 4,
        SealedAcceptedObjectKind::CausalRecord => 5,
    }
}

fn sealed_kind_from_code(
    code: u8,
) -> Result<tine_storage::sealed_accepted_index::SealedAcceptedObjectKind, String> {
    use tine_storage::sealed_accepted_index::SealedAcceptedObjectKind;
    match code {
        1 => Ok(SealedAcceptedObjectKind::MapNode),
        2 => Ok(SealedAcceptedObjectKind::StatusRecord),
        3 => Ok(SealedAcceptedObjectKind::SequenceLeaf),
        4 => Ok(SealedAcceptedObjectKind::SequenceNode),
        5 => Ok(SealedAcceptedObjectKind::CausalRecord),
        _ => Err("clean checkpoint has an unknown sealed object kind".into()),
    }
}

impl tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore for CheckpointSealedStore {
    fn read_sealed_accepted_object(
        &self,
        kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        address: ContentDigest,
    ) -> Result<Option<Vec<u8>>, tine_storage::sealed_accepted_index::SealedAcceptedIndexError>
    {
        Ok(self
            .objects
            .get(&(sealed_kind_code(kind), address))
            .cloned())
    }

    fn publish_sealed_accepted_object(
        &mut self,
        kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        address: ContentDigest,
        bytes: &[u8],
    ) -> Result<(), tine_storage::sealed_accepted_index::SealedAcceptedIndexError> {
        use tine_storage::sealed_accepted_index::SealedAcceptedIndexError;
        let key = (sealed_kind_code(kind), address);
        if let Some(existing) = self.objects.get(&key) {
            if existing != bytes {
                return Err(SealedAcceptedIndexError::Corrupt(
                    "same sealed checkpoint address has different bytes".into(),
                ));
            }
            return Ok(());
        }
        self.objects.insert(key, bytes.to_vec());
        Ok(())
    }
}

impl CheckpointSealedStore {
    fn retain_only(&mut self, retained: &BTreeSet<(u8, ContentDigest)>) {
        self.objects.retain(|key, _| retained.contains(key));
    }

    fn required_bytes(
        &self,
        kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        address: ContentDigest,
    ) -> Result<&[u8], String> {
        self.objects
            .get(&(sealed_kind_code(kind), address))
            .map(Vec::as_slice)
            .ok_or_else(|| format!("clean checkpoint sealed {kind} object {address} is missing"))
    }

    fn collect_map(
        &self,
        root: tine_storage::sealed_accepted_index::AuthenticatedMapRootV1,
    ) -> Result<BTreeMap<AuthenticatedMapKey, ContentDigest>, String> {
        use tine_storage::sealed_accepted_index::{
            SealedAcceptedObjectKind, SealedAuthenticatedMapNodeV2,
        };

        let mut rows = BTreeMap::new();
        let mut pending = root.root.into_iter().collect::<Vec<_>>();
        while let Some(link) = pending.pop() {
            let node = SealedAuthenticatedMapNodeV2::decode(
                link,
                self.required_bytes(SealedAcceptedObjectKind::MapNode, link.digest)?,
            )
            .map_err(|error| error.to_string())?;
            if rows.insert(node.key, node.value_digest).is_some() {
                return Err("clean checkpoint sealed map repeats a key".into());
            }
            pending.extend(node.left);
            pending.extend(node.right);
            if rows.len() > usize::try_from(root.count).unwrap_or(usize::MAX) {
                return Err("clean checkpoint sealed map exceeds its root count".into());
            }
        }
        if rows.len()
            != usize::try_from(root.count)
                .map_err(|_| "clean checkpoint map count exceeds usize")?
        {
            return Err("clean checkpoint sealed map count differs from its root".into());
        }
        Ok(rows)
    }
}

// This is a construction working-set budget, not a graph/history occupancy
// limit. A single larger legal record is published and flushed on its own.
const SEALED_STAGING_BATCH_BYTES: usize = 8 * 1024 * 1024;
// The shared batch retains a directory capability per publication. Bound that
// resource as well as payload bytes; flushing never refuses more history.
const SEALED_STAGING_BATCH_OBJECTS: usize = 64;
const SEALED_STAGING_FILE_PREFIX: &str = "sealed-v2";

fn sealed_staging_name(
    kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
    address: ContentDigest,
) -> String {
    format!(
        "{SEALED_STAGING_FILE_PREFIX}-{}-{address}",
        sealed_kind_code(kind)
    )
}

/// Read-only point access to exact sealed objects. This carries no generation
/// authority: only a later qualified generation commit can name its roots.
pub(crate) struct SealedGenerationDirectory {
    directory: cap_std::fs::Dir,
}

impl SealedGenerationDirectory {
    pub(crate) fn open(directory: &cap_std::fs::Dir) -> Result<Self, String> {
        Ok(Self {
            directory: directory.try_clone().map_err(|error| error.to_string())?,
        })
    }
}

impl tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore
    for SealedGenerationDirectory
{
    fn read_sealed_accepted_object(
        &self,
        kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        address: ContentDigest,
    ) -> Result<Option<Vec<u8>>, tine_storage::sealed_accepted_index::SealedAcceptedIndexError>
    {
        tine_storage::read_optional_regular(
            &self.directory,
            &sealed_staging_name(kind, address),
            MAX_CHECKPOINT_BYTES,
            None,
        )
        .map_err(|error| {
            tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Store(error.to_string())
        })
    }

    fn publish_sealed_accepted_object(
        &mut self,
        _kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        _address: ContentDigest,
        _bytes: &[u8],
    ) -> Result<(), tine_storage::sealed_accepted_index::SealedAcceptedIndexError> {
        Err(
            tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Store(
                "sealed generation directory is read-only".into(),
            ),
        )
    }
}

// Linux can batch data and name barriers. Every other target uses the
// retained private-directory primitive: Android needs its single-writer rename
// fallback, and Windows needs its write-through publication protocol.
enum SealedStagingPublication {
    Batch(tine_storage::ExactImmutablePublicationBatch),
    Immediate(tine_storage::DurableDirectoryPublication),
}

impl SealedStagingPublication {
    fn open(directory: &cap_std::fs::Dir) -> Result<Self, String> {
        if cfg!(target_os = "linux") {
            tine_storage::ExactImmutablePublicationBatch::new(directory)
                .map(Self::Batch)
                .map_err(|error| error.to_string())
        } else {
            Self::open_immediate(directory)
        }
    }

    fn open_immediate(directory: &cap_std::fs::Dir) -> Result<Self, String> {
        tine_storage::DurableDirectoryPublication::open(directory)
            .map(Self::Immediate)
            .map_err(|error| error.to_string())
    }

    fn publish(
        &mut self,
        directory: &cap_std::fs::Dir,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        match self {
            Self::Batch(batch) => batch.publish(directory, name, bytes),
            Self::Immediate(directory) => directory.publish_new_exact_single_writer(name, bytes),
        }
        .map_err(|error| error.to_string())
    }

    fn finish(self) -> Result<(), String> {
        match self {
            Self::Batch(batch) => batch
                .finish()
                .map(|_| ())
                .map_err(|error| error.to_string()),
            // Each immediate publication already completed its barrier.
            Self::Immediate(_) => Ok(()),
        }
    }
}

/// A caller-owned, sole-writer staging directory. Canonical node encoding and
/// address validation remain in the shared writer/reader. Reuse A5's memory
/// adapter only for the bounded unfinished publication batch, never as authority.
/// Drop abandons unfinished publication; successful finish returns point access
/// only after the shared durability primitive has completed.
pub(crate) struct SealedGenerationStagingStore {
    reader: SealedGenerationDirectory,
    publication: Option<SealedStagingPublication>,
    pending: CheckpointSealedStore,
    pending_bytes: usize,
    batch_byte_budget: usize,
    batch_object_budget: usize,
    failed: bool,
}

impl SealedGenerationStagingStore {
    pub(crate) fn open(directory: &cap_std::fs::Dir) -> Result<Self, String> {
        Ok(Self {
            reader: SealedGenerationDirectory::open(directory)?,
            publication: None,
            pending: CheckpointSealedStore::default(),
            pending_bytes: 0,
            batch_byte_budget: SEALED_STAGING_BATCH_BYTES,
            batch_object_budget: SEALED_STAGING_BATCH_OBJECTS,
            failed: false,
        })
    }

    fn flush(&mut self) -> Result<(), String> {
        if self.failed {
            return Err("sealed generation staging previously failed".into());
        }
        if let Some(publication) = self.publication.take() {
            if let Err(error) = publication.finish() {
                self.failed = true;
                return Err(error.to_string());
            }
            self.pending.objects.clear();
            self.pending_bytes = 0;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<SealedGenerationDirectory, String> {
        self.flush()?;
        Ok(self.reader)
    }
}

impl tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore
    for SealedGenerationStagingStore
{
    fn read_sealed_accepted_object(
        &self,
        kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        address: ContentDigest,
    ) -> Result<Option<Vec<u8>>, tine_storage::sealed_accepted_index::SealedAcceptedIndexError>
    {
        if self.failed {
            return Err(
                tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Store(
                    "sealed generation staging previously failed".into(),
                ),
            );
        }
        if let Some(bytes) = self.pending.read_sealed_accepted_object(kind, address)? {
            return Ok(Some(bytes));
        }
        self.reader.read_sealed_accepted_object(kind, address)
    }

    fn publish_sealed_accepted_object(
        &mut self,
        kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        address: ContentDigest,
        bytes: &[u8],
    ) -> Result<(), tine_storage::sealed_accepted_index::SealedAcceptedIndexError> {
        self.stage_named_bytes(
            sealed_kind_code(kind),
            address,
            &sealed_staging_name(kind, address),
            bytes,
        )
        .map_err(tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Store)
    }
}

impl SealedGenerationStagingStore {
    fn stage_named_bytes(
        &mut self,
        kind_code: u8,
        address: ContentDigest,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        let result = (|| -> Result<(), String> {
            if self.failed {
                return Err("sealed generation staging previously failed".into());
            }
            if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
                return Err(
                    "sealed construction record exceeds the current checkpoint record limit".into(),
                );
            }
            if let Some(existing) = self.pending.objects.get(&(kind_code, address)) {
                if existing != bytes {
                    return Err("sealed staging address has different pending bytes".into());
                }
                return Ok(());
            }
            if self.pending_bytes.saturating_add(bytes.len()) > self.batch_byte_budget {
                self.flush()?;
            }
            if self.publication.is_none() {
                self.publication = Some(SealedStagingPublication::open(&self.reader.directory)?);
            }
            self.publication
                .as_mut()
                .expect("publication opened")
                .publish(&self.reader.directory, name, bytes)
                .map_err(|error| error.to_string())?;
            self.pending
                .objects
                .insert((kind_code, address), bytes.to_vec());
            self.pending_bytes = self
                .pending_bytes
                .checked_add(bytes.len())
                .ok_or("sealed staging byte count overflowed")?;
            if self.pending_bytes >= self.batch_byte_budget
                || self.pending.objects.len() >= self.batch_object_budget
            {
                self.flush()?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn stage_capsule_blob(&mut self, bytes: &[u8]) -> Result<BlobDescription, String> {
        let blob = BlobDescription::of(bytes);
        // Zero is private construction bookkeeping; sealed-node kinds are 1..=5.
        self.stage_named_bytes(
            0,
            ContentDigest::from_bytes(*blob.sha256()),
            &capsule_blob_name(ContentDigest::from_bytes(*blob.sha256())),
            bytes,
        )?;
        Ok(blob)
    }
}

const CAPSULE_BLOB_PREFIX: &str = "capsule-v1";
const DOCUMENT_CAPSULE_SCHEMA: u32 = 1;

fn capsule_blob_name(digest: ContentDigest) -> String {
    format!("{CAPSULE_BLOB_PREFIX}-{digest}")
}

fn verify_capsule_blob(expected: BlobDescription, bytes: &[u8]) -> Result<(), String> {
    if BlobDescription::of(bytes) != expected {
        return Err("generation capsule blob differs from its exact description".into());
    }
    Ok(())
}

impl SealedGenerationDirectory {
    fn read_capsule_blob(&self, blob: BlobDescription) -> Result<Vec<u8>, String> {
        if blob.byte_length() > MAX_CHECKPOINT_BYTES {
            return Err(
                "generation capsule blob exceeds the current checkpoint record limit".into(),
            );
        }
        let bytes = tine_storage::read_optional_regular(
            &self.directory,
            &capsule_blob_name(ContentDigest::from_bytes(*blob.sha256())),
            blob.byte_length(),
            None,
        )
        .map_err(|error| error.to_string())?
        .ok_or("generation capsule blob is missing")?;
        verify_capsule_blob(blob, &bytes)?;
        Ok(bytes)
    }
}

/// Per-document immutable roster value. The checkpoint digest binds actual CRDT
/// bytes; dependencies retain stable document identity and accepted direct heads.
/// No run-local cutoff digest is serialized, so unchanged values remain shared.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentCapsuleRecord {
    schema: u32,
    dependencies: DocumentDependencies,
    checkpoint: BlobDescription,
}

impl DocumentCapsuleRecord {
    fn encode(&self) -> Result<Vec<u8>, String> {
        postcard::to_stdvec(self).map_err(|error| error.to_string())
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let (record, remaining): (Self, &[u8]) =
            postcard::take_from_bytes(bytes).map_err(|error| error.to_string())?;
        if record.schema != DOCUMENT_CAPSULE_SCHEMA
            || !remaining.is_empty()
            || record.encode()? != bytes
        {
            return Err("generation document capsule is not the current canonical record".into());
        }
        Ok(record)
    }
}

/// An immutable document map candidate. The enclosing generation must separately
/// prove the complete roster and bind workspace/catalog/cutoff/retention facts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SealedDocumentRoster {
    map: SealedDocumentMap,
}

impl SealedDocumentRoster {
    pub(crate) fn empty() -> Self {
        Self {
            map: SealedDocumentMap::empty(),
        }
    }

    pub(crate) fn with_document(
        self,
        store: &mut SealedGenerationStagingStore,
        cutoff: &SealedAcceptedCutoff,
        compact: &CompactAcceptedDocument,
    ) -> Result<Self, String> {
        if compact.cutoff_state_digest() != cutoff.frontier().state_digest() {
            return Err("generation capsule belongs to another accepted cutoff".into());
        }
        let checkpoint = store.stage_capsule_blob(compact.checkpoint())?;
        let record = DocumentCapsuleRecord {
            schema: DOCUMENT_CAPSULE_SCHEMA,
            dependencies: compact.dependencies().clone(),
            checkpoint,
        };
        let record_blob = store.stage_capsule_blob(&record.encode()?)?;
        let map = self.map.upsert(
            store,
            super::DocumentKey::Entity(record.dependencies.document_id()),
            ContentDigest::from_bytes(*record_blob.sha256()),
        )?;
        Ok(Self { map })
    }

    pub(crate) fn load_document(
        self,
        store: &SealedGenerationDirectory,
        catalog: DocumentId,
        document: DocumentId,
    ) -> Result<Option<(DocumentDependencies, loro::LoroDoc)>, String> {
        let Some(record) = self.document_record(store, document)? else {
            return Ok(None);
        };
        let checkpoint = store.read_capsule_blob(record.checkpoint)?;
        let restored =
            super::hot_engine::qualify_compact_document(catalog, &record.dependencies, &checkpoint)
                .map_err(|error| error.to_string())?;
        Ok(Some((record.dependencies, restored)))
    }
    pub(crate) fn qualify_complete_keys(
        self,
        store: &SealedGenerationDirectory,
        documents: impl Iterator<Item = DocumentId>,
    ) -> Result<(), String> {
        self.map
            .qualify_complete_keys(store, documents.map(super::DocumentKey::Entity))
    }

    pub(crate) fn document_count(self) -> u64 {
        self.map.count()
    }

    pub(crate) fn inherited_dependencies(
        self,
        store: &SealedGenerationStagingStore,
        document: DocumentId,
    ) -> Result<Option<DocumentDependencies>, String> {
        if store.failed {
            return Err("sealed generation staging previously failed".into());
        }
        Ok(self
            .document_record(&store.reader, document)?
            .map(|record| record.dependencies))
    }

    fn document_record(
        self,
        store: &SealedGenerationDirectory,
        document: DocumentId,
    ) -> Result<Option<DocumentCapsuleRecord>, String> {
        let Some(address) = self
            .map
            .value(store, super::DocumentKey::Entity(document))?
        else {
            return Ok(None);
        };
        // The map value authenticates the descriptor bytes. Its encoded size is
        // not stored in map nodes, so the existing per-record ceiling applies.
        let bytes = tine_storage::read_optional_regular(
            &store.directory,
            &capsule_blob_name(address),
            MAX_CHECKPOINT_BYTES,
            None,
        )
        .map_err(|error| error.to_string())?
        .ok_or("generation document descriptor is missing")?;
        if ContentDigest::of(&bytes) != address {
            return Err("generation document descriptor digest differs".into());
        }
        let record = DocumentCapsuleRecord::decode(&bytes)?;
        if record.dependencies.document_id() != document {
            return Err("generation document descriptor names another document".into());
        }
        Ok(Some(record))
    }
}

struct RecordingCheckpointSealedStore<'a> {
    inner: &'a CheckpointSealedStore,
    reads: RefCell<BTreeSet<(u8, ContentDigest)>>,
}

impl<'a> RecordingCheckpointSealedStore<'a> {
    fn new(inner: &'a CheckpointSealedStore) -> Self {
        Self {
            inner,
            reads: RefCell::new(BTreeSet::new()),
        }
    }

    fn reads(&self) -> BTreeSet<(u8, ContentDigest)> {
        self.reads.borrow().clone()
    }
}

impl tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore
    for RecordingCheckpointSealedStore<'_>
{
    fn read_sealed_accepted_object(
        &self,
        kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        address: ContentDigest,
    ) -> Result<Option<Vec<u8>>, tine_storage::sealed_accepted_index::SealedAcceptedIndexError>
    {
        self.reads
            .borrow_mut()
            .insert((sealed_kind_code(kind), address));
        tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore::read_sealed_accepted_object(
            self.inner,
            kind,
            address,
        )
    }

    fn publish_sealed_accepted_object(
        &mut self,
        _kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        _address: ContentDigest,
        _bytes: &[u8],
    ) -> Result<(), tine_storage::sealed_accepted_index::SealedAcceptedIndexError> {
        Err(
            tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Store(
                "recording checkpoint store is read-only".into(),
            ),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MapRootWire {
    count: u64,
    /// The map's full root key bytes. Shared authenticated-map keys are
    /// variable length, so this is never a fixed-width identity field.
    root_key: Option<Vec<u8>>,
    root_digest: Option<ContentDigest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SequenceRootWire {
    len: u64,
    height: u8,
    root_digest: Option<ContentDigest>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RosterRootsWire {
    batch_map: MapRootWire,
    status_map: MapRootWire,
    sequence: SequenceRootWire,
}

fn map_root_to_wire(
    root: tine_storage::sealed_accepted_index::AuthenticatedMapRootV1,
) -> MapRootWire {
    MapRootWire {
        count: root.count,
        root_key: root.root.map(|link| link.key.as_slice().to_vec()),
        root_digest: root.root.map(|link| link.digest),
    }
}

fn map_root_from_wire(
    wire: MapRootWire,
) -> Result<tine_storage::sealed_accepted_index::AuthenticatedMapRootV1, String> {
    use tine_storage::sealed_accepted_index::{AuthenticatedMapLinkV1, AuthenticatedMapRootV1};
    let root = match (wire.root_key.as_deref(), wire.root_digest) {
        (Some(key), Some(digest)) => Some(AuthenticatedMapLinkV1 {
            key: AuthenticatedMapKey::new(key)
                .map_err(|error| format!("clean checkpoint map root key is invalid: {error}"))?,
            digest,
        }),
        (None, None) => None,
        _ => return Err("clean checkpoint map root is partial".into()),
    };
    if (wire.count == 0) != root.is_none() {
        return Err("clean checkpoint map root count is inconsistent".into());
    }
    Ok(AuthenticatedMapRootV1 {
        count: wire.count,
        root,
    })
}

fn roots_from_wire(
    wire: RosterRootsWire,
) -> Result<tine_storage::sealed_accepted_index::SealedAcceptedIndexRootsV2, String> {
    use tine_storage::sealed_accepted_index::{AcceptedSequenceRootV2, SealedAcceptedIndexRootsV2};
    let roots = SealedAcceptedIndexRootsV2 {
        batch_map: map_root_from_wire(wire.batch_map)?,
        status_map: map_root_from_wire(wire.status_map)?,
        sequence: AcceptedSequenceRootV2 {
            len: wire.sequence.len,
            height: wire.sequence.height,
            root_digest: wire.sequence.root_digest,
        },
    };
    roots.validate_counts().map_err(|error| error.to_string())?;
    Ok(roots)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointPayloadV1 {
    schema_version: u32,
    state_bytes: Vec<u8>,
    sealed_objects: BTreeMap<(u8, ContentDigest), Vec<u8>>,
    roster_roots: RosterRootsWire,
    required_objects: Vec<ContentDigest>,
    capture_work: u64,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointGenerationV1 {
    schema_version: u32,
    sequence: u64,
    slot: u8,
    payload_digest: ContentDigest,
    payload_len: u64,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointPointerV1 {
    schema_version: u32,
    sequence: u64,
    slot: u8,
    generation_digest: ContentDigest,
}

fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    postcard::to_allocvec(value).map_err(|error| error.to_string())
}

fn decode_canonical<T: for<'de> Deserialize<'de> + Serialize>(bytes: &[u8]) -> Result<T, String> {
    let (value, trailing): (T, &[u8]) =
        postcard::take_from_bytes(bytes).map_err(|error| error.to_string())?;
    if !trailing.is_empty() || encode_canonical(&value)? != bytes {
        return Err("clean checkpoint value is noncanonical".into());
    }
    Ok(value)
}

/// An in-process accepted-history cutoff built from engine evidence. It is not
/// a durable generation, a portable frontier, or a decoded checkpoint payload.
/// The unchanged run-local frontier is only a qualification witness for this
/// construction; a later generation format must bind the canonical facts.
#[derive(Clone, Debug)]
pub(crate) struct SealedAcceptedCutoff {
    frontier: AcceptedFrontierRoot,
    roots: tine_storage::sealed_accepted_index::SealedAcceptedIndexRootsV2,
    causal_tip_root: tine_storage::sealed_accepted_index::AuthenticatedMapRootV1,
    causal_tips: BTreeMap<[u8; 16], tine_storage::sealed_accepted_index::CausalTipRecordV2>,
}

impl SealedAcceptedCutoff {
    pub(crate) fn empty(frontier: AcceptedFrontierRoot) -> Result<Self, String> {
        use tine_storage::sealed_accepted_index::{
            AcceptedSequenceRootV2, AuthenticatedMapRootV1, SealedAcceptedIndexRootsV2,
        };
        frontier
            .encode_canonical()
            .map_err(|error| error.to_string())?;
        let empty = AuthenticatedMapRootV1::empty();
        if frontier.acceptance_sequence() != 0
            || frontier.batch_map_root_key().is_some()
            || frontier.batch_map_root_digest() != empty.root_digest()
        {
            return Err("sealed cutoff bootstrap requires a sequence-zero frontier".into());
        }
        Ok(Self {
            frontier,
            roots: SealedAcceptedIndexRootsV2 {
                batch_map: empty,
                status_map: empty,
                sequence: AcceptedSequenceRootV2::empty(),
            },
            causal_tip_root: empty,
            causal_tips: BTreeMap::new(),
        })
    }

    pub(crate) fn frontier(&self) -> &AcceptedFrontierRoot {
        &self.frontier
    }

    pub(crate) fn roots(&self) -> tine_storage::sealed_accepted_index::SealedAcceptedIndexRootsV2 {
        self.roots
    }

    pub(crate) fn causal_tip_root(
        &self,
    ) -> tine_storage::sealed_accepted_index::AuthenticatedMapRootV1 {
        self.causal_tip_root
    }

    pub(crate) fn causal_tips(
        &self,
    ) -> impl Iterator<Item = &tine_storage::sealed_accepted_index::CausalTipRecordV2> {
        self.causal_tips.values()
    }

    pub(crate) fn builder<'a, Store>(
        &self,
        store: &'a mut Store,
    ) -> AcceptedCutoffBuilder<'a, Store>
    where
        Store: tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore,
    {
        AcceptedCutoffBuilder {
            store,
            cutoff: self.clone(),
        }
    }
}

pub(crate) struct AcceptedCutoffBuilder<'a, Store> {
    store: &'a mut Store,
    cutoff: SealedAcceptedCutoff,
}

impl<Store: tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore>
    AcceptedCutoffBuilder<'_, Store>
{
    pub(crate) fn append(&mut self, row: &CleanCheckpointAcceptedRow) -> Result<(), String> {
        use tine_storage::sealed_accepted_index::SealedAcceptedIndexReader;
        if row.evidence.prior_frontier_root() != &self.cutoff.frontier {
            return Err("sealed cutoff row does not extend its exact predecessor".into());
        }
        let batch_id = row.evidence.batch_id().as_uuid().into_bytes();
        if SealedAcceptedIndexReader::new(&*self.store)
            .map_value(self.cutoff.roots.batch_map, batch_id)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("sealed cutoff repeats an accepted batch".into());
        }
        let peer_id = row.causal_dot.peer_id().key().as_uuid().into_bytes();
        let prior_tip = self.cutoff.causal_tips.get(&peer_id).copied();
        let expected_tip = prior_tip
            .map(|tip| tip.value_digest())
            .transpose()
            .map_err(|error| error.to_string())?;
        if SealedAcceptedIndexReader::new(&*self.store)
            .map_value(self.cutoff.causal_tip_root, peer_id)
            .map_err(|error| error.to_string())?
            != expected_tip
        {
            return Err("sealed cutoff causal-tip predecessor does not authenticate".into());
        }
        let tip = tine_storage::sealed_accepted_index::CausalTipRecordV2 {
            peer_id,
            highest_accepted_counter: row.causal_dot.counter(),
            batch_id,
        };
        if prior_tip.is_some_and(|prior| {
            prior.highest_accepted_counter == tip.highest_accepted_counter
                && prior.batch_id != tip.batch_id
        }) {
            return Err("sealed cutoff has conflicting batches at one causal tip".into());
        }
        let advance_tip = prior_tip
            .is_none_or(|prior| prior.highest_accepted_counter < tip.highest_accepted_counter);
        // Publish immutable nodes first, but do not move any candidate roots
        // until every check passes. An interrupted/erroring append can leave
        // unreachable construction objects; predecessor roots still resolve.
        let roots = append_accepted_row(self.store, self.cutoff.roots, row)?;
        let frontier = row.evidence.post_frontier_root();
        if roots.batch_map.root.map(|link| link.key)
            != frontier.batch_map_root_key().map(AuthenticatedMapKey::from)
            || roots.batch_map.root_digest() != frontier.batch_map_root_digest()
            || roots.sequence.len != frontier.acceptance_sequence()
        {
            return Err("sealed cutoff causal membership differs from engine evidence".into());
        }
        let proof = SealedAcceptedIndexReader::new(&*self.store)
            .prove_membership(
                roots,
                row.evidence.acceptance_sequence(),
                batch_id,
                &TineAcceptedEvidenceDecoder,
            )
            .map_err(|error| error.to_string())?
            .ok_or("sealed cutoff membership is missing after publication")?;
        if proof.status.no_op != row.no_op
            || proof.status.exact_evidence_bytes
                != row
                    .evidence
                    .encode_canonical()
                    .map_err(|error| error.to_string())?
        {
            return Err("sealed cutoff status differs from engine acceptance".into());
        }
        let causal_tip_root = if advance_tip {
            tine_storage::sealed_accepted_index::SealedAcceptedIndexWriter::new(&mut *self.store)
                .upsert_map(
                    self.cutoff.causal_tip_root,
                    peer_id,
                    tip.value_digest().map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?
        } else {
            self.cutoff.causal_tip_root
        };
        let expected_tip = if advance_tip { Some(tip) } else { prior_tip }
            .ok_or("sealed cutoff causal tip disappeared")?;
        if SealedAcceptedIndexReader::new(&*self.store)
            .map_value(causal_tip_root, peer_id)
            .map_err(|error| error.to_string())?
            != Some(
                expected_tip
                    .value_digest()
                    .map_err(|error| error.to_string())?,
            )
        {
            return Err("sealed cutoff causal tip is missing after publication".into());
        }
        // Mutate only the builder's private token after all immutable writes
        // and point proofs succeed. The input predecessor remains unchanged.
        self.cutoff.frontier = frontier.clone();
        self.cutoff.roots = roots;
        self.cutoff.causal_tip_root = causal_tip_root;
        if advance_tip {
            self.cutoff.causal_tips.insert(peer_id, tip);
        }
        Ok(())
    }

    pub(crate) fn finish(
        self,
        expected: &AcceptedFrontierRoot,
    ) -> Result<SealedAcceptedCutoff, String> {
        if &self.cutoff.frontier != expected {
            return Err("sealed cutoff does not reach the requested engine frontier".into());
        }
        Ok(self.cutoff)
    }
}

/// One conversion from engine acceptance evidence to the shared sealed formats.
/// Both the disposable checkpoint and inert generation builder call this writer.
fn append_accepted_row<
    Store: tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore,
>(
    store: &mut Store,
    roots: tine_storage::sealed_accepted_index::SealedAcceptedIndexRootsV2,
    row: &CleanCheckpointAcceptedRow,
) -> Result<tine_storage::sealed_accepted_index::SealedAcceptedIndexRootsV2, String> {
    use tine_storage::sealed_accepted_index::{
        AcceptedSequenceEntryV2, AcceptedStatusRecordV2, SealedAcceptedCausalClockEntryV2,
        SealedAcceptedCausalRecordV2, SealedAcceptedIndexRootsV2, SealedAcceptedIndexWriter,
    };
    roots.validate_counts().map_err(|error| error.to_string())?;
    if roots.sequence.len.checked_add(1) != Some(row.evidence.acceptance_sequence()) {
        return Err("sealed accepted delta sequence is not contiguous".into());
    }
    let mut batch_map = roots.batch_map;
    let mut status_map = roots.status_map;
    let mut sequence_root = roots.sequence;
    let batch_id = row.evidence.batch_id().as_uuid().into_bytes();
    let causal = SealedAcceptedCausalRecordV2 {
        batch_id,
        manifest_fingerprint: row.evidence.manifest_fingerprint(),
        event_binding_digest: row.evidence.event_binding_digest(),
        causal_peer_id: row.causal_dot.peer_id().key().as_uuid().into_bytes(),
        causal_counter: row.causal_dot.counter(),
        canonical_causal_clock: row
            .canonical_causal_clock
            .iter()
            .map(|(peer, counter)| SealedAcceptedCausalClockEntryV2 {
                peer_id: peer.key().as_uuid().into_bytes(),
                counter: *counter,
            })
            .collect(),
    };
    let mut writer = SealedAcceptedIndexWriter::new(store);
    let causal_address = writer
        .publish_causal(&causal)
        .map_err(|error| error.to_string())?;
    let status = AcceptedStatusRecordV2 {
        batch_id,
        no_op: row.no_op,
        evidence_schema: ACCEPTED_EVIDENCE_SCHEMA_VERSION,
        exact_evidence_bytes: row
            .evidence
            .encode_canonical()
            .map_err(|error| error.to_string())?,
        accepted_causal_record_digest: causal_address,
    };
    let status_address = writer
        .publish_status(&status)
        .map_err(|error| error.to_string())?;
    batch_map = writer
        .upsert_map(batch_map, batch_id, causal_address)
        .map_err(|error| error.to_string())?;
    status_map = writer
        .upsert_map(status_map, batch_id, status_address)
        .map_err(|error| error.to_string())?;
    sequence_root = writer
        .append_sequence(
            sequence_root,
            AcceptedSequenceEntryV2 {
                sequence: row.evidence.acceptance_sequence(),
                batch_id,
                accepted_status_value_digest: status_address,
            },
        )
        .map_err(|error| error.to_string())?;
    let roots = SealedAcceptedIndexRootsV2 {
        batch_map,
        status_map,
        sequence: sequence_root,
    };
    roots.validate_counts().map_err(|error| error.to_string())?;
    Ok(roots)
}

fn build_payload(
    capture: CleanCheckpointCapture,
    predecessor: Option<(u64, CheckpointPayloadV1)>,
) -> Result<(u64, Vec<u8>), String> {
    use tine_storage::sealed_accepted_index::{AcceptedSequenceRootV2, AuthenticatedMapRootV1};

    let (mut store, mut batch_map, mut status_map, mut sequence_root, mut required_objects) =
        match predecessor {
            Some((sequence, payload)) => {
                if payload.schema_version != CHECKPOINT_SCHEMA_VERSION
                    || sequence < capture.base_sequence
                    || sequence > capture.target_sequence
                {
                    return Err("clean checkpoint predecessor frontier is incompatible".into());
                }
                let roots = roots_from_wire(payload.roster_roots)?;
                if roots.sequence.len != sequence {
                    return Err("clean checkpoint predecessor roster frontier differs".into());
                }
                (
                    CheckpointSealedStore {
                        objects: payload.sealed_objects,
                    },
                    roots.batch_map,
                    roots.status_map,
                    roots.sequence,
                    payload
                        .required_objects
                        .into_iter()
                        .collect::<BTreeSet<_>>(),
                )
            }
            None => {
                if capture.base_sequence != 0 {
                    return Err("clean checkpoint delta has no durable predecessor".into());
                }
                (
                    CheckpointSealedStore::default(),
                    AuthenticatedMapRootV1::empty(),
                    AuthenticatedMapRootV1::empty(),
                    AcceptedSequenceRootV2::empty(),
                    BTreeSet::new(),
                )
            }
        };
    for row in &capture.accepted_rows {
        if row.evidence.acceptance_sequence() <= sequence_root.len {
            continue;
        }
        let roots = append_accepted_row(
            &mut store,
            tine_storage::sealed_accepted_index::SealedAcceptedIndexRootsV2 {
                batch_map,
                status_map,
                sequence: sequence_root,
            },
            row,
        )?;
        batch_map = roots.batch_map;
        status_map = roots.status_map;
        sequence_root = roots.sequence;
    }
    let sequence = capture.target_sequence;
    if sequence_root.len != sequence {
        return Err("clean checkpoint delta does not reach its target frontier".into());
    }
    required_objects.extend(capture.required_objects.iter().copied());
    let roots = tine_storage::sealed_accepted_index::SealedAcceptedIndexRootsV2 {
        batch_map,
        status_map,
        sequence: sequence_root,
    };
    // The persistent writer path-copies authenticated nodes. Only the nodes
    // reachable from the final three roots belong in a disposable checkpoint;
    // retaining superseded construction nodes turns a linear roster into an
    // accidental O(N log N) payload. Drive the canonical shared reader across
    // every final membership proof and keep exactly what it actually reads.
    let recorder = RecordingCheckpointSealedStore::new(&store);
    let reader = tine_storage::sealed_accepted_index::SealedAcceptedIndexReader::new(&recorder);
    for expected_sequence in 1..=sequence {
        let entry = reader
            .sequence_entry(roots.sequence, expected_sequence)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "final clean checkpoint sequence is incomplete".to_owned())?;
        let proof = reader
            .prove_membership(
                roots,
                expected_sequence,
                entry.batch_id,
                &TineAcceptedEvidenceDecoder,
            )
            .map_err(|error| error.to_string())?;
        if proof.is_none() {
            return Err("final clean checkpoint roster membership is absent".into());
        }
    }
    let reachable = recorder.reads();
    drop(reader);
    drop(recorder);
    store.retain_only(&reachable);
    let payload = CheckpointPayloadV1 {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        state_bytes: capture.state_bytes,
        sealed_objects: store.objects,
        roster_roots: RosterRootsWire {
            batch_map: map_root_to_wire(roots.batch_map),
            status_map: map_root_to_wire(roots.status_map),
            sequence: SequenceRootWire {
                len: roots.sequence.len,
                height: roots.sequence.height,
                root_digest: roots.sequence.root_digest,
            },
        },
        required_objects: required_objects.into_iter().collect(),
        capture_work: capture.capture_work,
    };
    Ok((sequence, encode_canonical(&payload)?))
}

fn checkpoint_directory(store: &ObjectStore) -> Result<cap_std::fs::Dir, String> {
    let root = store
        .private_derived_root_capability()
        .map_err(|error| error.to_string())?;
    tine_storage::ensure_directory_nofollow(&root, CHECKPOINT_DIRECTORY)
        .map_err(|error| error.to_string())?;
    tine_storage::open_dir_nofollow(&root, CHECKPOINT_DIRECTORY).map_err(|error| error.to_string())
}

fn read_current_payload_for_extension(
    store: &ObjectStore,
) -> Result<Option<(u64, CheckpointPayloadV1)>, String> {
    let directory = checkpoint_directory(store)?;
    let Some(pointer_bytes) =
        tine_storage::read_optional_regular(&directory, CHECKPOINT_POINTER, 4 * 1024, None)
            .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let pointer: CheckpointPointerV1 = decode_canonical(&pointer_bytes)?;
    if pointer.schema_version != CHECKPOINT_SCHEMA_VERSION || pointer.slot >= 2 {
        return Err("clean checkpoint predecessor pointer is invalid".into());
    }
    let slot = pointer.slot as usize;
    let generation_bytes = tine_storage::read_optional_regular(
        &directory,
        CHECKPOINT_GENERATION_NAMES[slot],
        16 * 1024,
        None,
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "clean checkpoint predecessor generation is missing".to_owned())?;
    if ContentDigest::of(&generation_bytes) != pointer.generation_digest {
        return Err("clean checkpoint predecessor generation digest differs".into());
    }
    let generation: CheckpointGenerationV1 = decode_canonical(&generation_bytes)?;
    if generation.schema_version != CHECKPOINT_SCHEMA_VERSION
        || generation.slot != pointer.slot
        || generation.sequence != pointer.sequence
    {
        return Err("clean checkpoint predecessor generation is invalid".into());
    }
    let payload_bytes = tine_storage::read_optional_regular(
        &directory,
        CHECKPOINT_PAYLOAD_NAMES[slot],
        MAX_CHECKPOINT_BYTES,
        Some(generation.payload_len),
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "clean checkpoint predecessor payload is missing".to_owned())?;
    if ContentDigest::of(&payload_bytes) != generation.payload_digest {
        return Err("clean checkpoint predecessor payload digest differs".into());
    }
    let payload: CheckpointPayloadV1 = decode_canonical(&payload_bytes)?;
    if payload.schema_version != CHECKPOINT_SCHEMA_VERSION {
        return Err("clean checkpoint predecessor payload schema differs".into());
    }
    Ok(Some((generation.sequence, payload)))
}

fn install_replaceable_exact(
    directory: &cap_std::fs::Dir,
    publication: &tine_storage::DurableDirectoryPublication,
    name: &str,
    replacement: &[u8],
) -> Result<(), String> {
    match tine_storage::read_optional_regular(directory, name, MAX_CHECKPOINT_BYTES, None)
        .map_err(|error| error.to_string())?
    {
        Some(existing) if existing == replacement => Ok(()),
        Some(existing) => publication
            .replace_exact(name, &existing, replacement)
            .map_err(|error| error.to_string()),
        None => publication
            .publish_new_exact_single_writer(name, replacement)
            .map_err(|error| error.to_string()),
    }
}

fn publish_capture(store: &ObjectStore, capture: CleanCheckpointCapture) -> Result<u64, String> {
    #[cfg(test)]
    if FAIL_CHECKPOINT_WRITE_ROOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(store.root_path())
    {
        return Err("deterministic checkpoint publication failure".into());
    }
    let predecessor = read_current_payload_for_extension(store)?;
    let (sequence, payload_bytes) = build_payload(capture, predecessor)?;
    let payload_len = u64::try_from(payload_bytes.len())
        .map_err(|_| "clean checkpoint payload length exceeds u64".to_owned())?;
    if payload_len > MAX_CHECKPOINT_BYTES {
        return Err("clean checkpoint payload exceeds its disposable-cache limit".into());
    }
    let directory = checkpoint_directory(store)?;
    let publication = tine_storage::DurableDirectoryPublication::open(&directory)
        .map_err(|error| error.to_string())?;
    let prior_pointer_bytes =
        tine_storage::read_optional_regular(&directory, CHECKPOINT_POINTER, 4 * 1024, None)
            .map_err(|error| error.to_string())?;
    let prior_slot = prior_pointer_bytes
        .as_deref()
        .and_then(|bytes| decode_canonical::<CheckpointPointerV1>(bytes).ok())
        .filter(|pointer| pointer.schema_version == CHECKPOINT_SCHEMA_VERSION && pointer.slot < 2)
        .map(|pointer| pointer.slot as usize);
    let slot = prior_slot.map_or(0, |slot| 1 - slot);
    install_replaceable_exact(
        &directory,
        &publication,
        CHECKPOINT_PAYLOAD_NAMES[slot],
        &payload_bytes,
    )?;
    let generation = CheckpointGenerationV1 {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        sequence,
        slot: slot as u8,
        payload_digest: ContentDigest::of(&payload_bytes),
        payload_len,
    };
    let generation_bytes = encode_canonical(&generation)?;
    install_replaceable_exact(
        &directory,
        &publication,
        CHECKPOINT_GENERATION_NAMES[slot],
        &generation_bytes,
    )?;
    let pointer = CheckpointPointerV1 {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        sequence,
        slot: slot as u8,
        generation_digest: ContentDigest::of(&generation_bytes),
    };
    let pointer_bytes = encode_canonical(&pointer)?;
    match prior_pointer_bytes {
        Some(existing) if existing == pointer_bytes => {}
        Some(existing) => publication
            .replace_exact(CHECKPOINT_POINTER, &existing, &pointer_bytes)
            .map_err(|error| error.to_string())?,
        None => publication
            .publish_new_exact_single_writer(CHECKPOINT_POINTER, &pointer_bytes)
            .map_err(|error| error.to_string())?,
    }
    Ok(sequence)
}

pub(crate) enum CleanCheckpointOpen {
    Absent,
    Invalid(String),
    Loaded(CleanCheckpointLoaded),
}

pub(crate) struct CleanCheckpointLoaded {
    pub(crate) state_bytes: Vec<u8>,
    pub(crate) accepted_rows: Vec<CleanCheckpointAcceptedRow>,
    pub(crate) required_objects: BTreeSet<ContentDigest>,
    pub(crate) tail: BTreeSet<BatchId>,
    pub(crate) capture_work: u64,
    pub(crate) payload_bytes: usize,
}

#[derive(Debug)]
pub(crate) enum CleanCheckpointOpenError {
    ArchiveDamage(String),
    Store(String),
}

fn invalid(message: impl Into<String>) -> CleanCheckpointOpen {
    CleanCheckpointOpen::Invalid(message.into())
}

pub(crate) fn open_checkpoint(
    store: &ObjectStore,
) -> Result<CleanCheckpointOpen, CleanCheckpointOpenError> {
    use tine_storage::sealed_accepted_index::SealedAcceptedIndexReader;

    let root = store
        .private_derived_root_capability()
        .map_err(|error| CleanCheckpointOpenError::Store(error.to_string()))?;
    let Some(directory) = tine_storage::open_existing_dir_nofollow(&root, CHECKPOINT_DIRECTORY)
        .map_err(|error| CleanCheckpointOpenError::Store(error.to_string()))?
    else {
        return Ok(CleanCheckpointOpen::Absent);
    };
    let Some(pointer_bytes) =
        tine_storage::read_optional_regular(&directory, CHECKPOINT_POINTER, 4 * 1024, None)
            .map_err(|error| CleanCheckpointOpenError::Store(error.to_string()))?
    else {
        return Ok(CleanCheckpointOpen::Absent);
    };
    let pointer: CheckpointPointerV1 = match decode_canonical::<CheckpointPointerV1>(&pointer_bytes)
    {
        Ok(pointer) if pointer.schema_version == CHECKPOINT_SCHEMA_VERSION && pointer.slot < 2 => {
            pointer
        }
        Ok(_) | Err(_) => return Ok(invalid("clean checkpoint pointer is invalid")),
    };
    let slot = pointer.slot as usize;
    let generation_bytes = match tine_storage::read_optional_regular(
        &directory,
        CHECKPOINT_GENERATION_NAMES[slot],
        16 * 1024,
        None,
    )
    .map_err(|error| CleanCheckpointOpenError::Store(error.to_string()))?
    {
        Some(bytes) => bytes,
        None => return Ok(invalid("clean checkpoint generation is missing")),
    };
    if ContentDigest::of(&generation_bytes) != pointer.generation_digest {
        return Ok(invalid("clean checkpoint generation digest differs"));
    }
    let generation: CheckpointGenerationV1 =
        match decode_canonical::<CheckpointGenerationV1>(&generation_bytes) {
            Ok(generation)
                if generation.schema_version == CHECKPOINT_SCHEMA_VERSION
                    && generation.slot == pointer.slot
                    && generation.sequence == pointer.sequence =>
            {
                generation
            }
            Ok(_) | Err(_) => return Ok(invalid("clean checkpoint generation is invalid")),
        };
    let payload_bytes = match tine_storage::read_optional_regular(
        &directory,
        CHECKPOINT_PAYLOAD_NAMES[slot],
        MAX_CHECKPOINT_BYTES,
        Some(generation.payload_len),
    )
    .map_err(|error| CleanCheckpointOpenError::Store(error.to_string()))?
    {
        Some(bytes) => bytes,
        None => return Ok(invalid("clean checkpoint payload is missing")),
    };
    if ContentDigest::of(&payload_bytes) != generation.payload_digest {
        return Ok(invalid("clean checkpoint payload digest differs"));
    }
    let payload: CheckpointPayloadV1 = match decode_canonical::<CheckpointPayloadV1>(&payload_bytes)
    {
        Ok(payload) if payload.schema_version == CHECKPOINT_SCHEMA_VERSION => payload,
        Ok(_) | Err(_) => return Ok(invalid("clean checkpoint payload is invalid")),
    };
    if let Some((kind, _)) = payload
        .sealed_objects
        .keys()
        .find(|(kind, _)| sealed_kind_from_code(*kind).is_err())
    {
        return Ok(invalid(format!(
            "clean checkpoint has unknown sealed kind {kind}"
        )));
    }
    let sealed_store = CheckpointSealedStore {
        objects: payload.sealed_objects,
    };
    let roots = match roots_from_wire(payload.roster_roots) {
        Ok(roots) => roots,
        Err(error) => return Ok(invalid(error)),
    };
    if roots.sequence.len != generation.sequence {
        return Ok(invalid("clean checkpoint roster sequence differs"));
    }
    let status_addresses = match sealed_store.collect_map(roots.status_map) {
        Ok(rows) => rows,
        Err(error) => return Ok(invalid(error)),
    };
    let causal_addresses = match sealed_store.collect_map(roots.batch_map) {
        Ok(rows) => rows,
        Err(error) => return Ok(invalid(error)),
    };
    let reader = SealedAcceptedIndexReader::new(&sealed_store);
    let mut accepted_rows = Vec::new();
    let mut roster = BTreeSet::new();
    for sequence in 1..=roots.sequence.len {
        let entry = match reader.sequence_entry(roots.sequence, sequence) {
            Ok(Some(entry)) => entry,
            Ok(None) | Err(_) => return Ok(invalid("clean checkpoint sequence is incomplete")),
        };
        let Some(status_address) = status_addresses
            .get(&AuthenticatedMapKey::from(entry.batch_id))
            .copied()
        else {
            return Ok(invalid("clean checkpoint sequence names no status"));
        };
        let status = match tine_storage::sealed_accepted_index::AcceptedStatusRecordV2::decode(
            entry.batch_id,
            status_address,
            match sealed_store.required_bytes(
                tine_storage::sealed_accepted_index::SealedAcceptedObjectKind::StatusRecord,
                status_address,
            ) {
                Ok(bytes) => bytes,
                Err(error) => return Ok(invalid(error)),
            },
        ) {
            Ok(status) if entry.accepted_status_value_digest == status.value_digest() => status,
            Ok(_) | Err(_) => return Ok(invalid("clean checkpoint status binding failed")),
        };
        let Some(causal_address) = causal_addresses
            .get(&AuthenticatedMapKey::from(entry.batch_id))
            .copied()
        else {
            return Ok(invalid("clean checkpoint sequence names no causal record"));
        };
        if causal_address != status.accepted_causal_record_digest {
            return Ok(invalid("clean checkpoint status/causal binding failed"));
        }
        let causal = match tine_storage::sealed_accepted_index::SealedAcceptedCausalRecordV2::decode(
            entry.batch_id,
            causal_address,
            match sealed_store.required_bytes(
                tine_storage::sealed_accepted_index::SealedAcceptedObjectKind::CausalRecord,
                causal_address,
            ) {
                Ok(bytes) => bytes,
                Err(error) => return Ok(invalid(error)),
            },
        ) {
            Ok(causal) => causal,
            Err(_) => return Ok(invalid("clean checkpoint causal binding failed")),
        };
        let evidence = match AcceptedBatchEvidence::decode_canonical(&status.exact_evidence_bytes) {
            Ok(evidence)
                if evidence.batch_id().as_uuid().into_bytes() == entry.batch_id
                    && evidence.acceptance_sequence() == sequence
                    && evidence.manifest_fingerprint() == causal.manifest_fingerprint
                    && evidence.event_binding_digest() == causal.event_binding_digest =>
            {
                evidence
            }
            Err(_) => return Ok(invalid("clean checkpoint evidence is invalid")),
            Ok(_) => return Ok(invalid("clean checkpoint evidence binding failed")),
        };
        let peer = CausalPeerId::from_key(WriterIncarnationId::from_uuid(uuid::Uuid::from_bytes(
            causal.causal_peer_id,
        )));
        let causal_dot = match BatchCausalDot::new(peer, causal.causal_counter) {
            Ok(dot) => dot,
            Err(_) => return Ok(invalid("clean checkpoint causal dot is invalid")),
        };
        let canonical_causal_clock = causal
            .canonical_causal_clock
            .iter()
            .map(|entry| {
                (
                    CausalPeerId::from_key(WriterIncarnationId::from_uuid(uuid::Uuid::from_bytes(
                        entry.peer_id,
                    ))),
                    entry.counter,
                )
            })
            .collect();
        let batch_id = evidence.batch_id();
        roster.insert(batch_id);
        accepted_rows.push(CleanCheckpointAcceptedRow {
            no_op: status.no_op,
            evidence,
            causal_dot,
            canonical_causal_clock,
        });
    }
    if roster.len() != accepted_rows.len()
        || status_addresses.len() != accepted_rows.len()
        || causal_addresses.len() != accepted_rows.len()
    {
        return Ok(invalid("clean checkpoint roster maps and sequence differ"));
    }

    let required_objects = payload
        .required_objects
        .into_iter()
        .collect::<BTreeSet<_>>();
    let (tail, missing_manifest, missing_object) = store
        .checkpoint_namespace_delta(&roster, &required_objects)
        .map_err(|error| CleanCheckpointOpenError::Store(error.to_string()))?;
    // Archive damage is a refusal of the authoritative tail, not of the
    // disposable checkpoint: the accepted roster proves this manifest was
    // published, so its absence is a torn/partial delivery or media loss.
    // Carry the scenario marker so the public open classifies durably
    // (`MS-REF-DISK-CORRUPT`) instead of surfacing as an unmarked retryable
    // dead end (wave-2 review A5-2).
    if let Some(missing) = missing_manifest {
        return Err(CleanCheckpointOpenError::ArchiveDamage(format!(
            "accepted checkpoint roster manifest {missing} is missing from the archive [{}]",
            crate::oplog::refusal::ManagedStorageRefusalScenario::DiskCorrupt.as_str()
        )));
    }
    let live_fingerprints = store
        .validated_manifest_fingerprints()
        .map_err(|error| CleanCheckpointOpenError::Store(error.to_string()))?;
    for row in &accepted_rows {
        if live_fingerprints.get(&row.evidence.batch_id())
            != Some(&row.evidence.manifest_fingerprint())
        {
            return Ok(invalid(format!(
                "accepted checkpoint roster manifest {} was mutated",
                row.evidence.batch_id()
            )));
        }
    }
    if let Some(missing) = missing_object {
        return Err(CleanCheckpointOpenError::ArchiveDamage(format!(
            "accepted checkpoint roster object {missing} is missing from the archive [{}]",
            crate::oplog::refusal::ManagedStorageRefusalScenario::DiskCorrupt.as_str()
        )));
    }
    let payload_size = payload_bytes.len();
    Ok(CleanCheckpointOpen::Loaded(CleanCheckpointLoaded {
        state_bytes: payload.state_bytes,
        accepted_rows,
        required_objects,
        tail,
        capture_work: payload.capture_work,
        payload_bytes: payload_size,
    }))
}

struct PublisherState {
    in_flight: bool,
    queued: Option<CleanCheckpointCapture>,
}

struct PublisherInner {
    store: Arc<ObjectStore>,
    state: Mutex<PublisherState>,
    finished: Condvar,
    durable_sequence: AtomicU64,
    elevated_rewrite_observed: AtomicBool,
}

pub(crate) struct CleanCheckpointPublisher {
    inner: Arc<PublisherInner>,
}

impl CleanCheckpointPublisher {
    pub(crate) fn new(store: ObjectStore, durable_sequence: u64) -> Self {
        Self {
            inner: Arc::new(PublisherInner {
                store: Arc::new(store),
                state: Mutex::new(PublisherState {
                    in_flight: false,
                    queued: None,
                }),
                finished: Condvar::new(),
                durable_sequence: AtomicU64::new(durable_sequence),
                elevated_rewrite_observed: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn enqueue(&self, capture: CleanCheckpointCapture) {
        let sequence = capture.target_sequence;
        if sequence.saturating_sub(self.durable_sequence()) > CLEAN_CHECKPOINT_LAG_MAX {
            self.inner
                .elevated_rewrite_observed
                .store(true, Ordering::Release);
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.in_flight {
            if state
                .queued
                .as_ref()
                .is_none_or(|queued| queued.target_sequence <= capture.target_sequence)
            {
                state.queued = Some(capture);
            }
            return;
        }
        state.in_flight = true;
        drop(state);
        let inner = Arc::clone(&self.inner);
        let spawn = std::thread::Builder::new()
            .name("tine-clean-checkpoint".into())
            .spawn(move || publisher_loop(inner, capture));
        if let Err(error) = spawn {
            eprintln!("clean checkpoint writer could not start: {error}");
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.in_flight = false;
            self.inner.finished.notify_all();
        }
    }

    pub(crate) fn durable_sequence(&self) -> u64 {
        self.inner.durable_sequence.load(Ordering::Acquire)
    }

    pub(crate) fn durable_lag(&self, accepted_sequence: u64) -> u64 {
        accepted_sequence.saturating_sub(self.durable_sequence())
    }

    #[cfg(test)]
    pub(crate) fn elevated_rewrite_observed(&self) -> bool {
        self.inner.elevated_rewrite_observed.load(Ordering::Acquire)
    }
}

impl Drop for CleanCheckpointPublisher {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.in_flight {
            state = self
                .inner
                .finished
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

fn publisher_loop(inner: Arc<PublisherInner>, mut capture: CleanCheckpointCapture) {
    loop {
        match publish_capture(&inner.store, capture) {
            Ok(sequence) => inner.durable_sequence.store(sequence, Ordering::Release),
            Err(error) => {
                eprintln!("clean checkpoint write failed; retrying at the next trigger: {error}")
            }
        }
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(next) = state.queued.take() else {
            state.in_flight = false;
            inner.finished.notify_all();
            return;
        };
        capture = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::hot_engine::{
        accepted_causal_record_digest, authenticated_causal_clock_root, AcceptedFrontierRoot,
    };
    use crate::oplog::{
        BatchCausalDot, BatchId, CausalPeerId, ContentDigest, DeviceId, DocumentKey,
    };
    use tine_storage::sealed_accepted_index::{
        AcceptedSequenceEntryV2, AcceptedSequenceRootV2, AcceptedStatusRecordV2,
        AuthenticatedMapRootV1, SealedAcceptedCausalClockEntryV2, SealedAcceptedCausalRecordV2,
        SealedAcceptedEvidenceDecoder, SealedAcceptedIndexObjectStore, SealedAcceptedIndexReader,
        SealedAcceptedIndexRootsV2, SealedAcceptedIndexWriter, SealedAcceptedObjectKind,
    };

    #[derive(Default)]
    struct SealedMemoryStore {
        objects: Vec<(SealedAcceptedObjectKind, ContentDigest, Vec<u8>)>,
        reads: RefCell<Vec<(u8, ContentDigest)>>,
    }

    impl SealedAcceptedIndexObjectStore for SealedMemoryStore {
        fn read_sealed_accepted_object(
            &self,
            kind: SealedAcceptedObjectKind,
            address: ContentDigest,
        ) -> Result<Option<Vec<u8>>, tine_storage::sealed_accepted_index::SealedAcceptedIndexError>
        {
            self.reads
                .borrow_mut()
                .push((sealed_kind_code(kind), address));
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
            kind: SealedAcceptedObjectKind,
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

    fn evidence() -> AcceptedBatchEvidence {
        let batch_id = BatchId::from_uuid(uuid::Uuid::from_bytes([0x51; 16]));
        AcceptedBatchEvidence::for_test(
            batch_id,
            digest(0x61),
            digest(0x71),
            AcceptedFrontierRoot::empty(),
            Vec::new(),
            Vec::new(),
            vec![(batch_id, digest(0x81))],
            0,
        )
    }

    fn evidence_after(prior: &AcceptedBatchEvidence) -> AcceptedBatchEvidence {
        let first = prior.batch_id();
        let batch_id = BatchId::from_uuid(uuid::Uuid::from_bytes([0x52; 16]));
        AcceptedBatchEvidence::for_test(
            batch_id,
            digest(0x62),
            digest(0x72),
            prior.post_frontier_root().clone(),
            Vec::new(),
            Vec::new(),
            vec![(first, digest(0x81)), (batch_id, digest(0x82))],
            0,
        )
    }

    fn generation_rows(count: u64) -> Vec<CleanCheckpointAcceptedRow> {
        generation_rows_with_dots(&(1..=count).map(|counter| (19, counter)).collect::<Vec<_>>())
    }

    /// Explicit, stable fixture writer incarnation. Fixtures have no durable
    /// writer-lane record to read a real one from; production always does.
    fn fixture_incarnation(seed: u128) -> CausalPeerId {
        CausalPeerId::from_key(WriterIncarnationId::fixture_for_device(
            DeviceId::from_uuid(uuid::Uuid::from_u128(seed)),
        ))
    }

    fn generation_rows_with_dots(dots: &[(u128, u64)]) -> Vec<CleanCheckpointAcceptedRow> {
        let mut prior = AcceptedFrontierRoot::empty();
        let mut entries = Vec::new();
        dots.iter()
            .enumerate()
            .map(|(index, &(peer_id, counter))| {
                let sequence = index as u64 + 1;
                let peer = CausalPeerId::from_key(WriterIncarnationId::from_uuid(
                    uuid::Uuid::from_u128(peer_id),
                ));
                let batch_id = BatchId::from_uuid(uuid::Uuid::from_u128(sequence as u128));
                let fingerprint = ContentDigest::of(&sequence.to_le_bytes());
                let event = ContentDigest::of(&sequence.to_be_bytes());
                let dot = BatchCausalDot::new(peer, counter).unwrap();
                let clock = vec![(peer, counter)];
                let (key, digest) = authenticated_causal_clock_root(&clock).unwrap();
                let causal =
                    accepted_causal_record_digest(batch_id, fingerprint, event, dot, key, digest);
                entries.push((batch_id, causal));
                let evidence = AcceptedBatchEvidence::for_test(
                    batch_id,
                    fingerprint,
                    event,
                    prior.clone(),
                    Vec::new(),
                    Vec::new(),
                    entries.clone(),
                    0,
                );
                prior = evidence.post_frontier_root().clone();
                CleanCheckpointAcceptedRow {
                    no_op: sequence % 2 == 0,
                    evidence,
                    causal_dot: dot,
                    canonical_causal_clock: clock,
                }
            })
            .collect()
    }

    #[test]
    #[ignore = "manual architecture census: fixed live graph with increasing create/delete history"]
    fn rebaselining_constant_live_churn_census() {
        use crate::oplog::hot_engine::{LazyGenesisCheckpointBuilder, ShardedHotEngine};
        use crate::oplog::lazy_genesis::LazyGenesisPackBuilder;
        use crate::oplog::{
            AuthorBatch, BatchDisposition, BlobDescription, BlockId, BlockLocation, CrdtPeerId,
            DocumentId, LineageDigest, LogicalPageName, ManagedPath, ManagedTextKind,
            OperationTransaction, PageId, SemanticOperation, SessionId, WorkspaceId,
        };
        let root = std::env::temp_dir().join(format!("tine-churn-census-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = WorkspaceId::from_uuid(uuid::Uuid::from_u128(101));
        let lineage = LineageDigest::of(b"sealed-cutoff-engine");
        let catalog = DocumentId::from_uuid(uuid::Uuid::from_u128(102));
        let (checkpoint, dependencies) = LazyGenesisCheckpointBuilder::new(catalog)
            .unwrap()
            .finish()
            .unwrap();
        let baseline = Arc::new(
            LazyGenesisPackBuilder::new(
                workspace,
                lineage,
                catalog,
                BlobDescription::of(b"empty source"),
                &root,
            )
            .unwrap()
            .finish(checkpoint, dependencies)
            .unwrap(),
        );
        let archive = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        let mut engine = ShardedHotEngine::new(workspace, lineage, catalog);
        engine
            .install_lazy_genesis_baseline(Arc::clone(&baseline))
            .unwrap();
        engine
            .attach_clean_archive_store(archive.duplicate_retained_capability().unwrap())
            .unwrap();
        let claims = engine
            .clean_transient_projection_claim_snapshot()
            .unwrap()
            .unwrap();
        let commit = |engine: &mut ShardedHotEngine, sequence: u128, operations| {
            let transaction = OperationTransaction::new(operations).unwrap();
            let prepared = engine
                .prepare_fixture_transaction(
                    AuthorBatch {
                        batch_id: BatchId::from_uuid(uuid::Uuid::from_u128(100_000 + sequence)),
                        author_device_id: DeviceId::from_uuid(uuid::Uuid::from_u128(500)),
                        author_session_id: SessionId::from_uuid(uuid::Uuid::from_u128(501)),
                        crdt_peer_id: CrdtPeerId::from_u64(502),
                        causal_peer_id: fixture_incarnation(500),
                    },
                    &transaction,
                )
                .unwrap();
            let result = engine
                .commit_clean_prepared(&prepared, claims.as_ref())
                .unwrap();
            assert!(
                matches!(result.disposition(), BatchDisposition::Accepted { .. }),
                "{:?}",
                result.disposition()
            );
        };
        let create = |n: u128| {
            vec![
                SemanticOperation::CreatePage {
                    page_id: PageId::from_uuid(uuid::Uuid::from_u128(200_000 + n)),
                    home_document_id: DocumentId::from_uuid(uuid::Uuid::from_u128(300_000 + n)),
                    name: LogicalPageName::parse(format!("Churn {n}")).unwrap(),
                    path: ManagedPath::parse(format!("pages/Churn{n}.md")).unwrap(),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: BlockId::from_uuid(uuid::Uuid::from_u128(400_000 + n)),
                        home_document_id: DocumentId::from_uuid(uuid::Uuid::from_u128(300_000 + n)),
                    },
                    page_id: PageId::from_uuid(uuid::Uuid::from_u128(200_000 + n)),
                    parent: None,
                    order: "a".into(),
                    content: "Stable probe content".repeat(8),
                },
            ]
        };
        commit(&mut engine, 1, create(0));
        let live = engine.canonical_snapshot().unwrap();
        assert_eq!(live.pages.len(), 1);
        assert_eq!(live.blocks.len(), 1);
        let mut nodes = SealedMemoryStore::default();
        let mut cutoff = None;
        let capsule_root = root.join("live-closure-capsules");
        std::fs::create_dir(&capsule_root).unwrap();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&capsule_root, cap_std::ambient_authority())
                .unwrap();
        let mut previous_closure = None;

        for n in 1..=128_u128 {
            commit(&mut engine, 2 * n, create(n));
            commit(
                &mut engine,
                2 * n + 1,
                vec![SemanticOperation::DeletePage {
                    page_id: PageId::from_uuid(uuid::Uuid::from_u128(200_000 + n)),
                }],
            );
            if [8, 32, 128].contains(&n) {
                assert_eq!(engine.canonical_snapshot().unwrap(), live);
                let current = engine
                    .build_sealed_accepted_cutoff(&mut nodes, cutoff.as_ref())
                    .unwrap();
                let compact = engine
                    .build_compact_accepted_document(&current, catalog)
                    .unwrap();
                let pruned_bytes = crate::oplog::hot_engine::probe_pruned_checkpoint_bytes(
                    catalog,
                    compact.dependencies(),
                    &compact.checkpoint().to_vec(),
                )
                .unwrap();
                let closure = engine
                    .capture_live_graph_document_closure(&current)
                    .unwrap();
                assert_eq!(closure.document_count(), 2);
                assert!(closure.contains(catalog));
                assert!(closure.contains(DocumentId::from_uuid(uuid::Uuid::from_u128(300_000))));
                let mut staging = SealedGenerationStagingStore::open(&directory).unwrap();
                if let Some(previous) = &previous_closure {
                    assert!(engine
                        .build_live_graph_document_roster(&current, &mut staging, previous)
                        .is_err());
                }
                let active = engine
                    .build_live_graph_document_roster(&current, &mut staging, &closure)
                    .unwrap();
                assert_eq!(active.document_count(), 2);
                let disk = staging.finish().unwrap();
                engine
                    .qualify_live_graph_document_roster(&current, active, &disk, &closure)
                    .unwrap();
                assert!(engine
                    .qualify_full_document_roster(&current, active, &disk)
                    .is_err());
                drop(disk);
                eprintln!("rebaselining_churn cycles={n} live_pages=1 live_blocks=1 accepted={} accepted_documents={} compact_catalog_bytes={} live_capsules=2 experimental_pruned_bytes={pruned_bytes}",
                    current.frontier().acceptance_sequence(), current.frontier().document_count(), compact.checkpoint().len());
                previous_closure = Some(closure);
                cutoff = Some(current);
            }
        }
        // A bounded document count is insufficient too: churn inside one
        // still-live home shard can retain deleted block state and text.
        let page = PageId::from_uuid(uuid::Uuid::from_u128(200_000));
        let home = DocumentId::from_uuid(uuid::Uuid::from_u128(300_000));
        for n in 1..=128_u128 {
            let block_id = BlockId::from_uuid(uuid::Uuid::from_u128(500_000 + n));
            commit(
                &mut engine,
                1_000 + 2 * n,
                vec![SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id: home,
                    },
                    page_id: page,
                    parent: None,
                    order: "b".into(),
                    content: "Deleted block probe content".repeat(8),
                }],
            );
            commit(
                &mut engine,
                1_001 + 2 * n,
                vec![SemanticOperation::DeleteSubtree {
                    root_block_id: block_id,
                    page_id: page,
                }],
            );
            if [8, 32, 128].contains(&n) {
                assert_eq!(engine.canonical_snapshot().unwrap(), live);
                let current = engine
                    .build_sealed_accepted_cutoff(&mut nodes, cutoff.as_ref())
                    .unwrap();
                let compact = engine
                    .build_compact_accepted_document(&current, home)
                    .unwrap();
                let pruned_bytes = crate::oplog::hot_engine::probe_pruned_checkpoint_bytes(
                    catalog,
                    compact.dependencies(),
                    &compact.checkpoint().to_vec(),
                )
                .unwrap();
                let closure = engine
                    .capture_live_graph_document_closure(&current)
                    .unwrap();
                assert_eq!(closure.document_count(), 2);
                eprintln!("rebaselining_block_churn cycles={n} live_pages=1 live_blocks=1 accepted={} accepted_documents={} compact_home_bytes={} live_capsules=2 experimental_pruned_bytes={pruned_bytes}",
                    current.frontier().acceptance_sequence(), current.frontier().document_count(), compact.checkpoint().len());
                cutoff = Some(current);
            }
        }
        drop(directory);
        drop(engine);
        drop(archive);
        drop(baseline);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn sealed_cutoff_streams_real_engine_evidence_and_matches_clean_replay() {
        use crate::oplog::hot_engine::{LazyGenesisCheckpointBuilder, ShardedHotEngine};
        use crate::oplog::lazy_genesis::LazyGenesisPackBuilder;
        use crate::oplog::BlobDescription;
        use crate::oplog::{
            AuthorBatch, BatchDisposition, CrdtPeerId, DocumentId, LineageDigest, LogicalPageName,
            ManagedPath, ManagedTextKind, OperationTransaction, PageId, SemanticOperation,
            SessionId, WorkspaceId,
        };
        let root =
            std::env::temp_dir().join(format!("tine-sealed-cutoff-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = WorkspaceId::from_uuid(uuid::Uuid::from_u128(101));
        let lineage = LineageDigest::of(b"sealed-cutoff-engine");
        let catalog = DocumentId::from_uuid(uuid::Uuid::from_u128(102));
        let (checkpoint, dependencies) = LazyGenesisCheckpointBuilder::new(catalog)
            .unwrap()
            .finish()
            .unwrap();
        let baseline = Arc::new(
            LazyGenesisPackBuilder::new(
                workspace,
                lineage,
                catalog,
                BlobDescription::of(b"empty source"),
                &root,
            )
            .unwrap()
            .finish(checkpoint, dependencies)
            .unwrap(),
        );
        let archive = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        let mut engine = ShardedHotEngine::new(workspace, lineage, catalog);
        engine
            .install_lazy_genesis_baseline(Arc::clone(&baseline))
            .unwrap();
        engine
            .attach_clean_archive_store(archive.duplicate_retained_capability().unwrap())
            .unwrap();
        let claims = engine
            .clean_transient_projection_claim_snapshot()
            .unwrap()
            .unwrap();
        let mut store = SealedMemoryStore::default();
        let mut cutoff = engine
            .build_sealed_accepted_cutoff(&mut store, None)
            .unwrap();
        assert_eq!(cutoff.roots().sequence.len, 0);
        let capsule_root = root.join("capsules");
        std::fs::create_dir(&capsule_root).unwrap();
        let capsule_dir =
            cap_std::fs::Dir::open_ambient_dir(&capsule_root, cap_std::ambient_authority())
                .unwrap();
        let mut roster = SealedDocumentRoster::empty();
        let mut empty_store = SealedGenerationStagingStore::open(&capsule_dir).unwrap();
        let (empty_roster, empty_written) = engine
            .build_compact_document_roster(&cutoff, &mut empty_store, None)
            .unwrap();
        assert_eq!(empty_written, cutoff.frontier().document_count());
        let empty_disk = empty_store.finish().unwrap();
        engine
            .qualify_full_document_roster(&cutoff, empty_roster, &empty_disk)
            .unwrap();
        drop(empty_disk);

        for n in 1..=2 {
            let transaction = OperationTransaction::new(vec![
                SemanticOperation::CreatePage {
                    page_id: PageId::from_uuid(uuid::Uuid::from_u128(200 + n)),
                    home_document_id: DocumentId::from_uuid(uuid::Uuid::from_u128(300 + n)),
                    name: LogicalPageName::parse(format!("Page {n}")).unwrap(),
                    path: ManagedPath::parse(format!("pages/Page{n}.md")).unwrap(),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: crate::oplog::BlockLocation {
                        block_id: crate::oplog::BlockId::from_uuid(uuid::Uuid::from_u128(600 + n)),
                        home_document_id: DocumentId::from_uuid(uuid::Uuid::from_u128(300 + n)),
                    },
                    page_id: PageId::from_uuid(uuid::Uuid::from_u128(200 + n)),
                    parent: None,
                    order: "a".into(),
                    content: format!("Nested CRDT text {n}"),
                },
            ])
            .unwrap();
            let prepared = engine
                .prepare_fixture_transaction(
                    AuthorBatch {
                        batch_id: BatchId::from_uuid(uuid::Uuid::from_u128(400 + n)),
                        author_device_id: DeviceId::from_uuid(uuid::Uuid::from_u128(500)),
                        author_session_id: SessionId::from_uuid(uuid::Uuid::from_u128(501)),
                        crdt_peer_id: CrdtPeerId::from_u64(502),
                        causal_peer_id: fixture_incarnation(500),
                    },
                    &transaction,
                )
                .unwrap();
            let outcome = engine
                .commit_clean_prepared(&prepared, claims.as_ref())
                .unwrap();
            assert!(
                matches!(outcome.disposition(), BatchDisposition::Accepted { .. }),
                "{:?}",
                outcome.disposition()
            );
            assert!(engine
                .build_compact_accepted_document(&cutoff, catalog)
                .is_err());
            let before = engine.capture_clean_checkpoint(0).unwrap().state_bytes;
            let manifests = archive.committed_manifest_names().unwrap();
            cutoff = engine
                .build_sealed_accepted_cutoff(&mut store, Some(&cutoff))
                .unwrap();
            assert_eq!(cutoff.roots().sequence.len, n as u64);
            let retained = engine
                .build_policy_compact_accepted_document(
                    &cutoff,
                    catalog,
                    0,
                    crate::oplog::checkpoint_floor_policy::FloorPolicyConfig::default(),
                )
                .unwrap();
            let crate::oplog::checkpoint_floor_policy::LoroFloorDecision::Keep {
                retained: catalog_image,
                metrics: catalog_metrics,
                work: catalog_work,
            } = retained.decision()
            else {
                panic!("a fresh default-budget catalog must retain full history")
            };
            assert!(!catalog_image.checkpoint.is_empty());
            assert_eq!(catalog_image.actual_floor, loro::Frontiers::default());
            assert!(catalog_metrics.image_bytes > 0);
            assert_eq!(catalog_work.measurement_exports, 2);
            assert_eq!(
                retained.cutoff_state_digest(),
                cutoff.frontier().state_digest()
            );
            assert_eq!(retained.dependencies().document_id(), catalog);
            let page_retained = engine
                .build_policy_compact_accepted_document(
                    &cutoff,
                    DocumentId::from_uuid(uuid::Uuid::from_u128(300 + n)),
                    0,
                    crate::oplog::checkpoint_floor_policy::FloorPolicyConfig::default(),
                )
                .unwrap();
            let crate::oplog::checkpoint_floor_policy::LoroFloorDecision::Keep {
                retained: page_image,
                ..
            } = page_retained.decision()
            else {
                panic!("a fresh default-budget page must retain full history")
            };
            assert!(!page_image.checkpoint.is_empty());
            let previous_roster = roster;
            let mut capsule_store = SealedGenerationStagingStore::open(&capsule_dir).unwrap();
            for id in [
                catalog,
                DocumentId::from_uuid(uuid::Uuid::from_u128(300 + n)),
            ] {
                let compact = engine.build_compact_accepted_document(&cutoff, id).unwrap();
                assert_eq!(
                    compact.cutoff_state_digest(),
                    cutoff.frontier().state_digest()
                );
                assert_eq!(compact.dependencies().document_id(), id);
                let restored = loro::LoroDoc::new();
                assert!(restored
                    .import(compact.checkpoint())
                    .unwrap()
                    .pending
                    .is_none());
                assert!(!compact.checkpoint().is_empty());
                roster = roster
                    .with_document(&mut capsule_store, &cutoff, &compact)
                    .unwrap();
                let record = DocumentCapsuleRecord {
                    schema: DOCUMENT_CAPSULE_SCHEMA,
                    dependencies: compact.dependencies().clone(),
                    checkpoint: BlobDescription::of(compact.checkpoint()),
                };
                let canonical = record.encode().unwrap();
                assert_eq!(DocumentCapsuleRecord::decode(&canonical).unwrap(), record);
                let mut trailing = canonical.clone();
                trailing.push(0);
                assert!(DocumentCapsuleRecord::decode(&trailing).is_err());
                let mut wrong_schema = record;
                wrong_schema.schema += 1;
                assert!(DocumentCapsuleRecord::decode(&wrong_schema.encode().unwrap()).is_err());
            }
            drop(capsule_store.finish().unwrap());
            let reopened_capsules = SealedGenerationDirectory::open(&capsule_dir).unwrap();
            for id in std::iter::once(catalog)
                .chain((1..=n).map(|i| DocumentId::from_uuid(uuid::Uuid::from_u128(300 + i))))
            {
                let (dependencies, restored) = roster
                    .load_document(&reopened_capsules, catalog, id)
                    .unwrap()
                    .unwrap();
                let compact = engine.build_compact_accepted_document(&cutoff, id).unwrap();
                let expected = super::super::hot_engine::qualify_compact_document(
                    catalog,
                    compact.dependencies(),
                    &compact.checkpoint().to_vec(),
                )
                .unwrap();
                assert_eq!(&dependencies, compact.dependencies());
                assert_eq!(restored.get_deep_value(), expected.get_deep_value());
                assert_eq!(restored.oplog_frontiers(), expected.oplog_frontiers());
            }
            assert_eq!(roster.document_count(), n as u64 + 1);
            if n == 2 {
                let id = DocumentId::from_uuid(uuid::Uuid::from_u128(301));
                let old = previous_roster
                    .load_document(&reopened_capsules, catalog, id)
                    .unwrap()
                    .unwrap();
                let new = roster
                    .load_document(&reopened_capsules, catalog, id)
                    .unwrap()
                    .unwrap();
                assert_eq!(old.0, new.0);
                assert_eq!(old.1.get_deep_value(), new.1.get_deep_value());
            }
            let mut complete_store = SealedGenerationStagingStore::open(&capsule_dir).unwrap();
            let (automatic, written) = engine
                .build_compact_document_roster(
                    &cutoff,
                    &mut complete_store,
                    if n == 1 { None } else { Some(previous_roster) },
                )
                .unwrap();
            assert_eq!(written, 2, "only catalog plus new page need compaction");
            assert_eq!(automatic.map, roster.map);
            drop(complete_store.finish().unwrap());
            engine
                .qualify_full_document_roster(&cutoff, automatic, &reopened_capsules)
                .unwrap();
            assert!(engine
                .qualify_full_document_roster(
                    &cutoff,
                    SealedDocumentRoster::empty(),
                    &reopened_capsules
                )
                .is_err());
            let mut unchanged = SealedGenerationStagingStore::open(&capsule_dir).unwrap();
            let (same, written) = engine
                .build_compact_document_roster(&cutoff, &mut unchanged, Some(automatic))
                .unwrap();
            assert_eq!(written, 0);
            assert_eq!(same.map, automatic.map);
            assert!(unchanged.publication.is_none());
            assert!(unchanged.pending.objects.is_empty());
            unchanged.failed = true;
            assert!(engine
                .build_compact_document_roster(&cutoff, &mut unchanged, Some(automatic))
                .is_err());
            assert!(unchanged.finish().is_err());

            let address = SealedAcceptedIndexReader::new(&reopened_capsules)
                .map_value(
                    roster.map.entity_root(),
                    DocumentKey::Entity(catalog).authenticated_map_key(),
                )
                .unwrap()
                .unwrap();
            let path = capsule_root.join(capsule_blob_name(address));
            let exact = std::fs::read(&path).unwrap();
            let mut extra_store = SealedGenerationStagingStore::open(&capsule_dir).unwrap();
            let extra_id = DocumentId::from_uuid(uuid::Uuid::from_u128(888_888));
            let mut extra_root = SealedAcceptedIndexWriter::new(&mut extra_store)
                .upsert_map(
                    roster.map.entity_root(),
                    DocumentKey::Entity(extra_id).authenticated_map_key(),
                    address,
                )
                .unwrap();
            drop(extra_store.finish().unwrap());
            extra_root.count = roster.document_count(); // count alone must not certify completeness
            assert!(engine
                .qualify_full_document_roster(
                    &cutoff,
                    SealedDocumentRoster {
                        map: roster.map.with_entity_root_for_test(extra_root)
                    },
                    &reopened_capsules
                )
                .is_err());

            let record = DocumentCapsuleRecord::decode(&exact).unwrap();
            let checkpoint_path = capsule_root.join(capsule_blob_name(ContentDigest::from_bytes(
                *record.checkpoint.sha256(),
            )));
            let exact_checkpoint = std::fs::read(&checkpoint_path).unwrap();
            std::fs::write(&checkpoint_path, b"torn checkpoint").unwrap();
            assert!(roster
                .load_document(&reopened_capsules, catalog, catalog)
                .is_err());
            std::fs::write(&checkpoint_path, &exact_checkpoint).unwrap();
            std::fs::write(&path, b"torn descriptor").unwrap();
            assert!(roster
                .load_document(&reopened_capsules, catalog, catalog)
                .is_err());
            std::fs::remove_file(&path).unwrap();
            assert!(roster
                .load_document(&reopened_capsules, catalog, catalog)
                .is_err());
            std::fs::write(&path, &exact).unwrap();
            assert!(roster
                .load_document(&reopened_capsules, catalog, catalog)
                .unwrap()
                .is_some());
            // Validly addressed but semantically wrong bytes must not qualify.
            let mut malformed = SealedGenerationStagingStore::open(&capsule_dir).unwrap();
            let bad_checkpoint = malformed
                .stage_capsule_blob(b"not a CRDT checkpoint")
                .unwrap();
            let bad_record = DocumentCapsuleRecord {
                checkpoint: bad_checkpoint,
                ..record.clone()
            };
            let bad_blob = malformed
                .stage_capsule_blob(&bad_record.encode().unwrap())
                .unwrap();
            let bad_root = SealedAcceptedIndexWriter::new(&mut malformed)
                .upsert_map(
                    roster.map.entity_root(),
                    DocumentKey::Entity(catalog).authenticated_map_key(),
                    ContentDigest::from_bytes(*bad_blob.sha256()),
                )
                .unwrap();
            let mut wrong_vector = record.dependencies.peer_counters().to_vec();
            wrong_vector.push(crate::oplog::CrdtPeerCounter::new(
                CrdtPeerId::from_u64(999_999),
                0,
            ));
            let wrong_dependencies = DocumentDependencies::new(
                catalog,
                wrong_vector,
                record.dependencies.direct_dependency_heads().to_vec(),
            )
            .unwrap();
            let wrong_record = DocumentCapsuleRecord {
                dependencies: wrong_dependencies,
                ..record.clone()
            };
            let wrong_blob = malformed
                .stage_capsule_blob(&wrong_record.encode().unwrap())
                .unwrap();
            let wrong_root = SealedAcceptedIndexWriter::new(&mut malformed)
                .upsert_map(
                    roster.map.entity_root(),
                    DocumentKey::Entity(catalog).authenticated_map_key(),
                    ContentDigest::from_bytes(*wrong_blob.sha256()),
                )
                .unwrap();
            drop(malformed.finish().unwrap());
            assert!(SealedDocumentRoster {
                map: roster.map.with_entity_root_for_test(bad_root)
            }
            .load_document(&reopened_capsules, catalog, catalog)
            .is_err());
            assert!(SealedDocumentRoster {
                map: roster.map.with_entity_root_for_test(wrong_root)
            }
            .load_document(&reopened_capsules, catalog, catalog)
            .is_err());
            assert!(roster
                .load_document(&reopened_capsules, catalog, catalog)
                .unwrap()
                .is_some());
            drop(reopened_capsules);
            assert_eq!(cutoff.frontier(), &engine.accepted_frontier_root().unwrap());
            assert_eq!(archive.committed_manifest_names().unwrap(), manifests);
            assert_eq!(
                engine.capture_clean_checkpoint(0).unwrap().state_bytes,
                before
            );
        }
        let mut replay = ShardedHotEngine::new(workspace, lineage, catalog);
        replay.install_lazy_genesis_baseline(baseline).unwrap();
        replay
            .attach_clean_archive_store(archive.duplicate_retained_capability().unwrap())
            .unwrap();
        assert_eq!(
            replay.replay_clean_committed_tail(claims.as_ref()).unwrap(),
            2
        );
        let independent = replay
            .build_sealed_accepted_cutoff(&mut SealedMemoryStore::default(), None)
            .unwrap();
        assert_eq!(cutoff.roots(), independent.roots());
        assert_eq!(cutoff.frontier(), independent.frontier());
        let mut disk = SealedGenerationStagingStore::open(&capsule_dir).unwrap();
        let mut full_roster = SealedDocumentRoster::empty();
        for id in [
            catalog,
            DocumentId::from_uuid(uuid::Uuid::from_u128(301)),
            DocumentId::from_uuid(uuid::Uuid::from_u128(302)),
        ] {
            let compact = replay
                .build_compact_accepted_document(&independent, id)
                .unwrap();
            full_roster = full_roster
                .with_document(&mut disk, &independent, &compact)
                .unwrap();
        }
        assert_eq!(full_roster.map, roster.map);
        drop(disk.finish().unwrap());
        drop(capsule_dir);
        drop(replay);
        drop(engine);
        drop(archive);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn rebaselining_reconstructs_real_engine_ancestry_for_two_returning_peers() {
        use crate::oplog::hot_engine::{LazyGenesisCheckpointBuilder, ShardedHotEngine};
        use crate::oplog::lazy_genesis::LazyGenesisPackBuilder;
        use crate::oplog::{
            AuthorBatch, BatchDisposition, BlobDescription, BlockId, BlockLocation, CrdtPeerId,
            DocumentId, LineageDigest, LogicalPageName, ManagedPath, ManagedTextKind,
            OperationTransaction, PageId, SemanticOperation, SessionId, WorkspaceId,
        };
        let root = std::env::temp_dir().join(format!(
            "tine-rebaseline-returning-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = WorkspaceId::from_uuid(uuid::Uuid::from_u128(1101));
        let lineage = LineageDigest::of(b"real-engine-returning-peers");
        let catalog = DocumentId::from_uuid(uuid::Uuid::from_u128(1102));
        let home = DocumentId::from_uuid(uuid::Uuid::from_u128(1103));
        let page = PageId::from_uuid(uuid::Uuid::from_u128(1104));
        let destination = PageId::from_uuid(uuid::Uuid::from_u128(1106));
        let destination_home = DocumentId::from_uuid(uuid::Uuid::from_u128(1107));
        let block = BlockLocation {
            block_id: BlockId::from_uuid(uuid::Uuid::from_u128(1105)),
            home_document_id: home,
        };
        let (bytes, dependencies) = LazyGenesisCheckpointBuilder::new(catalog)
            .unwrap()
            .finish()
            .unwrap();
        let baseline = Arc::new(
            LazyGenesisPackBuilder::new(
                workspace,
                lineage,
                catalog,
                BlobDescription::of(b"empty source"),
                &root,
            )
            .unwrap()
            .finish(bytes, dependencies)
            .unwrap(),
        );
        // One scratch archive collects the exact immutable bytes accepted by
        // the simulated devices. No production inbound publication is implied.
        let archive = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        let fresh = || {
            let mut engine = ShardedHotEngine::new(workspace, lineage, catalog);
            engine
                .install_lazy_genesis_baseline(Arc::clone(&baseline))
                .unwrap();
            engine
                .attach_clean_archive_store(archive.duplicate_retained_capability().unwrap())
                .unwrap();
            engine
        };
        let mut engine = fresh();
        let claims = engine
            .clean_transient_projection_claim_snapshot()
            .unwrap()
            .unwrap();
        let author = |batch: u128, peer: u64| AuthorBatch {
            batch_id: BatchId::from_uuid(uuid::Uuid::from_u128(batch)),
            author_device_id: DeviceId::from_uuid(uuid::Uuid::from_u128(peer as u128)),
            author_session_id: SessionId::from_uuid(uuid::Uuid::from_u128(peer as u128 + 1)),
            crdt_peer_id: CrdtPeerId::from_u64(peer),
            causal_peer_id: fixture_incarnation(peer as u128),
        };
        let commit = |engine: &mut ShardedHotEngine, batch, peer, operations| {
            let transaction = OperationTransaction::new(operations).unwrap();
            let prepared = engine
                .prepare_fixture_transaction(author(batch, peer), &transaction)
                .unwrap();
            let outcome = engine
                .commit_clean_prepared(&prepared, claims.as_ref())
                .unwrap();
            assert!(
                matches!(outcome.disposition(), BatchDisposition::Accepted { .. }),
                "{:?}",
                outcome.disposition()
            );
        };
        commit(
            &mut engine,
            1,
            100,
            vec![
                SemanticOperation::CreatePage {
                    page_id: page,
                    home_document_id: home,
                    name: LogicalPageName::parse("Recovery").unwrap(),
                    path: ManagedPath::parse("pages/Recovery.md").unwrap(),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreatePage {
                    page_id: destination,
                    home_document_id: destination_home,
                    name: LogicalPageName::parse("Destination").unwrap(),
                    path: ManagedPath::parse("pages/Destination.md").unwrap(),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block,
                    page_id: page,
                    parent: None,
                    order: "a".into(),
                    content: "root".into(),
                },
            ],
        );
        let mut offline_a = fresh();
        let mut offline_b = fresh();
        for offline in [&mut offline_a, &mut offline_b] {
            assert_eq!(
                offline
                    .replay_clean_committed_tail(claims.as_ref())
                    .unwrap(),
                1
            );
        }
        commit(
            &mut engine,
            2,
            100,
            vec![
                SemanticOperation::MoveSubtree {
                    root: block,
                    from_page_id: page,
                    to_page_id: destination,
                    parent: None,
                    order: "a".into(),
                },
                SemanticOperation::EditBlockContent {
                    block,
                    content: "root MAIN".into(),
                },
            ],
        );
        let content = |engine: &ShardedHotEngine| {
            engine
                .canonical_snapshot()
                .unwrap()
                .blocks
                .into_iter()
                .find(|state| state.block_id == block.block_id)
                .unwrap()
                .content
        };
        let mut accepted = BTreeSet::from([author(1, 100).batch_id, author(2, 100).batch_id]);
        for (round, offline, offline_peer, offline_label, tail_label) in [
            (0, &mut offline_a, 200, "OFFLINE_A", "TAIL"),
            (1, &mut offline_b, 300, "OFFLINE_B", "NEXT"),
        ] {
            let cutoff = engine
                .build_sealed_accepted_cutoff(&mut SealedMemoryStore::default(), None)
                .unwrap();
            let compact = engine
                .build_compact_accepted_document(&cutoff, home)
                .unwrap();
            let compact_bytes = compact.checkpoint().to_vec();
            let tail_id = 3 + round * 2;
            let incoming_id = tail_id + 1;
            let updated = format!("{} {tail_label}", content(&engine));
            commit(
                &mut engine,
                tail_id,
                100,
                vec![SemanticOperation::EditBlockContent {
                    block,
                    content: updated,
                }],
            );
            accepted.insert(author(tail_id, 100).batch_id);
            let acknowledged = engine.canonical_snapshot().unwrap();
            let acknowledged_root = engine.accepted_frontier_root().unwrap();
            commit(
                offline,
                incoming_id,
                offline_peer,
                vec![SemanticOperation::EditBlockContent {
                    block,
                    content: format!("root {offline_label}"),
                }],
            );

            // Reconstruct *all* acknowledged ancestry in an isolated engine,
            // including the tail accepted after C. Then use the production
            // admission path for the returning batch, not a shallow import.
            let mut recovered = fresh();
            assert_eq!(
                recovered
                    .replay_clean_checkpoint_tail(&accepted, claims.as_ref())
                    .unwrap(),
                accepted.len()
            );
            assert_eq!(recovered.canonical_snapshot().unwrap(), acknowledged);
            assert_eq!(
                recovered.accepted_frontier_root().unwrap(),
                acknowledged_root
            );
            let incoming = BTreeSet::from([author(incoming_id, offline_peer).batch_id]);
            assert_eq!(
                recovered
                    .replay_clean_checkpoint_tail(&incoming, claims.as_ref())
                    .unwrap(),
                1
            );
            assert_eq!(
                engine.canonical_snapshot().unwrap(),
                acknowledged,
                "isolated recovery mutated live state"
            );
            assert_eq!(
                compact.checkpoint(),
                compact_bytes,
                "old compact bytes changed"
            );
            for preserved in ["MAIN", "TAIL", offline_label] {
                assert!(
                    content(&recovered).contains(preserved),
                    "recovery lost {preserved}"
                );
            }
            if round == 1 {
                assert!(content(&recovered).contains("OFFLINE_A"));
                assert!(content(&recovered).contains("NEXT"));
            }
            let snapshot = recovered.canonical_snapshot().unwrap();
            let membership = snapshot
                .memberships
                .iter()
                .find(|entry| entry.block_id == block.block_id)
                .unwrap();
            assert_eq!(membership.page_id, destination);
            assert_eq!(membership.home_document_id, home);
            assert_eq!(
                snapshot
                    .blocks
                    .iter()
                    .find(|entry| entry.block_id == block.block_id)
                    .unwrap()
                    .home_document_id,
                home
            );

            // Negative control: rebuilding only through C and then accepting
            // the offline branch loses the acknowledged post-C tail. The
            // recovery protocol must explicitly carry that tail forward.
            let mut omitted_tail = fresh();
            let mut incomplete = accepted.clone();
            incomplete.remove(&author(tail_id, 100).batch_id);
            omitted_tail
                .replay_clean_checkpoint_tail(&incomplete, claims.as_ref())
                .unwrap();
            omitted_tail
                .replay_clean_checkpoint_tail(&incoming, claims.as_ref())
                .unwrap();
            assert!(!content(&omitted_tail).contains(tail_label));
            assert_ne!(omitted_tail.canonical_snapshot().unwrap(), snapshot);
            accepted.extend(incoming);
            let next = recovered
                .build_sealed_accepted_cutoff(&mut SealedMemoryStore::default(), None)
                .unwrap();
            for id in [catalog, home, destination_home] {
                let next_compact = recovered
                    .build_compact_accepted_document(&next, id)
                    .unwrap();
                assert_eq!(next_compact.dependencies().document_id(), id);
            }
            assert_eq!(next.roots().sequence.len as usize, accepted.len());
            // Only the test's engine handle moves here. Durable marker/actor
            // installation remains a separate production qualification gate.
            engine = recovered;
        }
        let mut oracle = fresh();
        assert_eq!(
            oracle.replay_clean_committed_tail(claims.as_ref()).unwrap(),
            6
        );
        assert_eq!(
            oracle.canonical_snapshot().unwrap(),
            engine.canonical_snapshot().unwrap()
        );
        assert_eq!(
            oracle.accepted_frontier_root().unwrap(),
            engine.accepted_frontier_root().unwrap()
        );
        assert_eq!(archive.committed_manifest_names().unwrap(), accepted);
        // Delete the now-empty original page. Its immutable home still owns
        // the moved block and must survive even though it is not a live page.
        commit(
            &mut engine,
            7,
            100,
            vec![SemanticOperation::DeletePage { page_id: page }],
        );
        assert_eq!(engine.canonical_snapshot().unwrap().pages.len(), 1);
        assert!(content(&engine).contains("OFFLINE_A"));
        assert!(content(&engine).contains("OFFLINE_B"));
        let final_cutoff = engine
            .build_sealed_accepted_cutoff(&mut SealedMemoryStore::default(), None)
            .unwrap();
        let capsule_root = root.join("retained-home-capsules");
        std::fs::create_dir(&capsule_root).unwrap();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&capsule_root, cap_std::ambient_authority())
                .unwrap();
        let mut staging = SealedGenerationStagingStore::open(&directory).unwrap();
        let (complete, written) = engine
            .build_compact_document_roster(&final_cutoff, &mut staging, None)
            .unwrap();
        assert_eq!(written, 3);
        let disk = staging.finish().unwrap();
        engine
            .qualify_full_document_roster(&final_cutoff, complete, &disk)
            .unwrap();
        assert!(complete
            .load_document(&disk, catalog, home)
            .unwrap()
            .is_some());
        let closure = engine
            .capture_live_graph_document_closure(&final_cutoff)
            .unwrap();
        assert_eq!(closure.document_count(), 3);
        assert!(closure.contains(home));
        let mut live_store = SealedGenerationStagingStore::open(&directory).unwrap();
        let live_roster = engine
            .build_live_graph_document_roster(&final_cutoff, &mut live_store, &closure)
            .unwrap();
        drop(live_store.finish().unwrap());
        engine
            .qualify_live_graph_document_roster(&final_cutoff, live_roster, &disk, &closure)
            .unwrap();
        let mut incomplete_store = SealedGenerationStagingStore::open(&directory).unwrap();
        let mut visible_only = SealedDocumentRoster::empty();
        for id in [catalog, destination_home] {
            let compact = engine
                .build_compact_accepted_document(&final_cutoff, id)
                .unwrap();
            visible_only = visible_only
                .with_document(&mut incomplete_store, &final_cutoff, &compact)
                .unwrap();
        }
        drop(incomplete_store.finish().unwrap());
        assert!(engine
            .qualify_full_document_roster(&final_cutoff, visible_only, &disk)
            .is_err());
        assert!(engine
            .qualify_live_graph_document_roster(&final_cutoff, visible_only, &disk, &closure,)
            .is_err());
        drop(disk);
        drop(directory);
        drop(oracle);
        drop(offline_a);
        drop(offline_b);
        drop(engine);
        drop(archive);
        drop(baseline);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn sealed_directory_roundtrip_and_incremental_roots_match_memory_oracle() {
        let root =
            std::env::temp_dir().join(format!("tine-sealed-directory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
        let rows = generation_rows(17);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut staging = SealedGenerationStagingStore::open(&directory).unwrap();
        staging.batch_byte_budget = 4096;
        staging.batch_object_budget = 8;
        let mut cutoff = empty.clone();
        for row in &rows[..16] {
            let mut builder = cutoff.builder(&mut staging);
            builder.append(row).unwrap();
            cutoff = builder.finish(row.evidence.post_frontier_root()).unwrap();
            assert!(staging.pending_bytes < staging.batch_byte_budget);
            assert!(staging.pending.objects.len() < staging.batch_object_budget);
        }
        let prefix = cutoff.clone();
        drop(staging.finish().unwrap());
        let reopened = SealedGenerationDirectory::open(&directory).unwrap();
        let reader = SealedAcceptedIndexReader::new(&reopened);
        for row in &rows[..16] {
            let proof = reader
                .prove_membership(
                    prefix.roots(),
                    row.evidence.acceptance_sequence(),
                    row.evidence.batch_id().as_uuid().into_bytes(),
                    &TineAcceptedEvidenceDecoder,
                )
                .unwrap()
                .unwrap();
            assert_eq!(proof.status.no_op, row.no_op);
            assert_eq!(
                proof.status.exact_evidence_bytes,
                row.evidence.encode_canonical().unwrap()
            );
        }
        let mut extension = SealedGenerationStagingStore::open(&directory).unwrap();
        let mut builder = prefix.builder(&mut extension);
        builder.append(&rows[16]).unwrap();
        let complete = builder
            .finish(rows[16].evidence.post_frontier_root())
            .unwrap();
        drop(extension.finish().unwrap());
        let mut memory = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut memory);
        for row in &rows {
            builder.append(row).unwrap();
        }
        let expected = builder
            .finish(rows[16].evidence.post_frontier_root())
            .unwrap();
        assert_eq!(complete.roots(), expected.roots());
        assert_eq!(complete.causal_tip_root(), expected.causal_tip_root());
        // The same pre-existing reader can still resolve immutable predecessor
        // roots after a later generation adds nodes in the same object store.
        assert!(reader
            .prove_membership(
                prefix.roots(),
                16,
                16u128.to_be_bytes(),
                &TineAcceptedEvidenceDecoder
            )
            .unwrap()
            .is_some());
        assert!(reader
            .prove_membership(
                complete.roots(),
                17,
                17u128.to_be_bytes(),
                &TineAcceptedEvidenceDecoder
            )
            .unwrap()
            .is_some());
        // Exercise the immediate backend on the Linux host as well. Native
        // Windows/Android barriers still require their platform qualification.
        let immediate_root = root.join("immediate");
        std::fs::create_dir(&immediate_root).unwrap();
        let immediate_dir =
            cap_std::fs::Dir::open_ambient_dir(&immediate_root, cap_std::ambient_authority())
                .unwrap();
        let mut immediate = SealedGenerationStagingStore::open(&immediate_dir).unwrap();
        immediate.publication =
            Some(SealedStagingPublication::open_immediate(&immediate_dir).unwrap());
        immediate.batch_byte_budget = usize::MAX;
        immediate.batch_object_budget = usize::MAX;
        let mut builder = empty.builder(&mut immediate);
        for row in &rows {
            builder.append(row).unwrap();
        }
        let actual = builder
            .finish(rows[16].evidence.post_frontier_root())
            .unwrap();
        assert_eq!(actual.roots(), expected.roots());
        assert_eq!(actual.causal_tip_root(), expected.causal_tip_root());
        let immediate = immediate.finish().unwrap();
        for row in &rows {
            assert!(SealedAcceptedIndexReader::new(&immediate)
                .prove_membership(
                    actual.roots(),
                    row.evidence.acceptance_sequence(),
                    row.evidence.batch_id().as_uuid().into_bytes(),
                    &TineAcceptedEvidenceDecoder,
                )
                .unwrap()
                .is_some());
        }
        let mut publication = SealedStagingPublication::open_immediate(&immediate_dir).unwrap();
        publication
            .publish(&immediate_dir, "collision", b"original")
            .unwrap();
        assert!(publication
            .publish(&immediate_dir, "collision", b"different")
            .is_err());
        assert_eq!(
            std::fs::read(immediate_root.join("collision")).unwrap(),
            b"original"
        );
        drop(publication);
        drop(immediate);
        drop(immediate_dir);
        drop(reopened);
        drop(directory);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn sealed_directory_collision_or_corruption_never_changes_predecessor_authority() {
        use tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore;
        let root =
            std::env::temp_dir().join(format!("tine-sealed-damage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
        let row = generation_rows(1).remove(0);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut staging = SealedGenerationStagingStore::open(&directory).unwrap();
        let mut builder = empty.builder(&mut staging);
        builder.append(&row).unwrap();
        let cutoff = builder.finish(row.evidence.post_frontier_root()).unwrap();
        let mut reopened = staging.finish().unwrap();
        let address = cutoff.roots().batch_map.root.unwrap().digest;
        let kind = SealedAcceptedObjectKind::MapNode;
        let original = reopened
            .read_sealed_accepted_object(kind, address)
            .unwrap()
            .unwrap();
        assert!(reopened
            .publish_sealed_accepted_object(kind, address, b"wrong")
            .is_err());
        let mut collision = SealedGenerationStagingStore::open(&directory).unwrap();
        assert!(collision
            .publish_sealed_accepted_object(kind, address, b"wrong")
            .is_err());
        assert!(collision.finish().is_err());
        assert_eq!(
            reopened
                .read_sealed_accepted_object(kind, address)
                .unwrap()
                .unwrap(),
            original
        );
        let name = sealed_staging_name(kind, address);
        std::fs::write(root.join(&name), b"torn node").unwrap();
        assert!(SealedAcceptedIndexReader::new(&reopened)
            .prove_membership(
                cutoff.roots(),
                1,
                1u128.to_be_bytes(),
                &TineAcceptedEvidenceDecoder
            )
            .is_err());
        std::fs::remove_file(root.join(&name)).unwrap();
        assert!(reopened
            .read_sealed_accepted_object(kind, address)
            .unwrap()
            .is_none());
        #[cfg(unix)]
        {
            let outside = root.join("outside-node");
            std::fs::write(&outside, &original).unwrap();
            std::os::unix::fs::symlink(&outside, root.join(&name)).unwrap();
            assert!(reopened.read_sealed_accepted_object(kind, address).is_err());
            std::fs::remove_file(root.join(&name)).unwrap();
            std::fs::remove_file(outside).unwrap();
        }
        std::fs::write(root.join(&name), original).unwrap();
        assert!(SealedAcceptedIndexReader::new(&reopened)
            .prove_membership(
                cutoff.roots(),
                1,
                1u128.to_be_bytes(),
                &TineAcceptedEvidenceDecoder
            )
            .unwrap()
            .is_some());
        drop(reopened);
        drop(directory);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn sealed_directory_publication_fault_can_retry_without_replacing_predecessor() {
        let root = std::env::temp_dir().join(format!("tine-sealed-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
        let rows = generation_rows(2);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut first = SealedGenerationStagingStore::open(&directory).unwrap();
        let mut builder = empty.builder(&mut first);
        builder.append(&rows[0]).unwrap();
        let prefix = builder
            .finish(rows[0].evidence.post_frontier_root())
            .unwrap();
        let predecessor_reader = first.finish().unwrap();
        let mut interrupted = SealedGenerationStagingStore::open(&directory).unwrap();
        let mut builder = prefix.builder(&mut interrupted);
        builder.append(&rows[1]).unwrap();
        let candidate = builder
            .finish(rows[1].evidence.post_frontier_root())
            .unwrap();
        let ((kind, address), original) = interrupted
            .pending
            .objects
            .iter()
            .find(|((kind, _), _)| {
                *kind == sealed_kind_code(SealedAcceptedObjectKind::StatusRecord)
            })
            .expect("the extension has a new status record");
        let original = original.clone();
        let collision = root.join(sealed_staging_name(
            sealed_kind_from_code(*kind).unwrap(),
            *address,
        ));
        let installed_during_publish = collision.exists();
        std::fs::write(&collision, b"torn publication").unwrap();
        let finished = interrupted.finish();
        if installed_during_publish {
            // Some platforms install each immutable file during publish. A
            // later disk fault is detected by fresh canonical qualification,
            // not by treating the batch's durability receipt as integrity.
            if let Ok(reader) = finished {
                assert!(SealedAcceptedIndexReader::new(&reader)
                    .prove_membership(
                        candidate.roots(),
                        2,
                        2u128.to_be_bytes(),
                        &TineAcceptedEvidenceDecoder
                    )
                    .is_err());
            }
            std::fs::write(&collision, original).unwrap();
        } else {
            assert!(
                finished.is_err(),
                "a different exact-byte winner must refuse deferred installation"
            );
            std::fs::remove_file(collision).unwrap();
        }
        let reader = SealedAcceptedIndexReader::new(&predecessor_reader);
        assert!(reader
            .prove_membership(
                prefix.roots(),
                1,
                1u128.to_be_bytes(),
                &TineAcceptedEvidenceDecoder
            )
            .unwrap()
            .is_some());
        let mut retry = SealedGenerationStagingStore::open(&directory).unwrap();
        let mut builder = prefix.builder(&mut retry);
        builder.append(&rows[1]).unwrap();
        let retried = builder
            .finish(rows[1].evidence.post_frontier_root())
            .unwrap();
        let complete = retry.finish().unwrap();
        assert_eq!(retried.roots(), candidate.roots());
        assert_eq!(retried.causal_tip_root(), candidate.causal_tip_root());
        assert!(SealedAcceptedIndexReader::new(&complete)
            .prove_membership(
                retried.roots(),
                2,
                2u128.to_be_bytes(),
                &TineAcceptedEvidenceDecoder
            )
            .unwrap()
            .is_some());
        drop(complete);
        drop(predecessor_reader);
        drop(directory);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn sealed_cutoff_incremental_build_matches_independent_full_rederivation() {
        let rows = generation_rows(65);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut store = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut store);
        for row in &rows[..64] {
            builder.append(row).unwrap();
        }
        let first = builder
            .finish(rows[63].evidence.post_frontier_root())
            .unwrap();
        let mut builder = first.builder(&mut store);
        builder.append(&rows[64]).unwrap();
        let second = builder
            .finish(rows[64].evidence.post_frontier_root())
            .unwrap();

        let mut independent = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut independent);
        for row in &rows {
            builder.append(row).unwrap();
        }
        let full = builder
            .finish(rows[64].evidence.post_frontier_root())
            .unwrap();
        assert_eq!(second.roots(), full.roots());
        assert_eq!(second.frontier(), full.frontier());
        assert_eq!(second.causal_tip_root(), full.causal_tip_root());
        assert_eq!(
            second.causal_tips().collect::<Vec<_>>(),
            full.causal_tips().collect::<Vec<_>>()
        );
        let reader = SealedAcceptedIndexReader::new(&store);
        for row in &rows {
            let sequence = row.evidence.acceptance_sequence();
            let id = row.evidence.batch_id().as_uuid().into_bytes();
            let proof = reader
                .prove_membership(second.roots(), sequence, id, &TineAcceptedEvidenceDecoder)
                .unwrap()
                .unwrap();
            assert_eq!(proof.status.no_op, row.no_op);
            assert_eq!(
                proof.status.exact_evidence_bytes,
                row.evidence.encode_canonical().unwrap()
            );
            if sequence <= 64 {
                assert!(reader
                    .prove_membership(first.roots(), sequence, id, &TineAcceptedEvidenceDecoder)
                    .unwrap()
                    .is_some());
            }
        }
    }

    #[test]
    fn sealed_cutoff_causal_tips_keep_exact_highest_per_peer_and_reject_tip_forks() {
        let rows = generation_rows_with_dots(&[(19, 1), (23, 8), (19, 2), (23, 3)]);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut store = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut store);
        for row in &rows[..3] {
            builder.append(row).unwrap();
        }
        let first = builder
            .finish(rows[2].evidence.post_frontier_root())
            .unwrap();
        let mut builder = first.builder(&mut store);
        builder.append(&rows[3]).unwrap();
        let second = builder
            .finish(rows[3].evidence.post_frontier_root())
            .unwrap();
        // An older accepted counter must not replace the already qualified tip.
        assert_eq!(second.causal_tip_root(), first.causal_tip_root());
        let tips = second.causal_tips().copied().collect::<Vec<_>>();
        assert_eq!(tips.len(), 2);
        assert_eq!(
            (tips[0].highest_accepted_counter, tips[0].batch_id),
            (2, 3u128.to_be_bytes())
        );
        assert_eq!(
            (tips[1].highest_accepted_counter, tips[1].batch_id),
            (8, 2u128.to_be_bytes())
        );
        assert_eq!(second.causal_tip_root().count, 2);
        let reader = SealedAcceptedIndexReader::new(&store);
        for tip in &tips {
            assert_eq!(
                reader
                    .map_value(second.causal_tip_root(), tip.peer_id)
                    .unwrap(),
                Some(tip.value_digest().unwrap())
            );
        }
        let unchanged = second
            .builder(&mut store)
            .finish(second.frontier())
            .unwrap();
        assert_eq!(unchanged.causal_tip_root(), second.causal_tip_root());
        assert_eq!(
            unchanged.causal_tips().collect::<Vec<_>>(),
            second.causal_tips().collect::<Vec<_>>()
        );

        let forks = generation_rows_with_dots(&[(19, 1), (19, 1)]);
        let mut fork_store = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut fork_store);
        builder.append(&forks[0]).unwrap();
        assert!(builder
            .append(&forks[1])
            .unwrap_err()
            .contains("conflicting batches"));
        let preserved = builder
            .finish(forks[0].evidence.post_frontier_root())
            .unwrap();
        assert_eq!(preserved.roots().sequence.len, 1);
        assert_eq!(
            preserved.causal_tips().next().unwrap().batch_id,
            1u128.to_be_bytes()
        );
    }

    #[test]
    fn sealed_cutoff_damaged_causal_tip_predecessor_preserves_cutoff() {
        let rows = generation_rows(2);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut store = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut store);
        builder.append(&rows[0]).unwrap();
        let first = builder
            .finish(rows[0].evidence.post_frontier_root())
            .unwrap();
        let address = first.causal_tip_root().root.unwrap().digest;
        let slot = store
            .objects
            .iter()
            .position(|(kind, digest, _)| {
                *kind == SealedAcceptedObjectKind::MapNode && *digest == address
            })
            .unwrap();
        let original = store.objects[slot].2.clone();
        store.objects[slot].2 = vec![0xff];
        let mut builder = first.builder(&mut store);
        assert!(builder.append(&rows[1]).is_err());
        let preserved = builder.finish(first.frontier()).unwrap();
        assert_eq!(preserved.roots(), first.roots());
        assert_eq!(preserved.causal_tip_root(), first.causal_tip_root());
        store.objects[slot].2 = original;
        let mut builder = preserved.builder(&mut store);
        builder.append(&rows[1]).unwrap();
        assert_eq!(
            builder
                .finish(rows[1].evidence.post_frontier_root())
                .unwrap()
                .causal_tips()
                .next()
                .unwrap()
                .highest_accepted_counter,
            2
        );
    }

    #[test]
    fn sealed_cutoff_one_row_delta_does_not_visit_historical_status_or_sequence_leaves() {
        let rows = generation_rows(513);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut store = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut store);
        for row in &rows[..512] {
            builder.append(row).unwrap();
        }
        let first = builder
            .finish(rows[511].evidence.post_frontier_root())
            .unwrap();
        store.reads.borrow_mut().clear();
        let mut builder = first.builder(&mut store);
        builder.append(&rows[512]).unwrap();
        let second = builder
            .finish(rows[512].evidence.post_frontier_root())
            .unwrap();
        let reads = store.reads.borrow().clone();
        assert!(
            reads.len() < 256,
            "one-row append enumerated retained history: {} reads",
            reads.len()
        );
        assert_eq!(
            reads
                .iter()
                .filter(
                    |(kind, _)| *kind == sealed_kind_code(SealedAcceptedObjectKind::StatusRecord)
                )
                .count(),
            1
        );
        assert_eq!(
            reads
                .iter()
                .filter(
                    |(kind, _)| *kind == sealed_kind_code(SealedAcceptedObjectKind::SequenceLeaf)
                )
                .count(),
            1
        );
        assert_eq!(second.roots().sequence.len, 513);
    }

    #[test]
    fn sealed_cutoff_rejects_gaps_forks_wrong_causal_membership_and_target() {
        let rows = generation_rows(3);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut store = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut store);
        assert!(builder.append(&rows[1]).is_err());
        builder.append(&rows[0]).unwrap();
        assert!(builder.append(&rows[0]).is_err());
        let mut wrong = rows[1].clone();
        let peer = wrong.causal_dot.peer_id();
        wrong.causal_dot = BatchCausalDot::new(peer, 99).unwrap();
        wrong.canonical_causal_clock = vec![(peer, 99)];
        assert!(builder
            .append(&wrong)
            .unwrap_err()
            .contains("causal membership"));
        // The failed append did not move the roots. The correct immutable
        // records can still extend the preceding accepted cutoff.
        builder.append(&rows[1]).unwrap();
        assert!(builder
            .finish(rows[2].evidence.post_frontier_root())
            .is_err());
        assert!(
            SealedAcceptedCutoff::empty(rows[0].evidence.post_frontier_root().clone()).is_err()
        );
    }

    #[test]
    fn sealed_cutoff_damaged_predecessor_fails_without_changing_other_roots() {
        let rows = generation_rows(2);
        let empty = SealedAcceptedCutoff::empty(AcceptedFrontierRoot::empty()).unwrap();
        let mut store = SealedMemoryStore::default();
        let mut builder = empty.builder(&mut store);
        builder.append(&rows[0]).unwrap();
        let first = builder
            .finish(rows[0].evidence.post_frontier_root())
            .unwrap();
        let address = first.roots().batch_map.root.unwrap().digest;
        let (_, _, bytes) = store
            .objects
            .iter_mut()
            .find(|(kind, digest, _)| {
                *kind == SealedAcceptedObjectKind::MapNode && *digest == address
            })
            .unwrap();
        let saved = bytes.clone();
        bytes.push(0);
        assert!(first.builder(&mut store).append(&rows[1]).is_err());
        store
            .objects
            .iter_mut()
            .find(|(kind, digest, _)| {
                *kind == SealedAcceptedObjectKind::MapNode && *digest == address
            })
            .unwrap()
            .2 = saved;
        assert!(SealedAcceptedIndexReader::new(&store)
            .prove_membership(
                first.roots(),
                1,
                rows[0].evidence.batch_id().as_uuid().into_bytes(),
                &TineAcceptedEvidenceDecoder
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn sealed_cutoff_has_no_live_cutover_or_cache_adoption_path() {
        let engine = include_str!("hot_engine.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        let builder = engine
            .split("pub(crate) fn build_sealed_accepted_cutoff<Store>(")
            .nth(1)
            .unwrap()
            .split("/// Explicit full-roster construction over the current accepted document set.")
            .next()
            .unwrap();
        for forbidden in [
            "capture_clean_checkpoint(",
            "state_bytes",
            "ExportMode",
            "publish_activation_marker",
            "replace_activation_marker",
            "schedule_clean_checkpoint(",
        ] {
            assert!(
                !builder.contains(forbidden),
                "inert cutoff builder acquired {forbidden}"
            );
        }
        assert!(!engine.contains(".build_sealed_accepted_cutoff("));
        assert_eq!(
            engine.matches(".build_compact_accepted_document(").count(),
            1
        );
        for function in [
            "build_compact_document_roster",
            "qualify_full_document_roster",
            "capture_live_graph_document_closure",
            "build_live_graph_document_roster",
            "qualify_live_graph_document_roster",
        ] {
            assert!(!engine.contains(&format!(".{function}(")));
            assert!(!include_str!("../sync_runtime.rs").contains(function));
        }
        assert!(!include_str!("../sync_runtime.rs").contains("build_compact_accepted_document"));
        let compact = engine
            .split("pub(crate) fn build_compact_accepted_document(")
            .nth(1)
            .unwrap()
            .split("/// Capture the exact semantic")
            .next()
            .unwrap();
        for forbidden in [
            "LazyGenesisCheckpointBuilder",
            "publish_activation_marker",
            "replace_activation_marker",
            "fs::",
            "commit_clean_prepared",
        ] {
            assert!(
                !compact.contains(forbidden),
                "compact qualification acquired {forbidden}"
            );
        }
        assert!(!include_str!("../sync_runtime.rs").contains("build_sealed_accepted_cutoff"));
        let production = include_str!("checkpoint_generation.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        let staging = production
            .split("const SEALED_STAGING_BATCH_BYTES")
            .nth(1)
            .unwrap()
            .split("struct RecordingCheckpointSealedStore")
            .next()
            .unwrap();
        for forbidden in [
            "fs::write",
            "fs::rename",
            "read_dir(",
            "replace_activation_marker",
            "publish_activation_marker",
        ] {
            assert!(!staging.contains(forbidden), "staging acquired {forbidden}");
        }
        for caller in [include_str!("../sync_runtime.rs")] {
            assert!(!caller.contains("SealedGenerationStagingStore"));
            assert!(!caller.contains("SealedDocumentRoster"));
        }
        assert!(staging.contains("cfg!(target_os = \"linux\")"));
        assert!(staging.contains("Self::open_immediate(directory)"));
        assert!(staging.contains("publish_new_exact_single_writer(name, bytes)"));
        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        assert!(contract.contains("R1b accepted-cutoff builder"));
        assert!(contract.contains(&format!("{}-<kind>-<digest>", SEALED_STAGING_FILE_PREFIX)));
        assert!(contract.contains(&format!(
            "{} MiB or {} objects",
            SEALED_STAGING_BATCH_BYTES / (1024 * 1024),
            SEALED_STAGING_BATCH_OBJECTS
        )));
        assert!(contract.contains("publish_new_exact_single_writer"));
        assert!(contract.contains(&format!("{}-<digest>", CAPSULE_BLOB_PREFIX)));
        assert!(contract.contains(&format!(
            "schema={}, dependencies: DocumentDependencies",
            DOCUMENT_CAPSULE_SCHEMA
        )));
        assert!(contract.contains("no new on-disk format"));
    }

    #[test]
    fn decoder_accepts_only_the_one_current_evidence_schema() {
        let evidence = evidence();
        let bytes = evidence.encode_canonical().unwrap();
        let binding = TineAcceptedEvidenceDecoder
            .decode_accepted_evidence(ACCEPTED_EVIDENCE_SCHEMA_VERSION, &bytes)
            .unwrap();
        assert_eq!(binding.batch_id, [0x51; 16]);
        assert_eq!(binding.manifest_fingerprint, digest(0x61));
        assert_eq!(binding.event_binding_digest, digest(0x71));
        assert_eq!(binding.acceptance_sequence, 1);
        assert!(TineAcceptedEvidenceDecoder
            .decode_accepted_evidence(ACCEPTED_EVIDENCE_SCHEMA_VERSION - 1, &bytes)
            .is_err());

        let mut trailing = bytes;
        trailing.push(0);
        assert!(TineAcceptedEvidenceDecoder
            .decode_accepted_evidence(ACCEPTED_EVIDENCE_SCHEMA_VERSION, &trailing)
            .is_err());
    }

    #[test]
    fn tine_and_storage_share_the_exact_causal_record_address() {
        let low = CausalPeerId::from_key(WriterIncarnationId::from_uuid(uuid::Uuid::from_bytes(
            [0x11; 16],
        )));
        let author = CausalPeerId::from_key(WriterIncarnationId::from_uuid(
            uuid::Uuid::from_bytes([0x44; 16]),
        ));
        let (root_key, root_digest) =
            authenticated_causal_clock_root(&[(low, 3), (author, 7)]).unwrap();
        let engine_address = accepted_causal_record_digest(
            BatchId::from_uuid(uuid::Uuid::from_bytes([0x51; 16])),
            digest(0x22),
            digest(0x33),
            BatchCausalDot::new(author, 7).unwrap(),
            root_key,
            root_digest,
        );
        let storage_record = SealedAcceptedCausalRecordV2 {
            batch_id: [0x51; 16],
            manifest_fingerprint: digest(0x22),
            event_binding_digest: digest(0x33),
            causal_peer_id: [0x44; 16],
            causal_counter: 7,
            canonical_causal_clock: vec![
                SealedAcceptedCausalClockEntryV2 {
                    peer_id: [0x11; 16],
                    counter: 3,
                },
                SealedAcceptedCausalClockEntryV2 {
                    peer_id: [0x44; 16],
                    counter: 7,
                },
            ],
        };
        assert_eq!(storage_record.address().unwrap(), engine_address);
    }

    #[test]
    fn tine_decoder_completes_the_shared_membership_proof() {
        let evidence = evidence();
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
                evidence_schema: ACCEPTED_EVIDENCE_SCHEMA_VERSION,
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
    fn checkpoint_authoring_uses_only_the_audited_publication_boundary() {
        let production = include_str!("checkpoint_generation.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for forbidden in ["write_all", "std::fs::rename", ".remove_file("] {
            assert!(
                !production.contains(forbidden),
                "checkpoint authoring bypassed the audited publication boundary: {forbidden}"
            );
        }
        assert!(production.contains("SealedAcceptedIndexWriter"));
        assert!(production.contains("DurableDirectoryPublication"));
        assert!(production.contains("publish_new_exact_single_writer"));
        assert!(production.contains("replace_exact"));
    }

    #[test]
    fn checkpoint_open_counter_distinguishes_checkpoint_from_full_replay() {
        let runtime = include_str!("../sync_runtime.rs");
        assert!(runtime.contains("pub checkpoint_opens: usize"));
        assert!(runtime.contains("pub full_replay_opens: usize"));
        assert!(runtime.contains("CleanCheckpointOpen::Loaded"));
        assert!(runtime.contains("is_clean_genesis_frontier"));
        assert!(!runtime.contains("let projection = if replayed == 0"));
    }

    #[test]
    fn checkpoint_payload_uses_the_shared_sealed_roster_round_trip() {
        let evidence = evidence();
        let peer = CausalPeerId::from_key(WriterIncarnationId::from_uuid(uuid::Uuid::from_bytes(
            [0x44; 16],
        )));
        let capture = CleanCheckpointCapture {
            base_sequence: 0,
            target_sequence: 1,
            state_bytes: b"state".to_vec(),
            accepted_rows: vec![CleanCheckpointAcceptedRow {
                no_op: false,
                evidence: evidence.clone(),
                causal_dot: BatchCausalDot::new(peer, 7).unwrap(),
                canonical_causal_clock: vec![(peer, 7)],
            }],
            required_objects: BTreeSet::from([digest(0x91)]),
            capture_work: 3,
        };
        let (sequence, bytes) = build_payload(capture, None).unwrap();
        assert_eq!(sequence, 1);
        let payload: CheckpointPayloadV1 = decode_canonical(&bytes).unwrap();
        assert_eq!(payload.state_bytes, b"state");
        assert_eq!(payload.required_objects, vec![digest(0x91)]);
        let roots = roots_from_wire(payload.roster_roots).unwrap();
        let store = CheckpointSealedStore {
            objects: payload.sealed_objects,
        };
        let proof = SealedAcceptedIndexReader::new(&store)
            .prove_membership(roots, 1, [0x51; 16], &TineAcceptedEvidenceDecoder)
            .unwrap()
            .unwrap();
        assert_eq!(
            proof.status.exact_evidence_bytes,
            evidence.encode_canonical().unwrap()
        );
    }

    #[test]
    fn checkpoint_payload_extends_the_durable_frontier_from_one_row_delta() {
        let first = evidence();
        let second = evidence_after(&first);
        let peer = CausalPeerId::from_key(WriterIncarnationId::from_uuid(uuid::Uuid::from_bytes(
            [0x44; 16],
        )));
        let first_capture = CleanCheckpointCapture {
            base_sequence: 0,
            target_sequence: 1,
            state_bytes: b"frontier-one".to_vec(),
            accepted_rows: vec![CleanCheckpointAcceptedRow {
                no_op: false,
                evidence: first.clone(),
                causal_dot: BatchCausalDot::new(peer, 7).unwrap(),
                canonical_causal_clock: vec![(peer, 7)],
            }],
            required_objects: BTreeSet::from([digest(0x91)]),
            capture_work: 3,
        };
        let (_, first_bytes) = build_payload(first_capture, None).unwrap();
        let first_payload: CheckpointPayloadV1 = decode_canonical(&first_bytes).unwrap();
        let second_capture = CleanCheckpointCapture {
            base_sequence: 1,
            target_sequence: 2,
            state_bytes: b"frontier-two".to_vec(),
            accepted_rows: vec![CleanCheckpointAcceptedRow {
                no_op: false,
                evidence: second,
                causal_dot: BatchCausalDot::new(peer, 8).unwrap(),
                canonical_causal_clock: vec![(peer, 8)],
            }],
            required_objects: BTreeSet::from([digest(0x92)]),
            capture_work: 4,
        };
        let (sequence, second_bytes) =
            build_payload(second_capture, Some((1, first_payload))).unwrap();
        assert_eq!(sequence, 2);
        let payload: CheckpointPayloadV1 = decode_canonical(&second_bytes).unwrap();
        assert_eq!(payload.state_bytes, b"frontier-two");
        assert_eq!(payload.required_objects, vec![digest(0x91), digest(0x92)]);
        assert_eq!(payload.capture_work, 4);
        let roots = roots_from_wire(payload.roster_roots).unwrap();
        assert_eq!(roots.sequence.len, 2);
        let store = CheckpointSealedStore {
            objects: payload.sealed_objects,
        };
        let reader = SealedAcceptedIndexReader::new(&store);
        for (sequence, batch_id) in [(1, [0x51; 16]), (2, [0x52; 16])] {
            assert!(reader
                .prove_membership(roots, sequence, batch_id, &TineAcceptedEvidenceDecoder)
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn lag_over_sixty_four_marks_the_next_coalesced_rewrite_elevated() {
        let root = std::env::temp_dir().join(format!(
            "tine-clean-checkpoint-lag-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = crate::oplog::WorkspaceId::from_uuid(uuid::Uuid::from_u128(0xa564));
        let store = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        let publisher = CleanCheckpointPublisher::new(store, 0);
        let peer = CausalPeerId::from_key(WriterIncarnationId::from_uuid(uuid::Uuid::from_bytes(
            [0x44; 16],
        )));
        let row = CleanCheckpointAcceptedRow {
            no_op: false,
            evidence: evidence(),
            causal_dot: BatchCausalDot::new(peer, 7).unwrap(),
            canonical_causal_clock: vec![(peer, 7)],
        };
        publisher.enqueue(CleanCheckpointCapture {
            base_sequence: 0,
            target_sequence: CLEAN_CHECKPOINT_LAG_MAX + 1,
            state_bytes: Vec::new(),
            accepted_rows: vec![row; CLEAN_CHECKPOINT_LAG_MAX as usize + 1],
            required_objects: BTreeSet::new(),
            capture_work: 0,
        });
        assert!(publisher.elevated_rewrite_observed());
        drop(publisher);
        crate::test_support::remove_dir_all(root);
    }

    fn checkpoint_fault_fixture(tag: &str) -> (std::path::PathBuf, ObjectStore) {
        let root = std::env::temp_dir().join(format!(
            "tine-clean-checkpoint-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = crate::oplog::WorkspaceId::from_uuid(uuid::Uuid::new_v4());
        let store = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        (root, store)
    }

    fn empty_capture(state: &[u8]) -> CleanCheckpointCapture {
        CleanCheckpointCapture {
            base_sequence: 0,
            target_sequence: 0,
            state_bytes: state.to_vec(),
            accepted_rows: Vec::new(),
            required_objects: BTreeSet::new(),
            capture_work: 0,
        }
    }

    fn loaded_state(store: &ObjectStore) -> Vec<u8> {
        match open_checkpoint(store).unwrap() {
            CleanCheckpointOpen::Loaded(loaded) => loaded.state_bytes,
            CleanCheckpointOpen::Absent => panic!("checkpoint unexpectedly absent"),
            CleanCheckpointOpen::Invalid(detail) => {
                panic!("checkpoint unexpectedly invalid: {detail}")
            }
        }
    }

    #[test]
    fn every_checkpoint_publication_prefix_keeps_the_complete_predecessor() {
        let (root, store) = checkpoint_fault_fixture("publication-prefix");
        publish_capture(&store, empty_capture(b"predecessor")).unwrap();
        let directory = root.join("archive").join(CHECKPOINT_DIRECTORY);
        let predecessor_pointer = std::fs::read(directory.join(CHECKPOINT_POINTER)).unwrap();
        let predecessor: CheckpointPointerV1 = decode_canonical(&predecessor_pointer).unwrap();
        assert_eq!(loaded_state(&store), b"predecessor");

        let slot = 1 - predecessor.slot as usize;
        let (sequence, payload) = build_payload(empty_capture(b"successor"), None).unwrap();
        std::fs::write(directory.join(CHECKPOINT_PAYLOAD_NAMES[slot]), &payload).unwrap();
        assert_eq!(loaded_state(&store), b"predecessor");

        let generation = CheckpointGenerationV1 {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            sequence,
            slot: slot as u8,
            payload_digest: ContentDigest::of(&payload),
            payload_len: u64::try_from(payload.len()).unwrap(),
        };
        let generation_bytes = encode_canonical(&generation).unwrap();
        std::fs::write(
            directory.join(CHECKPOINT_GENERATION_NAMES[slot]),
            &generation_bytes,
        )
        .unwrap();
        assert_eq!(loaded_state(&store), b"predecessor");

        let successor_pointer = encode_canonical(&CheckpointPointerV1 {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            sequence,
            slot: slot as u8,
            generation_digest: ContentDigest::of(&generation_bytes),
        })
        .unwrap();
        std::fs::write(directory.join(CHECKPOINT_POINTER), successor_pointer).unwrap();
        assert_eq!(loaded_state(&store), b"successor");

        // Rolling back the pointer to an older still-complete generation is a
        // valid crash image: it restores the predecessor and leaves any newer
        // archive manifests to ordinary tail admission.
        std::fs::write(directory.join(CHECKPOINT_POINTER), predecessor_pointer).unwrap();
        assert_eq!(loaded_state(&store), b"predecessor");
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn post_publication_checkpoint_damage_is_private_fallback_state() {
        for damage in [
            "pointer-bitflip",
            "generation-truncate",
            "payload-truncate",
            "payload-oversize",
        ] {
            let (root, store) = checkpoint_fault_fixture(damage);
            publish_capture(&store, empty_capture(b"disposable")).unwrap();
            let directory = root.join("archive").join(CHECKPOINT_DIRECTORY);
            let pointer_bytes = std::fs::read(directory.join(CHECKPOINT_POINTER)).unwrap();
            let pointer: CheckpointPointerV1 = decode_canonical(&pointer_bytes).unwrap();
            let slot = pointer.slot as usize;
            match damage {
                "pointer-bitflip" => {
                    let mut bytes = pointer_bytes;
                    bytes[0] ^= 0x80;
                    std::fs::write(directory.join(CHECKPOINT_POINTER), bytes).unwrap();
                }
                "generation-truncate" => {
                    std::fs::write(directory.join(CHECKPOINT_GENERATION_NAMES[slot]), [0x01])
                        .unwrap();
                }
                "payload-truncate" => {
                    std::fs::write(directory.join(CHECKPOINT_PAYLOAD_NAMES[slot]), [0x01]).unwrap();
                }
                "payload-oversize" => {
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(directory.join(CHECKPOINT_PAYLOAD_NAMES[slot]))
                        .unwrap();
                    file.set_len(MAX_CHECKPOINT_BYTES + 1).unwrap();
                }
                _ => unreachable!(),
            }
            match open_checkpoint(&store) {
                Ok(CleanCheckpointOpen::Invalid(_)) | Err(CleanCheckpointOpenError::Store(_)) => {}
                Ok(CleanCheckpointOpen::Absent) => panic!("{damage} erased the checkpoint pointer"),
                Ok(CleanCheckpointOpen::Loaded(_)) => panic!("{damage} loaded damaged state"),
                Err(CleanCheckpointOpenError::ArchiveDamage(detail)) => {
                    panic!("{damage} was misclassified as archive authority damage: {detail}")
                }
            }
            crate::test_support::remove_dir_all(root);
        }
    }

    #[test]
    fn object_only_crash_residue_does_not_change_checkpoint_membership() {
        use crate::oplog::{DocumentId, ObjectKind, OperationObject};

        let (root, store) = checkpoint_fault_fixture("object-only-residue");
        publish_capture(&store, empty_capture(b"stable checkpoint")).unwrap();
        let object = OperationObject::new(
            store.workspace_id(),
            DocumentId::from_uuid(uuid::Uuid::new_v4()),
            ObjectKind::CrdtUpdate,
            b"valid orphaned operation object".to_vec(),
        )
        .unwrap();
        store.stage_object_bytes(&object.encode().unwrap()).unwrap();
        assert_eq!(store.committed_manifest_names().unwrap().len(), 0);
        assert_eq!(store.object_names().unwrap().len(), 1);
        assert_eq!(loaded_state(&store), b"stable checkpoint");
        crate::test_support::remove_dir_all(root);
    }
}
