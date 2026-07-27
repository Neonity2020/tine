use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(windows)]
use cap_fs_ext::OsMetadataExt as _;
use cap_std::fs::{Dir, OpenOptions};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{BatchId, ContentDigest, WorkspaceId};

pub(crate) const SCRATCH_DIR: &str = "engine-scratch-v2";
const MARKER_FILE: &str = "marker";
const LEASE_FILE: &str = "lease";
const PAGES_FILE: &str = "pages.index";
const BLOBS_FILE: &str = "blobs.data";
const SCRATCH_SCHEMA_VERSION: u32 = 12;
const SCRATCH_PAGE_SCHEMA_VERSION: u32 = 1;
const SCRATCH_LSM_LEVELS: usize = 32;
const ACCEPTED_SEQUENCE_SCHEMA_VERSION: u32 = 1;
const ACCEPTED_SEQUENCE_LEAF_CAPACITY: usize = 1;
const ACCEPTED_SEQUENCE_NODE_FANOUT: usize = 32;
const AUTHENTICATED_MAP_SCHEMA_VERSION: u32 = 1;
const AUTHENTICATED_CATALOG_SCHEMA_VERSION: u32 = 1;
const AUTHENTICATED_POINT_MAP_SCHEMA_VERSION: u32 = 1;
const CAUSAL_ACCUMULATOR_SCHEMA_VERSION: u32 = 1;
const MAX_AUTHENTICATED_MAP_DEPTH: usize = 256;
const MAX_AUTHENTICATED_CATALOG_VALUE_BYTES: usize = 8 * 1024;
#[cfg(test)]
pub(crate) const AUTHENTICATED_CATALOG_MAX_DEPTH: usize = MAX_AUTHENTICATED_MAP_DEPTH;
pub(crate) const AUTHENTICATED_POINT_MAX_DEPTH: usize = 256;
pub(crate) const AUTHENTICATED_POINT_MAX_KEY_BYTES: usize = 64;
pub(crate) const AUTHENTICATED_POINT_MAX_VALUE_BYTES: usize = MAX_PAGE_BYTES - 4096;
pub(crate) const AUTHENTICATED_POINT_MAX_MUTATIONS: usize = 65;
pub(crate) const AUTHENTICATED_POINT_MAX_PAGE_BYTES: usize = MAX_PAGE_BYTES;
pub(crate) const AUTHENTICATED_POINT_MAX_IO_PER_MUTATION: usize =
    8 * (AUTHENTICATED_POINT_MAX_DEPTH + 1);
const CURRENT_FILTER_WORDS: usize = 16_384;
const MAX_COVERED_BLOB_DEDUP_ROOTS: usize = 256;
const MAX_MARKER_BYTES: u64 = 4 * 1024;
const MAX_PAGE_BYTES: usize = 256 * 1024 * 1024;
const MAX_BLOB_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ScratchStats {
    pub page_reads: usize,
    pub page_writes: usize,
    pub page_bytes_read: usize,
    pub page_bytes_written: usize,
    pub max_page_bytes_read: usize,
    pub blob_reads: usize,
    pub blob_writes: usize,
    pub blob_bytes_read: usize,
    pub blob_bytes_written: usize,
    pub point_reads: usize,
    pub range_reads: usize,
    pub scratch_syncs: usize,
    pub stale_runs_reclaimed: usize,
    pub live_runs_skipped: usize,
    pub retained_runs_preserved: usize,
    pub unclassified_runs_preserved: usize,
}

#[derive(Debug, Default)]
struct ScratchCounters {
    page_reads: AtomicUsize,
    page_writes: AtomicUsize,
    page_bytes_read: AtomicUsize,
    page_bytes_written: AtomicUsize,
    max_page_bytes_read: AtomicUsize,
    blob_reads: AtomicUsize,
    blob_writes: AtomicUsize,
    blob_bytes_read: AtomicUsize,
    blob_bytes_written: AtomicUsize,
    point_reads: AtomicUsize,
    range_reads: AtomicUsize,
    // This deliberately has no increment site. Any future scratch sync must
    // become visible to the normal-flow regression gates.
    scratch_syncs: AtomicUsize,
    stale_runs_reclaimed: AtomicUsize,
    live_runs_skipped: AtomicUsize,
    /// Retained runs an ordinary reclamation pass observed and deliberately
    /// left intact. This is the deterministic counter that proves reclamation
    /// never silently deletes adoptable state.
    retained_runs_preserved: AtomicUsize,
    /// Sibling entries a reclamation pass could not authenticate or classify,
    /// and therefore preserved untouched. This is the deterministic counter
    /// that proves an unclassifiable sibling is skipped rather than deleted or
    /// allowed to veto the caller's own fresh run.
    unclassified_runs_preserved: AtomicUsize,
}

#[derive(Debug)]
struct FixedPointFilter {
    words: Vec<u64>,
    /// When set, the filter never answers "absent". A run adopted from durable
    /// bytes did not observe the inserts that produced them, so its run-local
    /// negative filter would otherwise report false negatives for data that is
    /// really present.
    saturated: bool,
}

impl Default for FixedPointFilter {
    fn default() -> Self {
        Self {
            words: vec![0; CURRENT_FILTER_WORDS],
            saturated: false,
        }
    }
}

impl FixedPointFilter {
    fn saturated() -> Self {
        Self {
            saturated: true,
            ..Self::default()
        }
    }

    fn insert(&mut self, key: &[u8]) {
        for position in self.positions(key) {
            self.words[position / 64] |= 1_u64 << (position % 64);
        }
    }

    fn might_contain(&self, key: &[u8]) -> bool {
        self.saturated
            || self
                .positions(key)
                .into_iter()
                .all(|position| self.words[position / 64] & (1_u64 << (position % 64)) != 0)
    }

    fn positions(&self, key: &[u8]) -> [usize; 4] {
        let digest = ContentDigest::of(key);
        let bytes = digest.as_bytes();
        let first = u64::from_be_bytes(bytes[..8].try_into().expect("digest word"));
        let second = u64::from_be_bytes(bytes[8..16].try_into().expect("digest word")) | 1;
        let bits = self.words.len() as u64 * 64;
        std::array::from_fn(|index| {
            first
                .wrapping_add(second.wrapping_mul(index as u64))
                .wrapping_rem(bits) as usize
        })
    }
}

#[derive(Debug)]
struct CoveredBlobDedupFilter {
    points: FixedPointFilter,
    covered_generation: u64,
    covered_roots: VecDeque<ScratchLsmRoot>,
}

impl Default for CoveredBlobDedupFilter {
    fn default() -> Self {
        Self {
            points: FixedPointFilter::default(),
            covered_generation: 0,
            covered_roots: VecDeque::from([ScratchLsmRoot::default()]),
        }
    }
}

impl CoveredBlobDedupFilter {
    fn record_insert(
        &mut self,
        parent: &ScratchLsmRoot,
        next: &ScratchLsmRoot,
        records: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) {
        for (key, value) in records {
            if value.is_some() {
                self.points.insert(key);
            }
        }
        self.covered_generation = self.covered_generation.max(next.next_generation);
        if self.covers_root(parent) {
            self.covered_roots.push_back(next.clone());
            if self.covered_roots.len() > MAX_COVERED_BLOB_DEDUP_ROOTS {
                self.covered_roots.pop_front();
            }
        }
    }

    fn covers_root(&self, root: &ScratchLsmRoot) -> bool {
        root.next_generation <= self.covered_generation
            && self
                .covered_roots
                .iter()
                .rev()
                .any(|covered| covered == root)
    }

    fn proves_absent(&self, root: &ScratchLsmRoot, key: &[u8]) -> bool {
        self.covers_root(root) && !self.points.might_contain(key)
    }
}

impl ScratchCounters {
    fn snapshot(&self) -> ScratchStats {
        ScratchStats {
            page_reads: self.page_reads.load(Ordering::Relaxed),
            page_writes: self.page_writes.load(Ordering::Relaxed),
            page_bytes_read: self.page_bytes_read.load(Ordering::Relaxed),
            page_bytes_written: self.page_bytes_written.load(Ordering::Relaxed),
            max_page_bytes_read: self.max_page_bytes_read.load(Ordering::Relaxed),
            blob_reads: self.blob_reads.load(Ordering::Relaxed),
            blob_writes: self.blob_writes.load(Ordering::Relaxed),
            blob_bytes_read: self.blob_bytes_read.load(Ordering::Relaxed),
            blob_bytes_written: self.blob_bytes_written.load(Ordering::Relaxed),
            point_reads: self.point_reads.load(Ordering::Relaxed),
            range_reads: self.range_reads.load(Ordering::Relaxed),
            scratch_syncs: self.scratch_syncs.load(Ordering::Relaxed),
            stale_runs_reclaimed: self.stale_runs_reclaimed.load(Ordering::Relaxed),
            live_runs_skipped: self.live_runs_skipped.load(Ordering::Relaxed),
            retained_runs_preserved: self.retained_runs_preserved.load(Ordering::Relaxed),
            unclassified_runs_preserved: self.unclassified_runs_preserved.load(Ordering::Relaxed),
        }
    }
}

/// Durable retention mode of one scratch run.
///
/// This is authenticated by the run marker itself, so a run's disposition is a
/// durable property of its own bytes rather than caller-asserted or path-derived
/// state. It is deliberately not an ambient registry: only the exact directory
/// capability plus this marker can classify a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum ScratchRetention {
    /// Reclaimable run. Drop removes it and a restart reclaims stale instances
    /// under the exclusive lease proof.
    Ephemeral,
    /// Adoptable run. Drop releases only the lease, and ordinary stale-run
    /// reclamation never removes it. Until an authenticated runtime-resume-point
    /// format exists, an orphan retained run is preserved rather than deleted.
    Retained,
}

/// Schema-12 durable run marker.
///
/// There is no legacy decode path: schema-11 bytes are rejected, never migrated.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScratchRunMarkerV3 {
    schema_version: u32,
    workspace_id: WorkspaceId,
    run_id: Uuid,
    retention: ScratchRetention,
    random_owner_nonce: [u8; 32],
}

