//! Convergent Direct Files cross-page moves (packet B2; invariants I-2, I-3, I-4).
//!
//! **The problem.** A Direct cross-page move writes `N + 1` files: the
//! destination (which GAINS the blocks) first, then the `N` sources (which LOSE
//! them). Ordering keeps the damage one-sided — a removal never lands before the
//! addition — but the process may die between any two of those writes (I-2), and
//! then the graph is left divergent: the blocks are present in the destination
//! AND still present in a source, silently, with nothing on disk saying a move
//! was in progress.
//!
//! Two or more `rename()`s cannot be made atomic, so the contract is not
//! atomicity but **convergence**: a durable record, written before the first
//! page write and retired after the last, that lets the next open either
//! COMPLETE the move or ROLL IT BACK — never leave a half-move.
//!
//! **The record binds enough to do both from the record alone.** For the
//! destination and every source it carries the graph-relative path, the page
//! identity, the base revision, the exact preimage bytes and the exact proposed
//! postimage bytes (as content-addressed blobs written and fsynced BEFORE the
//! record commits). Recovery therefore never has to reconstruct anything or
//! consult a parser.
//!
//! **Where it lives.** In the Tauri app-private, graph-keyed root — NEVER inside
//! the graph tree. The graph directory is Logseq-shared surface and a sync
//! transport carries it; device-private recovery state must not travel
//! (`docs/storage-sync-contract.md`, and `docs/contracts/direct-move-recovery.md`
//! for this record's own contract). `Graph` never holds that root: it is passed
//! in per call by the layer that owns it, because recovery must run BEFORE
//! `Graph::open_checked` parses anything.
//!
//! **Markdown/Org stays authoritative.** Every participant is classified by
//! comparing its CURRENT on-disk bytes against the two recorded images. Bytes
//! that match neither — an external editor, a second honest instance, a sync
//! delivery — mean recovery must not touch that move at all: the record is
//! quarantined with both versions preserved (the file untouched on disk, the
//! recorded images retained in the store) and Tine's ordinary conflict machinery
//! surfaces the divergence on the next save of that page. This byte comparison
//! is STRICTLY STRONGER than the base-revision guard it stands in for: the guard
//! asks "is the file still the revision the editor loaded", and this asks "is the
//! file still exactly one of the two byte strings this move accounted for".
//!
//! Refusal scenarios for every fail-closed path here are tabulated in
//! `docs/contracts/direct-move-recovery.md` §4 (I-8).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The record schema. Managed Storage is blank-slate until 0.7 and so is this:
/// there is exactly ONE current format, no dual readers and no migration. A
/// record whose schema is not this one is unrecognized private state — it is
/// preserved as quarantine and the graph is rebuilt from the files (I-7).
pub const RECORD_SCHEMA: u32 = 1;

/// Directory names under the graph-keyed recovery root.
const RECORDS_DIR: &str = "records";
const BLOBS_DIR: &str = "blobs";
const QUARANTINE_DIR: &str = "quarantine";

/// Bounded cleanup (contract §5). At most this many quarantined records are
/// retained; the oldest are dropped first. A quarantine entry is diagnostic —
/// the user's bytes are on disk, untouched — so an unbounded pile would be
/// device-private garbage, not safety.
pub const QUARANTINE_RETENTION: usize = 32;

/// A blob is unreachable once no live record (pending or quarantined) names it.
/// Reclaimed on the same bounded sweep.
///
/// Which side of the move a participant is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantRole {
    /// The page that GAINS the blocks. Written first.
    Destination,
    /// A page that LOSES blocks. Written only after the destination is durable.
    Source,
}

/// One of a participant's two byte images. `Absent` is a real state: a move may
/// create the destination file, and rolling that back means removing it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageRef {
    Absent,
    Blob { sha256: String, len: u64 },
}

impl ImageRef {
    pub fn blob_of(bytes: &[u8]) -> ImageRef {
        ImageRef::Blob {
            sha256: hex_digest(bytes),
            len: bytes.len() as u64,
        }
    }

    fn blob_name(&self) -> Option<&str> {
        match self {
            ImageRef::Absent => None,
            ImageRef::Blob { sha256, .. } => Some(sha256.as_str()),
        }
    }
}

