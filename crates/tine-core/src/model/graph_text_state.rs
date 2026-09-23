//! State records behind graph-text admission: the open-time recovery summary,
//! external-observation tickets, the guarded identity index state, the
//! complete admission index, prepared upserts and removes, and batch charges.

use super::*;

/// Outcome of the checked-open interrupted-publication walk. The surviving
/// claimant set is deliberately path-based: it is consulted only to prevent an
/// absent journal target from being misclassified as an external deletion.
#[derive(Debug, Default)]
pub struct RecoverySummary {
    pub(super) reconciled: usize,
    pub(super) claimants: std::collections::BTreeSet<GraphTextPath>,
}

impl RecoverySummary {
    pub(super) fn record_claimant(&mut self, graph: &Graph, target: &Path) -> io::Result<()> {
        let relative = target.strip_prefix(&graph.root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "editor recovery claimant target escapes the graph",
            )
        })?;
        let relative = relative.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "editor recovery claimant target is not UTF-8",
            )
        })?;
        let portable = GraphTextPath::parse(relative).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("editor recovery claimant target is not portable: {error}"),
            )
        })?;
        self.claimants.insert(portable);
        Ok(())
    }
}

pub(super) static NEXT_EXTERNAL_OBSERVATION_INSTANCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// Opaque acknowledgement ticket for one exact `Graph` instance's raw watcher
/// frontier. A same-root reopen cannot consume a ticket minted by its retired
/// predecessor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphTextExternalObservationTicket {
    pub(super) instance: u64,
    pub(super) epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExactGraphTextStateReceipt {
    pub(super) revision: String,
    pub(super) resource_identity: ContentDigest,
}

impl GraphTextExternalObservationTicket {
    pub fn later_for_same_instance(self, other: Self) -> Option<Self> {
        (self.instance == other.instance).then_some(if self.epoch >= other.epoch {
            self
        } else {
            other
        })
    }
}

#[derive(Debug)]
pub(super) struct GraphTextAdmissionInstance;

#[derive(Default)]
pub(super) struct GuardedGraphTextIdentityState {
    pub(super) index: Option<Arc<CompleteGraphTextAdmissionIndex>>,
    pub(super) invalidated: bool,
    pub(super) invalidation_cause: Option<String>,
    /// Epoch of the shared resource state represented by `index`. Before the
    /// first lazy build, this records the epoch observed at Graph open so the
    /// warm cache is reusable only if no sibling transition intervened.
    pub(super) observed_resource_epoch: Option<u64>,
    pub(super) generation: u64,
    /// Always recorded, NOT `#[cfg(test)]`. A complete rebuild of this index is
    /// the dominant cost of a save on a large graph, and "how many times did it
    /// rebuild?" is the first question any slow-save report raises. A counter
    /// that exists only in the test binary cannot answer that question on the
    /// machine that has the problem -- which is exactly how a recovery
    /// investigation burned a full diagnostic cycle on 2026-08-05/06.
    pub(super) complete_builds: usize,
    pub(super) exact_updates: usize,
    /// Cost of the most recent complete rebuild, split into its two phases.
    /// Durations and counts only -- never a path and never file content, so this
    /// is safe to surface from a user's own graph.
    pub(super) last_build: Option<GuardedGraphTextIdentityBuild>,
}

/// One complete rebuild of the guarded graph-text admission index, measured.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuardedGraphTextIdentityBuild {
    /// Two-pass whole-graph retained capture.
    pub capture: std::time::Duration,
    /// Admission-index construction, including the per-document parse when
    /// `decode_semantics` is set.
    pub index: std::time::Duration,
    pub decode_semantics: bool,
    pub captured_entries: usize,
    pub captured_bytes: u64,
}

/// Always-on report of what the guarded graph-text identity index has cost.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuardedGraphTextIdentityReport {
    pub complete_builds: usize,
    pub exact_updates: usize,
    pub invalidated: bool,
    pub generation: u64,
    pub last_build: Option<GuardedGraphTextIdentityBuild>,
}

pub(super) struct GraphTextParseBudgetPermit {
    pub(super) semantic_name_bytes: u64,
    pub(super) semantic_name_allocation_bytes: u64,
}

/// Core classification for one exact graph-relative platform event path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GraphTextExactFeedPathClass {
    /// The path is wholly within a fixed or configured excluded subtree.
    Excluded,
    /// The exact path may affect retained file/resource evidence.
    RetainedFile,
    /// `logseq/config.edn`: not graph text. The watcher's configuration queue
    /// decides how far a change reaches (`ConfigReach`); only a graph-reach
    /// change installs a fresh Graph instance.
    Configuration,
}

/// What a watched path can change in a graph's text inventory; see
/// `Graph::graph_text_watch_reach`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphTextWatchReach {
    /// Nothing this graph indexes lives at or under the path.
    Nothing,
    /// At most the one file at the path.
    File,
    /// Graph text may live under the path, so only a full diff is exact.
    Subtree,
}

#[derive(Clone, Debug)]
pub(super) struct GraphTextAdmissionRecord {
    pub(super) description: BlobDescription,
    pub(super) file_resource_id: ContentDigest,
    pub(super) link_count: u64,
    pub(super) semantic: PageEntry,
    pub(super) format: Format,
    /// Whether `semantic` came from parsing this file's bytes, rather than from
    /// the page cache or from the filename alone.
    ///
    /// A rebuild reuses a prior record's `semantic` when this is set and the
    /// prior `description` (a SHA-256 of the content, plus its length) equals
    /// the freshly captured one — the bytes are identical, so the parse result
    /// is too. Without the flag the reuse would launder a cache-derived guess
    /// into something later builds treat as parsed.
    pub(super) semantic_parsed: bool,
}