/// What one reclamation pass proved about one sibling scratch run.
///
/// Only `Reclaimed` unlinks bytes, and it is reachable only from a complete
/// authentication of that run's own marker, entry set, and free lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaleRunDisposition {
    /// The reclaiming store's own run.
    OwnRun,
    /// Authenticated ephemeral run of this workspace whose lease was free. Its
    /// bytes were removed.
    Reclaimed,
    /// Authenticated run whose exclusive lease another owner still holds.
    LivePreserved,
    /// Authenticated adoptable run.
    RetainedPreserved,
    /// Could not be authenticated or classified: absent, torn, old-schema,
    /// foreign, or unreadable marker; incomplete or stray entry set; a
    /// non-directory or non-UTF-8 name; or an ordinary per-run I/O error.
    /// Its bytes are preserved untouched.
    Unclassified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ScratchPageKind {
    BatchStatus = 1,
    DependencyWait = 2,
    ReadyQueue = 3,
    CausalBatch = 4,
    CausalDot = 5,
    CausalPeer = 6,
    DocumentCurrent = 7,
    DocumentExact = 8,
    DocumentAfterBatch = 9,
    BlobDedup = 10,
    Conflict = 11,
    LoroHistory = 12,
    DocumentExternalCurrent = 13,
    DocumentExternalExact = 14,
    AcceptedFrontier = 15,
    AcceptedSequenceLeaf = 16,
    AcceptedSequenceNode = 17,
    AcceptedDocumentMap = 18,
    AcceptedBatchMap = 19,
    PageNameCatalogFrontier = 20,
    DependencyFanout = 21,
    DependencyWaitProgress = 22,
    DependencyIdentity = 23,
    DependencyUnresolved = 24,
    CausalClockLength = 25,
    CausalAccumulator = 26,
    CurrentPathCatalog = 27,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchPageRef {
    offset: u64,
    encoded_len: u32,
    digest: ContentDigest,
    kind: ScratchPageKind,
    key_min: Vec<u8>,
    key_max: Vec<u8>,
}

impl ScratchPageRef {
    pub(crate) fn key_min(&self) -> &[u8] {
        &self.key_min
    }

    pub(crate) fn key_max(&self) -> &[u8] {
        &self.key_max
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScratchPageEnvelope {
    schema_version: u32,
    kind: ScratchPageKind,
    key_min: Vec<u8>,
    key_max: Vec<u8>,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchBlobRef {
    offset: u64,
    encoded_len: u32,
    digest: ContentDigest,
}

impl ScratchBlobRef {
    pub(crate) const fn digest(&self) -> ContentDigest {
        self.digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScratchRecord {
    key: Vec<u8>,
    value: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScratchSegment {
    schema_version: u32,
    kind: ScratchPageKind,
    generation: u64,
    entries: Vec<ScratchRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchSegmentRef {
    generation: u64,
    entry_count: u64,
    page_ref: ScratchPageRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchLsmRoot {
    next_generation: u64,
    levels: Vec<Option<ScratchSegmentRef>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchAcceptedSequenceRoot {
    schema_version: u32,
    len: u64,
    height: u8,
    root: Option<ScratchPageRef>,
}

impl Default for ScratchAcceptedSequenceRoot {
    fn default() -> Self {
        Self {
            schema_version: ACCEPTED_SEQUENCE_SCHEMA_VERSION,
            len: 0,
            height: 0,
            root: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedSequenceLeaf {
    schema_version: u32,
    first_sequence: u64,
    entries: Vec<AcceptedSequenceEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedSequenceEntry {
    pub batch_id: BatchId,
    pub evidence: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedSequenceNode {
    schema_version: u32,
    height: u8,
    first_leaf: u64,
    children: Vec<ScratchPageRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchAuthenticatedMapRoot {
    schema_version: u32,
    count: u64,
    root_key: Option<[u8; 16]>,
    root_digest: ContentDigest,
    root: Option<ScratchPageRef>,
}

/// Constant-size authenticated root for the accepted current-page catalog.
///
/// Unlike the digest-only accepted-document map, catalog nodes retain their
/// bounded semantic row value so reconstruction and paging never need a second
/// graph-sized heap mirror.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchAuthenticatedCatalogRoot {
    schema_version: u32,
    count: u64,
    root_key: Option<[u8; 16]>,
    root_digest: ContentDigest,
    root: Option<ScratchPageRef>,
}

impl Default for ScratchAuthenticatedCatalogRoot {
    fn default() -> Self {
        Self {
            schema_version: AUTHENTICATED_CATALOG_SCHEMA_VERSION,
            count: 0,
            root_key: None,
            root_digest: authenticated_catalog_empty_digest(),
            root: None,
        }
    }
}

impl ScratchAuthenticatedCatalogRoot {
    pub(crate) const fn count(&self) -> u64 {
        self.count
    }

    pub(crate) const fn root_digest(&self) -> ContentDigest {
        self.root_digest
    }
}

/// Root of the point-keyed authenticated map used only by bounded operational
/// staging, dependency fanout, and their causal control records.
///
/// A physical key is a domain-separated digest of `(page kind, logical key)`.
/// Every node also retains the complete logical key. A digest match with
/// different logical bytes is therefore rejected as a collision and can never
/// alias two records. Treap traversal is capped at
/// `AUTHENTICATED_POINT_MAX_DEPTH`, keys and values have fixed byte ceilings,
/// and one batched mutation call has a fixed item ceiling. Consequently one
/// point operation has a physical page-I/O and byte bound independent of the
/// current map cardinality.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchAuthenticatedPointRoot {
    schema_version: u32,
    count: u64,
    root_key_digest: Option<ContentDigest>,
    root_digest: ContentDigest,
    root: Option<ScratchPageRef>,
}

impl Default for ScratchAuthenticatedPointRoot {
    fn default() -> Self {
        Self {
            schema_version: AUTHENTICATED_POINT_MAP_SCHEMA_VERSION,
            count: 0,
            root_key_digest: None,
            root_digest: authenticated_point_empty_digest(),
            root: None,
        }
    }
}

impl ScratchAuthenticatedPointRoot {
    pub(crate) const fn count(&self) -> u64 {
        self.count
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchCausalAccumulatorRoot {
    schema_version: u32,
    count: u64,
    root_key: Option<[u8; 16]>,
    root_digest: ContentDigest,
    root: Option<ScratchPageRef>,
}

impl ScratchCausalAccumulatorRoot {
    pub(crate) const fn count(&self) -> u64 {
        self.count
    }
}

impl Default for ScratchCausalAccumulatorRoot {
    fn default() -> Self {
        Self {
            schema_version: CAUSAL_ACCUMULATOR_SCHEMA_VERSION,
            count: 0,
            root_key: None,
            root_digest: causal_accumulator_empty_digest(),
            root: None,
        }
    }
}

impl Default for ScratchAuthenticatedMapRoot {
    fn default() -> Self {
        Self {
            schema_version: AUTHENTICATED_MAP_SCHEMA_VERSION,
            count: 0,
            root_key: None,
            root_digest: authenticated_map_empty_digest(),
            root: None,
        }
    }
}

impl ScratchAuthenticatedMapRoot {
    pub(crate) const fn count(&self) -> u64 {
        self.count
    }

    pub(crate) const fn root_key(&self) -> Option<[u8; 16]> {
        self.root_key
    }

    pub(crate) const fn root_digest(&self) -> ContentDigest {
        self.root_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedMapChild {
    key: [u8; 16],
    digest: ContentDigest,
    page_ref: ScratchPageRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedMapNode {
    schema_version: u32,
    key: [u8; 16],
    priority: ContentDigest,
    value_digest: ContentDigest,
    left: Option<AuthenticatedMapChild>,
    right: Option<AuthenticatedMapChild>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedCatalogNode {
    schema_version: u32,
    key: [u8; 16],
    priority: ContentDigest,
    value: Vec<u8>,
    left: Option<AuthenticatedMapChild>,
    right: Option<AuthenticatedMapChild>,
}

/// Bounded in-order traversal over one pinned authenticated catalog root.
pub(crate) struct ScratchAuthenticatedCatalogCursor<'a> {
    store: &'a ScratchStore,
    stack: Vec<AuthenticatedCatalogNode>,
    current: Option<AuthenticatedMapChild>,
    after: Option<[u8; 16]>,
    initialized: bool,
    visited: usize,
}

impl ScratchAuthenticatedCatalogCursor<'_> {
    pub(crate) fn next_entry(&mut self) -> Result<Option<([u8; 16], Vec<u8>)>, ScratchError> {
        if !self.initialized {
            self.seek_after()?;
            self.initialized = true;
        } else {
            self.descend_left()?;
        }
        let Some(node) = self.stack.pop() else {
            return Ok(None);
        };
        self.current = node.right.clone();
        self.visited = self
            .visited
            .checked_add(1)
            .ok_or(ScratchError::IndexCapacity)?;
        Ok(Some((node.key, node.value)))
    }

    #[cfg(test)]
    pub(crate) const fn visited(&self) -> usize {
        self.visited
    }

    fn seek_after(&mut self) -> Result<(), ScratchError> {
        while let Some(child) = self.current.take() {
            if self.stack.len() > MAX_AUTHENTICATED_MAP_DEPTH {
                return Err(ScratchError::IndexCapacity);
            }
            let node = self.store.read_authenticated_catalog_node(&child)?;
            if self.after.is_some_and(|after| node.key <= after) {
                self.current = node.right;
            } else {
                self.current = node.left.clone();
                self.stack.push(node);
            }
        }
        Ok(())
    }

    fn descend_left(&mut self) -> Result<(), ScratchError> {
        while let Some(child) = self.current.take() {
            if self.stack.len() > MAX_AUTHENTICATED_MAP_DEPTH {
                return Err(ScratchError::IndexCapacity);
            }
            let node = self.store.read_authenticated_catalog_node(&child)?;
            self.current = node.left.clone();
            self.stack.push(node);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedPointChild {
    key_digest: ContentDigest,
    digest: ContentDigest,
    page_ref: ScratchPageRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedPointNode {
    schema_version: u32,
    key_digest: ContentDigest,
    logical_key: Vec<u8>,
    priority: ContentDigest,
    value: Vec<u8>,
    left: Option<AuthenticatedPointChild>,
    right: Option<AuthenticatedPointChild>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CausalAccumulatorNode {
    schema_version: u32,
    key: [u8; 16],
    priority: ContentDigest,
    counter: u64,
    left: Option<AuthenticatedMapChild>,
    right: Option<AuthenticatedMapChild>,
}

impl Default for ScratchLsmRoot {
    fn default() -> Self {
        Self {
            next_generation: 0,
            levels: vec![None; SCRATCH_LSM_LEVELS],
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchRoots {
    pub batch_status_root: ScratchAuthenticatedPointRoot,
    /// Canonical direct-dependency identities keyed by `(child, ordinal)`.
    /// Registration appends exactly one authenticated point per charged
    /// ordinal; the compact staged record never owns the whole sequence.
    pub dependency_root: ScratchAuthenticatedPointRoot,
    /// Live unresolved membership keyed by the same `(child, ordinal)` point.
    /// Fanout deletes exactly the point named by its durable wait edge.
    pub unresolved_dependency_root: ScratchAuthenticatedPointRoot,
    pub wait_root: ScratchAuthenticatedPointRoot,
    /// Per-parent `(registered, drained)` wait-edge ordinals. Wait edges are
    /// keyed by `parent || ordinal`, so the next undrained edge of a final
    /// parent is one point lookup rather than a successor scan over tombstones.
    pub wait_progress_root: ScratchAuthenticatedPointRoot,
    /// Durable dependent-fanout discovery index. `begin_finish` appends one
    /// slot per final parent that still owns live wait edges; a reconstructed
    /// engine rediscovers the exact remaining fanout from `fanout_head`.
    pub fanout_root: ScratchAuthenticatedPointRoot,
    pub fanout_head: u64,
    pub fanout_tail: u64,
    /// Weighted work already derived for the exact current fanout edge. The
    /// remaining credit survives bounded calls and same-process reconstruction.
    pub fanout_work_remaining: Option<u64>,
    /// Number of staged records whose direct-dependency registration is still
    /// point-paged in progress. Registration is durable, so this is the exact
    /// remaining registration continuation after engine reconstruction.
    pub registering_len: u64,
    pub ready_queue_root: ScratchAuthenticatedPointRoot,
    pub ready_queue_len: u64,
    pub causal_root: ScratchAuthenticatedPointRoot,
    pub causal_dot_root: ScratchAuthenticatedPointRoot,
    pub causal_peer_root: ScratchAuthenticatedPointRoot,
    /// Fixed-size causal-clock cardinality records keyed by accepted batch.
    /// Bounded staging uses this point index to derive a parent's merge weight
    /// before reading or traversing that parent's sparse clock.
    pub causal_clock_len_root: ScratchAuthenticatedPointRoot,
    pub document_current_root: ScratchLsmRoot,
    pub document_state_root: ScratchLsmRoot,
    pub document_after_batch_root: ScratchLsmRoot,
    pub blob_dedup_root: ScratchLsmRoot,
    pub conflict_root: ScratchLsmRoot,
    pub external_document_current_root: ScratchLsmRoot,
    pub external_document_state_root: ScratchLsmRoot,
    pub accepted_frontier_root: ScratchLsmRoot,
    pub accepted_sequence_root: ScratchAcceptedSequenceRoot,
    pub accepted_document_map_root: ScratchAuthenticatedMapRoot,
    pub accepted_batch_map_root: ScratchAuthenticatedMapRoot,
}

/// One reconstructible, authenticated run-local scratch namespace.
///
/// The authoritative archive is not reachable through this type. All removal
/// is capability-relative beneath the exact scratch namespace.
pub(crate) struct ScratchStore {
    namespace: Dir,
    run: Dir,
    run_name: String,
    marker: ScratchRunMarkerV3,
    lease: fs::File,
    pages: Mutex<fs::File>,
    blobs: Mutex<fs::File>,
    counters: Arc<ScratchCounters>,
    document_current_filter: Mutex<FixedPointFilter>,
    blob_dedup_filter: Mutex<CoveredBlobDedupFilter>,
}

impl fmt::Debug for ScratchStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScratchStore")
            .field("run_name", &self.run_name)
            .field("workspace_id", &self.marker.workspace_id)
            .finish_non_exhaustive()
    }
}

impl ScratchStore {
    pub(crate) fn open(
        archive_capability: &Dir,
        workspace_id: WorkspaceId,
    ) -> Result<Self, ScratchError> {
        Self::create_run(
            archive_capability,
            workspace_id,
            ScratchRetention::Ephemeral,
        )
    }

    /// Create a fresh adoptable run beneath the same directory capability.
    ///
    /// The only difference from an ordinary run is the durable retention mode
    /// authenticated by its own marker. Creation, lease acquisition, and stale
    /// reclamation are the identical construction.
    pub(crate) fn create_retained(
        archive_capability: &Dir,
        workspace_id: WorkspaceId,
    ) -> Result<Self, ScratchError> {
        Self::create_run(archive_capability, workspace_id, ScratchRetention::Retained)
    }

    /// Construct one fresh run transactionally from the namespace's viewpoint.
    ///
    /// Once the run directory exists, every later failure removes exactly that
    /// directory before returning the original error, so a fallible open, write,
    /// lock, or durability step can never publish a partially constructed run
    /// into the shared namespace. The removal is capability-relative and named:
    /// a sibling run is unreachable from it.
    fn create_run(
        archive_capability: &Dir,
        workspace_id: WorkspaceId,
        retention: ScratchRetention,
    ) -> Result<Self, ScratchError> {
        super::object_store::ensure_directory_nofollow(archive_capability, SCRATCH_DIR)?;
        let namespace = super::object_store::open_dir_nofollow(archive_capability, SCRATCH_DIR)?;
        let run_id = Uuid::new_v4();
        let run_name = format!("run-{run_id}");
        // The run directory must be minted by this exact call. `create_dir`
        // fails closed on any pre-existing entry, including a symlink, so the
        // cleanup below can only ever remove a directory this construction
        // created and never an existing sibling.
        namespace.create_dir(&run_name)?;
        let construction =
            Self::construct_own_run(&namespace, &run_name, run_id, workspace_id, retention);
        let store = match construction {
            Ok(store) => store,
            Err(error) => {
                remove_partial_own_run(&namespace, &run_name);
                return Err(error);
            }
        };
        // Reclamation is opportunistic garbage collection of disposable
        // sibling state. It is infallible by construction and carries no veto
        // over this fresh run.
        store.reclaim_stale_runs();
        #[cfg(test)]
        if let Err(error) = inject_create_run_fault(ScratchCreateBoundary::AfterReclaim) {
            // The identity was never returned to a caller and the run holds no
            // data yet, so removing it cannot orphan adoptable bytes.
            store.cleanup_own_run();
            return Err(error);
        }
        Ok(store)
    }

    /// Everything after the run directory exists. Every fallible step here is
    /// covered by the caller's removal of that exact run.
    fn construct_own_run(
        namespace: &Dir,
        run_name: &str,
        run_id: Uuid,
        workspace_id: WorkspaceId,
        retention: ScratchRetention,
    ) -> Result<Self, ScratchError> {
        #[cfg(test)]
        inject_create_run_fault(ScratchCreateBoundary::AfterRunDirectory)?;
        super::object_store::sync_dir_required(namespace)?;
        #[cfg(test)]
        inject_create_run_fault(ScratchCreateBoundary::AfterNamespaceSync)?;
        let run = super::object_store::open_dir_nofollow(namespace, run_name)?;
        #[cfg(test)]
        inject_create_run_fault(ScratchCreateBoundary::AfterRunOpen)?;
        let nonce_a = Uuid::new_v4();
        let nonce_b = Uuid::new_v4();
        let mut random_owner_nonce = [0_u8; 32];
        random_owner_nonce[..16].copy_from_slice(nonce_a.as_bytes());
        random_owner_nonce[16..].copy_from_slice(nonce_b.as_bytes());
        let marker = ScratchRunMarkerV3 {
            schema_version: SCRATCH_SCHEMA_VERSION,
            workspace_id,
            run_id,
            retention,
            random_owner_nonce,
        };
        write_new_regular(&run, MARKER_FILE, &encode_canonical(&marker)?)?;
        #[cfg(test)]
        inject_create_run_fault(ScratchCreateBoundary::AfterMarkerWrite)?;
        let lease = create_new_regular(&run, LEASE_FILE)?;
        #[cfg(test)]
        inject_create_run_fault(ScratchCreateBoundary::AfterLeaseCreate)?;
        lock_exclusive_nonblocking(&lease)?
            .then_some(())
            .ok_or_else(|| {
                ScratchError::UnsafeEntry("new scratch lease was already locked".into())
            })?;
        #[cfg(test)]
        inject_create_run_fault(ScratchCreateBoundary::AfterLeaseLock)?;
        let pages = create_new_regular(&run, PAGES_FILE)?;
        #[cfg(test)]
        inject_create_run_fault(ScratchCreateBoundary::AfterPagesCreate)?;
        let blobs = create_new_regular(&run, BLOBS_FILE)?;
        #[cfg(test)]
        inject_create_run_fault(ScratchCreateBoundary::AfterBlobsCreate)?;
        Ok(Self {
            namespace: namespace.try_clone()?,
            run,
            run_name: run_name.to_owned(),
            marker,
            lease,
            pages: Mutex::new(pages),
            blobs: Mutex::new(blobs),
            counters: Arc::new(ScratchCounters::default()),
            document_current_filter: Mutex::new(FixedPointFilter::default()),
            blob_dedup_filter: Mutex::new(CoveredBlobDedupFilter::default()),
        })
    }

    /// Adopt the retained run with exactly this run identity.
    ///
    /// Authority is the supplied directory capability plus the run's own durable
    /// marker; nothing is derived from an ambient path or a global registry.
    /// Adoption never creates a directory, marker, lease, or data file, so a
    /// missing, substituted, foreign, or ephemeral run fails closed instead of
    /// silently becoming a fresh empty run under the requested identity.
    pub(crate) fn adopt_retained(
        archive_capability: &Dir,
        workspace_id: WorkspaceId,
        run_id: Uuid,
    ) -> Result<Self, ScratchError> {
        let namespace = super::object_store::open_dir_nofollow(archive_capability, SCRATCH_DIR)?;
        let run_name = format!("run-{run_id}");
        // Reject a non-canonical identity spelling before it can name a
        // directory; `open_dir_nofollow` then rejects a symlinked alias.
        if parse_run_name(&run_name)? != run_id {
            return Err(ScratchError::MalformedMarker(run_name));
        }
        let run = super::object_store::open_dir_nofollow(&namespace, &run_name)?;

        // Take the exact existing exclusive run lease first, so every later
        // validation observes a run no other owner may be mutating. A failed
        // validation drops this file and releases the lock.
        let lease = open_regular_read_write_nofollow(&run, LEASE_FILE)?;
        if !lock_exclusive_nonblocking(&lease)? {
            return Err(ScratchError::UnsafeEntry(format!(
                "retained scratch run {run_name:?} is still leased"
            )));
        }

        let marker_bytes = read_regular_nofollow(&run, MARKER_FILE, MAX_MARKER_BYTES)?;
        let marker: ScratchRunMarkerV3 = decode_canonical(&marker_bytes)?;
        if marker.schema_version != SCRATCH_SCHEMA_VERSION
            || marker.workspace_id != workspace_id
            || marker.run_id != run_id
            || marker.retention != ScratchRetention::Retained
        {
            return Err(ScratchError::MalformedMarker(run_name));
        }
        validate_run_entries(&run)?;

        // Opened read-write with no create, truncate, or replacement publication:
        // existing page and blob bytes remain exactly as the previous owner left
        // them, and later appends land after them.
        let pages = open_regular_read_write_nofollow(&run, PAGES_FILE)?;
        let blobs = open_regular_read_write_nofollow(&run, BLOBS_FILE)?;

        // This process never observed the inserts behind the adopted bytes, so
        // the document-current negative filter must not answer "absent". The
        // covered blob-dedup filter is already fail-closed for unseen roots: it
        // recognizes only exact roots reached from a covered parent, and an
        // adopted root is not one.
        Ok(Self {
            namespace,
            run,
            run_name,
            marker,
            lease,
            pages: Mutex::new(pages),
            blobs: Mutex::new(blobs),
            counters: Arc::new(ScratchCounters::default()),
            document_current_filter: Mutex::new(FixedPointFilter::saturated()),
            blob_dedup_filter: Mutex::new(CoveredBlobDedupFilter::default()),
        })
    }

    pub(crate) const fn run_id(&self) -> Uuid {
        self.marker.run_id
    }

    #[cfg(test)]
    const fn retention(&self) -> ScratchRetention {
        self.marker.retention
    }

    pub(crate) fn stats(&self) -> ScratchStats {
        self.counters.snapshot()
    }

    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.marker.workspace_id
    }

    #[cfg(test)]
    pub(crate) fn truncate_pages_for_test(&self) {
        self.pages
            .lock()
            .expect("scratch pages lock")
            .set_len(0)
            .expect("truncate scratch pages");
    }

    #[cfg(test)]
    pub(crate) fn tamper_page_byte_for_test(&self, offset: u64) {
        let mut pages = self.pages.lock().expect("scratch pages lock");
        pages
            .seek(SeekFrom::Start(offset))
            .expect("seek scratch page");
        let mut byte = [0_u8; 1];
        pages.read_exact(&mut byte).expect("read scratch page byte");
        byte[0] ^= 0x80;
        pages
            .seek(SeekFrom::Start(offset))
            .expect("seek scratch page");
        pages.write_all(&byte).expect("tamper scratch page byte");
    }

    #[cfg(test)]
    pub(crate) fn tamper_authenticated_catalog_root_for_test(
        &self,
        root: &ScratchAuthenticatedCatalogRoot,
    ) {
        let offset = root
            .root
            .as_ref()
            .expect("nonempty authenticated catalog root")
            .offset;
        self.tamper_page_byte_for_test(offset);
    }

    #[cfg(test)]
    pub(crate) fn misbind_page_ref_for_test(page_ref: &mut ScratchPageRef) {
        page_ref.kind = ScratchPageKind::BatchStatus;
    }

    pub(crate) fn binding_digest(&self) -> Result<ContentDigest, ScratchError> {
        Ok(ContentDigest::of(&encode_canonical(&self.marker)?))
    }

    pub(crate) fn clone_pages_file(&self) -> Result<fs::File, ScratchError> {
        self.pages
            .lock()
            .map_err(|_| ScratchError::Poisoned)?
            .try_clone()
            .map_err(Into::into)
    }

    pub(crate) fn insert_many(
        &self,
        root: &ScratchLsmRoot,
        kind: ScratchPageKind,
        records: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Result<ScratchLsmRoot, ScratchError> {
        if records.is_empty() {
            return Ok(root.clone());
        }
        validate_root(root)?;
        let generation = root
            .next_generation
            .checked_add(1)
            .ok_or(ScratchError::MalformedPage)?;
        let mut merged = records.clone();
        let mut next = root.clone();
        next.next_generation = generation;
        for level in 0..SCRATCH_LSM_LEVELS {
            if let Some(existing) = next.levels[level].take() {
                let old = self.read_segment(kind, &existing)?;
                for record in old.entries {
                    merged.entry(record.key).or_insert(record.value);
                }
                continue;
            }
            let entries = merged
                .into_iter()
                .map(|(key, value)| ScratchRecord { key, value })
                .collect::<Vec<_>>();
            let segment = ScratchSegment {
                schema_version: SCRATCH_PAGE_SCHEMA_VERSION,
                kind,
                generation,
                entries,
            };
            validate_segment(&segment)?;
            let key_min = segment
                .entries
                .first()
                .expect("nonempty insertion")
                .key
                .clone();
            let key_max = segment
                .entries
                .last()
                .expect("nonempty insertion")
                .key
                .clone();
            let page_ref = self.append_page(kind, key_min, key_max, &segment)?;
            next.levels[level] = Some(ScratchSegmentRef {
                generation,
                entry_count: segment.entries.len() as u64,
                page_ref,
            });
            if kind == ScratchPageKind::DocumentCurrent {
                let mut filter = self
                    .document_current_filter
                    .lock()
                    .map_err(|_| ScratchError::Poisoned)?;
                for (key, value) in records {
                    if value.is_some() {
                        filter.insert(key);
                    }
                }
            }
            if kind == ScratchPageKind::BlobDedup {
                self.blob_dedup_filter
                    .lock()
                    .map_err(|_| ScratchError::Poisoned)?
                    .record_insert(root, &next, records);
            }
            return Ok(next);
        }
        Err(ScratchError::IndexCapacity)
    }

    pub(crate) fn lookup(
        &self,
        root: &ScratchLsmRoot,
        kind: ScratchPageKind,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, ScratchError> {
        validate_root(root)?;
        self.counters.point_reads.fetch_add(1, Ordering::Relaxed);
        if kind == ScratchPageKind::DocumentCurrent
            && !self
                .document_current_filter
                .lock()
                .map_err(|_| ScratchError::Poisoned)?
                .might_contain(key)
        {
            return Ok(None);
        }
        if kind == ScratchPageKind::BlobDedup
            && self
                .blob_dedup_filter
                .lock()
                .map_err(|_| ScratchError::Poisoned)?
                .proves_absent(root, key)
        {
            return Ok(None);
        }
        let mut segments = root
            .levels
            .iter()
            .flatten()
            .collect::<Vec<&ScratchSegmentRef>>();
        segments.sort_unstable_by_key(|segment| std::cmp::Reverse(segment.generation));
        for segment_ref in segments {
            if key < segment_ref.page_ref.key_min.as_slice()
                || key > segment_ref.page_ref.key_max.as_slice()
            {
                continue;
            }
            let segment = self.read_segment(kind, segment_ref)?;
            if let Ok(index) = segment
                .entries
                .binary_search_by(|record| record.key.as_slice().cmp(key))
            {
                return Ok(segment.entries[index].value.clone());
            }
        }
        Ok(None)
    }

    pub(crate) fn authenticated_point_lookup(
        &self,
        root: &ScratchAuthenticatedPointRoot,
        kind: ScratchPageKind,
        logical_key: &[u8],
    ) -> Result<Option<Vec<u8>>, ScratchError> {
        validate_authenticated_point_root(root)?;
        validate_authenticated_point_key(logical_key)?;
        self.counters.point_reads.fetch_add(1, Ordering::Relaxed);
        let key_digest = authenticated_point_key_digest(kind, logical_key);
        let mut current = root.root.as_ref().map(|page_ref| AuthenticatedPointChild {
            key_digest: root
                .root_key_digest
                .expect("validated nonempty point root key"),
            digest: root.root_digest,
            page_ref: page_ref.clone(),
        });
        for _ in 0..=AUTHENTICATED_POINT_MAX_DEPTH {
            let Some(child) = current else {
                return Ok(None);
            };
            let node = self.read_authenticated_point_node(kind, &child)?;
            match key_digest.cmp(&node.key_digest) {
                std::cmp::Ordering::Equal => {
                    if node.logical_key != logical_key {
                        return Err(ScratchError::KeyDigestCollision);
                    }
                    return Ok(Some(node.value));
                }
                std::cmp::Ordering::Less => current = node.left,
                std::cmp::Ordering::Greater => current = node.right,
            }
        }
        Err(ScratchError::IndexCapacity)
    }

    /// Apply a fixed-size collection of independent point mutations.
    ///
    /// Unlike the binary LSM, this never carries or rewrites a prior segment.
    /// Each item performs one bounded authenticated-tree operation. The
    /// collection ceiling covers the largest ready-heap path (one slot per bit
    /// of a `u64`, plus its terminal slot).
    pub(crate) fn authenticated_point_apply(
        &self,
        root: &ScratchAuthenticatedPointRoot,
        kind: ScratchPageKind,
        records: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Result<ScratchAuthenticatedPointRoot, ScratchError> {
        if records.len() > AUTHENTICATED_POINT_MAX_MUTATIONS {
            return Err(ScratchError::IndexCapacity);
        }
        let mut next = root.clone();
        for (key, value) in records {
            next = match value {
                Some(value) => self.authenticated_point_upsert(&next, kind, key, value)?,
                None => self.authenticated_point_remove(&next, kind, key)?,
            };
        }
        Ok(next)
    }

    pub(crate) fn authenticated_point_upsert(
        &self,
        root: &ScratchAuthenticatedPointRoot,
        kind: ScratchPageKind,
        logical_key: &[u8],
        value: &[u8],
    ) -> Result<ScratchAuthenticatedPointRoot, ScratchError> {
        validate_authenticated_point_root(root)?;
        validate_authenticated_point_key(logical_key)?;
        if value.len() > AUTHENTICATED_POINT_MAX_VALUE_BYTES {
            return Err(ScratchError::MalformedPage);
        }
        let key_digest = authenticated_point_key_digest(kind, logical_key);
        let (child, inserted) = self.authenticated_point_upsert_child(
            kind,
            root.root.as_ref().map(|page_ref| AuthenticatedPointChild {
                key_digest: root
                    .root_key_digest
                    .expect("validated nonempty point root key"),
                digest: root.root_digest,
                page_ref: page_ref.clone(),
            }),
            key_digest,
            logical_key,
            value,
            0,
        )?;
        let next = ScratchAuthenticatedPointRoot {
            schema_version: AUTHENTICATED_POINT_MAP_SCHEMA_VERSION,
            count: if inserted {
                root.count
                    .checked_add(1)
                    .ok_or(ScratchError::IndexCapacity)?
            } else {
                root.count
            },
            root_key_digest: Some(child.key_digest),
            root_digest: child.digest,
            root: Some(child.page_ref),
        };
        validate_authenticated_point_root(&next)?;
        Ok(next)
    }

    fn authenticated_point_upsert_child(
        &self,
        kind: ScratchPageKind,
        current: Option<AuthenticatedPointChild>,
        key_digest: ContentDigest,
        logical_key: &[u8],
        value: &[u8],
        depth: usize,
    ) -> Result<(AuthenticatedPointChild, bool), ScratchError> {
        if depth > AUTHENTICATED_POINT_MAX_DEPTH {
            return Err(ScratchError::IndexCapacity);
        }
        let Some(current) = current else {
            let node = AuthenticatedPointNode {
                schema_version: AUTHENTICATED_POINT_MAP_SCHEMA_VERSION,
                key_digest,
                logical_key: logical_key.to_vec(),
                priority: authenticated_point_priority(key_digest),
                value: value.to_vec(),
                left: None,
                right: None,
            };
            return Ok((self.write_authenticated_point_node(kind, &node)?, true));
        };
        let mut node = self.read_authenticated_point_node(kind, &current)?;
        let inserted;
        match key_digest.cmp(&node.key_digest) {
            std::cmp::Ordering::Equal => {
                if node.logical_key != logical_key {
                    return Err(ScratchError::KeyDigestCollision);
                }
                node.value = value.to_vec();
                inserted = false;
            }
            std::cmp::Ordering::Less => {
                let (left, was_inserted) = self.authenticated_point_upsert_child(
                    kind,
                    node.left.take(),
                    key_digest,
                    logical_key,
                    value,
                    depth + 1,
                )?;
                node.left = Some(left);
                inserted = was_inserted;
                if node.left.as_ref().is_some_and(|left| {
                    authenticated_point_priority_order(left.key_digest, node.key_digest).is_lt()
                }) {
                    return Ok((self.rotate_authenticated_point_right(kind, node)?, inserted));
                }
            }
            std::cmp::Ordering::Greater => {
                let (right, was_inserted) = self.authenticated_point_upsert_child(
                    kind,
                    node.right.take(),
                    key_digest,
                    logical_key,
                    value,
                    depth + 1,
                )?;
                node.right = Some(right);
                inserted = was_inserted;
                if node.right.as_ref().is_some_and(|right| {
                    authenticated_point_priority_order(right.key_digest, node.key_digest).is_lt()
                }) {
                    return Ok((self.rotate_authenticated_point_left(kind, node)?, inserted));
                }
            }
        }
        Ok((self.write_authenticated_point_node(kind, &node)?, inserted))
    }

    fn rotate_authenticated_point_right(
        &self,
        kind: ScratchPageKind,
        mut node: AuthenticatedPointNode,
    ) -> Result<AuthenticatedPointChild, ScratchError> {
        let left = node.left.take().ok_or(ScratchError::MalformedPage)?;
        let mut left_node = self.read_authenticated_point_node(kind, &left)?;
        node.left = left_node.right.take();
        left_node.right = Some(self.write_authenticated_point_node(kind, &node)?);
        self.write_authenticated_point_node(kind, &left_node)
    }

    fn rotate_authenticated_point_left(
        &self,
        kind: ScratchPageKind,
        mut node: AuthenticatedPointNode,
    ) -> Result<AuthenticatedPointChild, ScratchError> {
        let right = node.right.take().ok_or(ScratchError::MalformedPage)?;
        let mut right_node = self.read_authenticated_point_node(kind, &right)?;
        node.right = right_node.left.take();
        right_node.left = Some(self.write_authenticated_point_node(kind, &node)?);
        self.write_authenticated_point_node(kind, &right_node)
    }

    fn authenticated_point_remove(
        &self,
        root: &ScratchAuthenticatedPointRoot,
        kind: ScratchPageKind,
        logical_key: &[u8],
    ) -> Result<ScratchAuthenticatedPointRoot, ScratchError> {
        validate_authenticated_point_root(root)?;
        validate_authenticated_point_key(logical_key)?;
        let key_digest = authenticated_point_key_digest(kind, logical_key);
        let (child, removed) = self.authenticated_point_remove_child(
            kind,
            root.root.as_ref().map(|page_ref| AuthenticatedPointChild {
                key_digest: root
                    .root_key_digest
                    .expect("validated nonempty point root key"),
                digest: root.root_digest,
                page_ref: page_ref.clone(),
            }),
            key_digest,
            logical_key,
            0,
        )?;
        if !removed {
            return Ok(root.clone());
        }
        let count = root
            .count
            .checked_sub(1)
            .ok_or(ScratchError::MalformedPage)?;
        let next = match child {
            Some(child) => ScratchAuthenticatedPointRoot {
                schema_version: AUTHENTICATED_POINT_MAP_SCHEMA_VERSION,
                count,
                root_key_digest: Some(child.key_digest),
                root_digest: child.digest,
                root: Some(child.page_ref),
            },
            None if count == 0 => ScratchAuthenticatedPointRoot::default(),
            None => return Err(ScratchError::MalformedPage),
        };
        validate_authenticated_point_root(&next)?;
        Ok(next)
    }

    fn authenticated_point_remove_child(
        &self,
        kind: ScratchPageKind,
        current: Option<AuthenticatedPointChild>,
        key_digest: ContentDigest,
        logical_key: &[u8],
        depth: usize,
    ) -> Result<(Option<AuthenticatedPointChild>, bool), ScratchError> {
        if depth > AUTHENTICATED_POINT_MAX_DEPTH {
            return Err(ScratchError::IndexCapacity);
        }
        let Some(current) = current else {
            return Ok((None, false));
        };
        let mut node = self.read_authenticated_point_node(kind, &current)?;
        match key_digest.cmp(&node.key_digest) {
            std::cmp::Ordering::Equal => {
                if node.logical_key != logical_key {
                    return Err(ScratchError::KeyDigestCollision);
                }
                Ok((
                    self.merge_authenticated_point_children(
                        kind,
                        node.left,
                        node.right,
                        depth + 1,
                    )?,
                    true,
                ))
            }
            std::cmp::Ordering::Less => {
                let (left, removed) = self.authenticated_point_remove_child(
                    kind,
                    node.left.take(),
                    key_digest,
                    logical_key,
                    depth + 1,
                )?;
                if !removed {
                    return Ok((Some(current), false));
                }
                node.left = left;
                Ok((
                    Some(self.write_authenticated_point_node(kind, &node)?),
                    true,
                ))
            }
            std::cmp::Ordering::Greater => {
                let (right, removed) = self.authenticated_point_remove_child(
                    kind,
                    node.right.take(),
                    key_digest,
                    logical_key,
                    depth + 1,
                )?;
                if !removed {
                    return Ok((Some(current), false));
                }
                node.right = right;
                Ok((
                    Some(self.write_authenticated_point_node(kind, &node)?),
                    true,
                ))
            }
        }
    }

    fn merge_authenticated_point_children(
        &self,
        kind: ScratchPageKind,
        left: Option<AuthenticatedPointChild>,
        right: Option<AuthenticatedPointChild>,
        depth: usize,
    ) -> Result<Option<AuthenticatedPointChild>, ScratchError> {
        if depth > AUTHENTICATED_POINT_MAX_DEPTH {
            return Err(ScratchError::IndexCapacity);
        }
        let (left, right) = match (left, right) {
            (Some(left), Some(right)) => (left, right),
            (left, right) => return Ok(left.or(right)),
        };
        if authenticated_point_priority_order(left.key_digest, right.key_digest).is_lt() {
            let mut node = self.read_authenticated_point_node(kind, &left)?;
            node.right = self.merge_authenticated_point_children(
                kind,
                node.right.take(),
                Some(right),
                depth + 1,
            )?;
            Ok(Some(self.write_authenticated_point_node(kind, &node)?))
        } else {
            let mut node = self.read_authenticated_point_node(kind, &right)?;
            node.left = self.merge_authenticated_point_children(
                kind,
                Some(left),
                node.left.take(),
                depth + 1,
            )?;
            Ok(Some(self.write_authenticated_point_node(kind, &node)?))
        }
    }

    pub(crate) fn authenticated_point_materialize(
        &self,
        root: &ScratchAuthenticatedPointRoot,
        kind: ScratchPageKind,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ScratchError> {
        validate_authenticated_point_root(root)?;
        self.counters.range_reads.fetch_add(1, Ordering::Relaxed);
        let mut entries = Vec::with_capacity(root.count as usize);
        let mut stack = Vec::<AuthenticatedPointChild>::new();
        let mut current = root.root.as_ref().map(|page_ref| AuthenticatedPointChild {
            key_digest: root
                .root_key_digest
                .expect("validated nonempty point root key"),
            digest: root.root_digest,
            page_ref: page_ref.clone(),
        });
        while current.is_some() || !stack.is_empty() {
            while let Some(child) = current.take() {
                let node = self.read_authenticated_point_node(kind, &child)?;
                current = node.left.clone();
                stack.push(child);
            }
            let child = stack.pop().expect("nonempty point traversal stack");
            let node = self.read_authenticated_point_node(kind, &child)?;
            entries.push((node.logical_key, node.value));
            current = node.right;
        }
        if entries.len() != root.count as usize {
            return Err(ScratchError::MalformedPage);
        }
        Ok(entries)
    }

    pub(crate) fn authenticated_point_scan_prefix(
        &self,
        root: &ScratchAuthenticatedPointRoot,
        kind: ScratchPageKind,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ScratchError> {
        Ok(self
            .authenticated_point_materialize(root, kind)?
            .into_iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .collect())
    }

    pub(crate) fn append_accepted_sequence(
        &self,
        root: &ScratchAcceptedSequenceRoot,
        sequence: u64,
        batch_id: BatchId,
        evidence: Vec<u8>,
    ) -> Result<ScratchAcceptedSequenceRoot, ScratchError> {
        validate_accepted_sequence_root(root)?;
        if sequence == 0 || sequence != root.len.saturating_add(1) {
            return Err(ScratchError::MalformedPage);
        }
        let leaf_index = (sequence - 1) / ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64;
        let (page_ref, height) = match &root.root {
            None => (
                self.write_accepted_sequence_leaf(
                    sequence,
                    vec![AcceptedSequenceEntry { batch_id, evidence }],
                )?,
                0,
            ),
            Some(current)
                if leaf_index
                    < accepted_sequence_leaf_capacity(root.height)
                        .ok_or(ScratchError::IndexCapacity)? =>
            {
                (
                    self.append_accepted_sequence_at(
                        current,
                        root.height,
                        0,
                        leaf_index,
                        sequence,
                        batch_id,
                        evidence,
                    )?,
                    root.height,
                )
            }
            Some(current) => {
                let height = root
                    .height
                    .checked_add(1)
                    .ok_or(ScratchError::IndexCapacity)?;
                let new_child = self.build_accepted_sequence_path(
                    root.height,
                    leaf_index,
                    sequence,
                    batch_id,
                    evidence,
                )?;
                let node = AcceptedSequenceNode {
                    schema_version: ACCEPTED_SEQUENCE_SCHEMA_VERSION,
                    height,
                    first_leaf: 0,
                    children: vec![current.clone(), new_child],
                };
                (self.write_accepted_sequence_node(&node)?, height)
            }
        };
        let next = ScratchAcceptedSequenceRoot {
            schema_version: ACCEPTED_SEQUENCE_SCHEMA_VERSION,
            len: sequence,
            height,
            root: Some(page_ref),
        };
        validate_accepted_sequence_root(&next)?;
        Ok(next)
    }

    pub(crate) fn lookup_accepted_sequence(
        &self,
        root: &ScratchAcceptedSequenceRoot,
        sequence: u64,
    ) -> Result<Option<AcceptedSequenceEntry>, ScratchError> {
        validate_accepted_sequence_root(root)?;
        self.counters.point_reads.fetch_add(1, Ordering::Relaxed);
        if sequence == 0 || sequence > root.len {
            return Ok(None);
        }
        let leaf_index = (sequence - 1) / ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64;
        let mut page_ref = root.root.clone().ok_or(ScratchError::MalformedPage)?;
        let mut height = root.height;
        let mut first_leaf = 0_u64;
        while height > 0 {
            let node = self.read_accepted_sequence_node(&page_ref, height, first_leaf)?;
            let child_capacity =
                accepted_sequence_leaf_capacity(height - 1).ok_or(ScratchError::IndexCapacity)?;
            let slot = usize::try_from((leaf_index - first_leaf) / child_capacity)
                .map_err(|_| ScratchError::MalformedPage)?;
            page_ref = node
                .children
                .get(slot)
                .cloned()
                .ok_or(ScratchError::MalformedPage)?;
            first_leaf = first_leaf
                .checked_add(
                    u64::try_from(slot)
                        .map_err(|_| ScratchError::MalformedPage)?
                        .saturating_mul(child_capacity),
                )
                .ok_or(ScratchError::MalformedPage)?;
            height -= 1;
        }
        let leaf = self.read_accepted_sequence_leaf(&page_ref, first_leaf)?;
        let offset = usize::try_from((sequence - 1) % ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64)
            .map_err(|_| ScratchError::MalformedPage)?;
        leaf.entries
            .get(offset)
            .cloned()
            .ok_or(ScratchError::MalformedPage)
            .map(Some)
    }

    pub(crate) fn accepted_sequence_cursor<'a>(
        &'a self,
        root: &'a ScratchAcceptedSequenceRoot,
    ) -> Result<ScratchAcceptedSequenceCursor<'a>, ScratchError> {
        validate_accepted_sequence_root(root)?;
        Ok(ScratchAcceptedSequenceCursor {
            store: self,
            root,
            stack: Vec::new(),
            leaf: None,
            next_sequence: 1,
            initialized: false,
            page_reads: 0,
            page_bytes_read: 0,
            max_page_bytes_read: 0,
        })
    }

    pub(crate) fn authenticated_map_upsert(
        &self,
        root: &ScratchAuthenticatedMapRoot,
        key: [u8; 16],
        value_digest: ContentDigest,
    ) -> Result<ScratchAuthenticatedMapRoot, ScratchError> {
        self.authenticated_map_upsert_for_kind(
            ScratchPageKind::AcceptedDocumentMap,
            root,
            key,
            value_digest,
        )
    }

    pub(crate) fn accepted_batch_map_upsert(
        &self,
        root: &ScratchAuthenticatedMapRoot,
        key: [u8; 16],
        value_digest: ContentDigest,
    ) -> Result<ScratchAuthenticatedMapRoot, ScratchError> {
        self.authenticated_map_upsert_for_kind(
            ScratchPageKind::AcceptedBatchMap,
            root,
            key,
            value_digest,
        )
    }

    pub(crate) fn authenticated_catalog_cursor<'a>(
        &'a self,
        root: &ScratchAuthenticatedCatalogRoot,
        after: Option<[u8; 16]>,
    ) -> Result<ScratchAuthenticatedCatalogCursor<'a>, ScratchError> {
        validate_authenticated_catalog_root(root)?;
        self.counters.range_reads.fetch_add(1, Ordering::Relaxed);
        Ok(ScratchAuthenticatedCatalogCursor {
            store: self,
            stack: Vec::new(),
            current: root.root.as_ref().map(|page_ref| AuthenticatedMapChild {
                key: root.root_key.expect("validated nonempty catalog root"),
                digest: root.root_digest,
                page_ref: page_ref.clone(),
            }),
            after,
            initialized: false,
            visited: 0,
        })
    }

    pub(crate) fn authenticated_catalog_lookup(
        &self,
        root: &ScratchAuthenticatedCatalogRoot,
        key: [u8; 16],
    ) -> Result<Option<Vec<u8>>, ScratchError> {
        validate_authenticated_catalog_root(root)?;
        self.counters.point_reads.fetch_add(1, Ordering::Relaxed);
        let mut current = root.root.as_ref().map(|page_ref| AuthenticatedMapChild {
            key: root.root_key.expect("validated nonempty catalog root"),
            digest: root.root_digest,
            page_ref: page_ref.clone(),
        });
        for _ in 0..=MAX_AUTHENTICATED_MAP_DEPTH {
            let Some(child) = current else {
                return Ok(None);
            };
            let node = self.read_authenticated_catalog_node(&child)?;
            match key.cmp(&node.key) {
                std::cmp::Ordering::Equal => return Ok(Some(node.value)),
                std::cmp::Ordering::Less => current = node.left,
                std::cmp::Ordering::Greater => current = node.right,
            }
        }
        Err(ScratchError::IndexCapacity)
    }

    pub(crate) fn authenticated_catalog_upsert(
        &self,
        root: &ScratchAuthenticatedCatalogRoot,
        key: [u8; 16],
        value: &[u8],
    ) -> Result<ScratchAuthenticatedCatalogRoot, ScratchError> {
        validate_authenticated_catalog_root(root)?;
        if value.is_empty() || value.len() > MAX_AUTHENTICATED_CATALOG_VALUE_BYTES {
            return Err(ScratchError::MalformedPage);
        }
        let (child, inserted) = self.authenticated_catalog_upsert_child(
            root.root.as_ref().map(|page_ref| AuthenticatedMapChild {
                key: root.root_key.expect("validated nonempty catalog root"),
                digest: root.root_digest,
                page_ref: page_ref.clone(),
            }),
            key,
            value,
            0,
        )?;
        let next = ScratchAuthenticatedCatalogRoot {
            schema_version: AUTHENTICATED_CATALOG_SCHEMA_VERSION,
            count: if inserted {
                root.count
                    .checked_add(1)
                    .ok_or(ScratchError::IndexCapacity)?
            } else {
                root.count
            },
            root_key: Some(child.key),
            root_digest: child.digest,
            root: Some(child.page_ref),
        };
        validate_authenticated_catalog_root(&next)?;
        Ok(next)
    }

    fn authenticated_catalog_upsert_child(
        &self,
        current: Option<AuthenticatedMapChild>,
        key: [u8; 16],
        value: &[u8],
        depth: usize,
    ) -> Result<(AuthenticatedMapChild, bool), ScratchError> {
        if depth > MAX_AUTHENTICATED_MAP_DEPTH {
            return Err(ScratchError::IndexCapacity);
        }
        let Some(current) = current else {
            let node = AuthenticatedCatalogNode {
                schema_version: AUTHENTICATED_CATALOG_SCHEMA_VERSION,
                key,
                priority: authenticated_map_priority(key),
                value: value.to_vec(),
                left: None,
                right: None,
            };
            return Ok((self.write_authenticated_catalog_node(&node)?, true));
        };
        let mut node = self.read_authenticated_catalog_node(&current)?;
        let inserted;
        match key.cmp(&node.key) {
            std::cmp::Ordering::Equal => {
                node.value = value.to_vec();
                inserted = false;
            }
            std::cmp::Ordering::Less => {
                let (left, was_inserted) = self.authenticated_catalog_upsert_child(
                    node.left.take(),
                    key,
                    value,
                    depth + 1,
                )?;
                node.left = Some(left);
                inserted = was_inserted;
                if node.left.as_ref().is_some_and(|left| {
                    authenticated_map_priority_order(left.key, node.key).is_lt()
                }) {
                    return Ok((self.rotate_authenticated_catalog_right(node)?, inserted));
                }
            }
            std::cmp::Ordering::Greater => {
                let (right, was_inserted) = self.authenticated_catalog_upsert_child(
                    node.right.take(),
                    key,
                    value,
                    depth + 1,
                )?;
                node.right = Some(right);
                inserted = was_inserted;
                if node.right.as_ref().is_some_and(|right| {
                    authenticated_map_priority_order(right.key, node.key).is_lt()
                }) {
                    return Ok((self.rotate_authenticated_catalog_left(node)?, inserted));
                }
            }
        }
        Ok((self.write_authenticated_catalog_node(&node)?, inserted))
    }

    fn rotate_authenticated_catalog_right(
        &self,
        mut node: AuthenticatedCatalogNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        let left = node.left.take().ok_or(ScratchError::MalformedPage)?;
        let mut left_node = self.read_authenticated_catalog_node(&left)?;
        node.left = left_node.right.take();
        left_node.right = Some(self.write_authenticated_catalog_node(&node)?);
        self.write_authenticated_catalog_node(&left_node)
    }

    fn rotate_authenticated_catalog_left(
        &self,
        mut node: AuthenticatedCatalogNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        let right = node.right.take().ok_or(ScratchError::MalformedPage)?;
        let mut right_node = self.read_authenticated_catalog_node(&right)?;
        node.right = right_node.left.take();
        right_node.left = Some(self.write_authenticated_catalog_node(&node)?);
        self.write_authenticated_catalog_node(&right_node)
    }

    pub(crate) fn authenticated_catalog_remove(
        &self,
        root: &ScratchAuthenticatedCatalogRoot,
        key: [u8; 16],
    ) -> Result<ScratchAuthenticatedCatalogRoot, ScratchError> {
        validate_authenticated_catalog_root(root)?;
        let (child, removed) = self.authenticated_catalog_remove_child(
            root.root.as_ref().map(|page_ref| AuthenticatedMapChild {
                key: root.root_key.expect("validated nonempty catalog root"),
                digest: root.root_digest,
                page_ref: page_ref.clone(),
            }),
            key,
            0,
        )?;
        if !removed {
            return Ok(root.clone());
        }
        let count = root
            .count
            .checked_sub(1)
            .ok_or(ScratchError::MalformedPage)?;
        let next = match child {
            Some(child) => ScratchAuthenticatedCatalogRoot {
                schema_version: AUTHENTICATED_CATALOG_SCHEMA_VERSION,
                count,
                root_key: Some(child.key),
                root_digest: child.digest,
                root: Some(child.page_ref),
            },
            None if count == 0 => ScratchAuthenticatedCatalogRoot::default(),
            None => return Err(ScratchError::MalformedPage),
        };
        validate_authenticated_catalog_root(&next)?;
        Ok(next)
    }

    fn authenticated_catalog_remove_child(
        &self,
        current: Option<AuthenticatedMapChild>,
        key: [u8; 16],
        depth: usize,
    ) -> Result<(Option<AuthenticatedMapChild>, bool), ScratchError> {
        if depth > MAX_AUTHENTICATED_MAP_DEPTH {
            return Err(ScratchError::IndexCapacity);
        }
        let Some(current) = current else {
            return Ok((None, false));
        };
        let mut node = self.read_authenticated_catalog_node(&current)?;
        match key.cmp(&node.key) {
            std::cmp::Ordering::Equal => Ok((
                self.merge_authenticated_catalog_children(node.left, node.right, depth + 1)?,
                true,
            )),
            std::cmp::Ordering::Less => {
                let (left, removed) =
                    self.authenticated_catalog_remove_child(node.left.take(), key, depth + 1)?;
                if !removed {
                    return Ok((Some(current), false));
                }
                node.left = left;
                Ok((Some(self.write_authenticated_catalog_node(&node)?), true))
            }
            std::cmp::Ordering::Greater => {
                let (right, removed) =
                    self.authenticated_catalog_remove_child(node.right.take(), key, depth + 1)?;
                if !removed {
                    return Ok((Some(current), false));
                }
                node.right = right;
                Ok((Some(self.write_authenticated_catalog_node(&node)?), true))
            }
        }
    }

    fn merge_authenticated_catalog_children(
        &self,
        left: Option<AuthenticatedMapChild>,
        right: Option<AuthenticatedMapChild>,
        depth: usize,
    ) -> Result<Option<AuthenticatedMapChild>, ScratchError> {
        if depth > MAX_AUTHENTICATED_MAP_DEPTH {
            return Err(ScratchError::IndexCapacity);
        }
        let (left, right) = match (left, right) {
            (Some(left), Some(right)) => (left, right),
            (left, right) => return Ok(left.or(right)),
        };
        if authenticated_map_priority_order(left.key, right.key).is_lt() {
            let mut node = self.read_authenticated_catalog_node(&left)?;
            node.right = self.merge_authenticated_catalog_children(
                node.right.take(),
                Some(right),
                depth + 1,
            )?;
            Ok(Some(self.write_authenticated_catalog_node(&node)?))
        } else {
            let mut node = self.read_authenticated_catalog_node(&right)?;
            node.left =
                self.merge_authenticated_catalog_children(Some(left), node.left.take(), depth + 1)?;
            Ok(Some(self.write_authenticated_catalog_node(&node)?))
        }
    }

    fn authenticated_map_upsert_for_kind(
        &self,
        kind: ScratchPageKind,
        root: &ScratchAuthenticatedMapRoot,
        key: [u8; 16],
        value_digest: ContentDigest,
    ) -> Result<ScratchAuthenticatedMapRoot, ScratchError> {
        validate_authenticated_map_root(root)?;
        let (child, inserted) = self.authenticated_map_upsert_child(
            kind,
            root.root.as_ref().map(|page_ref| AuthenticatedMapChild {
                key: root.root_key.expect("validated nonempty root key"),
                digest: root.root_digest,
                page_ref: page_ref.clone(),
            }),
            key,
            value_digest,
            0,
        )?;
        let count = if inserted {
            root.count
                .checked_add(1)
                .ok_or(ScratchError::IndexCapacity)?
        } else {
            root.count
        };
        let next = ScratchAuthenticatedMapRoot {
            schema_version: AUTHENTICATED_MAP_SCHEMA_VERSION,
            count,
            root_key: Some(child.key),
            root_digest: child.digest,
            root: Some(child.page_ref),
        };
        validate_authenticated_map_root(&next)?;
        Ok(next)
    }

    fn authenticated_map_upsert_child(
        &self,
        kind: ScratchPageKind,
        current: Option<AuthenticatedMapChild>,
        key: [u8; 16],
        value_digest: ContentDigest,
        depth: usize,
    ) -> Result<(AuthenticatedMapChild, bool), ScratchError> {
        if depth > MAX_AUTHENTICATED_MAP_DEPTH {
            return Err(ScratchError::IndexCapacity);
        }
        let Some(current) = current else {
            let node = AuthenticatedMapNode {
                schema_version: AUTHENTICATED_MAP_SCHEMA_VERSION,
                key,
                priority: authenticated_map_priority(key),
                value_digest,
                left: None,
                right: None,
            };
            return Ok((self.write_authenticated_map_node(kind, &node)?, true));
        };
        let mut node = self.read_authenticated_map_node(kind, &current)?;
        let inserted;
        match key.cmp(&node.key) {
            std::cmp::Ordering::Equal => {
                node.value_digest = value_digest;
                inserted = false;
            }
            std::cmp::Ordering::Less => {
                let (left, was_inserted) = self.authenticated_map_upsert_child(
                    kind,
                    node.left.take(),
                    key,
                    value_digest,
                    depth + 1,
                )?;
                node.left = Some(left);
                inserted = was_inserted;
                if node.left.as_ref().is_some_and(|left| {
                    authenticated_map_priority_order(left.key, node.key).is_lt()
                }) {
                    return Ok((self.rotate_authenticated_map_right(kind, node)?, inserted));
                }
            }
            std::cmp::Ordering::Greater => {
                let (right, was_inserted) = self.authenticated_map_upsert_child(
                    kind,
                    node.right.take(),
                    key,
                    value_digest,
                    depth + 1,
                )?;
                node.right = Some(right);
                inserted = was_inserted;
                if node.right.as_ref().is_some_and(|right| {
                    authenticated_map_priority_order(right.key, node.key).is_lt()
                }) {
                    return Ok((self.rotate_authenticated_map_left(kind, node)?, inserted));
                }
            }
        }
        Ok((self.write_authenticated_map_node(kind, &node)?, inserted))
    }

    fn rotate_authenticated_map_right(
        &self,
        kind: ScratchPageKind,
        mut node: AuthenticatedMapNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        let left = node.left.take().ok_or(ScratchError::MalformedPage)?;
        let mut left_node = self.read_authenticated_map_node(kind, &left)?;
        node.left = left_node.right.take();
        left_node.right = Some(self.write_authenticated_map_node(kind, &node)?);
        self.write_authenticated_map_node(kind, &left_node)
    }

    fn rotate_authenticated_map_left(
        &self,
        kind: ScratchPageKind,
        mut node: AuthenticatedMapNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        let right = node.right.take().ok_or(ScratchError::MalformedPage)?;
        let mut right_node = self.read_authenticated_map_node(kind, &right)?;
        node.right = right_node.left.take();
        right_node.left = Some(self.write_authenticated_map_node(kind, &node)?);
        self.write_authenticated_map_node(kind, &right_node)
    }

    pub(crate) fn causal_accumulator_upsert_max(
        &self,
        root: &ScratchCausalAccumulatorRoot,
        key: [u8; 16],
        counter: u64,
    ) -> Result<ScratchCausalAccumulatorRoot, ScratchError> {
        validate_causal_accumulator_root(root)?;
        if counter == 0 {
            return Err(ScratchError::MalformedPage);
        }
        let (child, inserted) = self.causal_accumulator_upsert_child(
            root.root.as_ref().map(|page_ref| AuthenticatedMapChild {
                key: root.root_key.expect("validated nonempty accumulator root"),
                digest: root.root_digest,
                page_ref: page_ref.clone(),
            }),
            key,
            counter,
            0,
        )?;
        let next = ScratchCausalAccumulatorRoot {
            schema_version: CAUSAL_ACCUMULATOR_SCHEMA_VERSION,
            count: if inserted {
                root.count
                    .checked_add(1)
                    .ok_or(ScratchError::IndexCapacity)?
            } else {
                root.count
            },
            root_key: Some(child.key),
            root_digest: child.digest,
            root: Some(child.page_ref),
        };
        validate_causal_accumulator_root(&next)?;
        Ok(next)
    }

    fn causal_accumulator_upsert_child(
        &self,
        current: Option<AuthenticatedMapChild>,
        key: [u8; 16],
        counter: u64,
        depth: usize,
    ) -> Result<(AuthenticatedMapChild, bool), ScratchError> {
        if depth > MAX_AUTHENTICATED_MAP_DEPTH {
            return Err(ScratchError::IndexCapacity);
        }
        let Some(current) = current else {
            let node = CausalAccumulatorNode {
                schema_version: CAUSAL_ACCUMULATOR_SCHEMA_VERSION,
                key,
                priority: authenticated_map_priority(key),
                counter,
                left: None,
                right: None,
            };
            return Ok((self.write_causal_accumulator_node(&node)?, true));
        };
        let mut node = self.read_causal_accumulator_node(&current)?;
        let inserted;
        match key.cmp(&node.key) {
            std::cmp::Ordering::Equal => {
                node.counter = node.counter.max(counter);
                inserted = false;
            }
            std::cmp::Ordering::Less => {
                let (left, was_inserted) = self.causal_accumulator_upsert_child(
                    node.left.take(),
                    key,
                    counter,
                    depth + 1,
                )?;
                node.left = Some(left);
                inserted = was_inserted;
                if node.left.as_ref().is_some_and(|left| {
                    authenticated_map_priority_order(left.key, node.key).is_lt()
                }) {
                    return Ok((self.rotate_causal_accumulator_right(node)?, inserted));
                }
            }
            std::cmp::Ordering::Greater => {
                let (right, was_inserted) = self.causal_accumulator_upsert_child(
                    node.right.take(),
                    key,
                    counter,
                    depth + 1,
                )?;
                node.right = Some(right);
                inserted = was_inserted;
                if node.right.as_ref().is_some_and(|right| {
                    authenticated_map_priority_order(right.key, node.key).is_lt()
                }) {
                    return Ok((self.rotate_causal_accumulator_left(node)?, inserted));
                }
            }
        }
        Ok((self.write_causal_accumulator_node(&node)?, inserted))
    }

    fn rotate_causal_accumulator_right(
        &self,
        mut node: CausalAccumulatorNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        let left = node.left.take().ok_or(ScratchError::MalformedPage)?;
        let mut left_node = self.read_causal_accumulator_node(&left)?;
        node.left = left_node.right.take();
        left_node.right = Some(self.write_causal_accumulator_node(&node)?);
        self.write_causal_accumulator_node(&left_node)
    }

    fn rotate_causal_accumulator_left(
        &self,
        mut node: CausalAccumulatorNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        let right = node.right.take().ok_or(ScratchError::MalformedPage)?;
        let mut right_node = self.read_causal_accumulator_node(&right)?;
        node.right = right_node.left.take();
        right_node.left = Some(self.write_causal_accumulator_node(&node)?);
        self.write_causal_accumulator_node(&right_node)
    }

    pub(crate) fn causal_accumulator_entries(
        &self,
        root: &ScratchCausalAccumulatorRoot,
    ) -> Result<Vec<([u8; 16], u64)>, ScratchError> {
        validate_causal_accumulator_root(root)?;
        let mut entries = Vec::with_capacity(root.count as usize);
        let mut stack = Vec::<AuthenticatedMapChild>::new();
        let mut current = root.root.as_ref().map(|page_ref| AuthenticatedMapChild {
            key: root.root_key.expect("validated nonempty accumulator root"),
            digest: root.root_digest,
            page_ref: page_ref.clone(),
        });
        while current.is_some() || !stack.is_empty() {
            while let Some(child) = current.take() {
                let node = self.read_causal_accumulator_node(&child)?;
                current = node.left.clone();
                stack.push(child);
            }
            let child = stack.pop().expect("nonempty traversal stack");
            let node = self.read_causal_accumulator_node(&child)?;
            entries.push((node.key, node.counter));
            current = node.right;
        }
        if entries.len() != root.count as usize
            || entries.windows(2).any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(ScratchError::MalformedPage);
        }
        Ok(entries)
    }

    pub(crate) fn scan_prefix(
        &self,
        root: &ScratchLsmRoot,
        kind: ScratchPageKind,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ScratchError> {
        validate_root(root)?;
        self.counters.range_reads.fetch_add(1, Ordering::Relaxed);
        let mut segments = root
            .levels
            .iter()
            .flatten()
            .collect::<Vec<&ScratchSegmentRef>>();
        segments.sort_unstable_by_key(|segment| segment.generation);
        let mut merged = BTreeMap::<Vec<u8>, Option<Vec<u8>>>::new();
        for segment_ref in segments {
            let segment = self.read_segment(kind, segment_ref)?;
            for record in segment.entries {
                if record.key.starts_with(prefix) {
                    merged.insert(record.key, record.value);
                }
            }
        }
        Ok(merged
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect())
    }

    pub(crate) fn materialize(
        &self,
        root: &ScratchLsmRoot,
        kind: ScratchPageKind,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ScratchError> {
        self.scan_prefix(root, kind, &[])
    }

    pub(crate) fn append_blob(&self, bytes: &[u8]) -> Result<ScratchBlobRef, ScratchError> {
        if bytes.is_empty() || bytes.len() > MAX_BLOB_BYTES {
            return Err(ScratchError::MalformedBlob);
        }
        let digest = ContentDigest::of(bytes);
        let encoded_len = u32::try_from(bytes.len()).map_err(|_| ScratchError::MalformedBlob)?;
        let mut file = self.blobs.lock().map_err(|_| ScratchError::Poisoned)?;
        let offset = file.seek(SeekFrom::End(0))?;
        file.write_all(bytes)?;
        self.counters.blob_writes.fetch_add(1, Ordering::Relaxed);
        self.counters
            .blob_bytes_written
            .fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(ScratchBlobRef {
            offset,
            encoded_len,
            digest,
        })
    }

    pub(crate) fn read_blob(&self, blob_ref: &ScratchBlobRef) -> Result<Vec<u8>, ScratchError> {
        let length =
            usize::try_from(blob_ref.encoded_len).map_err(|_| ScratchError::MalformedBlob)?;
        if length == 0 || length > MAX_BLOB_BYTES {
            return Err(ScratchError::MalformedBlob);
        }
        let mut bytes = vec![0_u8; length];
        let mut file = self.blobs.lock().map_err(|_| ScratchError::Poisoned)?;
        file.seek(SeekFrom::Start(blob_ref.offset))?;
        file.read_exact(&mut bytes)
            .map_err(|_| ScratchError::MalformedBlob)?;
        if ContentDigest::of(&bytes) != blob_ref.digest {
            return Err(ScratchError::BlobDigestMismatch(blob_ref.digest));
        }
        self.counters.blob_reads.fetch_add(1, Ordering::Relaxed);
        self.counters
            .blob_bytes_read
            .fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(bytes)
    }

    pub(crate) fn append_page<T: Serialize>(
        &self,
        kind: ScratchPageKind,
        key_min: Vec<u8>,
        key_max: Vec<u8>,
        value: &T,
    ) -> Result<ScratchPageRef, ScratchError> {
        if key_min.is_empty() || key_min > key_max {
            return Err(ScratchError::MalformedPage);
        }
        let payload = encode_canonical(value)?;
        let envelope = ScratchPageEnvelope {
            schema_version: SCRATCH_PAGE_SCHEMA_VERSION,
            kind,
            key_min: key_min.clone(),
            key_max: key_max.clone(),
            payload,
        };
        let bytes = encode_canonical(&envelope)?;
        if bytes.len() > MAX_PAGE_BYTES {
            return Err(ScratchError::PageTooLarge(bytes.len()));
        }
        let digest = ContentDigest::of(&bytes);
        let encoded_len = u32::try_from(bytes.len()).map_err(|_| ScratchError::MalformedPage)?;
        let mut file = self.pages.lock().map_err(|_| ScratchError::Poisoned)?;
        let offset = file.seek(SeekFrom::End(0))?;
        file.write_all(&bytes)?;
        self.counters.page_writes.fetch_add(1, Ordering::Relaxed);
        self.counters
            .page_bytes_written
            .fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(ScratchPageRef {
            offset,
            encoded_len,
            digest,
            kind,
            key_min,
            key_max,
        })
    }

    pub(crate) fn read_page<T: DeserializeOwned + Serialize>(
        &self,
        page_ref: &ScratchPageRef,
        expected_kind: ScratchPageKind,
    ) -> Result<T, ScratchError> {
        if page_ref.kind != expected_kind {
            return Err(ScratchError::PageBindingMismatch);
        }
        let length =
            usize::try_from(page_ref.encoded_len).map_err(|_| ScratchError::MalformedPage)?;
        if length == 0 || length > MAX_PAGE_BYTES {
            return Err(ScratchError::MalformedPage);
        }
        let mut bytes = vec![0_u8; length];
        let mut file = self.pages.lock().map_err(|_| ScratchError::Poisoned)?;
        file.seek(SeekFrom::Start(page_ref.offset))?;
        file.read_exact(&mut bytes)
            .map_err(|_| ScratchError::MalformedPage)?;
        if ContentDigest::of(&bytes) != page_ref.digest {
            return Err(ScratchError::PageDigestMismatch(page_ref.digest));
        }
        let envelope: ScratchPageEnvelope = decode_canonical(&bytes)?;
        if envelope.schema_version != SCRATCH_PAGE_SCHEMA_VERSION
            || envelope.kind != expected_kind
            || envelope.key_min != page_ref.key_min
            || envelope.key_max != page_ref.key_max
        {
            return Err(ScratchError::PageBindingMismatch);
        }
        self.counters.page_reads.fetch_add(1, Ordering::Relaxed);
        self.counters
            .page_bytes_read
            .fetch_add(bytes.len(), Ordering::Relaxed);
        self.counters
            .max_page_bytes_read
            .fetch_max(bytes.len(), Ordering::Relaxed);
        decode_canonical(&envelope.payload)
    }

    fn read_segment(
        &self,
        kind: ScratchPageKind,
        segment_ref: &ScratchSegmentRef,
    ) -> Result<ScratchSegment, ScratchError> {
        let segment: ScratchSegment = self.read_page(&segment_ref.page_ref, kind)?;
        validate_segment(&segment)?;
        if segment.kind != kind
            || segment.generation != segment_ref.generation
            || segment.entries.len() as u64 != segment_ref.entry_count
            || segment
                .entries
                .first()
                .is_none_or(|record| record.key != segment_ref.page_ref.key_min)
            || segment
                .entries
                .last()
                .is_none_or(|record| record.key != segment_ref.page_ref.key_max)
        {
            return Err(ScratchError::PageBindingMismatch);
        }
        Ok(segment)
    }

    #[allow(clippy::too_many_arguments)]
    fn append_accepted_sequence_at(
        &self,
        page_ref: &ScratchPageRef,
        height: u8,
        first_leaf: u64,
        leaf_index: u64,
        sequence: u64,
        batch_id: BatchId,
        evidence: Vec<u8>,
    ) -> Result<ScratchPageRef, ScratchError> {
        if height == 0 {
            let mut leaf = self.read_accepted_sequence_leaf(page_ref, first_leaf)?;
            if leaf.entries.len() >= ACCEPTED_SEQUENCE_LEAF_CAPACITY
                || sequence
                    != leaf
                        .first_sequence
                        .saturating_add(leaf.entries.len() as u64)
            {
                return Err(ScratchError::MalformedPage);
            }
            leaf.entries
                .push(AcceptedSequenceEntry { batch_id, evidence });
            return self.write_accepted_sequence_leaf(leaf.first_sequence, leaf.entries);
        }
        let mut node = self.read_accepted_sequence_node(page_ref, height, first_leaf)?;
        let child_capacity =
            accepted_sequence_leaf_capacity(height - 1).ok_or(ScratchError::IndexCapacity)?;
        let slot = usize::try_from((leaf_index - first_leaf) / child_capacity)
            .map_err(|_| ScratchError::MalformedPage)?;
        if slot >= ACCEPTED_SEQUENCE_NODE_FANOUT || slot > node.children.len() {
            return Err(ScratchError::MalformedPage);
        }
        let child_first = first_leaf
            .checked_add(
                u64::try_from(slot)
                    .map_err(|_| ScratchError::MalformedPage)?
                    .saturating_mul(child_capacity),
            )
            .ok_or(ScratchError::MalformedPage)?;
        let child = if slot == node.children.len() {
            self.build_accepted_sequence_path(
                height - 1,
                child_first,
                sequence,
                batch_id,
                evidence,
            )?
        } else {
            self.append_accepted_sequence_at(
                &node.children[slot],
                height - 1,
                child_first,
                leaf_index,
                sequence,
                batch_id,
                evidence,
            )?
        };
        if slot == node.children.len() {
            node.children.push(child);
        } else {
            node.children[slot] = child;
        }
        self.write_accepted_sequence_node(&node)
    }

    fn build_accepted_sequence_path(
        &self,
        height: u8,
        first_leaf: u64,
        sequence: u64,
        batch_id: BatchId,
        evidence: Vec<u8>,
    ) -> Result<ScratchPageRef, ScratchError> {
        if height == 0 {
            return self.write_accepted_sequence_leaf(
                sequence,
                vec![AcceptedSequenceEntry { batch_id, evidence }],
            );
        }
        let child = self.build_accepted_sequence_path(
            height - 1,
            first_leaf,
            sequence,
            batch_id,
            evidence,
        )?;
        self.write_accepted_sequence_node(&AcceptedSequenceNode {
            schema_version: ACCEPTED_SEQUENCE_SCHEMA_VERSION,
            height,
            first_leaf,
            children: vec![child],
        })
    }

    fn write_accepted_sequence_leaf(
        &self,
        first_sequence: u64,
        entries: Vec<AcceptedSequenceEntry>,
    ) -> Result<ScratchPageRef, ScratchError> {
        let leaf = AcceptedSequenceLeaf {
            schema_version: ACCEPTED_SEQUENCE_SCHEMA_VERSION,
            first_sequence,
            entries,
        };
        validate_accepted_sequence_leaf(&leaf)?;
        let last_sequence = first_sequence
            .checked_add(leaf.entries.len() as u64 - 1)
            .ok_or(ScratchError::MalformedPage)?;
        self.append_page(
            ScratchPageKind::AcceptedSequenceLeaf,
            first_sequence.to_be_bytes().to_vec(),
            last_sequence.to_be_bytes().to_vec(),
            &leaf,
        )
    }

    fn read_accepted_sequence_leaf(
        &self,
        page_ref: &ScratchPageRef,
        first_leaf: u64,
    ) -> Result<AcceptedSequenceLeaf, ScratchError> {
        let leaf: AcceptedSequenceLeaf =
            self.read_page(page_ref, ScratchPageKind::AcceptedSequenceLeaf)?;
        validate_accepted_sequence_leaf(&leaf)?;
        let expected_first = first_leaf
            .checked_mul(ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64)
            .and_then(|value| value.checked_add(1))
            .ok_or(ScratchError::MalformedPage)?;
        let last = leaf
            .first_sequence
            .checked_add(leaf.entries.len() as u64 - 1)
            .ok_or(ScratchError::MalformedPage)?;
        if leaf.first_sequence != expected_first
            || page_ref.key_min != leaf.first_sequence.to_be_bytes()
            || page_ref.key_max != last.to_be_bytes()
        {
            return Err(ScratchError::PageBindingMismatch);
        }
        Ok(leaf)
    }

    fn write_accepted_sequence_node(
        &self,
        node: &AcceptedSequenceNode,
    ) -> Result<ScratchPageRef, ScratchError> {
        validate_accepted_sequence_node(node)?;
        let first_sequence = node
            .first_leaf
            .checked_mul(ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64)
            .and_then(|value| value.checked_add(1))
            .ok_or(ScratchError::MalformedPage)?;
        let last_sequence = node
            .children
            .last()
            .and_then(|child| <[u8; 8]>::try_from(child.key_max.as_slice()).ok())
            .map(u64::from_be_bytes)
            .ok_or(ScratchError::MalformedPage)?;
        self.append_page(
            ScratchPageKind::AcceptedSequenceNode,
            first_sequence.to_be_bytes().to_vec(),
            last_sequence.to_be_bytes().to_vec(),
            node,
        )
    }

    fn read_accepted_sequence_node(
        &self,
        page_ref: &ScratchPageRef,
        height: u8,
        first_leaf: u64,
    ) -> Result<AcceptedSequenceNode, ScratchError> {
        let node: AcceptedSequenceNode =
            self.read_page(page_ref, ScratchPageKind::AcceptedSequenceNode)?;
        validate_accepted_sequence_node(&node)?;
        let first_sequence = first_leaf
            .checked_mul(ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64)
            .and_then(|value| value.checked_add(1))
            .ok_or(ScratchError::MalformedPage)?;
        if node.height != height
            || node.first_leaf != first_leaf
            || page_ref.key_min != first_sequence.to_be_bytes()
            || page_ref.key_max
                != node
                    .children
                    .last()
                    .ok_or(ScratchError::MalformedPage)?
                    .key_max
        {
            return Err(ScratchError::PageBindingMismatch);
        }
        Ok(node)
    }

    fn write_authenticated_map_node(
        &self,
        kind: ScratchPageKind,
        node: &AuthenticatedMapNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        validate_authenticated_map_node(node)?;
        let digest = authenticated_map_node_digest(
            node.key,
            node.value_digest,
            node.left.as_ref().map(|child| (child.key, child.digest)),
            node.right.as_ref().map(|child| (child.key, child.digest)),
        );
        let key = node.key.to_vec();
        let page_ref = self.append_page(kind, key.clone(), key, node)?;
        Ok(AuthenticatedMapChild {
            key: node.key,
            digest,
            page_ref,
        })
    }

    fn read_authenticated_map_node(
        &self,
        kind: ScratchPageKind,
        child: &AuthenticatedMapChild,
    ) -> Result<AuthenticatedMapNode, ScratchError> {
        let node: AuthenticatedMapNode = self.read_page(&child.page_ref, kind)?;
        validate_authenticated_map_node(&node)?;
        if node.key != child.key
            || child.page_ref.key_min != child.key
            || child.page_ref.key_max != child.key
            || authenticated_map_node_digest(
                node.key,
                node.value_digest,
                node.left
                    .as_ref()
                    .map(|candidate| (candidate.key, candidate.digest)),
                node.right
                    .as_ref()
                    .map(|candidate| (candidate.key, candidate.digest)),
            ) != child.digest
        {
            return Err(ScratchError::PageBindingMismatch);
        }
        Ok(node)
    }

    fn write_authenticated_catalog_node(
        &self,
        node: &AuthenticatedCatalogNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        validate_authenticated_catalog_node(node)?;
        let digest = authenticated_catalog_node_digest(node);
        let key = node.key.to_vec();
        let page_ref =
            self.append_page(ScratchPageKind::CurrentPathCatalog, key.clone(), key, node)?;
        Ok(AuthenticatedMapChild {
            key: node.key,
            digest,
            page_ref,
        })
    }

    fn read_authenticated_catalog_node(
        &self,
        child: &AuthenticatedMapChild,
    ) -> Result<AuthenticatedCatalogNode, ScratchError> {
        let node: AuthenticatedCatalogNode =
            self.read_page(&child.page_ref, ScratchPageKind::CurrentPathCatalog)?;
        validate_authenticated_catalog_node(&node)?;
        if node.key != child.key
            || child.page_ref.key_min != child.key
            || child.page_ref.key_max != child.key
            || authenticated_catalog_node_digest(&node) != child.digest
        {
            return Err(ScratchError::PageBindingMismatch);
        }
        Ok(node)
    }

    fn write_authenticated_point_node(
        &self,
        kind: ScratchPageKind,
        node: &AuthenticatedPointNode,
    ) -> Result<AuthenticatedPointChild, ScratchError> {
        validate_authenticated_point_node(kind, node)?;
        let digest = authenticated_point_node_digest(node);
        let key = node.key_digest.as_bytes().to_vec();
        let page_ref = self.append_page(kind, key.clone(), key, node)?;
        Ok(AuthenticatedPointChild {
            key_digest: node.key_digest,
            digest,
            page_ref,
        })
    }

    fn read_authenticated_point_node(
        &self,
        kind: ScratchPageKind,
        child: &AuthenticatedPointChild,
    ) -> Result<AuthenticatedPointNode, ScratchError> {
        let node: AuthenticatedPointNode = self.read_page(&child.page_ref, kind)?;
        validate_authenticated_point_node(kind, &node)?;
        if node.key_digest != child.key_digest
            || child.page_ref.key_min != child.key_digest.as_bytes()
            || child.page_ref.key_max != child.key_digest.as_bytes()
            || authenticated_point_node_digest(&node) != child.digest
        {
            return Err(ScratchError::PageBindingMismatch);
        }
        Ok(node)
    }

    fn write_causal_accumulator_node(
        &self,
        node: &CausalAccumulatorNode,
    ) -> Result<AuthenticatedMapChild, ScratchError> {
        validate_causal_accumulator_node(node)?;
        let digest = causal_accumulator_node_digest(
            node.key,
            node.counter,
            node.left.as_ref().map(|child| (child.key, child.digest)),
            node.right.as_ref().map(|child| (child.key, child.digest)),
        );
        let key = node.key.to_vec();
        let page_ref =
            self.append_page(ScratchPageKind::CausalAccumulator, key.clone(), key, node)?;
        Ok(AuthenticatedMapChild {
            key: node.key,
            digest,
            page_ref,
        })
    }

    fn read_causal_accumulator_node(
        &self,
        child: &AuthenticatedMapChild,
    ) -> Result<CausalAccumulatorNode, ScratchError> {
        let node: CausalAccumulatorNode =
            self.read_page(&child.page_ref, ScratchPageKind::CausalAccumulator)?;
        validate_causal_accumulator_node(&node)?;
        if node.key != child.key
            || child.page_ref.key_min != child.key
            || child.page_ref.key_max != child.key
            || causal_accumulator_node_digest(
                node.key,
                node.counter,
                node.left
                    .as_ref()
                    .map(|candidate| (candidate.key, candidate.digest)),
                node.right
                    .as_ref()
                    .map(|candidate| (candidate.key, candidate.digest)),
            ) != child.digest
        {
            return Err(ScratchError::PageBindingMismatch);
        }
        Ok(node)
    }

    /// Opportunistic reclamation of sibling scratch runs.
    ///
    /// This is garbage collection of disposable, reconstructible run-local
    /// state, never a gate on the caller's own fresh run, so it is infallible
    /// by construction: a sibling this pass cannot authenticate has no veto
    /// over creating a new run.
    ///
    /// A sibling is removed only when its own durable marker authenticates it
    /// as an ephemeral run of this exact workspace, its entry set is complete,
    /// and its exclusive lease is free. Every other outcome preserves the
    /// sibling's bytes exactly and is counted instead.
    fn reclaim_stale_runs(&self) {
        let Ok(entries) = self.namespace.entries() else {
            // The namespace itself could not be enumerated. Nothing is proved
            // about any sibling, so nothing is removed.
            self.counters
                .unclassified_runs_preserved
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        for entry in entries {
            let disposition = entry
                .map_err(ScratchError::from)
                .and_then(|entry| self.reclaim_sibling_run(&entry))
                .unwrap_or(StaleRunDisposition::Unclassified);
            let counter = match disposition {
                StaleRunDisposition::OwnRun => continue,
                StaleRunDisposition::Reclaimed => &self.counters.stale_runs_reclaimed,
                StaleRunDisposition::LivePreserved => &self.counters.live_runs_skipped,
                StaleRunDisposition::RetainedPreserved => &self.counters.retained_runs_preserved,
                StaleRunDisposition::Unclassified => &self.counters.unclassified_runs_preserved,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Classify exactly one sibling entry, reclaiming it only when its own
    /// bytes prove that is safe.
    ///
    /// Every `Err` here means "not proved safe to touch": the caller converts
    /// it into [`StaleRunDisposition::Unclassified`], which preserves the
    /// sibling untouched. Errors are deliberately not propagated to the caller
    /// of `ScratchStore::open`.
    fn reclaim_sibling_run(
        &self,
        entry: &cap_std::fs::DirEntry,
    ) -> Result<StaleRunDisposition, ScratchError> {
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| ScratchError::UnsafeEntry("non-UTF-8 scratch run".into()))?
            .to_owned();
        if name == self.run_name {
            return Ok(StaleRunDisposition::OwnRun);
        }
        #[cfg(test)]
        inject_sibling_inspection_fault()?;
        let run_id = parse_run_name(&name)?;
        require_real_directory(entry, &name)?;
        let run = super::object_store::open_dir_nofollow(&self.namespace, &name)?;
        let marker_bytes = read_regular_nofollow(&run, MARKER_FILE, MAX_MARKER_BYTES)?;
        let marker: ScratchRunMarkerV3 = decode_canonical(&marker_bytes)?;
        if marker.schema_version != SCRATCH_SCHEMA_VERSION
            || marker.workspace_id != self.marker.workspace_id
            || marker.run_id != run_id
        {
            return Err(ScratchError::MalformedMarker(name));
        }
        validate_run_entries(&run)?;
        if marker.retention == ScratchRetention::Retained {
            // Ordinary reclamation carries no authority to discard adoptable
            // state. Until an authenticated runtime-resume-point format can
            // prove a retained run is unreachable, an orphan survives.
            return Ok(StaleRunDisposition::RetainedPreserved);
        }
        let lease = open_regular_read_write_nofollow(&run, LEASE_FILE)?;
        if !lock_exclusive_nonblocking(&lease)? {
            return Ok(StaleRunDisposition::LivePreserved);
        }
        remove_stale_run(&self.namespace, &run, &name, lease)?;
        Ok(StaleRunDisposition::Reclaimed)
    }

    fn cleanup_own_run(&self) {
        for name in [PAGES_FILE, BLOBS_FILE, MARKER_FILE] {
            let _ = self.run.remove_file(name);
        }
        unlock(&self.lease);
        let _ = self.run.remove_file(LEASE_FILE);
        let _ = self.namespace.remove_dir(&self.run_name);
    }
}

impl Drop for ScratchStore {
    fn drop(&mut self) {
        match self.marker.retention {
            ScratchRetention::Ephemeral => self.cleanup_own_run(),
            // Release only the exclusive lease. Every marker, page, blob, and
            // index byte stays exactly as written so the run stays adoptable.
            ScratchRetention::Retained => unlock(&self.lease),
        }
    }
}

struct AcceptedSequenceCursorFrame {
    node: AcceptedSequenceNode,
    next_child: usize,
}

pub(crate) struct ScratchAcceptedSequenceCursor<'a> {
    store: &'a ScratchStore,
    root: &'a ScratchAcceptedSequenceRoot,
    stack: Vec<AcceptedSequenceCursorFrame>,
    leaf: Option<(AcceptedSequenceLeaf, usize)>,
    next_sequence: u64,
    initialized: bool,
    page_reads: usize,
    page_bytes_read: usize,
    max_page_bytes_read: usize,
}

impl ScratchAcceptedSequenceCursor<'_> {
    pub(crate) const fn page_stats(&self) -> (usize, usize, usize) {
        (
            self.page_reads,
            self.page_bytes_read,
            self.max_page_bytes_read,
        )
    }

    pub(crate) fn next_batch(
        &mut self,
    ) -> Result<Option<(u64, AcceptedSequenceEntry)>, ScratchError> {
        if self.next_sequence > self.root.len {
            return Ok(None);
        }
        if !self.initialized {
            self.initialized = true;
            let root = self.root.root.clone().ok_or(ScratchError::MalformedPage)?;
            self.descend_left(root, self.root.height, 0)?;
        }
        loop {
            if let Some((leaf, index)) = &mut self.leaf {
                if let Some(entry) = leaf.entries.get(*index).cloned() {
                    let sequence = self.next_sequence;
                    if sequence
                        != leaf
                            .first_sequence
                            .checked_add(*index as u64)
                            .ok_or(ScratchError::MalformedPage)?
                    {
                        return Err(ScratchError::MalformedPage);
                    }
                    *index += 1;
                    self.next_sequence += 1;
                    return Ok(Some((sequence, entry)));
                }
                self.leaf = None;
            }
            let mut next = None;
            while let Some(frame) = self.stack.last_mut() {
                if frame.next_child < frame.node.children.len() {
                    let slot = frame.next_child;
                    frame.next_child += 1;
                    let child_capacity = accepted_sequence_leaf_capacity(frame.node.height - 1)
                        .ok_or(ScratchError::IndexCapacity)?;
                    let first_leaf = frame
                        .node
                        .first_leaf
                        .checked_add(
                            u64::try_from(slot)
                                .map_err(|_| ScratchError::MalformedPage)?
                                .saturating_mul(child_capacity),
                        )
                        .ok_or(ScratchError::MalformedPage)?;
                    next = Some((
                        frame.node.children[slot].clone(),
                        frame.node.height - 1,
                        first_leaf,
                    ));
                    break;
                }
                self.stack.pop();
            }
            let Some((page_ref, height, first_leaf)) = next else {
                return Err(ScratchError::MalformedPage);
            };
            self.descend_left(page_ref, height, first_leaf)?;
        }
    }

    fn descend_left(
        &mut self,
        mut page_ref: ScratchPageRef,
        mut height: u8,
        mut first_leaf: u64,
    ) -> Result<(), ScratchError> {
        while height > 0 {
            self.record_page_read(&page_ref);
            let node = self
                .store
                .read_accepted_sequence_node(&page_ref, height, first_leaf)?;
            let child = node
                .children
                .first()
                .cloned()
                .ok_or(ScratchError::MalformedPage)?;
            self.stack.push(AcceptedSequenceCursorFrame {
                node,
                next_child: 1,
            });
            page_ref = child;
            height -= 1;
            first_leaf = self
                .stack
                .last()
                .expect("pushed accepted sequence frame")
                .node
                .first_leaf;
        }
        self.leaf = Some((
            {
                self.record_page_read(&page_ref);
                self.store
                    .read_accepted_sequence_leaf(&page_ref, first_leaf)?
            },
            0,
        ));
        Ok(())
    }

    fn record_page_read(&mut self, page_ref: &ScratchPageRef) {
        let length = page_ref.encoded_len as usize;
        self.page_reads = self.page_reads.saturating_add(1);
        self.page_bytes_read = self.page_bytes_read.saturating_add(length);
        self.max_page_bytes_read = self.max_page_bytes_read.max(length);
    }
}

pub(crate) fn authenticated_map_empty_digest() -> ContentDigest {
    ContentDigest::of(b"tine/oplog/authenticated-map/v1/empty")
}

fn authenticated_point_empty_digest() -> ContentDigest {
    ContentDigest::of(b"tine/oplog/authenticated-point-map/v1/empty")
}

fn authenticated_point_key_digest(kind: ScratchPageKind, logical_key: &[u8]) -> ContentDigest {
    let mut bytes = b"tine/oplog/authenticated-point-map/v1/key\0".to_vec();
    bytes.push(kind as u8);
    bytes.extend_from_slice(&(logical_key.len() as u64).to_be_bytes());
    bytes.extend_from_slice(logical_key);
    ContentDigest::of(&bytes)
}

fn authenticated_point_priority(key_digest: ContentDigest) -> ContentDigest {
    let mut bytes = b"tine/oplog/authenticated-point-map/v1/priority\0".to_vec();
    bytes.extend_from_slice(key_digest.as_bytes());
    ContentDigest::of(&bytes)
}

fn authenticated_point_priority_order(
    left: ContentDigest,
    right: ContentDigest,
) -> std::cmp::Ordering {
    authenticated_point_priority(left)
        .as_bytes()
        .cmp(authenticated_point_priority(right).as_bytes())
        .then_with(|| left.as_bytes().cmp(right.as_bytes()))
}

fn authenticated_point_node_digest(node: &AuthenticatedPointNode) -> ContentDigest {
    let mut bytes = b"tine/oplog/authenticated-point-map/v1/node\0".to_vec();
    bytes.extend_from_slice(node.key_digest.as_bytes());
    bytes.extend_from_slice(&(node.logical_key.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&node.logical_key);
    bytes.extend_from_slice(&(node.value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(ContentDigest::of(&node.value).as_bytes());
    for child in [&node.left, &node.right] {
        match child {
            Some(child) => {
                bytes.push(1);
                bytes.extend_from_slice(child.key_digest.as_bytes());
                bytes.extend_from_slice(child.digest.as_bytes());
            }
            None => bytes.push(0),
        }
    }
    ContentDigest::of(&bytes)
}

fn causal_accumulator_empty_digest() -> ContentDigest {
    ContentDigest::of(b"tine/oplog/causal-accumulator/v1/empty")
}

fn causal_accumulator_node_digest(
    key: [u8; 16],
    counter: u64,
    left: Option<([u8; 16], ContentDigest)>,
    right: Option<([u8; 16], ContentDigest)>,
) -> ContentDigest {
    let mut bytes = b"tine/oplog/causal-accumulator/v1/node\0".to_vec();
    bytes.extend_from_slice(&key);
    bytes.extend_from_slice(&counter.to_be_bytes());
    for child in [left, right] {
        match child {
            Some((child_key, digest)) => {
                bytes.push(1);
                bytes.extend_from_slice(&child_key);
                bytes.extend_from_slice(digest.as_bytes());
            }
            None => bytes.push(0),
        }
    }
    ContentDigest::of(&bytes)
}

pub(crate) fn authenticated_map_priority(key: [u8; 16]) -> ContentDigest {
    let mut bytes = b"tine/oplog/authenticated-map/v1/priority\0".to_vec();
    bytes.extend_from_slice(&key);
    ContentDigest::of(&bytes)
}

pub(crate) fn authenticated_map_node_digest(
    key: [u8; 16],
    value_digest: ContentDigest,
    left: Option<([u8; 16], ContentDigest)>,
    right: Option<([u8; 16], ContentDigest)>,
) -> ContentDigest {
    let mut bytes = b"tine/oplog/authenticated-map/v1/node\0".to_vec();
    bytes.extend_from_slice(&key);
    bytes.extend_from_slice(value_digest.as_bytes());
    for child in [left, right] {
        match child {
            Some((child_key, digest)) => {
                bytes.push(1);
                bytes.extend_from_slice(&child_key);
                bytes.extend_from_slice(digest.as_bytes());
            }
            None => bytes.push(0),
        }
    }
    ContentDigest::of(&bytes)
}

fn authenticated_catalog_empty_digest() -> ContentDigest {
    ContentDigest::of(b"tine/oplog/authenticated-current-path-catalog/v1/empty")
}

fn authenticated_catalog_node_digest(node: &AuthenticatedCatalogNode) -> ContentDigest {
    let mut bytes = b"tine/oplog/authenticated-current-path-catalog/v1/node\0".to_vec();
    bytes.extend_from_slice(&node.key);
    bytes.extend_from_slice(ContentDigest::of(&node.value).as_bytes());
    for child in [&node.left, &node.right] {
        match child {
            Some(child) => {
                bytes.push(1);
                bytes.extend_from_slice(&child.key);
                bytes.extend_from_slice(child.digest.as_bytes());
            }
            None => bytes.push(0),
        }
    }
    ContentDigest::of(&bytes)
}

pub(crate) fn authenticated_map_priority_order(
    left: [u8; 16],
    right: [u8; 16],
) -> std::cmp::Ordering {
    authenticated_map_priority(left)
        .as_bytes()
        .cmp(authenticated_map_priority(right).as_bytes())
        .then_with(|| left.cmp(&right))
}

fn accepted_sequence_leaf_capacity(height: u8) -> Option<u64> {
    let mut capacity = 1_u64;
    for _ in 0..height {
        capacity = capacity.checked_mul(ACCEPTED_SEQUENCE_NODE_FANOUT as u64)?;
    }
    Some(capacity)
}

fn validate_accepted_sequence_root(root: &ScratchAcceptedSequenceRoot) -> Result<(), ScratchError> {
    if root.schema_version != ACCEPTED_SEQUENCE_SCHEMA_VERSION
        || (root.len == 0) != root.root.is_none()
        || (root.len == 0 && root.height != 0)
    {
        return Err(ScratchError::MalformedPage);
    }
    if root.len > 0 {
        let leaf_count = root
            .len
            .saturating_add(ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64 - 1)
            / ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64;
        let capacity =
            accepted_sequence_leaf_capacity(root.height).ok_or(ScratchError::IndexCapacity)?;
        if leaf_count == 0
            || leaf_count > capacity
            || (root.height > 0
                && leaf_count
                    <= accepted_sequence_leaf_capacity(root.height - 1)
                        .ok_or(ScratchError::IndexCapacity)?)
        {
            return Err(ScratchError::MalformedPage);
        }
    }
    Ok(())
}

fn validate_accepted_sequence_leaf(leaf: &AcceptedSequenceLeaf) -> Result<(), ScratchError> {
    if leaf.schema_version != ACCEPTED_SEQUENCE_SCHEMA_VERSION
        || leaf.first_sequence == 0
        || leaf.entries.is_empty()
        || leaf.entries.len() > ACCEPTED_SEQUENCE_LEAF_CAPACITY
        || leaf.entries.iter().any(|entry| entry.evidence.is_empty())
        || !(leaf.first_sequence - 1).is_multiple_of(ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64)
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_accepted_sequence_node(node: &AcceptedSequenceNode) -> Result<(), ScratchError> {
    if node.schema_version != ACCEPTED_SEQUENCE_SCHEMA_VERSION
        || node.height == 0
        || node.children.is_empty()
        || node.children.len() > ACCEPTED_SEQUENCE_NODE_FANOUT
    {
        return Err(ScratchError::MalformedPage);
    }
    let child_capacity =
        accepted_sequence_leaf_capacity(node.height - 1).ok_or(ScratchError::IndexCapacity)?;
    for (index, child) in node.children.iter().enumerate() {
        let child_first_leaf = node
            .first_leaf
            .checked_add(
                u64::try_from(index)
                    .map_err(|_| ScratchError::MalformedPage)?
                    .saturating_mul(child_capacity),
            )
            .ok_or(ScratchError::MalformedPage)?;
        let expected_first = child_first_leaf
            .checked_mul(ACCEPTED_SEQUENCE_LEAF_CAPACITY as u64)
            .and_then(|value| value.checked_add(1))
            .ok_or(ScratchError::MalformedPage)?;
        if child.key_min != expected_first.to_be_bytes()
            || child.key_max.len() != std::mem::size_of::<u64>()
        {
            return Err(ScratchError::PageBindingMismatch);
        }
    }
    Ok(())
}

fn validate_authenticated_map_root(root: &ScratchAuthenticatedMapRoot) -> Result<(), ScratchError> {
    if root.schema_version != AUTHENTICATED_MAP_SCHEMA_VERSION
        || (root.count == 0)
            != (root.root.is_none()
                && root.root_key.is_none()
                && root.root_digest == authenticated_map_empty_digest())
        || (root.count > 0 && (root.root.is_none() || root.root_key.is_none()))
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_authenticated_catalog_root(
    root: &ScratchAuthenticatedCatalogRoot,
) -> Result<(), ScratchError> {
    if root.schema_version != AUTHENTICATED_CATALOG_SCHEMA_VERSION
        || (root.count == 0)
            != (root.root.is_none()
                && root.root_key.is_none()
                && root.root_digest == authenticated_catalog_empty_digest())
        || (root.count > 0 && (root.root.is_none() || root.root_key.is_none()))
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_authenticated_point_key(logical_key: &[u8]) -> Result<(), ScratchError> {
    if logical_key.is_empty() || logical_key.len() > AUTHENTICATED_POINT_MAX_KEY_BYTES {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_authenticated_point_root(
    root: &ScratchAuthenticatedPointRoot,
) -> Result<(), ScratchError> {
    if root.schema_version != AUTHENTICATED_POINT_MAP_SCHEMA_VERSION
        || (root.count == 0)
            != (root.root.is_none()
                && root.root_key_digest.is_none()
                && root.root_digest == authenticated_point_empty_digest())
        || (root.count > 0 && (root.root.is_none() || root.root_key_digest.is_none()))
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_authenticated_point_node(
    kind: ScratchPageKind,
    node: &AuthenticatedPointNode,
) -> Result<(), ScratchError> {
    validate_authenticated_point_key(&node.logical_key)?;
    if node.schema_version != AUTHENTICATED_POINT_MAP_SCHEMA_VERSION
        || node.value.len() > AUTHENTICATED_POINT_MAX_VALUE_BYTES
        || node.key_digest != authenticated_point_key_digest(kind, &node.logical_key)
        || node.priority != authenticated_point_priority(node.key_digest)
        || node.left.as_ref().is_some_and(|left| {
            left.key_digest >= node.key_digest
                || !authenticated_point_priority_order(node.key_digest, left.key_digest).is_lt()
        })
        || node.right.as_ref().is_some_and(|right| {
            right.key_digest <= node.key_digest
                || !authenticated_point_priority_order(node.key_digest, right.key_digest).is_lt()
        })
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_causal_accumulator_root(
    root: &ScratchCausalAccumulatorRoot,
) -> Result<(), ScratchError> {
    if root.schema_version != CAUSAL_ACCUMULATOR_SCHEMA_VERSION
        || (root.count == 0)
            != (root.root.is_none()
                && root.root_key.is_none()
                && root.root_digest == causal_accumulator_empty_digest())
        || (root.count > 0 && (root.root.is_none() || root.root_key.is_none()))
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_authenticated_map_node(node: &AuthenticatedMapNode) -> Result<(), ScratchError> {
    if node.schema_version != AUTHENTICATED_MAP_SCHEMA_VERSION
        || node.priority != authenticated_map_priority(node.key)
        || node.left.as_ref().is_some_and(|left| {
            left.key >= node.key || !authenticated_map_priority_order(node.key, left.key).is_lt()
        })
        || node.right.as_ref().is_some_and(|right| {
            right.key <= node.key || !authenticated_map_priority_order(node.key, right.key).is_lt()
        })
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_authenticated_catalog_node(
    node: &AuthenticatedCatalogNode,
) -> Result<(), ScratchError> {
    if node.schema_version != AUTHENTICATED_CATALOG_SCHEMA_VERSION
        || node.value.is_empty()
        || node.value.len() > MAX_AUTHENTICATED_CATALOG_VALUE_BYTES
        || node.priority != authenticated_map_priority(node.key)
        || node.left.as_ref().is_some_and(|left| {
            left.key >= node.key || !authenticated_map_priority_order(node.key, left.key).is_lt()
        })
        || node.right.as_ref().is_some_and(|right| {
            right.key <= node.key || !authenticated_map_priority_order(node.key, right.key).is_lt()
        })
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_causal_accumulator_node(node: &CausalAccumulatorNode) -> Result<(), ScratchError> {
    if node.schema_version != CAUSAL_ACCUMULATOR_SCHEMA_VERSION
        || node.counter == 0
        || node.priority != authenticated_map_priority(node.key)
        || node.left.as_ref().is_some_and(|left| {
            left.key >= node.key || !authenticated_map_priority_order(node.key, left.key).is_lt()
        })
        || node.right.as_ref().is_some_and(|right| {
            right.key <= node.key || !authenticated_map_priority_order(node.key, right.key).is_lt()
        })
    {
        return Err(ScratchError::MalformedPage);
    }
    Ok(())
}

fn validate_root(root: &ScratchLsmRoot) -> Result<(), ScratchError> {
    if root.levels.len() != SCRATCH_LSM_LEVELS {
        return Err(ScratchError::MalformedPage);
    }
    for segment in root.levels.iter().flatten() {
        if segment.generation == 0
            || segment.generation > root.next_generation
            || segment.entry_count == 0
        {
            return Err(ScratchError::MalformedPage);
        }
    }
    Ok(())
}

fn validate_segment(segment: &ScratchSegment) -> Result<(), ScratchError> {
    if segment.schema_version != SCRATCH_PAGE_SCHEMA_VERSION
        || segment.generation == 0
        || segment.entries.is_empty()
    {
        return Err(ScratchError::MalformedPage);
    }
    let mut previous: Option<&[u8]> = None;
    for record in &segment.entries {
        if record.key.is_empty()
            || previous.is_some_and(|previous| previous >= record.key.as_slice())
        {
            return Err(ScratchError::MalformedPage);
        }
        previous = Some(&record.key);
    }
    Ok(())
}

fn parse_run_name(name: &str) -> Result<Uuid, ScratchError> {
    let suffix = name
        .strip_prefix("run-")
        .ok_or_else(|| ScratchError::UnsafeEntry(format!("unknown scratch entry {name:?}")))?;
    let run_id = Uuid::parse_str(suffix)
        .map_err(|_| ScratchError::UnsafeEntry(format!("malformed scratch run {name:?}")))?;
    if format!("run-{run_id}") != name {
        return Err(ScratchError::UnsafeEntry(format!(
            "non-canonical scratch run {name:?}"
        )));
    }
    Ok(run_id)
}

fn validate_run_entries(run: &Dir) -> Result<(), ScratchError> {
    let mut seen = BTreeMap::new();
    for entry in run.entries()? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| ScratchError::UnsafeEntry("non-UTF-8 scratch entry".into()))?
            .to_owned();
        if ![MARKER_FILE, LEASE_FILE, PAGES_FILE, BLOBS_FILE].contains(&name.as_str()) {
            return Err(ScratchError::UnsafeEntry(format!(
                "unknown scratch run entry {name:?}"
            )));
        }
        require_regular_entry(&entry, &name)?;
        if seen.insert(name.clone(), ()).is_some() {
            return Err(ScratchError::UnsafeEntry(format!(
                "duplicate scratch run entry {name:?}"
            )));
        }
    }
    for required in [MARKER_FILE, LEASE_FILE, PAGES_FILE, BLOBS_FILE] {
        if !seen.contains_key(required) {
            return Err(ScratchError::UnsafeEntry(format!(
                "scratch run is missing {required:?}"
            )));
        }
    }
    Ok(())
}

fn remove_stale_run(
    namespace: &Dir,
    run: &Dir,
    run_name: &str,
    lease: fs::File,
) -> Result<(), ScratchError> {
    // Validate the complete entry set before unlinking anything. No recursive
    // ambient deletion is used and no authoritative namespace is reachable.
    validate_run_entries(run)?;
    for name in [PAGES_FILE, BLOBS_FILE, MARKER_FILE] {
        run.remove_file(name)?;
    }
    unlock(&lease);
    drop(lease);
    run.remove_file(LEASE_FILE)?;
    namespace.remove_dir(run_name)?;
    Ok(())
}

/// Best-effort removal of exactly the run directory a failed construction
/// created.
///
/// Every step is capability-relative beneath the scratch namespace and named,
/// so no recursive or ambient deletion is used and no sibling run is reachable.
/// Failures are deliberately swallowed: the caller returns the original
/// construction error, and residue this pass cannot remove is preserved for a
/// later reclamation pass rather than escalated into a second failure.
fn remove_partial_own_run(namespace: &Dir, run_name: &str) {
    if let Ok(run) = super::object_store::open_dir_nofollow(namespace, run_name) {
        for name in [PAGES_FILE, BLOBS_FILE, MARKER_FILE, LEASE_FILE] {
            let _ = run.remove_file(name);
        }
    }
    let _ = namespace.remove_dir(run_name);
}

/// Named fallible boundaries of one run construction, after the run directory
/// exists. Deterministic fault injection at each of them proves the namespace
/// never retains self-created residue.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScratchCreateBoundary {
    AfterRunDirectory,
    AfterNamespaceSync,
    AfterRunOpen,
    AfterMarkerWrite,
    AfterLeaseCreate,
    AfterLeaseLock,
    AfterPagesCreate,
    AfterBlobsCreate,
    AfterReclaim,
}

#[cfg(test)]
impl ScratchCreateBoundary {
    /// Every post-`mkdir` boundary, in construction order.
    pub(crate) const ALL: [Self; 9] = [
        Self::AfterRunDirectory,
        Self::AfterNamespaceSync,
        Self::AfterRunOpen,
        Self::AfterMarkerWrite,
        Self::AfterLeaseCreate,
        Self::AfterLeaseLock,
        Self::AfterPagesCreate,
        Self::AfterBlobsCreate,
        Self::AfterReclaim,
    ];
}

#[cfg(test)]
thread_local! {
    /// One-shot construction fault. Thread-local and deterministic: no
    /// process-global resource limit or signal is involved, so parallel tests
    /// in other threads are unaffected.
    static CREATE_RUN_FAULT: std::cell::Cell<Option<ScratchCreateBoundary>> =
        const { std::cell::Cell::new(None) };
    /// Remaining injected per-sibling inspection failures. Models an ordinary
    /// transient I/O error while classifying one sibling.
    static SIBLING_INSPECTION_FAULTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn fail_next_scratch_run_creation_at(boundary: ScratchCreateBoundary) {
    CREATE_RUN_FAULT.with(|fault| fault.set(Some(boundary)));
}

#[cfg(test)]
pub(crate) fn fail_next_scratch_sibling_inspections(count: usize) {
    SIBLING_INSPECTION_FAULTS.with(|faults| faults.set(count));
}

#[cfg(test)]
fn inject_create_run_fault(boundary: ScratchCreateBoundary) -> Result<(), ScratchError> {
    CREATE_RUN_FAULT.with(|fault| {
        if fault.get() == Some(boundary) {
            fault.set(None);
            return Err(ScratchError::Io(format!(
                "injected scratch construction failure at {boundary:?}"
            )));
        }
        Ok(())
    })
}

#[cfg(test)]
fn inject_sibling_inspection_fault() -> Result<(), ScratchError> {
    SIBLING_INSPECTION_FAULTS.with(|faults| {
        let remaining = faults.get();
        if remaining == 0 {
            return Ok(());
        }
        faults.set(remaining - 1);
        Err(ScratchError::Io(
            "injected scratch sibling inspection failure".into(),
        ))
    })
}

fn create_new_regular(dir: &Dir, name: &str) -> Result<fs::File, ScratchError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    let file = dir.open_with(name, &options)?.into_std();
    ensure_opened_regular(&file, name)?;
    Ok(file)
}

fn write_new_regular(dir: &Dir, name: &str, bytes: &[u8]) -> Result<(), ScratchError> {
    let mut file = create_new_regular(dir, name)?;
    file.write_all(bytes)?;
    Ok(())
}

fn open_regular_read_write_nofollow(dir: &Dir, name: &str) -> Result<fs::File, ScratchError> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::AsFd as _;
        let path = CString::new(name)
            .map_err(|_| ScratchError::UnsafeEntry("invalid scratch filename".into()))?;
        // SAFETY: the path is a live C string and dirfd is an opened capability.
        let fd = unsafe {
            libc::openat(
                dir.as_fd().as_raw_fd(),
                path.as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: a successful openat returned an owned descriptor.
        let file = unsafe { fs::File::from_raw_fd(fd) };
        ensure_opened_regular(&file, name)?;
        Ok(file)
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        options.follow(FollowSymlinks::No);
        let file = dir.open_with(name, &options)?.into_std();
        ensure_opened_regular(&file, name)?;
        return Ok(file);
    }
}

fn read_regular_nofollow(dir: &Dir, name: &str, limit: u64) -> Result<Vec<u8>, ScratchError> {
    let mut file = open_regular_read_write_nofollow(dir, name)?;
    let metadata = file.metadata()?;
    if metadata.len() > limit {
        return Err(ScratchError::UnsafeEntry(format!(
            "scratch file {name:?} exceeds its bound"
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn ensure_opened_regular(file: &fs::File, name: &str) -> Result<(), ScratchError> {
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ScratchError::UnsafeEntry(format!(
                "{name:?} is a reparse point"
            )));
        }
    }
    if !metadata.is_file() {
        return Err(ScratchError::UnsafeEntry(format!(
            "{name:?} is not a regular file"
        )));
    }
    Ok(())
}

fn require_real_directory(entry: &cap_std::fs::DirEntry, name: &str) -> Result<(), ScratchError> {
    let file_type = entry.file_type()?;
    if file_type.is_symlink() || !file_type.is_dir() {
        return Err(ScratchError::UnsafeEntry(format!(
            "{name:?} is not a real directory"
        )));
    }
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if entry.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ScratchError::UnsafeEntry(format!(
                "{name:?} is a reparse point"
            )));
        }
    }
    Ok(())
}

fn require_regular_entry(entry: &cap_std::fs::DirEntry, name: &str) -> Result<(), ScratchError> {
    let file_type = entry.file_type()?;
    if file_type.is_symlink() || !file_type.is_file() {
        return Err(ScratchError::UnsafeEntry(format!(
            "{name:?} is not a regular file"
        )));
    }
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if entry.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ScratchError::UnsafeEntry(format!(
                "{name:?} is a reparse point"
            )));
        }
    }
    Ok(())
}

fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, ScratchError> {
    postcard::to_allocvec(value).map_err(|_| ScratchError::MalformedPage)
}

fn decode_canonical<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T, ScratchError> {
    let value: T = postcard::from_bytes(bytes).map_err(|_| ScratchError::MalformedPage)?;
    if encode_canonical(&value)? != bytes {
        return Err(ScratchError::MalformedPage);
    }
    Ok(value)
}

#[cfg(unix)]
fn lock_exclusive_nonblocking(file: &fs::File) -> Result<bool, ScratchError> {
    // SAFETY: flock only observes the live owned descriptor.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(error.into())
}

#[cfg(unix)]
fn unlock(file: &fs::File) {
    // SAFETY: flock only observes the live owned descriptor.
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(windows)]
fn lock_exclusive_nonblocking(file: &fs::File) -> Result<bool, ScratchError> {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, FALSE};
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    let mut overlapped = unsafe { std::mem::zeroed() };
    // SAFETY: the handle and OVERLAPPED remain live for the synchronous call.
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != FALSE {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        return Ok(false);
    }
    Err(error.into())
}

#[cfg(windows)]
fn unlock(file: &fs::File) {
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    let mut overlapped = unsafe { std::mem::zeroed() };
    // SAFETY: the handle and OVERLAPPED remain live for the synchronous call.
    let _ = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as _,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ScratchError {
    Io(String),
    UnsafeEntry(String),
    MalformedMarker(String),
    MalformedPage,
    MalformedBlob,
    PageTooLarge(usize),
    PageDigestMismatch(ContentDigest),
    BlobDigestMismatch(ContentDigest),
    PageBindingMismatch,
    KeyDigestCollision,
    IndexCapacity,
    Poisoned,
}

impl fmt::Display for ScratchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "scratch I/O failed: {error}"),
            Self::UnsafeEntry(reason) => write!(f, "unsafe scratch entry: {reason}"),
            Self::MalformedMarker(run) => write!(f, "malformed scratch marker in {run}"),
            Self::MalformedPage => write!(f, "malformed or non-canonical scratch page"),
            Self::MalformedBlob => write!(f, "malformed scratch blob"),
            Self::PageTooLarge(length) => write!(f, "scratch page is too large: {length} bytes"),
            Self::PageDigestMismatch(digest) => {
                write!(f, "scratch page digest mismatch for {digest}")
            }
            Self::BlobDigestMismatch(digest) => {
                write!(f, "scratch blob digest mismatch for {digest}")
            }
            Self::PageBindingMismatch => write!(f, "scratch page reference is misbound"),
            Self::KeyDigestCollision => {
                write!(f, "authenticated scratch point-key digest collision")
            }
            Self::IndexCapacity => write!(f, "scratch index exceeded its fixed capacity"),
            Self::Poisoned => write!(f, "scratch file lock was poisoned"),
        }
    }
}

impl std::error::Error for ScratchError {}

impl From<std::io::Error> for ScratchError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<super::object_store::StoreError> for ScratchError {
    fn from(error: super::object_store::StoreError) -> Self {
        Self::Io(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::ambient_authority;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn workspace(value: u128) -> WorkspaceId {
        WorkspaceId::from_uuid(Uuid::from_u128(value))
    }

    fn archive(root: &Path) -> Dir {
        fs::create_dir_all(root).unwrap();
        Dir::open_ambient_dir(root, ambient_authority()).unwrap()
    }

    fn scratch_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tine-scratch-{label}-{}", Uuid::new_v4()))
    }

    fn run_path(root: &Path, run_id: Uuid) -> PathBuf {
        root.join(SCRATCH_DIR).join(format!("run-{run_id}"))
    }

    /// Exact durable bytes of one scratch run. Retention proofs compare these
    /// snapshots instead of observing timing.
    fn run_snapshot(root: &Path, run_id: Uuid) -> BTreeMap<&'static str, Vec<u8>> {
        let run = run_path(root, run_id);
        [MARKER_FILE, LEASE_FILE, PAGES_FILE, BLOBS_FILE]
            .into_iter()
            .map(|name| (name, fs::read(run.join(name)).unwrap()))
            .collect()
    }

    fn write_marker(root: &Path, run_id: Uuid, bytes: &[u8]) {
        fs::write(run_path(root, run_id).join(MARKER_FILE), bytes).unwrap();
    }

    /// Retained run seeded with one authenticated page record and one blob.
    fn seed_retained_run(
        archive: &Dir,
        workspace_id: WorkspaceId,
    ) -> (Uuid, ScratchLsmRoot, ScratchBlobRef, ContentDigest) {
        let store = ScratchStore::create_retained(archive, workspace_id).unwrap();
        assert_eq!(store.retention(), ScratchRetention::Retained);
        let root = store
            .insert_many(
                &ScratchLsmRoot::default(),
                ScratchPageKind::DocumentCurrent,
                &BTreeMap::from([
                    (b"page-a".to_vec(), Some(b"alpha".to_vec())),
                    (b"page-b".to_vec(), Some(b"beta".to_vec())),
                ]),
            )
            .unwrap();
        let blob = store.append_blob(b"retained blob bytes").unwrap();
        let identity = (store.run_id(), store.binding_digest().unwrap());
        drop(store);
        (identity.0, root, blob, identity.1)
    }

    fn assert_retained_contents(
        store: &ScratchStore,
        root: &ScratchLsmRoot,
        blob: &ScratchBlobRef,
    ) {
        assert_eq!(
            store
                .lookup(root, ScratchPageKind::DocumentCurrent, b"page-a")
                .unwrap(),
            Some(b"alpha".to_vec())
        );
        assert_eq!(
            store
                .lookup(root, ScratchPageKind::DocumentCurrent, b"page-b")
                .unwrap(),
            Some(b"beta".to_vec())
        );
        assert_eq!(
            store
                .lookup(root, ScratchPageKind::DocumentCurrent, b"page-absent")
                .unwrap(),
            None
        );
        assert_eq!(
            store.read_blob(blob).unwrap(),
            b"retained blob bytes".to_vec()
        );
    }

    #[test]
    fn authenticated_lsm_is_canonical_and_newest_wins() {
        let path = std::env::temp_dir().join(format!("tine-scratch-lsm-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(1)).unwrap();
        let mut root = ScratchLsmRoot::default();
        root = store
            .insert_many(
                &root,
                ScratchPageKind::BatchStatus,
                &BTreeMap::from([
                    (b"a".to_vec(), Some(b"one".to_vec())),
                    (b"b".to_vec(), Some(b"two".to_vec())),
                ]),
            )
            .unwrap();
        root = store
            .insert_many(
                &root,
                ScratchPageKind::BatchStatus,
                &BTreeMap::from([
                    (b"a".to_vec(), Some(b"new".to_vec())),
                    (b"b".to_vec(), None),
                ]),
            )
            .unwrap();
        assert_eq!(
            store
                .lookup(&root, ScratchPageKind::BatchStatus, b"a")
                .unwrap(),
            Some(b"new".to_vec())
        );
        assert_eq!(
            store
                .lookup(&root, ScratchPageKind::BatchStatus, b"b")
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .scan_prefix(&root, ScratchPageKind::BatchStatus, b"")
                .unwrap(),
            vec![(b"a".to_vec(), b"new".to_vec())]
        );
        assert_eq!(store.stats().scratch_syncs, 0);
        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn authenticated_point_updates_have_a_cardinality_independent_physical_bound() {
        let path =
            std::env::temp_dir().join(format!("tine-scratch-point-bound-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(2)).unwrap();
        let mut root = ScratchAuthenticatedPointRoot::default();
        let max_io = AUTHENTICATED_POINT_MAX_IO_PER_MUTATION;
        let max_bytes = max_io.saturating_mul(AUTHENTICATED_POINT_MAX_PAGE_BYTES);

        for index in 0_u64..1_024 {
            let key = index.to_be_bytes();
            let value = (index ^ 0xa5a5_a5a5_a5a5_a5a5).to_be_bytes();
            let before = store.stats();
            root = store
                .authenticated_point_upsert(&root, ScratchPageKind::DependencyFanout, &key, &value)
                .unwrap();
            let after = store.stats();
            let io = after
                .page_reads
                .saturating_sub(before.page_reads)
                .saturating_add(after.page_writes.saturating_sub(before.page_writes));
            let bytes = after
                .page_bytes_read
                .saturating_sub(before.page_bytes_read)
                .saturating_add(
                    after
                        .page_bytes_written
                        .saturating_sub(before.page_bytes_written),
                );
            assert!(
                io <= max_io,
                "point update {index} used {io} page operations, bound {max_io}"
            );
            assert!(
                bytes <= max_bytes,
                "point update {index} used {bytes} bytes, bound {max_bytes}"
            );
            assert_eq!(
                store
                    .authenticated_point_lookup(&root, ScratchPageKind::DependencyFanout, &key,)
                    .unwrap(),
                Some(value.to_vec())
            );
        }
        assert_eq!(root.count(), 1_024);
        assert_eq!(
            store
                .authenticated_point_materialize(&root, ScratchPageKind::DependencyFanout,)
                .unwrap()
                .len(),
            1_024
        );
        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn authenticated_point_digest_collision_cannot_alias_logical_keys() {
        let path =
            std::env::temp_dir().join(format!("tine-scratch-point-collision-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(4)).unwrap();
        let root = store
            .authenticated_point_upsert(
                &ScratchAuthenticatedPointRoot::default(),
                ScratchPageKind::DependencyIdentity,
                b"complete-logical-key-a",
                b"value-a",
            )
            .unwrap();
        let current = AuthenticatedPointChild {
            key_digest: root.root_key_digest.unwrap(),
            digest: root.root_digest,
            page_ref: root.root.unwrap(),
        };
        assert!(matches!(
            store.authenticated_point_upsert_child(
                ScratchPageKind::DependencyIdentity,
                Some(current),
                authenticated_point_key_digest(
                    ScratchPageKind::DependencyIdentity,
                    b"complete-logical-key-a",
                ),
                b"complete-logical-key-b",
                b"value-b",
                0,
            ),
            Err(ScratchError::KeyDigestCollision)
        ));
        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn remaining_binary_lsm_carry_has_an_explicit_fixed_page_and_byte_bound() {
        let path = std::env::temp_dir().join(format!("tine-scratch-lsm-carry-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(3)).unwrap();
        let mut root = ScratchLsmRoot::default();
        for index in 0_u64..31 {
            root = store
                .insert_many(
                    &root,
                    ScratchPageKind::DocumentExact,
                    &BTreeMap::from([(
                        index.to_be_bytes().to_vec(),
                        Some(index.to_be_bytes().to_vec()),
                    )]),
                )
                .unwrap();
        }
        let before = store.stats();
        root = store
            .insert_many(
                &root,
                ScratchPageKind::DocumentExact,
                &BTreeMap::from([(
                    31_u64.to_be_bytes().to_vec(),
                    Some(31_u64.to_be_bytes().to_vec()),
                )]),
            )
            .unwrap();
        let after = store.stats();
        let reads = after.page_reads - before.page_reads;
        let writes = after.page_writes - before.page_writes;
        let bytes = after
            .page_bytes_read
            .saturating_sub(before.page_bytes_read)
            .saturating_add(
                after
                    .page_bytes_written
                    .saturating_sub(before.page_bytes_written),
            );
        assert_eq!(reads, 5, "the 32nd insert crosses five occupied levels");
        assert_eq!(writes, 1);
        assert!(reads + writes <= SCRATCH_LSM_LEVELS + 1);
        assert!(bytes <= (SCRATCH_LSM_LEVELS + 1).saturating_mul(MAX_PAGE_BYTES));
        assert_eq!(
            store
                .materialize(&root, ScratchPageKind::DocumentExact)
                .unwrap()
                .len(),
            32
        );
        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn covered_blob_dedup_negative_skips_physical_page_reads() {
        let path =
            std::env::temp_dir().join(format!("tine-scratch-dedup-negative-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(11)).unwrap();
        let root = store
            .insert_many(
                &ScratchLsmRoot::default(),
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([
                    (b"a".to_vec(), Some(b"left".to_vec())),
                    (b"z".to_vec(), Some(b"right".to_vec())),
                ]),
            )
            .unwrap();
        let before = store.stats();

        assert_eq!(
            store
                .lookup(&root, ScratchPageKind::BlobDedup, b"missing")
                .unwrap(),
            None
        );
        assert_eq!(store.stats().page_reads, before.page_reads);

        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn covered_blob_dedup_present_key_returns_canonical_bytes() {
        let path =
            std::env::temp_dir().join(format!("tine-scratch-dedup-present-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(12)).unwrap();
        let root = store
            .insert_many(
                &ScratchLsmRoot::default(),
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([(b"digest".to_vec(), Some(b"canonical-ref".to_vec()))]),
            )
            .unwrap();

        assert_eq!(
            store
                .lookup(&root, ScratchPageKind::BlobDedup, b"digest")
                .unwrap(),
            Some(b"canonical-ref".to_vec())
        );
        assert_eq!(store.stats().page_reads, 1);

        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn covered_blob_dedup_present_key_still_authenticates_tampered_page() {
        let path =
            std::env::temp_dir().join(format!("tine-scratch-dedup-tamper-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(15)).unwrap();
        let root = store
            .insert_many(
                &ScratchLsmRoot::default(),
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([(b"digest".to_vec(), Some(b"canonical-ref".to_vec()))]),
            )
            .unwrap();
        let page_offset = root
            .levels
            .iter()
            .flatten()
            .next()
            .expect("blob dedup segment")
            .page_ref
            .offset;
        store.tamper_page_byte_for_test(page_offset);

        assert!(matches!(
            store.lookup(&root, ScratchPageKind::BlobDedup, b"digest"),
            Err(ScratchError::PageDigestMismatch(_))
        ));

        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn uncovered_or_newer_blob_dedup_root_bypasses_negative_filter() {
        let path =
            std::env::temp_dir().join(format!("tine-scratch-dedup-uncovered-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(13)).unwrap();
        let root = store
            .insert_many(
                &ScratchLsmRoot::default(),
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([(b"stored".to_vec(), Some(b"value".to_vec()))]),
            )
            .unwrap();
        let mut unseen_newer = root.clone();
        unseen_newer.next_generation = unseen_newer.next_generation.saturating_add(1);
        {
            let mut filter = store
                .blob_dedup_filter
                .lock()
                .expect("blob dedup filter lock");
            filter.points = FixedPointFilter::default();
        }

        assert_eq!(
            store
                .lookup(&unseen_newer, ScratchPageKind::BlobDedup, b"stored")
                .unwrap(),
            Some(b"value".to_vec())
        );
        {
            let mut filter = store
                .blob_dedup_filter
                .lock()
                .expect("blob dedup filter lock");
            filter.covered_roots.retain(|covered| covered != &root);
        }
        assert_eq!(
            store
                .lookup(&root, ScratchPageKind::BlobDedup, b"stored")
                .unwrap(),
            Some(b"value".to_vec())
        );
        assert_eq!(store.stats().page_reads, 2);

        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn divergent_and_orphan_blob_dedup_roots_never_false_negative() {
        let path =
            std::env::temp_dir().join(format!("tine-scratch-dedup-divergent-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(14)).unwrap();
        let base = store
            .insert_many(
                &ScratchLsmRoot::default(),
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([(b"base".to_vec(), Some(b"base-value".to_vec()))]),
            )
            .unwrap();
        let orphan = store
            .insert_many(
                &base,
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([(b"orphan".to_vec(), Some(b"orphan-value".to_vec()))]),
            )
            .unwrap();
        let divergent = store
            .insert_many(
                &base,
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([(b"branch".to_vec(), Some(b"branch-value".to_vec()))]),
            )
            .unwrap();
        let tombstoned = store
            .insert_many(
                &orphan,
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([(b"orphan".to_vec(), None)]),
            )
            .unwrap();

        assert_eq!(
            store
                .lookup(&orphan, ScratchPageKind::BlobDedup, b"orphan")
                .unwrap(),
            Some(b"orphan-value".to_vec())
        );
        assert_eq!(
            store
                .lookup(&divergent, ScratchPageKind::BlobDedup, b"branch")
                .unwrap(),
            Some(b"branch-value".to_vec())
        );
        assert_eq!(
            store
                .lookup(&divergent, ScratchPageKind::BlobDedup, b"base")
                .unwrap(),
            Some(b"base-value".to_vec())
        );
        assert_eq!(
            store
                .lookup(&divergent, ScratchPageKind::BlobDedup, b"orphan")
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .lookup(&tombstoned, ScratchPageKind::BlobDedup, b"orphan")
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .blob_dedup_filter
                .lock()
                .expect("blob dedup filter lock")
                .covered_generation,
            tombstoned.next_generation
        );

        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn evicted_blob_dedup_root_falls_back_to_authenticated_lookup() {
        let path =
            std::env::temp_dir().join(format!("tine-scratch-dedup-evicted-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(14)).unwrap();
        let first_root = store
            .insert_many(
                &ScratchLsmRoot::default(),
                ScratchPageKind::BlobDedup,
                &BTreeMap::from([
                    (b"a".to_vec(), Some(b"left".to_vec())),
                    (b"z".to_vec(), Some(b"right".to_vec())),
                ]),
            )
            .unwrap();
        let mut current = first_root.clone();
        for index in 1..=MAX_COVERED_BLOB_DEDUP_ROOTS {
            current = store
                .insert_many(
                    &current,
                    ScratchPageKind::BlobDedup,
                    &BTreeMap::from([(
                        format!("key-{index:04}").into_bytes(),
                        Some(index.to_be_bytes().to_vec()),
                    )]),
                )
                .unwrap();
        }
        assert!(!store
            .blob_dedup_filter
            .lock()
            .expect("blob dedup filter lock")
            .covers_root(&first_root));
        let before = store.stats();

        assert_eq!(
            store
                .lookup(&first_root, ScratchPageKind::BlobDedup, b"middle")
                .unwrap(),
            None
        );
        assert!(store.stats().page_reads > before.page_reads);

        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn live_lease_survives_another_open_and_drop_reclaims_own_run() {
        let path = std::env::temp_dir().join(format!("tine-scratch-lease-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let first = ScratchStore::open(&archive, workspace(2)).unwrap();
        let first_name = first.run_name.clone();
        let second = ScratchStore::open(&archive, workspace(2)).unwrap();
        assert!(second.stats().live_runs_skipped >= 1);
        assert!(path.join(SCRATCH_DIR).join(&first_name).is_dir());
        drop(second);
        assert!(path.join(SCRATCH_DIR).join(&first_name).is_dir());
        drop(first);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn restart_reclaims_an_authenticated_stale_run_without_syncing() {
        let path = std::env::temp_dir().join(format!("tine-scratch-stale-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let first = ScratchStore::open(&archive, workspace(4)).unwrap();
        let run_name = first.run_name.clone();
        let marker = first.marker.clone();
        drop(first);
        let run_path = path.join(SCRATCH_DIR).join(&run_name);
        fs::create_dir(&run_path).unwrap();
        fs::write(
            run_path.join(MARKER_FILE),
            encode_canonical(&marker).unwrap(),
        )
        .unwrap();
        for name in [LEASE_FILE, PAGES_FILE, BLOBS_FILE] {
            fs::write(run_path.join(name), []).unwrap();
        }
        let restarted = ScratchStore::open(&archive, workspace(4)).unwrap();
        assert_eq!(restarted.stats().stale_runs_reclaimed, 1);
        assert_eq!(restarted.stats().scratch_syncs, 0);
        assert!(!run_path.exists());
        drop(restarted);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn restart_preserves_a_tampered_marker_without_blocking_a_fresh_run() {
        let path = std::env::temp_dir().join(format!("tine-scratch-marker-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let first = ScratchStore::open(&archive, workspace(5)).unwrap();
        let run_name = first.run_name.clone();
        drop(first);
        let run_path = path.join(SCRATCH_DIR).join(run_name);
        fs::create_dir(&run_path).unwrap();
        fs::write(run_path.join(MARKER_FILE), b"tampered").unwrap();
        for name in [LEASE_FILE, PAGES_FILE, BLOBS_FILE] {
            fs::write(run_path.join(name), []).unwrap();
        }
        // The sibling cannot be authenticated, so it is neither deleted nor
        // rewritten -- and it has no veto over a fresh run.
        let restarted = ScratchStore::open(&archive, workspace(5)).unwrap();
        assert_eq!(restarted.stats().unclassified_runs_preserved, 1);
        assert_eq!(restarted.stats().stale_runs_reclaimed, 0);
        assert!(run_path.exists());
        assert_eq!(
            fs::read(run_path.join(MARKER_FILE)).unwrap(),
            b"tampered".to_vec()
        );
        drop(restarted);
        fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_refuses_symlink_entries_without_following_them() {
        use std::os::unix::fs::symlink;
        let path = std::env::temp_dir().join(format!("tine-scratch-link-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let first = ScratchStore::open(&archive, workspace(3)).unwrap();
        let run_path = path.join(SCRATCH_DIR).join(&first.run_name);
        drop(first);
        fs::create_dir(&run_path).unwrap();
        symlink("/tmp", run_path.join("marker")).unwrap();
        fs::write(run_path.join("lease"), []).unwrap();
        fs::write(run_path.join("pages.index"), []).unwrap();
        fs::write(run_path.join("blobs.data"), []).unwrap();
        let restarted = ScratchStore::open(&archive, workspace(3)).unwrap();
        assert_eq!(restarted.stats().unclassified_runs_preserved, 1);
        assert_eq!(restarted.stats().stale_runs_reclaimed, 0);
        // The no-follow capability discipline is unchanged: the link target is
        // never opened, and the link itself is preserved rather than unlinked.
        assert!(Path::new("/tmp").is_dir());
        assert!(fs::symlink_metadata(run_path.join("marker"))
            .unwrap()
            .file_type()
            .is_symlink());
        drop(restarted);
        fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_refuses_special_entries_without_unlinking_them() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let path = std::env::temp_dir().join(format!("tine-scratch-fifo-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let first = ScratchStore::open(&archive, workspace(6)).unwrap();
        let run_name = first.run_name.clone();
        let marker = first.marker.clone();
        drop(first);
        let run_path = path.join(SCRATCH_DIR).join(run_name);
        fs::create_dir(&run_path).unwrap();
        fs::write(
            run_path.join(MARKER_FILE),
            encode_canonical(&marker).unwrap(),
        )
        .unwrap();
        fs::write(run_path.join(LEASE_FILE), []).unwrap();
        fs::write(run_path.join(BLOBS_FILE), []).unwrap();
        let fifo = run_path.join(PAGES_FILE);
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo_c` is a live NUL-terminated path in this test directory.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let restarted = ScratchStore::open(&archive, workspace(6)).unwrap();
        assert_eq!(restarted.stats().unclassified_runs_preserved, 1);
        assert_eq!(restarted.stats().stale_runs_reclaimed, 0);
        assert!(fifo.exists());
        drop(restarted);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn ephemeral_run_declares_its_mode_and_drop_removes_every_byte() {
        let path = scratch_root("ephemeral-mode");
        let archive = archive(&path);
        let store = ScratchStore::open(&archive, workspace(21)).unwrap();
        assert_eq!(store.retention(), ScratchRetention::Ephemeral);
        assert_eq!(store.stats().retained_runs_preserved, 0);
        let run_id = store.run_id();
        let run = run_path(&path, run_id);
        assert!(run.is_dir());
        assert_eq!(run_snapshot(&path, run_id).len(), 4);

        drop(store);

        assert!(!run.exists());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn retained_run_survives_drop_and_readopts_with_identical_bytes() {
        let path = scratch_root("retained-readopt");
        let archive = archive(&path);
        let (run_id, root, blob, binding) = seed_retained_run(&archive, workspace(22));

        let after_owner = run_snapshot(&path, run_id);
        assert!(!after_owner[PAGES_FILE].is_empty());
        assert!(!after_owner[BLOBS_FILE].is_empty());

        let adopted = ScratchStore::adopt_retained(&archive, workspace(22), run_id).unwrap();
        assert_eq!(adopted.run_id(), run_id);
        assert_eq!(adopted.retention(), ScratchRetention::Retained);
        // The marker is the run's identity; adoption must not mint a new one.
        assert_eq!(adopted.binding_digest().unwrap(), binding);
        assert_retained_contents(&adopted, &root, &blob);

        drop(adopted);
        assert_eq!(run_snapshot(&path, run_id), after_owner);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn adoption_appends_after_existing_bytes_without_rewriting_them() {
        let path = scratch_root("retained-append");
        let archive = archive(&path);
        let (run_id, root, blob, _) = seed_retained_run(&archive, workspace(23));
        let before = run_snapshot(&path, run_id);

        let adopted = ScratchStore::adopt_retained(&archive, workspace(23), run_id).unwrap();
        let extended = adopted
            .insert_many(
                &root,
                ScratchPageKind::DocumentCurrent,
                &BTreeMap::from([(b"page-c".to_vec(), Some(b"gamma".to_vec()))]),
            )
            .unwrap();
        let appended_blob = adopted.append_blob(b"second blob").unwrap();
        drop(adopted);

        let after = run_snapshot(&path, run_id);
        assert_eq!(after[MARKER_FILE], before[MARKER_FILE]);
        for name in [PAGES_FILE, BLOBS_FILE] {
            assert!(after[name].len() > before[name].len());
            assert_eq!(&after[name][..before[name].len()], &before[name][..]);
        }

        // Both the carried root and the extended root remain readable.
        let reopened = ScratchStore::adopt_retained(&archive, workspace(23), run_id).unwrap();
        assert_retained_contents(&reopened, &root, &blob);
        assert_eq!(
            reopened
                .lookup(&extended, ScratchPageKind::DocumentCurrent, b"page-c")
                .unwrap(),
            Some(b"gamma".to_vec())
        );
        assert_eq!(
            reopened.read_blob(&appended_blob).unwrap(),
            b"second blob".to_vec()
        );
        drop(reopened);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn repeated_adopt_and_drop_never_change_bytes_or_marker_identity() {
        let path = scratch_root("retained-repeat");
        let archive = archive(&path);
        let (run_id, root, blob, binding) = seed_retained_run(&archive, workspace(24));
        let baseline = run_snapshot(&path, run_id);

        for _ in 0..3 {
            let adopted = ScratchStore::adopt_retained(&archive, workspace(24), run_id).unwrap();
            assert_eq!(adopted.run_id(), run_id);
            assert_eq!(adopted.binding_digest().unwrap(), binding);
            assert_retained_contents(&adopted, &root, &blob);
            drop(adopted);
            assert_eq!(run_snapshot(&path, run_id), baseline);
        }
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn adoption_fails_while_an_owner_is_live_and_succeeds_after_a_clean_drop() {
        let path = scratch_root("retained-contention");
        let archive = archive(&path);
        let owner = ScratchStore::create_retained(&archive, workspace(25)).unwrap();
        let run_id = owner.run_id();

        assert!(ScratchStore::adopt_retained(&archive, workspace(25), run_id).is_err());
        drop(owner);

        let adopted = ScratchStore::adopt_retained(&archive, workspace(25), run_id).unwrap();
        // The adopting owner now holds the same exclusive lease.
        assert!(ScratchStore::adopt_retained(&archive, workspace(25), run_id).is_err());
        drop(adopted);

        let last = ScratchStore::adopt_retained(&archive, workspace(25), run_id).unwrap();
        assert_eq!(last.run_id(), run_id);
        drop(last);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn adoption_rejects_a_wrong_workspace_or_a_run_identity_that_does_not_exist() {
        let path = scratch_root("retained-identity");
        let archive = archive(&path);
        let (run_id, root, blob, _) = seed_retained_run(&archive, workspace(26));
        let absent = Uuid::new_v4();

        assert!(ScratchStore::adopt_retained(&archive, workspace(27), run_id).is_err());
        assert!(ScratchStore::adopt_retained(&archive, workspace(26), absent).is_err());
        // A rejected adoption never fabricates a run under the requested identity.
        assert!(!run_path(&path, absent).exists());

        let adopted = ScratchStore::adopt_retained(&archive, workspace(26), run_id).unwrap();
        assert_retained_contents(&adopted, &root, &blob);
        drop(adopted);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn adoption_rejects_marker_schema_workspace_mode_and_identity_tamper() {
        let path = scratch_root("retained-marker-tamper");
        let archive = archive(&path);
        let (run_id, root, blob, _) = seed_retained_run(&archive, workspace(28));
        let authentic = run_snapshot(&path, run_id)[MARKER_FILE].clone();
        let marker: ScratchRunMarkerV3 = decode_canonical(&authentic).unwrap();

        let tampered = [
            ScratchRunMarkerV3 {
                schema_version: SCRATCH_SCHEMA_VERSION + 1,
                ..marker.clone()
            },
            ScratchRunMarkerV3 {
                retention: ScratchRetention::Ephemeral,
                ..marker.clone()
            },
            ScratchRunMarkerV3 {
                workspace_id: workspace(29),
                ..marker.clone()
            },
            ScratchRunMarkerV3 {
                run_id: Uuid::new_v4(),
                ..marker.clone()
            },
        ];
        for variant in tampered {
            write_marker(&path, run_id, &encode_canonical(&variant).unwrap());
            assert!(
                ScratchStore::adopt_retained(&archive, workspace(28), run_id).is_err(),
                "adopted a run with marker {variant:?}"
            );
        }
        write_marker(&path, run_id, b"not a marker at all");
        assert!(ScratchStore::adopt_retained(&archive, workspace(28), run_id).is_err());

        // The authentic marker still adopts, so the rejections above are causal.
        write_marker(&path, run_id, &authentic);
        let adopted = ScratchStore::adopt_retained(&archive, workspace(28), run_id).unwrap();
        assert_retained_contents(&adopted, &root, &blob);
        drop(adopted);
        fs::remove_dir_all(path).unwrap();
    }

    /// Exact schema-11 marker shape. It exists only to prove those bytes are
    /// rejected; there is no legacy decode or migration path.
    #[derive(Serialize)]
    struct LegacyScratchRunMarkerV11 {
        schema_version: u32,
        workspace_id: WorkspaceId,
        run_id: Uuid,
        random_owner_nonce: [u8; 32],
    }

    #[test]
    fn schema_eleven_marker_bytes_are_rejected_and_never_migrated() {
        let path = scratch_root("retained-schema-11");
        let archive = archive(&path);
        let (run_id, _, _, _) = seed_retained_run(&archive, workspace(30));
        let legacy = encode_canonical(&LegacyScratchRunMarkerV11 {
            schema_version: 11,
            workspace_id: workspace(30),
            run_id,
            random_owner_nonce: [7_u8; 32],
        })
        .unwrap();
        let authentic = run_snapshot(&path, run_id)[MARKER_FILE].clone();
        write_marker(&path, run_id, &legacy);

        assert!(ScratchStore::adopt_retained(&archive, workspace(30), run_id).is_err());
        // Ordinary reclamation also refuses the legacy bytes rather than
        // rewriting or deleting them -- and, because it cannot authenticate
        // them, it skips the run instead of vetoing a fresh open.
        let restarted = ScratchStore::open(&archive, workspace(30)).unwrap();
        assert_eq!(restarted.stats().unclassified_runs_preserved, 1);
        assert_eq!(restarted.stats().stale_runs_reclaimed, 0);
        assert_eq!(restarted.stats().retained_runs_preserved, 0);
        drop(restarted);
        assert_eq!(run_snapshot(&path, run_id)[MARKER_FILE], legacy);

        write_marker(&path, run_id, &authentic);
        drop(ScratchStore::adopt_retained(&archive, workspace(30), run_id).unwrap());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn adoption_rejects_absent_entries_and_partially_created_runs() {
        // Each damaged shape gets its own namespace: a damaged run legitimately
        // blocks later reclamation, which would mask the adoption verdict.
        for missing in [MARKER_FILE, LEASE_FILE, PAGES_FILE, BLOBS_FILE] {
            let path = scratch_root("retained-partial-missing");
            let archive = archive(&path);
            let (run_id, _, _, _) = seed_retained_run(&archive, workspace(31));
            fs::remove_file(run_path(&path, run_id).join(missing)).unwrap();
            assert!(
                ScratchStore::adopt_retained(&archive, workspace(31), run_id).is_err(),
                "adopted a run missing {missing}"
            );
            fs::remove_dir_all(path).unwrap();
        }

        {
            let path = scratch_root("retained-partial-stray");
            let archive = archive(&path);
            let (run_id, _, _, _) = seed_retained_run(&archive, workspace(31));
            fs::write(run_path(&path, run_id).join("stray"), b"extra").unwrap();
            assert!(ScratchStore::adopt_retained(&archive, workspace(31), run_id).is_err());
            fs::remove_dir_all(path).unwrap();
        }

        // A bare directory is a partially created run, never an adoptable one.
        {
            let path = scratch_root("retained-partial-bare");
            let archive = archive(&path);
            drop(ScratchStore::open(&archive, workspace(31)).unwrap());
            let bare = Uuid::new_v4();
            fs::create_dir(run_path(&path, bare)).unwrap();
            assert!(ScratchStore::adopt_retained(&archive, workspace(31), bare).is_err());
            assert_eq!(fs::read_dir(run_path(&path, bare)).unwrap().count(), 0);
            fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn adoption_fails_closed_on_substituted_or_truncated_data() {
        let path = scratch_root("retained-substitution");
        let archive = archive(&path);

        let (substituted_id, substituted_root, _, _) = seed_retained_run(&archive, workspace(32));
        let pages = run_path(&path, substituted_id).join(PAGES_FILE);
        let mut bytes = fs::read(&pages).unwrap();
        bytes[0] ^= 0x80;
        fs::write(&pages, &bytes).unwrap();
        let adopted =
            ScratchStore::adopt_retained(&archive, workspace(32), substituted_id).unwrap();
        assert!(adopted
            .lookup(
                &substituted_root,
                ScratchPageKind::DocumentCurrent,
                b"page-a"
            )
            .is_err());
        drop(adopted);

        let (truncated_id, truncated_root, truncated_blob, _) =
            seed_retained_run(&archive, workspace(32));
        for name in [PAGES_FILE, BLOBS_FILE] {
            fs::OpenOptions::new()
                .write(true)
                .open(run_path(&path, truncated_id).join(name))
                .unwrap()
                .set_len(0)
                .unwrap();
        }
        let adopted = ScratchStore::adopt_retained(&archive, workspace(32), truncated_id).unwrap();
        // Truncation has no durable extent authority in this stage, so it is
        // caught at the authenticated read rather than silently answering
        // "absent" or returning replacement bytes.
        assert!(adopted
            .lookup(&truncated_root, ScratchPageKind::DocumentCurrent, b"page-a")
            .is_err());
        assert!(adopted.read_blob(&truncated_blob).is_err());
        drop(adopted);
        fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn adoption_refuses_symlinked_entries_and_aliased_run_directories() {
        use std::os::unix::fs::symlink;

        let path = scratch_root("retained-symlink");
        let archive = archive(&path);
        let (run_id, _, _, _) = seed_retained_run(&archive, workspace(33));

        // A stale path alias must never resolve to another run's bytes.
        let alias = Uuid::new_v4();
        symlink(run_path(&path, run_id), run_path(&path, alias)).unwrap();
        assert!(ScratchStore::adopt_retained(&archive, workspace(33), alias).is_err());
        fs::remove_file(run_path(&path, alias)).unwrap();

        let decoy = path.join("decoy");
        fs::write(&decoy, b"decoy bytes").unwrap();
        let pages = run_path(&path, run_id).join(PAGES_FILE);
        fs::remove_file(&pages).unwrap();
        symlink(&decoy, &pages).unwrap();
        assert!(ScratchStore::adopt_retained(&archive, workspace(33), run_id).is_err());
        assert_eq!(fs::read(&decoy).unwrap(), b"decoy bytes".to_vec());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn orphan_retained_runs_survive_ordinary_stale_run_reclamation() {
        let path = scratch_root("retained-orphan");
        let archive = archive(&path);
        let (retained_id, root, blob, _) = seed_retained_run(&archive, workspace(34));
        let retained_bytes = run_snapshot(&path, retained_id);

        // Synthesize a stale ephemeral run exactly as the existing reclamation
        // regression does.
        let stale = ScratchStore::open(&archive, workspace(34)).unwrap();
        let stale_id = stale.run_id();
        let stale_marker = stale.marker.clone();
        drop(stale);
        let stale_path = run_path(&path, stale_id);
        fs::create_dir(&stale_path).unwrap();
        fs::write(
            stale_path.join(MARKER_FILE),
            encode_canonical(&stale_marker).unwrap(),
        )
        .unwrap();
        for name in [LEASE_FILE, PAGES_FILE, BLOBS_FILE] {
            fs::write(stale_path.join(name), []).unwrap();
        }

        let reclaimer = ScratchStore::open(&archive, workspace(34)).unwrap();
        assert_eq!(reclaimer.stats().stale_runs_reclaimed, 1);
        assert_eq!(reclaimer.stats().retained_runs_preserved, 1);
        assert!(!stale_path.exists());
        assert_eq!(run_snapshot(&path, retained_id), retained_bytes);
        drop(reclaimer);

        let adopted = ScratchStore::adopt_retained(&archive, workspace(34), retained_id).unwrap();
        assert_retained_contents(&adopted, &root, &blob);
        drop(adopted);
        fs::remove_dir_all(path).unwrap();
    }

    /// Durable shapes of a sibling scratch run that a reclamation pass cannot
    /// authenticate. Every one of them is a real crash, disk-error, or
    /// old-build residue, and none of them may block a fresh scratch open.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum UnclassifiableSibling {
        /// Construction died between `mkdir` and the marker write.
        BareDirectory,
        /// Crash or ENOSPC part-way through the marker write.
        TornMarker,
        /// A run left by a schema-11 build.
        OldSchemaMarker,
        /// A marker naming a different run than its own directory.
        MisboundMarker,
        /// A well-formed run belonging to a different workspace.
        ForeignWorkspace,
        /// Construction died after the marker and before the lease.
        MissingLease,
        /// Construction died after the lease and before the page index.
        MissingIndex,
        /// Construction died after the page index and before the blob file.
        MissingBlob,
        /// Residue an interrupted removal or a foreign writer left behind.
        StrayEntry,
        /// An entry that is not a canonical `run-<uuid>` directory at all.
        UnknownEntryName,
    }

    const UNCLASSIFIABLE_SIBLINGS: [UnclassifiableSibling; 10] = [
        UnclassifiableSibling::BareDirectory,
        UnclassifiableSibling::TornMarker,
        UnclassifiableSibling::OldSchemaMarker,
        UnclassifiableSibling::MisboundMarker,
        UnclassifiableSibling::ForeignWorkspace,
        UnclassifiableSibling::MissingLease,
        UnclassifiableSibling::MissingIndex,
        UnclassifiableSibling::MissingBlob,
        UnclassifiableSibling::StrayEntry,
        UnclassifiableSibling::UnknownEntryName,
    ];

    fn namespace_dir(root: &Path) -> PathBuf {
        let namespace = root.join(SCRATCH_DIR);
        fs::create_dir_all(&namespace).unwrap();
        namespace
    }

    /// Every entry name directly beneath the scratch namespace.
    fn namespace_entry_names(root: &Path) -> BTreeSet<String> {
        fs::read_dir(root.join(SCRATCH_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// Exact durable bytes of every entry directly beneath one directory.
    /// `None` marks an entry whose bytes cannot be read as a regular file.
    fn dir_snapshot(path: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
        fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).ok(),
                )
            })
            .collect()
    }

    /// Authenticated ephemeral run whose owner died without releasing it.
    fn seed_stale_ephemeral_run(root: &Path, workspace_value: u128) -> PathBuf {
        let run_id = Uuid::new_v4();
        let path = namespace_dir(root).join(format!("run-{run_id}"));
        fs::create_dir(&path).unwrap();
        fs::write(
            path.join(MARKER_FILE),
            encode_canonical(&ScratchRunMarkerV3 {
                schema_version: SCRATCH_SCHEMA_VERSION,
                workspace_id: workspace(workspace_value),
                run_id,
                retention: ScratchRetention::Ephemeral,
                random_owner_nonce: [3_u8; 32],
            })
            .unwrap(),
        )
        .unwrap();
        for name in [LEASE_FILE, PAGES_FILE, BLOBS_FILE] {
            fs::write(path.join(name), []).unwrap();
        }
        path
    }

    fn seed_unclassifiable_sibling(
        root: &Path,
        workspace_value: u128,
        kind: UnclassifiableSibling,
    ) -> PathBuf {
        let namespace = namespace_dir(root);
        if kind == UnclassifiableSibling::UnknownEntryName {
            let path = namespace.join("not-a-run");
            fs::create_dir(&path).unwrap();
            return path;
        }
        let run_id = Uuid::new_v4();
        let path = namespace.join(format!("run-{run_id}"));
        fs::create_dir(&path).unwrap();
        if kind == UnclassifiableSibling::BareDirectory {
            return path;
        }
        let marker = ScratchRunMarkerV3 {
            schema_version: SCRATCH_SCHEMA_VERSION,
            workspace_id: workspace(workspace_value),
            run_id,
            retention: ScratchRetention::Ephemeral,
            random_owner_nonce: [9_u8; 32],
        };
        let marker_bytes = match kind {
            UnclassifiableSibling::TornMarker => {
                let mut bytes = encode_canonical(&marker).unwrap();
                bytes.truncate(bytes.len() / 2);
                bytes
            }
            UnclassifiableSibling::OldSchemaMarker => {
                encode_canonical(&LegacyScratchRunMarkerV11 {
                    schema_version: 11,
                    workspace_id: workspace(workspace_value),
                    run_id,
                    random_owner_nonce: [9_u8; 32],
                })
                .unwrap()
            }
            UnclassifiableSibling::MisboundMarker => encode_canonical(&ScratchRunMarkerV3 {
                run_id: Uuid::new_v4(),
                ..marker.clone()
            })
            .unwrap(),
            UnclassifiableSibling::ForeignWorkspace => encode_canonical(&ScratchRunMarkerV3 {
                workspace_id: workspace(workspace_value.wrapping_add(1_000)),
                ..marker.clone()
            })
            .unwrap(),
            _ => encode_canonical(&marker).unwrap(),
        };
        fs::write(path.join(MARKER_FILE), marker_bytes).unwrap();
        for name in [LEASE_FILE, PAGES_FILE, BLOBS_FILE] {
            let omit = match kind {
                UnclassifiableSibling::MissingLease => name == LEASE_FILE,
                UnclassifiableSibling::MissingIndex => name == PAGES_FILE,
                UnclassifiableSibling::MissingBlob => name == BLOBS_FILE,
                _ => false,
            };
            if !omit {
                fs::write(path.join(name), []).unwrap();
            }
        }
        if kind == UnclassifiableSibling::StrayEntry {
            fs::write(path.join("stray"), b"residue").unwrap();
        }
        path
    }

    /// Scratch run construction is transactional from the namespace's
    /// viewpoint: a failure at any boundary after the run directory exists
    /// leaves no self-created residue, and never touches a sibling.
    #[test]
    fn failure_at_every_construction_boundary_leaves_no_self_created_residue() {
        for boundary in ScratchCreateBoundary::ALL {
            let path = scratch_root("construction-boundary");
            let archive = archive(&path);

            // A live sibling proves the cleanup is scoped to the failing run.
            let live = ScratchStore::open(&archive, workspace(40)).unwrap();
            let live_name = live.run_name.clone();
            let live_bytes = run_snapshot(&path, live.run_id());

            fail_next_scratch_run_creation_at(boundary);
            let error = ScratchStore::open(&archive, workspace(40)).unwrap_err();
            assert!(
                error.to_string().contains(&format!("{boundary:?}")),
                "boundary {boundary:?} produced an unrelated error: {error}"
            );

            assert_eq!(
                namespace_entry_names(&path),
                BTreeSet::from([live_name.clone()]),
                "boundary {boundary:?} left self-created residue"
            );
            assert_eq!(
                run_snapshot(&path, live.run_id()),
                live_bytes,
                "boundary {boundary:?} disturbed a sibling"
            );

            // The namespace is still usable immediately afterwards.
            let recovered = ScratchStore::open(&archive, workspace(40)).unwrap();
            drop(recovered);
            drop(live);
            fs::remove_dir_all(path).unwrap();
        }
    }

    /// Reclamation carries no veto over a fresh open: a sibling that cannot be
    /// authenticated is preserved untouched and skipped.
    #[test]
    fn an_unclassifiable_sibling_cannot_block_a_fresh_scratch_open() {
        for kind in UNCLASSIFIABLE_SIBLINGS {
            let path = scratch_root("unclassifiable-sibling");
            let archive = archive(&path);
            let sibling = seed_unclassifiable_sibling(&path, 41, kind);
            let before = dir_snapshot(&sibling);

            let store = ScratchStore::open(&archive, workspace(41))
                .unwrap_or_else(|error| panic!("{kind:?} blocked a fresh open: {error}"));
            let stats = store.stats();
            assert_eq!(stats.unclassified_runs_preserved, 1, "{kind:?}");
            assert_eq!(stats.stale_runs_reclaimed, 0, "{kind:?}");
            assert_eq!(stats.retained_runs_preserved, 0, "{kind:?}");

            // Scratch bytes are never treated as authoritative user data, but
            // uncertain residue is preserved rather than silently deleted.
            assert!(sibling.is_dir(), "{kind:?} residue was removed");
            assert_eq!(dir_snapshot(&sibling), before, "{kind:?} residue changed");

            drop(store);
            fs::remove_dir_all(path).unwrap();
        }
    }

    /// One unclassifiable sibling does not stop the same pass from reclaiming
    /// a genuinely stale run or from preserving live and retained runs.
    #[test]
    fn an_unclassifiable_sibling_does_not_suppress_reclamation_or_preservation() {
        let path = scratch_root("mixed-siblings");
        let archive = archive(&path);

        let (retained_id, root, blob, _) = seed_retained_run(&archive, workspace(42));
        let retained_bytes = run_snapshot(&path, retained_id);
        let live = ScratchStore::open(&archive, workspace(42)).unwrap();
        let live_id = live.run_id();
        let live_bytes = run_snapshot(&path, live_id);
        let stale = seed_stale_ephemeral_run(&path, 42);
        let siblings = UNCLASSIFIABLE_SIBLINGS
            .map(|kind| (kind, seed_unclassifiable_sibling(&path, 42, kind)));

        let reclaimer = ScratchStore::open(&archive, workspace(42)).unwrap();
        let stats = reclaimer.stats();
        assert_eq!(stats.stale_runs_reclaimed, 1);
        assert_eq!(stats.retained_runs_preserved, 1);
        assert_eq!(stats.live_runs_skipped, 1);
        assert_eq!(
            stats.unclassified_runs_preserved,
            UNCLASSIFIABLE_SIBLINGS.len()
        );

        assert!(!stale.exists(), "authenticated stale run survived");
        assert_eq!(run_snapshot(&path, retained_id), retained_bytes);
        assert_eq!(run_snapshot(&path, live_id), live_bytes);
        for (kind, sibling) in &siblings {
            assert!(sibling.is_dir(), "{kind:?} was removed");
        }
        drop(reclaimer);

        // The retained run is still adoptable with its exact original bytes.
        let adopted = ScratchStore::adopt_retained(&archive, workspace(42), retained_id).unwrap();
        assert_retained_contents(&adopted, &root, &blob);
        drop(adopted);
        drop(live);
        fs::remove_dir_all(path).unwrap();
    }

    /// An ordinary transient I/O error while inspecting one sibling is not
    /// durable poison: the next pass classifies and reclaims the same run.
    #[test]
    fn a_transient_sibling_inspection_error_never_blocks_the_workspace() {
        let path = scratch_root("transient-sibling");
        let archive = archive(&path);
        let stale = seed_stale_ephemeral_run(&path, 43);

        fail_next_scratch_sibling_inspections(1);
        let first = ScratchStore::open(&archive, workspace(43)).unwrap();
        assert_eq!(first.stats().unclassified_runs_preserved, 1);
        assert_eq!(first.stats().stale_runs_reclaimed, 0);
        // Nothing was proved about the sibling, so nothing was removed.
        assert!(stale.is_dir());
        drop(first);

        let second = ScratchStore::open(&archive, workspace(43)).unwrap();
        assert_eq!(second.stats().unclassified_runs_preserved, 0);
        assert_eq!(second.stats().stale_runs_reclaimed, 1);
        assert!(!stale.exists());
        drop(second);
        fs::remove_dir_all(path).unwrap();
    }

    /// Repeated open/retry converges and never weakens lease contention: an
    /// authenticated live run keeps its lease and its exact bytes.
    #[test]
    fn repeated_opens_converge_without_disturbing_an_authenticated_live_run() {
        let path = scratch_root("repeated-open");
        let archive = archive(&path);
        let live = ScratchStore::open(&archive, workspace(44)).unwrap();
        let live_id = live.run_id();
        let live_bytes = run_snapshot(&path, live_id);
        let siblings =
            UNCLASSIFIABLE_SIBLINGS.map(|kind| seed_unclassifiable_sibling(&path, 44, kind));
        let sibling_bytes = siblings.each_ref().map(|sibling| dir_snapshot(sibling));

        for attempt in 0..3 {
            let store = ScratchStore::open(&archive, workspace(44))
                .unwrap_or_else(|error| panic!("attempt {attempt} was blocked: {error}"));
            let stats = store.stats();
            // The live run's lease is observed as held, not broken.
            assert_eq!(stats.live_runs_skipped, 1, "attempt {attempt}");
            assert_eq!(stats.stale_runs_reclaimed, 0, "attempt {attempt}");
            assert_eq!(
                stats.unclassified_runs_preserved,
                UNCLASSIFIABLE_SIBLINGS.len(),
                "attempt {attempt}"
            );
            drop(store);
            assert_eq!(
                run_snapshot(&path, live_id),
                live_bytes,
                "attempt {attempt} disturbed the live run"
            );
            assert_eq!(
                siblings.each_ref().map(|sibling| dir_snapshot(sibling)),
                sibling_bytes,
                "attempt {attempt} disturbed preserved residue"
            );
        }

        // The live owner is still functional after every retry.
        let root = live
            .insert_many(
                &ScratchLsmRoot::default(),
                ScratchPageKind::DocumentCurrent,
                &BTreeMap::from([(b"page-live".to_vec(), Some(b"live".to_vec()))]),
            )
            .unwrap();
        assert_eq!(
            live.lookup(&root, ScratchPageKind::DocumentCurrent, b"page-live")
                .unwrap(),
            Some(b"live".to_vec())
        );
        drop(live);
        fs::remove_dir_all(path).unwrap();
    }

    const ADOPT_HELPER_ROOT: &str = "TINE_SCRATCH_ADOPT_HELPER_ROOT";
    const ADOPT_HELPER_RUN: &str = "TINE_SCRATCH_ADOPT_HELPER_RUN";
    const ADOPT_HELPER_WORKSPACE: &str = "TINE_SCRATCH_ADOPT_HELPER_WORKSPACE";

    #[test]
    #[ignore = "subprocess helper invoked by forked_owner_blocks_adoption_until_release"]
    fn retained_adoption_subprocess_helper() {
        use std::io::BufRead as _;

        let Ok(root) = std::env::var(ADOPT_HELPER_ROOT) else {
            return;
        };
        let run_id = Uuid::parse_str(&std::env::var(ADOPT_HELPER_RUN).unwrap()).unwrap();
        let workspace_id = workspace(
            std::env::var(ADOPT_HELPER_WORKSPACE)
                .unwrap()
                .parse()
                .unwrap(),
        );
        let archive = Dir::open_ambient_dir(Path::new(&root), ambient_authority()).unwrap();
        let store = ScratchStore::adopt_retained(&archive, workspace_id, run_id).unwrap();
        println!("adopted");
        std::io::stdout().flush().unwrap();
        let mut command = String::new();
        std::io::BufReader::new(std::io::stdin())
            .read_line(&mut command)
            .unwrap();
        drop(store);
        println!("released");
        std::io::stdout().flush().unwrap();
    }

    fn spawn_adoption_helper(
        root: &Path,
        workspace_value: u128,
        run_id: Uuid,
    ) -> (
        std::process::Child,
        std::io::BufReader<std::process::ChildStdout>,
    ) {
        use std::io::BufRead as _;

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("retained_adoption_subprocess_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .env(ADOPT_HELPER_ROOT, root.as_os_str())
            .env(ADOPT_HELPER_RUN, run_id.to_string())
            .env(ADOPT_HELPER_WORKSPACE, workspace_value.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        // A blocking read, not a timed wait: the helper owns the lease once it
        // reports adoption.
        loop {
            let mut line = String::new();
            assert_ne!(
                reader.read_line(&mut line).unwrap(),
                0,
                "helper never adopted"
            );
            if line.trim() == "adopted" {
                return (child, reader);
            }
        }
    }

    #[test]
    fn forked_owner_blocks_adoption_until_release() {
        use std::io::BufRead as _;

        let path = scratch_root("retained-forked");
        let archive = archive(&path);
        let (run_id, root, blob, binding) = seed_retained_run(&archive, workspace(35));
        let baseline = run_snapshot(&path, run_id);

        // Process death without a clean drop still releases the lease.
        let (mut killed, _reader) = spawn_adoption_helper(&path, 35, run_id);
        assert!(ScratchStore::adopt_retained(&archive, workspace(35), run_id).is_err());
        killed.kill().unwrap();
        assert!(!killed.wait().unwrap().success());
        let after_death = ScratchStore::adopt_retained(&archive, workspace(35), run_id).unwrap();
        assert_eq!(after_death.binding_digest().unwrap(), binding);
        assert_retained_contents(&after_death, &root, &blob);
        drop(after_death);

        // A clean drop in the owning process releases it too.
        let (mut released, mut reader) = spawn_adoption_helper(&path, 35, run_id);
        assert!(ScratchStore::adopt_retained(&archive, workspace(35), run_id).is_err());
        released
            .stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "released");
        assert!(released.wait().unwrap().success());
        let after_release = ScratchStore::adopt_retained(&archive, workspace(35), run_id).unwrap();
        assert_retained_contents(&after_release, &root, &blob);
        drop(after_release);

        assert_eq!(run_snapshot(&path, run_id), baseline);
        fs::remove_dir_all(path).unwrap();
    }
}
