#[cfg(windows)]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsMaybeDirExt as _};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
#[cfg(unix)]
use std::ffi::CString;
use std::fmt;
use std::fs;
#[cfg(any(test, target_os = "android"))]
use std::io;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ahash::AHashMap;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions, ReadDir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smallvec::SmallVec;
use uuid::Uuid;

use super::enrollment::{EnrollmentBindingV1, ResumePointEnrollmentBinding};
use super::hot_engine::RuntimeResumeSnapshot;
use super::identity::{parse_digest, ARCHIVE_INSTANCE_CLAIM_FILE};
#[cfg(test)]
use super::resume_point::{clear_resume_points_in, ResumePointMaintenance};
use super::resume_point::{
    next_resume_sequence, prune_resume_points_below, ResumeEnrollmentAdmission, ResumePointError,
    ResumePointScan, ResumePointSet, RuntimeResumePointV2, MAX_RETAINED_RESUME_POINTS,
    RESUME_POINT_DIR,
};
use super::scratch_store::MAX_RETAINED_SCRATCH_RUNS;
#[cfg(test)]
use super::sync_layout::BLOCK_CLAIM_INDEX_DIR;
use super::sync_layout::{
    ARCHIVE_BATCHES_DIR as BATCHES_DIR, ARCHIVE_BOOTSTRAP_DIR as BOOTSTRAP_DIR,
    ARCHIVE_OBJECTS_DIR as OBJECTS_DIR, BLOCK_CLAIM_INDEX_FILE, BOOTSTRAP_AGGREGATES_DIR,
    BOOTSTRAP_COMMITS_DIR, BOOTSTRAP_EVIDENCE_DIR, BOOTSTRAP_OBJECTS_DIR, BOOTSTRAP_PARTS_DIR,
    BOOTSTRAP_PART_PACKS_DIR, BOOTSTRAP_PART_SPANS_DIR, BOOTSTRAP_SOURCE_BLOB_DIR,
    BOOTSTRAP_SOURCE_CHUNKS_DIR, BOOTSTRAP_SOURCE_INVENTORY_DIR, ENGINE_HISTORY_CLAIM_FILE,
    ENGINE_HISTORY_DIR, ENGINE_HISTORY_HEAD_FILE, ENGINE_HISTORY_NODES_DIR,
    ENGINE_HISTORY_ROOTS_DIR, ENGINE_HISTORY_ROOT_SUFFIX, ENGINE_HISTORY_TRANSITION_LOCK_FILE,
    LINEAGE_CLAIM_FILE, LOGSEQ_CLAIM_INDEX_DIR, PAGE_NAME_OWNERSHIP_INDEX_DIR,
    PORTABLE_PATH_INDEX_DIR, PROJECTION_WORK_DIR, PROMOTED_RUNTIME_STATE_FILE,
};
use super::{
    BatchError, BatchId, BatchOrigin, CanonicalArchiveResourceId, ContentDigest, DeviceId,
    DocumentId, ImportId, LineageDigest, ObjectDescriptor, OperationBatch, OperationObject,
    PreparedBatch, SessionId, ValidatedBatch, WorkspaceId, MAX_MANIFEST_BYTES, MAX_OBJECT_BYTES,
};

/// Retained, O(1)-memory enumeration of immutable manifest commit markers.
///
/// The cursor deliberately preserves the filesystem iterator instead of
/// materializing and sorting the complete archive. Callers which need a full
/// audit continue to use [`ObjectStore::committed_manifests`].
pub(crate) struct ObjectStoreManifestCursor {
    entries: ReadDir,
}
const ENGINE_HISTORY_ROOT_SCHEMA_VERSION: u32 = 8;
/// Device-local promoted-runtime state, published beside the endpoint's durable
/// engine history.
/// The first honest promoted-runtime state format. No earlier experimental
/// bytes were ever published, and any other value is rejected rather than
/// reinterpreted.
pub(crate) const PROMOTED_RUNTIME_STATE_SCHEMA_VERSION: u32 = 2;
const MAX_PROMOTED_RUNTIME_STATE_BYTES: u64 = 4096;
const MAX_ENGINE_HISTORY_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_ENGINE_HISTORY_INDEX_BYTES: u64 = 2 * 1024 * 1024;
const ENGINE_HISTORY_INDEX_SCHEMA_VERSION: u32 = 1;
pub(crate) const ENGINE_HISTORY_RADIX_DEPTH: u8 = 32;
const BLOCK_CLAIM_INDEX_SCHEMA_VERSION: u32 = 1;
const BLOCK_CLAIM_RADIX_DEPTH: u8 = 32;
// Large replay batches touch most hash prefixes. Keeping tens of thousands of
// compact claim records per leaf bounds point depth while avoiding hundreds
// of thousands of tiny copy-on-write page appends and syscalls. The encoded
// page byte ceiling remains the independent fail-closed bound.
const BLOCK_CLAIM_LEAF_ENTRIES: usize = 65_536;
const BLOCK_CLAIM_INDEX_LEVELS: usize = 8;
const BLOCK_CLAIM_SEGMENTS_PER_LEVEL: usize = 32;
const BLOCK_CLAIM_FILTER_BITS_PER_ENTRY: usize = 16;
const BLOCK_CLAIM_FILTER_HASHES: u64 = 7;
const BLOCK_CLAIM_GLOBAL_FILTER_BYTES: usize = 1024 * 1024;
const MAX_BLOCK_CLAIM_RECORD_BYTES: usize = 64 * 1024;
const MAX_BLOCK_CLAIM_PAGE_BYTES: usize = 8 * 1024 * 1024;

