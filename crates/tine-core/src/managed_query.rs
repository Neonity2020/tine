//! R4: the Managed Storage accepted-frontier query route.
//!
//! A simple query against a Managed graph whose actor holds NO pending local
//! suffix is answered the way a ready Direct Files query is (R3): from ONE
//! owned read snapshot of the accepted projection, through
//! `query::results::read_results`, with no page document loaded and no source
//! text read. The difference is WHERE it runs. The Managed projection is owned
//! by the sync actor, so the actor turn is kept short — it validates the turn,
//! consults the memo and, on a miss, CAPTURES the immutable inputs below — and
//! the read itself runs on the calling thread after the handle has released
//! its `operation` mutex. The actor keeps serving saves and navigation while
//! the statement runs; `QueryJobOwner` bounds how many run at once and drains
//! them before the actor removes, replaces, reopens or resets the file.
//!
//! An actor holding a pending suffix keeps today's masked walk: the suffix is
//! evidence the accepted frontier does not cover, and "pending may temporarily
//! walk" is the plan's stated route for it.
//!
//! Ownership (R4 dossier): this file's types, the shared memo, the census and
//! the capture/reply/drain wiring in `sync_runtime.rs` are the manager's (R4b);
//! the body of [`execute_managed_query`] and its tests are the R4a lane's.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::ParseConfig;
use crate::date::{JournalDate, JournalFormat};
use crate::model::RefGroup;
use crate::oplog::ContentDigest;
use crate::query::ir::{Query, ViewSettings};
use crate::query::registry::Registry;
use crate::query::{ConstructionProfile, PreViewGroups};
use crate::query_jobs::QueryJobOwner;

/// The ONE definition of "the full-text index is ready to be queried": the row
/// the FTS builder stamps when its build completed at this projection's
/// frontier. Both backends probe it with this exact statement
/// (`direct_projection::probe_fts_ready` and the Managed executor); a second
/// spelling would be a second definition.
pub(crate) const FTS_READY_PROBE_SQL: &str =
    "SELECT phase FROM search_fts_build WHERE singleton = 1";

/// The graph-wide half of a Managed answer's cache identity (C6/E6), and the
/// stamp the executor validates its snapshot against.
///
/// `acceptance_sequence` + `frontier_digest` are what
/// `PhysicalProjectionQuerySnapshot::open_managed` checks against the file's
/// `materialization_stamp`; a projection that has moved on answers `Stale` and
/// the handle re-captures. `config_digest` is carried unconditionally: the
/// journal title format decides a page's kind and day, so even a query with
/// no property leaf is config-sensitive, and a config edit never moves the
/// acceptance sequence. `today` is the execution day (§4.4): `(between …)`
/// resolves against it, so an answer from yesterday is not today's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedQueryStamp {
    pub(crate) acceptance_sequence: u64,
    pub(crate) frontier_digest: ContentDigest,
    pub(crate) config_digest: ContentDigest,
    pub(crate) today: i64,
}

/// The immutable inputs one actor turn captures for an accepted-frontier
/// query. Everything the executor reads is HERE; it touches no actor state,
/// no graph mutex and no live registry after the turn ends.
pub(crate) struct ManagedQueryCapture {
    /// The accepted projection's SQLite file.
    pub(crate) path: PathBuf,
    /// The graph root the projection's relative page paths hang off (recency).
    pub(crate) graph_root: PathBuf,
    pub(crate) stamp: ManagedQueryStamp,
    pub(crate) config: ParseConfig,
    /// The graph's journal title format: the Managed walk's recency producer
    /// is `JournalFormat::page_recency_secs(kind == Journal, name, path)` —
    /// by the page's NAME, `i64::MIN` when it does not parse — and the
    /// executor must produce exactly that (R4 verification D4).
    pub(crate) journal_format: JournalFormat,
    /// The property registry the query is lowered under. Built only when the
    /// query has a `props` leaf; otherwise the empty registry (C6).
    pub(crate) registry: Arc<Registry>,
    /// The registry generation the answer will be memoized under (0 when the
    /// query has no `props` leaf).
    pub(crate) registry_generation: u64,
    pub(crate) props: bool,
    pub(crate) query: Query,
    pub(crate) view: ViewSettings,
    pub(crate) today: JournalDate,
    pub(crate) max_rows: usize,
    pub(crate) max_bytes: usize,
    pub(crate) profile: ConstructionProfile,
    /// `query::simple_query_cache_key` of the resolved IR and bounds — the
    /// memo key, so two spellings of one query share one entry (I-12).
    pub(crate) key: String,
}

