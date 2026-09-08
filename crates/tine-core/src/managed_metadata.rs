//! RET2: SPEC §7.1's `query_registry` — the PUBLIC property-registry snapshot
//! — read off the actor from the same captured snapshots a result query reads.
//!
//! Before this packet the public request was answered on the RuntimeActor by
//! `application_property_registry_snapshot_ready`, which called the merged
//! registry builder: with a pending local suffix that builder hydrated every
//! pending page document through `application_navigation_overlay_ready` and
//! rebuilt the graph's whole registry on the actor, and a build that FAILED was
//! not an error at all — `serve_application_property_registry` served the last
//! published table, or an empty one before the first build. That is the
//! frontend's query type information: stale or empty metadata silently changes
//! what a filter means, so either behaviour breaks the database-owned query
//! contract even though the rows themselves come from SQL.
//!
//! After this packet the request is the same two phases every public Managed
//! query takes. A short actor turn captures the immutable inputs — the accepted
//! projection's path, the query stamp, the parse config, the ACCEPTED registry
//! table (through its existing fallible cached acquisition) and, when the actor
//! holds a pending suffix, the overlay instance and the revision the read must
//! carry — and [`execute_managed_metadata`] runs on the CALLING thread after
//! `operation` is released.
//!
//! **D-14: nothing here is a second acquisition.** The slot, the two opens in
//! their coherence order, the mask derived from the OPENED pending state and
//! the patched registry are all
//! [`crate::managed_query::open_managed_read`] — the one the result executor
//! uses — reached through [`crate::managed_query::ManagedReadInput`]. This
//! module fabricates no IR, no property predicate and no result rows to get
//! there, and it duplicates none of the query execution loop: what it adds is
//! the ONE line a metadata read does instead of a statement, `registry.snapshot()`.
//!
//! **No fallback and no memo.** Every non-answering disposition is the result
//! route's own typed outcome, classified by
//! `sync_runtime::managed_execution_error`: readiness (`Busy`, an exhausted
//! stale re-capture), `Cancelled`, or `Unavailable(ReadFailed)`. A failed
//! metadata read publishes nothing, memoizes nothing and evicts nothing, so the
//! next healthy request answers from durable evidence rather than from the
//! failure.
//!
//! **Generation semantics are unchanged (G7).** The snapshot's generation is
//! the ACCEPTED table's: a pending suffix is not accepted evidence, the patch
//! preserves `base.generation()`, and only the actor's publish step advances
//! it on acceptance.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::ParseConfig;
use crate::managed_query::{
    open_managed_read, ManagedQueryCensus, ManagedQueryOutcome, ManagedQueryStamp,
    ManagedReadInput, PatchedRegistryCache, PendingOverlayCapture,
};
use crate::query::ir::RegistrySnapshot;
use crate::query::registry::Registry;
use crate::query_jobs::{Admission, QueryJobOwner};

#[cfg(test)]
thread_local! {
    static DRAIN_AFTER_CONSTRUCTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn drain_after_next_construction() {
    DRAIN_AFTER_CONSTRUCTION.with(|enabled| enabled.set(true));
}

/// The immutable inputs one actor turn captures for a public metadata read.
///
/// Deliberately the SNAPSHOT half and nothing else: a metadata read has no IR,
/// no view, no bounds, no execution day of its own and no row shape, so it
/// carries none. Everything here is also on [`crate::managed_query::ManagedQueryCapture`]
/// and is lent to the shared open through the same borrowed view.
pub(crate) struct ManagedMetadataCapture {
    pub(crate) job_epoch: crate::query_jobs::QueryJobEpoch,
    /// The accepted projection's SQLite file.
    pub(crate) path: PathBuf,
    /// The pending overlay to patch over, when the actor held a pending suffix.
    pub(crate) overlay: Option<PendingOverlayCapture>,
    pub(crate) stamp: ManagedQueryStamp,
    pub(crate) config: ParseConfig,
    /// The ACCEPTED table this read patches: the actor's cached, fallibly
    /// acquired registry at the captured frontier, never a fallback.
    pub(crate) registry: Arc<Registry>,
}

impl ManagedMetadataCapture {
    /// The snapshot half of this capture, as the shared open reads it.
    ///
    /// `props` is unconditionally true: a metadata read IS the property table,
    /// so a pending suffix must always be patched into it. That is the only
    /// field whose value differs from a result capture's, and it is a fact
    /// about the request, not a dummy predicate invented to reach the open.
    fn read_input(&self) -> ManagedReadInput<'_> {
        ManagedReadInput {
            job_epoch: self.job_epoch,
            path: &self.path,
            overlay: self.overlay.as_ref(),
            stamp: &self.stamp,
            config: &self.config,
            registry: &self.registry,
            props: true,
        }
    }
}

