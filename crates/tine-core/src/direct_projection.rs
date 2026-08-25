use crate::doc::{property_key_norm, DocBlock, Document};
use crate::model::{Format, PageEntry, PageKind, ReferenceKind};
use crate::query::{
    run_parser_sparse_task_query_bounded, sparse_task_query_eligibility,
    ApplicationSparseQueryPage, BoundedGroups, ParserSparseQueryCandidate,
};
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tine_storage::sqlite::{
    PhysicalAliasDeclaration, PhysicalBlock, PhysicalEntityId, PhysicalGraphProjectionChange,
    PhysicalGraphProjectionDatabase, PhysicalGraphProjectionSourceRevision, PhysicalPage,
    PhysicalProperty, PhysicalReferencePosting, PhysicalReferenceTarget, PhysicalTask,
};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

type PageSnapshot = Arc<Vec<(PageEntry, Arc<Document>)>>;
type PageRevisions = Arc<HashMap<PathBuf, String>>;

// This is the parser-fact extractor identity, not an on-disk schema version.
// Bump it whenever unchanged source bytes must be lowered into new/different
// physical facts. The source-revision delta then rebuilds each page once even
// when tine-storage's disposable SQLite schema itself remains compatible.
const DIRECT_PROJECTION_FACTS_VERSION: u32 = 2;

#[cfg(test)]
static PHYSICAL_PAGE_LOWERINGS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
enum PageDelta {
    Replace(PageEntry, Arc<Document>, String),
    Delete(PageEntry),
}

#[derive(Default)]
struct PendingProjection {
    full: Option<(u64, PageSnapshot, PageRevisions)>,
    deltas: BTreeMap<String, (u64, PageDelta)>,
    latest_generation: u64,
    stop: bool,
}

struct ProjectionShared {
    path: PathBuf,
    pending: Mutex<PendingProjection>,
    changed: Condvar,
    ready: AtomicBool,
    ready_generation: AtomicU64,
    reader: Mutex<Option<PhysicalGraphProjectionDatabase>>,
    #[cfg(test)]
    indexed_reads: AtomicU64,
    #[cfg(test)]
    referenced_name_reads: AtomicU64,
    #[cfg(test)]
    fuzzy_candidate_reads: AtomicU64,
}

/// Direct Files' disposable parser-fact projection.
///
/// The foreground only publishes already-parsed `Arc<Document>` snapshots into
/// a coalescing page map. One worker owns SQLite, so an editor save never waits
/// for schema work, SQL, disk flushes, or a graph-sized rebuild. Read paths may
/// use the database only at the exact current cache generation.
pub(crate) struct DirectProjection {
    shared: Arc<ProjectionShared>,
}