thread_local! {
    // This one hook is also used by the crate-private deterministic simulator.
    // It is deliberately narrower than a general object-store fault injector:
    // the only observable boundary is after every immutable object is durable
    // and before the manifest commit marker is published.
    static HARNESS_PUBLISH_FAIL_AFTER_OBJECTS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    static ENROLLED_OPEN_USE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static ENROLLED_OPEN_ACT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static SEALED_HISTORY_AFTER_PREFLIGHT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static SEALED_HISTORY_AUTHORITY_WINDOW_HOOK:
        std::cell::RefCell<Option<Box<dyn FnMut(SealedHistoryAuthorityWindowStage)>>> =
        std::cell::RefCell::new(None);
    static ADVISORY_TRANSITION_CONTENTION_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static ENGINE_HISTORY_FAIL_BEFORE_HEAD_SWAP: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static ENGINE_HISTORY_FAIL_AFTER_HEAD_SWAP: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum SealedHistoryAuthorityWindowStage {
    Locked,
    Validated,
}

#[cfg(test)]
pub(crate) fn fail_next_engine_history_head_swap() {
    ENGINE_HISTORY_FAIL_BEFORE_HEAD_SWAP.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn fail_next_engine_history_after_head_swap() {
    ENGINE_HISTORY_FAIL_AFTER_HEAD_SWAP.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn fail_next_publish_after_objects() {
    fail_next_publish_after_objects_for_harness();
}

pub(crate) fn fail_next_publish_after_objects_for_harness() {
    HARNESS_PUBLISH_FAIL_AFTER_OBJECTS.with(|fail| fail.set(true));
}

thread_local! {
    static ARCHIVE_INSTALL_CUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm one deterministic cut immediately after the next staged archive
/// artifact is installed under its final name. On a strict publication this is
/// after the data barrier; on a turn-covered publication the first object cut
/// is deliberately before it, while the exact journal frame remains undrained.
#[cfg(test)]
pub(crate) fn cut_after_next_archive_install() {
    ARCHIVE_INSTALL_CUT.with(|armed| armed.set(true));
}

fn archive_install_cut_hook() -> Result<(), StoreError> {
    ARCHIVE_INSTALL_CUT.with(|armed| {
        if armed.replace(false) {
            Err(StoreError::Io(std::io::Error::other(
                "deterministic cut after an archive install",
            )))
        } else {
            Ok(())
        }
    })
}

fn publish_after_objects_hook() -> Result<(), StoreError> {
    HARNESS_PUBLISH_FAIL_AFTER_OBJECTS.with(|fail| {
        if fail.replace(false) {
            Err(StoreError::Io(std::io::Error::other(
                "deterministic failure after object publication",
            )))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
pub(crate) fn set_enrolled_open_use_hook(hook: impl FnOnce() + 'static) {
    ENROLLED_OPEN_USE_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
pub(crate) fn set_enrolled_open_act_hook(hook: impl FnOnce() + 'static) {
    ENROLLED_OPEN_ACT_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn set_sealed_history_after_preflight_hook(hook: impl FnOnce() + 'static) {
    SEALED_HISTORY_AFTER_PREFLIGHT_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn sealed_history_after_preflight_hook() {
    SEALED_HISTORY_AFTER_PREFLIGHT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn set_sealed_history_authority_window_hook(
    hook: impl FnMut(SealedHistoryAuthorityWindowStage) + 'static,
) {
    SEALED_HISTORY_AUTHORITY_WINDOW_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn sealed_history_authority_window_hook(stage: SealedHistoryAuthorityWindowStage) {
    SEALED_HISTORY_AUTHORITY_WINDOW_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(hook) = slot.as_mut() {
            hook(stage);
        }
        if matches!(stage, SealedHistoryAuthorityWindowStage::Validated) {
            slot.take();
        }
    });
}

#[cfg(test)]
fn set_advisory_transition_contention_hook(hook: impl FnOnce() + 'static) {
    ADVISORY_TRANSITION_CONTENTION_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn enrolled_open_use_hook() {
    ENROLLED_OPEN_USE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn enrolled_open_use_hook() {}

#[cfg(test)]
fn enrolled_open_act_hook() {
    ENROLLED_OPEN_ACT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn enrolled_open_act_hook() {}

/// A caller-rooted, v2-candidate immutable object and batch-manifest store.
///
/// Opening this type is the only persistence trigger. It is intentionally not
/// connected to graph startup, enrollment, or the legacy managed-sync store.
#[derive(Debug)]
pub struct ObjectStore {
    root_path: PathBuf,
    workspace_id: WorkspaceId,
    capability: Dir,
    counters: Arc<StoreCounters>,
}

/// One-shot bootstrap installer token. It seals only durable history and never
/// opens or creates projection-work authority.
#[cfg(test)]
pub(crate) struct HistoryOnlyOpen {
    store: Option<ObjectStore>,
    binding: super::hot_engine::ProjectionStorageBinding,
    history: Option<SealedControl<DurableEngineHistoryStore>>,
}

enum SealedControl<T> {
    Existing(T),
    Absent(AbsentControlName),
}

struct AbsentControlName {
    namespace_name: &'static str,
    namespace: Option<Dir>,
    namespace_identity: Option<ControlDirectoryIdentity>,
    endpoint_name: String,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ControlDirectoryIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ControlDirectoryIdentity {
    volume: u64,
    file_id: [u8; 16],
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ControlDirectoryIdentity;

impl ControlDirectoryIdentity {
    pub(crate) fn binding_digest(self) -> ContentDigest {
        let mut hasher = Sha256::new();
        hasher.update(b"tine/control-directory-identity-binding/v1\0");
        self.hash_platform_identity(&mut hasher);
        ContentDigest::from_bytes(hasher.finalize().into())
    }

    fn hash_platform_identity(self, hasher: &mut Sha256) {
        #[cfg(unix)]
        {
            hasher.update(b"unix-dev-inode\0");
            hasher.update(self.device.to_be_bytes());
            hasher.update(self.inode.to_be_bytes());
        }
        #[cfg(windows)]
        {
            hasher.update(b"windows-volume-file-id\0");
            hasher.update(self.volume.to_be_bytes());
            hasher.update(self.file_id);
        }
        #[cfg(not(any(unix, windows)))]
        {
            hasher.update(b"unsupported\0");
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AcceptedReadStats {
    pub manifest_reads: usize,
    pub object_reads: usize,
}

/// Process-wide `inspect_batch` cost, for the F49 quadratic probe only.
///
/// The per-store `counters` are reset with the store; these are not, so an
/// import's *total* re-read volume can be read once at the end of a run.
/// Diagnostic only — nothing reads these outside the probe.
pub static INSPECT_BATCH_CALLS: AtomicUsize = AtomicUsize::new(0);
/// `caller file:line -> (calls, required objects)`. `#[track_caller]` on
/// `inspect_batch` makes this exact without touching a single call site --
/// which matters, because guessing which callers dominate has already been
/// wrong once.
pub static INSPECT_BATCH_SITES: std::sync::Mutex<
    std::collections::BTreeMap<String, (usize, usize)>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());
pub static INSPECT_BATCH_OBJECT_READS: AtomicUsize = AtomicUsize::new(0);
pub static INSPECT_BATCH_OBJECT_BYTES: AtomicUsize = AtomicUsize::new(0);
pub static INSPECT_BATCH_DIGEST_BYTES: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ObjectStoreStats {
    pub directory_enumerations: usize,
    pub accepted_manifest_reads: usize,
    pub accepted_object_reads: usize,
    pub dag_manifest_reads: usize,
    pub history_record_reads: usize,
    pub history_index_reads: usize,
    pub history_index_writes: usize,
    pub history_decodes: usize,
    pub block_claim_index_reads: usize,
    pub block_claim_index_writes: usize,
    pub block_claim_index_syncs: usize,
    pub inspected_manifest_operations: usize,
    pub inspected_manifest_bytes: usize,
    pub inspected_object_operations: usize,
    pub inspected_object_bytes: usize,
}

#[derive(Debug, Default)]
struct StoreCounters {
    directory_enumerations: AtomicUsize,
    accepted_manifest_reads: AtomicUsize,
    accepted_object_reads: AtomicUsize,
    dag_manifest_reads: AtomicUsize,
    history_record_reads: AtomicUsize,
    history_index_reads: AtomicUsize,
    history_index_writes: AtomicUsize,
    history_decodes: AtomicUsize,
    block_claim_index_reads: AtomicUsize,
    block_claim_index_writes: AtomicUsize,
    block_claim_index_syncs: AtomicUsize,
    inspected_manifest_operations: AtomicUsize,
    inspected_manifest_bytes: AtomicUsize,
    inspected_object_operations: AtomicUsize,
    inspected_object_bytes: AtomicUsize,
}

#[derive(Debug)]
pub(crate) struct EngineHistoryStore {
    capability: Dir,
    counters: Arc<StoreCounters>,
    /// Sticky, process-local evidence that this exact open has already observed
    /// a durable engine-history storage fault: an index node that is missing,
    /// oversized, stored under the wrong content address, undecodable,
    /// non-canonical or structurally invalid, or a durable publication that did
    /// not complete. It only ever moves from `false` to `true`.
    ///
    /// The latch owns one job: while it is set, the store's
    /// authenticated-transition memo is disarmed, so every proof is decided by
    /// the complete walk a fresh open would perform. It lives beside the index
    /// because that is where almost every fault is observed;
    /// [`DurableEngineHistoryStore::publish`] latches it for the rest.
    storage_fault: AtomicBool,
}

#[derive(Debug)]
pub(crate) struct DurableEngineHistoryStore {
    workspace_id: WorkspaceId,
    endpoint_id: super::ProjectionEndpointId,
    graph_resource_id: super::CanonicalGraphResourceId,
    receipt_store_id: super::ProjectionReceiptStoreId,
    control: Dir,
    /// The retained no-follow capability of the archive root this control
    /// directory lives in. It is the only thing that can prove a promoted
    /// runtime state names *this* physical archive, so it is retained here
    /// rather than re-derived from an ambient pathname by each caller.
    archive_root: Dir,
    roots: Dir,
    index: EngineHistoryStore,
    transition_lock: fs::File,
    transition: Mutex<()>,
    authoritative_head: Mutex<Option<ContentDigest>>,
    /// Store-private, process-local memo of insertion-only transitions *this
    /// exact open* already authenticated. See
    /// [`Self::authenticate_current_history_extension`]; it is an accelerator
    /// for the walk, never an authority of its own, it never shortens the
    /// live-endpoint checks, and it is discarded permanently once
    /// [`EngineHistoryStore::storage_fault`] latches.
    authenticated_transitions: Mutex<Vec<AuthenticatedEngineHistoryTransition>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableEngineHistoryRoot {
    schema_version: u32,
    workspace_id: WorkspaceId,
    endpoint_id: super::ProjectionEndpointId,
    graph_resource_id: super::CanonicalGraphResourceId,
    receipt_store_id: super::ProjectionReceiptStoreId,
    generation: u64,
    index_root: ContentDigest,
    latest_batch_id: Option<BatchId>,
    binding: DurableEngineHistoryBinding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableEngineHistoryBinding {
    engine: EngineHistoryBinding,
}

impl DurableEngineHistoryBinding {
    fn ordinary(engine: EngineHistoryBinding) -> Self {
        Self { engine }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ArchiveDiscoveryInspection {
    Absent,
    Residue,
}

/// Inspect one explicit existing archive root without constructing an
/// [`ObjectStore`] or any writer/runtime authority.
///
/// With no expected enrollment binding this is intentionally only a presence
/// probe, used to distinguish true absence from unexplained archive residue.
/// With a binding it opens the exact existing engine-history control no-follow,
/// validates its canonical claim and live root, strictly decodes the promoted
/// state, and checks the graph/archive/resource/control identities.
pub(crate) fn inspect_existing_archive_at(
    archive_root: &Path,
    _expected_binding: Option<&EnrollmentBindingV1>,
) -> Result<ArchiveDiscoveryInspection, StoreError> {
    let Some(_archive) = open_existing_archive_root_nofollow(archive_root)? else {
        return Ok(ArchiveDiscoveryInspection::Absent);
    };
    // Every archive in this namespace predates the clean 0.7 storage format.
    // Preserve it through the caller archive-aside flow; never decode it into
    // current runtime authority.
    Ok(ArchiveDiscoveryInspection::Residue)
}

fn open_existing_archive_root_nofollow(root: &Path) -> Result<Option<Dir>, StoreError> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::UnsafeEntry(
            "archive root is not a real no-follow directory".into(),
        ));
    }
    let name = root
        .file_name()
        .ok_or_else(|| StoreError::UnsafeEntry("archive root has no final component".into()))?;
    if !matches!(root.components().next_back(), Some(Component::Normal(_))) {
        return Err(StoreError::UnsafeEntry(
            "archive root must end in a normal path component".into(),
        ));
    }
    let name = name.to_str().ok_or_else(|| {
        StoreError::UnsafeEntry("archive root final component is not UTF-8".into())
    })?;
    let parent = root.parent().ok_or_else(|| {
        StoreError::UnsafeEntry("archive root must have an existing parent".into())
    })?;
    let canonical_parent = fs::canonicalize(parent)?;
    let parent = Dir::open_ambient_dir(&canonical_parent, ambient_authority())?;
    open_existing_dir_nofollow(&parent, name)
}

#[cfg(test)]
mod discovery_inspector_tests {
    use super::*;

    #[test]
    fn explicit_archive_presence_probe_creates_nothing() {
        let parent =
            std::env::temp_dir().join(format!("tine-archive-discovery-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&parent).unwrap();
        let archive = parent.join("archive");
        assert_eq!(
            inspect_existing_archive_at(&archive, None).unwrap(),
            ArchiveDiscoveryInspection::Absent
        );
        assert!(!archive.exists());

        std::fs::create_dir(&archive).unwrap();
        assert_eq!(
            inspect_existing_archive_at(&archive, None).unwrap(),
            ArchiveDiscoveryInspection::Residue
        );
        assert!(std::fs::read_dir(&archive).unwrap().next().is_none());
        crate::test_support::remove_dir_all(parent);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EngineHistoryAuthority {
    pub generation: u64,
    pub index_root: ContentDigest,
}

/// Opaque proof that one authenticated durable history is either exact or an
/// insertion-only prefix of another. Only the history store can mint this
/// witness; projection authority must not move between raw generation/root
/// pairs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthenticatedEngineHistoryTransition {
    before: EngineHistoryAuthority,
    after: EngineHistoryAuthority,
}

impl AuthenticatedEngineHistoryTransition {
    pub(crate) const fn before(self) -> EngineHistoryAuthority {
        self.before
    }

    pub(crate) const fn after(self) -> EngineHistoryAuthority {
        self.after
    }

    #[cfg(test)]
    pub(crate) const fn for_test(
        before: EngineHistoryAuthority,
        after: EngineHistoryAuthority,
    ) -> Self {
        Self { before, after }
    }
}

/// How many distinct anchors one open memoizes transitions for. A promoted
/// runtime revalidates from exactly one immutable bootstrap anchor, so this
/// only needs headroom for an incidental second caller; the memo must stay a
/// couple of pointer-sized pairs, not a history cache.
const MAX_AUTHENTICATED_TRANSITION_ANCHORS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EngineHistoryBinding {
    pub portable_path_key_version: u32,
    pub portable_path_root: ContentDigest,
    pub catalog_checkpoint_binding: ContentDigest,
    pub portable_path_conflicts: Vec<super::PortablePathConflict>,
    pub terminal_evidence: Option<EngineTerminalEvidenceBinding>,
    pub page_names: PageNameDurableBinding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EngineTerminalEvidenceBinding {
    pub conflict_root: ContentDigest,
    pub conflict_count: u64,
    pub participant_count: u64,
    pub canonical_digest: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageNameDurableBinding {
    pub ownership_root: super::page_name_index::PageNameOwnershipRootV1,
    pub conflicts: Vec<super::page_name_index::PageNameConflictEvidenceV1>,
}

impl PageNameDurableBinding {
    pub(crate) fn empty() -> Self {
        Self {
            ownership_root: super::page_name_index::PageNameOwnershipRootV1::empty(),
            conflicts: Vec::new(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        self.ownership_root.encode()?;
        let digests = self
            .conflicts
            .iter()
            .map(super::page_name_index::PageNameConflictEvidenceV1::digest)
            .collect::<Result<Vec<_>, _>>()?;
        if digests.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(StoreError::MalformedHistoryIndex);
        }
        Ok(())
    }
}

impl EngineHistoryBinding {
    pub(crate) fn empty() -> Self {
        Self {
            portable_path_key_version: super::PORTABLE_PATH_KEY_VERSION,
            portable_path_root: super::PortablePathIndexRoot::empty().digest(),
            catalog_checkpoint_binding: ContentDigest::of(
                b"tine/empty-catalog-checkpoint-binding/v1",
            ),
            portable_path_conflicts: Vec::new(),
            terminal_evidence: None,
            page_names: PageNameDurableBinding::empty(),
        }
    }

    /// Compare the replay-stable typed authority. The catalog checkpoint is
    /// intentionally omitted because it embeds fresh scratch-run page
    /// references; authenticated recovery applies the same rule while exact
    /// historical record bytes continue to protect the retained checkpoint.
    pub(crate) fn same_replay_authority(&self, other: &Self) -> bool {
        self.portable_path_key_version == other.portable_path_key_version
            && self.portable_path_root == other.portable_path_root
            && self.portable_path_conflicts == other.portable_path_conflicts
            && self.terminal_evidence == other.terminal_evidence
            && self.page_names == other.page_names
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct BlockClaimIndexRoot {
    next_generation: u64,
    global_filter: Option<BlockClaimPageRef>,
    levels:
        [[Option<BlockClaimSegmentRef>; BLOCK_CLAIM_SEGMENTS_PER_LEVEL]; BLOCK_CLAIM_INDEX_LEVELS],
}

/// Test-only saturation of the block-claim root, for the resume-point byte
/// ceiling proof.
///
/// Every member here is fixed-size — `BlockClaimPageRef` carries no key span —
/// so the whole root's width is decided by the two fixed array dimensions and
/// the widest encodable field values.
#[cfg(test)]
impl BlockClaimIndexRoot {
    pub(crate) fn saturated_for_test() -> Self {
        let page_ref = BlockClaimPageRef {
            offset: u64::MAX,
            encoded_len: u32::MAX,
            digest: ContentDigest::of(b"saturated block claim page"),
        };
        Self {
            next_generation: u64::MAX,
            global_filter: Some(page_ref),
            levels: [[Some(BlockClaimSegmentRef {
                generation: u64::MAX,
                entry_count: u64::MAX,
                page_ref,
                filter_ref: page_ref,
            }); BLOCK_CLAIM_SEGMENTS_PER_LEVEL]; BLOCK_CLAIM_INDEX_LEVELS],
        }
    }
}

#[derive(Debug)]
pub(crate) struct BlockClaimIndexStore {
    backing: BlockClaimIndexBacking,
    counters: Arc<StoreCounters>,
}

#[derive(Debug)]
enum BlockClaimIndexBacking {
    Scratch(Arc<super::scratch_store::ScratchStore>),
    #[cfg(test)]
    Standalone(Mutex<fs::File>),
}

impl BlockClaimIndexStore {
    /// A run-local block-claim point index over a caller-owned scratch store.
    ///
    /// The block-claim root is reconstructible run-local derived state — no
    /// accepted cold record binds it — so it belongs in whichever scratch run
    /// owns the engine. Detached bootstrap authoring owns its own disposable
    /// scratch run rather than the archive's, and builds its point index the
    /// same way an enrolled engine does instead of falling back to the bounded
    /// in-memory test map, whose fixed capacity would otherwise cap an
    /// importable graph at a few thousand blocks.
    pub(crate) fn for_scratch(
        scratch: Arc<super::scratch_store::ScratchStore>,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            backing: BlockClaimIndexBacking::Scratch(scratch),
            counters: Arc::new(StoreCounters::default()),
        })
    }
}

/// One run-local engine scratch pair over a **retained** scratch run.
///
/// This type is the retention capability. It is minted only by
/// [`ObjectStore::create_retained_engine_scratch`] and
/// [`ObjectStore::adopt_retained_engine_scratch`], has no public constructor,
/// no `Default`, and no `Clone`, so an engine that holds one can treat "this
/// run survives my death and may be named by a durable resume point" as a
/// structural fact rather than a re-read marker byte.
pub(crate) struct RetainedEngineScratch {
    scratch: Arc<super::scratch_store::ScratchStore>,
    claim_index: BlockClaimIndexStore,
    run_id: Uuid,
    binding_digest: ContentDigest,
}

impl fmt::Debug for RetainedEngineScratch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetainedEngineScratch")
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

impl RetainedEngineScratch {
    fn seal(
        store: &ObjectStore,
        scratch: super::scratch_store::ScratchStore,
    ) -> Result<Self, StoreError> {
        let scratch = Arc::new(scratch);
        let claim_index = store.engine_claim_index(Arc::clone(&scratch))?;
        let run_id = scratch.run_id();
        let binding_digest = scratch
            .binding_digest()
            .map_err(|error| StoreError::Scratch(error.to_string()))?;
        Ok(Self {
            scratch,
            claim_index,
            run_id,
            binding_digest,
        })
    }

    pub(crate) const fn run_id(&self) -> Uuid {
        self.run_id
    }

    pub(crate) const fn binding_digest(&self) -> ContentDigest {
        self.binding_digest
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Arc<super::scratch_store::ScratchStore>,
        BlockClaimIndexStore,
        RetainedScratchIdentity,
    ) {
        let identity = RetainedScratchIdentity {
            run_id: self.run_id,
            binding_digest: self.binding_digest,
        };
        (self.scratch, self.claim_index, identity)
    }
}

/// The durable identity of the retained run an engine is running on.
///
/// Carried by the engine so a later quiescent snapshot can name its own run
/// without re-deriving retention, and so observability can report which run a
/// restart adopted or refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetainedScratchIdentity {
    run_id: Uuid,
    binding_digest: ContentDigest,
}

impl RetainedScratchIdentity {
    pub(crate) const fn run_id(&self) -> Uuid {
        self.run_id
    }

    pub(crate) const fn binding_digest(&self) -> ContentDigest {
        self.binding_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct BlockClaimIndexValue(SmallVec<[u8; 64]>);

impl BlockClaimIndexValue {
    pub(crate) fn from_slice(bytes: &[u8]) -> Self {
        Self(SmallVec::from_slice(bytes))
    }

    pub(crate) fn from_vec(bytes: Vec<u8>) -> Self {
        Self(SmallVec::from_vec(bytes))
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BlockClaimPageRef {
    offset: u64,
    encoded_len: u32,
    digest: ContentDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BlockClaimSegmentRef {
    generation: u64,
    entry_count: u64,
    page_ref: BlockClaimPageRef,
    filter_ref: BlockClaimPageRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BlockClaimFilterPage {
    schema_version: u32,
    entry_count: u64,
    bit_len: u64,
    bits: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BlockClaimGlobalFilterPage {
    schema_version: u32,
    insertions: u64,
    bits: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum BlockClaimIndexPage {
    Branch {
        schema_version: u32,
        depth: u8,
        children: Vec<(u8, BlockClaimPageRef)>,
    },
    Leaf {
        schema_version: u32,
        depth: u8,
        entries: Vec<([u8; 16], BlockClaimIndexValue)>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum HistoryIndexNode {
    Branch {
        schema_version: u32,
        depth: u8,
        children: Vec<(u8, ContentDigest)>,
    },
    Leaf {
        schema_version: u32,
        batch_id: BatchId,
        record: Vec<u8>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchInspection {
    /// No manifest commit marker exists. Object-only residue remains invisible.
    Absent,
    /// The manifest is valid, but these canonical descriptors are not present.
    Staged {
        manifest: OperationBatch,
        missing: Vec<ObjectDescriptor>,
    },
    /// The manifest and its exact closed object set have been validated.
    Ready(ValidatedBatch),
}

fn parse_available_kib(meminfo: &str) -> Option<u64> {
    let value = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    value.checked_mul(1024)
}

fn finite_cgroup_available(maximum: &str, current: &str) -> Option<u64> {
    let maximum = maximum.trim().parse::<u64>().ok()?;
    if maximum >= (1_u64 << 60) {
        return None;
    }
    let current = current.trim().parse::<u64>().ok()?;
    Some(maximum.saturating_sub(current))
}

pub(crate) fn available_memory_bytes() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let host = fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|meminfo| parse_available_kib(&meminfo));
        let cgroup_v2 = fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .zip(fs::read_to_string("/sys/fs/cgroup/memory.current").ok())
            .and_then(|(maximum, current)| finite_cgroup_available(&maximum, &current));
        let cgroup_v1 = fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")
            .ok()
            .zip(fs::read_to_string("/sys/fs/cgroup/memory/memory.usage_in_bytes").ok())
            .and_then(|(maximum, current)| finite_cgroup_available(&maximum, &current));
        return [host, cgroup_v2, cgroup_v1].into_iter().flatten().min();
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: `status` is initialized with the exact ABI size required by
        // GlobalMemoryStatusEx and remains exclusively borrowed for the call.
        (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.ullAvailPhys)
    }
}

impl ObjectStore {
    /// Open or create a store at an explicit root and retain the opened
    /// directory capability for all later operations.
    pub fn open(root: &Path, workspace_id: WorkspaceId) -> Result<Self, StoreError> {
        let store = Self::open_structural(root, workspace_id)?;
        store.validate_namespace()?;
        Ok(store)
    }

    /// Retain and structurally authenticate the archive without yet trusting
    /// immutable object contents. This narrow cold-open seam exists so the
    /// caller can acquire the workspace lease, recover undrained journal bytes,
    /// repair exactly covered torn object names, and only then run the ordinary
    /// full namespace validation.
    pub(crate) fn open_structural(
        root: &Path,
        workspace_id: WorkspaceId,
    ) -> Result<Self, StoreError> {
        let name = root
            .file_name()
            .ok_or_else(|| StoreError::UnsafeEntry("store root has no final component".into()))?;
        if !matches!(root.components().next_back(), Some(Component::Normal(_))) {
            return Err(StoreError::UnsafeEntry(
                "store root must end in a normal path component".into(),
            ));
        }
        let parent = root.parent().ok_or_else(|| {
            StoreError::UnsafeEntry("store root must have an existing parent".into())
        })?;
        let canonical_parent = fs::canonicalize(parent)?;
        let parent_capability = Dir::open_ambient_dir(&canonical_parent, ambient_authority())?;
        let relative = Path::new(name);
        let name = name.to_str().ok_or_else(|| {
            StoreError::UnsafeEntry("store root final component is not UTF-8".into())
        })?;

        match parent_capability.symlink_metadata(relative) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(StoreError::UnsafeEntry(
                    "store root is not a real no-follow directory".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                parent_capability.create_dir(relative)?;
                sync_dir_required(&parent_capability)?;
            }
            Err(error) => return Err(error.into()),
        }

        let capability = open_dir_nofollow(&parent_capability, name)?;
        ensure_directory(&capability, OBJECTS_DIR)?;
        ensure_directory(&capability, BATCHES_DIR)?;
        Ok(Self {
            root_path: canonical_parent.join(name),
            workspace_id,
            capability,
            counters: Arc::new(StoreCounters::default()),
        })
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Duplicate the retained no-follow archive-root capability that roots the
    /// workspace runtime lease.
    ///
    /// The lease is deliberately archive-rooted rather than app-data-rooted:
    /// the returned handle is the same physical directory resource this store
    /// already authenticated, so two processes with different XDG, HOME, or
    /// Flatpak roots still contend on one lock, and renaming the archive
    /// pathname cannot split it.
    pub(crate) fn workspace_runtime_lease_capability(&self) -> std::io::Result<Dir> {
        self.capability.try_clone()
    }

    /// Duplicate the retained archive capability for a lease-owned private
    /// derived namespace. The caller never reopens `root_path`, so an archive
    /// rename cannot redirect completion-index reads or writes.
    pub(crate) fn private_derived_root_capability(&self) -> std::io::Result<Dir> {
        self.capability.try_clone()
    }

    /// Install one coalesced group of immutable private derived objects with
    /// the archive batch protocol: data first, final names second, and one
    /// directory barrier for the shared namespace.
    pub(crate) fn publish_coalesced_private_derived(
        &self,
        namespace: &Dir,
        artifacts: &[(&str, &[u8], u64)],
        collision_kind: &'static str,
    ) -> Result<(), StoreError> {
        let mut publication = ArchiveBatchPublication::strict(&self.capability)?;
        let namespace_index = publication.namespace(namespace)?;
        for (name, bytes, limit) in artifacts {
            publication.stage(
                namespace_index,
                name,
                bytes,
                *limit,
                Collision::Exact(collision_kind),
                false,
            )?;
        }
        publication.commit()
    }

    /// Duplicate this store directly from its retained no-follow archive-root
    /// capability.
    ///
    /// The duplicate is the *same* physical directory resource, never a fresh
    /// ambient pathname open, so a caller that already authenticated one exact
    /// archive can hand a consuming API (`seal_history_only`, `seal_enrolled_projection`)
    /// its own store value without ever reintroducing a pathname race. An
    /// archive renamed while retained open stays bound to the enrolled archive,
    /// and a look-alike directory that appears at the old pathname is not
    /// reachable through the duplicate at all.
    pub(crate) fn duplicate_retained_capability(&self) -> Result<Self, StoreError> {
        Ok(Self {
            root_path: self.root_path.clone(),
            workspace_id: self.workspace_id,
            capability: self.capability.try_clone()?,
            counters: Arc::clone(&self.counters),
        })
    }

    /// Prove this store's retained capability and its enrolled archive pathname
    /// still name one and the same physical directory.
    ///
    /// The retained capability remains the authority; this only refuses an
    /// *ambiguous* archive. If the archive was renamed while it stayed retained
    /// open and a look-alike directory now occupies the enrolled pathname, then
    /// two different directories both answer to "the enrolled archive": one by
    /// resource identity, one by pathname. A one-shot durable publication must
    /// block there rather than silently pick a winner, because the two
    /// candidates diverge immediately afterwards.
    ///
    /// Nothing is created, repaired, claimed, or written. The check is one
    /// ambient parent open, one no-follow child open, and one identity stat.
    pub(crate) fn authenticate_unambiguous_archive_pathname(&self) -> Result<(), StoreError> {
        let parent = self.root_path.parent().ok_or_else(|| {
            StoreError::UnsafeEntry("store root must have an existing parent".into())
        })?;
        let name = self
            .root_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                StoreError::UnsafeEntry("store root final component is not UTF-8".into())
            })?;
        let parent_capability = Dir::open_ambient_dir(parent, ambient_authority())?;
        let named = open_existing_dir_nofollow(&parent_capability, name)?.ok_or_else(|| {
            StoreError::UnsafeEntry(
                "enrolled archive pathname no longer names a real no-follow directory".into(),
            )
        })?;
        if control_directory_identity(&named)? != self.canonical_archive_identity()? {
            return Err(StoreError::UnsafeEntry(
                "enrolled archive pathname no longer names the retained archive capability".into(),
            ));
        }
        Ok(())
    }

    /// Validate and retain one object independently of any manifest delivery.
    pub fn stage_object_bytes(&self, bytes: &[u8]) -> Result<ContentDigest, StoreError> {
        let object = OperationObject::decode(bytes)?;
        if object.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: object.workspace_id(),
            });
        }
        let digest = ContentDigest::of(bytes);
        let objects = self.open_namespace(OBJECTS_DIR)?;
        publish_immutable(
            &objects,
            &object_filename(digest),
            bytes,
            Collision::Object(digest),
        )?;
        Ok(digest)
    }

    /// Validate and publish the sole batch commit marker. Missing objects do
    /// not prevent staging the marker and remain invisible until complete.
    pub fn stage_manifest_bytes(&self, bytes: &[u8]) -> Result<BatchId, StoreError> {
        self.stage_manifest_bytes_impl(bytes, false)
    }

    /// Receive a manifest through one exact shared-enrollment descriptor.
    ///
    /// Historical bootstrap manifests are admitted only on this path. The
    /// descriptor authority is checked independently of the manifest before
    /// the ordinary immutable collision and lineage validation runs.
    pub(crate) fn stage_shared_provider_manifest_bytes(
        &self,
        ingress: &super::enrollment::SharedProviderIngressAuthority,
        bytes: &[u8],
    ) -> Result<BatchId, StoreError> {
        let manifest = OperationBatch::decode(bytes)?;
        if ingress.workspace_id() != self.workspace_id
            || manifest.workspace_id() != ingress.workspace_id()
        {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: manifest.workspace_id(),
            });
        }
        if manifest.lineage_digest() != ingress.lineage_digest() {
            return Err(StoreError::LineageMismatch {
                expected: ingress.lineage_digest(),
                found: manifest.lineage_digest(),
            });
        }
        self.stage_manifest_bytes_impl(bytes, true)
    }

    fn stage_manifest_bytes_impl(
        &self,
        bytes: &[u8],
        allow_bootstrap: bool,
    ) -> Result<BatchId, StoreError> {
        let manifest = OperationBatch::decode(bytes)?;
        if !allow_bootstrap && manifest.origin() == BatchOrigin::BootstrapImport {
            return Err(StoreError::BootstrapBatchRequiresDirectPublication);
        }
        if manifest.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: manifest.workspace_id(),
            });
        }
        let batch_id = manifest.batch_id();
        let batches = self.open_namespace(BATCHES_DIR)?;
        let filename = manifest_filename(batch_id);
        if read_optional_regular(&batches, &filename, MAX_MANIFEST_BYTES as u64, None)?.is_some() {
            self.check_or_establish_lineage(manifest.lineage_digest())?;
            publish_immutable(&batches, &filename, bytes, Collision::Batch(batch_id))?;
            return Ok(batch_id);
        }
        self.check_or_establish_lineage(manifest.lineage_digest())?;
        publish_immutable(&batches, &filename, bytes, Collision::Batch(batch_id))?;
        Ok(batch_id)
    }

    /// Publish a prevalidated complete batch in the required order: every
    /// content-addressed object first, then the manifest commit marker.
    pub fn publish_prepared(&self, batch: &PreparedBatch) -> Result<(), StoreError> {
        if batch.manifest().origin() == BatchOrigin::BootstrapImport {
            return Err(StoreError::BootstrapBatchRequiresDirectPublication);
        }
        self.publish_prepared_impl(batch, false, false)
    }

    /// Publish an ordinary batch whose exact bytes are still retained by the
    /// caller's undrained managed-local journal frame. Only that durable turn
    /// permits object installation without a pre-install data barrier.
    pub(crate) fn publish_turn_covered_prepared(
        &self,
        batch: &PreparedBatch,
    ) -> Result<(), StoreError> {
        if batch.manifest().origin() == BatchOrigin::BootstrapImport {
            return Err(StoreError::BootstrapBatchRequiresDirectPublication);
        }
        self.publish_prepared_impl(batch, false, true)
    }

    /// Publish one accepted batch's complete archive materialization.
    ///
    /// Ordinary callers use a strict batch barrier before any immutable name
    /// becomes visible. The managed-local drain may instead set
    /// `turn_covered_objects`: its durable, still-undrained journal frame is the
    /// exact recovery authority for object bytes, so object temporaries may be
    /// installed before the batch-wide data flush. That flush then covers both
    /// the installed object inodes and the staged manifest; the manifest is
    /// installed only afterward, and the journal checkpoint can advance only
    /// after publication returns. Cold open repairs only torn object names
    /// covered byte-for-byte by an undrained record, then runs the ordinary full
    /// namespace validation.
    fn publish_prepared_impl(
        &self,
        batch: &PreparedBatch,
        allow_bootstrap: bool,
        turn_covered_objects: bool,
    ) -> Result<(), StoreError> {
        let manifest = batch.manifest();
        if manifest.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: manifest.workspace_id(),
            });
        }
        if !allow_bootstrap && manifest.origin() == BatchOrigin::BootstrapImport {
            return Err(StoreError::BootstrapBatchRequiresDirectPublication);
        }
        let manifest_bytes = manifest.encode()?;
        let batch_id = manifest.batch_id();

        // The lineage claim is written once in an archive's lifetime and is a
        // precondition of every manifest, not part of this batch. It keeps its
        // own barrier so a manifest can never be durable before the lineage it
        // asserts.
        self.check_or_establish_lineage(manifest.lineage_digest())
            .map_err(|error| publication_stage_error("publish operation manifest", error))?;

        let objects = self.open_namespace(OBJECTS_DIR)?;
        let batches = self.open_namespace(BATCHES_DIR)?;
        let mut publication = if turn_covered_objects {
            ArchiveBatchPublication::turn_covered(&self.capability)?
        } else {
            ArchiveBatchPublication::strict(&self.capability)?
        };
        let objects_namespace = publication.namespace(&objects)?;
        let batches_namespace = publication.namespace(&batches)?;
        for object in batch.objects() {
            let bytes = object.encode()?;
            if object.workspace_id() != self.workspace_id {
                return Err(StoreError::WorkspaceMismatch {
                    expected: self.workspace_id,
                    found: object.workspace_id(),
                });
            }
            let digest = ContentDigest::of(&bytes);
            publication
                .stage(
                    objects_namespace,
                    &object_filename(digest),
                    &bytes,
                    MAX_OBJECT_BYTES as u64,
                    Collision::Object(digest),
                    false,
                )
                .map_err(|error| publication_stage_error("publish operation object", error))?;
        }
        publish_after_objects_hook()?;
        publication
            .stage(
                batches_namespace,
                &manifest_filename(batch_id),
                &manifest_bytes,
                MAX_MANIFEST_BYTES as u64,
                Collision::Batch(batch_id),
                true,
            )
            .map_err(|error| publication_stage_error("publish operation manifest", error))?;

        publication.commit().map_err(|error| {
            publication_stage_error("publish operation batch durability", error)
        })?;
        Ok(())
    }

    /// Inspect a single manifest and validate every present required object.
    /// Missing objects stage the batch; corrupt or mismatched objects reject it.
    #[track_caller]
    pub fn inspect_batch(&self, batch_id: BatchId) -> Result<BatchInspection, StoreError> {
        INSPECT_BATCH_CALLS.fetch_add(1, Ordering::Relaxed);
        let site = super::inspect_site_trace_enabled().then(|| {
            let caller = std::panic::Location::caller();
            format!("{}:{}", caller.file(), caller.line())
        });
        let batches = self.open_namespace(BATCHES_DIR)?;
        let filename = manifest_filename(batch_id);
        let manifest_bytes =
            match read_optional_regular(&batches, &filename, MAX_MANIFEST_BYTES as u64, None)? {
                None => return Ok(BatchInspection::Absent),
                Some(bytes) => bytes,
            };
        self.counters
            .inspected_manifest_operations
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .inspected_manifest_bytes
            .fetch_add(manifest_bytes.len(), Ordering::Relaxed);
        let manifest = OperationBatch::decode(&manifest_bytes)?;
        if manifest.batch_id() != batch_id {
            return Err(StoreError::ManifestPathMismatch {
                expected: batch_id,
                found: manifest.batch_id(),
            });
        }
        if manifest.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: manifest.workspace_id(),
            });
        }

        if let Some(site) = site {
            if let Ok(mut sites) = INSPECT_BATCH_SITES.lock() {
                let entry = sites.entry(site).or_insert((0, 0));
                entry.0 += 1;
                entry.1 += manifest.required_objects().len();
            }
        }
        let objects_dir = self.open_namespace(OBJECTS_DIR)?;
        let mut missing = Vec::new();
        let mut objects = Vec::with_capacity(manifest.required_objects().len());
        for descriptor in manifest.required_objects() {
            self.counters
                .inspected_object_operations
                .fetch_add(1, Ordering::Relaxed);
            let filename = object_filename(descriptor.content_digest());
            let Some(bytes) = read_optional_regular(
                &objects_dir,
                &filename,
                MAX_OBJECT_BYTES as u64,
                Some(descriptor.encoded_byte_length()),
            )?
            else {
                missing.push(descriptor.clone());
                continue;
            };
            self.counters
                .inspected_object_bytes
                .fetch_add(bytes.len(), Ordering::Relaxed);
            INSPECT_BATCH_OBJECT_READS.fetch_add(1, Ordering::Relaxed);
            INSPECT_BATCH_OBJECT_BYTES.fetch_add(bytes.len(), Ordering::Relaxed);
            INSPECT_BATCH_DIGEST_BYTES.fetch_add(bytes.len(), Ordering::Relaxed);
            let content_digest = ContentDigest::of(&bytes);
            if content_digest != descriptor.content_digest() {
                return Err(StoreError::ObjectPathMismatch(descriptor.content_digest()));
            }
            let object = OperationObject::decode(&bytes)?;
            if object.workspace_id() != self.workspace_id {
                return Err(StoreError::WorkspaceMismatch {
                    expected: self.workspace_id,
                    found: object.workspace_id(),
                });
            }
            let actual = ObjectDescriptor::new(
                object.document_id(),
                object.kind(),
                content_digest,
                bytes.len() as u64,
            )?;
            if actual != *descriptor {
                return Err(StoreError::Batch(BatchError::DescriptorMismatch {
                    expected: descriptor.clone(),
                    actual,
                }));
            }
            objects.push(object);
        }

        if !missing.is_empty() {
            return Ok(BatchInspection::Staged { manifest, missing });
        }
        // Exact lookup against the atomically established immutable lineage
        // claim keeps the Ready path independent of archive cardinality.
        // Store open and explicit `committed_manifests` remain full audits.
        self.require_lineage(manifest.lineage_digest())?;
        let prepared = PreparedBatch::new(manifest, objects)?;
        Ok(BatchInspection::Ready(ValidatedBatch::new(prepared)))
    }

    pub(crate) fn reload_accepted_document_object(
        &self,
        manifest: &OperationBatch,
        document_id: super::DocumentId,
    ) -> Result<OperationObject, StoreError> {
        let batch_id = manifest.batch_id();
        let descriptor = manifest
            .required_objects()
            .iter()
            .find(|descriptor| {
                descriptor.kind() == super::ObjectKind::CrdtUpdate
                    && descriptor.document_id() == document_id
            })
            .ok_or(StoreError::AcceptedDocumentUpdateMissing {
                batch_id,
                document_id,
            })?;
        let objects_dir = self.open_namespace(OBJECTS_DIR)?;
        let filename = object_filename(descriptor.content_digest());
        self.counters
            .accepted_object_reads
            .fetch_add(1, Ordering::Relaxed);
        crate::fast_commit::note_archive_object_read();
        let bytes = read_required_regular(
            &objects_dir,
            &filename,
            MAX_OBJECT_BYTES as u64,
            Some(descriptor.encoded_byte_length()),
        )?;
        if ContentDigest::of(&bytes) != descriptor.content_digest() {
            return Err(StoreError::ObjectPathMismatch(descriptor.content_digest()));
        }
        let object = OperationObject::decode(&bytes)?;
        if object.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: object.workspace_id(),
            });
        }
        let actual = object.descriptor()?;
        if actual != *descriptor {
            return Err(StoreError::Batch(BatchError::DescriptorMismatch {
                expected: descriptor.clone(),
                actual,
            }));
        }
        Ok(object)
    }

    pub(crate) fn reload_accepted_manifest(
        &self,
        batch_id: BatchId,
        expected_manifest_fingerprint: ContentDigest,
    ) -> Result<OperationBatch, StoreError> {
        let batches = self.open_namespace(BATCHES_DIR)?;
        let filename = manifest_filename(batch_id);
        self.counters
            .accepted_manifest_reads
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .dag_manifest_reads
            .fetch_add(1, Ordering::Relaxed);
        let bytes = read_required_regular(&batches, &filename, MAX_MANIFEST_BYTES as u64, None)?;
        let actual = ContentDigest::of(&bytes);
        if actual != expected_manifest_fingerprint {
            return Err(StoreError::AcceptedManifestMismatch {
                batch_id,
                expected: expected_manifest_fingerprint,
                actual,
            });
        }
        let manifest = OperationBatch::decode(&bytes)?;
        if manifest.batch_id() != batch_id {
            return Err(StoreError::ManifestPathMismatch {
                expected: batch_id,
                found: manifest.batch_id(),
            });
        }
        if manifest.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: manifest.workspace_id(),
            });
        }
        Ok(manifest)
    }

    pub(crate) fn accepted_read_stats(&self) -> AcceptedReadStats {
        AcceptedReadStats {
            manifest_reads: self
                .counters
                .accepted_manifest_reads
                .load(Ordering::Relaxed),
            object_reads: self.counters.accepted_object_reads.load(Ordering::Relaxed),
        }
    }

    pub fn instrumentation(&self) -> ObjectStoreStats {
        self.counters.snapshot()
    }

    /// Seal the durable-history control for inactive bootstrap installation.
    /// This performs the same no-follow, absence, retained-resource, and
    /// substitution checks as enrolled open without touching projection-work.
    #[cfg(test)]
    pub(crate) fn seal_history_only(
        self,
        binding: super::hot_engine::ProjectionStorageBinding,
    ) -> Result<HistoryOnlyOpen, (Self, StoreError)> {
        let mut history = match self.seal_existing_engine_history(binding) {
            Ok(history) => history,
            Err(error) => return Err((self, error)),
        };
        if let Err(error) = history.bind_absent_parent(&self.capability) {
            return Err((self, error));
        }
        Ok(HistoryOnlyOpen {
            store: Some(self),
            binding,
            history: Some(history),
        })
    }

    fn seal_existing_engine_history(
        &self,
        binding: super::hot_engine::ProjectionStorageBinding,
    ) -> Result<SealedControl<DurableEngineHistoryStore>, StoreError> {
        // Reject incompatible durable evidence before opening the durable lock
        // can create it. `open_sealed_existing` repeats the validation after
        // lock acquisition so a substitution in this window still fails shut.
        self.preflight_engine_history(binding)?;
        #[cfg(test)]
        sealed_history_after_preflight_hook();
        let Some(histories) = open_existing_dir_nofollow(&self.capability, ENGINE_HISTORY_DIR)?
        else {
            return Ok(SealedControl::Absent(AbsentControlName {
                namespace_name: ENGINE_HISTORY_DIR,
                namespace: None,
                namespace_identity: None,
                endpoint_name: binding.endpoint.endpoint_id.to_string(),
            }));
        };
        let endpoint_name = binding.endpoint.endpoint_id.to_string();
        let Some(control) = open_existing_dir_nofollow(&histories, &endpoint_name)? else {
            return Ok(SealedControl::Absent(AbsentControlName {
                namespace_name: ENGINE_HISTORY_DIR,
                namespace_identity: Some(control_directory_identity(&histories)?),
                namespace: Some(histories),
                endpoint_name,
            }));
        };
        let head = read_optional_regular(&control, ENGINE_HISTORY_HEAD_FILE, 64, None)?;
        let claim = read_optional_regular(&control, ENGINE_HISTORY_CLAIM_FILE, 256, None)?;
        match (head, claim) {
            (None, None) => Err(StoreError::MalformedHistoryIndex),
            (Some(_), Some(_)) => DurableEngineHistoryStore::open_sealed_existing(
                self.workspace_id,
                binding.endpoint.endpoint_id,
                binding.endpoint.graph_resource_id,
                binding.receipt_store_id,
                control,
                self.capability.try_clone()?,
                open_engine_history_transition_lock(&self.capability)?,
                Arc::clone(&self.counters),
            )
            .map(SealedControl::Existing),
            _ => Err(StoreError::MalformedHistoryIndex),
        }
    }

    #[cfg(test)]
    pub(crate) fn open_engine_history(
        &self,
        binding: super::hot_engine::ProjectionStorageBinding,
    ) -> Result<DurableEngineHistoryStore, StoreError> {
        self.preflight_engine_history(binding)?;
        let endpoint = binding.endpoint;
        ensure_directory_nofollow(&self.capability, ENGINE_HISTORY_DIR)?;
        let histories = open_dir_nofollow(&self.capability, ENGINE_HISTORY_DIR)?;
        let endpoint_name = endpoint.endpoint_id.to_string();
        ensure_directory_nofollow(&histories, &endpoint_name)?;
        let control = open_dir_nofollow(&histories, &endpoint_name)?;
        for name in [ENGINE_HISTORY_NODES_DIR, ENGINE_HISTORY_ROOTS_DIR] {
            ensure_directory_nofollow(&control, name)?;
        }
        DurableEngineHistoryStore::new(
            self.workspace_id,
            endpoint.endpoint_id,
            endpoint.graph_resource_id,
            binding.receipt_store_id,
            control.try_clone()?,
            self.capability.try_clone()?,
            open_dir_nofollow(&control, ENGINE_HISTORY_ROOTS_DIR)?,
            EngineHistoryStore {
                capability: open_dir_nofollow(&control, ENGINE_HISTORY_NODES_DIR)?,
                counters: Arc::clone(&self.counters),
                storage_fault: AtomicBool::new(false),
            },
            open_engine_history_transition_lock(&self.capability)?,
        )
    }

    fn open_absent_engine_history(
        &self,
        absence: AbsentControlName,
        binding: super::hot_engine::ProjectionStorageBinding,
    ) -> Result<DurableEngineHistoryStore, StoreError> {
        let control = absence.claim(&self.capability)?;
        for name in [ENGINE_HISTORY_NODES_DIR, ENGINE_HISTORY_ROOTS_DIR] {
            control.create_dir(name)?;
        }
        sync_dir_required(&control)?;
        DurableEngineHistoryStore::new(
            self.workspace_id,
            binding.endpoint.endpoint_id,
            binding.endpoint.graph_resource_id,
            binding.receipt_store_id,
            control.try_clone()?,
            self.capability.try_clone()?,
            open_dir_nofollow(&control, ENGINE_HISTORY_ROOTS_DIR)?,
            EngineHistoryStore {
                capability: open_dir_nofollow(&control, ENGINE_HISTORY_NODES_DIR)?,
                counters: Arc::clone(&self.counters),
                storage_fault: AtomicBool::new(false),
            },
            open_engine_history_transition_lock(&self.capability)?,
        )
    }

    #[cfg(test)]
    pub(crate) fn start_engine_history(&self) -> Result<EngineHistoryStore, StoreError> {
        ensure_directory(&self.capability, ENGINE_HISTORY_DIR)?;
        let histories = self.open_namespace(ENGINE_HISTORY_DIR)?;
        let run = format!("run-{}", Uuid::new_v4());
        ensure_directory(&histories, &run)?;
        Ok(EngineHistoryStore {
            capability: open_dir_nofollow(&histories, &run)?,
            counters: Arc::clone(&self.counters),
            storage_fault: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    pub(crate) fn start_block_claim_index(&self) -> Result<BlockClaimIndexStore, StoreError> {
        ensure_directory(&self.capability, BLOCK_CLAIM_INDEX_DIR)?;
        let indexes = self.open_namespace(BLOCK_CLAIM_INDEX_DIR)?;
        let run = format!("run-{}", Uuid::new_v4());
        ensure_directory(&indexes, &run)?;
        let run = open_dir_nofollow(&indexes, &run)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        let file = run.open_with(BLOCK_CLAIM_INDEX_FILE, &options)?.into_std();
        crate::durability_counters::sync_file(&file)?;
        sync_dir_required(&run)?;
        Ok(BlockClaimIndexStore {
            backing: BlockClaimIndexBacking::Standalone(Mutex::new(file)),
            counters: Arc::clone(&self.counters),
        })
    }

    /// Stable identity of the retained no-follow archive root capability.
    ///
    /// This is derived from the opened directory resource, never from an
    /// ambient path string, so two `ObjectStore` values opened over the same
    /// archive compare equal while a substituted directory does not.
    pub(crate) fn canonical_archive_identity(
        &self,
    ) -> Result<ControlDirectoryIdentity, StoreError> {
        control_directory_identity(&self.capability)
    }

    /// Authenticate the exact persisted canonical archive-resource claim
    /// retained inside this store's archive-root capability.
    ///
    /// This opens the already-enrolled archive-instance claim against the
    /// retained no-follow directory capability and confirms it derives to
    /// `expected`. It never derives, provisions, repairs, or overwrites the
    /// claim; a missing, substituted, or mismatched claim fails closed. The
    /// authenticated physical archive directory only proves its own control
    /// identity, so the persisted resource claim must be checked separately.
    pub(crate) fn validate_enrolled_archive_resource_id(
        &self,
        expected: super::CanonicalArchiveResourceId,
    ) -> std::io::Result<()> {
        super::CanonicalArchiveResourceId::open_enrolled_in_retained_directory(
            &self.capability,
            expected,
        )
        .map(|_| ())
    }

    /// Provision this store's canonical archive-resource claim exactly once and
    /// return its identity.
    ///
    /// The explicit local activation path uses this once to bind a newly
    /// created v2 archive to its enrollment. It goes through the same retained
    /// no-follow capability that
    /// [`Self::validate_enrolled_archive_resource_id`] later authenticates.
    pub(crate) fn provision_enrolled_archive_resource_id(
        &self,
    ) -> std::io::Result<super::CanonicalArchiveResourceId> {
        super::CanonicalArchiveResourceId::provision_in_retained_directory(&self.capability)
    }

    /// Publish or reopen the exact archive claim reserved in private
    /// application data before graph-local archive construction began.
    ///
    /// Publication uses the object store's immutable temp+sync+no-replace
    /// primitive. A crash may leave only a disposable temp, while retry always
    /// republishes the same canonical claim and refuses any different final
    /// claim instead of minting or adopting a replacement.
    pub(crate) fn provision_or_resume_local_activation_archive_resource_id(
        &self,
        instance_id: Uuid,
    ) -> Result<super::CanonicalArchiveResourceId, StoreError> {
        let claim = super::CanonicalArchiveResourceId::claim_bytes(instance_id)?;
        publish_immutable_exact(
            &self.capability,
            ARCHIVE_INSTANCE_CLAIM_FILE,
            &claim,
            "local activation archive claim",
        )?;
        super::CanonicalArchiveResourceId::open_exact_claim_in_retained_directory(
            &self.capability,
            &claim,
        )
        .map_err(StoreError::from)
    }

    pub(crate) fn start_engine_scratch(
        &self,
    ) -> Result<
        (
            Arc<super::scratch_store::ScratchStore>,
            BlockClaimIndexStore,
        ),
        StoreError,
    > {
        let scratch = Arc::new(
            super::scratch_store::ScratchStore::open(&self.capability, self.workspace_id)
                .map_err(|error| StoreError::Scratch(error.to_string()))?,
        );
        Ok((
            Arc::clone(&scratch),
            self.engine_claim_index(Arc::clone(&scratch))?,
        ))
    }

    /// Start only the disposable document scratch used by the clean managed
    /// runtime.
    ///
    /// The legacy runtime couples this scratch directory to a native block
    /// claim index.  Clean activation deliberately does not: current-state
    /// block ownership belongs to the frontier-stamped SQLite projection, and
    /// constructing the scratch must not create a second semantic index merely
    /// because the document engine needs spill space.
    pub(crate) fn start_clean_engine_scratch(
        &self,
    ) -> Result<Arc<super::scratch_store::ScratchStore>, StoreError> {
        super::scratch_store::ScratchStore::open(&self.capability, self.workspace_id)
            .map(Arc::new)
            .map_err(|error| StoreError::Scratch(error.to_string()))
    }

    fn engine_claim_index(
        &self,
        scratch: Arc<super::scratch_store::ScratchStore>,
    ) -> Result<BlockClaimIndexStore, StoreError> {
        Ok(BlockClaimIndexStore {
            backing: BlockClaimIndexBacking::Scratch(scratch),
            counters: Arc::clone(&self.counters),
        })
    }

    /// Mint a fresh **retained** engine scratch pair beneath this archive.
    ///
    /// The only difference from [`Self::start_engine_scratch`] is the run's own
    /// durable retention marker, which makes the run survive its owner's death
    /// instead of being reclaimed as disposable sibling state. Because
    /// [`RetainedEngineScratch`] can be minted only here and by
    /// [`Self::adopt_retained_engine_scratch`], holding one is itself the proof
    /// that the run is retained — the engine never has to re-derive retention
    /// from an ambient marker read.
    pub(crate) fn create_retained_engine_scratch(
        &self,
    ) -> Result<RetainedEngineScratch, StoreError> {
        let scratch = super::scratch_store::ScratchStore::create_retained(
            &self.capability,
            self.workspace_id,
        )
        .map_err(|error| StoreError::Scratch(error.to_string()))?;
        RetainedEngineScratch::seal(self, scratch)
    }

    /// Mint a retained archive-local run containing the exact byte address
    /// space of one live detached scratch run.
    ///
    /// This is the one-way same-process bootstrap migration seam. The source
    /// remains owned and leased by its detached candidate, the destination is
    /// freshly created beneath this exact archive capability, and no caller can
    /// supply roots independently of the source bytes. The enrolled engine
    /// still has to restore and authenticate a `RuntimeResumeSnapshot` against
    /// durable history before these reconstructible bytes become usable.
    pub(crate) fn create_retained_engine_scratch_from(
        &self,
        source: &super::scratch_store::ScratchStore,
    ) -> Result<RetainedEngineScratch, StoreError> {
        match source.clone_retained_into(&self.capability) {
            Ok(retained) => RetainedEngineScratch::seal(self, retained),
            Err(copy_error) => self
                .adopt_retained_engine_scratch(
                    source.run_id(),
                    source
                        .binding_digest()
                        .map_err(|error| StoreError::Scratch(error.to_string()))?,
                )
                .map_err(|adoption_error| {
                    StoreError::Scratch(format!(
                        "retained scratch migration failed ({copy_error}); retry adoption failed ({adoption_error})"
                    ))
                }),
        }
    }

    /// Adopt exactly one already-published retained run.
    ///
    /// Four independent facts must hold before this returns, and every one of
    /// them is read from the run's own durable bytes rather than asserted by the
    /// caller:
    ///
    /// 1. the run directory is reachable no-follow under *this* archive
    ///    capability's scratch namespace, under the canonical `run-<uuid>`
    ///    spelling of `run_id`;
    /// 2. its own exclusive lease is acquired, so no live owner is mutating it;
    /// 3. its marker authenticates as schema-current, retained, owned by this
    ///    workspace, and carrying exactly `run_id`, with a complete regular
    ///    entry set — all inside [`ScratchStore::adopt_retained`];
    /// 4. its canonical marker digest equals `binding_digest`, which is what
    ///    catches a *re-created* run that reused the same UUID: the owner nonce
    ///    is fresh, so the digest cannot match.
    ///
    /// Any failure is returned as an ordinary error and **changes nothing**: no
    /// directory, marker, lease, or data file is created, truncated, or
    /// replaced, so the candidate run's bytes are exactly as they were. The
    /// caller's correct response is a fresh retained run plus a full replay,
    /// never a repair. Adoption authorizes reuse of reconstructible bytes and
    /// nothing else.
    pub(crate) fn adopt_retained_engine_scratch(
        &self,
        run_id: Uuid,
        binding_digest: ContentDigest,
    ) -> Result<RetainedEngineScratch, StoreError> {
        let scratch = super::scratch_store::ScratchStore::adopt_retained(
            &self.capability,
            self.workspace_id,
            run_id,
        )
        .map_err(|error| StoreError::Scratch(error.to_string()))?;
        let sealed = RetainedEngineScratch::seal(self, scratch)?;
        if sealed.run_id != run_id || sealed.binding_digest != binding_digest {
            return Err(StoreError::RetainedScratchBindingMismatch);
        }
        Ok(sealed)
    }

    fn preflight_engine_history(
        &self,
        binding: super::hot_engine::ProjectionStorageBinding,
    ) -> Result<(), StoreError> {
        let Some(histories) = open_existing_dir_nofollow(&self.capability, ENGINE_HISTORY_DIR)?
        else {
            return Ok(());
        };
        let endpoint_name = binding.endpoint.endpoint_id.to_string();
        let Some(control) = open_existing_dir_nofollow(&histories, &endpoint_name)? else {
            return Ok(());
        };
        let head = read_optional_regular(&control, ENGINE_HISTORY_HEAD_FILE, 64, None)?;
        let claim = read_optional_regular(&control, ENGINE_HISTORY_CLAIM_FILE, 256, None)?;
        match (head, claim) {
            (None, None) => Ok(()),
            (Some(head), Some(claim)) => {
                validate_engine_history_claim(
                    &claim,
                    self.workspace_id,
                    binding.endpoint.endpoint_id,
                    binding.endpoint.graph_resource_id,
                    binding.receipt_store_id,
                )?;
                let _nodes = open_existing_dir_nofollow(&control, ENGINE_HISTORY_NODES_DIR)?
                    .ok_or(StoreError::MalformedHistoryIndex)?;
                let roots = open_existing_dir_nofollow(&control, ENGINE_HISTORY_ROOTS_DIR)?
                    .ok_or(StoreError::MalformedHistoryIndex)?;
                let text =
                    std::str::from_utf8(&head).map_err(|_| StoreError::MalformedHistoryIndex)?;
                let digest = parse_digest(text)
                    .map(ContentDigest::from_bytes)
                    .map_err(|_| StoreError::MalformedHistoryIndex)?;
                if digest.to_string().as_bytes() != head {
                    return Err(StoreError::MalformedHistoryIndex);
                }
                let bytes = read_optional_regular(
                    &roots,
                    &engine_history_root_filename(digest),
                    MAX_ENGINE_HISTORY_INDEX_BYTES,
                    None,
                )?
                .ok_or(StoreError::MalformedHistoryIndex)?;
                if ContentDigest::of(&bytes) != digest {
                    return Err(StoreError::HistoryIndexPathMismatch(digest));
                }
                let root: DurableEngineHistoryRoot =
                    postcard::from_bytes(&bytes).map_err(|_| StoreError::MalformedHistoryIndex)?;
                if postcard::to_allocvec(&root).map_err(|_| StoreError::MalformedHistoryIndex)?
                    != bytes
                {
                    return Err(StoreError::MalformedHistoryIndex);
                }
                validate_engine_history_root(
                    &root,
                    self.workspace_id,
                    binding.endpoint.endpoint_id,
                    binding.endpoint.graph_resource_id,
                    binding.receipt_store_id,
                )
            }
            _ => Err(StoreError::MalformedHistoryIndex),
        }
    }

    /// Enumerate all manifest commit markers in deterministic BatchId order.
    /// Staged manifests are included; readiness is determined by `inspect_batch`.
    pub fn committed_manifests(&self) -> Result<Vec<OperationBatch>, StoreError> {
        self.counters
            .directory_enumerations
            .fetch_add(1, Ordering::Relaxed);
        let batches = self.open_namespace(BATCHES_DIR)?;
        let mut manifests = Vec::new();
        for entry in batches.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| StoreError::MalformedPath("non-UTF-8 batch entry".into()))?;
            if is_temp_name(name) {
                require_regular_entry(&entry.file_type()?, name)?;
                continue;
            }
            require_regular_entry(&entry.file_type()?, name)?;
            let batch_id = parse_manifest_filename(name)?;
            let bytes = read_required_regular(&batches, name, MAX_MANIFEST_BYTES as u64, None)?;
            let manifest = OperationBatch::decode(&bytes)?;
            if manifest.batch_id() != batch_id {
                return Err(StoreError::ManifestPathMismatch {
                    expected: batch_id,
                    found: manifest.batch_id(),
                });
            }
            if manifest.workspace_id() != self.workspace_id {
                return Err(StoreError::WorkspaceMismatch {
                    expected: self.workspace_id,
                    found: manifest.workspace_id(),
                });
            }
            manifests.push(manifest);
        }
        manifests.sort_unstable_by_key(OperationBatch::batch_id);
        if let Some(first) = manifests.first() {
            for manifest in &manifests[1..] {
                if manifest.lineage_digest() != first.lineage_digest() {
                    return Err(StoreError::LineageMismatch {
                        expected: first.lineage_digest(),
                        found: manifest.lineage_digest(),
                    });
                }
            }
        }
        Ok(manifests)
    }

    /// Begin an incremental manifest enumeration without opening any manifest
    /// or object bytes.
    pub(crate) fn manifest_cursor(&self) -> Result<ObjectStoreManifestCursor, StoreError> {
        self.counters
            .directory_enumerations
            .fetch_add(1, Ordering::Relaxed);
        Ok(ObjectStoreManifestCursor {
            entries: self.open_namespace(BATCHES_DIR)?.entries()?,
        })
    }

    /// Visit at most one immutable manifest from a retained cursor.
    ///
    /// Temporary publication names are validated and skipped. A returned
    /// manifest is decoded and workspace-bound, but its objects are not opened.
    pub(crate) fn next_manifest(
        &self,
        cursor: &mut ObjectStoreManifestCursor,
    ) -> Result<Option<OperationBatch>, StoreError> {
        loop {
            let Some(entry) = cursor.entries.next() else {
                return Ok(None);
            };
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| StoreError::MalformedPath("non-UTF-8 batch entry".into()))?;
            require_regular_entry(&entry.file_type()?, name)?;
            if is_temp_name(name) {
                continue;
            }
            let batch_id = parse_manifest_filename(name)?;
            let bytes = self.read_manifest_bytes(batch_id)?;
            let manifest = OperationBatch::decode(&bytes)?;
            if manifest.batch_id() != batch_id {
                return Err(StoreError::ManifestPathMismatch {
                    expected: batch_id,
                    found: manifest.batch_id(),
                });
            }
            return Ok(Some(manifest));
        }
    }

    pub(crate) fn read_manifest_bytes(&self, batch_id: BatchId) -> Result<Vec<u8>, StoreError> {
        let batches = self.open_namespace(BATCHES_DIR)?;
        let bytes = read_required_regular(
            &batches,
            &manifest_filename(batch_id),
            MAX_MANIFEST_BYTES as u64,
            None,
        )?;
        let manifest = OperationBatch::decode(&bytes)?;
        if manifest.batch_id() != batch_id {
            return Err(StoreError::ManifestPathMismatch {
                expected: batch_id,
                found: manifest.batch_id(),
            });
        }
        if manifest.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: manifest.workspace_id(),
            });
        }
        Ok(bytes)
    }

    pub fn contains_object(&self, digest: ContentDigest) -> Result<bool, StoreError> {
        let objects = self.open_namespace(OBJECTS_DIR)?;
        let Some(bytes) = read_optional_regular(
            &objects,
            &object_filename(digest),
            MAX_OBJECT_BYTES as u64,
            None,
        )?
        else {
            return Ok(false);
        };
        if ContentDigest::of(&bytes) != digest {
            return Err(StoreError::ObjectPathMismatch(digest));
        }
        let object = OperationObject::decode(&bytes)?;
        if object.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: object.workspace_id(),
            });
        }
        Ok(true)
    }

    /// Read and validate only a batch's manifest, without touching its objects.
    ///
    /// The manifest's descriptors already carry `document_id`, `kind`,
    /// `content_digest` and `encoded_byte_length` for every object, so a caller
    /// that needs object *metadata* -- which documents a batch updates, how many
    /// bytes it retains, which object holds a given kind -- never needs to read
    /// the objects themselves. Pair this with [`Self::read_object`] to fetch the
    /// one payload that is genuinely required.
    pub(crate) fn read_manifest(
        &self,
        batch_id: BatchId,
    ) -> Result<Option<OperationBatch>, StoreError> {
        let batches = self.open_namespace(BATCHES_DIR)?;
        let filename = manifest_filename(batch_id);
        let Some(manifest_bytes) =
            read_optional_regular(&batches, &filename, MAX_MANIFEST_BYTES as u64, None)?
        else {
            return Ok(None);
        };
        self.counters
            .inspected_manifest_operations
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .inspected_manifest_bytes
            .fetch_add(manifest_bytes.len(), Ordering::Relaxed);
        let manifest = OperationBatch::decode(&manifest_bytes)?;
        if manifest.batch_id() != batch_id {
            return Err(StoreError::ManifestPathMismatch {
                expected: batch_id,
                found: manifest.batch_id(),
            });
        }
        if manifest.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: manifest.workspace_id(),
            });
        }
        Ok(Some(manifest))
    }

    /// Read exactly the one content-addressed object a caller already has the
    /// digest for.
    ///
    /// This is the access path [`Self::inspect_batch`] is *not*: inspecting a
    /// batch to obtain a single object costs O(whole batch) — it reads,
    /// SHA-256s and decodes every object the manifest requires. Callers that
    /// hold a digest are asking for a file whose name already *is* that digest,
    /// so there is nothing to search and nothing to re-prove about the rest of
    /// the batch.
    ///
    /// The object's own integrity is still checked here (digest + workspace),
    /// because that is O(one object) and keeps the content-addressing contract.
    /// What is dropped is re-proving *batch completeness* per object, which is
    /// established once at acceptance: `hot_engine.rs:13120-13127` admits a
    /// batch to the archive only on `BatchInspection::Ready`, and projection
    /// work rows reach `Ready` only inside `accept_batch_at_history`.
    pub(crate) fn read_object(&self, digest: ContentDigest) -> Result<OperationObject, StoreError> {
        let objects = self.open_namespace(OBJECTS_DIR)?;
        // Counted on the same counters as `inspect_batch`'s per-object reads.
        // These measure *object reads*, not one particular access path, and
        // tests use them as an oracle for how much an operation reconstructs
        // (`ordinary_drain_reconstructs_each_accepted_event_once`). A new path
        // that read objects silently would make that oracle lie.
        self.counters
            .inspected_object_operations
            .fetch_add(1, Ordering::Relaxed);
        let bytes = read_required_regular(
            &objects,
            &object_filename(digest),
            MAX_OBJECT_BYTES as u64,
            None,
        )?;
        self.counters
            .inspected_object_bytes
            .fetch_add(bytes.len(), Ordering::Relaxed);
        if ContentDigest::of(&bytes) != digest {
            return Err(StoreError::ObjectPathMismatch(digest));
        }
        let object = OperationObject::decode(&bytes)?;
        if object.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: object.workspace_id(),
            });
        }
        Ok(object)
    }

    pub(crate) fn read_object_bytes(&self, digest: ContentDigest) -> Result<Vec<u8>, StoreError> {
        let objects = self.open_namespace(OBJECTS_DIR)?;
        let bytes = read_required_regular(
            &objects,
            &object_filename(digest),
            MAX_OBJECT_BYTES as u64,
            None,
        )?;
        if ContentDigest::of(&bytes) != digest {
            return Err(StoreError::ObjectPathMismatch(digest));
        }
        let object = OperationObject::decode(&bytes)?;
        if object.workspace_id() != self.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: object.workspace_id(),
            });
        }
        Ok(bytes)
    }

    /// Repair only content-addressed object names whose canonical bytes remain
    /// authoritative in an undrained managed-local journal record.
    ///
    /// The caller holds the archive's sole-writer workspace lease. Each
    /// replacement is still bound to the exact bad bytes observed here, staged
    /// and flushed before an atomic same-directory replacement, followed by a
    /// directory barrier. Uncovered, ambiguous, malformed, or foreign objects
    /// are deliberately left for `validate_namespace` to refuse unchanged.
    pub(crate) fn repair_covered_object_mismatches(
        &self,
        covered: &BTreeMap<ContentDigest, Vec<u8>>,
    ) -> Result<usize, StoreError> {
        if covered.is_empty() {
            return Ok(0);
        }
        let objects = self.open_namespace(OBJECTS_DIR)?;
        let mut repaired = 0_usize;
        for (expected, replacement) in covered {
            let name = object_filename(*expected);
            let Some(observed) =
                read_optional_regular(&objects, &name, MAX_OBJECT_BYTES as u64, None)?
            else {
                continue;
            };
            if ContentDigest::of(&observed) == *expected {
                continue;
            }
            if ContentDigest::of(replacement) != *expected {
                return Err(StoreError::ObjectPathMismatch(*expected));
            }
            let object = OperationObject::decode(replacement)?;
            if object.workspace_id() != self.workspace_id {
                return Err(StoreError::WorkspaceMismatch {
                    expected: self.workspace_id,
                    found: object.workspace_id(),
                });
            }
            if object.encode()?.as_slice() != replacement {
                return Err(StoreError::ObjectPathMismatch(*expected));
            }
            tine_storage::DurableDirectoryPublication::open(&objects)
                .map_err(|error| publication_error(error, Collision::Object(*expected)))?
                .replace_exact(&name, &observed, replacement)
                .map_err(|error| publication_error(error, Collision::Object(*expected)))?;
            let installed = read_required_regular(
                &objects,
                &name,
                MAX_OBJECT_BYTES as u64,
                Some(replacement.len() as u64),
            )?;
            if installed != *replacement {
                return Err(StoreError::ObjectPathMismatch(*expected));
            }
            repaired = repaired.saturating_add(1);
        }
        Ok(repaired)
    }

    pub(crate) fn validate_namespace(&self) -> Result<(), StoreError> {
        let mut manifests = Vec::new();
        for (directory, kind) in [
            (OBJECTS_DIR, NamespaceKind::Objects),
            (BATCHES_DIR, NamespaceKind::Batches),
        ] {
            self.counters
                .directory_enumerations
                .fetch_add(1, Ordering::Relaxed);
            let dir = self.open_namespace(directory)?;
            for entry in dir.entries()? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    StoreError::MalformedPath(format!("non-UTF-8 entry under {directory}"))
                })?;
                require_regular_entry(&entry.file_type()?, name)?;
                if is_temp_name(name) {
                    let limit = match kind {
                        NamespaceKind::Objects => MAX_OBJECT_BYTES as u64,
                        NamespaceKind::Batches => MAX_MANIFEST_BYTES as u64,
                    };
                    read_required_regular(&dir, name, limit, None)?;
                    continue;
                }
                match kind {
                    NamespaceKind::Objects => {
                        let expected = parse_object_filename(name)?;
                        let bytes =
                            read_required_regular(&dir, name, MAX_OBJECT_BYTES as u64, None)?;
                        if ContentDigest::of(&bytes) != expected {
                            return Err(StoreError::ObjectPathMismatch(expected));
                        }
                        let object = OperationObject::decode(&bytes)?;
                        if object.workspace_id() != self.workspace_id {
                            return Err(StoreError::WorkspaceMismatch {
                                expected: self.workspace_id,
                                found: object.workspace_id(),
                            });
                        }
                        if object.encode()?.as_slice() != bytes {
                            return Err(StoreError::ObjectPathMismatch(expected));
                        }
                    }
                    NamespaceKind::Batches => {
                        let expected = parse_manifest_filename(name)?;
                        let bytes =
                            read_required_regular(&dir, name, MAX_MANIFEST_BYTES as u64, None)?;
                        let manifest = OperationBatch::decode(&bytes)?;
                        if manifest.batch_id() != expected {
                            return Err(StoreError::ManifestPathMismatch {
                                expected,
                                found: manifest.batch_id(),
                            });
                        }
                        if manifest.workspace_id() != self.workspace_id {
                            return Err(StoreError::WorkspaceMismatch {
                                expected: self.workspace_id,
                                found: manifest.workspace_id(),
                            });
                        }
                        manifests.push(manifest);
                    }
                }
            }
        }
        ensure_single_lineage(&manifests)?;
        if let Some(first) = manifests.first() {
            self.check_or_establish_lineage(first.lineage_digest())?;
        } else {
            let _ = read_optional_regular(&self.capability, LINEAGE_CLAIM_FILE, 32, Some(32))?;
        }
        Ok(())
    }

    fn check_or_establish_lineage(&self, lineage: LineageDigest) -> Result<(), StoreError> {
        if let Some(bytes) =
            read_optional_regular(&self.capability, LINEAGE_CLAIM_FILE, 32, Some(32))?
        {
            return require_lineage_bytes(lineage, &bytes);
        }
        match publish_immutable(
            &self.capability,
            LINEAGE_CLAIM_FILE,
            lineage.as_bytes(),
            Collision::Lineage(lineage),
        ) {
            Ok(()) => Ok(()),
            Err(StoreError::LineageClaimCollision(_)) => {
                let bytes =
                    read_required_regular(&self.capability, LINEAGE_CLAIM_FILE, 32, Some(32))?;
                require_lineage_bytes(lineage, &bytes)
            }
            Err(error) => Err(error),
        }
    }

    fn require_lineage(&self, lineage: LineageDigest) -> Result<(), StoreError> {
        let bytes = read_required_regular(&self.capability, LINEAGE_CLAIM_FILE, 32, Some(32))?;
        require_lineage_bytes(lineage, &bytes)
    }

    fn open_namespace(&self, name: &str) -> Result<Dir, StoreError> {
        let metadata = self.capability.symlink_metadata(name)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::UnsafeEntry(format!(
                "{name} is not a real no-follow directory"
            )));
        }
        open_dir_nofollow(&self.capability, name)
    }
}

