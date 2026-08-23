//! One finalized trusted-local edit through journal durability, exact graph
//! publication, and the committed hot overlay.
//!
//! This module deliberately stops at the foreground commit boundary. Archive
//! expansion and every derivative projection remain owned by later runtime
//! work.

use std::fmt;
use std::io;
use std::path::Path;

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::time::{Duration, Instant};

use crate::model::{
    content_rev, CommittedPendingJournalPageProjection, DurableJournalPageProjection, Format,
    JournalPageCommitError, JournalPageProjectionOutcome, JournalPageProjectionTarget,
};
use crate::{Graph, PageDto, PageKind};

use super::operational_coordinator::PreparedLocalMutation;
use super::{
    append_managed_local_record, BatchId, ManagedLocalAppendError, ManagedLocalAppendProof,
    ManagedLocalApplyOutcome, ManagedLocalJournalAppend, ManagedLocalRecord,
    ManagedLocalRecordError, ManagedTextKind, MaterializedPage, PreparedManagedLocalRecord,
    ShardedHotEngine,
};

pub(crate) struct TrustedLocalCommitCoordinator;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TrustedLocalCommitStageTimings {
    pub(crate) prepared_record: Duration,
    pub(crate) graph_total: Duration,
    pub(crate) graph_validation: Duration,
    pub(crate) journal_append: Duration,
    pub(crate) graph_publication: Duration,
    pub(crate) graph_cache_publication: Duration,
    pub(crate) hot_overlay_apply: Duration,
    pub(crate) direct_response: Duration,
}

#[cfg(test)]
thread_local! {
    static LAST_COMMIT_STAGE_TIMINGS: Cell<TrustedLocalCommitStageTimings> =
        Cell::new(TrustedLocalCommitStageTimings {
            prepared_record: Duration::ZERO,
            graph_total: Duration::ZERO,
            graph_validation: Duration::ZERO,
            journal_append: Duration::ZERO,
            graph_publication: Duration::ZERO,
            graph_cache_publication: Duration::ZERO,
            hot_overlay_apply: Duration::ZERO,
            direct_response: Duration::ZERO,
        });
}

#[cfg(test)]
fn reset_commit_stage_timings() {
    LAST_COMMIT_STAGE_TIMINGS.set(TrustedLocalCommitStageTimings::default());
}

#[cfg(test)]
pub(crate) fn last_commit_stage_timings() -> TrustedLocalCommitStageTimings {
    LAST_COMMIT_STAGE_TIMINGS.get()
}

#[cfg(test)]
fn note_commit_stage(update: impl FnOnce(&mut TrustedLocalCommitStageTimings)) {
    LAST_COMMIT_STAGE_TIMINGS.with(|timings| {
        let mut current = timings.get();
        update(&mut current);
        timings.set(current);
    });
}

#[cfg(test)]
pub(crate) fn note_trusted_local_graph_validation(elapsed: Duration) {
    note_commit_stage(|timings| timings.graph_validation += elapsed);
}

#[cfg(test)]
pub(crate) fn note_trusted_local_journal_append(elapsed: Duration) {
    note_commit_stage(|timings| timings.journal_append += elapsed);
}

#[cfg(test)]
pub(crate) fn note_trusted_local_graph_publication(elapsed: Duration) {
    note_commit_stage(|timings| timings.graph_publication += elapsed);
}

#[cfg(test)]
pub(crate) fn note_trusted_local_graph_cache_publication(elapsed: Duration) {
    note_commit_stage(|timings| timings.graph_cache_publication += elapsed);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLocalDeclineReason {
    ExistingPinnedPageRequired,
    EditableMarkdownOrOrgRequired,
    SlowPathOperation(String),
    PreparedPageMismatch(String),
}

impl fmt::Display for TrustedLocalDeclineReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExistingPinnedPageRequired => formatter.write_str(
                "the trusted-local path requires one existing page with an exact path and revision",
            ),
            Self::EditableMarkdownOrOrgRequired => formatter
                .write_str("the trusted-local path requires one editable Markdown or Org page"),
            Self::SlowPathOperation(reason) | Self::PreparedPageMismatch(reason) => {
                formatter.write_str(reason)
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum TrustedLocalCommitError {
    InvalidPreparedInput(String),
    ManagedRecord(ManagedLocalRecordError),
    PrecommitGraph(io::Error),
    JournalAppend(ManagedLocalAppendError),
}

impl fmt::Display for TrustedLocalCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPreparedInput(detail) => formatter.write_str(detail),
            Self::ManagedRecord(error) => error.fmt(formatter),
            Self::PrecommitGraph(error) => error.fmt(formatter),
            Self::JournalAppend(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TrustedLocalCommitError {}

pub(crate) enum TrustedLocalCommitOutcome {
    Declined { reason: TrustedLocalDeclineReason },
    Committed(TrustedLocalCommitted),
    CommittedPendingProjection(TrustedLocalCommittedPendingProjection),
    CommittedRecoveryRequired(TrustedLocalCommittedRecovery),
}

/// A journal-durable bounded multi-page mutation whose semantic hot overlay is
/// visible. Markdown projection, accepted history, SQLite and provider work are
/// deliberately left to the managed-local derivative queue.
pub(crate) struct TrustedLocalCompoundCommitted {
    prepared: PreparedManagedLocalRecord,
    post_pages: std::collections::BTreeMap<super::PageId, MaterializedPage>,
}

impl TrustedLocalCompoundCommitted {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.prepared.batch_id()
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.prepared.sequence()
    }

    pub(crate) const fn prepared_record(&self) -> &PreparedManagedLocalRecord {
        &self.prepared
    }

    pub(crate) const fn post_pages(
        &self,
    ) -> &std::collections::BTreeMap<super::PageId, MaterializedPage> {
        &self.post_pages
    }
}

pub(crate) enum TrustedLocalCompoundOutcome {
    Committed(TrustedLocalCompoundCommitted),
    CommittedRecoveryRequired {
        prepared: PreparedManagedLocalRecord,
        last_error: ManagedLocalRecordError,
    },
}

/// Parser-owned application response evidence, created at the same boundary
/// that parses the requested exact target.  It is intentionally crate-private
/// and may be retained only by an immediately durable trusted commit.
pub(crate) struct TrustedLocalResponseEvidence {
    page: PageDto,
    accepted_base_revision: String,
    parsed_target_revision: String,
}

impl TrustedLocalResponseEvidence {
    pub(crate) fn new(
        page: PageDto,
        accepted_base_revision: String,
        parsed_target_revision: String,
    ) -> Self {
        Self {
            page,
            accepted_base_revision,
            parsed_target_revision,
        }
    }

    /// One-use semantic receipt for the immediately adjacent Graph commit.
    /// Construction follows an exact target parse, identity resolution, and
    /// DTO conversion in the actor. Decoded/restarted records never have this
    /// process-local value and therefore retain Graph's complete parser and
    /// guarded-serialization validation.
    pub(crate) fn validates_projection(
        &self,
        page: &PageDto,
        base_revision: &str,
        target_revision: &str,
    ) -> bool {
        self.accepted_base_revision == base_revision
            && self.parsed_target_revision == target_revision
            && page_dto_equal_except_revision(&self.page, page)
    }
}

fn page_dto_equal_except_revision(left: &PageDto, right: &PageDto) -> bool {
    let PageDto {
        name: left_name,
        kind: left_kind,
        title: left_title,
        pre_block: left_pre_block,
        blocks: left_blocks,
        rev: _,
        format: left_format,
        read_only: left_read_only,
        path: left_path,
        activation: left_activation,
        guide: left_guide,
    } = left;
    let PageDto {
        name: right_name,
        kind: right_kind,
        title: right_title,
        pre_block: right_pre_block,
        blocks: right_blocks,
        rev: _,
        format: right_format,
        read_only: right_read_only,
        path: right_path,
        activation: right_activation,
        guide: right_guide,
    } = right;
    left_name == right_name
        && left_kind == right_kind
        && left_title == right_title
        && left_pre_block == right_pre_block
        && left_format == right_format
        && left_read_only == right_read_only
        && left_path == right_path
        && left_activation == right_activation
        && left_guide == right_guide
        && block_dtos_equal(left_blocks, right_blocks)
}

fn block_dtos_equal(left: &[crate::BlockDto], right: &[crate::BlockDto]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            let crate::BlockDto {
                id: left_id,
                raw: left_raw,
                collapsed: left_collapsed,
                children: left_children,
                breadcrumb: left_breadcrumb,
                page_property: left_page_property,
                marker: left_marker,
                priority: left_priority,
                heading_level: left_heading_level,
                scheduled: left_scheduled,
                deadline: left_deadline,
                tags: left_tags,
                properties: left_properties,
            } = left;
            let crate::BlockDto {
                id: right_id,
                raw: right_raw,
                collapsed: right_collapsed,
                children: right_children,
                breadcrumb: right_breadcrumb,
                page_property: right_page_property,
                marker: right_marker,
                priority: right_priority,
                heading_level: right_heading_level,
                scheduled: right_scheduled,
                deadline: right_deadline,
                tags: right_tags,
                properties: right_properties,
            } = right;
            left_id == right_id
                && left_raw == right_raw
                && left_collapsed == right_collapsed
                && left_breadcrumb == right_breadcrumb
                && left_page_property == right_page_property
                && left_marker == right_marker
                && left_priority == right_priority
                && left_heading_level == right_heading_level
                && left_scheduled == right_scheduled
                && left_deadline == right_deadline
                && left_tags == right_tags
                && left_properties == right_properties
                && block_dtos_equal(left_children, right_children)
        })
}