impl DirectProjection {
    pub(crate) fn start(path: PathBuf) -> std::io::Result<Self> {
        let shared = Arc::new(ProjectionShared {
            path,
            pending: Mutex::new(PendingProjection::default()),
            changed: Condvar::new(),
            ready: AtomicBool::new(false),
            ready_generation: AtomicU64::new(0),
            reader: Mutex::new(None),
            #[cfg(test)]
            indexed_reads: AtomicU64::new(0),
            #[cfg(test)]
            referenced_name_reads: AtomicU64::new(0),
            #[cfg(test)]
            fuzzy_candidate_reads: AtomicU64::new(0),
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("tine-direct-projection".into())
            .spawn(move || projection_worker(worker))?;
        Ok(Self { shared })
    }

    pub(crate) fn enqueue_full(
        &self,
        generation: u64,
        pages: PageSnapshot,
        revisions: PageRevisions,
    ) {
        self.shared.ready.store(false, Ordering::Release);
        let mut pending = self.shared.pending.lock().unwrap();
        pending.full = Some((generation, pages, revisions));
        pending.deltas.clear();
        pending.latest_generation = generation;
        self.shared.changed.notify_one();
    }

    pub(crate) fn enqueue_replace(
        &self,
        generation: u64,
        entry: PageEntry,
        document: Arc<Document>,
        revision: String,
    ) {
        self.enqueue_delta(generation, PageDelta::Replace(entry, document, revision));
    }

    pub(crate) fn enqueue_delete(&self, generation: u64, entry: PageEntry) {
        self.enqueue_delta(generation, PageDelta::Delete(entry));
    }

    fn enqueue_delta(&self, generation: u64, delta: PageDelta) {
        self.shared.ready.store(false, Ordering::Release);
        let key = match &delta {
            PageDelta::Replace(entry, _, _) | PageDelta::Delete(entry) => entry.rel_path.clone(),
        };
        let mut pending = self.shared.pending.lock().unwrap();
        pending.deltas.insert(key, (generation, delta));
        pending.latest_generation = pending.latest_generation.max(generation);
        self.shared.changed.notify_one();
    }

    pub(crate) fn mark_stale(&self) {
        self.shared.ready.store(false, Ordering::Release);
    }

    pub(crate) fn sparse_task_query(
        &self,
        graph_root: &Path,
        journal_format: &crate::date::JournalFormat,
        cache_generation: u64,
        pages: &[(PageEntry, Arc<Document>)],
        query_src: &str,
        max_rows: usize,
        max_bytes: usize,
    ) -> Option<BoundedGroups> {
        let eligibility = sparse_task_query_eligibility(query_src)?;
        if !self.shared.ready.load(Ordering::Acquire)
            || self.shared.ready_generation.load(Ordering::Acquire) != cache_generation
        {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut by_block = BTreeMap::new();
        let uses_recency = eligibility.uses_recency;
        const BATCH: usize = 1024;
        for marker in eligibility.markers {
            let mut after = None;
            loop {
                let rows = read
                    .task_candidate_locators_after(&marker, after, BATCH)
                    .ok()?;
                let count = rows.len();
                for row in rows {
                    after = Some((row.page_id, row.block_id));
                    by_block.entry(row.block_id).or_insert(row);
                }
                if count < BATCH {
                    break;
                }
            }
        }
        if self.shared.ready_generation.load(Ordering::Acquire) != cache_generation
            || !self.shared.ready.load(Ordering::Acquire)
        {
            return None;
        }
        let mut page_recencies = HashMap::<String, i64>::new();
        struct CandidateMetadata {
            block_id: String,
            parent_identity: Option<String>,
            order: Vec<String>,
            page: ApplicationSparseQueryPage,
        }
        let metadata = by_block
            .into_values()
            .map(|row| {
                let recency = if uses_recency {
                    *page_recencies
                        .entry(row.page_path.clone())
                        .or_insert_with(|| {
                            page_recency(
                                graph_root,
                                &row.page_name,
                                &row.page_path,
                                row.page_text_kind,
                                journal_format,
                            )
                        })
                } else {
                    i64::MIN
                };
                CandidateMetadata {
                    block_id: Uuid::from_bytes(row.block_id).to_string(),
                    parent_identity: row.parent.map(|id| Uuid::from_bytes(id).to_string()),
                    order: vec![row.order, Uuid::from_bytes(row.block_id).to_string()],
                    page: ApplicationSparseQueryPage {
                        name: row.page_name,
                        path: row.page_path.clone(),
                        kind: page_kind_from_sql(row.page_text_kind)?,
                        is_org: Format::from_path(Path::new(&row.page_path)) == Format::Org,
                        recency,
                    },
                }
                .into()
            })
            .collect::<Option<Vec<_>>>()?;
        let documents = pages
            .iter()
            .map(|(entry, document)| (entry.rel_path.as_str(), document.as_ref()))
            .collect::<HashMap<_, _>>();
        let candidates = metadata
            .iter()
            .map(|candidate| {
                let document = documents.get(candidate.page.path.as_str())?;
                let block = block_at_order(&document.roots, &candidate.order[0])?;
                (block.uuid == candidate.block_id).then_some(ParserSparseQueryCandidate {
                    block,
                    identity: &candidate.block_id,
                    page: &candidate.page,
                    parent_identity: candidate.parent_identity.as_deref(),
                    dfs_order: &candidate.order,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let result =
            run_parser_sparse_task_query_bounded(&candidates, query_src, max_rows, max_bytes)
                .ok()?;
        let current = (self.shared.ready.load(Ordering::Acquire)
            && self.shared.ready_generation.load(Ordering::Acquire) == cache_generation)
            .then_some(result);
        #[cfg(test)]
        if current.is_some() {
            self.shared.indexed_reads.fetch_add(1, Ordering::Relaxed);
        }
        current
    }

    pub(crate) fn referenced_page_names(&self, cache_generation: u64) -> Option<Vec<String>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut after: Option<(String, String, String, [u8; 16])> = None;
        let mut names = std::collections::HashMap::<String, String>::new();
        const BATCH: usize = 1024;
        loop {
            let rows = read
                .navigation_reference_names_after(
                    after.as_ref().map(|(path, raw, normalized, id)| {
                        (path.as_str(), raw.as_str(), normalized.as_str(), id)
                    }),
                    BATCH,
                )
                .ok()?;
            let count = rows.len();
            for row in rows {
                after = Some((
                    row.owner_path,
                    row.raw_name.clone(),
                    row.normalized_name,
                    row.source_page_id,
                ));
                names
                    .entry(crate::refs::page_key(&row.raw_name))
                    .or_insert(row.raw_name);
            }
            if count < BATCH {
                break;
            }
        }
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut names = names.into_values().collect::<Vec<_>>();
        names.sort_by_key(|name| crate::refs::page_key(name));
        #[cfg(test)]
        self.shared
            .referenced_name_reads
            .fetch_add(1, Ordering::Relaxed);
        Some(names)
    }

    pub(crate) fn fuzzy_candidate_paths(
        &self,
        cache_generation: u64,
        normalized_needle: &str,
    ) -> Option<std::collections::HashSet<String>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut after = None;
        let mut paths = std::collections::HashSet::new();
        const BATCH: usize = 1024;
        loop {
            let rows = read
                .fuzzy_subsequence_candidate_pages_after(normalized_needle, after, BATCH)
                .ok()?;
            let count = rows.len();
            for row in rows {
                after = Some(row.page_id);
                paths.insert(row.path);
            }
            if count < BATCH {
                break;
            }
        }
        let current = self.ready_at(cache_generation).then_some(paths);
        #[cfg(test)]
        if current.is_some() {
            self.shared
                .fuzzy_candidate_reads
                .fetch_add(1, Ordering::Relaxed);
        }
        current
    }

    pub(crate) fn page_aliases_with_owners(
        &self,
        cache_generation: u64,
    ) -> Option<Vec<(String, String, String)>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut after: Option<(String, String, [u8; 16])> = None;
        let mut aliases = Vec::new();
        const BATCH: usize = 1024;
        loop {
            let rows = read
                .navigation_aliases_after(
                    after
                        .as_ref()
                        .map(|(path, alias, id)| (path.as_str(), alias.as_str(), id)),
                    BATCH,
                )
                .ok()?;
            let count = rows.len();
            for row in rows {
                after = Some((
                    row.owner_path.clone(),
                    row.normalized_alias.clone(),
                    row.source_page_id,
                ));
                aliases.push((row.normalized_alias, row.owner_name, row.owner_path));
            }
            if count < BATCH {
                break;
            }
        }
        self.ready_at(cache_generation).then_some(aliases)
    }

    pub(crate) fn real_page_names(
        &self,
        cache_generation: u64,
    ) -> Option<crate::query::RealPageNames> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut after: Option<(String, [u8; 16])> = None;
        let mut names = crate::query::RealPageNames::new();
        const BATCH: usize = 1024;
        loop {
            let rows = read
                .navigation_pages_after_with_header_validation(
                    after.as_ref().map(|(path, _)| path.as_str()),
                    after.as_ref().map(|(_, id)| id),
                    BATCH,
                    |_, _| Ok(()),
                )
                .ok()?;
            let count = rows.len();
            for row in rows {
                after = Some((row.path.clone(), row.page_id));
                let path = PathBuf::from(&row.path);
                match names.get_mut(&row.name_key) {
                    Some((winner_path, winner_name)) if path < *winner_path => {
                        *winner_path = path;
                        *winner_name = row.name;
                    }
                    Some(_) => {}
                    None => {
                        names.insert(row.name_key, (path, row.name));
                    }
                }
            }
            if count < BATCH {
                break;
            }
        }
        self.ready_at(cache_generation).then_some(names)
    }

    pub(crate) fn reference_candidate_paths(
        &self,
        cache_generation: u64,
        names_norm: &[String],
        kind: ReferenceKind,
    ) -> Option<std::collections::BTreeSet<PathBuf>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        if kind == ReferenceKind::Plain
            && names_norm
                .iter()
                .any(|name| !name.chars().any(char::is_alphanumeric))
        {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut page_ids = std::collections::BTreeSet::new();
        const BATCH: usize = 1024;
        for name in names_norm {
            match kind {
                ReferenceKind::Explicit => {
                    let mut after = None;
                    loop {
                        let rows = read
                            .page_referrer_candidates_after(name, after, BATCH)
                            .ok()?;
                        let count = rows.len();
                        for row in rows {
                            after = Some((row.source_page_id, row.source));
                            page_ids.insert(row.source_page_id);
                        }
                        if count < BATCH {
                            break;
                        }
                    }
                }
                ReferenceKind::Plain => {
                    let mut after = None;
                    loop {
                        let rows = read
                            .plain_text_candidate_pages_after(name, after, BATCH)
                            .ok()?;
                        let count = rows.len();
                        for row in rows {
                            after = Some(row.page_id);
                            page_ids.insert(row.page_id);
                        }
                        if count < BATCH {
                            break;
                        }
                    }
                }
            }
        }
        let mut paths = std::collections::BTreeSet::new();
        for page_id in page_ids {
            let page = read
                .page_with_header_validation(page_id, |_, _| Ok(()))
                .ok()??;
            paths.insert(PathBuf::from(page.path));
        }
        self.ready_at(cache_generation).then_some(paths)
    }

    /// Outer `None` means projection unavailable/stale and requires parser
    /// fallback. Inner `None` is an exact current-generation miss.
    pub(crate) fn block_page_hint(
        &self,
        cache_generation: u64,
        uuid: &str,
    ) -> Option<Option<String>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let uuid = Uuid::parse_str(uuid).ok()?.into_bytes();
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let block = match read.block(uuid).ok()? {
            Some(block) => Some(block),
            None => {
                let mut claimants = read.blocks_by_logseq_uuid(uuid, 2).ok()?;
                (claimants.len() == 1).then(|| claimants.pop().expect("one UUID claimant"))
            }
        };
        let page = match block {
            Some(block) => read
                .page_with_header_validation(block.page_id, |_, _| Ok(()))
                .ok()?
                .map(|page| page.name),
            None => None,
        };
        self.ready_at(cache_generation).then_some(page)
    }

    pub(crate) fn block_ref_counts(
        &self,
        cache_generation: u64,
    ) -> Option<std::collections::HashMap<String, usize>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut after = None;
        let mut counts = std::collections::HashMap::new();
        const BATCH: usize = 1024;
        loop {
            let rows = read.block_reference_counts_after(after, BATCH).ok()?;
            let count = rows.len();
            for row in rows {
                after = Some(row.raw_uuid_claim);
                counts.insert(
                    Uuid::from_bytes(row.raw_uuid_claim).to_string(),
                    usize::try_from(row.distinct_source_blocks).ok()?,
                );
            }
            if count < BATCH {
                break;
            }
        }
        self.ready_at(cache_generation).then_some(counts)
    }

    pub(crate) fn block_referrer_candidate_paths(
        &self,
        cache_generation: u64,
        uuid: &str,
    ) -> Option<std::collections::BTreeSet<PathBuf>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let uuid = Uuid::parse_str(uuid).ok()?.into_bytes();
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut after = None;
        let mut page_ids = std::collections::BTreeSet::new();
        const BATCH: usize = 1024;
        loop {
            let rows = read
                .block_referrer_candidates_after(uuid, after, BATCH)
                .ok()?;
            let count = rows.len();
            for row in rows {
                after = Some((row.source_page_id, row.source_block_id));
                page_ids.insert(row.source_page_id);
            }
            if count < BATCH {
                break;
            }
        }
        let mut paths = std::collections::BTreeSet::new();
        for page_id in page_ids {
            let page = read
                .page_with_header_validation(page_id, |_, _| Ok(()))
                .ok()??;
            paths.insert(PathBuf::from(page.path));
        }
        self.ready_at(cache_generation).then_some(paths)
    }

