//! One current actionable-state producer for receipt and absence-sweep work.
//!
//! Ordinary opens used to answer "what work is still owed?" by enumerating
//! every receipt filename ever published and by reconstructing every sweep
//! chain ever written. Both terms grow with completed history (D-5), and the
//! retention census recorded them as the two remaining lifetime terms on the
//! ordinary open path.
//!
//! This module owns the two pieces that let those enumerations go away:
//!
//! 1. [`ProjectionActionCursor`] — a durable write-ahead cursor whose coverage
//!    is *bound to the consumer's roots*. The receipt store reserves a
//!    sequenced mark before it publishes any intent or completion; the
//!    current-action producer records, inside its own roots object, the exact
//!    cursor incarnation and the sequence it has folded through. What remains
//!    at open is exactly the uncovered receipt work.
//! 2. [`CurrentActionRoots`] / [`RetentionClosure`] — the bounded interface a
//!    generation capture reads to learn which historical objects an unfinished
//!    action or an explicit pending Restore still pins. Historical
//!    restoreability alone pins nothing here; the cold index keeps that.
//!
//! # Why coverage is durable state, not an in-memory flag
//!
//! An earlier revision proved coverage by "the cursor directory enumerated
//! empty". That is not a proof: recreating a lost cursor directory produces
//! the same observation as a fully covered one, so a preserved receipt that
//! had never been folded was silently dropped. An in-memory `was_created`
//! flag does not fix it either — a second crash between recreating the
//! directory and finishing the repair loses the flag and reproduces the
//! failure exactly.
//!
//! The durable binding is therefore:
//!
//! * The cursor directory holds a **head** object carrying a random
//!   `incarnation` and the monotone `reserved` sequence. Losing or tearing the
//!   head mints a new incarnation, which no roots object can match.
//! * Each consumer's roots object carries [`CursorCoverage`]: the incarnation
//!   it trusted and the sequence it folded through. A mismatch is a named,
//!   counted repair.
//! * Every sequence in `(covered_through, reserved]` must still have its mark
//!   on disk. A lost individual mark is therefore a detected gap, not silence,
//!   and a torn mark fails its name/content binding.
//!
//! Each of those conditions routes to the instrumented rebuild from retained
//! receipts. None of them is a refusal (D-3, I-10), and none of them is a cap
//! on how much work may legitimately be outstanding (D-5): a large uncovered
//! window is streamed through indexed point reads with durable per-chunk
//! progress, so a crash mid-catch-up resumes rather than restarts.
//!
//! Existing primitives searched before writing this (D-14):
//! `receiver_absence_summary.rs`'s chained summary object and
//! `local_completion_index.rs`'s delta/compaction chain both already persist
//! derived receipt state through `ObjectStore::publish_coalesced_private_derived`
//! — this module reuses that exact publication path and their private-derived
//! `sweeps` namespace rather than introducing a second one. Neither of them
//! could serve as the cursor: both are *summaries of already-published*
//! evidence, and the gap this cursor closes is the window before a receipt has
//! been summarized at all. `projection_turn_journal.rs` and the managed-local
//! journal already carry a monotone per-turn sequence and a durable drained
//! prefix, which is the same shape as this cursor and would cost no extra
//! barrier at all; its append and drain producers live in
//! `operational_coordinator.rs` and the `sync_runtime.rs` drain region, both
//! outside this packet's write set, so the exact seam is recorded in the
//! receipt instead of half-built here. No new content store, tree, serializer
//! or temp-rename protocol is introduced.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::absence_sweep::{SweepActionKind, SweepMember};
use super::object_store::{
    ensure_reconstructible_directory_nofollow, is_temp_name, open_dir_nofollow,
    read_optional_regular, require_regular_entry,
};
use super::{
    BatchId, DocumentId, FrontierV2, ManagedPath, ObjectStore, PageId, ProjectionIntent,
    ProjectionIntentId, StoreError, WorkspaceId,
};

pub(crate) const ACTION_NAMESPACE: &str = "sweeps";
const CURSOR_NAMESPACE: &str = "current-action-cursor-v1";
const CURSOR_INTENT_SUFFIX: &str = ".intent";
const CURSOR_COMPLETION_SUFFIX: &str = ".completion";
const CURSOR_HEAD_PREFIX: &str = "cursor-head-";
const CURSOR_HEAD_SUFFIX: &str = ".head";
const CURSOR_HEAD_SCHEMA_VERSION: u32 = 1;
const CURSOR_MARK_BYTES: u64 = 32;
const MAX_CURSOR_HEAD_BYTES: u64 = 4 * 1024;
const SEQUENCE_DIGITS: usize = 20;

