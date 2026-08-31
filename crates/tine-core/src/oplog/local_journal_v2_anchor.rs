use tine_storage::formats::LOCAL_JOURNAL_SEGMENT_PROTOCOL_VERSION;
use tine_storage::{
    ContentDigest, LineageDigest, LocalJournalError, LocalJournalFrame, LocalJournalPayloadKind,
    LocalJournalSegmentV2, LocalJournalSegmentV2Selection,
};
use uuid::Uuid;

use super::local_journal_drain::ManagedLocalDrainCheckpoint;
use super::{BatchId, WorkspaceId};

pub(crate) const MANAGED_LOCAL_ANCHOR_V2_MAGIC: &[u8; 8] = b"TINEANC2";
pub(crate) const MANAGED_LOCAL_ANCHOR_V2_SCHEMA: u32 = 2;
pub(crate) const MANAGED_LOCAL_ANCHOR_V2_BYTES: usize = 1024;

/// One already-open managed-local journal in the single current pre-0.7 format.
pub(crate) struct ManagedLocalJournal<K> {
    selector_generation: u64,
    segment: LocalJournalSegmentV2<K>,
}

impl<K: LocalJournalPayloadKind> ManagedLocalJournal<K> {
    pub(crate) fn from_open(selector_generation: u64, segment: LocalJournalSegmentV2<K>) -> Self {
        Self {
            selector_generation,
            segment,
        }
    }

    pub(crate) fn device_id(&self) -> Uuid {
        self.segment.selection().device_id()
    }

    pub(crate) fn base_sequence(&self) -> u64 {
        self.segment.selection().base_sequence()
    }

    pub(crate) fn next_sequence(&self) -> u64 {
        self.segment.next_sequence()
    }

    #[cfg(test)]
    pub(crate) fn committed_bytes(&self) -> u64 {
        self.segment.committed_bytes()
    }

    /// The selector generation that authenticated this open schema-2 tuple.
    ///
    /// Callers use this only to bind an already-open journal back to the
    /// authority generation re-enumerated immediately before successor
    /// publication.
    pub(crate) const fn selector_generation(&self) -> u64 {
        self.selector_generation
    }

    #[cfg(test)]
    pub(crate) const fn expected_successful_append_data_syncs(&self) -> u64 {
        2
    }

    pub(crate) fn replay(
        &self,
        visit: impl FnMut(LocalJournalFrame<K>),
    ) -> Result<u64, LocalJournalError> {
        self.segment.replay(visit)
    }

    pub(crate) fn append(
        &mut self,
        payload_kind: K,
        payload: &[u8],
    ) -> Result<tine_storage::LocalJournalAppend, tine_storage::LocalJournalAppendError> {
        self.segment.append(payload_kind, payload)
    }
}

const CHECKSUM_BYTES: usize = 32;
const CHECKSUM_OFFSET: usize = MANAGED_LOCAL_ANCHOR_V2_BYTES - CHECKSUM_BYTES;
const CHECKPOINT_CAPACITY: usize = 256;
const SEGMENT_NAME_CAPACITY: usize = 160;

const SCHEMA_OFFSET: usize = 8;
const PROTOCOL_OFFSET: usize = 12;
const SELECTOR_GENERATION_OFFSET: usize = 16;
const WORKSPACE_OFFSET: usize = 24;
const LINEAGE_OFFSET: usize = 40;
const DEVICE_OFFSET: usize = 72;
const BASE_SEQUENCE_OFFSET: usize = 88;
const CHECKPOINT_LENGTH_OFFSET: usize = 96;
const CHECKPOINT_OFFSET: usize = 98;
const ACCEPTED_BATCH_TAG_OFFSET: usize = CHECKPOINT_OFFSET + CHECKPOINT_CAPACITY;
const ACCEPTED_BATCH_OFFSET: usize = ACCEPTED_BATCH_TAG_OFFSET + 1;
const SEGMENT_ID_OFFSET: usize = ACCEPTED_BATCH_OFFSET + 16;
const SEGMENT_NAME_DIGEST_OFFSET: usize = SEGMENT_ID_OFFSET + 16;
const SEGMENT_NAME_LENGTH_OFFSET: usize = SEGMENT_NAME_DIGEST_OFFSET + 32;
const SEGMENT_NAME_OFFSET: usize = SEGMENT_NAME_LENGTH_OFFSET + 2;
const RESERVED_OFFSET: usize = SEGMENT_NAME_OFFSET + SEGMENT_NAME_CAPACITY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedLocalAnchorEncoding {
    Current,
    Unrecognized,
}

