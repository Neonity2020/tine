//! One plan-to-SQL lowering for simple-query page candidates.
//!
//! Managed Storage and Direct Files expose differently typed leases over the
//! same `SqliteGraphProjectionRead`. The adapters below erase only those ID and
//! cursor wrappers; this module's single `SimpleQueryCandidateSource` match is
//! the producer that decides which existing read family answers each source.

use std::collections::{BTreeSet, HashSet};

use tine_storage::sqlite::{
    MaterializationError as PhysicalReadError, PhysicalEntityId, SqliteGraphProjectionRead,
};

use crate::oplog::{
    BlockId, ManagedPath, ManagedTextKind, MaterializedEntityId, PageId, SqliteMaterializedRead,
};
use crate::query::{SimpleQueryCandidatePlan as Plan, SimpleQueryCandidateSource as Source};

const BATCH: usize = 512;

pub(crate) struct LoweredSimpleQueryCandidates {
    pub(crate) page_ids: BTreeSet<[u8; 16]>,
    pub(crate) inventory_pages: usize,
    pub(crate) inventory_scanned: bool,
}

pub(crate) enum SimpleQueryLoweringError<E> {
    Task(E),
    PageRef(E),
    BlockProperty(E),
    PageProperty(E),
    Navigation(E),
}

pub(crate) trait SimpleQuerySqlRead {
    type Error;