/// Uncovered marks one consumer resolves and durably records before it takes
/// the next batch.
///
/// This is a *soft scheduling budget*, not an occupancy cap: crossing it
/// installs the progress made so far and continues with the next chunk. No
/// amount of legitimate outstanding work can turn into a refusal or into a
/// lifetime reconstruction (D-5).
pub(crate) const CURSOR_RESUME_CHUNK_MARKS: usize = 256;

/// Stale head objects tolerated before one unlink pass reclaims them. Purely
/// a garbage-collection cadence; only the highest head is ever authority.
const MAX_RETAINED_HEADS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ProjectionActionKind {
    Intent,
    Completion,
}

impl ProjectionActionKind {
    const fn suffix(self) -> &'static str {
        match self {
            Self::Intent => CURSOR_INTENT_SUFFIX,
            Self::Completion => CURSOR_COMPLETION_SUFFIX,
        }
    }
}

/// A derived producer that must fold a receipt publication before its
/// discovery mark can be reclaimed.
///
/// Coverage is tracked per consumer because the two producers advance at
/// different rates: the receiver absence roots fold synchronously at the
/// receipt commit boundary, while the own-endpoint completion chain buffers
/// and flushes on a turn/deadline cadence. Reclaiming at the receiver's
/// watermark alone would delete the only discovery mark for a completion the
/// local chain had not yet made durable.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum CursorConsumer {
    ReceiverAbsenceRoots,
    /// A second registrant that exists only to exercise the
    /// minimum-over-consumers rule, and is never built in production.
    ///
    /// There is exactly one production consumer of these marks. The
    /// own-endpoint completion chain is deliberately not one: it does not
    /// discover work through this cursor at all. Its uncovered window is the
    /// undrained managed-local and projection-turn journal tail, which
    /// `sync_runtime::retained_local_completion_intents` reads directly from
    /// those journals — no receipt, no cursor — and whose replay re-stages the
    /// completion. Naming it here as if it were a reserved production consumer
    /// misdescribed that boundary, so the variant is test-only: the reclamation
    /// rule stays a real, exercised mechanism, and adding a genuine second
    /// consumer stays a registration rather than a redesign.
    #[cfg(test)]
    SecondConsumerForTest,
}

/// Exactly how far one consumer's durable roots have folded this cursor.
///
/// Stored *inside* the consumer's own roots object, so a roots object and the
/// coverage it claims are published by the same audited write and can never
/// disagree. `covered_through` is a contiguous watermark: every sequence at or
/// below it has been folded into those roots.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CursorCoverage {
    pub(crate) incarnation: Uuid,
    pub(crate) covered_through: u64,
}

/// One receipt publication the consumer's roots have not proven they cover.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UncoveredMark {
    pub(crate) sequence: u64,
    pub(crate) intent_id: ProjectionIntentId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorHeadObject {
    schema_version: u32,
    workspace_id: WorkspaceId,
    incarnation: Uuid,
    reserved: u64,
}

/// One directory read of the cursor: its authoritative head plus every mark.
#[derive(Clone, Debug)]
struct CursorSnapshot {
    incarnation: Uuid,
    reserved: u64,
    /// Sequence to the intent whose receipt publication reserved it.
    marks: BTreeMap<u64, ProjectionIntentId>,
}

/// Durable write-ahead record that a receipt publication is in flight or has
/// not yet been folded into the current-action roots.
///
/// Ordering is deliberately write-ahead. A mark whose receipt never landed is
/// resolved by one point read that finds nothing and is dropped; a receipt
/// that landed and was never summarized is found by exactly the same point
/// read. The reverse order (mark after publication) has a silent-loss window,
/// which is the failure this exists to remove.
pub(crate) struct ProjectionActionCursor {
    store: ObjectStore,
    directory: Dir,
    incarnation: Uuid,
    /// `false` when the head could not be made durable. The cursor still
    /// records marks, but it can prove nothing, so every consumer takes the
    /// named repair instead of trusting it.
    head_durable: bool,
    state: Mutex<CursorState>,
}

#[derive(Debug)]
struct CursorState {
    next_sequence: u64,
    /// Reserved sequences no *registered* consumer has yet proven durable.
    /// Seeded at open from the marks the directory still holds.
    pending: BTreeMap<u64, ProjectionIntentId>,
    /// Highest sequence handed out. Equals the durable head's `reserved`
    /// except inside one in-flight reservation, or after a crash that
    /// installed a mark but not its head.
    reserved: u64,
    /// Durable watermark each registered consumer last confirmed.
    ///
    /// Reclamation takes the MINIMUM over this map, which is what stops one
    /// consumer clearing the only discovery mark for another that has not
    /// caught up yet. A consumer that is not registered contributes no
    /// minimum, so an unwired consumer cannot silently pin every mark forever
    /// either — it just does not get cursor-based recovery.
    consumers: BTreeMap<CursorConsumer, u64>,
}

