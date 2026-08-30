//! Production shared-provider wire protocol and deterministic replay harness.
//!
//! The provider transport, descriptor/frontier/recovery records, exact paths,
//! and bounded cursors in this module are used by `crate::sync_runtime`.
//! Deterministic scenarios reuse those same production definitions: every
//! replica owns an isolated archive and provider inbox/outbox tree, provider
//! movement is explicit, and only bytes staged through `ObjectStore` cross
//! replicas. Production code must import this module directly; the sibling
//! `simulator` module is only a compatibility surface for scenario tests.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[cfg(windows)]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
#[cfg(windows)]
use cap_std::fs::OpenOptionsExt as _;
use cap_std::fs::{Dir, OpenOptions, ReadDir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::object_store::{ensure_directory_nofollow, open_dir_nofollow, sync_dir_required};
use super::sync_layout::{
    PROVIDER_DEVICE_AUTHORITY_FILE as PROVIDER_DEVICE_AUTHORITY_NAME,
    PROVIDER_PENDING_PUBLICATION_DIR as PROVIDER_PENDING_PUBLICATION_NAMESPACE,
    SHARED_ENROLLMENT_DIR as PROVIDER_ENROLLMENT_NAMESPACE,
    SHARED_MANIFESTS_DIR as PROVIDER_MANIFESTS_NAMESPACE,
    SHARED_OBJECTS_DIR as PROVIDER_OBJECTS_NAMESPACE,
    SHARED_REMOVED_DIR as PROVIDER_REMOVED_NAMESPACE,
    SHARED_RENAME_EVIDENCE_DIR as PROVIDER_RENAME_EVIDENCE_NAMESPACE,
    SHARED_TEMP_DIR as PROVIDER_TEMP_NAMESPACE,
};
pub(crate) use super::sync_layout::{
    SHARED_ENROLLMENT_DESCRIPTOR_PATH,
    SHARED_FRONTIER_HEADS_DIR as SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE,
    SHARED_MANIFEST_RECOVERY_BLOBS_DIR as SHARED_PROVIDER_MANIFEST_RECOVERY_BLOBS_NAMESPACE,
    SHARED_MANIFEST_RECOVERY_LINKS_DIR as SHARED_PROVIDER_MANIFEST_RECOVERY_LINKS_NAMESPACE,
    SHARED_PUBLICATION_INTENTS_DIR as SHARED_PROVIDER_PUBLICATION_INTENTS_NAMESPACE,
};
use super::{BatchId, DeviceId, LineageDigest, WorkspaceId};

/// Provider trees are deliberately small: rescan is a trace operation, not a
/// background watcher, and must never turn an adversarial trace into an
/// unbounded walk of a host filesystem.
pub const MAX_PROVIDER_RESCAN_ENTRIES: usize = 4_096;
pub const MAX_PROVIDER_RESCAN_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PROVIDER_RESCAN_DEPTH: usize = 16;
pub(crate) const SHARED_PROVIDER_CLEAN_BASELINES_NAMESPACE: &str = "clean-baselines-v1";
pub const MAX_PROVIDER_RESIDUE_ENTRIES: usize = 512;
pub const MAX_PROVIDER_PATH_BYTES: usize = 512;
pub const MAX_PROVIDER_JOURNAL_PENDING: usize = 4;
pub const MAX_PROVIDER_JOURNAL_COMPLETED: usize = 16_384;
pub const MAX_PROVIDER_JOURNAL_BLOB_BYTES: usize = MAX_PROVIDER_RESCAN_BYTES;
pub const MAX_PROVIDER_JOURNAL_RECORD_BYTES: usize = 4 * 1024;
pub const MAX_PROVIDER_JOURNAL_COMPLETION_BYTES: usize =
    MAX_PROVIDER_JOURNAL_COMPLETED * MAX_PROVIDER_JOURNAL_RECORD_BYTES;
pub const MAX_PROVIDER_JOURNAL_FILES: usize = MAX_PROVIDER_JOURNAL_PENDING * 2 + 1;
pub const MAX_PROVIDER_JOURNAL_BYTES: usize = MAX_PROVIDER_JOURNAL_BLOB_BYTES
    + (MAX_PROVIDER_JOURNAL_PENDING + 1) * MAX_PROVIDER_JOURNAL_RECORD_BYTES;
const PROVIDER_JOURNAL_SCHEMA_VERSION: u32 = 1;
const PROVIDER_AUTHORITY_SCHEMA_VERSION: u32 = 1;
const MAX_PROVIDER_AUTHORITY_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderItemKind {
    Object,
    Manifest,
}

/// The only two roots exposed by a simulated filesystem provider.  Their
/// names are fixed by the harness; scenario paths are always relative to one
/// of these roots.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTree {
    Inbox,
    Outbox,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderLocation {
    pub device: String,
    pub tree: ProviderTree,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSource {
    Mailbox { item_id: String },
    Tree { location: ProviderLocation },
}

struct ProviderStagingFile {
    file: fs::File,
    /// Present only when the staging file has a directory entry. Publication
    /// must consume this exact name; it may never create a second hard link.
    name: Option<String>,
}

impl std::ops::Deref for ProviderStagingFile {
    type Target = fs::File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

impl std::ops::DerefMut for ProviderStagingFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProviderJournalOperation {
    Put,
    Rename,
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProviderJournalPhase {
    Prepared,
    Staged,
    PublishIntent,
    Published,
    RetireIntent,
    Retired,
    Cleanup,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderIdentityRecord {
    platform: String,
    first: u64,
    second: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderJournalRecord {
    journal_schema_version: u32,
    operation_id: String,
    operation: ProviderJournalOperation,
    operation_binding: String,
    source_provenance: String,
    tree: ProviderTree,
    from_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_identity: Option<ProviderIdentityRecord>,
    source_len: u64,
    source_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blob_name: Option<String>,
    phase: ProviderJournalPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    staging_identity: Option<ProviderIdentityRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    destination_identity: Option<ProviderIdentityRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    staging_name: Option<String>,
    #[serde(default)]
    staging_generation: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostic_path: Option<String>,
    authentication_tag: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderAuthorityRecord {
    authority_schema_version: u32,
    authentication_key: String,
    device_identity: ProviderIdentityRecord,
    journal_identity: ProviderIdentityRecord,
    authority_key_identity: ProviderIdentityRecord,
    records_identity: ProviderIdentityRecord,
    blobs_identity: ProviderIdentityRecord,
    quarantine_identity: ProviderIdentityRecord,
    completed_identity: ProviderIdentityRecord,
}

struct ProviderRetryJournal {
    root: PathBuf,
    name: String,
    directory: Dir,
    directory_identity: ProviderFileIdentity,
    records: Dir,
    records_identity: ProviderFileIdentity,
    blobs: Dir,
    blobs_identity: ProviderFileIdentity,
    quarantine: Dir,
    quarantine_identity: ProviderFileIdentity,
    completed: Dir,
    completed_identity: ProviderFileIdentity,
    authentication_key: [u8; 32],
    transaction_authority: Arc<ProviderTransactionAuthority>,
}

/// The device directory is the outer authority: it is outside both mutable
/// provider residue and the retry journal. Unix locks that retained directory
/// descriptor directly. Windows retains the device directory and locks this
/// authority file, whose handle denies delete sharing so its name cannot be
/// replaced while any process scope is live.
struct ProviderTransactionAuthority {
    device_parent: Dir,
    device_name: String,
    device_directory: Dir,
    device_identity: ProviderFileIdentity,
    authority_file: fs::File,
    authority_identity: ProviderFileIdentity,
    authority_record_bytes: Vec<u8>,
    authority_key_file: fs::File,
    authority_key_identity: ProviderFileIdentity,
    local_held: AtomicBool,
}

/// An owned, typed capability proving that the one provider transaction gate
/// is held. It can cross Rust borrow boundaries, but helpers reject a token
/// minted by any other journal authority.
struct ProviderTransactionGate {
    authority: Arc<ProviderTransactionAuthority>,
    lock_file: fs::File,
}

enum ProviderSourceTransactionGate<'a> {
    Mailbox,
    Tree(&'a ProviderTransactionGate),
}

impl Drop for ProviderTransactionGate {
    fn drop(&mut self) {
        provider_unlock_file(&self.lock_file);
        self.authority.local_held.store(false, Ordering::Release);
    }
}

/// A validated provider file whose descriptor remains the authority for any
/// subsequent operation.  Keeping the handle and the bytes together prevents
/// an open-then-pathname publication or deletion race.
struct OpenProviderFile {
    file: fs::File,
    bytes: Vec<u8>,
}

/// A stable identity captured from the retained file handle.  Path checks are
/// only useful when they are tied back to this identity: an exposed provider
/// path may be renamed or replaced between trace actions.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderFileIdentity {
    volume: u64,
    file_id: [u8; 16],
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderFileIdentity;

/// Every namespace a Tine provider tree carries, in BOTH trees.
///
/// This is the inventory `ProviderRuntime::open` creates, and therefore the one
/// any reader that claims to recognize an untouched local skeleton must expect.
/// The two drifted the moment clean baselines were added: the reader still
/// expected ten namespaces while a first local activation wrote eleven, so an
/// ordinary local graph read as shared-or-unknown. One source of truth instead.
pub const SHARED_PROVIDER_TREE_NAMESPACES: [&str; 8] = [
    PROVIDER_OBJECTS_NAMESPACE,
    PROVIDER_MANIFESTS_NAMESPACE,
    PROVIDER_ENROLLMENT_NAMESPACE,
    SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE,
    SHARED_PROVIDER_CLEAN_BASELINES_NAMESPACE,
    PROVIDER_TEMP_NAMESPACE,
    PROVIDER_REMOVED_NAMESPACE,
    PROVIDER_RENAME_EVIDENCE_NAMESPACE,
];

struct ProviderRuntime {
    root: PathBuf,
    inbox: Dir,
    outbox: Dir,
}

impl ProviderRuntime {
    fn open(root: PathBuf) -> Result<Self, ScenarioError> {
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ScenarioError::UnsafeProviderEntry(root.display().to_string()))?;
        let parent = root
            .parent()
            .ok_or_else(|| ScenarioError::UnsafeProviderEntry(root.display().to_string()))?;
        let canonical_parent =
            fs::canonicalize(parent).map_err(|error| ScenarioError::Io(error.to_string()))?;
        let parent_capability = Dir::open_ambient_dir(&canonical_parent, ambient_authority())
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        ensure_shared_provider_directory(&parent_capability, name)?;
        let provider = open_provider_directory(&parent_capability, name)?;
        for tree in ["inbox", "outbox"] {
            ensure_shared_provider_directory(&provider, tree)?;
            let tree = open_provider_directory(&provider, tree)?;
            for namespace in SHARED_PROVIDER_TREE_NAMESPACES {
                ensure_shared_provider_directory(&tree, namespace)?;
                let _ = open_provider_directory(&tree, namespace)?;
            }
        }
        let inbox = open_provider_directory(&provider, "inbox")?;
        let outbox = open_provider_directory(&provider, "outbox")?;
        Ok(Self {
            root: canonical_parent.join(name),
            inbox,
            outbox,
        })
    }

    fn tree_path(&self, tree: ProviderTree) -> PathBuf {
        self.root.join(match tree {
            ProviderTree::Inbox => "inbox",
            ProviderTree::Outbox => "outbox",
        })
    }

    fn tree(&self, tree: ProviderTree) -> &Dir {
        match tree {
            ProviderTree::Inbox => &self.inbox,
            ProviderTree::Outbox => &self.outbox,
        }
    }

    fn parent_and_name(
        &self,
        tree: ProviderTree,
        path: &str,
        create: bool,
    ) -> Result<(Dir, String), ScenarioError> {
        if !valid_provider_path(path) {
            return Err(ScenarioError::InvalidProviderPath(path.into()));
        }
        let mut components = path.split('/').peekable();
        let mut parent = self
            .tree(tree)
            .try_clone()
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                return Ok((parent, component.into()));
            }
            if create {
                ensure_shared_provider_directory(&parent, component)?;
            }
            parent = open_provider_directory(&parent, component)?;
        }
        Err(ScenarioError::InvalidProviderPath(path.into()))
    }

    /// Resolve a read-only provider location whose intermediate directories a
    /// file-sync tool may not have delivered yet.
    ///
    /// `None` means one of those directories is absent. An entry that is
    /// present but is not a real no-follow directory still refuses.
    fn delivered_parent_and_name(
        &self,
        tree: ProviderTree,
        path: &str,
    ) -> Result<Option<(Dir, String)>, ScenarioError> {
        if !valid_provider_path(path) {
            return Err(ScenarioError::InvalidProviderPath(path.into()));
        }
        let mut components = path.split('/').peekable();
        let mut parent = self
            .tree(tree)
            .try_clone()
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        let mut parent_path = self.tree_path(tree);
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                return Ok(Some((parent, component.into())));
            }
            let Some(child) = open_delivered_provider_directory(
                &parent,
                component,
                &parent_path.join(component),
            )?
            else {
                return Ok(None);
            };
            parent_path.push(component);
            parent = child;
        }
        Err(ScenarioError::InvalidProviderPath(path.into()))
    }

    fn put_complete(
        &mut self,
        journal: &ProviderRetryJournal,
        gate: &ProviderTransactionGate,
        operation_binding: &str,
        source_provenance: &str,
        location: &ProviderLocation,
        bytes: &[u8],
        source_identity: Option<ProviderIdentityRecord>,
        initial_staging_name: Option<String>,
    ) -> Result<(), ScenarioError> {
        journal.require_transaction_gate(gate)?;
        reject_provider_temporary_path(&location.path)?;
        let supplied_source_identity = source_identity.clone();
        let (destination_dir, destination_name) =
            self.parent_and_name(location.tree, &location.path, true)?;
        let temporary_dir =
            open_provider_directory(self.tree(location.tree), PROVIDER_TEMP_NAMESPACE)?;
        let mut record = match journal.load(
            gate,
            ProviderJournalOperation::Put,
            operation_binding,
            source_provenance,
            location.tree,
            &location.path,
            None,
        )? {
            Some(record) => record,
            None => {
                if open_provider_regular_optional(
                    &destination_dir,
                    &destination_name,
                    MAX_PROVIDER_RESCAN_BYTES,
                    &location.path,
                )?
                .is_some()
                {
                    return Err(ScenarioError::ProviderConflictingBytes(
                        location.path.clone(),
                    ));
                }
                let operation_id = ProviderRetryJournal::operation_id(
                    ProviderJournalOperation::Put,
                    operation_binding,
                    source_provenance,
                    location.tree,
                    &location.path,
                    None,
                    u64::try_from(bytes.len()).map_err(|_| ScenarioError::ProviderJournalLimit)?,
                    &provider_digest(bytes),
                );
                let initial_identity = if initial_staging_name.is_some() {
                    source_identity.clone()
                } else {
                    None
                };
                let initial_phase = if initial_staging_name.is_some() {
                    ProviderJournalPhase::Staged
                } else {
                    ProviderJournalPhase::Prepared
                };
                let initial_generation = if initial_staging_name.is_none()
                    && operation_binding.starts_with("transfer:")
                {
                    1
                } else {
                    0
                };
                let record = ProviderJournalRecord {
                    journal_schema_version: PROVIDER_JOURNAL_SCHEMA_VERSION,
                    operation_id: operation_id.clone(),
                    operation: ProviderJournalOperation::Put,
                    operation_binding: operation_binding.into(),
                    source_provenance: source_provenance.into(),
                    tree: location.tree,
                    from_path: location.path.clone(),
                    to_path: None,
                    source_identity,
                    source_len: u64::try_from(bytes.len())
                        .map_err(|_| ScenarioError::ProviderJournalLimit)?,
                    source_digest: provider_digest(bytes),
                    blob_name: Some(ProviderRetryJournal::blob_name(&operation_id)),
                    phase: initial_phase,
                    staging_identity: initial_identity,
                    destination_identity: None,
                    staging_name: Some(initial_staging_name.unwrap_or_else(|| {
                        ProviderRetryJournal::staging_name(&operation_id, initial_generation)
                    })),
                    staging_generation: initial_generation,
                    diagnostic_path: None,
                    authentication_tag: String::new(),
                };
                journal.create(gate, &record, Some(bytes))?;
                provider_journal_after_phase_hook(record.phase)?;
                record
            }
        };
        if u64::try_from(bytes.len()).ok() != Some(record.source_len)
            || provider_digest(bytes) != record.source_digest
            || record.source_provenance != source_provenance
            || supplied_source_identity
                .as_ref()
                .is_some_and(|identity| record.source_identity.as_ref() != Some(identity))
        {
            return Err(ScenarioError::UnsafeProviderJournal(
                record.operation_id.clone(),
            ));
        }
        if record.phase == ProviderJournalPhase::Cleanup {
            validate_put_destination(
                &destination_dir,
                &destination_name,
                &location.path,
                bytes,
                &record,
            )?;
            return journal.complete(gate, &record);
        }
        let expected = journal.read_blob(gate, &record)?;
        if record.phase == ProviderJournalPhase::Prepared {
            if open_provider_regular_optional(
                &destination_dir,
                &destination_name,
                MAX_PROVIDER_RESCAN_BYTES,
                &location.path,
            )?
            .is_some()
            {
                return Err(ScenarioError::ProviderConflictingBytes(
                    location.path.clone(),
                ));
            }
            loop {
                let staging_name = record.staging_name.as_deref().ok_or_else(|| {
                    ScenarioError::UnsafeProviderJournal(record.operation_id.clone())
                })?;
                if open_provider_regular_optional(
                    &temporary_dir,
                    staging_name,
                    MAX_PROVIDER_RESCAN_BYTES,
                    staging_name,
                )?
                .is_none()
                {
                    break;
                }
                quarantine_unowned_staging(
                    journal,
                    gate,
                    &temporary_dir,
                    staging_name,
                    self.tree(location.tree),
                    &record.operation_id,
                    record.staging_generation,
                )?;
                record.staging_generation = record
                    .staging_generation
                    .checked_add(1)
                    .ok_or(ScenarioError::ProviderJournalLimit)?;
                record.staging_name = Some(ProviderRetryJournal::staging_name(
                    &record.operation_id,
                    record.staging_generation,
                ));
                journal.store(gate, &record)?;
            }
            let staging_name = record
                .staging_name
                .as_deref()
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
            let mut staged =
                create_provider_journal_staging(&temporary_dir, staging_name, &location.path)?;
            staged
                .write_all(&expected)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            crate::durability_counters::sync_file(&staged.file)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            validate_provider_file_bytes(&mut staged, &expected, &location.path)?;
            record.staging_identity = Some(provider_identity_record(provider_file_identity(
                &staged.file,
            )?));
            record.phase = ProviderJournalPhase::Staged;
            journal.store(gate, &record)?;
            provider_journal_after_phase_hook(ProviderJournalPhase::Staged)?;
        }
        if record.phase == ProviderJournalPhase::Staged {
            match validate_journal_staging(&temporary_dir, &record, &expected, &location.path) {
                Ok(()) => {}
                Err(ScenarioError::UnsafeProviderEntry(_)) => {
                    // In-scope recovery (a sync service or external editor
                    // replaced or removed the staging file across a crash at
                    // Staged): the private journal blob remains the byte
                    // authority, so the intruder is quarantined with its bytes
                    // preserved — never published — and staging is rebuilt
                    // from the blob instead of wedging every future retry of
                    // this operation on the same refusal.
                    let staging_name = record.staging_name.as_deref().ok_or_else(|| {
                        ScenarioError::UnsafeProviderJournal(record.operation_id.clone())
                    })?;
                    if open_provider_regular_optional(
                        &temporary_dir,
                        staging_name,
                        MAX_PROVIDER_RESCAN_BYTES,
                        staging_name,
                    )?
                    .is_some()
                    {
                        quarantine_unowned_staging(
                            journal,
                            gate,
                            &temporary_dir,
                            staging_name,
                            self.tree(location.tree),
                            &record.operation_id,
                            record.staging_generation,
                        )?;
                    }
                    record.staging_generation = record
                        .staging_generation
                        .checked_add(1)
                        .ok_or(ScenarioError::ProviderJournalLimit)?;
                    record.staging_name = Some(ProviderRetryJournal::staging_name(
                        &record.operation_id,
                        record.staging_generation,
                    ));
                    journal.store(gate, &record)?;
                    let staging_name = record.staging_name.as_deref().ok_or_else(|| {
                        ScenarioError::UnsafeProviderJournal(record.operation_id.clone())
                    })?;
                    let mut staged = create_provider_journal_staging(
                        &temporary_dir,
                        staging_name,
                        &location.path,
                    )?;
                    staged
                        .write_all(&expected)
                        .map_err(|error| ScenarioError::Io(error.to_string()))?;
                    crate::durability_counters::sync_file(&staged.file)
                        .map_err(|error| ScenarioError::Io(error.to_string()))?;
                    validate_provider_file_bytes(&mut staged, &expected, &location.path)?;
                    record.staging_identity = Some(provider_identity_record(
                        provider_file_identity(&staged.file)?,
                    ));
                    journal.store(gate, &record)?;
                    validate_journal_staging(&temporary_dir, &record, &expected, &location.path)?;
                }
                Err(error) => return Err(error),
            }
            record.phase = ProviderJournalPhase::PublishIntent;
            journal.store(gate, &record)?;
            provider_journal_after_phase_hook(ProviderJournalPhase::PublishIntent)?;
        }
        if record.phase == ProviderJournalPhase::PublishIntent {
            publish_journal_destination(
                journal,
                gate,
                &mut record,
                &temporary_dir,
                self.tree(location.tree),
                &destination_dir,
                &destination_name,
                &expected,
                &location.path,
            )?;
            validate_put_destination(
                &destination_dir,
                &destination_name,
                &location.path,
                &expected,
                &record,
            )?;
            sync_shared_provider_publication_directories(&destination_dir, Some(&temporary_dir))?;
            record.phase = ProviderJournalPhase::Published;
            journal.store(gate, &record)?;
            provider_journal_after_phase_hook(ProviderJournalPhase::Published)?;
        }
        validate_put_destination(
            &destination_dir,
            &destination_name,
            &location.path,
            &expected,
            &record,
        )?;
        journal.complete(gate, &record)
    }
}

pub(crate) const SHARED_PROVIDER_MANIFEST_RECOVERY_FORMAT_VERSION: u32 = 1;
pub(crate) const SHARED_PROVIDER_ACCEPTED_MANIFEST_AUDIT_FORMAT_VERSION: u32 = 1;
const SHARED_PROVIDER_FRONTIER_HEAD_SCHEMA_VERSION: u32 = 1;
const MAX_SHARED_PROVIDER_FRONTIER_HEAD_BYTES: usize = 256 * 1024;
const MAX_SHARED_PROVIDER_FRONTIER_TIPS: usize = 4_096;

const fn zero_u32(value: &u32) -> bool {
    *value == 0
}

const fn zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Immutable, content-addressed discovery hint for one device's accepted
/// causal frontier. A validated record can select exact immutable manifests
/// for inspection, but never grants graph-write or acceptance authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SharedProviderFrontierHeadV1 {
    schema_version: u32,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    descriptor_digest: super::ContentDigest,
    oplog_protocol_version: u32,
    author_device_id: DeviceId,
    accepted_generation: u64,
    accepted_frontier_root: super::ContentDigest,
    frontier_tips: Vec<BatchId>,
    #[serde(default, skip_serializing_if = "zero_u32")]
    manifest_recovery_format_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manifest_recovery_coverage_root: Option<super::ContentDigest>,
    #[serde(default, skip_serializing_if = "zero_u32")]
    accepted_manifest_audit_format_version: u32,
    #[serde(default, skip_serializing_if = "zero_u64")]
    accepted_manifest_audit_coverage_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accepted_manifest_audit_coverage_root: Option<super::ContentDigest>,
    #[serde(default, skip_serializing_if = "zero_u64")]
    accepted_manifest_revalidation_next_sequence: u64,
}

impl SharedProviderFrontierHeadV1 {
    pub(crate) fn new(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        descriptor_digest: super::ContentDigest,
        author_device_id: DeviceId,
        accepted_generation: u64,
        accepted_frontier_root: super::ContentDigest,
        frontier_tips: Vec<BatchId>,
        manifest_recovery_coverage_root: Option<super::ContentDigest>,
    ) -> Result<Self, ScenarioError> {
        Self::new_with_accepted_manifest_audit_coverage(
            workspace_id,
            lineage_digest,
            descriptor_digest,
            author_device_id,
            accepted_generation,
            accepted_frontier_root,
            frontier_tips,
            manifest_recovery_coverage_root,
            None,
            None,
        )
    }

    pub(crate) fn new_with_accepted_manifest_audit_coverage(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        descriptor_digest: super::ContentDigest,
        author_device_id: DeviceId,
        accepted_generation: u64,
        accepted_frontier_root: super::ContentDigest,
        mut frontier_tips: Vec<BatchId>,
        manifest_recovery_coverage_root: Option<super::ContentDigest>,
        accepted_manifest_audit_coverage_sequence: Option<u64>,
        accepted_manifest_revalidation_next_sequence: Option<u64>,
    ) -> Result<Self, ScenarioError> {
        frontier_tips.sort_unstable();
        frontier_tips.dedup();
        let record = Self {
            schema_version: SHARED_PROVIDER_FRONTIER_HEAD_SCHEMA_VERSION,
            workspace_id,
            lineage_digest,
            descriptor_digest,
            oplog_protocol_version: super::OPLOG_PROTOCOL_VERSION,
            author_device_id,
            accepted_generation,
            accepted_frontier_root,
            frontier_tips,
            manifest_recovery_format_version: manifest_recovery_coverage_root
                .map_or(0, |_| SHARED_PROVIDER_MANIFEST_RECOVERY_FORMAT_VERSION),
            manifest_recovery_coverage_root,
            accepted_manifest_audit_format_version: accepted_manifest_audit_coverage_sequence
                .map_or(0, |_| {
                    SHARED_PROVIDER_ACCEPTED_MANIFEST_AUDIT_FORMAT_VERSION
                }),
            accepted_manifest_audit_coverage_sequence: accepted_manifest_audit_coverage_sequence
                .unwrap_or(0),
            accepted_manifest_audit_coverage_root: accepted_manifest_audit_coverage_sequence
                .map(|_| accepted_frontier_root),
            accepted_manifest_revalidation_next_sequence:
                accepted_manifest_revalidation_next_sequence.unwrap_or(0),
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, ScenarioError> {
        self.validate()?;
        let bytes =
            serde_json::to_vec(self).map_err(|error| ScenarioError::Encode(error.to_string()))?;
        if bytes.len() > MAX_SHARED_PROVIDER_FRONTIER_HEAD_BYTES {
            return Err(ScenarioError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    pub(crate) fn decode(path: &str, bytes: &[u8]) -> Result<Self, ScenarioError> {
        if bytes.len() > MAX_SHARED_PROVIDER_FRONTIER_HEAD_BYTES {
            return Err(ScenarioError::TooLarge(bytes.len()));
        }
        let record: Self = serde_json::from_slice(bytes)
            .map_err(|error| ScenarioError::Decode(error.to_string()))?;
        record.validate()?;
        if record.encode()? != bytes || record.path()? != path {
            return Err(ScenarioError::NonCanonical);
        }
        Ok(record)
    }

    pub(crate) fn path(&self) -> Result<String, ScenarioError> {
        let bytes = self.encode()?;
        Ok(format!(
            "{SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE}/{}-{}.head",
            self.author_device_id,
            super::ContentDigest::of(&bytes)
        ))
    }

    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn lineage_digest(&self) -> LineageDigest {
        self.lineage_digest
    }

    pub(crate) const fn descriptor_digest(&self) -> super::ContentDigest {
        self.descriptor_digest
    }

    pub(crate) const fn author_device_id(&self) -> DeviceId {
        self.author_device_id
    }

    pub(crate) const fn accepted_generation(&self) -> u64 {
        self.accepted_generation
    }

    pub(crate) const fn accepted_frontier_root(&self) -> super::ContentDigest {
        self.accepted_frontier_root
    }

    pub(crate) fn frontier_tips(&self) -> &[BatchId] {
        &self.frontier_tips
    }

    pub(crate) const fn manifest_recovery_coverage_root(&self) -> Option<super::ContentDigest> {
        self.manifest_recovery_coverage_root
    }

    pub(crate) fn has_current_manifest_recovery_coverage(&self) -> bool {
        self.manifest_recovery_format_version == SHARED_PROVIDER_MANIFEST_RECOVERY_FORMAT_VERSION
            && self.manifest_recovery_coverage_root == Some(self.accepted_frontier_root)
    }

    pub(crate) fn accepted_manifest_audit_coverage_sequence(&self) -> Option<u64> {
        if self.accepted_manifest_audit_format_version
            == SHARED_PROVIDER_ACCEPTED_MANIFEST_AUDIT_FORMAT_VERSION
            && self.accepted_manifest_audit_coverage_sequence != 0
            && self.accepted_manifest_audit_coverage_root == Some(self.accepted_frontier_root)
        {
            Some(self.accepted_manifest_audit_coverage_sequence)
        } else {
            None
        }
    }

    pub(crate) fn has_current_accepted_manifest_audit_coverage(&self) -> bool {
        self.accepted_manifest_audit_coverage_sequence() == Some(self.accepted_generation)
    }

    pub(crate) fn accepted_manifest_revalidation_next_sequence(&self) -> Option<u64> {
        let maximum = self.accepted_generation.checked_add(1)?;
        (self.accepted_manifest_revalidation_next_sequence != 0
            && self.accepted_manifest_revalidation_next_sequence <= maximum)
            .then_some(self.accepted_manifest_revalidation_next_sequence)
    }

    fn validate(&self) -> Result<(), ScenarioError> {
        if self.schema_version != SHARED_PROVIDER_FRONTIER_HEAD_SCHEMA_VERSION
            || self.oplog_protocol_version != super::OPLOG_PROTOCOL_VERSION
            || self.frontier_tips.len() > MAX_SHARED_PROVIDER_FRONTIER_TIPS
            || self.frontier_tips.windows(2).any(|pair| pair[0] >= pair[1])
            || !matches!(
                (
                    self.manifest_recovery_format_version,
                    self.manifest_recovery_coverage_root,
                ),
                (0, None)
            ) && !(self.manifest_recovery_format_version
                == SHARED_PROVIDER_MANIFEST_RECOVERY_FORMAT_VERSION
                && self.manifest_recovery_coverage_root == Some(self.accepted_frontier_root))
            || !matches!(
                (
                    self.accepted_manifest_audit_format_version,
                    self.accepted_manifest_audit_coverage_sequence,
                    self.accepted_manifest_audit_coverage_root,
                ),
                (0, 0, None)
            ) && !(self.accepted_manifest_audit_format_version
                == SHARED_PROVIDER_ACCEPTED_MANIFEST_AUDIT_FORMAT_VERSION
                && self.accepted_manifest_audit_coverage_sequence != 0
                && self.accepted_manifest_audit_coverage_sequence <= self.accepted_generation
                && self.accepted_manifest_audit_coverage_root == Some(self.accepted_frontier_root))
            || (self.accepted_manifest_revalidation_next_sequence != 0
                && (self.accepted_manifest_audit_coverage_sequence != self.accepted_generation
                    || self
                        .accepted_generation
                        .checked_add(1)
                        .is_none_or(|maximum| {
                            self.accepted_manifest_revalidation_next_sequence > maximum
                        })))
        {
            return Err(ScenarioError::NonCanonical);
        }
        Ok(())
    }
}

pub(crate) struct SharedProviderFile {
    pub(crate) path: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) kind: Option<ProviderItemKind>,
}

pub(crate) struct SharedProviderScan {
    pub(crate) files: Vec<SharedProviderFile>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SharedProviderObservation {
    Path(String),
    ChunkBoundary,
    Complete,
}

const PROVIDER_PENDING_PUBLICATION_BYTES: usize = 64;

pub(crate) struct SharedProviderObservationCursor {
    phase: u8,
    entries: Option<ReadDir>,
    full: bool,
    observed_entries: usize,
    entry_limit: usize,
}

impl SharedProviderObservationCursor {
    pub(crate) fn begin_next_chunk(&mut self) {
        self.observed_entries = 0;
    }

    pub(crate) fn has_completed_authority_discovery(&self) -> bool {
        // A head cursor is authoritative only after descriptor, frontier-head,
        // and publication-intent discovery. A full cursor must additionally
        // exhaust recovery evidence plus canonical manifests and objects:
        // those later namespaces can reveal ingress that changes accepted
        // authority.
        self.phase > if self.full { 7 } else { 3 }
    }

    #[cfg(test)]
    pub(crate) fn set_entry_limit_for_test(&mut self, entry_limit: usize) {
        assert!(entry_limit > 0);
        self.entry_limit = entry_limit;
    }
}

pub(crate) struct SharedProviderPublicationCursor {
    entries: ReadDir,
}

/// Production-facing composition over the exact filesystem transport used by
/// deterministic provider scenarios. The transport retains its destination
/// directory and authenticated retry journal. Normal runtime work uses exact
/// paths and retained incremental cursors; `scan` remains a simulator audit.
pub(crate) struct SharedProviderTransport {
    runtime: ProviderRuntime,
    journal: ProviderRetryJournal,
    pending_publication: Dir,
}

impl SharedProviderTransport {
    pub(crate) fn open(
        provider_root: &Path,
        private_journal_root: &Path,
    ) -> Result<Self, ScenarioError> {
        let provider_parent = provider_root.parent().ok_or_else(|| {
            ScenarioError::UnsafeProviderEntry(provider_root.display().to_string())
        })?;
        fs::create_dir_all(provider_parent)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        let journal_device = private_journal_root.parent().ok_or_else(|| {
            ScenarioError::UnsafeProviderJournal(private_journal_root.display().to_string())
        })?;
        fs::create_dir_all(journal_device).map_err(|error| ScenarioError::Io(error.to_string()))?;
        let journal = ProviderRetryJournal::open(private_journal_root.to_path_buf())?;
        let canonical_journal_device = fs::canonicalize(journal_device)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        let journal_device = Dir::open_ambient_dir(&canonical_journal_device, ambient_authority())
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        ensure_provider_directory(&journal_device, PROVIDER_PENDING_PUBLICATION_NAMESPACE)?;
        let pending_publication =
            open_provider_directory(&journal_device, PROVIDER_PENDING_PUBLICATION_NAMESPACE)?;
        Ok(Self {
            runtime: ProviderRuntime::open(provider_root.to_path_buf())?,
            journal,
            pending_publication,
        })
    }

    pub(crate) fn publish_descriptor(&mut self, bytes: &[u8]) -> Result<(), ScenarioError> {
        self.publish(SHARED_ENROLLMENT_DESCRIPTOR_PATH, bytes)
    }

    pub(crate) fn publish_object(
        &mut self,
        digest: super::ContentDigest,
        bytes: &[u8],
    ) -> Result<(), ScenarioError> {
        self.publish(
            &format!("{PROVIDER_OBJECTS_NAMESPACE}/{digest}.object"),
            bytes,
        )
    }

    pub(crate) fn publish_object_exact(
        &mut self,
        digest: super::ContentDigest,
        bytes: &[u8],
    ) -> Result<(), ScenarioError> {
        self.publish_exact(
            &format!("{PROVIDER_OBJECTS_NAMESPACE}/{digest}.object"),
            bytes,
            super::MAX_OBJECT_BYTES,
        )
    }

    pub(crate) fn publish_manifest(
        &mut self,
        batch_id: super::BatchId,
        bytes: &[u8],
    ) -> Result<(), ScenarioError> {
        self.publish(
            &format!("{PROVIDER_MANIFESTS_NAMESPACE}/{batch_id}.manifest"),
            bytes,
        )
    }

    pub(crate) fn publish_clean_baseline_part(
        &mut self,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), ScenarioError> {
        if !path.starts_with(&format!("{SHARED_PROVIDER_CLEAN_BASELINES_NAMESPACE}/"))
            || bytes.len() > super::lazy_genesis::MAX_LAZY_GENESIS_PROVIDER_INDEX_BYTES
        {
            return Err(ScenarioError::InvalidProviderPath(path.into()));
        }
        self.publish_exact(
            path,
            bytes,
            super::lazy_genesis::MAX_LAZY_GENESIS_PROVIDER_INDEX_BYTES,
        )
    }

    pub(crate) fn publish_frontier_head(
        &mut self,
        record: &SharedProviderFrontierHeadV1,
    ) -> Result<String, ScenarioError> {
        let bytes = record.encode()?;
        let path = record.path()?;
        if let Some(existing) = self.read_exact(&path)? {
            if existing == bytes {
                return Ok(path);
            }
            return Err(ScenarioError::ProviderConflictingBytes(path));
        }
        self.publish(&path, &bytes)?;
        Ok(path)
    }

    fn publish(&mut self, path: &str, bytes: &[u8]) -> Result<(), ScenarioError> {
        let gate = self.journal.acquire_transaction_gate()?;
        let source = format!("generated:{}", provider_digest(bytes));
        let location = ProviderLocation {
            device: "local".into(),
            tree: ProviderTree::Outbox,
            path: path.into(),
        };
        self.journal.recycle_completed_put_for_absent_destination(
            &gate,
            &source,
            &source,
            &self.runtime,
            &location,
            bytes,
        )?;
        self.runtime.put_complete(
            &self.journal,
            &gate,
            &source,
            &source,
            &location,
            bytes,
            None,
            None,
        )
    }

    fn publish_exact(
        &mut self,
        path: &str,
        bytes: &[u8],
        limit: usize,
    ) -> Result<(), ScenarioError> {
        let gate = self.journal.acquire_transaction_gate()?;
        let source = format!("generated:{}", provider_digest(bytes));
        let location = ProviderLocation {
            device: "local".into(),
            tree: ProviderTree::Outbox,
            path: path.into(),
        };
        self.journal.recycle_completed_put_for_absent_destination(
            &gate,
            &source,
            &source,
            &self.runtime,
            &location,
            bytes,
        )?;
        let retry = self
            .journal
            .load(
                &gate,
                ProviderJournalOperation::Put,
                &source,
                &source,
                location.tree,
                &location.path,
                None,
            )?
            .is_some();
        if !retry {
            let (destination_dir, destination_name) =
                self.runtime
                    .parent_and_name(location.tree, &location.path, true)?;
            if let Some(existing) = open_provider_regular_optional(
                &destination_dir,
                &destination_name,
                limit,
                &location.path,
            )? {
                if existing.bytes != bytes {
                    return Err(ScenarioError::ProviderConflictingBytes(
                        location.path.clone(),
                    ));
                }
                sync_shared_provider_publication_directories(&destination_dir, None)?;
                let current = open_provider_regular_optional(
                    &destination_dir,
                    &destination_name,
                    limit,
                    &location.path,
                )?
                .ok_or_else(|| ScenarioError::UnsafeProviderEntry(location.path.clone()))?;
                if current.bytes != bytes
                    || !provider_files_have_same_identity(&existing.file, &current.file)?
                {
                    return Err(ScenarioError::ProviderConflictingBytes(
                        location.path.clone(),
                    ));
                }
                return Ok(());
            }
        }
        self.runtime.put_complete(
            &self.journal,
            &gate,
            &source,
            &source,
            &location,
            bytes,
            None,
            None,
        )
    }

    pub(crate) fn read_exact(&self, path: &str) -> Result<Option<Vec<u8>>, ScenarioError> {
        reject_provider_temporary_path(path)?;
        let limit = match provider_item_kind(path) {
            Some(ProviderItemKind::Object) => super::MAX_OBJECT_BYTES,
            Some(ProviderItemKind::Manifest) => super::MAX_MANIFEST_BYTES,
            None if path == SHARED_ENROLLMENT_DESCRIPTOR_PATH => {
                super::enrollment::MAX_ENROLLMENT_RECORD_BYTES
            }
            None if path.starts_with(&format!("{SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE}/")) => {
                MAX_SHARED_PROVIDER_FRONTIER_HEAD_BYTES
            }
            None if path.starts_with(&format!("{SHARED_PROVIDER_CLEAN_BASELINES_NAMESPACE}/")) => {
                super::lazy_genesis::MAX_LAZY_GENESIS_PROVIDER_INDEX_BYTES
            }
            None => MAX_PROVIDER_RESCAN_BYTES,
        };
        // `read_exact` answers "is this provider item here?", and a namespace
        // directory a file-sync tool has not delivered is one more way for the
        // answer to be no. Reporting it as an unsafe entry instead made the
        // runtime lanes that read through this surface — the frontier-head
        // check on every managed-local publication among them — refuse an
        // ordinary half-delivered tree, once per retry, forever.
        let Some((parent, name)) = self
            .runtime
            .delivered_parent_and_name(ProviderTree::Outbox, path)?
        else {
            return Ok(None);
        };
        open_provider_regular_optional(&parent, &name, limit, path)
            .map(|opened| opened.map(|opened| opened.bytes))
    }

    pub(crate) fn head_observation_cursor(
        &self,
    ) -> Result<SharedProviderObservationCursor, ScenarioError> {
        Ok(SharedProviderObservationCursor {
            phase: 0,
            entries: None,
            full: false,
            observed_entries: 0,
            entry_limit: MAX_PROVIDER_RESCAN_ENTRIES,
        })
    }

    pub(crate) fn full_observation_cursor(
        &self,
    ) -> Result<SharedProviderObservationCursor, ScenarioError> {
        Ok(SharedProviderObservationCursor {
            phase: 0,
            entries: None,
            full: true,
            observed_entries: 0,
            entry_limit: MAX_PROVIDER_RESCAN_ENTRIES,
        })
    }

    /// Return one provider-visible path without opening its bytes.
    ///
    /// The cursor visits enrollment and manifests before immutable objects, so
    /// ingress can remain manifest-driven while still eventually surfacing a
    /// pre-existing generated-object conflict copy.
    ///
    /// Phase 0 sweeps the outbox's own children. It refuses ONLY a canonical
    /// namespace that is present as something other than a real no-follow
    /// directory. Every other entry is skipped: a file-sync client writes its
    /// own temporary files and conflict copies into the very directories it is
    /// delivering, and a future Tine may add a namespace this build has never
    /// heard of. None of them are on a path this scan reads — phases 1 onwards
    /// open canonical namespaces by name — so none of them can grant
    /// authority, and refusing them stranded a device over litter.
    pub(crate) fn next_observed_path(
        &self,
        cursor: &mut SharedProviderObservationCursor,
    ) -> Result<SharedProviderObservation, ScenarioError> {
        loop {
            if cursor.phase == 0 {
                if cursor.entries.is_none() {
                    cursor.entries = Some(
                        self.runtime
                            .tree(ProviderTree::Outbox)
                            .entries()
                            .map_err(|error| ScenarioError::Io(error.to_string()))?,
                    );
                }
                let Some(entry) = cursor.entries.as_mut().expect("cursor opened").next() else {
                    cursor.entries = None;
                    cursor.phase = 1;
                    continue;
                };
                let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
                // A name this build cannot even spell is a name no canonical
                // namespace has, so it is one more entry nothing reads.
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if !SHARED_PROVIDER_TREE_NAMESPACES.contains(&name.as_str()) {
                    // Skipping is cheap per entry, but a sync tool can leave
                    // arbitrarily many of them here, and this sweep used to
                    // stop at the first one. Charge them to the same per-turn
                    // budget the namespaces use so one tick cannot walk an
                    // unbounded directory.
                    cursor.observed_entries = cursor.observed_entries.saturating_add(1);
                    if !cursor.full && cursor.observed_entries >= cursor.entry_limit {
                        return Ok(SharedProviderObservation::ChunkBoundary);
                    }
                    continue;
                }
                let kind = entry
                    .file_type()
                    .map_err(|error| ScenarioError::Io(error.to_string()))?;
                if kind.is_symlink() || !kind.is_dir() {
                    return Err(ScenarioError::UnsafeProviderEntry(format!(
                        "{}: expected a real no-follow directory",
                        self.runtime
                            .tree_path(ProviderTree::Outbox)
                            .join(&name)
                            .display()
                    )));
                }
                continue;
            }
            let namespace = match (cursor.full, cursor.phase) {
                (_, 1) => PROVIDER_ENROLLMENT_NAMESPACE,
                (_, 2) => SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE,
                (_, 3) => SHARED_PROVIDER_PUBLICATION_INTENTS_NAMESPACE,
                (true, 4) => SHARED_PROVIDER_MANIFEST_RECOVERY_LINKS_NAMESPACE,
                (true, 5) => SHARED_PROVIDER_MANIFEST_RECOVERY_BLOBS_NAMESPACE,
                (true, 6) => PROVIDER_MANIFESTS_NAMESPACE,
                (true, 7) => PROVIDER_OBJECTS_NAMESPACE,
                _ => return Ok(SharedProviderObservation::Complete),
            };
            if cursor.entries.is_none() {
                // A namespace a file-sync tool has not delivered (or has
                // removed while propagating another device's deletion) is an
                // EMPTY namespace, not a hostile tree. Refusing it turned every
                // provider scan into `UnsafeProviderEntry`, which the actor
                // reports as a `RecoveryBlocked` tick — and the cursor never
                // advances, so the same tick is produced on every retry for as
                // long as the directory is missing (GH: desktop pairing,
                // 2026-08-18).
                let Some(directory) = self.open_delivered_outbox_namespace(namespace)? else {
                    cursor.phase = cursor.phase.saturating_add(1);
                    continue;
                };
                cursor.entries = Some(
                    directory
                        .entries()
                        .map_err(|error| ScenarioError::Io(error.to_string()))?,
                );
            }
            if !cursor.full && cursor.observed_entries >= cursor.entry_limit {
                return Ok(SharedProviderObservation::ChunkBoundary);
            }
            let Some(entry) = cursor.entries.as_mut().expect("cursor opened").next() else {
                cursor.entries = None;
                cursor.phase = cursor.phase.saturating_add(1);
                continue;
            };
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            let name = entry.file_name().into_string().map_err(|_| {
                ScenarioError::UnsafeProviderEntry(format!("{namespace}/non-UTF-8"))
            })?;
            let path = format!("{namespace}/{name}");
            if path.len() > MAX_PROVIDER_PATH_BYTES || !valid_provider_path(&path) {
                return Err(ScenarioError::UnsafeProviderEntry(path));
            }
            if !entry
                .file_type()
                .map_err(|error| ScenarioError::Io(error.to_string()))?
                .is_file()
            {
                return Err(ScenarioError::UnsafeProviderEntry(path));
            }
            if provider_transient_path(&path) {
                cursor.observed_entries = cursor.observed_entries.saturating_add(1);
                return Ok(SharedProviderObservation::Path(path));
            }
            let Some(directory) = self.open_delivered_outbox_namespace(namespace)? else {
                cursor.entries = None;
                cursor.phase = cursor.phase.saturating_add(1);
                continue;
            };
            let file = open_provider_file_nofollow(&directory, &name)
                .map_err(|error| ScenarioError::UnsafeProviderEntry(error.to_string()))?;
            validate_provider_regular_file(&file, &path)?;
            cursor.observed_entries = cursor.observed_entries.saturating_add(1);
            return Ok(SharedProviderObservation::Path(path));
        }
    }

    /// Open one outbox namespace that a file-sync tool may not have delivered.
    ///
    /// `None` means the namespace directory is absent. An entry that IS present
    /// but is not a real no-follow directory still refuses, and the refusal
    /// names the path on disk rather than the bare component.
    fn open_delivered_outbox_namespace(
        &self,
        namespace: &str,
    ) -> Result<Option<Dir>, ScenarioError> {
        let outbox = self.runtime.tree(ProviderTree::Outbox);
        match open_dir_nofollow(outbox, namespace) {
            Ok(directory) => Ok(Some(directory)),
            Err(error) => match outbox.symlink_metadata(namespace) {
                Err(absent) if absent.kind() == ErrorKind::NotFound => Ok(None),
                _ => Err(ScenarioError::UnsafeProviderEntry(format!(
                    "{}: {error}",
                    self.runtime
                        .tree_path(ProviderTree::Outbox)
                        .join(namespace)
                        .display(),
                ))),
            },
        }
    }

    pub(crate) fn retire_frontier_head(&self, path: &str) -> Result<(), ScenarioError> {
        if !path.starts_with(&format!("{SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE}/")) {
            return Err(ScenarioError::InvalidProviderPath(path.into()));
        }
        let digest = Sha256::digest(
            [
                b"tine/provider-frontier-head-retirement/v1\0".as_slice(),
                path.as_bytes(),
            ]
            .concat(),
        );
        let mut event = [0_u8; 8];
        event.copy_from_slice(&digest[..8]);
        run_provider_remove_with(
            &self.runtime,
            &self.journal,
            "local",
            u64::from_be_bytes(event),
            ProviderTree::Outbox,
            path,
            None,
            ProviderRemoveMissingSourcePolicy::SettleIfAbsent,
        )
    }

    pub(crate) fn record_pending_publication(
        &self,
        batch_id: super::BatchId,
    ) -> Result<(), ScenarioError> {
        let gate = self.journal.acquire_transaction_gate()?;
        self.journal.require_transaction_gate(&gate)?;
        let name = format!("{batch_id}.pending");
        let bytes = batch_id.to_string().into_bytes();
        if let Some(mut existing) = open_provider_regular_optional(
            &self.pending_publication,
            &name,
            PROVIDER_PENDING_PUBLICATION_BYTES,
            &name,
        )? {
            return validate_local_file_bytes(&mut existing.file, &bytes, &name);
        }
        pending_publication_marker_creation_hook()?;
        let mut file = create_local_file_exclusive(&self.pending_publication, &name)?;
        file.write_all(&bytes)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        crate::durability_counters::sync_file(&file)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        validate_local_file_bytes(&mut file, &bytes, &name)?;
        sync_provider_directory(&self.pending_publication)
    }

    pub(crate) fn pending_publication_cursor(
        &self,
    ) -> Result<SharedProviderPublicationCursor, ScenarioError> {
        Ok(SharedProviderPublicationCursor {
            entries: self
                .pending_publication
                .entries()
                .map_err(|error| ScenarioError::Io(error.to_string()))?,
        })
    }

    pub(crate) fn next_pending_publication(
        &self,
        cursor: &mut SharedProviderPublicationCursor,
    ) -> Result<Option<super::BatchId>, ScenarioError> {
        let Some(entry) = cursor.entries.next() else {
            return Ok(None);
        };
        let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 publication".into()))?;
        let id = name
            .strip_suffix(".pending")
            .and_then(|value| Uuid::parse_str(value).ok())
            .map(super::BatchId::from_uuid)
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
        let bytes = id.to_string().into_bytes();
        let mut opened = open_provider_regular_optional(
            &self.pending_publication,
            &name,
            PROVIDER_PENDING_PUBLICATION_BYTES,
            &name,
        )?
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
        validate_local_file_bytes(&mut opened.file, &bytes, &name)?;
        Ok(Some(id))
    }

    /// Remove one provider-generated conflict name only after the retained
    /// transaction proves it is byte-identical to one exact canonical path.
    ///
    /// The ordinary crash-recovery retirement journal performs the actual
    /// no-follow removal, so a pathname replacement cannot redirect deletion.
    pub(crate) fn remove_identical_generated_conflict(
        &self,
        conflict_path: &str,
        canonical_path: &str,
    ) -> Result<(), ScenarioError> {
        let digest = Sha256::digest(
            [
                b"tine/provider-generated-conflict-removal/v1\0".as_slice(),
                conflict_path.as_bytes(),
                b"\0",
                canonical_path.as_bytes(),
            ]
            .concat(),
        );
        let mut event = [0_u8; 8];
        event.copy_from_slice(&digest[..8]);
        run_provider_remove_with(
            &self.runtime,
            &self.journal,
            "local",
            u64::from_be_bytes(event),
            ProviderTree::Outbox,
            conflict_path,
            Some(canonical_path),
            ProviderRemoveMissingSourcePolicy::RequirePresent,
        )
    }

    pub(crate) fn scan(&self) -> Result<SharedProviderScan, ScenarioError> {
        let files = bounded_provider_files(
            self.runtime.tree(ProviderTree::Outbox),
            false,
            MAX_PROVIDER_RESCAN_ENTRIES,
            MAX_PROVIDER_RESCAN_BYTES,
        )?
        .into_iter()
        .map(|file| SharedProviderFile {
            kind: provider_item_kind(&file.path),
            path: file.path,
            bytes: file.bytes,
        })
        .collect();
        Ok(SharedProviderScan { files })
    }
}

pub(crate) fn inspect_shared_provider_descriptor(
    provider_root: &Path,
) -> Result<Option<Vec<u8>>, ScenarioError> {
    inspect_shared_provider_descriptor_with(provider_root)
}

/// The non-authoritative shape of a provider tree while a filesystem sync is
/// still materializing its canonical enrollment prefix.  This deliberately
/// recognizes only names and real directory/file kinds; descriptor bytes stay
/// with [`inspect_shared_provider_descriptor`], which validates them
/// before cold discovery can join anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ColdSharedProviderPrefix {
    Partial,
    ReadyForDescriptorInspection,
    Refused,
}

/// Classify the provider-directory prefix that a file-sync client may expose
/// before it has delivered the canonical enrollment descriptor.  Discovery is
/// deliberately keyed by the canonical path rather than by an exact directory
/// inventory: provider temporary files, conflict copies, and future append-only
/// namespaces are not authority and must not strand a new device.  Only an
/// unsafe kind at one of the canonical path components is refused.
pub(crate) fn inspect_cold_shared_provider_prefix(
    provider_root: &Path,
) -> Result<ColdSharedProviderPrefix, ScenarioError> {
    let metadata = match fs::symlink_metadata(provider_root) {
        Ok(metadata) => metadata,
        // An absent root is the same "not yet" every other absent component
        // below is: the folder a file-sync tool has not created here. It is not
        // an unsafe kind, and the only thing a refusal buys is a scarier
        // message for the ordinary case.
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ColdSharedProviderPrefix::Partial)
        }
        Err(error) => return Err(ScenarioError::Io(error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(ColdSharedProviderPrefix::Refused);
    }
    let outbox = provider_root.join("outbox");
    let outbox_metadata = match fs::symlink_metadata(&outbox) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ColdSharedProviderPrefix::Partial)
        }
        Err(error) => return Err(ScenarioError::Io(error.to_string())),
    };
    if outbox_metadata.file_type().is_symlink() || !outbox_metadata.is_dir() {
        return Ok(ColdSharedProviderPrefix::Refused);
    }

    let enrollment = outbox.join(PROVIDER_ENROLLMENT_NAMESPACE);
    let enrollment_metadata = match fs::symlink_metadata(&enrollment) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ColdSharedProviderPrefix::Partial)
        }
        Err(error) => return Err(ScenarioError::Io(error.to_string())),
    };
    if enrollment_metadata.file_type().is_symlink() || !enrollment_metadata.is_dir() {
        return Ok(ColdSharedProviderPrefix::Refused);
    }

    let descriptor = enrollment.join("shared-enrollment-v1.json");
    let descriptor_metadata = match fs::symlink_metadata(&descriptor) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ColdSharedProviderPrefix::Partial)
        }
        Err(error) => return Err(ScenarioError::Io(error.to_string())),
    };
    if descriptor_metadata.file_type().is_symlink() || !descriptor_metadata.is_file() {
        return Ok(ColdSharedProviderPrefix::Refused);
    }
    Ok(ColdSharedProviderPrefix::ReadyForDescriptorInspection)
}

fn inspect_shared_provider_descriptor_with(
    provider_root: &Path,
) -> Result<Option<Vec<u8>>, ScenarioError> {
    let metadata = match fs::symlink_metadata(provider_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ScenarioError::Io(error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ScenarioError::UnsafeProviderEntry(
            provider_root.display().to_string(),
        ));
    }
    let root = Dir::open_ambient_dir(provider_root, ambient_authority())
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    // A provider tree that exists but is INCOMPLETE is the ordinary state of a
    // folder a sync tool is still filling: Syncthing and Dropbox deliver
    // entries in arbitrary order and may hold a directory back for minutes.
    // The descriptor file itself has always been optional here; the two
    // directories above it must be too, or an early device reads a hostile
    // tree where there is only an early one.
    let descriptor_path = provider_root
        .join("outbox")
        .join(SHARED_ENROLLMENT_DESCRIPTOR_PATH);
    // Name the file discovery actually wants. "unsafe provider entry:
    // enrollment" told the one person who could act on it nothing at all.
    let wanted = |error: ScenarioError| match error {
        ScenarioError::UnsafeProviderEntry(detail) => ScenarioError::UnsafeProviderEntry(format!(
            "{detail}; Tine reads sync data from another device at {}",
            descriptor_path.display()
        )),
        error => error,
    };
    let Some(outbox) =
        open_delivered_provider_directory(&root, "outbox", &provider_root.join("outbox"))
            .map_err(wanted)?
    else {
        return Ok(None);
    };
    let Some(enrollment) = open_delivered_provider_directory(
        &outbox,
        PROVIDER_ENROLLMENT_NAMESPACE,
        &provider_root
            .join("outbox")
            .join(PROVIDER_ENROLLMENT_NAMESPACE),
    )
    .map_err(wanted)?
    else {
        return Ok(None);
    };
    open_provider_regular_optional(
        &enrollment,
        "shared-enrollment-v1.json",
        super::enrollment::MAX_ENROLLMENT_RECORD_BYTES,
        SHARED_ENROLLMENT_DESCRIPTOR_PATH,
    )
    .map(|opened| opened.map(|opened| opened.bytes))
}

/// Open one canonical provider directory that a file-sync tool may not have
/// delivered yet.
///
/// `None` means exactly one thing: the entry is not there. Anything that IS
/// there but cannot be opened as a real no-follow directory — a symlink, a
/// regular file, an unreadable entry — is still
/// [`ScenarioError::UnsafeProviderEntry`], which is what that error was written
/// for. The refusal names the path on disk and the file discovery ultimately
/// wants, because "unsafe provider entry: enrollment" told the one person who
/// could act on it nothing at all.
fn open_delivered_provider_directory(
    parent: &Dir,
    name: &str,
    entry_path: &Path,
) -> Result<Option<Dir>, ScenarioError> {
    match open_dir_nofollow(parent, name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) => match parent.symlink_metadata(name) {
            Err(absent) if absent.kind() == ErrorKind::NotFound => Ok(None),
            _ => Err(ScenarioError::UnsafeProviderEntry(format!(
                "{}: {error}",
                entry_path.display(),
            ))),
        },
    }
}

impl ProviderRetryJournal {
    fn open(root: PathBuf) -> Result<Self, ScenarioError> {
        let journal_name = root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(root.display().to_string()))?;
        let device_path = root
            .parent()
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(root.display().to_string()))?;
        let device_name = device_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(root.display().to_string()))?;
        let device_parent_path = device_path
            .parent()
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(root.display().to_string()))?;
        let canonical_device_parent = fs::canonicalize(device_parent_path)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        let device_parent = Dir::open_ambient_dir(&canonical_device_parent, ambient_authority())
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        let device_directory = open_provider_directory(&device_parent, device_name)?;
        let device_identity = provider_directory_identity(&device_directory)?;
        let (authority_file, initial_lock_file, authority_created) =
            open_and_lock_provider_outer_authority(&device_directory)?;
        let authority_identity = provider_file_identity(&authority_file)?;
        let mut initial_lock_file = Some(initial_lock_file);
        let result = (|| {
            let existing_authority = if authority_created {
                None
            } else {
                Some(read_provider_authority_record(&authority_file)?)
            };

            let directory = if authority_created {
                ensure_provider_directory(&device_directory, journal_name)?;
                open_provider_directory(&device_directory, journal_name)?
            } else {
                open_provider_directory(&device_directory, journal_name).map_err(|_| {
                    ScenarioError::UnsafeProviderJournal(
                        "provider journal root was replaced".into(),
                    )
                })?
            };
            let directory_identity = provider_directory_identity(&directory)?;
            if existing_authority.as_ref().is_some_and(|(_, record)| {
                provider_identity_record(directory_identity) != record.journal_identity
            }) {
                return Err(ScenarioError::UnsafeProviderJournal(
                    "provider journal root identity changed".into(),
                ));
            }

            for child in ["records", "blobs", "quarantine", "completed"] {
                if authority_created {
                    ensure_provider_directory(&directory, child)?;
                }
            }
            let records = open_provider_directory(&directory, "records")?;
            let blobs = open_provider_directory(&directory, "blobs")?;
            let quarantine = open_provider_directory(&directory, "quarantine")?;
            let completed = open_provider_directory(&directory, "completed")?;
            let records_identity = provider_directory_identity(&records)?;
            let blobs_identity = provider_directory_identity(&blobs)?;
            let quarantine_identity = provider_directory_identity(&quarantine)?;
            let completed_identity = provider_directory_identity(&completed)?;
            if existing_authority.as_ref().is_some_and(|(_, record)| {
                provider_identity_record(records_identity) != record.records_identity
                    || provider_identity_record(blobs_identity) != record.blobs_identity
                    || provider_identity_record(quarantine_identity) != record.quarantine_identity
                    || provider_identity_record(completed_identity) != record.completed_identity
            }) {
                return Err(ScenarioError::UnsafeProviderJournal(
                    "provider journal namespace identity changed".into(),
                ));
            }

            let (mut authority_key_file, authentication_key, authority_key_identity) =
                if let Some((authority_record_bytes, authority_record)) =
                    existing_authority.as_ref()
                {
                    let key = decode_provider_authentication_key(authority_record)?;
                    let mut opened =
                        open_provider_authority_key_nofollow(&directory, "authority.key")?;
                    validate_local_file_bytes(&mut opened, &key, "authority.key")?;
                    let identity = provider_file_identity(&opened)?;
                    if provider_identity_record(device_identity) != authority_record.device_identity
                        || provider_identity_record(identity)
                            != authority_record.authority_key_identity
                    {
                        return Err(ScenarioError::UnsafeProviderJournal(
                            "provider authority binding changed".into(),
                        ));
                    }
                    let mut outer = authority_file
                        .try_clone()
                        .map_err(|error| ScenarioError::Io(error.to_string()))?;
                    validate_local_file_bytes(
                        &mut outer,
                        authority_record_bytes,
                        PROVIDER_DEVICE_AUTHORITY_NAME,
                    )?;
                    (opened, key, identity)
                } else {
                    if open_provider_authority_key_optional(&directory, "authority.key")?.is_some()
                        || records
                            .entries()
                            .map_err(|error| ScenarioError::Io(error.to_string()))?
                            .next()
                            .is_some()
                        || blobs
                            .entries()
                            .map_err(|error| ScenarioError::Io(error.to_string()))?
                            .next()
                            .is_some()
                        || quarantine
                            .entries()
                            .map_err(|error| ScenarioError::Io(error.to_string()))?
                            .next()
                            .is_some()
                        || completed
                            .entries()
                            .map_err(|error| ScenarioError::Io(error.to_string()))?
                            .next()
                            .is_some()
                    {
                        return Err(ScenarioError::UnsafeProviderJournal(
                            "missing outer provider authority".into(),
                        ));
                    }
                    let first = Uuid::new_v4();
                    let second = Uuid::new_v4();
                    let mut key = [0_u8; 32];
                    key[..16].copy_from_slice(first.as_bytes());
                    key[16..].copy_from_slice(second.as_bytes());
                    let mut file =
                        create_provider_authority_key_exclusive(&directory, "authority.key")?;
                    file.write_all(&key)
                        .map_err(|error| ScenarioError::Io(error.to_string()))?;
                    crate::durability_counters::sync_file(&file)
                        .map_err(|error| ScenarioError::Io(error.to_string()))?;
                    validate_local_file_bytes(&mut file, &key, "authority.key")?;
                    sync_provider_directory(&directory)?;
                    let identity = provider_file_identity(&file)?;
                    (file, key, identity)
                };

            let authority_record = existing_authority
                .as_ref()
                .map(|(_, record)| record.clone())
                .unwrap_or_else(|| ProviderAuthorityRecord {
                    authority_schema_version: PROVIDER_AUTHORITY_SCHEMA_VERSION,
                    authentication_key: base64url_encode(&authentication_key),
                    device_identity: provider_identity_record(device_identity),
                    journal_identity: provider_identity_record(directory_identity),
                    authority_key_identity: provider_identity_record(authority_key_identity),
                    records_identity: provider_identity_record(records_identity),
                    blobs_identity: provider_identity_record(blobs_identity),
                    quarantine_identity: provider_identity_record(quarantine_identity),
                    completed_identity: provider_identity_record(completed_identity),
                });
            let authority_record_bytes = canonical_provider_authority_bytes(&authority_record)?;
            if authority_created {
                let mut outer = authority_file
                    .try_clone()
                    .map_err(|error| ScenarioError::Io(error.to_string()))?;
                outer
                    .write_all(&authority_record_bytes)
                    .and_then(|()| crate::durability_counters::sync_file(&outer))
                    .map_err(|error| ScenarioError::Io(error.to_string()))?;
                validate_local_file_bytes(
                    &mut outer,
                    &authority_record_bytes,
                    PROVIDER_DEVICE_AUTHORITY_NAME,
                )?;
                sync_provider_directory(&device_directory)?;
            }
            validate_local_file_bytes(
                &mut authority_key_file,
                &authentication_key,
                "authority.key",
            )?;

            let transaction_authority = Arc::new(ProviderTransactionAuthority {
                device_parent: device_parent
                    .try_clone()
                    .map_err(|error| ScenarioError::Io(error.to_string()))?,
                device_name: device_name.into(),
                device_directory: device_directory
                    .try_clone()
                    .map_err(|error| ScenarioError::Io(error.to_string()))?,
                device_identity,
                authority_file,
                authority_identity,
                authority_record_bytes,
                authority_key_file,
                authority_key_identity,
                local_held: AtomicBool::new(true),
            });
            let journal = Self {
                root: canonical_device_parent.join(device_name).join(journal_name),
                name: journal_name.into(),
                directory,
                directory_identity,
                records,
                records_identity,
                blobs,
                blobs_identity,
                quarantine,
                quarantine_identity,
                completed,
                completed_identity,
                authentication_key,
                transaction_authority: Arc::clone(&transaction_authority),
            };
            let gate = ProviderTransactionGate {
                authority: transaction_authority,
                lock_file: initial_lock_file.take().ok_or_else(|| {
                    ScenarioError::UnsafeProviderJournal(
                        "provider transaction gate was lost".into(),
                    )
                })?,
            };
            journal.validate_transaction_binding(&gate)?;
            journal.validate_raw_pending_usage(&gate)?;
            // Quarantine recovery precedes graph validation because a crash
            // may leave authenticated creation bytes privately retained
            // while their signed update still names `.creating`.
            journal.reconcile_orphan_quarantine(&gate)?;
            // Validate the complete authenticated graph before reconciliation
            // can rename or remove anything. A valid-looking filename never
            // grants update, record, completion, or blob authority.
            let orphan_blobs = journal.validate_authenticated_graph(&gate)?;
            journal.retire_orphan_blobs(&gate, &orphan_blobs)?;
            journal.reconcile_updates(&gate)?;
            journal.reconcile_completed_updates(&gate)?;
            journal.validate_usage(&gate, 0, 0, false)?;
            journal.validate_completed_usage(&gate, 0, 0)?;
            drop(gate);
            Ok(journal)
        })();
        if let Some(lock_file) = initial_lock_file.as_ref() {
            provider_unlock_file(lock_file);
        }
        result
    }

    fn validate_raw_pending_usage(
        &self,
        gate: &ProviderTransactionGate,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut files = 0_usize;
        let mut bytes = 0_usize;
        let mut operations = BTreeSet::new();
        for (directory, kind) in [
            (&self.records, "record"),
            (&self.blobs, "blob"),
            (&self.quarantine, "quarantine"),
        ] {
            for entry in directory
                .entries()
                .map_err(|error| ScenarioError::Io(error.to_string()))?
            {
                let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
                files = files
                    .checked_add(1)
                    .ok_or(ScenarioError::ProviderJournalLimit)?;
                if files > MAX_PROVIDER_JOURNAL_FILES {
                    return Err(ScenarioError::ProviderJournalLimit);
                }
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
                let operation_id = match kind {
                    "record" => name
                        .strip_suffix(".json")
                        .or_else(|| name.strip_suffix(".update")),
                    "blob" => name
                        .strip_suffix(".blob")
                        .or_else(|| name.strip_suffix(".creating")),
                    "quarantine" => name.strip_suffix(".creating"),
                    _ => None,
                }
                .filter(|value| valid_provider_journal_id(value))
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
                operations.insert(operation_id.to_owned());
                if operations.len() > MAX_PROVIDER_JOURNAL_PENDING {
                    return Err(ScenarioError::ProviderJournalLimit);
                }
                let file = open_provider_file_nofollow(directory, &name)
                    .map_err(|error| ScenarioError::UnsafeProviderJournal(error.to_string()))?;
                let metadata = validate_provider_regular_file(&file, &name)
                    .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?;
                let len = usize::try_from(metadata.len())
                    .map_err(|_| ScenarioError::ProviderJournalLimit)?;
                if (kind == "record" && len > MAX_PROVIDER_JOURNAL_RECORD_BYTES)
                    || (kind != "record" && len > MAX_PROVIDER_JOURNAL_BLOB_BYTES)
                {
                    return Err(ScenarioError::UnsafeProviderJournal(name));
                }
                bytes = bytes
                    .checked_add(len)
                    .ok_or(ScenarioError::ProviderJournalLimit)?;
                if bytes > MAX_PROVIDER_JOURNAL_BYTES {
                    return Err(ScenarioError::ProviderJournalLimit);
                }
            }
        }
        Ok(())
    }

    fn acquire_transaction_gate(&self) -> Result<ProviderTransactionGate, ScenarioError> {
        if self
            .transaction_authority
            .local_held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(ScenarioError::UnsafeProviderJournal(
                "provider transaction gate is busy".into(),
            ));
        }
        let lock_file = match provider_transaction_lock_handle(&self.transaction_authority) {
            Ok(lock_file) => lock_file,
            Err(error) => {
                self.transaction_authority
                    .local_held
                    .store(false, Ordering::Release);
                return Err(ScenarioError::Io(error.to_string()));
            }
        };
        let acquired = match provider_lock_file_exclusive_nonblocking(&lock_file) {
            Ok(acquired) => acquired,
            Err(error) => {
                self.transaction_authority
                    .local_held
                    .store(false, Ordering::Release);
                return Err(ScenarioError::Io(error.to_string()));
            }
        };
        if !acquired {
            self.transaction_authority
                .local_held
                .store(false, Ordering::Release);
            return Err(ScenarioError::UnsafeProviderJournal(
                "provider transaction gate is held by another process".into(),
            ));
        }
        let gate = ProviderTransactionGate {
            authority: Arc::clone(&self.transaction_authority),
            lock_file,
        };
        self.validate_transaction_binding(&gate)?;
        Ok(gate)
    }

    fn require_transaction_gate(
        &self,
        gate: &ProviderTransactionGate,
    ) -> Result<(), ScenarioError> {
        if Arc::ptr_eq(&self.transaction_authority, &gate.authority)
            && self
                .transaction_authority
                .local_held
                .load(Ordering::Acquire)
        {
            Ok(())
        } else {
            Err(ScenarioError::UnsafeProviderJournal(
                "wrong provider transaction gate".into(),
            ))
        }
    }

    fn validate_transaction_binding(
        &self,
        gate: &ProviderTransactionGate,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let authority = &self.transaction_authority;
        let named_device = open_provider_directory(
            &authority.device_parent,
            &authority.device_name,
        )
        .map_err(|_| {
            ScenarioError::UnsafeProviderJournal("device authority path was replaced".into())
        })?;
        if provider_directory_identity(&named_device)? != authority.device_identity
            || provider_directory_identity(&authority.device_directory)?
                != authority.device_identity
        {
            return Err(ScenarioError::UnsafeProviderJournal(
                "device authority identity changed".into(),
            ));
        }
        let mut named_outer = open_provider_outer_authority_file_nofollow(
            &authority.device_directory,
            PROVIDER_DEVICE_AUTHORITY_NAME,
        )?;
        if provider_file_identity(&named_outer)? != authority.authority_identity
            || provider_file_identity(&authority.authority_file)? != authority.authority_identity
        {
            return Err(ScenarioError::UnsafeProviderJournal(
                "outer provider authority identity changed".into(),
            ));
        }
        validate_local_file_bytes(
            &mut named_outer,
            &authority.authority_record_bytes,
            PROVIDER_DEVICE_AUTHORITY_NAME,
        )?;

        validate_named_provider_directory(
            &authority.device_directory,
            &self.name,
            &self.directory,
            self.directory_identity,
        )?;
        validate_named_provider_directory(
            &self.directory,
            "records",
            &self.records,
            self.records_identity,
        )?;
        validate_named_provider_directory(
            &self.directory,
            "blobs",
            &self.blobs,
            self.blobs_identity,
        )?;
        validate_named_provider_directory(
            &self.directory,
            "quarantine",
            &self.quarantine,
            self.quarantine_identity,
        )?;
        validate_named_provider_directory(
            &self.directory,
            "completed",
            &self.completed,
            self.completed_identity,
        )?;
        let mut named_key = open_provider_authority_key_nofollow(&self.directory, "authority.key")?;
        if provider_file_identity(&named_key)? != authority.authority_key_identity
            || provider_file_identity(&authority.authority_key_file)?
                != authority.authority_key_identity
        {
            return Err(ScenarioError::UnsafeProviderJournal(
                "authority.key identity changed".into(),
            ));
        }
        validate_local_file_bytes(&mut named_key, &self.authentication_key, "authority.key")?;

        let mut root_entries = 0_usize;
        for entry in self
            .directory
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            root_entries = root_entries
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            let file_type = entry
                .file_type()
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            let valid = match name.as_str() {
                "records" | "blobs" | "quarantine" | "completed" => file_type.is_dir(),
                "authority.key" => file_type.is_file(),
                _ => false,
            };
            if !valid {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
            if root_entries > 5 {
                return Err(ScenarioError::ProviderJournalLimit);
            }
        }
        Ok(())
    }

    fn validate_authenticated_graph(
        &self,
        gate: &ProviderTransactionGate,
    ) -> Result<Vec<String>, ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut blob_owners = BTreeMap::<String, usize>::new();
        let mut pending_files = 0_usize;
        let mut pending_bytes = 0_usize;
        for entry in self
            .records
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            pending_files = pending_files
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if pending_files > MAX_PROVIDER_JOURNAL_FILES {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            let operation_id = name
                .strip_suffix(".json")
                .or_else(|| name.strip_suffix(".update"))
                .filter(|value| valid_provider_journal_id(value))
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
            let opened = open_provider_regular_optional(
                &self.records,
                &name,
                MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                &name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
            pending_bytes = pending_bytes
                .checked_add(opened.bytes.len())
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if pending_bytes > MAX_PROVIDER_JOURNAL_BYTES {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let record = self.decode_record(&opened.bytes, &name)?;
            self.validate_record_shape(gate, &record, false)?;
            if record.operation_id != operation_id {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
            let mut canonical_owner = name.ends_with(".json");
            let mut creation_owner = false;
            if name.ends_with(".update") {
                let current_name = Self::record_name(operation_id);
                let current = open_provider_regular_optional(
                    &self.records,
                    &current_name,
                    MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                    &current_name,
                )
                .map_err(|_| ScenarioError::UnsafeProviderJournal(current_name.clone()))?;
                if let Some(current) = current {
                    let current_record = self.decode_record(&current.bytes, &current_name)?;
                    self.validate_record_shape(gate, &current_record, false)?;
                    if current_record.operation_id != record.operation_id
                        || provider_journal_phase_rank(record.phase)
                            < provider_journal_phase_rank(current_record.phase)
                    {
                        return Err(ScenarioError::UnsafeProviderJournal(name));
                    }
                } else {
                    if !matches!(
                        record.phase,
                        ProviderJournalPhase::Prepared | ProviderJournalPhase::Staged
                    ) {
                        return Err(ScenarioError::UnsafeProviderJournal(name));
                    }
                    canonical_owner = true;
                    creation_owner = true;
                }
            }
            if canonical_owner {
                if let Some(blob_name) = record.blob_name.as_ref() {
                    let creating_name = Self::creating_blob_name(&record.operation_id);
                    let blob_exists = self.blobs.exists(blob_name);
                    let creating_exists = self.blobs.exists(&creating_name);
                    let owner_name = if blob_exists && !creating_exists {
                        Some(blob_name.clone())
                    } else if !blob_exists && creating_exists && creation_owner {
                        Some(creating_name)
                    } else if !blob_exists
                        && !creating_exists
                        && record.phase == ProviderJournalPhase::Cleanup
                    {
                        None
                    } else {
                        return Err(ScenarioError::UnsafeProviderJournal(blob_name.clone()));
                    };
                    if let Some(owner_name) = owner_name {
                        let owners = blob_owners.entry(owner_name).or_default();
                        *owners = owners
                            .checked_add(1)
                            .ok_or(ScenarioError::ProviderJournalLimit)?;
                    }
                }
            }
        }

        let mut completed_files = 0_usize;
        for entry in self
            .completed
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            completed_files = completed_files
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if completed_files > MAX_PROVIDER_JOURNAL_COMPLETED + 1 {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            let operation_id = name
                .strip_suffix(".json")
                .or_else(|| name.strip_suffix(".update"))
                .filter(|value| valid_provider_journal_id(value))
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
            let opened = open_provider_regular_optional(
                &self.completed,
                &name,
                MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                &name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
            let record = self.decode_record(&opened.bytes, &name)?;
            self.validate_record_shape(gate, &record, false)?;
            if record.operation_id != operation_id || record.phase != ProviderJournalPhase::Cleanup
            {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
        }

        let mut blobs = 0_usize;
        let mut orphan_blobs = Vec::new();
        for entry in self
            .blobs
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            blobs = blobs
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if blobs > MAX_PROVIDER_JOURNAL_PENDING {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            let (operation_id, creating) = if let Some(operation_id) = name
                .strip_suffix(".blob")
                .filter(|value| valid_provider_journal_id(value))
            {
                (operation_id, false)
            } else if let Some(operation_id) = name
                .strip_suffix(".creating")
                .filter(|value| valid_provider_journal_id(value))
            {
                (operation_id, true)
            } else {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            };
            if name
                != if creating {
                    Self::creating_blob_name(operation_id)
                } else {
                    Self::blob_name(operation_id)
                }
                || blob_owners.get(&name).is_some_and(|owners| *owners != 1)
            {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
            let opened = open_provider_regular_optional(
                &self.blobs,
                &name,
                MAX_PROVIDER_JOURNAL_BLOB_BYTES,
                &name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
            pending_bytes = pending_bytes
                .checked_add(opened.bytes.len())
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if pending_bytes > MAX_PROVIDER_JOURNAL_BYTES {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            if !blob_owners.contains_key(&name) {
                if creating {
                    orphan_blobs.push(name);
                    continue;
                }
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
            let record_name = Self::record_name(operation_id);
            let (record, owner_name) = if creating {
                let update_name = format!("{operation_id}.update");
                let update = open_provider_regular_optional(
                    &self.records,
                    &update_name,
                    MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                    &update_name,
                )
                .map_err(|_| ScenarioError::UnsafeProviderJournal(update_name.clone()))?
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
                (update, update_name)
            } else {
                let record = open_provider_regular_optional(
                    &self.records,
                    &record_name,
                    MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                    &record_name,
                )
                .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?;
                if let Some(record) = record {
                    (record, record_name)
                } else {
                    let update_name = format!("{operation_id}.update");
                    let update = open_provider_regular_optional(
                        &self.records,
                        &update_name,
                        MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                        &update_name,
                    )
                    .map_err(|_| ScenarioError::UnsafeProviderJournal(update_name.clone()))?
                    .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
                    (update, update_name)
                }
            };
            let record = self.decode_record(&record.bytes, &owner_name)?;
            let expected_blob_name = Self::blob_name(operation_id);
            if record.blob_name.as_deref() != Some(expected_blob_name.as_str())
                || u64::try_from(opened.bytes.len()).ok() != Some(record.source_len)
                || provider_digest(&opened.bytes) != record.source_digest
            {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
        }
        if blob_owners
            .iter()
            .any(|(name, owners)| *owners != 1 || !self.blobs.exists(name))
        {
            return Err(ScenarioError::UnsafeProviderJournal(
                "missing or shared blob ownership".into(),
            ));
        }
        Ok(orphan_blobs)
    }

    fn authenticated_creation_owner(
        &self,
        gate: &ProviderTransactionGate,
        operation_id: &str,
    ) -> Result<Option<ProviderJournalRecord>, ScenarioError> {
        self.require_transaction_gate(gate)?;
        let record_name = Self::record_name(operation_id);
        if open_provider_regular_optional(
            &self.records,
            &record_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &record_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?
        .is_some()
        {
            return Err(ScenarioError::UnsafeProviderJournal(record_name));
        }
        let update_name = format!("{operation_id}.update");
        let Some(update) = open_provider_regular_optional(
            &self.records,
            &update_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &update_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(update_name.clone()))?
        else {
            return Ok(None);
        };
        let record = self.decode_record(&update.bytes, &update_name)?;
        self.validate_record_shape(gate, &record, false)?;
        if record.operation_id != operation_id
            || !matches!(
                record.phase,
                ProviderJournalPhase::Prepared | ProviderJournalPhase::Staged
            )
            || record.blob_name.as_deref() != Some(Self::blob_name(operation_id).as_str())
        {
            return Err(ScenarioError::UnsafeProviderJournal(update_name));
        }
        Ok(Some(record))
    }

    fn reconcile_orphan_quarantine(
        &self,
        gate: &ProviderTransactionGate,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut names = Vec::new();
        for entry in self
            .quarantine
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            if names.len() >= MAX_PROVIDER_JOURNAL_PENDING {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            if !entry
                .file_type()
                .map_err(|error| ScenarioError::Io(error.to_string()))?
                .is_file()
                || !name
                    .strip_suffix(".creating")
                    .is_some_and(valid_provider_journal_id)
            {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
            names.push(name);
        }
        names.sort();
        for name in names {
            self.resolve_orphan_quarantine(gate, &name)?;
        }
        Ok(())
    }

    fn resolve_orphan_quarantine(
        &self,
        gate: &ProviderTransactionGate,
        quarantine_name: &str,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let operation_id = quarantine_name
            .strip_suffix(".creating")
            .filter(|value| valid_provider_journal_id(value))
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(quarantine_name.into()))?;
        let quarantined = open_provider_regular_optional(
            &self.quarantine,
            quarantine_name,
            MAX_PROVIDER_JOURNAL_BLOB_BYTES,
            quarantine_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(quarantine_name.into()))?
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(quarantine_name.into()))?;
        let quarantined_identity = provider_file_identity(&quarantined.file)?;
        let owner = self.authenticated_creation_owner(gate, operation_id)?;
        provider_journal_boundary_hook(ProviderJournalBoundary::OrphanOwnershipRechecked)?;
        if let Some(owner) = owner {
            if u64::try_from(quarantined.bytes.len()).ok() != Some(owner.source_len)
                || provider_digest(&quarantined.bytes) != owner.source_digest
                || self.blobs.exists(quarantine_name)
                || self.blobs.exists(&Self::blob_name(operation_id))
            {
                return Err(ScenarioError::UnsafeProviderJournal(quarantine_name.into()));
            }
            provider_rename_named_noreplace(
                &self.quarantine,
                quarantine_name,
                &self.blobs,
                quarantine_name,
            )
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
            sync_provider_publication_directories(&self.blobs, Some(&self.quarantine))?;
            provider_journal_boundary_hook(ProviderJournalBoundary::OrphanRestored)?;
            return Ok(());
        }
        provider_orphan_before_private_delete_hook();
        let retained = open_provider_regular_optional(
            &self.quarantine,
            quarantine_name,
            MAX_PROVIDER_JOURNAL_BLOB_BYTES,
            quarantine_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(quarantine_name.into()))?
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(quarantine_name.into()))?;
        if provider_file_identity(&retained.file)? != quarantined_identity {
            return Err(ScenarioError::UnsafeProviderJournal(quarantine_name.into()));
        }
        self.quarantine
            .remove_file(quarantine_name)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        sync_provider_directory(&self.quarantine)?;
        provider_journal_boundary_hook(ProviderJournalBoundary::OrphanPrivateDeleted)
    }

    fn retire_orphan_blobs(
        &self,
        gate: &ProviderTransactionGate,
        orphan_blobs: &[String],
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        for blob_name in orphan_blobs {
            if self.quarantine.exists(blob_name) {
                return Err(ScenarioError::UnsafeProviderJournal(blob_name.clone()));
            }
            provider_rename_named_noreplace(&self.blobs, blob_name, &self.quarantine, blob_name)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            sync_provider_publication_directories(&self.quarantine, Some(&self.blobs))?;
            provider_journal_boundary_hook(ProviderJournalBoundary::OrphanQuarantined)?;
            provider_orphan_after_quarantine_hook();
            self.resolve_orphan_quarantine(gate, blob_name)?;
        }
        Ok(())
    }

    fn operation_id(
        operation: ProviderJournalOperation,
        operation_binding: &str,
        source_provenance: &str,
        tree: ProviderTree,
        from_path: &str,
        to_path: Option<&str>,
        source_len: u64,
        source_digest: &str,
    ) -> String {
        let mut digest = Sha256::new();
        digest.update(b"tine-provider-local-journal-operation-v2\0");
        digest.update(match operation {
            ProviderJournalOperation::Put => b"put".as_slice(),
            ProviderJournalOperation::Rename => b"rename".as_slice(),
            ProviderJournalOperation::Remove => b"remove".as_slice(),
        });
        digest.update(b"\0");
        digest.update(operation_binding.as_bytes());
        digest.update(b"\0");
        digest.update(source_provenance.as_bytes());
        digest.update(b"\0");
        digest.update(provider_tree_name(tree).as_bytes());
        digest.update(b"\0");
        digest.update(from_path.as_bytes());
        digest.update(b"\0");
        if let Some(to_path) = to_path {
            digest.update(to_path.as_bytes());
        }
        digest.update(b"\0");
        digest.update(source_len.to_le_bytes());
        digest.update(b"\0");
        digest.update(source_digest.as_bytes());
        format!("{:x}", digest.finalize())
    }

    fn record_name(operation_id: &str) -> String {
        format!("{operation_id}.json")
    }

    fn blob_name(operation_id: &str) -> String {
        format!("{operation_id}.blob")
    }

    fn creating_blob_name(operation_id: &str) -> String {
        format!("{operation_id}.creating")
    }

    fn staging_name(operation_id: &str, generation: u32) -> String {
        format!("publish-{operation_id}-{generation}")
    }

    fn expected_staging_name(record: &ProviderJournalRecord) -> String {
        if record.operation == ProviderJournalOperation::Put && record.staging_generation == 0 {
            if let Some(transfer_id) = record.operation_binding.strip_prefix("transfer:") {
                return format!("{transfer_id}.part");
            }
        }
        Self::staging_name(&record.operation_id, record.staging_generation)
    }

    fn sign_record(&self, record: &mut ProviderJournalRecord) -> Result<(), ScenarioError> {
        record.authentication_tag.clear();
        let bytes =
            serde_json::to_vec(record).map_err(|error| ScenarioError::Io(error.to_string()))?;
        record.authentication_tag = hmac_sha256_hex(&self.authentication_key, &bytes);
        Ok(())
    }

    fn decode_record(
        &self,
        bytes: &[u8],
        name: &str,
    ) -> Result<ProviderJournalRecord, ScenarioError> {
        let record: ProviderJournalRecord = serde_json::from_slice(bytes)
            .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))?;
        let canonical =
            serde_json::to_vec(&record).map_err(|error| ScenarioError::Io(error.to_string()))?;
        if canonical != bytes || record.authentication_tag.len() != 64 {
            return Err(ScenarioError::UnsafeProviderJournal(name.into()));
        }
        let mut unsigned = record.clone();
        let supplied = std::mem::take(&mut unsigned.authentication_tag);
        let unsigned_bytes =
            serde_json::to_vec(&unsigned).map_err(|error| ScenarioError::Io(error.to_string()))?;
        let expected = hmac_sha256_hex(&self.authentication_key, &unsigned_bytes);
        if !constant_time_bytes_equal(supplied.as_bytes(), expected.as_bytes()) {
            return Err(ScenarioError::UnsafeProviderJournal(name.into()));
        }
        Ok(record)
    }

    fn reconcile_updates(&self, gate: &ProviderTransactionGate) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut scanned = 0_usize;
        let mut updates = Vec::new();
        for entry in self
            .records
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            scanned = scanned
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if scanned > MAX_PROVIDER_JOURNAL_FILES {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            if name.ends_with(".update") {
                updates.push(name);
            }
        }
        updates.sort();
        for update_name in updates {
            let operation_id = update_name
                .strip_suffix(".update")
                .filter(|value| {
                    value.len() == 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(update_name.clone()))?;
            let update = open_provider_regular_optional(
                &self.records,
                &update_name,
                MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                &update_name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(update_name.clone()))?
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(update_name.clone()))?;
            let update_record = self.decode_record(&update.bytes, &update_name)?;
            if update_record.operation_id != operation_id {
                return Err(ScenarioError::UnsafeProviderJournal(update_name));
            }
            self.validate_record_shape(gate, &update_record, false)?;
            let record_name = Self::record_name(operation_id);
            let current = open_provider_regular_optional(
                &self.records,
                &record_name,
                MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                &record_name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?;
            if let Some(current) = current.as_ref() {
                let current_record = self.decode_record(&current.bytes, &record_name)?;
                if current_record.operation_id != operation_id
                    || provider_journal_phase_rank(update_record.phase)
                        < provider_journal_phase_rank(current_record.phase)
                {
                    return Err(ScenarioError::UnsafeProviderJournal(update_name));
                }
            }
            if current.is_none() {
                if let Some(blob_name) = update_record.blob_name.as_deref() {
                    let creating_name = Self::creating_blob_name(operation_id);
                    if !self.blobs.exists(blob_name) {
                        let creating = open_provider_regular_optional(
                            &self.blobs,
                            &creating_name,
                            MAX_PROVIDER_JOURNAL_BLOB_BYTES,
                            &creating_name,
                        )
                        .map_err(|_| ScenarioError::UnsafeProviderJournal(creating_name.clone()))?
                        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(blob_name.into()))?;
                        if u64::try_from(creating.bytes.len()).ok()
                            != Some(update_record.source_len)
                            || provider_digest(&creating.bytes) != update_record.source_digest
                        {
                            return Err(ScenarioError::UnsafeProviderJournal(creating_name));
                        }
                        self.blobs
                            .rename(&creating_name, &self.blobs, blob_name)
                            .map_err(|error| ScenarioError::Io(error.to_string()))?;
                        sync_provider_directory(&self.blobs)?;
                        provider_journal_boundary_hook(ProviderJournalBoundary::BlobInstalled)?;
                    }
                }
            }
            self.records
                .rename(&update_name, &self.records, &record_name)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            sync_provider_directory(&self.records)?;
        }
        Ok(())
    }

    fn reconcile_completed_updates(
        &self,
        gate: &ProviderTransactionGate,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut scanned = 0_usize;
        let mut updates = Vec::new();
        for entry in self
            .completed
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            scanned = scanned
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if scanned > MAX_PROVIDER_JOURNAL_COMPLETED + 1 {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            if name.ends_with(".update") {
                updates.push(name);
            }
        }
        updates.sort();
        for update_name in updates {
            let operation_id = update_name
                .strip_suffix(".update")
                .filter(|value| {
                    value.len() == 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(update_name.clone()))?;
            let update = open_provider_regular_optional(
                &self.completed,
                &update_name,
                MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                &update_name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(update_name.clone()))?
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(update_name.clone()))?;
            let record = self.decode_record(&update.bytes, &update_name)?;
            if record.operation_id != operation_id || record.phase != ProviderJournalPhase::Cleanup
            {
                return Err(ScenarioError::UnsafeProviderJournal(update_name));
            }
            self.validate_record_shape(gate, &record, false)?;
            self.completed
                .rename(
                    &update_name,
                    &self.completed,
                    Self::record_name(operation_id),
                )
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            sync_provider_directory(&self.completed)?;
        }
        Ok(())
    }

    fn validate_completed_usage(
        &self,
        gate: &ProviderTransactionGate,
        additional_files: usize,
        additional_bytes: usize,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut files = 0_usize;
        let mut total_bytes = 0_usize;
        for entry in self
            .completed
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            files = files
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if files
                .checked_add(additional_files)
                .is_none_or(|files| files > MAX_PROVIDER_JOURNAL_COMPLETED)
            {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            if !name.ends_with(".json")
                || name.len() != 64 + ".json".len()
                || !name[..64]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
            let opened = open_provider_regular_optional(
                &self.completed,
                &name,
                MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                &name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
            total_bytes = total_bytes
                .checked_add(opened.bytes.len())
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if total_bytes
                .checked_add(additional_bytes)
                .is_none_or(|bytes| bytes > MAX_PROVIDER_JOURNAL_COMPLETION_BYTES)
            {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let record = self.decode_record(&opened.bytes, &name)?;
            if record.phase != ProviderJournalPhase::Cleanup
                || Self::record_name(&record.operation_id) != name
            {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
            self.validate_record_shape(gate, &record, false)?;
        }
        Ok(())
    }

    fn validate_usage(
        &self,
        gate: &ProviderTransactionGate,
        additional_blob_bytes: usize,
        additional_record_bytes: usize,
        reserve_record: bool,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut pending = 0_usize;
        let mut files = 0_usize;
        let mut total_bytes = 0_usize;
        for (directory, count_pending, quarantine) in [
            (&self.records, true, false),
            (&self.blobs, false, false),
            (&self.quarantine, false, true),
        ] {
            for entry in directory
                .entries()
                .map_err(|error| ScenarioError::Io(error.to_string()))?
            {
                let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
                files = files
                    .checked_add(1)
                    .ok_or(ScenarioError::ProviderJournalLimit)?;
                if files > MAX_PROVIDER_JOURNAL_FILES {
                    return Err(ScenarioError::ProviderJournalLimit);
                }
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
                let valid_name = if count_pending {
                    name.strip_suffix(".json")
                } else if quarantine {
                    name.strip_suffix(".creating")
                } else {
                    name.strip_suffix(".blob")
                        .or_else(|| name.strip_suffix(".creating"))
                }
                .is_some_and(valid_provider_journal_id);
                if !valid_name
                    || !entry
                        .file_type()
                        .map_err(|error| ScenarioError::Io(error.to_string()))?
                        .is_file()
                {
                    return Err(ScenarioError::UnsafeProviderJournal(name));
                }
                let file = open_provider_file_nofollow(directory, &name)
                    .map_err(|error| ScenarioError::UnsafeProviderJournal(error.to_string()))?;
                let metadata = validate_provider_regular_file(&file, &name)
                    .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?;
                let len = usize::try_from(metadata.len())
                    .map_err(|_| ScenarioError::ProviderJournalLimit)?;
                if count_pending {
                    pending = pending
                        .checked_add(1)
                        .ok_or(ScenarioError::ProviderJournalLimit)?;
                    if pending > MAX_PROVIDER_JOURNAL_PENDING {
                        return Err(ScenarioError::ProviderJournalLimit);
                    }
                    if len > MAX_PROVIDER_JOURNAL_RECORD_BYTES {
                        return Err(ScenarioError::UnsafeProviderJournal(name));
                    }
                }
                total_bytes = total_bytes
                    .checked_add(len)
                    .ok_or(ScenarioError::ProviderJournalLimit)?;
                if total_bytes > MAX_PROVIDER_JOURNAL_BYTES {
                    return Err(ScenarioError::ProviderJournalLimit);
                }
            }
        }
        total_bytes = total_bytes
            .checked_add(additional_blob_bytes)
            .and_then(|total| total.checked_add(additional_record_bytes))
            .ok_or(ScenarioError::ProviderJournalLimit)?;
        if pending + usize::from(reserve_record) > MAX_PROVIDER_JOURNAL_PENDING
            || files + usize::from(reserve_record) + usize::from(additional_blob_bytes != 0)
                > MAX_PROVIDER_JOURNAL_FILES - 1
            || total_bytes > MAX_PROVIDER_JOURNAL_BYTES
            || additional_blob_bytes > MAX_PROVIDER_JOURNAL_BLOB_BYTES
        {
            return Err(ScenarioError::ProviderJournalLimit);
        }
        Ok(())
    }

    fn load(
        &self,
        gate: &ProviderTransactionGate,
        operation: ProviderJournalOperation,
        operation_binding: &str,
        source_provenance: &str,
        tree: ProviderTree,
        from_path: &str,
        to_path: Option<&str>,
    ) -> Result<Option<ProviderJournalRecord>, ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut found = None;
        for (directory, completed, limit) in [
            (&self.records, false, MAX_PROVIDER_JOURNAL_PENDING),
            (&self.completed, true, MAX_PROVIDER_JOURNAL_COMPLETED),
        ] {
            let mut scanned = 0_usize;
            for entry in directory
                .entries()
                .map_err(|error| ScenarioError::Io(error.to_string()))?
            {
                let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
                scanned = scanned
                    .checked_add(1)
                    .ok_or(ScenarioError::ProviderJournalLimit)?;
                if scanned > limit {
                    return Err(ScenarioError::ProviderJournalLimit);
                }
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
                let opened = open_provider_regular_optional(
                    directory,
                    &name,
                    MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                    &name,
                )
                .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
                let record = self.decode_record(&opened.bytes, &name)?;
                self.validate_record_shape(gate, &record, !completed)?;
                if completed && record.phase != ProviderJournalPhase::Cleanup {
                    return Err(ScenarioError::UnsafeProviderJournal(name));
                }
                if record.operation == operation
                    && record.operation_binding == operation_binding
                    && record.source_provenance == source_provenance
                    && record.tree == tree
                    && record.from_path == from_path
                    && record.to_path.as_deref() == to_path
                {
                    if found.replace(record).is_some() {
                        return Err(ScenarioError::UnsafeProviderJournal(
                            operation_binding.into(),
                        ));
                    }
                }
            }
            if found.is_some() {
                break;
            }
        }
        Ok(found)
    }

    /// Recycle an exact completed generated-put receipt only after its bound
    /// destination name has disappeared. The completed receipt authenticates
    /// the bytes but retains the identity of the old provider file, which
    /// cannot authorize a legitimate republish after complete namespace loss.
    fn recycle_completed_put_for_absent_destination(
        &self,
        gate: &ProviderTransactionGate,
        operation_binding: &str,
        source_provenance: &str,
        runtime: &ProviderRuntime,
        location: &ProviderLocation,
        bytes: &[u8],
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let source_len =
            u64::try_from(bytes.len()).map_err(|_| ScenarioError::ProviderJournalLimit)?;
        let source_digest = provider_digest(bytes);
        let operation_id = Self::operation_id(
            ProviderJournalOperation::Put,
            operation_binding,
            source_provenance,
            location.tree,
            &location.path,
            None,
            source_len,
            &source_digest,
        );
        let record_name = Self::record_name(&operation_id);
        let Some(opened) = open_provider_regular_optional(
            &self.completed,
            &record_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &record_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?
        else {
            return Ok(());
        };
        let record = self.decode_record(&opened.bytes, &record_name)?;
        self.validate_record_shape(gate, &record, false)?;
        if record.operation_id != operation_id
            || record.operation != ProviderJournalOperation::Put
            || record.operation_binding != operation_binding
            || record.source_provenance != source_provenance
            || record.tree != location.tree
            || record.from_path != location.path
            || record.to_path.is_some()
            || record.source_len != source_len
            || record.source_digest != source_digest
            || record.phase != ProviderJournalPhase::Cleanup
        {
            return Err(ScenarioError::UnsafeProviderJournal(record_name));
        }
        let (destination_dir, destination_name) =
            runtime.parent_and_name(location.tree, &location.path, true)?;
        if open_provider_regular_optional(
            &destination_dir,
            &destination_name,
            MAX_PROVIDER_RESCAN_BYTES,
            &location.path,
        )?
        .is_some()
        {
            return Ok(());
        }
        // A crash may leave the same authenticated Cleanup record in both
        // pending and completed directories. Preserve both copies for the
        // normal retry validator instead of discarding its completion proof.
        if open_provider_regular_optional(
            &self.records,
            &record_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &record_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?
        .is_some()
        {
            return Ok(());
        }
        self.completed
            .remove_file(&record_name)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        sync_provider_directory(&self.completed)
    }

    /// Recycle an exact completed remove only when complete provider namespace
    /// loss erased its authenticated diagnostic and the same canonical source
    /// bytes were subsequently republished. The new removal must capture the
    /// new file identity; the vanished old identity cannot authorize it.
    fn recycle_completed_remove_for_reappeared_source(
        &self,
        gate: &ProviderTransactionGate,
        operation_binding: &str,
        source_provenance: &str,
        provider: &ProviderRuntime,
        tree: ProviderTree,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let source_len =
            u64::try_from(bytes.len()).map_err(|_| ScenarioError::ProviderJournalLimit)?;
        let source_digest = provider_digest(bytes);
        let operation_id = Self::operation_id(
            ProviderJournalOperation::Remove,
            operation_binding,
            source_provenance,
            tree,
            path,
            None,
            source_len,
            &source_digest,
        );
        let record_name = Self::record_name(&operation_id);
        let Some(opened) = open_provider_regular_optional(
            &self.completed,
            &record_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &record_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?
        else {
            return Ok(());
        };
        let record = self.decode_record(&opened.bytes, &record_name)?;
        self.validate_record_shape(gate, &record, false)?;
        if record.operation_id != operation_id
            || record.operation != ProviderJournalOperation::Remove
            || record.operation_binding != operation_binding
            || record.source_provenance != source_provenance
            || record.tree != tree
            || record.from_path != path
            || record.to_path.is_some()
            || record.source_len != source_len
            || record.source_digest != source_digest
            || record.phase != ProviderJournalPhase::Cleanup
        {
            return Err(ScenarioError::UnsafeProviderJournal(record_name));
        }
        let diagnostic = record
            .diagnostic_path
            .as_deref()
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record_name.clone()))?;
        let (diagnostic_dir, diagnostic_name) =
            provider.parent_and_name(tree, diagnostic, false)?;
        if open_provider_regular_optional(
            &diagnostic_dir,
            &diagnostic_name,
            MAX_PROVIDER_RESCAN_BYTES,
            diagnostic,
        )?
        .is_some()
        {
            return Ok(());
        }
        if open_provider_regular_optional(
            &self.records,
            &record_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &record_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?
        .is_some()
        {
            return Ok(());
        }
        self.completed
            .remove_file(&record_name)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        sync_provider_directory(&self.completed)
    }

    fn load_put_for_binding(
        &self,
        gate: &ProviderTransactionGate,
        operation_binding: &str,
    ) -> Result<Option<ProviderJournalRecord>, ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut found = None;
        let mut scanned = 0_usize;
        for entry in self
            .records
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            scanned = scanned
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if scanned > MAX_PROVIDER_JOURNAL_PENDING {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            if !name.ends_with(".json") {
                return Err(ScenarioError::UnsafeProviderJournal(name));
            }
            let opened = open_provider_regular_optional(
                &self.records,
                &name,
                MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                &name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
            let record = self.decode_record(&opened.bytes, &name)?;
            self.validate_record_shape(gate, &record, true)?;
            if record.operation == ProviderJournalOperation::Put
                && record.operation_binding == operation_binding
            {
                if found.replace(record).is_some() {
                    return Err(ScenarioError::UnsafeProviderJournal(
                        operation_binding.into(),
                    ));
                }
            }
        }
        if found.is_some() {
            return Ok(found);
        }
        let mut completed_scanned = 0_usize;
        for entry in self
            .completed
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            completed_scanned = completed_scanned
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            if completed_scanned > MAX_PROVIDER_JOURNAL_COMPLETED {
                return Err(ScenarioError::ProviderJournalLimit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ScenarioError::UnsafeProviderJournal("non-UTF-8 entry".into()))?;
            let opened = open_provider_regular_optional(
                &self.completed,
                &name,
                MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                &name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(name.clone()))?
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.clone()))?;
            let record = self.decode_record(&opened.bytes, &name)?;
            self.validate_record_shape(gate, &record, false)?;
            if record.operation == ProviderJournalOperation::Put
                && record.operation_binding == operation_binding
            {
                if found.replace(record).is_some() {
                    return Err(ScenarioError::UnsafeProviderJournal(
                        operation_binding.into(),
                    ));
                }
            }
        }
        Ok(found)
    }

    fn validate_record(
        &self,
        gate: &ProviderTransactionGate,
        record: &ProviderJournalRecord,
    ) -> Result<(), ScenarioError> {
        self.validate_record_shape(gate, record, true)
    }

    fn validate_record_shape(
        &self,
        gate: &ProviderTransactionGate,
        record: &ProviderJournalRecord,
        validate_blob: bool,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let expected_operation_id = Self::operation_id(
            record.operation,
            &record.operation_binding,
            &record.source_provenance,
            record.tree,
            &record.from_path,
            record.to_path.as_deref(),
            record.source_len,
            &record.source_digest,
        );
        if record.journal_schema_version != PROVIDER_JOURNAL_SCHEMA_VERSION
            || record.operation_id != expected_operation_id
            || record
                .source_identity
                .as_ref()
                .is_some_and(|identity| !valid_provider_identity_record(identity))
            || record
                .staging_identity
                .as_ref()
                .is_some_and(|identity| !valid_provider_identity_record(identity))
            || record
                .destination_identity
                .as_ref()
                .is_some_and(|identity| !valid_provider_identity_record(identity))
            || record.operation_binding.is_empty()
            || record.operation_binding.len() > MAX_PROVIDER_PATH_BYTES
            || record.source_provenance.is_empty()
            || record.source_provenance.len() > MAX_PROVIDER_PATH_BYTES
            || !valid_provider_user_path(&record.from_path)
            || record
                .to_path
                .as_deref()
                .is_some_and(|path| !valid_provider_user_path(path))
            || record.source_digest.len() != 64
            || !record
                .source_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || record.source_len
                > u64::try_from(MAX_PROVIDER_JOURNAL_BLOB_BYTES).unwrap_or(u64::MAX)
        {
            return Err(ScenarioError::UnsafeProviderJournal(
                record.operation_id.clone(),
            ));
        }
        match record.operation {
            ProviderJournalOperation::Put | ProviderJournalOperation::Rename => {
                let expected_blob = Self::blob_name(&record.operation_id);
                if record.blob_name.as_deref() != Some(expected_blob.as_str())
                    || (record.operation == ProviderJournalOperation::Rename
                        && record.to_path.is_none())
                    || (record.operation == ProviderJournalOperation::Put
                        && record.to_path.is_some())
                    || (record.operation == ProviderJournalOperation::Put
                        && matches!(
                            record.phase,
                            ProviderJournalPhase::RetireIntent | ProviderJournalPhase::Retired
                        ))
                    || (record.operation == ProviderJournalOperation::Rename
                        && record.source_identity.is_none())
                {
                    return Err(ScenarioError::UnsafeProviderJournal(
                        record.operation_id.clone(),
                    ));
                }
                if validate_blob && record.phase != ProviderJournalPhase::Cleanup {
                    let bytes = self.read_blob(gate, record)?;
                    if u64::try_from(bytes.len()).ok() != Some(record.source_len)
                        || provider_digest(&bytes) != record.source_digest
                    {
                        return Err(ScenarioError::UnsafeProviderJournal(expected_blob));
                    }
                }
            }
            ProviderJournalOperation::Remove => {
                if record.blob_name.is_some()
                    || record.to_path.is_some()
                    || record.source_identity.is_none()
                    || matches!(
                        record.phase,
                        ProviderJournalPhase::Staged
                            | ProviderJournalPhase::PublishIntent
                            | ProviderJournalPhase::Published
                    )
                {
                    return Err(ScenarioError::UnsafeProviderJournal(
                        record.operation_id.clone(),
                    ));
                }
            }
        }
        let destination_required = matches!(
            record.phase,
            ProviderJournalPhase::Published
                | ProviderJournalPhase::RetireIntent
                | ProviderJournalPhase::Retired
                | ProviderJournalPhase::Cleanup
        ) && record.operation == ProviderJournalOperation::Rename;
        let put_destination_required = matches!(
            record.phase,
            ProviderJournalPhase::Published | ProviderJournalPhase::Cleanup
        ) && record.operation == ProviderJournalOperation::Put;
        let retirement_path_required = record.operation != ProviderJournalOperation::Put
            && matches!(
                record.phase,
                ProviderJournalPhase::RetireIntent
                    | ProviderJournalPhase::Retired
                    | ProviderJournalPhase::Cleanup
            );
        let staging_required = record.operation != ProviderJournalOperation::Remove
            && record.phase != ProviderJournalPhase::Cleanup;
        let staging_identity_required = record.operation != ProviderJournalOperation::Remove
            && matches!(
                record.phase,
                ProviderJournalPhase::Staged
                    | ProviderJournalPhase::PublishIntent
                    | ProviderJournalPhase::Published
                    | ProviderJournalPhase::RetireIntent
                    | ProviderJournalPhase::Retired
            );
        let removal_retirement_identity_allowed = record.operation
            == ProviderJournalOperation::Remove
            && matches!(
                record.phase,
                ProviderJournalPhase::RetireIntent | ProviderJournalPhase::Retired
            );
        let any_destination_required = destination_required || put_destination_required;
        let destination_allowed = any_destination_required
            || record.operation != ProviderJournalOperation::Remove
                && record.phase == ProviderJournalPhase::PublishIntent;
        if (any_destination_required && record.destination_identity.is_none())
            || (!destination_allowed && record.destination_identity.is_some())
            || (staging_identity_required && record.staging_identity.is_none())
            || (!staging_identity_required
                && !removal_retirement_identity_allowed
                && record.staging_identity.is_some())
            || staging_required != record.staging_name.is_some()
            || record
                .staging_name
                .as_deref()
                .is_some_and(|name| name != Self::expected_staging_name(record))
            || retirement_path_required != record.diagnostic_path.is_some()
            || record.diagnostic_path.as_deref().is_some_and(|path| {
                !path.starts_with(&format!("{PROVIDER_REMOVED_NAMESPACE}/"))
                    || !valid_provider_path(path)
                    || path
                        != format!(
                            "{PROVIDER_REMOVED_NAMESPACE}/retired-{}",
                            record.operation_id
                        )
            })
        {
            return Err(ScenarioError::UnsafeProviderJournal(
                record.operation_id.clone(),
            ));
        }
        Ok(())
    }

    fn read_blob(
        &self,
        gate: &ProviderTransactionGate,
        record: &ProviderJournalRecord,
    ) -> Result<Vec<u8>, ScenarioError> {
        self.require_transaction_gate(gate)?;
        let name = record
            .blob_name
            .as_deref()
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
        open_provider_regular_optional(&self.blobs, name, MAX_PROVIDER_JOURNAL_BLOB_BYTES, name)
            .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))?
            .map(|opened| opened.bytes)
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(name.into()))
    }

    fn create(
        &self,
        gate: &ProviderTransactionGate,
        record: &ProviderJournalRecord,
        blob: Option<&[u8]>,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut signed = record.clone();
        self.sign_record(&mut signed)?;
        let record_bytes =
            serde_json::to_vec(&signed).map_err(|error| ScenarioError::Io(error.to_string()))?;
        if record_bytes.len() > MAX_PROVIDER_JOURNAL_RECORD_BYTES {
            return Err(ScenarioError::ProviderJournalLimit);
        }
        let existing_blob = if let Some(name) = record.blob_name.as_deref() {
            open_provider_regular_optional(&self.blobs, name, MAX_PROVIDER_JOURNAL_BLOB_BYTES, name)
                .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))?
        } else {
            None
        };
        let creating_name = Self::creating_blob_name(&record.operation_id);
        let existing_creating_blob = if record.blob_name.is_some() {
            open_provider_regular_optional(
                &self.blobs,
                &creating_name,
                MAX_PROVIDER_JOURNAL_BLOB_BYTES,
                &creating_name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(creating_name.clone()))?
        } else {
            None
        };
        if existing_blob.is_some() && existing_creating_blob.is_some() {
            return Err(ScenarioError::UnsafeProviderJournal(
                record.operation_id.clone(),
            ));
        }
        self.validate_usage(
            gate,
            if existing_blob.is_some() || existing_creating_blob.is_some() {
                0
            } else {
                blob.map_or(0, <[u8]>::len)
            },
            record_bytes.len(),
            true,
        )?;
        let record_name = Self::record_name(&record.operation_id);
        let provisional_name = format!("{}.update", record.operation_id);
        if let Some(blob) = blob {
            let name = record
                .blob_name
                .as_deref()
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
            if existing_blob.is_some() {
                return Err(ScenarioError::UnsafeProviderJournal(name.into()));
            }
            // Crash-closed creation order:
            // 1. sync bounded bytes under an ownerless `.creating` name;
            // 2. sync the authenticated update that binds those exact bytes;
            // 3. promote the bytes to the canonical `.blob` and sync;
            // 4. install and sync the canonical record.
            // Reopen may delete only state (1). States (2) and (3) are
            // authenticated and deterministically finish the two promotions.
            provider_journal_boundary_hook(ProviderJournalBoundary::BeforeBlobDurable)?;
            if let Some(mut existing) = existing_creating_blob {
                validate_local_file_bytes(&mut existing.file, blob, &creating_name)?;
            } else {
                let mut file = create_local_file_exclusive(&self.blobs, &creating_name)?;
                file.write_all(blob)
                    .map_err(|error| ScenarioError::Io(error.to_string()))?;
                crate::durability_counters::sync_file(&file)
                    .map_err(|error| ScenarioError::Io(error.to_string()))?;
                validate_local_file_bytes(&mut file, blob, &creating_name)?;
                sync_provider_directory(&self.blobs)?;
            }
            provider_journal_boundary_hook(ProviderJournalBoundary::BlobDurable)?;
            let mut provisional = create_local_file_exclusive(&self.records, &provisional_name)?;
            provisional
                .write_all(&record_bytes)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            crate::durability_counters::sync_file(&provisional)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            validate_local_file_bytes(&mut provisional, &record_bytes, &provisional_name)?;
            sync_provider_directory(&self.records)?;
            provider_journal_boundary_hook(ProviderJournalBoundary::CreationRecordDurable)?;
            self.blobs
                .rename(&creating_name, &self.blobs, name)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            sync_provider_directory(&self.blobs)?;
            provider_journal_boundary_hook(ProviderJournalBoundary::BlobInstalled)?;
            self.records
                .rename(&provisional_name, &self.records, &record_name)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            sync_provider_directory(&self.records)?;
        } else {
            let mut file = create_local_file_exclusive(&self.records, &record_name)?;
            file.write_all(&record_bytes)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            crate::durability_counters::sync_file(&file)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            validate_local_file_bytes(&mut file, &record_bytes, &record_name)?;
            sync_provider_directory(&self.records)?;
        }
        provider_journal_boundary_hook(ProviderJournalBoundary::RecordDurable)
    }

    fn store(
        &self,
        gate: &ProviderTransactionGate,
        record: &ProviderJournalRecord,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        self.validate_record(gate, record)?;
        let mut signed = record.clone();
        self.sign_record(&mut signed)?;
        let bytes =
            serde_json::to_vec(&signed).map_err(|error| ScenarioError::Io(error.to_string()))?;
        if bytes.len() > MAX_PROVIDER_JOURNAL_RECORD_BYTES {
            return Err(ScenarioError::ProviderJournalLimit);
        }
        let temporary_name = format!("{}.update", record.operation_id);
        let mut temporary = create_local_file_exclusive(&self.records, &temporary_name)?;
        temporary
            .write_all(&bytes)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        crate::durability_counters::sync_file(&temporary)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        validate_local_file_bytes(&mut temporary, &bytes, &temporary_name)?;
        sync_provider_directory(&self.records)?;
        provider_journal_boundary_hook(ProviderJournalBoundary::UpdateDurable)?;
        self.records
            .rename(
                &temporary_name,
                &self.records,
                Self::record_name(&record.operation_id),
            )
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        sync_provider_directory(&self.records)?;
        provider_journal_boundary_hook(ProviderJournalBoundary::UpdateInstalled)
    }

    fn complete(
        &self,
        gate: &ProviderTransactionGate,
        record: &ProviderJournalRecord,
    ) -> Result<(), ScenarioError> {
        self.require_transaction_gate(gate)?;
        let mut cleanup = record.clone();
        cleanup.phase = ProviderJournalPhase::Cleanup;
        cleanup.staging_name = None;
        cleanup.staging_identity = None;
        let record_name = Self::record_name(&record.operation_id);
        if open_provider_regular_optional(
            &self.records,
            &record_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &record_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?
        .is_some()
        {
            self.store(gate, &cleanup)?;
            provider_journal_after_phase_hook(ProviderJournalPhase::Cleanup)?;
        }
        if let Some(blob_name) = record.blob_name.as_deref() {
            if open_provider_regular_optional(
                &self.blobs,
                blob_name,
                MAX_PROVIDER_JOURNAL_BLOB_BYTES,
                blob_name,
            )
            .map_err(|_| ScenarioError::UnsafeProviderJournal(blob_name.into()))?
            .is_some()
            {
                self.blobs
                    .remove_file(blob_name)
                    .map_err(|error| ScenarioError::Io(error.to_string()))?;
            }
            sync_provider_directory(&self.blobs)?;
            provider_journal_boundary_hook(ProviderJournalBoundary::BlobRemoved)?;
        }
        let mut signed = cleanup;
        self.sign_record(&mut signed)?;
        let completion_bytes =
            serde_json::to_vec(&signed).map_err(|error| ScenarioError::Io(error.to_string()))?;
        let completion_name = Self::record_name(&record.operation_id);
        if let Some(mut completed) = open_provider_regular_optional(
            &self.completed,
            &completion_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &completion_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(completion_name.clone()))?
        {
            validate_local_file_bytes(&mut completed.file, &completion_bytes, &completion_name)?;
        } else {
            self.validate_completed_usage(gate, 1, completion_bytes.len())?;
            let update_name = format!("{}.update", record.operation_id);
            let mut update = create_local_file_exclusive(&self.completed, &update_name)?;
            update
                .write_all(&completion_bytes)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            crate::durability_counters::sync_file(&update)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            validate_local_file_bytes(&mut update, &completion_bytes, &update_name)?;
            sync_provider_directory(&self.completed)?;
            self.completed
                .rename(&update_name, &self.completed, &completion_name)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            sync_provider_directory(&self.completed)?;
        }
        provider_journal_boundary_hook(ProviderJournalBoundary::CompletionDurable)?;
        if open_provider_regular_optional(
            &self.records,
            &record_name,
            MAX_PROVIDER_JOURNAL_RECORD_BYTES,
            &record_name,
        )
        .map_err(|_| ScenarioError::UnsafeProviderJournal(record_name.clone()))?
        .is_some()
        {
            self.records
                .remove_file(&record_name)
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            sync_provider_directory(&self.records)?;
        }
        provider_journal_boundary_hook(ProviderJournalBoundary::RecordRemoved)?;
        Ok(())
    }
}

struct ScenarioRoot(PathBuf);

impl ScenarioRoot {
    fn new() -> Result<Self, ScenarioError> {
        let path = std::env::temp_dir().join(format!("tine-oplog-simulator-{}", Uuid::new_v4()));
        fs::create_dir(&path).map_err(|error| ScenarioError::Io(error.to_string()))?;
        Ok(Self(path))
    }
}

impl Drop for ScenarioRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn provider_transaction_device_names(
    source: &ProviderSource,
    destination_device: &str,
) -> Vec<String> {
    let mut devices = BTreeSet::from([destination_device.to_owned()]);
    if let ProviderSource::Tree { location } = source {
        devices.insert(location.device.clone());
    }
    devices.into_iter().collect()
}

fn valid_name(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

/// Data-safe provider rename semantics: publish an independent destination
/// inode/file from the bounded local blob, durably validate its recorded
/// identity, then move the validated source into diagnostic `removed/`.
/// Unix retirement uses an atomic exchange with a single-link placeholder so
/// a racing replacement is preserved only as diagnostic residue. Windows
/// retirement is handle-bound. No provider-visible residue authorizes retry.
fn run_provider_rename_with(
    provider: &ProviderRuntime,
    journal: &ProviderRetryJournal,
    provider_name: &str,
    event_id: u64,
    tree: ProviderTree,
    from_path: &str,
    to_path: &str,
) -> Result<(), ScenarioError> {
    let gate = journal.acquire_transaction_gate()?;
    reject_provider_temporary_path(from_path)?;
    reject_provider_temporary_path(to_path)?;
    let (from_dir, from_name) = provider.parent_and_name(tree, from_path, false)?;
    if from_path == to_path {
        open_provider_regular_optional(
            &from_dir,
            &from_name,
            MAX_PROVIDER_RESCAN_BYTES,
            from_path,
        )?
        .ok_or_else(|| ScenarioError::UnknownProviderPath(from_path.into()))?;
        return Ok(());
    }
    let (to_dir, to_name) = provider.parent_and_name(tree, to_path, true)?;
    let operation_binding = format!(
        "event:{event_id}:rename:{}:{from_path}:{to_path}",
        provider_tree_name(tree)
    );
    let source_provenance = format!(
        "provider:{}:{}:{from_path}",
        provider_name,
        provider_tree_name(tree)
    );
    let mut record = match journal.load(
        &gate,
        ProviderJournalOperation::Rename,
        &operation_binding,
        &source_provenance,
        tree,
        from_path,
        Some(to_path),
    )? {
        Some(record) => record,
        None => {
            let source = open_provider_regular_optional(
                &from_dir,
                &from_name,
                MAX_PROVIDER_RESCAN_BYTES,
                from_path,
            )?
            .ok_or_else(|| ScenarioError::UnknownProviderPath(from_path.into()))?;
            let operation_id = ProviderRetryJournal::operation_id(
                ProviderJournalOperation::Rename,
                &operation_binding,
                &source_provenance,
                tree,
                from_path,
                Some(to_path),
                u64::try_from(source.bytes.len())
                    .map_err(|_| ScenarioError::ProviderJournalLimit)?,
                &provider_digest(&source.bytes),
            );
            let record = ProviderJournalRecord {
                journal_schema_version: PROVIDER_JOURNAL_SCHEMA_VERSION,
                operation_id: operation_id.clone(),
                operation: ProviderJournalOperation::Rename,
                operation_binding: operation_binding.clone(),
                source_provenance: source_provenance.clone(),
                tree,
                from_path: from_path.into(),
                to_path: Some(to_path.into()),
                source_identity: Some(provider_identity_record(provider_file_identity(
                    &source.file,
                )?)),
                source_len: u64::try_from(source.bytes.len())
                    .map_err(|_| ScenarioError::ProviderJournalLimit)?,
                source_digest: provider_digest(&source.bytes),
                blob_name: Some(ProviderRetryJournal::blob_name(&operation_id)),
                phase: ProviderJournalPhase::Prepared,
                staging_identity: None,
                destination_identity: None,
                staging_name: Some(ProviderRetryJournal::staging_name(&operation_id, 0)),
                staging_generation: 0,
                diagnostic_path: None,
                authentication_tag: String::new(),
            };
            journal.create(&gate, &record, Some(&source.bytes))?;
            provider_journal_after_phase_hook(ProviderJournalPhase::Prepared)?;
            provider_post_validation_hook(ProviderPostValidationOperation::Rename);
            record
        }
    };
    let removed = open_provider_directory(provider.tree(tree), PROVIDER_REMOVED_NAMESPACE)?;
    let retirement_evidence =
        open_provider_directory(provider.tree(tree), PROVIDER_RENAME_EVIDENCE_NAMESPACE)?;
    let expected = if record.phase == ProviderJournalPhase::Cleanup {
        open_provider_regular_optional(&to_dir, &to_name, MAX_PROVIDER_RESCAN_BYTES, to_path)?
            .ok_or_else(|| ScenarioError::UnsafeProviderEntry(to_path.into()))?
            .bytes
    } else {
        journal.read_blob(&gate, &record)?
    };
    if record.phase == ProviderJournalPhase::Cleanup {
        validate_journal_destination(journal, &gate, &provider, &record, &expected, &removed)?;
        validate_retired_source(&provider, &record)?;
        return journal.complete(&gate, &record);
    }
    let temporary_dir = open_provider_directory(provider.tree(tree), PROVIDER_TEMP_NAMESPACE)?;
    if record.phase == ProviderJournalPhase::Prepared {
        if open_provider_regular_optional(&to_dir, &to_name, MAX_PROVIDER_RESCAN_BYTES, to_path)?
            .is_some()
        {
            return Err(ScenarioError::ProviderConflictingBytes(to_path.into()));
        }
        loop {
            let staging_name = record
                .staging_name
                .as_deref()
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
            if open_provider_regular_optional(
                &temporary_dir,
                staging_name,
                MAX_PROVIDER_RESCAN_BYTES,
                staging_name,
            )?
            .is_none()
            {
                break;
            }
            quarantine_unowned_staging(
                journal,
                &gate,
                &temporary_dir,
                staging_name,
                provider.tree(tree),
                &record.operation_id,
                record.staging_generation,
            )?;
            record.staging_generation = record
                .staging_generation
                .checked_add(1)
                .ok_or(ScenarioError::ProviderJournalLimit)?;
            record.staging_name = Some(ProviderRetryJournal::staging_name(
                &record.operation_id,
                record.staging_generation,
            ));
            journal.store(&gate, &record)?;
        }
        let staging_name = record
            .staging_name
            .as_deref()
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
        let mut staged = create_provider_journal_staging(&temporary_dir, staging_name, to_path)?;
        staged
            .write_all(&expected)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        crate::durability_counters::sync_file(&staged.file)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        validate_provider_file_bytes(&mut staged, &expected, to_path)?;
        record.staging_identity = Some(provider_identity_record(provider_file_identity(
            &staged.file,
        )?));
        record.phase = ProviderJournalPhase::Staged;
        journal.store(&gate, &record)?;
        provider_journal_after_phase_hook(ProviderJournalPhase::Staged)?;
    }

    if record.phase == ProviderJournalPhase::Staged {
        validate_journal_staging(&temporary_dir, &record, &expected, to_path)?;
        record.phase = ProviderJournalPhase::PublishIntent;
        journal.store(&gate, &record)?;
        provider_journal_after_phase_hook(ProviderJournalPhase::PublishIntent)?;
    }
    if record.phase == ProviderJournalPhase::PublishIntent {
        publish_journal_destination(
            journal,
            &gate,
            &mut record,
            &temporary_dir,
            provider.tree(tree),
            &to_dir,
            &to_name,
            &expected,
            to_path,
        )?;
        sync_shared_provider_publication_directories(&to_dir, Some(&temporary_dir))?;
        record.phase = ProviderJournalPhase::Published;
        journal.store(&gate, &record)?;
        provider_journal_after_phase_hook(ProviderJournalPhase::Published)?;
    }

    validate_journal_destination(journal, &gate, &provider, &record, &expected, &removed)?;
    if record.phase == ProviderJournalPhase::Published {
        record.diagnostic_path = Some(format!(
            "{PROVIDER_REMOVED_NAMESPACE}/retired-{}",
            record.operation_id
        ));
        record.phase = ProviderJournalPhase::RetireIntent;
        journal.store(&gate, &record)?;
        provider_journal_after_phase_hook(ProviderJournalPhase::RetireIntent)?;
    }
    if record.phase == ProviderJournalPhase::RetireIntent {
        reconcile_provider_retirement(
            journal,
            &gate,
            &from_dir,
            &from_name,
            &removed,
            &retirement_evidence,
            from_path,
            &mut record,
        )?;
        provider_rename_after_move_hook()?;
        record.phase = ProviderJournalPhase::Retired;
        journal.store(&gate, &record)?;
        provider_journal_after_phase_hook(ProviderJournalPhase::Retired)?;
    }
    validate_retired_source(&provider, &record)?;
    journal.complete(&gate, &record)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderRemoveMissingSourcePolicy {
    /// Generic scheduled removes and proof-bound conflict cleanup must observe
    /// the source they are authorized to remove.
    RequirePresent,
    /// Retirement owns the exact path already; provider-side convergence may
    /// have completed the removal before this device starts its local journal.
    SettleIfAbsent,
}

fn run_provider_remove_with(
    provider: &ProviderRuntime,
    journal: &ProviderRetryJournal,
    provider_name: &str,
    event_id: u64,
    tree: ProviderTree,
    path: &str,
    identical_canonical_path: Option<&str>,
    missing_source_policy: ProviderRemoveMissingSourcePolicy,
) -> Result<(), ScenarioError> {
    let gate = journal.acquire_transaction_gate()?;
    reject_provider_temporary_path(path)?;
    let canonical_bytes = identical_canonical_path
        .map(|canonical_path| {
            reject_provider_temporary_path(canonical_path)?;
            let (parent, name) = provider.parent_and_name(tree, canonical_path, false)?;
            open_provider_regular_optional(
                &parent,
                &name,
                MAX_PROVIDER_RESCAN_BYTES,
                canonical_path,
            )?
            .map(|opened| opened.bytes)
            .ok_or_else(|| ScenarioError::ProviderConflictingBytes(path.into()))
        })
        .transpose()?;
    let (parent, name) = provider.parent_and_name(tree, path, false)?;
    let operation_binding = format!(
        "event:{event_id}:remove:{}:{path}",
        provider_tree_name(tree)
    );
    let source_provenance = format!(
        "provider:{}:{}:{path}",
        provider_name,
        provider_tree_name(tree)
    );
    if let Some(source) =
        open_provider_regular_optional(&parent, &name, MAX_PROVIDER_RESCAN_BYTES, path)?
    {
        journal.recycle_completed_remove_for_reappeared_source(
            &gate,
            &operation_binding,
            &source_provenance,
            provider,
            tree,
            path,
            &source.bytes,
        )?;
    }
    let mut record = match journal.load(
        &gate,
        ProviderJournalOperation::Remove,
        &operation_binding,
        &source_provenance,
        tree,
        path,
        None,
    )? {
        Some(record) => record,
        None => {
            let Some(source) =
                open_provider_regular_optional(&parent, &name, MAX_PROVIDER_RESCAN_BYTES, path)?
            else {
                return match missing_source_policy {
                    ProviderRemoveMissingSourcePolicy::RequirePresent => {
                        Err(ScenarioError::UnknownProviderPath(path.into()))
                    }
                    ProviderRemoveMissingSourcePolicy::SettleIfAbsent => Ok(()),
                };
            };
            if canonical_bytes
                .as_ref()
                .is_some_and(|canonical| canonical != &source.bytes)
            {
                return Err(ScenarioError::ProviderConflictingBytes(path.into()));
            }
            let operation_id = ProviderRetryJournal::operation_id(
                ProviderJournalOperation::Remove,
                &operation_binding,
                &source_provenance,
                tree,
                path,
                None,
                u64::try_from(source.bytes.len())
                    .map_err(|_| ScenarioError::ProviderJournalLimit)?,
                &provider_digest(&source.bytes),
            );
            let record = ProviderJournalRecord {
                journal_schema_version: PROVIDER_JOURNAL_SCHEMA_VERSION,
                operation_id: operation_id.clone(),
                operation: ProviderJournalOperation::Remove,
                operation_binding: operation_binding.clone(),
                source_provenance: source_provenance.clone(),
                tree,
                from_path: path.into(),
                to_path: None,
                source_identity: Some(provider_identity_record(provider_file_identity(
                    &source.file,
                )?)),
                source_len: u64::try_from(source.bytes.len())
                    .map_err(|_| ScenarioError::ProviderJournalLimit)?,
                source_digest: provider_digest(&source.bytes),
                blob_name: None,
                phase: ProviderJournalPhase::Prepared,
                staging_identity: None,
                destination_identity: None,
                staging_name: None,
                staging_generation: 0,
                diagnostic_path: None,
                authentication_tag: String::new(),
            };
            journal.create(&gate, &record, None)?;
            provider_journal_after_phase_hook(ProviderJournalPhase::Prepared)?;
            provider_post_validation_hook(ProviderPostValidationOperation::Remove);
            record
        }
    };
    if canonical_bytes.as_ref().is_some_and(|canonical| {
        record.source_digest != provider_digest(canonical)
            || u64::try_from(canonical.len()).ok() != Some(record.source_len)
    }) {
        return Err(ScenarioError::ProviderConflictingBytes(path.into()));
    }
    let removed = open_provider_directory(provider.tree(tree), PROVIDER_REMOVED_NAMESPACE)?;
    let retirement_evidence =
        open_provider_directory(provider.tree(tree), PROVIDER_RENAME_EVIDENCE_NAMESPACE)?;
    if record.phase == ProviderJournalPhase::Cleanup {
        validate_retired_source(provider, &record)?;
        return journal.complete(&gate, &record);
    }
    if record.phase == ProviderJournalPhase::Prepared {
        ensure_provider_diagnostic_capacity(&removed, PROVIDER_REMOVED_NAMESPACE, 1)?;
        record.diagnostic_path = Some(format!(
            "{PROVIDER_REMOVED_NAMESPACE}/retired-{}",
            record.operation_id
        ));
        record.phase = ProviderJournalPhase::RetireIntent;
        journal.store(&gate, &record)?;
        provider_journal_after_phase_hook(ProviderJournalPhase::RetireIntent)?;
    }
    if record.phase == ProviderJournalPhase::RetireIntent {
        reconcile_provider_retirement(
            journal,
            &gate,
            &parent,
            &name,
            &removed,
            &retirement_evidence,
            path,
            &mut record,
        )?;
        record.phase = ProviderJournalPhase::Retired;
        journal.store(&gate, &record)?;
        provider_journal_after_phase_hook(ProviderJournalPhase::Retired)?;
    }
    validate_retired_source(provider, &record)?;
    journal.complete(&gate, &record)
}

fn validate_journal_destination(
    journal: &ProviderRetryJournal,
    gate: &ProviderTransactionGate,
    provider: &ProviderRuntime,
    record: &ProviderJournalRecord,
    expected: &[u8],
    removed: &Dir,
) -> Result<(), ScenarioError> {
    journal.require_transaction_gate(gate)?;
    let to_path = record
        .to_path
        .as_deref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let expected_identity = record
        .destination_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let (parent, name) = provider.parent_and_name(record.tree, to_path, false)?;
    let destination =
        match open_provider_regular_optional(&parent, &name, MAX_PROVIDER_RESCAN_BYTES, to_path) {
            Ok(Some(destination)) => destination,
            Ok(None) => return Err(ScenarioError::UnsafeProviderEntry(to_path.into())),
            Err(_) => {
                quarantine_provider_name(
                    journal,
                    gate,
                    &parent,
                    &name,
                    removed,
                    "destination-mismatch",
                )?;
                return Err(ScenarioError::UnsafeProviderEntry(to_path.into()));
            }
        };
    if destination.bytes != expected
        || !provider_file_matches_identity(&destination.file, expected_identity)?
    {
        quarantine_provider_name(
            journal,
            gate,
            &parent,
            &name,
            removed,
            "destination-mismatch",
        )?;
        return Err(ScenarioError::UnsafeProviderEntry(to_path.into()));
    }
    Ok(())
}

fn validate_retired_source(
    provider: &ProviderRuntime,
    record: &ProviderJournalRecord,
) -> Result<(), ScenarioError> {
    if !matches!(
        record.phase,
        ProviderJournalPhase::Retired | ProviderJournalPhase::Cleanup
    ) {
        return Err(ScenarioError::UnsafeProviderJournal(
            record.operation_id.clone(),
        ));
    }
    let diagnostic_path = record
        .diagnostic_path
        .as_deref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let (parent, name) = provider.parent_and_name(record.tree, diagnostic_path, false)?;
    let source =
        open_provider_regular_optional(&parent, &name, MAX_PROVIDER_RESCAN_BYTES, diagnostic_path)?
            .ok_or_else(|| ScenarioError::UnsafeProviderEntry(diagnostic_path.into()))?;
    let identity = record
        .source_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    if !provider_file_matches_identity(&source.file, identity)?
        || provider_digest(&source.bytes) != record.source_digest
        || u64::try_from(source.bytes.len()).ok() != Some(record.source_len)
    {
        return Err(ScenarioError::UnsafeProviderEntry(diagnostic_path.into()));
    }
    Ok(())
}

fn validate_retired_file(
    removed: &Dir,
    diagnostic_name: &str,
    diagnostic_path: &str,
    record: &ProviderJournalRecord,
) -> Result<(), ScenarioError> {
    let identity = record
        .source_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let retired = open_provider_regular_optional(
        removed,
        diagnostic_name,
        MAX_PROVIDER_RESCAN_BYTES,
        diagnostic_path,
    )?
    .ok_or_else(|| ScenarioError::UnsafeProviderEntry(diagnostic_path.into()))?;
    if !provider_file_matches_identity(&retired.file, identity)?
        || provider_digest(&retired.bytes) != record.source_digest
        || u64::try_from(retired.bytes.len()).ok() != Some(record.source_len)
    {
        return Err(ScenarioError::UnsafeProviderEntry(diagnostic_path.into()));
    }
    Ok(())
}

fn validate_provider_name_identity_or_quarantine(
    journal: &ProviderRetryJournal,
    gate: &ProviderTransactionGate,
    parent: &Dir,
    name: &str,
    retained: &fs::File,
    removed: &Dir,
    path: &str,
) -> Result<(), ScenarioError> {
    journal.require_transaction_gate(gate)?;
    let named = match open_provider_regular_optional(parent, name, MAX_PROVIDER_RESCAN_BYTES, path)
    {
        Ok(named) => named,
        Err(_) => {
            quarantine_provider_name(journal, gate, parent, name, removed, "destination-race")?;
            return Err(ScenarioError::UnsafeProviderEntry(path.into()));
        }
    };
    if let Some(named) = named.as_ref() {
        if provider_files_have_same_identity(retained, &named.file)? {
            return Ok(());
        }
    }
    if named.is_some() {
        quarantine_provider_name(journal, gate, parent, name, removed, "destination-race")?;
    }
    Err(ScenarioError::UnsafeProviderEntry(path.into()))
}

fn quarantine_provider_name(
    journal: &ProviderRetryJournal,
    gate: &ProviderTransactionGate,
    source_dir: &Dir,
    source_name: &str,
    removed: &Dir,
    prefix: &str,
) -> Result<(), ScenarioError> {
    journal.require_transaction_gate(gate)?;
    ensure_provider_diagnostic_capacity(removed, PROVIDER_REMOVED_NAMESPACE, 1)?;
    let source = open_provider_regular_optional(
        source_dir,
        source_name,
        MAX_PROVIDER_RESCAN_BYTES,
        source_name,
    )?
    .ok_or_else(|| ScenarioError::UnsafeProviderEntry(source_name.into()))?;
    let diagnostic_name = provider_quarantine_diagnostic_name(prefix, source_name, &source.bytes);
    if shared_diagnostic_name_is_taken(
        removed,
        &diagnostic_name,
        &format!("{PROVIDER_REMOVED_NAMESPACE}/{diagnostic_name}"),
    )? {
        return Err(ScenarioError::UnsafeProviderEntry(format!(
            "{PROVIDER_REMOVED_NAMESPACE}/{diagnostic_name}"
        )));
    }
    // RECONSTRUCTIBLE, but only because the fallback keeps the no-clobber
    // guarantee. These are FOREIGN bytes that took a name we expected to own, so
    // the graph is not their authority and they must not be destroyed — and they
    // are not: an occupied destination fails the exclusive reservation before
    // anything moves, and a rename that then fails leaves the foreign file
    // exactly where it was.
    provider_rename_reconstructible_noreplace(
        source_dir,
        source_name,
        removed,
        &diagnostic_name,
        "quarantining a raced shared provider name",
    )
    .map_err(|error| ScenarioError::Io(error.to_string()))?;
    sync_shared_provider_publication_directories(removed, Some(source_dir))
}

fn provider_quarantine_diagnostic_name(prefix: &str, source_name: &str, bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"tine-provider-diagnostic-name-v1\0");
    digest.update(prefix.as_bytes());
    digest.update(b"\0");
    digest.update(source_name.as_bytes());
    digest.update(b"\0");
    digest.update(provider_digest(bytes).as_bytes());
    format!("{prefix}-{:x}", digest.finalize())
}

fn ensure_provider_diagnostic_capacity(
    directory: &Dir,
    namespace: &str,
    additional_entries: usize,
) -> Result<(), ScenarioError> {
    let mut entries = 0_usize;
    for entry in directory
        .entries()
        .map_err(|error| ScenarioError::Io(error.to_string()))?
    {
        let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
        entries = entries
            .checked_add(1)
            .ok_or(ScenarioError::ProviderRescanLimit)?;
        if entries
            .checked_add(additional_entries)
            .is_none_or(|entries| entries > MAX_PROVIDER_RESIDUE_ENTRIES)
        {
            return Err(ScenarioError::ProviderRescanLimit);
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ScenarioError::UnsafeProviderEntry(format!("{namespace}/non-UTF-8")))?;
        if !entry
            .file_type()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
            .is_file()
        {
            return Err(ScenarioError::UnsafeProviderEntry(format!(
                "{namespace}/{name}"
            )));
        }
        let file = open_provider_file_nofollow(directory, &name)
            .map_err(|error| ScenarioError::UnsafeProviderEntry(error.to_string()))?;
        validate_provider_regular_file(&file, &format!("{namespace}/{name}"))?;
    }
    Ok(())
}

fn reconcile_provider_retirement(
    journal: &ProviderRetryJournal,
    gate: &ProviderTransactionGate,
    source_dir: &Dir,
    source_name: &str,
    removed: &Dir,
    evidence: &Dir,
    source_path: &str,
    record: &mut ProviderJournalRecord,
) -> Result<(), ScenarioError> {
    journal.require_transaction_gate(gate)?;
    let identity = record
        .source_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let diagnostic_path = record
        .diagnostic_path
        .as_deref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let diagnostic_name = diagnostic_path
        .strip_prefix(&format!("{PROVIDER_REMOVED_NAMESPACE}/"))
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let evidence_name = format!("retire-placeholder-{}", record.operation_id);
    ensure_provider_retirement_evidence(evidence, 0, 0)?;

    #[cfg(windows)]
    {
        let source = open_provider_regular_optional(
            source_dir,
            source_name,
            MAX_PROVIDER_RESCAN_BYTES,
            source_path,
        )?;
        let retired = open_provider_regular_optional(
            removed,
            diagnostic_name,
            MAX_PROVIDER_RESCAN_BYTES,
            diagnostic_path,
        )?;
        match (source, retired) {
            (Some(source), None) => {
                if !provider_file_matches_identity(&source.file, identity)?
                    || provider_digest(&source.bytes) != record.source_digest
                    || u64::try_from(source.bytes.len()).ok() != Some(record.source_len)
                {
                    return Err(ScenarioError::UnsafeProviderEntry(source_path.into()));
                }
                provider_retirement_after_validation_hook();
                provider_rename_handle_noreplace(&source.file, removed, diagnostic_name)
                    .map_err(|error| ScenarioError::Io(error.to_string()))?;
                sync_shared_provider_publication_directories(removed, Some(source_dir))?;
            }
            (None, Some(retired))
                if provider_file_matches_identity(&retired.file, identity)?
                    && provider_digest(&retired.bytes) == record.source_digest
                    && u64::try_from(retired.bytes.len()).ok() == Some(record.source_len) => {}
            (Some(_), Some(_)) => {
                return Err(ScenarioError::UnsafeProviderEntry(diagnostic_path.into()));
            }
            _ => return Err(ScenarioError::UnsafeProviderEntry(source_path.into())),
        }
    }

    #[cfg(unix)]
    {
        reconcile_private_retirement_evidence(evidence, &evidence_name, record)?;
        let mut source = open_provider_regular_optional(
            source_dir,
            source_name,
            MAX_PROVIDER_RESCAN_BYTES,
            source_path,
        )?;
        let mut retired = open_provider_regular_optional(
            removed,
            diagnostic_name,
            MAX_PROVIDER_RESCAN_BYTES,
            diagnostic_path,
        )?;

        if source.is_none() {
            let retired = retired
                .as_ref()
                .ok_or_else(|| ScenarioError::UnsafeProviderEntry(source_path.into()))?;
            if !provider_file_matches_identity(&retired.file, identity)?
                || provider_digest(&retired.bytes) != record.source_digest
                || u64::try_from(retired.bytes.len()).ok() != Some(record.source_len)
            {
                return Err(ScenarioError::UnsafeProviderEntry(diagnostic_path.into()));
            }
        } else {
            if retired.is_none() {
                let opened = source.as_ref().unwrap();
                if !provider_file_matches_identity(&opened.file, identity)?
                    || provider_digest(&opened.bytes) != record.source_digest
                    || u64::try_from(opened.bytes.len()).ok() != Some(record.source_len)
                {
                    return Err(ScenarioError::UnsafeProviderEntry(source_path.into()));
                }
                ensure_provider_diagnostic_capacity(removed, PROVIDER_REMOVED_NAMESPACE, 1)?;
                let placeholder = create_provider_destination_exclusive(
                    removed,
                    diagnostic_name,
                    diagnostic_path,
                )?;
                crate::durability_counters::sync_file(&placeholder)
                    .map_err(|error| ScenarioError::Io(error.to_string()))?;
                record.staging_identity = Some(provider_identity_record(provider_file_identity(
                    &placeholder,
                )?));
                journal.store(gate, record)?;
                sync_shared_provider_directory(removed)?;
                provider_journal_boundary_hook(
                    ProviderJournalBoundary::RetirementPlaceholderDurable,
                )?;
                retired = open_provider_regular_optional(
                    removed,
                    diagnostic_name,
                    MAX_PROVIDER_RESCAN_BYTES,
                    diagnostic_path,
                )?;
            }

            let placeholder_identity = record
                .staging_identity
                .as_ref()
                .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
            let source_is_original = source.as_ref().is_some_and(|opened| {
                provider_file_matches_identity(&opened.file, identity).unwrap_or(false)
                    && provider_digest(&opened.bytes) == record.source_digest
                    && u64::try_from(opened.bytes.len()).ok() == Some(record.source_len)
            });
            let retired_is_original = retired.as_ref().is_some_and(|opened| {
                provider_file_matches_identity(&opened.file, identity).unwrap_or(false)
                    && provider_digest(&opened.bytes) == record.source_digest
                    && u64::try_from(opened.bytes.len()).ok() == Some(record.source_len)
            });
            let retired_is_placeholder = retired.as_ref().is_some_and(|opened| {
                opened.bytes.is_empty()
                    && provider_file_matches_identity(&opened.file, placeholder_identity)
                        .unwrap_or(false)
            });

            if source_is_original && retired_is_placeholder {
                provider_retirement_after_validation_hook();
                provider_retire_original_into_placeholder(
                    source_dir,
                    source_name,
                    removed,
                    diagnostic_name,
                )
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
                sync_shared_provider_publication_directories(removed, Some(source_dir))?;
                provider_journal_boundary_hook(ProviderJournalBoundary::RetirementExchangeDurable)?;
                source = open_provider_regular_optional(
                    source_dir,
                    source_name,
                    MAX_PROVIDER_RESCAN_BYTES,
                    source_path,
                )?;
                retired = open_provider_regular_optional(
                    removed,
                    diagnostic_name,
                    MAX_PROVIDER_RESCAN_BYTES,
                    diagnostic_path,
                )?;
            } else if !retired_is_original {
                // Neither "the move has not run" nor "the move landed the exact
                // recorded original at the diagnostic name". The placeholder no
                // longer proves completion on its own: the single-rename
                // fallback consumes it and frees the source name, so recovery
                // keys on the diagnostic name holding the recorded original
                // identity, digest and length.
                return Err(ScenarioError::UnsafeProviderEntry(source_path.into()));
            }

            let retired_now = retired
                .as_ref()
                .ok_or_else(|| ScenarioError::UnsafeProviderEntry(diagnostic_path.into()))?;
            // The placeholder is a zero-length file this operation created, and
            // the emptiness is load-bearing, not decoration: the single-rename
            // fallback UNLINKS that inode, and a filesystem is free to hand the
            // same inode number to the next file created at the freed source
            // name — a racing delivery would then match the recorded identity
            // exactly. Requiring zero length keeps a delivery that carries bytes
            // from ever being mistaken for the placeholder, and a zero-length
            // impostor that still slips through costs zero bytes.
            let source_holds_placeholder = source.as_ref().is_some_and(|opened| {
                opened.bytes.is_empty()
                    && provider_file_matches_identity(&opened.file, placeholder_identity)
                        .unwrap_or(false)
            });
            if !provider_file_matches_identity(&retired_now.file, identity)?
                || provider_digest(&retired_now.bytes) != record.source_digest
                || u64::try_from(retired_now.bytes.len()).ok() != Some(record.source_len)
            {
                // Only an EXCHANGE can be undone, and only while this device
                // still holds the placeholder at the source name. After the
                // single-rename fallback there is nothing to swap back, so the
                // refusal is reported without pretending otherwise.
                if source_holds_placeholder {
                    let _ =
                        provider_exchange_names(source_dir, source_name, removed, diagnostic_name);
                    let _ = sync_shared_provider_publication_directories(removed, Some(source_dir));
                }
                return Err(ScenarioError::UnsafeProviderEntry(source_path.into()));
            }

            if source_holds_placeholder {
                provider_retirement_before_private_move_hook();
                // SOLE AUTHORITY of the exchange invariant, and deliberately
                // strict: this step exists only on the exchange path, and a
                // filesystem without RENAME_NOREPLACE has no RENAME_EXCHANGE
                // either, so the fallback above has already made it unreachable
                // there. A filesystem that somehow provided one and not the
                // other gets an honest named refusal rather than a two-step
                // substitute whose crash window this recovery cannot read.
                provider_rename_named_noreplace_named(
                    source_dir,
                    source_name,
                    evidence,
                    &evidence_name,
                    "retiring the shared provider retirement placeholder",
                )
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
                sync_shared_provider_publication_directories(evidence, Some(source_dir))?;
                provider_journal_boundary_hook(
                    ProviderJournalBoundary::RetirementPlaceholderQuarantined,
                )?;
                if let Err(error) =
                    reconcile_private_retirement_evidence(evidence, &evidence_name, record)
                {
                    return Err(error);
                }
            }
            if let Some(replacement) = open_provider_regular_optional(
                source_dir,
                source_name,
                MAX_PROVIDER_RESCAN_BYTES,
                source_path,
            )? {
                preserve_retirement_race(source_dir, source_name, evidence, &replacement.bytes)?;
                return Err(ScenarioError::UnsafeProviderEntry(source_path.into()));
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    return Err(ScenarioError::UnsafeProviderEntry(format!(
        "{source_path}: handle-safe retirement is unsupported"
    )));

    validate_retired_file(removed, diagnostic_name, diagnostic_path, record)
}

fn ensure_provider_retirement_evidence(
    evidence: &Dir,
    additional_entries: usize,
    additional_bytes: usize,
) -> Result<(), ScenarioError> {
    let mut count = 0_usize;
    let mut bytes = 0_usize;
    for entry in evidence
        .entries()
        .map_err(|error| ScenarioError::Io(error.to_string()))?
    {
        let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
        count = count
            .checked_add(1)
            .ok_or(ScenarioError::ProviderRescanLimit)?;
        if count
            .checked_add(additional_entries)
            .is_none_or(|count| count > MAX_PROVIDER_RESIDUE_ENTRIES)
        {
            return Err(ScenarioError::ProviderRescanLimit);
        }
        let name = entry.file_name().into_string().map_err(|_| {
            ScenarioError::UnsafeProviderEntry(format!(
                "{PROVIDER_RENAME_EVIDENCE_NAMESPACE}/non-UTF-8"
            ))
        })?;
        let valid_name = name
            .strip_prefix("retire-placeholder-")
            .or_else(|| name.strip_prefix("retirement-race-"))
            .is_some_and(valid_provider_journal_id);
        if !valid_name
            || !entry
                .file_type()
                .map_err(|error| ScenarioError::Io(error.to_string()))?
                .is_file()
        {
            return Err(ScenarioError::UnsafeProviderEntry(format!(
                "{PROVIDER_RENAME_EVIDENCE_NAMESPACE}/{name}"
            )));
        }
        let file = open_provider_file_nofollow(evidence, &name)
            .map_err(|error| ScenarioError::UnsafeProviderEntry(error.to_string()))?;
        let metadata = validate_provider_regular_file(
            &file,
            &format!("{PROVIDER_RENAME_EVIDENCE_NAMESPACE}/{name}"),
        )?;
        bytes = bytes
            .checked_add(
                usize::try_from(metadata.len()).map_err(|_| ScenarioError::ProviderRescanLimit)?,
            )
            .ok_or(ScenarioError::ProviderRescanLimit)?;
        if bytes
            .checked_add(additional_bytes)
            .is_none_or(|bytes| bytes > MAX_PROVIDER_RESCAN_BYTES)
        {
            return Err(ScenarioError::ProviderRescanLimit);
        }
    }
    if count
        .checked_add(additional_entries)
        .is_none_or(|count| count > MAX_PROVIDER_RESIDUE_ENTRIES)
        || bytes
            .checked_add(additional_bytes)
            .is_none_or(|bytes| bytes > MAX_PROVIDER_RESCAN_BYTES)
    {
        return Err(ScenarioError::ProviderRescanLimit);
    }
    Ok(())
}

fn reconcile_private_retirement_evidence(
    evidence: &Dir,
    evidence_name: &str,
    record: &ProviderJournalRecord,
) -> Result<(), ScenarioError> {
    let Some(opened) = open_provider_regular_optional(
        evidence,
        evidence_name,
        MAX_PROVIDER_RESCAN_BYTES,
        evidence_name,
    )?
    else {
        return Ok(());
    };
    let placeholder_identity = record
        .staging_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    if !provider_file_matches_identity(&opened.file, placeholder_identity)? {
        preserve_retirement_race(evidence, evidence_name, evidence, &opened.bytes)?;
        return Err(ScenarioError::UnsafeProviderEntry(evidence_name.into()));
    }
    let retained_identity = provider_file_identity(&opened.file)?;
    provider_retirement_before_private_delete_hook();
    let retained = open_provider_regular_optional(
        evidence,
        evidence_name,
        MAX_PROVIDER_RESCAN_BYTES,
        evidence_name,
    )?
    .ok_or_else(|| ScenarioError::UnsafeProviderEntry(evidence_name.into()))?;
    if provider_file_identity(&retained.file)? != retained_identity {
        return Err(ScenarioError::UnsafeProviderEntry(evidence_name.into()));
    }
    evidence
        .remove_file(evidence_name)
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    sync_shared_provider_directory(evidence)?;
    provider_journal_boundary_hook(ProviderJournalBoundary::RetirementPlaceholderPrivateDeleted)
}

fn preserve_retirement_race(
    source_dir: &Dir,
    source_name: &str,
    evidence: &Dir,
    bytes: &[u8],
) -> Result<(), ScenarioError> {
    ensure_provider_retirement_evidence(evidence, 1, bytes.len())?;
    let race_name = provider_quarantine_diagnostic_name("retirement-race", source_name, bytes);
    let race_path = format!("{PROVIDER_RENAME_EVIDENCE_NAMESPACE}/{race_name}");
    if shared_diagnostic_name_is_taken(evidence, &race_name, &race_path)? {
        return Err(ScenarioError::UnsafeProviderEntry(race_path));
    }
    // RECONSTRUCTIBLE by the same argument as the raced-name quarantine: the
    // bytes belong to whoever wrote them, the reservation refuses an occupied
    // destination before anything moves, and a failed rename leaves them in
    // place. The operation still refuses afterwards; only the evidence moves.
    provider_rename_reconstructible_noreplace(
        source_dir,
        source_name,
        evidence,
        &race_name,
        "preserving a shared provider retirement race",
    )
    .map_err(|error| ScenarioError::Io(error.to_string()))?;
    sync_shared_provider_publication_directories(evidence, Some(source_dir))
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path).is_relative()
        && !Path::new(path)
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
}

fn valid_provider_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PROVIDER_PATH_BYTES
        && !path.contains('\\')
        && !path.starts_with('/')
        && path.split('/').all(|component| {
            valid_name(component, 192)
                && component != "."
                && component != ".."
                && !component.contains(':')
        })
}

fn valid_provider_journal_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Recognized provider-owned staging siblings carry no protocol authority.
///
/// Keep this deliberately narrow: only direct children of an authoritative
/// shared-provider namespace and the documented hidden staging shapes qualify.
/// Malformed canonical protocol names and every other unknown entry continue
/// through ordinary fail-closed classification.
pub(crate) fn provider_transient_path(path: &str) -> bool {
    let Some((namespace, name)) = path.split_once('/') else {
        return false;
    };
    if name.contains('/')
        || ![
            PROVIDER_ENROLLMENT_NAMESPACE,
            PROVIDER_MANIFESTS_NAMESPACE,
            PROVIDER_OBJECTS_NAMESPACE,
            SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE,
            SHARED_PROVIDER_PUBLICATION_INTENTS_NAMESPACE,
            SHARED_PROVIDER_MANIFEST_RECOVERY_LINKS_NAMESPACE,
            SHARED_PROVIDER_MANIFEST_RECOVERY_BLOBS_NAMESPACE,
        ]
        .contains(&namespace)
    {
        return false;
    }
    [".syncthing.", "~syncthing~"].into_iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(".tmp"))
            .is_some_and(|payload| !payload.is_empty())
    })
}

fn valid_provider_user_path(path: &str) -> bool {
    valid_provider_path(path)
        && path.split('/').next().is_some_and(|namespace| {
            ![
                PROVIDER_TEMP_NAMESPACE,
                PROVIDER_REMOVED_NAMESPACE,
                PROVIDER_RENAME_EVIDENCE_NAMESPACE,
            ]
            .contains(&namespace)
        })
}

fn reject_provider_temporary_path(path: &str) -> Result<(), ScenarioError> {
    if valid_provider_user_path(path) {
        Ok(())
    } else {
        Err(ScenarioError::InvalidProviderPath(path.into()))
    }
}

fn provider_item_kind(path: &str) -> Option<ProviderItemKind> {
    let (namespace, remainder) = path.split_once('/')?;
    if remainder.is_empty() {
        return None;
    }
    match namespace {
        PROVIDER_OBJECTS_NAMESPACE => Some(ProviderItemKind::Object),
        PROVIDER_MANIFESTS_NAMESPACE => Some(ProviderItemKind::Manifest),
        _ => None,
    }
}

fn provider_tree_name(tree: ProviderTree) -> &'static str {
    match tree {
        ProviderTree::Inbox => "inbox",
        ProviderTree::Outbox => "outbox",
    }
}

fn ensure_provider_directory(parent: &Dir, name: &str) -> Result<(), ScenarioError> {
    ensure_directory_nofollow(parent, name)
        .map_err(|error| ScenarioError::UnsafeProviderEntry(format!("{name}: {error}")))
}

/// Create one directory in the graph-local shared provider namespace.
///
/// The entry-kind/no-follow checks remain mandatory. Android shared-storage
/// filesystems may, however, permit the create while denying directory fsync;
/// provider bytes remain recoverable/retryable and are not local authority, so
/// inability to issue that stronger barrier must not refuse ordinary sync.
fn ensure_shared_provider_directory(parent: &Dir, name: &str) -> Result<(), ScenarioError> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ScenarioError::UnsafeProviderEntry(format!(
                "{name}: expected a real no-follow directory"
            )));
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ScenarioError::UnsafeProviderEntry(format!(
                "{name}: {error}"
            )));
        }
    }
    parent
        .create_dir(name)
        .map_err(|error| ScenarioError::UnsafeProviderEntry(format!("{name}: {error}")))?;
    sync_shared_provider_directory(parent)
}

fn open_provider_directory(parent: &Dir, name: &str) -> Result<Dir, ScenarioError> {
    // A file-sync provider may materialize this directory under a different
    // Unix uid (Android shared storage, NFS, containers, or a shared group).
    // Ownership is therefore not provider authority. Keep the capability-
    // relative no-follow open; the exact path and entry kind remain enforced.
    open_dir_nofollow(parent, name)
        .map_err(|error| ScenarioError::UnsafeProviderEntry(format!("{name}: {error}")))
}

#[cfg(unix)]
fn open_provider_file_nofollow(parent: &Dir, name: &str) -> std::io::Result<fs::File> {
    let name = CString::new(name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider filename"))?;
    // SAFETY: the filename is a live C string and openat resolves it beneath
    // the retained parent capability. O_NOFOLLOW binds validation and reading
    // to one opened handle; O_NONBLOCK prevents special-file blocking.
    let fd = unsafe {
        libc::openat(
            parent.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: openat returned a newly owned file descriptor.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn open_provider_file_write_nofollow(parent: &Dir, name: &str) -> std::io::Result<fs::File> {
    let name = CString::new(name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider filename"))?;
    // SAFETY: the name is resolved beneath the retained parent capability;
    // O_NOFOLLOW and O_NONBLOCK reject link/special-file substitution.
    let fd = unsafe {
        libc::openat(
            parent.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: openat returned a newly owned descriptor.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

#[cfg(windows)]
fn open_provider_file_nofollow(parent: &Dir, name: &str) -> std::io::Result<fs::File> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .access_mode(GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    let file = parent.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "provider entry is a reparse point",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_provider_file_write_nofollow(parent: &Dir, name: &str) -> std::io::Result<fs::File> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .follow(FollowSymlinks::No)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    let file = parent.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "provider entry is a reparse point",
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_provider_file_nofollow(_parent: &Dir, _name: &str) -> std::io::Result<fs::File> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "atomic provider no-follow reads are unsupported",
    ))
}

#[cfg(not(any(unix, windows)))]
fn open_provider_file_write_nofollow(_parent: &Dir, _name: &str) -> std::io::Result<fs::File> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "provider no-follow writes are unsupported",
    ))
}

#[cfg(unix)]
fn validate_provider_regular_file(
    file: &fs::File,
    path: &str,
) -> Result<fs::Metadata, ScenarioError> {
    validate_provider_regular_file_with_link_count(file, path, true)
}

#[cfg(unix)]
fn validate_provider_regular_file_with_link_count(
    file: &fs::File,
    path: &str,
    require_single_link: bool,
) -> Result<fs::Metadata, ScenarioError> {
    let metadata = file
        .metadata()
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    // Provider files deliberately cross process, device, and filesystem
    // ownership boundaries. Their authority comes from validated immutable
    // content and exact capability-relative paths, never the Unix uid.
    if !metadata.is_file() || (require_single_link && metadata.nlink() != 1) {
        return Err(ScenarioError::UnsafeProviderEntry(path.into()));
    }
    Ok(metadata)
}

#[cfg(windows)]
fn validate_provider_regular_file(
    file: &fs::File,
    path: &str,
) -> Result<fs::Metadata, ScenarioError> {
    validate_provider_regular_file_with_link_count(file, path, true)
}

#[cfg(windows)]
fn validate_provider_regular_file_with_link_count(
    file: &fs::File,
    path: &str,
    require_single_link: bool,
) -> Result<fs::Metadata, ScenarioError> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_STANDARD_INFO,
    };

    let metadata = file
        .metadata()
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    let mut standard = FILE_STANDARD_INFO::default();
    // SAFETY: `file` owns a live handle, `standard` is writable for its full
    // declared size, and GetFileInformationByHandleEx does not retain either.
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&mut standard as *mut FILE_STANDARD_INFO).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(ScenarioError::Io(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    if !metadata.is_file()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        || (require_single_link && standard.NumberOfLinks != 1)
    {
        return Err(ScenarioError::UnsafeProviderEntry(path.into()));
    }
    Ok(metadata)
}

#[cfg(not(any(unix, windows)))]
fn validate_provider_regular_file(
    _file: &fs::File,
    path: &str,
) -> Result<fs::Metadata, ScenarioError> {
    Err(ScenarioError::UnsafeProviderEntry(path.into()))
}

#[cfg(not(any(unix, windows)))]
fn validate_provider_regular_file_with_link_count(
    _file: &fs::File,
    path: &str,
    _require_single_link: bool,
) -> Result<fs::Metadata, ScenarioError> {
    Err(ScenarioError::UnsafeProviderEntry(path.into()))
}

#[cfg(unix)]
fn provider_file_identity(file: &fs::File) -> Result<ProviderFileIdentity, ScenarioError> {
    let metadata = file
        .metadata()
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    Ok(ProviderFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn provider_file_identity(file: &fs::File) -> Result<ProviderFileIdentity, ScenarioError> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut information = FILE_ID_INFO::default();
    // SAFETY: `file` owns a live handle, `information` is writable for its
    // full declared size, and the system call retains neither pointer.
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(ScenarioError::Io(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(ProviderFileIdentity {
        volume: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
fn provider_file_identity(_file: &fs::File) -> Result<ProviderFileIdentity, ScenarioError> {
    Err(ScenarioError::UnsafeProviderEntry(
        "provider file identity is unsupported".into(),
    ))
}

fn provider_files_have_same_identity(
    left: &fs::File,
    right: &fs::File,
) -> Result<bool, ScenarioError> {
    Ok(provider_file_identity(left)? == provider_file_identity(right)?)
}

#[cfg(unix)]
fn provider_identity_record(identity: ProviderFileIdentity) -> ProviderIdentityRecord {
    ProviderIdentityRecord {
        platform: "unix".into(),
        first: identity.device,
        second: identity.inode.to_string(),
    }
}

#[cfg(windows)]
fn provider_identity_record(identity: ProviderFileIdentity) -> ProviderIdentityRecord {
    ProviderIdentityRecord {
        platform: "windows".into(),
        first: identity.volume,
        second: base64url_encode(&identity.file_id),
    }
}

#[cfg(not(any(unix, windows)))]
fn provider_identity_record(_identity: ProviderFileIdentity) -> ProviderIdentityRecord {
    ProviderIdentityRecord {
        platform: "unsupported".into(),
        first: 0,
        second: String::new(),
    }
}

fn provider_file_matches_identity(
    file: &fs::File,
    expected: &ProviderIdentityRecord,
) -> Result<bool, ScenarioError> {
    Ok(provider_identity_record(provider_file_identity(file)?) == *expected)
}

#[cfg(unix)]
fn valid_provider_identity_record(identity: &ProviderIdentityRecord) -> bool {
    identity.platform == "unix"
        && identity
            .second
            .parse::<u64>()
            .is_ok_and(|inode| inode.to_string() == identity.second)
}

#[cfg(windows)]
fn valid_provider_identity_record(identity: &ProviderIdentityRecord) -> bool {
    identity.platform == "windows"
        && base64url_decode(&identity.second).is_ok_and(|file_id| {
            file_id.len() == 16 && base64url_encode(&file_id) == identity.second
        })
}

#[cfg(not(any(unix, windows)))]
fn valid_provider_identity_record(_identity: &ProviderIdentityRecord) -> bool {
    false
}

fn provider_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hmac_sha256_hex(key: &[u8; 32], bytes: &[u8]) -> String {
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for (index, byte) in key.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(bytes);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    format!("{:x}", outer.finalize())
}

fn constant_time_bytes_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn provider_journal_phase_rank(phase: ProviderJournalPhase) -> u8 {
    match phase {
        ProviderJournalPhase::Prepared => 0,
        ProviderJournalPhase::Staged => 1,
        ProviderJournalPhase::PublishIntent => 2,
        ProviderJournalPhase::Published => 3,
        ProviderJournalPhase::RetireIntent => 4,
        ProviderJournalPhase::Retired => 5,
        ProviderJournalPhase::Cleanup => 6,
    }
}

fn canonical_provider_authority_bytes(
    record: &ProviderAuthorityRecord,
) -> Result<Vec<u8>, ScenarioError> {
    if record.authority_schema_version != PROVIDER_AUTHORITY_SCHEMA_VERSION
        || !valid_provider_identity_record(&record.device_identity)
        || !valid_provider_identity_record(&record.journal_identity)
        || !valid_provider_identity_record(&record.authority_key_identity)
        || !valid_provider_identity_record(&record.records_identity)
        || !valid_provider_identity_record(&record.blobs_identity)
        || !valid_provider_identity_record(&record.quarantine_identity)
        || !valid_provider_identity_record(&record.completed_identity)
    {
        return Err(ScenarioError::UnsafeProviderJournal(
            PROVIDER_DEVICE_AUTHORITY_NAME.into(),
        ));
    }
    let bytes = serde_json::to_vec(record).map_err(|error| ScenarioError::Io(error.to_string()))?;
    if bytes.len() > MAX_PROVIDER_AUTHORITY_BYTES {
        return Err(ScenarioError::ProviderJournalLimit);
    }
    Ok(bytes)
}

fn decode_provider_authentication_key(
    record: &ProviderAuthorityRecord,
) -> Result<[u8; 32], ScenarioError> {
    let decoded = base64url_decode(&record.authentication_key)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(PROVIDER_DEVICE_AUTHORITY_NAME.into()))?;
    if base64url_encode(&decoded) != record.authentication_key {
        return Err(ScenarioError::UnsafeProviderJournal(
            PROVIDER_DEVICE_AUTHORITY_NAME.into(),
        ));
    }
    decoded
        .try_into()
        .map_err(|_| ScenarioError::UnsafeProviderJournal(PROVIDER_DEVICE_AUTHORITY_NAME.into()))
}

fn read_provider_authority_record(
    authority_file: &fs::File,
) -> Result<(Vec<u8>, ProviderAuthorityRecord), ScenarioError> {
    let mut file = authority_file
        .try_clone()
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    let metadata = validate_provider_regular_file(&file, PROVIDER_DEVICE_AUTHORITY_NAME)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(PROVIDER_DEVICE_AUTHORITY_NAME.into()))?;
    let advertised =
        usize::try_from(metadata.len()).map_err(|_| ScenarioError::ProviderJournalLimit)?;
    if advertised > MAX_PROVIDER_AUTHORITY_BYTES {
        return Err(ScenarioError::ProviderJournalLimit);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    let mut bytes = Vec::with_capacity(advertised);
    Read::by_ref(&mut file)
        .take(
            u64::try_from(MAX_PROVIDER_AUTHORITY_BYTES + 1)
                .map_err(|_| ScenarioError::ProviderJournalLimit)?,
        )
        .read_to_end(&mut bytes)
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    if bytes.len() != advertised || bytes.len() > MAX_PROVIDER_AUTHORITY_BYTES {
        return Err(ScenarioError::UnsafeProviderJournal(
            PROVIDER_DEVICE_AUTHORITY_NAME.into(),
        ));
    }
    let record: ProviderAuthorityRecord = serde_json::from_slice(&bytes)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(PROVIDER_DEVICE_AUTHORITY_NAME.into()))?;
    if canonical_provider_authority_bytes(&record)? != bytes {
        return Err(ScenarioError::UnsafeProviderJournal(
            PROVIDER_DEVICE_AUTHORITY_NAME.into(),
        ));
    }
    decode_provider_authentication_key(&record)?;
    Ok((bytes, record))
}

fn provider_directory_identity(directory: &Dir) -> Result<ProviderFileIdentity, ScenarioError> {
    let file = directory
        .try_clone()
        .map_err(|error| ScenarioError::Io(error.to_string()))?
        .into_std_file();
    provider_file_identity(&file)
}

fn validate_named_provider_directory(
    parent: &Dir,
    name: &str,
    retained: &Dir,
    expected: ProviderFileIdentity,
) -> Result<(), ScenarioError> {
    let named = open_provider_directory(parent, name)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(format!("{name} was replaced")))?;
    if provider_directory_identity(&named)? != expected
        || provider_directory_identity(retained)? != expected
    {
        return Err(ScenarioError::UnsafeProviderJournal(format!(
            "{name} identity changed"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn open_provider_outer_authority_file_nofollow(
    parent: &Dir,
    name: &str,
) -> Result<fs::File, ScenarioError> {
    let file = open_provider_file_write_nofollow(parent, name)
        .map_err(|error| ScenarioError::UnsafeProviderJournal(format!("{name}: {error}")))?;
    validate_provider_regular_file(&file, name)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))?;
    Ok(file)
}

#[cfg(windows)]
fn open_provider_outer_authority_file_nofollow(
    parent: &Dir,
    name: &str,
) -> Result<fs::File, ScenarioError> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .follow(FollowSymlinks::No)
        .access_mode(GENERIC_READ | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    let file = parent
        .open_with(name, &options)
        .map_err(|error| ScenarioError::UnsafeProviderJournal(format!("{name}: {error}")))?
        .into_std();
    validate_provider_regular_file(&file, name)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))?;
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_provider_outer_authority_file_nofollow(
    _parent: &Dir,
    name: &str,
) -> Result<fs::File, ScenarioError> {
    Err(ScenarioError::UnsafeProviderJournal(format!(
        "{name}: provider authority is unsupported"
    )))
}

fn create_provider_outer_authority_file_exclusive(
    parent: &Dir,
    name: &str,
) -> Result<fs::File, ScenarioError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        options
            .follow(FollowSymlinks::No)
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = parent
        .open_with(name, &options)
        .map_err(|error| ScenarioError::UnsafeProviderJournal(format!("{name}: {error}")))?
        .into_std();
    validate_provider_regular_file(&file, name)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))?;
    Ok(file)
}

fn open_or_create_provider_outer_authority(
    device_directory: &Dir,
) -> Result<(fs::File, bool), ScenarioError> {
    match open_provider_outer_authority_file_nofollow(
        device_directory,
        PROVIDER_DEVICE_AUTHORITY_NAME,
    ) {
        Ok(file) => Ok((file, false)),
        Err(ScenarioError::UnsafeProviderJournal(_))
            if !device_directory.exists(PROVIDER_DEVICE_AUTHORITY_NAME) =>
        {
            match create_provider_outer_authority_file_exclusive(
                device_directory,
                PROVIDER_DEVICE_AUTHORITY_NAME,
            ) {
                Ok(file) => Ok((file, true)),
                Err(_) => open_provider_outer_authority_file_nofollow(
                    device_directory,
                    PROVIDER_DEVICE_AUTHORITY_NAME,
                )
                .map(|file| (file, false)),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_and_lock_provider_outer_authority(
    device_directory: &Dir,
) -> Result<(fs::File, fs::File, bool), ScenarioError> {
    let lock_file = device_directory
        .try_clone()
        .map_err(|error| ScenarioError::Io(error.to_string()))?
        .into_std_file();
    if !provider_lock_file_exclusive_nonblocking(&lock_file)
        .map_err(|error| ScenarioError::Io(error.to_string()))?
    {
        return Err(ScenarioError::UnsafeProviderJournal(
            "provider transaction gate is held by another process".into(),
        ));
    }
    match open_or_create_provider_outer_authority(device_directory) {
        Ok((authority, created)) => Ok((authority, lock_file, created)),
        Err(error) => {
            provider_unlock_file(&lock_file);
            Err(error)
        }
    }
}

#[cfg(windows)]
fn open_and_lock_provider_outer_authority(
    device_directory: &Dir,
) -> Result<(fs::File, fs::File, bool), ScenarioError> {
    let (authority, created) = open_or_create_provider_outer_authority(device_directory)?;
    let lock_file = authority
        .try_clone()
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    if !provider_lock_file_exclusive_nonblocking(&lock_file)
        .map_err(|error| ScenarioError::Io(error.to_string()))?
    {
        return Err(ScenarioError::UnsafeProviderJournal(
            "provider transaction gate is held by another process".into(),
        ));
    }
    Ok((authority, lock_file, created))
}

#[cfg(not(any(unix, windows)))]
fn open_and_lock_provider_outer_authority(
    _device_directory: &Dir,
) -> Result<(fs::File, fs::File, bool), ScenarioError> {
    Err(ScenarioError::UnsafeProviderJournal(
        "provider transaction authority is unsupported".into(),
    ))
}

#[cfg(unix)]
fn provider_transaction_lock_handle(
    authority: &ProviderTransactionAuthority,
) -> std::io::Result<fs::File> {
    authority
        .device_directory
        .try_clone()
        .map(Dir::into_std_file)
}

#[cfg(windows)]
fn provider_transaction_lock_handle(
    authority: &ProviderTransactionAuthority,
) -> std::io::Result<fs::File> {
    authority.authority_file.try_clone()
}

#[cfg(not(any(unix, windows)))]
fn provider_transaction_lock_handle(
    _authority: &ProviderTransactionAuthority,
) -> std::io::Result<fs::File> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "provider transaction locking is unsupported",
    ))
}

fn open_provider_authority_key_nofollow(
    parent: &Dir,
    name: &str,
) -> Result<fs::File, ScenarioError> {
    open_provider_outer_authority_file_nofollow(parent, name)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))
}

fn open_provider_authority_key_optional(
    parent: &Dir,
    name: &str,
) -> Result<Option<fs::File>, ScenarioError> {
    if !parent.exists(name) {
        return Ok(None);
    }
    open_provider_authority_key_nofollow(parent, name).map(Some)
}

fn create_provider_authority_key_exclusive(
    parent: &Dir,
    name: &str,
) -> Result<fs::File, ScenarioError> {
    create_provider_outer_authority_file_exclusive(parent, name)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))
}

fn create_local_file_exclusive(parent: &Dir, name: &str) -> Result<fs::File, ScenarioError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    let file = parent
        .open_with(name, &options)
        .map_err(|error| ScenarioError::UnsafeProviderJournal(format!("{name}: {error}")))?
        .into_std();
    validate_provider_regular_file(&file, name)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))?;
    Ok(file)
}

fn validate_local_file_bytes(
    file: &mut fs::File,
    expected: &[u8],
    name: &str,
) -> Result<(), ScenarioError> {
    validate_provider_regular_file(file, name)
        .map_err(|_| ScenarioError::UnsafeProviderJournal(name.into()))?;
    let expected_len =
        u64::try_from(expected.len()).map_err(|_| ScenarioError::ProviderJournalLimit)?;
    if file
        .metadata()
        .map_err(|error| ScenarioError::Io(error.to_string()))?
        .len()
        != expected_len
    {
        return Err(ScenarioError::UnsafeProviderJournal(name.into()));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    let mut actual = Vec::with_capacity(expected.len());
    Read::by_ref(file)
        .take(expected_len.saturating_add(1))
        .read_to_end(&mut actual)
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    if actual != expected {
        return Err(ScenarioError::UnsafeProviderJournal(name.into()));
    }
    Ok(())
}

fn open_provider_regular_optional(
    parent: &Dir,
    name: &str,
    limit: usize,
    path: &str,
) -> Result<Option<OpenProviderFile>, ScenarioError> {
    let mut file = match open_provider_file_nofollow(parent, name) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ScenarioError::UnsafeProviderEntry(format!(
                "{path}: {error}"
            )));
        }
    };
    let metadata = validate_provider_regular_file(&file, path)?;
    let advertised =
        usize::try_from(metadata.len()).map_err(|_| ScenarioError::ProviderRescanLimit)?;
    if advertised > limit {
        return Err(ScenarioError::ProviderRescanLimit);
    }
    let read_limit = u64::try_from(limit)
        .ok()
        .and_then(|limit| limit.checked_add(1))
        .ok_or(ScenarioError::ProviderRescanLimit)?;
    let mut bytes = Vec::with_capacity(advertised);
    Read::by_ref(&mut file)
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    if bytes.len() > limit || bytes.len() != advertised {
        return Err(ScenarioError::ProviderRescanLimit);
    }
    Ok(Some(OpenProviderFile { file, bytes }))
}

/// Read from the retained handle rather than a provider name.  A named
/// provider entry can be swapped between actions; only the retained handle is
/// authoritative for the bytes that are eligible for publication.
fn validate_provider_file_bytes(
    staged: &mut ProviderStagingFile,
    expected: &[u8],
    path: &str,
) -> Result<(), ScenarioError> {
    // Anonymous staging has zero links before publication; every named staging
    // object must remain single-link even though its pathname is never trusted
    // as publication authority.
    validate_provider_regular_file_with_link_count(&staged.file, path, staged.name.is_some())?;
    let expected_len =
        u64::try_from(expected.len()).map_err(|_| ScenarioError::ProviderRescanLimit)?;
    if staged
        .file
        .metadata()
        .map_err(|error| ScenarioError::Io(error.to_string()))?
        .len()
        != expected_len
    {
        return Err(ScenarioError::UnsafeProviderEntry(path.into()));
    }
    staged
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    let mut actual = Vec::with_capacity(expected.len());
    Read::by_ref(&mut staged.file)
        .take(expected_len.saturating_add(1))
        .read_to_end(&mut actual)
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    if actual != expected {
        return Err(ScenarioError::UnsafeProviderEntry(path.into()));
    }
    staged
        .file
        .seek(SeekFrom::Start(expected_len))
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    Ok(())
}

enum ProviderDestinationState {
    Absent,
    ExactBytes,
    ConflictingBytes,
}

/// Reconcile a destination against the exact retained bytes. This is the
/// publication state machine's recovery point: a previous call may have made
/// the name durable before an injected or validation error was returned.
fn provider_destination_state(
    destination_dir: &Dir,
    destination_name: &str,
    expected: &[u8],
    destination_path: &str,
) -> Result<ProviderDestinationState, ScenarioError> {
    match open_provider_regular_optional(
        destination_dir,
        destination_name,
        MAX_PROVIDER_RESCAN_BYTES,
        destination_path,
    )? {
        None => Ok(ProviderDestinationState::Absent),
        Some(opened) if opened.bytes == expected => Ok(ProviderDestinationState::ExactBytes),
        Some(_) => Ok(ProviderDestinationState::ConflictingBytes),
    }
}

fn validate_journal_staging(
    staging: &Dir,
    record: &ProviderJournalRecord,
    expected: &[u8],
    path: &str,
) -> Result<(), ScenarioError> {
    let staging_name = record
        .staging_name
        .as_deref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let identity = record
        .staging_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let staged = open_provider_regular_optional(
        staging,
        staging_name,
        MAX_PROVIDER_RESCAN_BYTES,
        staging_name,
    )?
    .ok_or_else(|| ScenarioError::UnsafeProviderEntry(path.into()))?;
    if staged.bytes != expected || !provider_file_matches_identity(&staged.file, identity)? {
        return Err(ScenarioError::UnsafeProviderEntry(path.into()));
    }
    Ok(())
}

/// Publish journaled bytes through an exclusively created destination handle.
/// The replaceable staging pathname is never the source of a production
/// rename. Its authenticated bytes come from the journal blob, and the new
/// destination identity is made durable in the record before that retained
/// handle is populated. A retry may therefore finish an empty or partial
/// destination only when its opened identity is the recorded one.
fn publish_journal_destination(
    journal: &ProviderRetryJournal,
    gate: &ProviderTransactionGate,
    record: &mut ProviderJournalRecord,
    staging: &Dir,
    tree: &Dir,
    destination_dir: &Dir,
    destination_name: &str,
    expected: &[u8],
    destination_path: &str,
) -> Result<(), ScenarioError> {
    journal.require_transaction_gate(gate)?;
    let existing = open_provider_regular_optional(
        destination_dir,
        destination_name,
        MAX_PROVIDER_RESCAN_BYTES,
        destination_path,
    )?;
    let mut destination = if let Some(existing) = existing {
        // File-sync providers commonly install an unchanged file through a
        // temp-file + rename, so its inode/file ID may change while its exact
        // immutable bytes do not. Exact destination bytes already satisfy a
        // Put; the private authenticated journal remains the retry authority.
        if existing.bytes == expected {
            cleanup_journal_staging(journal, gate, record, staging, tree)?;
            return Ok(());
        }
        let identity = record
            .destination_identity
            .as_ref()
            .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
        if !provider_file_matches_identity(&existing.file, identity)? {
            return Err(ScenarioError::ProviderConflictingBytes(
                destination_path.into(),
            ));
        }
        open_provider_file_write_nofollow(destination_dir, destination_name).map_err(|error| {
            ScenarioError::UnsafeProviderEntry(format!("{destination_path}: {error}"))
        })?
    } else {
        validate_journal_staging(staging, record, expected, destination_path)?;
        provider_publication_source_after_validation_hook();
        let destination = create_provider_destination_exclusive(
            destination_dir,
            destination_name,
            destination_path,
        )?;
        record.destination_identity = Some(provider_identity_record(provider_file_identity(
            &destination,
        )?));
        journal.store(gate, record)?;
        destination
    };
    let identity = record
        .destination_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    if !provider_file_matches_identity(&destination, identity)? {
        return Err(ScenarioError::UnsafeProviderEntry(destination_path.into()));
    }
    destination
        .set_len(0)
        .and_then(|()| destination.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| destination.write_all(expected))
        .and_then(|()| crate::durability_counters::sync_file(&destination))
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    validate_provider_open_file_bytes(&mut destination, expected, destination_path)?;
    provider_publication_after_publish_hook()?;

    cleanup_journal_staging(journal, gate, record, staging, tree)?;
    Ok(())
}

fn cleanup_journal_staging(
    journal: &ProviderRetryJournal,
    gate: &ProviderTransactionGate,
    record: &ProviderJournalRecord,
    staging: &Dir,
    tree: &Dir,
) -> Result<(), ScenarioError> {
    journal.require_transaction_gate(gate)?;
    let Some(staging_name) = record.staging_name.as_deref() else {
        return Ok(());
    };
    if open_provider_regular_optional(
        staging,
        staging_name,
        MAX_PROVIDER_RESCAN_BYTES,
        staging_name,
    )?
    .is_none()
    {
        return Ok(());
    }
    quarantine_unowned_staging(
        journal,
        gate,
        staging,
        staging_name,
        tree,
        &record.operation_id,
        record.staging_generation,
    )?;
    let diagnostic_name = format!(
        "orphan-{}-{}",
        record.operation_id, record.staging_generation
    );
    let removed = open_provider_directory(tree, PROVIDER_REMOVED_NAMESPACE)?;
    let diagnostic = open_provider_regular_optional(
        &removed,
        &diagnostic_name,
        MAX_PROVIDER_RESCAN_BYTES,
        &format!("{PROVIDER_REMOVED_NAMESPACE}/{diagnostic_name}"),
    )?
    .ok_or_else(|| ScenarioError::UnsafeProviderEntry(diagnostic_name.clone()))?;
    let identity = record
        .staging_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    if provider_file_matches_identity(&diagnostic.file, identity)? {
        removed
            .remove_file(&diagnostic_name)
            .map_err(|error| ScenarioError::Io(error.to_string()))?;
        sync_shared_provider_directory(&removed)?;
    }
    Ok(())
}

fn validate_put_destination(
    destination_dir: &Dir,
    destination_name: &str,
    destination_path: &str,
    expected: &[u8],
    record: &ProviderJournalRecord,
) -> Result<(), ScenarioError> {
    // The recorded identity authorizes resuming a partial destination that
    // this process created. Once the immutable destination is byte-exact, a
    // file-sync provider's inode replacement is benign and must not poison the
    // local retry journal.
    record
        .destination_identity
        .as_ref()
        .ok_or_else(|| ScenarioError::UnsafeProviderJournal(record.operation_id.clone()))?;
    let destination = open_provider_regular_optional(
        destination_dir,
        destination_name,
        MAX_PROVIDER_RESCAN_BYTES,
        destination_path,
    )?
    .ok_or_else(|| ScenarioError::UnsafeProviderEntry(destination_path.into()))?;
    if destination.bytes != expected {
        return Err(ScenarioError::ProviderConflictingBytes(
            destination_path.into(),
        ));
    }
    Ok(())
}

fn quarantine_unowned_staging(
    journal: &ProviderRetryJournal,
    gate: &ProviderTransactionGate,
    staging: &Dir,
    staging_name: &str,
    tree: &Dir,
    operation_id: &str,
    generation: u32,
) -> Result<(), ScenarioError> {
    journal.require_transaction_gate(gate)?;
    let removed = open_provider_directory(tree, PROVIDER_REMOVED_NAMESPACE)?;
    ensure_provider_diagnostic_capacity(&removed, PROVIDER_REMOVED_NAMESPACE, 1)?;
    let diagnostic_name = format!("orphan-{operation_id}-{generation}");
    if shared_diagnostic_name_is_taken(
        &removed,
        &diagnostic_name,
        &format!("{PROVIDER_REMOVED_NAMESPACE}/{diagnostic_name}"),
    )? {
        return Err(ScenarioError::UnsafeProviderEntry(format!(
            "{PROVIDER_REMOVED_NAMESPACE}/{diagnostic_name}"
        )));
    }
    // RECONSTRUCTIBLE. Abandoned staging is a second copy of bytes whose
    // authority is the private retry-journal blob, and every caller deletes this
    // diagnostic again as soon as its identity matches. Nothing reads
    // `removed/orphan-…` as authority for anything.
    provider_rename_reconstructible_noreplace(
        staging,
        staging_name,
        &removed,
        &diagnostic_name,
        "quarantining abandoned shared provider staging",
    )
    .map_err(|error| ScenarioError::Io(error.to_string()))?;
    sync_shared_provider_publication_directories(&removed, Some(staging))
}

fn create_provider_destination_exclusive(
    parent: &Dir,
    name: &str,
    path: &str,
) -> Result<fs::File, ScenarioError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(windows)]
    options.follow(FollowSymlinks::No);
    let file = parent
        .open_with(name, &options)
        .map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                ScenarioError::ProviderConflictingBytes(path.into())
            } else {
                ScenarioError::UnsafeProviderEntry(format!("{path}: {error}"))
            }
        })?
        .into_std();
    validate_provider_regular_file(&file, path)?;
    Ok(file)
}

fn create_provider_journal_staging(
    parent: &Dir,
    name: &str,
    path: &str,
) -> Result<ProviderStagingFile, ScenarioError> {
    let file = create_provider_destination_exclusive(parent, name, path)?;
    Ok(ProviderStagingFile {
        file,
        name: Some(name.into()),
    })
}

fn validate_provider_open_file_bytes(
    file: &mut fs::File,
    expected: &[u8],
    path: &str,
) -> Result<(), ScenarioError> {
    validate_provider_regular_file(file, path)?;
    let expected_len =
        u64::try_from(expected.len()).map_err(|_| ScenarioError::ProviderRescanLimit)?;
    if file
        .metadata()
        .map_err(|error| ScenarioError::Io(error.to_string()))?
        .len()
        != expected_len
    {
        return Err(ScenarioError::UnsafeProviderEntry(path.into()));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    let mut actual = Vec::with_capacity(expected.len());
    Read::by_ref(file)
        .take(expected_len.saturating_add(1))
        .read_to_end(&mut actual)
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    if actual != expected {
        return Err(ScenarioError::UnsafeProviderEntry(path.into()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderPublicationDurabilityStep {
    Published,
    DestinationDirectorySynced,
    SourceDirectorySynced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderJournalBoundary {
    BeforeBlobDurable,
    BlobDurable,
    CreationRecordDurable,
    BlobInstalled,
    RecordDurable,
    UpdateDurable,
    UpdateInstalled,
    OrphanQuarantined,
    OrphanOwnershipRechecked,
    OrphanRestored,
    OrphanPrivateDeleted,
    RetirementPlaceholderDurable,
    RetirementExchangeDurable,
    RetirementPlaceholderQuarantined,
    RetirementPlaceholderPrivateDeleted,
    BlobRemoved,
    CompletionDurable,
    RecordRemoved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderPostValidationOperation {
    Rename,
    Remove,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderRemovalDurabilityStep {
    DeletePending,
    HandleDropped,
    DirectorySyncing,
}

fn sync_provider_directory(directory: &Dir) -> Result<(), ScenarioError> {
    sync_dir_required(directory).map_err(|error| ScenarioError::Io(error.to_string()))
}

fn sync_shared_provider_directory(directory: &Dir) -> Result<(), ScenarioError> {
    match sync_dir_required(directory) {
        Ok(()) => Ok(()),
        #[cfg(target_os = "android")]
        Err(super::object_store::StoreError::Io(error))
            if shared_provider_directory_sync_may_be_unavailable(error.kind()) =>
        {
            Ok(())
        }
        Err(error) => Err(ScenarioError::Io(error.to_string())),
    }
}

#[cfg(any(test, target_os = "android"))]
const fn shared_provider_directory_sync_may_be_unavailable(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::PermissionDenied | ErrorKind::Unsupported | ErrorKind::InvalidInput
    )
}

#[cfg(unix)]
fn provider_lock_file_exclusive_nonblocking(file: &fs::File) -> std::io::Result<bool> {
    // SAFETY: flock only observes the retained authority-key descriptor.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn provider_unlock_file(file: &fs::File) {
    // SAFETY: flock only observes the retained authority-key descriptor.
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(windows)]
fn provider_lock_file_exclusive_nonblocking(file: &fs::File) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, FALSE};
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    let mut overlapped = unsafe { std::mem::zeroed() };
    // SAFETY: the handle and OVERLAPPED remain live for the synchronous call.
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != FALSE {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn provider_unlock_file(file: &fs::File) {
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    let mut overlapped = unsafe { std::mem::zeroed() };
    // SAFETY: the handle and OVERLAPPED remain live for the synchronous call.
    let _ = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as _,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
}

#[cfg(not(any(unix, windows)))]
fn provider_lock_file_exclusive_nonblocking(_file: &fs::File) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "provider retirement locking is unsupported",
    ))
}

#[cfg(not(any(unix, windows)))]
fn provider_unlock_file(_file: &fs::File) {}

/// The destination name is only considered published after its directory is
/// durable. A move-style operation also syncs the retained source/staging
/// directory after the destination, so recovery observes the same ordering.
fn sync_provider_publication_directories(
    destination_directory: &Dir,
    source_directory: Option<&Dir>,
) -> Result<(), ScenarioError> {
    sync_provider_directory(destination_directory)?;
    provider_publication_durability_hook(
        ProviderPublicationDurabilityStep::DestinationDirectorySynced,
    );
    if let Some(source_directory) = source_directory {
        sync_provider_directory(source_directory)?;
        provider_publication_durability_hook(
            ProviderPublicationDurabilityStep::SourceDirectorySynced,
        );
    }
    Ok(())
}

fn sync_shared_provider_publication_directories(
    destination_directory: &Dir,
    source_directory: Option<&Dir>,
) -> Result<(), ScenarioError> {
    sync_shared_provider_directory(destination_directory)?;
    provider_publication_durability_hook(
        ProviderPublicationDurabilityStep::DestinationDirectorySynced,
    );
    if let Some(source_directory) = source_directory {
        sync_shared_provider_directory(source_directory)?;
        provider_publication_durability_hook(
            ProviderPublicationDurabilityStep::SourceDirectorySynced,
        );
    }
    Ok(())
}

#[cfg(test)]
std::thread_local! {
    static FAIL_PROVIDER_PUBLICATION_AFTER_PHYSICAL_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_PROVIDER_RENAME_AFTER_PHYSICAL_MOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PROVIDER_PUBLICATION_DURABILITY_STEPS: std::cell::RefCell<Vec<ProviderPublicationDurabilityStep>> = const { std::cell::RefCell::new(Vec::new()) };
    static PROVIDER_REMOVAL_DURABILITY_STEPS: std::cell::RefCell<Vec<ProviderRemovalDurabilityStep>> = const { std::cell::RefCell::new(Vec::new()) };
    static PROVIDER_POST_VALIDATION_HOOK: std::cell::RefCell<Option<(ProviderPostValidationOperation, Box<dyn FnOnce()>)>> = const { std::cell::RefCell::new(None) };
    static PROVIDER_PUBLICATION_SOURCE_VALIDATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static PROVIDER_RETIREMENT_VALIDATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static PROVIDER_RETIREMENT_BEFORE_PRIVATE_MOVE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static PROVIDER_RETIREMENT_BEFORE_PRIVATE_DELETE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static PROVIDER_ORPHAN_AFTER_QUARANTINE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static PROVIDER_ORPHAN_BEFORE_PRIVATE_DELETE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static PROVIDER_FINISH_AFTER_GATE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static FAIL_PROVIDER_JOURNAL_AFTER_PHASE: std::cell::RefCell<Option<ProviderJournalPhase>> = const { std::cell::RefCell::new(None) };
    static FAIL_PROVIDER_JOURNAL_BOUNDARY: std::cell::RefCell<Option<ProviderJournalBoundary>> = const { std::cell::RefCell::new(None) };
    static FAIL_PENDING_PUBLICATION_MARKER_CREATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PROVIDER_SCAN_ENTRY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PROVIDER_SOURCE_INSPECTION_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn fail_next_pending_publication_marker_creation() {
    FAIL_PENDING_PUBLICATION_MARKER_CREATION.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn fail_next_provider_publication_after_physical_write() {
    FAIL_PROVIDER_PUBLICATION_AFTER_PHYSICAL_WRITE.with(|fail| fail.set(true));
}

#[cfg(test)]
fn pending_publication_marker_creation_hook() -> Result<(), ScenarioError> {
    if FAIL_PENDING_PUBLICATION_MARKER_CREATION.with(|fail| fail.replace(false)) {
        Err(ScenarioError::Io(
            "injected pending publication marker creation failure".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn pending_publication_marker_creation_hook() -> Result<(), ScenarioError> {
    Ok(())
}

fn provider_finish_after_gate_hook() {
    #[cfg(test)]
    PROVIDER_FINISH_AFTER_GATE_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback();
        }
    });
}

fn provider_source_inspection_visit() {
    #[cfg(test)]
    PROVIDER_SOURCE_INSPECTION_VISITS.with(|visits| visits.set(visits.get() + 1));
}

fn provider_post_validation_hook(_operation: ProviderPostValidationOperation) {
    #[cfg(test)]
    PROVIDER_POST_VALIDATION_HOOK.with(|hook| {
        let Some((expected, callback)) = hook.borrow_mut().take() else {
            return;
        };
        assert_eq!(expected, _operation, "provider validation hook operation");
        callback();
    });
}

fn provider_retirement_after_validation_hook() {
    #[cfg(test)]
    PROVIDER_RETIREMENT_VALIDATION_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback();
        }
    });
}

fn provider_retirement_before_private_move_hook() {
    #[cfg(test)]
    PROVIDER_RETIREMENT_BEFORE_PRIVATE_MOVE_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback();
        }
    });
}

fn provider_retirement_before_private_delete_hook() {
    #[cfg(test)]
    PROVIDER_RETIREMENT_BEFORE_PRIVATE_DELETE_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback();
        }
    });
}

fn provider_orphan_after_quarantine_hook() {
    #[cfg(test)]
    PROVIDER_ORPHAN_AFTER_QUARANTINE_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback();
        }
    });
}

fn provider_orphan_before_private_delete_hook() {
    #[cfg(test)]
    PROVIDER_ORPHAN_BEFORE_PRIVATE_DELETE_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback();
        }
    });
}

fn provider_publication_source_after_validation_hook() {
    #[cfg(test)]
    PROVIDER_PUBLICATION_SOURCE_VALIDATION_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback();
        }
    });
}

#[cfg(test)]
fn provider_journal_after_phase_hook(phase: ProviderJournalPhase) -> Result<(), ScenarioError> {
    let fail = FAIL_PROVIDER_JOURNAL_AFTER_PHASE.with(|hook| {
        if hook.borrow().as_ref() == Some(&phase) {
            hook.borrow_mut().take();
            true
        } else {
            false
        }
    });
    if fail {
        Err(ScenarioError::Io(format!(
            "injected provider journal crash after {phase:?}"
        )))
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn provider_journal_after_phase_hook(_phase: ProviderJournalPhase) -> Result<(), ScenarioError> {
    Ok(())
}

#[cfg(test)]
fn provider_journal_boundary_hook(boundary: ProviderJournalBoundary) -> Result<(), ScenarioError> {
    let fail = FAIL_PROVIDER_JOURNAL_BOUNDARY.with(|hook| {
        if hook.borrow().as_ref() == Some(&boundary) {
            hook.borrow_mut().take();
            true
        } else {
            false
        }
    });
    if fail {
        Err(ScenarioError::Io(format!(
            "injected provider journal crash at {boundary:?}"
        )))
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn provider_journal_boundary_hook(_boundary: ProviderJournalBoundary) -> Result<(), ScenarioError> {
    Ok(())
}

fn provider_scan_entry_visit() {
    #[cfg(test)]
    PROVIDER_SCAN_ENTRY_VISITS.with(|visits| visits.set(visits.get().saturating_add(1)));
}

#[cfg(test)]
fn provider_publication_after_publish_hook() -> Result<(), ScenarioError> {
    provider_publication_durability_hook(ProviderPublicationDurabilityStep::Published);
    if FAIL_PROVIDER_PUBLICATION_AFTER_PHYSICAL_WRITE.with(|hook| hook.replace(false)) {
        return Err(ScenarioError::Io(
            "injected provider publication validation failure".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn provider_rename_after_move_hook() -> Result<(), ScenarioError> {
    provider_publication_durability_hook(ProviderPublicationDurabilityStep::Published);
    if FAIL_PROVIDER_RENAME_AFTER_PHYSICAL_MOVE.with(|hook| hook.replace(false)) {
        return Err(ScenarioError::Io(
            "injected provider rename validation failure".into(),
        ));
    }
    Ok(())
}

#[cfg(not(test))]
fn provider_rename_after_move_hook() -> Result<(), ScenarioError> {
    provider_publication_durability_hook(ProviderPublicationDurabilityStep::Published);
    Ok(())
}

#[cfg(not(test))]
fn provider_publication_after_publish_hook() -> Result<(), ScenarioError> {
    provider_publication_durability_hook(ProviderPublicationDurabilityStep::Published);
    Ok(())
}

#[cfg(test)]
fn provider_publication_durability_hook(step: ProviderPublicationDurabilityStep) {
    PROVIDER_PUBLICATION_DURABILITY_STEPS.with(|steps| steps.borrow_mut().push(step));
}

#[cfg(not(test))]
fn provider_publication_durability_hook(_step: ProviderPublicationDurabilityStep) {}

#[cfg(test)]
fn provider_removal_durability_hook(step: ProviderRemovalDurabilityStep) {
    PROVIDER_REMOVAL_DURABILITY_STEPS.with(|steps| steps.borrow_mut().push(step));
}

#[cfg(not(test))]
#[allow(dead_code)]
fn provider_removal_durability_hook(_step: ProviderRemovalDurabilityStep) {}

#[cfg_attr(not(test), allow(dead_code))]
fn close_provider_delete_pending_file(file: fs::File) {
    provider_removal_durability_hook(ProviderRemovalDurabilityStep::DeletePending);
    drop(file);
    provider_removal_durability_hook(ProviderRemovalDurabilityStep::HandleDropped);
}

#[cfg(windows)]
fn provider_rename_handle_noreplace(
    file: &fs::File,
    destination_dir: &Dir,
    destination_name: &str,
) -> std::io::Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{FileRenameInfo, SetFileInformationByHandle};

    #[repr(C)]
    struct RenameInformation {
        replace_if_exists: u8,
        root_directory: HANDLE,
        file_name_length: u32,
        file_name: [u16; 1],
    }

    let destination: Vec<u16> = OsStr::new(destination_name).encode_wide().collect();
    if destination.is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "empty provider target",
        ));
    }
    let destination_bytes = destination
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "provider target too long"))?;
    let length = std::mem::size_of::<RenameInformation>()
        .checked_add(destination_bytes)
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "provider target too long"))?;
    // `FILE_RENAME_INFO` contains a HANDLE, so a Vec<u8> does not provide the
    // alignment required to cast its storage to the C layout. usize storage is
    // aligned for every field in RenameInformation and is rounded up to cover
    // the variable UTF-16 tail.
    let words = length.div_ceil(std::mem::size_of::<usize>());
    let mut information = vec![0_usize; words];
    let root = destination_dir.try_clone()?.into_std_file();
    let rename = information.as_mut_ptr().cast::<RenameInformation>();
    // SAFETY: `information` is aligned for RenameInformation and has at least
    // `length` initialized bytes for FILE_RENAME_INFO plus the UTF-16 tail.
    // Both handles remain live for the call, which atomically renames the
    // object selected by `file` itself.
    unsafe {
        (*rename).replace_if_exists = 0;
        (*rename).root_directory = root.as_raw_handle();
        (*rename).file_name_length = u32::try_from(destination_bytes).map_err(|_| {
            std::io::Error::new(ErrorKind::InvalidInput, "provider target too long")
        })?;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            (*rename).file_name.as_mut_ptr(),
            destination.len(),
        );
        if SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfo,
            rename.cast(),
            u32::try_from(length).map_err(|_| {
                std::io::Error::new(ErrorKind::InvalidInput, "provider target too long")
            })?,
        ) != 0
        {
            return Ok(());
        }
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn provider_rename_named_noreplace(
    source_dir: &Dir,
    source_name: &str,
    destination_dir: &Dir,
    destination_name: &str,
) -> std::io::Result<()> {
    let source = CString::new(source_name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider source"))?;
    let destination = CString::new(destination_name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider target"))?;
    // SAFETY: both names are live C strings and are resolved relative to
    // retained directory capabilities. RENAME_NOREPLACE atomically consumes
    // the exact source name without overwriting a destination. Calling the
    // syscall avoids Android's API-30-only renameat2 wrapper.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_dir.as_fd().as_raw_fd(),
            source.as_ptr(),
            destination_dir.as_fd().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn provider_rename_named_noreplace(
    source_dir: &Dir,
    source_name: &str,
    destination_dir: &Dir,
    destination_name: &str,
) -> std::io::Result<()> {
    let file = open_provider_file_nofollow(source_dir, source_name)?;
    validate_provider_regular_file(&file, source_name)
        .map_err(|error| std::io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
    provider_rename_handle_noreplace(&file, destination_dir, destination_name)?;
    match open_provider_file_nofollow(source_dir, source_name) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            ErrorKind::Other,
            "provider source was replaced during diagnostic move",
        )),
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn provider_exchange_names(
    source_dir: &Dir,
    source_name: &str,
    destination_dir: &Dir,
    destination_name: &str,
) -> std::io::Result<()> {
    let source = CString::new(source_name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider source"))?;
    let destination = CString::new(destination_name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider target"))?;
    // SAFETY: both names are live and capability-relative. RENAME_EXCHANGE
    // ensures that any racing replacement is preserved in diagnostic storage.
    // Calling the syscall avoids Android's API-30-only renameat2 wrapper.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_dir.as_fd().as_raw_fd(),
            source.as_ptr(),
            destination_dir.as_fd().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCHANGE as libc::c_uint,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn provider_rename_named_noreplace(
    source_dir: &Dir,
    source_name: &str,
    destination_dir: &Dir,
    destination_name: &str,
) -> std::io::Result<()> {
    let source = CString::new(source_name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider source"))?;
    let destination = CString::new(destination_name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider target"))?;
    // SAFETY: both names are live C strings and both directory descriptors
    // remain live for the atomic exclusive rename.
    let result = unsafe {
        libc::renameatx_np(
            source_dir.as_fd().as_raw_fd(),
            source.as_ptr(),
            destination_dir.as_fd().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn provider_exchange_names(
    source_dir: &Dir,
    source_name: &str,
    destination_dir: &Dir,
    destination_name: &str,
) -> std::io::Result<()> {
    let source = CString::new(source_name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider source"))?;
    let destination = CString::new(destination_name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid provider target"))?;
    // SAFETY: both names are live and capability-relative; RENAME_SWAP
    // atomically preserves either the validated source or a racing replacement.
    let result = unsafe {
        libc::renameatx_np(
            source_dir.as_fd().as_raw_fd(),
            source.as_ptr(),
            destination_dir.as_fd().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The exact platform primitive the shared-provider receipts name. Android CI
/// only ever returns a string, so a receipt that says only `Invalid argument
/// (os error 22)` costs a ~20-minute round trip to localise; every refusal in
/// this module names its call and both names instead.
#[cfg(any(target_os = "linux", target_os = "android"))]
const PROVIDER_NOREPLACE_RENAME_PRIMITIVE: &str = "renameat2(RENAME_NOREPLACE)";

#[cfg(any(target_os = "macos", target_os = "ios"))]
const PROVIDER_NOREPLACE_RENAME_PRIMITIVE: &str = "renameatx_np(RENAME_EXCL)";

#[cfg(windows)]
const PROVIDER_NOREPLACE_RENAME_PRIMITIVE: &str = "FileRenameInformation(ReplaceIfExists=false)";

#[cfg(not(any(unix, windows)))]
const PROVIDER_NOREPLACE_RENAME_PRIMITIVE: &str = "atomic no-clobber rename";

// Only the Unix targets have an atomic exchange, and only they reach
// `provider_retire_original_into_placeholder`.
#[cfg(any(target_os = "linux", target_os = "android"))]
const PROVIDER_EXCHANGE_RENAME_PRIMITIVE: &str = "renameat2(RENAME_EXCHANGE)";

#[cfg(any(target_os = "macos", target_os = "ios"))]
const PROVIDER_EXCHANGE_RENAME_PRIMITIVE: &str = "renameatx_np(RENAME_SWAP)";

#[cfg(unix)]
const PROVIDER_PLACEHOLDER_CONSUMING_RENAME_PRIMITIVE: &str =
    "rename onto the journaled retirement placeholder";

fn provider_rename_failure(operation: &str, from: &str, to: &str, error: std::io::Error) -> String {
    format!("{operation} failed at {from:?} -> {to:?}: {error}")
}

/// A `SharedReconstructibleProjection` name published without clobbering.
///
/// The six flagged renames in this module all operate under
/// `<graph>/.tine-sync/v2/shared`, and Android shared storage answers every
/// `renameat2` flag with `EINVAL` (CI run 32094662514). The three sites that
/// move DIAGNOSTIC RESIDUE — abandoned staging, a raced name, a preserved race
/// — reach the same reservation fallback the Markdown/Org projection uses, with
/// the same capability predicate and the same per-`st_dev` memo. The platform
/// primitive stays this module's own, because it carries provider-specific
/// validation the projection leg does not have.
fn provider_rename_reconstructible_noreplace(
    source_dir: &Dir,
    source_name: &str,
    destination_dir: &Dir,
    destination_name: &str,
    purpose: &str,
) -> std::io::Result<()> {
    let operation = format!("{PROVIDER_NOREPLACE_RENAME_PRIMITIVE} {purpose}");
    crate::model::rename_shared_reconstructible_noreplace(
        source_dir,
        source_name,
        destination_dir,
        destination_name,
        &operation,
        &|| {
            #[cfg(test)]
            if let Some(injected) = armed_shared_provider_flagged_rename() {
                return Err(injected);
            }
            provider_rename_named_noreplace(
                source_dir,
                source_name,
                destination_dir,
                destination_name,
            )
        },
    )
}

/// The strict, sole-authority no-clobber rename, named for its receipt. It has
/// no fallback: see `docs/storage-sync-contract.md` §2.10c for why the
/// retirement placeholder's private quarantine keeps the primitive.
#[cfg(unix)]
fn provider_rename_named_noreplace_named(
    source_dir: &Dir,
    source_name: &str,
    destination_dir: &Dir,
    destination_name: &str,
    purpose: &str,
) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(injected) = armed_shared_provider_flagged_rename() {
        return Err(std::io::Error::new(
            injected.kind(),
            provider_rename_failure(
                &format!("{PROVIDER_NOREPLACE_RENAME_PRIMITIVE} {purpose}"),
                source_name,
                destination_name,
                injected,
            ),
        ));
    }
    provider_rename_named_noreplace(source_dir, source_name, destination_dir, destination_name)
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                provider_rename_failure(
                    &format!("{PROVIDER_NOREPLACE_RENAME_PRIMITIVE} {purpose}"),
                    source_name,
                    destination_name,
                    error,
                ),
            )
        })
}

/// Retire the validated original into the diagnostic name that this operation's
/// journaled placeholder already occupies.
///
/// `RENAME_EXCHANGE` is the strict primitive: it flips both names in one step,
/// so the source name is never free and recovery can read which side of the flip
/// it is on from the two on-disk identities.
///
/// A filesystem that does not implement `renameat2` flags cannot provide that,
/// and there is no reservation-shaped substitute for an exchange. What there IS
/// is the observation that the placeholder was created for exactly this call and
/// is a zero-length file this device owns: a SINGLE plain `rename(2)` of the
/// original onto it is atomic on every POSIX filesystem, needs no scratch name,
/// and leaves precisely the state the exchange path reaches one step later —
/// source name gone, original at the diagnostic name, placeholder inode
/// unlinked. So the fallback is one rename, not three, and there is no window in
/// which the retired bytes exist at neither name.
///
/// What it gives up is the exchange's OTHER guarantee: afterwards the source
/// name is free rather than holding a known placeholder. Recovery therefore
/// keys on the diagnostic name holding the recorded original identity, and
/// anything found at the source name is treated as a racing replacement and
/// preserved as `rename-evidence/retirement-race-…`.
#[cfg(unix)]
fn provider_retire_original_into_placeholder(
    source_dir: &Dir,
    source_name: &str,
    removed: &Dir,
    diagnostic_name: &str,
) -> std::io::Result<()> {
    #[cfg(test)]
    let attempt = match armed_shared_provider_flagged_rename() {
        Some(injected) => Err(injected),
        None => provider_exchange_names(source_dir, source_name, removed, diagnostic_name),
    };
    #[cfg(not(test))]
    let attempt = provider_exchange_names(source_dir, source_name, removed, diagnostic_name);

    match attempt {
        Ok(()) => Ok(()),
        Err(error) if crate::model::flagged_rename_capability_refusal(&error) => source_dir
            .rename(source_name, removed, diagnostic_name)
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    provider_rename_failure(
                        PROVIDER_PLACEHOLDER_CONSUMING_RENAME_PRIMITIVE,
                        source_name,
                        diagnostic_name,
                        error,
                    ),
                )
            }),
        Err(error) => Err(std::io::Error::new(
            error.kind(),
            provider_rename_failure(
                &format!("{PROVIDER_EXCHANGE_RENAME_PRIMITIVE} retiring a shared provider name"),
                source_name,
                diagnostic_name,
                error,
            ),
        )),
    }
}

/// Is a shared-provider diagnostic name already taken?
///
/// The reservation fallback publishes such a name in two steps — reserve it with
/// an exclusive create, then rename onto it — so a crash inside that window
/// leaves a ZERO-LENGTH file at a deterministic diagnostic name with the source
/// still in place. `removed/` and `rename-evidence/` are diagnostic residue
/// namespaces and a zero-length entry in one of them holds no bytes to lose, so
/// reclaiming it lets the next attempt converge instead of refusing that
/// operation forever. A NON-EMPTY occupant is reported as occupied and left
/// untouched: that is either a real quarantine copy or a file a sync service
/// delivered, and neither may be destroyed.
fn shared_diagnostic_name_is_taken(
    directory: &Dir,
    name: &str,
    path: &str,
) -> Result<bool, ScenarioError> {
    let Some(existing) =
        open_provider_regular_optional(directory, name, MAX_PROVIDER_RESCAN_BYTES, path)?
    else {
        return Ok(false);
    };
    if !existing.bytes.is_empty() {
        return Ok(true);
    }
    drop(existing);
    directory
        .remove_file(name)
        .map_err(|error| ScenarioError::Io(error.to_string()))?;
    sync_shared_provider_directory(directory)?;
    Ok(false)
}

/// Make every flagged shared-provider rename fail with `errno` until the guard
/// is dropped, so a host test can reproduce a device whose shared storage does
/// not implement `renameat2` flags.
///
/// THREAD-SCOPED, not process-global. The earlier process-global arm made the
/// injected errno visible to every other test running concurrently under a
/// threaded `cargo test`: its exclusion lock only serialised two INJECTORS
/// against each other, and did nothing about the unrelated tests performing
/// ordinary flagged provider renames on other threads at the same moment.
/// `provider_rename_recovers_from_every_retry_and_retirement_boundary_without_overwrite`
/// failed roughly one run in five with a foreign `EIO` on a rename it never
/// injected, and the observed failure set differed on every run.
///
/// The reason the arm was global in the first place is real: the runtime
/// executes `prepare_shared` on its actor thread, so a thread-local armed on a
/// test thread would never be observed there. That case is now carried
/// explicitly instead of ambiently — `SyncRuntimeHandle::prepare_shared` reads
/// the caller's armed errno with `armed_shared_provider_flagged_rename_errno`
/// and hands it to the actor inside the request, and the actor installs it on
/// its own thread with `ScopedSharedProviderFlaggedRename` for exactly that
/// request. No other thread can see it.
#[cfg(test)]
std::thread_local! {
    static ARMED_SHARED_PROVIDER_FLAGGED_RENAME: std::cell::Cell<Option<i32>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn armed_shared_provider_flagged_rename() -> Option<std::io::Error> {
    ARMED_SHARED_PROVIDER_FLAGGED_RENAME
        .with(std::cell::Cell::get)
        .map(std::io::Error::from_raw_os_error)
}

/// The errno armed on THIS thread, for handing an injection across a request
/// boundary to the thread that will actually perform the rename.
#[cfg(test)]
pub(crate) fn armed_shared_provider_flagged_rename_errno() -> Option<i32> {
    ARMED_SHARED_PROVIDER_FLAGGED_RENAME.with(std::cell::Cell::get)
}

/// Install a handed-over injection on the current thread for one operation.
#[cfg(test)]
pub(crate) struct ScopedSharedProviderFlaggedRename(Option<i32>);

#[cfg(test)]
impl ScopedSharedProviderFlaggedRename {
    pub(crate) fn install(errno: Option<i32>) -> Self {
        if errno.is_some() {
            crate::model::forget_flagged_rename_capabilities();
        }
        Self(ARMED_SHARED_PROVIDER_FLAGGED_RENAME.with(|armed| armed.replace(errno)))
    }
}

#[cfg(test)]
impl Drop for ScopedSharedProviderFlaggedRename {
    fn drop(&mut self) {
        let restored = self.0;
        ARMED_SHARED_PROVIDER_FLAGGED_RENAME.with(|armed| armed.set(restored));
        crate::model::forget_flagged_rename_capabilities();
    }
}

#[cfg(test)]
pub(crate) struct InjectedSharedProviderFlaggedRenameFailure(Option<i32>);

#[cfg(test)]
impl InjectedSharedProviderFlaggedRenameFailure {
    pub(crate) fn enter(errno: i32) -> Self {
        crate::model::forget_flagged_rename_capabilities();
        Self(ARMED_SHARED_PROVIDER_FLAGGED_RENAME.with(|armed| armed.replace(Some(errno))))
    }
}

#[cfg(test)]
impl Drop for InjectedSharedProviderFlaggedRenameFailure {
    fn drop(&mut self) {
        let restored = self.0;
        ARMED_SHARED_PROVIDER_FLAGGED_RENAME.with(|armed| armed.set(restored));
        crate::model::forget_flagged_rename_capabilities();
    }
}

#[cfg(windows)]
#[allow(dead_code)]
fn provider_remove_open_file(file: fs::File) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };

    let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the retained handle selects the validated file object, the
    // disposition structure is initialized for the exact call size, and the
    // kernel retains neither pointer after the call.
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if result != 0 {
        close_provider_delete_pending_file(file);
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

struct ProviderDiskFile {
    path: String,
    bytes: Vec<u8>,
    temporary: bool,
}

fn bounded_provider_files(
    root: &Dir,
    include_temporary: bool,
    entry_limit: usize,
    byte_limit: usize,
) -> Result<Vec<ProviderDiskFile>, ScenarioError> {
    fn walk(
        directory: &Dir,
        prefix: &str,
        depth: usize,
        include_temporary: bool,
        entry_limit: usize,
        byte_limit: usize,
        entries: &mut usize,
        bytes: &mut usize,
        files: &mut Vec<ProviderDiskFile>,
    ) -> Result<(), ScenarioError> {
        if depth > MAX_PROVIDER_RESCAN_DEPTH {
            return Err(ScenarioError::ProviderRescanLimit);
        }
        for entry in directory
            .entries()
            .map_err(|error| ScenarioError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| ScenarioError::Io(error.to_string()))?;
            provider_scan_entry_visit();
            *entries = entries
                .checked_add(1)
                .ok_or(ScenarioError::ProviderRescanLimit)?;
            if *entries > entry_limit {
                return Err(ScenarioError::ProviderRescanLimit);
            }
            let name = entry.file_name().into_string().map_err(|_| {
                ScenarioError::UnsafeProviderEntry("non-UTF-8 provider entry".into())
            })?;
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if relative.len() > MAX_PROVIDER_PATH_BYTES || !valid_provider_path(&relative) {
                return Err(ScenarioError::ProviderRescanLimit);
            }
            let file_type = entry
                .file_type()
                .map_err(|error| ScenarioError::Io(error.to_string()))?;
            if file_type.is_symlink() {
                return Err(ScenarioError::UnsafeProviderEntry(relative));
            }
            if file_type.is_dir() {
                if prefix.is_empty() && name == PROVIDER_TEMP_NAMESPACE && !include_temporary {
                    let _ = open_provider_directory(directory, &name)?;
                    continue;
                }
                if depth >= MAX_PROVIDER_RESCAN_DEPTH {
                    return Err(ScenarioError::ProviderRescanLimit);
                }
                let child = open_provider_directory(directory, &name)?;
                walk(
                    &child,
                    &relative,
                    depth + 1,
                    include_temporary,
                    entry_limit,
                    byte_limit,
                    entries,
                    bytes,
                    files,
                )?;
            } else if file_type.is_file() {
                if prefix.is_empty()
                    && [
                        PROVIDER_OBJECTS_NAMESPACE,
                        PROVIDER_MANIFESTS_NAMESPACE,
                        SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE,
                        SHARED_PROVIDER_PUBLICATION_INTENTS_NAMESPACE,
                        PROVIDER_TEMP_NAMESPACE,
                        PROVIDER_REMOVED_NAMESPACE,
                        PROVIDER_RENAME_EVIDENCE_NAMESPACE,
                    ]
                    .contains(&name.as_str())
                {
                    return Err(ScenarioError::UnsafeProviderEntry(relative));
                }
                let temporary = relative
                    .split('/')
                    .next()
                    .is_some_and(|namespace| namespace == PROVIDER_TEMP_NAMESPACE);
                let remaining = byte_limit
                    .checked_sub(*bytes)
                    .ok_or(ScenarioError::ProviderRescanLimit)?;
                let opened =
                    open_provider_regular_optional(directory, &name, remaining, &relative)?
                        .ok_or_else(|| ScenarioError::UnknownProviderPath(relative.clone()))?;
                let file_bytes = opened.bytes;
                *bytes = bytes
                    .checked_add(file_bytes.len())
                    .ok_or(ScenarioError::ProviderRescanLimit)?;
                files.push(ProviderDiskFile {
                    path: relative,
                    bytes: file_bytes,
                    temporary,
                });
            } else {
                return Err(ScenarioError::UnsafeProviderEntry(relative));
            }
        }
        Ok(())
    }
    let mut entries = 0;
    let mut bytes = 0;
    let mut files = Vec::new();
    walk(
        root,
        "",
        0,
        include_temporary,
        entry_limit,
        byte_limit,
        &mut entries,
        &mut bytes,
        &mut files,
    )?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::{
        BatchCausalDot, BatchOrigin, CausalPeerId, ContentDigest, DocumentId, FrontierV2,
        ObjectKind, OperationBatch, OperationObject, SemanticEffectDigest, SessionId,
    };

    #[test]
    fn android_shared_provider_tolerates_only_missing_directory_sync_capability() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::Unsupported,
            ErrorKind::InvalidInput,
        ] {
            assert!(shared_provider_directory_sync_may_be_unavailable(kind));
        }
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::Interrupted,
            ErrorKind::InvalidData,
            ErrorKind::WriteZero,
            ErrorKind::Other,
        ] {
            assert!(!shared_provider_directory_sync_may_be_unavailable(kind));
        }
    }

    fn fixture_manifest(origin: BatchOrigin, batch_id: BatchId) -> OperationBatch {
        let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0x5eed));
        let payload = b"simulator fixture seam".to_vec();
        let object = OperationObject::new(
            workspace_id,
            DocumentId::from_uuid(Uuid::from_u128(0x5eee)),
            ObjectKind::SemanticEffect,
            payload.clone(),
        )
        .unwrap();
        let device_id = DeviceId::from_uuid(Uuid::from_u128(0x5eef));
        OperationBatch::new_with_causality(
            workspace_id,
            LineageDigest::of(b"simulator-fixture-seam-lineage"),
            batch_id,
            device_id,
            SessionId::from_uuid(Uuid::from_u128(0x5ef0)),
            origin,
            BatchCausalDot::new(CausalPeerId::from_device_id(device_id), 1).unwrap(),
            Vec::new(),
            FrontierV2::new(Vec::new()).unwrap(),
            SemanticEffectDigest::of(&payload),
            vec![object.descriptor().unwrap()],
        )
        .unwrap()
    }

    #[test]
    fn shared_provider_frontier_head_is_canonical_content_addressed_and_bound() {
        let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0x5f00));
        let lineage_digest = LineageDigest::of(b"frontier-head-lineage");
        let descriptor_digest = ContentDigest::of(b"frontier-head-descriptor");
        let device_id = DeviceId::from_uuid(Uuid::from_u128(0x5f01));
        let first = BatchId::from_uuid(Uuid::from_u128(0x5f02));
        let second = BatchId::from_uuid(Uuid::from_u128(0x5f03));
        let head = SharedProviderFrontierHeadV1::new(
            workspace_id,
            lineage_digest,
            descriptor_digest,
            device_id,
            17,
            ContentDigest::of(b"accepted frontier root"),
            vec![second, first, second],
            None,
        )
        .unwrap();
        assert_eq!(head.frontier_tips(), &[first, second]);
        assert!(!head.has_current_manifest_recovery_coverage());
        let bytes = head.encode().unwrap();
        assert!(bytes.len() <= MAX_SHARED_PROVIDER_FRONTIER_HEAD_BYTES);
        let path = head.path().unwrap();
        assert!(path.starts_with(&format!(
            "{SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE}/{device_id}-"
        )));
        assert!(path.ends_with(".head"));
        assert_eq!(
            SharedProviderFrontierHeadV1::decode(&path, &bytes).unwrap(),
            head
        );
        assert!(SharedProviderFrontierHeadV1::decode(
            &format!(
                "{SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE}/{device_id}-{}.head",
                "0".repeat(64)
            ),
            &bytes,
        )
        .is_err());

        let mut differing = bytes;
        differing.extend_from_slice(b" ");
        assert!(SharedProviderFrontierHeadV1::decode(&path, &differing).is_err());

        let accepted_root = ContentDigest::of(b"covered accepted frontier root");
        let covered = SharedProviderFrontierHeadV1::new(
            workspace_id,
            lineage_digest,
            descriptor_digest,
            device_id,
            18,
            accepted_root,
            vec![second],
            Some(accepted_root),
        )
        .unwrap();
        assert!(covered.has_current_manifest_recovery_coverage());
        assert_eq!(
            covered.manifest_recovery_coverage_root(),
            Some(accepted_root)
        );

        let audited = SharedProviderFrontierHeadV1::new_with_accepted_manifest_audit_coverage(
            workspace_id,
            lineage_digest,
            descriptor_digest,
            device_id,
            18,
            accepted_root,
            vec![second],
            Some(accepted_root),
            Some(18),
            Some(7),
        )
        .unwrap();
        assert!(audited.has_current_accepted_manifest_audit_coverage());
        assert_eq!(
            audited.accepted_manifest_audit_coverage_sequence(),
            Some(18)
        );
        assert_eq!(
            audited.accepted_manifest_revalidation_next_sequence(),
            Some(7)
        );
        let audited_bytes = audited.encode().unwrap();
        let audited_path = audited.path().unwrap();
        assert_eq!(
            SharedProviderFrontierHeadV1::decode(&audited_path, &audited_bytes).unwrap(),
            audited
        );
        assert!(
            SharedProviderFrontierHeadV1::new_with_accepted_manifest_audit_coverage(
                workspace_id,
                lineage_digest,
                descriptor_digest,
                device_id,
                18,
                accepted_root,
                vec![second],
                Some(accepted_root),
                Some(19),
                None,
            )
            .is_err()
        );
        assert!(
            SharedProviderFrontierHeadV1::new_with_accepted_manifest_audit_coverage(
                workspace_id,
                lineage_digest,
                descriptor_digest,
                device_id,
                18,
                accepted_root,
                vec![second],
                Some(accepted_root),
                Some(17),
                Some(7),
            )
            .is_err()
        );
    }

    #[test]
    fn absent_retirement_targets_settle_without_creating_local_journal_evidence() {
        let root = ScenarioRoot::new().unwrap();
        let provider_root = root.0.join("provider");
        let journal_root = root.0.join("private/device/journal");
        let provider = SharedProviderTransport::open(&provider_root, &journal_root).unwrap();

        provider
            .retire_frontier_head(&format!(
                "{SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE}/already-retired.head"
            ))
            .unwrap();
        assert_eq!(
            std::fs::read_dir(journal_root.join("records"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(journal_root.join("completed"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn shared_provider_observation_chunk_boundary_preserves_the_next_path() {
        let root = ScenarioRoot::new().unwrap();
        let provider_root = root.0.join("provider");
        let journal_root = root.0.join("private/device/journal");
        let mut provider = SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
        provider.publish_descriptor(b"descriptor").unwrap();

        // Three delivered files in a namespace the bounded head cursor visits
        // (frontier heads), written the way a file-sync service delivers them.
        // The cursor yields paths without decoding, so plain bytes are enough.
        let heads_dir = provider_root
            .join("outbox")
            .join(SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE);
        for index in 0_u128..3 {
            fs::write(
                heads_dir.join(format!("chunk-boundary-{index}.head")),
                format!("chunk-boundary-head-{index}").into_bytes(),
            )
            .unwrap();
        }

        let mut expected = Vec::new();
        let mut full = provider.full_observation_cursor().unwrap();
        loop {
            match provider.next_observed_path(&mut full).unwrap() {
                SharedProviderObservation::Path(path) => expected.push(path),
                SharedProviderObservation::Complete => break,
                SharedProviderObservation::ChunkBoundary => {
                    panic!("a full observation cursor cannot yield a chunk boundary")
                }
            }
        }

        let mut observed = Vec::new();
        let mut boundaries = 0;
        let mut chunked = provider.head_observation_cursor().unwrap();
        chunked.set_entry_limit_for_test(2);
        loop {
            match provider.next_observed_path(&mut chunked).unwrap() {
                SharedProviderObservation::Path(path) => observed.push(path),
                SharedProviderObservation::ChunkBoundary => {
                    boundaries += 1;
                    chunked.begin_next_chunk();
                }
                SharedProviderObservation::Complete => break,
            }
        }

        assert_eq!(boundaries, 2);
        assert_eq!(observed.len(), expected.len());
        observed.sort();
        expected.sort();
        assert_eq!(observed, expected);
    }

    #[test]
    fn provider_transient_classification_is_narrow_and_namespace_bound() {
        for path in [
            // Syncthing's documented Unix/macOS temporary sibling shape.
            "objects/.syncthing.0123456789.tmp",
            // Syncthing's documented Windows temporary sibling shape. A
            // shared provider can expose either shape to any receiving OS.
            "manifests/~syncthing~publication.tmp",
            "enrollment/.syncthing.descriptor.tmp",
            "frontier-heads-v1/~syncthing~head.tmp",
        ] {
            assert!(provider_transient_path(path), "{path}");
        }
        for path in [
            "objects/.syncthing..tmp",
            "objects/~syncthing~.tmp",
            "objects/.syncthing.batch.object",
            "objects/~syncthing~batch.object",
            "objects/.dropbox.tmp",
            "objects/.dropbox.publication.tmp",
            "objects/not-a-digest.object",
            "manifests/not-a-batch.manifest",
            "unknown/.syncthing.residue.tmp",
            "unknown/~syncthing~residue.tmp",
            "objects/nested/.syncthing.residue.tmp",
            "objects/nested/~syncthing~residue.tmp",
        ] {
            assert!(!provider_transient_path(path), "{path}");
        }
    }

    /// Stage 2e-ii sentinel relocation: the legacy simulator asserted the
    /// full-tree depth, entry, and aggregate actual-byte bounds plus
    /// temporary-file non-authority through scheduled `ReceiverRescan` runs.
    /// Those limits live in the retained transport
    /// (`SharedProviderTransport::scan` / `bounded_provider_files`), which the
    /// clean public ingress path never calls, so the sentinel moves here with
    /// the transport instead of being dropped with the simulator.
    #[test]
    fn retained_transport_scan_bounds_depth_entries_and_bytes_and_skips_unfinished_temporaries() {
        // (a) Depth refusal: one nesting level beyond the maximum fails the
        // whole-tree scan closed.
        let deep_root = ScenarioRoot::new().unwrap();
        let deep_transport = SharedProviderTransport::open(
            &deep_root.0.join("provider"),
            &deep_root.0.join("private/device/journal"),
        )
        .unwrap();
        let deep_outbox = deep_root.0.join("provider/outbox");
        let mut deep_path = deep_outbox.join("objects");
        for index in 0..=MAX_PROVIDER_RESCAN_DEPTH {
            deep_path.push(format!("d{index}"));
        }
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("object"), b"x").unwrap();
        assert!(matches!(
            deep_transport.scan(),
            Err(ScenarioError::ProviderRescanLimit)
        ));

        // (b) Entry-cap boundedness at the production cap: the walk stops at
        // exactly cap-plus-one visited entries and refuses, without visiting
        // the rest of an adversarially large tree.
        let cap_root = ScenarioRoot::new().unwrap();
        let cap_transport = SharedProviderTransport::open(
            &cap_root.0.join("provider"),
            &cap_root.0.join("private/device/journal"),
        )
        .unwrap();
        let cap_objects = cap_root.0.join("provider/outbox/objects");
        std::fs::create_dir_all(&cap_objects).unwrap();
        for index in 0..=MAX_PROVIDER_RESCAN_ENTRIES {
            std::fs::write(cap_objects.join(format!("{index:05}.object")), b"").unwrap();
        }
        PROVIDER_SCAN_ENTRY_VISITS.with(|visits| visits.set(0));
        assert!(matches!(
            cap_transport.scan(),
            Err(ScenarioError::ProviderRescanLimit)
        ));
        assert_eq!(
            PROVIDER_SCAN_ENTRY_VISITS.with(std::cell::Cell::get),
            MAX_PROVIDER_RESCAN_ENTRIES + 1,
            "the scan visited entries beyond the refusal boundary"
        );

        // (c) Aggregate actual-byte refusal: two files that each fit the
        // budget individually are refused once their real byte total exceeds
        // it. The single-file control proves the refusal is the aggregate.
        let byte_root = ScenarioRoot::new().unwrap();
        let byte_transport = SharedProviderTransport::open(
            &byte_root.0.join("provider"),
            &byte_root.0.join("private/device/journal"),
        )
        .unwrap();
        let byte_objects = byte_root.0.join("provider/outbox/objects");
        std::fs::create_dir_all(&byte_objects).unwrap();
        let over_half = vec![b'x'; MAX_PROVIDER_RESCAN_BYTES / 2 + 1];
        std::fs::write(byte_objects.join("first.object"), &over_half).unwrap();
        let single = byte_transport.scan().unwrap();
        assert_eq!(single.files.len(), 1);
        assert_eq!(single.files[0].bytes.len(), over_half.len());
        std::fs::write(byte_objects.join("second.object"), &over_half).unwrap();
        assert!(matches!(
            byte_transport.scan(),
            Err(ScenarioError::ProviderRescanLimit)
        ));

        // (d) A partially written temporary file in the provider tree stays
        // non-authoritative: the authoritative scan neither returns it nor
        // stops on it, and the inclusive walk marks it temporary.
        let temp_root = ScenarioRoot::new().unwrap();
        let mut temp_transport = SharedProviderTransport::open(
            &temp_root.0.join("provider"),
            &temp_root.0.join("private/device/journal"),
        )
        .unwrap();
        let object_bytes = b"published canonical object bytes";
        temp_transport
            .publish_object(ContentDigest::of(object_bytes), object_bytes)
            .unwrap();
        let manifest_bytes = b"published canonical manifest bytes";
        temp_transport
            .publish_manifest(
                BatchId::from_uuid(Uuid::from_u128(0x2ee2_0001)),
                manifest_bytes,
            )
            .unwrap();
        let temp_dir = temp_root
            .0
            .join("provider/outbox")
            .join(PROVIDER_TEMP_NAMESPACE);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let partial = temp_dir.join("unfinished-object.part");
        std::fs::write(&partial, b"partially written provider stag").unwrap();

        let scanned = temp_transport.scan().unwrap();
        assert_eq!(
            scanned.files.len(),
            2,
            "the partial temporary stopped or polluted the scan"
        );
        assert!(scanned.files.iter().all(|file| {
            !file.path.starts_with(PROVIDER_TEMP_NAMESPACE) && file.kind.is_some()
        }));
        assert!(
            partial.is_file(),
            "the scan must tolerate the unfinished temporary, not consume it"
        );
        let inclusive = bounded_provider_files(
            temp_transport.runtime.tree(ProviderTree::Outbox),
            true,
            MAX_PROVIDER_RESCAN_ENTRIES,
            MAX_PROVIDER_RESCAN_BYTES,
        )
        .unwrap();
        let staged = inclusive
            .iter()
            .find(|file| file.path == format!("{PROVIDER_TEMP_NAMESPACE}/unfinished-object.part"))
            .expect("the inclusive walk must surface the staging file for publication audits");
        assert!(staged.temporary);
        assert!(inclusive
            .iter()
            .all(|file| file.temporary == file.path.starts_with(PROVIDER_TEMP_NAMESPACE)));
    }

    // ===== Stage 2e-ii wave 3a: retained-provider retry and race controls =====
    //
    // The retained provider machinery (`SharedProviderTransport`,
    // `ProviderRetryJournal`, `put_complete`, `run_provider_rename`,
    // `run_provider_remove_with`) already consults the `#[cfg(test)]`
    // thread-local injection slots declared beside the production code
    // (`FAIL_PROVIDER_JOURNAL_AFTER_PHASE`, `FAIL_PROVIDER_JOURNAL_BOUNDARY`,
    // `FAIL_PROVIDER_PUBLICATION_AFTER_PHYSICAL_WRITE`,
    // `FAIL_PROVIDER_RENAME_AFTER_PHYSICAL_MOVE`,
    // `PROVIDER_PUBLICATION_SOURCE_VALIDATION_HOOK`,
    // `PROVIDER_POST_VALIDATION_HOOK`,
    // `PROVIDER_RETIREMENT_BEFORE_PRIVATE_MOVE_HOOK`,
    // `PROVIDER_STAGING_MODE`), so no new production consultation point is
    // needed: the installers below are RAII guards over those existing slots,
    // per the `InjectedSharedProviderFlaggedRenameFailure` house pattern (an
    // armed injection slot, cleared on drop). They are thread-local rather
    // than process-global because these tests drive the retained provider on
    // the test thread itself; a clean-runtime actor-thread test would need
    // the process-global variant of the pattern instead.

    /// One injected fault inside a retained provider retry operation.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ProviderRetryFault {
        /// Crash/power cut immediately after the named journal phase became
        /// durable.
        AfterDurablePhase(ProviderJournalPhase),
        /// Crash/power cut at the named journal file boundary.
        AtJournalBoundary(ProviderJournalBoundary),
        /// A validation failure reported after the destination bytes were
        /// already physically written (the torn-write/validation-race model).
        AfterPhysicalPublication,
    }

    /// The operation whose retry boundary the installed fault models.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ProviderRetryBoundary {
        Put(ProviderRetryFault),
        Rename(ProviderRetryFault),
        Remove(ProviderRetryFault),
    }

    struct InstalledProviderRetryBoundaryFault;

    fn install_provider_retry_boundary_fault_for_test(
        boundary: ProviderRetryBoundary,
    ) -> InstalledProviderRetryBoundaryFault {
        let fault = match boundary {
            ProviderRetryBoundary::Put(fault)
            | ProviderRetryBoundary::Rename(fault)
            | ProviderRetryBoundary::Remove(fault) => fault,
        };
        match fault {
            ProviderRetryFault::AfterDurablePhase(phase) => {
                FAIL_PROVIDER_JOURNAL_AFTER_PHASE.with(|hook| hook.replace(Some(phase)));
            }
            ProviderRetryFault::AtJournalBoundary(at) => {
                FAIL_PROVIDER_JOURNAL_BOUNDARY.with(|hook| hook.replace(Some(at)));
            }
            ProviderRetryFault::AfterPhysicalPublication => match boundary {
                ProviderRetryBoundary::Put(_) => {
                    FAIL_PROVIDER_PUBLICATION_AFTER_PHYSICAL_WRITE.with(|hook| hook.set(true));
                }
                ProviderRetryBoundary::Rename(_) => {
                    FAIL_PROVIDER_RENAME_AFTER_PHYSICAL_MOVE.with(|hook| hook.set(true));
                }
                ProviderRetryBoundary::Remove(_) => {
                    panic!("remove has no physical publication boundary")
                }
            },
        }
        InstalledProviderRetryBoundaryFault
    }

    impl Drop for InstalledProviderRetryBoundaryFault {
        fn drop(&mut self) {
            FAIL_PROVIDER_JOURNAL_AFTER_PHASE.with(|hook| hook.replace(None));
            FAIL_PROVIDER_JOURNAL_BOUNDARY.with(|hook| hook.replace(None));
            FAIL_PROVIDER_PUBLICATION_AFTER_PHYSICAL_WRITE.with(|hook| hook.set(false));
            FAIL_PROVIDER_RENAME_AFTER_PHYSICAL_MOVE.with(|hook| hook.set(false));
        }
    }

    /// One race window inside a retained provider operation at which an
    /// installed action mutates the filesystem while the operation continues.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ProviderPublicationRace {
        /// Put: after the staged source is validated, before the exclusive
        /// destination is created.
        PutAfterSourceValidation,
        /// Rename: right after the source file is validated into the new
        /// journal record.
        RenameAfterSourceValidation,
        /// Remove: right after the source file is validated into the new
        /// journal record.
        RemoveAfterSourceValidation,
        /// Rename/remove retirement: before the retired source moves into the
        /// private diagnostic namespace.
        RetirementBeforePrivateMove,
    }

    struct InstalledProviderPublicationRace;

    fn install_provider_publication_race_for_test(
        race: ProviderPublicationRace,
        action: Box<dyn FnOnce()>,
    ) -> InstalledProviderPublicationRace {
        match race {
            ProviderPublicationRace::PutAfterSourceValidation => {
                PROVIDER_PUBLICATION_SOURCE_VALIDATION_HOOK.with(|hook| hook.replace(Some(action)));
            }
            ProviderPublicationRace::RenameAfterSourceValidation => {
                PROVIDER_POST_VALIDATION_HOOK.with(|hook| {
                    hook.replace(Some((ProviderPostValidationOperation::Rename, action)))
                });
            }
            ProviderPublicationRace::RemoveAfterSourceValidation => {
                PROVIDER_POST_VALIDATION_HOOK.with(|hook| {
                    hook.replace(Some((ProviderPostValidationOperation::Remove, action)))
                });
            }
            ProviderPublicationRace::RetirementBeforePrivateMove => {
                PROVIDER_RETIREMENT_BEFORE_PRIVATE_MOVE_HOOK
                    .with(|hook| hook.replace(Some(action)));
            }
        }
        InstalledProviderPublicationRace
    }

    impl Drop for InstalledProviderPublicationRace {
        fn drop(&mut self) {
            PROVIDER_PUBLICATION_SOURCE_VALIDATION_HOOK.with(|hook| hook.replace(None));
            PROVIDER_POST_VALIDATION_HOOK.with(|hook| hook.replace(None));
            PROVIDER_RETIREMENT_BEFORE_PRIVATE_MOVE_HOOK.with(|hook| hook.replace(None));
        }
    }

    /// A direct retained-provider device: `ProviderRuntime` plus
    /// `ProviderRetryJournal` over a plain directory, with no simulator
    /// scenario, scheduler, engine, or object store behind it. Dropping the
    /// value and calling this again on the same root is the crash/power-cut
    /// plus restart model: all process state is lost and only the on-disk
    /// provider tree and private retry journal survive.
    struct RetainedProviderDevice {
        name: String,
        provider: ProviderRuntime,
        provider_journal: Option<ProviderRetryJournal>,
    }

    fn retained_provider_device(root: &std::path::Path) -> RetainedProviderDevice {
        RetainedProviderDevice {
            name: "retained".into(),
            provider: ProviderRuntime::open(root.join("provider")).unwrap(),
            provider_journal: Some(
                ProviderRetryJournal::open(root.join("provider-local-journal")).unwrap(),
            ),
        }
    }

    fn run_provider_rename(
        device: &RetainedProviderDevice,
        event_id: u64,
        tree: ProviderTree,
        from_path: &str,
        to_path: &str,
    ) -> Result<(), ScenarioError> {
        run_provider_rename_with(
            &device.provider,
            device
                .provider_journal
                .as_ref()
                .expect("open retry journal"),
            &device.name,
            event_id,
            tree,
            from_path,
            to_path,
        )
    }

    fn run_provider_remove(
        device: &RetainedProviderDevice,
        event_id: u64,
        tree: ProviderTree,
        path: &str,
    ) -> Result<(), ScenarioError> {
        run_provider_remove_with(
            &device.provider,
            device
                .provider_journal
                .as_ref()
                .expect("open retry journal"),
            &device.name,
            event_id,
            tree,
            path,
            None,
            ProviderRemoveMissingSourcePolicy::RequirePresent,
        )
    }

    fn retained_dir_count(path: &std::path::Path) -> usize {
        std::fs::read_dir(path).unwrap().count()
    }

    /// The deterministic operation id of a retained-transport generated put.
    fn generated_put_operation_id(path: &str, bytes: &[u8]) -> String {
        let binding = format!("generated:{}", provider_digest(bytes));
        ProviderRetryJournal::operation_id(
            ProviderJournalOperation::Put,
            &binding,
            &binding,
            ProviderTree::Outbox,
            path,
            None,
            u64::try_from(bytes.len()).unwrap(),
            &provider_digest(bytes),
        )
    }

    /// Stage 2e-ii item 10: every race boundary of the retained provider
    /// publication path revalidates instead of trusting a name, a handle, or
    /// an earlier validation. Replaces the simulator publication-race
    /// scenarios (`provider_finish_uses_retained_handle_and_leaves_replacement_untouched`,
    /// `provider_finish_never_reads_symlink_or_special_temp_replacements`,
    /// `provider_finish_conflict_keeps_honest_temp_and_never_claims_success`,
    /// `provider_finish_rejects_an_unrelated_identical_destination`,
    /// `provider_copy_rejects_an_unrelated_identical_destination`,
    /// `complete_copy_publication_rejects_a_replaced_named_staging_source`,
    /// `provider_put_accepts_exact_bytes_after_file_sync_replaces_the_inode`,
    /// and `anonymous_and_named_staging_produce_identical_canonical_provider_snapshots`)
    /// at the retained `SharedProviderTransport` boundary.
    #[test]
    fn provider_publication_revalidates_every_race_boundary() {
        let bytes: &[u8] = b"publication race object bytes";
        let object_path = format!(
            "{PROVIDER_OBJECTS_NAMESPACE}/{}.object",
            ContentDigest::of(bytes)
        );
        let open_fixture = || {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            let transport = SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            (root, provider_root, journal_root, transport)
        };
        let destination_of =
            |provider_root: &std::path::Path| provider_root.join("outbox").join(&object_path);
        let staging_path_of = |provider_root: &std::path::Path| {
            provider_root
                .join("outbox")
                .join(PROVIDER_TEMP_NAMESPACE)
                .join(ProviderRetryJournal::staging_name(
                    &generated_put_operation_id(&object_path, bytes),
                    0,
                ))
        };

        // (1) Unrelated identical destination. Threat: an honest concurrent
        // instance or a file-sync service already delivered a file carrying
        // our canonical name. The exact object path revalidates and accepts
        // byte-identical content without rewriting it; the plain manifest
        // path refuses any pre-existing occupant outright and leaves it
        // untouched.
        {
            let (_root, provider_root, journal_root, mut transport) = open_fixture();
            let destination = destination_of(&provider_root);
            std::fs::write(&destination, bytes).unwrap();
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), bytes);
            assert_eq!(retained_dir_count(&journal_root.join("records")), 0);

            let manifest_id = BatchId::from_uuid(Uuid::from_u128(0x2ee2_3a10));
            let manifest_path = provider_root
                .join("outbox")
                .join(PROVIDER_MANIFESTS_NAMESPACE)
                .join(format!("{manifest_id}.manifest"));
            std::fs::write(&manifest_path, b"occupant manifest bytes").unwrap();
            assert!(matches!(
                transport.publish_manifest(manifest_id, b"occupant manifest bytes"),
                Err(ScenarioError::ProviderConflictingBytes(path))
                    if path.ends_with(".manifest")
            ));
            assert_eq!(
                std::fs::read(&manifest_path).unwrap(),
                b"occupant manifest bytes"
            );
        }

        // (2) Deterministic staging-name collision. Threat: a previous
        // instance of this device crashed (power cut) or a sync service left
        // unrelated litter at the deterministic staging name. The occupant is
        // quarantined without byte loss and the publication converges.
        {
            let (_root, provider_root, journal_root, mut transport) = open_fixture();
            let staging_dir = provider_root.join("outbox").join(PROVIDER_TEMP_NAMESPACE);
            std::fs::write(
                staging_dir.join(ProviderRetryJournal::staging_name(
                    &generated_put_operation_id(&object_path, bytes),
                    0,
                )),
                b"foreign staging occupant",
            )
            .unwrap();
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(
                std::fs::read(destination_of(&provider_root)).unwrap(),
                bytes
            );
            let removed = provider_root
                .join("outbox")
                .join(PROVIDER_REMOVED_NAMESPACE);
            let quarantined: Vec<_> = std::fs::read_dir(&removed)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(quarantined.len(), 1);
            assert_eq!(
                std::fs::read(&quarantined[0]).unwrap(),
                b"foreign staging occupant"
            );
            assert_eq!(retained_dir_count(&staging_dir), 0);
            assert_eq!(retained_dir_count(&journal_root.join("records")), 0);
        }

        // (3) Staged-source replacement across a crash. Threat: crash/power
        // cut leaves the named staging file on disk and an external editor or
        // sync service replaces it before restart. The recorded staging
        // identity detects the replacement; the retry quarantines the foreign
        // bytes (preserved, never published) and re-stages from the journal
        // blob — the byte authority — so the operation recovers instead of
        // wedging every future retry. Deletion of the staging file across the
        // same crash recovers identically.
        for foreign in [Some(&b"attacker staging bytes"[..]), None] {
            let (_root, provider_root, journal_root, mut transport) = open_fixture();
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Staged),
                    ));
                assert!(matches!(
                    transport.publish_object_exact(ContentDigest::of(bytes), bytes),
                    Err(ScenarioError::Io(message)) if message.contains("injected")
                ));
            }
            let staging = staging_path_of(&provider_root);
            assert_eq!(std::fs::read(&staging).unwrap(), bytes);
            std::fs::remove_file(&staging).unwrap();
            if let Some(foreign_bytes) = foreign {
                std::fs::write(&staging, foreign_bytes).unwrap();
            }
            drop(transport);
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(
                std::fs::read(destination_of(&provider_root)).unwrap(),
                bytes
            );
            let removed_dir = provider_root
                .join("outbox")
                .join(PROVIDER_REMOVED_NAMESPACE);
            let quarantined: Vec<Vec<u8>> = match std::fs::read_dir(&removed_dir) {
                Ok(entries) => entries
                    .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
                    .collect(),
                Err(_) => Vec::new(),
            };
            match foreign {
                Some(foreign_bytes) => {
                    assert_eq!(
                        quarantined,
                        vec![foreign_bytes.to_vec()],
                        "the foreign staging bytes must survive as the single quarantine diagnostic"
                    );
                }
                None => assert!(
                    quarantined.is_empty(),
                    "recovery from a deleted staging file leaves no diagnostic residue"
                ),
            }
            // The recovered operation is exactly idempotent.
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(
                std::fs::read(destination_of(&provider_root)).unwrap(),
                bytes
            );
        }

        // (4) Symlink and special-file staging replacement across a crash.
        // Threat: an external editor or hostile-shaped sync residue swaps the
        // crashed staging name for a symlink or a FIFO; neither may ever be
        // read or published, and the symlink target must stay untouched.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            for replacement in ["symlink", "fifo"] {
                let (_root, provider_root, journal_root, mut transport) = open_fixture();
                {
                    let _fault =
                        install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                            ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Staged),
                        ));
                    assert!(transport
                        .publish_object_exact(ContentDigest::of(bytes), bytes)
                        .is_err());
                }
                let staging = staging_path_of(&provider_root);
                std::fs::remove_file(&staging).unwrap();
                let target = provider_root
                    .join("outbox")
                    .join(PROVIDER_TEMP_NAMESPACE)
                    .join("attacker-target");
                match replacement {
                    "symlink" => {
                        std::fs::write(&target, b"attacker replacement").unwrap();
                        symlink(&target, &staging).unwrap();
                    }
                    "fifo" => {
                        let name = CString::new(staging.as_os_str().as_encoded_bytes()).unwrap();
                        // SAFETY: `name` is a live NUL-terminated pathname and
                        // mkfifo does not retain it. A filesystem without
                        // FIFOs is an unavailable special-file case, not a
                        // failure of this test.
                        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
                            let error = std::io::Error::last_os_error();
                            if matches!(error.raw_os_error(), Some(libc::EPERM | libc::EOPNOTSUPP))
                            {
                                continue;
                            }
                            panic!("create FIFO replacement: {error}");
                        }
                    }
                    _ => unreachable!(),
                }
                drop(transport);
                let mut transport =
                    SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
                assert!(
                    transport
                        .publish_object_exact(ContentDigest::of(bytes), bytes)
                        .is_err(),
                    "{replacement}"
                );
                assert!(!destination_of(&provider_root).exists(), "{replacement}");
                assert!(std::fs::symlink_metadata(&staging).is_ok(), "{replacement}");
                if replacement == "symlink" {
                    assert_eq!(std::fs::read(&target).unwrap(), b"attacker replacement");
                }
            }
        }

        // (5) Post-validation staging replacement inside one publication.
        // Threat: an external editor or a concurrent honest instance replaces
        // the staging file in the window between staging validation and
        // destination creation. The private journal blob stays the byte
        // authority: the destination receives the validated bytes and the
        // replacement is preserved as quarantine residue, never published.
        {
            let (_root, provider_root, _journal_root, mut transport) = open_fixture();
            let race_staging = staging_path_of(&provider_root);
            let _race = install_provider_publication_race_for_test(
                ProviderPublicationRace::PutAfterSourceValidation,
                Box::new(move || {
                    // Delivered the way sync services install files: a
                    // sibling temp file renamed over the target, so the
                    // replacement is a genuinely distinct inode.
                    let delivery = race_staging.with_file_name("attacker-delivery.tmp");
                    std::fs::write(&delivery, b"attacker staging bytes").unwrap();
                    std::fs::rename(&delivery, &race_staging).unwrap();
                }),
            );
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(
                std::fs::read(destination_of(&provider_root)).unwrap(),
                bytes
            );
            let removed = provider_root
                .join("outbox")
                .join(PROVIDER_REMOVED_NAMESPACE);
            let quarantined: Vec<_> = std::fs::read_dir(&removed)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(quarantined.len(), 1);
            assert_eq!(
                std::fs::read(&quarantined[0]).unwrap(),
                b"attacker staging bytes"
            );
        }

        // (6) Exact-bytes destination inode replacement across a crash.
        // Threat: file-sync services commonly reinstall even unchanged bytes
        // through a temp-plus-rename, changing the inode while the crashed
        // put is mid-retry. Exact bytes remain acceptable.
        {
            let (_root, provider_root, journal_root, mut transport) = open_fixture();
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Published),
                    ));
                assert!(transport
                    .publish_object_exact(ContentDigest::of(bytes), bytes)
                    .is_err());
            }
            let destination = destination_of(&provider_root);
            assert_eq!(std::fs::read(&destination).unwrap(), bytes);
            let replacement = destination.with_extension("provider-replacement");
            std::fs::write(&replacement, bytes).unwrap();
            std::fs::rename(&replacement, &destination).unwrap();
            drop(transport);
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), bytes);
            assert_eq!(retained_dir_count(&journal_root.join("records")), 0);
        }

        // (7) Conflicting destination replacement across a crash. Threat: a
        // sync service delivers DIFFERENT bytes at the canonical name while
        // the put is mid-retry; the retry fails closed and never overwrites
        // the delivered file.
        {
            let (_root, provider_root, journal_root, mut transport) = open_fixture();
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Published),
                    ));
                assert!(transport
                    .publish_object_exact(ContentDigest::of(bytes), bytes)
                    .is_err());
            }
            let destination = destination_of(&provider_root);
            std::fs::remove_file(&destination).unwrap();
            std::fs::write(&destination, b"conflicting delivered bytes").unwrap();
            drop(transport);
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            assert!(matches!(
                transport.publish_object_exact(ContentDigest::of(bytes), bytes),
                Err(ScenarioError::ProviderConflictingBytes(_))
            ));
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                b"conflicting delivered bytes"
            );
        }

        // (8) Destination occupied during a pre-publication retry. Threat: an
        // external delivery races the crashed put's destination name. The
        // retry never claims success, never overwrites the occupant, keeps
        // the honest staging, and converges once the occupant clears.
        {
            let (_root, provider_root, journal_root, mut transport) = open_fixture();
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::PublishIntent),
                    ));
                assert!(transport
                    .publish_object_exact(ContentDigest::of(bytes), bytes)
                    .is_err());
            }
            let destination = destination_of(&provider_root);
            assert!(!destination.exists());
            let staging = staging_path_of(&provider_root);
            assert_eq!(std::fs::read(&staging).unwrap(), bytes);
            std::fs::write(&destination, b"externally delivered occupant").unwrap();
            drop(transport);
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            assert!(transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .is_err());
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                b"externally delivered occupant"
            );
            assert_eq!(std::fs::read(&staging).unwrap(), bytes);
            std::fs::remove_file(&destination).unwrap();
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), bytes);
            assert_eq!(retained_dir_count(&journal_root.join("records")), 0);
        }
    }

    /// Stage 2e-ii item 11: every retry boundary of the retained provider put
    /// path recovers across a crash without overwriting foreign bytes and
    /// converges exactly on retry. Replaces the simulator scenarios
    /// `provider_put_recovers_from_every_journal_file_boundary`,
    /// `provider_put_creation_crashes_reopen_without_overwrite`, and the
    /// retry half of
    /// `provider_finish_retries_after_physical_publication_validation_error`
    /// at the retained `SharedProviderTransport` boundary.
    #[test]
    fn provider_put_recovers_from_every_retry_boundary_without_overwrite() {
        let bytes: &[u8] = b"retry boundary put bytes";
        let object_path = format!(
            "{PROVIDER_OBJECTS_NAMESPACE}/{}.object",
            ContentDigest::of(bytes)
        );
        // Threat model per row: crash/power cut at the named durable journal
        // boundary or phase (torn write for the physical-publication row),
        // with a file-sync service free to race the canonical name while the
        // device is down.
        let faults = [
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::BeforeBlobDurable),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::BlobDurable),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::CreationRecordDurable),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::BlobInstalled),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::RecordDurable),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::UpdateDurable),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::UpdateInstalled),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::BlobRemoved),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::CompletionDurable),
            ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::RecordRemoved),
            ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Prepared),
            ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Staged),
            ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::PublishIntent),
            ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Published),
            ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Cleanup),
            ProviderRetryFault::AfterPhysicalPublication,
        ];
        for fault in faults {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            let destination = provider_root.join("outbox").join(&object_path);
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Put(fault),
                );
                assert!(
                    matches!(
                        transport.publish_object_exact(ContentDigest::of(bytes), bytes),
                        Err(ScenarioError::Io(message)) if message.contains("injected")
                    ),
                    "{fault:?}"
                );
            }
            // A destination either does not exist yet or already carries the
            // exact bytes; no boundary may expose torn destination bytes.
            let published_before_crash = destination.exists();
            if published_before_crash {
                assert_eq!(std::fs::read(&destination).unwrap(), bytes, "{fault:?}");
            }
            drop(transport);
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            if !published_before_crash {
                // Threat: a sync service delivers an unrelated file at the
                // canonical name while this device is crashed. The retry must
                // refuse and must not overwrite the delivered bytes.
                std::fs::write(&destination, b"unrelated delivered bytes").unwrap();
                assert!(
                    transport
                        .publish_object_exact(ContentDigest::of(bytes), bytes)
                        .is_err(),
                    "{fault:?}"
                );
                assert_eq!(
                    std::fs::read(&destination).unwrap(),
                    b"unrelated delivered bytes",
                    "{fault:?}"
                );
                std::fs::remove_file(&destination).unwrap();
            }
            // Exact convergence on retry.
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), bytes, "{fault:?}");
            assert_eq!(
                retained_dir_count(&journal_root.join("records")),
                0,
                "{fault:?}"
            );
            assert_eq!(
                retained_dir_count(&journal_root.join("blobs")),
                0,
                "{fault:?}"
            );
            assert_eq!(
                retained_dir_count(&provider_root.join("outbox").join(PROVIDER_TEMP_NAMESPACE)),
                0,
                "{fault:?}"
            );
            // Idempotent re-publication of the converged bytes.
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), bytes, "{fault:?}");
        }
    }

    /// Stage 2e-ii item 12: every retry and retirement boundary of the
    /// retained provider rename recovers across a crash without overwriting
    /// foreign bytes, and only the exact operation may resume its journal
    /// record. Replaces the simulator scenarios
    /// `provider_rename_recovers_from_every_durable_journal_phase`,
    /// `provider_rename_creation_crashes_reopen_without_overwrite`,
    /// `provider_rename_retry_reconciles_destination_before_source_reopen`,
    /// `provider_rename_retry_rejects_a_conflicting_published_destination`,
    /// `crash_restart_reconciles_rename_from_disk_after_process_state_loss`,
    /// `retirement_race_before_public_cleanup_preserves_foreign_bytes_and_retry_converges`,
    /// `rename_retirement_private_boundaries_are_crash_closed`, and
    /// `provider_rename_post_validation_replacement_cannot_publish_attacker_bytes`
    /// on the retained provider machinery without the simulator.
    #[test]
    fn provider_rename_recovers_from_every_retry_and_retirement_boundary_without_overwrite() {
        let bytes: &[u8] = b"retry boundary rename bytes";
        let rename = |device: &RetainedProviderDevice, event_id: u64| {
            run_provider_rename(
                device,
                event_id,
                ProviderTree::Inbox,
                "objects/source",
                "objects/destination",
            )
        };
        let seeded_device = |root: &std::path::Path| {
            let device = retained_provider_device(root);
            // The source arrives the way a file-sync service delivers it: as
            // a plain file in the provider tree.
            std::fs::write(root.join("provider/inbox/objects/source"), bytes).unwrap();
            device
        };

        // (A) Construction crashes: crash/power cut at each journal file
        // boundary of the rename record's construction. The source survives,
        // nothing reaches the destination, a foreign file delivered at the
        // destination name while the device is down is never overwritten, and
        // the exact retry converges with no journal or evidence residue.
        for boundary in [
            ProviderJournalBoundary::BeforeBlobDurable,
            ProviderJournalBoundary::BlobDurable,
            ProviderJournalBoundary::CreationRecordDurable,
            ProviderJournalBoundary::BlobInstalled,
            ProviderJournalBoundary::RecordDurable,
        ] {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Rename(ProviderRetryFault::AtJournalBoundary(boundary)),
                );
                assert!(
                    matches!(
                        rename(&device, 7),
                        Err(ScenarioError::Io(message)) if message.contains("injected")
                    ),
                    "{boundary:?}"
                );
            }
            assert_eq!(
                std::fs::read(inbox.join("objects/source")).unwrap(),
                bytes,
                "{boundary:?}"
            );
            assert!(!inbox.join("objects/destination").exists(), "{boundary:?}");
            drop(device);
            let device = retained_provider_device(&root.0);
            std::fs::write(inbox.join("objects/destination"), b"unrelated destination").unwrap();
            assert!(rename(&device, 7).is_err(), "{boundary:?}");
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                b"unrelated destination",
                "{boundary:?}"
            );
            assert_eq!(
                std::fs::read(inbox.join("objects/source")).unwrap(),
                bytes,
                "{boundary:?}"
            );
            std::fs::remove_file(inbox.join("objects/destination")).unwrap();
            rename(&device, 7).unwrap();
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes,
                "{boundary:?}"
            );
            assert!(!inbox.join("objects/source").exists(), "{boundary:?}");
            let journal = root.0.join("provider-local-journal");
            assert_eq!(
                retained_dir_count(&journal.join("records")),
                0,
                "{boundary:?}"
            );
            assert_eq!(
                retained_dir_count(&journal.join("blobs")),
                0,
                "{boundary:?}"
            );
            assert_eq!(
                retained_dir_count(&inbox.join(PROVIDER_RENAME_EVIDENCE_NAMESPACE)),
                0,
                "{boundary:?}"
            );
        }

        // (B) Durable-phase crashes: crash/power cut immediately after each
        // durable journal phase; the exact retry converges from disk alone.
        for phase in [
            ProviderJournalPhase::Prepared,
            ProviderJournalPhase::Staged,
            ProviderJournalPhase::PublishIntent,
            ProviderJournalPhase::Published,
            ProviderJournalPhase::RetireIntent,
            ProviderJournalPhase::Retired,
            ProviderJournalPhase::Cleanup,
        ] {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Rename(ProviderRetryFault::AfterDurablePhase(phase)),
                );
                assert!(rename(&device, 7).is_err(), "{phase:?}");
            }
            drop(device);
            let device = retained_provider_device(&root.0);
            rename(&device, 7).unwrap();
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes,
                "{phase:?}"
            );
            assert!(!inbox.join("objects/source").exists(), "{phase:?}");
            let journal = root.0.join("provider-local-journal");
            assert_eq!(retained_dir_count(&journal.join("records")), 0, "{phase:?}");
            assert_eq!(retained_dir_count(&journal.join("blobs")), 0, "{phase:?}");
        }

        // (C) Publication-validation fault: a validation error reported after
        // the destination was physically published (torn-write model). The
        // published destination is reconciled before the source is reopened,
        // and the retry leaves no residue.
        {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Rename(ProviderRetryFault::AfterPhysicalPublication),
                );
                assert!(matches!(
                    rename(&device, 7),
                    Err(ScenarioError::Io(message)) if message.contains("injected provider rename")
                ));
            }
            assert!(!inbox.join("objects/source").exists());
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes
            );
            rename(&device, 7).unwrap();
            assert_eq!(
                retained_dir_count(&inbox.join(PROVIDER_RENAME_EVIDENCE_NAMESPACE)),
                0
            );
            assert_eq!(
                retained_dir_count(&root.0.join("provider-local-journal/records")),
                0
            );
        }

        // (C2) The same fault followed by a conflicting external replacement
        // of the published destination: the retry fails closed, quarantines
        // the conflict into diagnostic residue, and never republishes over
        // foreign bytes.
        {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Rename(ProviderRetryFault::AfterPhysicalPublication),
                );
                assert!(rename(&device, 7).is_err());
            }
            std::fs::remove_file(inbox.join("objects/destination")).unwrap();
            std::fs::write(inbox.join("objects/destination"), b"attacker conflict").unwrap();
            assert!(matches!(
                rename(&device, 7),
                Err(ScenarioError::UnsafeProviderEntry(path)) if path == "objects/destination"
            ));
            assert!(!inbox.join("objects/destination").exists());
            assert!(
                std::fs::read_dir(inbox.join(PROVIDER_REMOVED_NAMESPACE))
                    .unwrap()
                    .any(|entry| std::fs::read(entry.unwrap().path()).unwrap()
                        == b"attacker conflict")
            );
        }

        // (D) Retirement private boundaries: crash/power cut at each private
        // boundary of the placeholder-exchange retirement; the retry
        // converges without evidence residue.
        #[cfg(unix)]
        for boundary in [
            ProviderJournalBoundary::RetirementPlaceholderDurable,
            ProviderJournalBoundary::RetirementExchangeDurable,
            ProviderJournalBoundary::RetirementPlaceholderQuarantined,
            ProviderJournalBoundary::RetirementPlaceholderPrivateDeleted,
        ] {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Rename(ProviderRetryFault::AtJournalBoundary(boundary)),
                );
                assert!(
                    matches!(
                        rename(&device, 7),
                        Err(ScenarioError::Io(message)) if message.contains("injected")
                    ),
                    "{boundary:?}"
                );
            }
            drop(device);
            let device = retained_provider_device(&root.0);
            rename(&device, 7).unwrap();
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes,
                "{boundary:?}"
            );
            assert!(!inbox.join("objects/source").exists(), "{boundary:?}");
            assert_eq!(
                retained_dir_count(&inbox.join(PROVIDER_RENAME_EVIDENCE_NAMESPACE)),
                0,
                "{boundary:?}"
            );
        }

        // (E) Freed-name race at retirement: an external editor or sync
        // service replaces the source in the retirement window. The foreign
        // replacement is preserved as rename evidence — never destroyed, never
        // published — and the exact retry converges while keeping it.
        {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            let raced_source = inbox.join("objects/source");
            let race_target = raced_source.clone();
            let _race = install_provider_publication_race_for_test(
                ProviderPublicationRace::RetirementBeforePrivateMove,
                Box::new(move || {
                    std::fs::remove_file(&race_target).unwrap();
                    std::fs::write(&race_target, b"foreign replacement").unwrap();
                }),
            );
            assert!(matches!(
                rename(&device, 7),
                Err(ScenarioError::UnsafeProviderEntry(_))
            ));
            drop(_race);
            assert!(!raced_source.exists());
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes
            );
            let evidence = inbox.join(PROVIDER_RENAME_EVIDENCE_NAMESPACE);
            let retained: Vec<_> = std::fs::read_dir(&evidence)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(retained.len(), 1);
            assert_eq!(std::fs::read(&retained[0]).unwrap(), b"foreign replacement");
            drop(device);
            let device = retained_provider_device(&root.0);
            rename(&device, 7).unwrap();
            assert_eq!(std::fs::read(&retained[0]).unwrap(), b"foreign replacement");
            assert_eq!(retained_dir_count(&evidence), 1);
        }

        // (F) Post-validation source replacement: an external editor swaps
        // the validated source for attacker bytes right after validation. The
        // journal blob keeps publication honest, the attacker bytes are never
        // published and never destroyed, and the operation reports failure.
        #[cfg(unix)]
        {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            let original = inbox.join("objects/source");
            let retained_name = inbox.join("objects/validated-original");
            let replacement = original.clone();
            let retained_for_race = retained_name.clone();
            let _race = install_provider_publication_race_for_test(
                ProviderPublicationRace::RenameAfterSourceValidation,
                Box::new(move || {
                    std::fs::rename(&replacement, &retained_for_race).unwrap();
                    std::fs::write(&replacement, b"attacker rename bytes").unwrap();
                }),
            );
            assert!(matches!(
                rename(&device, 7),
                Err(ScenarioError::UnsafeProviderEntry(path)) if path == "objects/source"
            ));
            assert_eq!(std::fs::read(&retained_name).unwrap(), bytes);
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes
            );
            assert_eq!(std::fs::read(&original).unwrap(), b"attacker rename bytes");
        }

        // (G) Exact-retry binding: after a crash at the Published phase, a
        // DIFFERENT rename operation (another event id — the shape of a
        // concurrent honest instance's own work) may not adopt the in-flight
        // journal record and fails against the occupied destination without
        // disturbing it; only the exact retry converges.
        {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Rename(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Published),
                    ));
                assert!(rename(&device, 7).is_err());
            }
            drop(device);
            let device = retained_provider_device(&root.0);
            assert!(matches!(
                rename(&device, 8),
                Err(ScenarioError::ProviderConflictingBytes(path))
                    if path == "objects/destination"
            ));
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes
            );
            assert_eq!(std::fs::read(inbox.join("objects/source")).unwrap(), bytes);
            rename(&device, 7).unwrap();
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes
            );
            assert!(!inbox.join("objects/source").exists());
        }
    }

    /// Stage 2e-ii item 13: every retry boundary of the retained provider
    /// remove recovers across a crash, leaves the prescribed visible
    /// evidence, and never deletes a replacement that took the freed name.
    /// Replaces the simulator scenarios
    /// `provider_remove_recovers_from_every_durable_journal_phase` and
    /// `provider_remove_post_validation_replacement_cannot_delete_attacker_bytes`
    /// on the retained provider machinery without the simulator.
    #[test]
    fn provider_remove_recovers_from_every_retry_boundary_without_deleting_replacements() {
        let bytes: &[u8] = b"retry boundary remove bytes";
        let remove = |device: &RetainedProviderDevice, event_id: u64| {
            run_provider_remove(device, event_id, ProviderTree::Inbox, "objects/source")
        };
        let seeded_device = |root: &std::path::Path| {
            let device = retained_provider_device(root);
            std::fs::write(root.join("provider/inbox/objects/source"), bytes).unwrap();
            device
        };
        let removed_evidence_bytes = |inbox: &std::path::Path| {
            std::fs::read_dir(inbox.join(PROVIDER_REMOVED_NAMESPACE))
                .unwrap()
                .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
                .collect::<Vec<_>>()
        };

        // (A) Durable-phase crashes: crash/power cut immediately after each
        // durable phase of the remove journal; the exact retry converges, the
        // source is gone, the retired bytes remain as visible diagnostic
        // evidence, and the journal is clean.
        for phase in [
            ProviderJournalPhase::Prepared,
            ProviderJournalPhase::RetireIntent,
            ProviderJournalPhase::Retired,
            ProviderJournalPhase::Cleanup,
        ] {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Remove(ProviderRetryFault::AfterDurablePhase(phase)),
                );
                assert!(
                    matches!(
                        remove(&device, 7),
                        Err(ScenarioError::Io(message)) if message.contains("injected")
                    ),
                    "{phase:?}"
                );
            }
            drop(device);
            let device = retained_provider_device(&root.0);
            remove(&device, 7).unwrap();
            assert!(!inbox.join("objects/source").exists(), "{phase:?}");
            assert_eq!(
                removed_evidence_bytes(&inbox),
                vec![bytes.to_vec()],
                "{phase:?}"
            );
            let journal = root.0.join("provider-local-journal");
            assert_eq!(retained_dir_count(&journal.join("records")), 0, "{phase:?}");
        }

        // (B) Retirement private boundaries: crash/power cut at each private
        // boundary of the placeholder-exchange retirement; the retry
        // converges without deleting anything but the authorized source.
        #[cfg(unix)]
        for boundary in [
            ProviderJournalBoundary::RetirementPlaceholderDurable,
            ProviderJournalBoundary::RetirementExchangeDurable,
            ProviderJournalBoundary::RetirementPlaceholderQuarantined,
            ProviderJournalBoundary::RetirementPlaceholderPrivateDeleted,
        ] {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Remove(ProviderRetryFault::AtJournalBoundary(boundary)),
                );
                assert!(
                    matches!(
                        remove(&device, 7),
                        Err(ScenarioError::Io(message)) if message.contains("injected")
                    ),
                    "{boundary:?}"
                );
            }
            drop(device);
            let device = retained_provider_device(&root.0);
            remove(&device, 7).unwrap();
            assert!(!inbox.join("objects/source").exists(), "{boundary:?}");
            assert_eq!(
                removed_evidence_bytes(&inbox),
                vec![bytes.to_vec()],
                "{boundary:?}"
            );
        }

        // (C) Replacement survival at the freed name: after the retirement
        // became durable but before completion, a sync service delivers a NEW
        // file at the removed path. The exact retry completes without
        // deleting the new owner's bytes.
        {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Remove(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Retired),
                    ));
                assert!(remove(&device, 7).is_err());
            }
            assert!(!inbox.join("objects/source").exists());
            std::fs::write(inbox.join("objects/source"), b"new owner bytes").unwrap();
            drop(device);
            let device = retained_provider_device(&root.0);
            remove(&device, 7).unwrap();
            assert_eq!(
                std::fs::read(inbox.join("objects/source")).unwrap(),
                b"new owner bytes"
            );
            assert_eq!(removed_evidence_bytes(&inbox), vec![bytes.to_vec()]);
            assert_eq!(
                retained_dir_count(&root.0.join("provider-local-journal/records")),
                0
            );
        }

        // (D) Post-validation replacement: an external editor swaps the
        // validated source for attacker bytes right after validation. The
        // remove fails closed, deletes nothing, and both the attacker bytes
        // and the original survive.
        #[cfg(unix)]
        {
            let root = ScenarioRoot::new().unwrap();
            let device = seeded_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            let original = inbox.join("objects/source");
            let retained_name = inbox.join("objects/validated-original");
            let replacement = original.clone();
            let retained_for_race = retained_name.clone();
            let _race = install_provider_publication_race_for_test(
                ProviderPublicationRace::RemoveAfterSourceValidation,
                Box::new(move || {
                    std::fs::rename(&replacement, &retained_for_race).unwrap();
                    std::fs::write(&replacement, b"attacker remove bytes").unwrap();
                }),
            );
            assert!(matches!(
                remove(&device, 7),
                Err(ScenarioError::UnsafeProviderEntry(path)) if path == "objects/source"
            ));
            assert_eq!(std::fs::read(&original).unwrap(), b"attacker remove bytes");
            assert_eq!(std::fs::read(&retained_name).unwrap(), bytes);
            assert_eq!(
                retained_dir_count(&inbox.join(PROVIDER_REMOVED_NAMESPACE)),
                0
            );
        }
    }

    // ===== Stage 2e-ii wave 3b: orphan quarantine, journal corruption, =====
    // ===== transaction gates, and retirement fallback on the retained  =====
    // ===== provider path                                               =====
    //
    // Same design rule as wave 3a: every boundary these tests need is already
    // reachable through an existing `#[cfg(test)]` slot beside the production
    // code (`FAIL_PROVIDER_JOURNAL_BOUNDARY`'s orphan and retirement
    // variants, `PROVIDER_ORPHAN_AFTER_QUARANTINE_HOOK`,
    // `PROVIDER_PUBLICATION_SOURCE_VALIDATION_HOOK`, and the process-global
    // `InjectedSharedProviderFlaggedRenameFailure` errno control) or through
    // the public journal surface itself (`ProviderRetryJournal::open`,
    // `acquire_transaction_gate`), so the installers below are RAII guards
    // over those existing slots and NO new production consultation point is
    // added. The retry-journal snapshot the coverage inventory named needs no
    // hook at all: the journal is a plain directory tree, so the snapshot is
    // read from disk.

    /// One private boundary inside the orphan-blob retirement that runs at
    /// journal open.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ProviderOrphanBoundary {
        /// Crash/power cut right after the unowned creation blob moved into
        /// the private quarantine.
        Quarantined,
        /// Crash/power cut right after the authenticated-owner recheck.
        OwnershipRechecked,
        /// Crash/power cut right after quarantined bytes were restored to an
        /// authenticated owner record.
        Restored,
        /// Crash/power cut right after the unowned quarantine copy was
        /// deleted.
        PrivateDeleted,
    }

    struct InstalledProviderOrphanBoundaryFault;

    fn install_provider_orphan_boundary_fault_for_test(
        boundary: ProviderOrphanBoundary,
    ) -> InstalledProviderOrphanBoundaryFault {
        let at = match boundary {
            ProviderOrphanBoundary::Quarantined => ProviderJournalBoundary::OrphanQuarantined,
            ProviderOrphanBoundary::OwnershipRechecked => {
                ProviderJournalBoundary::OrphanOwnershipRechecked
            }
            ProviderOrphanBoundary::Restored => ProviderJournalBoundary::OrphanRestored,
            ProviderOrphanBoundary::PrivateDeleted => ProviderJournalBoundary::OrphanPrivateDeleted,
        };
        FAIL_PROVIDER_JOURNAL_BOUNDARY.with(|hook| hook.replace(Some(at)));
        InstalledProviderOrphanBoundaryFault
    }

    impl Drop for InstalledProviderOrphanBoundaryFault {
        fn drop(&mut self) {
            FAIL_PROVIDER_JOURNAL_BOUNDARY.with(|hook| hook.replace(None));
        }
    }

    /// An action installed at the race window immediately after an orphan
    /// blob was quarantined, before its ownership recheck.
    struct InstalledProviderOrphanQuarantineRace;

    fn install_provider_orphan_quarantine_race_for_test(
        action: Box<dyn FnOnce()>,
    ) -> InstalledProviderOrphanQuarantineRace {
        PROVIDER_ORPHAN_AFTER_QUARANTINE_HOOK.with(|hook| hook.replace(Some(action)));
        InstalledProviderOrphanQuarantineRace
    }

    impl Drop for InstalledProviderOrphanQuarantineRace {
        fn drop(&mut self) {
            PROVIDER_ORPHAN_AFTER_QUARANTINE_HOOK.with(|hook| hook.replace(None));
        }
    }

    /// The complete private retry-journal state, straight from disk — the
    /// observation surface item 15 asked for. No production hook is needed:
    /// the journal IS a directory tree, and snapshot equality across a
    /// refused open is exactly "failed before mutation".
    fn provider_retry_journal_snapshot_for_test(
        journal_root: &std::path::Path,
    ) -> BTreeMap<String, Vec<u8>> {
        provider_tree_bytes(journal_root)
    }

    /// Stage 2e-ii item 14: orphan-blob quarantine at journal open recovers
    /// from a crash at every private boundary and restores the bytes when an
    /// authenticated owner record arrives inside the race window. Replaces
    /// the simulator scenarios
    /// `provider_orphan_blob_retirement_is_crash_closed`,
    /// `orphan_quarantine_restores_bytes_when_authenticated_owner_arrives_at_race_boundary`,
    /// and `orphan_retirement_private_boundaries_are_crash_closed` on the
    /// retained transport without the simulator.
    #[test]
    fn provider_orphan_quarantine_recovers_from_every_boundary() {
        let bytes: &[u8] = b"orphan quarantine boundary bytes";
        let object_path = format!(
            "{PROVIDER_OBJECTS_NAMESPACE}/{}.object",
            ContentDigest::of(bytes)
        );
        let operation_id = generated_put_operation_id(&object_path, bytes);
        // Seed one unowned `.creating` blob: crash/power cut right after the
        // creation blob became durable, before any owner record existed.
        let seed_orphan = |provider_root: &std::path::Path, journal_root: &std::path::Path| {
            let mut transport = SharedProviderTransport::open(provider_root, journal_root).unwrap();
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                        ProviderRetryFault::AtJournalBoundary(ProviderJournalBoundary::BlobDurable),
                    ));
                assert!(matches!(
                    transport.publish_object_exact(ContentDigest::of(bytes), bytes),
                    Err(ScenarioError::Io(message)) if message.contains("injected")
                ));
            }
            drop(transport);
            assert_eq!(retained_dir_count(&journal_root.join("records")), 0);
            assert_eq!(
                std::fs::read(
                    journal_root
                        .join("blobs")
                        .join(ProviderRetryJournal::creating_blob_name(&operation_id)),
                )
                .unwrap(),
                bytes
            );
        };

        // (1) Crash-closed retirement. Threat: crash/power cut at each
        // private boundary of the unowned-orphan retirement that runs at
        // reopen. The bytes are never torn out of both private namespaces at
        // once before their ownership was decided, the next reopen converges,
        // and the exact retry publishes.
        for boundary in [
            ProviderOrphanBoundary::Quarantined,
            ProviderOrphanBoundary::OwnershipRechecked,
            ProviderOrphanBoundary::PrivateDeleted,
        ] {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            seed_orphan(&provider_root, &journal_root);
            {
                let _fault = install_provider_orphan_boundary_fault_for_test(boundary);
                assert!(
                    matches!(
                        SharedProviderTransport::open(&provider_root, &journal_root),
                        Err(ScenarioError::Io(message)) if message.contains("journal crash")
                    ),
                    "{boundary:?}"
                );
            }
            match boundary {
                ProviderOrphanBoundary::Quarantined
                | ProviderOrphanBoundary::OwnershipRechecked => {
                    assert_eq!(
                        std::fs::read(
                            journal_root
                                .join("quarantine")
                                .join(ProviderRetryJournal::creating_blob_name(&operation_id)),
                        )
                        .unwrap(),
                        bytes,
                        "{boundary:?}"
                    );
                    assert_eq!(
                        retained_dir_count(&journal_root.join("blobs")),
                        0,
                        "{boundary:?}"
                    );
                }
                ProviderOrphanBoundary::PrivateDeleted => {
                    assert_eq!(
                        retained_dir_count(&journal_root.join("quarantine")),
                        0,
                        "{boundary:?}"
                    );
                    assert_eq!(
                        retained_dir_count(&journal_root.join("blobs")),
                        0,
                        "{boundary:?}"
                    );
                }
                ProviderOrphanBoundary::Restored => unreachable!(),
            }
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            assert_eq!(
                retained_dir_count(&journal_root.join("quarantine")),
                0,
                "{boundary:?}"
            );
            assert_eq!(
                retained_dir_count(&journal_root.join("blobs")),
                0,
                "{boundary:?}"
            );
            // Exact retry converges and is idempotent.
            let destination = provider_root.join("outbox").join(&object_path);
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), bytes, "{boundary:?}");
            assert_eq!(
                retained_dir_count(&journal_root.join("records")),
                0,
                "{boundary:?}"
            );
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), bytes, "{boundary:?}");
        }

        // (2) Authenticated-owner race. Threat: a crash/power cut interleaves
        // orphan retirement with the recovery shape an exact retry leaves
        // behind — an authenticated owner update record lands right after the
        // blob was quarantined. Ownership is rechecked AFTER quarantining, so
        // the bytes are restored to the owner's creation name rather than
        // deleted, and a further crash at the restore boundary still
        // converges to the published destination.
        {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            seed_orphan(&provider_root, &journal_root);
            let binding = format!("generated:{}", provider_digest(bytes));
            let hook_journal = journal_root.clone();
            let hook_operation_id = operation_id.clone();
            let hook_bytes = bytes.to_vec();
            let hook_path = object_path.clone();
            let race = install_provider_orphan_quarantine_race_for_test(Box::new(move || {
                let key: [u8; 32] = std::fs::read(hook_journal.join("authority.key"))
                    .unwrap()
                    .try_into()
                    .unwrap();
                let mut record = ProviderJournalRecord {
                    journal_schema_version: PROVIDER_JOURNAL_SCHEMA_VERSION,
                    operation_id: hook_operation_id.clone(),
                    operation: ProviderJournalOperation::Put,
                    operation_binding: binding.clone(),
                    source_provenance: binding.clone(),
                    tree: ProviderTree::Outbox,
                    from_path: hook_path.clone(),
                    to_path: None,
                    source_identity: None,
                    source_len: u64::try_from(hook_bytes.len()).unwrap(),
                    source_digest: provider_digest(&hook_bytes),
                    blob_name: Some(ProviderRetryJournal::blob_name(&hook_operation_id)),
                    phase: ProviderJournalPhase::Prepared,
                    staging_identity: None,
                    destination_identity: None,
                    staging_name: Some(ProviderRetryJournal::staging_name(&hook_operation_id, 0)),
                    staging_generation: 0,
                    diagnostic_path: None,
                    authentication_tag: String::new(),
                };
                let unsigned = serde_json::to_vec(&record).unwrap();
                record.authentication_tag = hmac_sha256_hex(&key, &unsigned);
                let update = serde_json::to_vec(&record).unwrap();
                let update_path = hook_journal
                    .join("records")
                    .join(format!("{hook_operation_id}.update"));
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(update_path)
                    .unwrap();
                file.write_all(&update).unwrap();
                file.sync_all().unwrap();
                std::fs::File::open(hook_journal.join("records"))
                    .unwrap()
                    .sync_all()
                    .unwrap();
            }));
            let fault =
                install_provider_orphan_boundary_fault_for_test(ProviderOrphanBoundary::Restored);
            assert!(matches!(
                SharedProviderTransport::open(&provider_root, &journal_root),
                Err(ScenarioError::Io(message)) if message.contains("journal crash")
            ));
            drop(race);
            drop(fault);
            // The crash landed AFTER the restore: the bytes are back at the
            // owner's creation name and the quarantine window lost nothing.
            assert_eq!(
                std::fs::read(
                    journal_root
                        .join("blobs")
                        .join(ProviderRetryJournal::creating_blob_name(&operation_id)),
                )
                .unwrap(),
                bytes
            );
            assert_eq!(retained_dir_count(&journal_root.join("quarantine")), 0);
            // Clean reopen converges and the exact retry publishes the bytes.
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(
                std::fs::read(provider_root.join("outbox").join(&object_path)).unwrap(),
                bytes
            );
            assert_eq!(retained_dir_count(&journal_root.join("records")), 0);
            assert_eq!(retained_dir_count(&journal_root.join("blobs")), 0);
        }
    }

    /// Stage 2e-ii item 15: every corruption or bound violation of the
    /// private retry journal fails BEFORE any journal or provider mutation,
    /// observed as snapshot equality across the refused operation. Replaces
    /// the simulator scenarios
    /// `every_journal_record_class_rejects_truncated_auth_invalid_unknown_and_future_bytes_on_open`,
    /// `orphan_wrong_and_shared_blob_ownership_fail_before_reconciliation`,
    /// `authenticated_record_load_rederives_source_provenance_length_and_digest_identity`,
    /// `corrupt_substituted_or_multilink_local_journal_fails_closed`, and the
    /// live half of `provider_journal_enforces_numeric_entry_and_byte_bounds`
    /// on the retained provider machinery without the simulator.
    #[test]
    fn provider_retry_journal_corruption_and_bounds_fail_before_mutation() {
        let bytes: &[u8] = b"retry journal corruption bytes";
        let object_path = format!(
            "{PROVIDER_OBJECTS_NAMESPACE}/{}.object",
            ContentDigest::of(bytes)
        );
        let sign_with_disk_key =
            |journal_root: &std::path::Path, record: &mut ProviderJournalRecord| {
                let key: [u8; 32] = std::fs::read(journal_root.join("authority.key"))
                    .unwrap()
                    .try_into()
                    .unwrap();
                record.authentication_tag = String::new();
                let unsigned = serde_json::to_vec(&record).unwrap();
                record.authentication_tag = hmac_sha256_hex(&key, &unsigned);
            };

        // (A) Record classes × byte corruption at open. Threats per
        // corruption: a torn write at power cut (truncated), disk corruption
        // of the tag or body (auth-invalid), and an honest newer app version
        // crashing before a downgrade (unknown field, future schema) — the
        // old binary must fail closed instead of mis-parsing.
        for record_class in ["pending", "update", "completed"] {
            for corruption in ["truncated", "auth-invalid", "unknown", "future"] {
                let case = format!("{record_class}/{corruption}");
                let root = ScenarioRoot::new().unwrap();
                let provider_root = root.0.join("provider");
                let journal_root = root.0.join("private/device/journal");
                let destination = provider_root.join("outbox").join(&object_path);
                let mut transport =
                    SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
                let target = match record_class {
                    "pending" => {
                        let _fault = install_provider_retry_boundary_fault_for_test(
                            ProviderRetryBoundary::Put(ProviderRetryFault::AfterDurablePhase(
                                ProviderJournalPhase::Prepared,
                            )),
                        );
                        assert!(
                            transport
                                .publish_object_exact(ContentDigest::of(bytes), bytes)
                                .is_err(),
                            "{case}"
                        );
                        journal_root
                            .join("records")
                            .join(ProviderRetryJournal::record_name(
                                &generated_put_operation_id(&object_path, bytes),
                            ))
                    }
                    "update" => {
                        let _fault = install_provider_retry_boundary_fault_for_test(
                            ProviderRetryBoundary::Put(ProviderRetryFault::AtJournalBoundary(
                                ProviderJournalBoundary::UpdateDurable,
                            )),
                        );
                        assert!(
                            transport
                                .publish_object_exact(ContentDigest::of(bytes), bytes)
                                .is_err(),
                            "{case}"
                        );
                        journal_root.join("records").join(format!(
                            "{}.update",
                            generated_put_operation_id(&object_path, bytes)
                        ))
                    }
                    "completed" => {
                        transport
                            .publish_object_exact(ContentDigest::of(bytes), bytes)
                            .unwrap();
                        std::fs::read_dir(journal_root.join("completed"))
                            .unwrap()
                            .next()
                            .unwrap()
                            .unwrap()
                            .path()
                    }
                    _ => unreachable!(),
                };
                drop(transport);
                let destination_before = destination.exists();
                let original = std::fs::read(&target).unwrap();
                match corruption {
                    "truncated" => {
                        std::fs::write(&target, &original[..original.len() / 2]).unwrap();
                    }
                    "auth-invalid" => {
                        let mut value: serde_json::Value =
                            serde_json::from_slice(&original).unwrap();
                        value["authentication_tag"] = serde_json::json!("0".repeat(64));
                        std::fs::write(&target, serde_json::to_vec(&value).unwrap()).unwrap();
                    }
                    "unknown" => {
                        let mut value: serde_json::Value =
                            serde_json::from_slice(&original).unwrap();
                        value["unknown_future_field"] = serde_json::json!(true);
                        std::fs::write(&target, serde_json::to_vec(&value).unwrap()).unwrap();
                    }
                    "future" => {
                        let mut record: ProviderJournalRecord =
                            serde_json::from_slice(&original).unwrap();
                        record.journal_schema_version = PROVIDER_JOURNAL_SCHEMA_VERSION + 1;
                        sign_with_disk_key(&journal_root, &mut record);
                        std::fs::write(&target, serde_json::to_vec(&record).unwrap()).unwrap();
                    }
                    _ => unreachable!(),
                }
                let before = provider_retry_journal_snapshot_for_test(&journal_root);
                assert!(
                    matches!(
                        ProviderRetryJournal::open(journal_root.clone()),
                        Err(ScenarioError::UnsafeProviderJournal(_))
                    ),
                    "{case}"
                );
                assert_eq!(
                    provider_retry_journal_snapshot_for_test(&journal_root),
                    before,
                    "{case}: the refused open must not mutate the journal"
                );
                assert_eq!(destination.exists(), destination_before, "{case}");
                if destination_before {
                    assert_eq!(std::fs::read(&destination).unwrap(), bytes, "{case}");
                }
            }
        }

        // (B) Blob ownership and link corruption at open. Threats: a crash of
        // a different (older/newer) binary at an unjournaled boundary or
        // disk-repair residue (bare orphan `.blob`), and fsck/lost+found
        // style relinking (wrong name, shared hard link). Each fails closed
        // before reconciliation can rename or delete anything.
        for corruption in ["bare-orphan-blob", "wrong-name", "shared-link"] {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            if corruption == "bare-orphan-blob" {
                drop(transport);
                std::fs::write(
                    journal_root
                        .join("blobs")
                        .join(format!("{}.blob", "a".repeat(64))),
                    bytes,
                )
                .unwrap();
            } else {
                {
                    let _fault =
                        install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                            ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Prepared),
                        ));
                    assert!(
                        transport
                            .publish_object_exact(ContentDigest::of(bytes), bytes)
                            .is_err(),
                        "{corruption}"
                    );
                }
                drop(transport);
                let blob = std::fs::read_dir(journal_root.join("blobs"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                let wrong = journal_root
                    .join("blobs")
                    .join(format!("{}.blob", "b".repeat(64)));
                if corruption == "wrong-name" {
                    std::fs::rename(blob, wrong).unwrap();
                } else {
                    std::fs::hard_link(blob, wrong).unwrap();
                }
            }
            let before = provider_retry_journal_snapshot_for_test(&journal_root);
            assert!(
                matches!(
                    ProviderRetryJournal::open(journal_root.clone()),
                    Err(ScenarioError::UnsafeProviderJournal(_))
                ),
                "{corruption}"
            );
            assert_eq!(
                provider_retry_journal_snapshot_for_test(&journal_root),
                before,
                "{corruption}: the refused open must not mutate the journal"
            );
        }

        // (C) Source-identity rederivation. Threat: disk corruption or a torn
        // partial rewrite drifts a record away from the identity its
        // operation id was derived from; an authenticated reload rederives
        // provenance, length, and digest and refuses the drifted record even
        // though its tag verifies.
        for field in ["provenance", "length", "digest"] {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Prepared),
                    ));
                assert!(
                    transport
                        .publish_object_exact(ContentDigest::of(bytes), bytes)
                        .is_err(),
                    "{field}"
                );
            }
            drop(transport);
            let record_path = journal_root
                .join("records")
                .join(ProviderRetryJournal::record_name(
                    &generated_put_operation_id(&object_path, bytes),
                ));
            let mut record: ProviderJournalRecord =
                serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
            match field {
                "provenance" => record.source_provenance.push_str(":changed"),
                "length" => record.source_len += 1,
                "digest" => record.source_digest = provider_digest(b"changed"),
                _ => unreachable!(),
            }
            sign_with_disk_key(&journal_root, &mut record);
            std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
            let before = provider_retry_journal_snapshot_for_test(&journal_root);
            assert!(
                matches!(
                    ProviderRetryJournal::open(journal_root.clone()),
                    Err(ScenarioError::UnsafeProviderJournal(_))
                ),
                "{field}"
            );
            assert_eq!(
                provider_retry_journal_snapshot_for_test(&journal_root),
                before,
                "{field}: the refused open must not mutate the journal"
            );
        }

        // (D) Retry-time corruption fails before provider mutation. Threat:
        // disk corruption or repair residue inside the private journal while
        // a crashed rename is pending; the exact retry must refuse before the
        // provider tree is touched — the source survives and nothing reaches
        // the destination.
        for attack in [
            "record-corrupt",
            "record-substitute",
            "record-link",
            "blob-corrupt",
            "blob-link",
        ] {
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            std::fs::write(inbox.join("objects/source"), bytes).unwrap();
            let rename = |device: &RetainedProviderDevice| {
                run_provider_rename(
                    device,
                    7,
                    ProviderTree::Inbox,
                    "objects/source",
                    "objects/destination",
                )
            };
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Rename(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Prepared),
                    ));
                assert!(rename(&device).is_err(), "{attack}");
            }
            let journal = root.0.join("provider-local-journal");
            let record_path = std::fs::read_dir(journal.join("records"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let blob_path = std::fs::read_dir(journal.join("blobs"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            match attack {
                "record-corrupt" => std::fs::write(&record_path, b"{}").unwrap(),
                "record-substitute" => {
                    let mut record: ProviderJournalRecord =
                        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
                    record.from_path = "objects/substituted".into();
                    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
                }
                "record-link" => {
                    std::fs::hard_link(&record_path, journal.join("record-alias")).unwrap();
                }
                "blob-corrupt" => std::fs::write(&blob_path, b"corrupt").unwrap(),
                "blob-link" => {
                    std::fs::hard_link(&blob_path, journal.join("blob-alias")).unwrap();
                }
                _ => unreachable!(),
            }
            let retry = rename(&device);
            assert!(
                matches!(retry, Err(ScenarioError::UnsafeProviderJournal(_))),
                "{attack}: {retry:?}"
            );
            assert!(!inbox.join("objects/destination").exists(), "{attack}");
            assert_eq!(
                std::fs::read(inbox.join("objects/source")).unwrap(),
                bytes,
                "{attack}"
            );
        }

        // (E) Entry-count bound. Threat: a crash-looping retry must not grow
        // the private journal without bound (it lives on the user's disk);
        // the refused operation fails with a limit receipt BEFORE creating a
        // record or touching the provider tree.
        {
            assert_eq!(MAX_PROVIDER_JOURNAL_PENDING, 4);
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            for index in 0..=MAX_PROVIDER_JOURNAL_PENDING {
                std::fs::write(inbox.join(format!("objects/source-{index}")), bytes).unwrap();
            }
            for index in 0..=MAX_PROVIDER_JOURNAL_PENDING {
                let result = {
                    let _fault = install_provider_retry_boundary_fault_for_test(
                        ProviderRetryBoundary::Rename(ProviderRetryFault::AfterDurablePhase(
                            ProviderJournalPhase::Prepared,
                        )),
                    );
                    run_provider_rename(
                        &device,
                        100 + index as u64,
                        ProviderTree::Inbox,
                        &format!("objects/source-{index}"),
                        &format!("objects/destination-{index}"),
                    )
                };
                if index < MAX_PROVIDER_JOURNAL_PENDING {
                    assert!(matches!(result, Err(ScenarioError::Io(_))), "{index}");
                } else {
                    assert!(
                        matches!(result, Err(ScenarioError::ProviderJournalLimit)),
                        "{index}: {result:?}"
                    );
                }
            }
            let journal = root.0.join("provider-local-journal");
            assert_eq!(
                retained_dir_count(&journal.join("records")),
                MAX_PROVIDER_JOURNAL_PENDING
            );
            assert_eq!(
                retained_dir_count(&journal.join("blobs")),
                MAX_PROVIDER_JOURNAL_PENDING
            );
            let last = MAX_PROVIDER_JOURNAL_PENDING;
            assert_eq!(
                std::fs::read(inbox.join(format!("objects/source-{last}"))).unwrap(),
                bytes
            );
            assert!(!inbox.join(format!("objects/destination-{last}")).exists());
        }

        // (F) Byte-count bounds. Threat: a torn or corrupted length must not
        // make open read unbounded bytes into memory; an oversized record or
        // blob is refused at open without mutation.
        for target_kind in ["record", "blob"] {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Put(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Prepared),
                    ));
                assert!(
                    transport
                        .publish_object_exact(ContentDigest::of(bytes), bytes)
                        .is_err(),
                    "{target_kind}"
                );
            }
            drop(transport);
            let operation_id = generated_put_operation_id(&object_path, bytes);
            let (target, limit) = if target_kind == "record" {
                (
                    journal_root
                        .join("records")
                        .join(ProviderRetryJournal::record_name(&operation_id)),
                    MAX_PROVIDER_JOURNAL_RECORD_BYTES,
                )
            } else {
                (
                    journal_root
                        .join("blobs")
                        .join(ProviderRetryJournal::blob_name(&operation_id)),
                    MAX_PROVIDER_JOURNAL_BLOB_BYTES,
                )
            };
            let mut inflated = std::fs::read(&target).unwrap();
            inflated.resize(limit + 1, b' ');
            std::fs::write(&target, &inflated).unwrap();
            let before = provider_retry_journal_snapshot_for_test(&journal_root);
            assert!(
                matches!(
                    ProviderRetryJournal::open(journal_root.clone()),
                    Err(ScenarioError::UnsafeProviderJournal(_)
                        | ScenarioError::ProviderJournalLimit)
                ),
                "{target_kind}"
            );
            assert_eq!(
                provider_retry_journal_snapshot_for_test(&journal_root),
                before,
                "{target_kind}: the refused open must not mutate the journal"
            );
        }
    }

    /// Stage 2e-ii item 16: the provider transaction gate is exclusive across
    /// scopes, device lock order is canonical and deduplicated, and gate
    /// acquisition precedes every source and retry-journal inspection.
    /// Replaces the simulator scenarios
    /// `provider_transaction_gate_rejects_a_second_process_scope`,
    /// `provider_transaction_device_order_is_canonical_and_unique`,
    /// `same_device_provider_source_uses_one_gate_and_copy_succeeds`,
    /// `cross_device_copy_waits_for_source_gate_before_source_inspection`,
    /// `cross_device_begin_write_waits_for_source_gate_before_source_inspection`,
    /// and `finish_provider_write_holds_gate_before_source_or_retry_inspection`
    /// at the retained journal and transport boundary.
    #[test]
    fn provider_transaction_gates_precede_source_and_retry_inspection() {
        let bytes: &[u8] = b"transaction gate bytes";

        // (A) Second-scope rejection. Threat: a concurrent honest instance
        // opens the same private journal; while one scope holds the gate the
        // other must be refused, and after release it must be admitted.
        {
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            let journal_root = root.0.join("provider-local-journal");
            let gate = device
                .provider_journal
                .as_ref()
                .unwrap()
                .acquire_transaction_gate()
                .unwrap();
            assert!(matches!(
                ProviderRetryJournal::open(journal_root.clone()),
                Err(ScenarioError::UnsafeProviderJournal(message)) if message.contains("gate")
            ));
            drop(gate);
            ProviderRetryJournal::open(journal_root.clone()).unwrap();
        }

        // (B) Canonical, deduplicated device lock order. Threat: two honest
        // instances locking two devices in opposite orders would deadlock;
        // the order must be canonical regardless of direction, and a
        // same-device source must not acquire its gate twice.
        {
            let alpha_source = ProviderSource::Tree {
                location: ProviderLocation {
                    device: "alpha".into(),
                    tree: ProviderTree::Outbox,
                    path: "objects/source".into(),
                },
            };
            let beta_source = ProviderSource::Tree {
                location: ProviderLocation {
                    device: "beta".into(),
                    tree: ProviderTree::Outbox,
                    path: "objects/source".into(),
                },
            };
            assert_eq!(
                provider_transaction_device_names(&alpha_source, "beta"),
                vec!["alpha".to_owned(), "beta".to_owned()]
            );
            assert_eq!(
                provider_transaction_device_names(&beta_source, "alpha"),
                vec!["alpha".to_owned(), "beta".to_owned()]
            );
            assert_eq!(
                provider_transaction_device_names(&beta_source, "beta"),
                vec!["beta".to_owned()]
            );
            assert_eq!(
                provider_transaction_device_names(
                    &ProviderSource::Mailbox {
                        item_id: "item".into(),
                    },
                    "beta",
                ),
                vec!["beta".to_owned()]
            );
        }

        // (C) Gate precedes source inspection. Threat: an operation that read
        // shared provider state before winning the gate could act on a torn
        // concurrent view. The trap is an ABSENT source: while a competing
        // scope holds the gate the operation reports the gate refusal — never
        // the missing source — and creates no journal record; after release
        // the same call reports the source condition, proving the trap was
        // armed and inspection follows acquisition. A single-device
        // operation then completes end to end with its one gate.
        {
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            let journal_root = root.0.join("provider-local-journal");
            let inbox = root.0.join("provider/inbox");
            let competing = ProviderRetryJournal::open(journal_root.clone()).unwrap();
            let competing_gate = competing.acquire_transaction_gate().unwrap();
            let gated = run_provider_rename(
                &device,
                7,
                ProviderTree::Inbox,
                "objects/source",
                "objects/destination",
            );
            assert!(
                matches!(
                    &gated,
                    Err(ScenarioError::UnsafeProviderJournal(message)) if message.contains("gate")
                ),
                "{gated:?}"
            );
            assert_eq!(retained_dir_count(&journal_root.join("records")), 0);
            drop(competing_gate);
            assert!(matches!(
                run_provider_rename(
                    &device,
                    7,
                    ProviderTree::Inbox,
                    "objects/source",
                    "objects/destination",
                ),
                Err(ScenarioError::UnknownProviderPath(path)) if path == "objects/source"
            ));
            // Same-device single acquisition: one gate, full operation.
            std::fs::write(inbox.join("objects/source"), bytes).unwrap();
            run_provider_rename(
                &device,
                8,
                ProviderTree::Inbox,
                "objects/source",
                "objects/destination",
            )
            .unwrap();
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes
            );
        }

        // (D) Gate precedes retry-journal inspection. Threat: a second scope
        // must not read or resume half-written retry state. With a crashed
        // rename pending, a competing scope's gate makes the exact retry
        // report the gate refusal and leave the journal byte-identical; after
        // release the exact retry converges.
        {
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            let journal_root = root.0.join("provider-local-journal");
            let inbox = root.0.join("provider/inbox");
            std::fs::write(inbox.join("objects/source"), bytes).unwrap();
            // The competing scope opens BEFORE the crash seeds retry state,
            // so its own open reconciles nothing.
            let competing = ProviderRetryJournal::open(journal_root.clone()).unwrap();
            {
                let _fault =
                    install_provider_retry_boundary_fault_for_test(ProviderRetryBoundary::Rename(
                        ProviderRetryFault::AfterDurablePhase(ProviderJournalPhase::Prepared),
                    ));
                assert!(run_provider_rename(
                    &device,
                    7,
                    ProviderTree::Inbox,
                    "objects/source",
                    "objects/destination",
                )
                .is_err());
            }
            let competing_gate = competing.acquire_transaction_gate().unwrap();
            let before = provider_retry_journal_snapshot_for_test(&journal_root);
            let gated = run_provider_rename(
                &device,
                7,
                ProviderTree::Inbox,
                "objects/source",
                "objects/destination",
            );
            assert!(
                matches!(
                    &gated,
                    Err(ScenarioError::UnsafeProviderJournal(message)) if message.contains("gate")
                ),
                "{gated:?}"
            );
            assert_eq!(
                provider_retry_journal_snapshot_for_test(&journal_root),
                before,
                "the gate refusal must not touch pending retry state"
            );
            drop(competing_gate);
            run_provider_rename(
                &device,
                7,
                ProviderTree::Inbox,
                "objects/source",
                "objects/destination",
            )
            .unwrap();
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes
            );
            assert_eq!(retained_dir_count(&journal_root.join("records")), 0);
        }

        // (E) The gate is held for the whole operation, not re-checked per
        // step. Threat: a second honest scope arriving mid-operation
        // (between source validation and publication) must be refused rather
        // than observe partial state. Inside the put's post-validation
        // window, a second-scope open is refused and the pending record
        // count is unchanged; the publication then completes.
        {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            let object_path = format!(
                "{PROVIDER_OBJECTS_NAMESPACE}/{}.object",
                ContentDigest::of(bytes)
            );
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            let hook_journal = journal_root.clone();
            let _race = install_provider_publication_race_for_test(
                ProviderPublicationRace::PutAfterSourceValidation,
                Box::new(move || {
                    let pending_before = std::fs::read_dir(hook_journal.join("records"))
                        .unwrap()
                        .count();
                    assert!(matches!(
                        ProviderRetryJournal::open(hook_journal.clone()),
                        Err(ScenarioError::UnsafeProviderJournal(message))
                            if message.contains("gate")
                    ));
                    assert_eq!(
                        std::fs::read_dir(hook_journal.join("records"))
                            .unwrap()
                            .count(),
                        pending_before
                    );
                }),
            );
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            assert_eq!(
                std::fs::read(provider_root.join("outbox").join(&object_path)).unwrap(),
                bytes
            );
        }
    }

    /// Stage 2e-ii item 17: capability absence (`renameat2` flags
    /// unimplemented, the Android shared-storage shape) selects
    /// byte-equivalent fallbacks for staging quarantine and retirement; a
    /// non-capability errno still fails closed with a named receipt; the
    /// fallback's two crash windows converge; the strict path's
    /// placeholder-quarantine boundaries are unreachable on the fallback;
    /// and a race at the freed source name is preserved. Replaces the
    /// simulator scenarios
    /// `shared_provider_publication_without_rename2_flags_matches_the_flagged_end_state`,
    /// `a_non_capability_errno_from_a_shared_provider_rename_still_fails_closed`,
    /// `shared_provider_retirement_without_rename2_flags_reaches_the_exchange_end_state`,
    /// `shared_provider_retirement_fallback_crash_windows_converge`, and
    /// `shared_provider_retirement_fallback_preserves_a_race_at_the_freed_source_name`
    /// on the retained provider machinery without the simulator.
    #[cfg(unix)]
    #[test]
    fn provider_retirement_fallback_crash_and_freed_name_races_converge() {
        let bytes: &[u8] = b"retirement fallback bytes";
        let object_path = format!(
            "{PROVIDER_OBJECTS_NAMESPACE}/{}.object",
            ContentDigest::of(bytes)
        );

        // (P1) Publication staging-quarantine fallback equality. Threat: a
        // device whose shared storage answers every `renameat2` flag with an
        // errno (Android, CI run 32094662514) still needs the deterministic
        // staging collision — litter from a crashed prior instance or a sync
        // service — quarantined; the fallback must reach the flagged path's
        // canonical tree exactly.
        let publish_with_collision = |errno: Option<i32>| -> BTreeMap<String, Vec<u8>> {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            std::fs::write(
                provider_root
                    .join("outbox")
                    .join(PROVIDER_TEMP_NAMESPACE)
                    .join(ProviderRetryJournal::staging_name(
                        &generated_put_operation_id(&object_path, bytes),
                        0,
                    )),
                b"foreign staging occupant",
            )
            .unwrap();
            let injected = errno.map(InjectedSharedProviderFlaggedRenameFailure::enter);
            transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .unwrap();
            drop(injected);
            provider_tree_bytes(&provider_root.join("outbox"))
        };
        let publication_control = publish_with_collision(None);
        assert_eq!(
            publication_control.get(&object_path).map(Vec::as_slice),
            Some(bytes),
            "the control publication must have published the object: {:?}",
            publication_control.keys().collect::<Vec<_>>()
        );
        assert!(
            publication_control.iter().any(|(path, occupant)| path
                .starts_with(PROVIDER_REMOVED_NAMESPACE)
                && occupant == b"foreign staging occupant"),
            "the control publication must have quarantined the occupant: {:?}",
            publication_control.keys().collect::<Vec<_>>()
        );
        for errno in [libc::EINVAL, libc::ENOSYS, libc::EOPNOTSUPP] {
            assert_eq!(
                publish_with_collision(Some(errno)),
                publication_control,
                "the fallback must reach the flagged path's tree exactly (errno {errno})"
            );
        }

        // (P2) A non-capability errno is not a capability answer. Threat: a
        // real disk I/O error on the flagged rename must fail the operation
        // closed — occupant preserved, nothing published — with a receipt
        // naming the primitive and both names (a bare os-error string on a
        // device receipt costs a CI round trip to localise).
        {
            let root = ScenarioRoot::new().unwrap();
            let provider_root = root.0.join("provider");
            let journal_root = root.0.join("private/device/journal");
            let mut transport =
                SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
            let staging = provider_root
                .join("outbox")
                .join(PROVIDER_TEMP_NAMESPACE)
                .join(ProviderRetryJournal::staging_name(
                    &generated_put_operation_id(&object_path, bytes),
                    0,
                ));
            std::fs::write(&staging, b"foreign staging occupant").unwrap();
            let _injected = InjectedSharedProviderFlaggedRenameFailure::enter(libc::EIO);
            let refusal = transport
                .publish_object_exact(ContentDigest::of(bytes), bytes)
                .expect_err("a real I/O error on the flagged rename must not be tolerated");
            let ScenarioError::Io(detail) = &refusal else {
                panic!("expected a filesystem refusal: {refusal:?}");
            };
            assert!(
                detail.contains(PROVIDER_NOREPLACE_RENAME_PRIMITIVE)
                    && detail.contains("quarantining abandoned shared provider staging")
                    && detail.contains("->"),
                "the refusal must name its operation and both names: {detail}"
            );
            assert_eq!(
                std::fs::read(&staging).unwrap(),
                b"foreign staging occupant"
            );
            assert!(!provider_root.join("outbox").join(&object_path).exists());
        }

        // (R1) Retirement fallback end-state equality. Threat: capability
        // absence on the retirement exchange selects the single-rename
        // fallback, which must leave exactly the exchange path's tree —
        // destination published, retired original as the diagnostic copy.
        let retire_one = |errno: Option<i32>| -> BTreeMap<String, Vec<u8>> {
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            std::fs::write(root.0.join("provider/inbox/objects/source"), bytes).unwrap();
            let injected = errno.map(InjectedSharedProviderFlaggedRenameFailure::enter);
            run_provider_rename(
                &device,
                7,
                ProviderTree::Inbox,
                "objects/source",
                "objects/destination",
            )
            .unwrap();
            drop(injected);
            provider_tree_bytes(&root.0.join("provider/inbox"))
        };
        let retirement_control = retire_one(None);
        assert_eq!(
            retirement_control
                .get("objects/destination")
                .map(Vec::as_slice),
            Some(bytes),
            "the control retirement must have published the destination: {:?}",
            retirement_control.keys().collect::<Vec<_>>()
        );
        for errno in [libc::EINVAL, libc::EOPNOTSUPP] {
            assert_eq!(
                retire_one(Some(errno)),
                retirement_control,
                "the exchange fallback must reach the exchange's tree exactly (errno {errno})"
            );
        }

        // (R2) The fallback's two crash windows converge. Threat:
        // crash/power cut before the single rename (placeholder durable) and
        // after it (exchange durable); recovery from disk alone plus the
        // exact retry reaches the converged retirement tree.
        for boundary in [
            ProviderJournalBoundary::RetirementPlaceholderDurable,
            ProviderJournalBoundary::RetirementExchangeDurable,
        ] {
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            std::fs::write(inbox.join("objects/source"), bytes).unwrap();
            let injected = InjectedSharedProviderFlaggedRenameFailure::enter(libc::EINVAL);
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Rename(ProviderRetryFault::AtJournalBoundary(boundary)),
                );
                assert!(
                    matches!(
                        run_provider_rename(
                            &device,
                            7,
                            ProviderTree::Inbox,
                            "objects/source",
                            "objects/destination",
                        ),
                        Err(ScenarioError::Io(message)) if message.contains("injected")
                    ),
                    "{boundary:?} must be reachable on the fallback path"
                );
            }
            drop(device);
            let device = retained_provider_device(&root.0);
            run_provider_rename(
                &device,
                7,
                ProviderTree::Inbox,
                "objects/source",
                "objects/destination",
            )
            .unwrap();
            drop(injected);
            assert_eq!(
                provider_tree_bytes(&inbox),
                retirement_control,
                "the fallback must converge from a crash at {boundary:?}"
            );
        }

        // (R3) The strict path's placeholder-quarantine boundaries are
        // unreachable on the fallback — the single rename consumed the
        // placeholder, so there is nothing to quarantine. If the fallback
        // ever reached them this would fail here instead of silently proving
        // nothing.
        for boundary in [
            ProviderJournalBoundary::RetirementPlaceholderQuarantined,
            ProviderJournalBoundary::RetirementPlaceholderPrivateDeleted,
        ] {
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            std::fs::write(inbox.join("objects/source"), bytes).unwrap();
            let injected = InjectedSharedProviderFlaggedRenameFailure::enter(libc::EINVAL);
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Rename(ProviderRetryFault::AtJournalBoundary(boundary)),
                );
                run_provider_rename(
                    &device,
                    7,
                    ProviderTree::Inbox,
                    "objects/source",
                    "objects/destination",
                )
                .unwrap_or_else(|error| {
                    panic!("{boundary:?} must be unreachable on the fallback path: {error:?}")
                });
            }
            drop(injected);
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes,
                "{boundary:?}"
            );
        }

        // (R4) What the fallback gives up: after its single rename the source
        // name is FREE rather than holding a known placeholder. Threat: an
        // honest concurrent instance or a sync-service delivery re-creates
        // the freed name across the crash window. Recovery keys on the
        // diagnostic name holding the recorded original, treats the newcomer
        // as a racing replacement, preserves its bytes, and refuses — it is
        // never published and never destroyed.
        {
            let foreign = b"a peer delivered these bytes after the retirement";
            let root = ScenarioRoot::new().unwrap();
            let device = retained_provider_device(&root.0);
            let inbox = root.0.join("provider/inbox");
            std::fs::write(inbox.join("objects/source"), bytes).unwrap();
            let _injected = InjectedSharedProviderFlaggedRenameFailure::enter(libc::EINVAL);
            {
                let _fault = install_provider_retry_boundary_fault_for_test(
                    ProviderRetryBoundary::Rename(ProviderRetryFault::AtJournalBoundary(
                        ProviderJournalBoundary::RetirementExchangeDurable,
                    )),
                );
                assert!(run_provider_rename(
                    &device,
                    7,
                    ProviderTree::Inbox,
                    "objects/source",
                    "objects/destination",
                )
                .is_err());
            }
            drop(device);
            let device = retained_provider_device(&root.0);
            assert!(
                !inbox.join("objects/source").exists(),
                "the fallback leaves the source name free, which is exactly the exposure"
            );
            std::fs::write(inbox.join("objects/source"), foreign).unwrap();
            let raced = run_provider_rename(
                &device,
                7,
                ProviderTree::Inbox,
                "objects/source",
                "objects/destination",
            );
            assert!(
                matches!(raced, Err(ScenarioError::UnsafeProviderEntry(_))),
                "{raced:?}"
            );
            assert!(!inbox.join("objects/source").exists());
            assert_eq!(
                std::fs::read(inbox.join("objects/destination")).unwrap(),
                bytes
            );
            let retained: Vec<_> =
                std::fs::read_dir(inbox.join(PROVIDER_RENAME_EVIDENCE_NAMESPACE))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
            assert_eq!(retained.len(), 1, "{retained:?}");
            assert_eq!(std::fs::read(&retained[0]).unwrap(), foreign);
            let removed: Vec<_> = std::fs::read_dir(inbox.join(PROVIDER_REMOVED_NAMESPACE))
                .unwrap()
                .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
                .collect();
            assert_eq!(
                removed,
                vec![bytes.to_vec()],
                "the retired original must still be the only diagnostic copy"
            );
        }
    }

    /// Every file the shared provider tree holds, keyed by its path relative to
    /// the tree root. The fallback must reach the SAME state the flagged
    /// primitives reach, so the comparison is against a control run rather than
    /// a remembered shape.
    fn provider_tree_bytes(root: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let mut entries = std::fs::read_dir(&directory)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if entry.file_type().unwrap().is_dir() {
                    files.insert(format!("{relative}/"), Vec::new());
                    pending.push(entry.path());
                } else {
                    files.insert(relative, std::fs::read(entry.path()).unwrap());
                }
            }
        }
        files
    }
    /// A provider tree that exists but is still being filled is the ORDINARY
    /// state of a folder a file-sync tool is delivering: Syncthing, Dropbox and
    /// friends create entries in arbitrary order and may hold a directory back
    /// for minutes. Every one of these shapes must read as "no sync data here
    /// yet", never as a hostile tree, because refusing them blocks recovery on
    /// the device that is merely early (GH: desktop pairing, 2026-08-18).
    #[test]
    fn a_partly_delivered_provider_tree_discovers_as_nothing_yet() {
        let root = ScenarioRoot::new().unwrap();
        let provider_root = root.0.join("provider");
        let journal_root = root.0.join("private/device/journal");
        let mut provider = SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
        provider.publish_descriptor(b"descriptor").unwrap();
        drop(provider);

        assert_eq!(
            inspect_shared_provider_descriptor(&provider_root).unwrap(),
            Some(b"descriptor".to_vec()),
            "a complete tree still discovers its descriptor"
        );

        // The descriptor file has not arrived yet.
        fs::remove_file(provider_root.join("outbox/enrollment/shared-enrollment-v1.json")).unwrap();
        assert_eq!(
            inspect_shared_provider_descriptor(&provider_root).unwrap(),
            None,
            "an undelivered descriptor file is nothing yet"
        );

        // The enrollment namespace has not arrived yet.
        fs::remove_dir_all(provider_root.join("outbox/enrollment")).unwrap();
        assert_eq!(
            inspect_shared_provider_descriptor(&provider_root).unwrap(),
            None,
            "an undelivered enrollment namespace is nothing yet, not an unsafe entry"
        );

        // The whole outbox tree has not arrived yet.
        fs::remove_dir_all(provider_root.join("outbox")).unwrap();
        assert_eq!(
            inspect_shared_provider_descriptor(&provider_root).unwrap(),
            None,
            "an undelivered outbox tree is nothing yet, not an unsafe entry"
        );

        // The provider root itself has not arrived yet.
        assert_eq!(
            inspect_shared_provider_descriptor(&root.0.join("never-delivered")).unwrap(),
            None,
        );
        assert_eq!(
            inspect_cold_shared_provider_prefix(&root.0.join("never-delivered")).unwrap(),
            ColdSharedProviderPrefix::Partial,
            "an absent provider root is nothing yet, not a refusal"
        );
    }

    /// The narrow set `UnsafeProviderEntry` was written for. Relaxing "absent"
    /// must not relax any of these.
    #[test]
    fn a_genuinely_unsafe_provider_entry_still_refuses_cold_discovery() {
        let root = ScenarioRoot::new().unwrap();
        let provider_root = root.0.join("provider");
        let journal_root = root.0.join("private/device/journal");
        let mut provider = SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
        provider.publish_descriptor(b"descriptor").unwrap();
        drop(provider);

        // A regular file where the enrollment namespace must be a directory.
        fs::remove_dir_all(provider_root.join("outbox/enrollment")).unwrap();
        fs::write(provider_root.join("outbox/enrollment"), b"not a directory").unwrap();
        assert!(matches!(
            inspect_shared_provider_descriptor(&provider_root),
            Err(ScenarioError::UnsafeProviderEntry(_))
        ));
        assert_eq!(
            inspect_cold_shared_provider_prefix(&provider_root).unwrap(),
            ColdSharedProviderPrefix::Refused
        );
        fs::remove_file(provider_root.join("outbox/enrollment")).unwrap();

        // A regular file where the outbox tree must be a directory.
        fs::remove_dir_all(provider_root.join("outbox")).unwrap();
        fs::write(provider_root.join("outbox"), b"not a directory").unwrap();
        assert!(matches!(
            inspect_shared_provider_descriptor(&provider_root),
            Err(ScenarioError::UnsafeProviderEntry(_))
        ));
        assert_eq!(
            inspect_cold_shared_provider_prefix(&provider_root).unwrap(),
            ColdSharedProviderPrefix::Refused
        );
        fs::remove_file(provider_root.join("outbox")).unwrap();

        // A regular file where the provider root must be a directory.
        let file_root = root.0.join("file-root");
        fs::write(&file_root, b"not a directory").unwrap();
        assert!(matches!(
            inspect_shared_provider_descriptor(&file_root),
            Err(ScenarioError::UnsafeProviderEntry(_))
        ));

        #[cfg(unix)]
        {
            // A symlink standing in for the enrollment namespace, pointing at a
            // real directory: the kind check, not the open, must refuse it.
            let elsewhere = root.0.join("elsewhere");
            fs::create_dir(&elsewhere).unwrap();
            fs::create_dir_all(provider_root.join("outbox")).unwrap();
            std::os::unix::fs::symlink(&elsewhere, provider_root.join("outbox/enrollment"))
                .unwrap();
            assert!(matches!(
                inspect_shared_provider_descriptor(&provider_root),
                Err(ScenarioError::UnsafeProviderEntry(_))
            ));
            assert_eq!(
                inspect_cold_shared_provider_prefix(&provider_root).unwrap(),
                ColdSharedProviderPrefix::Refused
            );
        }
    }

    /// The runtime ingress lane sees the same half-delivered tree the cold
    /// discovery path does. An undelivered namespace made every provider scan
    /// raise `UnsafeProviderEntry`, and the actor turned that into a
    /// `RecoveryBlocked` tick on every retry — the repeating desktop toast.
    #[test]
    fn an_undelivered_namespace_does_not_block_the_provider_scan() {
        let root = ScenarioRoot::new().unwrap();
        let provider_root = root.0.join("provider");
        let journal_root = root.0.join("private/device/journal");
        let mut provider = SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
        provider.publish_descriptor(b"descriptor").unwrap();

        let complete = drain_observed_paths(&provider);
        assert_eq!(complete, vec![SHARED_ENROLLMENT_DESCRIPTOR_PATH.to_owned()]);

        for namespace in [
            SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE,
            PROVIDER_MANIFESTS_NAMESPACE,
            PROVIDER_OBJECTS_NAMESPACE,
        ] {
            fs::remove_dir_all(provider_root.join("outbox").join(namespace)).unwrap();
        }
        assert_eq!(
            drain_observed_paths(&provider),
            vec![SHARED_ENROLLMENT_DESCRIPTOR_PATH.to_owned()],
            "namespaces a file sync has not delivered are simply empty"
        );

        fs::remove_dir_all(provider_root.join("outbox/enrollment")).unwrap();
        assert_eq!(
            drain_observed_paths(&provider),
            Vec::<String>::new(),
            "an undelivered enrollment namespace must not block the scan"
        );

        // The same tree read through the runtime's exact-read surface. This is
        // the one the frontier-head check uses on every managed-local
        // publication, so a refusal here is a `RecoveryBlocked` tick per retry.
        assert_eq!(
            provider
                .read_exact(SHARED_ENROLLMENT_DESCRIPTOR_PATH)
                .unwrap(),
            None
        );
        assert_eq!(
            provider
                .read_exact(&format!(
                    "{SHARED_PROVIDER_FRONTIER_HEADS_NAMESPACE}/never-delivered.head"
                ))
                .unwrap(),
            None
        );

        // Still refused when the namespace is present as something other than
        // a real directory.
        fs::write(provider_root.join("outbox/enrollment"), b"not a directory").unwrap();
        assert!(matches!(
            provider.read_exact(SHARED_ENROLLMENT_DESCRIPTOR_PATH),
            Err(ScenarioError::UnsafeProviderEntry(_))
        ));
    }

    /// Opening a provider transport is the FIRST thing share preparation does,
    /// before it publishes a single byte. So the tree it leaves is either
    /// complete or absent: a preparation that dies at any later step leaves a
    /// tree with no descriptor in it, which discovers as "nothing to join yet",
    /// never as a half-built tree another device could act on.
    #[test]
    fn opening_a_provider_publishes_the_whole_tree_before_any_bytes() {
        let root = ScenarioRoot::new().unwrap();
        let provider_root = root.0.join("provider");
        let journal_root = root.0.join("private/device/journal");
        let provider = SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
        drop(provider);

        for tree in ["inbox", "outbox"] {
            let mut present = fs::read_dir(provider_root.join(tree))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            present.sort();
            let mut expected = SHARED_PROVIDER_TREE_NAMESPACES.to_vec();
            expected.sort_unstable();
            assert_eq!(present, expected, "{tree} is not the canonical inventory");
        }
        assert_eq!(
            inspect_shared_provider_descriptor(&provider_root).unwrap(),
            None,
            "a prepared-but-unpublished tree is nothing to join yet"
        );
        assert_eq!(
            inspect_cold_shared_provider_prefix(&provider_root).unwrap(),
            ColdSharedProviderPrefix::Partial
        );
    }

    /// A file-sync client writes its own litter into the directories it is
    /// delivering. Syncthing leaves `.syncthing.<name>.tmp` and
    /// `<name>.sync-conflict-<date>-<device>` copies, Dropbox leaves
    /// `<name> (conflicted copy …)`, Seafile leaves `<name> (SFConflict …)`.
    /// None of them are on a path the provider scan reads, so none of them may
    /// stop it — and the rule is about WHAT IS READ, not about which tool wrote
    /// the litter, so no tool is named in the check.
    #[test]
    fn provider_litter_beside_the_namespaces_does_not_stop_the_scan() {
        let root = ScenarioRoot::new().unwrap();
        let provider_root = root.0.join("provider");
        let journal_root = root.0.join("private/device/journal");
        let mut provider = SharedProviderTransport::open(&provider_root, &journal_root).unwrap();
        provider.publish_descriptor(b"descriptor").unwrap();

        let outbox = provider_root.join("outbox");
        for stray in [
            ".syncthing.enrollment.tmp",
            "enrollment.sync-conflict-20260705-141233-A2B3C4D",
            "objects (conflicted copy 2026-08-18).txt",
            "notes (SFConflict martin 2026-08-18-15-04-05).md",
            ".stfolder",
            ".DS_Store",
            "namespace-from-a-newer-tine-v9",
        ] {
            fs::write(outbox.join(stray), b"litter").unwrap();
        }
        // A directory-shaped stray, and one whose name this build cannot spell.
        fs::create_dir(outbox.join("~syncthing~staging")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let non_utf8 = std::ffi::OsStr::from_bytes(b"stray-\xff-name");
            fs::write(outbox.join(non_utf8), b"litter").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.0.join("elsewhere"), outbox.join("dangling-stray"))
            .unwrap();

        assert_eq!(
            drain_observed_paths(&provider),
            vec![SHARED_ENROLLMENT_DESCRIPTOR_PATH.to_owned()],
            "the scan reads the canonical namespaces and ignores everything beside them"
        );

        // A CANONICAL namespace that is not a real directory is the shape this
        // sweep exists for, and it still refuses.
        fs::remove_dir_all(outbox.join(PROVIDER_REMOVED_NAMESPACE)).unwrap();
        fs::write(outbox.join(PROVIDER_REMOVED_NAMESPACE), b"not a directory").unwrap();
        assert!(matches!(
            drain_observed_paths_result(&provider),
            Err(ScenarioError::UnsafeProviderEntry(detail))
                if detail.contains(PROVIDER_REMOVED_NAMESPACE)
                    && detail.contains("real no-follow directory")
        ));
        fs::remove_file(outbox.join(PROVIDER_REMOVED_NAMESPACE)).unwrap();

        #[cfg(unix)]
        {
            let elsewhere = root.0.join("elsewhere-namespace");
            fs::create_dir(&elsewhere).unwrap();
            fs::remove_dir_all(outbox.join(PROVIDER_TEMP_NAMESPACE)).unwrap();
            std::os::unix::fs::symlink(&elsewhere, outbox.join(PROVIDER_TEMP_NAMESPACE)).unwrap();
            assert!(matches!(
                drain_observed_paths_result(&provider),
                Err(ScenarioError::UnsafeProviderEntry(_))
            ));
        }
    }

    fn drain_observed_paths(provider: &SharedProviderTransport) -> Vec<String> {
        drain_observed_paths_result(provider).unwrap()
    }

    fn drain_observed_paths_result(
        provider: &SharedProviderTransport,
    ) -> Result<Vec<String>, ScenarioError> {
        let mut cursor = provider.full_observation_cursor()?;
        let mut paths = Vec::new();
        loop {
            match provider.next_observed_path(&mut cursor)? {
                SharedProviderObservation::Path(path) => paths.push(path),
                SharedProviderObservation::ChunkBoundary => cursor.begin_next_chunk(),
                SharedProviderObservation::Complete => break,
            }
        }
        paths.sort();
        Ok(paths)
    }
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        encoded.push(TABLE[((value >> 18) & 0x3f) as usize] as char);
        encoded.push(TABLE[((value >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[((value >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(value & 0x3f) as usize] as char);
        }
    }
    encoded
}

fn base64url_decode(value: &str) -> Result<Vec<u8>, String> {
    if value.len() % 4 == 1 {
        return Err("invalid base64url length".into());
    }
    let decode = |byte: u8| -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    };
    let input = value.as_bytes();
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    for chunk in input.chunks(4) {
        let a = decode(chunk[0]).ok_or_else(|| "invalid base64url character".to_string())?;
        let b = decode(
            *chunk
                .get(1)
                .ok_or_else(|| "invalid base64url length".to_string())?,
        )
        .ok_or_else(|| "invalid base64url character".to_string())?;
        let c = chunk
            .get(2)
            .map(|byte| decode(*byte).ok_or_else(|| "invalid base64url character".to_string()))
            .transpose()?;
        let d = chunk
            .get(3)
            .map(|byte| decode(*byte).ok_or_else(|| "invalid base64url character".to_string()))
            .transpose()?;
        output.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            output.push((b << 4) | (c >> 2));
            if let Some(d) = d {
                output.push((c << 6) | d);
            }
        }
    }
    Ok(output)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScenarioError {
    Decode(String),
    Encode(String),
    TooLarge(usize),
    InvalidProviderPath(String),
    UnknownProviderPath(String),
    UnsafeProviderEntry(String),
    ProviderConflictingBytes(String),
    ProviderRescanLimit,
    ProviderJournalLimit,
    UnsafeProviderJournal(String),
    Io(String),
    NonCanonical,
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "scenario decode failed: {error}"),
            Self::Encode(error) => write!(f, "scenario encode failed: {error}"),
            Self::TooLarge(bytes) => write!(f, "scenario is too large: {bytes} bytes"),
            Self::InvalidProviderPath(path) => write!(f, "provider path is invalid: {path}"),
            Self::UnknownProviderPath(path) => write!(f, "unknown provider path: {path}"),
            Self::UnsafeProviderEntry(path) => write!(f, "unsafe provider entry: {path}"),
            Self::ProviderConflictingBytes(path) => {
                write!(f, "conflicting provider bytes at {path}")
            }
            Self::ProviderRescanLimit => f.write_str("provider rescan exceeded explicit bound"),
            Self::ProviderJournalLimit => {
                f.write_str("provider retry journal exceeded explicit bound")
            }
            Self::UnsafeProviderJournal(entry) => {
                write!(f, "unsafe provider retry journal: {entry}")
            }
            Self::Io(error) => write!(f, "scenario filesystem operation failed: {error}"),
            Self::NonCanonical => f.write_str("scenario bytes are not canonical"),
        }
    }
}

impl std::error::Error for ScenarioError {}
