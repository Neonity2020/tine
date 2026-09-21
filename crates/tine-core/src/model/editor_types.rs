//! Types shared by editor activation and the save path: activation records
//! and handles, conflict presentation, episodes, overrides and authority,
//! DirectSaveFailureCode / DirectSaveError, and the editor-conflict sites.

use super::*;

/// Identity for one LIVE editor instance over a path.
///
/// Opaque to the frontend: it is minted by [`Graph::activate_editor`], carried
/// across the save transport, and compared for exact equality. It is deliberately
/// NOT derived from the page's content, revision or path, because the defect this
/// exists to close is precisely two different editors that agree on all three — a
/// cloned `PageDto` with the same `base_rev` spending the live editor's epoch.
///
/// Values are unique within one `Graph`. The registry lives on the `Graph`, so a
/// token minted against a different graph is simply not found: the graph binding
/// the spec requires is structural rather than a compared field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EditorActivation(pub(super) u64);

impl EditorActivation {
    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn from_u64(raw: u64) -> Self {
        Self(raw)
    }
}

/// What an activation request means for a path that already has a live editor.
///
/// Path idempotence and same-path content replacement contradict each other
/// without this discriminator, which is why v5 of the spec was unimplementable at
/// the same-path row.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationIntent {
    /// Plain re-hydration: return the live activation and mint nothing. Does not
    /// burn the incumbent's authority.
    Reuse,
    /// The working instance is genuinely being replaced (`reloadPage`,
    /// `reloadPageIfStillSafe`, PDF-notes refresh). Mints a new activation; the
    /// frontend retires the exact incumbent only after installing the replacement.
    Replace,
}

/// What presenting a conflict observation established. No arm writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictPresentation {
    /// This editor's observation was live and is now spent: the discard proceeds.
    Authorised,
    /// A newer observation exists. There is a live banner to answer, so the user
    /// answers it again rather than being told nothing happened.
    Superseded,
    /// The authority is gone with no successor — typically the raw-watcher path,
    /// which revokes without emitting any page event. The banner must be
    /// re-observed rather than left dead.
    Withdrawn,
}

/// A live editor instance registered against a path.
#[derive(Clone, Debug)]
pub(super) struct ActivationRecord {
    pub(super) activation: EditorActivation,
    /// Present editors are live for an existing file. Absent editors are live for
    /// a prospective target that does not exist yet; the target is re-resolved and
    /// compared at first save.
    pub(super) prospective: bool,
    /// Exact source text this editor instance last loaded or successfully
    /// wrote. Unlike the graph cache or Concord ledger, an external watcher
    /// admission does not advance it; this is the true three-way base for a
    /// live save conflict.
    pub(super) baseline: Option<String>,
}

#[derive(Default)]
pub(super) struct EditorActivationState {
    pub(super) next: u64,
    // Replacement activation is two-phase across an async frontend boundary:
    // B must become live before A can be retired. Keep every exact activation
    // for the path during that interval; the last record is the prospective
    // current instance returned by idempotent Reuse.
    pub(super) live: std::collections::HashMap<PathBuf, Vec<ActivationRecord>>,
}

/// The outcome of activating an editor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditorActivationHandle {
    pub activation: EditorActivation,
    /// The exact path this activation is live for. For an absent editor this is
    /// the prospective target resolved at activation time, which first save
    /// re-resolves and compares.
    pub target: String,
    /// True when the target did not exist at activation time. Holding it reserves
    /// nothing on disk, so activation creates no unrequested write.
    pub prospective: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConflictEditorEpisode {
    pub(super) loaded_revision: Option<String>,
    /// The editor activation that observed the conflict.
    ///
    /// `None` is the editor-less writer (external import,
    /// sync-id migration, PDF-highlight write) and the pre-increment-3 caller.
    /// Those are legal on the ordinary path — the base-revision guard is their
    /// authority — and refused on the override path.
    pub(super) activation: Option<EditorActivation>,
}

/// Which conflict a "Keep mine" is answering.
///
/// A force request must name the observation the user was actually SHOWN. The
/// path alone is not enough: authority for a NEWER, unseen winner can be minted
/// between the moment a request is issued and the moment it runs, and a request
/// that names only its path will happily consume it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConflictOverride {
    pub observation_epoch: u64,
}

/// App-private recovery material for a live Direct Files conflict. The caller
/// persists this outside the graph so an unresolved draft survives navigation,
/// a clean shutdown, or a process crash. `disk_rev` is the exact revision the
/// review was computed against; resolution rechecks it under the page lock.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveSaveConflictCapture {
    pub diff: crate::sync_diff::SyncConflictDiff,
    pub base_text: Option<String>,
    pub disk_rev: String,
}

/// Which authority a live-conflict capsule review was computed under: the
/// process-local one-shot token, or the disk revision the review displayed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LiveSaveConflictReviewAuthority {
    Live { conflict_epoch: u64 },
    Durable { expected_disk_rev: String },
}

#[derive(Clone, Debug)]
pub(super) struct ConflictAuthority {
    pub(super) snapshot: ConflictSnapshot,
    pub(super) bytes: Option<String>,
    pub(super) editor_episode: ConflictEditorEpisode,
    pub(super) observation_epoch: u64,
}

#[derive(Default)]
pub(super) struct ConflictAuthorityState {
    pub(super) observation_epochs: std::collections::HashMap<PathBuf, u64>,
    pub(super) tokens: std::collections::HashMap<PathBuf, ConflictAuthority>,
}

