//! Registry construction over one read-only physical projection snapshot.
//!
//! [`read_registry`] is the shared SQL adapter used by the Direct query job.
//! [`patch_registry_from_snapshot`] is the corresponding affected-key rebuild
//! primitive for a later committed-registry owner: it is deliberately not
//! wired to cache publication or dirty-key production in this packet.

use std::collections::{BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::path::Path;

use tine_storage::sqlite::{
    MaterializationError, PhysicalProjectionQuerySnapshot, PhysicalQueryValue,
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
    use crate::model::Graph;
    use crate::query::ir::{Cardinality, ObservedType};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
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

    fn affected(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
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
}