/// Journal-committed operation whose exact graph target is durable and whose
/// semantic overlay is already visible. Its fields are private so callers can
/// observe, but cannot construct, committed evidence.
pub(crate) struct TrustedLocalCommitted {
    prepared: PreparedManagedLocalRecord,
    graph: DurableJournalPageProjection<ManagedLocalAppendProof>,
    post_page: MaterializedPage,
    /// Parser-owned DTO for the exact target that this foreground commit made
    /// durable.  This stays inside the nonconstructible committed outcome: it
    /// is never persisted or used by retry/recovery, which must retain the
    /// established exact-byte parser fallback.
    trusted_target_page: Option<PageDto>,
}

impl TrustedLocalCommitted {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.prepared.batch_id()
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.prepared.sequence()
    }

    pub(crate) const fn record(&self) -> &ManagedLocalRecord {
        self.prepared.record()
    }

    pub(crate) fn append(&self) -> &ManagedLocalAppendProof {
        self.graph.append_proof()
    }

    pub(crate) fn relative_path(&self) -> &str {
        self.graph.target().relative_path()
    }

    pub(crate) fn exact_target(&self) -> &[u8] {
        self.graph.target().target()
    }

    pub(crate) fn revision(&self) -> &str {
        self.graph.target().revision()
    }

    /// The one-shot parser-owned response DTO carried only by an immediately
    /// durable foreground commit.  A projection/overlay retry deliberately
    /// has no process-local DTO and therefore takes the established parser
    /// fallback when it eventually becomes durable.
    pub(crate) fn trusted_target_page(&self) -> Option<&PageDto> {
        self.trusted_target_page.as_ref()
    }

    pub(crate) const fn post_page(&self) -> &MaterializedPage {
        &self.post_page
    }

    pub(crate) const fn prepared_record(&self) -> &PreparedManagedLocalRecord {
        &self.prepared
    }
}

/// A committed record already applied to hot state whose graph publication can
/// retry without accepting an append callback or a semantic transaction.
pub(crate) struct TrustedLocalCommittedPendingProjection {
    prepared: PreparedManagedLocalRecord,
    graph: CommittedPendingJournalPageProjection<ManagedLocalAppendProof>,
    post_page: MaterializedPage,
}

impl TrustedLocalCommittedPendingProjection {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.prepared.batch_id()
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.prepared.sequence()
    }

    pub(crate) fn append(&self) -> &ManagedLocalAppendProof {
        self.graph.append_proof()
    }

    pub(crate) fn relative_path(&self) -> &str {
        self.graph.relative_path()
    }

    pub(crate) fn exact_target(&self) -> &[u8] {
        self.graph.target()
    }

    pub(crate) fn last_error(&self) -> &io::Error {
        self.graph.last_error()
    }

    pub(crate) const fn post_page(&self) -> &MaterializedPage {
        &self.post_page
    }

    pub(crate) const fn prepared_record(&self) -> &PreparedManagedLocalRecord {
        &self.prepared
    }
}

enum CommittedGraphState {
    Durable(DurableJournalPageProjection<ManagedLocalAppendProof>),
    Pending(CommittedPendingJournalPageProjection<ManagedLocalAppendProof>),
}

/// The append crossed the commit boundary but hot application could not be
/// completed. The exact prepared record, append receipt, and graph state stay
/// together for a no-redraft/no-reappend recovery attempt.
pub(crate) struct TrustedLocalCommittedRecovery {
    prepared: PreparedManagedLocalRecord,
    graph: CommittedGraphState,
    last_error: ManagedLocalRecordError,
}

impl TrustedLocalCommittedRecovery {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.prepared.batch_id()
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.prepared.sequence()
    }

    pub(crate) fn append(&self) -> &ManagedLocalAppendProof {
        match &self.graph {
            CommittedGraphState::Durable(graph) => graph.append_proof(),
            CommittedGraphState::Pending(graph) => graph.append_proof(),
        }
    }

    pub(crate) const fn prepared_record(&self) -> &PreparedManagedLocalRecord {
        &self.prepared
    }

    pub(crate) const fn last_error(&self) -> &ManagedLocalRecordError {
        &self.last_error
    }

    pub(crate) fn projection_is_pending(&self) -> bool {
        matches!(self.graph, CommittedGraphState::Pending(_))
    }
}

impl TrustedLocalCommitCoordinator {
    /// Commit one already-finalized bounded multi-page mutation at the same
    /// journal-first boundary as an ordinary editor save. No graph projection,
    /// accepted-history expansion or SQLite work occurs in this call.
    pub(crate) fn commit_compound<J: ManagedLocalJournalAppend>(
        journal: &mut J,
        engine: &mut ShardedHotEngine,
        prepared: PreparedLocalMutation,
    ) -> Result<TrustedLocalCompoundOutcome, TrustedLocalCommitError> {
        #[cfg(test)]
        reset_commit_stage_timings();
        let batch = prepared.into_trusted_batch();
        let sequence = engine.managed_local_prefix_state().next_sequence;
        #[cfg(test)]
        let stage_started = Instant::now();
        let mut prepared = engine
            .prepare_managed_local_record(batch, sequence)
            .map_err(TrustedLocalCommitError::ManagedRecord)?;
        #[cfg(test)]
        note_commit_stage(|timings| timings.prepared_record = stage_started.elapsed());
        #[cfg(test)]
        let stage_started = Instant::now();
        let append = append_managed_local_record(journal, &prepared)
            .map_err(TrustedLocalCommitError::JournalAppend)?;
        #[cfg(test)]
        note_commit_stage(|timings| timings.journal_append = stage_started.elapsed());
        #[cfg(test)]
        let stage_started = Instant::now();
        let applied = engine.apply_appended_managed_local_record(&append, &mut prepared);
        #[cfg(test)]
        note_commit_stage(|timings| timings.hot_overlay_apply = stage_started.elapsed());
        match applied {
            Ok(ManagedLocalApplyOutcome::Applied { batch_id, pages }) => {
                debug_assert_eq!(batch_id, prepared.batch_id());
                Ok(TrustedLocalCompoundOutcome::Committed(
                    TrustedLocalCompoundCommitted {
                        prepared,
                        post_pages: pages,
                    },
                ))
            }
            Err(last_error) => Ok(TrustedLocalCompoundOutcome::CommittedRecoveryRequired {
                prepared,
                last_error,
            }),
        }
    }

    pub(crate) fn commit<J: ManagedLocalJournalAppend>(
        graph: &Graph,
        journal: &mut J,
        engine: &mut ShardedHotEngine,
        page: &PageDto,
        base_revision: &str,
        prepared: PreparedLocalMutation,
    ) -> Result<TrustedLocalCommitOutcome, TrustedLocalCommitError> {
        Self::commit_with_response_evidence(
            graph,
            journal,
            engine,
            page,
            None,
            base_revision,
            prepared,
        )
    }

