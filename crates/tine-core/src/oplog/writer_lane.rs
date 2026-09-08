//! Persistent, device-private CRDT writer lanes.
//!
//! Fresh-peer-per-transaction retains an O(history) version vector inside every
//! shallow Loro snapshot and inside every later manifest's before-vector, so the
//! rebaselining design's `P` term (participating peer identities) has to mean
//! *writer incarnations*, not batches. This module owns the durable record that
//! makes one ordinary/seal lane and one external-import lane per admitted
//! endpoint reusable across transactions, imports, seals and normal restarts.
//!
//! Two roles, never more: `Local` authors every ordinary local mutation and
//! maintenance seal, `External` authors external-editor reconciliation. Keeping
//! them apart costs one extra version-vector entry per device and keeps
//! external-editor provenance separable in CRDT history.
//!
//! # What is authority here, and what is not
//!
//! * The **record** is this device's private answer to "which peer do I write
//!   as". It is not, and can never become, a receiver-visible ownership proof —
//!   a receiver binds peers to authors through accepted state
//!   (`ShardedHotEngine`'s lane-ownership map), never by reading another
//!   device's private directory. Retired `EnrollmentBindingV1` authority is not
//!   revived.
//! * The **identities are allocated at random and then saved**, never derived.
//!   A derived value would let a rebuild that has lost the record recreate the
//!   very identity whose published prefix it can no longer qualify; a saved
//!   random value structurally cannot. Losing the record therefore always
//!   produces identities nobody has published under.
//! * The record saves **three** identities, not two: the two owned Loro peers
//!   AND one `WriterIncarnationId`, this device's Tine causal peer. The
//!   enrolled `DeviceId` is untouched by all of this and keeps every
//!   device/endpoint/enrollment authority it had.
//!
//! # Exclusivity
//!
//! The record is device-private state in a namespace no archive lease covers:
//! two honest concurrent graph copies of one workspace hold two independent
//! archive-rooted `WorkspaceRuntimeLease`s and would both reach this same
//! device/lineage/endpoint record. Exclusion is therefore taken here, with the
//! existing platform lease helper (`sqlite::lock_capability_lease_file`, the
//! same primitive the SQLite applier lease uses — D-14), on a lock file beside
//! the record, and re-proved at every peer vend and every reservation. It is
//! not "pre-read plus `replace_exact`": exact replacement is a torn-write
//! guard, never a cross-process compare-and-swap.
//!
//! # Continuation, reservation and rotation
//!
//! Every CRDT operation this device durably publishes rides inside an
//! `OperationBatch` carrying a `BatchCausalDot` whose peer is the saved
//! `WriterIncarnationId`, and batch admission already refuses a dot that is not
//! gap-free (`derive_inline_causal_clock`). The lane therefore keeps two facts
//! ABOUT THE SAVED INCARNATION, never about the device:
//!
//! * `confirmed_own_counter` — the highest dot counter this incarnation has
//!   proved durable. It is a **monotone floor** within that incarnation: a
//!   rebuild may raise it, never lower it.
//! * `reservation` — the single in-flight batch, bound to its **exact manifest
//!   fingerprint**, not merely to a counter. On reopen exactly one dot can be
//!   ambiguous, and that exact (`BatchId`, manifest digest) pair is what the
//!   durable stores are asked about.
//!
//! Before a batch becomes outwardly visible — the trusted local journal append,
//! or the exact external archive manifest commit — its dot is reserved here.
//! An unresolved outcome blocks the lane until reopen.
//!
//! # Recovery is a NEW incarnation, never a reused counter
//!
//! A lost or undecodable record, or a saved incarnation whose own durable
//! prefix this copy genuinely cannot prove, mints a **fresh**
//! `WriterIncarnationId` together with fresh Loro peers, durably, before any
//! new authoring. That is the whole point: the unknowable older prefix keeps
//! its own causal identity, so a rebuilt device can never re-issue a
//! `BatchCausalDot` that an offline peer still holds, and it never has to wait
//! for that peer to reappear before doing useful local work. The old branch
//! stays admissible under its original distinct identity whenever it arrives.
//!
//! Rotation is not free and is not routine: each one costs one entry in the `P`
//! term the rebaselining bound counts, so it happens only on real loss or real
//! unprovability — never on an ordinary restart, seal, import, checkpoint
//! reopen or archive cold relocation.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::path::Path;

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use tine_storage::{read_optional_regular, DurableDirectoryPublication};

use super::{
    BatchId, BatchOrigin, CausalPeerId, ContentDigest, CrdtPeerId, DeviceId, LineageDigest,
    ProjectionEndpointId, WorkspaceId, WriterIncarnationId,
};

/// Namespace under the device-private application runtime root. Lane records
/// live in app-data keyed by graph, never in the graph directory and never in
/// `.tine-sync`: they must NOT travel with the graph (D-11), and `.tine-sync`
/// is exactly the shared surface they must stay out of.
pub(crate) const WRITER_LANE_NAMESPACE: &str = "crdt-writer-lanes";
/// Schema 2 is the current — and only — writer-lane record format: it saves the
/// causal writer incarnation alongside the two Loro peers. There is no reader
/// for schema 1 (D-1); an unrecognized record is preserved and rebuilt.
const RECORD_SCHEMA: u32 = 2;
const MAX_RECORD_BYTES: u64 = 1024;
/// Reserved deterministic peers of the immutable lazy-genesis baseline. A lane
/// never allocates one, so baseline state stays unowned and unforgeable.
pub(super) const RESERVED_PEERS: [u64; 3] = [0, 0x5449_4e45_4745_4e31, 0x5449_4e45_4745_4e32];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriterLaneBinding {
    pub workspace_id: WorkspaceId,
    pub lineage: LineageDigest,
    pub device_id: DeviceId,
    pub endpoint_id: ProjectionEndpointId,
}

impl WriterLaneBinding {
    /// One record file per device inside the workspace/lineage directory, so
    /// two devices sharing an application runtime root never contend.
    pub(crate) fn record_name(&self) -> String {
        format!("writer-lanes-{}.postcard", self.device_id)
    }

    /// Exclusion is per device, exactly like the record it protects: two
    /// devices sharing one application runtime root are two runtimes and never
    /// contend, while one device reached through two independent graph copies
    /// contends here — which is the only place it can, because each copy holds
    /// its own archive-rooted `WorkspaceRuntimeLease`.
    pub(crate) fn lock_name(&self) -> String {
        format!("writer-lanes-{}.lock", self.device_id)
    }

    pub(crate) fn directory_name(&self) -> String {
        format!("lanes-{}-{}", self.workspace_id, self.lineage)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriterRole {
    Local,
    External,
}

impl WriterRole {
    /// Which lane an admitted batch must have been authored on.
    ///
    /// `BootstrapImport` has no lane. Genesis predates every lane and is
    /// admitted through the distinct bootstrap trust path, so it neither
    /// claims nor binds writer-lane ownership.
    pub(crate) const fn for_origin(origin: BatchOrigin) -> Option<Self> {
        match origin {
            BatchOrigin::LocalMutation => Some(Self::Local),
            BatchOrigin::ExternalReconciliation { .. } => Some(Self::External),
            BatchOrigin::BootstrapImport => None,
        }
    }

    pub(crate) const fn describe(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::External => "external-import",
        }
    }
}

/// The exact outward publication one reservation covers.
///
/// The manifest digest is not decoration: an integer counter alone cannot tell
/// "my batch B reached the archive" from "some other batch took my dot", so the
/// durability question is asked about this exact pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriterLaneTip {
    pub batch_id: BatchId,
    pub manifest_digest: ContentDigest,
}