pub(crate) fn classify_managed_local_anchor(bytes: &[u8]) -> ManagedLocalAnchorEncoding {
    if bytes.starts_with(MANAGED_LOCAL_ANCHOR_V2_MAGIC) {
        ManagedLocalAnchorEncoding::Current
    } else {
        ManagedLocalAnchorEncoding::Unrecognized
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedLocalGenerationAnchorV2 {
    selector_generation: u64,
    checkpoint: ManagedLocalDrainCheckpoint,
    accepted_batch_id: Option<BatchId>,
    selection: LocalJournalSegmentV2Selection,
}

impl ManagedLocalGenerationAnchorV2 {
    pub(crate) fn new(
        selector_generation: u64,
        checkpoint: ManagedLocalDrainCheckpoint,
        accepted_batch_id: Option<BatchId>,
        segment_id: Uuid,
    ) -> Result<Self, String> {
        if selector_generation == 0 {
            return Err("managed-local schema-2 selector generation must be positive".into());
        }
        if segment_id.is_nil() {
            return Err(
                "managed-local schema-2 segment identity must be random and nonzero".into(),
            );
        }
        validate_checkpoint_batch_binding(
            checkpoint.next_sequence(),
            checkpoint.next_sequence(),
            accepted_batch_id,
        )?;
        let segment_name =
            managed_local_v2_segment_name(checkpoint.device_id(), selector_generation, segment_id);
        let selection = LocalJournalSegmentV2Selection::new(
            segment_name,
            segment_id,
            checkpoint.device_id(),
            checkpoint.next_sequence(),
        )
        .map_err(|error| format!("cannot construct managed-local v2 selection: {error}"))?;
        Ok(Self {
            selector_generation,
            checkpoint,
            accepted_batch_id,
            selection,
        })
    }

    pub(crate) const fn selector_generation(&self) -> u64 {
        self.selector_generation
    }

    pub(crate) const fn checkpoint(&self) -> &ManagedLocalDrainCheckpoint {
        &self.checkpoint
    }

    pub(crate) const fn accepted_batch_id(&self) -> Option<BatchId> {
        self.accepted_batch_id
    }

    /// Verify the accepted engine batch after the runtime has opened the
    /// engine. The anchor is itself the bootstrap locator for this evidence,
    /// so structural decoding cannot require the value as prior input.
    #[cfg(test)]
    pub(crate) fn require_accepted_batch_id(
        &self,
        accepted_batch_id: Option<BatchId>,
    ) -> Result<(), String> {
        if self.accepted_batch_id != accepted_batch_id {
            return Err("managed-local schema-2 accepted batch is mismatched".into());
        }
        Ok(())
    }

    pub(crate) const fn selection(&self) -> &LocalJournalSegmentV2Selection {
        &self.selection
    }

    pub(crate) fn encode(&self) -> Result<[u8; MANAGED_LOCAL_ANCHOR_V2_BYTES], String> {
        validate_checkpoint_batch_binding(
            self.selection.base_sequence(),
            self.checkpoint.next_sequence(),
            self.accepted_batch_id,
        )?;
        validate_selection_name(self.selector_generation, &self.selection)?;

        let checkpoint = self.checkpoint.encode()?;
        let checkpoint_length = u16::try_from(checkpoint.len())
            .map_err(|_| "managed-local checkpoint is too large for schema-2 anchor")?;
        if checkpoint.len() > CHECKPOINT_CAPACITY {
            return Err("managed-local checkpoint is too large for schema-2 anchor".into());
        }
        let segment_name = self.selection.segment_name().as_bytes();
        let segment_name_length = u16::try_from(segment_name.len())
            .map_err(|_| "managed-local v2 segment name is too long")?;
        if segment_name.is_empty() || segment_name.len() > SEGMENT_NAME_CAPACITY {
            return Err("managed-local v2 segment name is not canonically bounded".into());
        }

        let mut bytes = [0_u8; MANAGED_LOCAL_ANCHOR_V2_BYTES];
        bytes[..8].copy_from_slice(MANAGED_LOCAL_ANCHOR_V2_MAGIC);
        bytes[SCHEMA_OFFSET..PROTOCOL_OFFSET]
            .copy_from_slice(&MANAGED_LOCAL_ANCHOR_V2_SCHEMA.to_be_bytes());
        bytes[PROTOCOL_OFFSET..SELECTOR_GENERATION_OFFSET]
            .copy_from_slice(&LOCAL_JOURNAL_SEGMENT_PROTOCOL_VERSION.to_be_bytes());
        bytes[SELECTOR_GENERATION_OFFSET..WORKSPACE_OFFSET]
            .copy_from_slice(&self.selector_generation.to_be_bytes());
        bytes[WORKSPACE_OFFSET..LINEAGE_OFFSET]
            .copy_from_slice(self.checkpoint.workspace_id().as_uuid().as_bytes());
        bytes[LINEAGE_OFFSET..DEVICE_OFFSET]
            .copy_from_slice(self.checkpoint.lineage_digest().as_bytes());
        bytes[DEVICE_OFFSET..BASE_SEQUENCE_OFFSET]
            .copy_from_slice(self.checkpoint.device_id().as_bytes());
        bytes[BASE_SEQUENCE_OFFSET..CHECKPOINT_LENGTH_OFFSET]
            .copy_from_slice(&self.selection.base_sequence().to_be_bytes());
        bytes[CHECKPOINT_LENGTH_OFFSET..CHECKPOINT_OFFSET]
            .copy_from_slice(&checkpoint_length.to_be_bytes());
        bytes[CHECKPOINT_OFFSET..CHECKPOINT_OFFSET + checkpoint.len()].copy_from_slice(&checkpoint);
        if let Some(batch_id) = self.accepted_batch_id {
            bytes[ACCEPTED_BATCH_TAG_OFFSET] = 1;
            bytes[ACCEPTED_BATCH_OFFSET..SEGMENT_ID_OFFSET]
                .copy_from_slice(batch_id.as_uuid().as_bytes());
        }
        bytes[SEGMENT_ID_OFFSET..SEGMENT_NAME_DIGEST_OFFSET]
            .copy_from_slice(self.selection.segment_id().as_bytes());
        bytes[SEGMENT_NAME_DIGEST_OFFSET..SEGMENT_NAME_LENGTH_OFFSET]
            .copy_from_slice(self.selection.segment_name_digest().as_bytes());
        bytes[SEGMENT_NAME_LENGTH_OFFSET..SEGMENT_NAME_OFFSET]
            .copy_from_slice(&segment_name_length.to_be_bytes());
        bytes[SEGMENT_NAME_OFFSET..SEGMENT_NAME_OFFSET + segment_name.len()]
            .copy_from_slice(segment_name);
        let checksum = ContentDigest::of(&bytes[..CHECKSUM_OFFSET]);
        bytes[CHECKSUM_OFFSET..].copy_from_slice(checksum.as_bytes());
        Ok(bytes)
    }

    pub(crate) fn decode(
        bytes: &[u8],
        expected_selector_generation: u64,
        expected_workspace_id: WorkspaceId,
        expected_lineage_digest: LineageDigest,
        expected_device_id: Uuid,
    ) -> Result<Self, String> {
        if classify_managed_local_anchor(bytes) != ManagedLocalAnchorEncoding::Current {
            return Err("managed-local anchor is not schema 2".into());
        }
        if bytes.len() != MANAGED_LOCAL_ANCHOR_V2_BYTES {
            return Err("managed-local schema-2 anchor has a noncanonical length".into());
        }
        if read_u32(bytes, SCHEMA_OFFSET)? != MANAGED_LOCAL_ANCHOR_V2_SCHEMA
            || read_u32(bytes, PROTOCOL_OFFSET)? != LOCAL_JOURNAL_SEGMENT_PROTOCOL_VERSION
        {
            return Err("managed-local schema-2 anchor version is unsupported".into());
        }
        if bytes[CHECKSUM_OFFSET..] != ContentDigest::of(&bytes[..CHECKSUM_OFFSET]).as_bytes()[..] {
            return Err("managed-local schema-2 anchor checksum mismatch".into());
        }
        let selector_generation = read_u64(bytes, SELECTOR_GENERATION_OFFSET)?;
        if selector_generation == 0 || selector_generation != expected_selector_generation {
            return Err("managed-local schema-2 selector generation is mismatched".into());
        }
        let workspace_id = WorkspaceId::from_uuid(read_uuid(bytes, WORKSPACE_OFFSET)?);
        let lineage_digest = read_lineage_digest(bytes, LINEAGE_OFFSET)?;
        let device_id = read_uuid(bytes, DEVICE_OFFSET)?;
        if workspace_id != expected_workspace_id
            || lineage_digest != expected_lineage_digest
            || device_id != expected_device_id
        {
            return Err("managed-local schema-2 anchor binding is invalid".into());
        }
        let base_sequence = read_u64(bytes, BASE_SEQUENCE_OFFSET)?;
        let checkpoint_length = read_u16(bytes, CHECKPOINT_LENGTH_OFFSET)? as usize;
        if checkpoint_length == 0 || checkpoint_length > CHECKPOINT_CAPACITY {
            return Err("managed-local schema-2 checkpoint length is invalid".into());
        }
        if bytes[CHECKPOINT_OFFSET + checkpoint_length..CHECKPOINT_OFFSET + CHECKPOINT_CAPACITY]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err("managed-local schema-2 checkpoint padding is nonzero".into());
        }
        let checkpoint = ManagedLocalDrainCheckpoint::decode(
            &bytes[CHECKPOINT_OFFSET..CHECKPOINT_OFFSET + checkpoint_length],
            device_id,
            workspace_id,
            lineage_digest,
        )?;
        let accepted_batch_id = match bytes[ACCEPTED_BATCH_TAG_OFFSET] {
            0 => {
                if bytes[ACCEPTED_BATCH_OFFSET..SEGMENT_ID_OFFSET]
                    .iter()
                    .any(|byte| *byte != 0)
                {
                    return Err("managed-local schema-2 absent batch payload is nonzero".into());
                }
                None
            }
            1 => Some(BatchId::from_uuid(read_uuid(bytes, ACCEPTED_BATCH_OFFSET)?)),
            _ => return Err("managed-local schema-2 accepted-batch tag is invalid".into()),
        };
        validate_checkpoint_batch_binding(
            base_sequence,
            checkpoint.next_sequence(),
            accepted_batch_id,
        )?;

        let segment_id = read_uuid(bytes, SEGMENT_ID_OFFSET)?;
        if segment_id.is_nil() {
            return Err("managed-local schema-2 segment identity is invalid".into());
        }
        let stored_name_digest = read_digest(bytes, SEGMENT_NAME_DIGEST_OFFSET)?;
        let segment_name_length = read_u16(bytes, SEGMENT_NAME_LENGTH_OFFSET)? as usize;
        if segment_name_length == 0 || segment_name_length > SEGMENT_NAME_CAPACITY {
            return Err("managed-local schema-2 segment name length is invalid".into());
        }
        if bytes
            [SEGMENT_NAME_OFFSET + segment_name_length..SEGMENT_NAME_OFFSET + SEGMENT_NAME_CAPACITY]
            .iter()
            .any(|byte| *byte != 0)
            || bytes[RESERVED_OFFSET..CHECKSUM_OFFSET]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err("managed-local schema-2 reserved bytes are nonzero".into());
        }
        let segment_name = std::str::from_utf8(
            &bytes[SEGMENT_NAME_OFFSET..SEGMENT_NAME_OFFSET + segment_name_length],
        )
        .map_err(|_| "managed-local schema-2 segment name is not UTF-8")?;
        let parsed = parse_managed_local_v2_segment_name(segment_name, device_id)
            .ok_or_else(|| "managed-local schema-2 segment name is noncanonical".to_owned())?;
        if parsed != (selector_generation, segment_id) {
            return Err("managed-local schema-2 segment name binding is invalid".into());
        }
        let selection =
            LocalJournalSegmentV2Selection::new(segment_name, segment_id, device_id, base_sequence)
                .map_err(|error| {
                    format!("cannot reconstruct managed-local v2 selection: {error}")
                })?;
        if selection.segment_name_digest() != stored_name_digest {
            return Err("managed-local schema-2 segment name digest is invalid".into());
        }
        validate_selection_name(selector_generation, &selection)?;
        Ok(Self {
            selector_generation,
            checkpoint,
            accepted_batch_id,
            selection,
        })
    }
}

