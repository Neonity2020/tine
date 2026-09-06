//! R5c: the property registry a PENDING Managed query is lowered under, built
//! off the actor by patching the accepted table.
//!
//! Before R5c a pending query with a `props` leaf walked on the actor and the
//! actor rebuilt the whole registry — two full table scans — on every read,
//! because the registry cache key was `None` for the whole time a local suffix
//! was undrained. That made typing cost the graph (I-13).
//!
//! After R5c the actor caches exactly ONE registry, the ACCEPTED table, and a
//! captured pending query carries it. Here, under the executor's two owned
//! snapshots (the accepted projection and the pending overlay), only the keys
//! the pending pages can have changed are rebuilt:
//!
//! 1. the keys of the masked pages' accepted property rows,
//! 2. the keys of the overlay's property rows,
//! 3. every key whose DECLARATION page — a page named like the key, carrying
//!    `tine.type::` — is pending on either side (a page name reaches
//!    `build_registry` in exactly one place, the declaration insert).
//!
//! That rule is complete: a registry row for key K is a function of K's
//! complete row set with each row's page format, plus the `tine.type`
//! declaration bound to `refs::page_key(K)`; the first can only move if a
//! masked or overlay page carries a K row (1 ∪ 2), the second only if a page
//! whose name maps to `page_key(K)` is pending (3). A config change moves the
//! config digest, which moves both the accepted cache key and the query stamp,
//! so the whole table is rebuilt and nothing is patched.
//!
//! Each affected key's row is produced by [`build_registry`] itself over that
//! key's complete row set (D-4: one producer), and the rows are then spliced
//! into a copy of the base by [`patch_registry`]. An ordinary text edit
//! touches no property row, so nothing is affected and no accepted property
//! row is read at all.
//!
//! **D-15.** Every statement here crosses the projection's read-only statement
//! seam with BOUND parameters, on snapshots the executor already opened and
//! validated; nothing writable is reachable from this module.
//!
//! **D-3.** A row that names a page the SAME snapshot cannot answer is a
//! snapshot-consistency defect, exactly as [`RegistryError::UnknownPage`] is
//! for the actor's build: the read FAILS. It never falls back to a silently
//! wrong table.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;

use tine_storage::sqlite::{PhysicalProjectionQuerySnapshot, PhysicalQueryValue};
use uuid::Uuid;

use crate::config::ParseConfig;
use crate::doc::property_key_norm;
use crate::query::registry::{
    build_registry, is_internal_key, patch_registry, OwnerRow, OwnerType, PageMeta, Registry,
    DECLARED_TYPE_KEY,
};
use crate::refs;

/// Why a pending registry patch could not be produced.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PatchError {
    /// The owner cancelled the job mid-read (a drain before a file
    /// replacement, or close). Not a defect: the walk answers.
    Cancelled,
    /// The read was attempted and the snapshots contradict themselves — a
    /// statement the seam refused, a column of the wrong class, or a property
    /// row naming a page neither snapshot holds. D-3: this FAILS the query.
    Damaged,
}

/// The full `properties` projection, in the column order
/// `property_facet_rows_after` streams. Both files have the same schema.
macro_rules! property_columns {
    () => {
        "owner_type, owner_id, page_id, name, normalized_name, value, ordinal"
    };
}

/// Rows of ONE key, over `properties_lookup_idx(normalized_name, …)`.
const ROWS_BY_KEY_SQL: &str = concat!(
    "SELECT ",
    property_columns!(),
    " FROM properties WHERE normalized_name = ?1"
);

/// Every `tine.type::` PAGE-owner row, in the ACTOR's stream order.
///
/// The order is load-bearing: `build_registry`'s declaration map is
/// last-write-wins, so two pages whose names collide under `refs::page_key`
/// ("Status" and "status", both declaring) must resolve the way the full build
/// resolves them. `owner_type = 0` is PAGE (`PhysicalEntityId::sql_parts`);
/// `build_registry` reads declarations only from page owners.
const DECLARATIONS_SQL: &str = concat!(
    "SELECT ",
    property_columns!(),
    " FROM properties WHERE normalized_name = ?1 AND owner_type = 0",
    " ORDER BY owner_type, owner_id, name, ordinal"
);

/// The keys of one page's own property rows, over `properties_page_idx`.
const KEYS_OF_PAGE_SQL: &str = "SELECT DISTINCT normalized_name FROM properties WHERE page_id = ?1";