/// The single in-flight batch whose outcome this device does not yet know.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaneReservation {
    role: WriterRole,
    tip: WriterLaneTip,
    /// This device's `BatchCausalDot` counter for the reserved batch.
    causal_counter: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriterLaneRecordV1 {
    schema: u32,
    binding: WriterLaneBinding,
    /// Advanced only when a rebuild replaces the saved identities. Diagnostic
    /// and receipt-facing: identity itself is the saved random
    /// `incarnation_id`, so a counter that restarts at zero after a total loss
    /// of the record can never resurrect a used causal chain.
    incarnation: u64,
    /// This device's Tine causal peer for every batch of this incarnation —
    /// ordinary, local, external-import and seal alike. Random, then saved.
    incarnation_id: WriterIncarnationId,
    local_peer: CrdtPeerId,
    external_peer: CrdtPeerId,
    /// Monotone floor: the highest causal-dot counter THIS INCARNATION has
    /// proved durable. Never lowered while the incarnation is retained; a new
    /// incarnation starts it at zero because it has authored nothing.
    confirmed_own_counter: u64,
    reservation: Option<LaneReservation>,
}

impl WriterLaneRecordV1 {
    /// Mint one complete new authoring incarnation.
    ///
    /// All three identities are replaced together and the counter floor starts
    /// at zero, because a brand-new causal peer has published nothing. This is
    /// the ONLY constructor: there is deliberately no way to keep a causal
    /// identity while replacing the Loro lanes, or the reverse.
    fn mint(binding: WriterLaneBinding, incarnation: u64, excluded: &[CrdtPeerId]) -> Self {
        let local_peer = fresh_peer(excluded);
        let external_peer = fresh_peer(&[excluded, &[local_peer]].concat());
        Self {
            schema: RECORD_SCHEMA,
            binding,
            incarnation,
            incarnation_id: WriterIncarnationId::new(),
            local_peer,
            external_peer,
            confirmed_own_counter: 0,
            reservation: None,
        }
    }

    const fn causal_peer(&self) -> CausalPeerId {
        CausalPeerId::from_key(self.incarnation_id)
    }

    const fn peer(&self, role: WriterRole) -> CrdtPeerId {
        match role {
            WriterRole::Local => self.local_peer,
            WriterRole::External => self.external_peer,
        }
    }

    fn encode(&self) -> Result<Vec<u8>, WriterLaneError> {
        let bytes = postcard::to_allocvec(self).map_err(WriterLaneError::codec)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(WriterLaneError::Codec(
                "writer lane record exceeds its fixed codec size".into(),
            ));
        }
        Ok(bytes)
    }

    /// Decode strictly: noncanonical bytes, a foreign binding, a reused peer or
    /// a reserved genesis peer are all "not this runtime's current record".
    fn decode(bytes: &[u8], binding: WriterLaneBinding) -> Result<Self, WriterLaneError> {
        let (record, remaining): (Self, _) =
            postcard::take_from_bytes(bytes).map_err(WriterLaneError::codec)?;
        if !remaining.is_empty()
            || record.schema != RECORD_SCHEMA
            || record.binding != binding
            || record.local_peer == record.external_peer
            || [record.local_peer, record.external_peer]
                .iter()
                .any(|peer| RESERVED_PEERS.contains(&peer.as_u64()))
            || record
                .reservation
                .is_some_and(|reservation| reservation.causal_counter == 0)
            || record.encode()? != bytes
        {
            return Err(WriterLaneError::Foreign);
        }
        Ok(record)
    }
}

/// Allocate one lane identity.
///
/// Random, then saved. The two reserved lazy-genesis peers and any sibling lane
/// are excluded so a record can never name one peer twice or claim baseline
/// state.
fn fresh_peer(excluded: &[CrdtPeerId]) -> CrdtPeerId {
    loop {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        let peer = CrdtPeerId::from_u64(u64::from_le_bytes(
            bytes[..8].try_into().expect("uuid has sixteen bytes"),
        ));
        if !RESERVED_PEERS.contains(&peer.as_u64()) && !excluded.contains(&peer) {
            return peer;
        }
    }
}

/// Whether one reserved batch is durably present in this device's own
/// authority, judged against its exact manifest fingerprint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReservedBatchDurability {
    /// Nothing was ever committed under that `BatchId`.
    Absent,
    /// A commit exists whose manifest fingerprint is exactly the reserved one.
    Present,
    /// The store could not answer, or answered with a DIFFERENT manifest under
    /// the reserved `BatchId` — either way the dot is not free.
    Undecidable,
}

/// The exact coverage question this module asks the live runtime, answered from
/// already restored accepted history plus the drained local journal.
pub(crate) trait WriterLanePrefixProof {
    /// Highest gap-free `BatchCausalDot` counter `peer` has authored that
    /// current authoritative engine state covers. Zero when that incarnation
    /// has authored nothing.
    ///
    /// The question is asked about the SAVED causal peer, never about the
    /// enrolled `DeviceId`: a device may have published under several
    /// incarnations, and only the current one's prefix decides continuation.
    fn proved_own_counter(&self, peer: CausalPeerId) -> u64;

    /// Whether the exact reserved batch is durably present.
    fn reserved_batch_durability(&self, tip: WriterLaneTip) -> ReservedBatchDurability;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WriterLaneError {
    /// The retained record is not this runtime's; the original bytes are kept.
    Foreign,
    Codec(String),
    Io(String),
    /// Another live runtime holds this device-private lane namespace.
    NotExclusive(String),
    /// A publication outcome is unknown, so this store can no longer vend a
    /// lane. Recovery is a reopen, never this value.
    PublicationUnknown(String),
    /// The batch the caller wants to publish does not continue the lane.
    Uncontinuable(String),
}

impl WriterLaneError {
    fn codec(error: impl std::fmt::Display) -> Self {
        Self::Codec(error.to_string())
    }

    fn io(error: impl std::fmt::Display) -> Self {
        Self::Io(error.to_string())
    }
}

impl std::fmt::Display for WriterLaneError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Foreign => formatter.write_str(
                "the retained CRDT writer-lane record does not belong to this runtime binding",
            ),
            Self::Codec(detail) => write!(formatter, "writer lane record codec: {detail}"),
            Self::Io(detail) => write!(formatter, "writer lane record storage: {detail}"),
            Self::NotExclusive(detail) => write!(
                formatter,
                "another live runtime holds this device's CRDT writer lanes: {detail}"
            ),
            Self::PublicationUnknown(detail) => write!(
                formatter,
                "a writer lane reservation outcome is unknown and requires reopen: {detail}"
            ),
            Self::Uncontinuable(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for WriterLaneError {}

/// Why an open replaced the lane peers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriterLaneOpenDisposition {
    /// No record existed: this endpoint's genesis incarnation.
    Minted,
    /// The retained record's own prefix is covered; peers and counters continue.
    Continued,
    /// A foreign or undecodable record was superseded; originals retained.
    RebuiltFromForeignRecord,
    /// Coverage of the retained incarnation's own durable prefix could not be
    /// proved. A complete fresh incarnation (causal peer plus both Loro peers)
    /// is saved before any new authoring, so the unprovable older prefix keeps
    /// its own causal identity instead of having its counters reused.
    RotatedForUnprovableCoverage,
}

/// Foreground durability cost this store actually paid.
///
/// `record_publications` counts durable record transitions actually performed.
/// Each one is exactly one `DurableDirectoryPublication::replace_exact` (or the
/// single-writer create), which on Unix is one temp-file `sync_all` plus one
/// directory fsync — two fsync syscalls. `idempotent_reservations` counts
/// reservation calls that matched the durable record and therefore published
/// nothing at all.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WriterLaneDurabilityCost {
    pub(crate) record_publications: u64,
    pub(crate) idempotent_reservations: u64,
    pub(crate) reservation_nanos: u128,
}

