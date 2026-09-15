//! The derived result cache: bounded entries with byte estimates and LRU
//! pruning, the Direct query request and attempt types, and FindEntryIndex.

use super::*;

pub(super) struct DerivedCache {
    pub(super) gen: u64,
    pub(super) today: i64,
    /// The parse configuration under which the reference results were built.
    /// A mismatch drops the whole cache.
    pub(super) config_digest: tine_storage::ContentDigest,
    // `Arc<Vec<RefGroup>>` so serving a memoized result (every dataRev re-render)
    // is a refcount bump, not a deep clone of every matched block (see derived_memo).
    pub(super) results: std::collections::HashMap<String, (DerivedEntry, usize)>,
    pub(super) lru: std::collections::VecDeque<String>,
    pub(super) bytes: usize,
}

/// One cached backlink, unlinked-reference, or block-referrer value.
#[derive(Clone)]
pub(super) struct DerivedEntry {
    pub(super) result: BoundedRefGroups,
}

impl DerivedEntry {
    pub(super) fn plain(result: BoundedRefGroups) -> DerivedEntry {
        DerivedEntry { result }
    }
}

/// Which of SPEC §5.9's states one dispatched query attempt reached.
///
/// Generic in what a READY statement produced, because §5.9's states are a
/// property of the projection and not of the row shape: `@block` pre-view
/// groups, `@page` rows and an explanation's probe counts all reach the
/// projection the same way and owe the same classification
/// (`Graph::dispatch_direct_query` is the one place that pays them).
///
/// **RET2.** Every arm that is not `Answered` used to hand the query to the
/// parsed-graph walk. There is no walk here any more: the arms say what the
/// projection did, and the dispatcher turns each into either ONE bounded repair
/// or a typed [`crate::query::QueryExecutionError`]. An attempt returns an
/// OWNED answer, so every snapshot handle it opened is already dropped by the
/// time the dispatcher can decide to repair (R3 §2B).
pub(super) type DirectQueryRequest = Option<(
    Arc<crate::direct_projection::DirectProjection>,
    crate::query_jobs::QueryJobEpoch,
)>;

pub(super) enum DirectAttempt<T> {
    /// The statement answered and its rows were hydrated.
    Answered(T),
    /// The projection is not ready at this cache generation. Whether that is
    /// worth retrying, worth repairing, or terminal is
    /// [`crate::direct_projection::ProjectionProgress`]'s answer, not this
    /// arm's: the attempt only knows it could not read.
    NotReady,
    /// Capacity admission refused this attempt (R3): other jobs hold every
    /// slot. Work IS progressing, so this is retryable and never repaired.
    Busy,
    /// A read was attempted and did not answer. Repairable exactly once, and
    /// the reason travels so a contradiction (`InvalidSnapshot`) and a refused
    /// statement (`ReadFailed`) stay distinguishable at the public boundary.
    FailedRead(crate::query::QueryUnavailableReason),
    /// Projection repair cannot answer this request: no projection is
    /// attached at all (`ProjectionUnavailable`), or the compiler could not
    /// lower this shape (`UnsupportedRelation`). The second is a
    /// FUTURE-relation arm and not a live route — `query::sql::lower_query` is
    /// total today — but it is the arm a non-total lowering must take, because
    /// the alternative shapes (a fabricated empty answer, or a switch back to
    /// the evaluator) are both forbidden. Statistics resource exhaustion also
    /// takes this arm: an oversized fold does not mean the index is broken.
    Unavailable(crate::query::QueryUnavailableReason),
    /// The projection cancelled the job — a rebuild drained it, or the graph
    /// closed. Nothing is wrong with the projection, so it is NEVER repaired
    /// and never retried: the caller asked for a read that no longer has a
    /// subject (R3 §2B).
    Cancelled,
}

