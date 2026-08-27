//! Permanent primitive counters for durability barriers.
//!
//! A *durability barrier* is a syscall that forces data or metadata to stable
//! storage: `fsync` of a regular file, `fsync` of a directory, or `syncfs` of a
//! filesystem. Each one costs a real device round trip (~0.3 ms on the audit
//! box's ext4; plausibly hundreds of ms on a slow or network filesystem), so
//! the *number* of barriers an operation performs — not the time any one phase
//! reports — is the cost that scales with the user's hardware.
//!
//! The 2026-08-26 managed-storage cost-model audit found that one accepted
//! single-block managed save executed ~66 barriers against a blind budget of 3,
//! because durability was decided artifact by artifact and nobody ever saw the
//! per-operation sum. Phase timers cannot see multiplicity; counters can. These
//! counters therefore exist to make that sum a **testable budget** rather than
//! an invisible property — see
//! `sync_runtime::tests::managed_save_and_move_stay_within_their_barrier_budget`.
//!
//! ## What is counted, and what is not
//!
//! Every barrier `tine-core` itself initiates is counted, including the two
//! barriers that `tine_storage::publish_immutable_exact*` performs on the
//! caller's behalf: its documented contract is one file `fsync` plus one
//! directory `fsync` per published artifact, and
//! [`note_immutable_publication`] records exactly that pair.
//!
//! Barriers executed *inside* `tine-storage` on its own account — the local
//! journal's append `fsync`s, the SQLite VFS, and SQLite file-set publication —
//! are **not** counted, because they are not reachable from this crate without
//! a `tine-storage` API change. The audit measured that undercount at three
//! barriers per ordinary managed save (two local-journal appends and one SQLite
//! file-set checkpoint) and about four for a cross-page move. Budgets stated in
//! this crate are therefore *core-initiated* barriers, and the tests say so.
//!
//! ## Attribution
//!
//! The process-wide totals ([`snapshot`]) are always maintained; overhead is one
//! relaxed atomic increment on a code path that is about to block on a device
//! round trip, so the counters are always compiled in and no feature flag can
//! drift out of step with production.
//!
//! Tests need per-operation numbers, and `cargo test` runs many graphs
//! concurrently in one process, so process-wide totals cannot be differenced
//! safely. [`BarrierSession`] solves that: a session is a thread-local
//! attribution channel that the managed actor thread **inherits from whichever
//! thread spawned it**, so a test that opens a session before creating its
//! runtime sees that runtime's barriers and no other test's.

use std::cell::RefCell;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The maximum number of core-initiated durability barriers one accepted
/// single-block managed save may perform.
///
/// This is a *ceiling on current behaviour*, not the destination: the
/// cost-model audit's target is 3, and `docs/storage-sync-contract.md`
/// §2.10a-i names exactly which two remaining mechanisms hold the number
/// here. Its job is to make any further growth fail a test instead of
/// disappearing into a phase timer.
///
/// The value is asserted against the contract document by
/// `durability_counters::tests::the_contract_states_the_barrier_budget`, so
/// the two cannot drift apart.
pub const MANAGED_SAVE_BARRIER_BUDGET: u64 = 31;

/// The same ceiling for one accepted cross-page (for example cross-day) move,
/// which projects two pages and therefore pays the receipt-store cost twice.
pub const MANAGED_MOVE_BARRIER_BUDGET: u64 = 89;

/// The primitive kinds counted here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum Barrier {
    /// `fsync`/`FlushFileBuffers` of a regular file.
    File = 0,
    /// `fsync` of a directory: a durable name insertion or removal.
    Directory = 1,
    /// `syncfs` of a whole filesystem.
    Filesystem = 2,
}

const BARRIER_KINDS: usize = 3;

const BARRIER_NAMES: [&str; BARRIER_KINDS] = ["file_fsync", "dir_fsync", "syncfs"];

static TOTALS: [AtomicU64; BARRIER_KINDS] = [const { AtomicU64::new(0) }; BARRIER_KINDS];

type SessionCounts = Arc<[AtomicU64; BARRIER_KINDS]>;

thread_local! {
    /// The attribution channel this thread's barriers are additionally charged
    /// to, if any. Empty in production.
    static ATTRIBUTED: RefCell<Option<SessionCounts>> = const { RefCell::new(None) };
}

