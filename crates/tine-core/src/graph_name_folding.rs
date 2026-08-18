//! Does this graph's filesystem tell two page file names apart?
//!
//! Android CI run 32123012366 established that it does not always. The managed
//! storage journey writes two page files whose names differ only by case, and
//! on Android shared storage (`/storage/emulated/0/Download/…`) the second
//! write landed on the FIRST file:
//!
//! ```text
//! graph filesystem folds two journey page names into one file:
//! pages/K\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md reads back the bytes written
//! for pages/k\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md (18 bytes, not 8)
//! ```
//!
//! That is a platform fact, not a Tine defect, and it is not confined to
//! Android: FAT/exFAT removable media, NTFS, APFS in its default configuration
//! and any `ext4` directory with the casefold attribute all fold case, and
//! HFS+ additionally normalizes. So the question "would this filesystem hold
//! both of these two names?" has to be ASKED rather than assumed, and asked
//! once per filesystem rather than once per file.
//!
//! ## Why this is not, by itself, a data-safety hole
//!
//! Tine's logical page name is already case- and normalization-insensitive:
//! [`crate::oplog::LogicalPageName::key_digest`] hashes
//! `canonical_page_name_key`, which lowercases and then applies NFC, matching
//! Logseq. **Every pair of file names a case-folding or normalization-folding
//! filesystem cannot tell apart is therefore a pair Tine already treats as ONE
//! page.** Such a filesystem cannot merge two distinct Tine pages, because two
//! names that it folds were never two pages here.
//!
//! What it does change is that the non-authoritative DUPLICATE file — the one
//! `retain_authoritative_desired_pages` deliberately leaves on disk as ordinary
//! graph text with no page of its own — cannot exist on that filesystem at all.
//! Whoever wrote the second spelling (a sync client, a file manager, the user)
//! overwrote the authoritative file instead. Tine never saw two files and
//! cannot report a merge it has no evidence of; see
//! `docs/storage-sync-contract.md` §2.10d for the contract this module states.
//!
//! ## What the probe is for
//!
//! Three things, in descending order of weight:
//!
//! 1. Naming the platform fact in receipts, so a device round trip is not the
//!    only way to learn it.
//! 2. Letting a fixture — the managed storage journey above all — write a tree
//!    the filesystem can actually hold, and then assert the PRODUCT behavior on
//!    that tree instead of refusing to run.
//! 3. Letting a refusal that is really about folding say so in the user's
//!    words.
//!
//! The answer is never load-bearing for correctness. A probe that cannot run
//! answers [`GraphNameFolding::UNKNOWN`], which is the same shape as "this
//! filesystem folds nothing", and every caller degrades to the behavior it had
//! before this module existed.

#[cfg(test)]
use std::path::PathBuf;
use std::{collections::BTreeMap, fs, io, path::Path, sync::RwLock};

use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

/// Which name distinctions the graph's filesystem does NOT preserve.
///
/// The three axes are probed independently and deliberately not collapsed into
/// one boolean: a filesystem that folds ASCII case only (a very old FAT volume)
/// and one that folds full Unicode case (`ext4 +F`, APFS, Android shared
/// storage) permit different graphs, and a normalizing filesystem (HFS+) folds
/// a pair the other two hold apart. Guessing between them would put a wrong
/// statement in the storage contract.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphNameFolding {
    /// `Foo.md` and `foo.md` are one file.
    pub ascii_case: bool,
    /// `\u{17d}.md` and `\u{17e}.md` (Ž and ž) are one file.
    pub unicode_case: bool,
    /// `\u{17d}.md` and `Z\u{30c}.md` (NFC and NFD) are one file.
    pub normalization: bool,
}

impl GraphNameFolding {
    /// A filesystem that keeps every spelling apart — and equally the answer
    /// for a filesystem we could not ask. Both must behave identically, so
    /// there is one constant with two names rather than a tri-state that every
    /// caller would have to handle.
    pub const NONE: Self = Self {
        ascii_case: false,
        unicode_case: false,
        normalization: false,
    };