impl std::fmt::Debug for ProjectionActionCursor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectionActionCursor")
            .field("incarnation", &self.incarnation)
            .field("head_durable", &self.head_durable)
            .finish()
    }
}

impl ProjectionActionCursor {
    /// Open, or durably re-establish, the cursor for one archive.
    ///
    /// Re-establishing mints a *new* incarnation and publishes it before this
    /// call returns, so the very next consumer open — including one after a
    /// second crash that happens between here and the end of the repair —
    /// still sees an incarnation no roots object claims and repairs again.
    pub(crate) fn open(store: &ObjectStore) -> Result<Self, StoreError> {
        let root = store.private_derived_root_capability()?;
        ensure_reconstructible_directory_nofollow(&root, ACTION_NAMESPACE)?;
        let action = open_dir_nofollow(&root, ACTION_NAMESPACE)?;
        ensure_reconstructible_directory_nofollow(&action, CURSOR_NAMESPACE)?;
        let directory = open_dir_nofollow(&action, CURSOR_NAMESPACE)?;
        let store = store.duplicate_retained_capability()?;

        let observed = read_snapshot(&directory, store.workspace_id());
        let (incarnation, reserved, marks, head_durable) = match observed {
            Ok(snapshot) => (
                snapshot.incarnation,
                snapshot.reserved,
                snapshot.marks,
                true,
            ),
            Err(_) => {
                // No usable head: this is a fresh cursor or a damaged one, and
                // the two are deliberately indistinguishable. Mint a new
                // incarnation over whatever marks survive, and start the
                // sequence above them so a surviving mark can never be
                // confused with a future reservation.
                let incarnation = Uuid::new_v4();
                let surviving = read_mark_names(&directory).unwrap_or_default();
                let reserved = surviving.keys().next_back().copied().unwrap_or(0);
                let durable = publish_head(
                    &store,
                    &directory,
                    &CursorHeadObject {
                        schema_version: CURSOR_HEAD_SCHEMA_VERSION,
                        workspace_id: store.workspace_id(),
                        incarnation,
                        reserved,
                    },
                )
                .is_ok();
                // The surviving marks belong to a repudiated incarnation: no
                // roots object can claim them, so the repair that follows
                // covers them and this makes them reclaimable rather than
                // permanent residue.
                (incarnation, reserved, surviving, durable)
            }
        };

        Ok(Self {
            store,
            directory,
            incarnation,
            head_durable,
            state: Mutex::new(CursorState {
                next_sequence: reserved.saturating_add(1),
                pending: marks,
                reserved,
                consumers: BTreeMap::new(),
            }),
        })
    }

    pub(crate) fn incarnation(&self) -> Uuid {
        self.incarnation
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, CursorState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reserve the write-ahead mark for one receipt publication.
    ///
    /// The mark and the advanced head are staged into ONE coalesced
    /// publication, so a reservation costs the same barriers as the mark alone
    /// would. The mark is installed first: a crash between the two installs
    /// leaves a mark above `reserved`, which is folded anyway, while the
    /// reverse leaves a gap that the next open repairs. Both directions are
    /// safe; only the first is free.
    pub(crate) fn reserve(
        &self,
        intent_id: ProjectionIntentId,
        kind: ProjectionActionKind,
    ) -> Result<u64, StoreError> {
        let mut state = self.locked();
        let sequence = state.next_sequence;
        let head = CursorHeadObject {
            schema_version: CURSOR_HEAD_SCHEMA_VERSION,
            workspace_id: self.store.workspace_id(),
            incarnation: self.incarnation,
            reserved: sequence,
        };
        let head_bytes = encode_head(&head)?;
        let mark = mark_name(sequence, intent_id, kind);
        let head_name = head_name(sequence);
        self.store.publish_coalesced_private_derived(
            &self.directory,
            &[
                (
                    mark.as_str(),
                    intent_id.as_bytes().as_slice(),
                    CURSOR_MARK_BYTES,
                ),
                (
                    head_name.as_str(),
                    head_bytes.as_slice(),
                    MAX_CURSOR_HEAD_BYTES,
                ),
            ],
            "projection current-action cursor mark",
        )?;
        state.next_sequence = sequence.saturating_add(1);
        state.reserved = sequence;
        state.pending.insert(sequence, intent_id);
        Ok(sequence)
    }

    /// Repudiate this cursor's durable head.
    ///
    /// Used when a reservation could not be made durable while the receipt it
    /// guards is about to be published anyway. Truth is the receipt (D-3), so
    /// publication proceeds; what must not survive is a head that would let a
    /// later open *prove* coverage it does not have. Removing the head forces
    /// the next open to mint a new incarnation and repair.
    pub(crate) fn repudiate(&self) {
        self.locked().pending.clear();
        let Ok(entries) = self.directory.entries() else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(CURSOR_HEAD_PREFIX) && name.ends_with(CURSOR_HEAD_SUFFIX) {
                let _ = self.directory.remove_file(name);
            }
        }
    }