    pub(crate) fn ready_at(&self, generation: u64) -> bool {
        self.shared.ready.load(Ordering::Acquire)
            && self.shared.ready_generation.load(Ordering::Acquire) == generation
    }

    #[cfg(test)]
    pub(crate) fn indexed_reads(&self) -> u64 {
        self.shared.indexed_reads.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn referenced_name_reads(&self) -> u64 {
        self.shared.referenced_name_reads.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn fuzzy_candidate_reads(&self) -> u64 {
        self.shared.fuzzy_candidate_reads.load(Ordering::Relaxed)
    }
}

fn block_at_order<'a>(roots: &'a [DocBlock], order: &str) -> Option<&'a DocBlock> {
    let mut siblings = roots;
    let mut found = None;
    for component in order.split('/') {
        if component.len() != 8 {
            return None;
        }
        let index = usize::try_from(u32::from_str_radix(component, 16).ok()?).ok()?;
        let block = siblings.get(index)?;
        found = Some(block);
        siblings = &block.children;
    }
    found
}

impl Drop for DirectProjection {
    fn drop(&mut self) {
        let mut pending = self.shared.pending.lock().unwrap();
        pending.stop = true;
        self.shared.changed.notify_one();
    }
}

fn projection_worker(shared: Arc<ProjectionShared>) {
    let Some(parent) = shared.path.parent() else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        eprintln!("[tine] Direct Files SQLite projection disabled: create directory: {error}");
        return;
    }
    let lease_path = shared.path.with_extension("sqlite.writer.lock");
    let lease = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lease_path)
        .and_then(|file| {
            file.try_lock_exclusive()?;
            Ok(file)
        }) {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!(
                "[tine] Direct Files SQLite projection unavailable; another graph instance owns it or its lease cannot be opened: {error}"
            );
            return;
        }
    };
    let mut database = match open_projection_database(&shared.path) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("[tine] Direct Files SQLite projection disabled: {error}");
            return;
        }
    };
    // The lock file is app-private disposable state. Retain its exclusive lock
    // for the complete writer lifetime so another Graph instance cannot replace
    // this database's facts behind a locally-ready generation watermark.
    let _lease = lease;
    let mut requires_full_rebuild = false;
    loop {
        let (full, deltas, latest_generation) = {
            let mut pending = shared.pending.lock().unwrap();
            while pending.full.is_none() && pending.deltas.is_empty() && !pending.stop {
                pending = shared.changed.wait(pending).unwrap();
            }
            if pending.stop {
                return;
            }
            (
                pending.full.take(),
                std::mem::take(&mut pending.deltas),
                pending.latest_generation,
            )
        };
        let had_full = full.is_some();
        let applied = if requires_full_rebuild && !had_full {
            Err("a prior projection failure requires a complete parser snapshot".into())
        } else {
            apply_pending(&mut database, full, deltas)
        };
        if let Err(error) = applied {
            requires_full_rebuild = true;
            shared.ready.store(false, Ordering::Release);
            eprintln!(
                "[tine] Direct Files SQLite projection is stale; using parser fallback: {error}"
            );
            continue;
        }
        if had_full {
            requires_full_rebuild = false;
        }
        let pending = shared.pending.lock().unwrap();
        if pending.full.is_none()
            && pending.deltas.is_empty()
            && pending.latest_generation == latest_generation
        {
            shared
                .ready_generation
                .store(latest_generation, Ordering::Release);
            shared.ready.store(true, Ordering::Release);
        }
    }
}

fn open_projection_database(
    path: &Path,
) -> Result<PhysicalGraphProjectionDatabase, tine_storage::sqlite::MaterializationError> {
    let database = PhysicalGraphProjectionDatabase::open_writable(path)?;
    if database.validate_schema().is_ok() && database.quick_check().is_ok() {
        return Ok(database);
    }
    if database.initialize_schema().is_ok()
        && database.validate_schema().is_ok()
        && database.quick_check().is_ok()
    {
        return Ok(database);
    }
    drop(database);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let database = PhysicalGraphProjectionDatabase::open_writable(path)?;
    database.initialize_schema()?;
    database.validate_schema()?;
    Ok(database)
}

