//! Runtime resume points (RRP): the durable evidence that names which retained
//! scratch run a restart may reuse instead of recompute.
//!
//! A resume point is **inert evidence**, exactly like `PromotedRuntimeStateV1`
//! and `EngineHistoryBinding`. It never authorizes a write, a frontier
//! advance, a projection, a SQLite advance, or an import. Its only job is to
//! select *which reconstructible run-local bytes may be reused instead of
//! recomputed*; every acceptance and authorization gate downstream is
//! unchanged, and any doubt at all means "discard the point and replay".
//!
//! This module is the pure format and set layer:
//!
//! * one bounded, canonically encoded, digest-sealed record
//!   ([`RuntimeResumePointV1`]);
//! * one directory survey ([`ResumePointScan`]) that separates *recognized
//!   canonical points* from *preserved unrecognizable residue*;
//! * one strict proof ([`ResumePointSet`]) minted only from a survey that
//!   recognized every entry, whose answer to "which retained runs are still
//!   reachable" is total or absent, never partial.
//!
//! **The poison rule is the single most important property here, and it is
//! scoped to deletion authority over retained runs.** An unreadable pointer
//! must never be read as "points to nothing": if any entry of the resume-point
//! directory cannot be classified and authenticated, no [`ResumePointSet`]
//! exists, so no [`ReachableRetainedRuns`] can be minted, so no retained run is
//! reclaimed. A leaked run costs disk; a prematurely deleted run costs the only
//! resumable bytes.
//!
//! **Maintenance is deliberately *not* gated on that proof.** Removing
//! resume points never deletes user data or run bytes — it only makes the
//! restart replay, which is always correct. Refusing to prune or clear because
//! the directory also holds a `.DS_Store`, a Syncthing `.sync-conflict-*` copy,
//! a Dropbox ` (1)` duplicate, an editor `.bak`, or a torn point would make the
//! `Unsafe -> Safe` handoff drain permanently impossible for an ordinary
//! desktop accident. So [`prune_resume_points_below`] and
//! [`clear_resume_points_in`] remove exactly the points they fully recognized,
//! preserve every other byte untouched, report what they preserved, and leave
//! the reachability proof unmintable while that residue exists.
//!
//! Fault model: single user, multiple honest devices, fallible filesystem sync
//! providers (`specs/notes/2026-07-22-sparse-oplog-storage-execution.md` §0.1).
//! The bounded byte ceiling, canonical encode/decode equality, explicit schema
//! fence, payload digest and filename-to-sequence binding exist to detect
//! accidental damage — truncation, bit rot, a provider-restored or hand-copied
//! file, stale residue — and deliberately not to resist a hostile local
//! process. Such a process can already delete the oplog; hardening against it
//! is out of scope and must not be added here.

use std::collections::BTreeSet;

use cap_std::fs::Dir;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::hot_engine::AcceptedFrontierRoot;
use super::object_store::{
    open_existing_dir_nofollow, read_optional_regular, sync_dir_required, BlockClaimIndexRoot,
    StoreError,
};
use super::scratch_store::{ScratchAuthenticatedCatalogRoot, ScratchRoots};
use super::{ContentDigest, SessionId, WorkspaceId};

/// Directory of published resume points, beneath one endpoint's durable
/// engine-history control directory.
pub(crate) const RESUME_POINT_DIR: &str = "resume-points";
pub(crate) const RESUME_POINT_SUFFIX: &str = ".resume-point";
/// The first honest resume-point format. No earlier bytes were ever published,
/// and any other value is rejected rather than reinterpreted or migrated.
pub(crate) const RESUME_POINT_SCHEMA_VERSION: u32 = 1;
/// Hard fail-closed ceiling on one sealed record, on both encode and read.
///
/// Every payload member is a constant-size authenticated root, a digest, or a
/// `u64`; the only variable-length members are the authenticated LSM and point
/// roots, whose segment references carry bounded key spans. Exceeding the
/// ceiling is not an error the caller must recover from: it simply means this
/// engine state is not *publishable*, so the restart pays a full replay. That
/// is always available and always correct.
pub(crate) const MAX_RESUME_POINT_BYTES: u64 = 16 * 1024;
/// The bound one *publication* maintains, and the bound the strict adoption
/// proof requires.
///
/// `publish_resume_point` prunes below the durable latest *before* it publishes
/// whenever the recognized set has already reached this bound, so the widest
/// durable cut it can produce is `{latest, successor}`. That makes two the
/// steady-state and transient bound of the publication path — an invariant it
/// enforces, **not** a theorem about whatever is on disk. A directory can still
/// hold more, because an older build published without the pre-prune, or
/// because a provider restored a file.
///
/// A surplus is therefore a *recoverable* condition, not a brick: it fails the
/// strict [`ResumePointSet`] proof (so nothing is reclaimed), while
/// [`prune_resume_points_below`], [`clear_resume_points_in`] and the next
/// publication all still converge it.
pub(crate) const MAX_RETAINED_RESUME_POINTS: usize = 2;
/// Fixed-width zero-padded decimal, so lexicographic order is numeric order
/// and "highest valid" needs no parsing ambiguity. `u64::MAX` is exactly 20
/// digits.
const RESUME_POINT_SEQUENCE_DIGITS: usize = 20;
/// Residue class produced by this repository's own immutable publication
/// primitive (`object_store::publish_immutable`). A crash between the temp
/// write and the rename leaves exactly this shape, so it is ignored by the
/// scan rather than treated as an unclassifiable stranger.
const PUBLICATION_TEMP_PREFIX: &str = ".tmp-";

/// Why one resume point, or one strict resume-point proof, was refused.
///
/// Every variant means the same thing to the *proof* callers: **do not adopt,
/// do not reclaim, preserve every candidate retained run**. The variants exist
/// so the refusal is diagnosable, not so a caller can decide that some of them
/// are recoverable. Conservative maintenance is a separate surface
/// ([`prune_resume_points_below`], [`clear_resume_points_in`]) that carries the
/// same bytes as *preserved residue* instead of as a refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResumePointError {
    /// A sealed record at an unsupported schema version. Never migrated.
    UnsupportedSchema(u32),
    /// Bytes that do not decode, do not re-encode to themselves, or fail an
    /// internal representation invariant.
    Malformed(&'static str),
    /// The payload's own sequence disagrees with the file name it was read
    /// from: a copied, renamed, or provider-restored file.
    NameMismatch {
        named: u64,
        payload: u64,
    },
    /// A record over [`MAX_RESUME_POINT_BYTES`], on encode or on read.
    TooLarge {
        length: u64,
        limit: u64,
    },
    /// More recognized points than a publication leaves at any durable cut.
    /// Adoption and reclamation refuse; maintenance still converges it.
    TooManyPoints(usize),
    /// An entry in the resume-point directory that is not a published point
    /// and not this repository's own publication residue.
    UnexpectedEntry(String),
    Io(String),
}

impl std::fmt::Display for ResumePointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema(version) => {
                write!(f, "unsupported runtime resume-point schema {version}")
            }
            Self::Malformed(reason) => write!(f, "malformed runtime resume point: {reason}"),
            Self::NameMismatch { named, payload } => write!(
                f,
                "runtime resume point named {named} carries sequence {payload}"
            ),
            Self::TooLarge { length, limit } => write!(
                f,
                "runtime resume point is {length} bytes, over its {limit}-byte bound"
            ),
            Self::TooManyPoints(count) => {
                write!(f, "{count} published runtime resume points is unbounded")
            }
            Self::UnexpectedEntry(name) => {
                write!(f, "unexpected runtime resume-point entry {name:?}")
            }
            Self::Io(error) => write!(f, "runtime resume-point I/O failed: {error}"),
        }
    }
}