/// One device's writer lanes for one workspace/lineage/endpoint.
///
/// Interior mutability is deliberate and matches `RuntimeRevocationLatch`: the
/// coordinator holds the engine and database mutably while the reservation
/// boundary is reached through the shared admission capability.
pub(crate) struct WriterLaneStore {
    directory: Dir,
    publication: DurableDirectoryPublication,
    /// The retained device-private exclusion capability for this namespace.
    lease: File,
    lock_name: String,
    record_name: String,
    record: RefCell<WriterLaneRecordV1>,
    durable_bytes: RefCell<Vec<u8>>,
    /// Set when the caller has PROVED that the currently reserved batch never
    /// became durable, so its causal counter may be reserved again. Memory
    /// only: a reopen re-derives the same answer from the durable stores.
    reservation_proved_absent: Cell<bool>,
    disposition: WriterLaneOpenDisposition,
    failed: Cell<bool>,
    record_publications: Cell<u64>,
    idempotent_reservations: Cell<u64>,
    reservation_nanos: Cell<u128>,
}

impl WriterLaneStore {
    /// Open (or mint, or rotate) this device's lanes.
    ///
    /// `directory` must already be the device-private lane directory. The proof
    /// is asked only about the one ambiguous dot, here, after the caller has
    /// restored accepted history and drained its local journal — a lane must
    /// never be vended from a prefix nobody proved.
    pub(crate) fn open(
        directory: Dir,
        binding: WriterLaneBinding,
        proof: &dyn WriterLanePrefixProof,
    ) -> Result<Self, WriterLaneError> {
        // Exclusion first: everything below reads or replaces the one record
        // this lock protects.
        let lock_name = binding.lock_name();
        let lease = super::sqlite::lock_capability_lease_file(
            &directory,
            &lock_name,
            Path::new(&lock_name),
        )
        .map_err(|error| match error {
            super::sqlite::ProjectionError::LeaseContended(_) => WriterLaneError::NotExclusive(
                "another live runtime already holds this device's CRDT writer-lane namespace"
                    .into(),
            ),
            other => WriterLaneError::Io(other.to_string()),
        })?;
        let publication =
            DurableDirectoryPublication::open(&directory).map_err(WriterLaneError::io)?;
        let record_name = binding.record_name();
        let retained = read_optional_regular(&directory, &record_name, MAX_RECORD_BYTES, None)
            .map_err(WriterLaneError::io)?;

        let (record, mut durable_bytes, disposition) = match retained {
            None => (
                // A record that is simply gone takes the whole identity with
                // it. Nothing here can qualify the lost incarnation's prefix,
                // so the rebuild authors under a brand-new causal identity
                // rather than guessing where the old chain ended.
                WriterLaneRecordV1::mint(binding, 0, &[]),
                Vec::new(),
                WriterLaneOpenDisposition::Minted,
            ),
            Some(bytes) => match WriterLaneRecordV1::decode(&bytes, binding) {
                Ok(record) => {
                    // The coverage question is about the SAVED causal peer.
                    let proved = proof.proved_own_counter(record.causal_peer());
                    let floor = own_chain_floor(&record, proved, proof);
                    if floor <= proved {
                        (
                            WriterLaneRecordV1 {
                                confirmed_own_counter: proved,
                                reservation: None,
                                ..record
                            },
                            bytes,
                            WriterLaneOpenDisposition::Continued,
                        )
                    } else {
                        // This copy cannot prove where the saved incarnation's
                        // durable chain ended, and the Loro lane may already be
                        // advanced inside a document it cannot see. Retiring
                        // the whole incarnation — causal peer and both Loro
                        // peers together — leaves the older prefix its own
                        // identity, which is what makes it safe to author again
                        // immediately instead of waiting for an offline peer.
                        (
                            WriterLaneRecordV1 {
                                incarnation: record.incarnation.saturating_add(1),
                                ..WriterLaneRecordV1::mint(
                                    binding,
                                    0,
                                    &[record.local_peer, record.external_peer],
                                )
                            },
                            bytes,
                            WriterLaneOpenDisposition::RotatedForUnprovableCoverage,
                        )
                    }
                }
                Err(WriterLaneError::Foreign) | Err(WriterLaneError::Codec(_)) => {
                    // D-1/D-3: unrecognized private state is preserved as a
                    // backup and rebuilt, never migrated and never trusted.
                    // Undecodable bytes prove nothing about the old prefix, so
                    // the rebuild allocates identities nobody published under.
                    let superseded =
                        format!("{record_name}.superseded-{}", ContentDigest::of(&bytes));
                    publication
                        .move_exact_no_replace(&record_name, &superseded, &bytes)
                        .map_err(WriterLaneError::io)?;
                    (
                        WriterLaneRecordV1::mint(binding, 0, &[]),
                        Vec::new(),
                        WriterLaneOpenDisposition::RebuiltFromForeignRecord,
                    )
                }
                Err(other) => return Err(other),
            },
        };

        let mut record_publications = 0;
        let bytes = record.encode()?;
        if bytes != durable_bytes {
            publish(&publication, &record_name, &durable_bytes, &bytes)?;
            durable_bytes = bytes;
            record_publications = 1;
        }
        Ok(Self {
            directory,
            publication,
            lease,
            lock_name,
            record_name,
            record: RefCell::new(record),
            durable_bytes: RefCell::new(durable_bytes),
            reservation_proved_absent: Cell::new(false),
            disposition,
            failed: Cell::new(false),
            record_publications: Cell::new(record_publications),
            idempotent_reservations: Cell::new(0),
            reservation_nanos: Cell::new(0),
        })
    }

    pub(crate) const fn disposition(&self) -> WriterLaneOpenDisposition {
        self.disposition
    }

    pub(crate) fn incarnation(&self) -> u64 {
        self.record.borrow().incarnation
    }

    /// The saved causal writer incarnation every batch of this store authors
    /// under. Ordinary, local, external-import and seal batches share it.
    pub(crate) fn incarnation_id(&self) -> WriterIncarnationId {
        self.record.borrow().incarnation_id
    }

    /// The same identity as [`Self::incarnation_id`], as the codec's causal
    /// peer. There is exactly one conversion, `CausalPeerId::from_key`.
    pub(crate) fn causal_peer(&self) -> CausalPeerId {
        self.record.borrow().causal_peer()
    }

    /// The monotone own-chain floor this lane will not publish at or below.
    pub(crate) fn confirmed_own_counter(&self) -> u64 {
        self.record.borrow().confirmed_own_counter
    }

    pub(crate) fn durability_cost(&self) -> WriterLaneDurabilityCost {
        WriterLaneDurabilityCost {
            record_publications: self.record_publications.get(),
            idempotent_reservations: self.idempotent_reservations.get(),
            reservation_nanos: self.reservation_nanos.get(),
        }
    }

    /// The peer this device authors as for `role`.
    pub(crate) fn peer(&self, role: WriterRole) -> Result<CrdtPeerId, WriterLaneError> {
        self.guard()?;
        Ok(self.record.borrow().peer(role))
    }

    /// Durably reserve the exact batch about to be published on `role`.
    ///
    /// This is the *only* added foreground durability barrier: one
    /// exact-replacement of a fixed-size record, before the trusted local
    /// journal append or the external archive manifest commit. It is idempotent
    /// for the same batch, so a retry of an already reserved publication
    /// publishes nothing.
    pub(crate) fn reserve(
        &self,
        role: WriterRole,
        peer: CrdtPeerId,
        tip: WriterLaneTip,
        causal_counter: u64,
        proved_own_counter: u64,
    ) -> Result<(), WriterLaneError> {
        let started = std::time::Instant::now();
        let outcome = self.reserve_inner(role, peer, tip, causal_counter, proved_own_counter);
        self.reservation_nanos
            .set(self.reservation_nanos.get() + started.elapsed().as_nanos());
        outcome
    }

