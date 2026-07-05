//! Structural, block-level 2-way diff between a page (the "winner", kept in the
//! graph) and a sync-tool conflict copy (Syncthing/Dropbox) of it. Feeds the
//! conflict-merge UI (see `docs/plans/sync-conflict-merge.md`).
//!
//! This is NOT a text-blob diff: it aligns the two BLOCK TREES so the UI can
//! offer per-block keep-mine / keep-theirs / keep-both, and so the resolve step
//! can rebuild a merged tree by re-deriving the SAME alignment and applying the
//! user's per-row decisions (the diff and the apply are symmetric — both are a
//! pure function of the two parsed docs, so a row id means the same thing to
//! both). See `Graph::resolve_sync_conflict`.
//!
//! Matching (per the plan's 3-level scheme), applied to each SIBLING list:
//!   L1  same persisted `id::` (strongest anchor — a conflict copy shares the
//!       winner's ids) OR content-equal subtree → an anchor.
//!   L2  in the gaps between anchors, pair by first-line similarity
//!       (normalized Levenshtein > 0.8) → a *modified* hunk.
//!   L3  whatever is left: present only in the winner → *added*; present only in
//!       the conflict → *removed*. Never silently dropped.
//! Anchored/paired rows with both sides present recurse into their children.
//!
//! Complexity: per sibling list this is a DP LCS (O(k²) in that list's length)
//! plus O(g²) gap pairing — quadratic in the SIBLINGS AT ONE LEVEL, not the whole
//! tree. It runs on demand for a single conflict (never on a hot path), so the
//! clarity of DP LCS is worth more here than shaving to Myers' O(kd).

use crate::doc::DocBlock;
use serde::{Deserialize, Serialize};

/// One side of a diff row — enough for the UI to render (full `raw`, the UI
/// emphasizes its first line) and for a human to judge the hunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockView {
    /// Persisted `id::` if the block has one, else empty (display/debug only).
    pub uuid: String,
    /// The block's full dedented body (`raw`); may be multi-line.
    pub text: String,
    /// Number of direct children (so the UI can show "+3 sub-blocks").
    pub child_count: usize,
}

impl BlockView {
    fn of(b: &DocBlock) -> Self {
        BlockView {
            uuid: b.property("id").unwrap_or_default(),
            text: b.raw.clone(),
            child_count: b.children.len(),
        }
    }
}

/// How a row differs between winner (`mine`) and conflict (`theirs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RowKind {
    /// Content-equal subtree — present and identical on both sides.
    Unchanged,
    /// Matched (by id or similarity) but the block content differs.
    Modified,
    /// Present only in the winner.
    Added,
    /// Present only in the conflict copy.
    Removed,
}

/// One aligned position in the block trees. `id` is a stable path ("2.1" = 2nd
/// child of the 3rd row) that the resolve step reproduces exactly, so the UI's
/// per-row decisions map back onto the same blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRow {
    pub id: String,
    pub kind: RowKind,
    pub mine: Option<BlockView>,
    pub theirs: Option<BlockView>,
    /// Aligned children — only for `Modified`/`Unchanged` rows (both sides
    /// present). `Added`/`Removed` subtrees are atomic (one decision for the
    /// whole subtree), so they carry no child rows.
    pub children: Vec<DiffRow>,
}

/// The full diff of a conflict copy against its winner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConflictDiff {
    pub rows: Vec<DiffRow>,
    /// Winner's page-property pre-block, if any.
    pub mine_pre: Option<String>,
    /// Conflict copy's page-property pre-block, if any.
    pub theirs_pre: Option<String>,
    /// Whether the pre-blocks differ (so the UI can flag a property divergence).
    pub pre_differs: bool,
    /// True when the two block trees are identical (only the pre-block, or
    /// nothing, differs) — lets the UI say "no block changes".
    pub blocks_identical: bool,
}

/// Diff `theirs` (the conflict copy's blocks) against `mine` (the winner's).
pub fn diff_blocks(mine: &[DocBlock], theirs: &[DocBlock]) -> Vec<DiffRow> {
    align(mine, theirs, "")
}