impl std::error::Error for ResumePointError {}

impl From<std::io::Error> for ResumePointError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<StoreError> for ResumePointError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::StoredFileTooLarge { length, limit, .. } => {
                Self::TooLarge { length, limit }
            }
            other => Self::Io(other.to_string()),
        }
    }
}

/// The sealed on-disk envelope.
///
/// Its shape is frozen across every future generation so that the schema fence
/// stays readable: a v2 payload still decodes as this envelope and is refused
/// as [`ResumePointError::UnsupportedSchema`] rather than as undifferentiated
/// garbage. The payload is carried as opaque bytes so `payload_digest` can
/// cover *every* payload field without the self-reference a digest field
/// inside the payload would create.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedResumePointV1 {
    schema_version: u32,
    payload: Vec<u8>,
    /// Digest over the canonical encoding of the complete payload. Canonical
    /// re-encode equality already rejects non-canonical residue, but a flipped
    /// bit inside a `u64` generation still re-encodes to itself; only this
    /// digest catches that, and it is what stops silent mis-rooting of an
    /// adopted run.
    payload_digest: ContentDigest,
}

/// One durable runtime resume point.
///
/// Field selection: the durable `ColdHistoryRecord` already commits, per
/// generation, the portable-path root and conflicts, the catalog checkpoint
/// binding, terminal evidence, page-name authority, the Logseq claim root, the
/// reference-catalog policy/root, the manifest fingerprint, and the bootstrap
/// binding. Comparing that against the state a run-local reconstruction must
/// carry leaves exactly the members below. Everything else is a reopened
/// capability, a process-local mint, or observational telemetry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeResumePointV1 {
    /// Monotone per-endpoint publication sequence, cross-checked against the
    /// file name so a copied, renamed, or provider-restored file fails closed.
    /// Sequence zero is never published, so "no point" and "sequence 0" cannot
    /// be confused.
    pub(crate) resume_sequence: u64,

    // ---- authenticated binding ----
    pub(crate) workspace_id: WorkspaceId,
    /// `ContentDigest::of(&PromotedRuntimeStateV1::encode())`. This transitively
    /// binds the endpoint, device, graph resource, receipt store, archive
    /// resource claim, archive control identity, lineage, catalog document,
    /// bootstrap aggregate/import identity, the bootstrap anchor authority, the
    /// enrollment verification/binding digests, and the promotion session — one
    /// field instead of thirteen restatements that could drift apart.
    pub(crate) promoted_state_digest: ContentDigest,

    // ---- durable-history authority the run-local roots correspond to ----
    pub(crate) history_generation: u64,
    pub(crate) history_index_root: ContentDigest,

    // ---- LocalActive lifecycle evidence ----
    /// `EnrollmentRecordV1.generation` of the record whose lifecycle carries
    /// `HandoffV1::Unsafe`. Diagnostic and generation-ordering evidence only:
    /// the liveness oracle is the archive-rooted workspace runtime lease, never
    /// a recorded session identity. After a crash takeover the recorded head
    /// and session deliberately differ from the live ones.
    pub(crate) enrollment_generation: u64,
    pub(crate) enrollment_head: ContentDigest,
    pub(crate) unsafe_session_id: SessionId,

    // ---- the retained run ----
    pub(crate) scratch_run_id: Uuid,
    /// `ScratchStore::binding_digest()`, i.e. the digest of the run's own
    /// canonical marker. This catches a run whose owner nonce was replaced —
    /// a re-created run reusing the same UUID.
    pub(crate) scratch_binding_digest: ContentDigest,

    // ---- run-local roots not derivable from the durable history record ----
    pub(crate) scratch_roots: ScratchRoots,
    pub(crate) block_claim_root: BlockClaimIndexRoot,
    pub(crate) accepted_frontier_root: AcceptedFrontierRoot,
    pub(crate) next_acceptance_sequence: u64,
    pub(crate) current_path_catalog_root: ScratchAuthenticatedCatalogRoot,
    pub(crate) current_path_catalog_available: bool,
    pub(crate) current_path_catalog_frontier: AcceptedFrontierRoot,
}

impl RuntimeResumePointV1 {
    /// Representation invariants that hold independently of any archive.
    pub(crate) fn validate(&self) -> Result<(), ResumePointError> {
        if self.resume_sequence == 0 {
            return Err(ResumePointError::Malformed(
                "resume sequence zero is never published",
            ));
        }
        if self.scratch_run_id.is_nil() {
            return Err(ResumePointError::Malformed(
                "a resume point must name a real retained run",
            ));
        }
        Ok(())
    }