#[cfg(test)]
impl HistoryOnlyOpen {
    #[cfg(test)]
    pub(crate) const fn binding(&self) -> super::hot_engine::ProjectionStorageBinding {
        self.binding
    }

    #[cfg(test)]
    pub(crate) fn into_history(
        mut self,
    ) -> Result<(ObjectStore, DurableEngineHistoryStore), (ObjectStore, StoreError)> {
        enrolled_open_use_hook();
        if let SealedControl::Existing(history) = self
            .history
            .as_ref()
            .expect("sealed history control is present")
        {
            if let Err(error) = history.validate_sealed_open() {
                return Err((self.store.take().expect("sealed store is present"), error));
            }
        }
        enrolled_open_act_hook();

        let store = self.store.take().expect("sealed store is present");
        let validation = match self
            .history
            .as_ref()
            .expect("sealed history control is present")
        {
            SealedControl::Existing(history) => history.validate_sealed_open(),
            SealedControl::Absent(absence) => absence.validate_still_absent(&store.capability),
        };
        if let Err(error) = validation {
            return Err((store, error));
        }
        let history = match self
            .history
            .take()
            .expect("sealed history control is present")
        {
            SealedControl::Existing(history) => history,
            SealedControl::Absent(absence) => {
                match store.open_absent_engine_history(absence, self.binding) {
                    Ok(history) => history,
                    Err(error) => return Err((store, error)),
                }
            }
        };
        Ok((store, history))
    }
}

impl<T> SealedControl<T> {
    fn bind_absent_parent(&mut self, store_root: &Dir) -> Result<bool, StoreError> {
        let Self::Absent(absence) = self else {
            return Ok(false);
        };
        if absence.namespace.is_some() {
            return Ok(false);
        }
        store_root
            .create_dir(absence.namespace_name)
            .map_err(|error| {
                if error.kind() == ErrorKind::AlreadyExists {
                    StoreError::UnsafeEntry(format!(
                        "formerly absent {} was created while enrolled open was sealed",
                        absence.namespace_name
                    ))
                } else {
                    error.into()
                }
            })?;
        sync_dir_required(store_root)?;
        let namespace = open_dir_nofollow(store_root, absence.namespace_name)?;
        absence.namespace_identity = Some(control_directory_identity(&namespace)?);
        absence.namespace = Some(namespace);
        Ok(true)
    }
}

impl AbsentControlName {
    fn validate_still_absent(&self, store_root: &Dir) -> Result<(), StoreError> {
        let parent = match &self.namespace {
            Some(namespace) => {
                let live = open_existing_dir_nofollow(store_root, self.namespace_name)?
                    .ok_or_else(|| {
                        StoreError::UnsafeEntry(format!(
                            "enrolled-open parent {} disappeared",
                            self.namespace_name
                        ))
                    })?;
                let expected = self.namespace_identity.ok_or_else(|| {
                    StoreError::UnsafeEntry(format!(
                        "enrolled-open parent {} has no sealed identity",
                        self.namespace_name
                    ))
                })?;
                if control_directory_identity(&live)? != expected
                    || control_directory_identity(namespace)? != expected
                {
                    return Err(StoreError::UnsafeEntry(format!(
                        "enrolled-open parent {} was substituted",
                        self.namespace_name
                    )));
                }
                namespace
            }
            None => {
                return match store_root.symlink_metadata(self.namespace_name) {
                    Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                    Ok(_) => Err(StoreError::UnsafeEntry(format!(
                        "formerly absent {} was created before enrolled open consumed it",
                        self.namespace_name
                    ))),
                    Err(error) => Err(error.into()),
                };
            }
        };
        match parent.symlink_metadata(&self.endpoint_name) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(StoreError::UnsafeEntry(format!(
                "formerly absent {}/{} was created before enrolled open consumed it",
                self.namespace_name, self.endpoint_name
            ))),
            Err(error) => Err(error.into()),
        }
    }

    fn claim(self, store_root: &Dir) -> Result<Dir, StoreError> {
        self.validate_still_absent(store_root)?;
        let namespace = match self.namespace {
            Some(namespace) => namespace,
            None => {
                store_root
                    .create_dir(self.namespace_name)
                    .map_err(|error| {
                        if error.kind() == ErrorKind::AlreadyExists {
                            StoreError::UnsafeEntry(format!(
                                "formerly absent {} was created before enrolled open consumed it",
                                self.namespace_name
                            ))
                        } else {
                            error.into()
                        }
                    })?;
                sync_dir_required(store_root)?;
                open_dir_nofollow(store_root, self.namespace_name)?
            }
        };
        namespace.create_dir(&self.endpoint_name).map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                StoreError::UnsafeEntry(format!(
                    "formerly absent {}/{} was created before enrolled open consumed it",
                    self.namespace_name, self.endpoint_name
                ))
            } else {
                error.into()
            }
        })?;
        sync_dir_required(&namespace)?;
        open_dir_nofollow(&namespace, &self.endpoint_name)
    }
}

impl StoreCounters {
    fn snapshot(&self) -> ObjectStoreStats {
        ObjectStoreStats {
            directory_enumerations: self.directory_enumerations.load(Ordering::Relaxed),
            accepted_manifest_reads: self.accepted_manifest_reads.load(Ordering::Relaxed),
            accepted_object_reads: self.accepted_object_reads.load(Ordering::Relaxed),
            dag_manifest_reads: self.dag_manifest_reads.load(Ordering::Relaxed),
            history_record_reads: self.history_record_reads.load(Ordering::Relaxed),
            history_index_reads: self.history_index_reads.load(Ordering::Relaxed),
            history_index_writes: self.history_index_writes.load(Ordering::Relaxed),
            history_decodes: self.history_decodes.load(Ordering::Relaxed),
            block_claim_index_reads: self.block_claim_index_reads.load(Ordering::Relaxed),
            block_claim_index_writes: self.block_claim_index_writes.load(Ordering::Relaxed),
            block_claim_index_syncs: self.block_claim_index_syncs.load(Ordering::Relaxed),
            inspected_manifest_operations: self
                .inspected_manifest_operations
                .load(Ordering::Relaxed),
            inspected_manifest_bytes: self.inspected_manifest_bytes.load(Ordering::Relaxed),
            inspected_object_operations: self.inspected_object_operations.load(Ordering::Relaxed),
            inspected_object_bytes: self.inspected_object_bytes.load(Ordering::Relaxed),
        }
    }
}

impl EngineHistoryStore {
    pub(crate) fn empty_root() -> ContentDigest {
        ContentDigest::of(b"tine/oplog-engine-history/radix-v1/empty")
    }

    pub(crate) fn lookup(
        &self,
        root: ContentDigest,
        batch_id: BatchId,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if root == Self::empty_root() {
            return Ok(None);
        }
        let batch_uuid = batch_id.as_uuid();
        let key = batch_uuid.as_bytes();
        let mut digest = root;
        for depth in 0..=ENGINE_HISTORY_RADIX_DEPTH {
            match self.read_node(digest)? {
                HistoryIndexNode::Branch {
                    depth: found_depth,
                    children,
                    ..
                } => {
                    if depth >= ENGINE_HISTORY_RADIX_DEPTH || found_depth != depth {
                        return Err(StoreError::MalformedHistoryIndex);
                    }
                    let nibble = history_key_nibble(key, depth);
                    let Some((_, child)) =
                        children.iter().find(|(candidate, _)| *candidate == nibble)
                    else {
                        return Ok(None);
                    };
                    digest = *child;
                }
                HistoryIndexNode::Leaf {
                    batch_id: found,
                    record,
                    ..
                } => {
                    if depth != ENGINE_HISTORY_RADIX_DEPTH || found != batch_id {
                        return Err(StoreError::MalformedHistoryIndex);
                    }
                    return Ok(Some(record));
                }
            }
        }
        Err(StoreError::MalformedHistoryIndex)
    }

    pub(crate) fn insert(
        &self,
        root: ContentDigest,
        batch_id: BatchId,
        bytes: &[u8],
    ) -> Result<ContentDigest, StoreError> {
        if bytes.is_empty() || bytes.len() as u64 > MAX_ENGINE_HISTORY_RECORD_BYTES {
            return Err(StoreError::StoredFileTooLarge {
                path: history_filename(batch_id),
                length: bytes.len() as u64,
                limit: MAX_ENGINE_HISTORY_RECORD_BYTES,
            });
        }
        self.insert_at(root, batch_id, bytes, 0)
    }

    pub(crate) fn materialize(
        &self,
        root: ContentDigest,
    ) -> Result<Vec<(BatchId, Vec<u8>)>, StoreError> {
        if root == Self::empty_root() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        let mut pending = vec![(root, 0_u8)];
        while let Some((digest, expected_depth)) = pending.pop() {
            match self.read_node(digest)? {
                HistoryIndexNode::Branch {
                    depth, children, ..
                } => {
                    if depth != expected_depth || depth >= ENGINE_HISTORY_RADIX_DEPTH {
                        return Err(StoreError::MalformedHistoryIndex);
                    }
                    pending.extend(
                        children
                            .into_iter()
                            .rev()
                            .map(|(_, child)| (child, depth + 1)),
                    );
                }
                HistoryIndexNode::Leaf {
                    batch_id, record, ..
                } => {
                    if expected_depth != ENGINE_HISTORY_RADIX_DEPTH {
                        return Err(StoreError::MalformedHistoryIndex);
                    }
                    records.push((batch_id, record));
                }
            }
        }
        records.sort_unstable_by_key(|(batch_id, _)| *batch_id);
        Ok(records)
    }

    pub(crate) fn note_history_decode(&self) {
        self.counters
            .history_decodes
            .fetch_add(1, Ordering::Relaxed);
    }

    fn insert_at(
        &self,
        root: ContentDigest,
        batch_id: BatchId,
        record: &[u8],
        depth: u8,
    ) -> Result<ContentDigest, StoreError> {
        if depth == ENGINE_HISTORY_RADIX_DEPTH {
            if root != Self::empty_root() {
                match self.read_node(root)? {
                    HistoryIndexNode::Leaf {
                        batch_id: existing_batch,
                        record: existing_record,
                        ..
                    } if existing_batch == batch_id && existing_record == record => {
                        return Ok(root);
                    }
                    _ => return Err(StoreError::HistoryIndexCollision(batch_id)),
                }
            }
            return self.publish_node(&HistoryIndexNode::Leaf {
                schema_version: ENGINE_HISTORY_INDEX_SCHEMA_VERSION,
                batch_id,
                record: record.to_vec(),
            });
        }

        let mut children = if root == Self::empty_root() {
            Vec::new()
        } else {
            match self.read_node(root)? {
                HistoryIndexNode::Branch {
                    depth: found_depth,
                    children,
                    ..
                } if found_depth == depth => children,
                _ => return Err(StoreError::MalformedHistoryIndex),
            }
        };
        let nibble = history_key_nibble(batch_id.as_uuid().as_bytes(), depth);
        let existing_child = children
            .iter()
            .find(|(candidate, _)| *candidate == nibble)
            .map(|(_, digest)| *digest)
            .unwrap_or_else(Self::empty_root);
        let child = self.insert_at(existing_child, batch_id, record, depth + 1)?;
        match children.binary_search_by_key(&nibble, |(candidate, _)| *candidate) {
            Ok(index) => children[index].1 = child,
            Err(index) => children.insert(index, (nibble, child)),
        }
        self.publish_node(&HistoryIndexNode::Branch {
            schema_version: ENGINE_HISTORY_INDEX_SCHEMA_VERSION,
            depth,
            children,
        })
    }

    /// Latch [`Self::storage_fault`]. Monotone, so a plain store is enough and
    /// the observation can never be lost by racing with another latch.
    fn note_storage_fault(&self) {
        self.storage_fault.store(true, Ordering::SeqCst);
    }

    fn storage_faulted(&self) -> bool {
        self.storage_fault.load(Ordering::SeqCst)
    }

    fn publish_node(&self, node: &HistoryIndexNode) -> Result<ContentDigest, StoreError> {
        self.publish_node_checked(node)
            .inspect_err(|_| self.note_storage_fault())
    }

    fn publish_node_checked(&self, node: &HistoryIndexNode) -> Result<ContentDigest, StoreError> {
        validate_history_node(node)?;
        let bytes = postcard::to_allocvec(node).map_err(|_| StoreError::MalformedHistoryIndex)?;
        if bytes.len() as u64 > MAX_ENGINE_HISTORY_INDEX_BYTES {
            return Err(StoreError::StoredFileTooLarge {
                path: "engine history index node".into(),
                length: bytes.len() as u64,
                limit: MAX_ENGINE_HISTORY_INDEX_BYTES,
            });
        }
        let digest = ContentDigest::of(&bytes);
        self.counters
            .history_index_writes
            .fetch_add(1, Ordering::Relaxed);
        publish_immutable(
            &self.capability,
            &history_index_filename(digest),
            &bytes,
            Collision::HistoryIndex(digest),
        )?;
        Ok(digest)
    }

    /// Read one immutable index node.
    ///
    /// Every failure here is a durable storage fault — the node is missing,
    /// oversized, stored under the wrong content address, undecodable,
    /// non-canonical or structurally invalid — so every failure latches
    /// [`Self::storage_fault`]. Structural *lineage* rejections are decided by
    /// the callers from successfully read nodes and never reach this latch.
    fn read_node(&self, digest: ContentDigest) -> Result<HistoryIndexNode, StoreError> {
        self.read_node_checked(digest)
            .inspect_err(|_| self.note_storage_fault())
    }

    fn read_node_checked(&self, digest: ContentDigest) -> Result<HistoryIndexNode, StoreError> {
        self.counters
            .history_index_reads
            .fetch_add(1, Ordering::Relaxed);
        let bytes = read_required_regular(
            &self.capability,
            &history_index_filename(digest),
            MAX_ENGINE_HISTORY_INDEX_BYTES,
            None,
        )?;
        if ContentDigest::of(&bytes) != digest {
            return Err(StoreError::HistoryIndexPathMismatch(digest));
        }
        let node: HistoryIndexNode =
            postcard::from_bytes(&bytes).map_err(|_| StoreError::MalformedHistoryIndex)?;
        validate_history_node(&node)?;
        if postcard::to_allocvec(&node).map_err(|_| StoreError::MalformedHistoryIndex)? != bytes {
            return Err(StoreError::MalformedHistoryIndex);
        }
        if matches!(node, HistoryIndexNode::Leaf { .. }) {
            self.counters
                .history_record_reads
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(node)
    }
}

/// The authenticated endpoint facts one resume-point publication is sealed
/// against.
///
/// Every field is private to this module, so the only route to a value is
/// [`DurableEngineHistoryStore::resume_point_endpoint_binding`]. That method
/// reads this endpoint's *durable* promoted runtime state through
/// [`DurableEngineHistoryStore::read_promoted_runtime_state`] — itself gated by
/// `require_promoted_state_binding`, which proves the state names this
/// workspace, this endpoint, this graph resource, this receipt store and this
/// exact physical archive directory — and derives the next sequence from an
/// actual survey rather than from a caller's belief.
///
/// This is the compile-time half of "the lifecycle caller cannot omit facts":
/// `RuntimeResumePointV2::seal` needs one of these, and nothing outside this
/// module can build one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResumePointEndpointBinding {
    workspace_id: WorkspaceId,
    endpoint_id: super::ProjectionEndpointId,
    promoted_state_digest: ContentDigest,
    next_sequence: u64,
}

impl ResumePointEndpointBinding {
    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn endpoint_id(&self) -> super::ProjectionEndpointId {
        self.endpoint_id
    }

    pub(crate) const fn promoted_state_digest(&self) -> ContentDigest {
        self.promoted_state_digest
    }

    pub(crate) const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Hand-built binding for format-level tests that have no live endpoint.
    #[cfg(test)]
    pub(crate) const fn for_test(
        workspace_id: WorkspaceId,
        endpoint_id: super::ProjectionEndpointId,
        promoted_state_digest: ContentDigest,
        next_sequence: u64,
    ) -> Self {
        Self {
            workspace_id,
            endpoint_id,
            promoted_state_digest,
            next_sequence,
        }
    }
}

/// The live-open authority a published point must re-prove before it may be
/// offered to the engine as an adoption candidate.
///
/// Sealed for the same reason as [`ResumePointEndpointBinding`]: the digest, the
/// endpoint and the durable head all come from this store's own reads, so a
/// caller cannot weaken the comparison by supplying values it wishes were true.
/// The one caller-supplied member is the enrollment admission, because nothing
/// here can read the enrollment chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResumeAdoptionAuthority {
    workspace_id: WorkspaceId,
    endpoint_id: super::ProjectionEndpointId,
    promoted_state_digest: ContentDigest,
    history_generation: u64,
    history_index_root: ContentDigest,
    history_latest_batch_id: Option<BatchId>,
    enrollment: ResumeEnrollmentAdmission,
}

