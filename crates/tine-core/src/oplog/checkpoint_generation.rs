//! Adapter between Tine's one current accepted-evidence format and the shared
//! sealed accepted-history index.
//!
//! A5 completes the former R1a reader-only boundary with one disposable
//! generation publisher. Managed Storage is pre-0.7, so this module still
//! contains no legacy decoder, version dispatch, or migration bridge.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use serde::{Deserialize, Serialize};

use super::hot_engine::{
    AcceptedBatchEvidence, CleanCheckpointAcceptedRow, CleanCheckpointCapture,
    ACCEPTED_EVIDENCE_SCHEMA_VERSION,
};
use super::object_store::ObjectStore;
use super::{BatchCausalDot, BatchId, CausalPeerId, ContentDigest, DeviceId};

const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const CHECKPOINT_DIRECTORY: &str = "clean-open-checkpoint-v1";
const CHECKPOINT_POINTER: &str = "current";
const CHECKPOINT_PAYLOAD_NAMES: [&str; 2] = ["payload-a", "payload-b"];
const CHECKPOINT_GENERATION_NAMES: [&str; 2] = ["generation-a", "generation-b"];
const MAX_CHECKPOINT_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const CLEAN_CHECKPOINT_LAG_MAX: u64 = 64;

#[cfg(test)]
static FAIL_CHECKPOINT_WRITES: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn fail_checkpoint_writes_for_test(fail: bool) {
    FAIL_CHECKPOINT_WRITES.store(fail, Ordering::Release);
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
struct CheckpointSealedStore {
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
    ) -> Result<BTreeMap<[u8; 16], ContentDigest>, String> {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MapRootWire {
    count: u64,
    root_key: Option<[u8; 16]>,
    root_digest: Option<ContentDigest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SequenceRootWire {
    len: u64,
    height: u8,
    root_digest: Option<ContentDigest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
        root_key: root.root.map(|link| link.key),
        root_digest: root.root.map(|link| link.digest),
    }
}

fn map_root_from_wire(
    wire: MapRootWire,
) -> Result<tine_storage::sealed_accepted_index::AuthenticatedMapRootV1, String> {
    use tine_storage::sealed_accepted_index::{AuthenticatedMapLinkV1, AuthenticatedMapRootV1};
    let root = match (wire.root_key, wire.root_digest) {
        (Some(key), Some(digest)) => Some(AuthenticatedMapLinkV1 { key, digest }),
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

fn build_payload(
    capture: CleanCheckpointCapture,
    predecessor: Option<(u64, CheckpointPayloadV1)>,
) -> Result<(u64, Vec<u8>), String> {
    use tine_storage::sealed_accepted_index::{
        AcceptedSequenceEntryV2, AcceptedSequenceRootV2, AcceptedStatusRecordV2,
        AuthenticatedMapRootV1, SealedAcceptedCausalClockEntryV2, SealedAcceptedCausalRecordV2,
        SealedAcceptedIndexWriter,
    };

    let (
        mut store,
        mut batch_map,
        mut status_map,
        mut sequence_root,
        mut required_objects,
    ) = match predecessor {
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
                payload.required_objects.into_iter().collect::<BTreeSet<_>>(),
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
        if row.evidence.acceptance_sequence() != sequence_root.len.saturating_add(1) {
            return Err("clean checkpoint delta sequence is not contiguous".into());
        }
        let batch_id = row.evidence.batch_id().as_uuid().into_bytes();
        let causal = SealedAcceptedCausalRecordV2 {
            batch_id,
            manifest_fingerprint: row.evidence.manifest_fingerprint(),
            event_binding_digest: row.evidence.event_binding_digest(),
            causal_peer_id: row
                .causal_dot
                .peer_id()
                .as_device_id()
                .as_uuid()
                .into_bytes(),
            causal_counter: row.causal_dot.counter(),
            canonical_causal_clock: row
                .canonical_causal_clock
                .iter()
                .map(|(peer, counter)| SealedAcceptedCausalClockEntryV2 {
                    peer_id: peer.as_device_id().as_uuid().into_bytes(),
                    counter: *counter,
                })
                .collect(),
        };
        let mut writer = SealedAcceptedIndexWriter::new(&mut store);
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
    let Some(pointer_bytes) = tine_storage::read_optional_regular(
        &directory,
        CHECKPOINT_POINTER,
        4 * 1024,
        None,
    )
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
    if FAIL_CHECKPOINT_WRITES.load(Ordering::Acquire) {
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
        let Some(status_address) = status_addresses.get(&entry.batch_id).copied() else {
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
        let Some(causal_address) = causal_addresses.get(&entry.batch_id).copied() else {
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
        let peer = CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes(
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
                    CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes(
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
    use crate::oplog::{BatchCausalDot, BatchId, CausalPeerId, ContentDigest, DeviceId};
    use tine_storage::sealed_accepted_index::{
        AcceptedSequenceEntryV2, AcceptedSequenceRootV2, AcceptedStatusRecordV2,
        AuthenticatedMapRootV1, SealedAcceptedCausalClockEntryV2, SealedAcceptedCausalRecordV2,
        SealedAcceptedEvidenceDecoder, SealedAcceptedIndexObjectStore, SealedAcceptedIndexReader,
        SealedAcceptedIndexRootsV2, SealedAcceptedIndexWriter, SealedAcceptedObjectKind,
    };

    #[derive(Default)]
    struct SealedMemoryStore {
        objects: Vec<(SealedAcceptedObjectKind, ContentDigest, Vec<u8>)>,
    }

    impl SealedAcceptedIndexObjectStore for SealedMemoryStore {
        fn read_sealed_accepted_object(
            &self,
            kind: SealedAcceptedObjectKind,
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
        let low =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x11; 16])));
        let author =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x44; 16])));
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
        let peer =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x44; 16])));
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
        let peer =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x44; 16])));
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
        let peer =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x44; 16])));
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
