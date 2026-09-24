//! Narrow SQL sources for launch-time derived answers. Documents here contain
//! only selected blocks, never a parsed or retained whole-graph snapshot.
use super::*;
use crate::doc::{DocBlock, Document};
use crate::query::results::{
    read_admitted_payload, resolve_identity, PayloadChannel, PayloadFacts,
};

pub(crate) enum DerivedSelection<'a> {
    Resolve(&'a [String]),
    Preview(&'a [String]),
    Referrers(&'a str),
    Templates,
}

pub(crate) struct DerivedPage {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) kind: PageKind,
    /// The stored `pages.journal_day`, which is the parse's `date_key` by
    /// construction. A journal's day is read from its row, never parsed back
    /// from its name: a title format without a year cannot parse the name it
    /// wrote (GH #543, audit R13-06).
    pub(crate) journal_day: Option<i64>,
    pub(crate) document: Document,
    pub(crate) session_ids: Option<(String, Vec<(String, usize)>)>,
}

/// A row whose shape the schema forbids contradicts the image, as query
/// dispatch reads it (audit R12-05).
fn invalid() -> tine_storage::sqlite::MaterializationError {
    tine_storage::sqlite::MaterializationError::Corrupt("invalid derived row".into())
}

/// A stored `text_kind`, decoded where it is read so an unknown value is
/// reported as damage (audit R13-05). Decoding it later, in the model, turned
/// it into `None` that no one reported, and the read parsed the graph.
pub(crate) fn page_kind(kind: i64) -> Result<PageKind, tine_storage::sqlite::MaterializationError> {
    super::page_kind_from_sql(kind).ok_or_else(|| {
        tine_storage::sqlite::MaterializationError::Corrupt(format!(
            "unknown Direct Files text kind {kind}"
        ))
    })
}

fn optional_integer(
    row: &[PhysicalQueryValue],
    at: usize,
) -> Result<Option<i64>, tine_storage::sqlite::MaterializationError> {
    match row.get(at) {
        Some(PhysicalQueryValue::Null) => Ok(None),
        _ => integer(row, at).map(Some),
    }
}
fn text(
    row: &[PhysicalQueryValue],
    at: usize,
) -> Result<String, tine_storage::sqlite::MaterializationError> {
    match row.get(at) {
        Some(PhysicalQueryValue::Text(s)) => Ok(s.clone()),
        _ => Err(invalid()),
    }
}
fn integer(
    row: &[PhysicalQueryValue],
    at: usize,
) -> Result<i64, tine_storage::sqlite::MaterializationError> {
    match row.get(at) {
        Some(PhysicalQueryValue::Integer(n)) => Ok(*n),
        _ => Err(invalid()),
    }
}

struct BlockRow {
    block_id: i64,
    page_id: i64,
    parent: Option<i64>,
    result_id: String,
    estimate: usize,
    tags: usize,
    properties: usize,
    page: usize,
}

/// Whether every row of the stored image at `path` was lowered under the
/// facts version and parse configuration `prefix` names (the part of a
/// [`projection_source_revision`] before the content revision). An empty or
/// unreadable image is not: nothing is served from it.
pub(super) fn stored_facts_are(path: &Path, prefix: &str) -> bool {
    let Ok(mut snapshot) = PhysicalProjectionQuerySnapshot::open_direct(path, || Ok(())) else {
        return false;
    };
    let mut rows = 0_i64;
    let mut foreign = 0_i64;
    let read = crate::query::projection_sql::visit(
        &mut snapshot,
        "SELECT count(*), coalesce(sum(substr(revision, 1, ?) <> ?), 0) \
         FROM direct_source_revisions",
        &[
            PhysicalQueryValue::Integer(prefix.chars().count() as i64),
            PhysicalQueryValue::Text(prefix.to_owned()),
        ],
        |row| {
            rows = integer(row, 0)?;
            foreign = integer(row, 1)?;
            Ok(std::ops::ControlFlow::Break(()))
        },
    );
    read.is_ok() && rows > 0 && foreign == 0
}

impl DirectProjection {
    pub(crate) fn derived_pages(
        &self,
        at: ReadAt,
        selection: &DerivedSelection<'_>,
    ) -> Option<Vec<DerivedPage>> {
        let _reader = self.shared_reader_at(at)?;
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)?;
        let identity =
            super::capture_result_identity(&self.shared, &mut snapshot).reported(self)?;
        let mut seeds = std::collections::BTreeSet::new();
        let mut read_seeds = |sql: &str, params: &[PhysicalQueryValue]| {
            crate::query::projection_sql::visit(&mut snapshot, sql, params, |row| {
                seeds.insert(integer(row, 0)?);
                Ok(std::ops::ControlFlow::Continue(()))
            })
            .reported(self)
        };
        match selection {
            DerivedSelection::Resolve(ids) | DerivedSelection::Preview(ids) => {
                for id in ids.iter().collect::<std::collections::BTreeSet<_>>() {
                    let uuid = uuid::Uuid::parse_str(id.trim())
                        .ok()
                        .map(|id| PhysicalQueryValue::Blob(id.as_bytes().to_vec()))
                        .unwrap_or(PhysicalQueryValue::Null);
                    let spelling = PhysicalQueryValue::Text(id.clone());
                    // A live id this session gave a block is stored as the
                    // block's structural id (R3).
                    let stored = PhysicalQueryValue::Text(identity.stored_id(id).to_owned());
                    read_seeds(
                        "SELECT block_id FROM blocks WHERE result_id = ? OR logseq_uuid = ? OR block_id IN (SELECT o.owner_id FROM properties o JOIN names n ON n.name_id = o.name_id WHERE o.owner_type = 1 AND n.key = 'id' AND o.value = ?)",
                        &[stored, uuid, spelling],
                    )?;
                }
            }
            DerivedSelection::Referrers(id) => {
                if let Ok(uuid) = uuid::Uuid::parse_str(id.trim()) {
                    read_seeds("SELECT source_entity_id FROM reference_postings WHERE target_type = 1 AND source_entity_type = 1 AND raw_uuid_claim = ?", &[PhysicalQueryValue::Blob(uuid.as_bytes().to_vec())])?;
                }
            }
            DerivedSelection::Templates => {
                read_seeds("SELECT o.owner_id FROM properties o JOIN names n ON n.name_id = o.name_id WHERE o.owner_type = 1 AND n.key = 'template' AND o.value <> ''", &[])?;
            }
        }
        let mut selected = seeds.clone();
        // Ancestors retain the parser's breadcrumb and non-overlapping referrer
        // semantics. Templates/previews retain the complete selected subtree.
        let relatives = match selection {
            DerivedSelection::Referrers(_) => Some("WITH RECURSIVE relatives(block_id) AS (SELECT parent_block_id FROM blocks WHERE block_id = ? UNION SELECT b.parent_block_id FROM blocks b JOIN relatives r ON b.block_id = r.block_id) SELECT block_id FROM relatives WHERE block_id IS NOT NULL"),
            DerivedSelection::Templates | DerivedSelection::Preview(_) => Some("WITH RECURSIVE relatives(block_id) AS (SELECT block_id FROM blocks WHERE block_id = ? UNION SELECT b.block_id FROM blocks b JOIN relatives r ON b.parent_block_id = r.block_id) SELECT block_id FROM relatives"),
            DerivedSelection::Resolve(_) => None,
        };
        if let Some(sql) = relatives {
            for seed in seeds {
                crate::query::projection_sql::visit(
                    &mut snapshot,
                    sql,
                    &[PhysicalQueryValue::Integer(seed)],
                    |row| {
                        selected.insert(integer(row, 0)?);
                        Ok(std::ops::ControlFlow::Continue(()))
                    },
                )
                .reported(self)?;
            }
        }
        let mut pages: Vec<DerivedPage> = Vec::new();
        let mut page_indices = HashMap::new();
        let mut rows = Vec::new();
        for chunk in selected.into_iter().collect::<Vec<_>>().chunks(128) {
            let params = chunk
                .iter()
                .map(|id| PhysicalQueryValue::Integer(*id))
                .collect::<Vec<_>>();
            let sql = format!("SELECT b.block_id, b.page_id, b.parent_block_id, b.result_id, b.estimated_bytes, b.tag_count, b.property_count, b.order_key, p.path, n.raw, p.text_kind, p.journal_day FROM blocks b JOIN pages p ON p.page_id = b.page_id JOIN names n ON n.name_id = p.name_id WHERE b.block_id IN ({}) ORDER BY p.path, b.preorder", vec!["?"; chunk.len()].join(", "));
            crate::query::projection_sql::visit(&mut snapshot, &sql, &params, |row| {
                let path = text(row, 8)?;
                let page = *page_indices.entry(path.clone()).or_insert_with(|| {
                    let index = pages.len();
                    pages.push(DerivedPage {
                        name: String::new(),
                        path: path.clone(),
                        kind: PageKind::Page,
                        journal_day: None,
                        document: Document::default(),
                        session_ids: None,
                    });
                    index
                });
                pages[page].name = text(row, 9)?;
                pages[page].kind = page_kind(integer(row, 10)?)?;
                pages[page].journal_day = optional_integer(row, 11)?;
                let page_id = integer(row, 1)?;
                let (result_id, estimate) = resolve_identity(
                    &identity,
                    page_id,
                    &path,
                    &text(row, 7)?,
                    &text(row, 3)?,
                    integer(row, 4)? as usize,
                )
                .map_err(|_| invalid())?;
                rows.push((
                    text(row, 7)?,
                    BlockRow {
                        block_id: integer(row, 0)?,
                        page_id,
                        parent: match row.get(2) {
                            Some(PhysicalQueryValue::Null) => None,
                            _ => Some(integer(row, 2)?),
                        },
                        result_id,
                        estimate,
                        tags: integer(row, 5)? as usize,
                        properties: integer(row, 6)? as usize,
                        page,
                    },
                ));
                Ok(std::ops::ControlFlow::Continue(()))
            })
            .reported(self)?;
        }
        // Deterministic physical path/preorder wins for duplicate claimants.
        rows.sort_by(|a, b| {
            pages[a.1.page]
                .path
                .cmp(&pages[b.1.page].path)
                .then_with(|| a.0.cmp(&b.0))
        });
        let rows = rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>();
        let mut blocks: HashMap<i64, DocBlock> = HashMap::new();
        read_admitted_payload(
            &mut snapshot,
            &rows,
            PayloadChannel::Selection,
            |row| PayloadFacts {
                block_id: row.block_id,
                page_id: row.page_id,
                result_id: &row.result_id,
                estimated_bytes: row.estimate,
                tag_count: row.tags,
                property_count: row.properties,
            },
            |at, dto| {
                let is_org = crate::vocab::Format::from_path(Path::new(&pages[rows[at].page].path))
                    == crate::vocab::Format::Org;
                blocks.insert(
                    rows[at].block_id,
                    crate::vocab::dto_block_to_doc_block(&dto, is_org),
                );
            },
        )
        .reported(self)?;
        for row in rows.iter().rev() {
            let mut block = blocks.remove(&row.block_id)?;
            block.children.reverse();
            if let Some(parent) = row.parent.and_then(|id| blocks.get_mut(&id)) {
                parent.children.push(block);
            } else {
                pages[row.page].document.roots.push(block);
            }
        }
        for page in &mut pages {
            page.document.roots.reverse();
        }
        pages.sort_by(|a, b| a.path.cmp(&b.path));
        // SQL results also hand runtime ids to callers. Retain the complete
        // compact topology of those specific pages so a later lookup can find
        // an id that only exists structurally in this session, without text I/O.
        if !matches!(selection, DerivedSelection::Templates) {
            for page in &mut pages {
                let mut preorder = Vec::new();
                let mut revision = None;
                crate::query::projection_sql::visit(&mut snapshot,
                    "SELECT s.revision, b.result_id, b.order_key, b.estimated_bytes, b.page_id, (SELECT COUNT(*) FROM blocks child WHERE child.parent_block_id = b.block_id) FROM pages p JOIN direct_source_revisions s ON s.path = p.path JOIN blocks b ON b.page_id = p.page_id WHERE p.path = ? ORDER BY b.preorder",
                    &[PhysicalQueryValue::Text(page.path.clone())], |row| {
                        revision = Some(text(row, 0)?);
                        let (id, _) = resolve_identity(&identity, integer(row, 4)?, &page.path, &text(row, 2)?, &text(row, 1)?, integer(row, 3)? as usize).map_err(|_| invalid())?;
                        preorder.push((id, integer(row, 5)? as usize));
                        Ok(std::ops::ControlFlow::Continue(()))
                    }).reported(self)?;
                page.session_ids = revision.map(|revision| (revision, preorder));
            }
        }
        self.answers(at).then_some(pages)
    }

    /// Whether the index holds, or is being given, content `revision` of the
    /// page at graph-relative `rel` under the parse configuration `digest`.
    /// The one answer to "does the index have these bytes" (design §7): the
    /// page's queued or in-flight mark if it has one, else the stored image's
    /// row. While a fresh build is queued or running, the snapshot it builds
    /// from answers; a fresh image owed without one answers no: the row read
    /// would be the image's that is going away. Before the worker has set up the
    /// image is read as it is: if the worker then finds it damaged, the fresh
    /// build it owes takes every page from a complete snapshot.
    pub(crate) fn holds_source_revision(
        &self,
        _generation: u64,
        rel: &str,
        revision: &str,
        digest: &tine_storage::ContentDigest,
    ) -> bool {
        {
            let pending = self.shared.pending.lock().unwrap();
            if let Some(delta) = pending.queued(rel) {
                return delta.carries(revision, digest);
            }
            if pending.full.is_some() || pending.building {
                return pending
                    .snapshot
                    .as_ref()
                    .is_some_and(|(revisions, config)| {
                        config == digest && revisions.get(rel).is_some_and(|held| held == revision)
                    });
            }
            if pending.rebuild {
                return false;
            }
        }
        self.stored_revision_is(rel, &projection_source_revision(revision, digest.clone()))
    }

    /// Whether the stored image's row for `rel` is at exactly `revision`.
    fn stored_revision_is(&self, rel: &str, revision: &str) -> bool {
        let Some(mut snapshot) =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)
        else {
            return false;
        };
        let mut held = false;
        let read = crate::query::projection_sql::visit(
            &mut snapshot,
            "SELECT revision FROM direct_source_revisions WHERE path = ?",
            &[PhysicalQueryValue::Text(rel.to_owned())],
            |row| {
                held = matches!(row, [PhysicalQueryValue::Text(stored)] if stored == revision);
                Ok(std::ops::ControlFlow::Break(()))
            },
        );
        read.reported(self).is_some() && held
    }

    /// Every page the stored image holds, with its stored
    /// [`projection_source_revision`], for the launch survey. `None` when the
    /// image cannot be read; the survey then owes a fresh build.
    pub(crate) fn stored_revisions(&self) -> Option<HashMap<String, String>> {
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)?;
        let mut stored = HashMap::new();
        crate::query::projection_sql::visit(
            &mut snapshot,
            "SELECT path, revision FROM direct_source_revisions",
            &[],
            |row| {
                stored.insert(text(row, 0)?, text(row, 1)?);
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .reported(self)?;
        Some(stored)
    }

    /// Whether the stored image holds a page whose graph-relative path
    /// starts with `prefix`, other than those in `deleted`; `None` when it cannot
    /// be read.
    pub(crate) fn image_paths_under(
        &self,
        prefix: &str,
        deleted: &HashSet<String>,
    ) -> Option<bool> {
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)?;
        let mut held = false;
        crate::query::projection_sql::visit(
            &mut snapshot,
            "SELECT path FROM pages WHERE path >= ? ORDER BY path",
            &[PhysicalQueryValue::Text(prefix.to_owned())],
            |row| {
                let page = text(row, 0)?;
                if !page.starts_with(prefix) {
                    return Ok(std::ops::ControlFlow::Break(()));
                }
                if deleted.contains(&page) {
                    return Ok(std::ops::ControlFlow::Continue(()));
                }
                held = true;
                Ok(std::ops::ControlFlow::Break(()))
            },
        )
        .reported(self)?;
        Some(held)
    }

    /// Whether the stored image, ready at `generation`, holds the page at
    /// `rel` at exactly `revision` (a [`projection_source_revision`]: content
    /// and parse configuration). Unlike [`Self::holds_source_revision`] this
    /// is about the rows themselves, so their block ids are the ones a parse
    /// of those bytes gives.
    pub(crate) fn image_holds_source_revision(
        &self,
        at: ReadAt,
        rel: &str,
        revision: &str,
    ) -> bool {
        let Some(_reader) = self.shared_reader_at(at) else {
            return false;
        };
        self.stored_revision_is(rel, revision) && self.answers(at)
    }

    pub(crate) fn page_icon_rows(
        &self,
        at: ReadAt,
        keys: &[String],
    ) -> Option<Vec<(String, String)>> {
        let _reader = self.shared_reader_at(at)?;
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)?;
        let mut rows = Vec::new();
        for key in keys {
            crate::query::projection_sql::visit(&mut snapshot,
                "SELECT n.key, COALESCE(t.preamble, '') FROM pages p JOIN names n ON n.name_id = p.name_id LEFT JOIN page_text t ON t.page_id = p.page_id WHERE p.text_kind = 0 AND n.key = ? ORDER BY p.path",
                &[PhysicalQueryValue::Text(key.clone())], |row| {
                    rows.push((text(row, 0)?, text(row, 1)?));
                    Ok(std::ops::ControlFlow::Continue(()))
                }).reported(self)?;
        }
        self.answers(at).then_some(rows)
    }

    /// The days of the journals that have content, from their stored
    /// `journal_day` (see [`DerivedPage::journal_day`]).
    pub(crate) fn journal_content_days(&self, at: ReadAt) -> Option<Vec<i64>> {
        let _reader = self.shared_reader_at(at)?;
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)?;
        let mut content_pages = std::collections::BTreeMap::new();
        crate::query::projection_sql::visit(&mut snapshot,
            "SELECT p.path, p.journal_day, bt.content FROM pages p JOIN blocks b ON b.page_id = p.page_id JOIN block_text bt ON bt.block_id = b.block_id WHERE p.text_kind = 1 ORDER BY p.path, b.preorder", &[], |row| {
                let path = text(row, 0)?;
                if !content_pages.contains_key(&path) && crate::vocab::block_raw_has_content(&text(row, 2)?) {
                    content_pages.insert(path, optional_integer(row, 1)?);
                }
                Ok(std::ops::ControlFlow::Continue(()))
            }).reported(self)?;
        self.answers(at)
            .then(|| content_pages.into_values().flatten().collect())
    }

    /// Every journal's stored day by path (see [`DerivedPage::journal_day`]).
    pub(crate) fn journal_days(&self, at: ReadAt) -> Option<HashMap<String, i64>> {
        let _reader = self.shared_reader_at(at)?;
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)?;
        let mut days = HashMap::new();
        crate::query::projection_sql::visit(
            &mut snapshot,
            "SELECT path, journal_day FROM pages WHERE text_kind = 1 AND journal_day IS NOT NULL",
            &[],
            |row| {
                days.insert(text(row, 0)?, integer(row, 1)?);
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .reported(self)?;
        self.answers(at).then_some(days)
    }
}