    /// Digest over the canonical encoding of every payload field.
    pub(crate) fn payload_digest(&self) -> Result<ContentDigest, ResumePointError> {
        Ok(ContentDigest::of(&encode_canonical(self)?))
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, ResumePointError> {
        self.encode_bounded(MAX_RESUME_POINT_BYTES)
    }

    /// Seal and bound one record. The explicit limit exists so the ceiling
    /// mechanism itself is testable without fabricating a graph-sized root.
    fn encode_bounded(&self, limit: u64) -> Result<Vec<u8>, ResumePointError> {
        self.validate()?;
        let payload = encode_canonical(self)?;
        let sealed = SealedResumePointV1 {
            schema_version: RESUME_POINT_SCHEMA_VERSION,
            payload_digest: ContentDigest::of(&payload),
            payload,
        };
        let bytes = encode_canonical(&sealed)?;
        let length = bytes.len() as u64;
        if length > limit {
            return Err(ResumePointError::TooLarge { length, limit });
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ResumePointError> {
        let length = bytes.len() as u64;
        if length > MAX_RESUME_POINT_BYTES {
            return Err(ResumePointError::TooLarge {
                length,
                limit: MAX_RESUME_POINT_BYTES,
            });
        }
        let sealed: SealedResumePointV1 = decode_canonical(
            bytes,
            "sealed resume-point envelope does not decode",
            "sealed resume-point envelope is not canonical",
        )?;
        if sealed.schema_version != RESUME_POINT_SCHEMA_VERSION {
            return Err(ResumePointError::UnsupportedSchema(sealed.schema_version));
        }
        if ContentDigest::of(&sealed.payload) != sealed.payload_digest {
            return Err(ResumePointError::Malformed(
                "resume-point payload digest does not cover these bytes",
            ));
        }
        let point: Self = decode_canonical(
            &sealed.payload,
            "resume-point payload does not decode",
            "resume-point payload is not canonical",
        )?;
        point.validate()?;
        Ok(point)
    }

    pub(crate) fn file_name(&self) -> String {
        format!(
            "{:0width$}{RESUME_POINT_SUFFIX}",
            self.resume_sequence,
            width = RESUME_POINT_SEQUENCE_DIGITS
        )
    }
}

pub(crate) use reachability::ReachableRetainedRuns;

/// The entire privacy scope of the reachability proof's identity set.
///
/// Type privacy is the authority boundary, and this module is what makes that
/// boundary *small*. `ReachableRetainedRuns`' tuple field is private to this
/// module, so no `impl From<..>`, no `impl FromIterator<..>` and no second
/// inherent `impl` written anywhere else — in this file or in any other — can
/// construct the proof at all: that is `E0616`, a compile error, not a
/// convention. Reviewing the mint surface therefore means reviewing these few
/// lines rather than the whole crate, and the source contract in this module's
/// tests keeps *this* region honest.
mod reachability {
    use super::*;

    /// A retained-run reachability answer derived from a *complete*,
    /// poison-checked resume-point scan.
    ///
    /// Reclamation consumes this type rather than a bare identity set so the
    /// "complete proof" precondition is carried by the type system.
    /// [`ResumePointSet::reachable_runs`] is the **only** way to obtain one
    /// outside `#[cfg(test)]`, and a `ResumePointSet` can only be minted by a
    /// survey that recognized and authenticated every entry it saw.
    ///
    /// The absent constructions are load-bearing, not an oversight, and the
    /// `const _` surface guard below keeps them absent:
    ///
    /// * no `Default` — the default of a set is the *empty* set, i.e. "nothing
    ///   is reachable", which is the single most destructive value this type
    ///   can hold and exactly what the poison rule exists to prevent. It also
    ///   makes `…map(ResumePointSet::reachable_runs).unwrap_or_default()`
    ///   compile, which silently converts a failed or poisoned scan into
    ///   "delete everything";
    /// * no `Clone` — nothing needs to copy a proof, and a copy is one refactor
    ///   away from a mutated one;
    /// * no `From<BTreeSet<Uuid>>`, no `From<Vec<Uuid>>`, no
    ///   `FromIterator<Uuid>` and no `Deserialize` — each would let a bare,
    ///   collected or transported identity set become deletion authority.
    ///
    /// What this type proves is **completeness**, never **currency**: a survey
    /// of a stale or provider-damaged directory yields a complete-but-wrong
    /// answer. The live run is protected against that by its own exclusive
    /// lease, which `reclaim_unreachable_retained_runs` acquires before it
    /// unlinks anything.
    #[derive(Debug, Eq, PartialEq)]
    pub(crate) struct ReachableRetainedRuns(BTreeSet<Uuid>);

    /// Compile-level surface guard for the reachability proof.
    ///
    /// Each blanket implementation below overlaps its concrete twin exactly
    /// when [`ReachableRetainedRuns`] gains that blanket construction route.
    /// Re-deriving `Default` or `Clone`, adding a `From`/`FromIterator`
    /// conversion from an identity collection, or making the type
    /// deserializable therefore stops the crate compiling with a
    /// conflicting-impl error, instead of quietly restoring a forgeable proof.
    /// These fire even for an impl written inside this module, which is the
    /// half that privacy alone cannot cover.
    const _: () = {
        trait NoDefaultMint {}
        impl<T: Default> NoDefaultMint for T {}
        impl NoDefaultMint for ReachableRetainedRuns {}

        trait NoCloneMint {}
        impl<T: Clone> NoCloneMint for T {}
        impl NoCloneMint for ReachableRetainedRuns {}

        trait NoBareSetMint {}
        impl<T: From<BTreeSet<Uuid>>> NoBareSetMint for T {}
        impl NoBareSetMint for ReachableRetainedRuns {}

        trait NoOwnedSetMint {}
        impl<T: From<Vec<Uuid>>> NoOwnedSetMint for T {}
        impl NoOwnedSetMint for ReachableRetainedRuns {}

        trait NoIteratorMint {}
        impl<T: FromIterator<Uuid>> NoIteratorMint for T {}
        impl NoIteratorMint for ReachableRetainedRuns {}

        trait NoDecodedMint {}
        impl<T: DeserializeOwned> NoDecodedMint for T {}
        impl NoDecodedMint for ReachableRetainedRuns {}
    };

    impl ReachableRetainedRuns {
        /// The single production mint.
        ///
        /// It takes the strict proof itself rather than run identities, so
        /// there is no signature here through which a bare, collected or
        /// transported identity set could become deletion authority.
        pub(super) fn of_complete_set(set: &ResumePointSet) -> Self {
            Self(
                set.points()
                    .iter()
                    .map(|point| point.scratch_run_id)
                    .collect(),
            )
        }

        pub(crate) fn contains(&self, run_id: Uuid) -> bool {
            self.0.contains(&run_id)
        }

        pub(crate) fn len(&self) -> usize {
            self.0.len()
        }

        /// Hand-built reachability for reclamation tests. Deliberately
        /// test-only: production callers must derive the set from a complete
        /// scan.
        #[cfg(test)]
        pub(crate) fn from_run_ids_for_test(run_ids: impl IntoIterator<Item = Uuid>) -> Self {
            Self(run_ids.into_iter().collect())
        }
    }
}

/// One directory entry that is not a recognized canonical resume point.
///
/// Residue is *preserved and reported*, never deleted and never interpreted.
/// The shapes this fault model expects are all ordinary accidents: a macOS
/// `.DS_Store`, an editor or backup `.bak`, a Syncthing
/// `<base>.sync-conflict-<stamp>-<device>.resume-point` copy, a Dropbox
/// `<base> (1).resume-point` duplicate, a torn or half-copied point, a name
/// that is not exactly twenty digits plus the suffix, or a symlink, FIFO or
/// directory wearing a point name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnrecognizedResumePointEntry {
    pub(crate) name: String,
    pub(crate) reason: ResumePointError,
}

/// What one conservative maintenance pass removed, and what it refused to
/// touch.
///
/// `preserved` is the whole point of the type: a caller that drains resume
/// points still has to be able to see that the directory is accumulating
/// provider residue, because that residue is also what keeps the reachability
/// proof — and therefore retained-run reclamation — unavailable.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResumePointMaintenance {
    /// Recognized canonical points that were unlinked and durably synced.
    pub(crate) removed: usize,
    /// Names of the entries this pass deliberately left byte-for-byte intact.
    pub(crate) preserved: Vec<String>,
}

impl ResumePointMaintenance {
    pub(crate) fn preserved_residue(&self) -> bool {
        !self.preserved.is_empty()
    }
}

/// One complete survey of a resume-point directory.
///
/// The survey itself never fails on content: it *classifies*. Every entry is
/// either a recognized canonical point, this repository's own publication temp
/// residue (ignored, since no point was ever committed under that name), or
/// unrecognizable residue that is recorded and left alone. Only a failure to
/// enumerate the directory at all is an error, because that proves nothing
/// about anything.
///
/// Deciding what a survey *authorizes* is the caller's job, and the two answers
/// are deliberately different: [`Self::into_set`] is the strict proof used for
/// adoption and reclamation, while the maintenance functions act on
/// [`Self::points`] alone.
#[derive(Debug)]
pub(crate) struct ResumePointScan {
    /// Ascending by `resume_sequence`.
    points: Vec<RuntimeResumePointV1>,
    /// Ascending by name.
    residue: Vec<UnrecognizedResumePointEntry>,
}

impl ResumePointScan {
    /// Survey the resume-point directory beneath one endpoint's control
    /// directory.
    ///
    /// An absent directory is the ordinary "never published" shape and yields
    /// an empty survey, which is a genuine complete answer rather than a
    /// fabricated one: it still requires the control-directory capability, and
    /// a directory that is present but is not a real no-follow directory is an
    /// error. (An *accidentally deleted* directory therefore does read as
    /// "nothing is reachable". That is the one place where absence is trusted;
    /// it costs at most reconstructible accelerator bytes, and the live run is
    /// still protected by its own exclusive lease.)
    pub(crate) fn survey(control: &Dir) -> Result<Self, ResumePointError> {
        let Some(directory) = open_existing_dir_nofollow(control, RESUME_POINT_DIR)? else {
            return Ok(Self {
                points: Vec::new(),
                residue: Vec::new(),
            });
        };
        Self::survey_directory(&directory)
    }

