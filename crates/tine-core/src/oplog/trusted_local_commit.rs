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

    pub(crate) fn relative_path(&self) -> &str {
        self.graph.relative_path()
    }

    pub(crate) fn last_error(&self) -> &io::Error {
        self.graph.last_error()
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

    pub(crate) const fn prepared_record(&self) -> &PreparedManagedLocalRecord {
        &self.prepared
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
        let expected_base = projection
            .precondition_base()
            .ok_or_else(|| {
                TrustedLocalCommitError::InvalidPreparedInput(
                    "existing-page trusted commit has an absent projection precondition".into(),
                )
            })?
            .bytes();
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

        let turn = super::local_journal_drain::projection_turn_from_managed_local_record(
            prepared
                .record()
                .prepared_batch()
                .manifest()
                .author_device_id()
                .as_uuid(),
            engine.lineage_digest(),
            prepared.record(),
        )
        .map_err(|error| TrustedLocalCommitError::InvalidPreparedInput(error.to_string()))?;
        let turn_id = turn.turn_id();
        let turn_short_id = [turn_id[0], turn_id[1], turn_id[2], turn_id[3]];

        #[cfg(test)]
        let graph_started = Instant::now();
        let graph_outcome = match graph.commit_existing_page_with_journal_evidence(
            page,
            base_revision,
            expected_base,
            exact_target,
            response_evidence.as_ref(),
            turn_short_id,
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
        || projection
            .precondition_base()
            .is_none_or(|base| base.source_path().as_str() != page.path)
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

/// The one architectural fact this file's deleted dark test corpus was carrying.
///
/// The pre-0.7 fast-commit corpus that used to sit at the end of this file was
/// gated `#[cfg(all(test, any()))]` — always false — and targeted a retired API
/// (18 compile errors when enabled), so it was deleted (Martin, 2026-09-01)
/// after this assertion was salvaged. Assert it against the census's definition
/// of production source.
#[cfg(test)]
mod foreground_source_guard {
    use crate::projection_producer_census::production_rust;

    /// The foreground trusted-local commit path must not reach back into the old
    /// terminal pipeline: no coordinator execution, archive staging, SQLite
    /// frontier, receipt store, or application-page reload. Those are what made
    /// a keystroke wait on graph-sized work.
    #[test]
    fn foreground_source_has_no_old_terminal_pipeline_escape_hatch() {
        let file = production_rust()
            .iter()
            .find(|file| file.relative.ends_with("oplog/trusted_local_commit.rs"))
            .expect("trusted_local_commit.rs is production source");
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
                !file.code.contains(forbidden),
                "forbidden foreground token in trusted_local_commit.rs: {forbidden} -- the \
                 foreground commit path must not reach the old terminal pipeline"
            );
        }
        for required in [
            "commit_existing_page_with_journal",
            "append_managed_local_record",
            "apply_appended_managed_local_record",
        ] {
            assert!(
                file.code.contains(required),
                "trusted_local_commit.rs no longer defines {required}; this guard is \
                 describing a path that moved"
            );
        }
    }
}