/// One page taking part in a move.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveParticipant {
    pub role: ParticipantRole,
    /// Graph-root-relative, forward-slashed — the same shape `PageDto.path` uses.
    pub relative_path: String,
    /// Page identity as the editor understood it, for diagnostics and for the
    /// conflict surface a quarantine hands to the user.
    pub page_name: String,
    pub page_kind: String,
    /// The revision the editor held for this page when the move was composed
    /// (`content_rev` of the preimage), or `None` when the file did not exist.
    pub base_revision: Option<String>,
    pub preimage: ImageRef,
    pub postimage: ImageRef,
}

/// The durable recovery record for one Direct cross-page move.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectMoveRecord {
    pub schema: u32,
    pub move_id: String,
    /// The graph this move belongs to, as the app resolved it. Informational:
    /// the store is already graph-keyed, and recovery is handed the root.
    pub graph_root: String,
    pub created_unix_ms: u64,
    pub participants: Vec<MoveParticipant>,
}

impl DirectMoveRecord {
    pub fn destination(&self) -> Option<&MoveParticipant> {
        self.participants
            .iter()
            .find(|participant| participant.role == ParticipantRole::Destination)
    }

    /// Structural validation of a record read back from private state. A record
    /// that fails this is not trusted to name a file, let alone write one.
    fn validate(&self) -> Result<(), String> {
        if self.schema != RECORD_SCHEMA {
            return Err(format!(
                "unrecognized record schema {} (this build writes {RECORD_SCHEMA})",
                self.schema
            ));
        }
        if self.move_id.is_empty() || !move_id_is_portable(&self.move_id) {
            return Err("record identifier is not a portable name".to_string());
        }
        let destinations = self
            .participants
            .iter()
            .filter(|participant| participant.role == ParticipantRole::Destination)
            .count();
        if destinations != 1 {
            return Err(format!("record names {destinations} destinations, expected 1"));
        }
        if self.participants.len() < 2 {
            return Err("record names fewer than two participants".to_string());
        }
        let mut seen = BTreeSet::new();
        for participant in &self.participants {
            if !relative_path_is_contained(&participant.relative_path) {
                return Err(format!(
                    "participant path escapes the graph root: {}",
                    participant.relative_path
                ));
            }
            if !seen.insert(participant.relative_path.as_str()) {
                return Err(format!(
                    "participant path appears twice: {}",
                    participant.relative_path
                ));
            }
            for image in [&participant.preimage, &participant.postimage] {
                if let ImageRef::Blob { sha256, .. } = image {
                    if !is_hex_digest(sha256) {
                        return Err("participant image names a non-digest blob".to_string());
                    }
                }
            }
        }
        Ok(())
    }
}

/// The canonical ordered list of a Direct cross-page move's DURABLE steps.
///
/// This is the sequence the crash matrix cuts between, and the sequence
/// `src/store.ts` (`persistCrossPage`) and `src/carry.ts` emit — pinned on the
/// frontend side by `src/directMoveOrder.test.ts` and on this side by
/// `direct_move_durable_steps_match_the_contract`. It is written down because a
/// convergence proof is only as good as its claim about which cuts exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurableStep {
    /// Blobs fsynced, then the record atomically published. Before this, a crash
    /// leaves the graph exactly as it was.
    CommitRecord,
    /// The destination page's audited save (`Graph::save_page`).
    WriteDestination,
    /// One source page's audited save, in record order.
    WriteSource(usize),
    /// The record is removed once every participant is durably terminal.
    RetireRecord,
}

pub fn direct_move_durable_steps(record: &DirectMoveRecord) -> Vec<DurableStep> {
    let sources = record
        .participants
        .iter()
        .filter(|participant| participant.role == ParticipantRole::Source)
        .count();
    let mut steps = vec![DurableStep::CommitRecord, DurableStep::WriteDestination];
    steps.extend((0..sources).map(DurableStep::WriteSource));
    steps.push(DurableStep::RetireRecord);
    steps
}

