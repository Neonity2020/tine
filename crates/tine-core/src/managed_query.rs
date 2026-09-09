//! Managed query execution over one current main SQLite snapshot.
//!
//! The actor captures lifecycle, configuration, and, when property lowering
//! needs it, an immutable committed-registry cache input. The calling thread
//! opens the main snapshot, validates its accepted stamp inside that read
//! transaction, builds the registry there, and executes the statement. Local
//! editor/navigation pending state remains actor-owned and is not a query
//! source.

#[cfg(test)]
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use tine_storage::sqlite::{MaterializationError, PhysicalProjectionQuerySnapshot};

use crate::config::ParseConfig;
use crate::date::{JournalDate, JournalFormat};
use crate::model::PageKind;
use crate::oplog::ContentDigest;
use crate::query::ir::{Query, ViewSettings};
use crate::query::registry::Registry;
use crate::query::results::{
    read_page_results, read_results, BackendOrder, RecencyPage, ResultIdentity, ResultReadError,
    ResultReadInputs,
};
use crate::query::sql::{lower_query, LoweringInputs, RESULT_SET_RULE};
use crate::query::{ConstructionProfile, PreViewGroups};
use crate::query_jobs::{Admission, JobSlot, QueryJobOwner};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedQueryStamp {
    pub(crate) acceptance_sequence: u64,
    pub(crate) frontier_digest: ContentDigest,
    pub(crate) config_digest: ContentDigest,
}

#[derive(Clone)]
pub(crate) struct ManagedRegistryCapture {
    pub(crate) owner: crate::query::registry_cache::SharedRegistryCache,
    pub(crate) capture: crate::query::registry_cache::RegistryCapture,
}

pub(crate) struct ManagedReadInput<'a> {
    pub(crate) path: &'a Path,
    pub(crate) stamp: &'a ManagedQueryStamp,
    pub(crate) config: &'a ParseConfig,
    /// None for a property-free query, which avoids registry capture and SQL.
    pub(crate) registry: Option<&'a ManagedRegistryCapture>,
}

/// Immutable inputs for any operation reading the current main projection.
pub(crate) struct ManagedReadCapture {
    pub(crate) job_epoch: crate::query_jobs::QueryJobEpoch,
    pub(crate) path: PathBuf,
    pub(crate) graph_root: PathBuf,
    pub(crate) stamp: ManagedQueryStamp,
    pub(crate) config: ParseConfig,
    pub(crate) journal_format: JournalFormat,
    pub(crate) registry: Option<ManagedRegistryCapture>,
}

impl ManagedReadCapture {
    pub(crate) fn read_input(&self) -> ManagedReadInput<'_> {
        ManagedReadInput {
            path: &self.path,
            stamp: &self.stamp,
            config: &self.config,
            registry: self.registry.as_ref(),
        }
    }
}

pub(crate) struct ManagedQueryCapture {
    pub(crate) job_epoch: crate::query_jobs::QueryJobEpoch,
    pub(crate) path: PathBuf,
    pub(crate) graph_root: PathBuf,
    pub(crate) stamp: ManagedQueryStamp,
    pub(crate) config: ParseConfig,
    pub(crate) journal_format: JournalFormat,
    pub(crate) registry: Option<ManagedRegistryCapture>,
    pub(crate) query: Query,
    pub(crate) view: ViewSettings,
    pub(crate) today: JournalDate,
    pub(crate) max_rows: usize,
    pub(crate) max_bytes: usize,
    pub(crate) profile: ConstructionProfile,
    pub(crate) request: ManagedQueryRequest,
    pub(crate) report: crate::query::ir::QueryReport,
}

