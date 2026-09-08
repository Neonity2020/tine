//! Registry construction over one read-only physical projection snapshot.
//!
//! [`read_registry`] is the shared SQL adapter used by the Direct query job.
//! [`patch_registry_from_snapshot`] is the corresponding affected-key rebuild
//! primitive for a later committed-registry owner: it is deliberately not
//! wired to cache publication or dirty-key production in this packet.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::path::Path;

use tine_storage::sqlite::{
    MaterializationError, PhysicalPage, PhysicalProjectionQuerySnapshot, PhysicalQueryValue,
};

use crate::config::ParseConfig;
use crate::doc::property_key_norm;
use crate::query::registry::{
    build_registry, is_internal_key, patch_registry, OwnerRow, OwnerType, PageMeta, Registry,
    DECLARED_TYPE_KEY,
};
use crate::query::{QueryExecutionError, QueryUnavailableReason};

const FULL_PAGES_SQL: &str = "SELECT page_id, path, name, text_kind FROM pages";

const FULL_PROPERTIES_SQL: &str =
    "SELECT o.owner_type, o.owner_id, o.page_id, o.name, o.normalized_name, \
     o.value, o.ordinal, b.page_id FROM properties o \
     LEFT JOIN blocks b ON o.owner_type = 1 AND b.block_id = o.owner_id \
     ORDER BY o.owner_type, o.owner_id, o.name, o.ordinal";

const PATCH_ROWS_SQL: &str =
    "SELECT o.owner_type, o.owner_id, o.page_id, o.name, o.normalized_name, \
     o.value, o.ordinal, b.page_id, p.path, p.name, p.text_kind \
     FROM properties o \
     LEFT JOIN blocks b ON o.owner_type = 1 AND b.block_id = o.owner_id \
     LEFT JOIN pages p ON p.page_id = o.page_id \
     WHERE o.normalized_name = ?1 \
     ORDER BY o.owner_type, o.owner_id, o.name, o.ordinal";

const PATCH_DECLARATIONS_SQL: &str =
    "SELECT o.owner_type, o.owner_id, o.page_id, o.name, o.normalized_name, \
     o.value, o.ordinal, b.page_id, p.path, p.name, p.text_kind \
     FROM properties o \
     LEFT JOIN blocks b ON o.owner_type = 1 AND b.block_id = o.owner_id \
     LEFT JOIN pages p ON p.page_id = o.page_id \
     WHERE o.normalized_name = ?1 AND o.owner_type = 0 \
     ORDER BY o.owner_type, o.owner_id, o.name, o.ordinal";