    /// Survey one already-opened resume-point directory.
    pub(crate) fn survey_directory(dir: &Dir) -> Result<Self, ResumePointError> {
        let mut points = Vec::new();
        let mut residue: Vec<UnrecognizedResumePointEntry> = Vec::new();
        for entry in dir.entries()? {
            // Failing to enumerate an entry is the one hard error: it is not a
            // classification, so nothing at all has been proved or preserved.
            let entry = entry?;
            let raw_name = entry.file_name();
            let Some(name) = raw_name.to_str().map(str::to_owned) else {
                let lossy = raw_name.to_string_lossy().into_owned();
                residue.push(UnrecognizedResumePointEntry {
                    reason: ResumePointError::UnexpectedEntry(lossy.clone()),
                    name: lossy,
                });
                continue;
            };
            if name.starts_with(PUBLICATION_TEMP_PREFIX) {
                // Residue of this repository's own immutable publication: the
                // rename never happened, so no point was ever committed here.
                continue;
            }
            match recognize_point_entry(dir, &entry, &name) {
                Ok(point) => points.push(point),
                Err(reason) => residue.push(UnrecognizedResumePointEntry { name, reason }),
            }
        }
        points.sort_by_key(|point| point.resume_sequence);
        residue.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self { points, residue })
    }

    /// Recognized canonical points, ascending by sequence.
    pub(crate) fn points(&self) -> &[RuntimeResumePointV1] {
        &self.points
    }

    pub(crate) fn residue(&self) -> &[UnrecognizedResumePointEntry] {
        &self.residue
    }

    /// Refuse unless every entry was recognized.
    ///
    /// The returned error is the first residue entry's own reason, so a
    /// truncated point still reports `Malformed`, an oversize one `TooLarge`,
    /// and a copied one `NameMismatch` — the refusal stays as diagnosable as it
    /// was when the scan aborted on the first stranger.
    pub(crate) fn require_recognizable(&self) -> Result<(), ResumePointError> {
        match self.residue.first() {
            None => Ok(()),
            Some(entry) => Err(entry.reason.clone()),
        }
    }

    /// The strict proof: every entry recognized, and no more points than a
    /// publication leaves at a durable cut.
    ///
    /// A caller that gets an `Err` here has proved nothing about reachability
    /// and must preserve every candidate retained run.
    pub(crate) fn into_set(self) -> Result<ResumePointSet, ResumePointError> {
        self.require_recognizable()?;
        if self.points.len() > MAX_RETAINED_RESUME_POINTS {
            return Err(ResumePointError::TooManyPoints(self.points.len()));
        }
        Ok(ResumePointSet {
            points: self.points,
        })
    }
}

/// The complete validated resume-point set of one endpoint.
///
/// Minted only by a survey that recognized every entry, so
/// [`Self::reachable_runs`] is a *total* reachability answer and never a
/// partial one. It has no other constructor, in particular no empty one: the
/// "never published" value comes from an actual survey of an actual capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResumePointSet {
    /// Ascending by `resume_sequence`.
    points: Vec<RuntimeResumePointV1>,
}

impl ResumePointSet {
    /// Strictly read and validate every entry of one resume-point directory.
    pub(crate) fn read(dir: &Dir) -> Result<Self, ResumePointError> {
        ResumePointScan::survey_directory(dir)?.into_set()
    }

    pub(crate) fn points(&self) -> &[RuntimeResumePointV1] {
        &self.points
    }

    /// The highest-sequence validated point, if any.
    pub(crate) fn latest(&self) -> Option<&RuntimeResumePointV1> {
        self.points.last()
    }

    /// The sequence one publication would use next.
    pub(crate) fn next_sequence(&self) -> Result<u64, ResumePointError> {
        next_resume_sequence(&self.points)
    }

    /// Every retained scratch run this complete set still reaches.
    pub(crate) fn reachable_runs(&self) -> ReachableRetainedRuns {
        ReachableRetainedRuns::of_complete_set(self)
    }
}

/// The sequence one publication would use next, given the recognized points.
pub(crate) fn next_resume_sequence(
    points: &[RuntimeResumePointV1],
) -> Result<u64, ResumePointError> {
    match points.last() {
        None => Ok(1),
        Some(latest) => latest
            .resume_sequence
            .checked_add(1)
            .ok_or(ResumePointError::Malformed(
                "resume sequence space is exhausted",
            )),
    }
}

/// Remove every *recognized* point below `keep`, then make the removal durable.
///
/// Deliberately independent of the strict adoption bound and of the poison
/// rule. Pruning is the operation that *restores* the bound, so refusing to
/// prune because the bound is exceeded is exactly backwards; and residue in the
/// directory is preserved and reported rather than allowed to veto the removal
/// of points that were fully recognized.
pub(crate) fn prune_resume_points_below(
    dir: &Dir,
    keep: u64,
) -> Result<ResumePointMaintenance, ResumePointError> {
    remove_matching_points(dir, |point| point.resume_sequence < keep)
}

/// Remove every *recognized* point, then make the removal durable.
///
/// This is the `Unsafe -> Safe` drain. Afterwards no recognized point names a
/// retained run, which is what would let reclamation collect one — but only if
/// the directory was also residue-free, because reclamation needs the strict
/// [`ResumePointSet`] proof and residue still denies it. A drain that leaves
/// residue therefore leaks retained runs, which is the correct trade: the
/// handoff proceeds and no unproven bytes are deleted.
pub(crate) fn clear_resume_points_in(
    dir: &Dir,
) -> Result<ResumePointMaintenance, ResumePointError> {
    remove_matching_points(dir, |_| true)
}