    /// What a failed probe answers. Identical to [`Self::NONE`] on purpose: an
    /// unknown filesystem is treated exactly as a non-folding one, so no
    /// behavior depends on the probe having succeeded.
    pub const UNKNOWN: Self = Self::NONE;

    /// Does this filesystem fold anything at all?
    #[must_use]
    pub const fn folds(self) -> bool {
        self.ascii_case || self.unicode_case || self.normalization
    }

    /// A stable receipt token. Written into the managed storage journey receipt
    /// so a device run states the platform fact instead of leaving it to be
    /// inferred from whichever assertion happened to fail.
    #[must_use]
    pub fn diagnostic(self) -> String {
        if !self.folds() {
            return "none".to_owned();
        }
        let mut parts = Vec::new();
        if self.ascii_case {
            parts.push("ascii_case");
        }
        if self.unicode_case {
            parts.push("unicode_case");
        }
        if self.normalization {
            parts.push("normalization");
        }
        parts.join("+")
    }

    /// The spelling-independent key this filesystem effectively stores a name
    /// under.
    ///
    /// Order matters and mirrors [`crate::oplog`]'s own canonical page-name key
    /// (lowercase, then NFC) so that a folded filesystem name and a folded page
    /// name cannot disagree about which pairs are equivalent.
    #[must_use]
    pub fn effective_name(self, name: &str) -> String {
        let lowered: String = if self.unicode_case {
            name.chars().flat_map(char::to_lowercase).collect()
        } else if self.ascii_case {
            name.to_ascii_lowercase()
        } else {
            name.to_owned()
        };
        if self.normalization {
            lowered.nfc().collect()
        } else {
            lowered
        }
    }

    /// The key a whole graph-relative path is effectively stored under.
    ///
    /// Applied per component, because directory names fold on exactly the same
    /// terms as file names: `notes/Archiv/x.md` and `notes/archiv/x.md` are one
    /// path on a case-folding filesystem.
    #[must_use]
    pub fn effective_path(self, path: &str) -> String {
        if !self.folds() {
            return path.to_owned();
        }
        path.split('/')
            .map(|component| self.effective_name(component))
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Would this filesystem resolve these two graph-relative paths to one
    /// file?
    #[must_use]
    pub fn resolves_to_one_file(self, left: &str, right: &str) -> bool {
        left != right && self.effective_path(left) == self.effective_path(right)
    }

    /// The refusal text for the one folding condition a user can act on: they
    /// asked for a name this storage cannot hold beside a name it already
    /// holds.
    ///
    /// `shown` is the spelling that is on disk and is the page Tine displays;
    /// `requested` is the spelling that cannot get its own file here. The
    /// sentence names both, says which one wins, and gives the one action that
    /// works — because the alternative the device produced was a UUID the user
    /// cannot do anything with, repeated once per tick.
    #[must_use]
    pub fn explain_one_file_two_names(self, shown: &str, requested: &str) -> String {
        format!(
            "This device's storage cannot keep \u{201c}{requested}\u{201d} and \
             \u{201c}{shown}\u{201d} in two separate files ({}), so they would share one file \
             and one of them would be overwritten. Tine is keeping \u{201c}{shown}\u{201d} \
             unchanged. To keep both, give one of them a name that differs by more than \
             capitalisation or accent spelling.",
            self.plain_language_reason()
        )
    }

    fn plain_language_reason(self) -> &'static str {
        match (self.ascii_case || self.unicode_case, self.normalization) {
            (true, true) => {
                "it treats upper- and lower-case letters as the same, and treats two ways of \
                 spelling the same accented letter as the same"
            }
            (true, false) => "it treats upper- and lower-case letters as the same",
            (false, true) => "it treats two ways of spelling the same accented letter as the same",
            (false, false) => "names that differ only in ways this storage ignores",
        }
    }
}

