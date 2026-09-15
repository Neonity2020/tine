//! Recognising and promoting a page's header properties, and detecting a line
//! that an edit newly reclassified as a page property.

use super::*;

/// Canonical Markdown page-header property grammar mirrored from
/// `src/editor/properties.ts`. It is deliberately narrower than OG's historical
/// "first line contains `:: `" serializer heuristic, so ordinary prose/fences
/// can never be promoted accidentally.
pub(super) fn page_header_property_line(line: &str) -> Option<(&str, &str)> {
    static KEY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let (key, value) = line.split_once("::")?;
    if key.is_empty() || key.starts_with('#') {
        return None;
    }
    let valid = KEY
        .get_or_init(|| regex::Regex::new(r"^[\p{L}\p{M}\p{N}_./-]+$").unwrap())
        .is_match(key);
    valid.then_some((key, value))
}

pub(super) fn page_header_properties_only(raw: &str) -> bool {
    if raw.is_empty() || raw.starts_with('\n') || raw.ends_with('\n') {
        return false;
    }
    let mut saw_property = false;
    for line in raw.split('\n') {
        if line.is_empty() {
            if !saw_property {
                return false;
            }
            continue;
        }
        if page_header_property_line(line).is_none() {
            return false;
        }
        saw_property = true;
    }
    saw_property
}

pub(super) fn first_root_is_promotable_page_header(doc: &Document) -> bool {
    let Some(first) = doc.roots.first() else {
        return false;
    };
    first.children.is_empty()
        && page_header_properties_only(&first.raw)
        && !first.raw.split('\n').any(|line| {
            page_header_property_line(line).is_some_and(|(key, _)| key.eq_ignore_ascii_case("id"))
        })
}

pub(super) fn promote_first_root_page_header(doc: &mut Document) {
    if !first_root_is_promotable_page_header(doc) {
        return;
    }
    let first = doc.roots.remove(0);
    doc.pre_block = Some(first.raw);
}

/// Return the first property-shaped outline line that has no outline provenance
/// on disk while the proposal also loses page-header property slots.
///
/// The firewall is deliberately structural rather than an exact-string test:
/// a broken DTO must not evade it by editing the moved line's key/value. At the
/// same time, a property-shaped outline block that genuinely existed on disk is
/// allowed to stay, move, or be edited. We therefore treat existing outline
/// property lines as provenance slots: exact multiset matches consume their
/// original slots first, and remaining slots cover ordinary edits. Only an
/// excess proposed outline line is newly unproven. There is no implicit repair;
/// contradictory structure is rejected before bytes or cache can change.
pub(super) fn newly_reclassified_page_property_line(
    existing: &str,
    proposed: &Document,
) -> Option<String> {
    // The general data-preservation guard is intentionally a little broader
    // than Tine's editable property grammar: Logseq graphs can contain Unicode
    // or plugin-defined keys that Tine does not expose in its settings panel,
    // but they still must never be reclassified into outline content.
    fn page_header_property(line: &str) -> bool {
        let Some((key, _)) = line.split_once("::") else {
            return false;
        };
        let key = key.trim();
        !key.is_empty() && key.chars().all(|ch| !ch.is_whitespace() && ch != ':')
    }

    fn pre_property_lines(raw: Option<&str>) -> Vec<&str> {
        raw.unwrap_or("")
            .split('\n')
            .filter(|line| page_header_property(line))
            .collect()
    }

    fn outline_property_lines<'a>(blocks: &'a [DocBlock], out: &mut Vec<&'a str>) {
        let mut frames: [Option<std::slice::Iter<'a, DocBlock>>; MAX_BLOCK_DEPTH] =
            std::array::from_fn(|_| None);
        let mut len = usize::from(!blocks.is_empty());
        if len != 0 {
            frames[0] = Some(blocks.iter());
        }
        while len != 0 {
            let mut frame = frames[len - 1]
                .take()
                .expect("active property firewall frame");
            let Some(block) = frame.next() else {
                len -= 1;
                continue;
            };
            frames[len - 1] = Some(frame);
            out.extend(
                block
                    .raw
                    .split('\n')
                    .filter(|line| page_header_property(line)),
            );
            if !block.children.is_empty() {
                if len == MAX_BLOCK_DEPTH {
                    debug_assert!(false, "document nesting exceeded graph depth");
                    continue;
                }
                frames[len] = Some(block.children.iter());
                len += 1;
            }
        }
    }

    let existing_doc = doc::parse(existing);
    let existing_pre = pre_property_lines(existing_doc.pre_block.as_deref());
    let proposed_pre = pre_property_lines(proposed.pre_block.as_deref());
    if proposed_pre.len() >= existing_pre.len() {
        return None;
    }

    let mut existing_outline = Vec::new();
    outline_property_lines(&existing_doc.roots, &mut existing_outline);
    let mut proposed_outline = Vec::new();
    outline_property_lines(&proposed.roots, &mut proposed_outline);
    if proposed_outline.len() <= existing_outline.len() {
        return None;
    }

    // Cancel exact matches as a multiset so the diagnostic identifies a truly
    // excess proposal line even in the presence of duplicates. Any remaining
    // existing slots then cover changed/reordered pre-existing outline lines.
    let mut exact_slots: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for line in &existing_outline {
        *exact_slots.entry(*line).or_default() += 1;
    }
    let mut unmatched = Vec::new();
    let mut exact_matches = 0usize;
    for line in proposed_outline {
        match exact_slots.get_mut(line) {
            Some(count) if *count > 0 => {
                *count -= 1;
                exact_matches += 1;
            }
            _ => unmatched.push(line),
        }
    }
    let edited_provenance_slots = existing_outline.len() - exact_matches;
    unmatched
        .get(edited_provenance_slots)
        .map(|line| (*line).to_string())
}