fn apply_pending(
    database: &mut PhysicalGraphProjectionDatabase,
    full: Option<(u64, PageSnapshot, PageRevisions)>,
    deltas: BTreeMap<String, (u64, PageDelta)>,
) -> Result<(), String> {
    if let Some((_, pages, revisions)) = full {
        let sources = pages
            .iter()
            .map(|(entry, _)| {
                Ok(PhysicalGraphProjectionSourceRevision {
                    page_id: page_id(&entry.rel_path),
                    revision: projection_source_revision(revisions.get(&entry.path).ok_or_else(
                        || {
                            format!(
                                "parsed page has no exact source revision: {}",
                                entry.rel_path
                            )
                        },
                    )?),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let source_delta = database
            .source_delta(&sources)
            .map_err(|error| error.to_string())?;
        let replacements_needed = source_delta
            .replacements
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let lowered = pages
            .iter()
            .filter(|(entry, _)| replacements_needed.contains(&page_id(&entry.rel_path)))
            .map(|(entry, document)| physical_page(entry, document))
            .collect::<Result<Vec<_>, _>>()?;
        let mut replacements = Vec::with_capacity(lowered.len());
        let mut reference_postings = Vec::new();
        let mut aliases = Vec::new();
        for (page, mut postings, mut page_aliases) in lowered {
            replacements.push(page);
            reference_postings.append(&mut postings);
            aliases.append(&mut page_aliases);
        }
        let replacement_sources = sources
            .into_iter()
            .filter(|source| replacements_needed.contains(&source.page_id))
            .collect::<Vec<_>>();
        database
            .apply_with_source_revisions_and_aliases(
                &PhysicalGraphProjectionChange {
                    replacements,
                    deletions: source_delta.deletions,
                    reference_postings,
                },
                &replacement_sources,
                &aliases,
            )
            .map_err(|error| error.to_string())?;
    }
    if !deltas.is_empty() {
        let mut replacements = Vec::new();
        let mut reference_postings = Vec::new();
        let mut aliases = Vec::new();
        let mut replacement_sources = Vec::new();
        let mut deletions = Vec::new();
        for (_, (_, delta)) in deltas {
            match delta {
                PageDelta::Replace(entry, document, revision) => {
                    replacement_sources.push(PhysicalGraphProjectionSourceRevision {
                        page_id: page_id(&entry.rel_path),
                        revision: projection_source_revision(&revision),
                    });
                    let (page, mut postings, mut page_aliases) = physical_page(&entry, &document)?;
                    replacements.push(page);
                    reference_postings.append(&mut postings);
                    aliases.append(&mut page_aliases);
                }
                PageDelta::Delete(entry) => deletions.push(page_id(&entry.rel_path)),
            }
        }
        database
            .apply_with_source_revisions_and_aliases(
                &PhysicalGraphProjectionChange {
                    replacements,
                    deletions,
                    reference_postings,
                },
                &replacement_sources,
                &aliases,
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn projection_source_revision(content_revision: &str) -> String {
    format!("direct-facts-v{DIRECT_PROJECTION_FACTS_VERSION}:{content_revision}")
}

fn physical_page(
    entry: &PageEntry,
    document: &Document,
) -> Result<
    (
        PhysicalPage,
        Vec<PhysicalReferencePosting>,
        Vec<PhysicalAliasDeclaration>,
    ),
    String,
> {
    #[cfg(test)]
    PHYSICAL_PAGE_LOWERINGS.fetch_add(1, Ordering::Relaxed);
    let id = page_id(&entry.rel_path);
    let is_org = Format::from_path(Path::new(&entry.rel_path)) == Format::Org;
    let (preamble_search, properties, tags) = document
        .pre_block
        .as_deref()
        .map(|raw| facets(raw, is_org))
        .unwrap_or_default();
    let searchable_text = if preamble_search.is_empty() {
        entry.name.clone()
    } else {
        format!("{} {preamble_search}", entry.name)
    };
    let mut blocks = Vec::new();
    let mut reference_postings = Vec::new();
    let aliases = crate::query::document_aliases(document)
        .into_iter()
        .enumerate()
        .map(|(ordinal, alias)| {
            Ok(PhysicalAliasDeclaration {
                source_page_id: id,
                source_entity: PhysicalEntityId::Page(id),
                source_locator: b"page-alias".to_vec(),
                ordinal: u32::try_from(ordinal)
                    .map_err(|_| "one page exceeds u32::MAX aliases".to_string())?,
                raw_alias: alias.clone(),
                normalized_alias: alias,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if let Some(preamble) = document.pre_block.as_deref() {
        append_reference_postings(
            &mut reference_postings,
            id,
            PhysicalEntityId::Page(id),
            b"preamble",
            std::iter::empty(),
            crate::doc::property_reference_page_names(preamble).into_iter(),
        )?;
    }
    lower_blocks(
        &document.roots,
        id,
        None,
        &mut Vec::new(),
        &mut blocks,
        &mut reference_postings,
    )?;
    Ok((
        PhysicalPage {
            page_id: id,
            home_document_id: id,
            name: entry.name.clone(),
            name_key: crate::refs::page_key(&entry.name),
            path: entry.rel_path.clone(),
            text_kind: page_kind_to_sql(entry.kind),
            preamble: document.pre_block.clone(),
            normalized_searchable_text: searchable_text.to_lowercase().nfc().collect(),
            searchable_text,
            references: Vec::new(),
            properties,
            tags,
            blocks,
        },
        reference_postings,
        aliases,
    ))
}

fn lower_blocks(
    source: &[DocBlock],
    page_id: [u8; 16],
    parent: Option<[u8; 16]>,
    structural_path: &mut Vec<u32>,
    out: &mut Vec<PhysicalBlock>,
    reference_postings: &mut Vec<PhysicalReferencePosting>,
) -> Result<(), String> {
    for (position, block) in source.iter().enumerate() {
        let position = u32::try_from(position)
            .map_err(|_| "page has more than u32::MAX sibling blocks".to_string())?;
        structural_path.push(position);
        let block_id = Uuid::parse_str(&block.uuid)
            .map_err(|_| {
                format!(
                    "block has no assigned runtime UUID in projection: {}",
                    block.uuid
                )
            })?
            .into_bytes();
        let projection = block.projection();
        let order = structural_path
            .iter()
            .map(|part| format!("{part:08x}"))
            .collect::<Vec<_>>()
            .join("/");
        append_reference_postings(
            reference_postings,
            page_id,
            PhysicalEntityId::Block(block_id),
            order.as_bytes(),
            projection.refs_page.iter().cloned(),
            crate::doc::property_reference_page_names(&block.raw).into_iter(),
        )?;
        for raw_claim in &projection.block_refs {
            let Ok(raw_claim) = Uuid::parse_str(raw_claim) else {
                continue;
            };
            reference_postings.push(PhysicalReferencePosting {
                source_page_id: page_id,
                source_entity: PhysicalEntityId::Block(block_id),
                source_locator: order.as_bytes().to_vec(),
                ordinal: u32::try_from(reference_postings.len())
                    .map_err(|_| "one page exceeds u32::MAX reference postings".to_string())?,
                kind: 6,
                target: PhysicalReferenceTarget::ExternalUuid {
                    raw_claim: raw_claim.into_bytes(),
                    resolved_block_id: None,
                },
            });
        }
        let searchable_text = projection
            .visible
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let properties = projection
            .properties
            .iter()
            .map(|(name, value)| PhysicalProperty {
                name: name.clone(),
                normalized_name: property_key_norm(name),
                value: value.clone(),
            })
            .collect();
        let logseq_uuid = block
            .property("id")
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
            .map(Uuid::into_bytes);
        out.push(PhysicalBlock {
            block_id,
            home_document_id: page_id,
            parent,
            order,
            content: block.raw.clone(),
            normalized_searchable_text: searchable_text.to_lowercase().nfc().collect(),
            searchable_text,
            heading_level: projection.heading_level,
            collapsed: block.collapsed(),
            logseq_uuid,
            logseq_identity_origin: logseq_uuid.map(|_| 0),
            references: Vec::new(),
            properties,
            tags: projection.tags.clone(),
            task: projection.marker.as_ref().map(|marker| PhysicalTask {
                marker: marker.to_ascii_uppercase(),
                priority: projection.priority.clone(),
                scheduled: projection.scheduled.clone(),
                deadline: projection.deadline.clone(),
            }),
        });
        lower_blocks(
            &block.children,
            page_id,
            Some(block_id),
            structural_path,
            out,
            reference_postings,
        )?;
        structural_path.pop();
    }
    Ok(())
}

fn append_reference_postings(
    out: &mut Vec<PhysicalReferencePosting>,
    page_id: [u8; 16],
    source: PhysicalEntityId,
    source_locator: &[u8],
    inline_names: impl IntoIterator<Item = String>,
    property_names: impl IntoIterator<Item = String>,
) -> Result<(), String> {
    let mut ordinal = 0_u32;
    for (kind, names) in [
        (0_i64, inline_names.into_iter().collect::<Vec<_>>()),
        (3_i64, property_names.into_iter().collect::<Vec<_>>()),
    ] {
        for raw_name in names {
            out.push(PhysicalReferencePosting {
                source_page_id: page_id,
                source_entity: source,
                source_locator: source_locator.to_vec(),
                ordinal,
                kind,
                target: PhysicalReferenceTarget::PageName {
                    normalized_name: crate::refs::page_key(&raw_name),
                    raw_name,
                    resolved_page_id: None,
                },
            });
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| "one reference source exceeds u32::MAX postings".to_string())?;
        }
    }
    Ok(())
}

fn facets(raw: &str, is_org: bool) -> (String, Vec<PhysicalProperty>, Vec<String>) {
    let mut block = DocBlock::new(raw);
    block.is_org = is_org;
    let searchable = block
        .visible_text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let properties = block
        .projection()
        .properties
        .iter()
        .map(|(name, value)| PhysicalProperty {
            name: name.clone(),
            normalized_name: property_key_norm(name),
            value: value.clone(),
        })
        .collect();
    (searchable, properties, block.projection().tags.clone())
}

pub(crate) fn page_id(relative_path: &str) -> [u8; 16] {
    let mut digest = Sha256::new();
    digest.update(b"tine-direct-page-v1\0");
    digest.update(relative_path.as_bytes());
    let bytes = digest.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&bytes[..16]);
    id
}

fn page_kind_to_sql(kind: PageKind) -> i64 {
    match kind {
        PageKind::Page => 0,
        PageKind::Journal => 1,
    }
}

fn page_kind_from_sql(kind: i64) -> Option<PageKind> {
    match kind {
        0 => Some(PageKind::Page),
        1 => Some(PageKind::Journal),
        _ => None,
    }
}

fn page_recency(
    root: &Path,
    name: &str,
    relative_path: &str,
    kind: i64,
    journal_format: &crate::date::JournalFormat,
) -> i64 {
    journal_format.page_recency_secs(kind == 1, name, &root.join(relative_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Graph;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static PROJECTION_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tine-direct-projection-{tag}-{}", Uuid::new_v4()))
    }

    fn reset_lowerings() {
        PHYSICAL_PAGE_LOWERINGS.store(0, Ordering::Relaxed);
    }

    fn lowerings() -> u64 {
        PHYSICAL_PAGE_LOWERINGS.load(Ordering::Relaxed)
    }

    fn signature(groups: &[crate::model::RefGroup]) -> Vec<(String, Vec<(String, String)>)> {
        groups
            .iter()
            .map(|group| {
                (
                    group.page.clone(),
                    group
                        .blocks
                        .iter()
                        .map(|block| (block.id.clone(), block.raw.clone()))
                        .collect(),
                )
            })
            .collect()
    }

    fn wait_ready(graph: &Graph) {
        let started = Instant::now();
        while !graph.direct_projection_ready_test() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "Direct Files projection did not converge"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn direct_projection_matches_parser_tasks_and_tracks_replace_delete() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("task-parity");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        std::fs::write(
            root.join("pages/tasks.md"),
            "- TODO [#A] parent\n\t- TODO child\n- TODO other\n  SCHEDULED: <2026-08-13 Thu>\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/org.org"), "* TODO [#B] org task\n").unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        for query in [
            "(task TODO)",
            "(and (task TODO) (priority A))",
            "(and (task TODO) (scheduled))",
            "(and (task TODO) (sort-by priority desc))",
        ] {
            let oracle = crate::query::run_query_bounded(&graph, query, 100, 1_000_000);
            let indexed = graph.run_query_bounded(query, 100, 1_000_000);
            assert_eq!(
                signature(&indexed.groups),
                signature(&oracle.groups),
                "{query}"
            );
            assert_eq!(
                (indexed.total, indexed.exceeded),
                (oracle.total, oracle.exceeded)
            );
        }
        assert!(graph.direct_projection_indexed_reads_test() >= 4);
        let indexed_reads = graph.direct_projection_indexed_reads_test();
        let repeated = graph.run_query_bounded("(task TODO)", 100, 1_000_000);
        assert_eq!(
            signature(&repeated.groups),
            signature(
                &crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000).groups
            )
        );
        assert_eq!(
            graph.direct_projection_indexed_reads_test(),
            indexed_reads,
            "the generation-keyed presentation memo must avoid repeated SQL/parser work"
        );

        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "tasks")
            .unwrap();
        let mut page = graph.load_page(&entry).unwrap();
        let baseline = page.rev.clone();
        page.blocks[0].raw = "DONE [#A] parent".into();
        graph.save_page(&page, baseline.as_deref()).unwrap();
        wait_ready(&graph);
        for query in ["(task TODO)", "(task DONE)"] {
            let oracle = crate::query::run_query_bounded(&graph, query, 100, 1_000_000);
            let indexed = graph.run_query_bounded(query, 100, 1_000_000);
            assert_eq!(
                signature(&indexed.groups),
                signature(&oracle.groups),
                "{query}"
            );
        }

        graph.delete_page("org", PageKind::Page).unwrap();
        wait_ready(&graph);
        let oracle = crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000);
        let indexed = graph.run_query_bounded("(task TODO)", 100, 1_000_000);
        assert_eq!(signature(&indexed.groups), signature(&oracle.groups));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_projection_matches_fuzzy_search_and_virtual_reference_names() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("search-reference-parity");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/one.md"),
            "tags:: Page Tag, [[Property Page]]\nalias:: Alias Page\nquoted:: untouched\n\n- Characteristically useful [[Inline Page]]\n  aliases:: #Block Alias\n- c% literal\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/two.md"), "- unrelated content\n").unwrap();
        let graph = Graph::open(&root);
        graph.warm_cache();
        let oracle = crate::query::search(&graph, "cly", 20);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        wait_ready(&graph);

        let candidate_pages = graph
            .direct_projection_fuzzy_candidate_pages("cly")
            .unwrap();
        assert_eq!(candidate_pages.len(), 1);
        assert_eq!(candidate_pages[0].0.rel_path, "pages/one.md");
        assert_eq!(signature(&graph.search("cly", 20)), signature(&oracle));
        assert!(graph.direct_projection_fuzzy_candidate_reads_test() > 0);
        let names = graph
            .referenced_page_names()
            .into_iter()
            .map(|name| crate::refs::page_key(&name))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            [
                "page tag",
                "property page",
                "alias page",
                "inline page",
                "block",
                "block alias",
            ]
            .into_iter()
            .map(str::to_string)
            .collect()
        );
        assert!(graph.direct_projection_referenced_name_reads_test() > 0);

        let fuzzy_reads = graph.direct_projection_fuzzy_candidate_reads_test();
        let name_reads = graph.direct_projection_referenced_name_reads_test();
        graph.direct_projection_mark_stale_test();
        assert_eq!(signature(&graph.search("cly", 20)), signature(&oracle));
        assert_eq!(
            graph
                .referenced_page_names()
                .into_iter()
                .map(|name| crate::refs::page_key(&name))
                .collect::<std::collections::BTreeSet<_>>(),
            names
        );
        assert_eq!(
            graph.direct_projection_fuzzy_candidate_reads_test(),
            fuzzy_reads,
            "a stale generation must use the parser fallback"
        );
        assert_eq!(
            graph.direct_projection_referenced_name_reads_test(),
            name_reads,
            "a stale generation must not read reference names from SQLite"
        );

        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "one")
            .unwrap();
        let mut page = graph.load_page(&entry).unwrap();
        let baseline = page.rev.clone();
        page.blocks[0].raw = "Nothing matching [[Replacement Page]]".into();
        graph.save_page(&page, baseline.as_deref()).unwrap();
        wait_ready(&graph);
        assert!(graph.search("cly", 20).is_empty());
        let names = graph
            .referenced_page_names()
            .into_iter()
            .map(|name| crate::refs::page_key(&name))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(names.contains("replacement page"));
        assert!(!names.contains("inline page"));

        std::fs::write(
            root.join("pages/one.md"),
            "tags:: External Tag\n\n- Externally changed fuzzy [[External Page]]\n",
        )
        .unwrap();
        graph.sync_file_checked(&root.join("pages/one.md")).unwrap();
        wait_ready(&graph);
        assert!(!graph.search("ecf", 20).is_empty());
        let names = graph
            .referenced_page_names()
            .into_iter()
            .map(|name| crate::refs::page_key(&name))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(names.contains("external tag"));
        assert!(names.contains("external page"));
        assert!(!names.contains("replacement page"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_projection_matches_parser_reference_family_and_stale_fallback() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("reference-family-parity");
        let target_id = "11111111-2222-4333-8444-555555555555";
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/target.md"),
            format!("alias:: Alias Target\n\n- target\n  id:: {target_id}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("pages/referrer.md"),
            format!(
                "- [[Alias Target]] and plain Alias Target and (({target_id})) (({target_id}))\n- another (({target_id}))\n"
            ),
        )
        .unwrap();
        std::fs::write(root.join("pages/unrelated.md"), "- unrelated\n").unwrap();

        let graph = Graph::open(&root);
        graph.warm_cache();
        let parser_aliases = crate::query::page_aliases_with_owners(&graph);
        let parser_backlinks = crate::query::backlinks(&graph, "target");
        let parser_unlinked = crate::query::unlinked_refs(&graph, "target");
        let parser_referrers = crate::query::block_referrers(&graph, target_id);
        let parser_resolved = crate::query::resolve_block(&graph, target_id);
        let parser_counts = graph.block_ref_counts().unwrap();

        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        wait_ready(&graph);

        assert_eq!(graph.page_aliases_with_owners(), parser_aliases);
        let explicit_candidates = graph.reference_candidate_pages(
            &[
                crate::refs::page_key("target"),
                crate::refs::page_key("Alias Target"),
            ],
            ReferenceKind::Explicit,
        );
        assert!(explicit_candidates.indexed);
        assert!(explicit_candidates.pages.len() < explicit_candidates.full_page_count);
        assert_eq!(
            signature(&crate::query::backlinks(&graph, "target")),
            signature(&parser_backlinks)
        );
        assert_eq!(
            signature(&crate::query::unlinked_refs(&graph, "target")),
            signature(&parser_unlinked)
        );
        assert_eq!(
            signature(&crate::query::block_referrers(&graph, target_id)),
            signature(&parser_referrers)
        );
        assert_eq!(
            crate::query::resolve_block(&graph, target_id)
                .as_ref()
                .map(|group| signature(std::slice::from_ref(group))),
            parser_resolved
                .as_ref()
                .map(|group| signature(std::slice::from_ref(group)))
        );
        assert_eq!(
            graph.block_ref_counts().unwrap().as_ref(),
            parser_counts.as_ref()
        );
        assert_eq!(graph.block_ref_counts().unwrap().get(target_id), Some(&2));

        let custom_path = root.join("pages/custom.md");
        std::fs::write(&custom_path, "- custom identity\n  id:: not-a-uuid\n").unwrap();
        assert!(graph.sync_file(&custom_path).is_some());
        wait_ready(&graph);
        assert_eq!(
            crate::query::resolve_block(&graph, "not-a-uuid")
                .and_then(|group| group.blocks.into_iter().next())
                .map(|block| block.raw),
            Some("custom identity\nid:: not-a-uuid".to_string())
        );

        graph.direct_projection_mark_stale_test();
        assert_eq!(graph.page_aliases_with_owners(), parser_aliases);
        assert_eq!(
            signature(&crate::query::backlinks(&graph, "target")),
            signature(&parser_backlinks)
        );
        assert_eq!(
            signature(&crate::query::block_referrers(&graph, target_id)),
            signature(&parser_referrers)
        );
        assert_eq!(
            graph.block_ref_counts().unwrap().as_ref(),
            parser_counts.as_ref()
        );

        let target_path = root.join("pages/target.md");
        std::fs::write(
            &target_path,
            format!("alias:: Changed Alias\n\n- target\n  id:: {target_id}\n"),
        )
        .unwrap();
        assert!(graph.sync_file(&target_path).is_some());
        wait_ready(&graph);
        let changed_aliases = graph.page_aliases_with_owners();
        assert!(changed_aliases
            .iter()
            .any(|(alias, owner, _)| alias == "changed alias" && owner == "target"));
        assert!(!changed_aliases
            .iter()
            .any(|(alias, _, _)| alias == "alias target"));

        graph.delete_page("target", PageKind::Page).unwrap();
        wait_ready(&graph);
        assert!(!graph
            .page_aliases_with_owners()
            .iter()
            .any(|(alias, _, _)| alias == "changed alias"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_projection_preserves_external_uuid_ambiguity_for_parser_resolution() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("external-uuid-ambiguity");
        let target_id = "11111111-2222-4333-8444-555555555555";
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/alpha.md"),
            format!("- alpha claimant\n  id:: {target_id}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("pages/beta.md"),
            format!("- beta claimant\n  id:: {target_id}\n"),
        )
        .unwrap();

        let graph = Graph::open(&root);
        graph.warm_cache();
        let parser_resolution = crate::query::resolve_block(&graph, target_id)
            .map(|group| signature(std::slice::from_ref(&group)));
        let projection_path = root.join("private/projection.sqlite");
        graph
            .attach_direct_projection(projection_path.clone())
            .unwrap();
        wait_ready(&graph);

        let database = PhysicalGraphProjectionDatabase::open_read_only(&projection_path).unwrap();
        let claim = Uuid::parse_str(target_id).unwrap().into_bytes();
        assert_eq!(
            database
                .read()
                .blocks_by_logseq_uuid(claim, 2)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            crate::query::resolve_block(&graph, target_id)
                .map(|group| signature(std::slice::from_ref(&group))),
            parser_resolution,
            "SQLite must not choose one external UUID owner from an ambiguous graph"
        );
        drop(database);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reference_family_has_no_second_in_memory_semantic_index() {
        let model = include_str!("model.rs");
        for removed in [
            "alias_cache",
            "reference_candidate_index",
            "block_ref_count_cache",
            "block_index: RwLock",
        ] {
            assert!(
                !model.contains(removed),
                "Direct Files reference family reintroduced {removed} beside SQLite"
            );
        }
    }

    #[test]
    fn direct_projection_fuzzy_candidates_preserve_parser_corpus_semantics() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("search-corpus-parity");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/search.md"),
            "- Characteristically useful\n  - descendant Needle\n- Café and cafe\u{301}\n- 100% under_score back\\slash\n- MixedCASE\n- x a y b z\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pages/other.md"),
            "- Another characteristically useful result\n",
        )
        .unwrap();
        let cases = [
            ("", 20),
            ("   ", 20),
            ("cly", 20),
            ("needle", 20),
            ("CAFÉ", 20),
            ("cafe\u{301}", 20),
            ("%", 20),
            ("_", 20),
            ("\\", 20),
            ("mixedcase", 20),
            ("xyz", 20),
            ("cly", 1),
        ];
        let oracle_graph = Graph::open(&root);
        oracle_graph.warm_cache();
        let oracle = cases
            .iter()
            .map(|(query, limit)| signature(&crate::query::search(&oracle_graph, query, *limit)))
            .collect::<Vec<_>>();
        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        assert!(
            graph.warm_cache_cancellable(|| false),
            "corpus cache failed to warm: {:?}",
            graph.page_index_failures()
        );
        wait_ready(&graph);
        for ((query, limit), expected) in cases.into_iter().zip(oracle) {
            assert_eq!(
                signature(&graph.search(query, limit)),
                expected,
                "{query:?}"
            );
        }
        let cancellation_checks = std::cell::Cell::new(0);
        assert!(crate::query::search_cancellable(&graph, "cly", 20, || {
            cancellation_checks.set(cancellation_checks.get() + 1);
            cancellation_checks.get() > 1
        })
        .is_empty());

        graph.rename_page("search", "renamed search").unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        assert_eq!(
            signature(&graph.search("needle", 20)),
            signature(&crate::query::search(&graph, "needle", 20))
        );
        graph.delete_page("renamed search", PageKind::Page).unwrap();
        wait_ready(&graph);
        assert!(graph.search("needle", 20).is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unavailable_projection_keeps_direct_files_query_semantics() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("fallback");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/tasks.md"),
            "- TODO Characteristically readable [[Inline Only]]\n  alias:: #Alias Only\n",
        )
        .unwrap();
        let blocked_parent = root.join("not-a-directory");
        std::fs::write(&blocked_parent, b"ordinary file").unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(blocked_parent.join("projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        std::thread::sleep(Duration::from_millis(30));
        let oracle = crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000);
        let fallback = graph.run_query_bounded("(task TODO)", 100, 1_000_000);
        assert_eq!(signature(&fallback.groups), signature(&oracle.groups));
        assert_eq!(graph.direct_projection_indexed_reads_test(), 0);
        assert_eq!(
            signature(&graph.search("cly", 20)),
            signature(&crate::query::search(&graph, "cly", 20))
        );
        let names = graph
            .referenced_page_names()
            .into_iter()
            .map(|name| crate::refs::page_key(&name))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(names.contains("inline only"));
        assert!(names.contains("alias only"));
        assert_eq!(graph.direct_projection_fuzzy_candidate_reads_test(), 0);
        assert_eq!(graph.direct_projection_referenced_name_reads_test(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_graph_instance_cannot_replace_ready_projection_facts() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("single-writer");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/tasks.md"), "- TODO one\n").unwrap();
        let database = scratch("single-writer-db").join("projection.sqlite");

        let owner = Graph::open(&root);
        owner.attach_direct_projection(database.clone()).unwrap();
        owner.warm_cache();
        wait_ready(&owner);

        let fallback = Graph::open(&root);
        fallback.attach_direct_projection(database.clone()).unwrap();
        fallback.warm_cache();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !fallback.direct_projection_ready_test(),
            "a second graph instance must not publish into the first instance's ready database"
        );
        let oracle = crate::query::run_query_bounded(&fallback, "(task TODO)", 100, 1_000_000);
        let actual = fallback.run_query_bounded("(task TODO)", 100, 1_000_000);
        assert_eq!(signature(&actual.groups), signature(&oracle.groups));
        assert_eq!(fallback.direct_projection_indexed_reads_test(), 0);

        let owner_oracle = crate::query::run_query_bounded(&owner, "(task TODO)", 100, 1_000_000);
        let owner_actual = owner.run_query_bounded("(task TODO)", 100, 1_000_000);
        assert_eq!(
            signature(&owner_actual.groups),
            signature(&owner_oracle.groups)
        );
        assert!(owner.direct_projection_indexed_reads_test() > 0);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    fn clean_reopen_reuses_sqlite_and_external_edit_relowers_only_one_page() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("reopen-revisions");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/one.md"), "- TODO one\n").unwrap();
        std::fs::write(root.join("pages/two.md"), "- DONE two\n").unwrap();
        let database = scratch("reopen-revisions-db").join("projection.sqlite");

        reset_lowerings();
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
            assert_eq!(lowerings(), 2);
        }
        std::thread::sleep(Duration::from_millis(20));

        reset_lowerings();
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
            assert_eq!(lowerings(), 0, "unchanged pages must stay inside SQLite");
        }
        std::thread::sleep(Duration::from_millis(20));

        std::fs::write(root.join("pages/one.md"), "- TODO one changed\n").unwrap();
        reset_lowerings();
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
            assert_eq!(
                lowerings(),
                1,
                "one changed page must produce one SQL delta"
            );
            assert_eq!(
                signature(
                    &graph
                        .run_query_bounded("(task TODO)", 100, 1_000_000)
                        .groups
                ),
                signature(
                    &crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000).groups
                )
            );
        }

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    fn extractor_version_participates_in_disposable_source_revision() {
        let source = "sha256:unchanged-source";
        let projected = projection_source_revision(source);
        assert_eq!(projected, "direct-facts-v2:sha256:unchanged-source");
        assert_ne!(projected, source);
    }

    #[test]
    fn storage_contract_names_the_generation_bound_cutover() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        assert!(contract.contains("direct-files-projections/<canonical-graph-path-digest>.sqlite"));
        assert!(contract.contains("sparse_task_query_eligibility"));
        assert!(contract.contains("literal fuzzy-search candidate"));
        assert!(contract.contains("referenced-page\ninventory"));
        assert!(contract.contains("retains no separate semantic memo"));
        assert!(contract.contains("exact current parser-cache\ngeneration"));
        assert!(contract.contains("Direct fact-extractor version"));
        assert!(contract.contains("app-private graph-fact projection contains no managed state"));
        assert!(contract.contains("clean\nreopen lowers none"));
        assert!(
            contract.contains("memo of already-shaped frontend result DTOs remains Tine-native")
        );
        assert!(contract.contains("grants no\n   authority"));
    }

    #[test]
    #[ignore = "manual storage packet receipt; set TINE_DIRECT_PROJECTION_CORPUS"]
    fn real_corpus_projection_converges_and_matches_task_query() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = PathBuf::from(
            std::env::var("TINE_DIRECT_PROJECTION_CORPUS")
                .expect("TINE_DIRECT_PROJECTION_CORPUS is required"),
        );
        let database = scratch("real-corpus").join("projection.sqlite");
        let oracle_graph = Graph::open(&root);
        oracle_graph.warm_cache();
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        let started = Instant::now();
        graph.warm_cache();
        let warm = started.elapsed();
        wait_ready(&graph);
        let converged = started.elapsed();
        let oracle_started = Instant::now();
        let oracle =
            crate::query::run_query_bounded(&oracle_graph, "(task TODO)", 20_000, 32 << 20);
        let oracle_elapsed = oracle_started.elapsed();
        let query_started = Instant::now();
        let indexed = graph.run_query_bounded("(task TODO)", 20_000, 32 << 20);
        let indexed_elapsed = query_started.elapsed();
        assert_eq!(signature(&indexed.groups), signature(&oracle.groups));
        let indexed_reads = graph.direct_projection_indexed_reads_test();
        let memo_started = Instant::now();
        let repeated = graph.run_query_bounded("(task TODO)", 20_000, 32 << 20);
        let memo_elapsed = memo_started.elapsed();
        assert_eq!(signature(&repeated.groups), signature(&oracle.groups));
        assert_eq!(graph.direct_projection_indexed_reads_test(), indexed_reads);
        let mut fuzzy_indexed = Duration::ZERO;
        let mut fuzzy_oracle = Duration::ZERO;
        for value in ["a", "todo", "http", "2026", "%", "_", "é"] {
            let indexed_started = Instant::now();
            let indexed_search = graph.search(value, 5_000);
            fuzzy_indexed += indexed_started.elapsed();
            let oracle_started = Instant::now();
            let oracle_search = crate::query::search(&oracle_graph, value, 5_000);
            fuzzy_oracle += oracle_started.elapsed();
            assert_eq!(
                signature(&indexed_search),
                signature(&oracle_search),
                "real-corpus fuzzy search diverged for a bounded probe"
            );
        }
        eprintln!(
            "direct projection fuzzy receipt: indexed_total_ms={} oracle_total_ms={}",
            fuzzy_indexed.as_millis(),
            fuzzy_oracle.as_millis(),
        );
        let normalize_names = |mut names: Vec<String>| {
            names.sort_by_key(|name| crate::refs::page_key(name));
            names
        };
        assert_eq!(
            normalize_names(graph.referenced_page_names()),
            normalize_names(oracle_graph.referenced_page_names()),
            "real-corpus referenced-page inventory diverged"
        );
        assert!(graph.direct_projection_fuzzy_candidate_reads_test() > 0);
        assert!(graph.direct_projection_referenced_name_reads_test() > 0);
        let task_candidates = PhysicalGraphProjectionDatabase::open_read_only(&database)
            .unwrap()
            .read()
            .task_candidate_blocks_after("TODO", None, 10_000)
            .unwrap()
            .len();
        eprintln!(
            "direct projection receipt: warm_ms={} projection_total_ms={} oracle_query_us={} indexed_query_us={} repeated_query_us={} pages={} task_candidates={}",
            warm.as_millis(),
            converged.as_millis(),
            oracle_elapsed.as_micros(),
            indexed_elapsed.as_micros(),
            memo_elapsed.as_micros(),
            graph.list_pages().len(),
            task_candidates,
        );
    }

    #[test]
    #[ignore = "manual storage packet receipt; set TINE_DIRECT_PROJECTION_CORPUS"]
    fn real_corpus_clean_reopen_reuses_projected_pages() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = PathBuf::from(
            std::env::var("TINE_DIRECT_PROJECTION_CORPUS")
                .expect("TINE_DIRECT_PROJECTION_CORPUS is required"),
        );
        let database = scratch("real-corpus-reopen").join("projection.sqlite");
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        std::thread::sleep(Duration::from_millis(20));

        reset_lowerings();
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        let started = Instant::now();
        graph.warm_cache();
        let warm = started.elapsed();
        wait_ready(&graph);
        let converged = started.elapsed();
        let query_started = Instant::now();
        let indexed = graph.run_query_bounded("(task TODO)", 20_000, 32 << 20);
        let indexed_elapsed = query_started.elapsed();
        let oracle = crate::query::run_query_bounded(&graph, "(task TODO)", 20_000, 32 << 20);
        assert_eq!(signature(&indexed.groups), signature(&oracle.groups));
        assert_eq!(
            lowerings(),
            0,
            "clean reopen must not lower unchanged pages"
        );
        eprintln!(
            "direct projection clean-reopen receipt: warm_ms={} projection_total_ms={} projection_tail_ms={} indexed_query_us={} pages_lowered={}",
            warm.as_millis(),
            converged.as_millis(),
            converged.saturating_sub(warm).as_millis(),
            indexed_elapsed.as_micros(),
            lowerings(),
        );
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    #[ignore = "manual storage packet receipt; set TINE_DIRECT_PROJECTION_CORPUS"]
    fn real_corpus_reference_family_matches_parser_oracle() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = PathBuf::from(
            std::env::var("TINE_DIRECT_PROJECTION_CORPUS")
                .expect("TINE_DIRECT_PROJECTION_CORPUS is required"),
        );
        let database = scratch("real-corpus-reference-family").join("projection.sqlite");
        let oracle = Graph::open(&root);
        oracle.warm_cache();
        let aliases = crate::query::page_aliases_with_owners(&oracle);
        let alias_target = aliases.first().map(|(alias, _, _)| alias.clone());
        let oracle_backlinks = alias_target
            .as_deref()
            .map(|target| crate::query::backlinks(&oracle, target));
        let oracle_unlinked = alias_target
            .as_deref()
            .map(|target| crate::query::unlinked_refs(&oracle, target));
        let oracle_count_started = Instant::now();
        let oracle_counts = oracle.block_ref_counts().unwrap();
        let oracle_count_elapsed = oracle_count_started.elapsed();
        let block_claim = oracle.with_pages(|pages| {
            pages.iter().find_map(|(_, document)| {
                let mut claim = None;
                fn visit(blocks: &[DocBlock], claim: &mut Option<String>) {
                    for block in blocks {
                        if claim.is_none() {
                            *claim = block.projection().block_refs.first().cloned();
                        }
                        visit(&block.children, claim);
                    }
                }
                visit(&document.roots, &mut claim);
                claim
            })
        });
        let oracle_referrers = block_claim
            .as_deref()
            .map(|claim| crate::query::block_referrers(&oracle, claim));
        let oracle_resolved = block_claim
            .as_deref()
            .and_then(|claim| crate::query::resolve_block(&oracle, claim));

        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        assert_eq!(graph.page_aliases_with_owners(), aliases);
        let projected_count_started = Instant::now();
        let projected_counts = graph.block_ref_counts().unwrap();
        let projected_count_elapsed = projected_count_started.elapsed();
        assert_eq!(projected_counts.as_ref(), oracle_counts.as_ref());
        eprintln!(
            "real-corpus-reference counts={} parser_count_us={} sqlite_count_us={}",
            projected_counts.len(),
            oracle_count_elapsed.as_micros(),
            projected_count_elapsed.as_micros(),
        );
        if let Some(target) = alias_target.as_deref() {
            assert_eq!(
                signature(&crate::query::backlinks(&graph, target)),
                signature(oracle_backlinks.as_deref().unwrap())
            );
            assert_eq!(
                signature(&crate::query::unlinked_refs(&graph, target)),
                signature(oracle_unlinked.as_deref().unwrap())
            );
            let candidates = graph.reference_candidate_pages(
                &[crate::refs::page_key(target)],
                ReferenceKind::Explicit,
            );
            assert!(candidates.indexed);
            eprintln!(
                "real-corpus-reference explicit_candidates={} full_pages={}",
                candidates.pages.len(),
                candidates.full_page_count
            );
        }
        if let Some(claim) = block_claim.as_deref() {
            assert_eq!(
                signature(&crate::query::block_referrers(&graph, claim)),
                signature(oracle_referrers.as_deref().unwrap())
            );
            assert_eq!(
                crate::query::resolve_block(&graph, claim)
                    .as_ref()
                    .map(|group| signature(std::slice::from_ref(group))),
                oracle_resolved
                    .as_ref()
                    .map(|group| signature(std::slice::from_ref(group)))
            );
        }
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }
}