impl ResumeAdoptionAuthority {
    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn endpoint_id(&self) -> super::ProjectionEndpointId {
        self.endpoint_id
    }

    pub(crate) const fn promoted_state_digest(&self) -> ContentDigest {
        self.promoted_state_digest
    }

    pub(crate) const fn history_generation(&self) -> u64 {
        self.history_generation
    }

    pub(crate) const fn history_index_root(&self) -> ContentDigest {
        self.history_index_root
    }

    pub(crate) const fn history_latest_batch_id(&self) -> Option<BatchId> {
        self.history_latest_batch_id
    }

    pub(crate) const fn enrollment(&self) -> ResumeEnrollmentAdmission {
        self.enrollment
    }

    /// Hand-built authority for format-level tests that have no live endpoint.
    #[cfg(test)]
    pub(crate) const fn for_test(
        workspace_id: WorkspaceId,
        endpoint_id: super::ProjectionEndpointId,
        promoted_state_digest: ContentDigest,
        history: (u64, ContentDigest, Option<BatchId>),
        enrollment: ResumeEnrollmentAdmission,
    ) -> Self {
        Self {
            workspace_id,
            endpoint_id,
            promoted_state_digest,
            history_generation: history.0,
            history_index_root: history.1,
            history_latest_batch_id: history.2,
            enrollment,
        }
    }
}

/// Proof that one exact replacement resume point reached durability.
///
/// Minted only by [`DurableEngineHistoryStore::publish_resume_point`], on its
/// success path. Retained-run reclamation consumes one, which is how "reclaim
/// only *after* a successful replacement publication" becomes a fact the type
/// system carries instead of a comment a later caller can reorder past: until
/// the replacement point is durable, the run a predecessor point names may still
/// hold the only resumable bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PublishedResumePoint {
    workspace_id: WorkspaceId,
    resume_sequence: u64,
    scratch_run_id: Uuid,
}

impl PublishedResumePoint {
    pub(crate) const fn resume_sequence(&self) -> u64 {
        self.resume_sequence
    }

    pub(crate) const fn scratch_run_id(&self) -> Uuid {
        self.scratch_run_id
    }
}

/// The adoption input one resuming open consumes.
///
/// `Unavailable` is never an error the caller has to recover from: it means
/// "reuse nothing, replay everything", which is always available and always
/// correct. It is carried as a value rather than an `Err` precisely so that a
/// caller cannot accidentally propagate it into a startup failure with `?`.
#[derive(Debug)]
pub(crate) enum ResumeAdoptionCandidate {
    /// Hand this to `ShardedHotEngine::open_enrolled_projection_resuming`. The
    /// engine still re-proves the run, the durable descent and every run-local
    /// root before it reuses a single byte.
    Available(Box<RuntimeResumeSnapshot>),
    Unavailable(ResumeAcceleratorUnavailable),
}

/// Why this open gets no accelerator. Diagnosable, never actionable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResumeAcceleratorUnavailable {
    /// No point has ever been published for this endpoint.
    NeverPublished,
    /// The strict complete-set proof was denied: unrecognizable provider or
    /// desktop residue, a surplus over the publication bound, a torn, renamed
    /// or oversize point, or an entry that could not be classified at all.
    /// Nothing was proved about reachability, so nothing is reclaimed either.
    ProofDenied(ResumePointError),
    /// The latest point decoded and validated but did not re-prove the live
    /// open's authority.
    BindingRefused(ResumePointError),
    /// The store could not be read, or the published set does not bind this
    /// endpoint at all.
    Unavailable(String),
}

/// What retention the next engine scratch run may use.
///
/// The `Ephemeral` arm is the leak bound. Once retention is flipped on, a
/// resuming open mints one retained run per restart, and the only pass that can
/// collect one needs the strict resume-point proof. A directory holding one
/// permanent `.sync-conflict-*` copy denies that proof *forever*, so without
/// this decision every restart would leak one archive directory, silently.
/// Choosing an ephemeral run costs exactly one full replay — always available,
/// always correct — and converts an unbounded disk leak into a bounded loss of
/// an accelerator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EngineScratchRetentionPlan {
    /// A retained run may be minted or adopted: either reachability is
    /// provable, so an unreachable predecessor can be collected later, or the
    /// census is still inside [`MAX_RETAINED_SCRATCH_RUNS`].
    Retained { retained_runs: usize },
    /// Reachability cannot be proved *and* the census is already at its bound.
    Ephemeral {
        retained_runs: usize,
        reason: ResumePointError,
    },
}

/// What one bounded retained-run maintenance pass proved, reclaimed and
/// preserved.
///
/// Maintenance is diagnosable but never a correctness or startup failure, so
/// this type has no `Err` sibling: every failure mode is a variant of
/// [`RetainedRunMaintenanceOutcome`] carried alongside the counts that are
/// still known.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetainedRunMaintenanceReport {
    /// Retained runs whose bytes this pass removed. The only member that
    /// describes deletion.
    pub(crate) reclaimed: usize,
    /// Authenticated retained runs of this workspace still on disk afterwards.
    pub(crate) retained_runs_remaining: usize,
    pub(crate) within_retained_run_bound: bool,
    /// Scratch siblings that could not be authenticated or classified —
    /// including a replicated conflict copy of a run directory. Preserved
    /// untouched, forever, by design.
    pub(crate) unclassified_preserved: usize,
    /// Resume-point directory entries this pass refused to interpret or
    /// remove. Non-empty means the strict proof is denied and retained runs are
    /// leaking, which is the only place that becomes visible.
    pub(crate) preserved_resume_residue: Vec<String>,
    pub(crate) outcome: RetainedRunMaintenanceOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RetainedRunMaintenanceOutcome {
    /// A complete strict proof authorized the pass and it ran.
    Reclaimed,
    /// The strict proof was denied. Every retained run was preserved.
    ProofDenied(ResumePointError),
    /// The pass could not run at all.
    Unavailable(String),
}

impl DurableEngineHistoryStore {
    fn open_sealed_existing(
        workspace_id: WorkspaceId,
        endpoint_id: super::ProjectionEndpointId,
        graph_resource_id: super::CanonicalGraphResourceId,
        receipt_store_id: super::ProjectionReceiptStoreId,
        control: Dir,
        archive_root: Dir,
        transition_lock: fs::File,
        counters: Arc<StoreCounters>,
    ) -> Result<Self, StoreError> {
        // Retain a duplicate handle for the returned store while the guard
        // borrows the original. This keeps one uninterrupted advisory lock
        // across every post-open claim/head/root check and construction, then
        // releases it before callers can invoke a transition method on the
        // returned store and acquire the same lock themselves.
        let retained_transition_lock = transition_lock.try_clone()?;
        let _workspace_guard = AdvisoryTransitionGuard::lock(&transition_lock)?;
        #[cfg(test)]
        sealed_history_authority_window_hook(SealedHistoryAuthorityWindowStage::Locked);
        let claim = read_optional_regular(&control, ENGINE_HISTORY_CLAIM_FILE, 256, None)?
            .ok_or(StoreError::MalformedHistoryIndex)?;
        validate_engine_history_claim(
            &claim,
            workspace_id,
            endpoint_id,
            graph_resource_id,
            receipt_store_id,
        )?;
        let roots = open_existing_dir_nofollow(&control, ENGINE_HISTORY_ROOTS_DIR)?
            .ok_or(StoreError::MalformedHistoryIndex)?;
        let nodes = open_existing_dir_nofollow(&control, ENGINE_HISTORY_NODES_DIR)?
            .ok_or(StoreError::MalformedHistoryIndex)?;
        let store = Self {
            workspace_id,
            endpoint_id,
            graph_resource_id,
            receipt_store_id,
            control,
            archive_root,
            roots,
            index: EngineHistoryStore {
                capability: nodes,
                counters,
                storage_fault: AtomicBool::new(false),
            },
            transition_lock: retained_transition_lock,
            transition: Mutex::new(()),
            authoritative_head: Mutex::new(None),
            authenticated_transitions: Mutex::new(Vec::new()),
        };
        let (digest, root) = store.read_live_head_root()?;
        store.require_root_binding(&root)?;
        *store
            .authoritative_head
            .lock()
            .map_err(|_| StoreError::MalformedHistoryIndex)? = Some(digest);
        #[cfg(test)]
        sealed_history_authority_window_hook(SealedHistoryAuthorityWindowStage::Validated);
        Ok(store)
    }

    fn new(
        workspace_id: WorkspaceId,
        endpoint_id: super::ProjectionEndpointId,
        graph_resource_id: super::CanonicalGraphResourceId,
        receipt_store_id: super::ProjectionReceiptStoreId,
        control: Dir,
        archive_root: Dir,
        roots: Dir,
        index: EngineHistoryStore,
        transition_lock: fs::File,
    ) -> Result<Self, StoreError> {
        let store = Self {
            workspace_id,
            endpoint_id,
            graph_resource_id,
            receipt_store_id,
            control,
            archive_root,
            roots,
            index,
            transition_lock,
            transition: Mutex::new(()),
            authoritative_head: Mutex::new(None),
            authenticated_transitions: Mutex::new(Vec::new()),
        };
        store.initialize()?;
        Ok(store)
    }

    pub(crate) fn current(&self) -> Result<(u64, ContentDigest), StoreError> {
        let (_, root) = self.load_head_root()?;
        Ok((root.generation, root.index_root))
    }

    pub(crate) fn current_authority(&self) -> Result<EngineHistoryAuthority, StoreError> {
        let (_, root) = self.load_head_root()?;
        Ok(EngineHistoryAuthority {
            generation: root.generation,
            index_root: root.index_root,
        })
    }

    /// Authenticate an insertion-only transition directly from the shared
    /// immutable radix structure. Equal subtrees terminate immediately, so a
    /// normal point append is bounded by the changed radix paths rather than
    /// the lifetime history size.
    ///
    /// A promoted runtime proves every admission from one immutable bootstrap
    /// anchor, so the anchor falls further behind the head with each batch and
    /// the walk from that anchor grows with the post-anchor history. This open
    /// therefore memoizes the transitions it already proved and, when one of
    /// them starts at exactly this `before`, authenticates only the residual
    /// `middle -> current` step and composes.
    ///
    /// The memo is deliberately *transparent*: composition is attempted first
    /// and a failed residual step falls through to the complete
    /// `before -> current` walk, so the accepted/rejected outcome is exactly
    /// the one the uncached walk produces. That is what makes the accelerator
    /// safe rather than merely fast — see
    /// [`Self::compose_cached_history_extension`].
    ///
    /// A memo may only shorten the *walk*, never the availability and integrity
    /// facts the walk establishes about the live endpoints. Before any
    /// composition can run, [`Self::require_live_history_endpoint_nodes`]
    /// re-reads and re-authenticates from storage exactly the endpoint nodes
    /// the direct walk would have read, so a current root that has been
    /// deleted, truncated, substituted or digest-corrupted since this open
    /// warmed its memo is rejected identically warm and fresh. Faults *below*
    /// the endpoints stay a previously authenticated in-memory fact, guarded by
    /// the storage-fault latch described on [`EngineHistoryStore::storage_fault`]
    /// — see the residual note on [`Self::compose_cached_history_extension`].
    pub(crate) fn authenticate_current_history_extension(
        &self,
        before: EngineHistoryAuthority,
    ) -> Result<AuthenticatedEngineHistoryTransition, StoreError> {
        let after = self.current_authority()?;
        if (before.generation == 0) != (before.index_root == EngineHistoryStore::empty_root()) {
            return Err(StoreError::MalformedHistoryIndex);
        }
        let proof = match self.cached_history_extension(before) {
            Some(middle) => {
                self.require_live_history_endpoint_nodes(before.index_root, after.index_root)?;
                match self.compose_cached_history_extension(before, middle, after) {
                    Some(composed) => composed,
                    None => self.walk_history_extension(before, after)?,
                }
            }
            None => self.walk_history_extension(before, after)?,
        };
        self.remember_authenticated_history_extension(proof);
        Ok(proof)
    }

    /// The complete, unmemoized `before -> current` proof. This is the only
    /// thing that ever mints a transition out of raw storage, and it is exactly
    /// what a fresh open performs.
    fn walk_history_extension(
        &self,
        before: EngineHistoryAuthority,
        after: EngineHistoryAuthority,
    ) -> Result<AuthenticatedEngineHistoryTransition, StoreError> {
        let added = self.insertion_only_added_records(before.index_root, after.index_root, 0)?;
        if before
            .generation
            .checked_add(added)
            .filter(|generation| *generation == after.generation)
            .is_none()
        {
            return Err(StoreError::MalformedHistoryIndex);
        }
        Ok(AuthenticatedEngineHistoryTransition { before, after })
    }

    /// Re-establish, from storage, exactly the depth-0 availability and
    /// integrity facts [`Self::insertion_only_added_records`] would establish
    /// for this endpoint pair — no more, so a warm verdict stays byte-for-byte
    /// the fresh verdict, and no less, so a memo can never inherit them.
    ///
    /// The walk reads nothing when the two roots are identical (equal subtrees
    /// terminate immediately) and rejects a retreat to the empty root without
    /// reading anything, so those two cases are reproduced by reading nothing.
    /// Otherwise it reads `before` first and then `after`, requiring each to be
    /// an available, correctly addressed, canonical depth-0 branch; this
    /// mirrors the walk's own first step, including which endpoint reports the
    /// failure and with which error.
    fn require_live_history_endpoint_nodes(
        &self,
        before: ContentDigest,
        after: ContentDigest,
    ) -> Result<(), StoreError> {
        let empty = EngineHistoryStore::empty_root();
        if before == after || after == empty {
            return Ok(());
        }
        if before != empty {
            self.require_live_history_branch_root(before)?;
        }
        self.require_live_history_branch_root(after)
    }

    fn require_live_history_branch_root(&self, root: ContentDigest) -> Result<(), StoreError> {
        match self.index.read_node(root)? {
            HistoryIndexNode::Branch { depth: 0, .. } => Ok(()),
            // A node that reads cleanly but is not the radix root it is used as
            // is a substitution, not a lineage disagreement.
            _ => {
                self.index.note_storage_fault();
                Err(StoreError::MalformedHistoryIndex)
            }
        }
    }

    /// Compose a memoized `before -> middle` proof with a freshly walked
    /// `middle -> current` step.
    ///
    /// Soundness. A memo entry is only ever minted by
    /// [`Self::authenticate_current_history_extension`] on this exact store, so
    /// it carries this store's own proof that `middle`'s record set contains
    /// `before`'s with identical leaves on shared keys and that
    /// `before.generation + (|middle| - |before|) == middle.generation`. The
    /// residual step proves the same two facts for `middle -> current` with the
    /// identical walk and the identical exact-generation equality. Structural
    /// containment with agreeing leaves is transitive, and the two exact
    /// equalities telescope to
    /// `before.generation + (|current| - |before|) == current.generation`
    /// without overflow, because every intermediate sum is bounded by
    /// `current.generation`. Composition therefore establishes precisely what
    /// the direct `before -> current` walk establishes — including its
    /// rollback, divergence and missing-leaf rejections, which are exactly the
    /// walk's failures on the residual step.
    ///
    /// Lineage staleness. The memo records a fact about immutable
    /// content-addressed radix nodes, not a claim about the live head, so the
    /// *structural* fact cannot decay: no publish, failed publish or head
    /// replacement can make a once-true containment false. The live `current`
    /// is re-read on every call and the residual step is always freshly walked
    /// and digest-verified, so a memo that no longer lies on the live lineage
    /// can only fail to compose. Returning `None` on any such failure hands the
    /// decision back to the complete walk, so the memo can neither turn a
    /// rejection into an acceptance nor an acceptance into a rejection.
    ///
    /// Availability. What a memo *can* outlive is the storage the walk read.
    /// Two mechanisms bound that, because re-reading the whole authenticated
    /// region on every call is precisely the lifetime-sized work the memo
    /// exists to remove:
    ///
    /// 1. [`Self::require_live_history_endpoint_nodes`] re-reads and
    ///    re-authenticates the live endpoint nodes on every warm call, so
    ///    depth-0 loss, truncation, substitution and digest corruption are
    ///    rejected identically warm and fresh.
    /// 2. Deeper nodes stay a fact this same open authenticated earlier — every
    ///    node the direct `before -> current` walk would read was read and
    ///    digest-verified by this store when the memo entry was minted, by
    ///    induction over the composition chain. The compensating guarantee is
    ///    causal: the first operation that re-encounters damage down there —
    ///    any lookup, replay, rebuild or insertion that descends into it —
    ///    latches [`EngineHistoryStore::storage_fault`], which permanently
    ///    disarms this memo, so from that point the store decides exactly like
    ///    a fresh open. [`Self::publish`] latches it for an incomplete
    ///    publication too.
    ///
    /// The residual this leaves is narrow and deliberate: a node that this open
    /// already authenticated is destroyed by something outside Tine while Tine
    /// runs, and nothing touches it again before the next admission. Such an
    /// admission can extend the history along an undamaged radix path, which a
    /// fresh open would instead refuse; it cannot surface, project or replay
    /// the damaged region, because every path that reads it latches the fault
    /// first. A reopened store starts with an empty memo and pays the full walk
    /// once, so nothing here survives a restart.
    fn compose_cached_history_extension(
        &self,
        before: EngineHistoryAuthority,
        middle: EngineHistoryAuthority,
        after: EngineHistoryAuthority,
    ) -> Option<AuthenticatedEngineHistoryTransition> {
        let added = self
            .insertion_only_added_records(middle.index_root, after.index_root, 0)
            .ok()?;
        middle
            .generation
            .checked_add(added)
            .filter(|generation| *generation == after.generation)?;
        Some(AuthenticatedEngineHistoryTransition { before, after })
    }

    /// The furthest endpoint this store proved from exactly this anchor.
    ///
    /// Both anchor fields must match exactly; a substituted generation or index
    /// root simply misses the memo and is decided by the full walk. A latched
    /// storage fault discards the memo outright and keeps it discarded for the
    /// rest of this open.
    fn cached_history_extension(
        &self,
        before: EngineHistoryAuthority,
    ) -> Option<EngineHistoryAuthority> {
        let mut cache = self.authenticated_transitions.lock().ok()?;
        if self.index.storage_faulted() {
            cache.clear();
            return None;
        }
        cache
            .iter()
            .find(|entry| entry.before == before)
            .map(|entry| entry.after)
    }

    /// Retain the proof so the next admission from the same anchor only has to
    /// walk the records published after it.
    ///
    /// One entry per anchor and at most
    /// [`MAX_AUTHENTICATED_TRANSITION_ANCHORS`] anchors, evicted least-recently
    /// proved first. The recency order matters: the projection-work caller
    /// re-anchors on the head it just accepted, so it presents a *fresh* anchor
    /// every batch, while the promoted-runtime caller keeps proving from one
    /// immutable bootstrap anchor. Plain insertion order would let the moving
    /// anchor evict the fixed one within a few batches and restore exactly the
    /// growth this memo exists to remove; re-seating an anchor on every
    /// successful proof keeps the repeatedly used one resident. Only a proof
    /// this store just minted is recorded, so a rejected transition can neither
    /// enter the memo nor churn it. A poisoned memo lock degrades to no memo,
    /// never to a weaker proof, and so does a latched storage fault.
    fn remember_authenticated_history_extension(
        &self,
        proof: AuthenticatedEngineHistoryTransition,
    ) {
        let Ok(mut cache) = self.authenticated_transitions.lock() else {
            return;
        };
        if self.index.storage_faulted() {
            cache.clear();
            return;
        }
        cache.retain(|entry| entry.before != proof.before);
        if cache.len() >= MAX_AUTHENTICATED_TRANSITION_ANCHORS {
            cache.remove(0);
        }
        cache.push(proof);
    }

    fn insertion_only_added_records(
        &self,
        before: ContentDigest,
        after: ContentDigest,
        depth: u8,
    ) -> Result<u64, StoreError> {
        if before == after {
            return Ok(0);
        }
        if before == EngineHistoryStore::empty_root() {
            return self.history_record_count(after, depth);
        }
        if after == EngineHistoryStore::empty_root() {
            return Err(StoreError::MalformedHistoryIndex);
        }
        match (self.index.read_node(before)?, self.index.read_node(after)?) {
            (
                HistoryIndexNode::Branch {
                    depth: before_depth,
                    children: before_children,
                    ..
                },
                HistoryIndexNode::Branch {
                    depth: after_depth,
                    children: after_children,
                    ..
                },
            ) if before_depth == depth && after_depth == depth => {
                let mut added = 0_u64;
                for (nibble, before_child) in &before_children {
                    let after_child = after_children
                        .iter()
                        .find(|(candidate, _)| *candidate == *nibble)
                        .map(|(_, digest)| *digest)
                        .ok_or(StoreError::MalformedHistoryIndex)?;
                    added = added
                        .checked_add(self.insertion_only_added_records(
                            *before_child,
                            after_child,
                            depth + 1,
                        )?)
                        .ok_or(StoreError::MalformedHistoryIndex)?;
                }
                for (nibble, after_child) in after_children {
                    if !before_children
                        .iter()
                        .any(|(candidate, _)| *candidate == nibble)
                    {
                        added = added
                            .checked_add(self.history_record_count(after_child, depth + 1)?)
                            .ok_or(StoreError::MalformedHistoryIndex)?;
                    }
                }
                Ok(added)
            }
            (
                HistoryIndexNode::Leaf {
                    batch_id: before_batch,
                    record: before_record,
                    ..
                },
                HistoryIndexNode::Leaf {
                    batch_id: after_batch,
                    record: after_record,
                    ..
                },
            ) if depth == ENGINE_HISTORY_RADIX_DEPTH
                && before_batch == after_batch
                && before_record == after_record =>
            {
                Ok(0)
            }
            _ => Err(StoreError::MalformedHistoryIndex),
        }
    }

    fn history_record_count(&self, root: ContentDigest, depth: u8) -> Result<u64, StoreError> {
        if root == EngineHistoryStore::empty_root() {
            return Ok(0);
        }
        match self.index.read_node(root)? {
            HistoryIndexNode::Branch {
                depth: found,
                children,
                ..
            } if found == depth && depth < ENGINE_HISTORY_RADIX_DEPTH => {
                children.into_iter().try_fold(0_u64, |count, (_, child)| {
                    count
                        .checked_add(self.history_record_count(child, depth + 1)?)
                        .ok_or(StoreError::MalformedHistoryIndex)
                })
            }
            HistoryIndexNode::Leaf { .. } if depth == ENGINE_HISTORY_RADIX_DEPTH => Ok(1),
            _ => Err(StoreError::MalformedHistoryIndex),
        }
    }

    pub(crate) fn current_with_binding(
        &self,
    ) -> Result<(u64, ContentDigest, Option<BatchId>, EngineHistoryBinding), StoreError> {
        let (_, root) = self.load_head_root()?;
        Ok((
            root.generation,
            root.index_root,
            root.latest_batch_id,
            root.binding.engine.clone(),
        ))
    }

    pub(crate) fn current_record_count(&self) -> Result<u64, StoreError> {
        let (_, root) = self.load_head_root()?;
        self.history_record_count(root.index_root, 0)
    }

    fn validate_sealed_open(&self) -> Result<(), StoreError> {
        let claim = read_optional_regular(&self.control, ENGINE_HISTORY_CLAIM_FILE, 256, None)?
            .ok_or(StoreError::MalformedHistoryIndex)?;
        validate_engine_history_claim(
            &claim,
            self.workspace_id,
            self.endpoint_id,
            self.graph_resource_id,
            self.receipt_store_id,
        )?;
        let expected = self
            .authoritative_head
            .lock()
            .map_err(|_| StoreError::MalformedHistoryIndex)?
            .ok_or(StoreError::MalformedHistoryIndex)?;
        let (live, root) = self.read_live_head_root()?;
        if live != expected {
            return Err(StoreError::MalformedHistoryIndex);
        }
        self.require_root_binding(&root)
    }

    pub(crate) fn lookup(
        &self,
        index_root: ContentDigest,
        batch_id: BatchId,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.index.lookup(index_root, batch_id)
    }

    pub(crate) fn materialize(
        &self,
        index_root: ContentDigest,
    ) -> Result<Vec<(BatchId, Vec<u8>)>, StoreError> {
        self.index.materialize(index_root)
    }

    pub(crate) fn note_history_decode(&self) {
        self.index.note_history_decode();
    }

    /// Extend the durable history by one record.
    ///
    /// A publication that starts and does not complete may have failed anywhere
    /// between the head read and the head swap, including on damaged index or
    /// root storage. Nothing this open proved earlier may survive such a
    /// failure as a shortcut, so any outcome other than success or the
    /// read-only bootstrap refusal — which is decided before any storage is
    /// touched — latches the storage fault and disarms the
    /// authenticated-transition memo for the rest of this open.
    pub(crate) fn publish(
        &self,
        batch_id: BatchId,
        bytes: &[u8],
        binding: EngineHistoryBinding,
    ) -> Result<(u64, ContentDigest), StoreError> {
        let _guard = self
            .transition
            .lock()
            .map_err(|_| StoreError::MalformedHistoryIndex)?;
        let _workspace_guard = AdvisoryTransitionGuard::lock(&self.transition_lock)?;
        let published = self.publish_locked(batch_id, bytes, binding);
        if !matches!(published, Ok(_) | Err(StoreError::InactiveBootstrapHistory)) {
            self.index.note_storage_fault();
            if let Ok(mut cache) = self.authenticated_transitions.lock() {
                cache.clear();
            }
        }
        published
    }

    fn publish_locked(
        &self,
        batch_id: BatchId,
        bytes: &[u8],
        binding: EngineHistoryBinding,
    ) -> Result<(u64, ContentDigest), StoreError> {
        let (before_digest, before) = self.load_head_root()?;
        let index_root = self.index.insert(before.index_root, batch_id, bytes)?;
        if index_root == before.index_root {
            return Ok((before.generation, before.index_root));
        }
        let after = DurableEngineHistoryRoot {
            schema_version: ENGINE_HISTORY_ROOT_SCHEMA_VERSION,
            workspace_id: self.workspace_id,
            endpoint_id: self.endpoint_id,
            graph_resource_id: self.graph_resource_id,
            receipt_store_id: self.receipt_store_id,
            generation: before
                .generation
                .checked_add(1)
                .ok_or(StoreError::MalformedHistoryIndex)?,
            index_root,
            latest_batch_id: Some(batch_id),
            binding: DurableEngineHistoryBinding { engine: binding },
        };
        let after_digest = self.publish_root(&after)?;
        self.replace_head(before_digest, after_digest)?;
        Ok((after.generation, after.index_root))
    }

    fn initialize(&self) -> Result<(), StoreError> {
        let head = read_optional_regular(&self.control, ENGINE_HISTORY_HEAD_FILE, 64, None)?;
        let claim = read_optional_regular(&self.control, ENGINE_HISTORY_CLAIM_FILE, 256, None)?;
        match (head, claim) {
            (None, None) => {
                let empty = DurableEngineHistoryRoot {
                    schema_version: ENGINE_HISTORY_ROOT_SCHEMA_VERSION,
                    workspace_id: self.workspace_id,
                    endpoint_id: self.endpoint_id,
                    graph_resource_id: self.graph_resource_id,
                    receipt_store_id: self.receipt_store_id,
                    generation: 0,
                    index_root: EngineHistoryStore::empty_root(),
                    latest_batch_id: None,
                    binding: DurableEngineHistoryBinding::ordinary(EngineHistoryBinding::empty()),
                };
                let empty_digest = self.publish_root(&empty)?;
                publish_immutable_exact(
                    &self.control,
                    ENGINE_HISTORY_HEAD_FILE,
                    empty_digest.to_string().as_bytes(),
                    "engine history head",
                )?;
                let expected_claim = postcard::to_allocvec(&(
                    ENGINE_HISTORY_ROOT_SCHEMA_VERSION,
                    self.workspace_id,
                    self.endpoint_id,
                    self.graph_resource_id,
                    self.receipt_store_id,
                ))
                .map_err(|_| StoreError::MalformedHistoryIndex)?;
                publish_immutable_exact(
                    &self.control,
                    ENGINE_HISTORY_CLAIM_FILE,
                    &expected_claim,
                    "engine history claim",
                )?;
            }
            (Some(_), Some(claim)) => validate_engine_history_claim(
                &claim,
                self.workspace_id,
                self.endpoint_id,
                self.graph_resource_id,
                self.receipt_store_id,
            )?,
            _ => return Err(StoreError::MalformedHistoryIndex),
        }
        self.read_live_head_root()?;
        Ok(())
    }

    fn publish_root(&self, root: &DurableEngineHistoryRoot) -> Result<ContentDigest, StoreError> {
        self.require_root_binding(root)?;
        let bytes = postcard::to_allocvec(root).map_err(|_| StoreError::MalformedHistoryIndex)?;
        let digest = ContentDigest::of(&bytes);
        publish_immutable_exact(
            &self.roots,
            &engine_history_root_filename(digest),
            &bytes,
            "engine history authenticated root",
        )?;
        Ok(digest)
    }

    fn load_head_root(&self) -> Result<(ContentDigest, DurableEngineHistoryRoot), StoreError> {
        let sealed = self
            .authoritative_head
            .lock()
            .map_err(|_| StoreError::MalformedHistoryIndex)?
            .to_owned();
        match sealed {
            Some(expected) => {
                let (live, root) = self.read_live_head_root()?;
                if live != expected {
                    return Err(StoreError::MalformedHistoryIndex);
                }
                Ok((live, root))
            }
            None => self.read_live_head_root(),
        }
    }

    fn read_live_head_root(&self) -> Result<(ContentDigest, DurableEngineHistoryRoot), StoreError> {
        let head = read_optional_regular(&self.control, ENGINE_HISTORY_HEAD_FILE, 64, None)?
            .ok_or(StoreError::MalformedHistoryIndex)?;
        let text = std::str::from_utf8(&head).map_err(|_| StoreError::MalformedHistoryIndex)?;
        let digest = parse_digest(text)
            .map(ContentDigest::from_bytes)
            .map_err(|_| StoreError::MalformedHistoryIndex)?;
        if digest.to_string().as_bytes() != head {
            return Err(StoreError::MalformedHistoryIndex);
        }
        Ok((digest, self.load_root(digest)?))
    }

    fn load_root(&self, digest: ContentDigest) -> Result<DurableEngineHistoryRoot, StoreError> {
        let bytes = read_optional_regular(
            &self.roots,
            &engine_history_root_filename(digest),
            MAX_ENGINE_HISTORY_INDEX_BYTES,
            None,
        )?
        .ok_or(StoreError::MalformedHistoryIndex)?;
        if ContentDigest::of(&bytes) != digest {
            return Err(StoreError::HistoryIndexPathMismatch(digest));
        }
        let root: DurableEngineHistoryRoot =
            postcard::from_bytes(&bytes).map_err(|_| StoreError::MalformedHistoryIndex)?;
        if postcard::to_allocvec(&root).map_err(|_| StoreError::MalformedHistoryIndex)? != bytes {
            return Err(StoreError::MalformedHistoryIndex);
        }
        self.require_root_binding(&root)?;
        Ok(root)
    }

    fn require_root_binding(&self, root: &DurableEngineHistoryRoot) -> Result<(), StoreError> {
        validate_engine_history_root(
            root,
            self.workspace_id,
            self.endpoint_id,
            self.graph_resource_id,
            self.receipt_store_id,
        )
    }

    fn replace_head(
        &self,
        expected: ContentDigest,
        replacement: ContentDigest,
    ) -> Result<(), StoreError> {
        let (current, _) = self.read_live_head_root()?;
        if current != expected {
            return Err(StoreError::MalformedHistoryIndex);
        }
        let temp_name = format!(".tmp-{}", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut temp = self.control.open_with(&temp_name, &options)?;
        let result = (|| {
            temp.write_all(replacement.to_string().as_bytes())?;
            crate::durability_counters::sync_file(&temp)?;
            drop(temp);
            #[cfg(test)]
            ENGINE_HISTORY_FAIL_BEFORE_HEAD_SWAP.with(|fail| {
                if fail.replace(false) {
                    return Err(StoreError::Io(std::io::Error::other(
                        "injected engine history failure before authenticated head swap",
                    )));
                }
                Ok(())
            })?;
            self.control
                .rename(&temp_name, &self.control, ENGINE_HISTORY_HEAD_FILE)?;
            #[cfg(test)]
            ENGINE_HISTORY_FAIL_AFTER_HEAD_SWAP.with(|fail| {
                if fail.replace(false) {
                    return Err(StoreError::Io(std::io::Error::other(
                        "injected engine history failure after authenticated head swap",
                    )));
                }
                Ok(())
            })?;
            sync_dir_required(&self.control)?;
            Ok::<_, StoreError>(())
        })();
        let cleanup = self.control.remove_file(&temp_name);
        if let Err(error) = result {
            let _ = cleanup;
            return Err(error);
        }
        if cleanup
            .as_ref()
            .is_err_and(|error| error.kind() != ErrorKind::NotFound)
        {
            cleanup?;
        }
        *self
            .authoritative_head
            .lock()
            .map_err(|_| StoreError::MalformedHistoryIndex)? = Some(replacement);
        Ok(())
    }
}

/// Named durable boundaries of one resume-point publication, after the
/// resume-point directory exists.
///
/// These are the two cuts [`DurableEngineHistoryStore::publish_resume_point`]
/// can leave behind that no other test route can reach: the survey/publish
/// primitives are callable directly, but the *pre-prune* is only ever executed
/// from inside the publication, so a crash between it and the commit point —
/// the only window in which this packet deletes a durable point *before*
/// committing its replacement — is otherwise unobservable. Deterministic
/// injection at each of them proves at least one fully valid point survives
/// every cut.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumePublishBoundary {
    /// After step 1's pre-prune, before the immutable commit point.
    AfterPrePrune,
    /// After the commit point, before step 3's prune.
    AfterCommit,
}

