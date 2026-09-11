#[path = "support/production_source.rs"]
mod production_source;

use production_source::{compiled_source, production_source_files, relative_path, repo_root};

/// Packet 3 §1 landed the shallow-floor policy as a staging seam that NOTHING in
/// production calls: the format cutover needs §2 image references, §3 recovery
/// custody, §4 reconstruction and §5 crash recovery to land atomically, so
/// wiring the writer alone would leave the tree half-migrated between two
/// document-image formats (I-2, D-1).
///
/// `hot_engine.rs` says exactly that in a doc comment — "a staging building
/// block for §1, not the live capture path". A claim of the form "deliberately
/// inert" / "only used by tests" must be enforced by a test or must not be
/// written (AGENTS.md §2): the storage layer's "nothing here is wired" header
/// stayed true-looking for months after it became false and mistrained every
/// agent that read it. This test is that enforcement, and it is why the doc
/// comment is allowed to exist.
///
/// **When §2 wires the seam, this test fails. That failure is the reminder, not
/// a broken test** — delete this file in the same commit that makes the call
/// real, so "is the floor policy live yet?" stays a checkable fact rather than a
/// comment somebody has to trust.
///
/// The blessed exemplar for scanning what a shipped binary actually compiles is
/// `tests/support/production_source.rs`: it resolves `#[path]` against the
/// DECLARING file's directory and treats test-only-ness as transitive, so a
/// `#[cfg(test)]` caller cannot masquerade as production here.
const SEAM: &str = "build_policy_compact_accepted_document";

/// Whether one line of already-production-filtered source is a CALL of the
/// seam. The definition is production and says nothing; only a call would make
/// the policy live.
fn is_production_call(line: &str) -> bool {
    line.contains(SEAM) && !line.contains("fn ")
}

#[test]
fn the_shallow_floor_seam_has_no_production_caller() {
    let root = repo_root();
    let mut callers = Vec::new();
    for path in production_source_files() {
        let source = compiled_source(&path);
        for (index, line) in source.lines().enumerate() {
            if !is_production_call(line) {
                continue;
            }
            callers.push(format!("{}:{}", relative_path(&root, &path), index + 1));
        }
    }
    assert!(
        callers.is_empty(),
        "{SEAM} now has a production caller: {callers:?}.\n\
         If Packet 3 §2 deliberately wired the shallow floor policy into the live \
         capture path, DELETE tests/checkpoint_floor_policy_is_not_wired.rs in the \
         same commit and update the seam's doc comment in oplog/hot_engine.rs, \
         which still claims it is not the live capture path (I-11: the code does \
         not lie about itself).\n\
         If you did not mean to make it live, the writer must not move before \
         §3 recovery custody, §4 reconstruction and §5 crash recovery land with \
         it — a half-migrated document-image format is exactly the unproved \
         crash edge I-2 exists to prevent."
    );
}

/// A guard that cannot go red is decoration. Prove the classifier still sees a
/// production call, and still ignores the definition, without editing a real
/// source file to find out.
#[test]
fn the_scanner_still_recognises_a_production_call() {
    assert!(is_production_call(
        "            let compacted = engine.build_policy_compact_accepted_document(document, policy)?;"
    ));
    assert!(is_production_call(
        "        .build_policy_compact_accepted_document("
    ));
    assert!(!is_production_call(
        "    pub(crate) fn build_policy_compact_accepted_document("
    ));
    assert!(!is_production_call("    let unrelated = other_call();"));
}