    fn task_candidate_pages(
        &self,
        marker: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error>;

    fn page_referrer_candidates(
        &self,
        normalized: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error>;

    fn block_property_candidates(
        &self,
        normalized: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error>;

    fn page_property_candidates(
        &self,
        emit: &mut dyn FnMut([u8; 16], &str),
    ) -> Result<(), Self::Error>;

    fn navigation_pages(
        &self,
        emit: &mut dyn FnMut([u8; 16], &str, bool),
    ) -> Result<(), Self::Error>;
}

pub(crate) fn lower_simple_query_candidate_plan<R: SimpleQuerySqlRead>(
    read: &R,
    plan: &Plan,
    masked_page_ids: &HashSet<[u8; 16]>,
) -> Result<LoweredSimpleQueryCandidates, SimpleQueryLoweringError<R::Error>> {
    let (sources, scan_all) = match plan {
        Plan::Empty => {
            return Ok(LoweredSimpleQueryCandidates {
                page_ids: BTreeSet::new(),
                inventory_pages: 0,
                inventory_scanned: false,
            })
        }
        Plan::Indexed(sources) => (sources.as_slice(), false),
        Plan::All => (&[][..], true),
    };

    let mut page_ids = BTreeSet::new();
    let mut insert = |page_id| {
        if !masked_page_ids.contains(&page_id) {
            page_ids.insert(page_id);
        }
    };
    for source in sources {
        match source {
            Source::Task(marker) => read
                .task_candidate_pages(marker, &mut insert)
                .map_err(SimpleQueryLoweringError::Task)?,
            Source::PageRef(normalized) => read
                .page_referrer_candidates(normalized, &mut insert)
                .map_err(SimpleQueryLoweringError::PageRef)?,
            Source::BlockProperty(normalized) => read
                .block_property_candidates(normalized, &mut insert)
                .map_err(SimpleQueryLoweringError::BlockProperty)?,
            Source::PageProperty(_) | Source::Page(_) | Source::Namespace(_) | Source::Journal => {}
        }
    }

    let page_property_keys = sources
        .iter()
        .filter_map(|source| match source {
            Source::PageProperty(key) => Some(key.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    if !page_property_keys.is_empty() {
        read.page_property_candidates(&mut |page_id, normalized| {
            if page_property_keys.contains(normalized) {
                insert(page_id);
            }
        })
        .map_err(SimpleQueryLoweringError::PageProperty)?;
    }

    let needs_inventory = scan_all
        || sources.iter().any(|source| {
            matches!(
                source,
                Source::PageRef(_) | Source::Page(_) | Source::Namespace(_) | Source::Journal
            )
        });
    let mut inventory_pages = 0_usize;
    if needs_inventory {
        read.navigation_pages(&mut |page_id, name_key, is_journal| {
            inventory_pages = inventory_pages.saturating_add(1);
            let matches = scan_all
                || sources.iter().any(|source| match source {
                    Source::PageRef(name) | Source::Page(name) => name_key == name,
                    Source::Namespace(namespace) => name_key.starts_with(&format!("{namespace}/")),
                    Source::Journal => is_journal,
                    _ => false,
                });
            if matches {
                insert(page_id);
            }
        })
        .map_err(SimpleQueryLoweringError::Navigation)?;
    }

    Ok(LoweredSimpleQueryCandidates {
        page_ids,
        inventory_pages,
        inventory_scanned: needs_inventory,
    })
}

impl SimpleQuerySqlRead for SqliteMaterializedRead<'_> {
    type Error = crate::oplog::sqlite_materialization::MaterializationError;

    fn task_candidate_pages(
        &self,
        marker: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error> {
        let mut cursor = None;
        loop {
            let rows = self.task_candidate_pages_after(marker, cursor, BATCH)?;
            let len = rows.len();
            for row in rows {
                cursor = Some(row.page_id);
                emit(row.page_id.as_uuid().into_bytes());
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }

    fn page_referrer_candidates(
        &self,
        normalized: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error> {
        let mut cursor = None;
        loop {
            let rows = self.page_referrer_candidates_after(normalized, cursor, BATCH)?;
            let len = rows.len();
            for row in rows {
                cursor = Some((row.source_page_id, row.source));
                emit(row.source_page_id.as_uuid().into_bytes());
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }

    fn block_property_candidates(
        &self,
        normalized: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error> {
        let mut cursor: Option<(PageId, BlockId)> = None;
        loop {
            let rows = self.block_property_candidates_after(normalized, cursor, BATCH)?;
            let len = rows.len();
            for row in rows {
                cursor = Some((row.page_id, row.block_id));
                emit(row.page_id.as_uuid().into_bytes());
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }

    fn page_property_candidates(
        &self,
        emit: &mut dyn FnMut([u8; 16], &str),
    ) -> Result<(), Self::Error> {
        let mut cursor = None;
        loop {
            let rows = self.property_facet_rows_after(false, cursor.clone(), BATCH)?;
            let len = rows.len();
            for row in rows {
                cursor = Some((row.owner, row.source_name.clone(), row.ordinal));
                if matches!(row.owner, MaterializedEntityId::Page(_)) {
                    emit(row.page_id.as_uuid().into_bytes(), &row.normalized_name);
                }
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }

    fn navigation_pages(
        &self,
        emit: &mut dyn FnMut([u8; 16], &str, bool),
    ) -> Result<(), Self::Error> {
        let mut cursor: Option<(ManagedPath, PageId)> = None;
        loop {
            let rows = self.navigation_pages_after(
                cursor.as_ref().map(|(path, page_id)| (path, *page_id)),
                BATCH,
            )?;
            let len = rows.len();
            for row in rows {
                cursor = Some((row.path.clone(), row.page_id));
                emit(
                    row.page_id.as_uuid().into_bytes(),
                    &row.name_key,
                    row.kind == ManagedTextKind::Journal,
                );
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }
}

impl SimpleQuerySqlRead for SqliteGraphProjectionRead<'_> {
    type Error = PhysicalReadError;

    fn task_candidate_pages(
        &self,
        marker: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error> {
        let mut cursor = None;
        loop {
            let rows = self.task_candidate_pages_after(marker, cursor, BATCH)?;
            let len = rows.len();
            for row in rows {
                cursor = Some(row.page_id);
                emit(row.page_id);
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }

    fn page_referrer_candidates(
        &self,
        normalized: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error> {
        let mut cursor = None;
        loop {
            let rows = self.page_referrer_candidates_after(normalized, cursor, BATCH)?;
            let len = rows.len();
            for row in rows {
                cursor = Some((row.source_page_id, row.source));
                emit(row.source_page_id);
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }

    fn block_property_candidates(
        &self,
        normalized: &str,
        emit: &mut dyn FnMut([u8; 16]),
    ) -> Result<(), Self::Error> {
        let mut cursor = None;
        loop {
            let rows = self.block_property_candidates_after(normalized, cursor, BATCH)?;
            let len = rows.len();
            for row in rows {
                cursor = Some((row.page_id, row.block_id));
                emit(row.page_id);
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }

    fn page_property_candidates(
        &self,
        emit: &mut dyn FnMut([u8; 16], &str),
    ) -> Result<(), Self::Error> {
        let mut cursor = None;
        loop {
            let rows = self.property_facet_rows_after(false, cursor.clone(), BATCH)?;
            let len = rows.len();
            for row in rows {
                cursor = Some((row.owner, row.source_name.clone(), row.ordinal));
                if matches!(row.owner, PhysicalEntityId::Page(_)) {
                    emit(row.page_id, &row.normalized_name);
                }
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }

    fn navigation_pages(
        &self,
        emit: &mut dyn FnMut([u8; 16], &str, bool),
    ) -> Result<(), Self::Error> {
        let mut cursor: Option<(String, [u8; 16])> = None;
        loop {
            let rows = self.navigation_pages_after_with_header_validation(
                cursor.as_ref().map(|(path, _)| path.as_str()),
                cursor.as_ref().map(|(_, page_id)| page_id),
                BATCH,
                |_, kind| match kind {
                    0 | 1 => Ok(()),
                    _ => Err(PhysicalReadError::Corrupt(format!(
                        "unknown Direct Files text kind {kind}"
                    ))),
                },
            )?;
            let len = rows.len();
            for row in rows {
                cursor = Some((row.path, row.page_id));
                emit(row.page_id, &row.name_key, row.text_kind == 1);
            }
            if len < BATCH {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn source_to_sql_read_family_has_one_producer_file() {
        let files = [
            include_str!("query_lowering.rs"),
            include_str!("../sync_runtime.rs"),
            include_str!("../model.rs"),
            include_str!("../direct_projection.rs"),
            include_str!("../query.rs"),
        ];
        for (source, read_family) in [
            ("Source::Task", "task_candidate_pages_after"),
            ("Source::PageRef", "page_referrer_candidates_after"),
            ("Source::BlockProperty", "block_property_candidates_after"),
            ("Source::PageProperty", "property_facet_rows_after"),
        ] {
            assert_eq!(
                files
                    .iter()
                    .filter(|file| file.contains(source) && file.contains(read_family))
                    .count(),
                1,
                "{source} to {read_family} must be lowered in exactly one file"
            );
        }
    }
}