pub(crate) fn managed_local_v2_segment_name(
    device_id: Uuid,
    selector_generation: u64,
    segment_id: Uuid,
) -> String {
    format!(
        "device-{}-selector-{selector_generation:020}-segment-{}.journal-v2",
        device_id.simple(),
        segment_id.simple()
    )
}

pub(crate) fn managed_local_v2_anchor_name(device_id: Uuid, selector_generation: u64) -> String {
    format!(
        "device-{}-selector-{selector_generation:020}.anchor-v2",
        device_id.simple()
    )
}

pub(crate) fn parse_managed_local_v2_anchor_name(
    name: &str,
    expected_device_id: Uuid,
) -> Option<u64> {
    let prefix = format!("device-{}-selector-", expected_device_id.simple());
    let generation = name.strip_prefix(&prefix)?.strip_suffix(".anchor-v2")?;
    if generation.len() != 20 || !generation.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let selector_generation = generation.parse().ok()?;
    (selector_generation != 0
        && managed_local_v2_anchor_name(expected_device_id, selector_generation) == name)
        .then_some(selector_generation)
}

pub(crate) fn parse_managed_local_v2_segment_name(
    name: &str,
    expected_device_id: Uuid,
) -> Option<(u64, Uuid)> {
    let prefix = format!("device-{}-selector-", expected_device_id.simple());
    let rest = name.strip_prefix(&prefix)?.strip_suffix(".journal-v2")?;
    let (generation, segment_id) = rest.split_once("-segment-")?;
    if generation.len() != 20 || !generation.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let selector_generation = generation.parse().ok()?;
    let segment_id = Uuid::parse_str(segment_id).ok()?;
    (managed_local_v2_segment_name(expected_device_id, selector_generation, segment_id) == name)
        .then_some((selector_generation, segment_id))
}