    pub(crate) fn commit_with_response_evidence<J: ManagedLocalJournalAppend>(
        graph: &Graph,
        journal: &mut J,
        engine: &mut ShardedHotEngine,
        page: &PageDto,
        response_evidence: Option<TrustedLocalResponseEvidence>,
        base_revision: &str,
        prepared: PreparedLocalMutation,
    ) -> Result<TrustedLocalCommitOutcome, TrustedLocalCommitError> {
        if page.path.is_empty() || page.rev.is_none() || base_revision.is_empty() {
            return Ok(TrustedLocalCommitOutcome::Declined {
                reason: TrustedLocalDeclineReason::ExistingPinnedPageRequired,
            });
        }
        let extension = Path::new(&page.path)
            .extension()
            .and_then(|extension| extension.to_str());
        if page.guide
            || page.read_only
            || !matches!(extension, Some("md" | "markdown" | "org"))
            || Format::from_path(Path::new(&page.path)) != page.format
        {
            return Ok(TrustedLocalCommitOutcome::Declined {
                reason: TrustedLocalDeclineReason::EditableMarkdownOrOrgRequired,
            });
        }
        if page.rev.as_deref() != Some(base_revision) {
            return Err(TrustedLocalCommitError::InvalidPreparedInput(
                "prepared page revision differs from the requested exact base revision".into(),
            ));
        }

        #[cfg(test)]
        reset_commit_stage_timings();
        #[cfg(test)]
        let prepared_started = Instant::now();
        let batch = prepared.into_trusted_batch();
        let sequence = engine.managed_local_prefix_state().next_sequence;
        let prepared = match engine.prepare_managed_local_record(batch, sequence) {
            Ok(prepared) => prepared,
            Err(ManagedLocalRecordError::Unsupported(reason)) => {
                return Ok(TrustedLocalCommitOutcome::Declined {
                    reason: TrustedLocalDeclineReason::SlowPathOperation(reason),
                });
            }
            Err(error) => return Err(TrustedLocalCommitError::ManagedRecord(error)),
        };
        if let Some(reason) = prepared_page_mismatch(page, base_revision, &prepared) {
            return Ok(TrustedLocalCommitOutcome::Declined {
                reason: TrustedLocalDeclineReason::PreparedPageMismatch(reason),
            });
        }
        let projection = prepared.record().projection();
        let expected_base = projection.precondition_base().bytes();
        let exact_target = projection.intent().target().bytes().ok_or_else(|| {
            TrustedLocalCommitError::InvalidPreparedInput(
                "eligible prepared record unexpectedly has an absent target".into(),
            )
        })?;
        let response_evidence = response_evidence_for_exact_target(response_evidence, exact_target);
        if content_rev(std::str::from_utf8(expected_base).map_err(|_| {
            TrustedLocalCommitError::InvalidPreparedInput(
                "prepared existing-page base is not valid UTF-8".into(),
            )
        })?) != base_revision
        {
            return Err(TrustedLocalCommitError::InvalidPreparedInput(
                "prepared record base differs from the requested exact revision".into(),
            ));
        }

        #[cfg(test)]
        note_commit_stage(|timings| timings.prepared_record = prepared_started.elapsed());

        #[cfg(test)]
        let graph_started = Instant::now();
        let graph_outcome = match graph.commit_existing_page_with_journal_evidence(
            page,
            base_revision,
            expected_base,
            exact_target,
            response_evidence.as_ref(),
            || append_managed_local_record(journal, &prepared),
        ) {
            Ok(outcome) => outcome,
            Err(JournalPageCommitError::Precommit(error)) => {
                return Err(TrustedLocalCommitError::PrecommitGraph(error));
            }
            Err(JournalPageCommitError::Append(error)) => {
                return Err(TrustedLocalCommitError::JournalAppend(error));
            }
        };
        #[cfg(test)]
        note_commit_stage(|timings| timings.graph_total = graph_started.elapsed());
        Ok(finish_committed_graph(
            engine,
            prepared,
            graph_outcome,
            response_evidence,
        ))
    }

    /// Retry only graph publication for an already applied committed record.
    /// This boundary accepts neither a journal nor a transaction.
    pub(crate) fn retry_pending_projection(
        graph: &Graph,
        pending: TrustedLocalCommittedPendingProjection,
    ) -> TrustedLocalCommitOutcome {
        let TrustedLocalCommittedPendingProjection {
            prepared,
            graph: pending,
            post_page,
        } = pending;
        match graph.retry_committed_journal_page_projection(pending) {
            JournalPageProjectionOutcome::Durable(graph) => {
                TrustedLocalCommitOutcome::Committed(TrustedLocalCommitted {
                    prepared,
                    graph,
                    post_page,
                    trusted_target_page: None,
                })
            }
            JournalPageProjectionOutcome::CommittedPending(graph) => {
                TrustedLocalCommitOutcome::CommittedPendingProjection(
                    TrustedLocalCommittedPendingProjection {
                        prepared,
                        graph,
                        post_page,
                    },
                )
            }
        }
    }

    /// Retry only the atomic hot-overlay transition for a record that is
    /// already journal committed. No append or graph mutation is performed.
    pub(crate) fn retry_committed_recovery(
        engine: &mut ShardedHotEngine,
        recovery: TrustedLocalCommittedRecovery,
    ) -> TrustedLocalCommitOutcome {
        let TrustedLocalCommittedRecovery {
            prepared,
            graph,
            last_error: _,
        } = recovery;
        finish_committed_state(engine, prepared, graph, None)
    }

    pub(crate) fn restart_projection_input(
        record: &ManagedLocalRecord,
    ) -> Result<TrustedLocalRestartProjectionInput, TrustedLocalCommitError> {
        let projection = record.projection();
        let expected_base = projection.precondition_base().bytes();
        let exact_target = projection.intent().target().bytes().ok_or_else(|| {
            TrustedLocalCommitError::InvalidPreparedInput(
                "decoded managed-local record has an absent target".into(),
            )
        })?;
        let base = std::str::from_utf8(expected_base).map_err(|_| {
            TrustedLocalCommitError::InvalidPreparedInput(
                "decoded managed-local base is not valid UTF-8".into(),
            )
        })?;
        let target = std::str::from_utf8(exact_target).map_err(|_| {
            TrustedLocalCommitError::InvalidPreparedInput(
                "decoded managed-local target is not valid UTF-8".into(),
            )
        })?;
        Ok(TrustedLocalRestartProjectionInput {
            batch_id: record.prepared_batch().manifest().batch_id(),
            sequence: record.sequence(),
            relative_path: projection.intent().path().as_str().to_owned(),
            base_revision: content_rev(base),
            expected_base: expected_base.to_vec(),
            exact_target: exact_target.to_vec(),
            revision: content_rev(target),
        })
    }

    /// Rebind one authenticated decoded journal record to the graph after
    /// restart. The API is callback-free and cannot append or redraft.
    pub(crate) fn recover_projection_after_restart<A>(
        graph: &Graph,
        append_proof: A,
        input: TrustedLocalRestartProjectionInput,
    ) -> TrustedLocalRestartProjectionOutcome<A> {
        match graph.recover_committed_journal_page_projection(
            append_proof,
            &input.relative_path,
            &input.base_revision,
            &input.expected_base,
            &input.exact_target,
            &input.revision,
        ) {
            JournalPageProjectionOutcome::Durable(graph) => {
                TrustedLocalRestartProjectionOutcome::Durable(
                    TrustedLocalRestartProjectionDurable {
                        batch_id: input.batch_id,
                        sequence: input.sequence,
                        graph,
                    },
                )
            }
            JournalPageProjectionOutcome::CommittedPending(graph) => {
                TrustedLocalRestartProjectionOutcome::CommittedPending(
                    TrustedLocalRestartProjectionPending {
                        batch_id: input.batch_id,
                        sequence: input.sequence,
                        graph,
                    },
                )
            }
        }
    }

    #[cfg(test)]
    fn inject_before_overlay_failure(error: ManagedLocalRecordError) {
        BEFORE_OVERLAY_FAILURE.with(|failure| *failure.borrow_mut() = Some(error));
    }
}

fn prepared_page_mismatch(
    page: &PageDto,
    base_revision: &str,
    prepared: &PreparedManagedLocalRecord,
) -> Option<String> {
    let projection = prepared.record().projection();
    let post_page = prepared.post_page();
    let expected_kind = match page.kind {
        PageKind::Page => ManagedTextKind::Page,
        PageKind::Journal => ManagedTextKind::Journal,
    };
    if projection.intent().path().as_str() != page.path
        || projection.precondition_base().source_path().as_str() != page.path
        || post_page.path.as_str() != page.path
        || post_page.name.as_str() != page.name
        || post_page.kind != expected_kind
    {
        return Some(
            "prepared operation changes page title, path, kind, or exact projection ownership; the established slow path is required"
                .into(),
        );
    }
    if page.rev.as_deref() != Some(base_revision) {
        return Some("prepared page is not pinned to its exact loaded revision".into());
    }
    None
}

fn materialized_page_semantics_equal(left: &MaterializedPage, right: &MaterializedPage) -> bool {
    left.page_id == right.page_id
        && left.home_document_id == right.home_document_id
        && left.name == right.name
        && left.path == right.path
        && left.kind == right.kind
        && left.preamble == right.preamble
        && left.blocks == right.blocks
}

fn finish_committed_graph(
    engine: &mut ShardedHotEngine,
    prepared: PreparedManagedLocalRecord,
    graph: JournalPageProjectionOutcome<ManagedLocalAppendProof>,
    response_evidence: Option<TrustedLocalResponseEvidence>,
) -> TrustedLocalCommitOutcome {
    let (graph, trusted_target_page) = match graph {
        JournalPageProjectionOutcome::Durable(graph) => {
            // The evidence was accepted only when its parser-time revision
            // matched the prepared exact target.  Graph now proves that same
            // target durable; update only the response revision.
            let trusted_target_page = response_evidence
                .filter(|evidence| evidence.parsed_target_revision == graph.target().revision())
                .map(|evidence| {
                    let mut page = evidence.page;
                    page.rev = Some(graph.target().revision().to_owned());
                    page
                });
            (CommittedGraphState::Durable(graph), trusted_target_page)
        }
        JournalPageProjectionOutcome::CommittedPending(graph) => {
            (CommittedGraphState::Pending(graph), None)
        }
    };
    finish_committed_state(engine, prepared, graph, trusted_target_page)
}

fn response_evidence_for_exact_target(
    response_evidence: Option<TrustedLocalResponseEvidence>,
    exact_target: &[u8],
) -> Option<TrustedLocalResponseEvidence> {
    let response_evidence = response_evidence?;
    let target = std::str::from_utf8(exact_target).ok()?;
    (response_evidence.parsed_target_revision == content_rev(target)).then_some(response_evidence)
}