const PAGE_METADATA_BATCH_SIZE: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RegistryPropertyMetadata {
    pub(crate) owner_type: OwnerType,
    pub(crate) owner_id: [u8; 16],
    pub(crate) page_id: [u8; 16],
    pub(crate) source_name: String,
    pub(crate) normalized_name: String,
    pub(crate) value: String,
    pub(crate) ordinal: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryPageMetadata {
    pub(crate) page_id: [u8; 16],
    pub(crate) page: PageMeta,
    pub(crate) properties: Vec<RegistryPropertyMetadata>,
}

pub(crate) type PageRegistryMetadata = BTreeMap<[u8; 16], RegistryPageMetadata>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RegistryChanges {
    pub(crate) normalized_keys: BTreeSet<String>,
    pub(crate) declaration_page_names: BTreeSet<String>,
}

#[cfg(test)]
thread_local! {
    static STATEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_statement_count() {
    STATEMENTS.with(|count| count.set(0));
}

#[cfg(test)]
fn statement_count() -> usize {
    STATEMENTS.with(std::cell::Cell::get)
}

/// Build the complete registry from one projection snapshot.
///
/// This preserves the former `DirectQueryJob::read_registry` stream shape:
/// all page metadata first, then all property rows in global owner/name/order
/// order. The adapter only decodes physical rows; [`build_registry`] remains
/// the sole inference and aggregation producer.
pub(crate) fn read_registry(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    config: &ParseConfig,
) -> Result<Registry, QueryExecutionError> {
    check_cancelled(snapshot)?;
    let mut pages = HashMap::new();
    visit(snapshot, FULL_PAGES_SQL, &[], |row| {
        let id = page_key(blob16(row, 0, "pages.page_id")?);
        let meta = decode_page_meta(row, 1, 2, 3)?;
        if pages.insert(id, meta).is_some() {
            return Err("duplicate registry page".into());
        }
        Ok(())
    })?;

    let mut rows = Vec::new();
    visit(snapshot, FULL_PROPERTIES_SQL, &[], |row| {
        let owner = decode_owner_row(row)?;
        if !pages.contains_key(&owner.page_id) {
            return Err("registry property names an absent page".into());
        }
        rows.push(owner);
        Ok(())
    })?;

    let registry = build_registry(rows.into_iter(), &|page| pages.get(page).cloned(), config)
        .map_err(|_| invalid_snapshot())?;
    check_cancelled(snapshot)?;
    Ok(registry)
}

/// Rebuild only `affected` registry keys from one current projection snapshot.
///
/// This primitive does not own dirty-key discovery, cache publication, or a
/// registry generation. Its future caller must supply the complete affected
/// key set for the committed snapshot. A config change requires a full build,
/// represented here by the existing typed `InvalidSnapshot` result.
#[allow(dead_code)]
pub(crate) fn patch_registry_from_snapshot(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    base: &Registry,
    affected: &BTreeSet<String>,
    config: &ParseConfig,
) -> Result<Registry, QueryExecutionError> {
    check_cancelled(snapshot)?;
    if base.config_digest() != config.digest() {
        return Err(invalid_snapshot());
    }

    let affected: BTreeSet<String> = affected
        .iter()
        .map(|key| property_key_norm(key))
        .filter(|key| !key.is_empty() && !is_internal_key(key, config))
        .collect();
    if affected.is_empty() {
        return Ok(base.clone());
    }

    let mut rows = Vec::new();
    let mut pages = HashMap::new();
    for key in &affected {
        let parameters = [PhysicalQueryValue::Text(key.clone())];
        visit(snapshot, PATCH_ROWS_SQL, &parameters, |row| {
            collect_joined_row(row, &mut rows, &mut pages)
        })?;
    }

    let declaration = [PhysicalQueryValue::Text(DECLARED_TYPE_KEY.to_owned())];
    visit(snapshot, PATCH_DECLARATIONS_SQL, &declaration, |row| {
        collect_joined_row(row, &mut rows, &mut pages)
    })?;

    let rebuilt = build_registry(rows.into_iter(), &|page| pages.get(page).cloned(), config)
        .map_err(|_| invalid_snapshot())?;
    check_cancelled(snapshot)?;
    Ok(patch_registry(
        base,
        affected
            .into_iter()
            .map(|key| (key.clone(), rebuilt.row(&key).cloned())),
    ))
}

/// Read exactly the requested pages' registry-relevant physical metadata.
///
/// Missing pages are omitted so callers can represent creations. The query is
/// bounded by page count and retains the LEFT JOIN rows needed to reject an
/// orphan property instead of silently treating it as a missing page.
pub(crate) fn read_page_registry_metadata(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    ids: &BTreeSet<[u8; 16]>,
) -> Result<PageRegistryMetadata, QueryExecutionError> {
    check_cancelled(snapshot)?;
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut pages = BTreeMap::new();
    let ids = ids.iter().copied().collect::<Vec<_>>();
    for batch in ids.chunks(PAGE_METADATA_BATCH_SIZE) {
        let sql = page_metadata_sql(batch.len());
        let parameters = batch
            .iter()
            .map(|id| PhysicalQueryValue::Blob(id.to_vec()))
            .collect::<Vec<_>>();
        visit(snapshot, &sql, &parameters, |row| {
            collect_page_metadata_row(row, &mut pages)
        })?;
    }
    for page in pages.values_mut() {
        page.properties.sort();
    }
    check_cancelled(snapshot)?;
    Ok(pages)
}

/// Produce the same registry metadata that inserting `page` writes to the
/// physical projection. Property ordinals come from owner-local vector order,
/// exactly as tine-storage's insertion loop assigns them.
pub(crate) fn registry_metadata_from_physical_page(
    page: &PhysicalPage,
) -> Result<RegistryPageMetadata, String> {
    let page_meta = PageMeta {
        format: crate::model::Format::from_path(Path::new(&page.path)).into(),
        name: page.name.clone(),
    };
    let mut properties = Vec::new();
    append_physical_properties(
        &mut properties,
        OwnerType::Page,
        page.page_id,
        page.page_id,
        &page.properties,
    )?;
    for block in &page.blocks {
        append_physical_properties(
            &mut properties,
            OwnerType::Block,
            block.block_id,
            page.page_id,
            &block.properties,
        )?;
    }
    properties.sort();
    Ok(RegistryPageMetadata {
        page_id: page.page_id,
        page: page_meta,
        properties,
    })
}

/// Compare exact before/after page metadata and return the smallest registry
/// inputs whose aggregate rows or declaration binding can have changed.
pub(crate) fn registry_changes(
    before: &PageRegistryMetadata,
    after: &PageRegistryMetadata,
) -> RegistryChanges {
    let mut changes = RegistryChanges::default();
    let page_ids = before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for page_id in page_ids {
        let old = before.get(&page_id);
        let new = after.get(&page_id);
        if old == new {
            continue;
        }

        let old_by_key = old.map(properties_by_key).unwrap_or_default();
        let new_by_key = new.map(properties_by_key).unwrap_or_default();
        if matches!((old, new), (Some(old), Some(new)) if old.page.format != new.page.format) {
            changes
                .normalized_keys
                .extend(old_by_key.keys().chain(new_by_key.keys()).cloned());
        } else {
            changes.normalized_keys.extend(
                old_by_key
                    .keys()
                    .chain(new_by_key.keys())
                    .filter(|key| old_by_key.get(*key) != new_by_key.get(*key))
                    .cloned(),
            );
        }

        let old_declaration = old.filter(|page| has_page_declaration(page));
        let new_declaration = new.filter(|page| has_page_declaration(page));
        let declaration_rows_changed = declaration_rows(old) != declaration_rows(new);
        let declaration_name_changed = matches!(
            (old_declaration, new_declaration),
            (Some(old), Some(new)) if old.page.name != new.page.name
        );
        if declaration_rows_changed || declaration_name_changed {
            if let Some(page) = old_declaration {
                changes
                    .declaration_page_names
                    .insert(page.page.name.clone());
            }
            if let Some(page) = new_declaration {
                changes
                    .declaration_page_names
                    .insert(page.page.name.clone());
            }
        }
    }
    changes
}

fn page_metadata_sql(count: usize) -> String {
    debug_assert!(count > 0 && count <= PAGE_METADATA_BATCH_SIZE);
    let values = (1..=count)
        .map(|index| format!("(?{index})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "WITH requested(page_id) AS (VALUES {values}) \
         SELECT o.owner_type, o.owner_id, o.page_id, o.name, o.normalized_name, \
         o.value, o.ordinal, b.page_id, p.path, p.name, p.text_kind, \
         requested.page_id, p.page_id \
         FROM requested \
         LEFT JOIN pages p ON p.page_id = requested.page_id \
         LEFT JOIN properties o ON o.page_id = requested.page_id \
         LEFT JOIN blocks b ON o.owner_type = 1 AND b.block_id = o.owner_id"
    )
}

fn collect_page_metadata_row(
    row: &[PhysicalQueryValue],
    pages: &mut PageRegistryMetadata,
) -> Result<(), String> {
    let requested_id = blob16(row, 11, "requested.page_id")?;
    if is_null(row, 12) {
        if !is_null(row, 0) {
            return Err("registry property names an absent page".into());
        }
        return Ok(());
    }
    if blob16(row, 12, "pages.page_id")? != requested_id {
        return Err("registry page identity mismatch".into());
    }
    let meta = decode_page_meta(row, 8, 9, 10)?;
    let page = pages
        .entry(requested_id)
        .or_insert_with(|| RegistryPageMetadata {
            page_id: requested_id,
            page: meta.clone(),
            properties: Vec::new(),
        });
    if page.page != meta {
        return Err("inconsistent registry page metadata".into());
    }
    if !is_null(row, 0) {
        let decoded = decode_owner_row(row)?;
        let page_id = blob16(row, 2, "properties.page_id")?;
        if page_id != requested_id {
            return Err("registry property page identity mismatch".into());
        }
        page.properties.push(RegistryPropertyMetadata {
            owner_type: decoded.owner_type,
            owner_id: blob16(row, 1, "properties.owner_id")?,
            page_id,
            source_name: decoded.source_name,
            normalized_name: decoded.normalized_name,
            value: decoded.value,
            ordinal: decoded.ordinal,
        });
    }
    Ok(())
}

fn append_physical_properties(
    out: &mut Vec<RegistryPropertyMetadata>,
    owner_type: OwnerType,
    owner_id: [u8; 16],
    page_id: [u8; 16],
    properties: &[tine_storage::sqlite::PhysicalProperty],
) -> Result<(), String> {
    for (ordinal, property) in properties.iter().enumerate() {
        out.push(RegistryPropertyMetadata {
            owner_type,
            owner_id,
            page_id,
            source_name: property.name.clone(),
            normalized_name: property.normalized_name.clone(),
            value: property.value.clone(),
            ordinal: u32::try_from(ordinal)
                .map_err(|_| "physical property ordinal overflowed".to_owned())?,
        });
    }
    Ok(())
}

fn properties_by_key(
    page: &RegistryPageMetadata,
) -> BTreeMap<String, Vec<&RegistryPropertyMetadata>> {
    let mut by_key = BTreeMap::<String, Vec<&RegistryPropertyMetadata>>::new();
    for property in &page.properties {
        by_key
            .entry(metadata_key(property))
            .or_default()
            .push(property);
    }
    by_key
}

fn metadata_key(property: &RegistryPropertyMetadata) -> String {
    let source = if property.normalized_name.is_empty() {
        &property.source_name
    } else {
        &property.normalized_name
    };
    property_key_norm(source)
}

fn declaration_rows(page: Option<&RegistryPageMetadata>) -> Vec<&RegistryPropertyMetadata> {
    page.into_iter()
        .flat_map(|page| page.properties.iter())
        .filter(|property| {
            property.owner_type == OwnerType::Page && metadata_key(property) == DECLARED_TYPE_KEY
        })
        .collect()
}

fn has_page_declaration(page: &RegistryPageMetadata) -> bool {
    !declaration_rows(Some(page)).is_empty()
}

fn is_null(row: &[PhysicalQueryValue], at: usize) -> bool {
    matches!(row.get(at), Some(PhysicalQueryValue::Null))
}

fn collect_joined_row(
    row: &[PhysicalQueryValue],
    rows: &mut Vec<OwnerRow>,
    pages: &mut HashMap<String, PageMeta>,
) -> Result<(), String> {
    let owner = decode_owner_row(row)?;
    let meta = decode_page_meta(row, 8, 9, 10)?;
    match pages.get(&owner.page_id) {
        Some(existing) if existing != &meta => {
            return Err("inconsistent registry page metadata".into())
        }
        Some(_) => {}
        None => {
            pages.insert(owner.page_id.clone(), meta);
        }
    }
    rows.push(owner);
    Ok(())
}

/// Decode the physical property prefix shared by both the full and affected
/// streams. Ownership validation lives here once so neither adapter can omit
/// an absent block, mismatched page owner, or invalid ordinal.
fn decode_owner_row(row: &[PhysicalQueryValue]) -> Result<OwnerRow, String> {
    let owner_id = blob16(row, 1, "properties.owner_id")?;
    let page_id = blob16(row, 2, "properties.page_id")?;
    let (owner_type, prefix) = match integer(row, 0, "properties.owner_type")? {
        0 if owner_id == page_id => (OwnerType::Page, "p"),
        1 if blob16(row, 7, "blocks.page_id")? == page_id => (OwnerType::Block, "b"),
        _ => return Err("invalid registry property ownership".into()),
    };
    Ok(OwnerRow {
        owner_type,
        owner_id: format!("{prefix}:{}", hex16(owner_id)),
        page_id: page_key(page_id),
        source_name: text(row, 3, "properties.name")?,
        normalized_name: text(row, 4, "properties.normalized_name")?,
        value: text(row, 5, "properties.value")?,
        ordinal: u32::try_from(integer(row, 6, "properties.ordinal")?)
            .map_err(|_| "invalid registry property ordinal".to_owned())?,
    })
}

fn decode_page_meta(
    row: &[PhysicalQueryValue],
    path_at: usize,
    name_at: usize,
    kind_at: usize,
) -> Result<PageMeta, String> {
    let path = text(row, path_at, "pages.path")?;
    let name = text(row, name_at, "pages.name")?;
    if !matches!(integer(row, kind_at, "pages.text_kind")?, 0 | 1) {
        return Err("invalid registry page kind".into());
    }
    Ok(PageMeta {
        format: crate::model::Format::from_path(Path::new(&path)).into(),
        name,
    })
}

/// The projection registry's opaque snapshot-scoped page identity.
pub(crate) fn page_key(id: [u8; 16]) -> String {
    format!("page:{}", hex16(id))
}

/// The opaque physical-id spelling shared by the SQL reader and the retained
/// Direct facet adapter until that older adapter is retired.
pub(crate) fn hex16(id: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in id {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn text(row: &[PhysicalQueryValue], at: usize, field: &str) -> Result<String, String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Text(value)) => Ok(value.clone()),
        _ => Err(format!("invalid {field}")),
    }
}

fn integer(row: &[PhysicalQueryValue], at: usize, field: &str) -> Result<i64, String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Integer(value)) => Ok(*value),
        _ => Err(format!("invalid {field}")),
    }
}