/// What one captured metadata execution produced.
///
/// The non-answering half is the RESULT route's `ManagedQueryOutcome`, not a
/// parallel vocabulary: `sync_runtime::managed_execution_error` already owns
/// how each disposition is classified for a public caller, and the pending
/// repair protocol already keys on `PendingFailed { instance }`. A second enum
/// would be a second classification of the same five states.
pub(crate) enum ManagedMetadataOutcome {
    /// The wire snapshot of the effective property table at the opened state.
    Answered(RegistrySnapshot),
    NotAnswered(ManagedQueryOutcome),
}

/// Execute one captured metadata read on the CALLING thread.
///
/// The contract is [`crate::managed_query::execute_managed_query`]'s, verbatim:
/// capacity BEFORE any transaction, the shared open (overlay first, accepted
/// second with the stamp validated inside its transaction, mask from the opened
/// pending state, registry patched under both), and every read transaction
/// ended before the slot releases its capacity — so a drain that observes a
/// free slot can never still be waiting on this read. It never holds the actor,
/// `operation` or any graph mutex, never spawns a thread, and never memoizes.
///
/// It hydrates NO page document and parses no source text. A read with nothing
/// pending is one stamp validation; a read with a pending suffix additionally
/// pays the overlay open, the mask lookups and — only on a patch-cache miss —
/// the affected-key rebuild, which an ordinary text edit leaves empty.
pub(crate) fn execute_managed_metadata(
    capture: &ManagedMetadataCapture,
    owner: &QueryJobOwner,
    census: &ManagedQueryCensus,
    patched: &PatchedRegistryCache,
    wait: Duration,
) -> ManagedMetadataOutcome {
    // Capacity BEFORE any transaction (plan §2B), at the epoch the actor turn
    // captured: a job admitted before a drain but opening after it is cancelled
    // by the shared open's registrations.
    let slot = match owner.acquire_at_within(capture.job_epoch, wait) {
        Admission::Slot(slot) => slot,
        Admission::Busy => return ManagedMetadataOutcome::NotAnswered(ManagedQueryOutcome::Busy),
        Admission::Cancelled => {
            return ManagedMetadataOutcome::NotAnswered(ManagedQueryOutcome::Cancelled)
        }
    };
    let outcome = match open_managed_read(&capture.read_input(), &slot, census, patched) {
        Ok(opened) => {
            // The wire shape is taken while the snapshots are still open and
            // the table is still the one they produced; `opened` is then
            // dropped, ending both read transactions BEFORE the slot below.
            let snapshot = opened.registry.snapshot();
            #[cfg(test)]
            if DRAIN_AFTER_CONSTRUCTION.with(|enabled| enabled.replace(false)) {
                owner.begin_drain();
            }
            drop(opened);
            ManagedMetadataOutcome::Answered(snapshot)
        }
        Err(outcome) => ManagedMetadataOutcome::NotAnswered(outcome),
    };
    // Construction can outlast the last SQLite statement. Observe lifecycle
    // cancellation after construction and transaction release, while the job
    // still owns capacity, just as result execution does.
    let outcome = if slot.is_cancelled() {
        ManagedMetadataOutcome::NotAnswered(ManagedQueryOutcome::Cancelled)
    } else {
        if matches!(&outcome, ManagedMetadataOutcome::Answered(_)) {
            census.note_metadata_read();
        }
        outcome
    };
    drop(slot);
    outcome
}