/// What one execution attempt produced. There is no fifth state: an attempt
/// answers, or the handle re-captures (`Stale`), or the walk answers.
#[derive(Debug)]
pub(crate) enum ManagedQueryOutcome {
    /// The statement answered from the snapshot; pre-view, un-ordered.
    Answered(PreViewGroups),
    /// The file's stamp no longer matches the capture: an accepted batch
    /// landed between the turn and the open. Not a failure — re-capture.
    Stale,
    /// No slot freed within the owner's wait. The walk answers; counted.
    Busy,
    /// The owner cancelled the job (a drain before a file replacement, or
    /// close). The walk answers; nothing is counted and nothing is recovered.
    Cancelled,
    /// A read was attempted and did not answer: an unopenable file, a seam
    /// refusal or a projection that contradicts itself. The reason names a
    /// column or table CLASS, never a value (I-5). SPEC §5.9 M10: a failed
    /// Managed read surfaces as an error, exactly as a failed materialized
    /// read does today; there is no walk fallback for it. Counted.
    Failed(&'static str),
}

/// Test-visible counters for the accepted route, owned by the handle so they
/// can be read without an actor turn. Always compiled: three relaxed atomics
/// cost nothing and keep the production and test wiring identical.
#[derive(Debug, Default)]
pub(crate) struct ManagedQueryCensus {
    /// Executions that opened a snapshot and ran the descriptor statement.
    pub(crate) statement_reads: AtomicUsize,
    /// Queries the walk answered because the executor reported `Busy`, or
    /// `Stale` more times than the handle re-captures.
    pub(crate) fallback_reads: AtomicUsize,
    /// Executions that reported `Failed`; the caller received an error.
    pub(crate) failed_reads: AtomicUsize,
    /// Re-captures after a `Stale` execution.
    pub(crate) stale_recaptures: AtomicUsize,
}

/// A copy of [`ManagedQueryCensus`] for assertions.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedQueryCensusSnapshot {
    pub(crate) statement_reads: usize,
    pub(crate) fallback_reads: usize,
    pub(crate) failed_reads: usize,
    pub(crate) stale_recaptures: usize,
}

