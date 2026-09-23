//! Repairing a stored image in place: validating a warm walk against the
//! image, and bringing it to a full snapshot by lowering only the pages that
//! differ (GH #543).
use super::*;

/// The pages `full` differs from the image by: pages to lower again and
/// stored pages to delete. A page the snapshot could not read (`retained`,
/// or anything under a retained directory) keeps its rows, as an unreadable
/// page does in a warm validation.
pub(super) fn full_repair_delta(
    database: &PhysicalGraphProjectionDatabase,
    full: &PendingFull,
) -> Result<tine_storage::sqlite::PhysicalGraphProjectionSourceDelta, String> {
    let config_digest = full.parse_config.digest();
    let mut sources = Vec::with_capacity(full.pages.len());
    for (entry, _) in full.pages.iter() {
        let revision = full
            .revisions
            .get(&entry.path)
            .ok_or_else(|| format!("snapshot has no revision for {}", entry.rel_path))?;
        sources.push(PhysicalGraphProjectionSourceRevision {
            path: entry.rel_path.clone(),
            revision: projection_source_revision(revision, config_digest),
        });
    }
    let mut delta = database
        .source_delta(&sources)
        .map_err(|error| error.to_string())?;
    // A page under an unreadable directory is as unread as an unreadable
    // page: deleting its rows would empty the index for as long as the
    // directory stays unreadable, and publish that as ready.
    let unread = |path: &str| {
        full.retained.iter().any(|retained| {
            retained.is_empty()
                || path == retained
                || path
                    .strip_prefix(retained.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    delta.deletions.retain(|path| !unread(path));
    Ok(delta)
}

/// R6 warm validation inside one worker turn: compare the walk inventory's
/// exact revisions with `direct_source_revisions`, delete what the walk no
/// longer has, and name what must be relowered. Nothing here parses.
pub(super) fn validate_warm(
    database: &PhysicalGraphProjectionDatabase,
    warm: &PendingWarm,
) -> Result<WarmOutcome, String> {
    let config_digest = warm.parse_config.digest();
    let sources = warm
        .sources
        .iter()
        .map(|(entry, revision)| PhysicalGraphProjectionSourceRevision {
            path: entry.rel_path.clone(),
            revision: projection_source_revision(revision, config_digest),
        })
        .collect::<Vec<_>>();
    let mut source_delta = database
        .source_delta(&sources)
        .map_err(|error| error.to_string())?;
    if !warm.retained.is_empty() {
        // A page whose bytes could not be read is absent from `sources`, so
        // `source_delta` names it a deletion — and warm validation would drop
        // the rows of a page that still exists (GH #543). An unreadable page
        // keeps its previous rows AND its previous stored revision, so the
        // next warm names it a replacement and re-reads it once the read
        // succeeds. In-scope scenarios: a transient disk error, and a
        // Windows/macOS sharing violation while another process holds the file.
        let retained = warm
            .retained
            .iter()
            .map(|entry| entry.rel_path.clone())
            .collect::<std::collections::BTreeSet<_>>();
        source_delta.deletions.retain(|id| !retained.contains(id));
    }
    if !warm.published.is_empty() {
        let published = warm
            .published
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        source_delta.deletions.retain(|id| !published.contains(id));
        source_delta
            .replacements
            .retain(|id| !published.contains(id));
    }
    if source_delta.replacements.is_empty() && source_delta.deletions.is_empty() {
        return Ok(WarmOutcome::Clean);
    }
    let walk = warm.sources.len() + warm.retained.len();
    let changed = source_delta.replacements.len() + source_delta.deletions.len();
    // A page the walk could not read keeps its stored rows through the
    // repair, as it does through a clean validation and a full snapshot
    // (`apply_full_repair`): asking for a full parse instead gains
    // nothing, because that parse cannot read the page either.
    if !repair_is_proportionate(changed, walk) {
        return Ok(WarmOutcome::FreshBuildRequired);
    }
    Ok(WarmOutcome::Changed {
        replacements: source_delta.replacements,
        deletions: source_delta.deletions,
    })
}

/// Whether an image that differs from its source by `changed` of `total`
/// pages is repaired in place rather than built fresh. It is the one rule
/// for both repair paths, the warm validation and a full snapshot: the full
/// path once had no bound, so a parse configuration changed while Tine was
/// closed re-lowered every page in one worker turn that nothing could stop
/// and no progress bar showed (GH #543, audit R10-02). A fresh build of
/// that many pages is no dearer, stops between batches, and reports progress.
pub(super) fn repair_is_proportionate(changed: usize, total: usize) -> bool {
    changed * REPAIR_MAX_SHARE_DIVISOR <= total
}

/// Bring the image to a full snapshot by lowering only the pages `delta`
/// names, then apply the updates queued with it.
///
/// A snapshot may be incomplete: an unreadable page or directory is missing
/// from it, and reading that absence as a deletion would empty those pages'
/// rows and publish it as ready. So pages in `retained` keep their stored
/// rows and revision, exactly as the warm validation keeps a page it cannot
/// read; a retained page that later changes arrives as an ordinary update.
/// In-scope scenarios: a transient disk error, a sharing violation while
/// another process holds the file, and a page whose parse fails (GH #543).
pub(super) fn apply_full_repair(
    database: &mut PhysicalGraphProjectionDatabase,
    shared: &ProjectionShared,
    full: &PendingFull,
    delta: tine_storage::sqlite::PhysicalGraphProjectionSourceDelta,
    mut deltas: BTreeMap<String, (u64, PageDelta)>,
) -> Result<AppliedTurn, String> {
    let documents = full
        .pages
        .iter()
        .map(|(entry, document)| (entry.rel_path.as_str(), (entry, document)))
        .collect::<HashMap<_, _>>();
    let replacements = delta
        .replacements
        .iter()
        .map(|path| {
            let (entry, document) = documents
                .get(path.as_str())
                .ok_or_else(|| format!("snapshot has no page {path}"))?;
            let revision = full
                .revisions
                .get(&entry.path)
                .ok_or_else(|| format!("snapshot has no revision for {path}"))?;
            Ok(((*entry).clone(), Arc::clone(document), revision.clone()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    projection_diag(|| {
        format!(
            "full repair: relowering {} page(s), deleting {}, keeping the rows under {} unread source(s)",
            replacements.len(),
            delta.deletions.len(),
            full.retained.len()
        )
    });
    let order = apply_warm_repair(
        database,
        shared,
        WarmRepair {
            replacements,
            deletions: delta.deletions,
        },
        &full.parse_config,
    )?;
    shared
        .pending
        .lock()
        .unwrap()
        .adopt_order(&order, &mut deltas);
    apply_deltas(database, deltas)
}

/// Apply a `WarmRepair` in one transaction and reconcile every stored page's
/// position to the queue's order (the walk, then pages this session created),
/// restricted to the pages the image holds afterwards. Returns that order.
pub(super) fn apply_warm_repair(
    database: &mut PhysicalGraphProjectionDatabase,
    shared: &ProjectionShared,
    repair: WarmRepair,
    parse_config: &Arc<ParseConfig>,
) -> Result<Vec<String>, String> {
    let mut deltas = BTreeMap::new();
    for (entry, document, revision) in repair.replacements {
        deltas.insert(
            entry.rel_path.clone(),
            (
                0,
                PageDelta::Replace {
                    entry,
                    document,
                    revision,
                    parse_config: Arc::clone(parse_config),
                    page_position: None,
                },
            ),
        );
    }
    let lowered = lower_deltas(deltas)?;
    let mut change = lowered.change;
    change.deletions = repair.deletions;
    projection_diag(|| {
        format!(
            "warm repair: relowering {} page(s), deleting {}",
            change.replacements.len(),
            change.deletions.len()
        )
    });
    reconcile_page_order(
        database,
        shared,
        &change,
        &lowered.revisions,
        &lowered.aliases,
    )
}