#[cfg(test)]
impl ResumePublishBoundary {
    /// Every durable boundary of the publication, in publication order.
    pub(crate) const ALL: [Self; 2] = [Self::AfterPrePrune, Self::AfterCommit];
}

#[cfg(test)]
thread_local! {
    /// One-shot publication fault. Thread-local and deterministic: no
    /// process-global resource limit or signal is involved, so parallel tests
    /// in other threads are unaffected.
    static RESUME_PUBLISH_FAULT: std::cell::Cell<Option<ResumePublishBoundary>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn fail_next_resume_publication_at(boundary: ResumePublishBoundary) {
    RESUME_PUBLISH_FAULT.with(|fault| fault.set(Some(boundary)));
}

#[cfg(test)]
thread_local! {
    static RESUME_CLEAR_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_resume_clear() {
    RESUME_CLEAR_FAULT.with(|fault| fault.set(true));
}

#[cfg(test)]
fn inject_resume_clear_fault() -> Result<(), StoreError> {
    RESUME_CLEAR_FAULT.with(|fault| {
        if fault.replace(false) {
            Err(StoreError::Io(std::io::Error::other(
                "injected resume-point clear failure before the first removal",
            )))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn inject_resume_publish_fault(boundary: ResumePublishBoundary) -> Result<(), StoreError> {
    RESUME_PUBLISH_FAULT.with(|fault| {
        if fault.get() == Some(boundary) {
            fault.set(None);
            return Err(StoreError::Io(std::io::Error::other(format!(
                "injected resume-point publication failure at {boundary:?}"
            ))));
        }
        Ok(())
    })
}

fn validate_engine_history_root(
    root: &DurableEngineHistoryRoot,
    workspace_id: WorkspaceId,
    endpoint_id: super::ProjectionEndpointId,
    graph_resource_id: super::CanonicalGraphResourceId,
    receipt_store_id: super::ProjectionReceiptStoreId,
) -> Result<(), StoreError> {
    if root.schema_version < ENGINE_HISTORY_ROOT_SCHEMA_VERSION {
        return Err(StoreError::UpgradeRequired {
            store: "engine history",
            found: root.schema_version,
            current: ENGINE_HISTORY_ROOT_SCHEMA_VERSION,
        });
    }
    if root.schema_version > ENGINE_HISTORY_ROOT_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedStoreVersion {
            store: "engine history",
            version: root.schema_version,
        });
    }
    if root.workspace_id != workspace_id
        || root.endpoint_id != endpoint_id
        || root.graph_resource_id != graph_resource_id
        || root.receipt_store_id != receipt_store_id
        || root.binding.engine.portable_path_key_version != super::PORTABLE_PATH_KEY_VERSION
        || (root.generation == 0) != root.latest_batch_id.is_none()
        || root
            .binding
            .engine
            .portable_path_conflicts
            .windows(2)
            .any(|pair| pair[0].key_digest() >= pair[1].key_digest())
        || root
            .binding
            .engine
            .portable_path_conflicts
            .iter()
            .any(|conflict| {
                conflict.key_version() != super::PORTABLE_PATH_KEY_VERSION
                    || conflict.participants().len() < 2
                    || conflict
                        .participants()
                        .windows(2)
                        .any(|pair| pair[0] >= pair[1])
            })
        || (!root.binding.engine.portable_path_conflicts.is_empty()
            && root.binding.engine.terminal_evidence.is_none())
    {
        return Err(StoreError::MalformedHistoryIndex);
    }
    root.binding.engine.page_names.validate()?;
    Ok(())
}

impl BlockClaimIndexStore {
    fn with_file<T>(
        &self,
        operation: impl FnOnce(&mut fs::File) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        match &self.backing {
            BlockClaimIndexBacking::Scratch(scratch) => scratch
                .with_pages(operation)
                .map_err(|error| StoreError::Scratch(error.to_string()))?,
            #[cfg(test)]
            BlockClaimIndexBacking::Standalone(file) => {
                let mut file = file
                    .lock()
                    .map_err(|_| StoreError::MalformedBlockClaimIndex)?;
                operation(&mut file)
            }
        }
    }

    pub(crate) fn lookup_many(
        &self,
        root: BlockClaimIndexRoot,
        keys: &[[u8; 16]],
    ) -> Result<BTreeMap<[u8; 16], BlockClaimIndexValue>, StoreError> {
        if keys.is_empty() || root.levels.iter().flatten().all(Option::is_none) {
            return Ok(BTreeMap::new());
        }
        if !keys.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(StoreError::MalformedBlockClaimIndex);
        }
        self.with_file(|file| {
            let mut segments: Vec<_> = root.levels.into_iter().flatten().flatten().collect();
            segments.sort_unstable_by_key(|segment| std::cmp::Reverse(segment.generation));
            let mut remaining: Vec<_> = keys
                .iter()
                .copied()
                .map(|key| {
                    let (first, second) = block_claim_filter_hashes(&key);
                    (key, first, second)
                })
                .collect();
            let global_filter = self.read_claim_global_filter(
                file,
                root.global_filter
                    .ok_or(StoreError::MalformedBlockClaimIndex)?,
            )?;
            remaining.retain(|(_, first, second)| {
                block_claim_global_filter_might_contain(&global_filter, *first, *second)
            });
            if remaining.is_empty() {
                return Ok(BTreeMap::new());
            }
            let mut found = BTreeMap::new();
            for segment in segments {
                let filter = self.read_claim_filter(file, segment.filter_ref)?;
                if filter.entry_count != segment.entry_count {
                    return Err(StoreError::MalformedBlockClaimIndex);
                }
                let selected: Vec<_> = remaining
                    .iter()
                    .filter(|(_, first, second)| {
                        block_claim_filter_might_contain(&filter, *first, *second)
                    })
                    .map(|(key, _, _)| *key)
                    .collect();
                if selected.is_empty() {
                    continue;
                }
                let mut segment_found = BTreeMap::new();
                self.lookup_many_at(file, segment.page_ref, 0, &selected, &mut segment_found)?;
                found.extend(segment_found);
                remaining.retain(|(key, _, _)| !found.contains_key(key));
                if remaining.is_empty() {
                    break;
                }
            }
            Ok(found)
        })
    }

    pub(crate) fn insert_many(
        &self,
        root: BlockClaimIndexRoot,
        records: &[([u8; 16], BlockClaimIndexValue)],
    ) -> Result<BlockClaimIndexRoot, StoreError> {
        if records.is_empty() {
            return Ok(root);
        }
        if !records.windows(2).all(|pair| pair[0].0 < pair[1].0)
            || records
                .iter()
                .any(|(_, record)| record.is_empty() || record.len() > MAX_BLOCK_CLAIM_RECORD_BYTES)
        {
            return Err(StoreError::MalformedBlockClaimIndex);
        }
        self.with_file(|file| {
            let generation = root
                .next_generation
                .checked_add(1)
                .ok_or(StoreError::MalformedBlockClaimIndex)?;
            let mut global_filter = match root.global_filter {
                Some(page_ref) => self.read_claim_global_filter(file, page_ref)?,
                None => new_block_claim_global_filter(),
            };
            update_block_claim_global_filter(&mut global_filter, records)?;
            let mut next = root;
            next.next_generation = generation;
            let mut merged = records.to_vec();
            let mut installed = false;
            for level in &mut next.levels {
                if let Some(empty) = level.iter().position(Option::is_none) {
                    let entry_count = u64::try_from(merged.len())
                        .map_err(|_| StoreError::MalformedBlockClaimIndex)?;
                    let filter_ref = self.append_claim_filter(file, &merged)?;
                    let page_ref = self.build_claim_subtree(file, 0, merged)?;
                    level[empty] = Some(BlockClaimSegmentRef {
                        generation,
                        entry_count,
                        page_ref,
                        filter_ref,
                    });
                    installed = true;
                    break;
                }
                let mut existing: Vec<_> = level.iter_mut().filter_map(Option::take).collect();
                existing.sort_unstable_by_key(|segment| segment.generation);
                let capacity = existing.iter().try_fold(merged.len(), |capacity, segment| {
                    usize::try_from(segment.entry_count)
                        .ok()
                        .and_then(|entries| capacity.checked_add(entries))
                });
                let mut combined =
                    AHashMap::with_capacity(capacity.ok_or(StoreError::MalformedBlockClaimIndex)?);
                for segment in existing {
                    let mut older = Vec::with_capacity(
                        usize::try_from(segment.entry_count)
                            .map_err(|_| StoreError::MalformedBlockClaimIndex)?,
                    );
                    self.materialize_claim_segment(file, segment.page_ref, 0, &mut older)?;
                    if older.len() as u64 != segment.entry_count {
                        return Err(StoreError::MalformedBlockClaimIndex);
                    }
                    combined.extend(older);
                }
                combined.extend(merged);
                merged = combined.into_iter().collect();
            }
            if !installed {
                return Err(StoreError::MalformedBlockClaimIndex);
            }
            next.global_filter = Some(self.append_claim_global_filter(file, &global_filter)?);
            Ok(next)
        })
    }

    fn lookup_many_at(
        &self,
        file: &mut fs::File,
        page_ref: BlockClaimPageRef,
        expected_depth: u8,
        keys: &[[u8; 16]],
        found: &mut BTreeMap<[u8; 16], BlockClaimIndexValue>,
    ) -> Result<(), StoreError> {
        match self.read_claim_page(file, page_ref, expected_depth)? {
            BlockClaimIndexPage::Leaf { entries, .. } => {
                for key in keys {
                    if let Ok(index) =
                        entries.binary_search_by_key(key, |(candidate, _)| *candidate)
                    {
                        found.insert(*key, entries[index].1.clone());
                    }
                }
            }
            BlockClaimIndexPage::Branch {
                depth, children, ..
            } => {
                let mut grouped = BTreeMap::<u8, Vec<[u8; 16]>>::new();
                for key in keys {
                    grouped
                        .entry(block_claim_key_nibble(key, depth))
                        .or_default()
                        .push(*key);
                }
                for (nibble, selected) in grouped {
                    if let Ok(index) =
                        children.binary_search_by_key(&nibble, |(candidate, _)| *candidate)
                    {
                        self.lookup_many_at(file, children[index].1, depth + 1, &selected, found)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn build_claim_subtree(
        &self,
        file: &mut fs::File,
        depth: u8,
        mut entries: Vec<([u8; 16], BlockClaimIndexValue)>,
    ) -> Result<BlockClaimPageRef, StoreError> {
        let estimated_encoded_bytes = entries.iter().try_fold(32_usize, |total, (_, record)| {
            total.checked_add(26)?.checked_add(record.len())
        });
        if (entries.len() <= BLOCK_CLAIM_LEAF_ENTRIES
            && estimated_encoded_bytes.is_some_and(|bytes| bytes <= MAX_BLOCK_CLAIM_PAGE_BYTES))
            || depth == BLOCK_CLAIM_RADIX_DEPTH
        {
            entries.sort_unstable_by_key(|entry| entry.0);
            return self.append_claim_page(
                file,
                &BlockClaimIndexPage::Leaf {
                    schema_version: BLOCK_CLAIM_INDEX_SCHEMA_VERSION,
                    depth,
                    entries,
                },
            );
        }
        let mut grouped = BTreeMap::<u8, Vec<([u8; 16], BlockClaimIndexValue)>>::new();
        for entry in entries {
            grouped
                .entry(block_claim_key_nibble(&entry.0, depth))
                .or_default()
                .push(entry);
        }
        let mut children = Vec::with_capacity(grouped.len());
        for (nibble, selected) in grouped {
            children.push((nibble, self.build_claim_subtree(file, depth + 1, selected)?));
        }
        self.append_claim_page(
            file,
            &BlockClaimIndexPage::Branch {
                schema_version: BLOCK_CLAIM_INDEX_SCHEMA_VERSION,
                depth,
                children,
            },
        )
    }

    fn append_claim_page(
        &self,
        file: &mut fs::File,
        page: &BlockClaimIndexPage,
    ) -> Result<BlockClaimPageRef, StoreError> {
        validate_block_claim_page(page)?;
        let bytes =
            postcard::to_allocvec(page).map_err(|_| StoreError::MalformedBlockClaimIndex)?;
        self.append_claim_bytes(file, &bytes)
    }

    fn append_claim_filter(
        &self,
        file: &mut fs::File,
        entries: &[([u8; 16], BlockClaimIndexValue)],
    ) -> Result<BlockClaimPageRef, StoreError> {
        let filter = new_block_claim_filter(entries)?;
        let bytes =
            postcard::to_allocvec(&filter).map_err(|_| StoreError::MalformedBlockClaimIndex)?;
        self.append_claim_bytes(file, &bytes)
    }

    fn append_claim_global_filter(
        &self,
        file: &mut fs::File,
        filter: &BlockClaimGlobalFilterPage,
    ) -> Result<BlockClaimPageRef, StoreError> {
        validate_block_claim_global_filter(filter)?;
        let bytes =
            postcard::to_allocvec(filter).map_err(|_| StoreError::MalformedBlockClaimIndex)?;
        self.append_claim_bytes(file, &bytes)
    }

    fn append_claim_bytes(
        &self,
        file: &mut fs::File,
        bytes: &[u8],
    ) -> Result<BlockClaimPageRef, StoreError> {
        if bytes.len() > MAX_BLOCK_CLAIM_PAGE_BYTES {
            return Err(StoreError::StoredFileTooLarge {
                path: BLOCK_CLAIM_INDEX_FILE.into(),
                length: bytes.len() as u64,
                limit: MAX_BLOCK_CLAIM_PAGE_BYTES as u64,
            });
        }
        let encoded_len =
            u32::try_from(bytes.len()).map_err(|_| StoreError::MalformedBlockClaimIndex)?;
        let offset = file.seek(SeekFrom::End(0))?;
        file.write_all(&encoded_len.to_be_bytes())?;
        file.write_all(bytes)?;
        self.counters
            .block_claim_index_writes
            .fetch_add(1, Ordering::Relaxed);
        Ok(BlockClaimPageRef {
            offset,
            encoded_len,
            digest: ContentDigest::of(bytes),
        })
    }

    fn materialize_claim_segment(
        &self,
        file: &mut fs::File,
        page_ref: BlockClaimPageRef,
        expected_depth: u8,
        entries: &mut Vec<([u8; 16], BlockClaimIndexValue)>,
    ) -> Result<(), StoreError> {
        match self.read_claim_page(file, page_ref, expected_depth)? {
            BlockClaimIndexPage::Leaf {
                entries: selected, ..
            } => entries.extend(selected),
            BlockClaimIndexPage::Branch {
                depth, children, ..
            } => {
                for (_, child) in children {
                    self.materialize_claim_segment(file, child, depth + 1, entries)?;
                }
            }
        }
        Ok(())
    }

    fn read_claim_page(
        &self,
        file: &mut fs::File,
        page_ref: BlockClaimPageRef,
        expected_depth: u8,
    ) -> Result<BlockClaimIndexPage, StoreError> {
        let bytes = self.read_claim_bytes(file, page_ref)?;
        let page: BlockClaimIndexPage =
            postcard::from_bytes(&bytes).map_err(|_| StoreError::MalformedBlockClaimIndex)?;
        validate_block_claim_page(&page)?;
        if block_claim_page_depth(&page) != expected_depth
            || postcard::to_allocvec(&page).map_err(|_| StoreError::MalformedBlockClaimIndex)?
                != bytes
        {
            return Err(StoreError::MalformedBlockClaimIndex);
        }
        Ok(page)
    }

    fn read_claim_filter(
        &self,
        file: &mut fs::File,
        page_ref: BlockClaimPageRef,
    ) -> Result<BlockClaimFilterPage, StoreError> {
        let bytes = self.read_claim_bytes(file, page_ref)?;
        let filter: BlockClaimFilterPage =
            postcard::from_bytes(&bytes).map_err(|_| StoreError::MalformedBlockClaimIndex)?;
        validate_block_claim_filter(&filter)?;
        if postcard::to_allocvec(&filter).map_err(|_| StoreError::MalformedBlockClaimIndex)?
            != bytes
        {
            return Err(StoreError::MalformedBlockClaimIndex);
        }
        Ok(filter)
    }

    fn read_claim_global_filter(
        &self,
        file: &mut fs::File,
        page_ref: BlockClaimPageRef,
    ) -> Result<BlockClaimGlobalFilterPage, StoreError> {
        let bytes = self.read_claim_bytes(file, page_ref)?;
        let filter: BlockClaimGlobalFilterPage =
            postcard::from_bytes(&bytes).map_err(|_| StoreError::MalformedBlockClaimIndex)?;
        validate_block_claim_global_filter(&filter)?;
        if postcard::to_allocvec(&filter).map_err(|_| StoreError::MalformedBlockClaimIndex)?
            != bytes
        {
            return Err(StoreError::MalformedBlockClaimIndex);
        }
        Ok(filter)
    }

    fn read_claim_bytes(
        &self,
        file: &mut fs::File,
        page_ref: BlockClaimPageRef,
    ) -> Result<Vec<u8>, StoreError> {
        file.seek(SeekFrom::Start(page_ref.offset))?;
        let mut length = [0_u8; 4];
        file.read_exact(&mut length)?;
        let found_len = u32::from_be_bytes(length);
        if found_len != page_ref.encoded_len
            || usize::try_from(found_len)
                .ok()
                .is_none_or(|length| length == 0 || length > MAX_BLOCK_CLAIM_PAGE_BYTES)
        {
            return Err(StoreError::MalformedBlockClaimIndex);
        }
        let mut bytes = vec![0_u8; found_len as usize];
        file.read_exact(&mut bytes)?;
        if ContentDigest::of(&bytes) != page_ref.digest {
            return Err(StoreError::BlockClaimIndexPathMismatch(page_ref.digest));
        }
        self.counters
            .block_claim_index_reads
            .fetch_add(1, Ordering::Relaxed);
        Ok(bytes)
    }
}

#[derive(Clone, Copy)]
enum NamespaceKind {
    Objects,
    Batches,
}

/// `TINE_PUBLISH_TRACE=1` names the artifact class of every individually
/// published immutable artifact on stderr.
///
/// The barrier budget in `docs/storage-sync-contract.md` 2.10a-i is a number;
/// this is how you find out WHICH artifacts make it up when the number moves.
/// Read once per process, like the other `TINE_*_TRACE` switches in this crate.
fn publication_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TINE_PUBLISH_TRACE").is_some())
}

impl Collision {
    /// The artifact class this publication belongs to, for the publication
    /// trace.
    fn artifact_class(&self) -> String {
        match self {
            Self::Object(_) => "archive-object".to_owned(),
            Self::Batch(_) => "archive-manifest".to_owned(),
            Self::HistoryIndex(_) => "history-index".to_owned(),
            Self::Lineage(_) => "archive-lineage-claim".to_owned(),
            Self::Exact(kind) => (*kind).to_owned(),
            Self::Bootstrap(kind, _) => format!("bootstrap:{kind}"),
        }
    }
}

#[derive(Clone, Debug)]
enum Collision {
    Object(ContentDigest),
    Batch(BatchId),
    HistoryIndex(ContentDigest),
    Lineage(LineageDigest),
    Exact(&'static str),
    Bootstrap(&'static str, String),
}

fn ensure_single_lineage(manifests: &[OperationBatch]) -> Result<(), StoreError> {
    if let Some(first) = manifests.first() {
        for manifest in &manifests[1..] {
            if manifest.lineage_digest() != first.lineage_digest() {
                return Err(StoreError::LineageMismatch {
                    expected: first.lineage_digest(),
                    found: manifest.lineage_digest(),
                });
            }
        }
    }
    Ok(())
}

fn require_lineage_bytes(expected: LineageDigest, bytes: &[u8]) -> Result<(), StoreError> {
    let found_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| StoreError::MalformedPath(LINEAGE_CLAIM_FILE.into()))?;
    let found = LineageDigest::from_bytes(found_bytes);
    if found != expected {
        return Err(StoreError::LineageMismatch { expected, found });
    }
    Ok(())
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Batch(BatchError),
    Bootstrap(String),
    UnsafeEntry(String),
    MalformedPath(String),
    WorkspaceMismatch {
        expected: WorkspaceId,
        found: WorkspaceId,
    },
    LineageMismatch {
        expected: LineageDigest,
        found: LineageDigest,
    },
    ObjectCollision(ContentDigest),
    BatchCollision(BatchId),
    ObjectPathMismatch(ContentDigest),
    ManifestPathMismatch {
        expected: BatchId,
        found: BatchId,
    },
    AcceptedManifestMismatch {
        batch_id: BatchId,
        expected: ContentDigest,
        actual: ContentDigest,
    },
    AcceptedDocumentUpdateMissing {
        batch_id: BatchId,
        document_id: super::DocumentId,
    },
    HistoryIndexCollision(BatchId),
    HistoryIndexPathMismatch(ContentDigest),
    MalformedHistoryIndex,
    UpgradeRequired {
        store: &'static str,
        found: u32,
        current: u32,
    },
    UnsupportedStoreVersion {
        store: &'static str,
        version: u32,
    },
    BlockClaimIndexPathMismatch(ContentDigest),
    MalformedBlockClaimIndex,
    MissingLogseqClaimIndexNode(ContentDigest),
    LogseqClaimIndexPathMismatch(ContentDigest),
    MalformedLogseqClaimIndex,
    MissingExactLogicalPageNameBlob(ContentDigest),
    ExactLogicalPageNameBlobPathMismatch(ContentDigest),
    MalformedPageNameIndex,
    PageNamePointBatchTooLarge {
        actual: usize,
        limit: usize,
    },
    NonCanonicalPageNamePointKeys,
    MissingPageNameCatalogFrontier,
    MisboundPageNameCatalogFrontier,
    Scratch(String),
    LineageClaimCollision(LineageDigest),
    ImmutableCollision(&'static str),
    BootstrapArtifactCollision {
        kind: &'static str,
        identity: String,
    },
    BootstrapArtifactMismatch(&'static str),
    MissingBootstrapArtifact(&'static str),
    BootstrapBatchRequiresDirectPublication,
    BootstrapHistoryRequiresEmptyAuthority,
    InactiveBootstrapHistory,
    PromotedRuntimeStateAbsent,
    PromotedRuntimeStateMismatch(&'static str),
    MalformedPromotedRuntimeState,
    UnsupportedPromotedRuntimeSchema(u32),
    CompetingRuntimePromotion,
    /// One resume point, or one complete resume-point scan, was refused. Every
    /// shape means the same thing to a caller: do not adopt, do not prune, do
    /// not reclaim, preserve every candidate retained run.
    ResumePoint(String),
    ResumePointBindingMismatch(&'static str),
    ResumePointSequenceRegression {
        expected: u64,
        found: u64,
    },
    /// An adopted retained run authenticated as a real retained run of this
    /// workspace, but is not the exact run the caller named: its canonical
    /// marker digest differs, which is what a re-created run reusing the same
    /// UUID looks like. Nothing was changed; the caller must replay instead.
    RetainedScratchBindingMismatch,
    StoredLengthMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    StoredFileTooLarge {
        path: String,
        length: u64,
        limit: u64,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Batch(error) => error.fmt(f),
            Self::Bootstrap(error) => error.fmt(f),
            Self::UnsafeEntry(message) => write!(f, "unsafe store entry: {message}"),
            Self::MalformedPath(path) => write!(f, "malformed store path: {path}"),
            Self::WorkspaceMismatch { expected, found } => {
                write!(f, "workspace mismatch: expected {expected}, found {found}")
            }
            Self::LineageMismatch { expected, found } => {
                write!(f, "lineage mismatch: expected {expected}, found {found}")
            }
            Self::ObjectCollision(digest) => write!(f, "content-address collision at {digest}"),
            Self::BatchCollision(batch_id) => {
                write!(f, "fatal manifest collision for batch {batch_id}")
            }
            Self::ObjectPathMismatch(digest) => {
                write!(f, "stored object bytes do not match path {digest}")
            }
            Self::ManifestPathMismatch { expected, found } => write!(
                f,
                "manifest path names batch {expected}, but bytes name {found}"
            ),
            Self::AcceptedManifestMismatch {
                batch_id,
                expected,
                actual,
            } => write!(
                f,
                "accepted manifest {batch_id} fingerprint mismatch: expected {expected}, found {actual}"
            ),
            Self::AcceptedDocumentUpdateMissing {
                batch_id,
                document_id,
            } => write!(
                f,
                "accepted manifest {batch_id} has no CRDT update for document {document_id}"
            ),
            Self::HistoryIndexCollision(batch_id) => {
                write!(
                    f,
                    "authenticated history index collision for batch {batch_id}"
                )
            }
            Self::HistoryIndexPathMismatch(digest) => {
                write!(
                    f,
                    "authenticated history index bytes do not match path {digest}"
                )
            }
            Self::MalformedHistoryIndex => {
                f.write_str("authenticated history index is malformed or non-canonical")
            }
            Self::UpgradeRequired {
                store,
                found,
                current,
            } => write!(f, "{store} version {found} requires upgrade to {current}"),
            Self::UnsupportedStoreVersion { store, version } => {
                write!(f, "{store} version {version} is unsupported")
            }
            Self::BlockClaimIndexPathMismatch(digest) => write!(
                f,
                "authenticated block-claim index bytes do not match page {digest}"
            ),
            Self::MalformedBlockClaimIndex => {
                f.write_str("authenticated block-claim index is malformed or non-canonical")
            }
            Self::MissingLogseqClaimIndexNode(digest) => {
                write!(
                    f,
                    "authenticated Logseq claim index node {digest} is missing"
                )
            }
            Self::LogseqClaimIndexPathMismatch(digest) => write!(
                f,
                "authenticated Logseq claim index bytes do not match path {digest}"
            ),
            Self::MalformedLogseqClaimIndex => {
                f.write_str("authenticated Logseq claim index is malformed or non-canonical")
            }
            Self::MissingExactLogicalPageNameBlob(digest) => {
                write!(f, "exact logical page-name blob {digest} is missing")
            }
            Self::ExactLogicalPageNameBlobPathMismatch(digest) => {
                write!(
                    f,
                    "exact logical page-name blob bytes do not match path {digest}"
                )
            }
            Self::MalformedPageNameIndex => {
                f.write_str("authenticated page-name ownership index is malformed or non-canonical")
            }
            Self::PageNamePointBatchTooLarge { actual, limit } => write!(
                f,
                "page-name point batch has {actual} entries, exceeding {limit}"
            ),
            Self::NonCanonicalPageNamePointKeys => {
                f.write_str("page-name point keys are not strictly sorted and unique")
            }
            Self::MissingPageNameCatalogFrontier => {
                f.write_str("exact page-name catalog-frontier binding is missing")
            }
            Self::MisboundPageNameCatalogFrontier => {
                f.write_str("exact page-name catalog-frontier binding is misbound")
            }
            Self::Scratch(error) => write!(f, "engine scratch failed: {error}"),
            Self::LineageClaimCollision(lineage) => {
                write!(f, "immutable lineage claim collision for {lineage}")
            }
            Self::ImmutableCollision(kind) => {
                write!(f, "immutable {kind} collision")
            }
            Self::BootstrapArtifactCollision { kind, identity } => {
                write!(f, "immutable bootstrap {kind} collision at {identity}")
            }
            Self::BootstrapArtifactMismatch(kind) => {
                write!(f, "bootstrap {kind} does not match its direct authority")
            }
            Self::MissingBootstrapArtifact(kind) => {
                write!(f, "required bootstrap {kind} is missing")
            }
            Self::BootstrapBatchRequiresDirectPublication => {
                f.write_str("bootstrap batches require bootstrap-specific direct publication")
            }
            Self::BootstrapHistoryRequiresEmptyAuthority => {
                f.write_str("bootstrap history installation requires empty durable authority")
            }
            Self::InactiveBootstrapHistory => {
                f.write_str("inactive bootstrap history cannot be opened as ordinary runtime")
            }
            Self::PromotedRuntimeStateAbsent => {
                f.write_str("no durable promoted runtime state authorizes this archive")
            }
            Self::PromotedRuntimeStateMismatch(detail) => {
                write!(f, "promoted runtime state mismatch: {detail}")
            }
            Self::MalformedPromotedRuntimeState => {
                f.write_str("promoted runtime state is malformed, truncated, or non-canonical")
            }
            Self::UnsupportedPromotedRuntimeSchema(version) => write!(
                f,
                "unsupported promoted runtime state schema version {version}"
            ),
            Self::CompetingRuntimePromotion => f.write_str(
                "a different promoted runtime state is already committed for this archive",
            ),
            Self::ResumePoint(error) => write!(f, "{error}"),
            Self::ResumePointBindingMismatch(reason) => {
                write!(f, "runtime resume-point binding mismatch: {reason}")
            }
            Self::ResumePointSequenceRegression { expected, found } => write!(
                f,
                "runtime resume point {found} does not extend the published sequence {expected}"
            ),
            Self::RetainedScratchBindingMismatch => f.write_str(
                "retained scratch run does not carry the named canonical marker binding",
            ),
            Self::StoredLengthMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "stored file length mismatch at {path}: expected {expected}, found {actual}"
            ),
            Self::StoredFileTooLarge {
                path,
                length,
                limit,
            } => write!(
                f,
                "stored file at {path} is {length} bytes, exceeding limit {limit}"
            ),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Batch(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<BatchError> for StoreError {
    fn from(error: BatchError) -> Self {
        Self::Batch(error)
    }
}

impl From<ResumePointError> for StoreError {
    fn from(error: ResumePointError) -> Self {
        Self::ResumePoint(error.to_string())
    }
}

fn validate_engine_history_claim(
    bytes: &[u8],
    workspace_id: WorkspaceId,
    endpoint_id: super::ProjectionEndpointId,
    graph_resource_id: super::CanonicalGraphResourceId,
    receipt_store_id: super::ProjectionReceiptStoreId,
) -> Result<(), StoreError> {
    type CurrentClaim = (
        u32,
        WorkspaceId,
        super::ProjectionEndpointId,
        super::CanonicalGraphResourceId,
        super::ProjectionReceiptStoreId,
    );
    if let Ok(claim) = postcard::from_bytes::<CurrentClaim>(bytes) {
        if postcard::to_allocvec(&claim).ok().as_deref() != Some(bytes) {
            return Err(StoreError::MalformedHistoryIndex);
        }
        if claim.0 < ENGINE_HISTORY_ROOT_SCHEMA_VERSION {
            return Err(StoreError::UpgradeRequired {
                store: "engine history",
                found: claim.0,
                current: ENGINE_HISTORY_ROOT_SCHEMA_VERSION,
            });
        }
        if claim.0 > ENGINE_HISTORY_ROOT_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedStoreVersion {
                store: "engine history",
                version: claim.0,
            });
        }
        if claim.1 != workspace_id
            || claim.2 != endpoint_id
            || claim.3 != graph_resource_id
            || claim.4 != receipt_store_id
        {
            return Err(StoreError::MalformedHistoryIndex);
        }
        return Ok(());
    }
    type PriorClaim = (
        u32,
        WorkspaceId,
        super::ProjectionEndpointId,
        super::CanonicalGraphResourceId,
    );
    if let Ok(claim) = postcard::from_bytes::<PriorClaim>(bytes) {
        if postcard::to_allocvec(&claim).ok().as_deref() == Some(bytes)
            && claim.0 == ENGINE_HISTORY_ROOT_SCHEMA_VERSION - 1
        {
            return Err(StoreError::UpgradeRequired {
                store: "engine history",
                found: claim.0,
                current: ENGINE_HISTORY_ROOT_SCHEMA_VERSION,
            });
        }
    }
    Err(StoreError::MalformedHistoryIndex)
}

pub(crate) fn open_existing_dir_nofollow(
    root: &Dir,
    name: &str,
) -> Result<Option<Dir>, StoreError> {
    tine_storage::open_existing_dir_nofollow(root, name).map_err(filesystem_error_without_collision)
}

#[cfg(unix)]
pub fn control_directory_identity(dir: &Dir) -> Result<ControlDirectoryIdentity, StoreError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = dir.try_clone()?.into_std_file().metadata()?;
    Ok(ControlDirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
pub fn control_directory_identity(dir: &Dir) -> Result<ControlDirectoryIdentity, StoreError> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let file = dir.try_clone()?.into_std_file();
    let mut information = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(StoreError::Io(std::io::Error::last_os_error()));
    }
    Ok(ControlDirectoryIdentity {
        volume: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
pub fn control_directory_identity(_dir: &Dir) -> Result<ControlDirectoryIdentity, StoreError> {
    Err(StoreError::Io(std::io::Error::new(
        ErrorKind::Unsupported,
        "directory identity is unavailable on this platform",
    )))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrivateDirectoryDurability {
    StrictAuthority,
    Reconstructible,
}

fn ensure_directory_nofollow_with_durability(
    root: &Dir,
    name: &str,
    durability: PrivateDirectoryDurability,
) -> Result<(), StoreError> {
    #[cfg(target_os = "android")]
    {
        let component = Path::new(name);
        if !matches!(component.components().next(), Some(Component::Normal(_)))
            || component.components().count() != 1
        {
            return Err(StoreError::UnsafeEntry(format!(
                "managed private directory name is not one normal component: {name}"
            )));
        }
        match root.symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(StoreError::UnsafeEntry(format!(
                    "managed private directory is not a real no-follow directory: {name}"
                )));
            }
            Ok(_) => {
                if durability == PrivateDirectoryDurability::StrictAuthority {
                    sync_dir_required(root)?;
                }
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::NotFound => match root.create_dir(component) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(StoreError::Io(error)),
            },
            Err(error) => return Err(StoreError::Io(error)),
        }
        let metadata = root.symlink_metadata(component)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::UnsafeEntry(format!(
                "managed private directory is not a real no-follow directory: {name}"
            )));
        }
        match durability {
            PrivateDirectoryDurability::StrictAuthority => sync_dir_required(root)?,
            PrivateDirectoryDurability::Reconstructible => {
                crate::filesystem_durability::sync_reconstructible_directory(root)
                    .map_err(StoreError::Io)?;
            }
        }
        return Ok(());
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = durability;
        tine_storage::ensure_directory_nofollow(root, name)
            .map_err(filesystem_error_without_collision)
    }
}

/// Create or reopen a directory whose descendants can carry private durable
/// authority. Android must prove the parent namespace durable even when the
/// exact directory already exists after an earlier refused barrier or restart.
pub(crate) fn ensure_directory_nofollow(root: &Dir, name: &str) -> Result<(), StoreError> {
    ensure_directory_nofollow_with_durability(
        root,
        name,
        PrivateDirectoryDurability::StrictAuthority,
    )
}

/// Create or reopen a directory whose complete contents are disposable or
/// reconstructible from accepted authority elsewhere.
pub(crate) fn ensure_reconstructible_directory_nofollow(
    root: &Dir,
    name: &str,
) -> Result<(), StoreError> {
    ensure_directory_nofollow_with_durability(
        root,
        name,
        PrivateDirectoryDurability::Reconstructible,
    )
}

/// Create only the immediate parent of an explicitly bound object-store root.
/// The grandparent must already exist; the final parent component is opened
/// no-follow and its creation is durability-synced before store construction.
pub(crate) fn prepare_object_store_parent_nofollow(root: &Path) -> Result<(), StoreError> {
    let parent = root
        .parent()
        .ok_or_else(|| StoreError::UnsafeEntry("store root has no parent".into()))?;
    let name = parent
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StoreError::UnsafeEntry("store parent is not UTF-8".into()))?;
    if !matches!(parent.components().next_back(), Some(Component::Normal(_))) {
        return Err(StoreError::UnsafeEntry(
            "store parent must end in a normal path component".into(),
        ));
    }
    let grandparent = parent
        .parent()
        .ok_or_else(|| StoreError::UnsafeEntry("store parent has no grandparent".into()))?;
    let canonical_grandparent = fs::canonicalize(grandparent)?;
    let grandparent = Dir::open_ambient_dir(&canonical_grandparent, ambient_authority())?;
    ensure_directory_nofollow(&grandparent, name)
}

fn ensure_directory(root: &Dir, name: &str) -> Result<(), StoreError> {
    ensure_directory_nofollow(root, name)
}

fn publish_immutable(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    collision: Collision,
) -> Result<(), StoreError> {
    // ObjectStore is rooted in the app-private archive and is only mutated
    // while the managed runtime owns its sole-writer lease. This is distinct
    // from shared/provider publication, which must retain strict no-replace
    // behavior across processes.
    if publication_trace_enabled() {
        eprintln!("PUBLISH kind={}", collision.artifact_class());
    }
    crate::durability_counters::note_immutable_publication();
    tine_storage::publish_immutable_exact_single_writer(dir, filename, bytes)
        .map_err(|error| publication_error(error, collision))
}

/// One accepted batch's archive artifacts, published under either the strict
/// or journal-turn-covered protocol.
///
/// ## Why this exists
///
/// The per-artifact publisher (`tine_storage::publish_immutable_exact*`) pays
/// two device round trips — the file's `fsync` and its directory's `fsync` —
/// for every artifact. An ordinary one-block managed edit produces four to
/// eight archive artifacts, a cross-page move roughly eight, so artifact-local
/// durability alone cost ten to sixteen barriers per accepted edit
/// (2026-08-26 managed-storage cost-model audit, defect D1).
///
/// ## The protocol, and why each step is where it is
///
/// 1. **Stage.** Every artifact is written to a temporary name in its own
///    namespace with no barrier. Temporary names are invisible to every
///    reader: `ObjectStore::validate_namespace` and every replay path address
///    artifacts by their content-addressed or batch-addressed final names.
/// 2. **Object install for a journal-covered turn.** Its object final names are
///    inserted no-replace while the exact journal frame remains undrained.
///    Strict callers skip this step.
/// 3. **One data barrier.** `syncfs` flushes every staged inode. Strict callers
///    take it before any final-name install; a journal-covered turn takes it
///    after object install and before manifest install.
/// 4. **Remaining installs.** Strict callers insert every final name; the
///    journal-covered turn inserts its now-durable manifest commit marker.
/// 5. **Directory barriers.** One `fsync` per distinct namespace makes the
///    name insertions durable — two for an ordinary batch (objects, batches).
///
/// For a strict caller, the data barrier before install ensures no visible name
/// can refer to unflushed bytes. For the managed-local drain, a crash during
/// step 2 may leave a torn object final name; cold open replaces it only when
/// an uncheckpointed local journal record supplies the exact canonical bytes.
/// After step 3 every object is durable, so publication may return and the
/// caller may eventually checkpoint without losing repair authority too early.
/// The manifest is never installed before its data barrier.
///
/// ## Crash points
///
/// | crash at | on disk | recovery |
/// |---|---|---|
/// | during staging | temporary names only, contents arbitrary | the batch is not accepted; the journal frame is undrained and the drain republishes. Temporaries are ignored by every reader. |
/// | after staging, before an install | temporary names only | as above |
/// | during journal-covered object installs, before the barrier | a prefix of possibly torn object names; no manifest name | cold open repairs only exact journal-covered object mismatches before validation |
/// | after the barrier, during remaining installs | every surviving final name has durable correct bytes | the drain republishes and verifies exact existing names |
/// | after installs, before a directory barrier | possibly no final name durable after reboot; any surviving name has durable correct bytes | the drain republishes |
/// | after the directory barriers | the whole batch durable | the drain proceeds to checkpoint |
///
/// In every row the accepted operation itself is unaffected: it became durable
/// in the local journal during the foreground save, before this runs, and its
/// journal frame is checkpointed only after this returns.
///
/// On platforms without `syncfs` (Windows, macOS), `stage` publishes each
/// artifact through the ordinary durable publisher and `commit` is inert. The
/// barrier count there is unchanged.
struct ArchiveBatchPublication {
    archive: Dir,
    namespaces: Vec<Dir>,
    strict_filesystem_barrier: bool,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    staged: Vec<StagedArchiveArtifact>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
struct StagedArchiveArtifact {
    namespace: usize,
    temp_name: String,
    final_name: String,
    limit: u64,
    requires_preinstall_flush: bool,
}

impl ArchiveBatchPublication {
    fn strict(archive: &Dir) -> Result<Self, StoreError> {
        Self::new(archive, true)
    }

    fn turn_covered(archive: &Dir) -> Result<Self, StoreError> {
        Self::new(archive, false)
    }

    fn new(archive: &Dir, strict_filesystem_barrier: bool) -> Result<Self, StoreError> {
        Ok(Self {
            archive: archive.try_clone()?,
            namespaces: Vec::new(),
            strict_filesystem_barrier,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            staged: Vec::new(),
        })
    }

    /// Retain one namespace capability for the batch and return its index.
    fn namespace(&mut self, dir: &Dir) -> Result<usize, StoreError> {
        self.namespaces.push(dir.try_clone()?);
        Ok(self.namespaces.len() - 1)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn stage(
        &mut self,
        namespace: usize,
        final_name: &str,
        bytes: &[u8],
        limit: u64,
        _collision: Collision,
        requires_preinstall_flush: bool,
    ) -> Result<(), StoreError> {
        let dir = &self.namespaces[namespace];
        let temp_name = format!(".tmp-{}", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut temp = dir.open_with(&temp_name, &options)?;
        if let Err(error) = temp.write_all(bytes) {
            drop(temp);
            let _ = dir.remove_file(&temp_name);
            return Err(error.into());
        }
        drop(temp);
        self.staged.push(StagedArchiveArtifact {
            namespace,
            temp_name,
            final_name: final_name.to_owned(),
            limit,
            requires_preinstall_flush,
        });
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn stage(
        &mut self,
        namespace: usize,
        final_name: &str,
        bytes: &[u8],
        _limit: u64,
        collision: Collision,
        _requires_preinstall_flush: bool,
    ) -> Result<(), StoreError> {
        publish_immutable(&self.namespaces[namespace], final_name, bytes, collision)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn commit(self) -> Result<(), StoreError> {
        // `Drop` removes the temporaries whether this succeeds or not, so an
        // abandoned batch cannot leak names into the archive namespaces.
        self.commit_staged()
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn commit_staged(&self) -> Result<(), StoreError> {
        if self.staged.is_empty() {
            return Ok(());
        }
        if self.strict_filesystem_barrier {
            self.barrier_staged_data()?;
            self.install_staged_where(|_| true)?;
        } else {
            // The undrained journal is exact recovery authority while object
            // names install. Flush the whole staged set only after those names
            // exist, then install the now-durable manifest commit marker. The
            // journal checkpoint cannot advance until this method returns, so
            // every crash before the barrier remains repairable and every crash
            // after it has durable object bytes.
            self.install_staged_where(|artifact| !artifact.requires_preinstall_flush)?;
            self.barrier_staged_data()?;
            self.install_staged_where(|artifact| artifact.requires_preinstall_flush)?;
        }
        let mut barriered: Vec<usize> = Vec::new();
        for artifact in &self.staged {
            if barriered.contains(&artifact.namespace) {
                continue;
            }
            barriered.push(artifact.namespace);
            sync_dir_required(&self.namespaces[artifact.namespace])?;
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn install_staged_where(
        &self,
        predicate: impl Fn(&StagedArchiveArtifact) -> bool,
    ) -> Result<(), StoreError> {
        for artifact in self.staged.iter().filter(|artifact| predicate(artifact)) {
            install_staged_artifact(
                &self.namespaces[artifact.namespace],
                &artifact.temp_name,
                &artifact.final_name,
                artifact.limit,
            )?;
            archive_install_cut_hook()?;
        }
        Ok(())
    }

    /// Flush every staged inode. Strict callers take this before all installs;
    /// a journal-covered caller takes it after object installs but before the
    /// manifest install and before the journal checkpoint can advance.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn barrier_staged_data(&self) -> Result<(), StoreError> {
        let result = crate::filesystem_durability::sync_filesystem_containing(&self.archive);
        #[cfg(target_os = "android")]
        {
            return self.finish_android_staged_data_barrier(result);
        }
        #[cfg(not(target_os = "android"))]
        {
            result.map_err(Into::into)
        }
    }

    /// Android app-private storage can deny `syncfs` while still supporting
    /// exact file durability. Keep the branch host-executable so its selection
    /// and exact staged-file fallback are unit-tested rather than merely
    /// cross-compiled.
    #[cfg(any(target_os = "android", all(test, target_os = "linux")))]
    fn finish_android_staged_data_barrier(&self, result: io::Result<()>) -> Result<(), StoreError> {
        match result {
            Ok(()) => Ok(()),
            // Android app-private storage on some vendor filesystems denies the
            // filesystem-wide flush while permitting per-file flush. Falling
            // back to one `fsync` per staged artifact costs the barriers this
            // batch exists to avoid, but it is a correct durability point and
            // it keeps managed storage available on those devices. Every other
            // errno is a real I/O failure and stays fatal.
            Err(error) if crate::filesystem_durability::is_capability_refusal(&error) => {
                for artifact in &self.staged {
                    let name = match self.namespaces[artifact.namespace]
                        .symlink_metadata(&artifact.temp_name)
                    {
                        Ok(_) => artifact.temp_name.as_str(),
                        Err(error) if error.kind() == ErrorKind::NotFound => {
                            artifact.final_name.as_str()
                        }
                        Err(error) => return Err(error.into()),
                    };
                    let file = tine_storage::open_file_nofollow(
                        &self.namespaces[artifact.namespace],
                        name,
                    )?;
                    crate::durability_counters::sync_file(&file)?;
                }
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn commit(self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Drop for ArchiveBatchPublication {
    /// Remove every temporary this batch created, on the committed and the
    /// abandoned path alike. A batch abandoned mid-stage (a validation error,
    /// or the deterministic cut a crash test arms) must not leave temporaries
    /// behind: they are invisible to readers, but they are still bytes in the
    /// user's archive.
    fn drop(&mut self) {
        for artifact in &self.staged {
            let _ = self.namespaces[artifact.namespace].remove_file(&artifact.temp_name);
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod archive_batch_android_fallback_tests {
    use super::*;
    use crate::durability_counters::{Barrier, BarrierSession};

    #[test]
    fn android_group_commit_capability_refusal_flushes_every_exact_staged_file() {
        let root =
            std::env::temp_dir().join(format!("tine-android-archive-fallback-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("objects")).unwrap();
        let archive = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        let objects = open_dir_nofollow(&archive, "objects").unwrap();
        let mut publication = ArchiveBatchPublication::strict(&archive).unwrap();
        let namespace = publication.namespace(&objects).unwrap();
        publication
            .stage(
                namespace,
                "first",
                b"first exact staged object",
                1024,
                Collision::Exact("test object"),
                false,
            )
            .unwrap();
        publication
            .stage(
                namespace,
                "second",
                b"second exact staged object",
                1024,
                Collision::Exact("test object"),
                false,
            )
            .unwrap();

        let barriers = BarrierSession::begin();
        publication
            .finish_android_staged_data_barrier(Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "simulated Android syncfs capability refusal",
            )))
            .unwrap();
        assert_eq!(barriers.counts().get(Barrier::File), 2);
        BarrierSession::detach_current_thread();

        assert!(objects.symlink_metadata("first").is_err());
        assert!(objects.symlink_metadata("second").is_err());
        drop(publication);
        drop(objects);
        drop(archive);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn android_group_commit_does_not_hide_a_real_io_failure() {
        let root =
            std::env::temp_dir().join(format!("tine-android-archive-real-io-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let archive = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        let publication = ArchiveBatchPublication::strict(&archive).unwrap();
        let error = publication
            .finish_android_staged_data_barrier(Err(io::Error::new(
                ErrorKind::WriteZero,
                "simulated media failure",
            )))
            .unwrap_err();
        assert!(matches!(error, StoreError::Io(error) if error.kind() == ErrorKind::WriteZero));
        drop(publication);
        drop(archive);
        crate::test_support::remove_dir_all(root);
    }
}

/// Insert one already-durable staged artifact under its final immutable name.
///
/// No-replace, because an immutable name is authored once. A name that is
/// already present is not an error: the drain republishes the byte-identical
/// batch after any crash, so "already there with exactly these bytes" is the
/// expected outcome of resuming. Anything else is a genuine collision.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn install_staged_artifact(
    dir: &Dir,
    temp_name: &str,
    final_name: &str,
    limit: u64,
) -> Result<(), StoreError> {
    match dir.hard_link(temp_name, dir, final_name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let staged = tine_storage::read_required_regular(dir, temp_name, limit, None)
                .map_err(filesystem_error_without_collision)?;
            let existing = tine_storage::read_required_regular(dir, final_name, limit, None)
                .map_err(filesystem_error_without_collision)?;
            if existing == staged {
                Ok(())
            } else {
                Err(StoreError::ImmutableCollision("archive batch publication"))
            }
        }
        // Some Android app-private filesystems permit atomic same-directory
        // renames but deny the hard-link primitive. The archive is a
        // single-writer namespace, so proving the name absent and renaming is
        // equivalent there. This mirrors
        // `tine_storage::publish_immutable_exact_single_writer`.
        #[cfg(target_os = "android")]
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::Unsupported | ErrorKind::InvalidInput
            ) =>
        {
            match dir.symlink_metadata(final_name) {
                Ok(_) => {
                    let staged = tine_storage::read_required_regular(dir, temp_name, limit, None)
                        .map_err(filesystem_error_without_collision)?;
                    let existing =
                        tine_storage::read_required_regular(dir, final_name, limit, None)
                            .map_err(filesystem_error_without_collision)?;
                    if existing == staged {
                        Ok(())
                    } else {
                        Err(StoreError::ImmutableCollision("archive batch publication"))
                    }
                }
                Err(absent) if absent.kind() == ErrorKind::NotFound => {
                    dir.rename(temp_name, dir, final_name)?;
                    Ok(())
                }
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn publication_stage_error(stage: &'static str, error: StoreError) -> StoreError {
    match error {
        StoreError::Io(error) => StoreError::Io(std::io::Error::new(
            error.kind(),
            format!("{stage}: {error}"),
        )),
        error => error,
    }
}

pub(crate) fn publish_immutable_exact(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    kind: &'static str,
) -> Result<(), StoreError> {
    publish_immutable(dir, filename, bytes, Collision::Exact(kind))
}

fn publication_error(error: tine_storage::FilesystemError, collision: Collision) -> StoreError {
    match error {
        tine_storage::FilesystemError::ByteCollision => collision_error(collision),
        error => filesystem_error_without_collision(error),
    }
}

pub(crate) fn filesystem_error_without_collision(
    error: tine_storage::FilesystemError,
) -> StoreError {
    match error {
        tine_storage::FilesystemError::Io(error) => StoreError::Io(error),
        tine_storage::FilesystemError::DurableNameOperationUnavailable(message) => {
            StoreError::Io(std::io::Error::new(ErrorKind::Unsupported, message))
        }
        tine_storage::FilesystemError::UnsafeEntry(message) => StoreError::UnsafeEntry(message),
        tine_storage::FilesystemError::StoredLengthMismatch {
            path,
            expected,
            actual,
        } => StoreError::StoredLengthMismatch {
            path,
            expected,
            actual,
        },
        tine_storage::FilesystemError::StoredFileTooLarge {
            path,
            length,
            limit,
        } => StoreError::StoredFileTooLarge {
            path,
            length,
            limit,
        },
        tine_storage::FilesystemError::ByteCollision => {
            StoreError::ImmutableCollision("immutable publication")
        }
    }
}

fn collision_error(collision: Collision) -> StoreError {
    match collision {
        Collision::Object(digest) => StoreError::ObjectCollision(digest),
        Collision::Batch(batch_id) => StoreError::BatchCollision(batch_id),
        Collision::HistoryIndex(digest) => StoreError::HistoryIndexPathMismatch(digest),
        Collision::Lineage(lineage) => StoreError::LineageClaimCollision(lineage),
        Collision::Exact(kind) => StoreError::ImmutableCollision(kind),
        Collision::Bootstrap(kind, identity) => {
            StoreError::BootstrapArtifactCollision { kind, identity }
        }
    }
}

fn publish_bootstrap_immutable(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    kind: &'static str,
    identity: String,
) -> Result<(), StoreError> {
    publish_immutable(dir, filename, bytes, Collision::Bootstrap(kind, identity))
}

fn bootstrap_page_filename(ordinal: u32) -> String {
    ordinal.to_string()
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn read_regular_file_nofollow(dir: &Dir, name: &str) -> Result<fs::File, StoreError> {
    let file = open_file_nofollow(dir, name)?;
    if !file.metadata()?.is_file() {
        return Err(StoreError::UnsafeEntry(format!(
            "{name} is not a regular no-follow file"
        )));
    }
    Ok(file)
}

struct AdvisoryTransitionGuard<'a>(&'a fs::File);

impl<'a> AdvisoryTransitionGuard<'a> {
    fn lock(file: &'a fs::File) -> Result<Self, StoreError> {
        #[cfg(test)]
        {
            let contention_hook =
                ADVISORY_TRANSITION_CONTENTION_HOOK.with(|slot| slot.borrow_mut().take());
            if let Some(contention_hook) = contention_hook {
                match fs2::FileExt::try_lock_exclusive(file) {
                    Ok(()) => return Ok(Self(file)),
                    Err(error) if tine_storage::nonblocking_lock_is_contended(&error) => {
                        contention_hook()
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        fs2::FileExt::lock_exclusive(file)?;
        Ok(Self(file))
    }
}

impl Drop for AdvisoryTransitionGuard<'_> {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(self.0);
    }
}

#[cfg(unix)]
fn open_engine_history_transition_lock(root: &Dir) -> Result<fs::File, StoreError> {
    let name = CString::new(ENGINE_HISTORY_TRANSITION_LOCK_FILE).map_err(|_| {
        std::io::Error::new(ErrorKind::InvalidInput, "invalid transition lock name")
    })?;
    // SAFETY: the name is live and relative to the retained workspace
    // capability. O_NOFOLLOW rejects a final-component symlink atomically.
    let fd = unsafe {
        libc::openat(
            root.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned one newly owned descriptor.
    let file = unsafe { fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(StoreError::UnsafeEntry(
            "engine history transition lock is not a regular no-follow file".into(),
        ));
    }
    crate::durability_counters::sync_file(&file)?;
    sync_dir_required(root)?;
    Ok(file)
}

#[cfg(windows)]
fn open_engine_history_transition_lock(root: &Dir) -> Result<fs::File, StoreError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .follow(FollowSymlinks::No);
    let file = root
        .open_with(ENGINE_HISTORY_TRANSITION_LOCK_FILE, &options)?
        .into_std();
    let metadata = file.metadata()?;
    if metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
        || !metadata.is_file()
    {
        return Err(StoreError::UnsafeEntry(
            "engine history transition lock is not a regular no-follow file".into(),
        ));
    }
    crate::durability_counters::sync_file(&file)?;
    sync_dir_required(root)?;
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_engine_history_transition_lock(_root: &Dir) -> Result<fs::File, StoreError> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "workspace advisory transition locks are unsupported on this target",
    )
    .into())
}

pub(crate) fn open_file_nofollow(dir: &Dir, path: &str) -> std::io::Result<fs::File> {
    tine_storage::open_file_nofollow(dir, path)
}

pub(crate) fn open_dir_nofollow(dir: &Dir, path: &str) -> Result<Dir, StoreError> {
    tine_storage::open_dir_nofollow(dir, path).map_err(filesystem_error_without_collision)
    // SAFETY: `openat` returned a newly owned directory descriptor.
}

pub(crate) fn read_optional_regular(
    dir: &Dir,
    path: &str,
    limit: u64,
    expected_length: Option<u64>,
) -> Result<Option<Vec<u8>>, StoreError> {
    tine_storage::read_optional_regular(dir, path, limit, expected_length)
        .map_err(filesystem_error_without_collision)
}

fn read_required_regular(
    dir: &Dir,
    path: &str,
    limit: u64,
    expected_length: Option<u64>,
) -> Result<Vec<u8>, StoreError> {
    tine_storage::read_required_regular(dir, path, limit, expected_length)
        .map_err(filesystem_error_without_collision)
}

fn object_filename(digest: ContentDigest) -> String {
    format!("{digest}.object")
}

fn manifest_filename(batch_id: BatchId) -> String {
    format!("{batch_id}.manifest")
}

fn history_filename(batch_id: BatchId) -> String {
    format!("{batch_id}.status")
}

fn history_index_filename(digest: ContentDigest) -> String {
    format!("{digest}.index")
}

fn engine_history_root_filename(digest: ContentDigest) -> String {
    format!("{digest}{ENGINE_HISTORY_ROOT_SUFFIX}")
}

fn history_key_nibble(key: &[u8; 16], depth: u8) -> u8 {
    let byte = key[usize::from(depth / 2)];
    if depth.is_multiple_of(2) {
        byte >> 4
    } else {
        byte & 0x0f
    }
}

fn block_claim_key_nibble(key: &[u8; 16], depth: u8) -> u8 {
    let digest = ContentDigest::of(key);
    let byte = digest.as_bytes()[usize::from(depth / 2)];
    if depth.is_multiple_of(2) {
        byte >> 4
    } else {
        byte & 0x0f
    }
}

fn block_claim_page_depth(page: &BlockClaimIndexPage) -> u8 {
    match page {
        BlockClaimIndexPage::Branch { depth, .. } | BlockClaimIndexPage::Leaf { depth, .. } => {
            *depth
        }
    }
}

fn new_block_claim_filter(
    entries: &[([u8; 16], BlockClaimIndexValue)],
) -> Result<BlockClaimFilterPage, StoreError> {
    let bit_len = entries
        .len()
        .checked_mul(BLOCK_CLAIM_FILTER_BITS_PER_ENTRY)
        .ok_or(StoreError::MalformedBlockClaimIndex)?;
    let byte_len = bit_len
        .checked_add(7)
        .ok_or(StoreError::MalformedBlockClaimIndex)?
        / 8;
    let mut filter = BlockClaimFilterPage {
        schema_version: BLOCK_CLAIM_INDEX_SCHEMA_VERSION,
        entry_count: u64::try_from(entries.len())
            .map_err(|_| StoreError::MalformedBlockClaimIndex)?,
        bit_len: u64::try_from(bit_len).map_err(|_| StoreError::MalformedBlockClaimIndex)?,
        bits: vec![0; byte_len],
    };
    for (key, _) in entries {
        let (first, second) = block_claim_filter_hashes(key);
        for position in block_claim_filter_positions(first, second, filter.bit_len) {
            filter.bits[position as usize / 8] |= 1 << (position % 8);
        }
    }
    validate_block_claim_filter(&filter)?;
    Ok(filter)
}

fn new_block_claim_global_filter() -> BlockClaimGlobalFilterPage {
    BlockClaimGlobalFilterPage {
        schema_version: BLOCK_CLAIM_INDEX_SCHEMA_VERSION,
        insertions: 0,
        bits: vec![0; BLOCK_CLAIM_GLOBAL_FILTER_BYTES],
    }
}

fn update_block_claim_global_filter(
    filter: &mut BlockClaimGlobalFilterPage,
    records: &[([u8; 16], BlockClaimIndexValue)],
) -> Result<(), StoreError> {
    filter.insertions = filter
        .insertions
        .checked_add(
            u64::try_from(records.len()).map_err(|_| StoreError::MalformedBlockClaimIndex)?,
        )
        .ok_or(StoreError::MalformedBlockClaimIndex)?;
    let bit_len = u64::try_from(filter.bits.len())
        .ok()
        .and_then(|bytes| bytes.checked_mul(8))
        .ok_or(StoreError::MalformedBlockClaimIndex)?;
    for (key, _) in records {
        let (first, second) = block_claim_filter_hashes(key);
        for position in block_claim_filter_positions(first, second, bit_len) {
            filter.bits[position as usize / 8] |= 1 << (position % 8);
        }
    }
    Ok(())
}

fn validate_block_claim_global_filter(
    filter: &BlockClaimGlobalFilterPage,
) -> Result<(), StoreError> {
    if filter.schema_version != BLOCK_CLAIM_INDEX_SCHEMA_VERSION
        || filter.insertions == 0
        || filter.bits.len() != BLOCK_CLAIM_GLOBAL_FILTER_BYTES
    {
        return Err(StoreError::MalformedBlockClaimIndex);
    }
    Ok(())
}

fn block_claim_global_filter_might_contain(
    filter: &BlockClaimGlobalFilterPage,
    first: u64,
    second: u64,
) -> bool {
    let bit_len = (filter.bits.len() as u64) * 8;
    block_claim_filter_positions(first, second, bit_len)
        .into_iter()
        .all(|position| filter.bits[position as usize / 8] & (1 << (position % 8)) != 0)
}

fn validate_block_claim_filter(filter: &BlockClaimFilterPage) -> Result<(), StoreError> {
    let expected_bits = usize::try_from(filter.entry_count)
        .ok()
        .and_then(|entries| entries.checked_mul(BLOCK_CLAIM_FILTER_BITS_PER_ENTRY))
        .ok_or(StoreError::MalformedBlockClaimIndex)?;
    let expected_bytes = expected_bits
        .checked_add(7)
        .ok_or(StoreError::MalformedBlockClaimIndex)?
        / 8;
    if filter.schema_version != BLOCK_CLAIM_INDEX_SCHEMA_VERSION
        || filter.entry_count == 0
        || filter.bit_len != expected_bits as u64
        || filter.bits.len() != expected_bytes
    {
        return Err(StoreError::MalformedBlockClaimIndex);
    }
    let unused_bits = expected_bytes * 8 - expected_bits;
    if unused_bits != 0
        && filter.bits.last().is_some_and(|last| {
            let used_mask = u8::MAX >> unused_bits;
            *last & !used_mask != 0
        })
    {
        return Err(StoreError::MalformedBlockClaimIndex);
    }
    Ok(())
}

fn block_claim_filter_might_contain(
    filter: &BlockClaimFilterPage,
    first: u64,
    second: u64,
) -> bool {
    block_claim_filter_positions(first, second, filter.bit_len)
        .into_iter()
        .all(|position| filter.bits[position as usize / 8] & (1 << (position % 8)) != 0)
}

fn block_claim_filter_hashes(key: &[u8; 16]) -> (u64, u64) {
    let high = u64::from_be_bytes(key[..8].try_into().expect("fixed block key"));
    let low = u64::from_be_bytes(key[8..].try_into().expect("fixed block key"));
    let first = splitmix64(high ^ low.rotate_left(23));
    let second = splitmix64(low ^ high.rotate_right(17) ^ 0x9e37_79b9_7f4a_7c15) | 1;
    (first, second)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn block_claim_filter_positions(
    first: u64,
    second: u64,
    bit_len: u64,
) -> [u64; BLOCK_CLAIM_FILTER_HASHES as usize] {
    std::array::from_fn(|index| {
        first
            .wrapping_add((index as u64).wrapping_mul(second))
            .wrapping_rem(bit_len)
    })
}

fn validate_block_claim_page(page: &BlockClaimIndexPage) -> Result<(), StoreError> {
    match page {
        BlockClaimIndexPage::Branch {
            schema_version,
            depth,
            children,
        } => {
            if *schema_version != BLOCK_CLAIM_INDEX_SCHEMA_VERSION
                || *depth >= BLOCK_CLAIM_RADIX_DEPTH
                || children.is_empty()
                || children.iter().any(|(nibble, _)| *nibble >= 16)
                || !children.windows(2).all(|pair| pair[0].0 < pair[1].0)
            {
                return Err(StoreError::MalformedBlockClaimIndex);
            }
        }
        BlockClaimIndexPage::Leaf {
            schema_version,
            depth,
            entries,
        } => {
            if *schema_version != BLOCK_CLAIM_INDEX_SCHEMA_VERSION
                || *depth > BLOCK_CLAIM_RADIX_DEPTH
                || entries.is_empty()
                || (*depth < BLOCK_CLAIM_RADIX_DEPTH && entries.len() > BLOCK_CLAIM_LEAF_ENTRIES)
                || !entries.windows(2).all(|pair| pair[0].0 < pair[1].0)
                || entries.iter().any(|(_, record)| {
                    record.is_empty() || record.len() > MAX_BLOCK_CLAIM_RECORD_BYTES
                })
            {
                return Err(StoreError::MalformedBlockClaimIndex);
            }
        }
    }
    Ok(())
}

fn validate_history_node(node: &HistoryIndexNode) -> Result<(), StoreError> {
    match node {
        HistoryIndexNode::Branch {
            schema_version,
            depth,
            children,
        } => {
            if *schema_version != ENGINE_HISTORY_INDEX_SCHEMA_VERSION
                || *depth >= ENGINE_HISTORY_RADIX_DEPTH
                || children.is_empty()
                || children.iter().any(|(nibble, _)| *nibble >= 16)
                || !children.windows(2).all(|pair| pair[0].0 < pair[1].0)
            {
                return Err(StoreError::MalformedHistoryIndex);
            }
        }
        HistoryIndexNode::Leaf {
            schema_version,
            record,
            ..
        } => {
            if *schema_version != ENGINE_HISTORY_INDEX_SCHEMA_VERSION
                || record.is_empty()
                || record.len() as u64 > MAX_ENGINE_HISTORY_RECORD_BYTES
            {
                return Err(StoreError::MalformedHistoryIndex);
            }
        }
    }
    Ok(())
}

fn parse_object_filename(name: &str) -> Result<ContentDigest, StoreError> {
    let Some(digest) = name.strip_suffix(".object") else {
        return Err(StoreError::MalformedPath(name.into()));
    };
    if digest.len() != 64
        || digest
            .bytes()
            .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(StoreError::MalformedPath(name.into()));
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in digest.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0]).expect("validated hex") << 4)
            | hex_nibble(pair[1]).expect("validated hex");
    }
    Ok(ContentDigest::from_bytes(bytes))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn parse_manifest_filename(name: &str) -> Result<BatchId, StoreError> {
    let Some(batch_id) = name.strip_suffix(".manifest") else {
        return Err(StoreError::MalformedPath(name.into()));
    };
    let parsed = batch_id
        .parse::<BatchId>()
        .map_err(|_| StoreError::MalformedPath(name.into()))?;
    if parsed.to_string() != batch_id {
        return Err(StoreError::MalformedPath(name.into()));
    }
    Ok(parsed)
}

pub(crate) fn is_temp_name(name: &str) -> bool {
    name.strip_prefix(".tmp-")
        .and_then(|value| Uuid::parse_str(value).ok())
        .is_some()
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod history_index_tests {
    use super::*;

    fn test_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("tine-history-index-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn snapshot_tree(path: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut result = BTreeMap::new();
        let mut pending = vec![path.to_path_buf()];
        while let Some(entry_path) = pending.pop() {
            let relative = entry_path.strip_prefix(path).unwrap().to_path_buf();
            if entry_path.is_dir() {
                result.insert(relative, None);
                for entry in std::fs::read_dir(&entry_path).unwrap() {
                    pending.push(entry.unwrap().path());
                }
            } else {
                result.insert(relative, Some(std::fs::read(entry_path).unwrap()));
            }
        }
        result
    }

    fn snapshot_tree_with_identity(path: &Path) -> BTreeMap<PathBuf, (Vec<u8>, Option<Vec<u8>>)> {
        fn identity(path: &Path) -> Vec<u8> {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;

                let metadata = std::fs::symlink_metadata(path).unwrap();
                let mut identity = Vec::with_capacity(16);
                identity.extend_from_slice(&metadata.dev().to_be_bytes());
                identity.extend_from_slice(&metadata.ino().to_be_bytes());
                identity
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt as _;
                use windows_sys::Win32::Storage::FileSystem::{
                    FileIdInfo, GetFileInformationByHandleEx, FILE_FLAG_BACKUP_SEMANTICS,
                    FILE_ID_INFO,
                };

                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                    .open(path)
                    .unwrap();
                let mut information = FILE_ID_INFO::default();
                let result = unsafe {
                    GetFileInformationByHandleEx(
                        file.as_raw_handle(),
                        FileIdInfo,
                        (&mut information as *mut FILE_ID_INFO).cast(),
                        std::mem::size_of::<FILE_ID_INFO>() as u32,
                    )
                };
                assert_ne!(result, 0, "test filesystem identity");
                let mut identity = Vec::with_capacity(24);
                identity.extend_from_slice(&information.VolumeSerialNumber.to_be_bytes());
                identity.extend_from_slice(&information.FileId.Identifier);
                identity
            }
            #[cfg(not(any(unix, windows)))]
            {
                Vec::new()
            }
        }

        let mut result = BTreeMap::new();
        let mut pending = vec![path.to_path_buf()];
        while let Some(entry_path) = pending.pop() {
            let relative = entry_path.strip_prefix(path).unwrap().to_path_buf();
            if entry_path.is_dir() {
                result.insert(relative, (identity(&entry_path), None));
                for entry in std::fs::read_dir(&entry_path).unwrap() {
                    pending.push(entry.unwrap().path());
                }
            } else {
                result.insert(
                    relative,
                    (
                        identity(&entry_path),
                        Some(std::fs::read(&entry_path).unwrap()),
                    ),
                );
            }
        }
        result
    }

    fn enrolled_binding(endpoint: u128) -> crate::oplog::hot_engine::ProjectionStorageBinding {
        crate::oplog::hot_engine::ProjectionStorageBinding {
            endpoint: crate::oplog::ProjectionEndpointBinding {
                endpoint_id: crate::oplog::ProjectionEndpointId::from_uuid(Uuid::from_u128(
                    endpoint,
                )),
                device_id: crate::oplog::DeviceId::from_uuid(Uuid::from_u128(endpoint + 1)),
                graph_resource_id: crate::oplog::CanonicalGraphResourceId::from_capability_identity(
                    b"test",
                    &endpoint.to_be_bytes(),
                ),
            },
            receipt_store_id: crate::oplog::ProjectionReceiptStoreId::from_capability_identity(
                b"test",
                &(endpoint + 2).to_be_bytes(),
            ),
        }
    }

    #[test]
    fn sealed_history_baseline_survives_reads_until_an_anchored_transition() {
        let root = test_root("enrolled-head-rollback-subsequent-read");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(150));
        let binding = enrolled_binding(160);
        let store = ObjectStore::open(&archive, workspace).unwrap();
        let history = store.open_engine_history(binding).unwrap();
        let control = archive
            .join(ENGINE_HISTORY_DIR)
            .join(binding.endpoint.endpoint_id.to_string());
        let original = std::fs::read(control.join(ENGINE_HISTORY_HEAD_FILE)).unwrap();
        history
            .publish(
                BatchId::from_uuid(Uuid::from_u128(170)),
                b"accepted history",
                EngineHistoryBinding::empty(),
            )
            .unwrap();
        let accepted = std::fs::read(control.join(ENGINE_HISTORY_HEAD_FILE)).unwrap();
        drop(history);
        drop(store);

        let (_, history) = ObjectStore::open(&archive, workspace)
            .unwrap()
            .seal_history_only(binding)
            .unwrap()
            .into_history()
            .unwrap();
        assert_eq!(history.current().unwrap().0, 1);
        std::fs::write(control.join(ENGINE_HISTORY_HEAD_FILE), &original).unwrap();
        let attacked = snapshot_tree(&archive);
        assert!(
            history.current().is_err(),
            "rollback was accepted on reread"
        );
        assert_eq!(snapshot_tree(&archive), attacked);

        std::fs::write(control.join(ENGINE_HISTORY_HEAD_FILE), accepted).unwrap();
        assert_eq!(
            history.current().unwrap().0,
            1,
            "the sealed baseline was forgotten after a rejected rollback"
        );
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn authenticated_history_point_lookup_tamper_and_collision_fail_closed() {
        let root = test_root("integrity");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
        let store = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        let history = store.start_engine_history().unwrap();
        let batch_id = BatchId::from_uuid(Uuid::from_u128(2));
        let before_insert = store.instrumentation();
        let index_root = history
            .insert(EngineHistoryStore::empty_root(), batch_id, b"record")
            .unwrap();
        let after_insert = store.instrumentation();
        assert_eq!(
            after_insert.directory_enumerations - before_insert.directory_enumerations,
            0
        );
        assert_eq!(
            after_insert.history_index_reads - before_insert.history_index_reads,
            0
        );
        assert_eq!(
            after_insert.history_index_writes - before_insert.history_index_writes,
            33
        );

        let before = store.instrumentation();
        assert_eq!(
            history.lookup(index_root, batch_id).unwrap(),
            Some(b"record".to_vec())
        );
        let after = store.instrumentation();
        assert_eq!(
            after.directory_enumerations - before.directory_enumerations,
            0
        );
        assert!(after.history_index_reads - before.history_index_reads <= 33);
        assert_eq!(
            history
                .lookup(index_root, BatchId::from_uuid(Uuid::from_u128(3)))
                .unwrap(),
            None
        );

        let run = std::fs::read_dir(root.join("archive/engine-history"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let child_digest = match history.read_node(index_root).unwrap() {
            HistoryIndexNode::Branch { children, .. } => children[0].1,
            HistoryIndexNode::Leaf { .. } => panic!("radix root must be a branch"),
        };
        let child_path = run.join(history_index_filename(child_digest));
        let child_bytes = std::fs::read(&child_path).unwrap();
        let mut replaced_child = child_bytes.clone();
        let child_middle = replaced_child.len() / 2;
        replaced_child[child_middle] ^= 1;
        std::fs::write(&child_path, replaced_child).unwrap();
        assert!(matches!(
            history.lookup(index_root, batch_id),
            Err(StoreError::HistoryIndexPathMismatch(found)) if found == child_digest
        ));
        std::fs::write(&child_path, child_bytes).unwrap();

        let root_path = run.join(history_index_filename(index_root));
        let mut bytes = std::fs::read(&root_path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        std::fs::write(&root_path, bytes).unwrap();
        assert!(matches!(
            history.lookup(index_root, batch_id),
            Err(StoreError::HistoryIndexPathMismatch(_))
        ));

        let collision_batch = BatchId::from_uuid(Uuid::from_u128(4));
        let collision_node = HistoryIndexNode::Leaf {
            schema_version: ENGINE_HISTORY_INDEX_SCHEMA_VERSION,
            batch_id: collision_batch,
            record: b"collision".to_vec(),
        };
        let collision_bytes = postcard::to_allocvec(&collision_node).unwrap();
        let collision_digest = ContentDigest::of(&collision_bytes);
        std::fs::write(
            run.join(history_index_filename(collision_digest)),
            b"different immutable bytes",
        )
        .unwrap();
        assert!(matches!(
            history.publish_node(&collision_node),
            Err(StoreError::HistoryIndexPathMismatch(found)) if found == collision_digest
        ));
        drop(history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn durable_history_head_and_root_fail_closed() {
        let root = test_root("durable-root");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(5));
        let endpoint = crate::oplog::ProjectionEndpointId::from_uuid(Uuid::from_u128(7));
        let endpoint_binding = crate::oplog::ProjectionEndpointBinding {
            endpoint_id: endpoint,
            device_id: crate::oplog::DeviceId::from_uuid(Uuid::from_u128(8)),
            graph_resource_id: crate::oplog::CanonicalGraphResourceId::from_capability_identity(
                b"test",
                b"durable-root",
            ),
        };
        let store = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        let history = store
            .open_engine_history(crate::oplog::hot_engine::ProjectionStorageBinding {
                endpoint: endpoint_binding,
                receipt_store_id: crate::oplog::ProjectionReceiptStoreId::from_capability_identity(
                    b"test",
                    b"engine-history",
                ),
            })
            .unwrap();
        history
            .publish(
                BatchId::from_uuid(Uuid::from_u128(6)),
                b"bound durable record",
                EngineHistoryBinding::empty(),
            )
            .unwrap();

        let control = root
            .join("archive")
            .join(ENGINE_HISTORY_DIR)
            .join(endpoint.to_string());
        let head = std::fs::read_to_string(control.join(ENGINE_HISTORY_HEAD_FILE)).unwrap();
        let root_path = control
            .join(ENGINE_HISTORY_ROOTS_DIR)
            .join(format!("{head}{ENGINE_HISTORY_ROOT_SUFFIX}"));
        let original = std::fs::read(&root_path).unwrap();
        let mut tampered = original.clone();
        tampered[0] ^= 0x80;
        std::fs::write(&root_path, tampered).unwrap();
        assert!(matches!(
            history.current(),
            Err(StoreError::HistoryIndexPathMismatch(_))
        ));

        std::fs::write(&root_path, original).unwrap();
        std::fs::remove_file(control.join(ENGINE_HISTORY_HEAD_FILE)).unwrap();
        assert!(matches!(
            history.current(),
            Err(StoreError::MalformedHistoryIndex)
        ));
        drop(history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn authenticated_history_transition_accepts_only_exact_or_insertion_only_lineage() {
        let root = test_root("authenticated-transition-lineage");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(45_000));
        let binding = enrolled_binding(45_010);
        let store = ObjectStore::open(&archive, workspace).unwrap();
        let history = store.open_engine_history(binding).unwrap();
        let control = archive
            .join(ENGINE_HISTORY_DIR)
            .join(binding.endpoint.endpoint_id.to_string());
        let empty_head = std::fs::read(control.join(ENGINE_HISTORY_HEAD_FILE)).unwrap();
        let empty = history.current_authority().unwrap();

        let exact = history
            .authenticate_current_history_extension(empty)
            .unwrap();
        assert_eq!(exact.before(), empty);
        assert_eq!(exact.after(), empty);

        let first_batch = BatchId::from_uuid(Uuid::from_u128(45_020));
        let first_bytes = b"authenticated first record".to_vec();
        history
            .publish(first_batch, &first_bytes, EngineHistoryBinding::empty())
            .unwrap();
        let first = history.current_authority().unwrap();
        let extension = history
            .authenticate_current_history_extension(empty)
            .unwrap();
        assert_eq!(extension.before(), empty);
        assert_eq!(extension.after(), first);

        let second_batch = BatchId::from_uuid(Uuid::from_u128(45_021));
        let second_bytes = b"authenticated unrelated record".to_vec();
        history
            .publish(second_batch, &second_bytes, EngineHistoryBinding::empty())
            .unwrap();
        let forward = history.current_authority().unwrap();
        assert_eq!(
            history
                .authenticate_current_history_extension(first)
                .unwrap()
                .after(),
            forward
        );
        drop(history);
        drop(store);

        std::fs::write(control.join(ENGINE_HISTORY_HEAD_FILE), &empty_head).unwrap();
        let divergent_store = ObjectStore::open(&archive, workspace).unwrap();
        let divergent_history = divergent_store.open_engine_history(binding).unwrap();
        let divergent_batch = BatchId::from_uuid(Uuid::from_u128(45_030));
        divergent_history
            .publish(
                divergent_batch,
                b"equal-generation divergent record",
                EngineHistoryBinding::empty(),
            )
            .unwrap();
        assert_eq!(divergent_history.current_authority().unwrap().generation, 1);
        assert!(matches!(
            divergent_history.authenticate_current_history_extension(first),
            Err(StoreError::MalformedHistoryIndex)
        ));

        let divergent_later = BatchId::from_uuid(Uuid::from_u128(45_031));
        divergent_history
            .publish(
                divergent_later,
                b"higher-generation divergent record",
                EngineHistoryBinding::empty(),
            )
            .unwrap();
        assert_eq!(divergent_history.current_authority().unwrap().generation, 2);
        assert!(matches!(
            divergent_history.authenticate_current_history_extension(first),
            Err(StoreError::MalformedHistoryIndex)
        ));
        drop(divergent_history);
        drop(divergent_store);

        std::fs::write(control.join(ENGINE_HISTORY_HEAD_FILE), empty_head).unwrap();
        let rollback_store = ObjectStore::open(&archive, workspace).unwrap();
        let rollback_history = rollback_store.open_engine_history(binding).unwrap();
        assert!(matches!(
            rollback_history.authenticate_current_history_extension(first),
            Err(StoreError::MalformedHistoryIndex)
        ));

        drop(rollback_history);
        drop(rollback_store);
        crate::test_support::remove_dir_all(root);
    }

    /// Deterministic, well-spread batch identifiers. Multiplying by an odd
    /// constant is a bijection modulo 2^128, so every index yields a distinct
    /// key, and the high bits vary so the radix keys branch like real batch
    /// identifiers instead of sharing one long synthetic prefix.
    fn spread_history_batch_id(index: usize) -> BatchId {
        BatchId::from_uuid(Uuid::from_u128(
            0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835_u128.wrapping_mul(index as u128 + 1),
        ))
    }

    /// The constant live-endpoint revalidation every warm call performs: the
    /// two radix roots the direct walk itself would read, and never more.
    const LIVE_ENDPOINT_REVALIDATION_BOUND: usize = 2;

    /// One radix insertion touches `ENGINE_HISTORY_RADIX_DEPTH + 1` nodes. A
    /// residual `middle -> current` diff walk reads at most one such path on
    /// each side before it either terminates on an equal subtree or counts the
    /// single newly inserted record, so a memoized incremental step can never
    /// exceed twice one insertion path plus the constant live-endpoint
    /// revalidation — whatever the post-anchor history size.
    const INCREMENTAL_STEP_BOUND: usize =
        LIVE_ENDPOINT_REVALIDATION_BOUND + 2 * (ENGINE_HISTORY_RADIX_DEPTH as usize + 1);

    #[test]
    fn authenticated_history_extension_revalidation_is_bounded_per_step() {
        fn node_reads(store: &ObjectStore) -> usize {
            store.instrumentation().history_index_reads
        }

        // Post-anchor history sizes. Every assertion below is a statement about
        // *node reads*, and the memoized step is constant by construction, so
        // these only have to be large enough for each comparison to be decided
        // with real margin -- see the measured table on the closing assertion.
        // They used to be 1/1,000/10,000, which published 11,001 durable
        // records to decide those same comparisons and made this the slowest
        // test in the suite by an order of magnitude.
        let mut full_walks = Vec::new();
        for (run, size) in [1_usize, 64, 512].into_iter().enumerate() {
            let root = test_root(&format!("bounded-revalidation-{size}"));
            let archive = root.join("archive");
            let workspace = WorkspaceId::from_uuid(Uuid::from_u128(46_000 + run as u128));
            let binding = enrolled_binding(46_100 + run as u128 * 10);
            let store = ObjectStore::open(&archive, workspace).unwrap();
            let history = store.open_engine_history(binding).unwrap();
            let anchor = history.current_authority().unwrap();

            let mut bootstrap_step = 0_usize;
            let mut worst_incremental_step = 0_usize;
            for index in 0..size {
                history
                    .publish(
                        spread_history_batch_id(index),
                        b"bounded revalidation record",
                        EngineHistoryBinding::empty(),
                    )
                    .unwrap();
                let before = node_reads(&store);
                let proof = history
                    .authenticate_current_history_extension(anchor)
                    .unwrap();
                let step = node_reads(&store) - before;
                assert_eq!(proof.before(), anchor);
                assert_eq!(proof.after().generation, index as u64 + 1);
                if index == 0 {
                    bootstrap_step = step;
                } else {
                    worst_incremental_step = worst_incremental_step.max(step);
                    assert!(
                        step <= INCREMENTAL_STEP_BOUND,
                        "post-anchor record {index} of {size} revalidated with {step} node reads"
                    );
                }
            }

            // The very first proof from the immutable anchor is the one full
            // walk, and at post-anchor size 1 it is literally one radix
            // insertion path. The memo is cold, so it costs no revalidation.
            assert_eq!(bootstrap_step, ENGINE_HISTORY_RADIX_DEPTH as usize + 1);

            // Re-proving an unchanged head from the same anchor is composition
            // against an already-proved endpoint, so the residual walk is
            // empty. What it still costs — and must cost — is the live current
            // root, freshly read and authenticated. The anchor here is the
            // empty authority, which names no node.
            let before = node_reads(&store);
            let repeated = history
                .authenticate_current_history_extension(anchor)
                .unwrap();
            assert_eq!(node_reads(&store) - before, 1);
            assert_eq!(repeated.after().generation, size as u64);

            // A fresh open holds no memo and must pay the complete anchor ->
            // head walk, which visits every post-anchor record.
            drop(history);
            drop(store);
            let reopened_store = ObjectStore::open(&archive, workspace).unwrap();
            let reopened = reopened_store.open_engine_history(binding).unwrap();
            let before = node_reads(&reopened_store);
            let full = reopened
                .authenticate_current_history_extension(anchor)
                .unwrap();
            let full_walk = node_reads(&reopened_store) - before;
            assert_eq!(full.before(), anchor);
            assert_eq!(full.after().generation, size as u64);
            assert!(
                full_walk >= size,
                "a fresh full proof of {size} post-anchor records read only {full_walk} nodes"
            );
            let before = node_reads(&reopened_store);
            reopened
                .authenticate_current_history_extension(anchor)
                .unwrap();
            assert_eq!(node_reads(&reopened_store) - before, 1);

            if size >= 512 {
                assert!(
                    full_walk >= 100 * worst_incremental_step,
                    "full walk {full_walk} is not dominated by the {worst_incremental_step}-read \
                     incremental step at size {size}"
                );
            }
            full_walks.push(full_walk);

            drop(reopened);
            drop(reopened_store);
            crate::test_support::remove_dir_all(root);
        }

        // The unmemoized proof cost tracks the post-anchor history — which is
        // exactly the growth the memo removes from every step above.
        //
        // Measured here, one row per post-anchor size (worst memoized step /
        // full walk): 2 -> 35/65, 32 -> 36/1009, 64 -> 36/2001, 128 -> 36/3985,
        // 256 -> 37/7919, 512 -> 37/15633. The full walk is 31*size, the
        // memoized step is flat, and the separation is already two orders of
        // magnitude at 512. Growing the history further re-measures those same
        // two shapes at greater cost; it does not make either claim stronger.
        // Detection is immediate rather than asymptotic: with the memo removed
        // every step becomes its own full walk, and the per-step bound above is
        // breached by the third record (97 reads against 68), so the size that
        // catches a regression is small even though the size that makes the
        // contrast *legible* is 512.
        assert!(
            full_walks[2] >= 5 * full_walks[1],
            "full-walk cost {full_walks:?} did not scale with the post-anchor history"
        );
    }

    /// The accidental single-user damage a live immutable index node has to be
    /// re-checked against: it vanishes, it is cut short, or it is replaced by
    /// same-length bytes that no longer hash to the name it is stored under.
    #[derive(Clone, Copy, Debug)]
    enum HistoryNodeFault {
        Deleted,
        Truncated,
        DigestCorrupted,
    }

    fn history_node_path(
        archive: &Path,
        binding: crate::oplog::hot_engine::ProjectionStorageBinding,
        digest: ContentDigest,
    ) -> PathBuf {
        archive
            .join(ENGINE_HISTORY_DIR)
            .join(binding.endpoint.endpoint_id.to_string())
            .join(ENGINE_HISTORY_NODES_DIR)
            .join(history_index_filename(digest))
    }

    fn damage_history_node(path: &Path, fault: HistoryNodeFault) {
        let pristine = std::fs::read(path).unwrap();
        assert!(pristine.len() > 2);
        match fault {
            HistoryNodeFault::Deleted => std::fs::remove_file(path).unwrap(),
            HistoryNodeFault::Truncated => {
                std::fs::write(path, &pristine[..pristine.len() / 2]).unwrap();
            }
            HistoryNodeFault::DigestCorrupted => {
                let mut substituted = pristine.clone();
                let last = substituted.len() - 1;
                substituted[last] ^= 0xFF;
                assert_eq!(substituted.len(), pristine.len());
                std::fs::write(path, &substituted).unwrap();
            }
        }
    }

    /// The content address of the leaf `publish` stores for one record, so a
    /// test can name an individual deep node without walking the index.
    fn history_leaf_digest(batch_id: BatchId, record: &[u8]) -> ContentDigest {
        ContentDigest::of(
            &postcard::to_allocvec(&HistoryIndexNode::Leaf {
                schema_version: ENGINE_HISTORY_INDEX_SCHEMA_VERSION,
                batch_id,
                record: record.to_vec(),
            })
            .unwrap(),
        )
    }

    /// A memo may shorten the *walk*, never the availability and integrity
    /// facts the walk establishes about the live current endpoint. Losing the
    /// node named by `after.index_root` must be rejected identically whether or
    /// not this open already proved a transition from the same anchor.
    #[test]
    fn authenticated_history_extension_revalidates_the_live_current_root() {
        let root = test_root("live-current-root-revalidation");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(49_000));
        let binding = enrolled_binding(49_010);
        let open = || {
            let store = ObjectStore::open(&archive, workspace).unwrap();
            let history = store.open_engine_history(binding).unwrap();
            (store, history)
        };

        let (store, history) = open();
        let anchor = history.current_authority().unwrap();
        for index in 0..4_usize {
            history
                .publish(
                    spread_history_batch_id(index),
                    b"live current root record",
                    EngineHistoryBinding::empty(),
                )
                .unwrap();
        }
        let current = history.current_authority().unwrap();
        drop(history);
        drop(store);

        let node = history_node_path(&archive, binding, current.index_root);
        let pristine = std::fs::read(&node).unwrap();

        for fault in [
            HistoryNodeFault::Deleted,
            HistoryNodeFault::Truncated,
            HistoryNodeFault::DigestCorrupted,
        ] {
            // A warm store: the memo already holds `anchor -> current`, so the
            // residual step is empty and nothing below the head is walked.
            let (warm_store, warm) = open();
            warm.authenticate_current_history_extension(anchor).unwrap();
            warm.authenticate_current_history_extension(anchor).unwrap();

            damage_history_node(&node, fault);

            let warm_error = warm
                .authenticate_current_history_extension(anchor)
                .expect_err(&format!("a warm store accepted the {fault:?} current root"));
            drop(warm);
            drop(warm_store);

            let (fresh_store, fresh) = open();
            let fresh_error = fresh
                .authenticate_current_history_extension(anchor)
                .expect_err(&format!(
                    "a fresh store accepted the {fault:?} current root"
                ));
            assert_eq!(
                std::mem::discriminant(&warm_error),
                std::mem::discriminant(&fresh_error),
                "the {fault:?} current root was rejected differently warm ({warm_error:?}) and \
                 fresh ({fresh_error:?})"
            );
            drop(fresh);
            drop(fresh_store);

            // Repairing the exact immutable bytes restores the exact verdict.
            std::fs::write(&node, &pristine).unwrap();
            let (repaired_store, repaired) = open();
            assert_eq!(
                repaired
                    .authenticate_current_history_extension(anchor)
                    .unwrap()
                    .after(),
                current
            );
            drop(repaired);
            drop(repaired_store);
        }

        crate::test_support::remove_dir_all(root);
    }

    /// A publication that failed on a missing index node must not leave behind
    /// a memo that can authorize a later mutation against that storage.
    #[test]
    fn incomplete_publication_on_a_lost_index_node_disarms_the_memo() {
        let root = test_root("publication-failure-disarms-memo");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(50_000));
        let binding = enrolled_binding(50_010);
        let store = ObjectStore::open(&archive, workspace).unwrap();
        let history = store.open_engine_history(binding).unwrap();
        let node_reads = || store.instrumentation().history_index_reads;

        let anchor = history.current_authority().unwrap();
        for index in 0..4_usize {
            history
                .publish(
                    spread_history_batch_id(index),
                    b"publication failure record",
                    EngineHistoryBinding::empty(),
                )
                .unwrap();
        }
        let current = history.current_authority().unwrap();
        history
            .authenticate_current_history_extension(anchor)
            .unwrap();
        let before = node_reads();
        history
            .authenticate_current_history_extension(anchor)
            .unwrap();
        let warm_step = node_reads() - before;

        // The publication fails because the live root node is gone, which is
        // exactly a detected history/index I/O failure.
        let node = history_node_path(&archive, binding, current.index_root);
        let pristine = std::fs::read(&node).unwrap();
        std::fs::remove_file(&node).unwrap();
        assert!(history
            .publish(
                spread_history_batch_id(4),
                b"never committed",
                EngineHistoryBinding::empty(),
            )
            .is_err());

        // While the damage stands, the warm store must reject exactly like a
        // fresh one, and it may not authorize anything.
        assert!(history
            .authenticate_current_history_extension(anchor)
            .is_err());

        // Even after the exact immutable bytes come back, nothing this open
        // proved before the failure may be reused as a shortcut: the proof is
        // re-derived by the complete walk a fresh open would perform.
        std::fs::write(&node, &pristine).unwrap();
        let before = node_reads();
        assert_eq!(
            history
                .authenticate_current_history_extension(anchor)
                .unwrap()
                .after(),
            current
        );
        let disarmed_step = node_reads() - before;
        assert!(
            disarmed_step > warm_step,
            "a failed publication left a {disarmed_step}-read shortcut over the {warm_step}-read \
             warm path instead of disarming the memo"
        );

        drop(history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    /// Deeper immutable nodes stay a *previously authenticated* in-memory fact:
    /// re-reading them on every call is exactly the lifetime-sized work the
    /// memo exists to remove. The compensating contract is causal — the first
    /// operation that re-encounters the damage disarms the memo permanently, so
    /// from that point the warm store decides exactly like a fresh one.
    #[test]
    fn deeper_history_node_loss_disarms_the_memo_when_it_is_re_encountered() {
        let root = test_root("deep-node-loss");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(51_000));
        let binding = enrolled_binding(51_010);
        const RECORD: &[u8] = b"deep node loss record";
        let open = || {
            let store = ObjectStore::open(&archive, workspace).unwrap();
            let history = store.open_engine_history(binding).unwrap();
            (store, history)
        };

        let (store, history) = open();
        let node_reads = || store.instrumentation().history_index_reads;
        let anchor = history.current_authority().unwrap();
        for index in 0..4_usize {
            history
                .publish(
                    spread_history_batch_id(index),
                    RECORD,
                    EngineHistoryBinding::empty(),
                )
                .unwrap();
        }
        // Warm the memo on the first four records, then extend once more so the
        // residual `middle -> current` step provably cannot revisit them.
        history
            .authenticate_current_history_extension(anchor)
            .unwrap();
        history
            .publish(
                spread_history_batch_id(4),
                RECORD,
                EngineHistoryBinding::empty(),
            )
            .unwrap();
        history
            .authenticate_current_history_extension(anchor)
            .unwrap();
        let current = history.current_authority().unwrap();

        // A record whose radix path leaves the last insertion's path at the
        // root, so its leaf is outside the residual step by construction.
        let last_nibble = history_key_nibble(spread_history_batch_id(4).as_uuid().as_bytes(), 0);
        let doomed = (0..4_usize)
            .find(|index| {
                history_key_nibble(spread_history_batch_id(*index).as_uuid().as_bytes(), 0)
                    != last_nibble
            })
            .expect("a record diverging from the last insertion at the root nibble");
        let leaf = history_node_path(
            &archive,
            binding,
            history_leaf_digest(spread_history_batch_id(doomed), RECORD),
        );
        let pristine = std::fs::read(&leaf).unwrap();
        std::fs::remove_file(&leaf).unwrap();

        // The warm store still accepts: the missing leaf is a fact this open
        // authenticated earlier and nothing has re-encountered it yet. This is
        // the documented, bounded residual, and it costs only the constant
        // live-endpoint revalidation.
        let before = node_reads();
        assert_eq!(
            history
                .authenticate_current_history_extension(anchor)
                .unwrap()
                .after(),
            current
        );
        let warm_step = node_reads() - before;
        assert!(
            warm_step <= 2,
            "the warm step cost {warm_step} reads instead of the constant endpoint revalidation"
        );

        // Any replay, rebuild or lookup that reaches the damaged region — the
        // only way its bytes can reach a user or a projection — fails and
        // latches the fault.
        let replay_error = history
            .materialize(current.index_root)
            .expect_err("a replay over a missing leaf must fail");
        let warm_error = history
            .authenticate_current_history_extension(anchor)
            .expect_err("a re-encountered fault must disarm the memo");

        // A fresh open walks the whole post-anchor history and rejects the same
        // way, with no memo involved at all.
        drop(history);
        drop(store);
        let (fresh_store, fresh) = open();
        let fresh_error = fresh
            .authenticate_current_history_extension(anchor)
            .expect_err("a fresh store must reject a history with a missing leaf");
        assert_eq!(
            std::mem::discriminant(&warm_error),
            std::mem::discriminant(&fresh_error),
            "disarmed warm rejection {warm_error:?} differs from the fresh rejection \
             {fresh_error:?} (replay reported {replay_error:?})"
        );
        drop(fresh);
        drop(fresh_store);

        std::fs::write(&leaf, &pristine).unwrap();
        let (repaired_store, repaired) = open();
        assert_eq!(
            repaired
                .authenticate_current_history_extension(anchor)
                .unwrap()
                .after(),
            current
        );
        drop(repaired);
        drop(repaired_store);

        crate::test_support::remove_dir_all(root);
    }

    /// The projection-work caller re-anchors on the head it just accepted, so
    /// it presents a different anchor every batch while the promoted-runtime
    /// caller keeps proving from one immutable bootstrap anchor. The bounded
    /// memo must not let the moving anchor evict the fixed one.
    #[test]
    fn authenticated_history_extension_memo_keeps_the_reused_anchor_resident() {
        let root = test_root("memo-anchor-residency");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(48_000));
        let binding = enrolled_binding(48_010);
        let store = ObjectStore::open(&archive, workspace).unwrap();
        let history = store.open_engine_history(binding).unwrap();
        let node_reads = || store.instrumentation().history_index_reads;

        let fixed_anchor = history.current_authority().unwrap();
        let mut moving_anchor = fixed_anchor;
        for index in 0..200_usize {
            history
                .publish(
                    spread_history_batch_id(index),
                    b"anchor residency record",
                    EngineHistoryBinding::empty(),
                )
                .unwrap();
            // The moving anchor introduces a brand-new memo entry every batch.
            history
                .authenticate_current_history_extension(moving_anchor)
                .unwrap();
            moving_anchor = history.current_authority().unwrap();

            let before = node_reads();
            let fixed = history
                .authenticate_current_history_extension(fixed_anchor)
                .unwrap();
            let step = node_reads() - before;
            assert_eq!(fixed.after(), moving_anchor);
            if index > 0 {
                assert!(
                    step <= INCREMENTAL_STEP_BOUND,
                    "the reused anchor was evicted at batch {index}: {step} node reads"
                );
            }
        }

        drop(history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn authenticated_history_extension_memo_preserves_every_rejection() {
        let root = test_root("memo-preserves-rejection");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(47_000));
        let binding = enrolled_binding(47_010);
        let head_file = archive
            .join(ENGINE_HISTORY_DIR)
            .join(binding.endpoint.endpoint_id.to_string())
            .join(ENGINE_HISTORY_HEAD_FILE);
        let read_head = || std::fs::read(&head_file).unwrap();
        let open = || {
            let store = ObjectStore::open(&archive, workspace).unwrap();
            let history = store.open_engine_history(binding).unwrap();
            (store, history)
        };
        let node_reads = |store: &ObjectStore| store.instrumentation().history_index_reads;

        // Lineage A, built on one store whose memo is warmed from two anchors
        // by the ordinary publish-then-revalidate loop.
        let (store, history) = open();
        let empty_head = read_head();
        let anchor = history.current_authority().unwrap();
        let mut lineage_a = Vec::new();
        for index in 0..4_usize {
            history
                .publish(
                    spread_history_batch_id(index),
                    b"lineage a record",
                    EngineHistoryBinding::empty(),
                )
                .unwrap();
            lineage_a.push(history.current_authority().unwrap());
            history
                .authenticate_current_history_extension(anchor)
                .unwrap();
            history
                .authenticate_current_history_extension(lineage_a[0])
                .unwrap();
        }
        let (first, third, fourth) = (lineage_a[0], lineage_a[2], lineage_a[3]);
        let head_fourth = read_head();

        // An exact self-transition against a warm memo stays exact.
        let exact = history
            .authenticate_current_history_extension(fourth)
            .unwrap();
        assert_eq!(exact.before(), fourth);
        assert_eq!(exact.after(), fourth);

        // Failed publish, crash cut *before* the head swap: the head does not
        // move, so the verdict is unchanged — but an incomplete publication
        // disarms the memo, so the same verdict is re-derived by the complete
        // walk instead of inherited from anything this open proved earlier.
        fail_next_engine_history_head_swap();
        assert!(history
            .publish(
                spread_history_batch_id(4),
                b"never committed",
                EngineHistoryBinding::empty(),
            )
            .is_err());
        assert_eq!(read_head(), head_fourth);
        assert_eq!(history.current_authority().unwrap(), fourth);
        let before = node_reads(&store);
        assert_eq!(
            history
                .authenticate_current_history_extension(first)
                .unwrap()
                .after(),
            fourth
        );
        assert!(
            node_reads(&store) - before > LIVE_ENDPOINT_REVALIDATION_BOUND,
            "an incomplete publication left the memo armed"
        );
        drop(history);
        drop(store);

        // Failed publish, crash cut *after* the head swap: the record is
        // durable even though the call returned an error. The next open must
        // authenticate the advanced head from the same anchor.
        let (store, history) = open();
        fail_next_engine_history_after_head_swap();
        assert!(history
            .publish(
                spread_history_batch_id(4),
                b"committed under a failed publish",
                EngineHistoryBinding::empty(),
            )
            .is_err());
        let head_fifth = read_head();
        assert_ne!(head_fifth, head_fourth);
        drop(history);
        drop(store);

        let (store, history) = open();
        let fifth = history.current_authority().unwrap();
        assert_eq!(fifth.generation, 5);
        let advanced = history
            .authenticate_current_history_extension(first)
            .unwrap();
        assert_eq!(advanced.before(), first);
        assert_eq!(advanced.after(), fifth);
        drop(history);
        drop(store);

        // A divergent lineage B of the same and then greater length, published
        // over a head that was replaced back to the empty authority.
        let (store, history) = open();
        std::fs::write(&head_file, &empty_head).unwrap();
        let mut lineage_b = Vec::new();
        let mut heads_b = Vec::new();
        for index in 0..6_usize {
            history
                .publish(
                    spread_history_batch_id(1_000 + index),
                    b"lineage b record",
                    EngineHistoryBinding::empty(),
                )
                .unwrap();
            lineage_b.push(history.current_authority().unwrap());
            heads_b.push(read_head());
        }
        let divergent_equal = lineage_b[4];
        let divergent_longer = lineage_b[5];
        assert_eq!(divergent_equal.generation, fifth.generation);
        assert_eq!(divergent_longer.generation, fifth.generation + 1);
        drop(history);
        drop(store);

        // The adversarial phase runs on one store that never publishes, so it
        // follows the live head while keeping the memo it warmed.
        let (store, history) = open();
        std::fs::write(&head_file, &head_fifth).unwrap();
        assert_eq!(history.current_authority().unwrap(), fifth);
        let before = node_reads(&store);
        assert_eq!(
            history
                .authenticate_current_history_extension(first)
                .unwrap()
                .after(),
            fifth
        );
        assert!(
            node_reads(&store) - before > 0,
            "a fresh open must pay the full walk once"
        );
        assert_eq!(
            history
                .authenticate_current_history_extension(third)
                .unwrap()
                .after(),
            fifth
        );

        // Head replacement: rollback. The memo holds `first -> fifth` and
        // `third -> fifth`, and neither may survive the retreat as a proof
        // about `fifth` itself.
        std::fs::write(&head_file, &head_fourth).unwrap();
        assert!(matches!(
            history.authenticate_current_history_extension(fifth),
            Err(StoreError::MalformedHistoryIndex)
        ));
        // A still-valid ancestor anchor is still accepted, even though its warm
        // memo endpoint is now ahead of the live head: a stale memo may only
        // fail to compose, never turn an acceptance into a rejection.
        assert_eq!(
            history
                .authenticate_current_history_extension(first)
                .unwrap()
                .after(),
            fourth
        );

        // Head replacement: equal-generation divergence.
        std::fs::write(&head_file, &heads_b[4]).unwrap();
        assert_eq!(history.current_authority().unwrap(), divergent_equal);
        assert!(matches!(
            history.authenticate_current_history_extension(fifth),
            Err(StoreError::MalformedHistoryIndex)
        ));
        // Cached-middle substitution: `fourth` and `fifth` are exactly the
        // endpoints the memo holds for `first` and `third`, and they are what
        // an attacker would want spliced in to reach the divergent head.
        // Neither composition nor the fallback walk can manufacture that proof.
        assert!(matches!(
            history.authenticate_current_history_extension(first),
            Err(StoreError::MalformedHistoryIndex)
        ));
        assert!(matches!(
            history.authenticate_current_history_extension(third),
            Err(StoreError::MalformedHistoryIndex)
        ));

        // Head replacement: higher-generation non-descendant.
        std::fs::write(&head_file, &heads_b[5]).unwrap();
        assert_eq!(history.current_authority().unwrap(), divergent_longer);
        assert!(matches!(
            history.authenticate_current_history_extension(fifth),
            Err(StoreError::MalformedHistoryIndex)
        ));
        assert!(matches!(
            history.authenticate_current_history_extension(first),
            Err(StoreError::MalformedHistoryIndex)
        ));

        // Substituted anchors — a real generation paired with another real
        // index root — miss the memo and are rejected by the full walk.
        for substituted in [
            EngineHistoryAuthority {
                generation: third.generation,
                index_root: first.index_root,
            },
            EngineHistoryAuthority {
                generation: first.generation,
                index_root: fifth.index_root,
            },
            EngineHistoryAuthority {
                generation: fifth.generation,
                index_root: lineage_b[0].index_root,
            },
        ] {
            assert!(matches!(
                history.authenticate_current_history_extension(substituted),
                Err(StoreError::MalformedHistoryIndex)
            ));
        }
        // A `before` whose generation and root disagree about emptiness is
        // rejected outright, memo or not.
        assert!(matches!(
            history.authenticate_current_history_extension(EngineHistoryAuthority {
                generation: 0,
                index_root: first.index_root,
            }),
            Err(StoreError::MalformedHistoryIndex)
        ));

        // Head replacement: rollback below a non-empty anchor.
        std::fs::write(&head_file, &empty_head).unwrap();
        assert!(matches!(
            history.authenticate_current_history_extension(first),
            Err(StoreError::MalformedHistoryIndex)
        ));
        drop(history);
        drop(store);

        // Fresh store: no memo survives the open, the first proof pays the
        // complete walk, and every verdict above is reproduced without it.
        std::fs::write(&head_file, &heads_b[5]).unwrap();
        let (store, history) = open();
        let before = node_reads(&store);
        let fresh = history
            .authenticate_current_history_extension(lineage_b[0])
            .unwrap();
        assert!(node_reads(&store) - before > 0);
        assert_eq!(fresh.before(), lineage_b[0]);
        assert_eq!(fresh.after(), divergent_longer);
        for rejected in [first, third, fourth, fifth] {
            assert!(matches!(
                history.authenticate_current_history_extension(rejected),
                Err(StoreError::MalformedHistoryIndex)
            ));
        }
        drop(history);
        drop(store);

        std::fs::write(&head_file, &head_fourth).unwrap();
        let (store, history) = open();
        let before = node_reads(&store);
        let reproved = history
            .authenticate_current_history_extension(first)
            .unwrap();
        assert!(node_reads(&store) - before > 0);
        assert_eq!(reproved.after(), fourth);

        drop(history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn prior_version_durable_history_requires_upgrade_without_writes() {
        fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
            let mut result = BTreeMap::new();
            let mut pending = vec![path.to_path_buf()];
            while let Some(directory) = pending.pop() {
                for entry in std::fs::read_dir(&directory).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_dir() {
                        pending.push(entry.path());
                    } else {
                        result.insert(
                            entry.path().strip_prefix(path).unwrap().to_path_buf(),
                            std::fs::read(entry.path()).unwrap(),
                        );
                    }
                }
            }
            result
        }

        let root = test_root("prior-durable-root");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(50));
        let endpoint = crate::oplog::ProjectionEndpointId::from_uuid(Uuid::from_u128(51));
        let binding = crate::oplog::hot_engine::ProjectionStorageBinding {
            endpoint: crate::oplog::ProjectionEndpointBinding {
                endpoint_id: endpoint,
                device_id: crate::oplog::DeviceId::from_uuid(Uuid::from_u128(52)),
                graph_resource_id: crate::oplog::CanonicalGraphResourceId::from_capability_identity(
                    b"test",
                    b"prior-durable-root",
                ),
            },
            receipt_store_id: crate::oplog::ProjectionReceiptStoreId::from_capability_identity(
                b"test",
                b"prior-durable-receipts",
            ),
        };
        let archive_path = root.join("archive");
        let store = ObjectStore::open(&archive_path, workspace).unwrap();
        let history = store.open_engine_history(binding).unwrap();
        history
            .publish(
                BatchId::from_uuid(Uuid::from_u128(53)),
                b"preserved accepted history",
                EngineHistoryBinding::empty(),
            )
            .unwrap();
        let control = archive_path
            .join(ENGINE_HISTORY_DIR)
            .join(endpoint.to_string());
        let prior_version = ENGINE_HISTORY_ROOT_SCHEMA_VERSION - 1;
        let prior_claim = postcard::to_allocvec(&(
            prior_version,
            workspace,
            endpoint,
            binding.endpoint.graph_resource_id,
        ))
        .unwrap();
        std::fs::write(control.join(ENGINE_HISTORY_CLAIM_FILE), prior_claim).unwrap();
        let before = snapshot(&archive_path);

        let reopened = ObjectStore::open(&archive_path, workspace).unwrap();
        assert!(matches!(
            reopened.open_engine_history(binding),
            Err(StoreError::UpgradeRequired {
                store: "engine history",
                found,
                current
            }) if found == prior_version && current == ENGINE_HISTORY_ROOT_SCHEMA_VERSION
        ));
        assert_eq!(snapshot(&archive_path), before);
        drop(history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn engine_history_failure_before_head_swap_keeps_prior_authority() {
        let root = test_root("history-pre-head-swap-failure");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(55_000));
        let endpoint = crate::oplog::ProjectionEndpointId::from_uuid(Uuid::from_u128(55_001));
        let binding = crate::oplog::hot_engine::ProjectionStorageBinding {
            endpoint: crate::oplog::ProjectionEndpointBinding {
                endpoint_id: endpoint,
                device_id: crate::oplog::DeviceId::from_uuid(Uuid::from_u128(55_002)),
                graph_resource_id: crate::oplog::CanonicalGraphResourceId::from_capability_identity(
                    b"test",
                    b"history-pre-head-swap-failure",
                ),
            },
            receipt_store_id: crate::oplog::ProjectionReceiptStoreId::from_capability_identity(
                b"test",
                b"history-pre-head-swap-failure-receipts",
            ),
        };
        let archive_path = root.join("archive");
        let store = ObjectStore::open(&archive_path, workspace).unwrap();
        let history = store.open_engine_history(binding).unwrap();
        let before = history.current_with_binding().unwrap();
        fail_next_engine_history_head_swap();
        assert!(history
            .publish(
                BatchId::from_uuid(Uuid::from_u128(55_003)),
                b"unpublished record-v8 candidate",
                EngineHistoryBinding::empty(),
            )
            .is_err());
        assert_eq!(history.current_with_binding().unwrap(), before);
        drop(history);
        drop(store);

        let reopened = ObjectStore::open(&archive_path, workspace).unwrap();
        let reopened_history = reopened.open_engine_history(binding).unwrap();
        assert_eq!(reopened_history.current_with_binding().unwrap(), before);
        drop(reopened_history);
        drop(reopened);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn synthetic_future_durable_history_rejects_before_creating_layout() {
        let root = test_root("future-durable-root");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(60));
        let endpoint = crate::oplog::ProjectionEndpointId::from_uuid(Uuid::from_u128(61));
        let binding = crate::oplog::hot_engine::ProjectionStorageBinding {
            endpoint: crate::oplog::ProjectionEndpointBinding {
                endpoint_id: endpoint,
                device_id: crate::oplog::DeviceId::from_uuid(Uuid::from_u128(62)),
                graph_resource_id: crate::oplog::CanonicalGraphResourceId::from_capability_identity(
                    b"test",
                    b"future-durable-root",
                ),
            },
            receipt_store_id: crate::oplog::ProjectionReceiptStoreId::from_capability_identity(
                b"test",
                b"future-durable-receipts",
            ),
        };
        let archive_path = root.join("archive");
        let store = ObjectStore::open(&archive_path, workspace).unwrap();
        let control = archive_path
            .join(ENGINE_HISTORY_DIR)
            .join(endpoint.to_string());
        std::fs::create_dir_all(&control).unwrap();
        std::fs::write(control.join(ENGINE_HISTORY_HEAD_FILE), b"future-head").unwrap();
        let future_claim = postcard::to_allocvec(&(
            ENGINE_HISTORY_ROOT_SCHEMA_VERSION + 1,
            workspace,
            endpoint,
            binding.endpoint.graph_resource_id,
            binding.receipt_store_id,
        ))
        .unwrap();
        std::fs::write(control.join(ENGINE_HISTORY_CLAIM_FILE), future_claim).unwrap();
        let before = snapshot_tree(&archive_path);

        assert!(matches!(
            store.open_engine_history(binding),
            Err(StoreError::UnsupportedStoreVersion {
                store: "engine history",
                version
            }) if version == ENGINE_HISTORY_ROOT_SCHEMA_VERSION + 1
        ));
        assert_eq!(snapshot_tree(&archive_path), before);
        assert!(!control.join(ENGINE_HISTORY_NODES_DIR).exists());
        assert!(!control.join(ENGINE_HISTORY_ROOTS_DIR).exists());
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn sealed_history_substitution_after_preflight_fails_closed() {
        let root = test_root("sealed-history-preflight-substitution");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(65));
        let binding = enrolled_binding(66);
        let substitute = enrolled_binding(67);
        let store = ObjectStore::open(&archive, workspace).unwrap();
        drop(store.open_engine_history(binding).unwrap());
        drop(store.open_engine_history(substitute).unwrap());
        std::fs::remove_file(archive.join(ENGINE_HISTORY_TRANSITION_LOCK_FILE)).unwrap();

        let histories = archive.join(ENGINE_HISTORY_DIR);
        let target = histories.join(binding.endpoint.endpoint_id.to_string());
        let displaced = histories.join("displaced-after-preflight");
        let substitute_control = histories.join(substitute.endpoint.endpoint_id.to_string());
        let target_hook = target.clone();
        let displaced_hook = displaced.clone();
        set_sealed_history_after_preflight_hook(move || {
            std::fs::rename(target_hook, displaced_hook).unwrap();
            std::fs::rename(substitute_control, target).unwrap();
        });

        let error = store
            .seal_history_only(binding)
            .err()
            .expect("substituted history must be rejected")
            .1;
        assert!(matches!(error, StoreError::MalformedHistoryIndex));
        assert!(displaced.is_dir(), "the original control was not displaced");
        assert!(
            archive.join(ENGINE_HISTORY_TRANSITION_LOCK_FILE).is_file(),
            "compatible preflight must still reach the durable lock"
        );
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn valid_sealed_history_open_recreates_and_uses_transition_lock() {
        let root = test_root("valid-sealed-history-lock");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(68));
        let binding = enrolled_binding(69);
        let store = ObjectStore::open(&archive, workspace).unwrap();
        drop(store.open_engine_history(binding).unwrap());
        let lock_path = archive.join(ENGINE_HISTORY_TRANSITION_LOCK_FILE);
        std::fs::remove_file(&lock_path).unwrap();

        let open = store.seal_history_only(binding).unwrap();
        assert!(
            lock_path.is_file(),
            "valid sealed open did not create the lock"
        );
        let (store, history) = open.into_history().unwrap();
        let guard = AdvisoryTransitionGuard::lock(&history.transition_lock).unwrap();
        drop(guard);
        drop(history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    #[ignore = "subprocess helper invoked by sealed_history_validation_serializes_a_concurrent_valid_transition"]
    fn sealed_history_transition_subprocess_helper() {
        let Ok(archive) = std::env::var("TINE_SEALED_OPEN_HELPER_ARCHIVE") else {
            return;
        };
        let contended = std::env::var("TINE_SEALED_OPEN_HELPER_CONTENDED").unwrap();
        let store = ObjectStore::open(
            Path::new(&archive),
            WorkspaceId::from_uuid(Uuid::from_u128(0x7e00)),
        )
        .unwrap();
        let history = store.open_engine_history(enrolled_binding(0x7e01)).unwrap();
        set_advisory_transition_contention_hook(move || {
            std::fs::write(contended, b"contended").unwrap();
        });
        history
            .publish(
                BatchId::from_uuid(Uuid::from_u128(0x7e02)),
                b"serialized valid history transition",
                EngineHistoryBinding::empty(),
            )
            .unwrap();
    }

    #[test]
    fn sealed_history_validation_serializes_a_concurrent_valid_transition() {
        let root = test_root("sealed-history-transition-serialization");
        let archive = root.join("archive");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(0x7e00));
        let binding = enrolled_binding(0x7e01);
        let store = ObjectStore::open(&archive, workspace).unwrap();
        let publishing_history = store.open_engine_history(binding).unwrap();
        let initial_head = publishing_history.read_live_head_root().unwrap().0;
        let contended = root.join("child-contended");
        let child = Arc::new(Mutex::new(None));
        let child_for_hook = Arc::clone(&child);
        let archive_for_hook = archive.clone();
        let contended_for_hook = contended.clone();

        set_sealed_history_authority_window_hook(move |stage| match stage {
            SealedHistoryAuthorityWindowStage::Locked => {
                let spawned = std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("sealed_history_transition_subprocess_helper")
                    .arg("--ignored")
                    .arg("--nocapture")
                    .env(
                        "TINE_SEALED_OPEN_HELPER_ARCHIVE",
                        archive_for_hook.as_os_str(),
                    )
                    .env(
                        "TINE_SEALED_OPEN_HELPER_CONTENDED",
                        contended_for_hook.as_os_str(),
                    )
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                *child_for_hook.lock().unwrap() = Some(spawned);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !contended_for_hook.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                assert!(
                    contended_for_hook.exists(),
                    "valid subprocess transition did not contend with sealed validation"
                );
            }
            SealedHistoryAuthorityWindowStage::Validated => {
                assert!(
                    child_for_hook
                        .lock()
                        .unwrap()
                        .as_mut()
                        .expect("transition subprocess")
                        .try_wait()
                        .unwrap()
                        .is_none(),
                    "valid subprocess transition interleaved with sealed authority validation"
                );
            }
        });

        let sealed = store.seal_existing_engine_history(binding).unwrap();
        let opened = match sealed {
            SealedControl::Existing(history) => history,
            SealedControl::Absent(_) => panic!("initialized history reopened as absent"),
        };
        assert_eq!(
            *opened.authoritative_head.lock().unwrap(),
            Some(initial_head),
            "sealed validation did not pin the pre-transition authority"
        );
        let output = child
            .lock()
            .unwrap()
            .take()
            .expect("transition subprocess")
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "serialized subprocess transition failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            publishing_history
                .read_live_head_root()
                .unwrap()
                .1
                .generation,
            1
        );
        drop(opened);
        drop(publishing_history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn authenticated_durable_root_version_matrix_rejects_without_writes() {
        let root = test_root("durable-root-version-matrix");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(70));
        let endpoint = crate::oplog::ProjectionEndpointId::from_uuid(Uuid::from_u128(71));
        let binding = crate::oplog::hot_engine::ProjectionStorageBinding {
            endpoint: crate::oplog::ProjectionEndpointBinding {
                endpoint_id: endpoint,
                device_id: crate::oplog::DeviceId::from_uuid(Uuid::from_u128(72)),
                graph_resource_id: crate::oplog::CanonicalGraphResourceId::from_capability_identity(
                    b"test",
                    b"durable-root-version-matrix",
                ),
            },
            receipt_store_id: crate::oplog::ProjectionReceiptStoreId::from_capability_identity(
                b"test",
                b"durable-root-version-matrix-receipts",
            ),
        };
        let archive_path = root.join("archive");
        let store = ObjectStore::open(&archive_path, workspace).unwrap();
        drop(store.open_engine_history(binding).unwrap());
        let control = archive_path
            .join(ENGINE_HISTORY_DIR)
            .join(endpoint.to_string());
        let roots = control.join(ENGINE_HISTORY_ROOTS_DIR);

        for version in [
            ENGINE_HISTORY_ROOT_SCHEMA_VERSION - 1,
            ENGINE_HISTORY_ROOT_SCHEMA_VERSION + 1,
        ] {
            let authenticated_root = DurableEngineHistoryRoot {
                schema_version: version,
                workspace_id: workspace,
                endpoint_id: endpoint,
                graph_resource_id: binding.endpoint.graph_resource_id,
                receipt_store_id: binding.receipt_store_id,
                generation: 0,
                index_root: EngineHistoryStore::empty_root(),
                latest_batch_id: None,
                binding: DurableEngineHistoryBinding::ordinary(EngineHistoryBinding::empty()),
            };
            let bytes = postcard::to_allocvec(&authenticated_root).unwrap();
            let digest = ContentDigest::of(&bytes);
            std::fs::write(roots.join(engine_history_root_filename(digest)), &bytes).unwrap();
            std::fs::write(control.join(ENGINE_HISTORY_HEAD_FILE), digest.to_string()).unwrap();
            let before = snapshot_tree(&archive_path);

            let error = store.preflight_engine_history(binding).unwrap_err();
            if version < ENGINE_HISTORY_ROOT_SCHEMA_VERSION {
                assert!(matches!(
                    error,
                    StoreError::UpgradeRequired {
                        store: "engine history",
                        found,
                        current,
                    } if found == version && current == ENGINE_HISTORY_ROOT_SCHEMA_VERSION
                ));
            } else {
                assert!(matches!(
                    error,
                    StoreError::UnsupportedStoreVersion {
                        store: "engine history",
                        version: found,
                    } if found == version
                ));
            }
            assert_eq!(snapshot_tree(&archive_path), before);
            assert!(!archive_path
                .join(super::super::scratch_store::SCRATCH_DIR)
                .exists());
        }

        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        windows
    ))]
    #[test]
    fn authenticated_history_publication_is_concurrent_canonical_and_missing_safe() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let root = test_root("concurrent");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(10));
        let store = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        let history = Arc::new(store.start_engine_history().unwrap());
        let batch_id = BatchId::from_uuid(Uuid::from_u128(11));
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let history = Arc::clone(&history);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    history.insert(
                        EngineHistoryStore::empty_root(),
                        batch_id,
                        b"same immutable record",
                    )
                })
            })
            .collect();
        let roots: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect();
        assert!(roots.iter().all(|candidate| *candidate == roots[0]));
        assert_eq!(
            history.lookup(roots[0], batch_id).unwrap(),
            Some(b"same immutable record".to_vec())
        );

        let malformed = HistoryIndexNode::Branch {
            schema_version: ENGINE_HISTORY_INDEX_SCHEMA_VERSION,
            depth: 0,
            children: vec![(1, roots[0]), (1, roots[0])],
        };
        assert!(matches!(
            history.publish_node(&malformed),
            Err(StoreError::MalformedHistoryIndex)
        ));

        let run = std::fs::read_dir(root.join("archive/engine-history"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::remove_file(run.join(history_index_filename(roots[0]))).unwrap();
        assert!(matches!(
            history.lookup(roots[0], batch_id),
            Err(StoreError::Io(error)) if error.kind() == ErrorKind::NotFound
        ));
        drop(history);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }

    #[test]
    fn authenticated_block_claim_point_index_is_bounded_and_fails_closed() {
        let root = test_root("block-claim-integrity");
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(20));
        let store = ObjectStore::open(&root.join("archive"), workspace).unwrap();
        let index = store.start_block_claim_index().unwrap();
        let records: Vec<_> = (0_u128..256)
            .map(|value| {
                (
                    Uuid::from_u128(10_000 + value).into_bytes(),
                    BlockClaimIndexValue::from_slice(&value.to_be_bytes()),
                )
            })
            .collect();
        let before_insert = store.instrumentation();
        let mut index_root = index
            .insert_many(BlockClaimIndexRoot::default(), &records)
            .unwrap();
        let after_insert = store.instrumentation();
        assert_eq!(
            after_insert.directory_enumerations - before_insert.directory_enumerations,
            0
        );
        assert!(after_insert.block_claim_index_writes > before_insert.block_claim_index_writes);
        assert_eq!(
            after_insert.block_claim_index_syncs - before_insert.block_claim_index_syncs,
            0,
            "the reconstructible run-local index must not enter the authoritative durability path"
        );

        let requested = [
            records[0].0,
            records[127].0,
            records[255].0,
            Uuid::from_u128(99_999).into_bytes(),
        ];
        let before_lookup = store.instrumentation();
        let found = index.lookup_many(index_root, &requested).unwrap();
        let after_lookup = store.instrumentation();
        assert_eq!(found.len(), 3);
        assert_eq!(found[&records[127].0], records[127].1);
        assert_eq!(
            after_lookup.directory_enumerations - before_lookup.directory_enumerations,
            0
        );
        assert!(
            after_lookup.block_claim_index_reads - before_lookup.block_claim_index_reads <= 16,
            "point lookup escaped the requested radix paths"
        );

        assert!(matches!(
            index.lookup_many(index_root, &[records[1].0, records[0].0]),
            Err(StoreError::MalformedBlockClaimIndex)
        ));
        assert!(matches!(
            index.insert_many(
                index_root,
                &[
                    (records[1].0, BlockClaimIndexValue::from_slice(&[1])),
                    (records[0].0, BlockClaimIndexValue::from_slice(&[2]))
                ]
            ),
            Err(StoreError::MalformedBlockClaimIndex)
        ));

        let replacement = BlockClaimIndexValue::from_slice(b"newest canonical value");
        index_root = index
            .insert_many(index_root, &[(records[0].0, replacement.clone())])
            .unwrap();
        assert_eq!(
            index.lookup_many(index_root, &requested[..1]).unwrap()[&records[0].0],
            replacement,
            "newest authenticated segment must deterministically shadow an older value"
        );

        let run = std::fs::read_dir(root.join("archive/block-claim-index"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let page_path = run.join(BLOCK_CLAIM_INDEX_FILE);
        let original = std::fs::read(&page_path).unwrap();
        let global_ref = index_root.global_filter.unwrap();
        let global_payload_offset = usize::try_from(global_ref.offset).unwrap() + 4;
        let mut tampered_global = original.clone();
        tampered_global[global_payload_offset] ^= 1;
        std::fs::write(&page_path, &tampered_global).unwrap();
        assert!(matches!(
            index.lookup_many(index_root, &requested[..1]),
            Err(StoreError::BlockClaimIndexPathMismatch(found)) if found == global_ref.digest
        ));
        std::fs::write(&page_path, &original).unwrap();

        let root_segment = *index_root
            .levels
            .iter()
            .flatten()
            .flatten()
            .max_by_key(|segment| segment.generation)
            .unwrap();
        let root_ref = root_segment.page_ref;
        let payload_offset = usize::try_from(root_ref.offset).unwrap() + 4;
        let mut tampered = original.clone();
        tampered[payload_offset] ^= 1;
        std::fs::write(&page_path, &tampered).unwrap();
        assert!(matches!(
            index.lookup_many(index_root, &requested[..1]),
            Err(StoreError::BlockClaimIndexPathMismatch(found)) if found == root_ref.digest
        ));

        std::fs::write(&page_path, &original[..original.len() - 1]).unwrap();
        assert!(matches!(
            index.lookup_many(index_root, &requested[..1]),
            Err(StoreError::Io(error)) if error.kind() == ErrorKind::UnexpectedEof
        ));
        std::fs::write(&page_path, &original).unwrap();

        let malformed = BlockClaimIndexPage::Branch {
            schema_version: BLOCK_CLAIM_INDEX_SCHEMA_VERSION,
            depth: 0,
            children: vec![(0, root_ref), (0, root_ref)],
        };
        let malformed_bytes = postcard::to_allocvec(&malformed).unwrap();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&page_path)
            .unwrap();
        let offset = file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(&(malformed_bytes.len() as u32).to_be_bytes())
            .unwrap();
        file.write_all(&malformed_bytes).unwrap();
        file.sync_all().unwrap();
        let mut malformed_root = BlockClaimIndexRoot {
            next_generation: 1,
            global_filter: index_root.global_filter,
            ..BlockClaimIndexRoot::default()
        };
        malformed_root.levels[0][0] = Some(BlockClaimSegmentRef {
            generation: 1,
            entry_count: root_segment.entry_count,
            page_ref: BlockClaimPageRef {
                offset,
                encoded_len: malformed_bytes.len() as u32,
                digest: ContentDigest::of(&malformed_bytes),
            },
            filter_ref: root_segment.filter_ref,
        });
        assert!(matches!(
            index.lookup_many(malformed_root, &requested[..1]),
            Err(StoreError::MalformedBlockClaimIndex)
        ));

        let mut full_level = index_root;
        full_level.next_generation = BLOCK_CLAIM_SEGMENTS_PER_LEVEL as u64;
        for (slot, segment) in full_level.levels[0].iter_mut().enumerate() {
            let mut selected = root_segment;
            selected.generation = slot as u64 + 1;
            *segment = Some(selected);
        }
        let compacted_key = Uuid::from_u128(200_000).into_bytes();
        let compacted_value = BlockClaimIndexValue::from_slice(b"level carry");
        let compacted = index
            .insert_many(full_level, &[(compacted_key, compacted_value.clone())])
            .unwrap();
        assert!(compacted.levels[0].iter().all(Option::is_none));
        assert_eq!(compacted.levels[1].iter().flatten().count(), 1);
        let compacted_lookup = index
            .lookup_many(compacted, &[records[0].0, compacted_key])
            .unwrap();
        assert_eq!(compacted_lookup[&records[0].0], replacement);
        assert_eq!(compacted_lookup[&compacted_key], compacted_value);

        drop(index);
        drop(store);
        crate::test_support::remove_dir_all(root);
    }
}

pub(crate) fn require_regular_entry(
    file_type: &cap_std::fs::FileType,
    name: &str,
) -> Result<(), StoreError> {
    tine_storage::require_regular_entry(file_type, name).map_err(filesystem_error_without_collision)
}

pub(crate) fn sync_dir_required(dir: &Dir) -> Result<(), StoreError> {
    crate::durability_counters::note(crate::durability_counters::Barrier::Directory);
    tine_storage::sync_dir_required(dir).map_err(Into::into)
}