#[derive(Clone)]
pub(super) struct GraphTextAdmissionTombstone {
    pub(super) prior_record: Option<Arc<GraphTextAdmissionRecord>>,
    pub(super) prior_file_resource_id: ContentDigest,
    pub(super) prior_link_count: u64,
}

#[derive(Clone)]
pub(super) struct CompleteGraphTextAdmissionIndex {
    pub(super) instance: Arc<GraphTextAdmissionInstance>,
    pub(super) scope_binding: GraphTextScopeBinding,
    pub(super) graph_resource: CanonicalGraphResourceId,
    pub(super) generation: u64,
    pub(super) files_by_exact_path: PersistentMap<GraphTextPath, GraphTextAdmissionRecord>,
    pub(super) paths_by_portable_key:
        PersistentMap<PortablePathKey, std::collections::BTreeSet<GraphTextPath>>,
    pub(super) paths_by_file_resource:
        PersistentMap<ContentDigest, std::collections::BTreeSet<String>>,
    pub(super) file_resource_by_exact_relative: PersistentMap<String, ContentDigest>,
    pub(super) file_link_count_by_exact_relative: PersistentMap<String, u64>,
    pub(super) file_is_graph_text_by_exact_relative: PersistentMap<String, bool>,
    pub(super) paths_by_semantic_key:
        PersistentMap<(u8, String), std::collections::BTreeSet<GraphTextPath>>,
    pub(super) tombstones_by_exact_path: PersistentMap<GraphTextPath, GraphTextAdmissionTombstone>,
    pub(super) directories_by_exact_relative: PersistentMap<String, ContentDigest>,
    pub(super) permanent_bytes: u64,
    pub(super) permanent_limit: u64,
    pub(super) peak_limit: u64,
}

pub(super) struct PreparedGraphTextAdmissionUpsert {
    pub(super) relative: String,
    pub(super) description: BlobDescription,
    pub(super) file_resource_id: ContentDigest,
    pub(super) link_count: u64,
    pub(super) retained_growth: u64,
    pub(super) eligible: Option<(GraphTextPath, GraphTextAdmissionRecord)>,
}

pub(super) struct PreparedGraphTextAdmissionRemove {
    pub(super) relative: String,
    pub(super) retained_growth: u64,
}

pub(super) enum PreparedGraphTextAdmissionFinalState {
    Present(PreparedGraphTextAdmissionUpsert),
    Absent(PreparedGraphTextAdmissionRemove),
}

#[derive(Default)]
pub(super) struct GraphTextExactFeedBatchActualCharges {
    raw_bytes: u64,
    prepared_growth: u64,
}

impl GraphTextExactFeedBatchActualCharges {
    fn remaining_raw(&self) -> io::Result<u64> {
        MAX_GRAPH_TEXT_EXACT_FEED_BATCH_RAW_BYTES
            .checked_sub(self.raw_bytes)
            .ok_or_else(|| graph_text_capture_limit_error("exact feed batch aggregate raw bytes"))
    }

    pub(super) fn live_preparation_bytes(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
    ) -> io::Result<u64> {
        checked_add_bytes(index.permanent_bytes, batch_scratch)
            .and_then(|live| checked_add_bytes(live, self.prepared_growth))
    }

    pub(super) fn remaining_peak(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
    ) -> io::Result<u64> {
        index
            .peak_limit
            .checked_sub(self.live_preparation_bytes(index, batch_scratch)?)
            .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))
    }

    fn ensure_work_peak(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
        working_bytes: u64,
    ) -> io::Result<()> {
        ensure_graph_text_peak_limit(
            self.live_preparation_bytes(index, batch_scratch)?,
            working_bytes,
            index.peak_limit,
        )
    }

    pub(super) fn reserve_raw(
        &mut self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
        raw_bytes: u64,
    ) -> io::Result<()> {
        if raw_bytes > self.remaining_raw()? {
            return Err(graph_text_capture_limit_error(
                "exact feed batch aggregate raw bytes",
            ));
        }
        self.raw_bytes = checked_add_bytes(self.raw_bytes, raw_bytes)?;
        // `raw_bytes` is an aggregate admission cap, not live memory: each
        // touched file is read, parsed, and dropped before the next. Only this
        // file's buffer coexists with previously retained prepared records.
        self.ensure_work_peak(index, batch_scratch, raw_bytes)
    }

    pub(super) fn ensure_permanent_growth(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        growth: u64,
    ) -> io::Result<()> {
        let permanent = checked_add_bytes(index.permanent_bytes, self.prepared_growth)
            .and_then(|bytes| checked_add_bytes(bytes, growth))?;
        if permanent > index.permanent_limit {
            return Err(graph_text_capture_limit_error("permanent index memory"));
        }
        Ok(())
    }

    pub(super) fn retain_prepared_growth(
        &mut self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
        growth: u64,
    ) -> io::Result<()> {
        self.ensure_permanent_growth(index, growth)?;
        let next = checked_add_bytes(self.prepared_growth, growth)?;
        let prior = self.prepared_growth;
        self.prepared_growth = next;
        if let Err(error) = self.ensure_work_peak(index, batch_scratch, 0) {
            self.prepared_growth = prior;
            return Err(error);
        }
        Ok(())
    }
}