/// Build the full page-level diff, including the pre-block comparison.
pub fn diff_docs(mine: &crate::doc::Document, theirs: &crate::doc::Document) -> SyncConflictDiff {
    let rows = diff_blocks(&mine.roots, &theirs.roots);
    let blocks_identical = rows.iter().all(|r| r.kind == RowKind::Unchanged);
    let mine_pre = normalize_pre(mine.pre_block.as_deref());
    let theirs_pre = normalize_pre(theirs.pre_block.as_deref());
    SyncConflictDiff {
        pre_differs: mine_pre != theirs_pre,
        mine_pre,
        theirs_pre,
        rows,
        blocks_identical,
    }
}

fn normalize_pre(pre: Option<&str>) -> Option<String> {
    pre.map(|s| s.to_string()).filter(|s| !s.trim().is_empty())
}

/// The block's persisted `id::`, if present and non-empty.
fn persisted_id(b: &DocBlock) -> Option<String> {
    b.property("id").filter(|s| !s.is_empty())
}

/// Anchor equality for the LCS: same non-empty persisted id, or a content-equal
/// subtree. (Blocks without ids anchor only when their whole subtree matches.)
fn anchor_eq(a: &DocBlock, b: &DocBlock) -> bool {
    match (persisted_id(a), persisted_id(b)) {
        (Some(ia), Some(ib)) => ia == ib,
        _ => a == b, // DocBlock PartialEq = content-equal (raw + children), ignores uuid
    }
}

/// First visible line of a block (property lines stripped), lowercased and
/// trimmed — the key the L2 similarity pairing compares.
fn first_line_key(b: &DocBlock) -> String {
    b.visible_text().lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_lowercase()
}