    /// Exactly the receipt publications one consumer at `coverage` must still
    /// resolve, ascending by reservation sequence.
    ///
    /// An `Err` never means "too much work". It means the cursor cannot prove
    /// bounded resumption for this consumer — no head, a foreign incarnation,
    /// a lost mark, or a torn one — and the caller must take the named repair.
    pub(crate) fn uncovered_for(
        &self,
        coverage: Option<CursorCoverage>,
    ) -> Result<Vec<UncoveredMark>, ProjectionActionCursorError> {
        if !self.head_durable {
            return Err(ProjectionActionCursorError(
                "cursor head is not durable".into(),
            ));
        }
        let snapshot = read_snapshot(&self.directory, self.store.workspace_id())?;
        if snapshot.incarnation != self.incarnation {
            return Err(ProjectionActionCursorError(
                "cursor incarnation changed under an open handle".into(),
            ));
        }
        let Some(coverage) = coverage else {
            return Err(ProjectionActionCursorError(
                "roots record no cursor coverage".into(),
            ));
        };
        if coverage.incarnation != snapshot.incarnation {
            return Err(ProjectionActionCursorError(
                "roots were written against a different cursor incarnation".into(),
            ));
        }
        // A crash can install a mark and lose its head, so the highest
        // sequence any durable evidence admits to is the greater of the two.
        let reserved = snapshot
            .marks
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .max(snapshot.reserved);
        if coverage.covered_through > reserved {
            return Err(ProjectionActionCursorError(
                "roots claim coverage beyond every durable reservation".into(),
            ));
        }
        // Every reservation above the roots' watermark must still be on disk.
        // A hole is a lost mark, which is exactly the silent-loss case the
        // whole mechanism exists to detect.
        let mut expected = coverage.covered_through.saturating_add(1);
        while expected <= reserved {
            if !snapshot.marks.contains_key(&expected) {
                return Err(ProjectionActionCursorError(format!(
                    "current-action cursor mark {expected} is missing"
                )));
            }
            expected = expected.saturating_add(1);
        }
        Ok(snapshot
            .marks
            .range(coverage.covered_through.saturating_add(1)..)
            .map(|(sequence, intent_id)| UncoveredMark {
                sequence: *sequence,
                intent_id: *intent_id,
            })
            .collect())
    }

    /// Declare that `consumer` will fold this cursor, so reclamation waits for
    /// it. Registering with no confirmed coverage pins every mark until the
    /// consumer's first [`Self::commit_coverage`].
    pub(crate) fn register(&self, consumer: CursorConsumer) {
        self.locked().consumers.entry(consumer).or_insert(0);
    }

    /// The coverage `consumer` may claim once it has folded every mark at or
    /// below `sequence`. Pure computation; nothing is written or forgotten
    /// until [`Self::commit_coverage`] confirms the roots were installed.
    pub(crate) fn coverage_through(&self, sequence: u64) -> CursorCoverage {
        CursorCoverage {
            incarnation: self.incarnation,
            covered_through: sequence,
        }
    }

    /// The coverage `consumer` may claim once one intent's receipt work is
    /// folded, expressed as the *contiguous* watermark that still holds.
    ///
    /// Folding out of order is legitimate — a consumer may cover a later
    /// receipt first — so the watermark stops below the lowest reservation
    /// this consumer has not covered yet. The later mark simply stays on disk
    /// and is re-resolved, which is idempotent.
    pub(crate) fn coverage_after(
        &self,
        consumer: CursorConsumer,
        current: Option<CursorCoverage>,
        intent_id: ProjectionIntentId,
    ) -> CursorCoverage {
        let state = self.locked();
        let from = current
            .filter(|coverage| coverage.incarnation == self.incarnation)
            .map_or(0, |coverage| coverage.covered_through);
        let _ = consumer;
        let lowest_uncovered = state
            .pending
            .range(from.saturating_add(1)..)
            .find(|(_, pending_intent)| **pending_intent != intent_id)
            .map(|(sequence, _)| *sequence);
        CursorCoverage {
            incarnation: self.incarnation,
            covered_through: lowest_uncovered.map_or_else(
                || state.effective_reserved(),
                |sequence| sequence.saturating_sub(1),
            ),
        }
    }