#[cfg(test)]
pub(crate) fn next_managed_local_selector_generation(
    active_generations: impl IntoIterator<Item = u64>,
) -> Result<u64, String> {
    active_generations
        .into_iter()
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "managed-local selector generation overflow".into())
}

fn validate_checkpoint_batch_binding(
    base_sequence: u64,
    checkpoint_next_sequence: u64,
    accepted_batch_id: Option<BatchId>,
) -> Result<(), String> {
    if base_sequence != checkpoint_next_sequence {
        return Err("managed-local schema-2 base/checkpoint sequence is mismatched".into());
    }
    if (base_sequence == 0) != accepted_batch_id.is_none() {
        return Err("managed-local schema-2 accepted batch presence is invalid".into());
    }
    Ok(())
}

fn validate_selection_name(
    selector_generation: u64,
    selection: &LocalJournalSegmentV2Selection,
) -> Result<(), String> {
    let expected = managed_local_v2_segment_name(
        selection.device_id(),
        selector_generation,
        selection.segment_id(),
    );
    if selection.segment_name() != expected
        || selection.segment_name_digest() != ContentDigest::of(expected.as_bytes())
    {
        return Err("managed-local schema-2 selection name is invalid".into());
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    bytes
        .get(offset..offset + 2)
        .and_then(|slice| slice.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or_else(|| "managed-local schema-2 anchor is truncated".into())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| "managed-local schema-2 anchor is truncated".into())
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset + 8)
        .and_then(|slice| slice.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or_else(|| "managed-local schema-2 anchor is truncated".into())
}

fn read_uuid(bytes: &[u8], offset: usize) -> Result<Uuid, String> {
    bytes
        .get(offset..offset + 16)
        .and_then(|slice| slice.try_into().ok())
        .map(Uuid::from_bytes)
        .ok_or_else(|| "managed-local schema-2 anchor is truncated".into())
}

fn read_digest(bytes: &[u8], offset: usize) -> Result<ContentDigest, String> {
    bytes
        .get(offset..offset + 32)
        .and_then(|slice| slice.try_into().ok())
        .map(ContentDigest::from_bytes)
        .ok_or_else(|| "managed-local schema-2 anchor is truncated".into())
}

fn read_lineage_digest(bytes: &[u8], offset: usize) -> Result<LineageDigest, String> {
    bytes
        .get(offset..offset + 32)
        .and_then(|slice| slice.try_into().ok())
        .map(LineageDigest::from_bytes)
        .ok_or_else(|| "managed-local schema-2 anchor is truncated".into())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use cap_std::{ambient_authority, fs::Dir};
    use tine_storage::formats::LOCAL_JOURNAL_SEGMENT_HEADER_BYTES;

    use super::*;
    use crate::oplog::hot_engine::ManagedLocalJournalPayloadKind;

    struct TemporaryCapabilityDirectory {
        root: PathBuf,
        dir: Dir,
    }

    impl TemporaryCapabilityDirectory {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "tine-managed-local-journal-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir(&root).unwrap();
            let dir = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
            Self { root, dir }
        }
    }

    impl Drop for TemporaryCapabilityDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> ManagedLocalGenerationAnchorV2 {
        let device_id = Uuid::from_u128(0x112233445566778899aabbccddeeff00);
        let workspace_id =
            WorkspaceId::from_uuid(Uuid::from_u128(0x102030405060708090a0b0c0d0e0f000));
        let lineage = LineageDigest::from_bytes([0x5a; 32]);
        ManagedLocalGenerationAnchorV2::new(
            7,
            ManagedLocalDrainCheckpoint::initial(device_id, workspace_id, lineage),
            None,
            Uuid::from_u128(0xffeeddccbbaa99887766554433221100),
        )
        .unwrap()
    }

    #[test]
    fn schema2_anchor_round_trips_exact_selection() {
        let anchor = fixture();
        let bytes = anchor.encode().unwrap();
        let decoded = ManagedLocalGenerationAnchorV2::decode(
            &bytes,
            anchor.selector_generation(),
            anchor.checkpoint().workspace_id(),
            anchor.checkpoint().lineage_digest(),
            anchor.checkpoint().device_id(),
        )
        .unwrap();
        decoded
            .require_accepted_batch_id(anchor.accepted_batch_id())
            .unwrap();
        assert!(decoded
            .require_accepted_batch_id(Some(BatchId::from_uuid(Uuid::from_u128(9))))
            .is_err());
        assert_eq!(decoded, anchor);
        assert_eq!(
            decoded.selection().frontier_name(),
            format!(
                "{}{}",
                decoded.selection().segment_name(),
                tine_storage::formats::LOCAL_JOURNAL_FRONTIER_SUFFIX
            )
        );
        assert_eq!(
            classify_managed_local_anchor(&bytes),
            ManagedLocalAnchorEncoding::Current
        );
    }

    #[test]
    fn schema2_anchor_golden_digest_is_stable() {
        let bytes = fixture().encode().unwrap();
        assert_eq!(
            ContentDigest::of(&bytes).to_string(),
            "33d26edbd782115b37427664808575762c498c2d8cbfab202d2ca784251f82c0"
        );
    }

    #[test]
    fn schema2_anchor_refuses_corruption_padding_and_unknown_schema() {
        let anchor = fixture();
        let bytes = anchor.encode().unwrap();
        for offset in [
            0,
            SCHEMA_OFFSET,
            CHECKPOINT_OFFSET,
            RESERVED_OFFSET,
            CHECKSUM_OFFSET,
        ] {
            let mut corrupt = bytes;
            corrupt[offset] ^= 0x01;
            assert!(ManagedLocalGenerationAnchorV2::decode(
                &corrupt,
                7,
                anchor.checkpoint().workspace_id(),
                anchor.checkpoint().lineage_digest(),
                anchor.checkpoint().device_id(),
            )
            .is_err());
        }
        let mut unknown = bytes;
        unknown[..8].copy_from_slice(b"TINEANC3");
        assert_eq!(
            classify_managed_local_anchor(&unknown),
            ManagedLocalAnchorEncoding::Unrecognized
        );
    }

    #[test]
    fn segment_names_are_canonical_and_selector_ordinal_is_independent() {
        let anchor = fixture();
        let name = anchor.selection().segment_name();
        assert_eq!(
            parse_managed_local_v2_segment_name(name, anchor.checkpoint().device_id()),
            Some((
                anchor.selector_generation(),
                anchor.selection().segment_id()
            ))
        );
        assert!(parse_managed_local_v2_segment_name(
            &name.replace("00000000000000000007", "7"),
            anchor.checkpoint().device_id(),
        )
        .is_none());
        let anchor_name = managed_local_v2_anchor_name(
            anchor.checkpoint().device_id(),
            anchor.selector_generation(),
        );
        assert_eq!(
            parse_managed_local_v2_anchor_name(&anchor_name, anchor.checkpoint().device_id()),
            Some(anchor.selector_generation())
        );
        assert!(parse_managed_local_v2_anchor_name(
            &anchor_name.replace("00000000000000000007", "7"),
            anchor.checkpoint().device_id(),
        )
        .is_none());
        assert_eq!(
            next_managed_local_selector_generation([3, 99, 7]).unwrap(),
            100
        );
        assert!(next_managed_local_selector_generation([u64::MAX]).is_err());
    }

    #[test]
    fn schema2_anchor_refuses_zero_selector_or_segment_identity() {
        let fixture = fixture();
        assert!(ManagedLocalGenerationAnchorV2::new(
            0,
            fixture.checkpoint().clone(),
            None,
            Uuid::from_u128(1),
        )
        .is_err());
        assert!(ManagedLocalGenerationAnchorV2::new(
            1,
            fixture.checkpoint().clone(),
            None,
            Uuid::nil(),
        )
        .is_err());
    }

    #[test]
    fn accepted_batch_presence_tracks_the_checkpoint_base() {
        let batch = BatchId::from_uuid(Uuid::from_u128(1));
        assert!(validate_checkpoint_batch_binding(0, 0, None).is_ok());
        assert!(validate_checkpoint_batch_binding(0, 0, Some(batch)).is_err());
        assert!(validate_checkpoint_batch_binding(1, 1, Some(batch)).is_ok());
        assert!(validate_checkpoint_batch_binding(1, 1, None).is_err());
        assert!(validate_checkpoint_batch_binding(1, 2, Some(batch)).is_err());
    }

    #[test]
    fn managed_local_journal_wraps_a_reopened_current_segment() {
        let directory = TemporaryCapabilityDirectory::new("current");
        let device_id = Uuid::from_u128(0x3400);
        let base_sequence = 23;
        let selection = LocalJournalSegmentV2Selection::new(
            "managed-v2.journal",
            Uuid::from_u128(0x3500),
            device_id,
            base_sequence,
        )
        .unwrap();
        let expected = LocalJournalFrame::new(
            device_id,
            base_sequence,
            ManagedLocalJournalPayloadKind::RecordV1,
            b"v2-frame".to_vec(),
        );

        LocalJournalSegmentV2::<ManagedLocalJournalPayloadKind>::prepare(
            &directory.dir,
            &selection,
        )
        .unwrap();
        let append = {
            let (mut segment, recovery) =
                LocalJournalSegmentV2::open_selected(&directory.dir, &selection).unwrap();
            assert_eq!(recovery.frames_recovered, 0);
            let append = segment
                .append(expected.payload_kind(), expected.payload())
                .unwrap();
            drop(segment);
            append
        };
        let (segment, recovery) =
            LocalJournalSegmentV2::open_selected(&directory.dir, &selection).unwrap();
        assert_eq!(recovery.frames_recovered, 1);
        let journal = ManagedLocalJournal::from_open(7, segment);

        assert_eq!(journal.selector_generation(), 7);
        assert_eq!(journal.device_id(), device_id);
        assert_eq!(journal.base_sequence(), base_sequence);
        assert_eq!(journal.next_sequence(), base_sequence + 1);
        assert_eq!(
            journal.committed_bytes(),
            LOCAL_JOURNAL_SEGMENT_HEADER_BYTES as u64 + expected.encode().unwrap().len() as u64
        );
        assert_eq!(journal.expected_successful_append_data_syncs(), 2);
        assert_eq!(
            append.data_durability_syncs,
            journal.expected_successful_append_data_syncs()
        );

        let mut replayed = Vec::new();
        assert_eq!(journal.replay(|frame| replayed.push(frame)).unwrap(), 1);
        assert_eq!(replayed, vec![expected]);
    }
}