fn blob16(row: &[PhysicalQueryValue], at: usize, field: &str) -> Result<[u8; 16], String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Blob(bytes)) if bytes.len() == 16 => {
            Ok(bytes.as_slice().try_into().expect("a checked 16-byte id"))
        }
        _ => Err(format!("invalid {field}")),
    }
}

fn visit(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    sql: &str,
    parameters: &[PhysicalQueryValue],
    mut row: impl FnMut(&[PhysicalQueryValue]) -> Result<(), String>,
) -> Result<(), QueryExecutionError> {
    #[cfg(test)]
    STATEMENTS.with(|count| count.set(count.get() + 1));
    let outcome = snapshot.visit_projection_query(sql, parameters, |values| {
        row(values)
            .map(|()| ControlFlow::Continue(()))
            .map_err(MaterializationError::Corrupt)
    });
    match outcome {
        Ok(()) => check_cancelled(snapshot),
        Err(_) if snapshot.cancellation().is_cancelled() => Err(QueryExecutionError::Cancelled),
        Err(MaterializationError::Corrupt(_)) => Err(invalid_snapshot()),
        Err(_) => Err(QueryExecutionError::Unavailable(
            QueryUnavailableReason::ReadFailed,
        )),
    }
}

fn check_cancelled(snapshot: &PhysicalProjectionQuerySnapshot) -> Result<(), QueryExecutionError> {
    if snapshot.cancellation().is_cancelled() {
        Err(QueryExecutionError::Cancelled)
    } else {
        Ok(())
    }
}

