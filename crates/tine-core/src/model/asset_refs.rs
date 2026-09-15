//! Collecting the asset references a page's blocks make.

use super::*;

/// A unique-ish label (epoch millis + process-local sequence) for trashed files,
/// so deleting two pages with the same name doesn't collide in the trash.
/// Collect every `assets/<name>` reference in `text` into `into`. Captures both
/// markdown (`![](../assets/x.png)`, `[f](../assets/x.pdf)`) and org
/// (`[[file:../assets/x.png]]`) forms — the name runs from after `assets/` to the
/// next markup closer (`)`/`]`/quote/etc.) or line break. Crucially it does NOT
/// stop at a space, so a referenced filename containing spaces is matched in full
/// (mis-truncating it would make `orphan_assets` flag a file that IS in use). The
/// first path segment is added too, so a PDF area-image ref (`assets/<key>/p.png`)
/// marks `<key>` as in use.
pub(super) fn collect_asset_refs(text: &str, into: &mut std::collections::HashSet<String>) {
    let mut rest = text;
    while let Some(i) = rest.find("assets/") {
        let after = &rest[i + "assets/".len()..];
        let end = after
            .find(|c: char| {
                matches!(
                    c,
                    ')' | ']' | '"' | '\'' | '<' | '>' | '|' | '\n' | '\r' | '\t'
                )
            })
            .unwrap_or(after.len());
        let name = &after[..end];
        if !name.is_empty() {
            insert_asset_ref(into, name);
            if let Some(seg) = name.split('/').next() {
                if seg != name {
                    insert_asset_ref(into, seg);
                }
            }
        }
        rest = &after[end..];
    }
}

/// Record an asset reference under BOTH its raw form AND its percent-decoded form.
/// A link like `../assets/my%20file.png` names the on-disk file `my file.png`, so
/// comparing the raw URL substring against directory entries would miss the real
/// file and let `orphan_assets` offer an IN-USE asset for trashing (DS Codex#7).
/// Keeping the raw form too covers a file literally named with a `%` escape.
fn insert_asset_ref(into: &mut std::collections::HashSet<String>, raw: &str) {
    let decoded = percent_decode(raw);
    if decoded != raw {
        into.insert(decoded);
    }
    into.insert(raw.to_string());
}

pub(super) fn collect_block_asset_refs(b: &DocBlock, into: &mut std::collections::HashSet<String>) {
    collect_asset_refs(&b.raw, into);
    for c in &b.children {
        collect_block_asset_refs(c, into);
    }
}