    /// Coverage of every reservation this cursor has observed, claimable only
    /// by a consumer that rebuilt from retained receipt truth (which covers
    /// strictly more than any mark could).
    pub(crate) fn coverage_of_everything_reserved(&self) -> CursorCoverage {
        CursorCoverage {
            incarnation: self.incarnation,
            covered_through: self.locked().effective_reserved(),
        }
    }

    /// Record that `coverage` is now durable in `consumer`'s roots, and
    /// reclaim only the marks that EVERY registered consumer has covered.
    ///
    /// Reclamation is deliberately barrier-free: a mark that survives a failed
    /// unlink costs one redundant point read at the next open, while a
    /// directory barrier here would charge every receipt for garbage
    /// collection. Correctness comes from each consumer's durable watermark,
    /// never from the absence of a file.
    pub(crate) fn commit_coverage(&self, consumer: CursorConsumer, coverage: CursorCoverage) {
        if coverage.incarnation != self.incarnation {
            return;
        }
        let mut state = self.locked();
        let slot = state.consumers.entry(consumer).or_insert(0);
        *slot = (*slot).max(coverage.covered_through);
        let Some(reclaimable) = state.consumers.values().copied().min() else {
            return;
        };
        let covered = state
            .pending
            .range(..=reclaimable)
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>();
        for sequence in &covered {
            state.pending.remove(sequence);
        }
        drop(state);
        let Ok((mark_names, head_names)) = read_mark_names_and_heads(&self.directory) else {
            return;
        };
        for sequence in covered {
            if let Some(name) = mark_names.get(&sequence) {
                let _ = self.directory.remove_file(name);
            }
        }
        if head_names.len() > MAX_RETAINED_HEADS {
            for name in head_names.iter().rev().skip(1) {
                let _ = self.directory.remove_file(name);
            }
        }
    }
}

impl CursorState {
    /// The highest sequence any durable evidence admits to, whether that is
    /// the published head or a mark whose head install lost a crash race.
    fn effective_reserved(&self) -> u64 {
        self.pending
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .max(self.reserved)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectionActionCursorError(pub(crate) String);

impl std::fmt::Display for ProjectionActionCursorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "projection action cursor: {}", self.0)
    }
}

fn encode_head(head: &CursorHeadObject) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(head)
        .map_err(|error| StoreError::UnsafeEntry(format!("cursor head encode: {error}")))
}

fn publish_head(
    store: &ObjectStore,
    directory: &Dir,
    head: &CursorHeadObject,
) -> Result<(), StoreError> {
    let bytes = encode_head(head)?;
    let name = head_name(head.reserved);
    store.publish_coalesced_private_derived(
        directory,
        &[(name.as_str(), bytes.as_slice(), MAX_CURSOR_HEAD_BYTES)],
        "projection current-action cursor head",
    )
}

/// One enumeration of the cursor directory: names only, plus the single head
/// body. Bounded by outstanding work, never by retained receipt history.
fn read_snapshot(
    directory: &Dir,
    workspace_id: WorkspaceId,
) -> Result<CursorSnapshot, ProjectionActionCursorError> {
    let (marks, heads) = read_mark_names_and_heads(directory)?;
    let Some(latest) = heads.last() else {
        return Err(ProjectionActionCursorError("no cursor head".into()));
    };
    let bytes = read_optional_regular(directory, latest, MAX_CURSOR_HEAD_BYTES, None)
        .map_err(|error| ProjectionActionCursorError(error.to_string()))?
        .ok_or_else(|| ProjectionActionCursorError("cursor head disappeared".into()))?;
    let head: CursorHeadObject = serde_json::from_slice(&bytes)
        .map_err(|error| ProjectionActionCursorError(error.to_string()))?;
    let reencoded = serde_json::to_vec(&head)
        .map_err(|error| ProjectionActionCursorError(error.to_string()))?;
    if head.schema_version != CURSOR_HEAD_SCHEMA_VERSION
        || head.workspace_id != workspace_id
        || head_name(head.reserved) != *latest
        || reencoded != bytes
    {
        return Err(ProjectionActionCursorError(
            "cursor head binding mismatch".into(),
        ));
    }
    let mut resolved = BTreeMap::new();
    for (sequence, name) in &marks {
        let (parsed_sequence, intent_id, _) = parse_mark_name(name)?;
        if parsed_sequence != *sequence {
            return Err(ProjectionActionCursorError(format!(
                "non-canonical cursor mark {name}"
            )));
        }
        let body =
            read_optional_regular(directory, name, CURSOR_MARK_BYTES, Some(CURSOR_MARK_BYTES))
                .map_err(|error| ProjectionActionCursorError(error.to_string()))?
                .ok_or_else(|| ProjectionActionCursorError("cursor mark disappeared".into()))?;
        if body != intent_id.as_bytes().as_slice() {
            return Err(ProjectionActionCursorError(
                "cursor mark content does not bind its name".into(),
            ));
        }
        // Two reservations for one receipt (intent then completion) are
        // distinct sequences; both resolve to the same intent.
        resolved.insert(*sequence, intent_id);
    }
    Ok(CursorSnapshot {
        incarnation: head.incarnation,
        reserved: head.reserved,
        marks: resolved,
    })
}

