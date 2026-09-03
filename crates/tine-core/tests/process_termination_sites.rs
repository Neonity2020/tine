//! I-10: a shipped Tine binary must not be able to kill itself where the user
//! has no way out.
//!
//! `std::process::abort()` and `std::process::exit()` end the process with no
//! unwinding: no destructor runs, no in-flight save completes, no lock is
//! released. Tine's crash-cut tests deliberately use `abort()` to prove that
//! recovery works — W4-R2 added three such cuts to the Managed activation
//! commit path, and `oplog/sqlite.rs` carries four more. Every one of them is
//! `#[cfg(test)]`-gated, so none exists in a shipped binary.
//!
//! Nothing enforced that. Dropping one `#[cfg(test)]` would compile cleanly,
//! pass every suite, and ship a binary that aborts mid-save on a real graph.
//! This is the guard for that: the gate is now a tested fact rather than a
//! convention (AGENTS.md — architectural facts live in tests, not comments).
//!
//! Scope note: this is deliberately NOT anchored to line numbers. The I-5
//! print census pins `(file, line)` because it records a human's judgement of
//! one exact site, and it pays for that with re-anchoring on every packet that
//! inserts a line above one. Here the claim is per-file and per-reason, so
//! ordinary edits never move it.

#[path = "support/production_source.rs"]
mod production_source;

use production_source::{
    compiled_source, line_of, production_source_files, relative_path, repo_root,
};
use regex::Regex;

/// The only deliberate process termination in a shipped Tine binary.
struct AllowedTermination {
    file: &'static str,
    call: &'static str,
    why: &'static str,
}

const ALLOWED: &[AllowedTermination] = &[AllowedTermination {
    file: "src-tauri/src/data_home.rs",
    call: "exit",
    why: "the private data directory could not be established, so there is no \
          state to corrupt and no window to show; startup stops deliberately",
}];

#[test]
fn a_shipped_binary_terminates_itself_only_where_the_census_allows() {
    let root = repo_root();
    // `std::process::abort()` / `process::exit(..)`, however the path is spelled.
    let termination = Regex::new(r"\bprocess::(abort|exit)\s*\(").unwrap();

    let mut found = Vec::new();
    for file in production_source_files() {
        let source = compiled_source(&file);
        let relative = relative_path(&root, &file);
        for capture in termination.captures_iter(&source) {
            let whole = capture.get(0).unwrap();
            // Skip a commented-out mention.
            if source[..whole.start()]
                .rsplit_once('\n')
                .map_or(&source[..whole.start()], |(_, line)| line)
                .trim_start()
                .starts_with("//")
            {
                continue;
            }
            found.push((
                relative.clone(),
                capture[1].to_string(),
                line_of(&source, whole.start()),
            ));
        }
    }

    let unexplained = found
        .iter()
        .filter(|(file, call, _)| {
            !ALLOWED
                .iter()
                .any(|allowed| allowed.file == file && allowed.call == call)
        })
        .map(|(file, call, line)| format!("{file}:{line} calls process::{call}()"))
        .collect::<Vec<_>>();

    assert!(
        unexplained.is_empty(),
        "I-10: a shipped Tine binary would terminate itself here, with no \
         unwinding — no destructor runs, no in-flight save completes, no lock \
         is released:\n  {}\n\nIf this is a crash-cut used to PROVE recovery, \
         gate it: put `#[cfg(test)]` on both the call site and its helper. \
         `oplog/import.rs::abort_at_clean_activation_commit_cut_for_test` is \
         the exemplar to imitate, and `oplog/sqlite.rs::maybe_abort_rebuild_test` \
         is the same shape. If it is genuinely a deliberate fatal path, add it \
         to ALLOWED in this file with the reason a user cannot be harmed by it.",
        unexplained.join("\n  "),
    );

    // The allowlist must not outlive the code it describes: an entry that no
    // longer matches anything is a stale claim about the binary (I-11).
    for allowed in ALLOWED {
        assert!(
            found
                .iter()
                .any(|(file, call, _)| allowed.file == file && allowed.call == call),
            "I-11: the termination census still allows {}'s process::{}() ({}), \
             but no such call survives in compiled production source. Remove \
             the stale ALLOWED entry.",
            allowed.file,
            allowed.call,
            allowed.why,
        );
    }
}