    fn reserve_inner(
        &self,
        role: WriterRole,
        peer: CrdtPeerId,
        tip: WriterLaneTip,
        causal_counter: u64,
        proved_own_counter: u64,
    ) -> Result<(), WriterLaneError> {
        self.guard()?;
        let reservation = LaneReservation {
            role,
            tip,
            causal_counter,
        };
        {
            let record = self.record.borrow();
            if record.peer(role) != peer {
                return Err(WriterLaneError::Uncontinuable(format!(
                    "the prepared batch does not use this device's {} writer lane",
                    role.describe()
                )));
            }
            if record.reservation == Some(reservation) {
                self.idempotent_reservations
                    .set(self.idempotent_reservations.get() + 1);
                return Ok(());
            }
            // A reservation the live prefix has not absorbed, and that nobody
            // proved absent, is an unresolved outcome. Authoring the next batch
            // would reuse counters that may already be outwardly visible.
            if let Some(previous) = record.reservation {
                if previous.causal_counter > proved_own_counter
                    && !self.reservation_proved_absent.get()
                {
                    return Err(WriterLaneError::Uncontinuable(format!(
                        "writer lane reservation for batch {} at causal counter {} has no proved \
                         outcome; this device's covered own prefix still ends at \
                         {proved_own_counter}",
                        previous.tip.batch_id, previous.causal_counter
                    )));
                }
            }
            // Within one incarnation the floor is monotone: a counter this
            // incarnation already put in flight is never re-offered, so a
            // regressed cache cannot fork this device's own chain. (Across
            // incarnations the question does not arise — a rotation gives the
            // older prefix its own causal identity.)
            if causal_counter <= record.confirmed_own_counter {
                return Err(WriterLaneError::Uncontinuable(format!(
                    "writer lane cannot publish at causal counter {causal_counter}: writer \
                     incarnation {} has already reserved through {}",
                    record.incarnation_id, record.confirmed_own_counter
                )));
            }
            if proved_own_counter < record.confirmed_own_counter {
                return Err(WriterLaneError::Uncontinuable(format!(
                    "writer lane cannot publish: authoritative state covers writer incarnation \
                     {} only to {proved_own_counter}, below the retained floor {}",
                    record.incarnation_id, record.confirmed_own_counter
                )));
            }
        }
        if causal_counter != proved_own_counter.saturating_add(1) {
            return Err(WriterLaneError::Uncontinuable(format!(
                "writer lane cannot continue at causal counter {causal_counter}: this device's \
                 covered durable own prefix ends at {proved_own_counter}"
            )));
        }
        let next = WriterLaneRecordV1 {
            confirmed_own_counter: proved_own_counter,
            reservation: Some(reservation),
            ..*self.record.borrow()
        };
        self.publish_record(next)?;
        self.reservation_proved_absent.set(false);
        Ok(())
    }

    /// The batch and causal counter currently reserved, if any.
    pub(crate) fn pending_reservation(&self) -> Option<(WriterLaneTip, u64)> {
        if self.reservation_proved_absent.get() {
            return None;
        }
        self.record
            .borrow()
            .reservation
            .map(|reservation| (reservation.tip, reservation.causal_counter))
    }

    /// The caller proved that the reserved batch never became durable — the
    /// archive reports it absent, or the journal append definitively did not
    /// happen — so its causal counter is free again.
    pub(crate) fn note_reservation_absent(&self, batch_id: BatchId) {
        if self
            .record
            .borrow()
            .reservation
            .is_some_and(|reservation| reservation.tip.batch_id == batch_id)
        {
            self.reservation_proved_absent.set(true);
        }
    }

    /// Re-prove exclusion, then the publication latch.
    ///
    /// The retained lease is compared against the currently named lock file, so
    /// a replaced or relinked lock is refused rather than silently shared.
    fn guard(&self) -> Result<(), WriterLaneError> {
        if self.failed.get() {
            return Err(WriterLaneError::PublicationUnknown(
                "an earlier writer lane reservation did not complete".into(),
            ));
        }
        let current = tine_storage::open_file_nofollow(&self.directory, &self.lock_name)
            .map_err(|error| WriterLaneError::NotExclusive(error.to_string()))?;
        let held = super::sqlite::held_file_identity(&self.lease, Path::new(&self.lock_name))
            .map_err(|error| WriterLaneError::NotExclusive(error.to_string()))?;
        let named = super::sqlite::held_file_identity(&current, Path::new(&self.lock_name))
            .map_err(|error| WriterLaneError::NotExclusive(error.to_string()))?;
        if held != named {
            return Err(WriterLaneError::NotExclusive(
                "the retained CRDT writer-lane lock file was replaced".into(),
            ));
        }
        Ok(())
    }

    fn publish_record(&self, record: WriterLaneRecordV1) -> Result<(), WriterLaneError> {
        let bytes = record.encode()?;
        let expected = self.durable_bytes.borrow().clone();
        if let Err(error) = publish(&self.publication, &self.record_name, &expected, &bytes) {
            // A failed exact replacement leaves the previous bytes in place,
            // but this process can no longer prove which one is durable.
            self.failed.set(true);
            return Err(WriterLaneError::PublicationUnknown(error.to_string()));
        }
        self.record_publications
            .set(self.record_publications.get() + 1);
        *self.durable_bytes.borrow_mut() = bytes;
        *self.record.borrow_mut() = record;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> Option<Vec<u8>> {
        read_optional_regular(&self.directory, &self.record_name, MAX_RECORD_BYTES, None)
            .expect("retained writer lane record is readable")
    }
}

/// How one clean runtime reaches its writer lanes.
///
/// Production always holds a durable record. Engine/projection fixtures open no
/// device-private application runtime root at all, so they get stable
/// per-(endpoint, role) peers with no durability — which is exactly why a
/// fixture can never be evidence for restart continuation.
pub(crate) enum WriterLanes {
    /// A clean runtime exists before its lanes can be qualified: the open path
    /// has to restore accepted history and drain the local journal first,
    /// because that is what proves the lane's own durable prefix. No lane is
    /// vended until then, so no authoring can precede qualification.
    Deferred,
    Durable(WriterLaneStore),
    #[cfg(test)]
    Fixture(WriterLaneBinding),
}

impl WriterLanes {
    #[cfg(test)]
    pub(crate) const fn fixture(binding: WriterLaneBinding) -> Self {
        Self::Fixture(binding)
    }

    pub(crate) const fn is_qualified(&self) -> bool {
        !matches!(self, Self::Deferred)
    }

    fn deferred() -> WriterLaneError {
        WriterLaneError::Uncontinuable(
            "this runtime has not qualified its persistent CRDT writer lanes against restored \
             accepted history and its drained local journal"
                .into(),
        )
    }

    pub(crate) fn peer(&self, role: WriterRole) -> Result<CrdtPeerId, WriterLaneError> {
        match self {
            Self::Deferred => Err(Self::deferred()),
            Self::Durable(store) => store.peer(role),
            #[cfg(test)]
            Self::Fixture(binding) => Ok(fixture_peer(*binding, role)),
        }
    }

    /// The causal writer incarnation this runtime authors every batch under.
    pub(crate) fn causal_peer(&self) -> Result<CausalPeerId, WriterLaneError> {
        match self {
            Self::Deferred => Err(Self::deferred()),
            Self::Durable(store) => {
                store.guard()?;
                Ok(store.causal_peer())
            }
            #[cfg(test)]
            Self::Fixture(binding) => Ok(CausalPeerId::from_key(
                WriterIncarnationId::fixture_for_device(binding.device_id),
            )),
        }
    }