fn finish_committed_state(
    engine: &mut ShardedHotEngine,
    mut prepared: PreparedManagedLocalRecord,
    graph: CommittedGraphState,
    trusted_target_page: Option<PageDto>,
) -> TrustedLocalCommitOutcome {
    let append = match &graph {
        CommittedGraphState::Durable(graph) => graph.append_proof(),
        CommittedGraphState::Pending(graph) => graph.append_proof(),
    };
    #[cfg(test)]
    let overlay_started = Instant::now();
    let applied = before_overlay_hook()
        .and_then(|()| engine.apply_appended_managed_local_record(append, &mut prepared));
    #[cfg(test)]
    note_commit_stage(|timings| timings.hot_overlay_apply = overlay_started.elapsed());
    let post_page = match applied {
        Ok(ManagedLocalApplyOutcome::Applied { batch_id, pages }) => {
            debug_assert_eq!(batch_id, prepared.batch_id());
            let page = pages
                .into_values()
                .next()
                .expect("single-page trusted commit retains its post-page");
            debug_assert!(materialized_page_semantics_equal(
                &page,
                prepared.post_page()
            ));
            page
        }
        Err(last_error) => {
            return TrustedLocalCommitOutcome::CommittedRecoveryRequired(
                TrustedLocalCommittedRecovery {
                    prepared,
                    graph,
                    last_error,
                },
            );
        }
    };
    #[cfg(test)]
    let response_started = Instant::now();
    let outcome = match graph {
        CommittedGraphState::Durable(graph) => {
            TrustedLocalCommitOutcome::Committed(TrustedLocalCommitted {
                prepared,
                graph,
                post_page,
                trusted_target_page,
            })
        }
        CommittedGraphState::Pending(graph) => {
            TrustedLocalCommitOutcome::CommittedPendingProjection(
                TrustedLocalCommittedPendingProjection {
                    prepared,
                    graph,
                    post_page,
                },
            )
        }
    };
    #[cfg(test)]
    note_commit_stage(|timings| timings.direct_response = response_started.elapsed());
    outcome
}