/// Filesystems (by `st_dev`) whose folding answer is already known.
///
/// The answer is a property of the mounted filesystem, not of one directory, so
/// it is remembered once instead of costing four file writes per graph open —
/// the same reasoning, and the same key, as
/// `model::FLAGGED_RENAME_UNSUPPORTED_DEVICES`. A miss is never load-bearing:
/// an unknown device is simply probed again.
static PROBED_DEVICES: RwLock<BTreeMap<u64, GraphNameFolding>> = RwLock::new(BTreeMap::new());

/// Overrides installed by tests, keyed by graph root rather than by device.
///
/// No host filesystem this suite runs on folds anything, which is precisely why
/// the behavior has to be forced here — and it must be forced per graph root,
/// never per `st_dev`, because every temporary directory in a test run shares
/// one device and a device-keyed override would leak into unrelated tests.
#[cfg(test)]
static FORCED_ROOTS: RwLock<Vec<(PathBuf, GraphNameFolding)>> = RwLock::new(Vec::new());

/// Force the answer for one graph root, for tests only.
#[cfg(test)]
pub fn force_graph_name_folding_for_tests(root: &Path, folding: GraphNameFolding) {
    if let Ok(mut forced) = FORCED_ROOTS.write() {
        forced.retain(|(existing, _)| existing != root);
        forced.push((root.to_path_buf(), folding));
    }
}

/// Drop a forced answer. Tests that install one are responsible for this; the
/// list is small and a leaked entry only affects that exact path.
#[cfg(test)]
pub fn clear_graph_name_folding_for_tests(root: &Path) {
    if let Ok(mut forced) = FORCED_ROOTS.write() {
        forced.retain(|(existing, _)| existing != root);
    }
}

#[cfg(test)]
fn forced_graph_name_folding(root: &Path) -> Option<GraphNameFolding> {
    let forced = FORCED_ROOTS.read().ok()?;
    forced
        .iter()
        .find(|(existing, _)| root.starts_with(existing))
        .map(|(_, folding)| *folding)
}

#[cfg(not(test))]
fn forced_graph_name_folding(_root: &Path) -> Option<GraphNameFolding> {
    None
}

/// What this graph root's filesystem folds, probing at most once per
/// filesystem.
///
/// Never returns an error: a filesystem that cannot be probed answers
/// [`GraphNameFolding::UNKNOWN`], which is byte-identical to "folds nothing",
/// so no caller can accidentally make correctness depend on the probe.
#[must_use]
pub fn graph_name_folding(root: &Path) -> GraphNameFolding {
    if let Some(forced) = forced_graph_name_folding(root) {
        return forced;
    }
    if let Some(device) = graph_device_id(root) {
        if let Ok(probed) = PROBED_DEVICES.read() {
            if let Some(folding) = probed.get(&device) {
                return *folding;
            }
        }
        let folding = probe_graph_name_folding(root).unwrap_or(GraphNameFolding::UNKNOWN);
        if let Ok(mut probed) = PROBED_DEVICES.write() {
            probed.insert(device, folding);
        }
        return folding;
    }
    probe_graph_name_folding(root).unwrap_or(GraphNameFolding::UNKNOWN)
}

#[cfg(unix)]
fn graph_device_id(root: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    fs::metadata(root).ok().map(|metadata| metadata.dev())
}

/// Windows has no `st_dev`, so every root is probed on its own. The probe is
/// four small writes; a memo would be an optimisation, not a contract.
#[cfg(not(unix))]
const fn graph_device_id(_root: &Path) -> Option<u64> {
    None
}