/// The ONE translation from an attempted read into a dispatch state
/// (D-3, §5.9/M9).
///
/// A seam refusal and a projection that contradicts itself are both "the read
/// was attempted and did not answer", so both are repaired — but they stay
/// distinguishable at the public boundary, because "the index could not be
/// read" and "the index returned inconsistent results" are different things to
/// tell a user. `From<ResultReadError>` in `query::results` is the one place
/// the free-form payload is dropped (I-5).
pub(super) fn direct_attempt_from_read<T>(
    read: Result<T, crate::query::QueryExecutionError>,
) -> DirectAttempt<T> {
    use crate::query::QueryExecutionError as Error;
    match read {
        Ok(answer) => DirectAttempt::Answered(answer),
        Err(Error::Cancelled) => DirectAttempt::Cancelled,
        Err(Error::NotReady(_)) => DirectAttempt::NotReady,
        Err(Error::Unavailable(
            reason @ crate::query::QueryUnavailableReason::StatisticsResourceLimit,
        )) => DirectAttempt::Unavailable(reason),
        Err(Error::Unavailable(reason)) => DirectAttempt::FailedRead(reason),
    }
}

// Query results contain owned DTO subtrees and can be close to graph-sized. A
// graph-lifetime, key-unbounded memo turns ordinary navigation through many
// pages' Linked References into unbounded retained memory. Oversized results are
// returned to their caller but deliberately not retained here.
pub(super) const DERIVED_CACHE_MAX_ENTRIES: usize = 64;
const DERIVED_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
pub(super) const DERIVED_CACHE_MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn result_cache_key_estimated_bytes(key: &str) -> usize {
    // The HashMap owns one key and the LRU owns another. Account both copies.
    key.len().saturating_mul(2).saturating_add(128)
}

/// Conservative owned-memory estimate for a result payload. Tauri commands use
/// this before serialization as a second guard beside the row cap; derived
/// caches use the same accounting so transport and retention budgets cannot
/// drift apart.
pub fn ref_groups_estimated_bytes(groups: &[RefGroup]) -> usize {
    groups
        .iter()
        .map(|group| {
            group.page.len()
                + group
                    .blocks
                    .iter()
                    .map(block_dto_estimated_bytes)
                    .sum::<usize>()
                + group
                    .evidence
                    .iter()
                    .map(|evidence| {
                        evidence.block_id.len()
                            + evidence
                                .occurrences
                                .iter()
                                .map(|occurrence| {
                                    occurrence.matched_name.len()
                                        + occurrence.canonical.len()
                                        + occurrence.rule.len()
                                        + std::mem::size_of::<ReferenceOccurrence>()
                                })
                                .sum::<usize>()
                    })
                    .sum::<usize>()
                + std::mem::size_of::<RefGroup>()
        })
        .sum()
}

pub(super) fn bounded_ref_groups(computed: crate::query::BoundedGroups) -> BoundedRefGroups {
    BoundedRefGroups {
        matched_total: None,
        statistics: None,
        groups: Arc::new(computed.groups),
        total: computed.total,
        exceeded: computed.exceeded,
    }
}

pub(super) fn touch_lru(lru: &mut std::collections::VecDeque<String>, key: &str) {
    if let Some(pos) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(pos);
    }
    lru.push_back(key.to_owned());
}

pub(super) fn prune_result_cache<T>(
    results: &mut std::collections::HashMap<String, (T, usize)>,
    lru: &mut std::collections::VecDeque<String>,
    bytes: &mut usize,
) {
    while results.len() > DERIVED_CACHE_MAX_ENTRIES || *bytes > DERIVED_CACHE_MAX_BYTES {
        let Some(oldest) = lru.pop_front() else { break };
        if let Some((_, removed_bytes)) = results.remove(&oldest) {
            *bytes = bytes.saturating_sub(removed_bytes);
        }
    }
}

pub(super) struct FindEntryIndex {
    pub(super) entries: std::collections::HashMap<(PageKind, String), PageEntry>,
    pages_loaded: bool,
    journals_loaded: bool,
}

impl FindEntryIndex {
    pub(super) fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            pages_loaded: false,
            journals_loaded: false,
        }
    }

    pub(super) fn has_kind(&self, kind: PageKind) -> bool {
        match kind {
            PageKind::Journal => self.journals_loaded,
            PageKind::Page => self.pages_loaded,
        }
    }

    pub(super) fn mark_kind_loaded(&mut self, kind: PageKind) {
        match kind {
            PageKind::Journal => self.journals_loaded = true,
            PageKind::Page => self.pages_loaded = true,
        }
    }
}