fn invalid_snapshot() -> QueryExecutionError {
    QueryExecutionError::Unavailable(QueryUnavailableReason::InvalidSnapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct_projection::QueryJobOpen;
    use crate::model::{Graph, PageEntry, PageKind};
    use crate::query::ir::{Cardinality, ObservedType};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tine_storage::sqlite::{
        PhysicalGraphProjectionChange, PhysicalGraphProjectionDatabase, PhysicalProperty,
    };
    use uuid::Uuid;

    static SERIAL: Mutex<()> = Mutex::new(());

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tine-registry-sql-{tag}-{}", Uuid::new_v4()))
    }

    fn projection_fixture(tag: &str) -> (PathBuf, PathBuf, Graph) {
        let root = scratch(tag);
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/Markdown.md"),
            "counted:: page-md\nunchanged:: stable\ngone:: old\n\n- markdown row\n  counted:: 10\n  mixed:: alpha, beta\n  score:: 1\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pages/Org.org"),
            "#+TITLE: Org Values\n:PROPERTIES:\n:counted: page-org\n:END:\n\n* org row\n:PROPERTIES:\n:counted: 20\n:mixed: [[Ref]]\n:score: 2\n:END:\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/Declaration A.md"), "tine.type:: text\n").unwrap();
        std::fs::write(root.join("pages/Declaration B.md"), "tine.type:: number\n").unwrap();

        let database = root.join("private/projection.sqlite");
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        let started = Instant::now();
        while !graph.direct_projection_ready_test() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "registry SQL fixture projection did not become ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        (root, database, graph)
    }

    fn open(database: &Path) -> PhysicalProjectionQuerySnapshot {
        PhysicalProjectionQuerySnapshot::open_direct(database, || Ok(())).unwrap()
    }

    fn build_base(database: &Path, config: &ParseConfig) -> Registry {
        read_registry(&mut open(database), config).unwrap()
    }

    fn physical_page(name: &str, path: &str, source: &str) -> PhysicalPage {
        let mut document = match crate::model::Format::from_path(Path::new(path)) {
            crate::model::Format::Md => crate::doc::parse(source),
            crate::model::Format::Org => crate::org::parse_org(source),
        };
        crate::model::assign_doc_runtime_ids(&mut document.roots, path);
        crate::direct_projection::physical_page_for_test(
            &PageEntry {
                name: name.to_owned(),
                kind: PageKind::Page,
                date_key: None,
                rel_path: path.to_owned(),
                path: PathBuf::from(path),
            },
            &document,
            &ParseConfig::default(),
        )
        .unwrap()
    }

    fn store_pages(tag: &str, pages: &[PhysicalPage]) -> (PathBuf, PathBuf) {
        let root = scratch(tag);
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("projection.sqlite");
        let mut projection = PhysicalGraphProjectionDatabase::open_writable(&database).unwrap();
        projection.initialize_schema().unwrap();
        projection
            .apply(&PhysicalGraphProjectionChange {
                replacements: pages.to_vec(),
                deletions: Vec::new(),
                reference_postings: Vec::new(),
            })
            .unwrap();
        drop(projection);
        (root, database)
    }

    fn metadata_of(pages: &[PhysicalPage]) -> PageRegistryMetadata {
        pages
            .iter()
            .map(|page| {
                (
                    page.page_id,
                    registry_metadata_from_physical_page(page).unwrap(),
                )
            })
            .collect()
    }

    fn requested(pages: &[PhysicalPage]) -> BTreeSet<[u8; 16]> {
        pages.iter().map(|page| page.page_id).collect()
    }

    fn affected(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn touched_page_reader_matches_actual_markdown_and_org_physical_pages() {
        let _serial = SERIAL.lock().unwrap();
        let markdown = physical_page(
            "Markdown Metadata",
            "pages/metadata.md",
            "Counted:: page-one\ncounted:: page-two\n\n- markdown block\n  Score:: 1\n  score:: 2\n",
        );
        let org = physical_page(
            "Org Metadata",
            "pages/metadata.org",
            ":PROPERTIES:\n:Counted: page-one\n:counted: page-two\n:END:\n\n* org block\n:PROPERTIES:\n:Score: 1\n:score: 2\n:END:\n",
        );
        let pages = vec![markdown, org];
        let (root, database) = store_pages("physical-parity", &pages);
        let actual = read_page_registry_metadata(&mut open(&database), &requested(&pages)).unwrap();
        let expected = metadata_of(&pages);
        assert_eq!(actual, expected);
        assert_eq!(
            actual[&pages[0].page_id].page.format,
            crate::query::atom::AtomFormat::Markdown
        );
        assert_eq!(
            actual[&pages[1].page_id].page.format,
            crate::query::atom::AtomFormat::Org
        );
        assert!(actual.values().all(|page| {
            page.properties
                .iter()
                .filter(|property| metadata_key(property) == "counted")
                .map(|property| property.ordinal)
                .collect::<BTreeSet<_>>()
                == BTreeSet::from([0, 1])
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn registry_change_diff_is_exact_per_key_and_ignores_ordinary_text() {
        let _serial = SERIAL.lock().unwrap();
        let old_page = physical_page(
            "Diff Page",
            "pages/diff.md",
            "Alpha:: one\nBeta:: two\n\n- old visible text\n  Score:: 1\n  score:: 2\n",
        );
        let text_only = physical_page(
            "Diff Page",
            "pages/diff.md",
            "Alpha:: one\nBeta:: two\n\n- completely different visible text\n  Score:: 1\n  score:: 2\n",
        );
        let before = metadata_of(std::slice::from_ref(&old_page));
        let after_text = metadata_of(std::slice::from_ref(&text_only));
        assert_eq!(
            registry_changes(&before, &after_text),
            RegistryChanges::default()
        );

        for mutate in [
            |property: &mut RegistryPropertyMetadata| property.value = "changed".into(),
            |property: &mut RegistryPropertyMetadata| property.source_name = "SCORE".into(),
            |property: &mut RegistryPropertyMetadata| property.ordinal = 99,
        ] {
            let mut after = before.clone();
            let property = after
                .get_mut(&old_page.page_id)
                .unwrap()
                .properties
                .iter_mut()
                .find(|property| metadata_key(property) == "score")
                .unwrap();
            mutate(property);
            assert_eq!(
                registry_changes(&before, &after).normalized_keys,
                affected(&["score"])
            );
        }

        let mut removed = before.clone();
        removed
            .get_mut(&old_page.page_id)
            .unwrap()
            .properties
            .retain(|property| metadata_key(property) != "beta");
        assert_eq!(
            registry_changes(&before, &removed).normalized_keys,
            affected(&["beta"])
        );

        let mut added_page = old_page.clone();
        added_page.properties.push(PhysicalProperty {
            name: "Gamma".into(),
            normalized_name: "gamma".into(),
            value: "three".into(),
        });
        let added = metadata_of(std::slice::from_ref(&added_page));
        assert_eq!(
            registry_changes(&before, &added).normalized_keys,
            affected(&["gamma"])
        );

        assert_eq!(
            registry_changes(&PageRegistryMetadata::new(), &before).normalized_keys,
            affected(&["alpha", "beta", "score"])
        );
        assert_eq!(
            registry_changes(&before, &PageRegistryMetadata::new()).normalized_keys,
            affected(&["alpha", "beta", "score"])
        );

        let mut renamed = before.clone();
        renamed.get_mut(&old_page.page_id).unwrap().page.name = "Renamed".into();
        assert_eq!(
            registry_changes(&before, &renamed),
            RegistryChanges::default()
        );

        let mut reformatted = before.clone();
        reformatted.get_mut(&old_page.page_id).unwrap().page.format =
            crate::query::atom::AtomFormat::Org;
        assert_eq!(
            registry_changes(&before, &reformatted).normalized_keys,
            affected(&["alpha", "beta", "score"])
        );
    }

    #[test]
    fn declaration_changes_report_raw_old_and_new_page_names_separately() {
        let _serial = SERIAL.lock().unwrap();
        let declaration = physical_page(
            "score",
            "pages/score.md",
            "tine.type:: number\n\n- declaration page\n",
        );
        let mut changed = declaration.clone();
        changed.properties[0].value = "text".into();
        let before = metadata_of(std::slice::from_ref(&declaration));
        let changed_metadata = metadata_of(std::slice::from_ref(&changed));
        assert_eq!(
            registry_changes(&before, &changed_metadata).declaration_page_names,
            affected(&["score"])
        );

        let mut renamed = changed_metadata.clone();
        renamed.get_mut(&declaration.page_id).unwrap().page.name = "Points".into();
        assert_eq!(
            registry_changes(&changed_metadata, &renamed).declaration_page_names,
            affected(&["score", "Points"])
        );
        assert_eq!(
            registry_changes(&PageRegistryMetadata::new(), &before).declaration_page_names,
            affected(&["score"])
        );
        assert_eq!(
            registry_changes(&before, &PageRegistryMetadata::new()).declaration_page_names,
            affected(&["score"])
        );

        let mut removed_declaration = declaration.clone();
        removed_declaration.properties.clear();
        assert_eq!(
            registry_changes(
                &before,
                &metadata_of(std::slice::from_ref(&removed_declaration))
            )
            .declaration_page_names,
            affected(&["score"])
        );

        let collision = physical_page("score", "pages/score-collision.md", "tine.type:: text\n");
        let collision_before = metadata_of(&[declaration.clone(), collision.clone()]);
        let collision_after = metadata_of(std::slice::from_ref(&collision));
        assert_eq!(
            registry_changes(&collision_before, &collision_after).declaration_page_names,
            affected(&["score"])
        );

        let block_only = physical_page(
            "block declaration",
            "pages/block-declaration.md",
            "- block\n  tine.type:: number\n",
        );
        let mut changed_block = block_only.clone();
        changed_block.blocks[0].properties[0].value = "text".into();
        let block_changes = registry_changes(
            &metadata_of(std::slice::from_ref(&block_only)),
            &metadata_of(std::slice::from_ref(&changed_block)),
        );
        assert_eq!(block_changes.normalized_keys, affected(&["tine.type"]));
        assert!(block_changes.declaration_page_names.is_empty());
    }

    #[test]
    fn direct_wrapper_and_shared_full_reader_return_the_same_registry() {
        let _serial = SERIAL.lock().unwrap();
        let (root, database, graph) = projection_fixture("direct-parity");
        let config = graph.config.parse_config();
        let projection = graph.direct_projection_test().unwrap();
        let QueryJobOpen::Job(mut job) = projection.open_query_job(graph.cache_generation()) else {
            panic!("ready fixture must admit a Direct query job");
        };
        let through_job = job.read_registry(&config).unwrap();
        let shared = read_registry(&mut open(&database), &config).unwrap();
        assert!(through_job.rows_equal(&shared));
        drop(job);
        drop(graph);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn affected_patch_matches_full_rebuild_across_values_counts_and_declarations() {
        let _serial = SERIAL.lock().unwrap();
        let (root, database, graph) = projection_fixture("patch-parity");
        let config = graph.config.parse_config();
        let base = build_base(&database, &config).with_generation(37);
        assert!(base.row("score").unwrap().declared.is_none());
        let unchanged = base.row("unchanged").unwrap().clone();
        drop(graph);

        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE properties SET value = '99' WHERE normalized_name = 'score' AND value = '1'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE properties SET value = 'gamma, delta' WHERE normalized_name = 'mixed' AND value = 'alpha, beta'",
                [],
            )
            .unwrap();
        connection
            .execute("DELETE FROM properties WHERE normalized_name = 'gone'", [])
            .unwrap();
        connection
            .execute(
                "UPDATE pages SET name = 'score' WHERE path IN ('pages/Declaration A.md', 'pages/Declaration B.md')",
                [],
            )
            .unwrap();
        drop(connection);

        let requested = affected(&[
            " SCORE ",
            "score",
            "mixed",
            "gone",
            "counted",
            "",
            "tine.type",
        ]);
        let patched =
            patch_registry_from_snapshot(&mut open(&database), &base, &requested, &config).unwrap();
        let full = read_registry(&mut open(&database), &config).unwrap();
        assert!(
            patched.rows_equal(&full),
            "affected-key patch must equal a full same-snapshot rebuild"
        );
        assert_eq!(patched.generation(), 37);
        assert_eq!(patched.config_digest(), base.config_digest());
        assert_eq!(patched.row("unchanged"), Some(&unchanged));
        assert!(patched.row("gone").is_none());

        let counted = patched.row("counted").unwrap();
        assert_eq!((counted.count_pages, counted.count_blocks), (2, 2));
        assert_eq!(counted.cardinality, Cardinality::One);
        let mixed = patched.row("mixed").unwrap();
        assert_eq!(mixed.count_blocks, 2);
        assert!(
            mixed.top_values.iter().any(|(value, _)| value == "Ref"),
            "the Org reference value must be decoded by the shared builder"
        );
        let score = patched.row("score").unwrap();
        assert!(
            matches!(
                score.declared,
                Some((ObservedType::Text | ObservedType::Number, Cardinality::One))
            ),
            "colliding declaration pages must retain the full stream's last-write result"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn affected_patch_does_not_decode_unrelated_property_rows() {
        let _serial = SERIAL.lock().unwrap();
        let (root, database, graph) = projection_fixture("bounded-rows");
        let config = graph.config.parse_config();
        let base = build_base(&database, &config);
        drop(graph);

        // Corrupt an unrelated row: a full reader must reject it, whereas a
        // score-only patch must never fetch or decode it. This distinguishes
        // restricted SQL reads from a full stream filtered in Rust.
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        assert_eq!(
            connection
                .execute(
                    "UPDATE properties SET ordinal = -1 WHERE normalized_name = 'unchanged'",
                    [],
                )
                .unwrap(),
            1
        );
        drop(connection);

        reset_statement_count();
        let patched = patch_registry_from_snapshot(
            &mut open(&database),
            &base,
            &affected(&["score"]),
            &config,
        )
        .unwrap();
        assert!(patched.rows_equal(&base));
        assert_eq!(statement_count(), 2, "one key stream plus declarations");
        assert!(matches!(
            read_registry(&mut open(&database), &config),
            Err(QueryExecutionError::Unavailable(
                QueryUnavailableReason::InvalidSnapshot
            ))
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_patch_is_statement_free_but_still_honors_digest_and_cancellation() {
        let _serial = SERIAL.lock().unwrap();
        let (root, database, graph) = projection_fixture("empty");
        let config = graph.config.parse_config();
        let base = build_base(&database, &config);
        drop(graph);

        let mut snapshot = open(&database);
        reset_statement_count();
        let same =
            patch_registry_from_snapshot(&mut snapshot, &base, &BTreeSet::new(), &config).unwrap();
        assert!(same.rows_equal(&base));
        assert_eq!(statement_count(), 0);

        let mut changed_config = config.clone();
        changed_config.hidden_properties.push("score".into());
        let mut snapshot = open(&database);
        reset_statement_count();
        assert!(matches!(
            patch_registry_from_snapshot(&mut snapshot, &base, &BTreeSet::new(), &changed_config,),
            Err(QueryExecutionError::Unavailable(
                QueryUnavailableReason::InvalidSnapshot
            ))
        ));
        assert_eq!(statement_count(), 0);

        let mut snapshot = open(&database);
        snapshot.cancellation().cancel();
        reset_statement_count();
        assert!(matches!(
            patch_registry_from_snapshot(&mut snapshot, &base, &BTreeSet::new(), &config),
            Err(QueryExecutionError::Cancelled)
        ));
        assert_eq!(statement_count(), 0);

        let _ = std::fs::remove_dir_all(root);
    }

    fn assert_patch_rejects_damage(tag: &str, damage: &str) {
        let (root, database, graph) = projection_fixture(tag);
        let config = graph.config.parse_config();
        let base = build_base(&database, &config);
        drop(graph);
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        connection.execute_batch(damage).unwrap();
        drop(connection);
        assert!(matches!(
            patch_registry_from_snapshot(
                &mut open(&database),
                &base,
                &affected(&["score"]),
                &config,
            ),
            Err(QueryExecutionError::Unavailable(
                QueryUnavailableReason::InvalidSnapshot
            ))
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn affected_patch_rejects_missing_block_missing_page_and_invalid_ordinal() {
        let _serial = SERIAL.lock().unwrap();
        assert_patch_rejects_damage(
            "missing-block",
            "DELETE FROM blocks WHERE block_id IN (SELECT owner_id FROM properties WHERE owner_type = 1 AND normalized_name = 'score');",
        );
        assert_patch_rejects_damage(
            "missing-page",
            "DELETE FROM pages WHERE page_id IN (SELECT page_id FROM properties WHERE normalized_name = 'score');",
        );
        assert_patch_rejects_damage(
            "invalid-ordinal",
            "UPDATE properties SET ordinal = -1 WHERE normalized_name = 'score';",
        );
    }

    fn assert_metadata_rejects_damage(tag: &str, damage: &str) {
        let page = physical_page(
            "Damaged Metadata",
            &format!("pages/{tag}.md"),
            "Score:: page\n\n- block\n  Score:: block\n",
        );
        let page_id = page.page_id;
        let (root, database) = store_pages(tag, &[page]);
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        connection.execute_batch(damage).unwrap();
        drop(connection);
        assert!(matches!(
            read_page_registry_metadata(&mut open(&database), &BTreeSet::from([page_id])),
            Err(QueryExecutionError::Unavailable(
                QueryUnavailableReason::InvalidSnapshot
            ))
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn touched_page_reader_rejects_damaged_owner_page_and_ordinal() {
        let _serial = SERIAL.lock().unwrap();
        assert_metadata_rejects_damage(
            "metadata-owner",
            "UPDATE properties SET owner_id = randomblob(16) WHERE owner_type = 0;",
        );
        assert_metadata_rejects_damage(
            "metadata-page",
            "DELETE FROM pages WHERE page_id IN (SELECT page_id FROM properties LIMIT 1);",
        );
        assert_metadata_rejects_damage(
            "metadata-ordinal",
            "UPDATE properties SET ordinal = -1 WHERE normalized_name = 'score';",
        );
    }

    #[test]
    fn touched_page_reader_is_bounded_statement_free_when_empty_and_cancellable() {
        let _serial = SERIAL.lock().unwrap();
        let wanted = physical_page("Wanted", "pages/wanted.md", "Wanted:: valid\n");
        let unrelated = physical_page("Unrelated", "pages/unrelated.md", "Broken:: corrupt me\n");
        let (root, database) = store_pages("bounded-page-metadata", &[wanted.clone(), unrelated]);
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        connection
            .execute(
                "UPDATE properties SET ordinal = -1 WHERE normalized_name = 'broken'",
                [],
            )
            .unwrap();
        drop(connection);

        reset_statement_count();
        let rows = read_page_registry_metadata(
            &mut open(&database),
            &BTreeSet::from([wanted.page_id, [0xff; 16]]),
        )
        .unwrap();
        assert_eq!(rows, metadata_of(std::slice::from_ref(&wanted)));
        assert_eq!(statement_count(), 1);

        reset_statement_count();
        assert!(
            read_page_registry_metadata(&mut open(&database), &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        assert_eq!(statement_count(), 0);

        let mut cancelled = open(&database);
        cancelled.cancellation().cancel();
        reset_statement_count();
        assert!(matches!(
            read_page_registry_metadata(&mut cancelled, &BTreeSet::new()),
            Err(QueryExecutionError::Cancelled)
        ));
        assert_eq!(statement_count(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn touched_page_reader_batches_more_than_128_requested_pages() {
        let _serial = SERIAL.lock().unwrap();
        let pages = (0..129)
            .map(|index| {
                physical_page(
                    &format!("Page {index}"),
                    &format!("pages/batch-{index}.md"),
                    "",
                )
            })
            .collect::<Vec<_>>();
        let (root, database) = store_pages("metadata-batches", &pages);
        reset_statement_count();
        let actual = read_page_registry_metadata(&mut open(&database), &requested(&pages)).unwrap();
        assert_eq!(actual, metadata_of(&pages));
        assert_eq!(
            statement_count(),
            2,
            "129 page IDs require two bounded batches"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