impl ManagedQueryCensus {
    pub(crate) fn note_statement_read(&self) {
        self.statement_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_fallback_read(&self) {
        self.fallback_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_failed_read(&self) {
        self.failed_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_stale_recapture(&self) {
        self.stale_recaptures.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> ManagedQueryCensusSnapshot {
        ManagedQueryCensusSnapshot {
            statement_reads: self.statement_reads.load(Ordering::Relaxed),
            fallback_reads: self.fallback_reads.load(Ordering::Relaxed),
            failed_reads: self.failed_reads.load(Ordering::Relaxed),
            stale_recaptures: self.stale_recaptures.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    pub(crate) fn reset(&self) {
        self.statement_reads.store(0, Ordering::Relaxed);
        self.fallback_reads.store(0, Ordering::Relaxed);
        self.failed_reads.store(0, Ordering::Relaxed);
        self.stale_recaptures.store(0, Ordering::Relaxed);
    }
}

/// How many times the handle re-captures after a `Stale` execution before it
/// takes the walk. Two accepted batches landing inside one query's capture
/// window is a burst; three is a runtime that is not going to settle for this
/// answer.
pub(crate) const MAX_STALE_RECAPTURES: usize = 2;

/// Execute one captured accepted-frontier query on the CALLING thread.
///
/// The contract (R4 dossier, "The executor's contract"): capacity before any
/// transaction (`owner.acquire_within(wait)`), `open_managed` validated
/// against the stamp, the FTS probe, `lower_query` with no masked pages,
/// `read_results` with `BackendOrder::Managed` + `ResultIdentity::Stored`,
/// snapshot dropped before the slot. Never holds the actor, `operation`, or
/// any graph mutex; never spawns a thread; never memoizes. `wait` is the
/// owner's bounded slot wait — `QUERY_JOB_WAIT` in production, shorter under
/// a test that exercises `Busy`.
///
/// R4b ships this as a stub that reports `Busy` — the executor cannot run —
/// so the whole capture → execute → walk wiring is exercised end to end (and
/// counted as a fallback) before the lane replaces the body.
pub(crate) fn execute_managed_query(
    capture: &ManagedQueryCapture,
    owner: &QueryJobOwner,
    census: &ManagedQueryCensus,
    wait: Duration,
) -> ManagedQueryOutcome {
    let _ = (capture, owner, census, wait);
    ManagedQueryOutcome::Busy
}

/// Everything the accepted route shares between the actor and the handle:
/// the job owner (the actor drains it before touching the file), the census,
/// and the memo (the actor turn reads it, the handle and the walk fill it).
#[derive(Default)]
pub(crate) struct ManagedQueryShared {
    pub(crate) jobs: QueryJobOwner,
    pub(crate) census: ManagedQueryCensus,
    pub(crate) memo: Mutex<ApplicationSimpleQueryMemo>,
    /// Test hook: the outcomes the handle uses INSTEAD of executing the next
    /// captures, in order. Lets a test drive every handle-side transition
    /// (`Stale` re-capture, the third `Stale`, `Busy`, `Cancelled`, `Failed`)
    /// without a projection that produces it. Never consulted in production.
    #[cfg(test)]
    pub(crate) injected_outcomes: Mutex<VecDeque<ManagedQueryOutcome>>,
    /// Test hook: overrides the owner's slot wait so `Busy` costs a test
    /// milliseconds, not `QUERY_JOB_WAIT`.
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

    /// How long one execution waits for a slot.
    pub(crate) fn job_wait(&self) -> Duration {
        #[cfg(test)]
        if let Some(wait) = *self.job_wait.lock().unwrap() {
            return wait;
        }
        crate::query_jobs::QUERY_JOB_WAIT
    }

    /// Run the executor — or, under test, the next injected outcome.
    pub(crate) fn execute(&self, capture: &ManagedQueryCapture) -> ManagedQueryOutcome {
        #[cfg(test)]
        if let Some(outcome) = self.injected_outcomes.lock().unwrap().pop_front() {
            return outcome;
        }
        execute_managed_query(capture, &self.jobs, &self.census, self.job_wait())
    }
}

impl Default for QueryJobOwner {
    fn default() -> Self {
        Self::new(crate::query_jobs::DEFAULT_QUERY_JOB_CAPACITY)
    }
}

/// How many distinct simple-query answers one runtime retains at a time.
const APPLICATION_SIMPLE_QUERY_MEMO_ENTRIES: usize = 4;

/// The largest answer the memo will retain, counted in emitted result blocks.
/// A bigger answer is served but never stored, so the retained set stays a
/// small multiple of this bound rather than of the caller's `max_rows`.
const APPLICATION_SIMPLE_QUERY_MEMO_MAX_BLOCKS: usize = 4_096;

/// One memoized PRE-VIEW answer: base-ordered groups plus the recency axis,
/// so `sort-by` and `sample` are applied per request over one shared entry —
/// the Direct twin is `Graph::derived_memo_pre_view`.
#[derive(Clone, Debug)]
pub(crate) struct MemoizedPreView {
    pub(crate) groups: Arc<Vec<RefGroup>>,
    pub(crate) recency_by_page: Arc<HashMap<String, i64>>,
    pub(crate) total: usize,
    pub(crate) exceeded: bool,
}

#[derive(Debug)]
struct ApplicationSimpleQueryMemoEntry {
    key: String,
    /// Whether the query has a `props` leaf, so its answer depends on the
    /// registry's effective types and a generation advance evicts it (C6).
    props: bool,
    result: MemoizedPreView,
}

/// Bounded memo for a Managed simple query's PRE-VIEW answer.
///
/// Keyed by the resolved IR (`query::simple_query_cache_key`) and the
/// [`ManagedQueryStamp`]; every entry goes when the stamp moves — an accepted
/// batch, a config edit, or the execution day — and the `props` entries go
/// when the registry generation advances. A runtime holding a pending local
/// suffix neither reads nor fills it: the suffix is evidence the stamp does
/// not cover. Filled by the executor after a successful read and by the walk
/// after a complete evaluation; NEVER after `Stale`, `Busy`, `Cancelled` or
/// `Failed`, which are not answers.
///
/// It is a cache of a pure function over durable evidence, never authority: a
/// dropped or absent entry costs one recomputation and nothing else.
#[derive(Debug, Default)]
pub(crate) struct ApplicationSimpleQueryMemo {
    stamp: Option<ManagedQueryStamp>,
    registry_generation: u64,
    entries: VecDeque<ApplicationSimpleQueryMemoEntry>,
}

impl ApplicationSimpleQueryMemo {
    pub(crate) fn get(
        &mut self,
        stamp: &ManagedQueryStamp,
        registry_generation: u64,
        key: &str,
    ) -> Option<MemoizedPreView> {
        if self.stamp.as_ref() != Some(stamp) {
            self.stamp = Some(stamp.clone());
            self.entries.clear();
            self.registry_generation = registry_generation;
            return None;
        }
        self.note_registry_generation(registry_generation);
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.result.clone())
    }

    /// Drop every `props` entry when the registry generation advanced. The
    /// non-`props` entries are untouched: their answers cannot depend on an
    /// effective type they never read (C6).
    fn note_registry_generation(&mut self, registry_generation: u64) {
        if self.registry_generation == registry_generation {
            return;
        }
        self.registry_generation = registry_generation;
        self.entries.retain(|entry| !entry.props);
    }

    /// Store a base-ordered pre-view answer. Returns the entry as stored (or
    /// as it would have been stored, when the answer is too large to retain),
    /// so the caller applies the view to the same shared groups either way.
    pub(crate) fn insert(
        &mut self,
        stamp: &ManagedQueryStamp,
        registry_generation: u64,
        key: &str,
        props: bool,
        pre: PreViewGroups,
    ) -> MemoizedPreView {
        let result = MemoizedPreView {
            groups: Arc::new(pre.groups),
            recency_by_page: Arc::new(pre.recency_by_page),
            total: pre.total,
            exceeded: pre.exceeded,
        };
        if self.stamp.as_ref() != Some(stamp) {
            self.stamp = Some(stamp.clone());
            self.entries.clear();
            self.registry_generation = registry_generation;
        } else {
            self.note_registry_generation(registry_generation);
        }
        let blocks = result
            .groups
            .iter()
            .map(|group| group.blocks.len())
            .sum::<usize>();
        if blocks > APPLICATION_SIMPLE_QUERY_MEMO_MAX_BLOCKS {
            return result;
        }
        self.entries.retain(|entry| entry.key != key);
        while self.entries.len() >= APPLICATION_SIMPLE_QUERY_MEMO_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(ApplicationSimpleQueryMemoEntry {
            key: key.to_owned(),
            props,
            result: result.clone(),
        });
        result
    }

    /// The off-actor executor's insert: store the answer only while the memo
    /// still stands at the capture's stamp. An accepted batch can land between
    /// capture and insert, and a late result must not evict the entries the
    /// actor has since filled under the newer stamp (I-20); it is returned
    /// unstored and served once.
    pub(crate) fn insert_if_current(
        &mut self,
        stamp: &ManagedQueryStamp,
        registry_generation: u64,
        key: &str,
        props: bool,
        pre: PreViewGroups,
    ) -> MemoizedPreView {
        if self.stamp.as_ref() != Some(stamp) {
            return MemoizedPreView {
                groups: Arc::new(pre.groups),
                recency_by_page: Arc::new(pre.recency_by_page),
                total: pre.total,
                exceeded: pre.exceeded,
            };
        }
        self.insert(stamp, registry_generation, key, props, pre)
    }

    pub(crate) fn clear(&mut self) {
        self.stamp = None;
        self.entries.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(sequence: u64, config: &str, today: i64) -> ManagedQueryStamp {
        ManagedQueryStamp {
            acceptance_sequence: sequence,
            frontier_digest: ContentDigest::of(b"frontier"),
            config_digest: ContentDigest::of(config.as_bytes()),
            today,
        }
    }

    fn pre(total: usize) -> PreViewGroups {
        PreViewGroups {
            groups: Vec::new(),
            recency_by_page: HashMap::new(),
            total,
            exceeded: false,
        }
    }

    #[test]
    fn the_managed_memo_is_keyed_by_the_config_digest_and_the_execution_day() {
        let mut memo = ApplicationSimpleQueryMemo::default();
        let before = stamp(7, "one", 100);
        memo.insert(&before, 0, "k", false, pre(3));
        assert_eq!(
            memo.get(&before, 0, "k").map(|r| r.total),
            Some(3),
            "the same evidence under the same rules is the same answer"
        );
        // E6: the journal title format decides a page's kind and day, so a
        // query with no property leaf at all is config-sensitive, and editing
        // `logseq/config.edn` never moves the acceptance sequence.
        assert_eq!(
            memo.get(&stamp(7, "two", 100), 0, "k").map(|r| r.total),
            None,
            "a config change under an unchanged sequence must not serve the old answer"
        );
        // §4.4: `(between -7d today)` is a different question tomorrow.
        memo.insert(&before, 0, "k", false, pre(3));
        assert_eq!(
            memo.get(&stamp(7, "one", 101), 0, "k").map(|r| r.total),
            None
        );
        // And the frontier digest is part of the identity, not only the sequence.
        memo.insert(&before, 0, "k", false, pre(3));
        let other_frontier = ManagedQueryStamp {
            frontier_digest: ContentDigest::of(b"other"),
            ..before.clone()
        };
        assert_eq!(memo.get(&other_frontier, 0, "k").map(|r| r.total), None);
    }

    #[test]
    fn a_registry_generation_advance_evicts_the_managed_memos_props_entries_only() {
        let mut memo = ApplicationSimpleQueryMemo::default();
        let s = stamp(7, "one", 100);
        memo.insert(&s, 4, "props-query", true, pre(1));
        memo.insert(&s, 4, "task-query", false, pre(2));
        // A graph-wide effective-type change. Per-page retention cannot see
        // it: the pages holding the answer did not change, only the key's
        // type did.
        assert_eq!(
            memo.get(&s, 5, "props-query").map(|r| r.total),
            None,
            "the typed query is recomputed under the new generation"
        );
        assert_eq!(
            memo.get(&s, 5, "task-query").map(|r| r.total),
            Some(2),
            "a query that reads no property atom cannot depend on an effective type"
        );
    }

    #[test]
    fn the_memo_is_bounded_by_entries_and_by_answer_size() {
        let mut memo = ApplicationSimpleQueryMemo::default();
        let s = stamp(1, "c", 1);
        for i in 0..APPLICATION_SIMPLE_QUERY_MEMO_ENTRIES + 2 {
            memo.insert(&s, 0, &format!("k{i}"), false, pre(i));
        }
        assert_eq!(memo.len(), APPLICATION_SIMPLE_QUERY_MEMO_ENTRIES);
        assert!(
            memo.get(&s, 0, "k0").is_none(),
            "the oldest entry is evicted"
        );
        assert!(memo.get(&s, 0, "k5").is_some());

        let mut huge = pre(0);
        huge.groups.push(RefGroup {
            page: "big".into(),
            kind: crate::model::PageKind::Page,
            blocks: (0..APPLICATION_SIMPLE_QUERY_MEMO_MAX_BLOCKS + 1)
                .map(|_| crate::model::BlockDto::default())
                .collect(),
            evidence: Vec::new(),
        });
        let served = memo.insert(&s, 0, "huge", false, huge);
        assert_eq!(
            served.groups[0].blocks.len(),
            APPLICATION_SIMPLE_QUERY_MEMO_MAX_BLOCKS + 1
        );
        assert!(memo.get(&s, 0, "huge").is_none(), "served but never stored");
        memo.clear();
        assert_eq!(memo.len(), 0);
    }

    #[test]
    fn a_late_executor_insert_never_evicts_a_newer_memo() {
        let mut memo = ApplicationSimpleQueryMemo::default();
        let old = stamp(7, "c", 1);
        let new = stamp(8, "c", 1);
        // The actor filled the memo under the newer stamp while an off-actor
        // execution captured under the older one was still running.
        memo.insert(&new, 0, "fresh", false, pre(5));
        let served = memo.insert_if_current(&old, 0, "late", false, pre(9));
        assert_eq!(served.total, 9, "the late answer is still served once");
        assert_eq!(
            memo.get(&new, 0, "fresh").map(|r| r.total),
            Some(5),
            "I-20: a late result cannot land on newer state"
        );
        assert!(memo.get(&new, 0, "late").is_none());
        // Under the current stamp it is an ordinary insert.
        memo.insert_if_current(&new, 0, "late", false, pre(9));
        assert_eq!(memo.get(&new, 0, "late").map(|r| r.total), Some(9));
    }

    /// The storage contract's account of this route, pinned sentence by
    /// sentence so a rewrite of either side fails here first.
    #[test]
    fn storage_contract_names_the_off_actor_accepted_route() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        let section = contract
            .split("**A Managed simple query over the accepted frontier runs off the actor.**")
            .nth(1)
            .and_then(|tail| tail.split("## 2. Enrollment").next())
            .expect("the accepted-route paragraph precedes section 2");
        for sentence in [
            "one short actor turn",
            "executes on\nthe calling thread after the actor's operation lock is released",
            "the same owner every off-actor read of that file is admitted\nby",
            "only while that stamp is still the actor's current stamp",
            "re-captures at most twice, then walks",
            "`Cancelled` (a drain caught it) walks and is\nnot counted as a fallback",
            "a `Failed` read is an error",
            "A pending local suffix is never captured",
            "closed in exactly three places",
            "writes a checkpoint sidecar, never a WAL\ncheckpoint",
        ] {
            assert!(section.contains(sentence), "contract lost: {sentence}");
        }
        assert_eq!(MAX_STALE_RECAPTURES, 2, "the contract says twice");
    }

    #[test]
    fn the_stub_executor_reports_busy_and_counts_nothing() {
        let owner = QueryJobOwner::new(1);
        let census = ManagedQueryCensus::default();
        let today = JournalDate::today();
        let (query, view) = crate::query::parse_query_source("(task TODO)", today);
        let profile = ConstructionProfile::from_view(&view);
        let config = crate::config::Config::default();
        let capture = ManagedQueryCapture {
            path: PathBuf::from("/nonexistent/projection.sqlite"),
            graph_root: PathBuf::from("/nonexistent"),
            stamp: stamp(1, "c", today.ordinal_key()),
            config: config.parse_config(),
            journal_format: crate::date::JournalFormat::new(
                config.journal_file_name_format.as_deref(),
                config.journal_page_title_format.as_deref(),
            ),
            registry: Arc::new(Registry::empty(&config.parse_config())),
            registry_generation: 0,
            props: false,
            key: crate::query::simple_query_cache_key(&query, 10, 100, profile),
            query,
            view,
            today,
            max_rows: 10,
            max_bytes: 100,
            profile,
        };
        assert!(matches!(
            execute_managed_query(&capture, &owner, &census, Duration::from_millis(1)),
            ManagedQueryOutcome::Busy
        ));
        assert_eq!(census.snapshot(), ManagedQueryCensusSnapshot::default());
        assert_eq!(owner.active(), 0);
    }
}