/// A composed, not-yet-published move record together with every image byte
/// string it references. Produced by `Graph::prepare_direct_cross_page_move`,
/// consumed by `RecoveryStore::commit`.
#[derive(Clone, Debug)]
pub struct PreparedDirectMove {
    pub record: DirectMoveRecord,
    pub images: BTreeMap<String, Vec<u8>>,
}

/// A fresh record identifier. Portable alphabet only — it names a file.
pub fn new_move_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// The app-private, graph-keyed recovery store.
///
/// Every write here goes through `model::atomic_write` — the named audited
/// protocol (temp → fsync → atomic rename → directory barrier), the same one the
/// graph's own page writes use (I-1).
#[derive(Clone, Debug)]
pub struct RecoveryStore {
    root: PathBuf,
}

impl RecoveryStore {
    pub fn new(root: impl Into<PathBuf>) -> RecoveryStore {
        RecoveryStore { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn records_dir(&self) -> PathBuf {
        self.root.join(RECORDS_DIR)
    }

    fn blobs_dir(&self) -> PathBuf {
        self.root.join(BLOBS_DIR)
    }

    fn quarantine_dir(&self) -> PathBuf {
        self.root.join(QUARANTINE_DIR)
    }

    fn record_path(&self, move_id: &str) -> PathBuf {
        self.records_dir().join(format!("{move_id}.json"))
    }

    /// Publish one record durably.
    ///
    /// ORDER IS THE CONTRACT: every referenced blob is written and fsynced (and
    /// its directory barriered) BEFORE the record itself is published, so a
    /// crash can never leave a live record pointing at bytes that are not on
    /// stable storage. A crash before the record's own rename leaves orphan
    /// blobs, which the bounded sweep reclaims — the harmless direction.
    pub fn commit(
        &self,
        record: &DirectMoveRecord,
        images: &BTreeMap<String, Vec<u8>>,
    ) -> io::Result<()> {
        record
            .validate()
            .map_err(|reason| io::Error::new(io::ErrorKind::InvalidInput, reason))?;
        fs::create_dir_all(self.blobs_dir())?;
        fs::create_dir_all(self.records_dir())?;
        for participant in &record.participants {
            for image in [&participant.preimage, &participant.postimage] {
                let Some(name) = image.blob_name() else { continue };
                let bytes = images.get(name).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "record references an image the caller did not supply",
                    )
                })?;
                if hex_digest(bytes) != name {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "supplied image does not hash to the name the record uses",
                    ));
                }
                let path = self.blobs_dir().join(name);
                if path.exists() {
                    continue; // content-addressed: identical bytes, already durable
                }
                crate::model::atomic_write(&path, bytes)?;
            }
        }
        let encoded = serde_json::to_vec_pretty(record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        crate::model::atomic_write(&self.record_path(&record.move_id), &encoded)
    }

    /// Retire a record. Called only once every participant is durably terminal.
    pub fn retire(&self, move_id: &str) -> io::Result<()> {
        if !move_id_is_portable(move_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "record identifier is not a portable name",
            ));
        }
        let path = self.record_path(move_id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        // The removal must be durable for the same reason the publication was:
        // a crash right after an un-barriered unlink can resurrect the record,
        // and recovery would then re-apply a move whose participants have since
        // moved on.
        crate::filesystem_durability::sync_reconstructible_directory_path(&self.records_dir())
    }

    pub fn read_blob(&self, name: &str) -> io::Result<Vec<u8>> {
        if !is_hex_digest(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "blob name is not a digest",
            ));
        }
        let bytes = fs::read(self.blobs_dir().join(name))?;
        if hex_digest(&bytes) != name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recovery blob does not hash to its own name",
            ));
        }
        Ok(bytes)
    }

    fn image_bytes(&self, image: &ImageRef) -> io::Result<Option<Vec<u8>>> {
        match image {
            ImageRef::Absent => Ok(None),
            ImageRef::Blob { sha256, len } => {
                let bytes = self.read_blob(sha256)?;
                if bytes.len() as u64 != *len {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "recovery blob length disagrees with the record",
                    ));
                }
                Ok(Some(bytes))
            }
        }
    }

    /// Every pending record, oldest first. A file that will not decode or will
    /// not validate is moved straight to quarantine: unrecognized private state
    /// is preserved, never acted on and never allowed to block the open (I-7).
    pub fn pending(&self) -> Vec<DirectMoveRecord> {
        let Ok(entries) = fs::read_dir(self.records_dir()) else {
            return Vec::new();
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<DirectMoveRecord>(&bytes).ok())
            {
                Some(record) if record.validate().is_ok() => records.push(record),
                _ => {
                    let name = path.file_name().map(|name| name.to_os_string());
                    if let Some(name) = name {
                        let _ = fs::create_dir_all(self.quarantine_dir());
                        let _ = fs::rename(&path, self.quarantine_dir().join(name));
                    }
                }
            }
        }
        records.sort_by(|left, right| {
            (left.created_unix_ms, left.move_id.as_str())
                .cmp(&(right.created_unix_ms, right.move_id.as_str()))
        });
        records
    }

    /// Preserve a record we must not act on, with the reason. The graph files
    /// themselves are left exactly as they are — that is the point.
    fn quarantine(&self, record: &DirectMoveRecord, reason: &str) -> io::Result<()> {
        fs::create_dir_all(self.quarantine_dir())?;
        let mut value = serde_json::to_value(record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "quarantine_reason".to_string(),
                serde_json::Value::String(reason.to_string()),
            );
        }
        let encoded = serde_json::to_vec_pretty(&value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        crate::model::atomic_write(
            &self.quarantine_dir().join(format!("{}.json", record.move_id)),
            &encoded,
        )?;
        self.retire(&record.move_id)
    }

    /// Bounded cleanup (contract §5): keep at most `QUARANTINE_RETENTION`
    /// quarantined records, then reclaim every blob no live record names.
    pub fn sweep(&self) {
        let mut quarantined: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(
            self.quarantine_dir(),
        )
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            path.is_file().then_some((modified, path))
        })
        .collect();
        quarantined.sort();
        while quarantined.len() > QUARANTINE_RETENTION {
            let (_, path) = quarantined.remove(0);
            let _ = fs::remove_file(path);
        }

        let mut referenced = BTreeSet::new();
        let live = fs::read_dir(self.records_dir())
            .into_iter()
            .flatten()
            .chain(fs::read_dir(self.quarantine_dir()).into_iter().flatten());
        for entry in live.flatten() {
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<DirectMoveRecord>(&bytes) else {
                continue;
            };
            for participant in &record.participants {
                for image in [&participant.preimage, &participant.postimage] {
                    if let Some(name) = image.blob_name() {
                        referenced.insert(name.to_string());
                    }
                }
            }
        }
        for entry in fs::read_dir(self.blobs_dir()).into_iter().flatten().flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !referenced.contains(&name) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// Where one participant's file currently stands relative to the recorded move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParticipantState {
    /// The file still holds the preimage: this page's write has not landed.
    Pending,
    /// The file holds the postimage: this page's write is durably terminal.
    /// Checked first, so a page whose two images are identical (a no-op save)
    /// is terminal in both directions, which it genuinely is.
    Completed,
    /// The file holds neither image. Markdown/Org is authoritative and somebody
    /// else wrote it; recovery must not touch this move.
    Diverged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Every participant was already terminal — the move had landed in full and
    /// only the record survived the crash. No graph bytes written.
    AlreadyComplete,
    /// Nothing had landed yet. No graph bytes written.
    NothingApplied,
    /// The destination was durable, so the move was carried forward: the listed
    /// sources had their removals published.
    Completed { pages_written: usize },
    /// The destination was NOT durable, so the move was undone: the listed
    /// participants were restored to their preimages. This is the direction
    /// that cannot lose blocks — a source restored to its preimage still
    /// contains them.
    RolledBack { pages_written: usize },
    /// A participant's bytes matched neither image. Nothing was written; both
    /// versions are preserved (the file on disk, the images in the store) and
    /// the ordinary conflict machinery surfaces it on the next save.
    Quarantined { reason: String },
    /// Recovery could not read or write what it needed. The record is LEFT in
    /// place so the next open tries again — a transient disk error must not
    /// silently discard a pending move (I-10: the state has a way out).
    Failed { error: String },
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryReport {
    pub outcomes: Vec<(String, RecordOutcome)>,
}