#[cfg(test)]
thread_local! {
    static BEFORE_OVERLAY_FAILURE: std::cell::RefCell<Option<ManagedLocalRecordError>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn before_overlay_hook() -> Result<(), ManagedLocalRecordError> {
    BEFORE_OVERLAY_FAILURE.with(|failure| match failure.borrow_mut().take() {
        Some(error) => Err(error),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn before_overlay_hook() -> Result<(), ManagedLocalRecordError> {
    Ok(())
}

pub(crate) struct TrustedLocalRestartProjectionInput {
    batch_id: BatchId,
    sequence: u64,
    relative_path: String,
    base_revision: String,
    expected_base: Vec<u8>,
    exact_target: Vec<u8>,
    revision: String,
}

pub(crate) enum TrustedLocalRestartProjectionOutcome<A> {
    Durable(TrustedLocalRestartProjectionDurable<A>),
    CommittedPending(TrustedLocalRestartProjectionPending<A>),
}

pub(crate) struct TrustedLocalRestartProjectionDurable<A> {
    batch_id: BatchId,
    sequence: u64,
    graph: DurableJournalPageProjection<A>,
}

impl<A> TrustedLocalRestartProjectionDurable<A> {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.batch_id
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn append_proof(&self) -> &A {
        self.graph.append_proof()
    }

    pub(crate) fn target(&self) -> &JournalPageProjectionTarget {
        self.graph.target()
    }
}

pub(crate) struct TrustedLocalRestartProjectionPending<A> {
    batch_id: BatchId,
    sequence: u64,
    graph: CommittedPendingJournalPageProjection<A>,
}

impl<A> TrustedLocalRestartProjectionPending<A> {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.batch_id
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn append_proof(&self) -> &A {
        self.graph.append_proof()
    }

    pub(crate) fn relative_path(&self) -> &str {
        self.graph.relative_path()
    }

    pub(crate) fn exact_target(&self) -> &[u8] {
        self.graph.target()
    }

    pub(crate) fn last_error(&self) -> &io::Error {
        self.graph.last_error()
    }
}

// Pre-0.7 enrolled fast-commit corpus. The clean runtime does not call this
// coordinator; retain the production surface only until the compiler-guided
// retirement wave removes its remaining type references.
#[cfg(all(test, any()))]
mod tests {
    use std::fs;

    use uuid::Uuid;

    use super::*;
    use crate::fast_commit::{forbidden_commit_work, graph_wide_commit_work, GraphWideCommitWork};
    use crate::oplog::hot_engine::{
        force_generic_before_projection_affine_reuse_for_test,
        force_generic_pending_local_predecessor_replay_for_test,
        last_local_mutation_detail_timings, reset_local_mutation_detail_timings,
        LocalAuthorCapture,
    };
    use crate::oplog::hot_engine_integration_tests::hot_overlay_tests::OverlayFixture;
    use crate::oplog::local_active::LocalRuntimeAdmission;
    use crate::oplog::operational_coordinator::{
        OperationalCoordinator, PreparedLocalMutationState,
    };
    use crate::oplog::projection::{
        prepared_editor_projection_instrumentation,
        reset_prepared_editor_projection_instrumentation, PreparedEditorProjection,
    };
    use crate::oplog::{
        BlockLocation, DocumentId, LogicalPageName, LogseqIdentityMutation, LogseqIdentityTrigger,
        LogseqUuid, ManagedPath, OperationTransaction, PageId, PageRename, SemanticOperation,
    };

    fn prepared_edit(
        fixture: &mut OverlayFixture,
        seed: u128,
        generation: usize,
    ) -> PreparedLocalMutation {
        let author = fixture.local_author(seed);
        let transaction = fixture.content_edit(generation);
        match OperationalCoordinator::prepare_trusted_local_with_author(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &fixture.graph,
            &fixture.receipts,
            &mut fixture.engine,
            author,
            &transaction,
        )
        .unwrap()
        {
            PreparedLocalMutationState::Prepared(prepared) => prepared,
            PreparedLocalMutationState::ReconciliationRequired(reconciliation) => {
                panic!("unexpected reconciliation for {:?}", reconciliation.paths())
            }
        }
    }

    fn prepared_transaction(
        fixture: &mut OverlayFixture,
        seed: u128,
        transaction: &OperationTransaction,
    ) -> PreparedLocalMutation {
        let author = fixture.local_author(seed);
        match OperationalCoordinator::prepare_trusted_local_with_author(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &fixture.graph,
            &fixture.receipts,
            &mut fixture.engine,
            author,
            transaction,
        )
        .unwrap()
        {
            PreparedLocalMutationState::Prepared(prepared) => prepared,
            PreparedLocalMutationState::ReconciliationRequired(reconciliation) => {
                panic!("unexpected reconciliation for {:?}", reconciliation.paths())
            }
        }
    }

    fn edited_page(fixture: &OverlayFixture, generation: usize) -> (PageDto, String) {
        let mut page = fixture
            .graph
            .load_by_path(fixture.page_path.as_str())
            .unwrap()
            .unwrap();
        let base_revision = page.rev.clone().unwrap();
        page.blocks[0].raw = format!("managed revision {generation}");
        (page, base_revision)
    }

    fn commit_edit(
        fixture: &mut OverlayFixture,
        journal: &mut impl ManagedLocalJournalAppend,
        seed: u128,
        generation: usize,
    ) -> TrustedLocalCommitOutcome {
        let (page, base_revision) = edited_page(fixture, generation);
        let prepared = prepared_edit(fixture, seed, generation);
        TrustedLocalCommitCoordinator::commit(
            &fixture.graph,
            journal,
            &mut fixture.engine,
            &page,
            &base_revision,
            prepared,
        )
        .unwrap()
    }

    fn assert_transaction_declined(
        label: &str,
        extension: &str,
        seed: u128,
        build: impl FnOnce(&OverlayFixture) -> OperationTransaction,
    ) {
        let mut fixture = OverlayFixture::new(label, extension, 8);
        let (_, mut journal) = fixture.journal("decline");
        let graph_before = fs::read(fixture.graph_path.join(fixture.page_path.as_str())).unwrap();
        let overlay_before = fixture.engine.managed_local_prefix_state();
        let (page, base_revision) = edited_page(&fixture, 1);
        let transaction = build(&fixture);
        let prepared = prepared_transaction(&mut fixture, seed, &transaction);
        let outcome = TrustedLocalCommitCoordinator::commit(
            &fixture.graph,
            &mut journal,
            &mut fixture.engine,
            &page,
            &base_revision,
            prepared,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            TrustedLocalCommitOutcome::Declined { .. }
        ));
        assert_eq!(journal.stats().frames_appended, 0);
        assert_eq!(fixture.engine.managed_local_prefix_state(), overlay_before);
        assert_eq!(
            fs::read(fixture.graph_path.join(fixture.page_path.as_str())).unwrap(),
            graph_before
        );
    }

    fn assert_page_semantics(left: &MaterializedPage, right: &MaterializedPage) {
        assert_eq!(left.page_id, right.page_id);
        assert_eq!(left.home_document_id, right.home_document_id);
        assert_eq!(left.name, right.name);
        assert_eq!(left.path, right.path);
        assert_eq!(left.kind, right.kind);
        assert_eq!(left.preamble, right.preamble);
        assert_eq!(left.blocks, right.blocks);
    }

    #[test]
    fn markdown_org_and_nested_targets_commit_one_frame_and_match_the_old_pipeline() {
        for (label, path) in [
            ("markdown", "notes/deep/Nonstandard.markdown"),
            ("org", "areas/deep/Nonstandard.org"),
        ] {
            let mut fast =
                OverlayFixture::new_at_path(&format!("trusted-local-fast-{label}"), path, 8);
            let (_, mut journal) = fast.journal("semantic");
            let (page, base_revision) = edited_page(&fast, 1);
            let prepared = prepared_edit(&mut fast, 1_200_100, 1);
            let forbidden_before = forbidden_commit_work();
            let graph_work_before = graph_wide_commit_work();
            let outcome = TrustedLocalCommitCoordinator::commit(
                &fast.graph,
                &mut journal,
                &mut fast.engine,
                &page,
                &base_revision,
                prepared,
            )
            .unwrap();
            let TrustedLocalCommitOutcome::Committed(committed) = outcome else {
                panic!("eligible {label} edit did not commit");
            };
            assert_eq!(journal.stats().frames_appended, 1);
            assert_eq!(journal.stats().data_durability_syncs, 1);
            assert_eq!(committed.relative_path(), path);
            assert_eq!(
                fast.graph.read_projection_input(&fast.page_path).unwrap(),
                Some(committed.exact_target().to_vec())
            );
            assert_page_semantics(
                &fast
                    .engine
                    .materialize_current_page_at_path(&fast.page_path)
                    .unwrap()
                    .unwrap(),
                committed.post_page(),
            );
            assert!(forbidden_commit_work().since(forbidden_before).is_none());
            assert_eq!(
                graph_wide_commit_work().since(graph_work_before),
                Default::default()
            );

            let mut old =
                OverlayFixture::new_at_path(&format!("trusted-local-old-{label}"), path, 8);
            let old_prepared = old.finalize_edit(1_200_100, 1);
            old.accept_and_project(&old_prepared);
            let expected = old.engine.materialize_page(old.page_id).unwrap();
            assert_page_semantics(committed.post_page(), &expected);
            assert_eq!(
                committed.exact_target(),
                old.graph
                    .read_projection_input(&old.page_path)
                    .unwrap()
                    .unwrap()
            );
        }
    }

    #[test]
    fn mismatched_parser_evidence_commits_and_defers_to_exact_byte_response_fallback() {
        let mut fixture = OverlayFixture::new("trusted-local-response-evidence-mismatch", "md", 8);
        let (_, mut journal) = fixture.journal("mismatch");
        let (page, base_revision) = edited_page(&fixture, 1);
        let prepared = prepared_edit(&mut fixture, 1_200_110, 1);
        let evidence = TrustedLocalResponseEvidence::new(
            page.clone(),
            base_revision.clone(),
            "deliberately-not-the-prepared-target-revision".into(),
        );

        let outcome = TrustedLocalCommitCoordinator::commit_with_response_evidence(
            &fixture.graph,
            &mut journal,
            &mut fixture.engine,
            &page,
            Some(evidence),
            &base_revision,
            prepared,
        )
        .expect("digest mismatch must not reject the already valid commit");
        let TrustedLocalCommitOutcome::Committed(committed) = outcome else {
            panic!("valid commit with mismatched response evidence was not durable")
        };
        assert!(
            committed.trusted_target_page().is_none(),
            "mismatched parser evidence must use the exact-byte response fallback"
        );
        assert_eq!(journal.stats().frames_appended, 1);
        assert_eq!(
            fixture
                .graph
                .read_projection_input(&fixture.page_path)
                .unwrap(),
            Some(committed.exact_target().to_vec())
        );
    }

    #[test]
    fn consecutive_edits_compose_before_any_derivative_pipeline_runs() {
        let mut fixture = OverlayFixture::new("trusted-local-chain", "md", 32);
        let (_, mut journal) = fixture.journal("chain");
        let forbidden_before = forbidden_commit_work();
        let graph_before = graph_wide_commit_work();
        let managed_before = fixture.engine.managed_local_work();
        let (mut page, mut base_revision) = edited_page(&fixture, 1);
        for generation in 1..=12 {
            page.blocks[0].raw = format!("managed revision {generation}");
            let prepared = prepared_edit(
                &mut fixture,
                1_210_000 + generation as u128 * 10,
                generation,
            );
            let outcome = TrustedLocalCommitCoordinator::commit(
                &fixture.graph,
                &mut journal,
                &mut fixture.engine,
                &page,
                &base_revision,
                prepared,
            )
            .unwrap();
            let TrustedLocalCommitOutcome::Committed(committed) = outcome else {
                panic!("managed edit {generation} did not commit");
            };
            assert_eq!(committed.sequence(), generation as u64 - 1);
            assert_eq!(
                committed.post_page().blocks[0].content,
                format!("managed revision {generation}")
            );
            base_revision = committed.revision().to_owned();
            page.rev = Some(base_revision.clone());
        }
        assert_eq!(journal.stats().frames_appended, 12);
        assert_eq!(
            fixture.engine.managed_local_prefix_state().records_applied,
            12
        );
        assert_eq!(
            fixture
                .engine
                .materialize_current_page_at_path(&fixture.page_path)
                .unwrap()
                .unwrap()
                .blocks[0]
                .content,
            "managed revision 12"
        );
        let forbidden = forbidden_commit_work().since(forbidden_before);
        assert_eq!(forbidden.sqlite_drains, 0);
        assert_eq!(forbidden.application_page_loads, 0);
        assert_eq!(
            graph_wide_commit_work().since(graph_before),
            Default::default()
        );
        let managed = fixture.engine.managed_local_work().since(managed_before);
        assert_eq!(managed.accepted_base_documents_loaded, 0);
        assert_eq!(managed.retained_author_candidates_used, 12);
    }

    #[test]
    fn capture_sealed_pending_local_predecessor_matches_forced_generic_prepared_and_journal_bytes()
    {
        let mut fixture = OverlayFixture::new("capture-sealed-byte-equivalence", "md", 32);
        let (_, mut journal) = fixture.journal("sealed-byte-equivalence");
        let first = commit_edit(&mut fixture, &mut journal, 1_211_000, 1);
        assert!(matches!(first, TrustedLocalCommitOutcome::Committed(_)));
        assert_eq!(
            fixture.engine.managed_local_prefix_state().records_applied,
            1
        );

        reset_prepared_editor_projection_instrumentation();
        force_generic_pending_local_predecessor_replay_for_test(true);
        let generic = prepared_edit(&mut fixture, 1_211_100, 2)
            .prepared_batch()
            .clone();
        force_generic_pending_local_predecessor_replay_for_test(false);
        let generic_counters = prepared_editor_projection_instrumentation();
        assert_eq!(generic_counters.finalizer_predecessor_replay_render, 1);
        assert_eq!(
            generic_counters.capture_sealed_pending_local_predecessor_success,
            0
        );
        assert_eq!(
            generic_counters.finalizer_sealed_pending_local_predecessor_use,
            0
        );

        reset_prepared_editor_projection_instrumentation();
        let sealed = prepared_edit(&mut fixture, 1_211_100, 2)
            .prepared_batch()
            .clone();
        let sealed_counters = prepared_editor_projection_instrumentation();
        assert_eq!(sealed_counters.finalizer_predecessor_replay_render, 0);
        assert_eq!(
            sealed_counters.capture_sealed_pending_local_predecessor_success,
            1
        );
        assert_eq!(
            sealed_counters.finalizer_sealed_pending_local_predecessor_use,
            1
        );

        assert_eq!(
            generic.manifest().encode().unwrap(),
            sealed.manifest().encode().unwrap()
        );
        let object_bytes = |prepared: &crate::oplog::PreparedBatch| {
            prepared
                .objects()
                .iter()
                .map(|object| object.encode().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(object_bytes(&generic), object_bytes(&sealed));
        let sequence = fixture.engine.managed_local_prefix_state().next_sequence;
        let generic_record = fixture
            .engine
            .prepare_managed_local_record(generic, sequence)
            .unwrap();
        let sealed_record = fixture
            .engine
            .prepare_managed_local_record(sealed, sequence)
            .unwrap();
        assert_eq!(
            generic_record.journal_payload(),
            sealed_record.journal_payload(),
            "the capture-only seal cannot alter durable managed-local journal bytes"
        );
    }

    #[test]
    fn affine_before_projection_matches_forced_generic_same_state_same_author_bytes() {
        let mut fixture = OverlayFixture::new("affine-before-projection-differential", "md", 32);
        let (_, mut journal) = fixture.journal("affine-before-projection-differential");
        let first = commit_edit(&mut fixture, &mut journal, 1_211_150, 1);
        assert!(matches!(first, TrustedLocalCommitOutcome::Committed(_)));

        let prepare = |force_generic| {
            reset_local_mutation_detail_timings();
            let accepted_page = fixture.engine.materialize_page(fixture.page_id).unwrap();
            let mut requested_page = accepted_page.clone();
            requested_page.blocks[0].content = "managed revision 2".into();
            let exact_base = fixture
                .graph
                .read_projection_input(&fixture.page_path)
                .unwrap()
                .expect("the first pending-local commit owns the next exact editor base");
            let prepared_editor_projection =
                PreparedEditorProjection::prepare(requested_page, &accepted_page, exact_base)
                    .unwrap()
                    .bind_accepted_page(accepted_page);
            force_generic_before_projection_affine_reuse_for_test(force_generic);
            let drafted = fixture
                .engine
                .draft_author_transaction_with_prepared_editor_for_test(
                    fixture.local_author(1_211_160),
                    &fixture.content_edit(2),
                    prepared_editor_projection,
                );
            force_generic_before_projection_affine_reuse_for_test(false);
            let prepared = fixture
                .engine
                .finalize_author_transaction(
                    drafted.unwrap(),
                    &fixture.graph,
                    &fixture.receipts,
                    fixture.binding,
                )
                .unwrap();
            (prepared, last_local_mutation_detail_timings())
        };

        let (generic, generic_detail) = prepare(true);
        let (affine, affine_detail) = prepare(false);

        assert_eq!(generic_detail.before_projection_full_materializations, 1);
        assert_eq!(generic_detail.before_projection_affine_attempts, 0);
        assert_eq!(generic_detail.before_projection_affine_reuses, 0);
        assert_eq!(affine_detail.before_projection_full_materializations, 0);
        assert_eq!(affine_detail.before_projection_affine_attempts, 1);
        assert_eq!(affine_detail.before_projection_affine_reuses, 1);
        assert_eq!(affine_detail.before_projection_affine_fallbacks, 0);

        assert_eq!(
            generic.manifest().encode().unwrap(),
            affine.manifest().encode().unwrap()
        );
        let object_bytes = |prepared: &crate::oplog::PreparedBatch| {
            prepared
                .objects()
                .iter()
                .map(|object| object.encode().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(object_bytes(&generic), object_bytes(&affine));

        let generic_projection = crate::oplog::projection_manifest::validate_projection_object_set(
            generic.manifest(),
            generic.objects(),
        )
        .unwrap();
        let affine_projection = crate::oplog::projection_manifest::validate_projection_object_set(
            affine.manifest(),
            affine.objects(),
        )
        .unwrap();
        assert_eq!(generic_projection.intents(), affine_projection.intents());
        assert_eq!(generic_projection.intents().len(), 1);
        assert_eq!(
            generic_projection.intents()[0].post_frontier(),
            affine_projection.intents()[0].post_frontier()
        );
        assert_eq!(
            generic_projection.intents()[0].target().annotations(),
            affine_projection.intents()[0].target().annotations()
        );

        let sequence = fixture.engine.managed_local_prefix_state().next_sequence;
        let generic_record = fixture
            .engine
            .prepare_managed_local_record(generic, sequence)
            .unwrap();
        let affine_record = fixture
            .engine
            .prepare_managed_local_record(affine, sequence)
            .unwrap();
        assert_eq!(
            generic_record.journal_payload(),
            affine_record.journal_payload(),
            "the affine pre-page reuse cannot alter canonical managed-local journal bytes"
        );
    }

    #[test]
    fn affine_before_projection_refuses_multi_document_pre_snapshot() {
        let mut fixture =
            OverlayFixture::new("affine-before-projection-snapshot-negative", "md", 32);
        let (_, mut journal) = fixture.journal("affine-before-projection-snapshot-negative");
        assert!(matches!(
            commit_edit(&mut fixture, &mut journal, 1_211_170, 1),
            TrustedLocalCommitOutcome::Committed(_)
        ));

        let accepted_page = fixture.engine.materialize_page(fixture.page_id).unwrap();
        let mut requested_page = accepted_page.clone();
        requested_page.blocks[0].content = "managed revision 2".into();
        let exact_base = fixture
            .graph
            .read_projection_input(&fixture.page_path)
            .unwrap()
            .expect("the first pending-local commit owns the next exact editor base");
        let prepared_editor_projection =
            PreparedEditorProjection::prepare(requested_page, &accepted_page, exact_base)
                .unwrap()
                .bind_accepted_page(accepted_page);

        reset_local_mutation_detail_timings();
        let drafted = fixture
            .engine
            .draft_author_transaction_with_prepared_editor_for_test(
                fixture.local_author(1_211_180),
                &fixture.content_edit_with_noop_foreign_read(2),
                prepared_editor_projection,
            )
            .unwrap();
        let detail = last_local_mutation_detail_timings();
        assert_eq!(detail.before_projection_affine_attempts, 1);
        assert_eq!(detail.before_projection_affine_reuses, 0);
        assert_eq!(detail.before_projection_affine_fallbacks, 1);
        assert_eq!(detail.before_projection_full_materializations, 1);
        fixture
            .engine
            .finalize_author_transaction(
                drafted,
                &fixture.graph,
                &fixture.receipts,
                fixture.binding,
            )
            .expect("the generic path remains valid after rejecting the multi-document snapshot");
    }

    #[test]
    fn affine_before_projection_refuses_foreign_pending_local_frontier_before_append() {
        let mut fixture =
            OverlayFixture::new("affine-before-projection-frontier-negative", "md", 32);
        let (_, mut journal) = fixture.journal("affine-before-projection-frontier-negative");
        let first = commit_edit(&mut fixture, &mut journal, 1_211_190, 1);
        let TrustedLocalCommitOutcome::Committed(first) = first else {
            panic!("first pending managed-local record was not durable");
        };
        let graph_before = fs::read(fixture.graph_path.join(fixture.page_path.as_str())).unwrap();
        let frames_before = journal.stats().frames_appended;
        assert!(fixture
            .engine
            .rewrite_managed_local_projection_frontier_with_foreign_document_for_test(
                first.sequence(),
                first.batch_id(),
            ));

        let accepted_page = fixture.engine.materialize_page(fixture.page_id).unwrap();
        let mut requested_page = accepted_page.clone();
        requested_page.blocks[0].content = "managed revision 2".into();
        let exact_base = fixture
            .graph
            .read_projection_input(&fixture.page_path)
            .unwrap()
            .expect("the first pending-local commit owns the next exact editor base");
        let prepared_editor_projection =
            PreparedEditorProjection::prepare(requested_page, &accepted_page, exact_base)
                .unwrap()
                .bind_accepted_page(accepted_page);

        reset_local_mutation_detail_timings();
        fixture
            .engine
            .draft_author_transaction_with_prepared_editor_for_test(
                fixture.local_author(1_211_200),
                &fixture.content_edit(2),
                prepared_editor_projection,
            )
            .expect("the generic draft remains available after frontier rejection");
        let detail = last_local_mutation_detail_timings();
        assert_eq!(detail.before_projection_affine_attempts, 1);
        assert_eq!(detail.before_projection_affine_reuses, 0);
        assert_eq!(detail.before_projection_affine_fallbacks, 1);
        assert_eq!(detail.before_projection_full_materializations, 1);
        assert_eq!(journal.stats().frames_appended, frames_before);
        assert_eq!(
            fs::read(fixture.graph_path.join(fixture.page_path.as_str())).unwrap(),
            graph_before,
            "frontier rejection must happen before a new managed-local append or graph overwrite"
        );
    }

    #[test]
    fn capture_sealed_pending_local_predecessor_refuses_changed_overlay_endpoint_before_append() {
        let mut fixture = OverlayFixture::new("capture-sealed-overlay-removal", "md", 32);
        let (_, mut journal) = fixture.journal("capture-sealed-overlay-removal");
        let first = commit_edit(&mut fixture, &mut journal, 1_211_200, 1);
        let TrustedLocalCommitOutcome::Committed(first) = first else {
            panic!("first pending managed-local record was not durable");
        };
        let graph_before = fs::read(fixture.graph_path.join(fixture.page_path.as_str())).unwrap();
        let frames_before = journal.stats().frames_appended;
        let draft = fixture
            .engine
            .draft_author_transaction(
                fixture.local_author(1_211_300),
                crate::oplog::BatchOrigin::LocalMutation,
                &fixture.content_edit(2),
            )
            .unwrap();

        reset_prepared_editor_projection_instrumentation();
        let captured = match fixture
            .engine
            .capture_local_author_transaction(
                draft,
                &fixture.graph,
                &fixture.receipts,
                fixture.binding,
                None,
            )
            .unwrap()
        {
            LocalAuthorCapture::Captured(captured) => captured,
            LocalAuthorCapture::ReconciliationNeeded(_) => {
                panic!("fresh pending managed-local predecessor unexpectedly reconciled")
            }
        };
        assert!(fixture
            .engine
            .rewrite_managed_local_authority_endpoint_for_test(first.sequence(), first.batch_id()));

        assert!(matches!(
            fixture
                .engine
                .finalize_captured_author_transaction(captured, &fixture.receipts),
            Err(crate::oplog::EngineError::ProjectionManifest(message))
                if message.contains("authority") || message.contains("binding")
        ));
        let counters = prepared_editor_projection_instrumentation();
        assert_eq!(counters.capture_sealed_pending_local_predecessor_success, 1);
        assert_eq!(counters.finalizer_predecessor_replay_render, 0);
        assert_eq!(counters.finalizer_sealed_pending_local_predecessor_use, 0);
        assert_eq!(journal.stats().frames_appended, frames_before);
        assert_eq!(
            fs::read(fixture.graph_path.join(fixture.page_path.as_str())).unwrap(),
            graph_before,
            "a failed sealed finalization must not append or overwrite the graph"
        );
    }

    #[test]
    fn stale_external_race_and_append_failure_leave_no_committed_operation_effect() {
        let mut stale = OverlayFixture::new("trusted-local-stale", "md", 8);
        let (_, mut stale_journal) = stale.journal("stale");
        let before = stale
            .graph
            .read_projection_input(&stale.page_path)
            .unwrap()
            .unwrap();
        let (mut page, _) = edited_page(&stale, 1);
        page.rev = Some("stale-revision".into());
        let prepared = prepared_edit(&mut stale, 1_220_010, 1);
        let result = TrustedLocalCommitCoordinator::commit(
            &stale.graph,
            &mut stale_journal,
            &mut stale.engine,
            &page,
            "stale-revision",
            prepared,
        );
        assert!(matches!(
            result,
            Err(TrustedLocalCommitError::InvalidPreparedInput(_))
        ));
        assert_eq!(stale_journal.stats().frames_appended, 0);
        assert_eq!(
            stale.graph.read_projection_input(&stale.page_path).unwrap(),
            Some(before.clone())
        );
        assert_eq!(stale.engine.managed_local_prefix_state().records_applied, 0);

        let mut raced = OverlayFixture::new("trusted-local-race", "org", 8);
        let (_, mut raced_journal) = raced.journal("race");
        let (page, base_revision) = edited_page(&raced, 1);
        let prepared = prepared_edit(&mut raced, 1_220_020, 1);
        let external = b"* external winner\n".to_vec();
        fs::write(raced.graph_path.join(raced.page_path.as_str()), &external).unwrap();
        let result = TrustedLocalCommitCoordinator::commit(
            &raced.graph,
            &mut raced_journal,
            &mut raced.engine,
            &page,
            &base_revision,
            prepared,
        );
        assert!(matches!(
            result,
            Err(TrustedLocalCommitError::PrecommitGraph(_))
        ));
        assert_eq!(raced_journal.stats().frames_appended, 0);
        assert_eq!(
            fs::read(raced.graph_path.join(raced.page_path.as_str())).unwrap(),
            external
        );
        assert_eq!(raced.engine.managed_local_prefix_state().records_applied, 0);

        let mut append = OverlayFixture::new("trusted-local-append", "md", 8);
        let wrong_device = Uuid::from_u128(0xdead_beef);
        let (_, mut wrong_journal) = append.journal_for_device("wrong-device", wrong_device);
        let append_before = append
            .graph
            .read_projection_input(&append.page_path)
            .unwrap()
            .unwrap();
        let (page, base_revision) = edited_page(&append, 1);
        let prepared = prepared_edit(&mut append, 1_220_030, 1);
        let result = TrustedLocalCommitCoordinator::commit(
            &append.graph,
            &mut wrong_journal,
            &mut append.engine,
            &page,
            &base_revision,
            prepared,
        );
        assert!(matches!(
            result,
            Err(TrustedLocalCommitError::JournalAppend(
                ManagedLocalAppendError::DefinitelyNotAppended(
                    ManagedLocalRecordError::WrongDurabilityProof
                )
            ))
        ));
        assert_eq!(wrong_journal.stats().frames_appended, 0);
        assert_eq!(
            append
                .graph
                .read_projection_input(&append.page_path)
                .unwrap(),
            Some(append_before)
        );
        assert_eq!(
            append.engine.managed_local_prefix_state().records_applied,
            0
        );
        let (_, mut right_journal) = append.journal("right-device");
        let later_prepared = prepared_edit(&mut append, 1_220_040, 1);
        assert!(matches!(
            TrustedLocalCommitCoordinator::commit(
                &append.graph,
                &mut right_journal,
                &mut append.engine,
                &page,
                &base_revision,
                later_prepared,
            ),
            Ok(TrustedLocalCommitOutcome::Committed(_))
        ));
        assert_eq!(right_journal.stats().frames_appended, 1);
        assert_eq!(
            append.engine.managed_local_prefix_state().records_applied,
            1
        );
    }

    #[test]
    fn committed_pending_retries_and_restart_rebinds_without_another_frame() {
        let mut fixture = OverlayFixture::new("trusted-local-pending", "md", 8);
        let (_, mut journal) = fixture.journal("pending");
        let base = fixture
            .graph
            .read_projection_input(&fixture.page_path)
            .unwrap()
            .unwrap();
        let (page, base_revision) = edited_page(&fixture, 1);
        let prepared = prepared_edit(&mut fixture, 1_230_010, 1);
        crate::model::inject_journal_projection_before_publish_failure(io::Error::new(
            io::ErrorKind::Interrupted,
            "injected append-before-publish cut",
        ));
        let outcome = TrustedLocalCommitCoordinator::commit(
            &fixture.graph,
            &mut journal,
            &mut fixture.engine,
            &page,
            &base_revision,
            prepared,
        )
        .unwrap();
        let TrustedLocalCommitOutcome::CommittedPendingProjection(pending) = outcome else {
            panic!("post-append graph cut was not retained as committed-pending");
        };
        assert_eq!(journal.stats().frames_appended, 1);
        assert_eq!(
            fixture
                .graph
                .read_projection_input(&fixture.page_path)
                .unwrap(),
            Some(base)
        );
        assert_eq!(
            fixture.engine.managed_local_prefix_state().records_applied,
            1
        );

        let restart_input = TrustedLocalCommitCoordinator::restart_projection_input(
            pending.prepared_record().record(),
        )
        .unwrap();
        let recovered = TrustedLocalCommitCoordinator::recover_projection_after_restart(
            &fixture.graph,
            *pending.append(),
            restart_input,
        );
        let TrustedLocalRestartProjectionOutcome::Durable(recovered) = recovered else {
            panic!("callback-free restart recovery did not reach the exact target");
        };
        assert_eq!(recovered.sequence(), 0);
        assert_eq!(journal.stats().frames_appended, 1);

        let outcome =
            TrustedLocalCommitCoordinator::retry_pending_projection(&fixture.graph, pending);
        let TrustedLocalCommitOutcome::Committed(committed) = outcome else {
            panic!("in-process retry did not reprove the exact recovered target");
        };
        assert!(
            committed.trusted_target_page().is_none(),
            "recovered projection must retain the exact-byte parser fallback"
        );
        assert_eq!(journal.stats().frames_appended, 1);
        assert_eq!(
            fixture
                .graph
                .read_projection_input(&fixture.page_path)
                .unwrap(),
            Some(committed.exact_target().to_vec())
        );
    }

    #[test]
    fn committed_overlay_failure_replays_once_into_fresh_hot_state() {
        let mut committed = OverlayFixture::new("trusted-local-recovery-source", "org", 8);
        let (_, mut journal) = committed.journal("recovery");
        let (page, base_revision) = edited_page(&committed, 1);
        let prepared = prepared_edit(&mut committed, 1_240_010, 1);
        TrustedLocalCommitCoordinator::inject_before_overlay_failure(
            ManagedLocalRecordError::StaleBase,
        );
        let outcome = TrustedLocalCommitCoordinator::commit(
            &committed.graph,
            &mut journal,
            &mut committed.engine,
            &page,
            &base_revision,
            prepared,
        )
        .unwrap();
        let TrustedLocalCommitOutcome::CommittedRecoveryRequired(recovery) = outcome else {
            panic!("post-append overlay cut was not retained for recovery");
        };
        assert_eq!(journal.stats().frames_appended, 1);
        assert_eq!(
            committed
                .engine
                .managed_local_prefix_state()
                .records_applied,
            0
        );
        assert!(!recovery.projection_is_pending());

        let mut fresh = OverlayFixture::new("trusted-local-recovery-fresh", "org", 8);
        let outcome =
            TrustedLocalCommitCoordinator::retry_committed_recovery(&mut fresh.engine, recovery);
        let TrustedLocalCommitOutcome::Committed(recovered) = outcome else {
            panic!("fresh hot state did not apply the committed record");
        };
        assert_eq!(recovered.sequence(), 0);
        assert_eq!(fresh.engine.managed_local_prefix_state().records_applied, 1);
        assert_page_semantics(
            recovered.post_page(),
            &fresh
                .engine
                .materialize_current_page_at_path(&fresh.page_path)
                .unwrap()
                .unwrap(),
        );
        assert_eq!(journal.stats().frames_appended, 1);
    }

    #[test]
    fn non_content_operation_classes_decline_before_append() {
        assert_transaction_declined("trusted-local-decline-path", "md", 1_250_010, |fixture| {
            OperationTransaction::new(vec![SemanticOperation::EditPagePath {
                page_id: fixture.page_id,
                path: ManagedPath::parse("pages/Renamed.md").unwrap(),
            }])
            .unwrap()
        });
        assert_transaction_declined(
            "trusted-local-decline-title-rename",
            "org",
            1_250_020,
            |fixture| {
                OperationTransaction::new(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                    page_changes: vec![PageRename {
                        page_id: fixture.page_id,
                        new_name: LogicalPageName::parse("Renamed Title").unwrap(),
                        new_path: ManagedPath::parse("pages/Renamed Title.org").unwrap(),
                    }],
                    block_rewrites: Vec::new(),
                    page_preamble_rewrites: Vec::new(),
                }])
                .unwrap()
            },
        );
        assert_transaction_declined("trusted-local-decline-kind", "md", 1_250_030, |fixture| {
            OperationTransaction::new(vec![SemanticOperation::SetPageKind {
                page_id: fixture.page_id,
                kind: ManagedTextKind::Journal,
            }])
            .unwrap()
        });
        assert_transaction_declined(
            "trusted-local-decline-delete",
            "org",
            1_250_040,
            |fixture| {
                OperationTransaction::new(vec![SemanticOperation::DeletePage {
                    page_id: fixture.page_id,
                }])
                .unwrap()
            },
        );
        assert_transaction_declined("trusted-local-decline-create", "md", 1_250_050, |_| {
            OperationTransaction::new(vec![SemanticOperation::CreatePage {
                page_id: PageId::from_uuid(Uuid::from_u128(1_250_051)),
                home_document_id: DocumentId::from_uuid(Uuid::from_u128(1_250_052)),
                name: LogicalPageName::parse("Created Elsewhere").unwrap(),
                path: ManagedPath::parse("pages/Created Elsewhere.md").unwrap(),
                kind: ManagedTextKind::Page,
            }])
            .unwrap()
        });
        assert_transaction_declined(
            "trusted-local-decline-identity",
            "org",
            1_250_060,
            |fixture| {
                OperationTransaction::new(vec![SemanticOperation::MutateBlockLogseqIdentity {
                    block: BlockLocation {
                        block_id: fixture.block_id,
                        home_document_id: fixture.home_document_id,
                    },
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: LogseqUuid::from_uuid(Uuid::from_u128(1_250_061)),
                        trigger: LogseqIdentityTrigger::ExportUserAction,
                    },
                }])
                .unwrap()
            },
        );
        assert_transaction_declined(
            "trusted-local-decline-multi-page",
            "md",
            1_250_070,
            |fixture| {
                let mut operations = fixture.content_edit(1).operations;
                operations.push(SemanticOperation::CreatePage {
                    page_id: PageId::from_uuid(Uuid::from_u128(1_250_071)),
                    home_document_id: DocumentId::from_uuid(Uuid::from_u128(1_250_072)),
                    name: LogicalPageName::parse("Second Projection").unwrap(),
                    path: ManagedPath::parse("pages/Second Projection.md").unwrap(),
                    kind: ManagedTextKind::Page,
                });
                OperationTransaction::new(operations).unwrap()
            },
        );

        let mut unpinned = OverlayFixture::new("trusted-local-decline-unpinned", "md", 8);
        let (_, mut unpinned_journal) = unpinned.journal("unpinned");
        let (mut page, base_revision) = edited_page(&unpinned, 1);
        page.path.clear();
        let prepared = prepared_edit(&mut unpinned, 1_250_080, 1);
        let outcome = TrustedLocalCommitCoordinator::commit(
            &unpinned.graph,
            &mut unpinned_journal,
            &mut unpinned.engine,
            &page,
            &base_revision,
            prepared,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            TrustedLocalCommitOutcome::Declined {
                reason: TrustedLocalDeclineReason::ExistingPinnedPageRequired
            }
        ));
        assert_eq!(unpinned_journal.stats().frames_appended, 0);
    }

    #[test]
    fn foreground_source_has_no_old_terminal_pipeline_escape_hatch() {
        let source = include_str!("trusted_local_commit.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for forbidden in [
            "OperationalCoordinator::execute_local",
            "publish_prepared",
            "stage_archive_batch",
            "reserve_bound_mutation",
            "enqueue_reserved",
            "drain_ready",
            "SqliteFrontier",
            "ProjectionReceiptStore",
            "record_local_authorship_receipt",
            "record_provider_publication",
            "load_current_editor_page",
            "reload_application_page",
            "settle_application_publication",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden foreground token: {forbidden}"
            );
        }
        assert!(source.contains("commit_existing_page_with_journal"));
        assert!(source.contains("append_managed_local_record"));
        assert!(source.contains("apply_appended_managed_local_record"));
    }

    #[test]
    #[ignore = "manual release probe: builds synthetic 100 and 10,000 page graphs"]
    fn trusted_local_commit_manual_release_boundedness_probe() {
        let page_counts = std::env::var("TINE_TRUSTED_LOCAL_BENCH_PAGES")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .map(|part| part.parse::<usize>().unwrap())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![100, 10_000]);
        let edits = std::env::var("TINE_TRUSTED_LOCAL_BENCH_EDITS")
            .ok()
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(7);
        let warmups = std::env::var("TINE_TRUSTED_LOCAL_BENCH_WARMUPS")
            .ok()
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(2);
        assert!(edits > warmups);

        fn p50(mut samples: Vec<Duration>) -> Duration {
            samples.sort_unstable();
            samples[samples.len() / 2]
        }
        fn millis(duration: Duration) -> f64 {
            duration.as_secs_f64() * 1_000.0
        }

        let mut observed = Vec::new();
        for pages in page_counts {
            let mut fixture =
                OverlayFixture::new(&format!("trusted-local-bounded-{pages}"), "md", pages);
            let (_, mut journal) = fixture.journal("bounded");
            let managed_before = fixture.engine.managed_local_work();
            let mut totals = Vec::new();
            let mut prepared_stages = Vec::new();
            let mut graph_stages = Vec::new();
            let mut graph_validation_stages = Vec::new();
            let mut append_stages = Vec::new();
            let mut graph_publication_stages = Vec::new();
            let mut graph_cache_stages = Vec::new();
            let mut overlay_stages = Vec::new();
            let mut response_stages = Vec::new();
            for generation in 1..=edits {
                // Exact page loading and finalized-operation preparation are
                // deliberately outside the measured commit interval.
                let (page, base_revision) = edited_page(&fixture, generation);
                let prepared = prepared_edit(
                    &mut fixture,
                    1_260_000 + pages as u128 * 100 + generation as u128,
                    generation,
                );
                let graph_before = graph_wide_commit_work();
                let forbidden_before = forbidden_commit_work();
                let started = std::time::Instant::now();
                let outcome = TrustedLocalCommitCoordinator::commit(
                    &fixture.graph,
                    &mut journal,
                    &mut fixture.engine,
                    &page,
                    &base_revision,
                    prepared,
                )
                .unwrap();
                let elapsed = started.elapsed();
                assert!(matches!(outcome, TrustedLocalCommitOutcome::Committed(_)));
                assert!(forbidden_commit_work().since(forbidden_before).is_none());
                assert_eq!(
                    graph_wide_commit_work().since(graph_before),
                    Default::default()
                );
                if generation > warmups {
                    let stages = last_commit_stage_timings();
                    totals.push(elapsed);
                    prepared_stages.push(stages.prepared_record);
                    graph_stages.push(stages.graph_total);
                    graph_validation_stages.push(stages.graph_validation);
                    append_stages.push(stages.journal_append);
                    graph_publication_stages.push(stages.graph_publication);
                    graph_cache_stages.push(stages.graph_cache_publication);
                    overlay_stages.push(stages.hot_overlay_apply);
                    response_stages.push(stages.direct_response);
                }
            }
            let managed = fixture.engine.managed_local_work().since(managed_before);
            assert_eq!(managed.commits_applied, edits);
            assert_eq!(managed.documents_imported, edits);
            assert_eq!(managed.accepted_base_documents_loaded, 0);
            assert_eq!(managed.retained_author_candidates_used, edits);
            let total_p50 = p50(totals);
            eprintln!(
                "trusted-local bounded probe: pages={pages} samples={} total_p50_ms={:.6} prepared_p50_ms={:.6} graph_p50_ms={:.6} graph_validation_p50_ms={:.6} append_p50_ms={:.6} graph_publication_p50_ms={:.6} graph_cache_p50_ms={:.6} overlay_p50_ms={:.6} response_p50_ms={:.6} managed_work={managed:?} graph_work={:?}",
                edits - warmups,
                millis(total_p50),
                millis(p50(prepared_stages)),
                millis(p50(graph_stages)),
                millis(p50(graph_validation_stages)),
                millis(p50(append_stages)),
                millis(p50(graph_publication_stages)),
                millis(p50(graph_cache_stages)),
                millis(p50(overlay_stages)),
                millis(p50(response_stages)),
                GraphWideCommitWork::default(),
            );
            assert!(
                total_p50 < Duration::from_millis(10),
                "warm commit p50 exceeded 10 ms at {pages} pages: {total_p50:?}"
            );
            observed.push((pages, total_p50, managed));
        }
        assert!(observed.len() >= 2);
        assert_eq!(observed[0].2, observed[1].2);
    }
}
