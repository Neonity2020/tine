//! The derived projection rows `block_path_refs` and `property_atoms`, built
//! ONCE here and written by BOTH physical producers (SPEC §5.8, D-4/N14/M6).
//!
//! Direct Files reaches this from `direct_projection::physical_page`; Managed
//! Storage from `oplog::sqlite_materialization::lower_pages_with_derived_rows`.
//! Neither computes a row of its own: a second implementation that agreed by
//! inspection is exactly the parity defect §5.8 guard (b) exists to catch.

use std::collections::HashMap;

use tine_storage::sqlite::PhysicalPropertyAtom;

use crate::config::ParseConfig;
use crate::query::atom::{AtomFormat, AtomOrigin};
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