impl ManagedQueryCapture {
    pub(crate) fn read_input(&self) -> ManagedReadInput<'_> {
        ManagedReadInput {
            path: &self.path,
            stamp: &self.stamp,
            config: &self.config,
            registry: self.registry.as_ref(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ManagedQueryRequest {
    Blocks,
    Pages,
    Counts(Vec<Query>),
}

#[derive(Debug)]
pub(crate) enum ManagedQueryAnswer {
    Blocks(PreViewGroups),
    Pages(crate::query::results::PageAnswer),
    Counts(Vec<usize>),
}

impl ManagedQueryAnswer {
    pub(crate) fn into_blocks(self) -> Option<PreViewGroups> {
        match self {
            Self::Blocks(answer) => Some(answer),
            _ => None,
        }
    }

    pub(crate) fn into_pages(self) -> Option<crate::query::results::PageAnswer> {
        match self {
            Self::Pages(answer) => Some(answer),
            _ => None,
        }
    }

    pub(crate) fn into_counts(self) -> Option<Vec<usize>> {
        match self {
            Self::Counts(answer) => Some(answer),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ManagedQueryOutcome {
    Answered(ManagedQueryAnswer),
    Stale,
    Busy,
    Cancelled,
    Failed(&'static str),
}

#[derive(Debug, Default)]
pub(crate) struct ManagedQueryCensus {
    pub(crate) statement_reads: AtomicUsize,
    pub(crate) failed_reads: AtomicUsize,
    pub(crate) stale_recaptures: AtomicUsize,
    pub(crate) metadata_reads: AtomicUsize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedQueryCensusSnapshot {
    pub(crate) statement_reads: usize,
    pub(crate) failed_reads: usize,
    pub(crate) stale_recaptures: usize,
    pub(crate) metadata_reads: usize,
}

impl ManagedQueryCensus {
    pub(crate) fn note_statement_read(&self) {
        self.statement_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_failed_read(&self) {
        self.failed_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_stale_recapture(&self) {
        self.stale_recaptures.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_metadata_read(&self) {
        self.metadata_reads.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> ManagedQueryCensusSnapshot {
        ManagedQueryCensusSnapshot {
            statement_reads: self.statement_reads.load(Ordering::Relaxed),
            failed_reads: self.failed_reads.load(Ordering::Relaxed),
            stale_recaptures: self.stale_recaptures.load(Ordering::Relaxed),
            metadata_reads: self.metadata_reads.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    pub(crate) fn reset(&self) {
        self.statement_reads.store(0, Ordering::Relaxed);
        self.failed_reads.store(0, Ordering::Relaxed);
        self.stale_recaptures.store(0, Ordering::Relaxed);
        self.metadata_reads.store(0, Ordering::Relaxed);
    }
}

pub(crate) const MAX_STALE_RECAPTURES: usize = 2;

pub(crate) fn execute_managed_query(
    capture: &ManagedQueryCapture,
    owner: &QueryJobOwner,
    census: &ManagedQueryCensus,
    wait: Duration,
) -> ManagedQueryOutcome {
    match with_managed_read(
        capture.job_epoch,
        &capture.read_input(),
        owner,
        wait,
        |opened| {
            Ok(execute_main_source(
                capture,
                opened.snapshot,
                &opened.registry,
                census,
            ))
        },
    ) {
        Ok(outcome) | Err(outcome) => outcome,
    }
}

/// Admission precedes opening the transaction; every exit drops the snapshot
/// before releasing capacity and checks cancellation after construction.
pub(crate) fn with_managed_read<T>(
    epoch: crate::query_jobs::QueryJobEpoch,
    input: &ManagedReadInput<'_>,
    owner: &QueryJobOwner,
    wait: Duration,
    read: impl FnOnce(OpenedManagedRead) -> Result<T, ManagedQueryOutcome>,
) -> Result<T, ManagedQueryOutcome> {
    let slot = match owner.acquire_at_within(epoch, wait) {
        Admission::Slot(slot) => slot,
        Admission::Busy => return Err(ManagedQueryOutcome::Busy),
        Admission::Cancelled => return Err(ManagedQueryOutcome::Cancelled),
    };
    let outcome = open_managed_read(input, &slot).and_then(read);
    let outcome = if slot.is_cancelled() {
        Err(ManagedQueryOutcome::Cancelled)
    } else {
        outcome
    };
    drop(slot);
    outcome
}

pub(crate) struct OpenedManagedRead {
    pub(crate) snapshot: PhysicalProjectionQuerySnapshot,
    pub(crate) registry: Arc<Registry>,
}

pub(crate) fn open_managed_read(
    input: &ManagedReadInput<'_>,
    slot: &JobSlot<'_>,
) -> Result<OpenedManagedRead, ManagedQueryOutcome> {
    #[cfg(test)]
    run_before_managed_open_hook();
    let mut snapshot = match PhysicalProjectionQuerySnapshot::open_managed(
        input.path,
        input.stamp.acceptance_sequence,
        input.stamp.frontier_digest,
    ) {
        Ok(snapshot) => snapshot,
        Err(MaterializationError::Stale { .. }) => return Err(ManagedQueryOutcome::Stale),
        Err(_) => return Err(ManagedQueryOutcome::Failed("managed projection snapshot")),
    };
    if !slot.register(snapshot.cancellation()) {
        return Err(ManagedQueryOutcome::Cancelled);
    }
    let registry = match input.registry {
        Some(registry) => {
            let built = registry
                .capture
                .build_at_validated_revision(
                    &mut snapshot,
                    input.config,
                    input.stamp.acceptance_sequence,
                )
                .map_err(registry_build_outcome)?;
            registry
                .owner
                .lock()
                .unwrap()
                .publish(registry.capture.clone(), built)
                .map_err(registry_build_outcome)?
        }
        None => Arc::new(Registry::empty(input.config)),
    };
    Ok(OpenedManagedRead { snapshot, registry })
}

fn registry_build_outcome(error: crate::query::QueryExecutionError) -> ManagedQueryOutcome {
    match error {
        crate::query::QueryExecutionError::Cancelled => ManagedQueryOutcome::Cancelled,
        _ => ManagedQueryOutcome::Failed("property_registry"),
    }
}

fn execute_main_source(
    capture: &ManagedQueryCapture,
    mut snapshot: PhysicalProjectionQuerySnapshot,
    registry: &Arc<Registry>,
    census: &ManagedQueryCensus,
) -> ManagedQueryOutcome {
    let fts_ready = match probe_fts_ready(&mut snapshot) {
        Ok(ready) => ready,
        Err(outcome) => return outcome,
    };
    let lower = |query: &Query| {
        let query = anchored_for(query, &capture.request);
        let compiled = crate::query::eval::CompiledLeaves::for_query(&query.evaluable_filter());
        let statement = lower_query(
            &query,
            &LoweringInputs {
                today: capture.today,
                registry,
                cutoff: None,
                compiled: &compiled,
                fts_ready,
                result_set_rule: RESULT_SET_RULE,
            },
        );
        (query.anchor, statement)
    };
    let recency = |page: RecencyPage<'_>| {
        capture.journal_format.page_recency_secs(
            page.kind == PageKind::Journal,
            page.name,
            &capture.graph_root.join(page.path),
        )
    };
    let read = |snapshot: &mut PhysicalProjectionQuerySnapshot,
                anchor: crate::query::ir::Anchor,
                statement: &crate::query::sql::SqlQuery|
     -> Result<ManagedQueryAnswer, ResultReadError> {
        if statement.matches_nothing {
            return Ok(empty_answer(anchor));
        }
        match anchor {
            crate::query::ir::Anchor::Page => Ok(ManagedQueryAnswer::Pages(read_page_results(
                snapshot,
                statement,
                BackendOrder::Managed,
                capture.max_rows,
            )?)),
            crate::query::ir::Anchor::Block => Ok(ManagedQueryAnswer::Blocks(read_results(
                snapshot,
                &ResultReadInputs {
                    statement,
                    order: BackendOrder::Managed,
                    identity: &ResultIdentity::Stored,
                    max_rows: capture.max_rows,
                    max_bytes: capture.max_bytes,
                    profile: capture.profile,
                    recency: &recency,
                },
            )?)),
        }
    };
    let answer = match &capture.request {
        ManagedQueryRequest::Counts(probes) => {
            let mut counts = Vec::with_capacity(probes.len());
            let mut failure = None;
            for probe in probes {
                let (anchor, statement) = lower(probe);
                match read(&mut snapshot, anchor, &statement) {
                    Ok(answer) => counts.push(answer_total(&answer)),
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            match failure {
                Some(error) => Err(error),
                None => Ok(ManagedQueryAnswer::Counts(counts)),
            }
        }
        _ => {
            let (anchor, statement) = lower(&capture.query);
            read(&mut snapshot, anchor, &statement)
        }
    };
    match answer {
        Ok(answer) => {
            census.note_statement_read();
            ManagedQueryOutcome::Answered(answer)
        }
        Err(ResultReadError::Cancelled) => ManagedQueryOutcome::Cancelled,
        Err(ResultReadError::Sql(_)) => ManagedQueryOutcome::Failed("managed projection statement"),
        Err(ResultReadError::Corrupt(_)) => {
            ManagedQueryOutcome::Failed("managed projection result rows")
        }
    }
}

fn anchored_for(query: &Query, request: &ManagedQueryRequest) -> Query {
    match (request, query.anchor) {
        (ManagedQueryRequest::Pages, _)
        | (ManagedQueryRequest::Counts(_), crate::query::ir::Anchor::Page) => query.clone(),
        _ => crate::query::block_anchored_query(query),
    }
}

fn empty_answer(anchor: crate::query::ir::Anchor) -> ManagedQueryAnswer {
    match anchor {
        crate::query::ir::Anchor::Page => {
            ManagedQueryAnswer::Pages(crate::query::results::PageAnswer::default())
        }
        crate::query::ir::Anchor::Block => ManagedQueryAnswer::Blocks(PreViewGroups::default()),
    }
}

fn answer_total(answer: &ManagedQueryAnswer) -> usize {
    match answer {
        ManagedQueryAnswer::Blocks(pre) => pre.total,
        ManagedQueryAnswer::Pages(pages) => pages.total,
        ManagedQueryAnswer::Counts(_) => 0,
    }
}

#[cfg(test)]
thread_local! {
    static BEFORE_MANAGED_OPEN: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_managed_open_hook(hook: Option<Box<dyn Fn()>>) {
    BEFORE_MANAGED_OPEN.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn run_before_managed_open_hook() {
    let taken = BEFORE_MANAGED_OPEN.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = taken {
        hook();
        BEFORE_MANAGED_OPEN.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
}

fn probe_fts_ready(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
) -> Result<bool, ManagedQueryOutcome> {
    crate::query::results::probe_fts_ready(snapshot).map_err(|error| match error {
        ResultReadError::Cancelled => ManagedQueryOutcome::Cancelled,
        _ => ManagedQueryOutcome::Failed("search_fts_build phase"),
    })
}

#[derive(Default)]
pub(crate) struct ManagedQueryShared {
    pub(crate) jobs: QueryJobOwner,
    pub(crate) census: ManagedQueryCensus,
    #[cfg(test)]
    pub(crate) injected_outcomes: Mutex<VecDeque<ManagedQueryOutcome>>,
    #[cfg(test)]
    pub(crate) job_wait: Mutex<Option<Duration>>,
}

impl ManagedQueryShared {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            jobs: QueryJobOwner::new(capacity),
            ..Self::default()
        }
    }

    pub(crate) fn job_wait(&self) -> Duration {
        #[cfg(test)]
        if let Some(wait) = *self.job_wait.lock().unwrap() {
            return wait;
        }
        crate::query_jobs::QUERY_JOB_WAIT
    }

    pub(crate) fn execute(&self, capture: &ManagedQueryCapture) -> ManagedQueryOutcome {
        #[cfg(test)]
        if let Some(outcome) = self.injected_outcomes.lock().unwrap().pop_front() {
            return outcome;
        }
        execute_managed_query(capture, &self.jobs, &self.census, self.job_wait())
    }

    pub(crate) fn execute_export(
        &self,
        capture: &ManagedReadCapture,
        prepared: &crate::query::export_execute::PreparedExportBatch,
        max_roots: usize,
        max_nodes: usize,
        max_bytes: usize,
    ) -> Result<crate::query::QueryExportBatch, ManagedQueryOutcome> {
        let answer = with_managed_read(
            capture.job_epoch,
            &capture.read_input(),
            &self.jobs,
            self.job_wait(),
            |mut opened| {
                let recency = |page: RecencyPage<'_>| {
                    capture.journal_format.page_recency_secs(
                        page.kind == PageKind::Journal,
                        page.name,
                        &capture.graph_root.join(page.path),
                    )
                };
                prepared
                    .execute(
                        &mut opened.snapshot,
                        &crate::query::export_execute::ExportExecutionInputs {
                            registry: &opened.registry,
                            identity: &ResultIdentity::Stored,
                            order: BackendOrder::Managed,
                            recency: &recency,
                            max_roots,
                            max_nodes,
                            max_bytes,
                        },
                    )
                    .map_err(|error| match error {
                        ResultReadError::Cancelled => ManagedQueryOutcome::Cancelled,
                        ResultReadError::Sql(_) => {
                            ManagedQueryOutcome::Failed("managed export statement")
                        }
                        ResultReadError::Corrupt(_) => {
                            ManagedQueryOutcome::Failed("managed export result rows")
                        }
                    })
            },
        );
        if answer.is_ok() {
            self.census.note_statement_read();
        }
        answer
    }

    pub(crate) fn execute_metadata(
        &self,
        capture: &crate::managed_metadata::ManagedMetadataCapture,
    ) -> crate::managed_metadata::ManagedMetadataOutcome {
        #[cfg(test)]
        if let Some(outcome) = self.injected_outcomes.lock().unwrap().pop_front() {
            return crate::managed_metadata::ManagedMetadataOutcome::NotAnswered(outcome);
        }
        crate::managed_metadata::execute_managed_metadata(
            capture,
            &self.jobs,
            &self.census,
            self.job_wait(),
        )
    }
}

impl Default for QueryJobOwner {
    fn default() -> Self {
        Self::new(crate::query_jobs::DEFAULT_QUERY_JOB_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(sequence: u64, config: &str) -> ManagedQueryStamp {
        ManagedQueryStamp {
            acceptance_sequence: sequence,
            frontier_digest: ContentDigest::of(b"frontier"),
            config_digest: ContentDigest::of(config.as_bytes()),
        }
    }

    fn capture(owner: &QueryJobOwner) -> ManagedQueryCapture {
        let today = JournalDate::today();
        let (query, view) = crate::query::parse_query_source("(task TODO)", today);
        let profile = ConstructionProfile::from_view(&view);
        let config = crate::config::Config::default();
        ManagedQueryCapture {
            job_epoch: owner.capture_epoch(),
            path: PathBuf::from("/nonexistent/projection.sqlite"),
            graph_root: PathBuf::from("/nonexistent"),
            stamp: stamp(1, "c"),
            config: config.parse_config(),
            journal_format: crate::date::JournalFormat::new(
                config.journal_file_name_format.as_deref(),
                config.journal_page_title_format.as_deref(),
            ),
            registry: None,
            query,
            view,
            today,
            max_rows: 10,
            max_bytes: 100,
            profile,
            request: ManagedQueryRequest::Blocks,
            report: crate::query::ir::QueryReport {
                ran: Vec::new(),
                ignored: Vec::new(),
                supported: true,
            },
        }
    }

    #[test]
    fn an_unopenable_main_projection_fails_and_releases_capacity() {
        let owner = QueryJobOwner::new(1);
        let census = ManagedQueryCensus::default();
        let capture = capture(&owner);
        assert!(matches!(
            execute_managed_query(&capture, &owner, &census, Duration::from_millis(1)),
            ManagedQueryOutcome::Failed("managed projection snapshot")
        ));
        assert_eq!(census.snapshot(), ManagedQueryCensusSnapshot::default());
        assert_eq!(owner.active(), 0);

        let held = match owner.acquire() {
            Admission::Slot(slot) => slot,
            _ => panic!("the only slot admits"),
        };
        assert!(matches!(
            execute_managed_query(&capture, &owner, &census, Duration::from_millis(1)),
            ManagedQueryOutcome::Busy
        ));
        drop(held);
        owner.close();
        assert!(matches!(
            execute_managed_query(&capture, &owner, &census, Duration::from_millis(1)),
            ManagedQueryOutcome::Cancelled
        ));
    }

    #[test]
    fn stamp_contains_only_main_and_config_evidence() {
        let stamp = stamp(7, "config");
        assert_eq!(stamp.acceptance_sequence, 7);
        assert_eq!(stamp.frontier_digest, ContentDigest::of(b"frontier"));
        assert_eq!(stamp.config_digest, ContentDigest::of(b"config"));
    }

    #[test]
    fn storage_contract_names_the_one_main_snapshot_route() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        let section = contract
            .split("**Managed live queries read one current main snapshot.**")
            .nth(1)
            .and_then(|tail| tail.split("## 2. Enrollment").next())
            .expect("the Managed main-only query paragraph precedes section 2");
        for phrase in [
            "one short actor turn captures immutable inputs",
            "actual database frontier root",
            "opens one main SQLite snapshot",
            "inside that read transaction",
            "outside the actor and cache lock",
            "transaction ends before the job slot is released",
            "loads no page document and parses no source text",
            "pending editor and navigation state",
        ] {
            assert!(section.contains(phrase), "contract lost: {phrase}");
        }
    }
}
