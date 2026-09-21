//! Shared document vocabulary used by the document and query layers.
use crate::config::FileNameFormat;
use crate::doc::{self, DocBlock, Document};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

mod block_dto;
mod dto;

pub use block_dto::*;
pub use dto::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageKind {
    Journal,
    Page,
}

/// On-disk file format of a page. Markdown (`.md`/`.markdown`) is the default; Logseq org
/// graphs use `.org`. A graph may mix the two — format is decided per file by
/// extension, never graph-wide (matching OG, which stores `:block/format` per
/// page). The graph's `:preferred-format` only chooses the extension for NEW
/// files (see `Graph::preferred_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Md,
    Org,
}

impl Format {
    /// Format of a page file by its extension (`.org` → Org, else Md).
    pub fn from_path(p: &Path) -> Format {
        match p.extension().and_then(|e| e.to_str()) {
            Some(extension) if extension.eq_ignore_ascii_case("org") => Format::Org,
            _ => Format::Md,
        }
    }
    /// File extension (no dot) for this format.
    pub fn ext(self) -> &'static str {
        match self {
            Format::Md => "md",
            Format::Org => "org",
        }
    }
}

/// If `stem` is a sync tool's conflict copy of another file, return the base file
/// stem it shadows. Recognises the GENERATED shapes only (a page whose name
/// merely resembles one stays a real page):
///
/// - Syncthing: `name.sync-conflict-YYYYMMDD-HHMMSS-DEVICEID`
///   (`conflictName` in syncthing `lib/model/folder_sendrecv.go`; the device id
///   is the modifying device's short id — up to 7 base32 chars `[A-Z2-7]`,
///   empty when unknown — and pre-1.1.0 versions omitted `-DEVICEID`).
/// - Seafile: `name (SFConflict [modifier ]YYYY-MM-DD-HH-MM-SS)`
///   (`gen_conflict_path` in seafile `common/vc-common.c`; the modifier is the
///   editing user's id when known).
/// - Dropbox: `name (conflicted copy …)` / `name (<user>'s conflicted copy …)`.
///
/// Deliberately NOT recognized (too ambiguous to distinguish from a real page
/// name, so treating them as conflict copies would deindex real pages):
/// OneDrive's `name-COMPUTERNAME.ext` and Google Drive's `name (1).ext`.
///
/// A conflict copy is NOT a real page — the versioned graph-text policy keeps it
/// out of normal discovery and exact page resolution. The explicit conflict
/// workflow has its own retained-capability path.
pub fn sync_conflict_base(stem: &str) -> Option<&str> {
    const SYNCTHING_TAG: &str = ".sync-conflict-";
    let mut search = 0;
    while let Some(found) = stem[search..].find(SYNCTHING_TAG) {
        let i = search + found;
        if syncthing_conflict_tail(&stem[i + SYNCTHING_TAG.len()..]) {
            return Some(&stem[..i]);
        }
        search = i + SYNCTHING_TAG.len();
    }
    const SEAFILE_TAG: &str = " (SFConflict ";
    if let Some(inner) = stem.strip_suffix(')') {
        if let Some(i) = inner.rfind(SEAFILE_TAG) {
            let args = &inner[i + SEAFILE_TAG.len()..];
            let timestamp = args.rsplit(' ').next().unwrap_or(args);
            if seafile_conflict_timestamp(timestamp) && !args.contains(')') {
                return Some(&stem[..i]);
            }
        }
    }
    // Dropbox: "<base> (conflicted copy …)" or "<base> (<user>'s conflicted copy …)".
    if let Some(i) = stem.find(" (") {
        if stem[i..].contains("conflicted copy") {
            return Some(&stem[..i]);
        }
    }
    None
}