/// The probe itself: write a pair, read the first one back, and see whose bytes
/// come out.
///
/// Deliberately a WRITE probe rather than an inspection of the mount table.
/// `renameat2` taught this project that a filesystem's advertised identity is
/// not its behavior (`docs/storage-sync-contract.md` §2.10b: "upstream source
/// is evidence about upstream intent, not proof about the running device"), and
/// the same holds here — Android shared storage is `ext4` underneath a FUSE
/// daemon, and the fold is the daemon's, not the filesystem's.
///
/// Everything happens inside one hidden, uniquely named directory that is
/// removed before returning, so the graph scan never sees it (hidden entries
/// are skipped) and two concurrent probes cannot collide.
pub fn probe_graph_name_folding(root: &Path) -> io::Result<GraphNameFolding> {
    let probe_root = root.join(format!(".tine-name-folding-{}", Uuid::new_v4()));
    fs::create_dir_all(&probe_root)?;
    let result = probe_inside(&probe_root);
    let _ = fs::remove_dir_all(&probe_root);
    result
}

fn probe_inside(probe_root: &Path) -> io::Result<GraphNameFolding> {
    Ok(GraphNameFolding {
        ascii_case: probe_pair(probe_root, "Ascii-Case.md", "ascii-case.md")?,
        // Ž (U+017D) against ž (U+017E). Precomposed on both sides, so a
        // filesystem that folds normalization but not case answers `false`
        // here, which is what makes the three axes independent.
        unicode_case: probe_pair(probe_root, "Unicode-\u{17d}.md", "Unicode-\u{17e}.md")?,
        // Ž (U+017D) against Z + combining caron (U+005A U+030C). Same case on
        // both sides, so a case-folding filesystem answers `false` here.
        normalization: probe_pair(probe_root, "Norm-\u{17d}.md", "Norm-Z\u{30c}.md")?,
    })
}

