use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::CString;
use std::fmt;
#[cfg(target_os = "android")]
use std::fs;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write as _};
#[cfg(unix)]
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
#[cfg(any(test, target_os = "android"))]
use std::sync::Mutex;
#[cfg(target_os = "android")]
use std::sync::OnceLock;
use std::sync::RwLock;

#[cfg(windows)]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
#[cfg(not(target_os = "android"))]
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::object_store::{
    is_temp_name, open_dir_nofollow as open_dir_nofollow_strict,
    publish_immutable_exact as publish_immutable_exact_strict,
    read_optional_regular as read_optional_regular_strict, require_regular_entry,
    sync_dir_required, StoreError,
};
use super::sync_layout::{
    MUTATION_AUTHORITY_LEASE_SUFFIX, MUTATION_AUTHORITY_SUFFIX,
    PROJECTION_ATTEMPTS_DIR as ATTEMPTS_DIR, PROJECTION_BASES_DIR as BASES_DIR,
    PROJECTION_CLEANUP_ROUND_0_DIR, PROJECTION_CLEANUP_ROUND_1_DIR,
    PROJECTION_CLEANUP_ROUND_STATE_FILE as PENDING_CLEANUP_ROUND_STATE,
    PROJECTION_COMPLETIONS_DIR as COMPLETIONS_DIR, PROJECTION_FORENSICS_DIR as FORENSICS_DIR,
    PROJECTION_INTENTS_DIR as INTENTS_DIR,
    PROJECTION_PENDING_CLEANUP_AUTHORITY_FILE as PENDING_CLEANUP_AUTHORITY,
    PROJECTION_PENDING_CLEANUP_DIR as PENDING_CLEANUP_DIR,
    PROJECTION_PENDING_CLEANUP_SUFFIX as PENDING_CLEANUP_SUFFIX,
    PROJECTION_STORE_CLAIM_FILE as STORE_CLAIM_FILE, PROJECTION_STORE_INIT_FILE as STORE_INIT_FILE,
};
use super::{
    BaseBlob, BlobDescription, CapabilityCapturedProjectionInput,
    CapabilityCapturedProjectionState, ContentDigest, ManagedPath, ProjectionCompletedReceipt,
    ProjectionCompletion, ProjectionEndpointBinding, ProjectionIntent, ProjectionIntentId,
    ProjectionPrecondition, ProjectionReceiptStoreId, ProjectionWorkTarget, ReceiptError,
    WorkspaceId,
};
#[cfg(test)]
use crate::model::ProjectionRecoveryCleanup;
use crate::model::{Graph, ProjectionRecoveryEvidence, ProjectionWriteProof};

thread_local! {
    /// The turn executor records the exact per-page name seed before entering
    /// the retained receipt-backed writer. The store persists this value; it
    /// never derives a competing identity from its own resource id (2b soak).
    static PROJECTION_TURN_ATTEMPT: std::cell::Cell<Option<Uuid>> = const {
        std::cell::Cell::new(None)
    };
}

pub(crate) struct ProjectionTurnAttemptScope {
    previous: Option<Uuid>,
}

pub(crate) fn enter_projection_turn_attempt(attempt_id: Uuid) -> ProjectionTurnAttemptScope {
    let previous = PROJECTION_TURN_ATTEMPT.replace(Some(attempt_id));
    ProjectionTurnAttemptScope { previous }
}

impl Drop for ProjectionTurnAttemptScope {
    fn drop(&mut self) {
        PROJECTION_TURN_ATTEMPT.set(self.previous);
    }
}

const PENDING_CLEANUP_ROUND_DIRS: [&str; 2] = [
    PROJECTION_CLEANUP_ROUND_0_DIR,
    PROJECTION_CLEANUP_ROUND_1_DIR,
];

/// The current private receipt-store claim.
///
/// Sub-design (c) §1 (v19): the record format now carries an explicit
/// target-kind discriminant, and the 0.7 blank-slate decision says pre-(c)
/// stores are refused rather than migrated. The claim version IS that
/// transition -- there is no dual acceptance, no legacy classification, and no
/// migration apparatus. A pre-(c) magic falls into the store's existing
/// recognized-and-refused convention below, carrying the re-activation remedy;
/// the user's Markdown is untouched.
const STORE_CLAIM_MAGIC: &[u8; 8] = b"TINEPR6\0";
const PRIOR_STORE_CLAIM_MAGICS: [&[u8; 8]; 3] = [b"TINEPR5\0", b"TINEPR4\0", b"TINEPR3\0"];
const STORE_INIT_MAGIC: &[u8; 8] = b"TINEPI5\0";
const STORE_CLAIM_VERSION: u32 = 6;
const STORE_CLAIM_BASE_LEN: usize = STORE_CLAIM_MAGIC.len() + 4 + 32 + 16 + 1 + 16 + 16 + 32;
/// The version-specific claim envelope length. A current-magic claim of any
/// other length is malformed, never a shorter older format: this is what the
/// cold-open precheck holds a claim to before anything can mutate the graph.
pub(crate) const STORE_CLAIM_LEN: usize = STORE_CLAIM_BASE_LEN + 5 * 32;
const STORE_INIT_LEN: usize = STORE_CLAIM_BASE_LEN;
pub(crate) const MAX_PROJECTION_EVIDENCE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_PROJECTION_CATALOG_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PROJECTION_CATALOG_ROWS: usize = 2_000_000;
const MAX_PROJECTION_CATALOG_DIRECTORY_ENTRIES: usize = 4_000_000;
const LOCAL_ATTEMPT_SCHEMA_VERSION: u32 = 1;
const LOCAL_FORENSIC_SCHEMA_VERSION: u32 = 2;
const PRIOR_LOCAL_FORENSIC_SCHEMA_VERSION: u32 = 1;
const PENDING_CLEANUP_ROUND_STATE_SCHEMA_VERSION: u32 = 1;
const MAX_PENDING_CLEANUP_ROUND_STATE_BYTES: u64 = 4 * 1024;
const PENDING_CLEANUP_MARKER_SCHEMA_VERSION: u32 = 1;
const PENDING_CLEANUP_OBSERVATION_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_PENDING_PROJECTION_CLEANUP_PER_PASS: usize = 64;
const PENDING_CLEANUP_NAMESPACE_SCHEMA_VERSION: u32 = 1;
const MUTATION_AUTHORITY_SCHEMA_VERSION: u32 = 1;
const MAX_MUTATION_ATTEMPTS: usize = 1_000_000;
const MAX_MUTATION_AUTHORITY_BYTES: usize = 64 * 1024 * 1024;

type DirectoryIdentity = [u8; 32];

fn open_dir_nofollow(root: &Dir, name: &str) -> Result<Dir, StoreError> {
    #[cfg(target_os = "android")]
    {
        let component = Path::new(name);
        if !matches!(component.components().next(), Some(Component::Normal(_)))
            || component.components().count() != 1
        {
            return Err(StoreError::UnsafeEntry(format!(
                "private receipt directory name is not one normal component: {name}"
            )));
        }
        let name = CString::new(name)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid receipt directory"))?;
        // Android app-private receipt state has one honest Tine writer. Avoid
        // the Linux hostile-replacement flag that physical Android storage may
        // reject; the final handle is still checked to be a directory.
        let fd = unsafe {
            libc::openat(
                root.as_fd().as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
            )
        };
        if fd < 0 {
            return Err(StoreError::Io(io::Error::last_os_error()));
        }
        // SAFETY: openat returned one newly owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_dir() {
            return Err(StoreError::UnsafeEntry(
                "private receipt handle is not a directory".into(),
            ));
        }
        return Ok(Dir::from_std_file(file));
    }

    #[cfg(not(target_os = "android"))]
    open_dir_nofollow_strict(root, name)
}

/// Open one app-private receipt directory through Android's ordinary file API.
///
/// `cap_std::Dir::open_ambient_dir` deliberately retains a Linux-style
/// capability handle.  Some physical Android kernels permit ordinary access
/// to the app's private data directory but reject operations issued relative
/// to that handle.  The private receipt tree is single-writer Tine state, so
/// the honest-local boundary is the real-directory check before and after the
/// open, not the particular Linux descriptor flags used to reach it.
#[cfg(target_os = "android")]
fn open_android_private_directory(path: &Path) -> Result<Dir, ProjectionStoreError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(ProjectionStoreError::from)
        .map_err(|error| {
            error.at(format!(
                "inspect Android private directory {}",
                path.display()
            ))
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ProjectionStoreError::UnsafeEntry(format!(
            "Android private receipt path is not a real directory: {}",
            path.display()
        )));
    }
    let file = File::open(path)
        .map_err(ProjectionStoreError::from)
        .map_err(|error| error.at(format!("open Android private directory {}", path.display())))?;
    if !file
        .metadata()
        .map_err(ProjectionStoreError::from)
        .map_err(|error| {
            error.at(format!(
                "verify Android private directory {}",
                path.display()
            ))
        })?
        .is_dir()
    {
        return Err(ProjectionStoreError::UnsafeEntry(format!(
            "Android private receipt handle is not a directory: {}",
            path.display()
        )));
    }
    Ok(Dir::from_std_file(file))
}

#[cfg(target_os = "android")]
fn create_android_private_directory(path: &Path) -> Result<Dir, ProjectionStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ProjectionStoreError::UnsafeEntry(format!(
                "Android private receipt path is not a real directory: {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(ProjectionStoreError::from(error).at(format!(
                    "create Android private directory {}",
                    path.display()
                )));
            }
        },
        Err(error) => {
            return Err(ProjectionStoreError::from(error).at(format!(
                "inspect Android private directory {}",
                path.display()
            )));
        }
    }
    open_android_private_directory(path)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReceiptDirectoryDurability {
    /// The store has not yet published the claim enrollment will promote, so
    /// the namespace is still reconstructible from unchanged Direct Files.
    PrePromotionBootstrap,
    /// Enrollment has promoted the store identity. Its receipt names are now
    /// private durable authority and every directory barrier is strict.
    PromotedAuthority,
}

#[cfg(any(test, target_os = "android"))]
type AndroidReceiptDirectoryIdentity = (u64, u64);

#[cfg(any(test, target_os = "android"))]
#[derive(Debug, Default)]
struct AndroidReceiptBarrierState {
    verified_parents: Mutex<BTreeSet<AndroidReceiptDirectoryIdentity>>,
}

#[cfg(any(test, target_os = "android"))]
impl AndroidReceiptBarrierState {
    fn record_mutation_barrier<E>(
        &self,
        identity: AndroidReceiptDirectoryIdentity,
        barrier: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        // A namespace mutation invalidates the process-local proof before the
        // barrier runs. If the barrier refuses or panics, this process and a
        // future process both have to verify the parent before accepting an
        // exact existing name.
        self.verified_parents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&identity);
        barrier()?;
        self.verified_parents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(identity);
        Ok(())
    }

    fn verify_existing<E>(
        &self,
        identity: AndroidReceiptDirectoryIdentity,
        barrier: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        if self
            .verified_parents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&identity)
        {
            return Ok(());
        }
        barrier()?;
        self.verified_parents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(identity);
        Ok(())
    }
}

#[cfg(target_os = "android")]
fn android_receipt_barrier_state() -> &'static AndroidReceiptBarrierState {
    static STATE: OnceLock<AndroidReceiptBarrierState> = OnceLock::new();
    STATE.get_or_init(AndroidReceiptBarrierState::default)
}

#[cfg(target_os = "android")]
fn android_receipt_directory_identity(
    directory: &Dir,
) -> Result<AndroidReceiptDirectoryIdentity, StoreError> {
    let metadata = directory.try_clone()?.into_std_file().metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

/// A strict receipt barrier can refuse after the namespace mutation became
/// visible. Remember that exact parent within this process so only its next
/// idempotent retry re-establishes durability; ordinary existing-name reads do
/// not pay an extra device round trip. See storage-sync-contract.md §2.10c.
#[cfg(target_os = "android")]
fn sync_promoted_receipt_directory(directory: &Dir) -> Result<(), StoreError> {
    let identity = android_receipt_directory_identity(directory)?;
    android_receipt_barrier_state()
        .record_mutation_barrier(identity, || sync_dir_required(directory))
}

#[cfg(target_os = "android")]
fn verify_promoted_receipt_parent(directory: &Dir) -> Result<(), StoreError> {
    let identity = android_receipt_directory_identity(directory)?;
    android_receipt_barrier_state().verify_existing(identity, || sync_dir_required(directory))
}

fn ensure_directory_nofollow_with_durability(
    root: &Dir,
    name: &str,
    durability: ReceiptDirectoryDurability,
) -> Result<(), ProjectionStoreError> {
    #[cfg(target_os = "android")]
    {
        let component = Path::new(name);
        if !matches!(component.components().next(), Some(Component::Normal(_)))
            || component.components().count() != 1
        {
            return Err(ProjectionStoreError::UnsafeEntry(format!(
                "private receipt directory name is not one normal component: {name}"
            )));
        }
        let component = CString::new(name).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "invalid private receipt directory")
        })?;
        // Keep this on the same ordinary Android syscall boundary as the
        // directory open below. cap-std's create_dir adds Linux capability
        // preflights which some physical app-private filesystems reject even
        // though mkdirat/openat themselves are permitted.
        let created =
            unsafe { libc::mkdirat(root.as_fd().as_raw_fd(), component.as_ptr(), libc::S_IRWXU) };
        if created < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        // Reopen and classify the actual handle. Android app-private state has
        // one honest Tine writer, so an fstatat-style preflight adds no safety
        // but is rejected by some physical devices even when mkdir/open work.
        open_dir_nofollow(root, name)?;
        match durability {
            ReceiptDirectoryDurability::PrePromotionBootstrap => {
                crate::filesystem_durability::sync_reconstructible_directory(root)?;
            }
            ReceiptDirectoryDurability::PromotedAuthority => {
                sync_promoted_receipt_directory(root)?;
            }
        }
        return Ok(());
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = durability;
        super::object_store::ensure_directory_nofollow(root, name).map_err(Into::into)
    }
}

fn ensure_directory_nofollow(root: &Dir, name: &str) -> Result<(), ProjectionStoreError> {
    ensure_directory_nofollow_with_durability(
        root,
        name,
        ReceiptDirectoryDurability::PromotedAuthority,
    )
}

fn ensure_bootstrap_directory_nofollow(root: &Dir, name: &str) -> Result<(), ProjectionStoreError> {
    ensure_directory_nofollow_with_durability(
        root,
        name,
        ReceiptDirectoryDurability::PrePromotionBootstrap,
    )
}

fn read_optional_regular(
    dir: &Dir,
    path: &str,
    limit: u64,
    expected_length: Option<u64>,
) -> Result<Option<Vec<u8>>, StoreError> {
    #[cfg(target_os = "android")]
    {
        let component = Path::new(path);
        if !matches!(component.components().next(), Some(Component::Normal(_)))
            || component.components().count() != 1
        {
            return Err(StoreError::UnsafeEntry(format!(
                "private receipt filename is not one normal component: {path}"
            )));
        }
        let name = CString::new(path)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid receipt filename"))?;
        // Do not ask Android's app-private filesystem for Linux hostile-path
        // flags or an fstatat preflight. Open the honest-local name normally,
        // then validate the retained handle before reading any bytes.
        let fd = unsafe {
            libc::openat(
                dir.as_fd().as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        // SAFETY: openat returned one newly owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(StoreError::UnsafeEntry(format!(
                "private receipt path is not a regular file: {path}"
            )));
        }
        let length = metadata.len();
        if let Some(expected) = expected_length {
            if length != expected {
                return Err(StoreError::StoredLengthMismatch {
                    path: path.into(),
                    expected,
                    actual: length,
                });
            }
        }
        if length > limit {
            return Err(StoreError::StoredFileTooLarge {
                path: path.into(),
                length,
                limit,
            });
        }
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(StoreError::StoredFileTooLarge {
                path: path.into(),
                length: bytes.len() as u64,
                limit,
            });
        }
        if bytes.len() as u64 != length {
            return Err(StoreError::StoredLengthMismatch {
                path: path.into(),
                expected: length,
                actual: bytes.len() as u64,
            });
        }
        return Ok(Some(bytes));
    }

    #[cfg(not(target_os = "android"))]
    read_optional_regular_strict(dir, path, limit, expected_length)
}

fn publish_immutable_exact(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    kind: &'static str,
) -> Result<(), StoreError> {
    publish_immutable_exact_with_durability(
        dir,
        filename,
        bytes,
        kind,
        ReceiptDirectoryDurability::PromotedAuthority,
    )
}

fn publish_immutable_exact_with_durability(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    kind: &'static str,
    durability: ReceiptDirectoryDurability,
) -> Result<(), StoreError> {
    #[cfg(target_os = "android")]
    {
        return publish_android_private_immutable(dir, filename, bytes, kind, durability);
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = durability;
        crate::durability_counters::note_immutable_publication();
        publish_immutable_exact_strict(dir, filename, bytes, kind)
    }
}

fn publish_bootstrap_immutable_exact(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    kind: &'static str,
) -> Result<(), StoreError> {
    publish_immutable_exact_with_durability(
        dir,
        filename,
        bytes,
        kind,
        ReceiptDirectoryDurability::PrePromotionBootstrap,
    )
}