/// Whether the text after `.sync-conflict-` matches Syncthing's generated
/// `YYYYMMDD-HHMMSS[-DEVICEID]` tail exactly to the end of the stem.
fn syncthing_conflict_tail(tail: &str) -> bool {
    let bytes = tail.as_bytes();
    if bytes.len() < 15
        || !bytes[..8].iter().all(u8::is_ascii_digit)
        || bytes[8] != b'-'
        || !bytes[9..15].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    match &bytes[15..] {
        // Pre-1.1.0 Syncthing: no `-DEVICEID` suffix at all.
        [] => true,
        // The short device id: up to 7 chars of RFC 4648 base32 (`[A-Z2-7]`),
        // empty when the modifying device is unknown (zero ShortID).
        [b'-', device @ ..] => {
            device.len() <= 7
                && device
                    .iter()
                    .all(|&b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
        }
        _ => false,
    }
}

/// Whether `text` is Seafile's `%Y-%m-%d-%H-%M-%S` conflict timestamp
/// (`gen_conflict_path` in seafile `common/vc-common.c`).
fn seafile_conflict_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 19
        && bytes.iter().enumerate().all(|(i, &b)| {
            if matches!(i, 4 | 7 | 10 | 13 | 16) {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// Whether `stem` names a sync-tool conflict copy (see [`sync_conflict_base`]).
pub fn is_sync_conflict(stem: &str) -> bool {
    sync_conflict_base(stem).is_some()
}

/// Whether `path`'s file stem names a sync-tool conflict copy — the `Path`-level
/// convenience used by the watcher (which works in paths, not stems).
pub fn path_is_sync_conflict(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(is_sync_conflict)
}

pub const PUBLISHED_QUERIES_DIR: &str = "published-queries";

/// The comparison form of an atom's text: NFC-lowercased and trimmed.
pub fn atom_key(text: &str) -> String {
    text.trim().to_lowercase().nfc().collect()
}

pub(crate) const MAX_BLOCK_DEPTH: usize = 128;

pub fn block_dto_estimated_bytes(block: &BlockDto) -> usize {
    block.id.len()
        + block.raw.len()
        + block.breadcrumb.iter().map(String::len).sum::<usize>()
        + block.tags.iter().map(String::len).sum::<usize>()
        + block
            .properties
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
        + block
            .children
            .iter()
            .map(block_dto_estimated_bytes)
            .sum::<usize>()
        + 128
}

pub(crate) struct ReferenceCandidatePages {
    pub pages: Vec<(PageEntry, Arc<Document>)>,
    /// The referring blocks, when the index named them. `None` means "classify
    /// every block of every candidate page", which is what every caller did
    /// before this field existed, so the walk is the behaviour a partial or
    /// absent index falls back to rather than a lossy shortcut.
    pub blocks: Option<std::collections::HashSet<String>>,
    /// Interactive page entities that survived the verified window. `None`
    /// means page-preamble admission is unrestricted (Exhaustive or fallback).
    pub page_owners: Option<std::collections::HashSet<std::path::PathBuf>>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub indexed: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub full_page_count: usize,
}

pub(crate) fn allocation_overflow() -> io::Error {
    graph_text_inventory_limit_error("aggregate retained content bytes")
}

pub(crate) fn usize_to_u64(value: usize) -> io::Result<u64> {
    u64::try_from(value).map_err(|_| allocation_overflow())
}

pub(crate) fn graph_text_inventory_limit_error(resource: &'static str) -> io::Error {
    DirectSaveError::into_io(
        DirectSaveFailureCode::PrecheckLimit,
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("graph text inventory {resource} bound exceeded"),
        ),
    )
}

/// Closed producer vocabulary for failures returned by a Direct Files save.
/// The strings are the stable diagnostic/retry contract; user-controlled error
/// prose is display-only and never participates in classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectSaveFailureCode {
    PrecheckSymlink,
    PrecheckInterrupted,
    PrecheckPortableCollision,
    PrecheckResourceAlias,
    PrecheckNotPortable,
    PrecheckNofollow,
    PrecheckLimit,
    IdentityOwnedElsewhere,
    IdentityNameTaken,
    ConflictRetrySaveBaselinePresent,
    ConflictRetrySaveBaselineAbsent,
    ConflictRetryCommitRecheck,
    ConflictRetryReplacePreRetirement,
    ConflictRetryReplaceRetiredMismatch,
    ConflictRetryReplacePublicationCollision,
    ConflictRetryCreatePublicationCollision,
    ConflictRetryFinalRereadAbsent,
    ConflictRetryFinalRereadPresent,
    ConflictRetryReplacePostPublication,
    ConflictAuthoritySuperseded,
    ConflictAuthorityOtherEpisode,
    ConflictAuthoritySpent,
    ConflictSaveBaselinePresent,
    ConflictSaveBaselineAbsent,
    ConflictCommitRecheck,
    ConflictReplacePreRetirement,
    ConflictReplaceRetiredMismatch,
    ConflictReplacePublicationCollision,
    ConflictCreatePublicationCollision,
    ConflictFinalRereadAbsent,
    ConflictFinalRereadPresent,
    ConflictReplacePostPublication,
    ConflictPinnedOwner,
    ConflictBaseRev,
    RefusedDataPreservation,
    Unknown,
}

impl DirectSaveFailureCode {
    pub const ALL: [Self; 36] = [
        Self::PrecheckSymlink,
        Self::PrecheckInterrupted,
        Self::PrecheckPortableCollision,
        Self::PrecheckResourceAlias,
        Self::PrecheckNotPortable,
        Self::PrecheckNofollow,
        Self::PrecheckLimit,
        Self::IdentityOwnedElsewhere,
        Self::IdentityNameTaken,
        Self::ConflictRetrySaveBaselinePresent,
        Self::ConflictRetrySaveBaselineAbsent,
        Self::ConflictRetryCommitRecheck,
        Self::ConflictRetryReplacePreRetirement,
        Self::ConflictRetryReplaceRetiredMismatch,
        Self::ConflictRetryReplacePublicationCollision,
        Self::ConflictRetryCreatePublicationCollision,
        Self::ConflictRetryFinalRereadAbsent,
        Self::ConflictRetryFinalRereadPresent,
        Self::ConflictRetryReplacePostPublication,
        Self::ConflictAuthoritySuperseded,
        Self::ConflictAuthorityOtherEpisode,
        Self::ConflictAuthoritySpent,
        Self::ConflictSaveBaselinePresent,
        Self::ConflictSaveBaselineAbsent,
        Self::ConflictCommitRecheck,
        Self::ConflictReplacePreRetirement,
        Self::ConflictReplaceRetiredMismatch,
        Self::ConflictReplacePublicationCollision,
        Self::ConflictCreatePublicationCollision,
        Self::ConflictFinalRereadAbsent,
        Self::ConflictFinalRereadPresent,
        Self::ConflictReplacePostPublication,
        Self::ConflictPinnedOwner,
        Self::ConflictBaseRev,
        Self::RefusedDataPreservation,
        Self::Unknown,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrecheckSymlink => "precheck.symlink",
            Self::PrecheckInterrupted => "precheck.interrupted",
            Self::PrecheckPortableCollision => "precheck.portable_collision",
            Self::PrecheckResourceAlias => "precheck.resource_alias",
            Self::PrecheckNotPortable => "precheck.not_portable",
            Self::PrecheckNofollow => "precheck.nofollow",
            Self::PrecheckLimit => "precheck.limit",
            Self::IdentityOwnedElsewhere => "identity.owned_elsewhere",
            Self::IdentityNameTaken => "identity.name_taken",
            Self::ConflictRetrySaveBaselinePresent => "conflict_retry.save_baseline_present",
            Self::ConflictRetrySaveBaselineAbsent => "conflict_retry.save_baseline_absent",
            Self::ConflictRetryCommitRecheck => "conflict_retry.commit_recheck",
            Self::ConflictRetryReplacePreRetirement => "conflict_retry.replace_pre_retirement",
            Self::ConflictRetryReplaceRetiredMismatch => "conflict_retry.replace_retired_mismatch",
            Self::ConflictRetryReplacePublicationCollision => {
                "conflict_retry.replace_publication_collision"
            }
            Self::ConflictRetryCreatePublicationCollision => {
                "conflict_retry.create_publication_collision"
            }
            Self::ConflictRetryFinalRereadAbsent => "conflict_retry.final_reread_absent",
            Self::ConflictRetryFinalRereadPresent => "conflict_retry.final_reread_present",
            Self::ConflictRetryReplacePostPublication => "conflict_retry.replace_post_publication",
            Self::ConflictAuthoritySuperseded => "conflict_authority.superseded",
            Self::ConflictAuthorityOtherEpisode => "conflict_authority.other_episode",
            Self::ConflictAuthoritySpent => "conflict_authority.spent",
            Self::ConflictSaveBaselinePresent => "conflict.save_baseline_present",
            Self::ConflictSaveBaselineAbsent => "conflict.save_baseline_absent",
            Self::ConflictCommitRecheck => "conflict.commit_recheck",
            Self::ConflictReplacePreRetirement => "conflict.replace_pre_retirement",
            Self::ConflictReplaceRetiredMismatch => "conflict.replace_retired_mismatch",
            Self::ConflictReplacePublicationCollision => "conflict.replace_publication_collision",
            Self::ConflictCreatePublicationCollision => "conflict.create_publication_collision",
            Self::ConflictFinalRereadAbsent => "conflict.final_reread_absent",
            Self::ConflictFinalRereadPresent => "conflict.final_reread_present",
            Self::ConflictReplacePostPublication => "conflict.replace_post_publication",
            Self::ConflictPinnedOwner => "conflict.pinned_owner",
            Self::ConflictBaseRev => "conflict.base_rev",
            Self::RefusedDataPreservation => "refused.data_preservation",
            Self::Unknown => "unknown",
        }
    }
}

/// Typed inner error retained inside the public `io::Error` save surface.
#[derive(Debug)]
pub struct DirectSaveError {
    pub(crate) code: DirectSaveFailureCode,
    pub(crate) conflict_epoch: Option<u64>,
    pub(crate) source: io::Error,
}

impl DirectSaveError {
    pub fn into_io(code: DirectSaveFailureCode, source: io::Error) -> io::Error {
        Self::into_io_with_conflict_epoch(code, None, source)
    }

    pub fn into_io_with_conflict_epoch(
        code: DirectSaveFailureCode,
        conflict_epoch: Option<u64>,
        source: io::Error,
    ) -> io::Error {
        let kind = source.kind();
        io::Error::new(
            kind,
            Self {
                code,
                conflict_epoch,
                source,
            },
        )
    }

    pub fn ensure_io(source: io::Error) -> io::Error {
        if source
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<Self>())
            .is_some()
        {
            source
        } else {
            Self::into_io(DirectSaveFailureCode::Unknown, source)
        }
    }

    pub const fn code(&self) -> DirectSaveFailureCode {
        self.code
    }

    pub const fn conflict_epoch(&self) -> Option<u64> {
        self.conflict_epoch
    }
}

impl fmt::Display for DirectSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for DirectSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}