/// Every key the (small) overlay holds a row for.
const OVERLAY_KEYS_SQL: &str = "SELECT DISTINCT normalized_name FROM properties";

const PAGE_NAME_SQL: &str = "SELECT name FROM pages WHERE page_id = ?1";
const OVERLAY_PAGE_NAMES_SQL: &str = "SELECT name FROM pages";
const PAGE_META_SQL: &str = "SELECT name, path FROM pages WHERE page_id = ?1";

/// The `page_id` spelling of a projection row, mirroring the actor's
/// `managed_registry_page_key`. Both files carry the real Managed ids, so one
/// spelling over both is what makes the mask partition the id space: a masked
/// page's overlay entry replaces its accepted one under the same string.
fn page_key_of(id: &[u8; 16]) -> String {
    format!("page:{}", Uuid::from_bytes(*id))
}

// Counts the per-key ACCEPTED property reads this build issued, so a gate can
// prove that an ordinary text edit reads none (test-only).
#[cfg(test)]
thread_local! {
    static ACCEPTED_KEY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_accepted_key_reads() {
    ACCEPTED_KEY_READS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn accepted_key_reads() -> usize {
    ACCEPTED_KEY_READS.with(|count| count.get())
}

#[cfg(test)]
fn note_accepted_key_read() {
    ACCEPTED_KEY_READS.with(|count| count.set(count.get() + 1));
}

/// The registry a captured PENDING query is lowered under: `base` with exactly
/// the affected keys rebuilt over `(accepted rows minus masked pages) ∪
/// (overlay rows)`.
///
/// `accepted` is the projection snapshot the executor already validated
/// against the capture's stamp; `overlay` is the pending overlay at the
/// flushed revision the capture required; `masked_pages` is the executor's own
/// mask (the accepted page id of every pending path). Both snapshots stay
/// exactly as the two-source read left them — this reads, and never writes,
/// either file.
pub(crate) fn patched_pending_registry(
    accepted: &mut PhysicalProjectionQuerySnapshot,
    overlay: &mut PhysicalProjectionQuerySnapshot,
    masked_pages: &[[u8; 16]],
    base: &Registry,
    config: &ParseConfig,
) -> Result<Registry, PatchError> {
    let masked: HashSet<[u8; 16]> = masked_pages.iter().copied().collect();
    let mut affected: BTreeSet<String> = BTreeSet::new();
    // Names of every page that is pending on either side: the masked accepted
    // pages (their old names, which is the source side of a rename) and the
    // overlay's pages (their current names).
    let mut pending_names: Vec<String> = Vec::new();

    // (1) The masked pages' accepted keys and names.
    for id in masked_pages {
        let parameter = [PhysicalQueryValue::Blob(id.to_vec())];
        visit(accepted, KEYS_OF_PAGE_SQL, &parameter, |row| {
            affected.insert(text(row, 0)?);
            Ok(())
        })?;
        visit(accepted, PAGE_NAME_SQL, &parameter, |row| {
            pending_names.push(text(row, 0)?);
            Ok(())
        })?;
    }
    // (2) The overlay's keys and page names. Both scans are bounded by the
    // pending pages, which is the edit, not the graph.
    let mut overlay_keys: Vec<String> = Vec::new();
    visit(overlay, OVERLAY_KEYS_SQL, &[], |row| {
        overlay_keys.push(text(row, 0)?);
        Ok(())
    })?;
    visit(overlay, OVERLAY_PAGE_NAMES_SQL, &[], |row| {
        pending_names.push(text(row, 0)?);
        Ok(())
    })?;
    affected.extend(overlay_keys.iter().cloned());

    // (3) Declaration-affected keys: a page whose NAME is a key's name is that
    // key's declaration page. Built once as a map from `page_key` to the keys
    // that map to it, so this is one hash lookup per pending page name rather
    // than a scan of the base per name.
    let mut keys_by_page_key: HashMap<String, Vec<String>> = HashMap::new();
    for key in base
        .rows()
        .iter()
        .map(|row| row.normalized_name.clone())
        .chain(overlay_keys)
    {
        keys_by_page_key
            .entry(refs::page_key(&key))
            .or_default()
            .push(key);
    }
    for name in &pending_names {
        if let Some(keys) = keys_by_page_key.get(&refs::page_key(name)) {
            affected.extend(keys.iter().cloned());
        }
    }

    // (4) An internal key never has a row, so it is never patched.
    // `tine.type` itself is handled below as a declaration, not as a key.
    affected.retain(|key| {
        let normalized = property_key_norm(key);
        !normalized.is_empty() && !is_internal_key(&normalized, config)
    });
    if affected.is_empty() {
        // An ordinary text edit: nothing else is read at all.
        return Ok(base.clone());
    }

    // (5) The declarations, once for the whole build: the accepted stream
    // first (masked pages dropped), the overlay's after it, each in the
    // actor's own stream order.
    let declaration_key = [PhysicalQueryValue::Text(DECLARED_TYPE_KEY.to_string())];
    let mut declarations: Vec<OwnerRow> = Vec::new();
    let mut needed_pages: BTreeSet<[u8; 16]> = BTreeSet::new();
    visit(accepted, DECLARATIONS_SQL, &declaration_key, |row| {
        let (page_id, owner) = owner_row(row)?;
        if !masked.contains(&page_id) {
            needed_pages.insert(page_id);
            declarations.push(owner);
        }
        Ok(())
    })?;
    visit(overlay, DECLARATIONS_SQL, &declaration_key, |row| {
        let (page_id, owner) = owner_row(row)?;
        needed_pages.insert(page_id);
        declarations.push(owner);
        Ok(())
    })?;

    // (6) One rebuild per affected key, over the key's COMPLETE row set.
    let mut pages = PageMetaCache::default();
    pages.resolve(accepted, overlay, &masked, &needed_pages)?;
    let mut patches: Vec<(String, Option<crate::query::ir::RegistryRow>)> =
        Vec::with_capacity(affected.len());
    for key in &affected {
        let parameter = [PhysicalQueryValue::Text(key.clone())];
        let mut rows: Vec<OwnerRow> = Vec::new();
        let mut needed: BTreeSet<[u8; 16]> = BTreeSet::new();
        // `OwnerRow`s are built INSIDE the visitor: a common key can own a row
        // on every block, and `run_projection_query` would hold the whole set
        // twice.
        #[cfg(test)]
        note_accepted_key_read();
        visit(accepted, ROWS_BY_KEY_SQL, &parameter, |row| {
            let (page_id, owner) = owner_row(row)?;
            if !masked.contains(&page_id) {
                needed.insert(page_id);
                rows.push(owner);
            }
            Ok(())
        })?;
        visit(overlay, ROWS_BY_KEY_SQL, &parameter, |row| {
            let (page_id, owner) = owner_row(row)?;
            needed.insert(page_id);
            rows.push(owner);
            Ok(())
        })?;
        pages.resolve(accepted, overlay, &masked, &needed)?;
        rows.extend(declarations.iter().cloned());
        let built = build_registry(rows.into_iter(), &|page_id| pages.get(page_id), config)
            .map_err(|_| PatchError::Damaged)?;
        patches.push((key.clone(), built.row(key).cloned()));
    }
    Ok(patch_registry(base, patches))
}

/// The same-snapshot page lookup [`build_registry`] requires (§6.2 G3, E4),
/// merged over the two files exactly as the actor's own builder merges them.
///
/// ONE map per build. The overlay answers first: it holds exactly the pending
/// pages, and a pending page's accepted rows are masked away, so the overlay's
/// entry is the only right answer for a masked id and the only entry at all
/// for a page the accepted file has never seen. A lookup miss is a
/// snapshot-consistency defect and fails the build.
#[derive(Default)]
struct PageMetaCache {
    metas: HashMap<String, PageMeta>,
    resolved: HashSet<[u8; 16]>,
}

impl PageMetaCache {
    fn get(&self, page_id: &str) -> Option<PageMeta> {
        self.metas.get(page_id).cloned()
    }

    fn resolve(
        &mut self,
        accepted: &mut PhysicalProjectionQuerySnapshot,
        overlay: &mut PhysicalProjectionQuerySnapshot,
        masked: &HashSet<[u8; 16]>,
        needed: &BTreeSet<[u8; 16]>,
    ) -> Result<(), PatchError> {
        for id in needed {
            if !self.resolved.insert(*id) {
                continue;
            }
            let parameter = [PhysicalQueryValue::Blob(id.to_vec())];
            let mut meta: Option<PageMeta> = None;
            visit(overlay, PAGE_META_SQL, &parameter, |row| {
                meta = Some(page_meta(row)?);
                Ok(())
            })?;
            if meta.is_none() && !masked.contains(id) {
                visit(accepted, PAGE_META_SQL, &parameter, |row| {
                    meta = Some(page_meta(row)?);
                    Ok(())
                })?;
            }
            // E4/D-3: a property row naming a page neither snapshot holds is
            // the projection contradicting itself, never a Markdown default.
            self.metas
                .insert(page_key_of(id), meta.ok_or(PatchError::Damaged)?);
        }
        Ok(())
    }
}

fn page_meta(row: &[PhysicalQueryValue]) -> Result<PageMeta, PatchError> {
    let name = text(row, 0)?;
    let path = text(row, 1)?;
    Ok(PageMeta {
        format: crate::model::Format::from_path(Path::new(path.as_str())).into(),
        name,
    })
}

/// One `properties` row as the registry's opaque-id [`OwnerRow`], plus the raw
/// page id the mask and the page lookup are keyed by.
///
/// `owner_type` 0 is PAGE and 1 is BLOCK (`PhysicalEntityId::sql_parts`), and
/// the id spellings mirror the actor's (`p:<uuid>` / `b:<uuid>`, page
/// `page:<uuid>`) so a build over both files agrees with itself.
fn owner_row(row: &[PhysicalQueryValue]) -> Result<([u8; 16], OwnerRow), PatchError> {
    let owner_type = match integer(row, 0)? {
        0 => OwnerType::Page,
        1 => OwnerType::Block,
        _ => return Err(PatchError::Damaged),
    };
    let owner_id = blob(row, 1)?;
    let page_id = blob(row, 2)?;
    let owner_id = match owner_type {
        OwnerType::Page => format!("p:{}", Uuid::from_bytes(owner_id)),
        OwnerType::Block => format!("b:{}", Uuid::from_bytes(owner_id)),
    };
    let ordinal = u32::try_from(integer(row, 6)?).map_err(|_| PatchError::Damaged)?;
    Ok((
        page_id,
        OwnerRow {
            owner_type,
            owner_id,
            page_id: page_key_of(&page_id),
            source_name: text(row, 3)?,
            normalized_name: text(row, 4)?,
            ordinal,
            value: text(row, 5)?,
        },
    ))
}

fn text(row: &[PhysicalQueryValue], at: usize) -> Result<String, PatchError> {
    match row.get(at) {
        Some(PhysicalQueryValue::Text(value)) => Ok(value.clone()),
        _ => Err(PatchError::Damaged),
    }
}

fn integer(row: &[PhysicalQueryValue], at: usize) -> Result<i64, PatchError> {
    match row.get(at) {
        Some(PhysicalQueryValue::Integer(value)) => Ok(*value),
        _ => Err(PatchError::Damaged),
    }
}

fn blob(row: &[PhysicalQueryValue], at: usize) -> Result<[u8; 16], PatchError> {
    match row.get(at) {
        Some(PhysicalQueryValue::Blob(bytes)) if bytes.len() == 16 => {
            Ok(bytes.as_slice().try_into().expect("a checked 16-byte id"))
        }
        _ => Err(PatchError::Damaged),
    }
}

/// One bound statement over the read-only seam, visited row by row.
///
/// A cancelled snapshot is [`PatchError::Cancelled`] — the drain, not a defect
/// — exactly as the executor's own probes classify it; any other refusal is
/// `Damaged` and the query FAILS (D-3).
fn visit(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    sql: &str,
    parameters: &[PhysicalQueryValue],
    mut row: impl FnMut(&[PhysicalQueryValue]) -> Result<(), PatchError>,
) -> Result<(), PatchError> {
    let mut refused: Option<PatchError> = None;
    // A visitor REFUSAL breaks cleanly rather than raising a storage error:
    // a clean break finalizes the statement and leaves the snapshot usable,
    // and the refusal itself is what the caller must see.
    let outcome = snapshot.visit_projection_query(sql, parameters, |values| match row(values) {
        Ok(()) => Ok(ControlFlow::Continue(())),
        Err(error) => {
            refused = Some(error);
            Ok(ControlFlow::Break(()))
        }
    });
    if let Some(error) = refused {
        return Err(error);
    }
    match outcome {
        Ok(()) => Ok(()),
        Err(_) if snapshot.cancellation().is_cancelled() => Err(PatchError::Cancelled),
        Err(_) => Err(PatchError::Damaged),
    }
}

/// Open the executor's two snapshots BY PATH, off the actor, and mask by the
/// overlay's published pending set — exactly what the executor does before it
/// reaches a registry producer (test-only).
///
/// The ONE opener both test entry points below share, so a gate that times the
/// patch and a gate that compares it to the oracle are looking at the same two
/// read transactions over the same mask.
#[cfg(test)]
fn open_pending_pair(
    accepted_path: &Path,
    overlay_path: &Path,
    pending_paths: &BTreeSet<String>,
) -> Result<
    (
        PhysicalProjectionQuerySnapshot,
        PhysicalProjectionQuerySnapshot,
        Vec<[u8; 16]>,
    ),
    PatchError,
> {
    let mut accepted = PhysicalProjectionQuerySnapshot::open_direct(accepted_path, || Ok(()))
        .map_err(|_| PatchError::Damaged)?;
    let overlay = PhysicalProjectionQuerySnapshot::open_direct(overlay_path, || Ok(()))
        .map_err(|_| PatchError::Damaged)?;
    let mut mask: Vec<[u8; 16]> = Vec::new();
    for path in pending_paths {
        let parameter = [PhysicalQueryValue::Text(path.clone())];
        visit(
            &mut accepted,
            "SELECT page_id FROM pages WHERE path = ?1",
            &parameter,
            |row| {
                mask.push(blob(row, 0)?);
                Ok(())
            },
        )?;
    }
    Ok((accepted, overlay, mask))
}

/// The patched registry ALONE, over a freshly opened pair (test-only).
///
/// The cost receipt times this rather than `pending_registry_pair`, so the
/// number it prints is the patch, not the patch plus the oracle it is checked
/// against.
#[cfg(test)]
pub(crate) fn pending_registry_patched(
    accepted_path: &Path,
    overlay_path: &Path,
    pending_paths: &BTreeSet<String>,
    base: &Registry,
    config: &ParseConfig,
) -> Result<Registry, PatchError> {
    let (mut accepted, mut overlay, mask) =
        open_pending_pair(accepted_path, overlay_path, pending_paths)?;
    patched_pending_registry(&mut accepted, &mut overlay, &mask, base, config)
}

/// Open the executor's two snapshots BY PATH, off the actor, and produce both
/// the patched registry and the full-build oracle over the same pending state
/// and the same mask (test-only).
///
/// This is how every I-19 gate compares the two: one pair of read
/// transactions, one mask, two producers. `pending_paths` is the overlay's
/// published pending set, exactly what the executor masks by.
#[cfg(test)]
pub(crate) fn pending_registry_pair(
    accepted_path: &Path,
    overlay_path: &Path,
    pending_paths: &BTreeSet<String>,
    base: &Registry,
    config: &ParseConfig,
) -> Result<(Registry, Registry), PatchError> {
    let (mut accepted, mut overlay, mask) =
        open_pending_pair(accepted_path, overlay_path, pending_paths)?;
    let patched = patched_pending_registry(&mut accepted, &mut overlay, &mask, base, config)?;
    let full = full_pending_registry(&mut accepted, &mut overlay, &mask, config)?;
    Ok((patched, full))
}

/// The projection's own key frequencies, most common first (test-only).
///
/// A corpus gate picks the keys it exercises from the graph rather than
/// naming them, so no corpus content ever enters this source (AGENTS §4).
#[cfg(test)]
pub(crate) fn most_common_keys(accepted_path: &Path, limit: usize) -> Vec<(String, i64)> {
    let mut accepted = PhysicalProjectionQuerySnapshot::open_direct(accepted_path, || Ok(()))
        .expect("the accepted projection opens for reading");
    let mut keys: Vec<(String, i64)> = Vec::new();
    visit(
        &mut accepted,
        "SELECT normalized_name, COUNT(*) AS rows FROM properties \
         GROUP BY normalized_name ORDER BY rows DESC, normalized_name",
        &[],
        |row| {
            if keys.len() < limit {
                keys.push((text(row, 0)?, integer(row, 1)?));
            }
            Ok(())
        },
    )
    .expect("the key frequency scan reads");
    keys
}

/// The two facts the by-key read is only correct under, pinned against a real
/// projection rather than remembered (test-only).
///
/// Returns `(property rows seen, page-owner rows seen)`. Panics naming the
/// offending row class — never a value (I-5) — when the producer's
/// `normalized_name` is not `property_key_norm(name)`, or when a PAGE-owner
/// row is not `owner_type = 0`.
#[cfg(test)]
pub(crate) fn pin_property_row_conventions(accepted_path: &Path) -> (usize, usize) {
    let mut accepted = PhysicalProjectionQuerySnapshot::open_direct(accepted_path, || Ok(()))
        .expect("the accepted projection opens for reading");
    let mut rows = 0usize;
    let mut page_owned = 0usize;
    visit(
        &mut accepted,
        concat!("SELECT ", property_columns!(), " FROM properties"),
        &[],
        |row| {
            let owner_type = integer(row, 0)?;
            let source_name = text(row, 3)?;
            let normalized = text(row, 4)?;
            assert_eq!(
                normalized,
                property_key_norm(&source_name),
                "the producer writes normalized_name = property_key_norm(name); \
                 the by-key lookup is only correct under that identity"
            );
            rows += 1;
            assert!(
                owner_type == 0 || owner_type == 1,
                "owner_type is 0 (page) or 1 (block)"
            );
            if owner_type == 0 {
                page_owned += 1;
            }
            Ok(())
        },
    )
    .expect("the properties scan reads");
    (rows, page_owned)
}

/// The FULL build over the same two snapshots — the oracle every patch
/// gate compares against. It is deliberately the dumbest possible reading
/// of "(accepted rows minus masked pages) ∪ (overlay rows)": both
/// `properties` tables scanned whole, both `pages` tables scanned whole,
/// then ONE `build_registry`. It is a test helper, never production.
#[cfg(test)]
pub(crate) fn full_pending_registry(
    accepted: &mut PhysicalProjectionQuerySnapshot,
    overlay: &mut PhysicalProjectionQuerySnapshot,
    masked_pages: &[[u8; 16]],
    config: &ParseConfig,
) -> Result<Registry, PatchError> {
    let masked: HashSet<[u8; 16]> = masked_pages.iter().copied().collect();
    let mut pages: HashMap<String, PageMeta> = HashMap::new();
    let mut rows: Vec<OwnerRow> = Vec::new();
    let all_pages = "SELECT page_id, name, path FROM pages";
    visit(accepted, all_pages, &[], |row| {
        let id = blob(row, 0)?;
        if !masked.contains(&id) {
            pages.insert(
                page_key_of(&id),
                PageMeta {
                    format: crate::model::Format::from_path(Path::new(text(row, 2)?.as_str()))
                        .into(),
                    name: text(row, 1)?,
                },
            );
        }
        Ok(())
    })?;
    // The overlay is inserted OVER the accepted map, exactly as the
    // actor's own builder overlays it (C4).
    visit(overlay, all_pages, &[], |row| {
        let id = blob(row, 0)?;
        pages.insert(
            page_key_of(&id),
            PageMeta {
                format: crate::model::Format::from_path(Path::new(text(row, 2)?.as_str())).into(),
                name: text(row, 1)?,
            },
        );
        Ok(())
    })?;
    let stream = concat!(
        "SELECT ",
        property_columns!(),
        " FROM properties ORDER BY owner_type, owner_id, name, ordinal"
    );
    visit(accepted, stream, &[], |row| {
        let (page_id, owner) = owner_row(row)?;
        if !masked.contains(&page_id) {
            rows.push(owner);
        }
        Ok(())
    })?;
    visit(overlay, stream, &[], |row| {
        let (_, owner) = owner_row(row)?;
        rows.push(owner);
        Ok(())
    })?;
    build_registry(
        rows.into_iter(),
        &|page_id| pages.get(page_id).cloned(),
        config,
    )
    .map_err(|_| PatchError::Damaged)
}

#[cfg(test)]
mod tests {
    /// The contract's account of this route, pinned sentence by sentence so a
    /// rewrite of either side fails here first.
    #[test]
    fn r5c_storage_contract_names_the_patched_pending_registry() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        let section = contract
            .split("**The property registry is patched, never rebuilt, for a pending query.**")
            .nth(1)
            .and_then(|tail| tail.split("## 2. Enrollment").next())
            .expect("the patched-registry paragraph precedes section 2");
        for sentence in [
            "The actor caches ONE registry: the accepted table",
            "keyed by acceptance\nsequence, frontier digest and parse config",
            "a pending suffix does not evict it\nand does not advance its generation",
            "the executor patches, off the actor and under its two\nsnapshots, exactly the keys the pending pages can have changed",
            "the keys of the\nmasked pages' rows, the keys of the overlay's rows, and every key whose\ndeclaration page",
            "carrying `tine.type::`) is\npending",
            "rebuilt by the one producer over the key's\ncomplete row set",
            "An ordinary text edit affects no key and reads no accepted\nproperty row",
            "built per read and never\npublished",
            "readers fall back to the accepted\ntable (a coherent older answer), never to a merged table from another pending\nrevision",
        ] {
            assert!(section.contains(sentence), "contract lost: {sentence}");
        }
    }
}
