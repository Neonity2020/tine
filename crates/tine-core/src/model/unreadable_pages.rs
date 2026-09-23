//! The record of graph text Tine could not read (GH #543, audit round 15).
//!
//! It says what the disk holds, not what the parsed cache holds, so it
//! outlives the cache: discarding the cache after a rename, merge, rescue or
//! journal migration used to clear it, and the launch survey dropped its own
//! findings whenever the generation moved while it read. Either way the name
//! an unreadable page owns stopped being refused, and creating it wrote a
//! second file for that name (`DIRECT-REF-CREATE-UNREADABLE-OWNER`).
//!
//! The list is private to this module, so nothing can assign or clear it.
//! Every writer states what it observed about one path ([`record`],
//! [`retire`]); only a pass that read the whole graph replaces it, and a pass
//! that read while others wrote merges by path ([`merge_pass`]). Every writer
//! holds the parsed-cache write lock, the lock the generation moves under.
//!
//! [`record`]: UnreadablePages::record
//! [`retire`]: UnreadablePages::retire
//! [`merge_pass`]: UnreadablePages::merge_pass

use super::page_cache::failure_sources;
use super::*;

/// Graph-relative paths of pages Tine could not read or parse, and listing
/// skips (`path: why`), sorted and deduplicated.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct UnreadablePages(Vec<String>);

impl UnreadablePages {
    pub(super) fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub(super) fn to_vec(&self) -> Vec<String> {
        self.0.clone()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `failure` could not be read or parsed as of this observation.
    pub(super) fn record(&mut self, failure: String) -> bool {
        match self.0.binary_search(&failure) {
            Ok(_) => false,
            Err(at) => {
                self.0.insert(at, failure);
                true
            }
        }
    }

    /// The file at `rel_path` was read and parsed, or is gone: every failure
    /// naming exactly that file leaves the record. A failure naming a
    /// directory above it stays; only a listing of that directory settles it.
    pub(super) fn retire(&mut self, rel_path: &str) -> bool {
        let before = self.0.len();
        self.0
            .retain(|failure| !failure_sources(failure).any(|source| source == rel_path));
        self.0.len() != before
    }

    /// A pass that read every graph text file with nothing changing under
    /// it: its failures are the record.
    pub(super) fn replace_after_full_read(&mut self, failures: Vec<String>) {
        self.0 = normalized(failures);
    }

    /// A pass that read every graph text file while other writers ran:
    /// `newer` says whether a failure is about a path something changed after
    /// the pass read it. For those paths the record already holds the newer
    /// observation; for the rest the pass's own is the newest.
    pub(super) fn merge_pass(&mut self, pass: Vec<String>, newer: impl Fn(&str) -> bool) {
        let mut merged: Vec<String> = self.0.drain(..).filter(|failure| newer(failure)).collect();
        merged.extend(pass.into_iter().filter(|failure| !newer(failure)));
        self.0 = normalized(merged);
    }
}

impl Graph {
    /// A mutation moved, wrote or re-read the graph text at `path`, and
    /// found Tine can (`readable`) or cannot read and parse it now; a file
    /// that is gone is `readable` here, as it owns nothing. Called before
    /// the mutation moves the generation, so no answer outlives it.
    pub(super) fn note_graph_text_state(&self, path: &Path, readable: bool) {
        let _cache = self.cache.write().unwrap();
        let rel_path = self.rel_path(path);
        let mut record = self.page_index_failures.write().unwrap();
        record.retire(&rel_path);
        if !readable {
            record.record(rel_path);
        }
    }
}

fn normalized(mut failures: Vec<String>) -> Vec<String> {
    failures.sort();
    failures.dedup();
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retire_removes_a_file_and_its_listing_skip_but_not_its_directory() {
        let mut record = UnreadablePages::default();
        record.replace_after_full_read(vec![
            "pages/a.md".into(),
            "pages/a.md: graph text entry is not a regular file".into(),
            "pages".into(),
            "pages/b.md".into(),
        ]);
        assert!(record.retire("pages/a.md"));
        assert_eq!(record.as_slice(), ["pages", "pages/b.md"]);
        assert!(!record.retire("pages/a.md"));
    }

    #[test]
    fn merge_keeps_the_newer_observation_per_path() {
        let mut record = UnreadablePages::default();
        record.record("pages/watcher.md".into());
        record.record("pages/old.md".into());
        record.merge_pass(
            vec!["pages/survey.md".into(), "pages/watcher-fixed.md".into()],
            |failure| failure == "pages/watcher.md" || failure == "pages/watcher-fixed.md",
        );
        assert_eq!(record.as_slice(), ["pages/survey.md", "pages/watcher.md"]);
    }
}