impl RecoveryReport {
    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }

    /// One privacy-safe line per record for the always-on diagnostic record
    /// (I-5, I-9): outcome family and counts only — never a page name or path.
    pub fn summary(&self) -> String {
        let mut complete = 0usize;
        let mut nothing = 0usize;
        let mut completed = 0usize;
        let mut rolled_back = 0usize;
        let mut quarantined = 0usize;
        let mut failed = 0usize;
        for (_, outcome) in &self.outcomes {
            match outcome {
                RecordOutcome::AlreadyComplete => complete += 1,
                RecordOutcome::NothingApplied => nothing += 1,
                RecordOutcome::Completed { .. } => completed += 1,
                RecordOutcome::RolledBack { .. } => rolled_back += 1,
                RecordOutcome::Quarantined { .. } => quarantined += 1,
                RecordOutcome::Failed { .. } => failed += 1,
            }
        }
        format!(
            "direct move recovery: records={} already_complete={complete} nothing_applied={nothing} completed={completed} rolled_back={rolled_back} quarantined={quarantined} failed={failed}",
            self.outcomes.len()
        )
    }
}

/// Complete or roll back every pending Direct move for `graph_root`.
///
/// MUST run before any page content is served — see the module header and
/// `docs/contracts/direct-move-recovery.md` §3. It never opens a `Graph`: the
/// only authority it needs is byte equality against the record's own images.
pub fn recover_all(store_root: &Path, graph_root: &Path) -> RecoveryReport {
    let store = RecoveryStore::new(store_root);
    let mut report = RecoveryReport::default();
    for record in store.pending() {
        let outcome = recover_one(&store, graph_root, &record);
        match &outcome {
            RecordOutcome::Quarantined { reason } => {
                let _ = store.quarantine(&record, reason);
            }
            RecordOutcome::Failed { .. } => {}
            _ => {
                let _ = store.retire(&record.move_id);
            }
        }
        report.outcomes.push((record.move_id.clone(), outcome));
    }
    store.sweep();
    report
}

