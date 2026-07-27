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
//! * one *complete* poison-checked directory scan ([`ResumePointSet`]) whose
//!   answer to "which retained runs are still reachable" is total or absent,
//!   never partial.
//!
//! The poison rule is the single most important property here. An unreadable
//! pointer must never be read as "points to nothing": if any entry of the
//! resume-point directory cannot be classified and authenticated, the whole
//! scan fails and the caller preserves every candidate retained run. A leaked
//! run costs disk; a prematurely deleted run costs the only resumable bytes.
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
    read_optional_regular, sync_dir_required, BlockClaimIndexRoot, StoreError,
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
/// Publish-then-delete leaves at most one superseded point at any crash cut,
/// so two is the exact steady-state and transient bound. Three means a prune
/// never ran, which is a fail-closed corruption signal, not a state to repair
/// by deleting evidence.
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

/// Why one resume point, or one complete resume-point scan, was refused.
///
/// Every variant means the same thing to a caller: **do not adopt, do not
/// prune, do not reclaim, preserve every candidate retained run**. The
/// variants exist so the refusal is diagnosable, not so a caller can decide
/// that some of them are recoverable.
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
    /// More published points than publish-then-delete can produce.
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

/// A retained-run reachability answer derived from a *complete*, poison-checked
/// resume-point scan.
///
/// Reclamation consumes this type rather than a bare identity set so the
/// "complete proof" precondition is carried by the type system: the only
/// non-test way to obtain one is [`ResumePointSet::reachable_runs`], and a
/// `ResumePointSet` can only be minted by a scan that classified and
/// authenticated every entry it saw.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReachableRetainedRuns(BTreeSet<Uuid>);

impl ReachableRetainedRuns {
    pub(crate) fn contains(&self, run_id: Uuid) -> bool {
        self.0.contains(&run_id)
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Hand-built reachability for reclamation tests. Deliberately test-only:
    /// production callers must derive the set from a complete scan.
    #[cfg(test)]
    pub(crate) fn from_run_ids_for_test(run_ids: impl IntoIterator<Item = Uuid>) -> Self {
        Self(run_ids.into_iter().collect())
    }
}

/// The complete validated resume-point set of one endpoint.
///
/// Minted only by a full, poison-checked directory scan, so
/// [`Self::reachable_runs`] is a *total* reachability answer and never a
/// partial one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResumePointSet {
    /// Ascending by `resume_sequence`.
    points: Vec<RuntimeResumePointV1>,
}

impl ResumePointSet {
    /// The set of an endpoint that has never published a resume point. This is
    /// the ordinary, non-error shape for an absent directory.
    pub(crate) const fn empty() -> Self {
        Self { points: Vec::new() }
    }