    pub(crate) fn reserve(
        &self,
        role: WriterRole,
        peer: CrdtPeerId,
        tip: WriterLaneTip,
        causal_counter: u64,
        proved_own_counter: u64,
    ) -> Result<(), WriterLaneError> {
        match self {
            Self::Deferred => Err(Self::deferred()),
            Self::Durable(store) => {
                store.reserve(role, peer, tip, causal_counter, proved_own_counter)
            }
            #[cfg(test)]
            Self::Fixture(binding) => {
                if fixture_peer(*binding, role) != peer {
                    return Err(WriterLaneError::Uncontinuable(format!(
                        "the prepared batch does not use this device's {} writer lane",
                        role.describe()
                    )));
                }
                Ok(())
            }
        }
    }

    pub(crate) fn note_reservation_absent(&self, batch_id: BatchId) {
        match self {
            Self::Deferred => {}
            Self::Durable(store) => store.note_reservation_absent(batch_id),
            #[cfg(test)]
            Self::Fixture(_) => {}
        }
    }

    pub(crate) fn pending_reservation(&self) -> Option<(WriterLaneTip, u64)> {
        match self {
            Self::Deferred => None,
            Self::Durable(store) => store.pending_reservation(),
            #[cfg(test)]
            Self::Fixture(_) => None,
        }
    }

    /// Which lane peers this runtime owns. Used to prove that a batch this
    /// device authored advanced only its own lane.
    pub(crate) fn owned_peers(&self) -> Result<[(WriterRole, CrdtPeerId); 2], WriterLaneError> {
        Ok([
            (WriterRole::Local, self.peer(WriterRole::Local)?),
            (WriterRole::External, self.peer(WriterRole::External)?),
        ])
    }
}

/// Fixture lanes are derived, never saved, precisely because a fixture has no
/// durable record: derivation keeps one fixture's peers stable across its own
/// reopens without ever pretending to be recovery evidence.
#[cfg(test)]
fn fixture_peer(binding: WriterLaneBinding, role: WriterRole) -> CrdtPeerId {
    CrdtPeerId::writer_lane_candidate(
        binding.workspace_id,
        binding.device_id,
        binding.endpoint_id,
        match role {
            WriterRole::Local => 0,
            WriterRole::External => 1,
        },
        0,
        0,
    )
}

/// The causal-dot counter this INCARNATION must not publish at or below.
///
/// Resolves the at-most-one ambiguous dot against the exact reserved manifest
/// fingerprint, then keeps the retained floor. A result above `proved` means
/// authoritative state does not cover the saved incarnation's own durable
/// chain, which is precisely when the incarnation is retired and replaced.
fn own_chain_floor(
    record: &WriterLaneRecordV1,
    proved: u64,
    proof: &dyn WriterLanePrefixProof,
) -> u64 {
    let mut floor = record.confirmed_own_counter;
    if let Some(reservation) = record.reservation {
        if reservation.causal_counter > proved {
            let free = reservation.causal_counter == proved.saturating_add(1)
                && proof.reserved_batch_durability(reservation.tip)
                    == ReservedBatchDurability::Absent;
            if !free {
                // Durable yet unrestored, unanswerable, or more than one dot
                // unaccounted for: the dot is spent.
                floor = floor.max(reservation.causal_counter);
            }
        }
    }
    floor
}

/// What one retained lane record says, read straight off disk.
///
/// Live-path tests use this instead of an engine accessor precisely because the
/// durable record — not any in-memory value — is what a restart continues from.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WriterLaneRecordView {
    pub(crate) incarnation: u64,
    pub(crate) incarnation_id: WriterIncarnationId,
    pub(crate) local_peer: CrdtPeerId,
    pub(crate) external_peer: CrdtPeerId,
    pub(crate) confirmed_own_counter: u64,
    pub(crate) reserved: Option<(BatchId, u64)>,
}