/// Retire one record if — and only if — every participant is durably terminal.
///
/// Called by the frontend at the end of a successful move. It never writes a
/// graph file: a not-yet-terminal participant means a live save is still in
/// flight or conflicted, and that decision belongs to the user's conflict UI,
/// not to a cleanup call. Whatever it leaves behind, the next open converges.
///
/// A participant whose bytes match neither image IS quarantined here, for the
/// same reason recovery quarantines: the record must not survive as a licence to
/// overwrite somebody else's write later.
pub fn retire_if_terminal(store_root: &Path, graph_root: &Path, move_id: &str) -> bool {
    let store = RecoveryStore::new(store_root);
    let Some(record) = store
        .pending()
        .into_iter()
        .find(|record| record.move_id == move_id)
    else {
        return false;
    };
    // Classify ONLY. `recover_one` writes, and calling it here would force a move
    // forward whose source save is sitting behind a live conflict banner — which
    // is precisely the decision that belongs to the user, not to a cleanup call.
    match classify(&store, graph_root, &record) {
        Err(outcome) => {
            if let RecordOutcome::Quarantined { reason } = &outcome {
                let _ = store.quarantine(&record, reason);
            }
            false
        }
        Ok(states) => {
            let terminal = states
                .iter()
                .all(|(_, state, _, _)| *state == ParticipantState::Completed);
            terminal && store.retire(&record.move_id).is_ok()
        }
    }
}

/// Read every participant's current bytes and place it against the two recorded
/// images. `Err` is an outcome that needs no further decision (a divergence, or
/// an unreadable image); `Ok` hands back the states for the caller to act on.
type ParticipantStates<'a> = Vec<(
    &'a MoveParticipant,
    ParticipantState,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
)>;