    /// Read and validate every entry of one resume-point directory.
    ///
    /// Any deviation aborts the whole scan: an unknown or non-regular entry, a
    /// name that is not a canonical fixed-width sequence, bytes that exceed the
    /// ceiling or fail to decode canonically, a payload digest that does not
    /// cover the bytes, a payload sequence that disagrees with its file name,
    /// or more points than publish-then-delete can produce. A caller that gets
    /// an `Err` here has proved nothing about reachability and must preserve
    /// every candidate retained run.
    pub(crate) fn read(dir: &Dir) -> Result<Self, ResumePointError> {
        let mut points = Vec::new();
        for entry in dir.entries()? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| {
                    ResumePointError::UnexpectedEntry("non-UTF-8 resume-point entry".into())
                })?
                .to_owned();
            if name.starts_with(PUBLICATION_TEMP_PREFIX) {
                // Residue of this repository's own immutable publication: the
                // rename never happened, so no point was ever committed here.
                continue;
            }
            let sequence = parse_resume_point_name(&name)
                .ok_or_else(|| ResumePointError::UnexpectedEntry(name.clone()))?;
            require_regular_point_entry(&entry, &name)?;
            let bytes = read_optional_regular(dir, &name, MAX_RESUME_POINT_BYTES, None)?
                .ok_or_else(|| ResumePointError::UnexpectedEntry(name.clone()))?;
            let point = RuntimeResumePointV1::decode(&bytes)?;
            if point.resume_sequence != sequence {
                return Err(ResumePointError::NameMismatch {
                    named: sequence,
                    payload: point.resume_sequence,
                });
            }
            points.push(point);
        }
        points.sort_by_key(|point| point.resume_sequence);
        if points.len() > MAX_RETAINED_RESUME_POINTS {
            return Err(ResumePointError::TooManyPoints(points.len()));
        }
        Ok(Self { points })
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
        match self.latest() {
            None => Ok(1),
            Some(latest) => {
                latest
                    .resume_sequence
                    .checked_add(1)
                    .ok_or(ResumePointError::Malformed(
                        "resume sequence space is exhausted",
                    ))
            }
        }
    }

    /// Every retained scratch run this complete set still reaches.
    pub(crate) fn reachable_runs(&self) -> ReachableRetainedRuns {
        ReachableRetainedRuns(
            self.points
                .iter()
                .map(|point| point.scratch_run_id)
                .collect(),
        )
    }

    /// Remove every published point below `keep`, then make the removal
    /// durable.
    ///
    /// The directory is rescanned under the same poison rule first, so a pass
    /// that cannot classify every entry removes nothing at all.
    pub(crate) fn prune_below(dir: &Dir, keep: u64) -> Result<usize, ResumePointError> {
        Self::remove_matching(dir, |point| point.resume_sequence < keep)
    }

    /// Remove every published point, then make the removal durable.
    ///
    /// This is the `Unsafe -> Safe` drain: afterwards no retained run is
    /// reachable, which is exactly what lets reclamation collect it. The same
    /// poison rule applies, so a directory this pass cannot fully classify
    /// fails the drain instead of silently half-clearing it.
    pub(crate) fn clear(dir: &Dir) -> Result<usize, ResumePointError> {
        Self::remove_matching(dir, |_| true)
    }

    fn remove_matching(
        dir: &Dir,
        select: impl Fn(&RuntimeResumePointV1) -> bool,
    ) -> Result<usize, ResumePointError> {
        let set = Self::read(dir)?;
        let mut removed = 0;
        for point in set.points.iter().filter(|point| select(point)) {
            dir.remove_file(point.file_name())?;
            removed += 1;
        }
        if removed > 0 {
            sync_dir_required(dir)?;
        }
        Ok(removed)
    }
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
    fn more_than_two_points_is_fail_closed() {
        let root = resume_root("too-many");
        let dir = open_root(&root);
        for sequence in 1..=(MAX_RETAINED_RESUME_POINTS as u64 + 1) {
            publish(&root, &point(sequence));
        }
        assert_eq!(
            ResumePointSet::read(&dir),
            Err(ResumePointError::TooManyPoints(
                MAX_RETAINED_RESUME_POINTS + 1
            ))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_torn_point_poisons_the_scan_and_prunes_nothing() {
        let root = resume_root("torn");
        let dir = open_root(&root);
        let intact = point(1);
        publish(&root, &intact);
        let torn = point(2);
        let torn_bytes = torn.encode().unwrap();
        std::fs::write(
            root.join(torn.file_name()),
            &torn_bytes[..torn_bytes.len() - 3],
        )
        .unwrap();

        assert!(ResumePointSet::read(&dir).is_err());
        assert!(ResumePointSet::prune_below(&dir, 2).is_err());
        assert!(ResumePointSet::clear(&dir).is_err());
        assert_eq!(
            std::fs::read(root.join(intact.file_name())).unwrap(),
            intact.encode().unwrap()
        );
        assert_eq!(
            std::fs::read(root.join(torn.file_name())).unwrap(),
            torn_bytes[..torn_bytes.len() - 3].to_vec()
        );
        std::fs::remove_dir_all(root).unwrap();
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

        assert_eq!(ResumePointSet::prune_below(&dir, 2).unwrap(), 1);
        assert!(!root.join(older.file_name()).exists());
        assert_eq!(ResumePointSet::read(&dir).unwrap().points(), &[newer]);
        // Idempotent: a repeated prune at the same watermark removes nothing.
        assert_eq!(ResumePointSet::prune_below(&dir, 2).unwrap(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clear_removes_every_point() {
        let root = resume_root("clear");
        let dir = open_root(&root);
        publish(&root, &point(1));
        publish(&root, &point(2));
        assert_eq!(ResumePointSet::clear(&dir).unwrap(), 2);
        assert!(ResumePointSet::read(&dir).unwrap().points().is_empty());
        assert_eq!(ResumePointSet::clear(&dir).unwrap(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
