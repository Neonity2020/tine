//! Narrow SQL sources for launch-time derived answers. Documents here contain
//! only selected blocks, never a parsed or retained whole-graph snapshot.
use super::*;
use crate::doc::{DocBlock, Document};
use crate::query::results::{
    read_admitted_payload, resolve_identity, PayloadChannel, PayloadFacts, ResultIdentity,
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
    pub(crate) kind: i64,
    pub(crate) document: Document,
    pub(crate) session_ids: Option<(String, Vec<(String, usize)>)>,
}

fn invalid() -> tine_storage::sqlite::MaterializationError {
    tine_storage::sqlite::MaterializationError::InvalidQuery("invalid derived row".into())
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

impl DirectProjection {
    pub(crate) fn derived_pages(
        &self,
        generation: u64,
        selection: &DerivedSelection<'_>,
    ) -> Option<Vec<DerivedPage>> {
        let _reader = self.shared_reader_at(generation)?;
        let identity = ResultIdentity {
            session_pages: Arc::clone(&self.shared.session_pages.lock().unwrap()),
            all_session: false,
        };
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(())).ok()?;
        let mut seeds = std::collections::BTreeSet::new();
        let mut read_seeds = |sql: &str, params: &[PhysicalQueryValue]| {
            crate::query::projection_sql::visit(&mut snapshot, sql, params, |row| {
                seeds.insert(integer(row, 0)?);
                Ok(std::ops::ControlFlow::Continue(()))
            })
            .ok()
        };
        match selection {
            DerivedSelection::Resolve(ids) | DerivedSelection::Preview(ids) => {
                for id in ids.iter().collect::<std::collections::BTreeSet<_>>() {
                    let uuid = uuid::Uuid::parse_str(id.trim())
                        .ok()
                        .map(|id| PhysicalQueryValue::Blob(id.as_bytes().to_vec()))
                        .unwrap_or(PhysicalQueryValue::Null);
                    let spelling = PhysicalQueryValue::Text(id.clone());
                    read_seeds(
                        "SELECT block_id FROM blocks WHERE result_id = ? OR logseq_uuid = ? OR block_id IN (SELECT o.owner_id FROM properties o JOIN names n ON n.name_id = o.name_id WHERE o.owner_type = 1 AND n.key = 'id' AND o.value = ?)",
                        &[spelling.clone(), uuid, spelling],
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
                .ok()?;
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
            let sql = format!("SELECT b.block_id, b.page_id, b.parent_block_id, b.result_id, b.estimated_bytes, b.tag_count, b.property_count, b.order_key, p.path, n.raw, p.text_kind FROM blocks b JOIN pages p ON p.page_id = b.page_id JOIN names n ON n.name_id = p.name_id WHERE b.block_id IN ({}) ORDER BY p.path, b.preorder", vec!["?"; chunk.len()].join(", "));
            crate::query::projection_sql::visit(&mut snapshot, &sql, &params, |row| {
                let path = text(row, 8)?;
                let page = *page_indices.entry(path.clone()).or_insert_with(|| {
                    let index = pages.len();
                    pages.push(DerivedPage {
                        name: String::new(),
                        path: path.clone(),
                        kind: 0,
                        document: Document::default(),
                        session_ids: None,
                    });
                    index
                });
                pages[page].name = text(row, 9)?;
                pages[page].kind = integer(row, 10)?;
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
            .ok()?;
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
        .ok()?;
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
                    }).ok()?;
                page.session_ids = revision.map(|revision| (revision, preorder));
            }
        }
        self.ready_at(generation).then_some(pages)
    }

    /// Whether the image, ready at `generation`, holds the page at `rel` at
    /// exactly `revision` (a [`projection_source_revision`]: content and
    /// parse configuration).
    pub(crate) fn holds_source_revision(&self, generation: u64, rel: &str, revision: &str) -> bool {
        let Some(_reader) = self.shared_reader_at(generation) else {
            return false;
        };
        let Ok(mut snapshot) =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
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
        read.is_ok() && held && self.ready_at(generation)
    }

    pub(crate) fn page_icon_rows(
        &self,
        generation: u64,
        keys: &[String],
    ) -> Option<Vec<(String, String)>> {
        let _reader = self.shared_reader_at(generation)?;
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(())).ok()?;
        let mut rows = Vec::new();
        for key in keys {
            crate::query::projection_sql::visit(&mut snapshot,
                "SELECT n.key, COALESCE(t.preamble, '') FROM pages p JOIN names n ON n.name_id = p.name_id LEFT JOIN page_text t ON t.page_id = p.page_id WHERE p.text_kind = 0 AND n.key = ? ORDER BY p.path",
                &[PhysicalQueryValue::Text(key.clone())], |row| {
                    rows.push((text(row, 0)?, text(row, 1)?));
                    Ok(std::ops::ControlFlow::Continue(()))
                }).ok()?;
        }
        self.ready_at(generation).then_some(rows)
    }

    pub(crate) fn journal_content_names(&self, generation: u64) -> Option<Vec<String>> {
        let _reader = self.shared_reader_at(generation)?;
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(())).ok()?;
        let mut content_pages = std::collections::BTreeMap::new();
        crate::query::projection_sql::visit(&mut snapshot,
            "SELECT p.path, n.raw, bt.content FROM pages p JOIN names n ON n.name_id = p.name_id JOIN blocks b ON b.page_id = p.page_id JOIN block_text bt ON bt.block_id = b.block_id WHERE p.text_kind = 1 ORDER BY p.path, b.preorder", &[], |row| {
                let path = text(row, 0)?;
                if !content_pages.contains_key(&path) && crate::vocab::block_raw_has_content(&text(row, 2)?) {
                    content_pages.insert(path, text(row, 1)?);
                }
                Ok(std::ops::ControlFlow::Continue(()))
            }).ok()?;
        self.ready_at(generation)
            .then(|| content_pages.into_values().collect())
    }
}