fn read_mark_names(
    directory: &Dir,
) -> Result<BTreeMap<u64, ProjectionIntentId>, ProjectionActionCursorError> {
    let (names, _) = read_mark_names_and_heads(directory)?;
    let mut marks = BTreeMap::new();
    for (sequence, name) in &names {
        let (_, intent_id, _) = parse_mark_name(name)?;
        marks.insert(*sequence, intent_id);
    }
    Ok(marks)
}

/// Names only. Heads are returned ascending by their reserved sequence, so the
/// last entry is the authoritative head.
fn read_mark_names_and_heads(
    directory: &Dir,
) -> Result<(BTreeMap<u64, String>, Vec<String>), ProjectionActionCursorError> {
    let mut marks = BTreeMap::new();
    let mut heads = BTreeMap::new();
    for entry in directory
        .entries()
        .map_err(|error| ProjectionActionCursorError(error.to_string()))?
    {
        let entry = entry.map_err(|error| ProjectionActionCursorError(error.to_string()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ProjectionActionCursorError("non-UTF-8 cursor entry".into()))?;
        if is_temp_name(&name) {
            continue;
        }
        require_regular_entry(
            &entry
                .file_type()
                .map_err(|error| ProjectionActionCursorError(error.to_string()))?,
            &name,
        )
        .map_err(|error| ProjectionActionCursorError(error.to_string()))?;
        if let Some(reserved) = parse_head_name(&name) {
            heads.insert(reserved, name);
            continue;
        }
        let (sequence, _, _) = parse_mark_name(&name)?;
        if marks.insert(sequence, name).is_some() {
            return Err(ProjectionActionCursorError(
                "cursor reservation sequence twin".into(),
            ));
        }
    }
    Ok((marks, heads.into_values().collect()))
}

fn head_name(reserved: u64) -> String {
    format!("{CURSOR_HEAD_PREFIX}{reserved:020}{CURSOR_HEAD_SUFFIX}")
}

fn parse_head_name(name: &str) -> Option<u64> {
    let digits = name
        .strip_prefix(CURSOR_HEAD_PREFIX)?
        .strip_suffix(CURSOR_HEAD_SUFFIX)?;
    if digits.len() != SEQUENCE_DIGITS || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let reserved = digits.parse::<u64>().ok()?;
    (head_name(reserved) == name).then_some(reserved)
}

fn mark_name(sequence: u64, intent_id: ProjectionIntentId, kind: ProjectionActionKind) -> String {
    format!(
        "{sequence:020}-{}{}",
        hex(intent_id.as_bytes()),
        kind.suffix()
    )
}

fn parse_mark_name(
    name: &str,
) -> Result<(u64, ProjectionIntentId, ProjectionActionKind), ProjectionActionCursorError> {
    let unknown = || ProjectionActionCursorError(format!("unknown cursor mark {name}"));
    let (body, kind) = name
        .strip_suffix(CURSOR_COMPLETION_SUFFIX)
        .map(|body| (body, ProjectionActionKind::Completion))
        .or_else(|| {
            name.strip_suffix(CURSOR_INTENT_SUFFIX)
                .map(|body| (body, ProjectionActionKind::Intent))
        })
        .ok_or_else(unknown)?;
    let (digits, digest) = body.split_once('-').ok_or_else(unknown)?;
    if digits.len() != SEQUENCE_DIGITS || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProjectionActionCursorError(format!(
            "non-canonical cursor mark {name}"
        )));
    }
    let sequence = digits
        .parse::<u64>()
        .map_err(|error| ProjectionActionCursorError(error.to_string()))?;
    if digest.len() != 64 {
        return Err(ProjectionActionCursorError(format!(
            "non-canonical cursor mark {name}"
        )));
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in digest.as_bytes().chunks_exact(2).enumerate() {
        let nibble = |byte: u8| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(ProjectionActionCursorError(format!(
                "non-canonical cursor mark {name}"
            ))),
        };
        bytes[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    let intent_id = ProjectionIntentId::from_marker_digest(bytes);
    if mark_name(sequence, intent_id, kind) != name {
        return Err(ProjectionActionCursorError(format!(
            "non-canonical cursor mark {name}"
        )));
    }
    Ok((sequence, intent_id, kind))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

/// One absence-sweep member an *active* obligation still pins.
///
/// Every field the Restore workflow consumes is carried verbatim: the exact
/// predecessor accepted state (page identity plus `FrontierV2`) and the
/// best-effort prior present intent. A capsule digest cannot replace either —
/// `plan_revive_page_operations` reads the predecessor frontier itself, and
/// activation-era members legitimately have no prior intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SweepRetentionPin {
    pub(crate) sweep_id: Uuid,
    pub(crate) page_id: PageId,
    pub(crate) path: ManagedPath,
    pub(crate) predecessor_page_id: PageId,
    pub(crate) predecessor_frontier: FrontierV2,
    pub(crate) prior_present_intent_id: Option<ProjectionIntentId>,
    pub(crate) deletion_batch_id: Option<BatchId>,
    pub(crate) pending_action: Option<SweepActionKind>,
}

impl SweepRetentionPin {
    pub(crate) fn from_member(
        sweep_id: Uuid,
        member: &SweepMember,
        pending_action: Option<SweepActionKind>,
    ) -> Self {
        Self {
            sweep_id,
            page_id: member.page_id,
            path: member.path.clone(),
            predecessor_page_id: member.predecessor_accepted_state.page_id,
            predecessor_frontier: member.predecessor_accepted_state.frontier.clone(),
            prior_present_intent_id: member.prior_present_intent_id,
            deletion_batch_id: member.deletion_batch_id,
            pending_action,
        }
    }
}

/// The whole of "what is still owed" on an ordinary open.
///
/// Unfinished receipt work plus unfinished sweeps and explicit pending
/// Restore. Completed chains are not here: they stay point-addressable
/// historical state, which is what keeps this value bounded by actionable
/// obligations rather than by retained history.
#[derive(Clone, Debug, Default)]
pub(crate) struct CurrentActionRoots {
    pub(crate) actionable_intents: Vec<ProjectionIntent>,
    pub(crate) sweep_pins: Vec<SweepRetentionPin>,
}

impl CurrentActionRoots {
    /// Exact transitive obligation set a generation capture must retain.
    ///
    /// Documents and dependency heads come from the pinned predecessor
    /// frontiers, so a later Restore finds every ancestor it needs. This is a
    /// read of already-derived roots, never a walk of retained history.
    pub(crate) fn retention_closure(&self) -> RetentionClosure {
        let mut closure = RetentionClosure::default();
        for intent in &self.actionable_intents {
            if let Ok(intent_id) = intent.id() {
                closure.intents.insert(intent_id);
            }
            closure.pages.insert(intent.page_id());
            closure.absorb_frontier(intent.frontier());
        }
        for pin in &self.sweep_pins {
            closure.sweeps.insert(pin.sweep_id);
            closure.pages.insert(pin.page_id);
            closure.pages.insert(pin.predecessor_page_id);
            closure.intents.extend(pin.prior_present_intent_id);
            closure.batches.extend(pin.deletion_batch_id);
            closure.absorb_frontier(&pin.predecessor_frontier);
        }
        closure
    }
}

/// The bounded retention closure handed to generation capture.
///
/// Membership here means "cold relocation must keep this logical object
/// reachable", not "keep this record active". Physical placement stays a
/// cold-pack responsibility; this names the closure, nothing else.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RetentionClosure {
    pub(crate) documents: BTreeSet<DocumentId>,
    pub(crate) batches: BTreeSet<BatchId>,
    pub(crate) pages: BTreeSet<PageId>,
    pub(crate) intents: BTreeSet<ProjectionIntentId>,
    pub(crate) sweeps: BTreeSet<Uuid>,
}