/// Record one barrier of `kind`.
#[inline]
pub(crate) fn note(kind: Barrier) {
    TOTALS[kind as usize].fetch_add(1, Ordering::Relaxed);
    attribute(kind, 1);
}

#[inline]
fn attribute(kind: Barrier, count: u64) {
    ATTRIBUTED.with(|slot| {
        if let Some(counts) = slot.borrow().as_ref() {
            counts[kind as usize].fetch_add(count, Ordering::Relaxed);
        }
    });
}

/// Record the two barriers that one `tine_storage` immutable publication
/// performs: the temporary file's `fsync`, and the containing directory's
/// `fsync` after the immutable name is installed.
///
/// Counting the storage crate's documented contract at its `tine-core` call
/// site keeps the per-operation sum complete without a `tine-storage` API
/// change. `tine_storage::publish_immutable_exact_impl` is the function whose
/// contract this mirrors; if it ever stops performing exactly one file barrier
/// and one directory barrier, this helper is what must change with it.
#[inline]
pub(crate) fn note_immutable_publication() {
    note(Barrier::File);
    note(Barrier::Directory);
}

/// A regular file or directory handle that can be forced to stable storage.
///
/// Both standard and capability-scoped handles occur throughout `tine-core`.
/// Keeping their raw primitive calls here gives the source guard one complete,
/// reviewable boundary for file and directory barriers.
pub(crate) trait DurableHandle {
    fn sync_to_stable_storage(&self) -> io::Result<()>;
}

impl DurableHandle for std::fs::File {
    fn sync_to_stable_storage(&self) -> io::Result<()> {
        self.sync_all()
    }
}

impl DurableHandle for cap_std::fs::File {
    fn sync_to_stable_storage(&self) -> io::Result<()> {
        self.sync_all()
    }
}

/// Force one regular file to stable storage and record its barrier.
#[inline]
pub(crate) fn sync_file(file: &impl DurableHandle) -> io::Result<()> {
    note(Barrier::File);
    file.sync_to_stable_storage()
}

/// Force one directory handle to stable storage and record its barrier.
#[inline]
pub(crate) fn sync_directory(directory: &impl DurableHandle) -> io::Result<()> {
    note(Barrier::Directory);
    directory.sync_to_stable_storage()
}

/// A point-in-time reading of every counter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BarrierCounts {
    counts: [u64; BARRIER_KINDS],
}

/// Read the process-wide totals.
pub fn snapshot() -> BarrierCounts {
    BarrierCounts {
        counts: std::array::from_fn(|index| TOTALS[index].load(Ordering::Relaxed)),
    }
}

impl BarrierCounts {
    /// Barriers of one kind.
    pub fn get(&self, kind: Barrier) -> u64 {
        self.counts[kind as usize]
    }

    /// Barriers of every kind.
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The counts accumulated between `earlier` and `self`.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            counts: std::array::from_fn(|index| {
                self.counts[index].saturating_sub(earlier.counts[index])
            }),
        }
    }

    /// A one-line rendering: `file_fsync=6 dir_fsync=13 syncfs=0 total=19`.
    pub fn report(&self) -> String {
        let mut out = String::new();
        for (index, name) in BARRIER_NAMES.iter().enumerate() {
            out.push_str(&format!("{name}={} ", self.counts[index]));
        }
        out.push_str(&format!("total={}", self.total()));
        out
    }
}

/// A thread-local attribution channel for durability barriers.
///
/// Open one before creating a managed runtime; the actor thread inherits it at
/// spawn, so [`BarrierSession::counts`] reports the barriers that runtime
/// performed — on the actor thread and on the opening thread — without seeing
/// the barriers of any concurrently running test.
///
/// Dropping the handle detaches the *opening* thread. Threads that inherited it
/// keep charging to the same counts until they exit, which is what makes an
/// actor's deferred drain measurable.
#[derive(Clone, Debug)]
pub struct BarrierSession {
    counts: SessionCounts,
}