fn classify<'a>(
    store: &RecoveryStore,
    graph_root: &Path,
    record: &'a DirectMoveRecord,
) -> Result<ParticipantStates<'a>, RecordOutcome> {
    let mut states: ParticipantStates<'a> = Vec::with_capacity(record.participants.len());
    for participant in &record.participants {
        let preimage = store
            .image_bytes(&participant.preimage)
            .map_err(|error| RecordOutcome::Failed { error: error.to_string() })?;
        let postimage = store
            .image_bytes(&participant.postimage)
            .map_err(|error| RecordOutcome::Failed { error: error.to_string() })?;
        let path = graph_root.join(
            participant
                .relative_path
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        let current = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(RecordOutcome::Failed { error: error.to_string() }),
        };
        let state = if current.as_deref() == postimage.as_deref() {
            ParticipantState::Completed
        } else if current.as_deref() == preimage.as_deref() {
            ParticipantState::Pending
        } else {
            // Deliberately does NOT name the page: this reason is retained in
            // app-private state and echoed into the always-on diagnostic record.
            return Err(RecordOutcome::Quarantined {
                reason: format!(
                    "a {} page's bytes match neither the recorded preimage nor the recorded postimage",
                    match participant.role {
                        ParticipantRole::Destination => "destination",
                        ParticipantRole::Source => "source",
                    }
                ),
            });
        };
        states.push((participant, state, preimage, postimage));
    }
    Ok(states)
}

fn recover_one(
    store: &RecoveryStore,
    graph_root: &Path,
    record: &DirectMoveRecord,
) -> RecordOutcome {
    let states = match classify(store, graph_root, record) {
        Ok(states) => states,
        Err(outcome) => return outcome,
    };

    if states
        .iter()
        .all(|(_, state, _, _)| *state == ParticipantState::Completed)
    {
        return RecordOutcome::AlreadyComplete;
    }
    if states
        .iter()
        .all(|(_, state, _, _)| *state == ParticipantState::Pending)
    {
        return RecordOutcome::NothingApplied;
    }

    let Some((_, destination_state, _, _)) = states
        .iter()
        .find(|(participant, _, _, _)| participant.role == ParticipantRole::Destination)
    else {
        return RecordOutcome::Quarantined {
            reason: "record names no destination".to_string(),
        };
    };

    // The one decision. The destination is the ADDITION side and is written
    // first, so:
    //   destination durable  -> the additions exist; carry the removals forward.
    //   destination pending  -> the additions do NOT exist; undo any removal,
    //                           because a removal without its addition is the
    //                           only state that loses the user's blocks.
    let forward = *destination_state == ParticipantState::Completed;
    let mut written = 0usize;
    for (participant, state, preimage, postimage) in &states {
        let target = match (forward, state) {
            (true, ParticipantState::Pending) => postimage,
            (false, ParticipantState::Completed) => preimage,
            _ => continue,
        };
        let path = graph_root.join(
            participant
                .relative_path
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        if let Err(error) = apply_image(&path, target.as_deref()) {
            return RecordOutcome::Failed { error: error.to_string() };
        }
        written += 1;
    }
    if forward {
        RecordOutcome::Completed { pages_written: written }
    } else {
        RecordOutcome::RolledBack { pages_written: written }
    }
}

/// Publish one participant's terminal bytes.
///
/// `Some(bytes)` goes through `model::atomic_write` — the named audited protocol
/// (temp → fsync → rename → directory barrier). `None` means the file must not
/// exist (rolling back a destination this move created); the unlink is followed
/// by the same directory barrier, because an un-barriered unlink can be undone
/// by a crash and would resurrect a file the user's graph never had.
fn apply_image(path: &Path, bytes: Option<&[u8]>) -> io::Result<()> {
    match bytes {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            crate::model::atomic_write(path, bytes)
        }
        None => {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            crate::filesystem_durability::sync_reconstructible_directory_path(parent)
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Record identifiers name a file in app-private state, so they are restricted
/// to a portable, traversal-free alphabet rather than trusted.
fn move_id_is_portable(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// A participant path must stay inside the graph root. In-scope scenario: the
/// record is app-private state that a crash, a torn write or a disk error can
/// leave malformed; a path with `..` in it would then let recovery write outside
/// the user's graph. (This is not a defence against an attacker with arbitrary
/// write access to the user's account — see the 2026-08-07 trust boundary.)
fn relative_path_is_contained(value: &str) -> bool {
    if value.is_empty() || value.contains('\0') || value.contains('\\') {
        return false;
    }
    let path = Path::new(value);
    path.is_relative()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

pub fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "direct_move_recovery_tests.rs"]
mod tests;
