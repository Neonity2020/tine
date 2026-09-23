//! The pages a fresh build carries over from the image it replaces: the ones
//! a snapshot could not read (GH #543, decision DK4).
//!
//! A snapshot that cannot read a page still owes the index that page's rows:
//! erasing them over a transient disk error or a sharing violation empties
//! the page from search and backlinks for as long as the read keeps failing.
//! The in-place repair kept them by not deleting them, which is why an
//! incomplete snapshot was once never built fresh -- and so a whole-graph
//! re-lowering over one unreadable page ran as one unstoppable repair turn
//! (audit R11-01). A fresh build keeps them instead by lowering them again,
//! from the text the image stored for them: each block's raw body and its
//! place in the tree, exactly what a parse of the file produced. The stored
//! source revision goes with them unchanged, so the page is read from its file
//! again as soon as it can be.
use super::*;
use tine_storage::sqlite::MaterializationError;

fn text(row: &[PhysicalQueryValue], at: usize) -> Result<String, MaterializationError> {
    match row.get(at) {
        Some(PhysicalQueryValue::Text(value)) => Ok(value.clone()),
        _ => Err(MaterializationError::InvalidQuery(
            "invalid carried-page row".into(),
        )),
    }
}

fn integer(row: &[PhysicalQueryValue], at: usize) -> Result<Option<i64>, MaterializationError> {
    match row.get(at) {
        Some(PhysicalQueryValue::Integer(value)) => Ok(Some(*value)),
        Some(PhysicalQueryValue::Null) => Ok(None),
        _ => Err(MaterializationError::InvalidQuery(
            "invalid carried-page row".into(),
        )),
    }
}

/// Whether `path` lies under a source the snapshot could not read: the source
/// itself, a directory above it, or `""`, the whole graph.
pub(super) fn is_unread(retained: &[String], path: &str) -> bool {
    retained.iter().any(|retained| {
        retained.is_empty()
            || path == retained
            || path
                .strip_prefix(retained.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Every page the image at `image` holds under `retained`, except those in
/// `superseded` (pages the snapshot read, or an update taken with it
/// replaces or deletes), as a page to lower and the source revision to store
/// with it, lowered under `parse_config`.
pub(super) fn stored_unread_pages(
    image: &Path,
    retained: &[String],
    superseded: &std::collections::HashSet<&str>,
    parse_config: &Arc<ParseConfig>,
) -> Result<Vec<LoweringInput>, String> {
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(image, || Ok(()))
        .map_err(|error| error.to_string())?;
    let mut pages = Vec::new();
    crate::query::projection_sql::visit(
        &mut snapshot,
        "SELECT p.path, n.raw, p.text_kind, t.preamble, s.revision, p.journal_day FROM pages p JOIN names n ON n.name_id = p.name_id JOIN direct_source_revisions s ON s.path = p.path LEFT JOIN page_text t ON t.page_id = p.page_id ORDER BY p.path",
        &[],
        |row| {
            let path = text(row, 0)?;
            if is_unread(retained, &path) && !superseded.contains(path.as_str()) {
                let preamble = match row.get(3) {
                    Some(PhysicalQueryValue::Null) => None,
                    _ => Some(text(row, 3)?),
                };
                pages.push((
                    path,
                    text(row, 1)?,
                    integer(row, 2)?,
                    preamble,
                    text(row, 4)?,
                    integer(row, 5)?,
                ));
            }
            Ok(std::ops::ControlFlow::Continue(()))
        },
    )
    .map_err(|error| error.to_string())?;
    let mut carried = Vec::with_capacity(pages.len());
    for (rel_path, name, kind, pre_block, revision, journal_day) in pages {
        let kind = kind
            .and_then(page_kind_from_sql)
            .ok_or_else(|| format!("carried page {rel_path} has no known text kind"))?;
        let is_org = Format::from_path(Path::new(&rel_path)) == Format::Org;
        // Preorder puts every parent before its children; building back to
        // front hands each finished block to a parent not yet finished.
        let mut rows = Vec::new();
        crate::query::projection_sql::visit(
            &mut snapshot,
            "SELECT b.block_id, b.parent_block_id, bt.content FROM blocks b JOIN pages p ON p.page_id = b.page_id JOIN block_text bt ON bt.block_id = b.block_id WHERE p.path = ? ORDER BY b.preorder",
            &[PhysicalQueryValue::Text(rel_path.clone())],
            |row| {
                let block_id = integer(row, 0)?.ok_or_else(|| {
                    MaterializationError::InvalidQuery("carried block has no id".into())
                })?;
                rows.push((block_id, integer(row, 1)?, text(row, 2)?));
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .map_err(|error| error.to_string())?;
        let mut blocks: HashMap<i64, DocBlock> = rows
            .iter()
            .map(|(id, _, raw)| {
                let mut block = DocBlock::new(raw.as_str());
                block.is_org = is_org;
                (*id, block)
            })
            .collect();
        let mut roots = Vec::new();
        for (id, parent, _) in rows.iter().rev() {
            let block = blocks
                .remove(id)
                .ok_or_else(|| format!("carried page {rel_path} repeats a block"))?;
            match parent {
                Some(parent) => blocks
                    .get_mut(parent)
                    .ok_or_else(|| format!("carried page {rel_path} has an orphan block"))?
                    .children
                    .push(block),
                None => roots.push(block),
            }
        }
        fn restore_order(blocks: &mut [DocBlock]) {
            blocks.reverse();
            for block in blocks {
                restore_order(&mut block.children);
            }
        }
        restore_order(&mut roots);
        crate::vocab::assign_doc_runtime_ids(&mut roots, &rel_path);
        carried.push(LoweringInput {
            entry: PageEntry {
                name,
                kind,
                // A carried page keeps the day its row holds: the day is the
                // page's own `date_key`, which the stem alone cannot recover
                // for a `title::`-named journal (GH #543, audit R13-06).
                date_key: journal_day,
                path: PathBuf::from(&rel_path),
                rel_path,
            },
            document: Arc::new(Document { pre_block, roots }),
            revision,
            parse_config: Arc::clone(parse_config),
        });
    }
    Ok(carried)
}