impl BarrierSession {
    /// Open a session and attach the calling thread to it.
    pub fn begin() -> Self {
        let counts: SessionCounts = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
        ATTRIBUTED.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&counts)));
        Self { counts }
    }

    /// Attach the calling thread to this session.
    pub fn attach(&self) {
        ATTRIBUTED.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&self.counts)));
    }

    /// Detach the calling thread from any session.
    pub fn detach_current_thread() {
        ATTRIBUTED.with(|slot| *slot.borrow_mut() = None);
    }

    /// The barriers charged to this session so far.
    pub fn counts(&self) -> BarrierCounts {
        BarrierCounts {
            counts: std::array::from_fn(|index| self.counts[index].load(Ordering::Relaxed)),
        }
    }

    /// Reset this session's counts to zero, so the next measured operation
    /// starts from a clean slate without closing the session.
    pub fn reset(&self) {
        for counter in self.counts.iter() {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

/// The session the calling thread is attached to, if any.
///
/// A thread that is about to spawn a long-lived worker calls this and passes
/// the result into the new thread, which calls [`BarrierSession::attach`]. That
/// is how the managed actor thread inherits its creator's attribution.
pub fn current_session() -> Option<BarrierSession> {
    ATTRIBUTED.with(|slot| {
        slot.borrow().as_ref().map(|counts| BarrierSession {
            counts: Arc::clone(counts),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The living contract states the enforced budget. If a change moves the
    /// constant without moving the document (or the reverse), this fails.
    #[test]
    fn the_contract_states_the_barrier_budget() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        assert!(
            contract.contains("Durability barriers and the batch commit point"),
            "the storage contract must carry the durability-barrier section"
        );
        assert!(
            contract.contains(&format!(
                "`MANAGED_SAVE_BARRIER_BUDGET` = **{MANAGED_SAVE_BARRIER_BUDGET}**"
            )),
            "the storage contract must state the enforced save barrier budget \
             ({MANAGED_SAVE_BARRIER_BUDGET})"
        );
        assert!(
            contract.contains(&format!(
                "`MANAGED_MOVE_BARRIER_BUDGET` =\n**{MANAGED_MOVE_BARRIER_BUDGET}**"
            )) || contract.contains(&format!(
                "`MANAGED_MOVE_BARRIER_BUDGET` = **{MANAGED_MOVE_BARRIER_BUDGET}**"
            )),
            "the storage contract must state the enforced move barrier budget \
             ({MANAGED_MOVE_BARRIER_BUDGET})"
        );
    }

    #[test]
    fn no_read_path_reintroduces_a_durability_barrier() {
        let source = include_str!("model.rs");
        for banned in [
            "fn sync_and_read_projection_regular",
            "fn sync_open_and_read_projection_regular",
            "fn sync_and_reread_retained_projection_file",
        ] {
            assert!(
                !source.contains(banned),
                "a read path reintroduced a durability barrier: {banned}. \
                 See the removed-checks table in docs/storage-sync-contract.md."
            );
        }
    }

    #[test]
    fn production_barrier_primitives_stay_inside_counted_wrappers() {
        use std::collections::HashSet;
        use std::fs;
        use std::path::{Path, PathBuf};

        fn visit(directory: &Path, files: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, files);
                } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                    files.push(path);
                }
            }
        }

        fn module_directory(source_path: &Path) -> PathBuf {
            match source_path.file_name().and_then(|name| name.to_str()) {
                Some("lib.rs" | "mod.rs") => source_path.parent().unwrap().to_path_buf(),
                _ => source_path
                    .parent()
                    .unwrap()
                    .join(source_path.file_stem().unwrap()),
            }
        }

        fn test_only_external_modules(source_path: &Path, source: &str) -> Vec<PathBuf> {
            let module_directory = module_directory(source_path);
            let mut modules = Vec::new();
            for suffix in source.split("#[cfg(test)]").skip(1) {
                let declaration = suffix
                    .trim_start()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim();
                let declaration = declaration
                    .strip_prefix("pub(crate) ")
                    .or_else(|| declaration.strip_prefix("pub "))
                    .unwrap_or(declaration);
                let Some(name) = declaration
                    .strip_prefix("mod ")
                    .and_then(|name| name.strip_suffix(';'))
                else {
                    continue;
                };
                for candidate in [
                    module_directory.join(format!("{name}.rs")),
                    module_directory.join(name).join("mod.rs"),
                ] {
                    if candidate.exists() {
                        modules.push(candidate);
                    }
                }
            }
            modules
        }

        fn without_inline_test_modules(source: &str) -> String {
            fn matching_brace(source: &str, open: usize) -> Option<usize> {
                let bytes = source.as_bytes();
                let mut index = open;
                let mut depth = 0_usize;
                while index < bytes.len() {
                    if bytes[index..].starts_with(b"//") {
                        index += 2;
                        while index < bytes.len() && bytes[index] != b'\n' {
                            index += 1;
                        }
                        continue;
                    }
                    if bytes[index..].starts_with(b"/*") {
                        index += 2;
                        let mut comment_depth = 1_usize;
                        while index < bytes.len() && comment_depth > 0 {
                            if bytes[index..].starts_with(b"/*") {
                                comment_depth += 1;
                                index += 2;
                            } else if bytes[index..].starts_with(b"*/") {
                                comment_depth -= 1;
                                index += 2;
                            } else {
                                index += 1;
                            }
                        }
                        continue;
                    }
                    let raw_prefix = match bytes[index] {
                        b'r' => Some(index + 1),
                        b'b' if bytes.get(index + 1) == Some(&b'r') => Some(index + 2),
                        _ => None,
                    };
                    if let Some(mut delimiter) = raw_prefix {
                        let mut hashes = 0_usize;
                        while bytes.get(delimiter) == Some(&b'#') {
                            hashes += 1;
                            delimiter += 1;
                        }
                        if bytes.get(delimiter) == Some(&b'"') {
                            index = delimiter + 1;
                            while index < bytes.len() {
                                if bytes[index] == b'"'
                                    && bytes.get(index + 1..index + 1 + hashes).is_some_and(
                                        |suffix| suffix.iter().all(|byte| *byte == b'#'),
                                    )
                                {
                                    index += 1 + hashes;
                                    break;
                                }
                                index += 1;
                            }
                            continue;
                        }
                    }
                    let string_quote = bytes[index] == b'"'
                        || (bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'"'));
                    if string_quote {
                        if bytes[index] == b'b' {
                            index += 1;
                        }
                        index += 1;
                        while index < bytes.len() {
                            match bytes[index] {
                                b'\\' => index += 2,
                                b'"' => {
                                    index += 1;
                                    break;
                                }
                                _ => index += 1,
                            }
                        }
                        continue;
                    }
                    if bytes[index] == b'\'' {
                        let line_end = bytes[index + 1..]
                            .iter()
                            .position(|byte| *byte == b'\n')
                            .map_or(bytes.len(), |relative| index + 1 + relative);
                        if let Some(relative_close) = bytes[index + 1..line_end]
                            .iter()
                            .position(|byte| *byte == b'\'')
                        {
                            let close = index + 1 + relative_close;
                            if close.saturating_sub(index) <= 8 {
                                index = close + 1;
                                continue;
                            }
                        }
                    }
                    match bytes[index] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(index + 1);
                            }
                        }
                        _ => {}
                    }
                    index += 1;
                }
                None
            }

            let mut production = source.to_owned();
            let mut search_from = 0;
            while let Some(relative_attribute) = production[search_from..].find("#[cfg(test)]") {
                let attribute = search_from + relative_attribute;
                let mut item = attribute + "#[cfg(test)]".len();
                loop {
                    item += production[item..]
                        .find(|character: char| !character.is_whitespace())
                        .unwrap_or(production.len() - item);
                    if !production[item..].starts_with("#[") {
                        break;
                    }
                    let Some(attribute_end) = production[item..].find(']') else {
                        break;
                    };
                    item += attribute_end + 1;
                }
                if !production[item..].starts_with("mod ") {
                    search_from = item;
                    continue;
                }
                let Some(relative_open) = production[item..].find('{') else {
                    search_from = item;
                    continue;
                };
                if production[item..]
                    .find(';')
                    .is_some_and(|semicolon| semicolon < relative_open)
                {
                    search_from = item;
                    continue;
                }
                let open = item + relative_open;
                let close = matching_brace(&production, open)
                    .expect("a cfg(test) module must have balanced braces");
                production.replace_range(attribute..close, "");
                search_from = attribute;
            }
            production
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        visit(&root, &mut files);
        files.sort();

        // External modules declared directly behind `#[cfg(test)]` are not
        // production source. Discover them from their parent declarations
        // instead of maintaining a filename convention or an exclusion list.
        let test_only_files = files
            .iter()
            .flat_map(|path| {
                let source = fs::read_to_string(path).unwrap();
                test_only_external_modules(path, &source)
            })
            .collect::<HashSet<_>>();

        let primitive_patterns = [
            ".sync_all(",
            ".sync_data(",
            "libc::fsync(",
            "libc::fdatasync(",
            "libc::syncfs(",
            "rustix::fs::fsync(",
            "rustix::fs::fdatasync(",
            "rustix::fs::syncfs(",
            "nix::unistd::fsync(",
            "nix::unistd::fdatasync(",
            "FlushFileBuffers(",
            "NtFlushBuffersFile(",
            "F_FULLFSYNC",
            "F_BARRIERFSYNC",
        ];
        let mut raw_primitives = Vec::new();
        for path in files {
            if test_only_files.contains(&path) {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            // Inline modules declared directly behind `#[cfg(test)]` are also
            // test-only. Remove their complete brace-balanced bodies while
            // retaining production items that follow them in the same file.
            let production = without_inline_test_modules(&source);
            let compact = production
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>();
            let relative = path.strip_prefix(&root).unwrap().to_string_lossy();
            for pattern in primitive_patterns {
                for _ in 0..compact.matches(pattern).count() {
                    raw_primitives.push((relative.to_string(), pattern));
                }
            }
        }
        raw_primitives.sort();

        assert_eq!(
            raw_primitives,
            vec![
                ("durability_counters.rs".into(), ".sync_all("),
                ("durability_counters.rs".into(), ".sync_all("),
                ("filesystem_durability.rs".into(), "libc::syncfs("),
                ("filesystem_durability.rs".into(), "libc::syncfs("),
            ],
            "a production durability primitive exists outside the counted \
             file/directory wrappers in durability_counters.rs or the counted \
             filesystem wrappers in filesystem_durability.rs"
        );

        let wrappers = without_inline_test_modules(include_str!("durability_counters.rs"));
        assert_eq!(wrappers.matches("note(Barrier::File);").count(), 2);
        assert_eq!(wrappers.matches("note(Barrier::Directory);").count(), 2);
        let filesystem_wrappers = include_str!("filesystem_durability.rs");
        assert_eq!(
            filesystem_wrappers
                .matches("note(crate::durability_counters::Barrier::Filesystem);")
                .count(),
            2
        );
    }

    #[test]
    fn a_session_counts_only_its_own_threads() {
        let session = BarrierSession::begin();
        note(Barrier::File);
        note(Barrier::Directory);
        note(Barrier::Directory);
        let inherited = current_session().expect("the opening thread is attached");
        let worker = std::thread::spawn(move || {
            inherited.attach();
            note(Barrier::Filesystem);
        });
        worker.join().unwrap();
        let unattached = std::thread::spawn(|| note(Barrier::File));
        unattached.join().unwrap();

        let counts = session.counts();
        assert_eq!(counts.get(Barrier::File), 1);
        assert_eq!(counts.get(Barrier::Directory), 2);
        assert_eq!(counts.get(Barrier::Filesystem), 1);
        assert_eq!(counts.total(), 4);
        BarrierSession::detach_current_thread();
    }

    #[test]
    fn an_immutable_publication_costs_one_file_and_one_directory_barrier() {
        let session = BarrierSession::begin();
        note_immutable_publication();
        let counts = session.counts();
        assert_eq!(counts.get(Barrier::File), 1);
        assert_eq!(counts.get(Barrier::Directory), 1);
        BarrierSession::detach_current_thread();
    }

    #[test]
    fn resetting_a_session_starts_the_next_measurement_from_zero() {
        let session = BarrierSession::begin();
        note(Barrier::File);
        session.reset();
        note(Barrier::Directory);
        assert_eq!(session.counts().get(Barrier::File), 0);
        assert_eq!(session.counts().get(Barrier::Directory), 1);
        BarrierSession::detach_current_thread();
    }
}