/// Android's app-private filesystem is single-writer from Tine's point of
/// view, but some devices reject the hard-link primitive used by the generic
/// no-replace publisher. Keep the crash-safe temporary-file publication and
/// exact collision check while using the ordinary atomic rename supported by
/// Android app storage. A concurrent hostile namespace writer is outside the
/// managed-storage threat model; honest concurrent Tine writers are excluded
/// by the runtime lease before this store becomes authoritative.
#[cfg(target_os = "android")]
fn publish_android_private_immutable(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    kind: &'static str,
    durability: ReceiptDirectoryDurability,
) -> Result<(), StoreError> {
    let verify_existing = || -> Result<bool, StoreError> {
        match read_optional_regular(dir, filename, bytes.len() as u64, Some(bytes.len() as u64)) {
            Ok(Some(existing)) if existing == bytes => Ok(true),
            Ok(Some(_)) => Err(StoreError::ImmutableCollision(kind)),
            Ok(None) => Ok(false),
            Err(
                StoreError::StoredLengthMismatch { .. } | StoreError::StoredFileTooLarge { .. },
            ) => Err(StoreError::ImmutableCollision(kind)),
            Err(error) => Err(error),
        }
    };

    // A failed strict directory barrier may leave the exact final name in the
    // page cache and namespace even though its insertion is not crash-durable.
    // The first promoted acceptance for each parent in every process therefore
    // establishes a barrier before accepting byte-identical publication. That
    // closes both same-process retry and process-death windows without adding a
    // barrier to later accepted names under the same unchanged parent.
    // Bootstrap state remains deliberately reconstructible.
    let accept_existing = || -> Result<bool, StoreError> {
        let exists = verify_existing()?;
        if exists && matches!(durability, ReceiptDirectoryDurability::PromotedAuthority) {
            verify_promoted_receipt_parent(dir)?;
        }
        Ok(exists)
    };

    if accept_existing()? {
        return Ok(());
    }

    let temp_name = format!(".tmp-{}", Uuid::new_v4());
    let temp_name_c = CString::new(temp_name.as_str())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid receipt temp filename"))?;
    let filename_c = CString::new(filename)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid receipt filename"))?;
    // As for receipt directories, use the ordinary Android primitive without
    // the hostile-namespace flags added by the generic capability publisher.
    // The retained file is still create-new, fully synced, and verified byte
    // for byte before the publication is accepted.
    let temp_fd = unsafe {
        libc::openat(
            dir.as_fd().as_raw_fd(),
            temp_name_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            libc::S_IRUSR | libc::S_IWUSR,
        )
    };
    if temp_fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: openat returned one newly owned descriptor.
    let mut temp = unsafe { File::from_raw_fd(temp_fd) };
    let result = (|| {
        temp.write_all(bytes)?;
        crate::durability_counters::sync_file(&temp)?;
        drop(temp);

        if accept_existing()? {
            return Ok(());
        }

        let renamed = unsafe {
            libc::renameat(
                dir.as_fd().as_raw_fd(),
                temp_name_c.as_ptr(),
                dir.as_fd().as_raw_fd(),
                filename_c.as_ptr(),
            )
        };
        if renamed < 0 {
            return Err(StoreError::Io(io::Error::last_os_error()));
        }
        match durability {
            ReceiptDirectoryDurability::PrePromotionBootstrap => {
                crate::filesystem_durability::sync_reconstructible_directory(dir)?;
            }
            ReceiptDirectoryDurability::PromotedAuthority => {
                sync_promoted_receipt_directory(dir)?;
            }
        }
        if !verify_existing()? {
            return Err(StoreError::Io(io::Error::new(
                ErrorKind::NotFound,
                format!("Android private immutable publication lost {filename}"),
            )));
        }
        Ok(())
    })();
    let cleanup = match unsafe { libc::unlinkat(dir.as_fd().as_raw_fd(), temp_name_c.as_ptr(), 0) }
    {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    };
    if let Err(error) = result {
        let _ = cleanup;
        return Err(error);
    }
    if cleanup
        .as_ref()
        .is_err_and(|error| error.kind() != ErrorKind::NotFound)
    {
        cleanup?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static MUTATION_AUTHORITY_CAPTURED_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static MUTATION_AUTHORITY_ACT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static MUTATION_AUTHORITY_LEASED_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static MUTATION_AUTHORITY_DROP_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static ATTEMPT_PUBLICATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static COMPLETION_PUBLICATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static COMPLETION_PUBLICATION_ACT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    /// Runs after the immutable completion is durable while its mutation
    /// authority still retains the durable slot.
    static COMPLETION_RETAINED_SLOT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static RECEIPT_SCAN_COUNTERS: std::cell::Cell<ProjectionStoreTestCounters> =
        std::cell::Cell::new(ProjectionStoreTestCounters::ZERO);
    static FAIL_BEFORE_PROJECTION_CLEANUP_MARKER_SWAP: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static FAIL_AFTER_PROJECTION_CLEANUP_MARKER_SWAP: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectionStoreTestCounters {
    pub completion_lookups: usize,
    pub catalog_directory_entries: usize,
    pub pending_cleanup_entries: usize,
}

#[cfg(test)]
impl ProjectionStoreTestCounters {
    const ZERO: Self = Self {
        completion_lookups: 0,
        catalog_directory_entries: 0,
        pending_cleanup_entries: 0,
    };
}

#[cfg(test)]
fn mutation_authority_captured_hook() {
    MUTATION_AUTHORITY_CAPTURED_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn mutation_authority_captured_hook() {}

#[cfg(test)]
fn mutation_authority_act_hook() {
    MUTATION_AUTHORITY_ACT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn mutation_authority_act_hook() {}

#[cfg(test)]
fn mutation_authority_leased_hook() {
    MUTATION_AUTHORITY_LEASED_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn mutation_authority_leased_hook() {}

#[cfg(test)]
fn mutation_authority_drop_hook() {
    MUTATION_AUTHORITY_DROP_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn mutation_authority_drop_hook() {}

#[cfg(test)]
fn attempt_publication_hook() {
    ATTEMPT_PUBLICATION_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn attempt_publication_hook() {}

#[cfg(test)]
fn completion_publication_hook() {
    COMPLETION_PUBLICATION_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn completion_publication_hook() {}

#[cfg(test)]
fn completion_publication_act_hook() {
    COMPLETION_PUBLICATION_ACT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn completion_publication_act_hook() {}

#[cfg(test)]
fn completion_retained_slot_hook() {
    COMPLETION_RETAINED_SLOT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn count_completion_lookup() {
    RECEIPT_SCAN_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.completion_lookups += 1;
        counters.set(value);
    });
}

#[cfg(test)]
fn count_catalog_directory_entry() {
    RECEIPT_SCAN_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.catalog_directory_entries += 1;
        counters.set(value);
    });
}

#[cfg(test)]
fn count_pending_cleanup_entry() {
    RECEIPT_SCAN_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.pending_cleanup_entries += 1;
        counters.set(value);
    });
}

#[cfg(test)]
pub(crate) fn reset_projection_store_test_counters() {
    RECEIPT_SCAN_COUNTERS.with(|counters| counters.set(ProjectionStoreTestCounters::ZERO));
}

/// Clear all receipt-store one-shot fault hooks without affecting measurements.
#[cfg(test)]
pub(crate) fn reset_projection_store_test_hooks() {
    MUTATION_AUTHORITY_CAPTURED_HOOK.with(|hook| drop(hook.borrow_mut().take()));
    MUTATION_AUTHORITY_ACT_HOOK.with(|hook| drop(hook.borrow_mut().take()));
    MUTATION_AUTHORITY_LEASED_HOOK.with(|hook| drop(hook.borrow_mut().take()));
    MUTATION_AUTHORITY_DROP_HOOK.with(|hook| drop(hook.borrow_mut().take()));
    ATTEMPT_PUBLICATION_HOOK.with(|hook| drop(hook.borrow_mut().take()));
    COMPLETION_PUBLICATION_HOOK.with(|hook| drop(hook.borrow_mut().take()));
    COMPLETION_PUBLICATION_ACT_HOOK.with(|hook| drop(hook.borrow_mut().take()));
    COMPLETION_RETAINED_SLOT_HOOK.with(|hook| drop(hook.borrow_mut().take()));
    FAIL_BEFORE_PROJECTION_CLEANUP_MARKER_SWAP.with(|fail| fail.set(false));
    FAIL_AFTER_PROJECTION_CLEANUP_MARKER_SWAP.with(|fail| fail.set(false));
}

#[cfg(test)]
fn fail_before_projection_cleanup_marker_swap_for_test() {
    FAIL_BEFORE_PROJECTION_CLEANUP_MARKER_SWAP.with(|fail| fail.set(true));
}

#[cfg(test)]
fn fail_after_projection_cleanup_marker_swap_for_test() {
    FAIL_AFTER_PROJECTION_CLEANUP_MARKER_SWAP.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn projection_store_test_counters() -> ProjectionStoreTestCounters {
    RECEIPT_SCAN_COUNTERS.with(std::cell::Cell::get)
}

#[derive(Debug)]
struct BoundNamespace {
    capability: Dir,
    identity: DirectoryIdentity,
}

#[derive(Debug)]
struct ReceiptNamespaces {
    bases: BoundNamespace,
    intents: BoundNamespace,
    completions: BoundNamespace,
    attempts: BoundNamespace,
    forensics: BoundNamespace,
    pending_cleanup: BoundNamespace,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingCleanupNamespaceAuthority {
    schema_version: u32,
    store_id: ProjectionReceiptStoreId,
    directory_identity: DirectoryIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingCleanupRoundState {
    schema_version: u32,
    store_id: ProjectionReceiptStoreId,
    namespace_identity: DirectoryIdentity,
    round_identities: [DirectoryIdentity; 2],
    active_round: u8,
}

struct OpenPendingCleanupRounds {
    state: PendingCleanupRoundState,
    state_bytes: Vec<u8>,
    rounds: [Dir; 2],
}

impl ReceiptNamespaces {
    fn get(&self, name: &str) -> Option<&BoundNamespace> {
        match name {
            BASES_DIR => Some(&self.bases),
            INTENTS_DIR => Some(&self.intents),
            COMPLETIONS_DIR => Some(&self.completions),
            ATTEMPTS_DIR => Some(&self.attempts),
            FORENSICS_DIR => Some(&self.forensics),
            _ => None,
        }
    }

    fn identities(&self) -> [DirectoryIdentity; 5] {
        [
            self.bases.identity,
            self.intents.identity,
            self.completions.identity,
            self.attempts.identity,
            self.forensics.identity,
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableProjectionMutationAuthority {
    schema_version: u32,
    authority_id: Uuid,
    store_id: ProjectionReceiptStoreId,
    store_claim_digest: [u8; 32],
    workspace_id: WorkspaceId,
    endpoint_binding: Option<Vec<u8>>,
    intent_id: ProjectionIntentId,
    intent_digest: [u8; 32],
    base: Option<BlobDescription>,
    namespace_identities: [DirectoryIdentity; 5],
    attempts_identity: DirectoryIdentity,
    forensics_identity: DirectoryIdentity,
    active_attempt_id: Option<Uuid>,
    reservation_bytes: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionAttemptReservation {
    schema_version: u32,
    intent_id: ProjectionIntentId,
    attempt_id: Uuid,
    target_path: ManagedPath,
    recovery_filename: String,
}

impl ProjectionAttemptReservation {
    pub const fn intent_id(&self) -> ProjectionIntentId {
        self.intent_id
    }

    pub const fn attempt_id(&self) -> Uuid {
        self.attempt_id
    }

    pub fn target_path(&self) -> &ManagedPath {
        &self.target_path
    }

    pub fn recovery_filename(&self) -> &str {
        &self.recovery_filename
    }

    #[cfg(test)]
    pub(crate) fn for_test(target_path: &str) -> Self {
        let target_path = ManagedPath::parse(target_path).expect("valid test projection path");
        let target_filename = target_path.file_name().to_owned();
        let attempt_id = Uuid::new_v4();
        Self {
            schema_version: LOCAL_ATTEMPT_SCHEMA_VERSION,
            intent_id: ProjectionIntentId::test_only_zero(),
            attempt_id,
            target_path,
            recovery_filename: format!(
                ".{target_filename}.{}.projection.recovery",
                attempt_id.simple()
            ),
        }
    }

    fn new(intent: &ProjectionIntent, attempt_id: Uuid) -> Result<Self, ProjectionStoreError> {
        let target_filename = intent.path().file_name();
        let reservation = Self {
            schema_version: LOCAL_ATTEMPT_SCHEMA_VERSION,
            intent_id: intent.id()?,
            attempt_id,
            target_path: intent.path().clone(),
            recovery_filename: format!(
                ".{target_filename}.{}.projection.recovery",
                attempt_id.simple()
            ),
        };
        reservation.validate(intent)?;
        Ok(reservation)
    }

    fn validate(&self, intent: &ProjectionIntent) -> Result<(), ProjectionStoreError> {
        let expected = Self::new_unchecked(intent, self.attempt_id)?;
        if self.schema_version != LOCAL_ATTEMPT_SCHEMA_VERSION
            || self.intent_id != intent.id()?
            || self.target_path != *intent.path()
            || self.recovery_filename != expected.recovery_filename
        {
            return Err(ProjectionStoreError::AttemptBindingMismatch);
        }
        Ok(())
    }

    fn new_unchecked(
        intent: &ProjectionIntent,
        attempt_id: Uuid,
    ) -> Result<Self, ProjectionStoreError> {
        let target_filename = intent.path().file_name();
        Ok(Self {
            schema_version: LOCAL_ATTEMPT_SCHEMA_VERSION,
            intent_id: intent.id()?,
            attempt_id,
            target_path: intent.path().clone(),
            recovery_filename: format!(
                ".{target_filename}.{}.projection.recovery",
                attempt_id.simple()
            ),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalProjectionEvidenceRecord {
    schema_version: u32,
    intent_id: ProjectionIntentId,
    attempt_id: Uuid,
    target_path: ManagedPath,
    recovery_relative_path: String,
    recovery_filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery_resource_id: Option<ContentDigest>,
    observed: BlobDescription,
}

impl LocalProjectionEvidenceRecord {
    pub const fn intent_id(&self) -> ProjectionIntentId {
        self.intent_id
    }

    pub const fn attempt_id(&self) -> Uuid {
        self.attempt_id
    }

    pub fn recovery_relative_path(&self) -> &str {
        &self.recovery_relative_path
    }

    pub fn recovery_filename(&self) -> &str {
        &self.recovery_filename
    }

    pub const fn recovery_resource_id(&self) -> Option<ContentDigest> {
        self.recovery_resource_id
    }

    pub const fn observed(&self) -> BlobDescription {
        self.observed
    }

    pub const fn is_cleanup_bound(&self) -> bool {
        self.schema_version == LOCAL_FORENSIC_SCHEMA_VERSION && self.recovery_resource_id.is_some()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingProjectionCleanupObservation {
    schema_version: u32,
    evidence_digest: [u8; 32],
    session_id: Uuid,
    observed_unix_seconds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingProjectionCleanupMarker {
    schema_version: u32,
    evidence: LocalProjectionEvidenceRecord,
    observation: Option<PendingProjectionCleanupObservation>,
}

impl PendingProjectionCleanupMarker {
    fn new(evidence: LocalProjectionEvidenceRecord) -> Self {
        Self {
            schema_version: PENDING_CLEANUP_MARKER_SCHEMA_VERSION,
            evidence,
            observation: None,
        }
    }

    fn validate(&self) -> Result<(), ProjectionStoreError> {
        let evidence_digest = local_forensic_record_digest(&self.evidence)?;
        if self.schema_version != PENDING_CLEANUP_MARKER_SCHEMA_VERSION
            || !self.evidence.is_cleanup_bound()
            || self.observation.as_ref().is_some_and(|observation| {
                observation.schema_version != PENDING_CLEANUP_OBSERVATION_SCHEMA_VERSION
                    || observation.evidence_digest != evidence_digest
            })
        {
            return Err(ProjectionStoreError::ForensicBindingMismatch);
        }
        Ok(())
    }
}

/// Disconnected immutable storage for projection bases, intents, and completions.
///
/// Opening this store is never performed by graph startup. Every path operation
/// remains relative to the retained no-follow directory capability.
#[derive(Debug)]
pub struct ProjectionReceiptStore {
    root_path: PathBuf,
    store_id: ProjectionReceiptStoreId,
    workspace_id: WorkspaceId,
    endpoint: Option<ProjectionEndpointBinding>,
    capability: Dir,
    namespaces: ReceiptNamespaces,
    retired_own_endpoint_intents: RwLock<BTreeSet<ProjectionIntentId>>,
}

/// Private one-shot authority spanning one exact graph operation and its
/// completion publication. The durable record at the receipt-store root is a
/// recovery-stable witness even if a validated child namespace is moved after
/// the graph operation starts.
pub(crate) struct ProjectionMutationAuthority {
    durable: DurableProjectionMutationAuthority,
    durable_bytes: Vec<u8>,
    durable_name: String,
    _lease: File,
    root: Dir,
    bases: Dir,
    intents: Dir,
    attempts_parent: Dir,
    attempts: Dir,
    forensics_parent: Dir,
    forensics: Dir,
    pending_cleanup: Dir,
    completions: Dir,
    reservations: Vec<ProjectionAttemptReservation>,
    active: Option<ProjectionAttemptReservation>,
    created_durable_record: bool,
    graph_operation_consumed: bool,
    completion_published: bool,
}

/// One-shot graph mutation evidence derived exclusively from the durable
/// projection turn currently being replayed. Unlike
/// [`ProjectionMutationAuthority`], this authors and consults no receipt-store
/// artifact: the turn supplies the stable attempt id and the local completion
/// index supplies own-endpoint execution evidence.
pub(crate) struct ProjectionTurnMutationAuthority {
    reservation: ProjectionAttemptReservation,
    graph_operation_consumed: bool,
}

pub(crate) trait ProjectionMutationEvidence {
    fn consume_write_evidence<T>(
        &mut self,
        relative_path: &str,
        operation: impl FnOnce(
            &ProjectionAttemptReservation,
            &[ProjectionAttemptReservation],
            Option<&ProjectionRecoveryEvidencePublisher<'_>>,
        ) -> io::Result<T>,
    ) -> io::Result<T>;

    fn consume_recovery_evidence<T>(
        &mut self,
        relative_path: &str,
        operation: impl FnOnce(&[ProjectionAttemptReservation]) -> io::Result<T>,
    ) -> io::Result<T>;
}

pub(crate) struct ProjectionRecoveryEvidencePublisher<'a> {
    pending_cleanup: &'a Dir,
    forensics: &'a Dir,
    store_id: ProjectionReceiptStoreId,
    intent_id: ProjectionIntentId,
    target_path: &'a ManagedPath,
    reservation: &'a ProjectionAttemptReservation,
}

impl ProjectionRecoveryEvidencePublisher<'_> {
    pub(crate) fn publish(&self, evidence: &ProjectionRecoveryEvidence) -> io::Result<()> {
        let expected_relative_path = self
            .target_path
            .join_sibling(evidence.filename())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid recovery evidence"))?;
        if self.reservation.recovery_filename() != evidence.filename()
            || expected_relative_path != evidence.path()
            || evidence.resource_id().is_none()
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "recovery evidence is not bound to the active displaced handle",
            ));
        }
        let record = LocalProjectionEvidenceRecord {
            schema_version: LOCAL_FORENSIC_SCHEMA_VERSION,
            intent_id: self.intent_id,
            attempt_id: self.reservation.attempt_id(),
            target_path: self.target_path.clone(),
            recovery_relative_path: evidence.path().to_owned(),
            recovery_filename: evidence.filename().to_owned(),
            recovery_resource_id: evidence.resource_id(),
            observed: BlobDescription::from_parts(*evidence.digest(), evidence.len()),
        };
        let bytes = serde_json::to_vec(&record)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
        let pending_bytes =
            encode_pending_cleanup_marker(&PendingProjectionCleanupMarker::new(record.clone()))
                .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
        publish_pending_cleanup_marker(
            self.pending_cleanup,
            self.store_id,
            &record,
            &pending_bytes,
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        publish_immutable_exact(
            self.forensics,
            &format!("{}.evidence", hex(&digest)),
            &bytes,
            "local projection forensic evidence",
        )
        .map_err(|error| io::Error::other(error.to_string()))
    }
}

impl fmt::Debug for ProjectionMutationAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectionMutationAuthority")
            .field("store_id", &self.durable.store_id)
            .field("intent_id", &self.durable.intent_id)
            .field("authority_id", &self.durable.authority_id)
            .field("graph_operation_consumed", &self.graph_operation_consumed)
            .finish_non_exhaustive()
    }
}

impl ProjectionTurnMutationAuthority {
    pub(crate) fn for_current_turn(
        intent: &ProjectionIntent,
    ) -> Result<Self, ProjectionStoreError> {
        let attempt_id = if let Some(attempt_id) = PROJECTION_TURN_ATTEMPT.get() {
            attempt_id
        } else {
            #[cfg(test)]
            {
                let mut bytes = [0_u8; 16];
                bytes.copy_from_slice(&intent.id()?.as_bytes()[..16]);
                bytes[6] = (bytes[6] & 0x0f) | 0x80;
                bytes[8] = (bytes[8] & 0x3f) | 0x80;
                Uuid::from_bytes(bytes)
            }
            #[cfg(not(test))]
            {
                return Err(ProjectionStoreError::MissingTurnAttemptContext);
            }
        };
        Ok(Self {
            reservation: ProjectionAttemptReservation::new(intent, attempt_id)?,
            graph_operation_consumed: false,
        })
    }

    pub(crate) fn cleanup_records(
        &self,
        intent: &ProjectionIntent,
        proof: &ProjectionWriteProof,
    ) -> Result<Vec<LocalProjectionEvidenceRecord>, ProjectionStoreError> {
        if !self.graph_operation_consumed
            || proof.path() != intent.path().as_str()
            || proof.digest() != intent.target().sha256()
            || BlobDescription::of(proof.bytes()) != intent.target()
        {
            return Err(ProjectionStoreError::WriteProofMismatch);
        }
        proof
            .recovery_evidence()
            .iter()
            .map(|evidence| {
                if self.reservation.recovery_filename() != evidence.filename() {
                    return Err(ProjectionStoreError::UnreservedRecoveryEvidence);
                }
                Ok(LocalProjectionEvidenceRecord {
                    schema_version: LOCAL_FORENSIC_SCHEMA_VERSION,
                    intent_id: intent.id()?,
                    attempt_id: self.reservation.attempt_id(),
                    target_path: intent.path().clone(),
                    recovery_relative_path: evidence.path().to_owned(),
                    recovery_filename: evidence.filename().to_owned(),
                    recovery_resource_id: evidence.resource_id(),
                    observed: BlobDescription::from_parts(*evidence.digest(), evidence.len()),
                })
            })
            .collect()
    }

    fn consume_graph_operation(&mut self, relative_path: &str) -> io::Result<()> {
        if self.graph_operation_consumed {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "projection turn mutation authority was already consumed",
            ));
        }
        if self.reservation.target_path().as_str() != relative_path {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "projection turn mutation authority target path mismatch",
            ));
        }
        self.graph_operation_consumed = true;
        Ok(())
    }
}

/// Canonical read-only catalog row used only by the combined import authority.
///
/// Fields stay crate-private so a downstream caller cannot manufacture a
/// durable intent/completion claim. Construction validates the entire intent
/// and completion namespaces, including exact base bytes and orphan entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectionCatalogEntry {
    pub(crate) intent: ProjectionIntent,
    pub(crate) completion: Option<ProjectionCompletion>,
}

impl ProjectionReceiptStore {
    pub fn open(root: &Path, workspace_id: WorkspaceId) -> Result<Self, ProjectionStoreError> {
        Self::open_with_binding(root, workspace_id, None)
    }

    /// Refuse an unusable private receipt store BEFORE anything can touch the
    /// user's graph tree.
    ///
    /// Sub-design (c) §1 (R20-C1/R21-C1). The store's full claim validation is
    /// already the first thing `open` does, and it is kept as defense in depth
    /// — but on the clean cold-open path it runs *after* `Graph::open_checked`,
    /// whose publication recovery renames graph files and moves artifacts to
    /// `.trash/`. A store this build cannot serve must not get that far.
    ///
    /// In-scope scenario: an honest pre-(c) private store meets a (c) build
    /// (Martin's own dev devices). Recovery is re-activation; the Markdown is
    /// intact and untouched. The torn-claim arm additionally covers an
    /// interrupted or truncated write of the claim itself: a current-magic
    /// header on a short body must not pass, or graph recovery runs before the
    /// in-place length check ever fires.
    ///
    /// The precheck is read-only and mutates nothing on any path. It applies
    /// only to a store the caller has already proven authoritative; a fresh
    /// store has no claim to check and initializes exactly as today.
    pub(crate) fn precheck_authoritative_claim(root: &Path) -> Result<(), ProjectionStoreError> {
        // No private store directory at all: nothing to refuse, and
        // initialization owns the state.
        match std::fs::symlink_metadata(root) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(ProjectionStoreError::from(StoreError::Io(error))
                    .at("inspect private receipt store for claim precheck"))
            }
        }
        // Android app-private storage cannot take the hostile-replacement open
        // flags; it uses the same checked opener the rest of this module does.
        #[cfg(target_os = "android")]
        let capability = open_android_private_directory(root)?;
        #[cfg(not(target_os = "android"))]
        let capability = Dir::open_ambient_dir(root, ambient_authority()).map_err(|error| {
            ProjectionStoreError::from(StoreError::Io(error))
                .at("open private receipt store for claim precheck")
        })?;
        let metadata = match capability.symlink_metadata(STORE_CLAIM_FILE) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ProjectionStoreError::from(StoreError::Io(error))
                    .at("read private receipt store claim"))
            }
        };
        let Some(metadata) = metadata else {
            // A claimless store root. The claim provably predates the
            // activation authority marker, so on an authoritative store a
            // populated claimless root is the in-place claimless-nonempty
            // refusal, reached one step earlier. An empty or absent root is
            // left to initialization exactly as today.
            if capability
                .entries()
                .map_err(|error| {
                    ProjectionStoreError::from(StoreError::Io(error))
                        .at("enumerate private receipt store for claim precheck")
                })?
                .next()
                .transpose()
                .map_err(|error| {
                    ProjectionStoreError::from(StoreError::Io(error))
                        .at("enumerate private receipt store for claim precheck")
                })?
                .is_some()
            {
                return Err(ProjectionStoreError::ClaimlessNonemptyStore);
            }
            return Ok(());
        };
        if !metadata.is_file() {
            return Err(ProjectionStoreError::MalformedStoreClaim);
        }
        let mut bytes = vec![0_u8; STORE_CLAIM_LEN + 1];
        let read = {
            let mut file = capability.open(STORE_CLAIM_FILE).map_err(|error| {
                ProjectionStoreError::from(StoreError::Io(error))
                    .at("read private receipt store claim")
            })?;
            read_claim_prefix(&mut file, &mut bytes).map_err(|error| {
                ProjectionStoreError::from(StoreError::Io(error))
                    .at("read private receipt store claim")
            })?
        };
        bytes.truncate(read);
        classify_precheck_claim(&bytes)
    }

    /// Open a receipt namespace durably enrolled to one endpoint and one exact
    /// graph-root filesystem resource.
    pub fn open_for_endpoint(
        root: &Path,
        workspace_id: WorkspaceId,
        endpoint: ProjectionEndpointBinding,
    ) -> Result<Self, ProjectionStoreError> {
        Self::open_with_binding(root, workspace_id, Some(endpoint))
    }

    /// Open an enrolled receipt namespace without creating its root, claim, or
    /// child namespaces.
    ///
    /// This is the runtime-host reopen boundary. The expected physical store
    /// identity comes from the authenticated enrollment, so replacing a
    /// missing store with a newly initialized directory cannot silently become
    /// authority.
    pub(crate) fn open_existing_for_endpoint(
        root: &Path,
        workspace_id: WorkspaceId,
        endpoint: ProjectionEndpointBinding,
        expected_store_id: ProjectionReceiptStoreId,
    ) -> Result<Self, ProjectionStoreError> {
        let name = root
            .file_name()
            .ok_or_else(|| ProjectionStoreError::UnsafeEntry("store root has no name".into()))?;
        if !matches!(root.components().next_back(), Some(Component::Normal(_))) {
            return Err(ProjectionStoreError::UnsafeEntry(
                "store root must end in a normal path component".into(),
            ));
        }
        let name = name.to_str().ok_or_else(|| {
            ProjectionStoreError::UnsafeEntry("store root name is not UTF-8".into())
        })?;
        let parent = root.parent().ok_or_else(|| {
            ProjectionStoreError::UnsafeEntry("store root has no existing parent".into())
        })?;
        let canonical_parent = std::fs::canonicalize(parent)?;
        #[cfg(target_os = "android")]
        let capability = open_android_private_directory(&canonical_parent.join(name))?;
        #[cfg(not(target_os = "android"))]
        let capability = {
            let parent_capability = Dir::open_ambient_dir(&canonical_parent, ambient_authority())?;
            open_dir_nofollow(&parent_capability, name)?
        };
        let store_id = canonical_receipt_store_id(&capability)?;
        if store_id != expected_store_id {
            return Err(ProjectionStoreError::EndpointBindingMismatch);
        }
        let bytes = read_optional_regular(&capability, STORE_CLAIM_FILE, 512, None)?
            .ok_or(ProjectionStoreError::MalformedStoreClaim)?;
        let expected = validate_claim(&bytes, store_id, workspace_id, Some(endpoint))?;
        let namespaces = open_receipt_namespaces(
            &capability,
            store_id,
            ReceiptDirectoryDurability::PromotedAuthority,
        )?;
        if namespaces.identities() != expected {
            return Err(ProjectionStoreError::NamespaceSubstitution(
                "top-level receipt namespace".into(),
            ));
        }
        Ok(Self {
            root_path: canonical_parent.join(name),
            store_id,
            workspace_id,
            endpoint: Some(endpoint),
            capability,
            namespaces,
            retired_own_endpoint_intents: RwLock::new(BTreeSet::new()),
        })
    }

    fn open_with_binding(
        root: &Path,
        workspace_id: WorkspaceId,
        endpoint: Option<ProjectionEndpointBinding>,
    ) -> Result<Self, ProjectionStoreError> {
        let name = root
            .file_name()
            .ok_or_else(|| ProjectionStoreError::UnsafeEntry("store root has no name".into()))?;
        if !matches!(root.components().next_back(), Some(Component::Normal(_))) {
            return Err(ProjectionStoreError::UnsafeEntry(
                "store root must end in a normal path component".into(),
            ));
        }
        let name = name.to_str().ok_or_else(|| {
            ProjectionStoreError::UnsafeEntry("store root name is not UTF-8".into())
        })?;
        let parent = root.parent().ok_or_else(|| {
            ProjectionStoreError::UnsafeEntry("store root has no existing parent".into())
        })?;
        let canonical_parent = std::fs::canonicalize(parent)
            .map_err(ProjectionStoreError::from)
            .map_err(|error| error.at("canonicalize private receipt parent"))?;
        #[cfg(target_os = "android")]
        let capability = create_android_private_directory(&canonical_parent.join(name))
            .map_err(|error| error.at("create Android private receipt root"))?;
        #[cfg(not(target_os = "android"))]
        let capability = {
            let parent_capability = Dir::open_ambient_dir(&canonical_parent, ambient_authority())
                .map_err(ProjectionStoreError::from)
                .map_err(|error| error.at("open private receipt parent"))?;
            ensure_directory_nofollow(&parent_capability, name)
                .map_err(|error| error.at("create private receipt root"))?;
            open_dir_nofollow(&parent_capability, name)
                .map_err(ProjectionStoreError::from)
                .map_err(|error| error.at("open private receipt root"))?
        };
        let store_id = canonical_receipt_store_id(&capability)
            .map_err(|error| error.at("identify private receipt root"))?;
        let namespaces = Self::initialize(&capability, store_id, workspace_id, endpoint)
            .map_err(|error| error.at("initialize private receipt store"))?;

        Ok(Self {
            root_path: canonical_parent.join(name),
            store_id,
            workspace_id,
            endpoint,
            capability,
            namespaces,
            retired_own_endpoint_intents: RwLock::new(BTreeSet::new()),
        })
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub const fn store_id(&self) -> ProjectionReceiptStoreId {
        self.store_id
    }

    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub const fn endpoint_binding(&self) -> Option<ProjectionEndpointBinding> {
        self.endpoint
    }

    /// Capture an exact authoring precondition through Graph's retained
    /// no-follow capability. A present input is accepted only with a completion
    /// reloaded from this enrolled store and bound to the exact current bytes.
    pub fn capture_projection_input(
        &self,
        graph: &Graph,
        endpoint: ProjectionEndpointBinding,
        path: ManagedPath,
        prior_intent: Option<&ProjectionIntent>,
    ) -> Result<CapabilityCapturedProjectionInput, ProjectionStoreError> {
        self.require_endpoint(endpoint)?;
        let graph_resource_id = graph
            .canonical_resource_id()
            .map_err(ProjectionStoreError::Io)?;
        if graph_resource_id != endpoint.graph_resource_id {
            return Err(ProjectionStoreError::GraphResourceMismatch);
        }
        let current = graph
            .read_projection_input(&path)
            .map_err(ProjectionStoreError::Io)?;
        let state = match (current, prior_intent) {
            (None, None) => CapabilityCapturedProjectionState::Absent,
            (None, Some(_)) => return Err(ProjectionStoreError::CapturedInputMismatch),
            (Some(_), None) => return Err(ProjectionStoreError::MissingPriorCompletion),
            (Some(bytes), Some(intent)) => {
                if intent.workspace_id() != self.workspace_id
                    || intent.path() != &path
                    || intent.target() != BlobDescription::of(&bytes)
                {
                    return Err(ProjectionStoreError::CapturedInputMismatch);
                }
                let prior_completion = self
                    .load_completion(intent)?
                    .ok_or(ProjectionStoreError::MissingPriorCompletion)?;
                CapabilityCapturedProjectionState::Present {
                    bytes,
                    prior_intent: intent.clone(),
                    prior_completion,
                }
            }
        };
        Ok(CapabilityCapturedProjectionInput::from_graph_capability(
            path,
            endpoint,
            self.store_id,
            state,
        ))
    }

    /// Publish immutable base bytes first and the canonical intent last.
    pub fn publish_intent(
        &self,
        intent: &ProjectionIntent,
        base_bytes: Option<&[u8]>,
    ) -> Result<ProjectionIntentId, ProjectionStoreError> {
        self.require_workspace(intent)?;
        let bytes = intent.encode()?;
        require_evidence_length(
            "projection target",
            intent.target().byte_length(),
            MAX_PROJECTION_EVIDENCE_BYTES,
        )?;
        require_evidence_length(
            "projection intent",
            bytes.len() as u64,
            MAX_PROJECTION_EVIDENCE_BYTES,
        )?;

        let intent_id = intent.id()?;
        match (intent.precondition(), base_bytes) {
            (ProjectionPrecondition::Absent, None) => {}
            (ProjectionPrecondition::Absent, Some(_)) => {
                return Err(ProjectionStoreError::UnexpectedBase);
            }
            (ProjectionPrecondition::Base(description), None) => {
                return Err(ProjectionStoreError::MissingBase(*description));
            }
            (ProjectionPrecondition::Base(description), Some(base_bytes)) => {
                require_evidence_length(
                    "projection base",
                    description.byte_length(),
                    MAX_PROJECTION_EVIDENCE_BYTES,
                )?;
                if BlobDescription::of(base_bytes) != *description {
                    return Err(ProjectionStoreError::BaseEvidenceMismatch(*description));
                }
                let bases = self.namespace(BASES_DIR)?;
                publish_immutable_exact(
                    &bases,
                    &base_filename(*description),
                    base_bytes,
                    "projection base",
                )?;
            }
        }

        let intents = self.namespace(INTENTS_DIR)?;
        let intent_name = intent_filename(intent_id);
        let already_published =
            read_optional_regular(&intents, &intent_name, MAX_PROJECTION_EVIDENCE_BYTES, None)?
                .is_some();
        // The intent is the commit marker for its local recovery namespaces.
        // Once it is visible, both per-intent directory identities must
        // already be durably bound and can never be recreated by name.
        if already_published {
            self.required_intent_namespace(ATTEMPTS_DIR, intent_id)?;
            self.required_intent_namespace(FORENSICS_DIR, intent_id)?;
        } else {
            self.intent_namespace(ATTEMPTS_DIR, intent_id)?;
            self.intent_namespace(FORENSICS_DIR, intent_id)?;
        }
        publish_immutable_exact(&intents, &intent_name, &bytes, "projection intent")?;
        Ok(intent_id)
    }

    /// Load and validate the intent and every base byte needed to authorize it.
    pub fn load_intent(
        &self,
        intent_id: ProjectionIntentId,
    ) -> Result<Option<ProjectionIntent>, ProjectionStoreError> {
        let intents = self.namespace(INTENTS_DIR)?;
        let Some(bytes) = read_optional_regular(
            &intents,
            &intent_filename(intent_id),
            MAX_PROJECTION_EVIDENCE_BYTES,
            None,
        )?
        else {
            return Ok(None);
        };
        let intent = ProjectionIntent::decode(&bytes)?;
        self.require_workspace(&intent)?;
        if intent.id()? != intent_id {
            return Err(ProjectionStoreError::PathBindingMismatch(
                "projection intent",
            ));
        }
        if intent.encode()? != bytes {
            return Err(ProjectionStoreError::NonCanonical("projection intent"));
        }
        self.load_base(&intent)?;
        Ok(Some(intent))
    }

    /// Retrieve exact base bytes from the immutable base namespace.
    pub fn load_base(
        &self,
        intent: &ProjectionIntent,
    ) -> Result<Option<BaseBlob>, ProjectionStoreError> {
        self.require_workspace(intent)?;
        let ProjectionPrecondition::Base(description) = intent.precondition() else {
            return Ok(None);
        };
        require_evidence_length(
            "projection base",
            description.byte_length(),
            MAX_PROJECTION_EVIDENCE_BYTES,
        )?;
        let bases = self.namespace(BASES_DIR)?;
        let filename = base_filename(*description);
        let bytes = read_optional_regular(
            &bases,
            &filename,
            MAX_PROJECTION_EVIDENCE_BYTES,
            Some(description.byte_length()),
        )?
        .ok_or(ProjectionStoreError::MissingBase(*description))?;
        if BlobDescription::of(&bytes) != *description {
            return Err(ProjectionStoreError::BaseEvidenceMismatch(*description));
        }
        Ok(Some(BaseBlob::from_parts(*description, bytes)?))
    }

    /// Retrieve an exact content-addressed projection base retained for any
    /// intent. Sweep restoration uses this only after an authenticated prior
    /// Present intent names the same description; the blob is layout evidence,
    /// never the semantic restore payload.
    pub(crate) fn load_retained_base(
        &self,
        description: BlobDescription,
    ) -> Result<Option<BaseBlob>, ProjectionStoreError> {
        require_evidence_length(
            "projection base",
            description.byte_length(),
            MAX_PROJECTION_EVIDENCE_BYTES,
        )?;
        let bases = self.namespace(BASES_DIR)?;
        let bytes = read_optional_regular(
            &bases,
            &base_filename(description),
            MAX_PROJECTION_EVIDENCE_BYTES,
            Some(description.byte_length()),
        )?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        if BlobDescription::of(&bytes) != description {
            return Err(ProjectionStoreError::BaseEvidenceMismatch(description));
        }
        Ok(Some(BaseBlob::from_parts(description, bytes)?))
    }

    /// Durably reserve the exact recovery filename Graph must use before any
    /// live page name can be retired or published.
    pub fn reserve_attempt(
        &self,
        intent: &ProjectionIntent,
    ) -> Result<ProjectionAttemptReservation, ProjectionStoreError> {
        let intent_id = self.require_published_intent(intent)?;
        let _lease = self.acquire_mutation_lease(intent_id)?;
        mutation_authority_leased_hook();
        self.reserve_attempt_under_lease(intent, intent_id)
    }

    fn reserve_attempt_under_lease(
        &self,
        intent: &ProjectionIntent,
        intent_id: ProjectionIntentId,
    ) -> Result<ProjectionAttemptReservation, ProjectionStoreError> {
        let attempt_id = self.turn_attempt_id(intent_id)?;
        self.reserve_deterministic_attempt_under_lease(intent, intent_id, attempt_id, true)
    }

    pub(crate) fn reserve_fallback_attempt(
        &self,
        intent: &ProjectionIntent,
    ) -> Result<ProjectionAttemptReservation, ProjectionStoreError> {
        let intent_id = self.require_published_intent(intent)?;
        let _lease = self.acquire_mutation_lease(intent_id)?;
        mutation_authority_leased_hook();
        let attempt_id = self.turn_attempt_id(intent_id)?;
        self.reserve_deterministic_attempt_under_lease(intent, intent_id, attempt_id, true)
    }

    fn turn_attempt_id(&self, intent_id: ProjectionIntentId) -> Result<Uuid, ProjectionStoreError> {
        if let Some(attempt_id) = PROJECTION_TURN_ATTEMPT.get() {
            return Ok(attempt_id);
        }
        #[cfg(test)]
        {
            // Unit tests that exercise the receipt store below the turn
            // executor retain their historical deterministic fixture seed.
            return Ok(deterministic_mutation_uuid(
                b"tine/projection-attempt/v1\0",
                self.store_id,
                intent_id,
            ));
        }
        #[cfg(not(test))]
        {
            let _ = intent_id;
            Err(ProjectionStoreError::MissingTurnAttemptContext)
        }
    }

    fn reserve_deterministic_attempt_under_lease(
        &self,
        intent: &ProjectionIntent,
        intent_id: ProjectionIntentId,
        attempt_id: Uuid,
        reuse_any: bool,
    ) -> Result<ProjectionAttemptReservation, ProjectionStoreError> {
        let durable_name = mutation_authority_filename(intent_id);
        if read_optional_mutation_authority(&self.capability, &durable_name, None)?.is_some() {
            return Err(ProjectionStoreError::MutationAuthorityPending);
        }
        let attempts = self.required_intent_namespace(ATTEMPTS_DIR, intent_id)?;
        let reservations = self.load_attempt_reservations_from(intent, &attempts)?;
        let existing = reservations
            .iter()
            .find(|reservation| reservation.attempt_id() == attempt_id)
            .or_else(|| {
                if reuse_any {
                    reservations.first()
                } else {
                    None
                }
            });
        let reservation = if let Some(existing) = existing {
            existing.clone()
        } else {
            if reservations.len() == MAX_MUTATION_ATTEMPTS {
                return Err(ProjectionStoreError::MutationAuthorityTooLarge {
                    attempts: reservations.len() + 1,
                    bytes: 0,
                });
            }
            let reservation = ProjectionAttemptReservation::new(intent, attempt_id)?;
            let bytes = serde_json::to_vec(&reservation)
                .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?;
            publish_immutable_exact(
                &attempts,
                &attempt_filename(reservation.attempt_id),
                &bytes,
                "projection attempt reservation",
            )?;
            reservation
        };
        attempt_publication_hook();
        Ok(reservation)
    }

    /// Load only this intent's bounded attempt namespace. Recovery never scans
    /// graph page directories or other intents for generated-name patterns.
    pub fn load_attempt_reservations(
        &self,
        intent: &ProjectionIntent,
    ) -> Result<Vec<ProjectionAttemptReservation>, ProjectionStoreError> {
        let intent_id = self.require_published_intent(intent)?;
        let attempts = self.required_intent_namespace(ATTEMPTS_DIR, intent_id)?;
        self.load_attempt_reservations_from(intent, &attempts)
    }

    fn load_attempt_reservations_from(
        &self,
        intent: &ProjectionIntent,
        attempts: &Dir,
    ) -> Result<Vec<ProjectionAttemptReservation>, ProjectionStoreError> {
        let mut reservations = Vec::new();
        for entry in attempts.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry("non-UTF-8 projection attempt entry".into())
            })?;
            require_regular_entry(&entry.file_type()?, name)?;
            if is_temp_name(name) {
                continue;
            }
            let attempt_id = parse_attempt_filename(name)?;
            let bytes =
                read_optional_regular(&attempts, name, MAX_PROJECTION_EVIDENCE_BYTES, None)?
                    .ok_or_else(|| {
                        ProjectionStoreError::UnsafeEntry(format!(
                            "projection attempt disappeared during enumeration: {name}"
                        ))
                    })?;
            let reservation: ProjectionAttemptReservation = serde_json::from_slice(&bytes)
                .map_err(|error| ProjectionStoreError::Decode(error.to_string()))?;
            reservation.validate(intent)?;
            if reservation.attempt_id != attempt_id
                || serde_json::to_vec(&reservation)
                    .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?
                    != bytes
            {
                return Err(ProjectionStoreError::AttemptBindingMismatch);
            }
            if reservations.len() == MAX_MUTATION_ATTEMPTS {
                return Err(ProjectionStoreError::MutationAuthorityTooLarge {
                    attempts: reservations.len() + 1,
                    bytes: 0,
                });
            }
            reservations.push(reservation);
        }
        reservations.sort_unstable_by_key(ProjectionAttemptReservation::attempt_id);
        Ok(reservations)
    }

    /// Seal the exact receipt capabilities and canonical attempt bytes that one
    /// graph operation may consume. The root-level immutable record is written
    /// before this authority can cross into Graph.
    pub(crate) fn begin_mutation(
        &self,
        intent: &ProjectionIntent,
        active: Option<&ProjectionAttemptReservation>,
    ) -> Result<ProjectionMutationAuthority, ProjectionStoreError> {
        let intent_id = self.require_published_intent(intent)?;
        let lease = self.acquire_mutation_lease(intent_id)?;
        mutation_authority_leased_hook();
        let durable_name = mutation_authority_filename(intent_id);
        let existing_durable_bytes =
            read_optional_mutation_authority(&self.capability, &durable_name, None)?;
        // A newly established recovery slot carries one fresh current attempt
        // in addition to all retained evidence. If proof-only recovery finds
        // that the target was retired before publication, the same immutable
        // slot can authorize the guarded writer without appending on retries.
        let recovery_active = if existing_durable_bytes.is_none() && active.is_none() {
            Some(self.reserve_attempt_under_lease(intent, intent_id)?)
        } else {
            None
        };
        let requested_active = active.cloned().or(recovery_active);
        let store_claim = read_optional_regular(
            &self.capability,
            STORE_CLAIM_FILE,
            STORE_CLAIM_LEN as u64,
            Some(STORE_CLAIM_LEN as u64),
        )?
        .ok_or(ProjectionStoreError::MalformedStoreClaim)?;
        let bases = self.namespace(BASES_DIR)?;
        let intents = self.namespace(INTENTS_DIR)?;
        let attempts_parent = self.namespace(ATTEMPTS_DIR)?;
        let attempts = self.required_intent_namespace(ATTEMPTS_DIR, intent_id)?;
        let forensics_parent = self.namespace(FORENSICS_DIR)?;
        let forensics = self.required_intent_namespace(FORENSICS_DIR, intent_id)?;
        let completions = self.namespace(COMPLETIONS_DIR)?;
        let reservations = self.load_attempt_reservations_from(intent, &attempts)?;
        if let Some(active) = active {
            active.validate(intent)?;
            if !reservations.iter().any(|reservation| reservation == active) {
                return Err(ProjectionStoreError::AttemptBindingMismatch);
            }
        }
        let reservation_bytes = reservations
            .iter()
            .map(|reservation| {
                serde_json::to_vec(reservation)
                    .map_err(|error| ProjectionStoreError::Encode(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let intent_bytes = intent.encode()?;
        let base = match intent.precondition() {
            ProjectionPrecondition::Absent => None,
            ProjectionPrecondition::Base(description) => {
                let bytes = read_optional_regular(
                    &bases,
                    &base_filename(*description),
                    MAX_PROJECTION_EVIDENCE_BYTES,
                    Some(description.byte_length()),
                )?
                .ok_or(ProjectionStoreError::MissingBase(*description))?;
                if BlobDescription::of(&bytes) != *description {
                    return Err(ProjectionStoreError::BaseEvidenceMismatch(*description));
                }
                Some(*description)
            }
        };
        let store_claim_digest = Sha256::digest(&store_claim).into();
        let endpoint_binding = self.endpoint.map(endpoint_binding_bytes);
        let intent_digest = Sha256::digest(&intent_bytes).into();
        let namespace_identities = self.namespaces.identities();
        let attempts_identity = canonical_directory_identity(&attempts)?;
        let forensics_identity = canonical_directory_identity(&forensics)?;
        let (durable, durable_bytes, reservations, authority_active, created_durable_record) =
            if let Some(durable_bytes) = existing_durable_bytes {
                let durable = decode_durable_mutation_authority(
                    &durable_bytes,
                    intent,
                    self.store_id,
                    store_claim_digest,
                    self.workspace_id,
                    endpoint_binding.as_deref(),
                    intent_id,
                    intent_digest,
                    base,
                    namespace_identities,
                    attempts_identity,
                    forensics_identity,
                    &reservations,
                )?;
                let reservations = decode_mutation_reservations(&durable, intent)?;
                let durable_active = durable.active_attempt_id.and_then(|active_attempt_id| {
                    reservations
                        .iter()
                        .find(|reservation| reservation.attempt_id() == active_attempt_id)
                        .cloned()
                });
                if requested_active.is_some() && requested_active != durable_active {
                    return Err(ProjectionStoreError::MutationAuthorityPending);
                }
                (
                    durable,
                    durable_bytes,
                    reservations,
                    requested_active.or(durable_active),
                    false,
                )
            } else {
                let authority_active = requested_active;
                let durable = DurableProjectionMutationAuthority {
                    schema_version: MUTATION_AUTHORITY_SCHEMA_VERSION,
                    authority_id: deterministic_mutation_uuid(
                        b"tine/projection-mutation-authority/v1\0",
                        self.store_id,
                        intent_id,
                    ),
                    store_id: self.store_id,
                    store_claim_digest,
                    workspace_id: self.workspace_id,
                    endpoint_binding,
                    intent_id,
                    intent_digest,
                    base,
                    namespace_identities,
                    attempts_identity,
                    forensics_identity,
                    active_attempt_id: authority_active
                        .as_ref()
                        .map(ProjectionAttemptReservation::attempt_id),
                    reservation_bytes,
                };
                let durable_bytes = serde_json::to_vec(&durable)
                    .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?;
                if durable_bytes.len() > MAX_MUTATION_AUTHORITY_BYTES {
                    return Err(ProjectionStoreError::MutationAuthorityTooLarge {
                        attempts: reservations.len(),
                        bytes: durable_bytes.len(),
                    });
                }
                publish_immutable_exact(
                    &self.capability,
                    &durable_name,
                    &durable_bytes,
                    "projection graph mutation authority",
                )?;
                (durable, durable_bytes, reservations, authority_active, true)
            };
        let authority = ProjectionMutationAuthority {
            durable,
            durable_bytes,
            durable_name,
            _lease: lease,
            root: self.capability.try_clone()?,
            bases,
            intents,
            attempts_parent,
            attempts,
            forensics_parent,
            forensics,
            pending_cleanup: self.namespaces.pending_cleanup.capability.try_clone()?,
            completions,
            reservations,
            active: authority_active,
            created_durable_record,
            graph_operation_consumed: false,
            completion_published: false,
        };
        authority.validate_live_names()?;
        mutation_authority_captured_hook();
        Ok(authority)
    }

    /// Publish completion only through the same one-shot capability session
    /// that Graph consumed for the exact mutation or recovery operation.
    pub(crate) fn publish_completion(
        &self,
        mut authority: ProjectionMutationAuthority,
        intent: &ProjectionIntent,
        proof: &ProjectionWriteProof,
    ) -> Result<ProjectionCompletion, ProjectionStoreError> {
        completion_publication_hook();
        completion_publication_act_hook();
        authority.consume_completion_publication(self, intent, |authority| {
            self.require_write_proof(intent, proof)?;
            let intent_id = authority.durable.intent_id;
            for evidence in proof.recovery_evidence() {
                let reservation = authority
                    .reservations
                    .iter()
                    .find(|reservation| reservation.recovery_filename() == evidence.filename())
                    .ok_or(ProjectionStoreError::UnreservedRecoveryEvidence)?;
                let record = LocalProjectionEvidenceRecord {
                    schema_version: LOCAL_FORENSIC_SCHEMA_VERSION,
                    intent_id,
                    attempt_id: reservation.attempt_id(),
                    target_path: intent.path().clone(),
                    recovery_relative_path: evidence.path().to_owned(),
                    recovery_filename: evidence.filename().to_owned(),
                    recovery_resource_id: evidence.resource_id(),
                    observed: BlobDescription::from_parts(*evidence.digest(), evidence.len()),
                };
                self.validate_forensic_record_with_reservation(intent, &record, reservation)?;
                let record_bytes = serde_json::to_vec(&record)
                    .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?;
                if record.is_cleanup_bound() {
                    let pending_bytes = encode_pending_cleanup_marker(
                        &PendingProjectionCleanupMarker::new(record.clone()),
                    )?;
                    publish_pending_cleanup_marker(
                        &authority.pending_cleanup,
                        self.store_id,
                        &record,
                        &pending_bytes,
                    )?;
                }
                let digest: [u8; 32] = Sha256::digest(&record_bytes).into();
                publish_immutable_exact(
                    &authority.forensics,
                    &format!("{}.evidence", hex(&digest)),
                    &record_bytes,
                    "local projection forensic evidence",
                )?;
            }
            let completion = ProjectionCompletion::for_intent(intent, proof.bytes())?;
            let bytes = completion.encode()?;
            require_evidence_length(
                "projection completion",
                bytes.len() as u64,
                MAX_PROJECTION_EVIDENCE_BYTES,
            )?;
            publish_immutable_exact(
                &authority.completions,
                &completion_filename(intent_id),
                &bytes,
                "projection completion",
            )?;
            #[cfg(test)]
            completion_retained_slot_hook();
            Ok(completion)
        })
    }

    pub fn load_completion(
        &self,
        intent: &ProjectionIntent,
    ) -> Result<Option<ProjectionCompletion>, ProjectionStoreError> {
        #[cfg(test)]
        count_completion_lookup();
        let candidate_id = intent.id()?;
        if self
            .retired_own_endpoint_intents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&candidate_id)
        {
            return Ok(None);
        }
        let intent_id = self.require_published_intent(intent)?;
        let completions = self.namespace(COMPLETIONS_DIR)?;
        let Some(bytes) = read_optional_regular(
            &completions,
            &completion_filename(intent_id),
            MAX_PROJECTION_EVIDENCE_BYTES,
            None,
        )?
        else {
            return Ok(None);
        };
        let completion = ProjectionCompletion::decode_bound(&bytes, intent)?;
        if completion.encode()? != bytes {
            return Err(ProjectionStoreError::NonCanonical("projection completion"));
        }
        self.reconcile_completed_mutation(intent, intent_id)?;
        Ok(Some(completion))
    }

    fn load_intent_by_id(
        &self,
        intent_id: ProjectionIntentId,
    ) -> Result<ProjectionIntent, ProjectionStoreError> {
        let intents = self.namespace(INTENTS_DIR)?;
        let filename = intent_filename(intent_id);
        let bytes =
            read_optional_regular(&intents, &filename, MAX_PROJECTION_EVIDENCE_BYTES, None)?
                .ok_or(ProjectionStoreError::MissingIntent(intent_id))?;
        let intent = ProjectionIntent::decode(&bytes)?;
        self.require_workspace(&intent)?;
        if intent.encode()? != bytes
            || intent.id()? != intent_id
            || filename != intent_filename(intent.id()?)
        {
            return Err(ProjectionStoreError::PathBindingMismatch(
                "projection intent",
            ));
        }
        Ok(intent)
    }

    /// Load one completed receipt through an authenticated work-index row.
    /// This performs direct immutable intent/completion reads only; it never
    /// enumerates a receipt namespace.
    pub(crate) fn load_completed_receipt(
        &self,
        completed: &ProjectionCompletedReceipt,
    ) -> Result<(ProjectionIntent, ProjectionCompletion), ProjectionStoreError> {
        crate::fast_commit::note_projection_receipt_load();
        let intent =
            self.load_intent_by_id(completed.intent_id())
                .map_err(|error| match error {
                    ProjectionStoreError::MissingIntent(_) => {
                        ProjectionStoreError::MissingPriorCompletion
                    }
                    error => error,
                })?;
        let target_matches = match completed.target() {
            ProjectionWorkTarget::Absent => intent.target() == BlobDescription::of(&[]),
            ProjectionWorkTarget::Present(target) => intent.target() == target,
        };
        if intent.page_id() != completed.page_id()
            || intent.path() != completed.path()
            || intent.frontier() != completed.frontier()
            || !target_matches
        {
            return Err(ProjectionStoreError::EndpointBindingMismatch);
        }
        let completion = self
            .load_completion(&intent)?
            .ok_or(ProjectionStoreError::MissingPriorCompletion)?;
        if completion.logical_completion_id() != completed.logical_completion_id() {
            return Err(ProjectionStoreError::PathBindingMismatch(
                "projection completed-work mapping",
            ));
        }
        Ok((intent, completion))
    }

    pub fn local_forensic_evidence(
        &self,
        intent: &ProjectionIntent,
    ) -> Result<Vec<LocalProjectionEvidenceRecord>, ProjectionStoreError> {
        let intent_id = self.require_published_intent(intent)?;
        let forensics = self.required_intent_namespace(FORENSICS_DIR, intent_id)?;
        let mut records = Vec::new();
        for entry in forensics.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry("non-UTF-8 forensic evidence entry".into())
            })?;
            require_regular_entry(&entry.file_type()?, name)?;
            if is_temp_name(name) {
                continue;
            }
            require_canonical_evidence_name(name, ".evidence")?;
            let bytes =
                read_optional_regular(&forensics, name, MAX_PROJECTION_EVIDENCE_BYTES, None)?
                    .ok_or_else(|| {
                        ProjectionStoreError::UnsafeEntry(format!(
                            "forensic evidence disappeared during enumeration: {name}"
                        ))
                    })?;
            let record = decode_local_forensic_record(&bytes)?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            if name != format!("{}.evidence", hex(&digest)) {
                return Err(ProjectionStoreError::ForensicBindingMismatch);
            }
            self.validate_forensic_record(intent, &record)?;
            records.push(record);
        }
        records.sort_unstable_by_key(LocalProjectionEvidenceRecord::attempt_id);
        Ok(records)
    }

    /// Load only live, exact cleanup markers. Historical completions and their
    /// forensic namespaces are never enumerated by this restart index.
    #[cfg(test)]
    pub(crate) fn pending_projection_cleanup(
        &self,
    ) -> Result<Vec<(ProjectionIntent, LocalProjectionEvidenceRecord)>, ProjectionStoreError> {
        let (_, _, retired_pending_prefixes) = self.retired_own_endpoint_names();
        let queue = open_pending_cleanup_rounds(
            &self.namespaces.pending_cleanup.capability,
            self.store_id,
            self.namespaces.pending_cleanup.identity,
        )?;
        let mut pending = Vec::new();
        for round in &queue.rounds {
            for entry in round.entries()? {
                let entry = entry?;
                count_pending_cleanup_entry();
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    ProjectionStoreError::UnsafeEntry(
                        "non-UTF-8 pending projection cleanup entry".into(),
                    )
                })?;
                if retired_pending_prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                {
                    continue;
                }
                if is_temp_name(name) {
                    continue;
                }
                if !name.ends_with(PENDING_CLEANUP_SUFFIX) {
                    return Err(ProjectionStoreError::UnsafeEntry(format!(
                        "unknown pending projection cleanup entry: {name}"
                    )));
                }
                require_regular_entry(&entry.file_type()?, name)?;
                let bytes =
                    read_optional_mutation_authority(round, name, None)?.ok_or_else(|| {
                        ProjectionStoreError::UnsafeEntry(format!(
                            "pending projection cleanup disappeared during enumeration: {name}"
                        ))
                    })?;
                let marker = decode_pending_cleanup_marker(&bytes)?;
                let record = marker.evidence;
                if !record.is_cleanup_bound() || pending_cleanup_filename(&record) != name {
                    return Err(ProjectionStoreError::ForensicBindingMismatch);
                }
                let intent = self.load_intent_by_id(record.intent_id())?;
                self.validate_forensic_record(&intent, &record)?;
                pending.push((intent, record));
            }
        }
        pending.sort_unstable_by_key(|(_, record)| (record.intent_id(), record.attempt_id()));
        Ok(pending)
    }

    /// Rotate at most `max_entries` out of the current round. Retained markers
    /// move to the other authenticated round before they are returned, while
    /// new markers are appended to that same inactive round. The active round
    /// flips only after it is empty, so no retained prefix can be revisited
    /// until every marker that shared its round has received a bounded visit.
    /// The flip is durable, so it is elided when BOTH rounds are empty: there
    /// is then nothing to make reachable and the write would cost a barrier per
    /// call on the ordinary save path, where the queue is empty.
    pub(crate) fn pending_projection_cleanup_bounded(
        &self,
        max_entries: usize,
    ) -> Result<Vec<(ProjectionIntent, LocalProjectionEvidenceRecord)>, ProjectionStoreError> {
        if max_entries == 0 {
            return Ok(Vec::new());
        }
        let (_, _, retired_pending_prefixes) = self.retired_own_endpoint_names();
        let is_retired_own_name = |name: &str| {
            retired_pending_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix))
        };
        let namespace = &self.namespaces.pending_cleanup.capability;
        let mut queue = open_pending_cleanup_rounds(
            namespace,
            self.store_id,
            self.namespaces.pending_cleanup.identity,
        )?;
        let mut active = usize::from(queue.state.active_round);
        let mut entries = queue.rounds[active].entries()?;
        let mut first = next_non_retired_pending_entry(&mut entries, &retired_pending_prefixes)?;
        if first.is_none() {
            drop(entries);
            // An empty active round normally means "flip, then drain whatever
            // the other round retained". When the inactive round is empty too,
            // the entire queue is empty: the flip has nothing to make
            // reachable, yet it still writes the round state durably and
            // barriers the namespace directory. That is the ordinary case —
            // every accepted save enters this function twice with nothing
            // queued — so peek the inactive round first. The peek reads a
            // directory and writes nothing, and it uses the same entry
            // semantics as the enumerator below: a round holding only
            // removable temporary entries is NOT empty here, so it falls
            // through to the flip and the existing temp-removal path rather
            // than inventing a second cleanup route.
            let mut inactive_entries = queue.rounds[1 - active].entries()?;
            if next_non_retired_pending_entry(&mut inactive_entries, &retired_pending_prefixes)?
                .is_none()
            {
                return Ok(Vec::new());
            }
            drop(inactive_entries);
            flip_pending_cleanup_round(namespace, &queue)?;
            queue = open_pending_cleanup_rounds(
                namespace,
                self.store_id,
                self.namespaces.pending_cleanup.identity,
            )?;
            active = usize::from(queue.state.active_round);
            entries = queue.rounds[active].entries()?;
            first = next_non_retired_pending_entry(&mut entries, &retired_pending_prefixes)?;
        }
        let Some(first) = first else {
            return Ok(Vec::new());
        };
        let inactive = 1usize - active;
        let mut pending = Vec::new();
        let mut removed_temporary = false;
        let mut rotated = false;
        let mut visited_live_entries = 0;
        for entry in std::iter::once(Ok(first)).chain(entries) {
            let entry = entry?;
            #[cfg(test)]
            count_pending_cleanup_entry();
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry(
                    "non-UTF-8 pending projection cleanup entry".into(),
                )
            })?;
            if is_retired_own_name(name) {
                continue;
            }
            if visited_live_entries == max_entries {
                break;
            }
            visited_live_entries += 1;
            if is_temp_name(name) {
                require_regular_entry(&entry.file_type()?, name)?;
                queue.rounds[active].remove_file(name)?;
                removed_temporary = true;
                continue;
            }
            if !name.ends_with(PENDING_CLEANUP_SUFFIX) {
                return Err(ProjectionStoreError::UnsafeEntry(format!(
                    "unknown pending projection cleanup entry: {name}"
                )));
            }
            require_regular_entry(&entry.file_type()?, name)?;
            let bytes = read_optional_mutation_authority(&queue.rounds[active], name, None)?
                .ok_or_else(|| {
                    ProjectionStoreError::UnsafeEntry(format!(
                        "pending projection cleanup disappeared during enumeration: {name}"
                    ))
                })?;
            let marker = decode_pending_cleanup_marker(&bytes)?;
            let record = marker.evidence;
            if !record.is_cleanup_bound() || pending_cleanup_filename(&record) != name {
                return Err(ProjectionStoreError::ForensicBindingMismatch);
            }
            let intent = self.load_intent_by_id(record.intent_id())?;
            self.validate_forensic_record(&intent, &record)?;
            if read_optional_mutation_authority(&queue.rounds[inactive], name, None)?.is_some() {
                return Err(ProjectionStoreError::ForensicBindingMismatch);
            }
            move_pending_cleanup_marker_noreplace(
                &queue.rounds[active],
                &queue.rounds[inactive],
                name,
            )?;
            rotated = true;
            pending.push((intent, record));
        }
        if removed_temporary || rotated {
            sync_dir_required(&queue.rounds[active])?;
        }
        if rotated {
            sync_dir_required(&queue.rounds[inactive])?;
        }
        pending.sort_unstable_by_key(|(_, record)| (record.intent_id(), record.attempt_id()));
        Ok(pending)
    }

    pub(crate) fn retire_pending_projection_cleanup(
        &self,
        record: &LocalProjectionEvidenceRecord,
    ) -> Result<(), ProjectionStoreError> {
        if !record.is_cleanup_bound() {
            return Err(ProjectionStoreError::ForensicBindingMismatch);
        }
        let name = pending_cleanup_filename(record);
        let (round, expected) = read_pending_cleanup_marker(
            &self.namespaces.pending_cleanup.capability,
            self.store_id,
            self.namespaces.pending_cleanup.identity,
            &name,
        )?;
        let marker = decode_pending_cleanup_marker(&expected)?;
        if marker.evidence != *record {
            return Err(ProjectionStoreError::ForensicBindingMismatch);
        }
        remove_mutation_authority_if_exact(&round, &name, &expected)
    }

    /// Enumerate every durably published intent that has no valid completion.
    ///
    /// Both namespaces are validated as a whole. Only exact immutable-publication
    /// temporary names are ignored; malformed names and non-regular entries fail
    /// closed instead of disappearing from recovery.
    pub fn incomplete_intents(&self) -> Result<Vec<ProjectionIntent>, ProjectionStoreError> {
        Ok(self
            .validated_catalog()?
            .into_iter()
            .filter_map(|entry| entry.completion.is_none().then_some(entry.intent))
            .collect())
    }

    /// Validate and load the complete durable intent/completion catalog.
    ///
    /// This is deliberately crate-private: import authority is minted only by
    /// the projection bridge after it also proves enrolled endpoint, accepted
    /// engine frontier, and immutable object readiness.
    pub(crate) fn validated_catalog(
        &self,
    ) -> Result<Vec<ProjectionCatalogEntry>, ProjectionStoreError> {
        let (retired_intent_names, retired_completion_names, _) = self.retired_own_endpoint_names();
        let intents_dir = self.namespace(INTENTS_DIR)?;
        let mut intents = BTreeMap::new();
        let mut validated_bases = std::collections::BTreeSet::new();
        let mut catalog_bytes = 0_u64;
        let mut directory_entries = 0_usize;
        for entry in intents_dir.entries()? {
            #[cfg(test)]
            count_catalog_directory_entry();
            charge_catalog_directory_entry(
                &mut directory_entries,
                MAX_PROJECTION_CATALOG_DIRECTORY_ENTRIES,
            )?;
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry("non-UTF-8 projection intent entry".into())
            })?;
            if retired_intent_names.contains(name) {
                continue;
            }
            require_regular_entry(&entry.file_type()?, name)?;
            if is_temp_name(name) {
                continue;
            }
            if intents.len() == MAX_PROJECTION_CATALOG_ROWS {
                return Err(ProjectionStoreError::EvidenceTooLarge {
                    kind: "projection catalog rows",
                    declared: intents.len().saturating_add(1) as u64,
                    limit: MAX_PROJECTION_CATALOG_ROWS as u64,
                });
            }
            require_canonical_evidence_name(name, ".intent")?;
            let bytes =
                read_optional_regular(&intents_dir, name, MAX_PROJECTION_EVIDENCE_BYTES, None)?
                    .ok_or_else(|| {
                        ProjectionStoreError::UnsafeEntry(format!(
                            "projection intent disappeared during enumeration: {name}"
                        ))
                    })?;
            let intent = ProjectionIntent::decode(&bytes)?;
            self.require_workspace(&intent)?;
            if intent.encode()? != bytes || intent_filename(intent.id()?) != name {
                return Err(ProjectionStoreError::PathBindingMismatch(
                    "projection intent",
                ));
            }
            catalog_bytes = catalog_bytes.checked_add(bytes.len() as u64).ok_or(
                ProjectionStoreError::EvidenceTooLarge {
                    kind: "projection catalog",
                    declared: u64::MAX,
                    limit: MAX_PROJECTION_CATALOG_BYTES,
                },
            )?;
            if let ProjectionPrecondition::Base(description) = intent.precondition() {
                if validated_bases.insert(*description) {
                    catalog_bytes = catalog_bytes.checked_add(description.byte_length()).ok_or(
                        ProjectionStoreError::EvidenceTooLarge {
                            kind: "projection catalog",
                            declared: u64::MAX,
                            limit: MAX_PROJECTION_CATALOG_BYTES,
                        },
                    )?;
                    self.load_base(&intent)?;
                }
            }
            if catalog_bytes > MAX_PROJECTION_CATALOG_BYTES {
                return Err(ProjectionStoreError::EvidenceTooLarge {
                    kind: "projection catalog",
                    declared: catalog_bytes,
                    limit: MAX_PROJECTION_CATALOG_BYTES,
                });
            }
            intents.insert(completion_filename(intent.id()?), intent);
        }

        let completions_dir = self.namespace(COMPLETIONS_DIR)?;
        let mut completed = BTreeMap::new();
        for entry in completions_dir.entries()? {
            #[cfg(test)]
            count_catalog_directory_entry();
            charge_catalog_directory_entry(
                &mut directory_entries,
                MAX_PROJECTION_CATALOG_DIRECTORY_ENTRIES,
            )?;
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry("non-UTF-8 projection completion entry".into())
            })?;
            if retired_completion_names.contains(name) {
                continue;
            }
            require_regular_entry(&entry.file_type()?, name)?;
            if is_temp_name(name) {
                continue;
            }
            if completed.len() == MAX_PROJECTION_CATALOG_ROWS {
                return Err(ProjectionStoreError::EvidenceTooLarge {
                    kind: "projection completion rows",
                    declared: completed.len().saturating_add(1) as u64,
                    limit: MAX_PROJECTION_CATALOG_ROWS as u64,
                });
            }
            require_canonical_evidence_name(name, ".completion")?;
            let intent = intents
                .get(name)
                .ok_or_else(|| ProjectionStoreError::OrphanCompletion(name.into()))?;
            let bytes =
                read_optional_regular(&completions_dir, name, MAX_PROJECTION_EVIDENCE_BYTES, None)?
                    .ok_or_else(|| {
                        ProjectionStoreError::UnsafeEntry(format!(
                            "projection completion disappeared during enumeration: {name}"
                        ))
                    })?;
            let completion = ProjectionCompletion::decode_bound(&bytes, intent)?;
            if completion.encode()? != bytes {
                return Err(ProjectionStoreError::NonCanonical("projection completion"));
            }
            self.reconcile_completed_mutation(intent, intent.id()?)?;
            catalog_bytes = catalog_bytes.checked_add(bytes.len() as u64).ok_or(
                ProjectionStoreError::EvidenceTooLarge {
                    kind: "projection catalog",
                    declared: u64::MAX,
                    limit: MAX_PROJECTION_CATALOG_BYTES,
                },
            )?;
            if catalog_bytes > MAX_PROJECTION_CATALOG_BYTES {
                return Err(ProjectionStoreError::EvidenceTooLarge {
                    kind: "projection catalog",
                    declared: catalog_bytes,
                    limit: MAX_PROJECTION_CATALOG_BYTES,
                });
            }
            completed.insert(name.to_owned(), completion);
        }

        Ok(intents
            .into_iter()
            .map(|(completion_name, intent)| ProjectionCatalogEntry {
                completion: completed.remove(&completion_name),
                intent,
            })
            .collect())
    }

    /// Names-only completion snapshot for the disposable absence summary.
    /// Retired own-endpoint residue is excluded exactly as it is from the full
    /// validated catalog. No receipt content is opened on this path.
    pub(crate) fn absence_summary_evidence_names(
        &self,
    ) -> Result<BTreeSet<String>, ProjectionStoreError> {
        let (retired_intent_names, retired_completion_names, _) = self.retired_own_endpoint_names();
        let mut names = BTreeSet::new();
        for (namespace, suffix, kind, retired) in [
            (
                COMPLETIONS_DIR,
                ".completion",
                "projection completion rows",
                &retired_completion_names,
            ),
            (
                INTENTS_DIR,
                ".intent",
                "projection intent rows",
                &retired_intent_names,
            ),
        ] {
            let directory = self.namespace(namespace)?;
            let mut directory_entries = 0_usize;
            let mut namespace_rows = 0_usize;
            for entry in directory.entries()? {
                charge_catalog_directory_entry(
                    &mut directory_entries,
                    MAX_PROJECTION_CATALOG_DIRECTORY_ENTRIES,
                )?;
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    ProjectionStoreError::UnsafeEntry("non-UTF-8 projection evidence entry".into())
                })?;
                if retired.contains(name) {
                    continue;
                }
                require_regular_entry(&entry.file_type()?, name)?;
                if is_temp_name(name) {
                    continue;
                }
                if namespace_rows == MAX_PROJECTION_CATALOG_ROWS {
                    return Err(ProjectionStoreError::EvidenceTooLarge {
                        kind,
                        declared: namespace_rows.saturating_add(1) as u64,
                        limit: MAX_PROJECTION_CATALOG_ROWS as u64,
                    });
                }
                require_canonical_evidence_name(name, suffix)?;
                namespace_rows += 1;
                if !names.insert(name.to_owned()) {
                    return Err(ProjectionStoreError::MalformedEvidenceName(name.into()));
                }
            }
        }
        Ok(names)
    }

    /// Read exactly the newly published receiver intents (no completion yet)
    /// not represented by a current summary.
    pub(crate) fn absence_summary_intent_delta(
        &self,
        newly_intended_names: &BTreeSet<String>,
    ) -> Result<Vec<ProjectionIntent>, ProjectionStoreError> {
        let intents_dir = self.namespace(INTENTS_DIR)?;
        let mut intents = Vec::new();
        for intent_name in newly_intended_names {
            require_canonical_evidence_name(intent_name, ".intent")?;
            let bytes = read_optional_regular(
                &intents_dir,
                intent_name,
                MAX_PROJECTION_EVIDENCE_BYTES,
                None,
            )?
            .ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry(format!(
                    "projection intent disappeared after names snapshot: {intent_name}"
                ))
            })?;
            let intent = ProjectionIntent::decode(&bytes)?;
            self.require_workspace(&intent)?;
            if intent.encode()? != bytes || intent_filename(intent.id()?) != *intent_name {
                return Err(ProjectionStoreError::PathBindingMismatch(
                    "projection intent",
                ));
            }
            intents.push(intent);
        }
        Ok(intents)
    }

    /// Read exactly the newly completed receiver rows not represented by a
    /// current summary. The matching intent filename is derived directly from
    /// each completion name; no lifetime intent-directory walk occurs.
    pub(crate) fn absence_summary_catalog_delta(
        &self,
        newly_completed_names: &BTreeSet<String>,
    ) -> Result<Vec<ProjectionCatalogEntry>, ProjectionStoreError> {
        let intents_dir = self.namespace(INTENTS_DIR)?;
        let mut rows = Vec::new();
        for completion_name in newly_completed_names {
            require_canonical_evidence_name(completion_name, ".completion")?;
            let intent_name = format!(
                "{}.intent",
                completion_name
                    .strip_suffix(".completion")
                    .expect("suffix was checked")
            );
            let bytes = read_optional_regular(
                &intents_dir,
                &intent_name,
                MAX_PROJECTION_EVIDENCE_BYTES,
                None,
            )?
            .ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry(format!(
                    "projection completion has no matching intent: {completion_name}"
                ))
            })?;
            let intent = ProjectionIntent::decode(&bytes)?;
            self.require_workspace(&intent)?;
            if intent.encode()? != bytes
                || intent_filename(intent.id()?) != intent_name
                || completion_filename(intent.id()?) != *completion_name
            {
                return Err(ProjectionStoreError::PathBindingMismatch(
                    "projection intent",
                ));
            }
            let completion = self.load_completion(&intent)?.ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry(format!(
                    "projection completion disappeared after names snapshot: {completion_name}"
                ))
            })?;
            rows.push(ProjectionCatalogEntry {
                intent,
                completion: Some(completion),
            });
        }
        Ok(rows)
    }

    /// Reconstruct completion only from an authorized replay and Graph's fresh
    /// capability-bound durable-target proof.
    pub(crate) fn reconstruct_completion(
        &self,
        authority: ProjectionMutationAuthority,
        intent: &ProjectionIntent,
        replayed_target: &[u8],
        proof: &ProjectionWriteProof,
    ) -> Result<ProjectionCompletion, ProjectionStoreError> {
        if BlobDescription::of(replayed_target) != intent.target() {
            return Err(ProjectionStoreError::RecoveryTargetMismatch);
        }
        self.require_write_proof(intent, proof)?;
        self.publish_completion(authority, intent, proof)
    }

    fn initialize(
        capability: &Dir,
        store_id: ProjectionReceiptStoreId,
        workspace_id: WorkspaceId,
        endpoint: Option<ProjectionEndpointBinding>,
    ) -> Result<ReceiptNamespaces, ProjectionStoreError> {
        let existing = read_optional_regular(capability, STORE_CLAIM_FILE, 512, None)
            .map_err(ProjectionStoreError::from)
            .map_err(|error| error.at("read private receipt store claim"))?;
        if let Some(bytes) = existing {
            let expected = validate_claim(&bytes, store_id, workspace_id, endpoint)?;
            let namespaces = open_receipt_namespaces(
                capability,
                store_id,
                ReceiptDirectoryDurability::PromotedAuthority,
            )?;
            if namespaces.identities() != expected {
                return Err(ProjectionStoreError::NamespaceSubstitution(
                    "top-level receipt namespace".into(),
                ));
            }
            return Ok(namespaces);
        }

        let expected_init = init_claim_bytes(store_id, workspace_id, endpoint);
        match read_optional_regular(capability, STORE_INIT_FILE, 256, None)
            .map_err(ProjectionStoreError::from)
            .map_err(|error| error.at("read private receipt initialization claim"))?
        {
            Some(bytes) => {
                if bytes != expected_init {
                    return Err(ProjectionStoreError::MalformedStoreClaim);
                }
            }
            None => {
                if capability.entries()?.next().transpose()?.is_some() {
                    return Err(ProjectionStoreError::ClaimlessNonemptyStore);
                }
                publish_bootstrap_immutable_exact(
                    capability,
                    STORE_INIT_FILE,
                    &expected_init,
                    "projection receipt store initialization claim",
                )
                .map_err(ProjectionStoreError::from)
                .map_err(|error| error.at("publish private receipt initialization claim"))?;
            }
        }

        for namespace in [
            BASES_DIR,
            INTENTS_DIR,
            COMPLETIONS_DIR,
            ATTEMPTS_DIR,
            FORENSICS_DIR,
        ] {
            ensure_bootstrap_directory_nofollow(capability, namespace).map_err(|error| {
                error.at(format!("create private receipt namespace {namespace}"))
            })?;
        }
        require_incomplete_store_is_empty(capability)?;
        let namespaces = open_receipt_namespaces(
            capability,
            store_id,
            ReceiptDirectoryDurability::PrePromotionBootstrap,
        )
        .map_err(|error| error.at("open private receipt namespaces"))?;
        let claim = claim_bytes(store_id, workspace_id, endpoint, &namespaces.identities());
        publish_bootstrap_immutable_exact(
            capability,
            STORE_CLAIM_FILE,
            &claim,
            "projection receipt store claim",
        )
        .map_err(ProjectionStoreError::from)
        .map_err(|error| error.at("publish private receipt store claim"))?;
        Ok(namespaces)
    }

    fn namespace(&self, name: &str) -> Result<Dir, ProjectionStoreError> {
        let retained = self.namespaces.get(name).ok_or_else(|| {
            ProjectionStoreError::UnsafeEntry(format!("unknown receipt namespace {name}"))
        })?;
        let live = open_dir_nofollow(&self.capability, name).map_err(|error| {
            ProjectionStoreError::NamespaceSubstitution(format!("{name}: {error}"))
        })?;
        if canonical_directory_identity(&live)? != retained.identity {
            return Err(ProjectionStoreError::NamespaceSubstitution(name.into()));
        }
        retained.capability.try_clone().map_err(Into::into)
    }

    /// Open this intent's private recovery namespace, creating it if it is
    /// absent.
    ///
    /// **Refusal census 2026-08-26 (P-census).** This used to bind the
    /// directory with two immutable artifacts per namespace — a reservation
    /// published before `mkdir` and an authority published after it, recording
    /// the directory's device/inode identity — and to refuse
    /// `NamespaceSubstitution` whenever either was absent, non-canonical, or
    /// disagreed with the live directory. Four artifacts, eight durability
    /// barriers, per projected page.
    ///
    /// The only failure those refusals detected is an actor who can rename or
    /// replace a directory *inside Tine's app-private receipt store*. That
    /// actor already has write access as the user and could replace the Tine
    /// binary, which
    /// `specs/notes/2026-08-07-trust-model-and-threat-model-decision.md` puts
    /// explicitly out of scope. No in-scope failure — crash or power loss,
    /// torn write, disk error, Syncthing/Dropbox delivery, external-editor
    /// race, honest concurrent instance, honest multi-device divergence,
    /// malformed imported content — is detected by them: the receipt store is
    /// app-private, is never synced, is single-writer under the workspace
    /// runtime lease, and a crash cannot rename a directory. What the refusals
    /// *did* add was a wedge: a torn 1 KB JSON binding, or a namespace whose
    /// binding artifact was lost, refused the page's projection permanently.
    ///
    /// Per the refusal-scenario rule, the check is therefore gone and absence
    /// is a **recovery**: recreate the directory and continue. Recreating it is
    /// safe because everything inside is content- or intent-addressed and is
    /// republished byte-identically by the drain, which still holds the
    /// undrained journal frame for the accepted edit.
    fn intent_namespace(
        &self,
        namespace: &str,
        intent_id: ProjectionIntentId,
    ) -> Result<Dir, ProjectionStoreError> {
        self.open_intent_namespace(namespace, intent_id, true)?
            .ok_or_else(|| {
                ProjectionStoreError::NamespaceSubstitution(format!(
                    "{namespace}/{}",
                    hex(intent_id.as_bytes())
                ))
            })
    }

    fn existing_intent_namespace(
        &self,
        namespace: &str,
        intent_id: ProjectionIntentId,
    ) -> Result<Option<Dir>, ProjectionStoreError> {
        self.open_intent_namespace(namespace, intent_id, false)
    }

    /// The recovery form of [`Self::intent_namespace`]: a namespace that an
    /// earlier published intent implies must exist is recreated when it is
    /// missing instead of refusing the projection forever.
    fn required_intent_namespace(
        &self,
        namespace: &str,
        intent_id: ProjectionIntentId,
    ) -> Result<Dir, ProjectionStoreError> {
        self.intent_namespace(namespace, intent_id)
    }

    fn open_intent_namespace(
        &self,
        namespace: &str,
        intent_id: ProjectionIntentId,
        create: bool,
    ) -> Result<Option<Dir>, ProjectionStoreError> {
        let parent = self.namespace(namespace)?;
        let name = hex(intent_id.as_bytes());
        match parent.symlink_metadata(&name) {
            // A non-directory (or a symlink) at a per-intent namespace name is
            // still refused: `open_dir_nofollow` would refuse it anyway, and
            // this names the artifact instead of returning a bare ENOTDIR.
            Ok(metadata) if !metadata.is_dir() => {
                return Err(ProjectionStoreError::UnsafeEntry(format!(
                    "private receipt namespace {namespace}/{name} is not a directory"
                )));
            }
            Ok(_) => {
                // On Android a prior strict mkdir barrier can refuse after the
                // directory entry became visible, then the process can die
                // before recording that refusal. The first create/recovery use
                // of this parent in every process therefore establishes one
                // strict barrier before accepting an existing name. Read-only
                // inspection does not mutate and pays no barrier. See
                // storage-sync-contract.md §2.10c.
                #[cfg(target_os = "android")]
                if create {
                    verify_promoted_receipt_parent(&parent)?;
                }
                return Ok(Some(open_dir_nofollow(&parent, &name)?));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if !create {
            return Ok(None);
        }
        ensure_directory_nofollow(&parent, &name)?;
        Ok(Some(open_dir_nofollow(&parent, &name)?))
    }

    fn validate_forensic_record(
        &self,
        intent: &ProjectionIntent,
        record: &LocalProjectionEvidenceRecord,
    ) -> Result<(), ProjectionStoreError> {
        if !valid_local_forensic_version(record)
            || record.intent_id != intent.id()?
            || record.target_path != *intent.path()
        {
            return Err(ProjectionStoreError::ForensicBindingMismatch);
        }
        require_evidence_length(
            "local projection forensic evidence",
            record.observed.byte_length(),
            MAX_PROJECTION_EVIDENCE_BYTES,
        )?;
        let reservation = self
            .load_attempt_reservations(intent)?
            .into_iter()
            .find(|reservation| reservation.attempt_id() == record.attempt_id)
            .ok_or(ProjectionStoreError::ForensicBindingMismatch)?;
        self.validate_forensic_record_with_reservation(intent, record, &reservation)
    }

    fn validate_forensic_record_with_reservation(
        &self,
        intent: &ProjectionIntent,
        record: &LocalProjectionEvidenceRecord,
        reservation: &ProjectionAttemptReservation,
    ) -> Result<(), ProjectionStoreError> {
        if !valid_local_forensic_version(record)
            || record.intent_id != intent.id()?
            || record.target_path != *intent.path()
            || reservation.attempt_id() != record.attempt_id
        {
            return Err(ProjectionStoreError::ForensicBindingMismatch);
        }
        require_evidence_length(
            "local projection forensic evidence",
            record.observed.byte_length(),
            MAX_PROJECTION_EVIDENCE_BYTES,
        )?;
        let expected_relative_path = intent
            .path()
            .join_sibling(&record.recovery_filename)
            .map_err(|_| ProjectionStoreError::ForensicBindingMismatch)?;
        if reservation.recovery_filename() != record.recovery_filename
            || record.recovery_relative_path != expected_relative_path
        {
            return Err(ProjectionStoreError::ForensicBindingMismatch);
        }
        Ok(())
    }

    fn require_workspace(&self, intent: &ProjectionIntent) -> Result<(), ProjectionStoreError> {
        if intent.workspace_id() != self.workspace_id {
            return Err(ProjectionStoreError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: intent.workspace_id(),
            });
        }
        Ok(())
    }

    pub(crate) fn require_endpoint(
        &self,
        endpoint: ProjectionEndpointBinding,
    ) -> Result<(), ProjectionStoreError> {
        if self.endpoint != Some(endpoint) {
            return Err(ProjectionStoreError::EndpointBindingMismatch);
        }
        Ok(())
    }

    /// Best-effort names-only reporting for pre-2c own-endpoint receipt
    /// residue. The supplied ids come exclusively from the own turn/journal
    /// and local-completion authorities. This method never decodes a receipt,
    /// never treats one as recovery evidence, and never deletes or rewrites an
    /// artifact; receiver rows outside the supplied set remain untouched.
    pub(crate) fn retired_own_endpoint_artifacts(
        &self,
        own_intent_ids: &BTreeSet<ProjectionIntentId>,
    ) -> Vec<String> {
        self.retired_own_endpoint_intents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(own_intent_ids.iter().copied());
        let mut reported = BTreeSet::new();
        let own_prefixes = own_intent_ids
            .iter()
            .map(|intent_id| format!("{}.", hex(intent_id.as_bytes())))
            .collect::<Vec<_>>();
        for intent_id in own_intent_ids {
            let intent_name = intent_filename(*intent_id);
            if self
                .namespaces
                .intents
                .capability
                .symlink_metadata(&intent_name)
                .is_ok()
            {
                reported.insert(format!("{INTENTS_DIR}/{intent_name}"));
            }
            let completion_name = completion_filename(*intent_id);
            if self
                .namespaces
                .completions
                .capability
                .symlink_metadata(&completion_name)
                .is_ok()
            {
                reported.insert(format!("{COMPLETIONS_DIR}/{completion_name}"));
            }
            let intent_directory = hex(intent_id.as_bytes());
            for (namespace, directory) in [
                (ATTEMPTS_DIR, &self.namespaces.attempts.capability),
                (FORENSICS_DIR, &self.namespaces.forensics.capability),
            ] {
                if directory.symlink_metadata(&intent_directory).is_ok() {
                    reported.insert(format!("{namespace}/{intent_directory}/"));
                }
            }
            for name in [
                mutation_authority_filename(*intent_id),
                mutation_authority_lease_filename(*intent_id),
            ] {
                if self.capability.symlink_metadata(&name).is_ok() {
                    reported.insert(name);
                }
            }
        }
        // Pending-cleanup marker names begin with the exact intent id. Report
        // matching names without decoding the marker or opening any evidence
        // path: residue is diagnostic, never own-endpoint recovery authority.
        for round_name in PENDING_CLEANUP_ROUND_DIRS {
            let Ok(round) =
                open_dir_nofollow(&self.namespaces.pending_cleanup.capability, round_name)
            else {
                continue;
            };
            let Ok(entries) = round.entries() else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if own_prefixes.iter().any(|prefix| name.starts_with(prefix)) {
                    reported.insert(format!(
                        "{FORENSICS_DIR}/{PENDING_CLEANUP_DIR}/{round_name}/{name}"
                    ));
                }
            }
        }
        reported.into_iter().collect()
    }

    fn retired_own_endpoint_names(&self) -> (BTreeSet<String>, BTreeSet<String>, Vec<String>) {
        let intent_ids = self
            .retired_own_endpoint_intents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let intents = intent_ids
            .iter()
            .map(|intent_id| intent_filename(*intent_id))
            .collect();
        let completions = intent_ids
            .iter()
            .map(|intent_id| completion_filename(*intent_id))
            .collect();
        let pending_prefixes = intent_ids
            .iter()
            .map(|intent_id| format!("{}.", hex(intent_id.as_bytes())))
            .collect();
        (intents, completions, pending_prefixes)
    }

    fn require_write_proof(
        &self,
        intent: &ProjectionIntent,
        proof: &ProjectionWriteProof,
    ) -> Result<(), ProjectionStoreError> {
        if proof.path() != intent.path().as_str()
            || proof.digest() != intent.target().sha256()
            || BlobDescription::of(proof.bytes()) != intent.target()
        {
            return Err(ProjectionStoreError::WriteProofMismatch);
        }
        Ok(())
    }

    fn reconcile_completed_mutation(
        &self,
        intent: &ProjectionIntent,
        intent_id: ProjectionIntentId,
    ) -> Result<(), ProjectionStoreError> {
        let _lease = self.acquire_mutation_lease(intent_id)?;
        mutation_authority_leased_hook();
        let durable_name = mutation_authority_filename(intent_id);
        let Some(durable_bytes) =
            read_optional_mutation_authority(&self.capability, &durable_name, None)?
        else {
            return Ok(());
        };

        let store_claim = read_optional_regular(
            &self.capability,
            STORE_CLAIM_FILE,
            STORE_CLAIM_LEN as u64,
            Some(STORE_CLAIM_LEN as u64),
        )?
        .ok_or(ProjectionStoreError::MalformedStoreClaim)?;
        let bases = self.namespace(BASES_DIR)?;
        let attempts = self.required_intent_namespace(ATTEMPTS_DIR, intent_id)?;
        let forensics = self.required_intent_namespace(FORENSICS_DIR, intent_id)?;
        let reservations = self.load_attempt_reservations_from(intent, &attempts)?;
        let intent_bytes = intent.encode()?;
        let base = match intent.precondition() {
            ProjectionPrecondition::Absent => None,
            ProjectionPrecondition::Base(description) => {
                let bytes = read_optional_regular(
                    &bases,
                    &base_filename(*description),
                    MAX_PROJECTION_EVIDENCE_BYTES,
                    Some(description.byte_length()),
                )?
                .ok_or(ProjectionStoreError::MissingBase(*description))?;
                if BlobDescription::of(&bytes) != *description {
                    return Err(ProjectionStoreError::BaseEvidenceMismatch(*description));
                }
                Some(*description)
            }
        };
        decode_durable_mutation_authority(
            &durable_bytes,
            intent,
            self.store_id,
            Sha256::digest(&store_claim).into(),
            self.workspace_id,
            self.endpoint.map(endpoint_binding_bytes).as_deref(),
            intent_id,
            Sha256::digest(&intent_bytes).into(),
            base,
            self.namespaces.identities(),
            canonical_directory_identity(&attempts)?,
            canonical_directory_identity(&forensics)?,
            &reservations,
        )?;
        remove_mutation_authority_if_exact(&self.capability, &durable_name, &durable_bytes)
    }

    fn acquire_mutation_lease(
        &self,
        intent_id: ProjectionIntentId,
    ) -> Result<File, ProjectionStoreError> {
        let name = mutation_authority_lease_filename(intent_id);
        let file = open_mutation_authority_lease_file(&self.capability, &name)?;
        if let Err(error) = file.try_lock_exclusive() {
            if error.kind() == ErrorKind::PermissionDenied
                || tine_storage::nonblocking_lock_is_contended(&error)
            {
                return Err(ProjectionStoreError::MutationAuthorityPending);
            }
            return Err(error.into());
        }
        validate_mutation_authority_lease_file(&file, &name)?;
        file.set_len(0)?;
        Ok(file)
    }

    fn require_published_intent(
        &self,
        intent: &ProjectionIntent,
    ) -> Result<ProjectionIntentId, ProjectionStoreError> {
        self.require_workspace(intent)?;
        let intent_id = intent.id()?;
        let stored = self
            .load_intent(intent_id)?
            .ok_or(ProjectionStoreError::MissingIntent(intent_id))?;
        if stored != *intent {
            return Err(ProjectionStoreError::IntentCollision(intent_id));
        }
        Ok(intent_id)
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_durable_mutation_authority(
    durable_bytes: &[u8],
    intent: &ProjectionIntent,
    store_id: ProjectionReceiptStoreId,
    store_claim_digest: [u8; 32],
    workspace_id: WorkspaceId,
    endpoint_binding: Option<&[u8]>,
    intent_id: ProjectionIntentId,
    intent_digest: [u8; 32],
    base: Option<BlobDescription>,
    namespace_identities: [DirectoryIdentity; 5],
    attempts_identity: DirectoryIdentity,
    forensics_identity: DirectoryIdentity,
    live_reservations: &[ProjectionAttemptReservation],
) -> Result<DurableProjectionMutationAuthority, ProjectionStoreError> {
    let durable: DurableProjectionMutationAuthority = serde_json::from_slice(durable_bytes)
        .map_err(|error| ProjectionStoreError::Decode(error.to_string()))?;
    if serde_json::to_vec(&durable)
        .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?
        != durable_bytes
        || durable.schema_version != MUTATION_AUTHORITY_SCHEMA_VERSION
        || durable.store_id != store_id
        || durable.store_claim_digest != store_claim_digest
        || durable.workspace_id != workspace_id
        || durable.endpoint_binding.as_deref() != endpoint_binding
        || durable.intent_id != intent_id
        || durable.intent_digest != intent_digest
        || durable.base != base
        || durable.namespace_identities != namespace_identities
        || durable.attempts_identity != attempts_identity
        || durable.forensics_identity != forensics_identity
    {
        return Err(ProjectionStoreError::MutationAuthorityMismatch);
    }
    let durable_reservations = decode_mutation_reservations(&durable, intent)?;
    if durable_reservations != live_reservations {
        return Err(ProjectionStoreError::AttemptBindingMismatch);
    }
    Ok(durable)
}

fn decode_mutation_reservations(
    durable: &DurableProjectionMutationAuthority,
    intent: &ProjectionIntent,
) -> Result<Vec<ProjectionAttemptReservation>, ProjectionStoreError> {
    if durable.reservation_bytes.len() > MAX_MUTATION_ATTEMPTS {
        return Err(ProjectionStoreError::MutationAuthorityTooLarge {
            attempts: durable.reservation_bytes.len(),
            bytes: 0,
        });
    }
    let mut reservations = Vec::with_capacity(durable.reservation_bytes.len());
    for bytes in &durable.reservation_bytes {
        let reservation: ProjectionAttemptReservation = serde_json::from_slice(bytes)
            .map_err(|error| ProjectionStoreError::Decode(error.to_string()))?;
        reservation.validate(intent)?;
        if serde_json::to_vec(&reservation)
            .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?
            != *bytes
        {
            return Err(ProjectionStoreError::AttemptBindingMismatch);
        }
        if reservations
            .last()
            .is_some_and(|prior: &ProjectionAttemptReservation| {
                prior.attempt_id() >= reservation.attempt_id()
            })
        {
            return Err(ProjectionStoreError::AttemptBindingMismatch);
        }
        reservations.push(reservation);
    }
    if durable.active_attempt_id.is_some_and(|active_attempt_id| {
        !reservations
            .iter()
            .any(|reservation| reservation.attempt_id() == active_attempt_id)
    }) {
        return Err(ProjectionStoreError::AttemptBindingMismatch);
    }
    Ok(reservations)
}

impl ProjectionMutationAuthority {
    pub(crate) fn consume_write_evidence<T>(
        &mut self,
        relative_path: &str,
        operation: impl FnOnce(
            &ProjectionAttemptReservation,
            &[ProjectionAttemptReservation],
            &ProjectionRecoveryEvidencePublisher<'_>,
        ) -> io::Result<T>,
    ) -> io::Result<T> {
        mutation_authority_act_hook();
        self.consume_graph_operation(relative_path)?;
        let active = self.active.as_ref().ok_or_else(|| {
            io::Error::new(
                ErrorKind::PermissionDenied,
                "projection mutation authority has no active reserved attempt",
            )
        })?;
        let publisher = ProjectionRecoveryEvidencePublisher {
            pending_cleanup: &self.pending_cleanup,
            forensics: &self.forensics,
            store_id: self.durable.store_id,
            intent_id: self.durable.intent_id,
            target_path: active.target_path(),
            reservation: active,
        };
        operation(active, &self.reservations, &publisher)
    }

    pub(crate) fn consume_recovery_evidence<T>(
        &mut self,
        relative_path: &str,
        operation: impl FnOnce(&[ProjectionAttemptReservation]) -> io::Result<T>,
    ) -> io::Result<T> {
        mutation_authority_act_hook();
        self.consume_graph_operation(relative_path)?;
        if self.reservations.is_empty() {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "projection recovery authority has no durable attempts",
            ));
        }
        operation(&self.reservations)
    }

    /// Release a slot after Graph's read-only recovery probe rejected the
    /// current target. Callers may then admit the one deterministic fallback
    /// attempt without ever overwriting an occupied recovery filename.
    pub(crate) fn release_failed_recovery(self) -> Result<(), ProjectionStoreError> {
        self.require_consumed()?;
        self.remove_durable_record_if_exact()
    }

    fn consume_completion_publication<T>(
        &mut self,
        store: &ProjectionReceiptStore,
        intent: &ProjectionIntent,
        operation: impl FnOnce(&Self) -> Result<T, ProjectionStoreError>,
    ) -> Result<T, ProjectionStoreError> {
        self.require_store_and_intent(store, intent)?;
        self.require_consumed()?;
        self.validate_live_names()?;
        let result = operation(self)?;
        self.validate_live_names()?;
        self.completion_published = true;
        self.retire_durable_record()?;
        Ok(result)
    }

    fn retire_durable_record(&self) -> Result<(), ProjectionStoreError> {
        self.remove_durable_record_if_exact()
    }

    fn remove_durable_record_if_exact(&self) -> Result<(), ProjectionStoreError> {
        remove_mutation_authority_if_exact(&self.root, &self.durable_name, &self.durable_bytes)
    }

    fn consume_graph_operation(&mut self, relative_path: &str) -> io::Result<()> {
        if self.graph_operation_consumed {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "projection mutation authority was already consumed",
            ));
        }
        self.validate_live_names().map_err(|error| {
            io::Error::new(
                ErrorKind::PermissionDenied,
                format!("projection mutation authority is no longer live: {error}"),
            )
        })?;
        if self
            .reservations
            .iter()
            .any(|reservation| reservation.target_path().as_str() != relative_path)
            || self
                .active
                .as_ref()
                .is_some_and(|reservation| reservation.target_path().as_str() != relative_path)
        {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "projection mutation authority target path mismatch",
            ));
        }
        self.graph_operation_consumed = true;
        Ok(())
    }

    fn require_store_and_intent(
        &self,
        store: &ProjectionReceiptStore,
        intent: &ProjectionIntent,
    ) -> Result<(), ProjectionStoreError> {
        let intent_bytes = intent.encode()?;
        if self.durable.store_id != store.store_id
            || self.durable.workspace_id != store.workspace_id
            || self.durable.endpoint_binding != store.endpoint.map(endpoint_binding_bytes)
            || self.durable.intent_id != intent.id()?
            || self.durable.intent_digest != <[u8; 32]>::from(Sha256::digest(&intent_bytes))
            || self.durable.namespace_identities != store.namespaces.identities()
        {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        Ok(())
    }

    fn require_consumed(&self) -> Result<(), ProjectionStoreError> {
        if !self.graph_operation_consumed {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        Ok(())
    }

    fn validate_live_names(&self) -> Result<(), ProjectionStoreError> {
        if canonical_receipt_store_id(&self.root)? != self.durable.store_id {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        let store_claim = read_optional_regular(
            &self.root,
            STORE_CLAIM_FILE,
            STORE_CLAIM_LEN as u64,
            Some(STORE_CLAIM_LEN as u64),
        )?
        .ok_or(ProjectionStoreError::MalformedStoreClaim)?;
        if <[u8; 32]>::from(Sha256::digest(&store_claim)) != self.durable.store_claim_digest {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        let stored = read_optional_mutation_authority(
            &self.root,
            &self.durable_name,
            Some(self.durable_bytes.len() as u64),
        )?
        .ok_or_else(|| {
            ProjectionStoreError::NamespaceSubstitution(
                "projection mutation authority disappeared".into(),
            )
        })?;
        if stored != self.durable_bytes {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        for (index, name) in [
            BASES_DIR,
            INTENTS_DIR,
            COMPLETIONS_DIR,
            ATTEMPTS_DIR,
            FORENSICS_DIR,
        ]
        .into_iter()
        .enumerate()
        {
            let live = open_dir_nofollow(&self.root, name).map_err(|error| {
                ProjectionStoreError::NamespaceSubstitution(format!("{name}: {error}"))
            })?;
            if canonical_directory_identity(&live)? != self.durable.namespace_identities[index] {
                return Err(ProjectionStoreError::NamespaceSubstitution(name.into()));
            }
        }
        if canonical_directory_identity(&self.bases)? != self.durable.namespace_identities[0]
            || canonical_directory_identity(&self.completions)?
                != self.durable.namespace_identities[2]
            || canonical_directory_identity(&self.intents)? != self.durable.namespace_identities[1]
            || canonical_directory_identity(&self.attempts_parent)?
                != self.durable.namespace_identities[3]
            || canonical_directory_identity(&self.forensics_parent)?
                != self.durable.namespace_identities[4]
            || canonical_directory_identity(&self.attempts)? != self.durable.attempts_identity
            || canonical_directory_identity(&self.forensics)? != self.durable.forensics_identity
        {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        let live_pending = open_dir_nofollow(&self.forensics_parent, PENDING_CLEANUP_DIR)
            .map_err(|_| ProjectionStoreError::MutationAuthorityMismatch)?;
        if canonical_directory_identity(&live_pending)?
            != canonical_directory_identity(&self.pending_cleanup)?
        {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        let intent_bytes = read_optional_regular(
            &self.intents,
            &intent_filename(self.durable.intent_id),
            MAX_PROJECTION_EVIDENCE_BYTES,
            None,
        )?
        .ok_or(ProjectionStoreError::MissingIntent(self.durable.intent_id))?;
        if <[u8; 32]>::from(Sha256::digest(&intent_bytes)) != self.durable.intent_digest {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        if let Some(description) = self.durable.base {
            let bytes = read_optional_regular(
                &self.bases,
                &base_filename(description),
                MAX_PROJECTION_EVIDENCE_BYTES,
                Some(description.byte_length()),
            )?
            .ok_or(ProjectionStoreError::MissingBase(description))?;
            if BlobDescription::of(&bytes) != description {
                return Err(ProjectionStoreError::BaseEvidenceMismatch(description));
            }
        }
        if self.reservations.len() != self.durable.reservation_bytes.len() {
            return Err(ProjectionStoreError::MutationAuthorityMismatch);
        }
        let expected_attempts = self
            .reservations
            .iter()
            .zip(&self.durable.reservation_bytes)
            .map(|(reservation, bytes)| (attempt_filename(reservation.attempt_id()), bytes))
            .collect::<BTreeMap<_, _>>();
        let mut live_attempts = BTreeMap::new();
        for entry in self.attempts.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                ProjectionStoreError::UnsafeEntry("non-UTF-8 projection attempt entry".into())
            })?;
            require_regular_entry(&entry.file_type()?, name)?;
            if is_temp_name(name) {
                continue;
            }
            parse_attempt_filename(name)?;
            let expected_bytes = expected_attempts
                .get(name)
                .ok_or(ProjectionStoreError::AttemptBindingMismatch)?;
            let bytes = read_optional_regular(
                &self.attempts,
                name,
                MAX_PROJECTION_EVIDENCE_BYTES,
                Some(expected_bytes.len() as u64),
            )?
            .ok_or(ProjectionStoreError::AttemptBindingMismatch)?;
            if bytes.as_slice() != expected_bytes.as_slice() {
                return Err(ProjectionStoreError::AttemptBindingMismatch);
            }
            live_attempts.insert(name.to_owned(), bytes);
        }
        if live_attempts.len() != expected_attempts.len() {
            return Err(ProjectionStoreError::AttemptBindingMismatch);
        }
        validate_live_intent_namespace(
            &self.attempts_parent,
            ATTEMPTS_DIR,
            self.durable.store_id,
            self.durable.intent_id,
            self.durable.attempts_identity,
        )?;
        validate_live_intent_namespace(
            &self.forensics_parent,
            FORENSICS_DIR,
            self.durable.store_id,
            self.durable.intent_id,
            self.durable.forensics_identity,
        )
    }
}

impl ProjectionMutationEvidence for ProjectionMutationAuthority {
    fn consume_write_evidence<T>(
        &mut self,
        relative_path: &str,
        operation: impl FnOnce(
            &ProjectionAttemptReservation,
            &[ProjectionAttemptReservation],
            Option<&ProjectionRecoveryEvidencePublisher<'_>>,
        ) -> io::Result<T>,
    ) -> io::Result<T> {
        ProjectionMutationAuthority::consume_write_evidence(
            self,
            relative_path,
            |reservation, attempts, publisher| operation(reservation, attempts, Some(publisher)),
        )
    }

    fn consume_recovery_evidence<T>(
        &mut self,
        relative_path: &str,
        operation: impl FnOnce(&[ProjectionAttemptReservation]) -> io::Result<T>,
    ) -> io::Result<T> {
        ProjectionMutationAuthority::consume_recovery_evidence(self, relative_path, operation)
    }
}

impl ProjectionMutationEvidence for ProjectionTurnMutationAuthority {
    fn consume_write_evidence<T>(
        &mut self,
        relative_path: &str,
        operation: impl FnOnce(
            &ProjectionAttemptReservation,
            &[ProjectionAttemptReservation],
            Option<&ProjectionRecoveryEvidencePublisher<'_>>,
        ) -> io::Result<T>,
    ) -> io::Result<T> {
        self.consume_graph_operation(relative_path)?;
        operation(
            &self.reservation,
            std::slice::from_ref(&self.reservation),
            None,
        )
    }

    fn consume_recovery_evidence<T>(
        &mut self,
        relative_path: &str,
        operation: impl FnOnce(&[ProjectionAttemptReservation]) -> io::Result<T>,
    ) -> io::Result<T> {
        self.consume_graph_operation(relative_path)?;
        operation(std::slice::from_ref(&self.reservation))
    }
}

impl Drop for ProjectionMutationAuthority {
    fn drop(&mut self) {
        mutation_authority_drop_hook();
        if self.completion_published
            || (self.created_durable_record && !self.graph_operation_consumed)
        {
            let _ = self.remove_durable_record_if_exact();
        }
    }
}

/// Re-check that a per-intent recovery namespace still is the exact directory
/// this in-flight mutation authority was sealed against.
///
/// **Refusal census 2026-08-26 (P-census).** The artifact half of this check —
/// reading the per-intent `*.namespace-authority` binding — is gone with the
/// artifact; see [`ProjectionReceiptStore::intent_namespace`]. What remains is
/// the live device/inode comparison against the identity the durable authority
/// already recorded, which costs no durability barrier and needs no artifact.
/// It is retained rather than deleted because it is free, and because a
/// mismatch is not reachable from any in-scope failure: a crash cannot rename a
/// directory, and the authority is created and consumed inside one drain turn.
fn validate_live_intent_namespace(
    parent: &Dir,
    namespace: &str,
    _store_id: ProjectionReceiptStoreId,
    intent_id: ProjectionIntentId,
    expected_identity: DirectoryIdentity,
) -> Result<(), ProjectionStoreError> {
    let name = hex(intent_id.as_bytes());
    let live = open_dir_nofollow(parent, &name).map_err(|error| {
        ProjectionStoreError::NamespaceSubstitution(format!("{namespace}/{name}: {error}"))
    })?;
    if canonical_directory_identity(&live)? != expected_identity {
        return Err(ProjectionStoreError::NamespaceSubstitution(format!(
            "{namespace}/{name}"
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub enum ProjectionStoreError {
    Io(std::io::Error),
    Store(Box<StoreError>),
    Operation {
        operation: String,
        source: Box<ProjectionStoreError>,
    },
    Receipt(ReceiptError),
    UnsafeEntry(String),
    UnknownStoreVersion(u32),
    UpgradeRequired {
        found: u32,
        current: u32,
    },
    MalformedStoreClaim,
    ClaimlessNonemptyStore,
    NamespaceSubstitution(String),
    EndpointBindingMismatch,
    GraphResourceMismatch,
    CapturedInputMismatch,
    MissingPriorCompletion,
    WorkspaceMismatch {
        expected: WorkspaceId,
        found: WorkspaceId,
    },
    MissingBase(BlobDescription),
    UnexpectedBase,
    MissingIntent(ProjectionIntentId),
    BaseEvidenceMismatch(BlobDescription),
    PathBindingMismatch(&'static str),
    NonCanonical(&'static str),
    IntentCollision(ProjectionIntentId),
    EvidenceTooLarge {
        kind: &'static str,
        declared: u64,
        limit: u64,
    },
    MalformedEvidenceName(String),
    OrphanCompletion(String),
    WriteProofMismatch,
    RecoveryTargetMismatch,
    AttemptBindingMismatch,
    MissingTurnAttemptContext,
    MutationAuthorityMismatch,
    MutationAuthorityPending,
    MutationAuthorityTooLarge {
        attempts: usize,
        bytes: usize,
    },
    ForensicBindingMismatch,
    UnreservedRecoveryEvidence,
    Decode(String),
    Encode(String),
}

impl fmt::Display for ProjectionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::Operation { operation, source } => write!(f, "{operation}: {source}"),
            Self::Receipt(error) => error.fmt(f),
            Self::UnsafeEntry(message) => write!(f, "unsafe projection store entry: {message}"),
            Self::UnknownStoreVersion(version) => {
                write!(f, "unknown projection store version {version}")
            }
            // The remedy is part of the error, not of a caller's prose: this
            // string is what the managed-open failure channel carries to the
            // user, and the whole point of the (c) blank-slate transition is
            // that the user is told exactly how to proceed.
            Self::UpgradeRequired { found, current } => write!(
                f,
                "projection receipt store version {found} requires upgrade to {current}: \
                 re-activate managed storage for this graph; your Markdown files are intact \
                 and are not modified by this refusal [{}]",
                crate::oplog::refusal::ManagedStorageRefusalScenario::ProtocolIncompatible.as_str()
            ),
            Self::MalformedStoreClaim => f.write_str("malformed projection store claim"),
            Self::ClaimlessNonemptyStore => {
                f.write_str("claimless nonempty projection receipt store cannot be initialized")
            }
            Self::NamespaceSubstitution(namespace) => {
                write!(
                    f,
                    "projection receipt namespace no longer denotes retained resource: {namespace}"
                )
            }
            Self::EndpointBindingMismatch => {
                f.write_str("projection receipt store endpoint enrollment mismatch")
            }
            Self::GraphResourceMismatch => {
                f.write_str("projection graph capability does not match endpoint enrollment")
            }
            Self::CapturedInputMismatch => {
                f.write_str("capability-captured projection input does not match its completion")
            }
            Self::MissingPriorCompletion => {
                f.write_str("present projection input has no durable prior completion")
            }
            Self::WorkspaceMismatch { expected, found } => {
                write!(f, "workspace mismatch: expected {expected}, found {found}")
            }
            Self::MissingBase(description) => {
                write!(f, "missing immutable projection base {description:?}")
            }
            Self::UnexpectedBase => {
                f.write_str("base bytes were supplied for an absent projection precondition")
            }
            Self::MissingIntent(intent_id) => {
                write!(f, "missing immutable projection intent {intent_id}")
            }
            Self::BaseEvidenceMismatch(description) => {
                write!(f, "projection base evidence mismatch for {description:?}")
            }
            Self::PathBindingMismatch(kind) => {
                write!(f, "{kind} bytes do not match their canonical path")
            }
            Self::NonCanonical(kind) => write!(f, "{kind} bytes are not canonical"),
            Self::IntentCollision(intent_id) => {
                write!(f, "stored projection intent differs at {intent_id}")
            }
            Self::EvidenceTooLarge {
                kind,
                declared,
                limit,
            } => {
                write!(
                    f,
                    "{kind} declares {declared} bytes, exceeding reload limit {limit}"
                )
            }
            Self::MalformedEvidenceName(name) => {
                write!(f, "malformed projection evidence name: {name}")
            }
            Self::OrphanCompletion(name) => {
                write!(f, "projection completion has no matching intent: {name}")
            }
            Self::WriteProofMismatch => {
                f.write_str("Graph write proof does not match the exact projection intent")
            }
            Self::RecoveryTargetMismatch => {
                f.write_str("current bytes do not equal the exact replayed projection target")
            }
            Self::AttemptBindingMismatch => {
                f.write_str("local projection attempt is not canonically bound to its intent")
            }
            Self::MissingTurnAttemptContext => {
                f.write_str("managed projection mutation has no turn-derived attempt identity")
            }
            Self::MutationAuthorityMismatch => {
                f.write_str("projection mutation authority does not match the durable operation")
            }
            Self::MutationAuthorityPending => {
                f.write_str("projection mutation authority is pending recovery")
            }
            Self::MutationAuthorityTooLarge { attempts, bytes } => write!(
                f,
                "projection mutation authority exceeds its bound: {attempts} attempts, {bytes} bytes"
            ),
            Self::ForensicBindingMismatch => {
                f.write_str("local projection forensic evidence is not bound to its attempt")
            }
            Self::UnreservedRecoveryEvidence => {
                f.write_str("Graph returned recovery evidence without a durable reservation")
            }
            Self::Decode(error) => write!(f, "local projection evidence decode failed: {error}"),
            Self::Encode(error) => write!(f, "local projection evidence encode failed: {error}"),
        }
    }
}

impl ProjectionStoreError {
    fn at(self, operation: impl Into<String>) -> Self {
        Self::Operation {
            operation: operation.into(),
            source: Box::new(self),
        }
    }
}

impl std::error::Error for ProjectionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Receipt(error) => Some(error),
            Self::Operation { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ProjectionStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<StoreError> for ProjectionStoreError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Io(error) if error.kind() == ErrorKind::InvalidData => Self::Io(error),
            other => Self::Store(Box::new(other)),
        }
    }
}

impl From<ReceiptError> for ProjectionStoreError {
    fn from(error: ReceiptError) -> Self {
        Self::Receipt(error)
    }
}

fn claim_bytes(
    store_id: ProjectionReceiptStoreId,
    workspace_id: WorkspaceId,
    endpoint: Option<ProjectionEndpointBinding>,
    namespace_identities: &[DirectoryIdentity; 5],
) -> Vec<u8> {
    let mut bytes = enrollment_claim_bytes(STORE_CLAIM_MAGIC, store_id, workspace_id, endpoint);
    bytes.reserve(5 * 32);
    for identity in namespace_identities {
        bytes.extend_from_slice(identity);
    }
    debug_assert_eq!(bytes.len(), STORE_CLAIM_LEN);
    bytes
}

fn init_claim_bytes(
    store_id: ProjectionReceiptStoreId,
    workspace_id: WorkspaceId,
    endpoint: Option<ProjectionEndpointBinding>,
) -> Vec<u8> {
    let bytes = enrollment_claim_bytes(STORE_INIT_MAGIC, store_id, workspace_id, endpoint);
    debug_assert_eq!(bytes.len(), STORE_INIT_LEN);
    bytes
}

fn enrollment_claim_bytes(
    magic: &[u8; 8],
    store_id: ProjectionReceiptStoreId,
    workspace_id: WorkspaceId,
    endpoint: Option<ProjectionEndpointBinding>,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(STORE_CLAIM_BASE_LEN);
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(&STORE_CLAIM_VERSION.to_be_bytes());
    bytes.extend_from_slice(store_id.as_bytes());
    bytes.extend_from_slice(workspace_id.as_uuid().as_bytes());
    match endpoint {
        Some(endpoint) => {
            bytes.push(1);
            bytes.extend_from_slice(endpoint.endpoint_id.as_uuid().as_bytes());
            bytes.extend_from_slice(endpoint.device_id.as_uuid().as_bytes());
            bytes.extend_from_slice(endpoint.graph_resource_id.as_bytes());
        }
        None => {
            bytes.push(0);
            bytes.extend_from_slice(&[0_u8; 16]);
            bytes.extend_from_slice(&[0_u8; 16]);
            bytes.extend_from_slice(&[0_u8; 32]);
        }
    }
    bytes
}

fn charge_catalog_directory_entry(
    count: &mut usize,
    limit: usize,
) -> Result<(), ProjectionStoreError> {
    *count = count.saturating_add(1);
    if *count > limit {
        Err(ProjectionStoreError::EvidenceTooLarge {
            kind: "projection catalog directory entries",
            declared: *count as u64,
            limit: limit as u64,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod catalog_limit_tests {
    use super::*;

    #[test]
    fn catalog_directory_budget_counts_temp_entries_before_loading() {
        let mut count = 0;
        charge_catalog_directory_entry(&mut count, 2).unwrap();
        charge_catalog_directory_entry(&mut count, 2).unwrap();
        assert!(matches!(
            charge_catalog_directory_entry(&mut count, 2),
            Err(ProjectionStoreError::EvidenceTooLarge {
                kind: "projection catalog directory entries",
                declared: 3,
                limit: 2
            })
        ));
    }
}

/// The current claim magic as the contract spells it (an escaped trailing NUL).
#[cfg(test)]
pub(crate) fn store_claim_magic_display() -> String {
    display_claim_magic(STORE_CLAIM_MAGIC)
}

#[cfg(test)]
pub(crate) const fn store_claim_version() -> u32 {
    STORE_CLAIM_VERSION
}

#[cfg(test)]
pub(crate) fn prior_store_claim_magics_display() -> Vec<String> {
    PRIOR_STORE_CLAIM_MAGICS
        .iter()
        .map(|magic| display_claim_magic(magic))
        .collect()
}

#[cfg(test)]
fn display_claim_magic(magic: &[u8; 8]) -> String {
    let text = std::str::from_utf8(&magic[..7]).expect("claim magic is ASCII");
    assert_eq!(magic[7], 0, "a claim magic ends in one NUL byte");
    format!("{text}\\0")
}

fn read_claim_prefix(file: &mut impl Read, bytes: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < bytes.len() {
        match file.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// The precheck's decision, split out so it can be exercised on bytes alone.
///
/// It deliberately answers only the questions that can be answered without the
/// store id, workspace or endpoint: magic family, version, and the exact
/// version-specific envelope length. Everything else stays with `validate_claim`
/// at the in-place boundary.
fn classify_precheck_claim(bytes: &[u8]) -> Result<(), ProjectionStoreError> {
    for magic in PRIOR_STORE_CLAIM_MAGICS {
        if bytes.len() >= magic.len() + 4 && &bytes[..magic.len()] == magic {
            let version = u32::from_be_bytes(
                bytes[magic.len()..magic.len() + 4]
                    .try_into()
                    .expect("prior claim version slice"),
            );
            return Err(ProjectionStoreError::UpgradeRequired {
                found: version,
                current: STORE_CLAIM_VERSION,
            });
        }
    }
    if bytes.len() < STORE_CLAIM_MAGIC.len() + 4
        || &bytes[..STORE_CLAIM_MAGIC.len()] != STORE_CLAIM_MAGIC
    {
        return Err(ProjectionStoreError::MalformedStoreClaim);
    }
    let version = u32::from_be_bytes(
        bytes[STORE_CLAIM_MAGIC.len()..STORE_CLAIM_MAGIC.len() + 4]
            .try_into()
            .expect("claim version slice"),
    );
    if version < STORE_CLAIM_VERSION {
        return Err(ProjectionStoreError::UpgradeRequired {
            found: version,
            current: STORE_CLAIM_VERSION,
        });
    }
    if version > STORE_CLAIM_VERSION {
        return Err(ProjectionStoreError::UnknownStoreVersion(version));
    }
    // R21-C1: a current-magic header on a truncated -- or over-long -- body
    // must not pass. Without this, graph publication recovery runs before the
    // in-place length check ever fires.
    if bytes.len() != STORE_CLAIM_LEN {
        return Err(ProjectionStoreError::MalformedStoreClaim);
    }
    Ok(())
}

fn validate_claim(
    bytes: &[u8],
    expected_store_id: ProjectionReceiptStoreId,
    expected_workspace: WorkspaceId,
    expected_endpoint: Option<ProjectionEndpointBinding>,
) -> Result<[DirectoryIdentity; 5], ProjectionStoreError> {
    for magic in PRIOR_STORE_CLAIM_MAGICS {
        if bytes.len() >= magic.len() + 4 && &bytes[..magic.len()] == magic {
            let version = u32::from_be_bytes(
                bytes[magic.len()..magic.len() + 4]
                    .try_into()
                    .expect("prior claim version slice"),
            );
            return Err(ProjectionStoreError::UpgradeRequired {
                found: version,
                current: STORE_CLAIM_VERSION,
            });
        }
    }
    if bytes.len() < STORE_CLAIM_MAGIC.len() + 4
        || &bytes[..STORE_CLAIM_MAGIC.len()] != STORE_CLAIM_MAGIC
    {
        return Err(ProjectionStoreError::MalformedStoreClaim);
    }
    let version = u32::from_be_bytes(
        bytes[STORE_CLAIM_MAGIC.len()..STORE_CLAIM_MAGIC.len() + 4]
            .try_into()
            .expect("claim version slice"),
    );
    if version < STORE_CLAIM_VERSION {
        return Err(ProjectionStoreError::UpgradeRequired {
            found: version,
            current: STORE_CLAIM_VERSION,
        });
    }
    if version > STORE_CLAIM_VERSION {
        return Err(ProjectionStoreError::UnknownStoreVersion(version));
    }
    if bytes.len() != STORE_CLAIM_LEN {
        return Err(ProjectionStoreError::MalformedStoreClaim);
    }
    let store_offset = STORE_CLAIM_MAGIC.len() + 4;
    if bytes[store_offset..store_offset + 32] != *expected_store_id.as_bytes() {
        return Err(ProjectionStoreError::EndpointBindingMismatch);
    }
    let workspace_offset = store_offset + 32;
    let workspace = WorkspaceId::from_uuid(
        Uuid::from_slice(&bytes[workspace_offset..workspace_offset + 16])
            .map_err(|_| ProjectionStoreError::MalformedStoreClaim)?,
    );
    if workspace != expected_workspace {
        return Err(ProjectionStoreError::WorkspaceMismatch {
            expected: expected_workspace,
            found: workspace,
        });
    }
    let mut identities = [[0_u8; 32]; 5];
    for (index, identity) in identities.iter_mut().enumerate() {
        let offset = STORE_CLAIM_BASE_LEN + index * 32;
        identity.copy_from_slice(&bytes[offset..offset + 32]);
    }
    if bytes
        != claim_bytes(
            expected_store_id,
            expected_workspace,
            expected_endpoint,
            &identities,
        )
    {
        return Err(ProjectionStoreError::EndpointBindingMismatch);
    }
    Ok(identities)
}

fn open_receipt_namespaces(
    capability: &Dir,
    store_id: ProjectionReceiptStoreId,
    durability: ReceiptDirectoryDurability,
) -> Result<ReceiptNamespaces, ProjectionStoreError> {
    let forensics = open_bound_namespace(capability, FORENSICS_DIR)?;
    let pending_cleanup =
        open_pending_cleanup_namespace(&forensics.capability, store_id, durability)?;
    Ok(ReceiptNamespaces {
        bases: open_bound_namespace(capability, BASES_DIR)?,
        intents: open_bound_namespace(capability, INTENTS_DIR)?,
        completions: open_bound_namespace(capability, COMPLETIONS_DIR)?,
        attempts: open_bound_namespace(capability, ATTEMPTS_DIR)?,
        forensics,
        pending_cleanup,
    })
}

fn open_pending_cleanup_namespace(
    forensics: &Dir,
    store_id: ProjectionReceiptStoreId,
    durability: ReceiptDirectoryDurability,
) -> Result<BoundNamespace, ProjectionStoreError> {
    let existing = read_optional_regular(forensics, PENDING_CLEANUP_AUTHORITY, 1024, None)?;
    let initializing = existing.is_none();
    if initializing {
        match open_dir_nofollow(forensics, PENDING_CLEANUP_DIR) {
            // If a prior create made the name visible before its barrier
            // refused, the authority publication below synchronizes this same
            // parent before initialization can succeed.
            Ok(_) => {}
            Err(StoreError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                ensure_directory_nofollow_with_durability(
                    forensics,
                    PENDING_CLEANUP_DIR,
                    durability,
                )?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let directory = open_dir_nofollow(forensics, PENDING_CLEANUP_DIR)?;
    let identity = canonical_directory_identity(&directory)?;
    let authority = PendingCleanupNamespaceAuthority {
        schema_version: PENDING_CLEANUP_NAMESPACE_SCHEMA_VERSION,
        store_id,
        directory_identity: identity,
    };
    let expected = serde_json::to_vec(&authority)
        .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?;
    match &existing {
        Some(bytes) if *bytes != expected => {
            return Err(ProjectionStoreError::NamespaceSubstitution(
                PENDING_CLEANUP_DIR.into(),
            ))
        }
        Some(_) => {}
        None => {}
    }
    initialize_pending_cleanup_rounds(&directory, store_id, identity, initializing, durability)?;
    if initializing {
        publish_immutable_exact_with_durability(
            forensics,
            PENDING_CLEANUP_AUTHORITY,
            &expected,
            "pending projection cleanup namespace authority",
            durability,
        )?;
    }
    Ok(BoundNamespace {
        capability: directory,
        identity,
    })
}

fn initialize_pending_cleanup_rounds(
    namespace: &Dir,
    store_id: ProjectionReceiptStoreId,
    namespace_identity: DirectoryIdentity,
    allow_initialization: bool,
    durability: ReceiptDirectoryDurability,
) -> Result<(), ProjectionStoreError> {
    let existing = read_optional_mutation_authority_bounded(
        namespace,
        PENDING_CLEANUP_ROUND_STATE,
        MAX_PENDING_CLEANUP_ROUND_STATE_BYTES,
        None,
    )?;
    if existing.is_none() {
        if !allow_initialization {
            return Err(ProjectionStoreError::NamespaceSubstitution(
                PENDING_CLEANUP_ROUND_STATE.into(),
            ));
        }
        validate_pending_cleanup_round_root(namespace, false)?;
        for name in PENDING_CLEANUP_ROUND_DIRS {
            match open_dir_nofollow(namespace, name) {
                // The round-state publication below synchronizes this same
                // parent, including any existing name left by a refused create.
                Ok(_) => {}
                Err(StoreError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                    ensure_directory_nofollow_with_durability(namespace, name, durability)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        let rounds = [
            open_dir_nofollow(namespace, PENDING_CLEANUP_ROUND_DIRS[0])?,
            open_dir_nofollow(namespace, PENDING_CLEANUP_ROUND_DIRS[1])?,
        ];
        let state = PendingCleanupRoundState {
            schema_version: PENDING_CLEANUP_ROUND_STATE_SCHEMA_VERSION,
            store_id,
            namespace_identity,
            round_identities: [
                canonical_directory_identity(&rounds[0])?,
                canonical_directory_identity(&rounds[1])?,
            ],
            active_round: 0,
        };
        let bytes = encode_pending_cleanup_round_state(&state)?;
        publish_immutable_exact_with_durability(
            namespace,
            PENDING_CLEANUP_ROUND_STATE,
            &bytes,
            "pending projection cleanup round state",
            durability,
        )?;
    }
    let _ = open_pending_cleanup_rounds(namespace, store_id, namespace_identity)?;
    Ok(())
}

fn validate_pending_cleanup_round_root(
    namespace: &Dir,
    require_state: bool,
) -> Result<(), ProjectionStoreError> {
    let mut seen_state = false;
    let mut seen_rounds = [false; 2];
    let mut removed_temporary = false;
    let mut count = 0usize;
    for entry in namespace.entries()? {
        count += 1;
        if count > 4 {
            return Err(ProjectionStoreError::UnsafeEntry(
                "pending projection cleanup round root has too many entries".into(),
            ));
        }
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            ProjectionStoreError::UnsafeEntry(
                "non-UTF-8 pending projection cleanup round entry".into(),
            )
        })?;
        if is_temp_name(name) {
            require_regular_entry(&entry.file_type()?, name)?;
            namespace.remove_file(name)?;
            removed_temporary = true;
            continue;
        }
        if name == PENDING_CLEANUP_ROUND_STATE {
            require_regular_entry(&entry.file_type()?, name)?;
            if seen_state {
                return Err(ProjectionStoreError::UnsafeEntry(
                    "duplicate pending projection cleanup round state".into(),
                ));
            }
            seen_state = true;
            continue;
        }
        let Some(round) = PENDING_CLEANUP_ROUND_DIRS
            .iter()
            .position(|candidate| *candidate == name)
        else {
            return Err(ProjectionStoreError::UnsafeEntry(format!(
                "unknown pending projection cleanup round entry: {name}"
            )));
        };
        if !entry.file_type()?.is_dir() || seen_rounds[round] {
            return Err(ProjectionStoreError::NamespaceSubstitution(name.into()));
        }
        seen_rounds[round] = true;
    }
    if removed_temporary {
        sync_dir_required(namespace)?;
    }
    if require_state && (!seen_state || seen_rounds.iter().any(|seen| !seen)) {
        return Err(ProjectionStoreError::NamespaceSubstitution(
            PENDING_CLEANUP_DIR.into(),
        ));
    }
    if !require_state && seen_state {
        return Err(ProjectionStoreError::NamespaceSubstitution(
            PENDING_CLEANUP_ROUND_STATE.into(),
        ));
    }
    Ok(())
}

fn open_pending_cleanup_rounds(
    namespace: &Dir,
    store_id: ProjectionReceiptStoreId,
    namespace_identity: DirectoryIdentity,
) -> Result<OpenPendingCleanupRounds, ProjectionStoreError> {
    validate_pending_cleanup_round_root(namespace, true)?;
    let state_bytes = read_optional_mutation_authority_bounded(
        namespace,
        PENDING_CLEANUP_ROUND_STATE,
        MAX_PENDING_CLEANUP_ROUND_STATE_BYTES,
        None,
    )?
    .ok_or_else(|| {
        ProjectionStoreError::NamespaceSubstitution(PENDING_CLEANUP_ROUND_STATE.into())
    })?;
    let state = decode_pending_cleanup_round_state(&state_bytes)?;
    let rounds = [
        open_dir_nofollow(namespace, PENDING_CLEANUP_ROUND_DIRS[0])?,
        open_dir_nofollow(namespace, PENDING_CLEANUP_ROUND_DIRS[1])?,
    ];
    let round_identities = [
        canonical_directory_identity(&rounds[0])?,
        canonical_directory_identity(&rounds[1])?,
    ];
    if state.store_id != store_id
        || state.namespace_identity != namespace_identity
        || state.round_identities != round_identities
        || state.active_round > 1
    {
        return Err(ProjectionStoreError::NamespaceSubstitution(
            PENDING_CLEANUP_ROUND_STATE.into(),
        ));
    }
    Ok(OpenPendingCleanupRounds {
        state,
        state_bytes,
        rounds,
    })
}

fn open_bound_namespace(
    capability: &Dir,
    name: &str,
) -> Result<BoundNamespace, ProjectionStoreError> {
    let directory = open_dir_nofollow(capability, name)
        .map_err(|error| ProjectionStoreError::NamespaceSubstitution(format!("{name}: {error}")))?;
    Ok(BoundNamespace {
        identity: canonical_directory_identity(&directory)?,
        capability: directory,
    })
}

fn require_incomplete_store_is_empty(capability: &Dir) -> Result<(), ProjectionStoreError> {
    for entry in capability.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            ProjectionStoreError::UnsafeEntry("non-UTF-8 entry in incomplete receipt store".into())
        })?;
        if name == STORE_INIT_FILE {
            require_regular_entry(&entry.file_type()?, name)?;
            continue;
        }
        if ![
            BASES_DIR,
            INTENTS_DIR,
            COMPLETIONS_DIR,
            ATTEMPTS_DIR,
            FORENSICS_DIR,
        ]
        .contains(&name)
            || !entry.file_type()?.is_dir()
        {
            return Err(ProjectionStoreError::ClaimlessNonemptyStore);
        }
        let directory = open_dir_nofollow(capability, name)?;
        if directory.entries()?.next().transpose()?.is_some() {
            return Err(ProjectionStoreError::ClaimlessNonemptyStore);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn canonical_receipt_store_id(dir: &Dir) -> Result<ProjectionReceiptStoreId, ProjectionStoreError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = dir.try_clone()?.into_std_file().metadata()?;
    let mut identity = [0_u8; 16];
    identity[..8].copy_from_slice(&metadata.dev().to_be_bytes());
    identity[8..].copy_from_slice(&metadata.ino().to_be_bytes());
    Ok(ProjectionReceiptStoreId::from_capability_identity(
        b"unix-dev-inode",
        &identity,
    ))
}

#[cfg(windows)]
fn canonical_receipt_store_id(dir: &Dir) -> Result<ProjectionReceiptStoreId, ProjectionStoreError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let file = dir.try_clone()?.into_std_file();
    let mut information = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(ProjectionStoreError::Io(std::io::Error::last_os_error()));
    }
    let mut identity = [0_u8; 24];
    identity[..8].copy_from_slice(&information.VolumeSerialNumber.to_be_bytes());
    identity[8..].copy_from_slice(&information.FileId.Identifier);
    Ok(ProjectionReceiptStoreId::from_capability_identity(
        b"windows-volume-file-id",
        &identity,
    ))
}

#[cfg(not(any(unix, windows)))]
fn canonical_receipt_store_id(
    _dir: &Dir,
) -> Result<ProjectionReceiptStoreId, ProjectionStoreError> {
    Err(ProjectionStoreError::Io(std::io::Error::new(
        ErrorKind::Unsupported,
        "projection receipt-store identity is unsupported on this platform",
    )))
}

#[cfg(unix)]
fn canonical_directory_identity(dir: &Dir) -> Result<DirectoryIdentity, ProjectionStoreError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = dir.try_clone()?.into_std_file().metadata()?;
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-directory-identity/unix-v1\0");
    hasher.update(metadata.dev().to_be_bytes());
    hasher.update(metadata.ino().to_be_bytes());
    Ok(hasher.finalize().into())
}

#[cfg(windows)]
fn canonical_directory_identity(dir: &Dir) -> Result<DirectoryIdentity, ProjectionStoreError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let file = dir.try_clone()?.into_std_file();
    let mut information = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(ProjectionStoreError::Io(std::io::Error::last_os_error()));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-directory-identity/windows-v1\0");
    hasher.update(information.VolumeSerialNumber.to_be_bytes());
    hasher.update(information.FileId.Identifier);
    Ok(hasher.finalize().into())
}

#[cfg(not(any(unix, windows)))]
fn canonical_directory_identity(_dir: &Dir) -> Result<DirectoryIdentity, ProjectionStoreError> {
    Err(ProjectionStoreError::Io(std::io::Error::new(
        ErrorKind::Unsupported,
        "projection directory identity is unsupported on this platform",
    )))
}

fn base_filename(description: BlobDescription) -> String {
    format!("{}.base", hex(description.sha256()))
}

fn intent_filename(intent_id: ProjectionIntentId) -> String {
    format!("{}.intent", hex(intent_id.as_bytes()))
}

fn completion_filename(intent_id: ProjectionIntentId) -> String {
    format!("{}.completion", hex(intent_id.as_bytes()))
}

fn deterministic_mutation_uuid(
    domain: &[u8],
    store_id: ProjectionReceiptStoreId,
    intent_id: ProjectionIntentId,
) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(store_id.as_bytes());
    hasher.update(intent_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562 UUIDv8 marks these SHA-256-derived bytes as application-defined
    // while preserving the standard UUID variant.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn mutation_authority_filename(intent_id: ProjectionIntentId) -> String {
    format!("{}{}", hex(intent_id.as_bytes()), MUTATION_AUTHORITY_SUFFIX)
}

fn mutation_authority_lease_filename(intent_id: ProjectionIntentId) -> String {
    format!(
        "{}{}",
        hex(intent_id.as_bytes()),
        MUTATION_AUTHORITY_LEASE_SUFFIX
    )
}

#[cfg(unix)]
fn open_mutation_authority_file(directory: &Dir, name: &str) -> io::Result<File> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid authority file name"))?;
    // SAFETY: `name` is a live NUL-terminated relative name and `directory`
    // retains the receipt-store capability. O_NOFOLLOW binds validation and
    // reading to the same opened authority file.
    let fd = unsafe {
        libc::openat(
            directory.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `openat` returned a newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(windows)]
fn open_mutation_authority_file(directory: &Dir, name: &str) -> io::Result<File> {
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    Ok(directory.open_with(name, &options)?.into_std())
}

#[cfg(not(any(unix, windows)))]
fn open_mutation_authority_file(_directory: &Dir, _name: &str) -> io::Result<File> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "atomic no-follow projection mutation authorities are unsupported on this target",
    ))
}

fn read_optional_mutation_authority(
    directory: &Dir,
    name: &str,
    expected_length: Option<u64>,
) -> Result<Option<Vec<u8>>, ProjectionStoreError> {
    read_optional_mutation_authority_bounded(
        directory,
        name,
        MAX_MUTATION_AUTHORITY_BYTES as u64,
        expected_length,
    )
}

fn read_optional_mutation_authority_bounded(
    directory: &Dir,
    name: &str,
    max_length: u64,
    expected_length: Option<u64>,
) -> Result<Option<Vec<u8>>, ProjectionStoreError> {
    let mut file = match open_mutation_authority_file(directory, name) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(ProjectionStoreError::UnsafeEntry(format!(
            "projection mutation authority is not a regular file: {name}"
        )));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(ProjectionStoreError::UnsafeEntry(format!(
            "projection mutation authority has unexpected links: {name}"
        )));
    }
    #[cfg(windows)]
    if metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
    {
        return Err(ProjectionStoreError::UnsafeEntry(format!(
            "projection mutation authority is a reparse point: {name}"
        )));
    }
    let length = metadata.len();
    if length > max_length || expected_length.is_some_and(|expected| expected != length) {
        return Err(ProjectionStoreError::MutationAuthorityMismatch);
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length {
        return Err(ProjectionStoreError::MutationAuthorityMismatch);
    }
    Ok(Some(bytes))
}

fn remove_mutation_authority_if_exact(
    directory: &Dir,
    name: &str,
    expected: &[u8],
) -> Result<(), ProjectionStoreError> {
    let Some(bytes) =
        read_optional_mutation_authority(directory, name, Some(expected.len() as u64))?
    else {
        return Ok(());
    };
    if bytes != expected {
        return Err(ProjectionStoreError::MutationAuthorityMismatch);
    }
    directory.remove_file(name)?;
    sync_dir_required(directory)?;
    Ok(())
}

fn replace_mutation_authority_if_exact(
    directory: &Dir,
    name: &str,
    expected: &[u8],
    replacement: &[u8],
) -> Result<(), ProjectionStoreError> {
    replace_mutation_authority_if_exact_inner(directory, name, expected, replacement, true)
}

fn replace_mutation_authority_if_exact_inner(
    directory: &Dir,
    name: &str,
    expected: &[u8],
    replacement: &[u8],
    inject_cleanup_marker_failures: bool,
) -> Result<(), ProjectionStoreError> {
    #[cfg(not(test))]
    let _ = inject_cleanup_marker_failures;
    let current = read_optional_mutation_authority(directory, name, Some(expected.len() as u64))?
        .ok_or(ProjectionStoreError::MutationAuthorityMismatch)?;
    if current != expected || replacement.len() > MAX_MUTATION_AUTHORITY_BYTES {
        return Err(ProjectionStoreError::MutationAuthorityMismatch);
    }
    let temp_name = format!(".tmp-{}", Uuid::new_v4());
    let mut options = CapOpenOptions::new();
    options.write(true).create_new(true);
    let mut temp = directory.open_with(&temp_name, &options)?;
    let result = (|| {
        temp.write_all(replacement)?;
        crate::durability_counters::sync_file(&temp)?;
        drop(temp);
        #[cfg(test)]
        if inject_cleanup_marker_failures {
            FAIL_BEFORE_PROJECTION_CLEANUP_MARKER_SWAP.with(|fail| {
                if fail.replace(false) {
                    return Err(ProjectionStoreError::Io(io::Error::new(
                        ErrorKind::Interrupted,
                        "injected failure before projection cleanup marker swap",
                    )));
                }
                Ok(())
            })?;
        }
        directory.rename(&temp_name, directory, name)?;
        #[cfg(test)]
        if inject_cleanup_marker_failures {
            FAIL_AFTER_PROJECTION_CLEANUP_MARKER_SWAP.with(|fail| {
                if fail.replace(false) {
                    return Err(ProjectionStoreError::Io(io::Error::new(
                        ErrorKind::Interrupted,
                        "injected failure after projection cleanup marker swap",
                    )));
                }
                Ok(())
            })?;
        }
        sync_dir_required(directory)?;
        Ok::<_, ProjectionStoreError>(())
    })();
    let cleanup = directory.remove_file(&temp_name);
    if let Err(error) = result {
        let _ = cleanup;
        return Err(error);
    }
    if cleanup
        .as_ref()
        .is_err_and(|error| error.kind() != ErrorKind::NotFound)
    {
        cleanup?;
    }
    Ok(())
}

#[cfg(unix)]
fn open_mutation_authority_lease_file(directory: &Dir, name: &str) -> io::Result<File> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid lease file name"))?;
    // SAFETY: `name` is a live NUL-terminated relative name and `directory`
    // retains the authoritative receipt-store capability. O_NOFOLLOW rejects
    // a final-component symlink in the same open that produces the locked
    // handle.
    let fd = unsafe {
        libc::openat(
            directory.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `openat` returned a newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(windows)]
fn open_mutation_authority_lease_file(directory: &Dir, name: &str) -> io::Result<File> {
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .follow(FollowSymlinks::No);
    Ok(directory.open_with(name, &options)?.into_std())
}

#[cfg(not(any(unix, windows)))]
fn open_mutation_authority_lease_file(_directory: &Dir, _name: &str) -> io::Result<File> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "atomic no-follow projection mutation leases are unsupported on this target",
    ))
}

fn validate_mutation_authority_lease_file(
    file: &File,
    name: &str,
) -> Result<(), ProjectionStoreError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(ProjectionStoreError::UnsafeEntry(format!(
            "projection mutation lease is not a regular file: {name}"
        )));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(ProjectionStoreError::UnsafeEntry(format!(
            "projection mutation lease has unexpected links: {name}"
        )));
    }
    #[cfg(windows)]
    if metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
    {
        return Err(ProjectionStoreError::UnsafeEntry(format!(
            "projection mutation lease is a reparse point: {name}"
        )));
    }
    Ok(())
}

fn attempt_filename(attempt_id: Uuid) -> String {
    format!("{}.attempt", attempt_id.simple())
}

fn pending_cleanup_filename(record: &LocalProjectionEvidenceRecord) -> String {
    format!(
        "{}.{}{}",
        hex(record.intent_id().as_bytes()),
        record.attempt_id().simple(),
        PENDING_CLEANUP_SUFFIX
    )
}

fn next_non_retired_pending_entry(
    entries: &mut cap_std::fs::ReadDir,
    retired_prefixes: &[String],
) -> Result<Option<cap_std::fs::DirEntry>, ProjectionStoreError> {
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            ProjectionStoreError::UnsafeEntry("non-UTF-8 pending projection cleanup entry".into())
        })?;
        if !retired_prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn encode_pending_cleanup_round_state(
    state: &PendingCleanupRoundState,
) -> Result<Vec<u8>, ProjectionStoreError> {
    if state.schema_version != PENDING_CLEANUP_ROUND_STATE_SCHEMA_VERSION || state.active_round > 1
    {
        return Err(ProjectionStoreError::ForensicBindingMismatch);
    }
    serde_json::to_vec(state).map_err(|error| ProjectionStoreError::Encode(error.to_string()))
}

fn decode_pending_cleanup_round_state(
    bytes: &[u8],
) -> Result<PendingCleanupRoundState, ProjectionStoreError> {
    let state: PendingCleanupRoundState = serde_json::from_slice(bytes)
        .map_err(|error| ProjectionStoreError::Decode(error.to_string()))?;
    if encode_pending_cleanup_round_state(&state)? != bytes {
        return Err(ProjectionStoreError::ForensicBindingMismatch);
    }
    Ok(state)
}

fn publish_pending_cleanup_marker(
    namespace: &Dir,
    store_id: ProjectionReceiptStoreId,
    record: &LocalProjectionEvidenceRecord,
    bytes: &[u8],
) -> Result<(), ProjectionStoreError> {
    let namespace_identity = canonical_directory_identity(namespace)?;
    let queue = open_pending_cleanup_rounds(namespace, store_id, namespace_identity)?;
    let name = pending_cleanup_filename(record);
    let existing = [
        read_optional_mutation_authority(&queue.rounds[0], &name, None)?,
        read_optional_mutation_authority(&queue.rounds[1], &name, None)?,
    ];
    match (&existing[0], &existing[1]) {
        (Some(_), Some(_)) => {
            return Err(ProjectionStoreError::ForensicBindingMismatch);
        }
        (Some(existing), None) | (None, Some(existing)) => {
            if existing != bytes {
                return Err(ProjectionStoreError::ForensicBindingMismatch);
            }
            return Ok(());
        }
        (None, None) => {}
    }
    let inactive = 1usize - usize::from(queue.state.active_round);
    publish_immutable_exact(
        &queue.rounds[inactive],
        &name,
        bytes,
        "pending projection cleanup",
    )?;
    Ok(())
}

fn read_pending_cleanup_marker(
    namespace: &Dir,
    store_id: ProjectionReceiptStoreId,
    namespace_identity: DirectoryIdentity,
    name: &str,
) -> Result<(Dir, Vec<u8>), ProjectionStoreError> {
    let queue = open_pending_cleanup_rounds(namespace, store_id, namespace_identity)?;
    let first = read_optional_mutation_authority(&queue.rounds[0], name, None)?;
    let second = read_optional_mutation_authority(&queue.rounds[1], name, None)?;
    match (first, second) {
        (Some(bytes), None) => Ok((queue.rounds[0].try_clone()?, bytes)),
        (None, Some(bytes)) => Ok((queue.rounds[1].try_clone()?, bytes)),
        (Some(_), Some(_)) => Err(ProjectionStoreError::ForensicBindingMismatch),
        (None, None) => Err(ProjectionStoreError::ForensicBindingMismatch),
    }
}

fn flip_pending_cleanup_round(
    namespace: &Dir,
    queue: &OpenPendingCleanupRounds,
) -> Result<(), ProjectionStoreError> {
    let mut replacement = queue.state.clone();
    replacement.active_round = 1 - replacement.active_round;
    let replacement = encode_pending_cleanup_round_state(&replacement)?;
    replace_mutation_authority_if_exact_inner(
        namespace,
        PENDING_CLEANUP_ROUND_STATE,
        &queue.state_bytes,
        &replacement,
        false,
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn move_pending_cleanup_marker_noreplace(
    source: &Dir,
    destination: &Dir,
    name: &str,
) -> io::Result<()> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid cleanup marker name"))?;
    // SAFETY: both retained directory descriptors and the NUL-terminated leaf
    // name remain live for the duration of this capability-relative syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source.as_fd().as_raw_fd(),
            name.as_ptr(),
            destination.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn move_pending_cleanup_marker_noreplace(
    source: &Dir,
    destination: &Dir,
    name: &str,
) -> io::Result<()> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "invalid cleanup marker name"))?;
    // SAFETY: both retained directory descriptors and the NUL-terminated leaf
    // name remain live for the duration of this capability-relative syscall.
    let result = unsafe {
        libc::renameatx_np(
            source.as_fd().as_raw_fd(),
            name.as_ptr(),
            destination.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_EXCL as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(windows)]
fn move_pending_cleanup_marker_noreplace(
    source: &Dir,
    destination: &Dir,
    name: &str,
) -> io::Result<()> {
    source.rename(name, destination, name)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
fn move_pending_cleanup_marker_noreplace(
    _source: &Dir,
    _destination: &Dir,
    _name: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "atomic capability-relative cleanup queue rotation is unsupported",
    ))
}

fn local_forensic_record_digest(
    record: &LocalProjectionEvidenceRecord,
) -> Result<[u8; 32], ProjectionStoreError> {
    let bytes = serde_json::to_vec(record)
        .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?;
    Ok(Sha256::digest(&bytes).into())
}

fn encode_pending_cleanup_marker(
    marker: &PendingProjectionCleanupMarker,
) -> Result<Vec<u8>, ProjectionStoreError> {
    marker.validate()?;
    serde_json::to_vec(marker).map_err(|error| ProjectionStoreError::Encode(error.to_string()))
}

fn decode_pending_cleanup_marker(
    bytes: &[u8],
) -> Result<PendingProjectionCleanupMarker, ProjectionStoreError> {
    let marker: PendingProjectionCleanupMarker = serde_json::from_slice(bytes)
        .map_err(|error| ProjectionStoreError::Decode(error.to_string()))?;
    marker.validate()?;
    if encode_pending_cleanup_marker(&marker)? != bytes {
        return Err(ProjectionStoreError::ForensicBindingMismatch);
    }
    Ok(marker)
}

fn valid_local_forensic_version(record: &LocalProjectionEvidenceRecord) -> bool {
    match record.schema_version {
        PRIOR_LOCAL_FORENSIC_SCHEMA_VERSION => record.recovery_resource_id.is_none(),
        LOCAL_FORENSIC_SCHEMA_VERSION => true,
        _ => false,
    }
}

fn decode_local_forensic_record(
    bytes: &[u8],
) -> Result<LocalProjectionEvidenceRecord, ProjectionStoreError> {
    let record: LocalProjectionEvidenceRecord = serde_json::from_slice(bytes)
        .map_err(|error| ProjectionStoreError::Decode(error.to_string()))?;
    if !valid_local_forensic_version(&record)
        || serde_json::to_vec(&record)
            .map_err(|error| ProjectionStoreError::Encode(error.to_string()))?
            != bytes
    {
        return Err(ProjectionStoreError::ForensicBindingMismatch);
    }
    Ok(record)
}

fn parse_attempt_filename(name: &str) -> Result<Uuid, ProjectionStoreError> {
    let value = name
        .strip_suffix(".attempt")
        .ok_or_else(|| ProjectionStoreError::MalformedEvidenceName(name.into()))?;
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProjectionStoreError::MalformedEvidenceName(name.into()));
    }
    Uuid::parse_str(value).map_err(|_| ProjectionStoreError::MalformedEvidenceName(name.into()))
}

fn require_evidence_length(
    kind: &'static str,
    declared: u64,
    limit: u64,
) -> Result<(), ProjectionStoreError> {
    if declared > limit {
        return Err(ProjectionStoreError::EvidenceTooLarge {
            kind,
            declared,
            limit,
        });
    }
    Ok(())
}

fn require_canonical_evidence_name(
    name: &str,
    suffix: &'static str,
) -> Result<(), ProjectionStoreError> {
    let Some(digest) = name.strip_suffix(suffix) else {
        return Err(ProjectionStoreError::MalformedEvidenceName(name.into()));
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProjectionStoreError::MalformedEvidenceName(name.into()));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn endpoint_binding_bytes(binding: ProjectionEndpointBinding) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(binding.endpoint_id().as_uuid().as_bytes());
    bytes.extend_from_slice(binding.device_id().as_uuid().as_bytes());
    bytes.extend_from_slice(binding.graph_resource_id().as_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::oplog::{
        DocumentId, FrontierV2, LineageDigest, ObjectStore, PageId, ShardedHotEngine,
    };

    use super::*;

    #[test]
    fn android_promoted_receipt_revalidates_an_existing_parent_after_process_restart() {
        let identity = (17, 29);
        let first_process = AndroidReceiptBarrierState::default();
        first_process
            .record_mutation_barrier(identity, || Err::<(), _>(io::Error::other("refused")))
            .unwrap_err();

        // A new process has no inherited in-memory debt. It must still prove
        // the parent directory durable before accepting an exact visible name.
        let next_process = AndroidReceiptBarrierState::default();
        let barriers = AtomicUsize::new(0);
        next_process
            .verify_existing(identity, || {
                barriers.fetch_add(1, Ordering::SeqCst);
                Ok::<(), io::Error>(())
            })
            .unwrap();
        assert_eq!(barriers.load(Ordering::SeqCst), 1);

        next_process
            .verify_existing(identity, || {
                barriers.fetch_add(1, Ordering::SeqCst);
                Ok::<(), io::Error>(())
            })
            .unwrap();
        assert_eq!(
            barriers.load(Ordering::SeqCst),
            1,
            "one successful parent barrier covers later exact names in that process"
        );
    }

    #[test]
    fn android_promoted_receipt_refusal_or_panic_never_marks_the_parent_verified() {
        let identity = (31, 37);
        let state = AndroidReceiptBarrierState::default();
        let barriers = AtomicUsize::new(0);
        state
            .verify_existing(identity, || {
                barriers.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::other("refused"))
            })
            .unwrap_err();
        state
            .verify_existing(identity, || {
                barriers.fetch_add(1, Ordering::SeqCst);
                Ok::<(), io::Error>(())
            })
            .unwrap();
        assert_eq!(barriers.load(Ordering::SeqCst), 2);

        let panic_result = std::panic::catch_unwind(|| {
            state
                .record_mutation_barrier(identity, || -> Result<(), io::Error> {
                    panic!("cut after namespace mutation and before verification insertion")
                })
                .unwrap();
        });
        assert!(panic_result.is_err());
        state
            .verify_existing(identity, || {
                barriers.fetch_add(1, Ordering::SeqCst);
                Ok::<(), io::Error>(())
            })
            .unwrap();
        assert_eq!(barriers.load(Ordering::SeqCst), 3);
    }

    struct Fixture {
        root: PathBuf,
        graph_root: PathBuf,
        store: ProjectionReceiptStore,
        graph: Graph,
        intent: ProjectionIntent,
        target: Vec<u8>,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            Self::new_at(label, "pages/authority.md")
        }

        fn new_replacement(label: &str) -> Self {
            Self::new_at_with_base(label, "pages/authority.md", Some(b"- base\n"))
        }

        fn new_at(label: &str, target_path: &str) -> Self {
            Self::new_at_with_base(label, target_path, None)
        }

        fn new_at_with_base(label: &str, target_path: &str, base: Option<&[u8]>) -> Self {
            let root = std::env::temp_dir()
                .join(format!("tine-receipt-authority-{label}-{}", Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            let graph_root = root.join("graph");
            fs::create_dir(&graph_root).unwrap();
            fs::create_dir(graph_root.join("pages")).unwrap();
            if let Some(base) = base {
                let target = graph_root.join(target_path);
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::write(target, base).unwrap();
            }
            let graph = Graph::open(&graph_root);
            let store = ProjectionReceiptStore::open(
                &root.join("receipts"),
                WorkspaceId::from_uuid(Uuid::from_u128(1)),
            )
            .unwrap();
            let target = b"- target\n".to_vec();
            let intent = ProjectionIntent::new(
                store.workspace_id(),
                PageId::from_uuid(Uuid::from_u128(2)),
                ManagedPath::parse(target_path).unwrap(),
                FrontierV2::default(),
                Vec::new(),
                base.map_or(ProjectionPrecondition::Absent, |base| {
                    ProjectionPrecondition::Base(BlobDescription::of(base))
                }),
                crate::oplog::ProjectionTargetKind::Present,
                BlobDescription::of(&target),
                Vec::new(),
            )
            .unwrap();
            store.publish_intent(&intent, base).unwrap();
            Self {
                root,
                graph_root,
                store,
                graph,
                intent,
                target,
            }
        }

        fn complete_replacement(
            &self,
        ) -> (
            ProjectionAttemptReservation,
            PathBuf,
            Vec<LocalProjectionEvidenceRecord>,
        ) {
            let reservation = self.store.reserve_attempt(&self.intent).unwrap();
            let recovery_path = self
                .graph_root
                .join(self.intent.path().as_str())
                .parent()
                .unwrap()
                .join(reservation.recovery_filename());
            let mut authority = self
                .store
                .begin_mutation(&self.intent, Some(&reservation))
                .unwrap();
            let proof = self
                .graph
                .write_page_projection(
                    self.intent.path().as_str(),
                    Some(b"- base\n"),
                    &self.target,
                    &mut authority,
                )
                .unwrap();
            self.store
                .publish_completion(authority, &self.intent, &proof)
                .unwrap();
            let records = self.store.local_forensic_evidence(&self.intent).unwrap();
            (reservation, recovery_path, records)
        }

        fn snapshot_graph(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
            let mut snapshot = BTreeMap::new();
            let mut pending = vec![self.graph_root.clone()];
            while let Some(path) = pending.pop() {
                let relative = path.strip_prefix(&self.graph_root).unwrap().to_path_buf();
                if path.is_dir() {
                    snapshot.insert(relative, None);
                    for entry in fs::read_dir(path).unwrap() {
                        pending.push(entry.unwrap().path());
                    }
                } else {
                    snapshot.insert(relative, Some(fs::read(path).unwrap()));
                }
            }
            snapshot
        }

        fn snapshot_store(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
            let mut snapshot = BTreeMap::new();
            let mut pending = vec![self.store.root_path().to_path_buf()];
            while let Some(path) = pending.pop() {
                let relative = path
                    .strip_prefix(self.store.root_path())
                    .unwrap()
                    .to_path_buf();
                if path.is_dir() {
                    snapshot.insert(relative, None);
                    for entry in fs::read_dir(path).unwrap() {
                        pending.push(entry.unwrap().path());
                    }
                } else {
                    snapshot.insert(relative, Some(fs::read(path).unwrap()));
                }
            }
            snapshot
        }

        fn authority_path(&self, intent: &ProjectionIntent) -> PathBuf {
            self.store
                .root_path()
                .join(mutation_authority_filename(intent.id().unwrap()))
        }

        fn reopen_store(&self) -> ProjectionReceiptStore {
            ProjectionReceiptStore::open(self.store.root_path(), self.store.workspace_id()).unwrap()
        }

        fn authority_stats(&self) -> (usize, u64) {
            fs::read_dir(self.store.root_path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with(MUTATION_AUTHORITY_SUFFIX)
                })
                .fold((0, 0), |(count, bytes), entry| {
                    (count + 1, bytes + entry.metadata().unwrap().len())
                })
        }

        fn attempt_stats(&self, intent: &ProjectionIntent) -> (usize, u64) {
            fs::read_dir(
                self.store
                    .root_path()
                    .join(ATTEMPTS_DIR)
                    .join(hex(intent.id().unwrap().as_bytes())),
            )
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".attempt"))
            .fold((0, 0), |(count, bytes), entry| {
                (count + 1, bytes + entry.metadata().unwrap().len())
            })
        }

        fn attempt_snapshot(&self, intent: &ProjectionIntent) -> BTreeMap<String, Vec<u8>> {
            fs::read_dir(
                self.store
                    .root_path()
                    .join(ATTEMPTS_DIR)
                    .join(hex(intent.id().unwrap().as_bytes())),
            )
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                name.ends_with(".attempt")
                    .then(|| (name, fs::read(entry.path()).unwrap()))
            })
            .collect()
        }

        fn leave_durable_completion_with_slot(&self) -> ProjectionCompletion {
            let reservation = self.store.reserve_attempt(&self.intent).unwrap();
            let mut authority = self
                .store
                .begin_mutation(&self.intent, Some(&reservation))
                .unwrap();
            let proof = self
                .graph
                .write_page_projection(
                    self.intent.path().as_str(),
                    None,
                    &self.target,
                    &mut authority,
                )
                .unwrap();
            let completion = ProjectionCompletion::for_intent(&self.intent, proof.bytes()).unwrap();
            let bytes = completion.encode().unwrap();
            publish_immutable_exact(
                &authority.completions,
                &completion_filename(self.intent.id().unwrap()),
                &bytes,
                "projection completion",
            )
            .unwrap();
            drop(authority);
            assert!(self.authority_path(&self.intent).exists());
            completion
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn absence_decision_map_steady_open_skips_the_full_receiver_catalog() {
        let fixture = Fixture::new("absence-summary-map-cost-fail-before");
        let archive_path = fixture.root.join("operations");
        let mut rebuild_engine = ShardedHotEngine::new(
            fixture.store.workspace_id(),
            LineageDigest::of(b"absence-summary-map-cost"),
            DocumentId::from_uuid(Uuid::from_u128(0xc6_0001)),
        );
        rebuild_engine
            .attach_clean_archive_store(
                ObjectStore::open(&archive_path, fixture.store.workspace_id()).unwrap(),
            )
            .unwrap();
        rebuild_engine
            .open_absence_decision_map(&fixture.store)
            .unwrap();

        let mut engine = ShardedHotEngine::new(
            fixture.store.workspace_id(),
            LineageDigest::of(b"absence-summary-map-cost"),
            DocumentId::from_uuid(Uuid::from_u128(0xc6_0001)),
        );
        engine
            .attach_clean_archive_store(
                ObjectStore::open(&archive_path, fixture.store.workspace_id()).unwrap(),
            )
            .unwrap();

        reset_projection_store_test_counters();
        engine.open_absence_decision_map(&fixture.store).unwrap();
        let measured = projection_store_test_counters();
        assert_eq!(
            measured.catalog_directory_entries, 0,
            "a steady absence-map open must not enumerate the lifetime receiver catalog"
        );
        let summary = engine
            .receiver_absence_summary_open_stats_for_test()
            .expect("managed open records summary cost");
        assert_eq!(summary.full_catalog_passes, 0);
        assert_eq!(summary.receipt_content_reads, 0);
    }

    #[test]
    fn root_attempt_reservation_uses_the_target_filename_without_panicking() {
        let fixture = Fixture::new_at("root-attempt-reservation", "Root.md");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();

        assert_eq!(
            reservation.recovery_filename(),
            format!(
                ".Root.md.{}.projection.recovery",
                reservation.attempt_id().simple()
            )
        );
    }

    #[test]
    fn root_and_nested_forensic_paths_validate_exactly_and_forgery_fails_closed() {
        for (label, target_path, expected_parent) in [
            ("root-forensic-path", "Root.md", None),
            (
                "nested-forensic-path",
                "pages/deep/Target.md",
                Some("pages/deep"),
            ),
        ] {
            let fixture = Fixture::new_at(label, target_path);
            let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
            let expected_filename = format!(
                ".{}.{}.projection.recovery",
                fixture.intent.path().file_name(),
                reservation.attempt_id().simple()
            );
            assert_eq!(reservation.recovery_filename(), expected_filename);

            let expected_relative_path = match expected_parent {
                Some(parent) => format!("{parent}/{expected_filename}"),
                None => expected_filename.clone(),
            };
            let record = LocalProjectionEvidenceRecord {
                schema_version: LOCAL_FORENSIC_SCHEMA_VERSION,
                intent_id: fixture.intent.id().unwrap(),
                attempt_id: reservation.attempt_id(),
                target_path: fixture.intent.path().clone(),
                recovery_relative_path: expected_relative_path.clone(),
                recovery_filename: expected_filename.clone(),
                recovery_resource_id: Some(ContentDigest::from_bytes([7; 32])),
                observed: BlobDescription::of(b"- displaced\n"),
            };
            fixture
                .store
                .validate_forensic_record_with_reservation(&fixture.intent, &record, &reservation)
                .unwrap();
            assert_eq!(
                record.recovery_relative_path(),
                expected_relative_path.as_str()
            );

            if expected_parent.is_none() {
                let mut leading_slash = record.clone();
                leading_slash.recovery_relative_path =
                    format!("/{}", leading_slash.recovery_relative_path);
                assert!(matches!(
                    fixture.store.validate_forensic_record_with_reservation(
                        &fixture.intent,
                        &leading_slash,
                        &reservation,
                    ),
                    Err(ProjectionStoreError::ForensicBindingMismatch)
                ));

                let mut wrong_parent = record.clone();
                wrong_parent.recovery_relative_path =
                    format!("pages/{}", wrong_parent.recovery_filename);
                assert!(matches!(
                    fixture.store.validate_forensic_record_with_reservation(
                        &fixture.intent,
                        &wrong_parent,
                        &reservation,
                    ),
                    Err(ProjectionStoreError::ForensicBindingMismatch)
                ));

                let mut wrong_filename = record.clone();
                wrong_filename.recovery_filename = format!(
                    ".Wrong.md.{}.projection.recovery",
                    reservation.attempt_id().simple()
                );
                wrong_filename.recovery_relative_path = fixture
                    .intent
                    .path()
                    .join_sibling(&wrong_filename.recovery_filename)
                    .unwrap();
                assert!(matches!(
                    fixture.store.validate_forensic_record_with_reservation(
                        &fixture.intent,
                        &wrong_filename,
                        &reservation,
                    ),
                    Err(ProjectionStoreError::ForensicBindingMismatch)
                ));
            }
        }
    }

    #[test]
    fn attempt_namespace_delete_or_substitute_after_capture_denies_before_graph_mutation() {
        #[derive(Clone, Copy)]
        enum Attack {
            DeleteFile,
            SubstituteFile,
            DeleteDirectory,
            SubstituteDirectory,
        }

        for (label, attack) in [
            ("delete-file-before-mutation", Attack::DeleteFile),
            ("substitute-file-before-mutation", Attack::SubstituteFile),
            ("delete-dir-before-mutation", Attack::DeleteDirectory),
            (
                "substitute-dir-before-mutation",
                Attack::SubstituteDirectory,
            ),
        ] {
            let fixture = Fixture::new(label);
            let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
            let before = fixture.snapshot_graph();
            let intent_name = hex(fixture.intent.id().unwrap().as_bytes());
            let attempt_dir = fixture
                .store
                .root_path()
                .join(ATTEMPTS_DIR)
                .join(&intent_name);
            let attempt_file = attempt_dir.join(attempt_filename(reservation.attempt_id()));
            MUTATION_AUTHORITY_CAPTURED_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || match attack {
                    Attack::DeleteFile => fs::remove_file(attempt_file).unwrap(),
                    Attack::SubstituteFile => {
                        fs::write(attempt_file, b"substituted reservation").unwrap()
                    }
                    Attack::DeleteDirectory | Attack::SubstituteDirectory => {
                        fs::remove_file(attempt_file).unwrap();
                        fs::remove_dir(&attempt_dir).unwrap();
                        if matches!(attack, Attack::SubstituteDirectory) {
                            fs::create_dir(&attempt_dir).unwrap();
                        }
                    }
                }));
            });
            let mut authority = fixture
                .store
                .begin_mutation(&fixture.intent, Some(&reservation))
                .unwrap();

            assert!(fixture
                .graph
                .write_page_projection(
                    fixture.intent.path().as_str(),
                    None,
                    &fixture.target,
                    &mut authority,
                )
                .is_err());
            assert_eq!(fixture.snapshot_graph(), before);
            drop(authority);
            assert!(!fs::read_dir(fixture.store.root_path())
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(MUTATION_AUTHORITY_SUFFIX)));
        }
    }

    #[test]
    fn authority_or_attempt_change_after_validation_denies_before_graph_mutation() {
        enum Attack {
            RemoveRootAuthority,
            RemoveActiveAttempt,
            SubstituteAttemptNamespace,
            InsertCanonicalAttempt,
        }

        for (label, attack) in [
            ("remove-root-authority-at-act", Attack::RemoveRootAuthority),
            ("remove-active-attempt-at-act", Attack::RemoveActiveAttempt),
            (
                "substitute-attempt-namespace-at-act",
                Attack::SubstituteAttemptNamespace,
            ),
            (
                "insert-canonical-attempt-at-act",
                Attack::InsertCanonicalAttempt,
            ),
        ] {
            let fixture = Fixture::new(label);
            let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
            let before = fixture.snapshot_graph();
            let intent_name = hex(fixture.intent.id().unwrap().as_bytes());
            let attempt_dir = fixture
                .store
                .root_path()
                .join(ATTEMPTS_DIR)
                .join(&intent_name);
            let attempt_file = attempt_dir.join(attempt_filename(reservation.attempt_id()));
            let root = fixture.store.root_path().to_path_buf();
            let reservation_for_hook = reservation.clone();
            MUTATION_AUTHORITY_ACT_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || match attack {
                    Attack::RemoveRootAuthority => {
                        let authority = fs::read_dir(&root)
                            .unwrap()
                            .map(Result::unwrap)
                            .find(|entry| {
                                entry
                                    .file_name()
                                    .to_string_lossy()
                                    .ends_with(MUTATION_AUTHORITY_SUFFIX)
                            })
                            .expect("published mutation authority");
                        fs::remove_file(authority.path()).unwrap();
                    }
                    Attack::RemoveActiveAttempt => fs::remove_file(attempt_file).unwrap(),
                    Attack::SubstituteAttemptNamespace => {
                        fs::remove_file(attempt_file).unwrap();
                        fs::remove_dir(&attempt_dir).unwrap();
                        fs::create_dir(&attempt_dir).unwrap();
                    }
                    Attack::InsertCanonicalAttempt => {
                        let mut extra = reservation_for_hook;
                        extra.attempt_id = Uuid::new_v4();
                        extra.recovery_filename = format!(
                            ".authority.md.{}.projection.recovery",
                            extra.attempt_id.simple()
                        );
                        fs::write(
                            attempt_dir.join(attempt_filename(extra.attempt_id)),
                            serde_json::to_vec(&extra).unwrap(),
                        )
                        .unwrap();
                    }
                }));
            });
            let mut authority = fixture
                .store
                .begin_mutation(&fixture.intent, Some(&reservation))
                .unwrap();

            assert!(
                fixture
                    .graph
                    .write_page_projection(
                        fixture.intent.path().as_str(),
                        None,
                        &fixture.target,
                        &mut authority,
                    )
                    .is_err(),
                "attack {label} reached the graph act"
            );
            assert_eq!(
                fixture.snapshot_graph(),
                before,
                "attack {label} mutated the graph"
            );
        }
    }

    #[test]
    fn store_claim_or_intent_delete_or_substitute_after_capture_denies_before_mutation() {
        #[derive(Clone, Copy)]
        enum Attack {
            DeleteClaim,
            SubstituteClaim,
            DeleteIntent,
            SubstituteIntent,
        }

        for (label, attack) in [
            ("delete-claim-before-mutation", Attack::DeleteClaim),
            ("substitute-claim-before-mutation", Attack::SubstituteClaim),
            ("delete-intent-before-mutation", Attack::DeleteIntent),
            (
                "substitute-intent-before-mutation",
                Attack::SubstituteIntent,
            ),
        ] {
            let fixture = Fixture::new(label);
            let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
            let before = fixture.snapshot_graph();
            let target = match attack {
                Attack::DeleteClaim | Attack::SubstituteClaim => {
                    fixture.store.root_path().join(STORE_CLAIM_FILE)
                }
                Attack::DeleteIntent | Attack::SubstituteIntent => fixture
                    .store
                    .root_path()
                    .join(INTENTS_DIR)
                    .join(intent_filename(fixture.intent.id().unwrap())),
            };
            MUTATION_AUTHORITY_CAPTURED_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || match attack {
                    Attack::DeleteClaim | Attack::DeleteIntent => fs::remove_file(target).unwrap(),
                    Attack::SubstituteClaim | Attack::SubstituteIntent => {
                        fs::remove_file(&target).unwrap();
                        fs::write(target, b"substituted durable authority").unwrap()
                    }
                }));
            });
            let mut authority = fixture
                .store
                .begin_mutation(&fixture.intent, Some(&reservation))
                .unwrap();

            assert!(fixture
                .graph
                .write_page_projection(
                    fixture.intent.path().as_str(),
                    None,
                    &fixture.target,
                    &mut authority,
                )
                .is_err());
            assert_eq!(fixture.snapshot_graph(), before);
        }
    }

    #[test]
    fn completion_substitution_after_graph_mutation_keeps_recovery_authority_and_resumes() {
        let fixture = Fixture::new("completion-after-mutation");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let proof = fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut authority,
            )
            .unwrap();
        let completions = fixture.store.root_path().join(COMPLETIONS_DIR);
        let moved = fixture.store.root_path().join("completions-moved-for-test");
        let completions_hook = completions.clone();
        let replacement_hook = completions.clone();
        let moved_hook = moved.clone();
        COMPLETION_PUBLICATION_ACT_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::rename(completions_hook, moved_hook).unwrap();
                fs::create_dir(replacement_hook).unwrap();
            }));
        });

        assert!(fixture
            .store
            .publish_completion(authority, &fixture.intent, &proof)
            .is_err());
        assert_eq!(
            fs::read(fixture.graph_root.join("pages/authority.md")).unwrap(),
            fixture.target
        );
        assert!(ProjectionReceiptStore::open(
            fixture.store.root_path(),
            fixture.store.workspace_id()
        )
        .is_err());

        fs::remove_dir(&completions).unwrap();
        fs::rename(&moved, &completions).unwrap();
        let reopened =
            ProjectionReceiptStore::open(fixture.store.root_path(), fixture.store.workspace_id())
                .unwrap();
        let mut recovery = reopened.begin_mutation(&fixture.intent, None).unwrap();
        let proof = fixture
            .graph
            .recover_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut recovery,
            )
            .unwrap();
        reopened
            .publish_completion(recovery, &fixture.intent, &proof)
            .unwrap();
        assert!(reopened.load_completion(&fixture.intent).unwrap().is_some());
    }

    #[test]
    fn root_authority_removal_before_completion_publication_preserves_recovery() {
        let fixture = Fixture::new("completion-root-authority");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let proof = fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut authority,
            )
            .unwrap();
        let root = fixture.store.root_path().to_path_buf();
        COMPLETION_PUBLICATION_ACT_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let authority = fs::read_dir(root)
                    .unwrap()
                    .map(Result::unwrap)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .ends_with(MUTATION_AUTHORITY_SUFFIX)
                    })
                    .expect("live mutation authority");
                fs::remove_file(authority.path()).unwrap();
            }));
        });
        assert!(fixture
            .store
            .publish_completion(authority, &fixture.intent, &proof)
            .is_err());
        assert_eq!(
            fs::read(fixture.graph_root.join("pages/authority.md")).unwrap(),
            fixture.target
        );

        let mut recovery = fixture.store.begin_mutation(&fixture.intent, None).unwrap();
        let proof = fixture
            .graph
            .recover_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut recovery,
            )
            .unwrap();
        fixture
            .store
            .publish_completion(recovery, &fixture.intent, &proof)
            .unwrap();
    }

    #[test]
    fn crashes_after_attempt_publication_reuse_one_byte_stable_reservation() {
        let fixture = Fixture::new("attempt-publication-crash");
        let mut stable = None;

        for _ in 0..16 {
            ATTEMPT_PUBLICATION_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(|| panic!("simulated process crash")));
            });
            let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = fixture.store.begin_mutation(&fixture.intent, None);
            }));
            assert!(crashed.is_err());
            assert_eq!(fixture.authority_stats(), (0, 0));
            assert_eq!(fixture.attempt_stats(&fixture.intent).0, 1);
            let snapshot = fixture.attempt_snapshot(&fixture.intent);
            if let Some(expected) = &stable {
                assert_eq!(&snapshot, expected);
            } else {
                stable = Some(snapshot);
            }
        }
    }

    #[test]
    fn fallback_reuses_the_turn_derived_attempt_instead_of_inventing_a_second_name() {
        let fresh = Fixture::new("fresh-turn-derived-fallback");
        let derived = Uuid::from_u128(0xf2f2_f2f2_f2f2_f2f2_f2f2_f2f2_f2f2_f2f2);
        let _turn = enter_projection_turn_attempt(derived);
        let fresh_fallback = fresh.store.reserve_fallback_attempt(&fresh.intent).unwrap();
        assert_eq!(fresh_fallback.attempt_id(), derived);

        let fixture = Fixture::new("fallback-attempt-publication-crash");
        let primary = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut recovery = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&primary))
            .unwrap();
        assert!(fixture
            .graph
            .recover_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut recovery,
            )
            .is_err());
        recovery.release_failed_recovery().unwrap();
        for _ in 0..8 {
            let fallback = fixture
                .store
                .reserve_fallback_attempt(&fixture.intent)
                .unwrap();
            assert_eq!(fallback, primary);
            assert_eq!(fixture.authority_stats(), (0, 0));
            assert_eq!(fixture.attempt_stats(&fixture.intent).0, 1);
        }
    }

    #[test]
    fn repeated_reservations_across_handles_return_one_exact_attempt() {
        let fixture = Fixture::new("attempt-reservation-reuse");
        let first = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let stable = fixture.attempt_snapshot(&fixture.intent);
        let reopened = fixture.reopen_store();
        let second = reopened.reserve_attempt(&fixture.intent).unwrap();
        let third = fixture.store.reserve_attempt(&fixture.intent).unwrap();

        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(fixture.attempt_stats(&fixture.intent).0, 1);
        assert_eq!(fixture.attempt_snapshot(&fixture.intent), stable);
    }

    #[test]
    fn repeated_pregraph_drops_keep_attempt_and_next_authority_bytes_stable() {
        let fixture = Fixture::new("pregraph-byte-stability");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let stable_attempts = fixture.attempt_snapshot(&fixture.intent);
        let authority_path = fixture.authority_path(&fixture.intent);
        let mut stable_authority = None;

        for iteration in 0..12 {
            let authority = if iteration % 2 == 0 {
                fixture
                    .store
                    .begin_mutation(&fixture.intent, Some(&reservation))
                    .unwrap()
            } else {
                fixture.store.begin_mutation(&fixture.intent, None).unwrap()
            };
            let bytes = fs::read(&authority_path).unwrap();
            if let Some(expected) = &stable_authority {
                assert_eq!(&bytes, expected);
            } else {
                stable_authority = Some(bytes);
            }
            drop(authority);
            assert!(!authority_path.exists());
            assert_eq!(fixture.attempt_snapshot(&fixture.intent), stable_attempts);
        }
    }

    #[test]
    fn interrupted_recoveries_reuse_one_exact_authority_slot() {
        let fixture = Fixture::new("bounded-interrupted-recovery");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut authority,
            )
            .unwrap();
        drop(authority);

        let authority_path = fixture.authority_path(&fixture.intent);
        let witness = fs::read(&authority_path).unwrap();
        let stable_stats = fixture.authority_stats();
        let stable_attempt_stats = fixture.attempt_stats(&fixture.intent);
        assert_eq!(stable_stats, (1, witness.len() as u64));

        let reopened =
            ProjectionReceiptStore::open(fixture.store.root_path(), fixture.store.workspace_id())
                .unwrap();
        let recovery = reopened.begin_mutation(&fixture.intent, None).unwrap();
        drop(recovery);
        assert_eq!(
            fs::read(&authority_path).unwrap(),
            witness,
            "dropping a reopened pre-graph recovery must retain the sole witness"
        );

        for _ in 0..3 {
            let reopened = ProjectionReceiptStore::open(
                fixture.store.root_path(),
                fixture.store.workspace_id(),
            )
            .unwrap();
            let mut recovery = reopened.begin_mutation(&fixture.intent, None).unwrap();
            fixture
                .graph
                .recover_page_projection(
                    fixture.intent.path().as_str(),
                    None,
                    &fixture.target,
                    &mut recovery,
                )
                .unwrap();
            drop(recovery);
            assert_eq!(fixture.authority_stats(), stable_stats);
            assert_eq!(fixture.attempt_stats(&fixture.intent), stable_attempt_stats);
            assert_eq!(fs::read(&authority_path).unwrap(), witness);
        }
    }

    #[test]
    fn pending_recovery_reopens_same_attempt_and_blocks_new_reservation() {
        let fixture = Fixture::new("pending-recovery-blocks-active");
        let active = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let same = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        assert_eq!(same, active);
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&active))
            .unwrap();
        fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut authority,
            )
            .unwrap();
        drop(authority);

        let authority_path = fixture.authority_path(&fixture.intent);
        let witness = fs::read(&authority_path).unwrap();
        let attempts_path = fixture
            .store
            .root_path()
            .join(ATTEMPTS_DIR)
            .join(hex(fixture.intent.id().unwrap().as_bytes()));
        let attempt_count = fs::read_dir(&attempts_path).unwrap().count();
        let reopened = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&same))
            .unwrap();
        drop(reopened);
        assert!(matches!(
            fixture.store.reserve_attempt(&fixture.intent),
            Err(ProjectionStoreError::MutationAuthorityPending)
        ));
        assert_eq!(fs::read_dir(attempts_path).unwrap().count(), attempt_count);
        assert_eq!(fs::read(authority_path).unwrap(), witness);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn begin_lease_blocks_competing_reservation_until_release() {
        let fixture = Fixture::new("begin-lease-blocks-reservation");
        let competing_root = fixture.store.root_path().to_path_buf();
        let workspace_id = fixture.store.workspace_id();
        let competing_intent = fixture.intent.clone();
        MUTATION_AUTHORITY_LEASED_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let competing =
                    ProjectionReceiptStore::open(&competing_root, workspace_id).unwrap();
                assert!(matches!(
                    competing.reserve_attempt(&competing_intent),
                    Err(ProjectionStoreError::MutationAuthorityPending)
                ));
            }));
        });

        let authority = fixture.store.begin_mutation(&fixture.intent, None).unwrap();
        drop(authority);

        fixture.store.reserve_attempt(&fixture.intent).unwrap();
        assert_eq!(
            fs::read(
                fixture
                    .store
                    .root_path()
                    .join(mutation_authority_lease_filename(
                        fixture.intent.id().unwrap()
                    ))
            )
            .unwrap(),
            Vec::<u8>::new()
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn reservation_lease_blocks_competing_begin_until_release() {
        let fixture = Fixture::new("reservation-lease-blocks-begin");
        let competing_root = fixture.store.root_path().to_path_buf();
        let workspace_id = fixture.store.workspace_id();
        let competing_intent = fixture.intent.clone();
        MUTATION_AUTHORITY_LEASED_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let competing =
                    ProjectionReceiptStore::open(&competing_root, workspace_id).unwrap();
                assert!(matches!(
                    competing.begin_mutation(&competing_intent, None),
                    Err(ProjectionStoreError::MutationAuthorityPending)
                ));
            }));
        });

        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let retry = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        drop(retry);
    }

    #[cfg(unix)]
    #[test]
    fn mutation_lease_rejects_a_final_component_symlink() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("mutation-lease-symlink");
        let target = fixture.root.join("lease-target");
        fs::write(&target, b"sentinel").unwrap();
        symlink(
            &target,
            fixture
                .store
                .root_path()
                .join(mutation_authority_lease_filename(
                    fixture.intent.id().unwrap(),
                )),
        )
        .unwrap();

        assert!(fixture.store.reserve_attempt(&fixture.intent).is_err());
        assert_eq!(fs::read(target).unwrap(), b"sentinel");
        assert_eq!(fixture.attempt_stats(&fixture.intent), (0, 0));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn recovery_reopen_contends_across_handles_and_retries_after_release() {
        let fixture = Fixture::new("recovery-reopen-lease");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut creator = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut creator,
            )
            .unwrap();
        drop(creator);

        let reopened = fixture.reopen_store();
        let recovery = reopened.begin_mutation(&fixture.intent, None).unwrap();
        assert!(matches!(
            fixture.store.begin_mutation(&fixture.intent, None),
            Err(ProjectionStoreError::MutationAuthorityPending)
        ));
        assert!(matches!(
            fixture.store.reserve_attempt(&fixture.intent),
            Err(ProjectionStoreError::MutationAuthorityPending)
        ));

        drop(recovery);
        let retry = fixture.store.begin_mutation(&fixture.intent, None).unwrap();
        drop(retry);
        assert!(fixture.authority_path(&fixture.intent).exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn creator_drop_serializes_slot_removal_before_reopen() {
        let fixture = Fixture::new("creator-drop-lease");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let creator = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let authority_path = fixture.authority_path(&fixture.intent);
        assert!(authority_path.exists());

        let competing_root = fixture.store.root_path().to_path_buf();
        let workspace_id = fixture.store.workspace_id();
        let competing_intent = fixture.intent.clone();
        let competing_reservation = reservation.clone();
        MUTATION_AUTHORITY_DROP_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let competing =
                    ProjectionReceiptStore::open(&competing_root, workspace_id).unwrap();
                assert!(matches!(
                    competing.begin_mutation(&competing_intent, Some(&competing_reservation)),
                    Err(ProjectionStoreError::MutationAuthorityPending)
                ));
            }));
        });

        drop(creator);
        assert!(!authority_path.exists());
        let reopened = fixture.reopen_store();
        let second = reopened
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        assert!(authority_path.exists());
        drop(second);
        assert!(!authority_path.exists());
    }

    #[test]
    fn completion_removes_only_its_matching_authority_slot() {
        let fixture = Fixture::new("matching-authority-retirement");
        let second_target = b"- second target\n".to_vec();
        let second_intent = ProjectionIntent::new(
            fixture.store.workspace_id(),
            PageId::from_uuid(Uuid::from_u128(3)),
            ManagedPath::parse("pages/second-authority.md").unwrap(),
            FrontierV2::default(),
            Vec::new(),
            ProjectionPrecondition::Absent,
            crate::oplog::ProjectionTargetKind::Present,
            BlobDescription::of(&second_target),
            Vec::new(),
        )
        .unwrap();
        fixture.store.publish_intent(&second_intent, None).unwrap();

        for (intent, target) in [
            (&fixture.intent, fixture.target.as_slice()),
            (&second_intent, second_target.as_slice()),
        ] {
            let reservation = fixture.store.reserve_attempt(intent).unwrap();
            let mut authority = fixture
                .store
                .begin_mutation(intent, Some(&reservation))
                .unwrap();
            fixture
                .graph
                .write_page_projection(intent.path().as_str(), None, target, &mut authority)
                .unwrap();
            drop(authority);
        }

        let first_path = fixture.authority_path(&fixture.intent);
        let second_path = fixture.authority_path(&second_intent);
        let second_witness = fs::read(&second_path).unwrap();
        let mut recovery = fixture.store.begin_mutation(&fixture.intent, None).unwrap();
        let proof = fixture
            .graph
            .recover_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut recovery,
            )
            .unwrap();
        fixture
            .store
            .publish_completion(recovery, &fixture.intent, &proof)
            .unwrap();

        assert!(!first_path.exists());
        assert_eq!(fs::read(second_path).unwrap(), second_witness);
        assert_eq!(fixture.authority_stats(), (1, second_witness.len() as u64));
    }

    #[test]
    fn load_completion_reconciles_a_crash_retained_slot_idempotently() {
        let fixture = Fixture::new("completion-load-reconciliation");
        let expected = fixture.leave_durable_completion_with_slot();
        let authority_path = fixture.authority_path(&fixture.intent);
        let reopened = fixture.reopen_store();

        assert_eq!(
            reopened.load_completion(&fixture.intent).unwrap(),
            Some(expected.clone())
        );
        assert!(!authority_path.exists());
        assert_eq!(
            reopened.load_completion(&fixture.intent).unwrap(),
            Some(expected)
        );
        assert!(!authority_path.exists());
    }

    #[test]
    fn catalog_paths_reconcile_one_exact_completed_slot_per_row() {
        let incomplete_fixture = Fixture::new("completion-incomplete-catalog-reconciliation");
        incomplete_fixture.leave_durable_completion_with_slot();
        assert!(incomplete_fixture
            .store
            .incomplete_intents()
            .unwrap()
            .is_empty());
        assert!(!incomplete_fixture
            .authority_path(&incomplete_fixture.intent)
            .exists());

        let validated_fixture = Fixture::new("completion-validated-catalog-reconciliation");
        let expected = validated_fixture.leave_durable_completion_with_slot();
        let catalog = validated_fixture.store.validated_catalog().unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].completion, Some(expected));
        assert!(!validated_fixture
            .authority_path(&validated_fixture.intent)
            .exists());
    }

    #[test]
    fn completed_slot_bound_to_another_intent_fails_closed_without_removal() {
        let fixture = Fixture::new("completion-slot-intent-mismatch");
        fixture.leave_durable_completion_with_slot();
        let first_path = fixture.authority_path(&fixture.intent);

        let second_target = b"- second target\n".to_vec();
        let second_intent = ProjectionIntent::new(
            fixture.store.workspace_id(),
            PageId::from_uuid(Uuid::from_u128(3)),
            ManagedPath::parse("pages/second-authority.md").unwrap(),
            FrontierV2::default(),
            Vec::new(),
            ProjectionPrecondition::Absent,
            crate::oplog::ProjectionTargetKind::Present,
            BlobDescription::of(&second_target),
            Vec::new(),
        )
        .unwrap();
        fixture.store.publish_intent(&second_intent, None).unwrap();
        let second_reservation = fixture.store.reserve_attempt(&second_intent).unwrap();
        let second_authority = fixture
            .store
            .begin_mutation(&second_intent, Some(&second_reservation))
            .unwrap();
        let second_path = fixture.authority_path(&second_intent);
        let mismatched_bytes = fs::read(&second_path).unwrap();
        fs::write(&first_path, &mismatched_bytes).unwrap();

        assert!(matches!(
            fixture.store.load_completion(&fixture.intent),
            Err(ProjectionStoreError::MutationAuthorityMismatch)
        ));
        assert_eq!(fs::read(&first_path).unwrap(), mismatched_bytes);
        assert!(second_path.exists());
        drop(second_authority);
    }

    #[cfg(unix)]
    #[test]
    fn completed_hardlinked_slot_fails_closed_without_removal() {
        let fixture = Fixture::new("completion-slot-hardlink");
        fixture.leave_durable_completion_with_slot();
        let authority_path = fixture.authority_path(&fixture.intent);
        let extra_link = fixture.root.join("authority-extra-link");
        fs::hard_link(&authority_path, &extra_link).unwrap();

        assert!(matches!(
            fixture.store.load_completion(&fixture.intent),
            Err(ProjectionStoreError::UnsafeEntry(_))
        ));
        assert!(authority_path.exists());
        assert!(extra_link.exists());
    }

    #[test]
    fn pre_graph_drop_frees_only_a_new_authority_slot() {
        let fixture = Fixture::new("pre-graph-authority-drop");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let authority_path = fixture.authority_path(&fixture.intent);

        let authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        assert!(authority_path.exists());
        drop(authority);
        assert!(!authority_path.exists());

        let recovery = fixture.store.begin_mutation(&fixture.intent, None).unwrap();
        assert!(authority_path.exists());
        drop(recovery);
        assert!(!authority_path.exists());
    }

    #[test]
    fn a_turn_crashed_under_2a_replays_under_2b_without_refusal() {
        let fixture = Fixture::new_replacement("legacy-attempt-turn-continuation");

        // Packet 2a reserved from receipt-store identity. There is deliberately
        // no turn scope around this call, which preserves that old producer in
        // test builds for compatibility fixtures.
        let legacy = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut interrupted = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&legacy))
            .unwrap();
        fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                Some(b"- base\n"),
                &fixture.target,
                &mut interrupted,
            )
            .unwrap();
        drop(interrupted);
        assert!(fixture.authority_path(&fixture.intent).exists());

        // Packet 2b derives a different fresh id from the replay turn. Durable
        // residue is stronger evidence: it resumes the recorded attempt rather
        // than refusing or manufacturing a parallel attempt.
        let derived = Uuid::from_u128(0x2b2b_2b2b_2b2b_2b2b_2b2b_2b2b_2b2b_2b2b);
        assert_ne!(legacy.attempt_id(), derived);
        let reopened = fixture.reopen_store();
        let _turn = enter_projection_turn_attempt(derived);
        let resumed = reopened.begin_mutation(&fixture.intent, None).unwrap();
        assert_eq!(
            resumed.active.as_ref().map(|attempt| attempt.attempt_id()),
            Some(legacy.attempt_id())
        );
        assert_eq!(resumed.reservations.len(), 1);
        assert_eq!(fixture.attempt_stats(&fixture.intent).0, 1);
    }

    #[test]
    fn completed_mutation_authorities_do_not_accumulate_at_store_root() {
        let fixture = Fixture::new("completed-authority-lifecycle");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let proof = fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut authority,
            )
            .unwrap();
        fixture
            .store
            .publish_completion(authority, &fixture.intent, &proof)
            .unwrap();

        let mut interrupted = fixture.store.begin_mutation(&fixture.intent, None).unwrap();
        fixture
            .graph
            .recover_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut interrupted,
            )
            .unwrap();
        drop(interrupted);
        assert_eq!(
            fs::read_dir(fixture.store.root_path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(MUTATION_AUTHORITY_SUFFIX))
                .count(),
            1,
            "post-graph interruption must retain recovery authority"
        );

        for _ in 0..3 {
            let mut recovery = fixture.store.begin_mutation(&fixture.intent, None).unwrap();
            let proof = fixture
                .graph
                .recover_page_projection(
                    fixture.intent.path().as_str(),
                    None,
                    &fixture.target,
                    &mut recovery,
                )
                .unwrap();
            fixture
                .store
                .publish_completion(recovery, &fixture.intent, &proof)
                .unwrap();
            assert_eq!(
                fs::read_dir(fixture.store.root_path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with(MUTATION_AUTHORITY_SUFFIX))
                    .count(),
                0
            );
        }
    }

    #[test]
    fn registered_own_endpoint_residue_is_inert_and_remains_byte_identical() {
        let fixture = Fixture::new_replacement("registered-own-residue");
        let (_, _, records) = fixture.complete_replacement();
        assert_eq!(records.len(), 1);
        assert_eq!(fixture.store.validated_catalog().unwrap().len(), 1);
        let before_store = fixture.snapshot_store();
        let before_graph = fixture.snapshot_graph();

        let own = [fixture.intent.id().unwrap()].into_iter().collect();
        let reported = fixture.store.retired_own_endpoint_artifacts(&own);
        assert!(reported.iter().any(|path| path.ends_with(".intent")));
        assert!(reported.iter().any(|path| path.ends_with(".completion")));
        assert!(reported
            .iter()
            .any(|path| path.ends_with(PENDING_CLEANUP_SUFFIX)));
        assert!(fixture
            .store
            .load_completion(&fixture.intent)
            .unwrap()
            .is_none());
        assert!(fixture.store.validated_catalog().unwrap().is_empty());
        assert!(fixture
            .store
            .pending_projection_cleanup_bounded(MAX_PENDING_PROJECTION_CLEANUP_PER_PASS)
            .unwrap()
            .is_empty());
        assert_eq!(fixture.snapshot_store(), before_store);
        assert_eq!(fixture.snapshot_graph(), before_graph);
    }

    #[test]
    fn completed_recovery_retirement_preserves_same_byte_replacement() {
        let fixture = Fixture::new_replacement("completed-recovery-retirement-binding");
        let base = b"- base\n";
        let (_, recovery_path, _) = fixture.complete_replacement();
        let displaced = recovery_path.with_extension("provider-retained");
        fs::rename(&recovery_path, &displaced).unwrap();
        fs::write(&recovery_path, base).unwrap();
        let reopened =
            ProjectionReceiptStore::open(fixture.store.root_path(), fixture.store.workspace_id())
                .unwrap();
        let records = reopened.local_forensic_evidence(&fixture.intent).unwrap();
        let conflict = fixture
            .graph
            .retire_completed_projection_recovery(fixture.intent.path().as_str(), &records)
            .unwrap();
        let ProjectionRecoveryCleanup::ConflictRetained { relative_path } = conflict else {
            panic!("same-byte replacement did not become a recoverable conflict: {conflict:?}");
        };
        assert!(!recovery_path.exists());
        assert_eq!(
            fs::read(fixture.graph_root.join(relative_path)).unwrap(),
            base
        );
        assert_eq!(fs::read(&displaced).unwrap(), base);
    }

    /// §4 row C3: a pre-existing derived staged name is data, not scratch.
    /// The writer moves it intact to strict conflict trash before recreating
    /// the exact turn-derived name for its own bytes.
    #[test]
    fn a_staged_name_occupied_after_crash_is_quarantined_not_deleted() {
        let fixture = Fixture::new_replacement("occupied-derived-staged-name");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let staged_name = format!(
            ".{}.{}.projection.staged",
            fixture.intent.path().file_name(),
            reservation.attempt_id().simple()
        );
        let staged_path = fixture
            .graph_root
            .join(fixture.intent.path().as_str())
            .parent()
            .unwrap()
            .join(&staged_name);
        let unknown = b"- pre-existing staged-name bytes\n";
        fs::write(&staged_path, unknown).unwrap();

        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let proof = fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                Some(b"- base\n"),
                &fixture.target,
                &mut authority,
            )
            .unwrap();
        fixture
            .store
            .publish_completion(authority, &fixture.intent, &proof)
            .unwrap();

        assert!(!staged_path.exists());
        assert_eq!(
            fs::read(fixture.graph_root.join(fixture.intent.path().as_str())).unwrap(),
            fixture.target
        );
        let quarantined = fixture
            .snapshot_graph()
            .into_iter()
            .filter_map(|(path, bytes)| bytes.map(|bytes| (path, bytes)))
            .find(|(path, bytes)| {
                path.to_string_lossy().contains("projection-residue") && bytes == unknown
            });
        assert!(
            quarantined.is_some(),
            "the occupied derived name must survive byte-identically in conflict trash"
        );
    }

    #[test]
    fn turn_replay_after_publication_retires_the_displaced_pre_image() {
        let fixture = Fixture::new_replacement("in-turn-exact-recovery-retirement");
        let (_reservation, recovery_path, records) = fixture.complete_replacement();

        assert_eq!(
            fixture
                .graph
                .retire_completed_projection_recovery(fixture.intent.path().as_str(), &records,)
                .unwrap(),
            ProjectionRecoveryCleanup::Retired
        );
        assert!(!recovery_path.exists());
    }

    #[test]
    fn a_post_crash_recovery_file_is_trashed_not_unlinked() {
        let fixture = Fixture::new_replacement("post-crash-recovery-retention");
        let (_reservation, recovery_path, records) = fixture.complete_replacement();
        let graph_root = fixture.graph_root.clone();
        let target_path = fixture.intent.path().to_owned();
        let records_for_restart = records.clone();

        // A new thread has no process-local in-turn unlink authority, which is
        // the exact capability loss a process crash creates.
        let cleanup = std::thread::spawn(move || {
            Graph::open(&graph_root)
                .retire_completed_projection_recovery(target_path.as_str(), &records_for_restart)
                .unwrap()
        })
        .join()
        .unwrap();
        let ProjectionRecoveryCleanup::ConflictRetained { relative_path } = cleanup else {
            panic!("post-crash cleanup discarded retained bytes: {cleanup:?}");
        };
        assert!(!recovery_path.exists());
        assert_eq!(
            fs::read(fixture.graph_root.join(relative_path)).unwrap(),
            b"- base\n"
        );
    }

    #[test]
    fn an_externally_substituted_recovery_inode_is_retained() {
        let fixture = Fixture::new_replacement("externally-substituted-recovery");
        let (_reservation, recovery_path, records) = fixture.complete_replacement();
        let original = recovery_path.with_extension("original-provider-inode");
        fs::rename(&recovery_path, &original).unwrap();
        let substituted = b"- external substitute\n";
        fs::write(&recovery_path, substituted).unwrap();
        let graph_root = fixture.graph_root.clone();
        let target_path = fixture.intent.path().to_owned();

        let cleanup = std::thread::spawn(move || {
            Graph::open(&graph_root)
                .retire_completed_projection_recovery(target_path.as_str(), &records)
                .unwrap()
        })
        .join()
        .unwrap();
        let ProjectionRecoveryCleanup::ConflictRetained { relative_path } = cleanup else {
            panic!("substituted recovery was not retained: {cleanup:?}");
        };
        assert_eq!(fs::read(original).unwrap(), b"- base\n");
        assert_eq!(
            fs::read(fixture.graph_root.join(relative_path)).unwrap(),
            substituted
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_crash_hardlinked_recovery_refuses_and_keeps_durable_residue() {
        let fixture = Fixture::new_replacement("post-crash-hardlinked-recovery");
        let (_reservation, recovery_path, records) = fixture.complete_replacement();
        let extra_link = recovery_path.with_extension("linked-recovery-copy");
        fs::hard_link(&recovery_path, &extra_link).unwrap();
        let graph_root = fixture.graph_root.clone();
        let target_path = fixture.intent.path().to_owned();
        let records_for_restart = records.clone();

        let refusal = std::thread::spawn(move || {
            Graph::open(&graph_root)
                .retire_completed_projection_recovery(target_path.as_str(), &records_for_restart)
                .unwrap_err()
        })
        .join()
        .unwrap();
        assert_eq!(refusal.kind(), io::ErrorKind::AlreadyExists, "{refusal}");
        assert_eq!(fs::read(&recovery_path).unwrap(), b"- base\n");
        assert_eq!(fs::read(&extra_link).unwrap(), b"- base\n");
        assert_eq!(
            fixture.store.pending_projection_cleanup().unwrap().len(),
            1,
            "the durable receipt remains the residue record for a refused quarantine"
        );
    }

    /// §4.6 / §4.2 row C4: the W2 displacement fault point produces
    /// "displaced, not yet published" — the live name gone, the derived
    /// recovery name holding the exact precondition, and nothing staged.
    ///
    /// This cut did not exist before packet 1:
    /// `projection_recovery_after_bound_capture_hook` fires BEFORE the
    /// displacement rename, so it cannot reach this state at all.
    #[test]
    fn turn_replay_after_displacement_republishes_and_retires() {
        let fixture = Fixture::new_replacement("projection-after-displacement-hook");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let target_path = fixture.graph_root.join(fixture.intent.path().as_str());
        let parent = target_path.parent().unwrap().to_path_buf();
        let recovery_path = parent.join(reservation.recovery_filename());
        let staged_path = parent.join(format!(
            ".{}.{}.projection.staged",
            fixture.intent.path().file_name(),
            reservation.attempt_id().simple()
        ));

        type Observation = (bool, bool, Vec<u8>, bool, Vec<u8>);
        let observed: std::sync::Arc<std::sync::Mutex<Option<Observation>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let recorder = std::sync::Arc::clone(&observed);
        let observed_target = target_path.clone();
        let observed_recovery = recovery_path.clone();
        let observed_staged = staged_path.clone();
        crate::model::set_projection_after_displacement_hook_for_test(
            target_path.clone(),
            move || {
                *recorder.lock().unwrap() = Some((
                    observed_target.exists(),
                    observed_recovery.exists(),
                    fs::read(&observed_recovery).unwrap_or_default(),
                    observed_staged.exists(),
                    fs::read(&observed_staged).unwrap_or_default(),
                ));
                panic!("simulated process crash after displacement")
            },
        );

        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fixture.graph.write_page_projection(
                fixture.intent.path().as_str(),
                Some(b"- base\n"),
                &fixture.target,
                &mut authority,
            );
        }));
        assert!(crashed.is_err());
        drop(authority);

        let (target_present, recovery_present, recovery_bytes, staged_present, staged_bytes) =
            observed
                .lock()
                .unwrap()
                .take()
                .expect("the projection displacement hook must fire");
        assert!(
            !target_present,
            "the live name must already be gone at the displacement cut"
        );
        assert!(recovery_present, "the displaced pre-image must be retained");
        assert_eq!(recovery_bytes, b"- base\n".to_vec());
        assert!(
            staged_present,
            "the turn-derived staged name must survive the cut"
        );
        assert_eq!(staged_bytes, fixture.target);

        let graph_root = fixture.graph_root.clone();
        let receipt_root = fixture.store.root_path().to_path_buf();
        let workspace_id = fixture.store.workspace_id();
        let intent = fixture.intent.clone();
        let target = fixture.target.clone();
        std::thread::spawn(move || {
            let graph = Graph::open(&graph_root);
            let store = ProjectionReceiptStore::open(&receipt_root, workspace_id).unwrap();
            let mut resumed = store.begin_mutation(&intent, None).unwrap();
            let proof = graph
                .write_page_projection(
                    intent.path().as_str(),
                    Some(b"- base\n"),
                    &target,
                    &mut resumed,
                )
                .unwrap();
            store.publish_completion(resumed, &intent, &proof).unwrap();
            let records = store.local_forensic_evidence(&intent).unwrap();
            assert_eq!(
                records.len(),
                1,
                "resumed retirement must publish bound evidence"
            );
            assert!(matches!(
                graph
                    .retire_completed_projection_recovery(intent.path().as_str(), &records)
                    .unwrap(),
                ProjectionRecoveryCleanup::ConflictRetained { .. }
            ));
            store
                .retire_pending_projection_cleanup(&records[0])
                .unwrap();
        })
        .join()
        .unwrap();

        assert_eq!(fs::read(&target_path).unwrap(), fixture.target);
        assert!(!recovery_path.exists());
        assert!(!staged_path.exists());
        assert!(fixture
            .reopen_store()
            .pending_projection_cleanup()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_resumed_retirement_still_retires_its_recovery_file() {
        turn_replay_after_displacement_republishes_and_retires();
    }

    #[test]
    fn a_lost_directory_entry_after_the_single_barrier_converges() {
        let fixture = Fixture::new_replacement("lost-single-turn-barrier");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let recovery_path = fixture
            .graph_root
            .join(fixture.intent.path().as_str())
            .parent()
            .unwrap()
            .join(reservation.recovery_filename());
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let turn = crate::model::ProjectionTurnBarrierScope::begin().unwrap();
        let proof = fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                Some(b"- base\n"),
                &fixture.target,
                &mut authority,
            )
            .unwrap();
        fixture
            .store
            .publish_completion(authority, &fixture.intent, &proof)
            .unwrap();
        super::super::projection::retire_pending_projection_recovery(
            &fixture.graph,
            &fixture.store,
            None,
        )
        .unwrap();
        crate::model::fail_next_projection_directory_sync_for_test();
        assert!(
            turn.finish().is_err(),
            "the simulated final barrier must fail"
        );

        let replay = crate::model::ProjectionTurnBarrierScope::begin().unwrap();
        fixture
            .graph
            .rebarrier_page_projection(fixture.intent.path(), Some(fixture.target.as_slice()))
            .unwrap();
        replay.finish().unwrap();

        assert_eq!(
            fs::read(fixture.graph_root.join(fixture.intent.path().as_str())).unwrap(),
            fixture.target
        );
        assert!(!recovery_path.exists());
        assert!(fixture
            .snapshot_graph()
            .keys()
            .all(|path| !path.to_string_lossy().ends_with(".projection.staged")));
    }

    #[test]
    fn pre_evidence_publication_same_byte_rebind_never_gains_cleanup_authority() {
        let fixture = Fixture::new_replacement("pre-evidence-same-byte-rebind");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let recovery_path = fixture
            .graph_root
            .join(fixture.intent.path().as_str())
            .parent()
            .unwrap()
            .join(reservation.recovery_filename());
        let original = recovery_path.with_extension("original-displaced");
        let raced_recovery = recovery_path.clone();
        let raced_original = original.clone();
        crate::model::set_projection_after_retire_collision_hook_for_test(move || {
            fs::rename(&raced_recovery, &raced_original)?;
            fs::write(&raced_recovery, b"- base\n")
        });
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        assert!(fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                Some(b"- base\n"),
                &fixture.target,
                &mut authority,
            )
            .is_err());
        drop(authority);

        let (_, marker) = fixture
            .store
            .pending_projection_cleanup()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(matches!(
            fixture.graph.retire_completed_projection_recovery(
                fixture.intent.path().as_str(),
                std::slice::from_ref(&marker),
            ),
            Ok(ProjectionRecoveryCleanup::ConflictRetained { .. })
        ));
        assert!(!recovery_path.exists());
        assert_eq!(fs::read(original).unwrap(), b"- base\n");
    }

    #[test]
    fn same_byte_rebind_after_handle_capture_never_publishes_cleanup_authority() {
        let fixture = Fixture::new_replacement("same-byte-rebind-after-handle-capture");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let target_path = fixture.graph_root.join(fixture.intent.path().as_str());
        let original = target_path.with_extension("original-provider-inode");
        let raced_target = target_path.clone();
        let raced_original = original.clone();
        crate::model::set_projection_recovery_after_bound_capture_hook_for_test(
            target_path.clone(),
            move || {
                fs::rename(&raced_target, &raced_original)?;
                fs::write(&raced_target, b"- base\n")
            },
        );
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        assert!(fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                Some(b"- base\n"),
                &fixture.target,
                &mut authority,
            )
            .is_err());
        drop(authority);

        let recovery_path = target_path
            .parent()
            .unwrap()
            .join(reservation.recovery_filename());
        assert_eq!(fs::read(original).unwrap(), b"- base\n");
        assert_eq!(fs::read(recovery_path).unwrap(), b"- base\n");
        assert!(
            fixture
                .store
                .pending_projection_cleanup()
                .unwrap()
                .is_empty(),
            "same-byte replacement was promoted into cleanup authority"
        );
    }

    #[test]
    fn canonical_v1_forensic_evidence_decodes_but_never_authorizes_cleanup() {
        let fixture = Fixture::new_replacement("v1-forensic-compatibility");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let record = LocalProjectionEvidenceRecord {
            schema_version: PRIOR_LOCAL_FORENSIC_SCHEMA_VERSION,
            intent_id: fixture.intent.id().unwrap(),
            attempt_id: reservation.attempt_id(),
            target_path: fixture.intent.path().clone(),
            recovery_relative_path: fixture
                .intent
                .path()
                .join_sibling(reservation.recovery_filename())
                .unwrap(),
            recovery_filename: reservation.recovery_filename().to_owned(),
            recovery_resource_id: None,
            observed: BlobDescription::of(b"- base\n"),
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(
            !std::str::from_utf8(&bytes)
                .unwrap()
                .contains("recovery_resource_id"),
            "v1 canonical bytes must retain their historical field set"
        );
        let forensics = fixture
            .store
            .required_intent_namespace(FORENSICS_DIR, fixture.intent.id().unwrap())
            .unwrap();
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        publish_immutable_exact(
            &forensics,
            &format!("{}.evidence", hex(&digest)),
            &bytes,
            "v1 compatibility evidence",
        )
        .unwrap();

        let loaded = fixture
            .store
            .local_forensic_evidence(&fixture.intent)
            .unwrap();
        assert_eq!(loaded, vec![record]);
        assert!(!loaded[0].is_cleanup_bound());
        assert!(fixture
            .store
            .pending_projection_cleanup()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pending_cleanup_namespace_substitution_denies_before_graph_mutation() {
        let fixture = Fixture::new_replacement("pending-cleanup-namespace-substitution");
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let pending = fixture
            .store
            .root_path()
            .join(FORENSICS_DIR)
            .join(PENDING_CLEANUP_DIR);
        let retained = pending.with_extension("retained");
        fs::rename(&pending, &retained).unwrap();
        fs::create_dir(&pending).unwrap();

        assert!(fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                Some(b"- base\n"),
                &fixture.target,
                &mut authority,
            )
            .is_err());
        assert_eq!(
            fs::read(fixture.graph_root.join(fixture.intent.path().as_str())).unwrap(),
            b"- base\n"
        );
        let retained_entries = fs::read_dir(retained)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            retained_entries,
            [
                std::ffi::OsString::from(PENDING_CLEANUP_ROUND_STATE),
                std::ffi::OsString::from(PENDING_CLEANUP_ROUND_DIRS[0]),
                std::ffi::OsString::from(PENDING_CLEANUP_ROUND_DIRS[1]),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn pending_cleanup_round_state_and_identity_fail_closed() {
        let interrupted = Fixture::new("pending-cleanup-round-state-interrupted-swap");
        let interrupted_root = interrupted
            .store
            .root_path()
            .join(FORENSICS_DIR)
            .join(PENDING_CLEANUP_DIR);
        let temp = interrupted_root.join(format!(".tmp-{}", Uuid::new_v4()));
        fs::write(&temp, b"incomplete cursor replacement").unwrap();
        let reopened = interrupted.reopen_store();
        assert!(!temp.exists());
        let queue = open_pending_cleanup_rounds(
            &reopened.namespaces.pending_cleanup.capability,
            reopened.store_id,
            reopened.namespaces.pending_cleanup.identity,
        )
        .unwrap();
        assert_eq!(queue.state.active_round, 0);

        let deleted = Fixture::new("pending-cleanup-round-state-deleted");
        let deleted_root = deleted
            .store
            .root_path()
            .join(FORENSICS_DIR)
            .join(PENDING_CLEANUP_DIR);
        fs::remove_file(deleted_root.join(PENDING_CLEANUP_ROUND_STATE)).unwrap();
        assert!(matches!(
            deleted
                .store
                .pending_projection_cleanup_bounded(1)
                .unwrap_err(),
            ProjectionStoreError::NamespaceSubstitution(_)
        ));
        assert!(ProjectionReceiptStore::open(
            deleted.store.root_path(),
            deleted.store.workspace_id()
        )
        .is_err());

        let substituted = Fixture::new("pending-cleanup-round-identity-substituted");
        let substituted_root = substituted
            .store
            .root_path()
            .join(FORENSICS_DIR)
            .join(PENDING_CLEANUP_DIR);
        let round = substituted_root.join(PENDING_CLEANUP_ROUND_DIRS[0]);
        let retained = substituted_root
            .parent()
            .unwrap()
            .join("retained-pending-cleanup-round-0");
        fs::rename(&round, &retained).unwrap();
        fs::create_dir(&round).unwrap();
        assert!(matches!(
            substituted
                .store
                .pending_projection_cleanup_bounded(1)
                .unwrap_err(),
            ProjectionStoreError::NamespaceSubstitution(_)
        ));
        assert!(ProjectionReceiptStore::open(
            substituted.store.root_path(),
            substituted.store.workspace_id()
        )
        .is_err());
    }

    #[test]
    fn pending_cleanup_round_temp_is_bounded_and_unicode_unknown_fails_closed() {
        let fixture = Fixture::new("pending-cleanup-round-temp-unicode");
        let round = fixture
            .store
            .root_path()
            .join(FORENSICS_DIR)
            .join(PENDING_CLEANUP_DIR)
            .join(PENDING_CLEANUP_ROUND_DIRS[0]);
        let temp = round.join(format!(".tmp-{}", Uuid::new_v4()));
        fs::write(&temp, b"incomplete replacement").unwrap();
        reset_projection_store_test_counters();
        assert!(fixture
            .store
            .pending_projection_cleanup_bounded(1)
            .unwrap()
            .is_empty());
        assert_eq!(projection_store_test_counters().pending_cleanup_entries, 1);
        assert!(!temp.exists());

        let unknown = round.join("未認証.projection-cleanup");
        fs::write(&unknown, b"not a canonical marker").unwrap();
        reset_projection_store_test_counters();
        assert!(fixture.store.pending_projection_cleanup_bounded(1).is_err());
        assert_eq!(projection_store_test_counters().pending_cleanup_entries, 1);
        assert_eq!(fs::read(unknown).unwrap(), b"not a canonical marker");
    }

    /// The durable round flip is real work when the inactive round retains
    /// markers, and pure cost when it does not. Every accepted save enters
    /// this function twice with an entirely empty queue, so the both-empty
    /// case must write nothing and take no barrier — while the
    /// inactive-non-empty case must still flip and drain.
    #[test]
    fn an_empty_pending_cleanup_queue_elides_the_durable_round_flip() {
        let fixture = Fixture::new("pending-cleanup-empty-queue-elides-flip");
        let read_state = |store: &ProjectionReceiptStore| {
            let namespace = &store.namespaces.pending_cleanup;
            let queue = open_pending_cleanup_rounds(
                &namespace.capability,
                store.store_id,
                namespace.identity,
            )
            .unwrap();
            (queue.state.active_round, queue.state_bytes)
        };

        let before = read_state(&fixture.store);
        let session = crate::durability_counters::BarrierSession::begin();
        assert!(fixture
            .store
            .pending_projection_cleanup_bounded(MAX_PENDING_PROJECTION_CLEANUP_PER_PASS)
            .unwrap()
            .is_empty());
        let counts = session.counts();
        crate::durability_counters::BarrierSession::detach_current_thread();
        assert_eq!(
            counts.total(),
            0,
            "an entirely empty cleanup queue took durability barriers: {}",
            counts.report()
        );
        assert_eq!(
            read_state(&fixture.store),
            before,
            "an entirely empty cleanup queue rewrote the durable round state"
        );

        let path = ManagedPath::parse("pages/elision.md").unwrap();
        fs::write(fixture.graph_root.join(path.as_str()), b"- base\n").unwrap();
        let target = b"- target\n";
        let intent = ProjectionIntent::new(
            fixture.store.workspace_id(),
            PageId::from_uuid(Uuid::from_u128(70_001)),
            path,
            FrontierV2::default(),
            Vec::new(),
            ProjectionPrecondition::Base(BlobDescription::of(b"- base\n")),
            crate::oplog::ProjectionTargetKind::Present,
            BlobDescription::of(target),
            Vec::new(),
        )
        .unwrap();
        fixture
            .store
            .publish_intent(&intent, Some(b"- base\n"))
            .unwrap();
        let reservation = fixture.store.reserve_attempt(&intent).unwrap();
        let mut authority = fixture
            .store
            .begin_mutation(&intent, Some(&reservation))
            .unwrap();
        let proof = fixture
            .graph
            .write_page_projection(
                intent.path().as_str(),
                Some(b"- base\n"),
                target,
                &mut authority,
            )
            .unwrap();
        fixture
            .store
            .publish_completion(authority, &intent, &proof)
            .unwrap();

        // `publish_pending_cleanup_marker` appends to the INACTIVE round, so
        // the active round is still empty and the flip is now doing the work
        // it exists for: making that marker reachable. It must still happen,
        // and the entry must drain in the same pass.
        assert_eq!(
            fixture
                .store
                .pending_projection_cleanup_bounded(MAX_PENDING_PROJECTION_CLEANUP_PER_PASS)
                .unwrap()
                .len(),
            1,
            "a non-empty inactive round did not become reachable and drain"
        );
        assert_ne!(
            read_state(&fixture.store).0,
            before.0,
            "the durable flip was elided while the inactive round held a marker"
        );
    }

    #[test]
    fn mutation_authority_preserves_deeply_nested_projection_layouts() {
        let fixture = Fixture::new_at(
            "nested-authority-layout",
            "pages/topic/subtopic/archive/a.md",
        );
        let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
        let mut authority = fixture
            .store
            .begin_mutation(&fixture.intent, Some(&reservation))
            .unwrap();
        let proof = fixture
            .graph
            .write_page_projection(
                fixture.intent.path().as_str(),
                None,
                &fixture.target,
                &mut authority,
            )
            .unwrap();
        fixture
            .store
            .publish_completion(authority, &fixture.intent, &proof)
            .unwrap();
        assert_eq!(
            fs::read(fixture.graph_root.join("pages/topic/subtopic/archive/a.md")).unwrap(),
            fixture.target
        );
    }

    #[test]
    fn every_published_evidence_class_obeys_the_reload_limit() {
        for kind in [
            "projection base",
            "projection target",
            "projection intent",
            "projection completion",
        ] {
            assert!(require_evidence_length(
                kind,
                MAX_PROJECTION_EVIDENCE_BYTES,
                MAX_PROJECTION_EVIDENCE_BYTES
            )
            .is_ok());
            assert!(matches!(
                require_evidence_length(
                    kind,
                    MAX_PROJECTION_EVIDENCE_BYTES + 1,
                    MAX_PROJECTION_EVIDENCE_BYTES
                ),
                Err(ProjectionStoreError::EvidenceTooLarge {
                    kind: found,
                    declared,
                    limit,
                }) if found == kind
                    && declared == MAX_PROJECTION_EVIDENCE_BYTES + 1
                    && limit == MAX_PROJECTION_EVIDENCE_BYTES
            ));
        }
    }
}