/// One axis: does writing `second` land on `first`?
///
/// The two payloads differ in LENGTH as well as content, so a partial write or
/// a stale page-cache read cannot be mistaken for a fold, and a fold cannot be
/// mistaken for an unchanged file.
fn probe_pair(probe_root: &Path, first: &str, second: &str) -> io::Result<bool> {
    const FIRST_BYTES: &[u8] = b"first";
    const SECOND_BYTES: &[u8] = b"second-and-longer";

    let first_path = probe_root.join(first);
    let second_path = probe_root.join(second);
    fs::write(&first_path, FIRST_BYTES)?;
    fs::write(&second_path, SECOND_BYTES)?;
    let read_back = fs::read(&first_path)?;
    // Clean up eagerly: on a folding filesystem both names are one file, so
    // removing both would fail the second time, and on a non-folding one a left
    // -over file would be visible to the next axis' `read_dir` if that ever
    // starts enumerating.
    let _ = fs::remove_file(&second_path);
    let _ = fs::remove_file(&first_path);
    if read_back == FIRST_BYTES {
        return Ok(false);
    }
    if read_back == SECOND_BYTES {
        return Ok(true);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "name-folding probe read back neither payload from {first}: {} bytes",
            read_back.len()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("tine-name-folding-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// The host filesystems this suite runs on hold every spelling apart. That
    /// is not a weakness of the probe, it is the reason the probe exists: the
    /// behavior it detects is unreachable here and reachable on the platform we
    /// cannot debug interactively.
    #[test]
    fn a_host_graph_filesystem_folds_nothing() {
        let root = probe_root("host");
        let folding = probe_graph_name_folding(&root).unwrap();
        assert_eq!(folding, GraphNameFolding::NONE, "{folding:?}");
        assert_eq!(folding.diagnostic(), "none");
        assert!(!folding.folds());
        let _ = fs::remove_dir_all(&root);
    }

    /// The probe must leave the graph exactly as it found it. It runs against
    /// the user's own graph root, and a stray probe file there would be
    /// observed by the watcher and imported as a page.
    #[test]
    fn the_probe_removes_everything_it_wrote() {
        let root = probe_root("residue");
        probe_graph_name_folding(&root).unwrap();
        let residue = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(residue.is_empty(), "{residue:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A probe that cannot run must answer exactly like a filesystem that folds
    /// nothing, so that no caller's correctness depends on it having succeeded.
    #[test]
    fn an_unprobeable_root_answers_as_a_non_folding_filesystem() {
        let root = probe_root("absent").join("does-not-exist/and-cannot-be-created");
        // A path whose parent is a FILE cannot be created as a directory.
        let blocker = probe_root("blocked").join("file");
        fs::write(&blocker, b"x").unwrap();
        assert!(probe_graph_name_folding(&blocker.join("under-a-file")).is_err());
        assert_eq!(graph_name_folding(&root), GraphNameFolding::UNKNOWN);
        assert_eq!(GraphNameFolding::UNKNOWN, GraphNameFolding::NONE);
    }

    /// The three axes are independent, and each folds exactly the pair it is
    /// about. A single `folds: bool` would have made all three of these one
    /// answer and put a wrong statement in the storage contract.
    #[test]
    fn each_axis_folds_only_its_own_pair() {
        let case = GraphNameFolding {
            ascii_case: true,
            unicode_case: true,
            normalization: false,
        };
        assert!(case.resolves_to_one_file("pages/Foo.md", "pages/foo.md"));
        assert!(case.resolves_to_one_file("pages/\u{17d}.md", "pages/\u{17e}.md"));
        assert!(!case.resolves_to_one_file("pages/\u{17d}.md", "pages/Z\u{30c}.md"));

        let normalization = GraphNameFolding {
            ascii_case: false,
            unicode_case: false,
            normalization: true,
        };
        assert!(!normalization.resolves_to_one_file("pages/Foo.md", "pages/foo.md"));
        assert!(normalization.resolves_to_one_file("pages/\u{17d}.md", "pages/Z\u{30c}.md"));

        let ascii_only = GraphNameFolding {
            ascii_case: true,
            unicode_case: false,
            normalization: false,
        };
        assert!(ascii_only.resolves_to_one_file("pages/Foo.md", "pages/foo.md"));
        assert!(!ascii_only.resolves_to_one_file("pages/\u{17d}.md", "pages/\u{17e}.md"));

        assert!(!GraphNameFolding::NONE.resolves_to_one_file("pages/Foo.md", "pages/foo.md"));
        // Never "folds" a path with itself: callers use this to find a DIFFERENT
        // file that would collide, and a true answer for one path would make
        // every path collide with itself.
        assert!(!case.resolves_to_one_file("pages/Foo.md", "pages/Foo.md"));
    }

    /// Directory components fold on the same terms as file names. A graph whose
    /// pages live under `notes/Archiv/` and `notes/archiv/` is one directory
    /// there, and a caller that only folded the basename would plan a tree the
    /// filesystem cannot hold.
    #[test]
    fn every_path_component_folds_not_only_the_file_name() {
        let case = GraphNameFolding {
            ascii_case: true,
            unicode_case: true,
            normalization: false,
        };
        assert!(case.resolves_to_one_file("notes/Archiv/x.md", "notes/archiv/x.md"));
        assert_eq!(
            case.effective_path("Notes/Archiv/X.md"),
            "notes/archiv/x.md"
        );
        assert_eq!(
            GraphNameFolding::NONE.effective_path("Notes/Archiv/X.md"),
            "Notes/Archiv/X.md"
        );
    }

    /// Every folding filesystem Tine can meet must have a receipt token, and
    /// the tokens must be distinguishable — the whole point is that a device
    /// receipt states which folding is in play rather than leaving it to be
    /// guessed from whichever assertion failed first.
    #[test]
    fn the_receipt_token_names_each_axis_separately() {
        assert_eq!(GraphNameFolding::NONE.diagnostic(), "none");
        assert_eq!(
            GraphNameFolding {
                ascii_case: true,
                unicode_case: true,
                normalization: false,
            }
            .diagnostic(),
            "ascii_case+unicode_case"
        );
        assert_eq!(
            GraphNameFolding {
                ascii_case: false,
                unicode_case: false,
                normalization: true,
            }
            .diagnostic(),
            "normalization"
        );
        assert_eq!(
            GraphNameFolding {
                ascii_case: true,
                unicode_case: true,
                normalization: true,
            }
            .diagnostic(),
            "ascii_case+unicode_case+normalization"
        );
    }

    /// The user-facing sentence must name BOTH spellings, say which one
    /// survives, and give an action. The message it replaces on the device was
    /// `decoded destination logical page name … is already owned by page
    /// <uuid>` — three facts the user cannot use and one identifier they cannot
    /// see anywhere in the app.
    #[test]
    fn the_user_facing_explanation_names_both_spellings_and_an_action() {
        let folding = GraphNameFolding {
            ascii_case: true,
            unicode_case: true,
            normalization: false,
        };
        let message = folding.explain_one_file_two_names("K\u{16f}\u{148}", "k\u{16f}\u{148}");
        assert!(message.contains("K\u{16f}\u{148}"), "{message}");
        assert!(message.contains("k\u{16f}\u{148}"), "{message}");
        assert!(message.contains("upper- and lower-case"), "{message}");
        assert!(message.contains("Tine is keeping"), "{message}");
        assert!(!message.contains("logical page name"), "{message}");
        assert!(!message.contains("uuid"), "{message}");

        let normalizing = GraphNameFolding {
            ascii_case: false,
            unicode_case: false,
            normalization: true,
        };
        assert!(normalizing
            .explain_one_file_two_names("\u{17d}", "Z\u{30c}")
            .contains("accented letter"),);
    }

    /// A forced answer is scoped to one graph root. Every temporary directory
    /// in this suite shares one `st_dev`, so a device-keyed override would make
    /// one test's simulated Android filesystem apply to every other test.
    #[test]
    fn a_forced_answer_is_scoped_to_one_graph_root() {
        let folding = GraphNameFolding {
            ascii_case: true,
            unicode_case: true,
            normalization: false,
        };
        let forced = probe_root("forced");
        let untouched = probe_root("untouched");
        force_graph_name_folding_for_tests(&forced, folding);
        assert_eq!(graph_name_folding(&forced), folding);
        assert_eq!(graph_name_folding(&forced.join("pages")), folding);
        assert_eq!(graph_name_folding(&untouched), GraphNameFolding::NONE);
        clear_graph_name_folding_for_tests(&forced);
        assert_eq!(graph_name_folding(&forced), GraphNameFolding::NONE);
        let _ = fs::remove_dir_all(&forced);
        let _ = fs::remove_dir_all(&untouched);
    }

    /// Tine's own page-name key already folds case and NFC/NFD. This is the
    /// load-bearing fact behind the whole contract: a folding filesystem's
    /// equivalence classes are a SUBSET of Tine's, so folding can never merge
    /// two pages Tine holds apart. If this ever stops being true, the contract
    /// in §2.10d stops being true with it.
    #[test]
    fn filesystem_folding_never_separates_names_tine_already_treats_as_one() {
        use crate::oplog::LogicalPageName;

        let everything = GraphNameFolding {
            ascii_case: true,
            unicode_case: true,
            normalization: true,
        };
        for (left, right) in [
            (
                "K\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}",
                "k\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}",
            ),
            ("\u{17d} pilot notes", "Z\u{30c} pilot notes"),
            ("Foo", "foo"),
        ] {
            assert!(
                everything.resolves_to_one_file(
                    &format!("pages/{left}.md"),
                    &format!("pages/{right}.md")
                ),
                "{left} / {right}"
            );
            assert_eq!(
                LogicalPageName::parse(left).unwrap().key_digest(),
                LogicalPageName::parse(right).unwrap().key_digest(),
                "a filesystem fold must never split a pair Tine treats as one page: \
                 {left} / {right}"
            );
        }
    }
}