/// Normalized similarity of two short strings in [0,1] (1 = identical). Bounded
/// input (block first-lines), so the O(len²) Levenshtein is cheap.
fn similarity(a: &str, b: &str) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let d = levenshtein(a, b);
    let max = a.chars().count().max(b.chars().count());
    if max == 0 {
        1.0
    } else {
        1.0 - (d as f32 / max as f32)
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

const SIMILARITY_THRESHOLD: f32 = 0.8;

/// Align two sibling lists into ordered diff rows. `prefix` is the path id of the
/// parent ("" at the root, "2.0." style otherwise) so child ids are stable.
fn align(mine: &[DocBlock], theirs: &[DocBlock], prefix: &str) -> Vec<DiffRow> {
    // --- L1: LCS over the sibling sequences using anchor equality. ---
    let matched = lcs_pairs(mine, theirs);
    // matched: sorted (i, j) anchor pairs (both strictly increasing).

    let mut rows: Vec<DiffRow> = Vec::new();
    let mut mi = 0usize; // next unconsumed index in `mine`
    let mut ti = 0usize; // next unconsumed index in `theirs`
    let mut counter = 0usize; // row index at this level → stable id

    for (i, j) in matched.iter().copied() {
        // Everything before this anchor on either side is an unmatched gap.
        emit_gap(mine, theirs, mi, i, ti, j, prefix, &mut rows, &mut counter);
        // The anchor itself.
        let id = row_id(prefix, counter);
        counter += 1;
        let a = &mine[i];
        let b = &theirs[j];
        if a == b {
            rows.push(DiffRow {
                id,
                kind: RowKind::Unchanged,
                mine: Some(BlockView::of(a)),
                theirs: Some(BlockView::of(b)),
                children: Vec::new(),
            });
        } else {
            let children = align(&a.children, &b.children, &format!("{id}."));
            rows.push(DiffRow {
                id,
                kind: RowKind::Modified,
                mine: Some(BlockView::of(a)),
                theirs: Some(BlockView::of(b)),
                children,
            });
        }
        mi = i + 1;
        ti = j + 1;
    }
    // Trailing gap after the last anchor.
    emit_gap(mine, theirs, mi, mine.len(), ti, theirs.len(), prefix, &mut rows, &mut counter);
    rows
}

/// Emit rows for an unmatched gap: pair similar first-lines as `Modified`, then
/// leftover winner blocks as `Added` and leftover conflict blocks as `Removed`.
#[allow(clippy::too_many_arguments)]
fn emit_gap(
    mine: &[DocBlock],
    theirs: &[DocBlock],
    m_from: usize,
    m_to: usize,
    t_from: usize,
    t_to: usize,
    prefix: &str,
    rows: &mut Vec<DiffRow>,
    counter: &mut usize,
) {
    // Greedy left-to-right similarity pairing within the gap.
    let mut used_theirs = vec![false; t_to.saturating_sub(t_from)];
    // Precompute conflict-side keys once.
    let their_keys: Vec<String> =
        (t_from..t_to).map(|j| first_line_key(&theirs[j])).collect();

    // We must keep OUTPUT ORDER stable and interleaved by winner position, but a
    // pure per-winner greedy can emit a Removed after a later Added out of order.
    // To keep it simple and order-faithful, walk BOTH gaps with a merge: for each
    // winner block, if it pairs with the earliest still-unused similar conflict
    // block at-or-after the current conflict cursor, first flush the skipped
    // conflict blocks as Removed, then emit the Modified pair; else emit Added.
    let mut tj = t_from; // conflict cursor
    for i in m_from..m_to {
        let key = first_line_key(&mine[i]);
        // Find the best similar unused conflict block from the cursor onward.
        let mut best: Option<(usize, f32)> = None;
        for j in tj..t_to {
            if used_theirs[j - t_from] {
                continue;
            }
            let s = similarity(&key, &their_keys[j - t_from]);
            if s >= SIMILARITY_THRESHOLD && best.map_or(true, |(_, bs)| s > bs) {
                best = Some((j, s));
            }
        }
        if let Some((j, _)) = best {
            // Flush conflict blocks before the match as Removed (preserve order).
            for k in tj..j {
                if !used_theirs[k - t_from] {
                    emit_single(theirs, k, RowKind::Removed, prefix, rows, counter, false);
                    used_theirs[k - t_from] = true;
                }
            }
            // The Modified pair (recurse children).
            let id = row_id(prefix, *counter);
            *counter += 1;
            let children =
                align(&mine[i].children, &theirs[j].children, &format!("{id}."));
            rows.push(DiffRow {
                id,
                kind: RowKind::Modified,
                mine: Some(BlockView::of(&mine[i])),
                theirs: Some(BlockView::of(&theirs[j])),
                children,
            });
            used_theirs[j - t_from] = true;
            tj = j + 1;
        } else {
            emit_single(mine, i, RowKind::Added, prefix, rows, counter, true);
        }
    }
    // Any remaining unused conflict blocks are Removed.
    for j in t_from..t_to {
        if !used_theirs[j - t_from] {
            emit_single(theirs, j, RowKind::Removed, prefix, rows, counter, false);
        }
    }
}

/// Emit one atomic (Added/Removed) subtree row. `is_mine` picks which side the
/// block populates.
fn emit_single(
    blocks: &[DocBlock],
    idx: usize,
    kind: RowKind,
    prefix: &str,
    rows: &mut Vec<DiffRow>,
    counter: &mut usize,
    is_mine: bool,
) {
    let id = row_id(prefix, *counter);
    *counter += 1;
    let view = BlockView::of(&blocks[idx]);
    rows.push(DiffRow {
        id,
        kind,
        mine: if is_mine { Some(view.clone()) } else { None },
        theirs: if is_mine { None } else { Some(view) },
        children: Vec::new(),
    });
}

fn row_id(prefix: &str, n: usize) -> String {
    format!("{prefix}{n}")
}

/// Longest common subsequence of two sibling lists under [`anchor_eq`], returned
/// as sorted `(mine_idx, theirs_idx)` pairs. Standard O(k²) DP + backtrack.
fn lcs_pairs(mine: &[DocBlock], theirs: &[DocBlock]) -> Vec<(usize, usize)> {
    let n = mine.len();
    let m = theirs.len();
    if n == 0 || m == 0 {
        return Vec::new();
    }
    // dp[i][j] = LCS length of mine[i..] and theirs[j..].
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if anchor_eq(&mine[i], &theirs[j]) {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if anchor_eq(&mine[i], &theirs[j]) {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;

    fn parse(s: &str) -> doc::Document {
        let mut d = doc::parse(s);
        crate::model::assign_doc_uuids(&mut d.roots);
        d
    }

    fn kinds(rows: &[DiffRow]) -> Vec<(String, RowKind)> {
        let mut out = Vec::new();
        fn rec(rows: &[DiffRow], out: &mut Vec<(String, RowKind)>) {
            for r in rows {
                out.push((r.id.clone(), r.kind));
                rec(&r.children, out);
            }
        }
        rec(rows, &mut out);
        out
    }

    #[test]
    fn identical_docs_are_all_unchanged() {
        let a = parse("- one\n- two\n\t- child\n");
        let d = diff_docs(&a, &a);
        assert!(d.blocks_identical);
        assert!(d.rows.iter().all(|r| r.kind == RowKind::Unchanged));
        assert!(!d.pre_differs);
    }

    #[test]
    fn added_and_removed_without_ids() {
        // winner has A, B; conflict has A, C.  B is added (winner-only), C removed.
        let mine = parse("- alpha\n- beta\n");
        let theirs = parse("- alpha\n- gamma\n");
        let d = diff_docs(&mine, &theirs);
        let k = kinds(&d.rows);
        // alpha unchanged; beta vs gamma are dissimilar → Added + Removed
        assert_eq!(k[0].1, RowKind::Unchanged);
        let has_added = k.iter().any(|(_, kind)| *kind == RowKind::Added);
        let has_removed = k.iter().any(|(_, kind)| *kind == RowKind::Removed);
        assert!(has_added && has_removed, "kinds: {k:?}");
        assert!(!d.blocks_identical);
    }

    #[test]
    fn modified_by_similar_first_line() {
        let mine = parse("- the quick brown fox jumps\n");
        let theirs = parse("- the quick brown fox leaps\n");
        let d = diff_docs(&mine, &theirs);
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].kind, RowKind::Modified);
    }

    #[test]
    fn modified_matched_by_id_even_when_text_differs() {
        // Same id::, very different text → matched as Modified (id anchor), not
        // Added+Removed.
        let mine = parse("- hello world\n  id:: aaaaaaaa-0000-0000-0000-0000000000ab\n");
        let theirs =
            parse("- totally rewritten line\n  id:: aaaaaaaa-0000-0000-0000-0000000000ab\n");
        let d = diff_docs(&mine, &theirs);
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].kind, RowKind::Modified);
    }

    #[test]
    fn child_change_recurses() {
        // A small edit in one child (one typo) → the child pairs as Modified via
        // similarity, and its parent is Modified because its subtree changed.
        let mine = parse("- parent\n\t- the first child\n\t- the second child line\n");
        let theirs = parse("- parent\n\t- the first child\n\t- the second child lyne\n");
        let d = diff_docs(&mine, &theirs);
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].kind, RowKind::Modified);
        let ck = kinds(&d.rows[0].children);
        assert!(ck.iter().any(|(_, k)| *k == RowKind::Unchanged), "{ck:?}");
        assert!(ck.iter().any(|(_, k)| *k == RowKind::Modified), "{ck:?}");
    }

    #[test]
    fn large_child_edit_shows_add_remove_not_wrong_pairing() {
        // A change too big to be confidently the "same" block → add+remove, never a
        // misleading Modified pairing (the plan's data-safety default).
        let mine = parse("- parent\n\t- kid two\n");
        let theirs = parse("- parent\n\t- kid TWO totally rewritten and much longer now\n");
        let d = diff_docs(&mine, &theirs);
        let ck = kinds(&d.rows[0].children);
        assert!(ck.iter().any(|(_, k)| *k == RowKind::Added), "{ck:?}");
        assert!(ck.iter().any(|(_, k)| *k == RowKind::Removed), "{ck:?}");
        assert!(!ck.iter().any(|(_, k)| *k == RowKind::Modified), "{ck:?}");
    }

    #[test]
    fn reordered_blocks_keep_one_anchor() {
        // winner: A B C ; conflict: A C B  → LCS keeps A and one of B/C as anchors,
        // the other becomes an Added/Removed pair. No crash, order preserved.
        let mine = parse("- aaa\n- bbb\n- ccc\n");
        let theirs = parse("- aaa\n- ccc\n- bbb\n");
        let d = diff_docs(&mine, &theirs);
        assert_eq!(d.rows.iter().filter(|r| r.kind == RowKind::Unchanged).count() >= 2, true);
        assert!(!d.blocks_identical);
    }
}