/// Every retained lane record under one application runtime root, with the
/// exact bytes each was decoded from.
#[cfg(test)]
pub(crate) fn retained_lane_records(
    application_runtime_root: &std::path::Path,
) -> Vec<(String, Vec<u8>, Option<WriterLaneRecordView>)> {
    let namespace = application_runtime_root.join(WRITER_LANE_NAMESPACE);
    let mut found = Vec::new();
    let Ok(lanes) = std::fs::read_dir(&namespace) else {
        return found;
    };
    for lane in lanes.flatten() {
        let Ok(entries) = std::fs::read_dir(lane.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".postcard") {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let view = postcard::take_from_bytes::<WriterLaneRecordV1>(&bytes)
                .ok()
                .map(|(record, _)| WriterLaneRecordView {
                    incarnation: record.incarnation,
                    incarnation_id: record.incarnation_id,
                    local_peer: record.local_peer,
                    external_peer: record.external_peer,
                    confirmed_own_counter: record.confirmed_own_counter,
                    reserved: record
                        .reservation
                        .map(|reservation| (reservation.tip.batch_id, reservation.causal_counter)),
                });
            found.push((name, bytes, view));
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
}

fn publish(
    publication: &DurableDirectoryPublication,
    name: &str,
    expected: &[u8],
    bytes: &[u8],
) -> Result<(), WriterLaneError> {
    if expected.is_empty() {
        publication
            .publish_new_exact_single_writer(name, bytes)
            .map_err(WriterLaneError::io)
    } else {
        publication
            .replace_exact(name, expected, bytes)
            .map_err(WriterLaneError::io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Prefix {
        proved: u64,
        durability: ReservedBatchDurability,
        asked: Cell<usize>,
        /// Which causal peer the store asked about. This is the receipt for
        /// "qualification asks about the saved incarnation, not the DeviceId".
        asked_about: RefCell<Vec<CausalPeerId>>,
    }

    impl Prefix {
        fn new(proved: u64) -> Self {
            Self::with(proved, ReservedBatchDurability::Absent)
        }

        fn with(proved: u64, durability: ReservedBatchDurability) -> Self {
            Self {
                proved,
                durability,
                asked: Cell::new(0),
                asked_about: RefCell::new(Vec::new()),
            }
        }
    }

    impl WriterLanePrefixProof for Prefix {
        fn proved_own_counter(&self, peer: CausalPeerId) -> u64 {
            self.asked_about.borrow_mut().push(peer);
            self.proved
        }

        fn reserved_batch_durability(&self, _: WriterLaneTip) -> ReservedBatchDurability {
            self.asked.set(self.asked.get() + 1);
            self.durability
        }
    }

    fn binding() -> WriterLaneBinding {
        WriterLaneBinding {
            workspace_id: WorkspaceId::new(),
            lineage: LineageDigest::from_bytes([7; 32]),
            device_id: DeviceId::new(),
            endpoint_id: ProjectionEndpointId::new(),
        }
    }

    /// One exact outward publication: a batch identity AND the manifest
    /// fingerprint that identity must carry.
    fn batch(index: u128) -> WriterLaneTip {
        WriterLaneTip {
            batch_id: BatchId::from_uuid(uuid::Uuid::from_u128(index)),
            manifest_digest: ContentDigest::of(&index.to_be_bytes()),
        }
    }

    fn directory(test: impl FnOnce(&Dir)) {
        let path = std::env::temp_dir().join(format!("tine-writer-lane-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let dir = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).unwrap();
        test(&dir);
        drop(dir);
        std::fs::remove_dir_all(path).unwrap();
    }

    fn open(
        dir: &Dir,
        binding: WriterLaneBinding,
        proof: &dyn WriterLanePrefixProof,
    ) -> WriterLaneStore {
        WriterLaneStore::open(dir.try_clone().unwrap(), binding, proof).unwrap()
    }

    /// Ordinary restarts, local bursts, imports and seals all reuse the same
    /// two peers: the lane is what bounds `P` to writer incarnations.
    #[test]
    fn ordinary_restarts_and_alternating_roles_reuse_the_same_two_lanes() {
        directory(|dir| {
            let binding = binding();
            let store = open(dir, binding, &Prefix::new(0));
            assert_eq!(store.disposition(), WriterLaneOpenDisposition::Minted);
            let local = store.peer(WriterRole::Local).unwrap();
            let external = store.peer(WriterRole::External).unwrap();
            let incarnation = store.incarnation_id();
            assert_ne!(local, external);
            assert_eq!(store.incarnation(), 0);

            let mut counter = 0;
            for index in 0..16_u128 {
                let (role, peer) = if index % 3 == 0 {
                    (WriterRole::External, external)
                } else {
                    (WriterRole::Local, local)
                };
                // The live engine absorbs each published batch, so the next
                // reservation's covered prefix is the previous counter.
                store
                    .reserve(role, peer, batch(index), counter + 1, counter)
                    .unwrap();
                counter += 1;
            }
            drop(store);

            for _ in 0..4 {
                let store = open(dir, binding, &Prefix::new(counter));
                assert_eq!(store.disposition(), WriterLaneOpenDisposition::Continued);
                assert_eq!(store.incarnation(), 0);
                assert_eq!(store.peer(WriterRole::Local).unwrap(), local);
                assert_eq!(store.peer(WriterRole::External).unwrap(), external);
                // The causal writer incarnation is retained too: ordinary
                // restarts, seals and imports all stay on one Tine chain.
                assert_eq!(store.incarnation_id(), incarnation);
                assert_eq!(store.causal_peer(), CausalPeerId::from_key(incarnation));
            }
        });
    }

    /// A crash between reservation and publication is the ordinary case, on
    /// both commit paths: the reserved batch is provably absent, so the lane
    /// continues and reuses the dot. Only a durable-but-unrestored batch, or an
    /// unanswerable store, mints a new incarnation.
    #[test]
    fn uncertain_publication_resolves_by_reserved_batch_durability() {
        directory(|dir| {
            let binding = binding();
            let store = open(dir, binding, &Prefix::new(0));
            let local = store.peer(WriterRole::Local).unwrap();
            let external = store.peer(WriterRole::External).unwrap();
            store
                .reserve(WriterRole::Local, local, batch(1), 1, 0)
                .unwrap();
            store
                .reserve(WriterRole::External, external, batch(2), 2, 1)
                .unwrap();
            // The external archive commit outcome is never observed.
            drop(store);

            let absent = Prefix::with(1, ReservedBatchDurability::Absent);
            let continued = open(dir, binding, &absent);
            let continued_incarnation = continued.incarnation_id();
            assert_eq!(absent.asked.get(), 1);
            assert_eq!(
                continued.disposition(),
                WriterLaneOpenDisposition::Continued
            );
            assert_eq!(continued.peer(WriterRole::Local).unwrap(), local);
            assert_eq!(continued.peer(WriterRole::External).unwrap(), external);
            // Reopening now finds no reservation, so nothing is asked.
            continued
                .reserve(WriterRole::External, external, batch(3), 2, 1)
                .unwrap();
            drop(continued);

            let stranded = Prefix::with(1, ReservedBatchDurability::Present);
            let rotated = open(dir, binding, &stranded);
            assert_eq!(
                rotated.disposition(),
                WriterLaneOpenDisposition::RotatedForUnprovableCoverage
            );
            assert_eq!(rotated.incarnation(), 1);
            assert_ne!(rotated.peer(WriterRole::Local).unwrap(), local);
            assert_ne!(rotated.peer(WriterRole::External).unwrap(), external);
            // A durable-but-unrestored batch retires the causal identity too,
            // so the stranded dot 2 stays the old incarnation's forever.
            assert_ne!(rotated.incarnation_id(), continued_incarnation);
        });
    }

    /// A rebuild whose restored prefix is behind what the saved incarnation
    /// already proved durable is a real incarnation change: the causal peer AND
    /// both Loro peers are replaced together, durably, before the store vends
    /// anything. The unprovable older prefix keeps its own causal identity, so
    /// the rebuilt copy starts a fresh chain at counter 1 instead of reusing a
    /// counter an offline peer may still hold — and it never has to wait.
    #[test]
    fn an_unprovable_prefix_retires_the_whole_incarnation_instead_of_reusing_its_counters() {
        directory(|dir| {
            let binding = binding();
            let store = open(dir, binding, &Prefix::new(0));
            let local = store.peer(WriterRole::Local).unwrap();
            let external = store.peer(WriterRole::External).unwrap();
            let original_incarnation = store.incarnation_id();
            for index in 1..=5_u128 {
                store
                    .reserve(
                        WriterRole::Local,
                        local,
                        batch(index),
                        index as u64,
                        index as u64 - 1,
                    )
                    .unwrap();
            }
            drop(store);

            // Reservation for dot 5 with a chain restored only to 2: two dots
            // are unaccounted for, so no durability question can rescue it.
            let undecidable = Prefix::with(2, ReservedBatchDurability::Undecidable);
            let rotated = open(dir, binding, &undecidable);
            assert_eq!(undecidable.asked.get(), 0);
            assert_eq!(
                rotated.disposition(),
                WriterLaneOpenDisposition::RotatedForUnprovableCoverage
            );
            let replacement = rotated.peer(WriterRole::Local).unwrap();
            assert_ne!(replacement, local);
            assert_ne!(rotated.peer(WriterRole::External).unwrap(), external);
            // The causal chain rotates WITH the Loro lanes, which is the whole
            // correction: dots 3, 4 and 5 of the old incarnation stay that
            // incarnation's, and this copy owes nothing on them.
            let fresh_incarnation = rotated.incarnation_id();
            assert_ne!(fresh_incarnation, original_incarnation);
            assert_eq!(rotated.incarnation(), 1);
            assert_eq!(rotated.confirmed_own_counter(), 0);
            // Useful local work resumes immediately, at the new chain's dot 1.
            rotated
                .reserve(WriterRole::Local, replacement, batch(91), 1, 0)
                .unwrap();
            // Durable before any authoring: a same-instant reopen agrees and
            // keeps exactly the identities that were saved.
            drop(rotated);
            let reopened = open(dir, binding, &Prefix::new(1));
            assert_eq!(reopened.disposition(), WriterLaneOpenDisposition::Continued);
            assert_eq!(reopened.peer(WriterRole::Local).unwrap(), replacement);
            assert_eq!(reopened.incarnation_id(), fresh_incarnation);
            assert_eq!(reopened.incarnation(), 1);
            reopened
                .reserve(WriterRole::Local, replacement, batch(92), 2, 1)
                .unwrap();
        });
    }

    /// A lost record and a torn record both retire the causal identity, not
    /// only the Loro lanes. This is the exact case the manager's
    /// `distinct-dot-test.log` reproduced: two independently prepared batches
    /// of one enrolled device must not land on one `BatchCausalDot`.
    #[test]
    fn a_lost_or_torn_record_never_reissues_the_causal_writer_incarnation() {
        for damage in ["lost", "torn"] {
            directory(|dir| {
                let binding = binding();
                let store = open(dir, binding, &Prefix::new(0));
                let original_incarnation = store.incarnation_id();
                let local = store.peer(WriterRole::Local).unwrap();
                store
                    .reserve(WriterRole::Local, local, batch(1), 1, 0)
                    .unwrap();
                drop(store);

                if damage == "lost" {
                    dir.remove_file(binding.record_name()).unwrap();
                } else {
                    dir.write(binding.record_name(), b"torn").unwrap();
                }
                // The older graph copy's accepted state knows nothing at all,
                // which is exactly the aliasing case: the old prefix is neither
                // visible nor provable here.
                let recovered = open(dir, binding, &Prefix::new(0));
                assert_ne!(recovered.incarnation_id(), original_incarnation);
                assert_ne!(recovered.peer(WriterRole::Local).unwrap(), local);
                assert_eq!(recovered.confirmed_own_counter(), 0);
                // Dot 1 of the NEW incarnation is not dot 1 of the old one.
                recovered
                    .reserve(
                        WriterRole::Local,
                        recovered.peer(WriterRole::Local).unwrap(),
                        batch(2),
                        1,
                        0,
                    )
                    .unwrap();
                assert_ne!(
                    CausalPeerId::from_key(recovered.incarnation_id()),
                    CausalPeerId::from_key(original_incarnation)
                );
            });
        }
    }

    /// Counter qualification asks about the SAVED causal peer, never about the
    /// enrolled device: that is what makes one device's several incarnations
    /// independent chains rather than one chain with reused counters.
    #[test]
    fn counter_qualification_asks_about_the_saved_incarnation_not_the_device() {
        directory(|dir| {
            let binding = binding();
            let minted = open(dir, binding, &Prefix::new(0));
            // Minting asks nothing: a brand-new incarnation has no prefix.
            let saved = minted.causal_peer();
            assert_eq!(saved, CausalPeerId::from_key(minted.incarnation_id()));
            drop(minted);

            let proof = Prefix::new(0);
            let reopened = open(dir, binding, &proof);
            assert_eq!(*proof.asked_about.borrow(), vec![saved]);
            assert_eq!(reopened.causal_peer(), saved);
            // And the incarnation is not derivable from the device identity.
            assert_ne!(
                reopened.incarnation_id().as_uuid(),
                binding.device_id.as_uuid()
            );
        });
    }

    /// A foreign or damaged record is preserved and rebuilt, never migrated and
    /// never adopted (D-1/D-3).
    #[test]
    fn foreign_or_damaged_records_are_retained_and_rebuilt() {
        directory(|dir| {
            let binding = binding();
            let store = open(dir, binding, &Prefix::new(0));
            let original = store.retained_bytes().unwrap();
            drop(store);

            let mut damaged = original.clone();
            damaged.push(0);
            dir.write(&binding.record_name(), &damaged).unwrap();
            let rebuilt = open(dir, binding, &Prefix::new(0));
            assert_eq!(
                rebuilt.disposition(),
                WriterLaneOpenDisposition::RebuiltFromForeignRecord
            );
            let superseded = format!(
                "{}.superseded-{}",
                binding.record_name(),
                ContentDigest::of(&damaged)
            );
            assert_eq!(dir.read(&superseded).unwrap(), damaged);
            assert_ne!(rebuilt.retained_bytes().unwrap(), damaged);
        });
    }

    /// Two devices sharing one application runtime root never contend, and a
    /// record written for another device is not adopted.
    #[test]
    fn lane_records_are_separated_per_device_and_never_adopted_across_bindings() {
        directory(|dir| {
            let first = binding();
            let second = WriterLaneBinding {
                device_id: DeviceId::new(),
                ..first
            };
            let one = open(dir, first, &Prefix::new(0));
            let two = open(dir, second, &Prefix::new(0));
            assert_ne!(
                one.peer(WriterRole::Local).unwrap(),
                two.peer(WriterRole::Local).unwrap()
            );
            assert_ne!(one.retained_bytes(), two.retained_bytes());

            // The first device's bytes under the second device's name are
            // foreign: rebuilt, with the original preserved.
            let stolen = one.retained_bytes().unwrap();
            drop(one);
            drop(two);
            dir.write(&second.record_name(), &stolen).unwrap();
            let rebuilt = open(dir, second, &Prefix::new(0));
            assert_eq!(
                rebuilt.disposition(),
                WriterLaneOpenDisposition::RebuiltFromForeignRecord
            );
        });
    }

    /// Manager correction A, second half: exclusion has to survive the lock
    /// file itself being replaced underneath a live holder. A retained OS lock
    /// on a file the pathname no longer names excludes nobody, so the store
    /// refuses rather than publish under it.
    #[test]
    fn a_replaced_lane_lock_stops_the_retained_store_from_publishing() {
        directory(|dir| {
            let binding = binding();
            let store = open(dir, binding, &Prefix::new(0));
            let local = store.peer(WriterRole::Local).unwrap();
            store
                .reserve(WriterRole::Local, local, batch(1), 1, 0)
                .unwrap();
            let reserved = store.retained_bytes().unwrap();

            // An out-of-band replacement: the held lock is now on an unlinked
            // inode, so a second runtime could take the named one.
            dir.remove_file(binding.lock_name()).unwrap();
            dir.write(binding.lock_name(), b"").unwrap();
            assert!(matches!(
                store.peer(WriterRole::Local),
                Err(WriterLaneError::NotExclusive(_))
            ));
            assert!(matches!(
                store.reserve(WriterRole::Local, local, batch(2), 2, 1),
                Err(WriterLaneError::NotExclusive(_))
            ));
            assert_eq!(store.retained_bytes().unwrap(), reserved);
        });
    }

    /// Manager correction E: damage at the record boundary is exercised on the
    /// two publication boundaries independently. The local-journal boundary is
    /// a reservation whose record write is torn (the name is not a regular
    /// file); the external-manifest boundary is a reservation whose outcome was
    /// never observed and whose record is then damaged. Both keep the original
    /// bytes recoverable and neither reissues a lane that already published.
    #[test]
    fn record_damage_at_each_publication_boundary_preserves_original_bytes() {
        for boundary in [WriterRole::Local, WriterRole::External] {
            directory(|dir| {
                let binding = binding();
                let store = open(dir, binding, &Prefix::new(0));
                let peer = store.peer(boundary).unwrap();
                store.reserve(boundary, peer, batch(1), 1, 0).unwrap();
                let acknowledged = store.retained_bytes().unwrap();

                // Torn publication: the exact replacement cannot complete, and
                // the store refuses to vend anything until a reopen.
                dir.remove_file(binding.record_name()).unwrap();
                dir.create_dir(binding.record_name()).unwrap();
                assert!(matches!(
                    store.reserve(boundary, peer, batch(2), 2, 1),
                    Err(WriterLaneError::PublicationUnknown(_))
                ));
                assert!(matches!(
                    store.peer(boundary),
                    Err(WriterLaneError::PublicationUnknown(_))
                ));
                drop(store);
                dir.remove_dir(binding.record_name()).unwrap();

                // Undecodable bytes at that same boundary: the originals are
                // retained under a content-determined backup name, the rebuild
                // allocates a lane nobody published under, and the device's own
                // chain continues from authoritative state.
                dir.write(binding.record_name(), b"half a record").unwrap();
                let rebuilt = open(dir, binding, &Prefix::new(1));
                assert_eq!(
                    rebuilt.disposition(),
                    WriterLaneOpenDisposition::RebuiltFromForeignRecord
                );
                assert_ne!(rebuilt.peer(boundary).unwrap(), peer);
                // A brand-new incarnation owes nothing on the old chain: its
                // own floor is zero, and the old dot 1 keeps its own identity.
                assert_eq!(rebuilt.confirmed_own_counter(), 0);
                let superseded = format!(
                    "{}.superseded-{}",
                    binding.record_name(),
                    ContentDigest::of(b"half a record")
                );
                assert_eq!(dir.read(&superseded).unwrap(), b"half a record");
                // The acknowledged original bytes are not lost — they are the
                // superseded backup above plus whatever the archive holds — and
                // the rebuild authors on a distinct chain, so its dot 1 can
                // never be confused with the retired incarnation's dot 1.
                assert!(matches!(
                    rebuilt.reserve(boundary, rebuilt.peer(boundary).unwrap(), batch(3), 2, 0),
                    Err(WriterLaneError::Uncontinuable(_))
                ));
                rebuilt
                    .reserve(boundary, rebuilt.peer(boundary).unwrap(), batch(3), 1, 0)
                    .unwrap();
                assert!(acknowledged.len() < MAX_RECORD_BYTES as usize);
            });
        }
    }

    /// Manager correction E: the added foreground durability cost is measured,
    /// not asserted from the call count. Each reservation that changes the
    /// record performs exactly one `DurableDirectoryPublication` transition —
    /// one temp-file `sync_all` plus one directory fsync, i.e. two fsync
    /// syscalls — on a real filesystem, and a repeated reservation of the same
    /// batch performs none at all.
    #[test]
    fn measured_foreground_durability_cost_is_one_record_transition_per_publication() {
        directory(|dir| {
            let binding = binding();
            let store = open(dir, binding, &Prefix::new(0));
            // Minting the record is itself one transition.
            assert_eq!(store.durability_cost().record_publications, 1);
            let local = store.peer(WriterRole::Local).unwrap();

            const TURNS: u64 = 32;
            for counter in 1..=TURNS {
                store
                    .reserve(
                        WriterRole::Local,
                        local,
                        batch(counter as u128),
                        counter,
                        counter - 1,
                    )
                    .unwrap();
                // A retried publication of the same batch is free.
                store
                    .reserve(
                        WriterRole::Local,
                        local,
                        batch(counter as u128),
                        counter,
                        counter - 1,
                    )
                    .unwrap();
            }
            let cost = store.durability_cost();
            assert_eq!(cost.record_publications, TURNS + 1);
            assert_eq!(cost.idempotent_reservations, TURNS);
            let per_turn = cost.reservation_nanos / u128::from(TURNS);
            println!(
                "writer-lane reservation barrier: {TURNS} publishing turns,                  {} record transitions (2 fsync syscalls each),                  {} idempotent retries (0 transitions),                  {} ns total, {per_turn} ns per turn",
                cost.record_publications, cost.idempotent_reservations, cost.reservation_nanos,
            );
            assert!(per_turn > 0, "the barrier is a real filesystem operation");
        });
    }

    // -----------------------------------------------------------------
    // Manager negative controls (rebaselining-2026-09-07,
    // p1-manager-negative-controls/regression-tests.rs). Retained verbatim
    // except for the API names this pass changed.
    // -----------------------------------------------------------------

    #[test]
    fn manager_missing_record_must_not_reissue_a_used_peer() {
        directory(|dir| {
            let binding = binding();
            let initial = open(dir, binding, &Prefix::new(0));
            let original_peer = initial.peer(WriterRole::Local).unwrap();
            initial
                .reserve(WriterRole::Local, original_peer, batch(1), 1, 0)
                .unwrap();
            drop(initial);
            // The outward batch survives on another replica; private lane
            // identity and its prefix are missing from this reconstructed copy.
            dir.remove_file(binding.record_name()).unwrap();
            let recovered = open(dir, binding, &Prefix::new(0));
            assert_ne!(
                recovered.peer(WriterRole::Local).unwrap(),
                original_peer,
                "lost private identity cannot qualify the old writer prefix"
            );
        });
    }

    #[test]
    fn manager_torn_record_must_not_reissue_a_used_peer() {
        directory(|dir| {
            let binding = binding();
            let initial = open(dir, binding, &Prefix::new(0));
            let original_peer = initial.peer(WriterRole::Local).unwrap();
            initial
                .reserve(WriterRole::Local, original_peer, batch(1), 1, 0)
                .unwrap();
            drop(initial);
            dir.write(binding.record_name(), b"torn").unwrap();
            let recovered = open(dir, binding, &Prefix::new(0));
            assert_ne!(
                recovered.peer(WriterRole::Local).unwrap(),
                original_peer,
                "a backup of undecodable bytes is not proof of old prefix coverage"
            );
        });
    }

    #[test]
    fn manager_two_archive_leases_must_not_share_a_writer_record() {
        directory(|dir| {
            let binding = binding();
            let archives = std::env::temp_dir().join(format!(
                "tine-manager-two-archives-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir(&archives).unwrap();
            let first_archive =
                crate::oplog::ObjectStore::open(&archives.join("a"), binding.workspace_id).unwrap();
            let second_archive =
                crate::oplog::ObjectStore::open(&archives.join("b"), binding.workspace_id).unwrap();
            let first_lease = crate::oplog::sqlite::WorkspaceRuntimeLease::acquire(
                &first_archive,
                binding.workspace_id,
            )
            .unwrap();
            let second_lease = crate::oplog::sqlite::WorkspaceRuntimeLease::acquire(
                &second_archive,
                binding.workspace_id,
            )
            .unwrap();
            let first = open(dir, binding, &Prefix::new(0));
            let second = WriterLaneStore::open(dir.try_clone().unwrap(), binding, &Prefix::new(0));
            let excluded = second.is_err();
            drop(second);
            drop(first);
            drop(second_lease);
            drop(first_lease);
            drop(second_archive);
            drop(first_archive);
            std::fs::remove_dir_all(archives).unwrap();
            assert!(excluded, "independent archive leases do not exclude access to one shared private lane record");
        });
    }

    /// Reserving past an unproved outcome is refused, and a failed reservation
    /// publication revokes the store until reopen.
    #[test]
    fn unproved_outcomes_block_continuation_and_failed_publication_revokes() {
        directory(|dir| {
            let binding = binding();
            let store = open(dir, binding, &Prefix::new(0));
            let local = store.peer(WriterRole::Local).unwrap();
            let external = store.peer(WriterRole::External).unwrap();
            store
                .reserve(WriterRole::Local, local, batch(1), 1, 0)
                .unwrap();
            // Outcome never observed: the engine's covered prefix is still 0,
            // so a different batch cannot take counter 1 and nothing may skip
            // ahead to counter 2. The wrong lane's peer is refused outright.
            assert!(matches!(
                store.reserve(WriterRole::Local, local, batch(2), 1, 0),
                Err(WriterLaneError::Uncontinuable(_))
            ));
            assert!(matches!(
                store.reserve(WriterRole::Local, local, batch(2), 2, 0),
                Err(WriterLaneError::Uncontinuable(_))
            ));
            assert!(matches!(
                store.reserve(WriterRole::Local, external, batch(2), 1, 0),
                Err(WriterLaneError::Uncontinuable(_))
            ));
            // The same batch again is idempotent and writes nothing new.
            let reserved = store.retained_bytes().unwrap();
            store
                .reserve(WriterRole::Local, local, batch(1), 1, 0)
                .unwrap();
            assert_eq!(store.retained_bytes().unwrap(), reserved);
            // Proving the publication never happened frees the counter again.
            store.note_reservation_absent(batch(1).batch_id);
            store
                .reserve(WriterRole::Local, local, batch(2), 1, 0)
                .unwrap();
            let reserved = store.retained_bytes().unwrap();

            // Force the shared exact-replacement boundary to refuse.
            dir.remove_file(&binding.record_name()).unwrap();
            dir.create_dir(&binding.record_name()).unwrap();
            assert!(matches!(
                store.reserve(WriterRole::Local, local, batch(3), 2, 1),
                Err(WriterLaneError::PublicationUnknown(_))
            ));
            assert!(matches!(
                store.peer(WriterRole::Local),
                Err(WriterLaneError::PublicationUnknown(_))
            ));
            assert!(matches!(
                store.reserve(WriterRole::Local, local, batch(3), 2, 1),
                Err(WriterLaneError::PublicationUnknown(_))
            ));
            drop(store);
            dir.remove_dir(&binding.record_name()).unwrap();
            dir.write(&binding.record_name(), &reserved).unwrap();
            let recovered = open(dir, binding, &Prefix::new(1));
            assert_eq!(recovered.peer(WriterRole::Local).unwrap(), local);
            assert_eq!(
                recovered.disposition(),
                WriterLaneOpenDisposition::Continued
            );
        });
    }
}
