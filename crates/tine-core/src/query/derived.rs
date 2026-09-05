//! The derived projection rows `block_path_refs` and `property_atoms`, built
//! ONCE here and written by BOTH physical producers (SPEC §5.8, D-4/N14/M6).
//!
//! Direct Files reaches this from `direct_projection::physical_page`; Managed
//! Storage from `oplog::sqlite_materialization::lower_pages_with_derived_rows`.
//! Neither computes a row of its own: a second implementation that agreed by
//! inspection is exactly the parity defect §5.8 guard (b) exists to catch.

use std::collections::HashMap;

use tine_storage::sqlite::{PhysicalPlanning, PhysicalPropertyAtom, PhysicalTag};

use crate::config::ParseConfig;
use crate::query::atom::{AtomFormat, AtomOrigin};
use crate::query::eval::planning_day;
use crate::query::path_refs::{path_refs_closure, PathRefBlock};
use crate::query::registry::owner_property_atoms;

/// `origin` as the `property_atoms` column spells it. Only the registry's `ref`
/// class reads it; no match depends on it (§6.2).
const fn origin_to_sql(origin: AtomOrigin) -> i64 {
    match origin {
        AtomOrigin::Ref => 0,
        AtomOrigin::Plain => 1,
    }
}

/// One owner's `property_atoms` rows, from its property lines in source order.
pub fn property_atom_rows(
    properties: &[(String, String)],
    format: AtomFormat,
    config: &ParseConfig,
) -> Vec<PhysicalPropertyAtom> {
    owner_property_atoms(properties, format, config)
        .into_iter()
        .flat_map(|(normalized_name, atoms)| {
            atoms.into_iter().map(move |atom| PhysicalPropertyAtom {
                normalized_name: normalized_name.clone(),
                ordinal: atom.ordinal,
                atom: atom.text,
                atom_key: atom.key,
                origin: origin_to_sql(atom.origin),
                atom_num: atom.num,
                atom_day: atom.day,
            })
        })
        .collect()
}

/// Every block's `block_path_refs` closure for one page, keyed by block id.
///
/// The names arrive sorted and de-duplicated from
/// [`crate::query::path_refs::closure_names`], so the per-block vectors are
/// already the canonical form two independent builds must agree on.
pub fn path_ref_rows<Id>(
    page_name: &str,
    blocks: &[PathRefBlock<'_, Id>],
) -> HashMap<Id, Vec<String>>
where
    Id: Copy + Eq + std::hash::Hash,
{
    let mut rows: HashMap<Id, Vec<String>> = HashMap::with_capacity(blocks.len());
    path_refs_closure(page_name, blocks, |id, name| {
        rows.entry(id).or_default().push(name.to_owned());
    });
    rows
}

/// The two `blocks` query-text columns from one block's visible text (§5.8,
/// §5.10).
///
/// `folded` is `BlockProjection::visible_lower` where the caller already holds
/// it: `BlockProjection::projection` fills that field with exactly
/// `search_query::canonical_fold(visible)`, so passing it
/// saves a second fold on the Direct Files build path without introducing a
/// second folding rule. Managed Storage carries only the visible text across
/// its input capture and passes `None`.
///
/// Deliberately not `searchable_text`: both producers collapse whitespace in
/// that column for the existing search consumers, and a content predicate has
/// to be able to tell `a  b` from `a b`.
pub fn query_visible_columns(visible: &str, folded: Option<&str>) -> (String, String) {
    let folded = folded.map_or_else(
        || crate::search_query::canonical_fold(visible),
        str::to_owned,
    );
    (visible.to_owned(), folded)
}

/// One owner's `tags` rows: the spelling the source used, plus the page-name
/// key `tag('x')` compares on (§3.2 K18).
///
/// `refs::page_key` and not a second normalizer: `#x` is OG's `[[x]]`, so tag
/// identity IS page identity, and a tag key computed by any other rule would
/// make `tag('X')` and `[[X]]` disagree about the same word.
pub fn tag_rows(tags: &[String]) -> Vec<PhysicalTag> {
    tags.iter()
        .map(|tag| PhysicalTag {
            tag: tag.clone(),
            tag_key: crate::refs::page_key(tag),
        })
        .collect()
}

/// One block's `block_planning` row, or `None` when the block carries no
/// planning facet at all (§3.2 M2).
///
/// Independent of the task marker by construction: the three projection fields
/// are the only input, so a markerless `SCHEDULED:` block gets a row exactly as
/// a `TODO` one does. The day columns come from the ONE `planning_day`
/// primitive and are `None` when the text is not a calendar day -- presence
/// without a day is the malformed-timestamp case (E1), and it has to be
/// physically representable.
pub fn planning_row(
    priority: Option<&str>,
    scheduled: Option<&str>,
    deadline: Option<&str>,
) -> Option<PhysicalPlanning> {
    if priority.is_none() && scheduled.is_none() && deadline.is_none() {
        return None;
    }
    Some(PhysicalPlanning {
        priority: priority.map(str::to_owned),
        scheduled: scheduled.map(str::to_owned),
        scheduled_day: scheduled.and_then(planning_day),
        deadline: deadline.map(str::to_owned),
        deadline_day: deadline.and_then(planning_day),
    })
}

/// The ONE answer to "what journal day is this page?", for `pages.journal_day`.
///
/// It reproduces `Graph::graph_entry_for_relative_path` --
/// decode the file stem under the graph's `:file/name-format`, then parse it
/// with the graph's `JournalFormat` -- because that function is what decides
/// `PageEntry::date_key` today, and a second rule here would let the column and
/// the page's own kind disagree. Both inputs are `ParseConfig` fields precisely
/// so a config edit forces the rebuild that keeps them in step (§5.8 C3).
///
/// Held as a value rather than recomputed per page: `JournalFormat::new`
/// compiles five patterns, which is per-graph work, not per-page work.
pub struct JournalDays {
    format: crate::date::JournalFormat,
    file_name_format: crate::config::FileNameFormat,
}

impl JournalDays {
    pub fn new(config: &ParseConfig) -> Self {
        Self {
            format: crate::date::JournalFormat::new(
                config.journal_file_name_format.as_deref(),
                config.journal_page_title_format.as_deref(),
            ),
            file_name_format: config.file_name_format,
        }
    }

    /// `yyyymmdd` for a journal page whose stem parses, else `None`.
    ///
    /// A non-journal page has no day even if its name looks like a date, and a
    /// journal page whose stem does not parse has none either -- both mirror
    /// `PageEntry::date_key`, which is `Some` only alongside `PageKind::Journal`
    /// and a successful parse.
    pub fn day(&self, rel_path: &str, is_journal: bool) -> Option<i64> {
        if !is_journal {
            return None;
        }
        let filename = std::path::Path::new(rel_path).file_name()?.to_str()?;
        let (stem, _) = filename.rsplit_once('.')?;
        let decoded = crate::model::decode_page_name(stem, self.file_name_format);
        self.format.parse(&decoded).map(|date| date.ordinal_key())
    }
}
