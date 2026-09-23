//! The image's page order: giving a stored image the queue's order, and
//! settling deltas published before the session seeded that order
//! (GH #550, GH #543).
use super::*;

/// Give a stored image the queue's order, for a turn that keeps the image
/// after the queue seeded its order from an inventory (a warm walk that came
/// back clean, a full snapshot whose sources the image already holds). A
/// reopened image keeps the positions it was written with: a page deleted
/// in an earlier session left a gap, and the next new page took a stored
/// page's position (`UNIQUE constraint failed: pages.position`), failing
/// every later turn (GH #543; the reused-snapshot path, audit R8-03). An
/// image already in that order is not written.
pub(super) fn adopt_queue_order(
    database: &mut PhysicalGraphProjectionDatabase,
    shared: &ProjectionShared,
    taken: &mut BTreeMap<String, (u64, PageDelta)>,
) -> Result<(), String> {
    let order = reconcile_page_order(
        database,
        shared,
        &PhysicalGraphProjectionChange {
            replacements: Vec::new(),
            deletions: Vec::new(),
            reference_postings: Vec::new(),
        },
        &[],
        &[],
    )?;
    shared.pending.lock().unwrap().adopt_order(&order, taken);
    Ok(())
}

/// Apply `change` and give every page the image holds afterwards its place in
/// the queue's order (the walk, then pages this session created). Returns
/// that order. Positions that already match are not written.
pub(super) fn reconcile_page_order(
    database: &mut PhysicalGraphProjectionDatabase,
    shared: &ProjectionShared,
    change: &PhysicalGraphProjectionChange,
    revisions: &[PhysicalGraphProjectionSourceRevision],
    aliases: &[PhysicalAliasDeclaration],
) -> Result<Vec<String>, String> {
    // Against an empty inventory every stored page reads as a deletion: that
    // is the set of paths the image holds.
    let mut after = database
        .source_delta(&[])
        .map_err(|error| error.to_string())?
        .deletions
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    for path in &change.deletions {
        after.remove(path);
    }
    after.extend(change.replacements.iter().map(|page| page.path.clone()));
    let mut order = shared
        .pending
        .lock()
        .unwrap()
        .ordered_inventory()
        .into_iter()
        .filter(|path| after.remove(path))
        .collect::<Vec<_>>();
    // Nothing should be left; anything that is still gets a place at the end.
    order.extend(after);
    database
        .apply_with_source_revisions_aliases_and_page_order(change, revisions, aliases, &order)
        .map_err(|error| error.to_string())?;
    Ok(order)
}

/// GH #550: settle the deltas a session published before it had seeded its
/// page order (they carry no position). Launch reads -- the Journals feed
/// loading its first days -- publish every page they read, and they arrive
/// before the warm has validated the reopened image.
///
/// - A page the image already holds at this exact source revision is
///   dropped: re-lowering it writes rows identical to the ones stored.
/// - A changed page the image holds is applied without a position, so it
///   keeps its stored one.
/// - A page the image does not hold cannot be placed without the session's
///   inventory. It is returned second, to wait for that inventory instead of
///   inventing a position (`PendingProjection::unplaced`).
#[allow(clippy::type_complexity)]
pub(super) fn settle_unseeded_deltas(
    database: &PhysicalGraphProjectionDatabase,
    mut deltas: BTreeMap<String, (u64, PageDelta)>,
) -> Result<
    (
        BTreeMap<String, (u64, PageDelta)>,
        BTreeMap<String, (u64, PageDelta)>,
    ),
    String,
> {
    let unseeded = deltas
        .values()
        .filter_map(|(_, delta)| match delta {
            PageDelta::Replace {
                entry,
                revision,
                parse_config,
                page_position: None,
                ..
            } => Some(PhysicalGraphProjectionSourceRevision {
                path: entry.rel_path.clone(),
                revision: projection_source_revision(revision, parse_config.digest()),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut unplaced = BTreeMap::new();
    if unseeded.is_empty() {
        return Ok((deltas, unplaced));
    }
    // Against an empty inventory every stored page reads as a deletion: that
    // is the set of paths the image holds.
    let stored = database
        .source_delta(&[])
        .map_err(|error| error.to_string())?
        .deletions
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let changed = database
        .source_delta(&unseeded)
        .map_err(|error| error.to_string())?
        .replacements
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    for source in unseeded {
        if !stored.contains(&source.path) {
            if let Some(update) = deltas.remove(&source.path) {
                unplaced.insert(source.path, update);
            }
            continue;
        }
        if !changed.contains(&source.path) {
            deltas.remove(&source.path);
        }
    }
    Ok((deltas, unplaced))
}