impl RetentionClosure {
    fn absorb_frontier(&mut self, frontier: &FrontierV2) {
        for document in frontier.documents() {
            self.documents.insert(document.document_id());
            self.batches
                .extend(document.direct_dependency_heads().iter().copied());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_mark_names_are_canonical_and_round_trip() {
        let intent_id = ProjectionIntentId::from_marker_digest([0xab; 32]);
        for kind in [
            ProjectionActionKind::Intent,
            ProjectionActionKind::Completion,
        ] {
            let name = mark_name(7, intent_id, kind);
            let (sequence, parsed, parsed_kind) = parse_mark_name(&name).unwrap();
            assert_eq!(sequence, 7);
            assert_eq!(parsed, intent_id);
            assert_eq!(parsed_kind, kind);
        }
        let digest = hex(&[0xab; 32]);
        for rejected in [
            "ab.intent",
            "AB.intent",
            &format!("{digest}.intent"),
            &format!("00000000000000000007-{digest}.other"),
            &format!("7-{digest}.intent"),
            &format!("00000000000000000007-{digest}.intent.extra"),
        ] {
            assert!(parse_mark_name(rejected).is_err(), "accepted {rejected}");
        }
    }

    /// Reclamation waits for the slowest registered consumer.
    ///
    /// A `release_kind` split by intent/completion proves only that one half of
    /// one receipt was folded; it says nothing about whether every consumer of
    /// that receipt has caught up. The minimum over registered watermarks is
    /// what actually holds a discovery mark until the last consumer is durable.
    #[test]
    fn a_mark_is_reclaimed_only_when_every_registered_consumer_has_covered_it() {
        let root = std::env::temp_dir().join(format!("tine-cursor-consumers-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0xc7_0001));
        let store = ObjectStore::open(&root, workspace_id).unwrap();
        let cursor = ProjectionActionCursor::open(&store).unwrap();
        cursor.register(CursorConsumer::ReceiverAbsenceRoots);
        cursor.register(CursorConsumer::SecondConsumerForTest);

        let first = ProjectionIntentId::from_marker_digest([0x11; 32]);
        let second = ProjectionIntentId::from_marker_digest([0x22; 32]);
        assert_eq!(
            cursor.reserve(first, ProjectionActionKind::Intent).unwrap(),
            1
        );
        assert_eq!(
            cursor
                .reserve(second, ProjectionActionKind::Intent)
                .unwrap(),
            2
        );

        // The fast consumer covers everything.
        cursor.commit_coverage(
            CursorConsumer::ReceiverAbsenceRoots,
            cursor.coverage_through(2),
        );
        let (marks, _) = read_mark_names_and_heads(&cursor.directory).unwrap();
        assert_eq!(
            marks.len(),
            2,
            "one consumer's coverage cannot clear the only discovery mark for another"
        );

        // The slow consumer catches up halfway, then fully.
        cursor.commit_coverage(
            CursorConsumer::SecondConsumerForTest,
            cursor.coverage_through(1),
        );
        let (marks, _) = read_mark_names_and_heads(&cursor.directory).unwrap();
        assert_eq!(marks.keys().copied().collect::<Vec<_>>(), vec![2]);

        cursor.commit_coverage(
            CursorConsumer::SecondConsumerForTest,
            cursor.coverage_through(2),
        );
        let (marks, _) = read_mark_names_and_heads(&cursor.directory).unwrap();
        assert!(marks.is_empty());

        drop(cursor);
        drop(store);
        crate::test_support::remove_dir_all(&root);
    }

    /// Own-endpoint completion discovery is journal-covered, not receipt-covered.
    ///
    /// This is the boundary the removed "reserved consumer" variant used to
    /// describe. The production recovery set for own completions is computed
    /// from the managed-local frames and the undrained projection turns; if it
    /// ever started reading receipts or this cursor, the single production
    /// consumer above would become wrong and marks would need pinning.
    #[test]
    fn own_completion_recovery_reads_journals_and_never_receipts_or_this_cursor() {
        let source = include_str!("../sync_runtime.rs");
        let body = source
            .split_once("fn retained_local_completion_intents(")
            .expect("the own-completion recovery set has one producer")
            .1
            .split_once("\n}\n")
            .expect("its body is brace-terminated")
            .0;
        assert!(
            body.contains("managed.frames"),
            "own-completion recovery must read the managed-local journal"
        );
        assert!(
            body.contains("turns.undrained_turns()"),
            "own-completion recovery must read the undrained projection turns"
        );
        for forbidden in [
            "receipts",
            "ProjectionReceiptStore",
            "cursor",
            "ProjectionActionCursor",
        ] {
            assert!(
                !body.contains(forbidden),
                "own-completion recovery must not depend on {forbidden}: it is \
journal-covered, which is why the completion chain is not a registered cursor \
consumer"
            );
        }
    }

    #[test]
    fn cursor_head_names_are_canonical_and_round_trip() {
        assert_eq!(parse_head_name(&head_name(0)), Some(0));
        assert_eq!(parse_head_name(&head_name(4097)), Some(4097));
        for rejected in ["cursor-head-7.head", "cursor-head-.head", "cursor-head-x"] {
            assert!(parse_head_name(rejected).is_none(), "accepted {rejected}");
        }
    }
}