#[derive(Clone, Copy)]
pub(super) enum EditorConflictSite {
    SaveBaselinePresent,
    SaveBaselineAbsent,
    CommitRecheck,
    ReplacePreRetirement,
    ReplaceRetiredMismatch,
    ReplacePublicationCollision,
    CreatePublicationCollision,
    FinalRereadAbsent,
    FinalRereadPresent,
    ReplacePostPublication,
}

impl EditorConflictSite {
    /// Every conflict-minting site. Exhaustive by construction: the length is
    /// pinned, so adding a variant without adding it here fails to compile and
    /// the site-to-code guards cannot silently stop covering it.
    #[cfg(test)]
    pub(super) const ALL: [Self; 10] = [
        Self::SaveBaselinePresent,
        Self::SaveBaselineAbsent,
        Self::CommitRecheck,
        Self::ReplacePreRetirement,
        Self::ReplaceRetiredMismatch,
        Self::ReplacePublicationCollision,
        Self::CreatePublicationCollision,
        Self::FinalRereadAbsent,
        Self::FinalRereadPresent,
        Self::ReplacePostPublication,
    ];

    pub(super) fn message(self) -> &'static str {
        match self {
            Self::SaveBaselinePresent => "editor conflict: save baseline present",
            Self::SaveBaselineAbsent => "editor conflict: save baseline absent",
            Self::CommitRecheck => "editor conflict: commit recheck",
            Self::ReplacePreRetirement => "editor conflict: replace pre-retirement",
            Self::ReplaceRetiredMismatch => "editor conflict: retired mismatch",
            Self::ReplacePublicationCollision => "editor conflict: publication collision",
            Self::CreatePublicationCollision => "editor conflict: create publication collision",
            Self::FinalRereadAbsent => "editor conflict: final reread absent",
            Self::FinalRereadPresent => "editor conflict: final reread present",
            Self::ReplacePostPublication => "editor conflict: post-publication validation",
        }
    }

    pub(super) fn tokenless_message(self) -> &'static str {
        match self {
            Self::SaveBaselinePresent => "tokenless editor conflict: save baseline present",
            Self::SaveBaselineAbsent => "tokenless editor conflict: save baseline absent",
            Self::CommitRecheck => "tokenless editor conflict: commit recheck",
            Self::ReplacePreRetirement => "tokenless editor conflict: replace pre-retirement",
            Self::ReplaceRetiredMismatch => "tokenless editor conflict: retired mismatch",
            Self::ReplacePublicationCollision => "tokenless editor conflict: publication collision",
            Self::CreatePublicationCollision => {
                "tokenless editor conflict: create publication collision"
            }
            Self::FinalRereadAbsent => "tokenless editor conflict: final reread absent",
            Self::FinalRereadPresent => "tokenless editor conflict: final reread present",
            Self::ReplacePostPublication => {
                "tokenless editor conflict: post-publication validation"
            }
        }
    }

    pub(super) fn conflict_code(self) -> DirectSaveFailureCode {
        match self {
            Self::SaveBaselinePresent => DirectSaveFailureCode::ConflictSaveBaselinePresent,
            Self::SaveBaselineAbsent => DirectSaveFailureCode::ConflictSaveBaselineAbsent,
            Self::CommitRecheck => DirectSaveFailureCode::ConflictCommitRecheck,
            Self::ReplacePreRetirement => DirectSaveFailureCode::ConflictReplacePreRetirement,
            Self::ReplaceRetiredMismatch => DirectSaveFailureCode::ConflictReplaceRetiredMismatch,
            Self::ReplacePublicationCollision => {
                DirectSaveFailureCode::ConflictReplacePublicationCollision
            }
            Self::CreatePublicationCollision => {
                DirectSaveFailureCode::ConflictCreatePublicationCollision
            }
            Self::FinalRereadAbsent => DirectSaveFailureCode::ConflictFinalRereadAbsent,
            Self::FinalRereadPresent => DirectSaveFailureCode::ConflictFinalRereadPresent,
            Self::ReplacePostPublication => DirectSaveFailureCode::ConflictReplacePostPublication,
        }
    }

    pub(super) fn tokenless_code(self) -> DirectSaveFailureCode {
        match self {
            Self::SaveBaselinePresent => DirectSaveFailureCode::ConflictRetrySaveBaselinePresent,
            Self::SaveBaselineAbsent => DirectSaveFailureCode::ConflictRetrySaveBaselineAbsent,
            Self::CommitRecheck => DirectSaveFailureCode::ConflictRetryCommitRecheck,
            Self::ReplacePreRetirement => DirectSaveFailureCode::ConflictRetryReplacePreRetirement,
            Self::ReplaceRetiredMismatch => {
                DirectSaveFailureCode::ConflictRetryReplaceRetiredMismatch
            }
            Self::ReplacePublicationCollision => {
                DirectSaveFailureCode::ConflictRetryReplacePublicationCollision
            }
            Self::CreatePublicationCollision => {
                DirectSaveFailureCode::ConflictRetryCreatePublicationCollision
            }
            Self::FinalRereadAbsent => DirectSaveFailureCode::ConflictRetryFinalRereadAbsent,
            Self::FinalRereadPresent => DirectSaveFailureCode::ConflictRetryFinalRereadPresent,
            Self::ReplacePostPublication => {
                DirectSaveFailureCode::ConflictRetryReplacePostPublication
            }
        }
    }
}