fn remove_matching_points(
    dir: &Dir,
    select: impl Fn(&RuntimeResumePointV1) -> bool,
) -> Result<ResumePointMaintenance, ResumePointError> {
    let scan = ResumePointScan::survey_directory(dir)?;
    let mut removed = 0;
    let mut failure = None;
    for point in scan.points.iter().filter(|point| select(point)) {
        if let Err(error) = dir.remove_file(point.file_name()) {
            failure = Some(ResumePointError::from(error));
            break;
        }
        removed += 1;
    }
    // Durability before reporting, including on the partial-failure path: a
    // caller that saw this fail must not be able to lose the removals that did
    // happen to a later power cut and see them resurrect.
    if removed > 0 {
        sync_dir_required(dir)?;
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(ResumePointMaintenance {
        removed,
        preserved: scan
            .residue
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>(),
    })
}

/// Recognize exactly one directory entry as a canonical resume point.
///
/// Every `Err` means "this is not a point of mine", and the caller records it
/// as preserved residue rather than acting on it.
fn recognize_point_entry(
    dir: &Dir,
    entry: &cap_std::fs::DirEntry,
    name: &str,
) -> Result<RuntimeResumePointV1, ResumePointError> {
    let sequence = parse_resume_point_name(name)
        .ok_or_else(|| ResumePointError::UnexpectedEntry(name.to_owned()))?;
    require_regular_point_entry(entry, name)?;
    let bytes = read_optional_regular(dir, name, MAX_RESUME_POINT_BYTES, None)?
        .ok_or_else(|| ResumePointError::UnexpectedEntry(name.to_owned()))?;
    let point = RuntimeResumePointV1::decode(&bytes)?;
    if point.resume_sequence != sequence {
        return Err(ResumePointError::NameMismatch {
            named: sequence,
            payload: point.resume_sequence,
        });
    }
    Ok(point)
}

/// Parse one canonical resume-point file name.
///
/// The name must be exactly [`RESUME_POINT_SEQUENCE_DIGITS`] ASCII digits plus
/// the suffix. A short, padded-differently, non-decimal, or overflowing name is
/// not a resume point at all, and the caller treats it as poison rather than
/// guessing what was intended.
fn parse_resume_point_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(RESUME_POINT_SUFFIX)?;
    if digits.len() != RESUME_POINT_SEQUENCE_DIGITS
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

/// Require one directory entry to be a real regular file.
///
/// The check uses the entry's own no-follow file type, so a symlink, FIFO,
/// socket, or directory wearing a resume-point name is refused before anything
/// opens it.
fn require_regular_point_entry(
    entry: &cap_std::fs::DirEntry,
    name: &str,
) -> Result<(), ResumePointError> {
    let file_type = entry.file_type()?;
    if file_type.is_symlink() || !file_type.is_file() {
        return Err(ResumePointError::UnexpectedEntry(format!(
            "{name} is not a regular no-follow file"
        )));
    }
    Ok(())
}

fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, ResumePointError> {
    postcard::to_allocvec(value)
        .map_err(|_| ResumePointError::Malformed("resume point is not encodable"))
}

/// Decode and require the bytes to be the exact canonical encoding of what they
/// decoded to. This rejects trailing bytes, alternative encodings, and any
/// residue that happens to parse.
fn decode_canonical<T: DeserializeOwned + Serialize>(
    bytes: &[u8],
    undecodable: &'static str,
    noncanonical: &'static str,
) -> Result<T, ResumePointError> {
    let value: T =
        postcard::from_bytes(bytes).map_err(|_| ResumePointError::Malformed(undecodable))?;
    if encode_canonical(&value)? != bytes {
        return Err(ResumePointError::Malformed(noncanonical));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::ambient_authority;
    use std::path::{Path, PathBuf};

    fn resume_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tine-resume-point-{label}-{}", Uuid::new_v4()))
    }

    fn open_root(root: &Path) -> Dir {
        std::fs::create_dir_all(root).unwrap();
        Dir::open_ambient_dir(root, ambient_authority()).unwrap()
    }

    fn digest(seed: u8) -> ContentDigest {
        ContentDigest::from_bytes([seed; 32])
    }

    /// Mint a second, distinct value of one opaque authenticated root.
    ///
    /// The run-local root types keep their fields private on purpose, so a
    /// single canonical-encoding byte change is how a format test obtains a
    /// different value without reaching through another module's invariants.
    fn tweaked<T>(value: &T, index: usize, byte: u8) -> T
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let mut bytes = postcard::to_allocvec(value).unwrap();
        assert_ne!(bytes[index], byte, "the tweak must change the encoding");
        bytes[index] = byte;
        let rebuilt: T = postcard::from_bytes(&bytes).unwrap();
        assert_ne!(&rebuilt, value);
        rebuilt
    }

    fn point(sequence: u64) -> RuntimeResumePointV1 {
        RuntimeResumePointV1 {
            resume_sequence: sequence,
            workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(0x9001)),
            promoted_state_digest: digest(0x11),
            history_generation: 7,
            history_index_root: digest(0x22),
            enrollment_generation: 3,
            enrollment_head: digest(0x33),
            unsafe_session_id: SessionId::from_uuid(Uuid::from_u128(0x9002)),
            scratch_run_id: Uuid::from_u128(0x9003),
            scratch_binding_digest: digest(0x44),
            scratch_roots: ScratchRoots::default(),
            block_claim_root: BlockClaimIndexRoot::default(),
            accepted_frontier_root: AcceptedFrontierRoot::empty(),
            next_acceptance_sequence: 12,
            current_path_catalog_root: ScratchAuthenticatedCatalogRoot::default(),
            current_path_catalog_available: true,
            current_path_catalog_frontier: AcceptedFrontierRoot::empty(),
        }
    }

    /// One single-field mutation of the reference point, per payload field.
    fn field_mutations() -> Vec<(&'static str, RuntimeResumePointV1)> {
        let mut mutations = Vec::new();
        let mut with = |label: &'static str, mutate: fn(&mut RuntimeResumePointV1)| {
            let mut mutated = point(1);
            mutate(&mut mutated);
            mutations.push((label, mutated));
        };
        with("resume_sequence", |p| p.resume_sequence = 2);
        with("workspace_id", |p| {
            p.workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0x9101));
        });
        with("promoted_state_digest", |p| {
            p.promoted_state_digest = digest(0x12);
        });
        with("history_generation", |p| p.history_generation = 8);
        with("history_index_root", |p| {
            p.history_index_root = digest(0x23)
        });
        with("enrollment_generation", |p| p.enrollment_generation = 4);
        with("enrollment_head", |p| p.enrollment_head = digest(0x34));
        with("unsafe_session_id", |p| {
            p.unsafe_session_id = SessionId::from_uuid(Uuid::from_u128(0x9102));
        });
        with("scratch_run_id", |p| {
            p.scratch_run_id = Uuid::from_u128(0x9103);
        });
        with("scratch_binding_digest", |p| {
            p.scratch_binding_digest = digest(0x45);
        });
        with("scratch_roots", |p| p.scratch_roots.fanout_head = 5);
        with("block_claim_root", |p| {
            p.block_claim_root = tweaked(&p.block_claim_root, 0, 9);
        });
        with("accepted_frontier_root", |p| {
            p.accepted_frontier_root = tweaked(&p.accepted_frontier_root, 1, 3);
        });
        with("next_acceptance_sequence", |p| {
            p.next_acceptance_sequence = 13;
        });
        with("current_path_catalog_root", |p| {
            p.current_path_catalog_root = tweaked(&p.current_path_catalog_root, 1, 5);
        });
        with("current_path_catalog_available", |p| {
            p.current_path_catalog_available = false;
        });
        with("current_path_catalog_frontier", |p| {
            p.current_path_catalog_frontier = tweaked(&p.current_path_catalog_frontier, 1, 4);
        });
        mutations
    }

    fn seal(bytes: &[u8]) -> SealedResumePointV1 {
        postcard::from_bytes(bytes).unwrap()
    }

    fn reseal(sealed: &SealedResumePointV1) -> Vec<u8> {
        postcard::to_allocvec(sealed).unwrap()
    }

    fn publish(dir: &Path, point: &RuntimeResumePointV1) {
        std::fs::write(dir.join(point.file_name()), point.encode().unwrap()).unwrap();
    }

    #[test]
    fn canonical_encoding_round_trips_every_field() {
        let original = point(1);
        let bytes = original.encode().unwrap();
        let decoded = RuntimeResumePointV1::decode(&bytes).unwrap();
        assert_eq!(decoded, original);
        assert_eq!(decoded.encode().unwrap(), bytes);
    }

    #[test]
    fn every_payload_field_binds_the_sealed_bytes_and_digest() {
        let reference = point(1);
        let reference_bytes = reference.encode().unwrap();
        let reference_digest = reference.payload_digest().unwrap();
        for (label, mutated) in field_mutations() {
            let bytes = mutated.encode().unwrap();
            assert_ne!(bytes, reference_bytes, "{label} does not change the record");
            assert_ne!(
                mutated.payload_digest().unwrap(),
                reference_digest,
                "{label} is outside the payload digest"
            );
            assert_eq!(RuntimeResumePointV1::decode(&bytes).unwrap(), mutated);
        }
    }

    #[test]
    fn a_forged_payload_digest_is_rejected() {
        let bytes = point(1).encode().unwrap();
        let mut sealed = seal(&bytes);
        sealed.payload_digest = digest(0xff);
        assert!(matches!(
            RuntimeResumePointV1::decode(&reseal(&sealed)),
            Err(ResumePointError::Malformed(_))
        ));
    }

    #[test]
    fn an_unknown_schema_version_is_fenced_instead_of_migrated() {
        let bytes = point(1).encode().unwrap();
        let mut sealed = seal(&bytes);
        sealed.schema_version = RESUME_POINT_SCHEMA_VERSION + 1;
        assert_eq!(
            RuntimeResumePointV1::decode(&reseal(&sealed)),
            Err(ResumePointError::UnsupportedSchema(
                RESUME_POINT_SCHEMA_VERSION + 1
            ))
        );
    }

    #[test]
    fn an_extra_payload_field_fails_closed_even_with_a_recomputed_digest() {
        let bytes = point(1).encode().unwrap();
        let mut sealed = seal(&bytes);
        sealed.payload.push(0);
        sealed.payload_digest = ContentDigest::of(&sealed.payload);
        assert!(matches!(
            RuntimeResumePointV1::decode(&reseal(&sealed)),
            Err(ResumePointError::Malformed(_))
        ));
    }

    #[test]
    fn trailing_bytes_after_the_sealed_record_fail_closed() {
        let mut bytes = point(1).encode().unwrap();
        bytes.push(0);
        assert!(matches!(
            RuntimeResumePointV1::decode(&bytes),
            Err(ResumePointError::Malformed(_))
        ));
    }

    #[test]
    fn a_truncated_record_fails_closed() {
        let bytes = point(1).encode().unwrap();
        for cut in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                RuntimeResumePointV1::decode(&bytes[..cut]).is_err(),
                "a {cut}-byte prefix decoded"
            );
        }
    }

    #[test]
    fn an_oversize_encoding_is_refused_before_publication() {
        let point = point(1);
        let length = point.encode().unwrap().len() as u64;
        assert!(length <= MAX_RESUME_POINT_BYTES);
        assert_eq!(
            point.encode_bounded(length - 1),
            Err(ResumePointError::TooLarge {
                length,
                limit: length - 1,
            })
        );
    }

    /// Measured sentinel for the byte ceiling.
    ///
    /// A record whose run-local roots are all empty already costs ~3.1 KiB,
    /// because every authenticated digest is carried as a 64-character hex
    /// string and `BlockClaimIndexRoot` alone spells out 8 x 32 empty segment
    /// slots. Populated roots grow from there, and a fully occupied block-claim
    /// LSM would exceed [`MAX_RESUME_POINT_BYTES`] on its own. That is safe —
    /// an over-ceiling state is simply not publishable and the restart replays
    /// — but it is the number the later snapshot packet must measure against
    /// real graphs before it decides this ceiling is the right one.
    #[test]
    fn an_empty_rooted_record_records_its_measured_headroom() {
        let length = point(1).encode().unwrap().len() as u64;
        assert!(
            (3_000..=3_500).contains(&length),
            "empty-root sealed resume point is {length} bytes"
        );
        assert!(length * 4 < MAX_RESUME_POINT_BYTES);
    }

    #[test]
    fn a_record_larger_than_the_ceiling_is_refused_on_read() {
        let root = resume_root("oversize-read");
        let dir = open_root(&root);
        std::fs::write(
            root.join(point(1).file_name()),
            vec![0_u8; MAX_RESUME_POINT_BYTES as usize + 1],
        )
        .unwrap();
        assert!(matches!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::TooLarge { .. })
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sequence_zero_and_a_nil_run_are_not_representable() {
        assert!(matches!(
            point(0).encode(),
            Err(ResumePointError::Malformed(_))
        ));
        let mut nil_run = point(1);
        nil_run.scratch_run_id = Uuid::nil();
        assert!(matches!(
            nil_run.encode(),
            Err(ResumePointError::Malformed(_))
        ));
    }

    #[test]
    fn the_file_name_binds_the_payload_sequence() {
        assert_eq!(point(1).file_name(), "00000000000000000001.resume-point");
        assert_eq!(
            point(u64::MAX).file_name(),
            "18446744073709551615.resume-point"
        );
        assert_eq!(
            parse_resume_point_name("00000000000000000042.resume-point"),
            Some(42)
        );
        for rejected in [
            "42.resume-point",
            "0000000000000000004a.resume-point",
            "00000000000000000042.resume-point.bak",
            "00000000000000000042",
            "99999999999999999999.resume-point",
            " 0000000000000000042.resume-point",
        ] {
            assert_eq!(parse_resume_point_name(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn a_renamed_point_fails_the_name_to_sequence_binding() {
        let root = resume_root("renamed");
        let dir = open_root(&root);
        let point = point(1);
        std::fs::write(
            root.join("00000000000000000002.resume-point"),
            point.encode().unwrap(),
        )
        .unwrap();
        assert_eq!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::NameMismatch {
                named: 2,
                payload: 1
            })
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_empty_directory_has_no_points_and_starts_at_sequence_one() {
        let root = resume_root("empty");
        let dir = open_root(&root);
        let set = ResumePointSet::read(&dir).unwrap();
        assert!(set.points().is_empty());
        assert!(set.latest().is_none());
        assert_eq!(set.next_sequence().unwrap(), 1);
        assert_eq!(set.reachable_runs().len(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_scan_returns_points_in_ascending_sequence_order() {
        let root = resume_root("ascending");
        let dir = open_root(&root);
        let older = point(41);
        let mut newer = point(42);
        newer.scratch_run_id = Uuid::from_u128(0x9203);
        publish(&root, &newer);
        publish(&root, &older);
        let set = ResumePointSet::read(&dir).unwrap();
        assert_eq!(
            set.points()
                .iter()
                .map(|p| p.resume_sequence)
                .collect::<Vec<_>>(),
            vec![41, 42]
        );
        assert_eq!(set.latest().unwrap(), &newer);
        assert_eq!(set.next_sequence().unwrap(), 43);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_temp_residue_is_ignored_by_the_scan() {
        let root = resume_root("temp-residue");
        let dir = open_root(&root);
        let published = point(1);
        publish(&root, &published);
        std::fs::write(root.join(format!(".tmp-{}", Uuid::new_v4())), b"torn").unwrap();
        let set = ResumePointSet::read(&dir).unwrap();
        assert_eq!(set.points(), &[published]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unknown_entry_poisons_the_whole_scan() {
        let root = resume_root("unknown-entry");
        let dir = open_root(&root);
        publish(&root, &point(1));
        std::fs::write(root.join("stray"), b"stray").unwrap();
        assert!(matches!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::UnexpectedEntry(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_point_poisons_the_scan() {
        let root = resume_root("symlinked");
        let dir = open_root(&root);
        let point = point(1);
        std::fs::write(root.join("decoy"), point.encode().unwrap()).unwrap();
        std::os::unix::fs::symlink(root.join("decoy"), root.join(point.file_name())).unwrap();
        assert!(matches!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::UnexpectedEntry(_))
        ));
        assert!(root.join("decoy").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_named_like_a_point_poisons_the_scan() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let root = resume_root("fifo");
        let dir = open_root(&root);
        let fifo = root.join(point(1).file_name());
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo_c` is a live NUL-terminated path in this test directory.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::UnexpectedEntry(_))
        ));
        assert!(fifo.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_directory_named_like_a_point_poisons_the_scan() {
        let root = resume_root("directory-entry");
        let dir = open_root(&root);
        std::fs::create_dir(root.join(point(1).file_name())).unwrap();
        assert!(matches!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::UnexpectedEntry(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn more_than_two_points_fails_the_strict_proof_but_maintenance_converges_it() {
        let root = resume_root("too-many");
        let dir = open_root(&root);
        for sequence in 1..=(MAX_RETAINED_RESUME_POINTS as u64 + 1) {
            publish(&root, &point(sequence));
        }
        // Adoption and reclamation refuse: a surplus means a prune never ran,
        // so nothing here may authorize deleting a retained run.
        assert_eq!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::TooManyPoints(
                MAX_RETAINED_RESUME_POINTS + 1
            ))
        );
        // Maintenance is not gated on that bound, so the surplus is a state the
        // endpoint recovers from rather than a permanent brick.
        assert_eq!(
            prune_resume_points_below(&dir, MAX_RETAINED_RESUME_POINTS as u64 + 1).unwrap(),
            ResumePointMaintenance {
                removed: MAX_RETAINED_RESUME_POINTS,
                preserved: Vec::new(),
            }
        );
        assert_eq!(
            ResumePointSet::read(&dir).unwrap().points(),
            &[point(MAX_RETAINED_RESUME_POINTS as u64 + 1)]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_surplus_of_points_is_fully_clearable() {
        let root = resume_root("too-many-clear");
        let dir = open_root(&root);
        for sequence in 1..=(MAX_RETAINED_RESUME_POINTS as u64 + 3) {
            publish(&root, &point(sequence));
        }
        assert!(ResumePointSet::read(&dir).is_err());
        assert_eq!(
            clear_resume_points_in(&dir).unwrap().removed,
            MAX_RETAINED_RESUME_POINTS + 3
        );
        assert!(ResumePointSet::read(&dir).unwrap().points().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_torn_point_is_preserved_residue_that_never_becomes_authority() {
        let root = resume_root("torn");
        let dir = open_root(&root);
        let intact = point(1);
        publish(&root, &intact);
        let torn = point(2);
        let torn_bytes = torn.encode().unwrap();
        let torn_prefix = torn_bytes[..torn_bytes.len() - 3].to_vec();
        std::fs::write(root.join(torn.file_name()), &torn_prefix).unwrap();

        // The strict proof still fails closed, with the same diagnosis it gave
        // when the whole scan aborted on the first stranger.
        assert!(matches!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::Malformed(_))
        ));
        let scan = ResumePointScan::survey_directory(&dir).unwrap();
        assert_eq!(scan.points(), &[intact.clone()]);
        assert_eq!(scan.residue().len(), 1);
        assert_eq!(scan.residue()[0].name, torn.file_name());

        // Maintenance drains what it recognized and never touches the rest.
        assert_eq!(
            clear_resume_points_in(&dir).unwrap(),
            ResumePointMaintenance {
                removed: 1,
                preserved: vec![torn.file_name()],
            }
        );
        assert!(!root.join(intact.file_name()).exists());
        assert_eq!(
            std::fs::read(root.join(torn.file_name())).unwrap(),
            torn_prefix
        );
        // And the torn bytes still deny the reachability proof afterwards, so
        // no retained run can be reclaimed on their account.
        assert!(ResumePointSet::read(&dir).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The exact sync-provider and desktop residue family from the campaign's
    /// in-scope fault list. Every shape must be preserved byte-for-byte, must
    /// keep the reachability proof unmintable, and must not stop the drain.
    #[test]
    fn provider_and_desktop_residue_is_preserved_and_never_blocks_the_drain() {
        let canonical = point(1);
        let valid_bytes = canonical.encode().unwrap();
        // Deliberately *valid point bytes* under a residue name: a conflict
        // copy must never be silently promoted to canonical authority.
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (".DS_Store", b"\x00\x01Bud1 desktop residue".to_vec()),
            ("00000000000000000001.resume-point.bak", valid_bytes.clone()),
            (
                "00000000000000000001.sync-conflict-20260728-120000-ABCDEFG.resume-point",
                valid_bytes.clone(),
            ),
            ("00000000000000000001 (1).resume-point", valid_bytes.clone()),
            ("Icon\r", b"desktop database".to_vec()),
            ("00000000000000000009.resume-point", valid_bytes.clone()),
        ];

        for (index, (stranger, bytes)) in cases.into_iter().enumerate() {
            let root = resume_root(&format!("residue-{index}"));
            let dir = open_root(&root);
            publish(&root, &canonical);
            std::fs::write(root.join(stranger), &bytes).unwrap();

            assert!(
                ResumePointSet::read(&dir).is_err(),
                "{stranger} must not mint a reachability proof"
            );
            let maintenance = clear_resume_points_in(&dir).unwrap();
            assert_eq!(
                maintenance,
                ResumePointMaintenance {
                    removed: 1,
                    preserved: vec![stranger.to_owned()],
                },
                "{stranger}"
            );
            assert!(maintenance.preserved_residue());
            assert!(!root.join(canonical.file_name()).exists(), "{stranger}");
            assert_eq!(
                std::fs::read(root.join(stranger)).unwrap(),
                bytes,
                "{stranger}"
            );
            assert!(
                ResumePointSet::read(&dir).is_err(),
                "{stranger} must still deny the proof after the drain"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// A symlink, FIFO or directory wearing a canonical point name is residue
    /// too — and the drain must never `remove_file` it or follow it.
    #[test]
    fn special_entries_wearing_a_point_name_are_preserved_by_the_drain() {
        let root = resume_root("residue-special");
        let dir = open_root(&root);
        let canonical = point(1);
        publish(&root, &canonical);

        let elsewhere = root.join("decoy-target");
        std::fs::write(&elsewhere, b"a file the drain must not unlink").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, root.join(point(2).file_name())).unwrap();
        std::fs::create_dir(root.join(point(3).file_name())).unwrap();
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt as _;
            let fifo = root.join(point(4).file_name());
            let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            // SAFETY: `fifo_c` is a live NUL-terminated path in this test root.
            assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        }

        assert!(ResumePointSet::read(&dir).is_err());
        let maintenance = clear_resume_points_in(&dir).unwrap();
        assert_eq!(maintenance.removed, 1);
        assert!(!root.join(canonical.file_name()).exists());
        // `decoy-target` is itself an unrecognized name, so it is preserved too.
        assert!(maintenance.preserved.contains(&"decoy-target".to_owned()));
        assert!(maintenance.preserved.contains(&point(3).file_name()));
        #[cfg(unix)]
        {
            assert!(maintenance.preserved.contains(&point(2).file_name()));
            assert!(maintenance.preserved.contains(&point(4).file_name()));
            assert!(root.join(point(4).file_name()).exists());
            assert!(std::fs::symlink_metadata(root.join(point(2).file_name()))
                .unwrap()
                .file_type()
                .is_symlink());
        }
        assert_eq!(
            std::fs::read(&elsewhere).unwrap(),
            b"a file the drain must not unlink"
        );
        assert!(root.join(point(3).file_name()).is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The reachability proof must stay non-forgeable.
    ///
    /// Type privacy is the authority boundary and it does the heavy lifting:
    /// the tuple field lives in `mod reachability`, so every route that
    /// constructs the proof — a second inherent `impl`, an `impl From<..>`, an
    /// `impl FromIterator<..>` — has to be *inside that module* or it does not
    /// compile. The blanket-overlap guards beside the type then close the
    /// named conversion traits even inside it.
    ///
    /// This test adds the one part neither mechanism can state: that within the
    /// module's whole body — not merely its first `impl` block — the only mints
    /// are the scan's and the `#[cfg(test)]` fixture, that the guards are all
    /// still present, and that no other file in the packet's write set
    /// constructs or defaults one.
    #[test]
    fn the_reachability_proof_has_exactly_one_production_mint() {
        const RESUME_POINT_SOURCE: &str = include_str!("resume_point.rs");
        const OBJECT_STORE_SOURCE: &str = include_str!("object_store.rs");
        const SCRATCH_STORE_SOURCE: &str = include_str!("scratch_store.rs");

        // Needles are composed at run time so this test does not match its own
        // source and quietly inflate the counts it is checking.
        let proof = "ReachableRetainedRuns";
        let mint = format!("{proof}(");
        let self_mint = format!("Self{}", "(");
        let open = "{";

        // The complete privacy scope of the tuple field: everything from the
        // module header to the column-0 brace that closes it.
        let module = RESUME_POINT_SOURCE
            .split_once(format!("mod reachability {open}").as_str())
            .expect("the private reachability module")
            .1
            .split_once("\n}\n")
            .expect("the end of the private reachability module")
            .0;
        assert!(module.contains(format!("pub(crate) struct {mint}BTreeSet<Uuid>);").as_str()));

        // The named spelling appears exactly once in the whole file — the
        // declaration — and it is inside the module. Anything else, anywhere,
        // is a new construction route.
        assert_eq!(
            RESUME_POINT_SOURCE.matches(mint.as_str()).count(),
            1,
            "a new {proof} construction appeared"
        );
        assert_eq!(module.matches(mint.as_str()).count(), 1);

        // `Self(..)` appears exactly twice in the module, in this order: the
        // production mint, then the `#[cfg(test)]` fixture. Scanning the whole
        // module rather than one `impl` block is what makes a *second* inherent
        // impl, or a conversion impl, visible here.
        let mints: Vec<&str> = module.split(self_mint.as_str()).collect();
        assert_eq!(mints.len(), 3, "the module's {self_mint}..) count changed");
        assert!(
            mints[0].contains("pub(super) fn of_complete_set(set: &ResumePointSet) -> Self"),
            "the first mint is no longer the complete-scan mint"
        );
        let inherent_prefix = mints[0]
            .rsplit_once(format!("impl {proof} {open}").as_str())
            .expect("the inherent impl block")
            .1;
        assert!(
            !inherent_prefix.contains("#[cfg(test)]"),
            "the production mint moved behind #[cfg(test)]"
        );
        assert!(
            mints[1].contains("#[cfg(test)]"),
            "the second mint left the #[cfg(test)] fixture"
        );

        // Every blanket surface guard is still declared. Deleting one is how a
        // forgeable route returns without any other line changing.
        for guard in [
            "impl<T: Default> NoDefaultMint for T {}",
            "impl<T: Clone> NoCloneMint for T {}",
            "impl<T: From<BTreeSet<Uuid>>> NoBareSetMint for T {}",
            "impl<T: From<Vec<Uuid>>> NoOwnedSetMint for T {}",
            "impl<T: FromIterator<Uuid>> NoIteratorMint for T {}",
            "impl<T: DeserializeOwned> NoDecodedMint for T {}",
        ] {
            assert!(module.contains(guard), "the surface guard {guard} is gone");
        }

        // No other file in the packet's write set constructs or defaults one.
        let default_mint = format!("{proof}::default");
        for (label, source) in [
            ("object_store.rs", OBJECT_STORE_SOURCE),
            ("scratch_store.rs", SCRATCH_STORE_SOURCE),
        ] {
            assert!(
                !source.contains(mint.as_str()),
                "{label} constructs a reachability proof directly"
            );
            assert!(
                !source.contains(default_mint.as_str()),
                "{label} mints an empty reachability proof"
            );
        }
        // `ResumePointSet` likewise has no free empty constructor to route
        // around the scan with.
        assert!(!RESUME_POINT_SOURCE.contains(format!("fn empty() -> {}", "Self").as_str()));
    }

    #[test]
    fn reachable_runs_names_every_validated_point() {
        let root = resume_root("reachable");
        let dir = open_root(&root);
        let mut older = point(1);
        older.scratch_run_id = Uuid::from_u128(0x9301);
        let mut newer = point(2);
        newer.scratch_run_id = Uuid::from_u128(0x9302);
        publish(&root, &older);
        publish(&root, &newer);

        let reachable = ResumePointSet::read(&dir).unwrap().reachable_runs();
        assert_eq!(reachable.len(), 2);
        assert!(reachable.contains(Uuid::from_u128(0x9301)));
        assert!(reachable.contains(Uuid::from_u128(0x9302)));
        assert!(!reachable.contains(Uuid::from_u128(0x9303)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_below_keeps_the_named_sequence_and_removes_only_lower_ones() {
        let root = resume_root("prune");
        let dir = open_root(&root);
        let older = point(1);
        let newer = point(2);
        publish(&root, &older);
        publish(&root, &newer);

        assert_eq!(prune_resume_points_below(&dir, 2).unwrap().removed, 1);
        assert!(!root.join(older.file_name()).exists());
        assert_eq!(ResumePointSet::read(&dir).unwrap().points(), &[newer]);
        // Idempotent: a repeated prune at the same watermark removes nothing.
        assert_eq!(prune_resume_points_below(&dir, 2).unwrap().removed, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clear_removes_every_point() {
        let root = resume_root("clear");
        let dir = open_root(&root);
        publish(&root, &point(1));
        publish(&root, &point(2));
        assert_eq!(clear_resume_points_in(&dir).unwrap().removed, 2);
        assert!(ResumePointSet::read(&dir).unwrap().points().is_empty());
        assert_eq!(clear_resume_points_in(&dir).unwrap().removed, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The publication temp class stays ignored rather than becoming residue:
    /// nothing was ever committed under that name, so it neither denies the
    /// proof nor gets reported as something the operator must clean up.
    #[test]
    fn publication_temp_residue_is_neither_a_point_nor_residue() {
        let root = resume_root("temp-not-residue");
        let dir = open_root(&root);
        publish(&root, &point(1));
        let temp = format!(".tmp-{}", Uuid::new_v4());
        std::fs::write(root.join(&temp), b"torn").unwrap();

        let scan = ResumePointScan::survey_directory(&dir).unwrap();
        assert_eq!(scan.points().len(), 1);
        assert!(scan.residue().is_empty());
        assert_eq!(
            clear_resume_points_in(&dir).unwrap(),
            ResumePointMaintenance {
                removed: 1,
                preserved: Vec::new(),
            }
        );
        assert!(root.join(&temp).exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
