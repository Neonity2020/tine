//! Additive cold whole-object storage and the single logical-object resolver.
//!
//! # What this is
//!
//! R2's cold tier under the existing object model. A logical object keeps its
//! canonical encoded bytes, its content digest and its identity; only its
//! *physical placement* changes. A cold record lives inside an immutable pack
//! file together with many other records, so retiring a large history does not
//! cost one filesystem object -- or one read request -- per logical block.
//!
//! # Existing primitives searched (D-14)
//!
//! * `ObjectStore`'s `objects/` and `batches/` namespaces are the hot tier and
//!   remain unchanged. This module never replaces them; it is consulted only
//!   after the hot name is absent.
//! * `tine_storage::sealed_accepted_index`'s canonical authenticated map is the
//!   locator index. No second tree, no tuple hashing, no truncation of a
//!   SHA-256 into a UUID: the object domain composes two 16-byte map key halves
//!   exactly the way `SealedDocumentMap` composes a membership pair.
//! * `checkpoint_generation::{SealedGenerationDirectory, SealedGenerationStagingStore}`
//!   is the sealed map-node object store, reused verbatim over this module's own
//!   private directory capability. There is no second node codec or publisher.
//! * `tine_storage::DurableDirectoryPublication` publishes pack bytes and
//!   replaces the root marker (D-7/I-1/I-2). No bespoke temp+rename exists here.
//! * `tine_storage::package_store` was considered and rejected: it publishes
//!   whole immutable *directories* by no-clobber transition, so it cannot
//!   address a byte range inside one packed file.
//! * `tine_storage` has no ranged-read primitive; the range read below is one
//!   seek plus one `read_exact` on the shared `open_file_nofollow` handle.
//!
//! # Physical format (current, one format, no migration -- D-1)
//!
//! ```text
//! <archive>/cold-history-v1/
//!   pack-v1-<uuid>          immutable pack
//!   sealed-v2-<kind>-<addr> shared authenticated-map nodes
//!   current                 canonical root marker, installed last
//! ```
//!
//! A pack is `record* footer footer_len:u64be magic:8`. A record is
//! `sha256(payload):32 payload_len:u64be payload`, so one ranged read
//! self-verifies its payload without consulting the index a second time. The
//! footer makes the pack self-describing, which is what lets the *index* be
//! disposable derived state (D-3): `repack_cold_history` rebuilds every root
//! from pack footers alone.
//!
//! A locator is exactly 32 bytes -- `pack_uuid:16 offset:u64be length:u64be` --
//! so it fits the authenticated map's fixed value slot with no side blob and no
//! extra filesystem object per logical record.
//!
//! Object domain: the full 256-bit key is carried by *composing two of the same
//! authenticated maps*, exactly the way `SealedDocumentMap` composes a
//! membership pair. The outer map is keyed by `sha256[0..16]`; its value
//! locates a fixed-size *inner-root descriptor* record, and that descriptor
//! names an inner authenticated map keyed by `sha256[16..32]` whose values are
//! the record locators. There is no list, no occupancy assumption and no cap on
//! how many objects may share a 128-bit prefix -- an outer entry holds a whole
//! map, so a prefix with one member and a prefix with a million members differ
//! only in the inner map's depth. The descriptor is packed with the same
//! physical pack machinery as every other record, which is why the outer map's
//! fixed 32-byte value slot suffices.
//! A lookup costs `O(log n)` outer map-node reads, one bounded ranged read of
//! the descriptor, `O(log m)` inner map-node reads and one bounded ranged read
//! of the payload: two pack reads however large history or a prefix grows.
//! Manifest domain: map key is the `BatchId` UUID; the value locates the record
//! directly, and the record header still carries the manifest's full SHA-256.
//!
//! # Exactness, not name presence
//!
//! A repeated identity is only "already archived" when its *bytes* are
//! byte-identical to what cold history already holds. `BatchId` is an identity,
//! not a content address: two valid canonical manifests can legitimately carry
//! the same `BatchId` and differ (a different `SessionId` alone suffices). Every
//! duplicate path -- publication, repack, and footer reconstruction -- therefore
//! compares the original bytes through the shared reader before treating a
//! repeated ID as present, and refuses a conflict by name while preserving the
//! predecessor root and the original bytes. Objects are additionally bound by
//! their content digest, which every read re-proves.
//!
//! # Lost root marker is a repair condition, never absence
//!
//! The root marker is derived state, the packs are the self-describing truth.
//! An archive with no cold directory, or a cold directory with neither marker
//! nor packs, has never published cold history: ordinary absence. A cold
//! directory whose packs survive but whose marker is gone is a *named damaged
//! state* (`StoreError::ColdHistoryRootMissing`), never ordinary absence and
//! never a licence to publish a fresh empty-based root over old history. A
//! *torn or otherwise malformed* marker over surviving packs is the same
//! damaged class: ordinary reads still reject it, and repair treats it exactly
//! like a missing one. `repair_cold_history_root` recovers either case
//! explicitly from the pack footers, reusing the surviving records in place and
//! swapping the marker last through the audited guarded replacement. A damaged
//! marker with no surviving pack is never replaced with an empty history --
//! there is nothing to rebuild from. Healthy opens stay bounded point
//! operations: only the marker-absent path enumerates, and it short-circuits at
//! the first pack it sees.
//!
//! # What this packet does NOT do
//!
//! Publication is additive. Nothing here retires a hot original, and nothing
//! here retires a superseded pack: `repack_cold_history` publishes new packs and
//! swaps the root, leaving the predecessor packs in place. Enabling deletion
//! needs P5's generation/fallback/retention proofs. There is no content-defined
//! chunker here either -- that is R3, deliberately after this cut.

// Every consumer that may legitimately reach cold history lives in a module
// this packet does not own (`hot_engine`, `sync_runtime`, the receipt/sweep
// modules). Their call sites are handed off in RECEIPT.md for the manager to
// apply, so the resolver is deliberately unreferenced from production code at
// this cut; its tests exercise the whole surface.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, Read, Seek, SeekFrom};

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::checkpoint_generation::{SealedGenerationDirectory, SealedGenerationStagingStore};
use super::object_store::{filesystem_error_without_collision, ObjectStore, StoreError};
use super::{BatchId, ContentDigest, MAX_MANIFEST_BYTES, MAX_OBJECT_BYTES};
use tine_storage::sealed_accepted_index::{
    AuthenticatedMapKey, AuthenticatedMapLinkV1, AuthenticatedMapRootV1, SealedAcceptedIndexReader,
    SealedAcceptedIndexWriter,
};

/// The private cold-history namespace, rooted in the retained archive
/// capability exactly like `clean-open-checkpoint-v2`.
pub(crate) const COLD_HISTORY_DIRECTORY: &str = "cold-history-v1";
/// The canonical root marker. It is installed last, after every pack and index
/// node it names is already durable.
const COLD_ROOT_MARKER: &str = "current";
const COLD_PACK_PREFIX: &str = "pack-v1";
const COLD_SCHEMA_VERSION: u32 = 1;

/// `sha256(payload) || payload_len` prefixed to every packed record.
const COLD_RECORD_HEADER_BYTES: usize = 40;
const COLD_PACK_MAGIC: [u8; 8] = *b"TINECLD1";
/// A construction target, not an occupancy cap (D-5): one legal record larger
/// than this is packed alone rather than refused.
const COLD_PACK_TARGET_BYTES: usize = 4 * 1024 * 1024;
/// One record is at most `MAX_OBJECT_BYTES`; a pack holds one oversize record
/// plus its footer, or many target-sized ones.
const MAX_COLD_PACK_BYTES: u64 = 512 * 1024 * 1024;
const MAX_COLD_PACK_FOOTER_BYTES: u64 = 64 * 1024 * 1024;
/// An inner-root descriptor is a fixed-shape record -- a schema tag, a count and
/// an optional `(key, digest)` link -- so this is the codec size of one constant
/// structure. It does not bound how many objects share a 128-bit prefix: those
/// live in the inner map the descriptor names, not in the descriptor.
const MAX_COLD_INNER_ROOT_BYTES: u64 = 256;
const MAX_COLD_ROOT_BYTES: u64 = 4 * 1024;

const COLD_CLASS_OBJECT: u8 = 1;
const COLD_CLASS_MANIFEST: u8 = 2;
/// The packed descriptor of one 128-bit prefix's inner authenticated map.
const COLD_CLASS_INNER_ROOT: u8 = 3;

fn pack_filename(pack: Uuid) -> String {
    format!("{COLD_PACK_PREFIX}-{pack}")
}

fn parse_pack_filename(name: &str) -> Option<Uuid> {
    let rest = name.strip_prefix(COLD_PACK_PREFIX)?.strip_prefix('-')?;
    let pack = Uuid::parse_str(rest).ok()?;
    (pack.to_string() == rest).then_some(pack)
}

fn cold_index_error(message: impl Into<String>) -> StoreError {
    StoreError::ColdHistoryIndexUnavailable(message.into())
}

fn cold_object_error(digest: ContentDigest, message: impl Into<String>) -> StoreError {
    StoreError::ColdObjectUnavailable {
        digest,
        reason: message.into(),
    }
}

fn cold_manifest_error(batch_id: BatchId, message: impl Into<String>) -> StoreError {
    StoreError::ColdManifestUnavailable {
        batch_id,
        reason: message.into(),
    }
}

fn cold_manifest_conflict(batch_id: BatchId, message: impl Into<String>) -> StoreError {
    StoreError::ColdManifestConflict {
        batch_id,
        reason: message.into(),
    }
}

/// Physical placement of one packed record. Deliberately exactly 32 bytes so it
/// occupies the authenticated map's fixed value slot directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ColdLocatorV1 {
    pack: Uuid,
    offset: u64,
    length: u64,
}

impl ColdLocatorV1 {
    fn to_bytes(self) -> [u8; 32] {
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(self.pack.as_bytes());
        bytes[16..24].copy_from_slice(&self.offset.to_be_bytes());
        bytes[24..].copy_from_slice(&self.length.to_be_bytes());
        bytes
    }

    fn from_bytes(bytes: [u8; 32]) -> Result<Self, String> {
        let mut pack = [0_u8; 16];
        pack.copy_from_slice(&bytes[..16]);
        let mut offset = [0_u8; 8];
        offset.copy_from_slice(&bytes[16..24]);
        let mut length = [0_u8; 8];
        length.copy_from_slice(&bytes[24..]);
        let locator = Self {
            pack: Uuid::from_bytes(pack),
            offset: u64::from_be_bytes(offset),
            length: u64::from_be_bytes(length),
        };
        if locator.length < COLD_RECORD_HEADER_BYTES as u64
            || locator.length > MAX_COLD_PACK_BYTES
            || locator
                .offset
                .checked_add(locator.length)
                .is_none_or(|end| end > MAX_COLD_PACK_BYTES)
        {
            return Err("cold locator names an impossible pack range".into());
        }
        Ok(locator)
    }

    fn value(self) -> ContentDigest {
        ContentDigest::from_bytes(self.to_bytes())
    }

    fn decode_value(value: ContentDigest) -> Result<Self, String> {
        Self::from_bytes(*value.as_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColdPackFooterEntryV1 {
    class: u8,
    key: Vec<u8>,
    offset: u64,
    length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColdPackFooterV1 {
    schema: u32,
    entries: Vec<ColdPackFooterEntryV1>,
}

/// The packed descriptor of one 128-bit prefix's inner authenticated map.
///
/// Fixed shape: it names a map root, it never lists members. The members live
/// in the inner map itself, keyed by the exact low 128 bits of their SHA-256.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColdInnerRootV1 {
    schema: u32,
    root: ColdMapRootWire,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColdMapRootWire {
    count: u64,
    root_key: Option<[u8; 16]>,
    root_digest: Option<ContentDigest>,
}

/// The complete cold lookup root. P5 binds these same values into the
/// generation commit's "cold logical-object lookup root"; the marker here is
/// this packet's additive standalone authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ColdHistoryRootsV1 {
    schema: u32,
    /// Outer object map: `sha256[0..16] -> prefix-bucket locator`.
    objects: ColdMapRootWire,
    /// Manifest map: `BatchId uuid -> record locator`.
    manifests: ColdMapRootWire,
    /// Exact logical counts. `objects.count` counts *prefixes* (outer map
    /// entries); `object_count` is the exact number of full 256-bit keys
    /// summed across every inner map, so the two differ as soon as any two
    /// objects share a 128-bit prefix.
    object_count: u64,
    manifest_count: u64,
}

fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    postcard::to_allocvec(value).map_err(|error| error.to_string())
}

fn decode_canonical<T: for<'de> Deserialize<'de> + Serialize>(bytes: &[u8]) -> Result<T, String> {
    let (value, trailing): (T, &[u8]) =
        postcard::take_from_bytes(bytes).map_err(|error| error.to_string())?;
    if !trailing.is_empty() || encode_canonical(&value)? != bytes {
        return Err("cold history record is noncanonical".into());
    }
    Ok(value)
}

/// Cold lookup keys are exactly 128 bits wide by construction. Reject rather
/// than truncate if a root ever carries a wider shared key.
fn cold_map_root_key_bytes(key: AuthenticatedMapKey) -> Result<[u8; 16], String> {
    <[u8; 16]>::try_from(key.as_slice())
        .map_err(|_| "cold history map root key is not a 128-bit lookup key".to_string())
}

fn map_root_to_wire(root: AuthenticatedMapRootV1) -> Result<ColdMapRootWire, String> {
    Ok(ColdMapRootWire {
        count: root.count,
        root_key: root
            .root
            .map(|link| cold_map_root_key_bytes(link.key))
            .transpose()?,
        root_digest: root.root.map(|link| link.digest),
    })
}

fn map_root_from_wire(wire: ColdMapRootWire) -> Result<AuthenticatedMapRootV1, String> {
    let root = match (wire.root_key, wire.root_digest) {
        (Some(key), Some(digest)) => Some(AuthenticatedMapLinkV1 {
            key: AuthenticatedMapKey::from(key),
            digest,
        }),
        (None, None) => None,
        _ => return Err("cold history map root is partial".into()),
    };
    if (wire.count == 0) != root.is_none() {
        return Err("cold history map root count is inconsistent".into());
    }
    Ok(AuthenticatedMapRootV1 {
        count: wire.count,
        root,
    })
}

/// Encode one prefix's inner map root as a fixed-size packed record.
fn encode_inner_root(root: AuthenticatedMapRootV1) -> Result<Vec<u8>, String> {
    let bytes = encode_canonical(&ColdInnerRootV1 {
        schema: COLD_SCHEMA_VERSION,
        root: map_root_to_wire(root)?,
    })?;
    if bytes.len() as u64 > MAX_COLD_INNER_ROOT_BYTES {
        return Err("cold inner-root descriptor exceeds its fixed codec size".into());
    }
    Ok(bytes)
}

fn decode_inner_root(bytes: &[u8]) -> Result<AuthenticatedMapRootV1, String> {
    let record: ColdInnerRootV1 = decode_canonical(bytes)?;
    if record.schema != COLD_SCHEMA_VERSION {
        return Err("cold inner-root descriptor is not the current schema".into());
    }
    let root = map_root_from_wire(record.root)?;
    if root.count == 0 {
        // An empty prefix map is never published: an outer entry exists only
        // because at least one full key lives under it.
        return Err("cold inner-root descriptor names an empty prefix map".into());
    }
    Ok(root)
}

impl ColdHistoryRootsV1 {
    fn empty() -> Self {
        Self {
            schema: COLD_SCHEMA_VERSION,
            objects: map_root_to_wire(AuthenticatedMapRootV1::empty())
                .expect("the empty map root has no key"),
            manifests: map_root_to_wire(AuthenticatedMapRootV1::empty())
                .expect("the empty map root has no key"),
            object_count: 0,
            manifest_count: 0,
        }
    }

    fn object_root(self) -> Result<AuthenticatedMapRootV1, String> {
        map_root_from_wire(self.objects)
    }

    fn manifest_root(self) -> Result<AuthenticatedMapRootV1, String> {
        map_root_from_wire(self.manifests)
    }

    pub(crate) const fn object_count(self) -> u64 {
        self.object_count
    }

    pub(crate) const fn manifest_count(self) -> u64 {
        self.manifest_count
    }
}

fn split_digest(digest: ContentDigest) -> ([u8; 16], [u8; 16]) {
    let bytes = digest.as_bytes();
    let mut high = [0_u8; 16];
    let mut low = [0_u8; 16];
    high.copy_from_slice(&bytes[..16]);
    low.copy_from_slice(&bytes[16..]);
    (high, low)
}

// ---------------------------------------------------------------------------
// Directory access
// ---------------------------------------------------------------------------

fn cold_directory(store: &ObjectStore) -> Result<Dir, StoreError> {
    let root = store.private_derived_root_capability()?;
    super::object_store::ensure_directory_nofollow(&root, COLD_HISTORY_DIRECTORY)?;
    super::object_store::open_dir_nofollow(&root, COLD_HISTORY_DIRECTORY)
}

fn open_existing_cold_directory(store: &ObjectStore) -> Result<Option<Dir>, StoreError> {
    let root = store.private_derived_root_capability()?;
    tine_storage::open_existing_dir_nofollow(&root, COLD_HISTORY_DIRECTORY)
        .map_err(filesystem_error_without_collision)
}

/// The exact raw marker bytes, or `None` when no marker file exists.
///
/// Only the explicit repair path uses this: it needs the bytes as the audited
/// replacement guard even when they do not decode. Every ordinary read goes
/// through [`read_roots`], which still rejects a damaged marker.
fn read_root_marker_bytes(directory: &Dir) -> Result<Option<Vec<u8>>, StoreError> {
    super::object_store::read_optional_regular(
        directory,
        COLD_ROOT_MARKER,
        MAX_COLD_ROOT_BYTES,
        None,
    )
}

fn read_roots(directory: &Dir) -> Result<Option<(Vec<u8>, ColdHistoryRootsV1)>, StoreError> {
    let Some(bytes) = read_root_marker_bytes(directory)? else {
        return Ok(None);
    };
    let roots = decode_roots(&bytes)?;
    Ok(Some((bytes, roots)))
}

fn decode_roots(bytes: &[u8]) -> Result<ColdHistoryRootsV1, StoreError> {
    let roots: ColdHistoryRootsV1 = decode_canonical(bytes).map_err(cold_index_error)?;
    if roots.schema != COLD_SCHEMA_VERSION {
        return Err(cold_index_error(
            "cold history root marker is not the current schema",
        ));
    }
    // Reject a marker whose roots cannot be decoded before any caller can treat
    // it as authority.
    roots.object_root().map_err(cold_index_error)?;
    roots.manifest_root().map_err(cold_index_error)?;
    Ok(roots)
}

/// What the cold directory's *derived* root marker says about this archive.
///
/// The distinction this type exists to make: never-initialized absence is not
/// the same state as a lost marker over preserved packs. Conflating them is
/// what lets a fresh empty-based root be published over old history.
enum ColdRootState {
    /// No marker and no pack: cold history was never published here.
    NeverInitialized,
    /// A healthy current marker. Reaching this costs one bounded file read.
    Published {
        marker: Vec<u8>,
        roots: ColdHistoryRootsV1,
    },
    /// Self-describing packs survive but their derived root marker is gone.
    /// A named damaged state; `repair_cold_history_root` recovers it.
    RootLostWithPreservedPacks,
}

/// Whether this directory holds at least one pack file.
///
/// Only the marker-absent path calls this, and it stops at the first pack, so a
/// healthy open never pays for it and a damaged open pays one short listing.
fn contains_any_pack(directory: &Dir) -> Result<bool, StoreError> {
    for entry in directory
        .entries()
        .map_err(|error| cold_index_error(error.to_string()))?
    {
        let entry = entry.map_err(|error| cold_index_error(error.to_string()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if parse_pack_filename(name).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_root_state(directory: &Dir) -> Result<ColdRootState, StoreError> {
    if let Some((marker, roots)) = read_roots(directory)? {
        return Ok(ColdRootState::Published { marker, roots });
    }
    if contains_any_pack(directory)? {
        return Ok(ColdRootState::RootLostWithPreservedPacks);
    }
    Ok(ColdRootState::NeverInitialized)
}

// ---------------------------------------------------------------------------
// Ranged pack reads
// ---------------------------------------------------------------------------

/// Read exactly one packed record's payload, proving it against the record
/// header's digest. Knows nothing about what the payload means, so it serves
/// object records, manifest records and index descriptors alike -- and, later,
/// P5 capsule records.
fn read_pack_range(
    directory: &Dir,
    locator: ColdLocatorV1,
    payload_limit: u64,
) -> Result<Vec<u8>, String> {
    if locator.length > payload_limit.saturating_add(COLD_RECORD_HEADER_BYTES as u64) {
        return Err("cold record exceeds its class byte limit".into());
    }
    let name = pack_filename(locator.pack);
    match directory.symlink_metadata(&name) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!("cold pack {name} is not a regular no-follow file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(format!("cold pack {name} is missing"));
        }
        Err(error) => return Err(error.to_string()),
    }
    let mut file = tine_storage::open_file_nofollow(directory, &name).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            format!("cold pack {name} is missing")
        } else {
            error.to_string()
        }
    })?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err(format!("cold pack {name} is not a regular no-follow file"));
    }
    let end = locator
        .offset
        .checked_add(locator.length)
        .ok_or("cold locator range overflows")?;
    if end > metadata.len() {
        return Err(format!(
            "cold pack {name} is shorter than its locator range"
        ));
    }
    file.seek(SeekFrom::Start(locator.offset))
        .map_err(|error| error.to_string())?;
    let mut raw = vec![0_u8; locator.length as usize];
    file.read_exact(&mut raw)
        .map_err(|error| error.to_string())?;
    let (digest, payload_start) = parse_record_header(&raw)?;
    let payload = raw[payload_start..].to_vec();
    if ContentDigest::of(&payload) != digest {
        return Err("cold record bytes differ from their record digest".into());
    }
    Ok(payload)
}

/// Validate a packed record header and return `(payload digest, payload start)`.
fn parse_record_header(raw: &[u8]) -> Result<(ContentDigest, usize), String> {
    if raw.len() < COLD_RECORD_HEADER_BYTES {
        return Err("cold record is shorter than its header".into());
    }
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&raw[..32]);
    let mut length = [0_u8; 8];
    length.copy_from_slice(&raw[32..COLD_RECORD_HEADER_BYTES]);
    let payload_len = u64::from_be_bytes(length);
    if payload_len != (raw.len() - COLD_RECORD_HEADER_BYTES) as u64 {
        return Err("cold record header length differs from its locator range".into());
    }
    Ok((ContentDigest::from_bytes(digest), COLD_RECORD_HEADER_BYTES))
}

// ---------------------------------------------------------------------------
// The resolver's cold tier
// ---------------------------------------------------------------------------

/// Exact physical work one cold lookup performed.
///
/// This is the oracle for "bounded by the requested object's ranges and index
/// paths, not all packs/history": `pack_reads` is a constant 2 for an object
/// and 1 for a manifest however large history grows, and `index_nodes` grows
/// only with the map's depth.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ColdReadWork {
    pub(crate) index_nodes: usize,
    pub(crate) pack_reads: usize,
}

/// Counts sealed map-node reads without changing how they are read: the shared
/// point reader remains the only implementation.
struct CountingSealedReader {
    inner: SealedGenerationDirectory,
    reads: std::cell::Cell<usize>,
}

impl tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore for CountingSealedReader {
    fn read_sealed_accepted_object(
        &self,
        kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        address: ContentDigest,
    ) -> Result<Option<Vec<u8>>, tine_storage::sealed_accepted_index::SealedAcceptedIndexError>
    {
        self.reads.set(self.reads.get().saturating_add(1));
        tine_storage::sealed_accepted_index::SealedAcceptedIndexObjectStore::read_sealed_accepted_object(
            &self.inner, kind, address,
        )
    }

    fn publish_sealed_accepted_object(
        &mut self,
        _kind: tine_storage::sealed_accepted_index::SealedAcceptedObjectKind,
        _address: ContentDigest,
        _bytes: &[u8],
    ) -> Result<(), tine_storage::sealed_accepted_index::SealedAcceptedIndexError> {
        Err(
            tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Store(
                "the cold history reader is read-only".into(),
            ),
        )
    }
}

/// Point access to cold logical history. Carries no hot-tier authority: the
/// caller consults this only after the hot original name is absent.
pub(crate) struct ColdHistoryReader {
    directory: Dir,
    sealed: CountingSealedReader,
    roots: ColdHistoryRootsV1,
    pack_reads: std::cell::Cell<usize>,
}

impl ColdHistoryReader {
    /// `Ok(None)` when this archive has never published cold history. That is
    /// ordinary absence, never a refusal.
    ///
    /// A cold directory whose packs survive but whose derived root marker is
    /// gone is *not* absence: it refuses with the named repair condition
    /// [`StoreError::ColdHistoryRootMissing`], and
    /// [`repair_cold_history_root`] recovers it from the preserved packs. A
    /// healthy open costs one bounded marker read and enumerates nothing.
    pub(crate) fn open(store: &ObjectStore) -> Result<Option<Self>, StoreError> {
        let Some(directory) = open_existing_cold_directory(store)? else {
            return Ok(None);
        };
        match read_root_state(&directory)? {
            ColdRootState::NeverInitialized => Ok(None),
            ColdRootState::RootLostWithPreservedPacks => Err(StoreError::ColdHistoryRootMissing),
            ColdRootState::Published { roots, .. } => Self::from_parts(&directory, roots).map(Some),
        }
    }

    fn from_parts(directory: &Dir, roots: ColdHistoryRootsV1) -> Result<Self, StoreError> {
        Ok(Self {
            directory: directory
                .try_clone()
                .map_err(|error| cold_index_error(error.to_string()))?,
            sealed: CountingSealedReader {
                inner: SealedGenerationDirectory::open(directory).map_err(cold_index_error)?,
                reads: std::cell::Cell::new(0),
            },
            roots,
            pack_reads: std::cell::Cell::new(0),
        })
    }

    pub(crate) fn roots(&self) -> ColdHistoryRootsV1 {
        self.roots
    }

    /// Physical work performed since this reader was opened.
    pub(crate) fn work(&self) -> ColdReadWork {
        ColdReadWork {
            index_nodes: self.sealed.reads.get(),
            pack_reads: self.pack_reads.get(),
        }
    }

    fn read_range(&self, locator: ColdLocatorV1, limit: u64) -> Result<Vec<u8>, String> {
        self.pack_reads.set(self.pack_reads.get().saturating_add(1));
        read_pack_range(&self.directory, locator, limit)
    }

    /// The inner authenticated map holding every full key under one 128-bit
    /// prefix, or `None` when no object shares that prefix.
    fn prefix_map(
        &self,
        digest: ContentDigest,
        high: [u8; 16],
    ) -> Result<Option<AuthenticatedMapRootV1>, StoreError> {
        let root = self.roots.object_root().map_err(cold_index_error)?;
        let Some(value) = SealedAcceptedIndexReader::new(&self.sealed)
            .map_value(root, high)
            .map_err(|error| cold_object_error(digest, error.to_string()))?
        else {
            return Ok(None);
        };
        let locator =
            ColdLocatorV1::decode_value(value).map_err(|error| cold_object_error(digest, error))?;
        let bytes = self
            .read_range(locator, MAX_COLD_INNER_ROOT_BYTES)
            .map_err(|error| cold_object_error(digest, error))?;
        decode_inner_root(&bytes)
            .map(Some)
            .map_err(|error| cold_object_error(digest, error))
    }

    /// Locate one logical object without reading its payload.
    ///
    /// Work is bounded by this object's own two index paths plus one bounded
    /// descriptor read. No pack is enumerated, no unrelated history is touched,
    /// and no step depends on how many objects share this object's prefix.
    fn locate_object(&self, digest: ContentDigest) -> Result<Option<ColdLocatorV1>, StoreError> {
        let (high, low) = split_digest(digest);
        let Some(prefix_map) = self.prefix_map(digest, high)? else {
            return Ok(None);
        };
        let Some(value) = SealedAcceptedIndexReader::new(&self.sealed)
            .map_value(prefix_map, low)
            .map_err(|error| cold_object_error(digest, error.to_string()))?
        else {
            return Ok(None);
        };
        ColdLocatorV1::decode_value(value)
            .map(Some)
            .map_err(|error| cold_object_error(digest, error))
    }

    /// Resolve one logical object's exact canonical bytes.
    pub(crate) fn object_bytes(
        &self,
        digest: ContentDigest,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(locator) = self.locate_object(digest)? else {
            return Ok(None);
        };
        let payload = self
            .read_range(locator, MAX_OBJECT_BYTES as u64)
            .map_err(|error| cold_object_error(digest, error))?;
        if ContentDigest::of(&payload) != digest {
            return Err(cold_object_error(
                digest,
                "cold record resolves to another logical object",
            ));
        }
        Ok(Some(payload))
    }

    fn locate_manifest(&self, batch_id: BatchId) -> Result<Option<ColdLocatorV1>, StoreError> {
        let root = self.roots.manifest_root().map_err(cold_index_error)?;
        let Some(value) = SealedAcceptedIndexReader::new(&self.sealed)
            .map_value(root, batch_id.as_uuid().into_bytes())
            .map_err(|error| cold_manifest_error(batch_id, error.to_string()))?
        else {
            return Ok(None);
        };
        ColdLocatorV1::decode_value(value)
            .map(Some)
            .map_err(|error| cold_manifest_error(batch_id, error))
    }

    /// Resolve one batch manifest's exact canonical bytes.
    pub(crate) fn manifest_bytes(&self, batch_id: BatchId) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(locator) = self.locate_manifest(batch_id)? else {
            return Ok(None);
        };
        self.read_range(locator, MAX_MANIFEST_BYTES as u64)
            .map(Some)
            .map_err(|error| cold_manifest_error(batch_id, error))
    }

    /// Enumerate the committed cold manifest membership from the authenticated
    /// manifest map itself. Full-history reconstruction is the one consumer
    /// allowed to pay this lifetime-sized walk; using the sealed map keeps the
    /// root marker as the sole inventory and avoids interpreting pack or
    /// directory order as committed membership.
    pub(crate) fn manifest_batch_ids(&self) -> Result<BTreeSet<BatchId>, StoreError> {
        let root = self.roots.manifest_root().map_err(cold_index_error)?;
        let reader = SealedAcceptedIndexReader::new(&self.sealed);
        let mut pending = root.root.into_iter().collect::<Vec<_>>();
        let mut batches = BTreeSet::new();
        while let Some(link) = pending.pop() {
            let node = reader
                .read_map_node(link)
                .map_err(|error| cold_index_error(error.to_string()))?;
            let key = node.key.as_slice();
            let bytes: [u8; 16] = key
                .try_into()
                .map_err(|_| cold_index_error("cold manifest map contains a non-BatchId key"))?;
            if !batches.insert(BatchId::from_uuid(Uuid::from_bytes(bytes))) {
                return Err(cold_index_error(
                    "cold manifest map repeats a BatchId identity",
                ));
            }
            pending.extend(node.left);
            pending.extend(node.right);
        }
        if u64::try_from(batches.len()).ok() != Some(root.count) {
            return Err(cold_index_error(
                "cold manifest map traversal differs from its authenticated count",
            ));
        }
        Ok(batches)
    }
}

// ---------------------------------------------------------------------------
// Additive publication
// ---------------------------------------------------------------------------

/// One in-progress pack. Records are appended in memory up to the construction
/// target and the pack is published as soon as it is sealed, so publication
/// memory is bounded by that target plus one oversize record.
///
/// Deliberately generic over `(class, key, payload)` and ignorant of the object
/// model: this builder and [`read_pack_range`] are the physical pack/range seam
/// P5 capsule packing reuses rather than growing a twin. A new payload kind
/// needs only a new class byte, not a second packer.
struct ColdPackBuilder {
    pack: Uuid,
    bytes: Vec<u8>,
    entries: Vec<ColdPackFooterEntryV1>,
}

impl ColdPackBuilder {
    fn new() -> Self {
        Self {
            pack: Uuid::new_v4(),
            bytes: Vec::new(),
            entries: Vec::new(),
        }
    }

    fn append(&mut self, class: u8, key: Vec<u8>, payload: &[u8]) -> Result<ColdLocatorV1, String> {
        let offset = self.bytes.len() as u64;
        self.bytes
            .extend_from_slice(ContentDigest::of(payload).as_bytes());
        self.bytes
            .extend_from_slice(&(payload.len() as u64).to_be_bytes());
        self.bytes.extend_from_slice(payload);
        let length = self.bytes.len() as u64 - offset;
        if self.bytes.len() as u64 > MAX_COLD_PACK_BYTES {
            return Err("cold pack exceeds its physical record limit".into());
        }
        self.entries.push(ColdPackFooterEntryV1 {
            class,
            key,
            offset,
            length,
        });
        Ok(ColdLocatorV1 {
            pack: self.pack,
            offset,
            length,
        })
    }

    fn finish(mut self) -> Result<(Uuid, Vec<u8>), String> {
        let footer = encode_canonical(&ColdPackFooterV1 {
            schema: COLD_SCHEMA_VERSION,
            entries: self.entries,
        })?;
        if footer.len() as u64 > MAX_COLD_PACK_FOOTER_BYTES {
            return Err("cold pack footer exceeds its physical limit".into());
        }
        self.bytes.extend_from_slice(&footer);
        self.bytes
            .extend_from_slice(&(footer.len() as u64).to_be_bytes());
        self.bytes.extend_from_slice(&COLD_PACK_MAGIC);
        Ok((self.pack, self.bytes))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ColdPublicationOutcome {
    pub(crate) objects_published: usize,
    pub(crate) manifests_published: usize,
    pub(crate) objects_already_present: usize,
    pub(crate) manifests_already_present: usize,
    pub(crate) packs_published: usize,
}

/// A staged additive publication whose packs and index nodes are durable but
/// whose root marker has not been installed.
///
/// Everything staged is unreferenced residue until [`install_cold_roots`] runs.
/// A crash here leaves the predecessor root -- or no root at all -- exactly as
/// authoritative as it was.
pub(crate) struct StagedColdPublication {
    roots: ColdHistoryRootsV1,
    prior_marker: Option<Vec<u8>>,
    outcome: ColdPublicationOutcome,
}

impl StagedColdPublication {
    pub(crate) fn outcome(&self) -> ColdPublicationOutcome {
        self.outcome
    }
}

struct ColdPublicationSession<'a> {
    directory: &'a Dir,
    current: ColdPackBuilder,
    published_packs: usize,
    objects_published: usize,
    manifests_published: usize,
    object_entries: BTreeMap<[u8; 16], BTreeMap<[u8; 16], [u8; 32]>>,
    manifest_entries: BTreeMap<[u8; 16], [u8; 32]>,
}

impl<'a> ColdPublicationSession<'a> {
    fn new(directory: &'a Dir) -> Self {
        Self {
            directory,
            current: ColdPackBuilder::new(),
            published_packs: 0,
            objects_published: 0,
            manifests_published: 0,
            object_entries: BTreeMap::new(),
            manifest_entries: BTreeMap::new(),
        }
    }

    fn append(
        &mut self,
        class: u8,
        key: Vec<u8>,
        payload: &[u8],
    ) -> Result<ColdLocatorV1, StoreError> {
        let locator = self
            .current
            .append(class, key, payload)
            .map_err(cold_index_error)?;
        if self.current.bytes.len() >= COLD_PACK_TARGET_BYTES {
            self.seal_current_pack()?;
        }
        Ok(locator)
    }

    fn seal_current_pack(&mut self) -> Result<(), StoreError> {
        if self.current.entries.is_empty() {
            return Ok(());
        }
        let builder = std::mem::replace(&mut self.current, ColdPackBuilder::new());
        let (pack, bytes) = builder.finish().map_err(cold_index_error)?;
        tine_storage::DurableDirectoryPublication::open(self.directory)
            .map_err(filesystem_error_without_collision)?
            .publish_new_exact_single_writer(&pack_filename(pack), &bytes)
            .map_err(filesystem_error_without_collision)?;
        self.published_packs += 1;
        Ok(())
    }

    fn add_object(&mut self, digest: ContentDigest, payload: &[u8]) -> Result<(), StoreError> {
        if payload.len() > MAX_OBJECT_BYTES {
            return Err(cold_object_error(
                digest,
                "logical object exceeds the current object byte limit",
            ));
        }
        if ContentDigest::of(payload) != digest {
            return Err(cold_object_error(
                digest,
                "logical object bytes differ from their content address",
            ));
        }
        let (high, low) = split_digest(digest);
        let locator = self.append(COLD_CLASS_OBJECT, digest.as_bytes().to_vec(), payload)?;
        if self
            .object_entries
            .entry(high)
            .or_default()
            .insert(low, locator.to_bytes())
            .is_none()
        {
            self.objects_published += 1;
        }
        Ok(())
    }

    fn add_manifest(&mut self, batch_id: BatchId, payload: &[u8]) -> Result<(), StoreError> {
        if payload.len() > MAX_MANIFEST_BYTES {
            return Err(cold_manifest_error(
                batch_id,
                "logical manifest exceeds the current manifest byte limit",
            ));
        }
        // A `BatchId` is an identity, not a content address, so bind these
        // exact bytes to it before they are packed under that key.
        let manifest = super::OperationBatch::decode(payload)?;
        if manifest.batch_id() != batch_id {
            return Err(cold_manifest_conflict(
                batch_id,
                "these canonical manifest bytes belong to another batch id",
            ));
        }
        let key = batch_id.as_uuid().into_bytes();
        let locator = self.append(COLD_CLASS_MANIFEST, key.to_vec(), payload)?;
        if self
            .manifest_entries
            .insert(key, locator.to_bytes())
            .is_none()
        {
            self.manifests_published += 1;
        }
        Ok(())
    }
}

/// Stage an additive cold publication: pack bytes and index nodes become
/// durable, the root marker does not.
///
/// Ordering is the durability contract. Every payload record is sealed into a
/// durable pack before any index node can name it; every inner-root descriptor
/// is sealed before the outer map names it; every index node is durable before
/// [`install_cold_roots`] publishes the marker that names it.
fn stage_cold_publication(
    directory: &Dir,
    base: ColdHistoryRootsV1,
    prior_marker: Option<Vec<u8>>,
    mut session: ColdPublicationSession<'_>,
    objects_already_present: usize,
    manifests_already_present: usize,
) -> Result<StagedColdPublication, StoreError> {
    let mut object_root = base.object_root().map_err(cold_index_error)?;
    let mut manifest_root = base.manifest_root().map_err(cold_index_error)?;
    let mut object_count = base.object_count;
    let mut manifest_count = base.manifest_count;

    // 1. Payload bytes durable first.
    session.seal_current_pack()?;
    let object_entries = std::mem::take(&mut session.object_entries);
    let manifest_entries = std::mem::take(&mut session.manifest_entries);

    // 2. One inner authenticated map per touched 128-bit prefix, keyed by the
    //    exact low 128 bits. An existing prefix is extended, never replaced, so
    //    an additive publication can never lose a predecessor's entry -- and a
    //    prefix can hold arbitrarily many members, because it is a map.
    let mut staging = SealedGenerationStagingStore::open(directory).map_err(cold_index_error)?;
    let mut prefix_maps: Vec<([u8; 16], AuthenticatedMapRootV1)> =
        Vec::with_capacity(object_entries.len());
    for (high, lows) in object_entries {
        let mut prefix_map = match SealedAcceptedIndexReader::new(&staging)
            .map_value(object_root, high)
            .map_err(|error| cold_index_error(error.to_string()))?
        {
            Some(value) => {
                let locator = ColdLocatorV1::decode_value(value).map_err(cold_index_error)?;
                let bytes = read_pack_range(directory, locator, MAX_COLD_INNER_ROOT_BYTES)
                    .map_err(cold_index_error)?;
                decode_inner_root(&bytes).map_err(cold_index_error)?
            }
            None => AuthenticatedMapRootV1::empty(),
        };
        for (low, locator) in lows {
            let locator = ColdLocatorV1::from_bytes(locator).map_err(cold_index_error)?;
            let next = SealedAcceptedIndexWriter::new(&mut staging)
                .upsert_map(prefix_map, low, locator.value())
                .map_err(|error| cold_index_error(error.to_string()))?;
            if next.count != prefix_map.count {
                object_count = object_count
                    .checked_add(1)
                    .ok_or_else(|| cold_index_error("cold object count overflow"))?;
            }
            prefix_map = next;
        }
        prefix_maps.push((high, prefix_map));
    }

    // 3. Pack each new inner root as a fixed-size descriptor record, using the
    //    same physical pack machinery, and seal before the outer map names it.
    let mut descriptors: Vec<([u8; 16], ColdLocatorV1)> = Vec::with_capacity(prefix_maps.len());
    for (high, prefix_map) in &prefix_maps {
        let bytes = encode_inner_root(*prefix_map).map_err(cold_index_error)?;
        let locator = session.append(COLD_CLASS_INNER_ROOT, high.to_vec(), &bytes)?;
        descriptors.push((*high, locator));
    }
    session.seal_current_pack()?;

    let outcome = ColdPublicationOutcome {
        objects_published: session.objects_published,
        manifests_published: session.manifests_published,
        objects_already_present,
        manifests_already_present,
        packs_published: session.published_packs,
    };
    drop(session);

    // 4. Outer map and manifest map last; both name durable pack bytes.
    for (high, locator) in &descriptors {
        object_root = SealedAcceptedIndexWriter::new(&mut staging)
            .upsert_map(object_root, *high, locator.value())
            .map_err(|error| cold_index_error(error.to_string()))?;
    }
    for (key, locator) in &manifest_entries {
        let locator = ColdLocatorV1::from_bytes(*locator).map_err(cold_index_error)?;
        let next = SealedAcceptedIndexWriter::new(&mut staging)
            .upsert_map(manifest_root, *key, locator.value())
            .map_err(|error| cold_index_error(error.to_string()))?;
        if next.count != manifest_root.count {
            manifest_count = manifest_count
                .checked_add(1)
                .ok_or_else(|| cold_index_error("cold manifest count overflow"))?;
        }
        manifest_root = next;
    }
    staging.finish().map_err(cold_index_error)?;

    Ok(StagedColdPublication {
        roots: ColdHistoryRootsV1 {
            schema: COLD_SCHEMA_VERSION,
            objects: map_root_to_wire(object_root).map_err(cold_index_error)?,
            manifests: map_root_to_wire(manifest_root).map_err(cold_index_error)?,
            object_count,
            manifest_count,
        },
        prior_marker,
        outcome,
    })
}

/// Install the root marker last. Before this returns, every staged pack and
/// index node is unreferenced residue and the predecessor root is unchanged.
fn install_cold_roots(
    directory: &Dir,
    staged: StagedColdPublication,
) -> Result<ColdPublicationOutcome, StoreError> {
    let bytes = encode_canonical(&staged.roots).map_err(cold_index_error)?;
    if bytes.len() as u64 > MAX_COLD_ROOT_BYTES {
        return Err(cold_index_error(
            "cold history root marker exceeds its fixed codec size",
        ));
    }
    let publication = tine_storage::DurableDirectoryPublication::open(directory)
        .map_err(filesystem_error_without_collision)?;
    match staged.prior_marker {
        Some(existing) if existing == bytes => {}
        Some(existing) => publication
            .replace_exact(COLD_ROOT_MARKER, &existing, &bytes)
            .map_err(filesystem_error_without_collision)?,
        None => publication
            .publish_new_exact_single_writer(COLD_ROOT_MARKER, &bytes)
            .map_err(filesystem_error_without_collision)?,
    }
    Ok(staged.outcome)
}

/// The base this publication extends.
///
/// A lost root marker over preserved packs refuses here for the same reason it
/// refuses on read: publishing a fresh empty-based root would silently orphan
/// old history. Repair is explicit.
fn publication_base(directory: &Dir) -> Result<(Option<Vec<u8>>, ColdHistoryRootsV1), StoreError> {
    match read_root_state(directory)? {
        ColdRootState::Published { marker, roots } => Ok((Some(marker), roots)),
        ColdRootState::NeverInitialized => Ok((None, ColdHistoryRootsV1::empty())),
        ColdRootState::RootLostWithPreservedPacks => Err(StoreError::ColdHistoryRootMissing),
    }
}

/// Stage a caller-built session against the current roots without installing
/// the root marker.
fn stage_publication_from_session(
    directory: &Dir,
    session: ColdPublicationSession<'_>,
) -> Result<StagedColdPublication, StoreError> {
    let (prior_marker, base) = publication_base(directory)?;
    stage_cold_publication(directory, base, prior_marker, session, 0, 0)
}

/// Stage an additive publication of exact logical bytes without installing the
/// root marker.
///
/// A repeated identity is resolved by *bytes*, not by name presence: the shared
/// reader reconstructs what cold history already holds and compares it with the
/// incoming canonical bytes. Identical bytes are a counted no-op; different
/// bytes are a named conflict. The whole exactness pass runs before a single
/// record is appended, so a conflict leaves no residue at all -- the predecessor
/// root and the original bytes are exactly as authoritative as before.
fn stage_publication(
    directory: &Dir,
    objects: &BTreeMap<ContentDigest, Vec<u8>>,
    manifests: &BTreeMap<BatchId, Vec<u8>>,
) -> Result<StagedColdPublication, StoreError> {
    let (prior_marker, base) = publication_base(directory)?;
    let base_reader = ColdHistoryReader::from_parts(directory, base)?;

    let mut new_objects: Vec<(ContentDigest, &Vec<u8>)> = Vec::new();
    let mut new_manifests: Vec<(BatchId, &Vec<u8>)> = Vec::new();
    let mut objects_already_present = 0;
    let mut manifests_already_present = 0;
    for (digest, bytes) in objects {
        // `object_bytes` re-proves the stored payload against this exact
        // content address, so the comparison below is a byte comparison of two
        // fully reconstructed originals.
        match base_reader.object_bytes(*digest)? {
            Some(existing) if existing == *bytes => objects_already_present += 1,
            Some(_) => {
                return Err(cold_object_error(
                    *digest,
                    "cold history holds different bytes under this content address",
                ))
            }
            None => new_objects.push((*digest, bytes)),
        }
    }
    for (batch_id, bytes) in manifests {
        match base_reader.manifest_bytes(*batch_id)? {
            Some(existing) if existing == *bytes => manifests_already_present += 1,
            Some(_) => {
                return Err(cold_manifest_conflict(
                    *batch_id,
                    "cold history already holds a different canonical manifest under this batch id",
                ))
            }
            None => new_manifests.push((*batch_id, bytes)),
        }
    }
    drop(base_reader);

    let mut session = ColdPublicationSession::new(directory);
    for (digest, bytes) in new_objects {
        session.add_object(digest, bytes)?;
    }
    for (batch_id, bytes) in new_manifests {
        session.add_manifest(batch_id, bytes)?;
    }
    stage_cold_publication(
        directory,
        base,
        prior_marker,
        session,
        objects_already_present,
        manifests_already_present,
    )
}

/// Additively publish exact logical objects and batch manifests into immutable
/// cold packs and extend the point-addressable locator index.
///
/// Canonical bytes, content digests and `BatchId`s are preserved verbatim: this
/// is a physical relocation below the object model, never a re-encoding. Hot
/// originals are untouched -- this packet is additive and enables no retirement.
pub(crate) fn publish_cold_history(
    store: &ObjectStore,
    objects: &BTreeMap<ContentDigest, Vec<u8>>,
    manifests: &BTreeMap<BatchId, Vec<u8>>,
) -> Result<ColdPublicationOutcome, StoreError> {
    let directory = cold_directory(store)?;
    let staged = stage_publication(&directory, objects, manifests)?;
    install_cold_roots(&directory, staged)
}

/// Copy the exact hot originals named by these batches into cold history.
///
/// This is the production maintenance entry point: it reads each batch's
/// manifest and every object the manifest requires from the hot namespace and
/// republishes those exact bytes cold. It performs no deletion. R4 owns the
/// bounded resumable schedule that calls it; P5 owns enabling hot retirement
/// afterwards.
pub(crate) fn publish_cold_history_for_batches(
    store: &ObjectStore,
    batches: &BTreeSet<BatchId>,
) -> Result<ColdPublicationOutcome, StoreError> {
    let mut objects = BTreeMap::new();
    let mut manifests = BTreeMap::new();
    for batch_id in batches {
        let manifest_bytes = store.read_manifest_bytes(*batch_id)?;
        let manifest = super::OperationBatch::decode(&manifest_bytes)?;
        for descriptor in manifest.required_objects() {
            let digest = descriptor.content_digest();
            if objects.contains_key(&digest) {
                continue;
            }
            objects.insert(digest, store.read_object_bytes(digest)?);
        }
        manifests.insert(*batch_id, manifest_bytes);
    }
    publish_cold_history(store, &objects, &manifests)
}

/// One logical record recovered from a pack footer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColdInventoryKey {
    Object(ContentDigest),
    Manifest(BatchId),
}

/// Enumerate every logical record a pack file declares.
///
/// This is a maintenance/repair path only. Its cost is the pack inventory, not
/// the graph or the index: no ordinary read reaches it.
fn read_pack_inventory(
    directory: &Dir,
    pack: Uuid,
) -> Result<Vec<(ColdInventoryKey, ColdLocatorV1)>, StoreError> {
    let name = pack_filename(pack);
    let mut file = tine_storage::open_file_nofollow(directory, &name)
        .map_err(|error| cold_index_error(format!("cold pack {name}: {error}")))?;
    let length = file
        .metadata()
        .map_err(|error| cold_index_error(error.to_string()))?
        .len();
    let trailer = (COLD_PACK_MAGIC.len() + 8) as u64;
    if length < trailer || length > MAX_COLD_PACK_BYTES {
        return Err(cold_index_error(format!(
            "cold pack {name} is not a current pack file"
        )));
    }
    file.seek(SeekFrom::Start(length - trailer))
        .map_err(|error| cold_index_error(error.to_string()))?;
    let mut tail = [0_u8; 16];
    file.read_exact(&mut tail)
        .map_err(|error| cold_index_error(error.to_string()))?;
    if tail[8..] != COLD_PACK_MAGIC {
        return Err(cold_index_error(format!(
            "cold pack {name} has no current pack trailer"
        )));
    }
    let mut footer_len = [0_u8; 8];
    footer_len.copy_from_slice(&tail[..8]);
    let footer_len = u64::from_be_bytes(footer_len);
    if footer_len > MAX_COLD_PACK_FOOTER_BYTES || footer_len + trailer > length {
        return Err(cold_index_error(format!(
            "cold pack {name} footer length is impossible"
        )));
    }
    file.seek(SeekFrom::Start(length - trailer - footer_len))
        .map_err(|error| cold_index_error(error.to_string()))?;
    let mut footer_bytes = vec![0_u8; footer_len as usize];
    file.read_exact(&mut footer_bytes)
        .map_err(|error| cold_index_error(error.to_string()))?;
    let footer: ColdPackFooterV1 = decode_canonical(&footer_bytes).map_err(cold_index_error)?;
    if footer.schema != COLD_SCHEMA_VERSION {
        return Err(cold_index_error(format!(
            "cold pack {name} footer is not the current schema"
        )));
    }
    let mut inventory = Vec::new();
    for entry in footer.entries {
        let key = match (entry.class, entry.key.len()) {
            (COLD_CLASS_OBJECT, 32) => {
                let mut bytes = [0_u8; 32];
                bytes.copy_from_slice(&entry.key);
                ColdInventoryKey::Object(ContentDigest::from_bytes(bytes))
            }
            (COLD_CLASS_MANIFEST, 16) => {
                let mut bytes = [0_u8; 16];
                bytes.copy_from_slice(&entry.key);
                ColdInventoryKey::Manifest(BatchId::from_uuid(Uuid::from_bytes(bytes)))
            }
            // Inner-root descriptors are index bytes, not logical history. A
            // rebuild derives fresh prefix maps from the object records.
            (COLD_CLASS_INNER_ROOT, 16) => continue,
            _ => {
                return Err(cold_index_error(format!(
                    "cold pack {name} footer names an unknown record class"
                )))
            }
        };
        inventory.push((
            key,
            ColdLocatorV1 {
                pack,
                offset: entry.offset,
                length: entry.length,
            },
        ));
    }
    Ok(inventory)
}

fn cold_pack_names(directory: &Dir) -> Result<BTreeSet<Uuid>, StoreError> {
    let mut packs = BTreeSet::new();
    for entry in directory
        .entries()
        .map_err(|error| cold_index_error(error.to_string()))?
    {
        let entry = entry.map_err(|error| cold_index_error(error.to_string()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pack) = parse_pack_filename(name) else {
            continue;
        };
        super::object_store::require_regular_entry(
            &entry
                .file_type()
                .map_err(|error| cold_index_error(error.to_string()))?,
            name,
        )?;
        packs.insert(pack);
    }
    Ok(packs)
}

/// Every logical record the preserved packs declare, resolved to one exact
/// placement each.
#[derive(Default)]
struct ColdInventory {
    objects: BTreeMap<ContentDigest, ColdLocatorV1>,
    manifests: BTreeMap<BatchId, ColdLocatorV1>,
    packs: usize,
}

/// Enumerate every logical record the preserved packs declare, applying the
/// same exactness rule as publication to repeated identities.
///
/// This is a maintenance/repair path only. Its cost is the pack inventory, not
/// the graph or the index: no ordinary read reaches it. An object appearing at
/// several placements is bound by its content address, which each ranged read
/// re-proves, so any placement is the same bytes. A `BatchId` appearing twice is
/// the same logical manifest only if its *bytes* match; a genuine conflict is
/// refused by name rather than resolved by pack ordering.
fn collect_cold_inventory(directory: &Dir) -> Result<ColdInventory, StoreError> {
    let mut inventory = ColdInventory::default();
    for pack in cold_pack_names(directory)? {
        inventory.packs += 1;
        for (key, locator) in read_pack_inventory(directory, pack)? {
            match key {
                ColdInventoryKey::Object(digest) => {
                    let payload = read_pack_range(directory, locator, MAX_OBJECT_BYTES as u64)
                        .map_err(|error| cold_object_error(digest, error))?;
                    if ContentDigest::of(&payload) != digest {
                        return Err(cold_object_error(
                            digest,
                            "a packed record is filed under another object's content address",
                        ));
                    }
                    inventory.objects.entry(digest).or_insert(locator);
                }
                ColdInventoryKey::Manifest(batch_id) => {
                    let payload = read_pack_range(directory, locator, MAX_MANIFEST_BYTES as u64)
                        .map_err(|error| cold_manifest_error(batch_id, error))?;
                    let manifest = super::OperationBatch::decode(&payload)?;
                    if manifest.batch_id() != batch_id {
                        return Err(cold_manifest_conflict(
                            batch_id,
                            "a packed manifest record is filed under another batch id",
                        ));
                    }
                    if let Some(previous) = inventory.manifests.get(&batch_id) {
                        let existing =
                            read_pack_range(directory, *previous, MAX_MANIFEST_BYTES as u64)
                                .map_err(|error| cold_manifest_error(batch_id, error))?;
                        if existing != payload {
                            return Err(cold_manifest_conflict(
                                batch_id,
                                "preserved packs hold two different canonical manifests for this batch id",
                            ));
                        }
                        continue;
                    }
                    inventory.manifests.insert(batch_id, locator);
                }
            }
        }
    }
    Ok(inventory)
}

/// The result of an explicit cold-history root repair.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ColdRepairOutcome {
    /// True only when a damaged state was actually recovered. A healthy or
    /// never-initialized archive reports `false` and changes nothing.
    pub(crate) repaired: bool,
    pub(crate) packs_scanned: usize,
    pub(crate) objects_recovered: usize,
    pub(crate) manifests_recovered: usize,
}

/// Recover a lost cold root marker from the preserved self-describing packs.
///
/// This is the explicit damaged-state path named by
/// [`StoreError::ColdHistoryRootMissing`], and the only place cold history is
/// enumerated on a recovery. It reuses the surviving records exactly where they
/// already are -- no byte is rewritten, relocated or re-encoded -- and rebuilds
/// only the derived locator index (D-3). A healthy or never-initialized archive
/// is left untouched and reports `repaired: false`.
pub(crate) fn repair_cold_history_root(
    store: &ObjectStore,
) -> Result<ColdRepairOutcome, StoreError> {
    let Some(directory) = open_existing_cold_directory(store)? else {
        return Ok(ColdRepairOutcome::default());
    };
    // Capture the exact marker bytes once. Ordinary reads still reject a
    // damaged root; explicit repair derives replacement roots from the original
    // packs and uses these bytes only as the audited replacement guard, never as
    // history. Missing and torn markers are the same damaged class here: both
    // are derived state over surviving self-describing packs.
    let prior_marker = read_root_marker_bytes(&directory)?;
    if prior_marker
        .as_deref()
        .is_some_and(|bytes| decode_roots(bytes).is_ok())
    {
        return Ok(ColdRepairOutcome::default());
    }
    if !contains_any_pack(&directory)? {
        return if prior_marker.is_none() {
            // Never initialized: ordinary absence, nothing to repair.
            Ok(ColdRepairOutcome::default())
        } else {
            // A damaged marker with no surviving pack is never replaced with a
            // fresh empty history: there is nothing to rebuild it from.
            Err(cold_index_error(
                "damaged cold root has no preserved packs to rebuild from",
            ))
        };
    }
    let inventory = collect_cold_inventory(&directory)?;
    let mut session = ColdPublicationSession::new(&directory);
    for (digest, locator) in &inventory.objects {
        let (high, low) = split_digest(*digest);
        session
            .object_entries
            .entry(high)
            .or_default()
            .insert(low, locator.to_bytes());
        session.objects_published += 1;
    }
    for (batch_id, locator) in &inventory.manifests {
        session
            .manifest_entries
            .insert(batch_id.as_uuid().into_bytes(), locator.to_bytes());
        session.manifests_published += 1;
    }
    let staged = stage_cold_publication(
        &directory,
        ColdHistoryRootsV1::empty(),
        prior_marker,
        session,
        0,
        0,
    )?;
    install_cold_roots(&directory, staged)?;
    Ok(ColdRepairOutcome {
        repaired: true,
        packs_scanned: inventory.packs,
        objects_recovered: inventory.objects.len(),
        manifests_recovered: inventory.manifests.len(),
    })
}

/// Republish every cold logical record into fresh packs and rebuild every root
/// from the pack footers alone.
///
/// This proves two things the design requires and one it forbids relaxing:
/// logical identity is independent of physical placement (the same digests and
/// `BatchId`s resolve to the same bytes at new offsets in new packs); the
/// locator index is genuinely derived, disposable state (D-3) because it is
/// rebuilt here without consulting the previous roots; and publication remains
/// publish-new-before-retire-old -- the predecessor packs are left in place,
/// because retiring them needs P5's retention proofs.
pub(crate) fn repack_cold_history(
    store: &ObjectStore,
) -> Result<ColdPublicationOutcome, StoreError> {
    let Some(directory) = open_existing_cold_directory(store)? else {
        return Ok(ColdPublicationOutcome::default());
    };
    let prior_marker = match read_root_state(&directory)? {
        ColdRootState::NeverInitialized => return Ok(ColdPublicationOutcome::default()),
        // A repack derives roots, it does not recover them: repairing a lost
        // marker is the explicit, separately named operation.
        ColdRootState::RootLostWithPreservedPacks => {
            return Err(StoreError::ColdHistoryRootMissing)
        }
        ColdRootState::Published { marker, .. } => Some(marker),
    };
    let inventory = collect_cold_inventory(&directory)?;
    let mut session = ColdPublicationSession::new(&directory);
    for (digest, locator) in &inventory.objects {
        let payload = read_pack_range(&directory, *locator, MAX_OBJECT_BYTES as u64)
            .map_err(|error| cold_object_error(*digest, error))?;
        session.add_object(*digest, &payload)?;
    }
    for (batch_id, locator) in &inventory.manifests {
        let payload = read_pack_range(&directory, *locator, MAX_MANIFEST_BYTES as u64)
            .map_err(|error| cold_manifest_error(*batch_id, error))?;
        session.add_manifest(*batch_id, &payload)?;
    }
    let staged = stage_cold_publication(
        &directory,
        ColdHistoryRootsV1::empty(),
        prior_marker,
        session,
        0,
        0,
    )?;
    install_cold_roots(&directory, staged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::{
        BatchCausalDot, BatchInspection, BatchOrigin, CausalPeerId, CrdtPeerCounter, CrdtPeerId,
        DeviceId, DocumentDependencies, DocumentId, FrontierV2, LineageDigest, ObjectKind,
        OperationObject, PreparedBatch, SemanticEffectDigest, SessionId, WorkspaceId,
    };

    struct TestArchive {
        root: std::path::PathBuf,
        store: ObjectStore,
    }

    impl TestArchive {
        fn open(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!("tine-cold-{label}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let store =
                ObjectStore::open(&root.join("archive"), WorkspaceId::from_uuid(uuid(1))).unwrap();
            Self { root, store }
        }

        fn cold_directory(&self) -> std::path::PathBuf {
            self.store.root_path().join(COLD_HISTORY_DIRECTORY)
        }

        /// Remove one batch's hot originals, exactly the way a completed R2
        /// retirement eventually will. Cold history must still reconstruct
        /// every original byte and prove every digest.
        fn remove_hot_originals(&self, batch: &PreparedBatch) {
            let archive = self.store.root_path();
            std::fs::remove_file(
                archive
                    .join("batches")
                    .join(format!("{}.manifest", batch.manifest().batch_id())),
            )
            .unwrap();
            for object in batch.objects() {
                let digest = ContentDigest::of(&object.encode().unwrap());
                std::fs::remove_file(archive.join("objects").join(format!("{digest}.object")))
                    .unwrap();
            }
        }
    }

    impl Drop for TestArchive {
        fn drop(&mut self) {
            crate::test_support::remove_dir_all(std::mem::take(&mut self.root));
        }
    }

    fn uuid(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn sample_batch(workspace_id: WorkspaceId, seed: u128) -> PreparedBatch {
        let semantic_payload = format!("semantic effect payload {seed}").into_bytes();
        let semantic = OperationObject::new(
            workspace_id,
            DocumentId::from_uuid(uuid(0x10_0000 + seed)),
            ObjectKind::SemanticEffect,
            semantic_payload.clone(),
        )
        .unwrap();
        let update = OperationObject::new(
            workspace_id,
            DocumentId::from_uuid(uuid(0x20_0000 + seed)),
            ObjectKind::CrdtUpdate,
            format!("crdt update payload {seed} {}", "x".repeat(64)).into_bytes(),
        )
        .unwrap();
        let objects = vec![semantic, update];
        let descriptors = objects
            .iter()
            .map(|object| object.descriptor().unwrap())
            .collect();
        let device = DeviceId::from_uuid(uuid(30));
        let frontier = FrontierV2::new(vec![DocumentDependencies::new(
            DocumentId::from_uuid(uuid(0x20_0000 + seed)),
            vec![CrdtPeerCounter::new(CrdtPeerId::from_u64(8), 12)],
            Vec::new(),
        )
        .unwrap()])
        .unwrap();
        let manifest = crate::oplog::OperationBatch::new_with_causality(
            workspace_id,
            LineageDigest::of(b"cold-history-lineage"),
            BatchId::from_uuid(uuid(0x1000 + seed)),
            device,
            SessionId::from_uuid(uuid(31)),
            BatchOrigin::LocalMutation,
            BatchCausalDot::new(
                CausalPeerId::from_key(crate::oplog::WriterIncarnationId::fixture_for_device(
                    device,
                )),
                u64::try_from(seed).unwrap() + 1,
            )
            .unwrap(),
            Vec::new(),
            frontier,
            SemanticEffectDigest::of(&semantic_payload),
            descriptors,
        )
        .unwrap();
        PreparedBatch::new(manifest, objects).unwrap()
    }

    fn publish_batches(
        archive: &TestArchive,
        seeds: impl IntoIterator<Item = u128>,
    ) -> Vec<PreparedBatch> {
        let batches: Vec<_> = seeds
            .into_iter()
            .map(|seed| sample_batch(archive.store.workspace_id(), seed))
            .collect();
        for batch in &batches {
            archive.store.publish_prepared(batch).unwrap();
        }
        batches
    }

    fn relocate(archive: &TestArchive, batches: &[PreparedBatch]) -> ColdPublicationOutcome {
        let roster = batches
            .iter()
            .map(|batch| batch.manifest().batch_id())
            .collect();
        archive
            .store
            .publish_cold_history_for_batches(&roster)
            .unwrap()
    }

    #[test]
    fn cold_history_resolves_exact_original_bytes_after_hot_originals_are_removed() {
        let archive = TestArchive::open("reconstruct");
        let batches = publish_batches(&archive, 0..6);
        let outcome = relocate(&archive, &batches);
        assert_eq!(outcome.objects_published, 12);
        assert_eq!(outcome.manifests_published, 6);

        // Qualify the resolver against a fixture where the covered hot
        // originals no longer exist at all.
        for batch in &batches {
            archive.remove_hot_originals(batch);
        }

        for batch in &batches {
            let batch_id = batch.manifest().batch_id();
            let manifest_bytes = archive
                .store
                .resolve_logical_manifest_bytes(batch_id)
                .unwrap();
            assert_eq!(manifest_bytes, batch.manifest().encode().unwrap());
            let resolved = archive
                .store
                .resolve_logical_manifest(batch_id)
                .unwrap()
                .unwrap();
            assert_eq!(resolved.batch_id(), batch_id);

            for object in batch.objects() {
                let bytes = object.encode().unwrap();
                let digest = ContentDigest::of(&bytes);
                assert_eq!(
                    archive.store.resolve_logical_object_bytes(digest).unwrap(),
                    bytes,
                    "cold history must return byte-identical originals"
                );
                let object = archive.store.resolve_logical_object(digest).unwrap();
                assert_eq!(ContentDigest::of(&object.encode().unwrap()), digest);
                assert!(archive.store.contains_logical_object(digest).unwrap());
            }

            // Full replay of the batch: every required object reconstructs and
            // the whole batch validates, with no hot original left on disk.
            match archive
                .store
                .inspect_batch_with_cold_history(batch_id)
                .unwrap()
            {
                BatchInspection::Ready(validated) => {
                    assert_eq!(validated.manifest().batch_id(), batch_id);
                }
                other => panic!("expected a cold-resolved Ready batch, got {other:?}"),
            }
        }

        // The ordinary hot-only reader still reports honest absence and has
        // touched no pack byte.
        assert_eq!(
            archive
                .store
                .inspect_batch(batches[0].manifest().batch_id())
                .unwrap(),
            BatchInspection::Absent
        );
    }

    #[test]
    fn ordinary_hot_reads_never_touch_cold_history() {
        let archive = TestArchive::open("hot-only");
        let batches = publish_batches(&archive, 0..3);
        relocate(&archive, &batches);
        let before = archive.store.instrumentation();

        for batch in &batches {
            assert!(matches!(
                archive
                    .store
                    .inspect_batch(batch.manifest().batch_id())
                    .unwrap(),
                BatchInspection::Ready(_)
            ));
            for object in batch.objects() {
                let digest = ContentDigest::of(&object.encode().unwrap());
                archive.store.read_object(digest).unwrap();
                archive.store.read_object_bytes(digest).unwrap();
            }
            archive
                .store
                .read_manifest(batch.manifest().batch_id())
                .unwrap()
                .unwrap();
        }
        let after = archive.store.instrumentation();
        assert_eq!(after.cold_object_reads, before.cold_object_reads);
        assert_eq!(after.cold_manifest_reads, before.cold_manifest_reads);
        assert_eq!(after.cold_object_reads, 0);
        assert_eq!(after.cold_manifest_reads, 0);

        // With hot originals intact the resolver also stays hot: relocation is
        // additive, so nothing forces a cold read.
        for batch in &batches {
            for object in batch.objects() {
                let digest = ContentDigest::of(&object.encode().unwrap());
                archive.store.resolve_logical_object_bytes(digest).unwrap();
            }
        }
        assert_eq!(archive.store.instrumentation().cold_object_reads, 0);
    }

    #[test]
    fn full_sha256_key_domains_stay_full_across_the_composed_maps() {
        let archive = TestArchive::open("full-keys");
        let batches = publish_batches(&archive, 0..2);
        relocate(&archive, &batches);
        let first = batches[0].objects()[0].encode().unwrap();
        let second = batches[1].objects()[0].encode().unwrap();
        let first_digest = ContentDigest::of(&first);
        let second_digest = ContentDigest::of(&second);
        let (first_high, first_low) = split_digest(first_digest);
        let (_, second_low) = split_digest(second_digest);

        let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
        // Four objects across two batches, four distinct 128-bit prefixes: the
        // outer map counts prefixes, the exact-key total counts full keys.
        assert_eq!(reader.roots().object_count(), 4);
        assert_eq!(reader.roots().objects.count, 4);
        let prefix_map = reader
            .prefix_map(first_digest, first_high)
            .unwrap()
            .expect("the high half addresses this object's prefix map");
        assert_eq!(prefix_map.count, 1);

        // The low 128 bits are consulted, not discarded: a probe that keeps the
        // high half and changes only the low half is honest absence, never the
        // neighbouring object's bytes.
        let mut probe = [0_u8; 32];
        probe[..16].copy_from_slice(&first_high);
        probe[16..].copy_from_slice(&second_low);
        assert_eq!(
            reader
                .locate_object(ContentDigest::from_bytes(probe))
                .unwrap(),
            None
        );
        probe[16..].copy_from_slice(&first_low);
        probe[31] ^= 0xff;
        assert_eq!(
            reader
                .locate_object(ContentDigest::from_bytes(probe))
                .unwrap(),
            None
        );
        // The high 128 bits are equally load-bearing.
        let mut probe = *first_digest.as_bytes();
        probe[0] ^= 0xff;
        assert_eq!(
            reader
                .locate_object(ContentDigest::from_bytes(probe))
                .unwrap(),
            None
        );
        // Only the exact full 256-bit key resolves the exact original bytes.
        assert_eq!(reader.object_bytes(first_digest).unwrap(), Some(first));
        assert_eq!(reader.object_bytes(second_digest).unwrap(), Some(second));
    }

    /// Index-domain test: many *keys* sharing one 128-bit prefix.
    ///
    /// This asserts nothing about the fixture payloads' hashes -- a SHA-256
    /// prefix collision cannot be manufactured. It exercises the index boundary
    /// directly, by publishing many distinct full keys that share a high half,
    /// which is exactly the shape a serialized-list bucket with a byte cap could
    /// not represent. Composing a second authenticated map removes the question:
    /// an outer entry holds a whole map, so occupancy is unbounded and lookup
    /// cost is a map path, not a scan.
    #[test]
    fn many_keys_sharing_one_128_bit_prefix_stay_independently_addressable() {
        const SHARED: usize = 2048;
        let archive = TestArchive::open("shared-prefix");
        let directory = cold_directory(&archive.store).unwrap();
        let high = [0xa5_u8; 16];
        let mut expected: BTreeMap<[u8; 16], Vec<u8>> = BTreeMap::new();
        let mut session = ColdPublicationSession::new(&directory);
        for index in 0..SHARED {
            let mut low = [0x11_u8; 16];
            low[..8].copy_from_slice(&(index as u64).to_be_bytes());
            let mut key = [0_u8; 32];
            key[..16].copy_from_slice(&high);
            key[16..].copy_from_slice(&low);
            let payload = format!("shared-prefix payload {index}").into_bytes();
            let locator = session
                .append(COLD_CLASS_OBJECT, key.to_vec(), &payload)
                .unwrap();
            session
                .object_entries
                .entry(high)
                .or_default()
                .insert(low, locator.to_bytes());
            session.objects_published += 1;
            expected.insert(low, payload);
        }
        let staged = stage_publication_from_session(&directory, session).unwrap();
        install_cold_roots(&directory, staged).unwrap();

        let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
        // One outer entry holding one inner map of 512 exact low halves. There
        // is no bucket list, no byte cap and no fixed occupancy refusal.
        assert_eq!(reader.roots().objects.count, 1);
        assert_eq!(reader.roots().object_count(), SHARED as u64);
        let sample = {
            let mut key = [0_u8; 32];
            key[..16].copy_from_slice(&high);
            key[16..].copy_from_slice(expected.keys().next().unwrap());
            ContentDigest::from_bytes(key)
        };
        assert_eq!(
            reader.prefix_map(sample, high).unwrap().unwrap().count,
            SHARED as u64
        );

        // Fail-before, made structural rather than historical: the replaced
        // representation serialized one prefix as a canonical list of
        // `(sha256[16..32], locator)` pairs under a 64 KiB codec ceiling. This
        // fixture's prefix cannot be expressed that way at all, which is the
        // whole point -- a hash prefix has no small fixed number of members.
        let as_serialized_list = encode_canonical(
            &expected
                .keys()
                .map(|low| (*low, [0_u8; 32]))
                .collect::<Vec<([u8; 16], [u8; 32])>>(),
        )
        .unwrap()
        .len();
        assert!(
            as_serialized_list > 64 * 1024,
            "a serialized prefix list of {SHARED} entries is {as_serialized_list} bytes, \
             which must exceed the replaced 64 KiB bucket ceiling for this to be a real fixture"
        );

        for (low, payload) in &expected {
            let mut key = [0_u8; 32];
            key[..16].copy_from_slice(&high);
            key[16..].copy_from_slice(low);
            let locator = reader
                .locate_object(ContentDigest::from_bytes(key))
                .unwrap()
                .expect("every shared-prefix key is addressable by its own low half");
            assert_eq!(
                &read_pack_range(&reader.directory, locator, MAX_OBJECT_BYTES as u64).unwrap(),
                payload,
                "a shared-prefix key must resolve to its own record"
            );
        }

        // A non-member low half under the same prefix is honest absence.
        let mut absent = [0_u8; 32];
        absent[..16].copy_from_slice(&high);
        absent[16..].copy_from_slice(&[0xfe_u8; 16]);
        assert_eq!(
            reader
                .locate_object(ContentDigest::from_bytes(absent))
                .unwrap(),
            None
        );

        // Lookup cost under a 512-member prefix is still one descriptor read
        // plus one payload read, and the index path is sublinear in occupancy.
        let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
        let last = {
            let mut key = [0_u8; 32];
            key[..16].copy_from_slice(&high);
            key[16..].copy_from_slice(expected.keys().next_back().unwrap());
            ContentDigest::from_bytes(key)
        };
        reader.locate_object(last).unwrap().unwrap();
        let work = reader.work();
        assert_eq!(work.pack_reads, 1, "one inner-root descriptor read");
        assert!(
            work.index_nodes * 4 < SHARED,
            "index path {} is not sublinear in {SHARED} shared-prefix members",
            work.index_nodes
        );

        // These synthetic index keys are not their payloads' content addresses,
        // so the resolver still refuses to hand the bytes back under them: the
        // index composition never launders identity.
        assert!(matches!(
            reader.object_bytes(sample).unwrap_err(),
            StoreError::ColdObjectUnavailable { .. }
        ));
    }

    #[test]
    fn lookup_work_stays_bounded_as_unrelated_history_grows() {
        let archive = TestArchive::open("bounded");
        let probe = publish_batches(&archive, 0..1);
        relocate(&archive, &probe);
        let digest = ContentDigest::of(&probe[0].objects()[0].encode().unwrap());
        let batch_id = probe[0].manifest().batch_id();

        let measure = |archive: &TestArchive| {
            let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
            reader.object_bytes(digest).unwrap().unwrap();
            let object = reader.work();
            let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
            reader.manifest_bytes(batch_id).unwrap().unwrap();
            (object, reader.work())
        };

        let (small_object, small_manifest) = measure(&archive);
        for chunk in 0..8u128 {
            let batches = publish_batches(&archive, (chunk * 64 + 1)..(chunk * 64 + 65));
            relocate(&archive, &batches);
        }
        let roots = archive.store.cold_history_roots().unwrap().unwrap();
        assert_eq!(roots.object_count(), 2 * 512 + 2);
        assert_eq!(roots.manifest_count(), 513);

        let (large_object, large_manifest) = measure(&archive);
        // Byte range reads are constant: one bucket plus one record for an
        // object, one record for a manifest, whatever the history size.
        assert_eq!(small_object.pack_reads, 2);
        assert_eq!(large_object.pack_reads, 2);
        assert_eq!(small_manifest.pack_reads, 1);
        assert_eq!(large_manifest.pack_reads, 1);
        // Index-path reads grow only with map depth, never with history.
        assert!(
            large_object.index_nodes <= small_object.index_nodes + 24,
            "object index path grew from {} to {} across 512 unrelated objects",
            small_object.index_nodes,
            large_object.index_nodes
        );
        assert!(
            large_object.index_nodes * 8 < usize::try_from(roots.object_count()).unwrap(),
            "index path {} is not sublinear in {} objects",
            large_object.index_nodes,
            roots.object_count()
        );
    }

    #[test]
    fn duplicate_relocation_is_a_no_op_and_preserves_the_predecessor_root() {
        let archive = TestArchive::open("duplicates");
        let batches = publish_batches(&archive, 0..4);
        let first = relocate(&archive, &batches);
        assert_eq!(first.objects_published, 8);
        assert_eq!(first.objects_already_present, 0);
        let roots = archive.store.cold_history_roots().unwrap().unwrap();

        let second = relocate(&archive, &batches);
        assert_eq!(second.objects_published, 0);
        assert_eq!(second.manifests_published, 0);
        assert_eq!(second.objects_already_present, 8);
        assert_eq!(second.manifests_already_present, 4);
        assert_eq!(second.packs_published, 0);
        assert_eq!(archive.store.cold_history_roots().unwrap().unwrap(), roots);

        // A later additive publication extends the same roots without
        // disturbing the predecessor's entries.
        let more = publish_batches(&archive, 4..6);
        relocate(&archive, &more);
        let extended = archive.store.cold_history_roots().unwrap().unwrap();
        assert_eq!(extended.object_count(), 12);
        assert_eq!(extended.manifest_count(), 6);
        for batch in &batches {
            archive.remove_hot_originals(batch);
            for object in batch.objects() {
                let bytes = object.encode().unwrap();
                assert_eq!(
                    archive
                        .store
                        .resolve_logical_object_bytes(ContentDigest::of(&bytes))
                        .unwrap(),
                    bytes
                );
            }
        }
    }

    #[test]
    fn an_interrupted_additive_publication_never_becomes_authority() {
        let archive = TestArchive::open("interrupted");
        let installed = publish_batches(&archive, 0..2);
        relocate(&archive, &installed);
        let root_before = archive.store.cold_history_roots().unwrap().unwrap();

        // Stage a second publication's packs and index nodes durably, then
        // stop before the root marker -- exactly the crash window.
        let pending = publish_batches(&archive, 2..4);
        let directory = cold_directory(&archive.store).unwrap();
        let mut objects = BTreeMap::new();
        let mut manifests = BTreeMap::new();
        for batch in &pending {
            manifests.insert(
                batch.manifest().batch_id(),
                batch.manifest().encode().unwrap(),
            );
            for object in batch.objects() {
                let bytes = object.encode().unwrap();
                objects.insert(ContentDigest::of(&bytes), bytes);
            }
        }
        let staged = stage_publication(&directory, &objects, &manifests).unwrap();
        assert_eq!(staged.outcome().objects_published, 4);
        drop(staged);

        // Residue exists on disk, and confers nothing.
        assert!(cold_pack_names(&directory).unwrap().len() >= 2);
        assert_eq!(
            archive.store.cold_history_roots().unwrap().unwrap(),
            root_before
        );
        for batch in &pending {
            archive.remove_hot_originals(batch);
            let batch_id = batch.manifest().batch_id();
            assert!(matches!(
                archive.store.resolve_logical_manifest(batch_id).unwrap(),
                None
            ));
            assert!(matches!(
                archive
                    .store
                    .inspect_batch_with_cold_history(batch_id)
                    .unwrap(),
                BatchInspection::Absent
            ));
        }
        // Everything the installed root names is still exactly resolvable.
        for batch in &installed {
            archive.remove_hot_originals(batch);
            for object in batch.objects() {
                let bytes = object.encode().unwrap();
                assert_eq!(
                    archive
                        .store
                        .resolve_logical_object_bytes(ContentDigest::of(&bytes))
                        .unwrap(),
                    bytes
                );
            }
        }
    }

    #[test]
    fn missing_or_corrupt_cold_data_names_its_logical_object_and_spares_healthy_state() {
        let archive = TestArchive::open("damage");
        let batches = publish_batches(&archive, 0..4);
        relocate(&archive, &batches);
        let damaged = &batches[0];
        let healthy = &batches[3];
        for batch in &batches {
            archive.remove_hot_originals(batch);
        }
        let damaged_digest = ContentDigest::of(&damaged.objects()[0].encode().unwrap());
        let healthy_digest = ContentDigest::of(&healthy.objects()[0].encode().unwrap());

        let directory = archive.cold_directory();

        // 1. A corrupt record inside an intact pack refuses, and names the
        //    exact logical object it could not reconstruct. The pack is chosen
        //    by the damaged object's own locator, not by directory order.
        let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
        let locator = reader.locate_object(damaged_digest).unwrap().unwrap();
        drop(reader);
        let pack_path = directory.join(pack_filename(locator.pack));
        let original = std::fs::read(&pack_path).unwrap();
        let mut corrupt = original.clone();
        let payload_start = locator.offset as usize + COLD_RECORD_HEADER_BYTES;
        corrupt[payload_start] ^= 0xff;
        std::fs::write(&pack_path, &corrupt).unwrap();
        match archive
            .store
            .resolve_logical_object_bytes(damaged_digest)
            .unwrap_err()
        {
            StoreError::ColdObjectUnavailable { digest, .. } => {
                assert_eq!(digest, damaged_digest);
            }
            other => panic!("expected a named cold-object refusal, got {other:?}"),
        }
        // The unrelated object in the same pack is untouched.
        assert!(archive
            .store
            .resolve_logical_object_bytes(healthy_digest)
            .is_ok());
        std::fs::write(&pack_path, &original).unwrap();

        // 2. A missing pack refuses by name and still spares the hot tier.
        std::fs::remove_file(&pack_path).unwrap();
        match archive
            .store
            .resolve_logical_object_bytes(damaged_digest)
            .unwrap_err()
        {
            StoreError::ColdObjectUnavailable { digest, reason } => {
                assert_eq!(digest, damaged_digest);
                assert!(reason.contains("missing"), "reason was {reason}");
            }
            other => panic!("expected a named cold-object refusal, got {other:?}"),
        }
        // A live batch published after the damage still opens and validates:
        // cold damage is history authority, never active-state authority.
        let live = publish_batches(&archive, 100..101);
        assert!(matches!(
            archive
                .store
                .inspect_batch(live[0].manifest().batch_id())
                .unwrap(),
            BatchInspection::Ready(_)
        ));
        std::fs::write(&pack_path, &original).unwrap();

        // 3. Removing the live object-map root node refuses every object
        //    lookup by name, and still never invents absence.
        let root_digest = archive
            .store
            .cold_history_roots()
            .unwrap()
            .unwrap()
            .objects
            .root_digest
            .expect("a populated object map has a root node");
        let node = directory.join(format!("sealed-v2-1-{root_digest}"));
        let node_bytes = std::fs::read(&node).unwrap();
        std::fs::remove_file(&node).unwrap();
        for digest in [damaged_digest, healthy_digest] {
            match archive
                .store
                .resolve_logical_object_bytes(digest)
                .unwrap_err()
            {
                StoreError::ColdObjectUnavailable { digest: named, .. } => {
                    assert_eq!(named, digest);
                }
                other => panic!("expected a named cold-object refusal, got {other:?}"),
            }
        }
        // The hot tier is entirely unaffected by a damaged locator index.
        assert!(matches!(
            archive
                .store
                .inspect_batch(live[0].manifest().batch_id())
                .unwrap(),
            BatchInspection::Ready(_)
        ));
        std::fs::write(&node, &node_bytes).unwrap();
        assert!(archive
            .store
            .resolve_logical_object_bytes(healthy_digest)
            .is_ok());

        // 4. Damage confined to one prefix's inner map refuses only that
        //    prefix's objects. The composition localises index damage: the
        //    outer map and every other prefix map stay usable.
        let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
        let (damaged_high, _) = split_digest(damaged_digest);
        let inner_root_digest = reader
            .prefix_map(damaged_digest, damaged_high)
            .unwrap()
            .expect("the damaged object has a prefix map")
            .root
            .expect("a populated prefix map has a root node")
            .digest;
        drop(reader);
        let inner_node = directory.join(format!("sealed-v2-1-{inner_root_digest}"));
        let inner_bytes = std::fs::read(&inner_node).unwrap();
        std::fs::remove_file(&inner_node).unwrap();
        match archive
            .store
            .resolve_logical_object_bytes(damaged_digest)
            .unwrap_err()
        {
            StoreError::ColdObjectUnavailable { digest, .. } => {
                assert_eq!(digest, damaged_digest);
            }
            other => panic!("expected a named cold-object refusal, got {other:?}"),
        }
        assert!(
            archive
                .store
                .resolve_logical_object_bytes(healthy_digest)
                .is_ok(),
            "a damaged prefix map must not take unrelated prefixes down with it"
        );
        std::fs::write(&inner_node, &inner_bytes).unwrap();
        assert!(archive
            .store
            .resolve_logical_object_bytes(damaged_digest)
            .is_ok());

        // 5. A corrupt root marker refuses the index, not the archive.
        let marker = directory.join("current");
        let marker_bytes = std::fs::read(&marker).unwrap();
        std::fs::write(&marker, b"not a canonical cold root").unwrap();
        assert!(matches!(
            archive.store.cold_history_roots().unwrap_err(),
            StoreError::ColdHistoryIndexUnavailable(_)
        ));
        assert!(matches!(
            archive
                .store
                .inspect_batch(live[0].manifest().batch_id())
                .unwrap(),
            BatchInspection::Ready(_)
        ));
        std::fs::write(&marker, &marker_bytes).unwrap();
    }

    #[test]
    fn a_lost_root_marker_is_a_named_repair_condition_and_repair_restores_exact_history() {
        let archive = TestArchive::open("root-repair");
        let batches = publish_batches(&archive, 0..4);
        relocate(&archive, &batches);
        let healthy = archive.store.cold_history_roots().unwrap().unwrap();
        for batch in &batches {
            archive.remove_hot_originals(batch);
        }
        let probe = ContentDigest::of(&batches[0].objects()[0].encode().unwrap());
        // Retained in memory only because the hot originals are already gone.
        let archived_manifests: BTreeMap<BatchId, Vec<u8>> = batches
            .iter()
            .map(|batch| {
                (
                    batch.manifest().batch_id(),
                    batch.manifest().encode().unwrap(),
                )
            })
            .collect();

        // Repair on a healthy archive is a no-op.
        assert_eq!(
            archive.store.repair_cold_history_root().unwrap(),
            ColdRepairOutcome::default()
        );

        let marker = archive.cold_directory().join(COLD_ROOT_MARKER);
        std::fs::remove_file(&marker).unwrap();

        // Every path that could have called this "never published" now names
        // the repair condition instead.
        assert!(matches!(
            ColdHistoryReader::open(&archive.store),
            Err(StoreError::ColdHistoryRootMissing)
        ));
        assert!(matches!(
            archive.store.cold_history_roots(),
            Err(StoreError::ColdHistoryRootMissing)
        ));
        assert!(matches!(
            archive.store.resolve_logical_object_bytes(probe),
            Err(StoreError::ColdHistoryRootMissing)
        ));
        // Publication refuses rather than rooting a fresh empty history over
        // the preserved packs.
        assert!(matches!(
            publish_cold_history(&archive.store, &BTreeMap::new(), &archived_manifests),
            Err(StoreError::ColdHistoryRootMissing)
        ));
        assert!(matches!(
            archive.store.repack_cold_history(),
            Err(StoreError::ColdHistoryRootMissing)
        ));
        // Healthy hot and current state stay available throughout.
        let live = publish_batches(&archive, 100..101);
        assert!(matches!(
            archive
                .store
                .inspect_batch(live[0].manifest().batch_id())
                .unwrap(),
            BatchInspection::Ready(_)
        ));

        let outcome = archive.store.repair_cold_history_root().unwrap();
        assert!(outcome.repaired);
        assert_eq!(outcome.objects_recovered, 8);
        assert_eq!(outcome.manifests_recovered, 4);

        // Repair restored the exact old objects and manifests, with no hot
        // original anywhere on disk.
        let repaired = archive.store.cold_history_roots().unwrap().unwrap();
        assert_eq!(repaired.object_count(), healthy.object_count());
        assert_eq!(repaired.manifest_count(), healthy.manifest_count());
        for batch in &batches {
            let batch_id = batch.manifest().batch_id();
            assert_eq!(
                archive
                    .store
                    .resolve_logical_manifest_bytes(batch_id)
                    .unwrap(),
                batch.manifest().encode().unwrap()
            );
            for object in batch.objects() {
                let bytes = object.encode().unwrap();
                assert_eq!(
                    archive
                        .store
                        .resolve_logical_object_bytes(ContentDigest::of(&bytes))
                        .unwrap(),
                    bytes
                );
            }
        }
        // Repaired history accepts ordinary additive publication again -- and
        // recognizes the recovered bytes as exactly present -- and a second
        // repair is once more a no-op.
        assert_eq!(
            publish_cold_history(&archive.store, &BTreeMap::new(), &archived_manifests)
                .unwrap()
                .manifests_already_present,
            4
        );
        assert_eq!(
            archive.store.repair_cold_history_root().unwrap(),
            ColdRepairOutcome::default()
        );
    }

    #[test]
    fn a_never_initialized_archive_is_absence_not_a_repair_condition() {
        let archive = TestArchive::open("never-initialized");
        assert!(ColdHistoryReader::open(&archive.store).unwrap().is_none());
        assert!(archive.store.cold_history_roots().unwrap().is_none());
        assert_eq!(
            archive.store.repair_cold_history_root().unwrap(),
            ColdRepairOutcome::default()
        );
        // A cold directory that exists but holds no pack is still ordinary
        // absence: this is the shape an interrupted first publication leaves.
        let directory = cold_directory(&archive.store).unwrap();
        assert!(matches!(
            read_root_state(&directory).unwrap(),
            ColdRootState::NeverInitialized
        ));
        assert!(ColdHistoryReader::open(&archive.store).unwrap().is_none());
        assert_eq!(
            archive.store.repair_cold_history_root().unwrap(),
            ColdRepairOutcome::default()
        );
    }

    #[test]
    fn a_conflicting_manifest_never_displaces_archived_bytes_and_leaves_no_residue() {
        let archive = TestArchive::open("manifest-conflict");
        let batches = publish_batches(&archive, 0..2);
        relocate(&archive, &batches);
        let roots_before = archive.store.cold_history_roots().unwrap().unwrap();
        let directory = cold_directory(&archive.store).unwrap();
        let packs_before = cold_pack_names(&directory).unwrap();
        let manifest = batches[0].manifest();
        let original = manifest.encode().unwrap();

        // A different SessionId alone yields a valid, distinct canonical
        // manifest under the same BatchId.
        let conflicting = crate::oplog::OperationBatch::new_with_causality(
            manifest.workspace_id(),
            manifest.lineage_digest(),
            manifest.batch_id(),
            manifest.author_device_id(),
            SessionId::new(),
            manifest.origin(),
            manifest.causal_dot(),
            manifest.causal_dependency_heads().to_vec(),
            manifest.dependency_frontier().clone(),
            manifest.semantic_effect_digest(),
            manifest.required_objects().to_vec(),
        )
        .unwrap();
        let conflicting_bytes = conflicting.encode().unwrap();
        assert_ne!(original, conflicting_bytes);

        match publish_cold_history(
            &archive.store,
            &BTreeMap::new(),
            &BTreeMap::from([(manifest.batch_id(), conflicting_bytes)]),
        )
        .unwrap_err()
        {
            StoreError::ColdManifestConflict { batch_id, .. } => {
                assert_eq!(batch_id, manifest.batch_id());
            }
            other => panic!("expected a named cold-manifest conflict, got {other:?}"),
        }

        // Predecessor root, original bytes and pack set are all untouched: the
        // exactness pass runs before a single record is appended.
        assert_eq!(
            archive.store.cold_history_roots().unwrap().unwrap(),
            roots_before
        );
        assert_eq!(cold_pack_names(&directory).unwrap(), packs_before);
        for batch in &batches {
            archive.remove_hot_originals(batch);
            assert_eq!(
                archive
                    .store
                    .resolve_logical_manifest_bytes(batch.manifest().batch_id())
                    .unwrap(),
                batch.manifest().encode().unwrap()
            );
        }
        // The identical bytes remain an ordinary counted no-op.
        let repeat = publish_cold_history(
            &archive.store,
            &BTreeMap::new(),
            &BTreeMap::from([(manifest.batch_id(), original)]),
        )
        .unwrap();
        assert_eq!(repeat.manifests_already_present, 1);
        assert_eq!(repeat.manifests_published, 0);
        assert_eq!(repeat.packs_published, 0);
        assert_eq!(
            archive.store.cold_history_roots().unwrap().unwrap(),
            roots_before
        );
    }

    #[test]
    fn repacking_preserves_every_logical_identity_and_byte() {
        let archive = TestArchive::open("repack");
        let batches = publish_batches(&archive, 0..5);
        relocate(&archive, &batches);
        let before_roots = archive.store.cold_history_roots().unwrap().unwrap();
        let before_locators: BTreeMap<ContentDigest, ColdLocatorV1> = {
            let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
            batches
                .iter()
                .flat_map(|batch| batch.objects())
                .map(|object| {
                    let digest = ContentDigest::of(&object.encode().unwrap());
                    (digest, reader.locate_object(digest).unwrap().unwrap())
                })
                .collect()
        };
        for batch in &batches {
            archive.remove_hot_originals(batch);
        }

        let outcome = archive.store.repack_cold_history().unwrap();
        assert_eq!(outcome.objects_published, 10);
        assert_eq!(outcome.manifests_published, 5);
        let after_roots = archive.store.cold_history_roots().unwrap().unwrap();
        assert_eq!(after_roots.object_count(), before_roots.object_count());
        assert_eq!(after_roots.manifest_count(), before_roots.manifest_count());

        let reader = ColdHistoryReader::open(&archive.store).unwrap().unwrap();
        let mut relocated = 0;
        for batch in &batches {
            let batch_id = batch.manifest().batch_id();
            assert_eq!(
                archive
                    .store
                    .resolve_logical_manifest_bytes(batch_id)
                    .unwrap(),
                batch.manifest().encode().unwrap(),
                "a repack must not change a manifest byte"
            );
            for object in batch.objects() {
                let bytes = object.encode().unwrap();
                let digest = ContentDigest::of(&bytes);
                assert_eq!(
                    archive.store.resolve_logical_object_bytes(digest).unwrap(),
                    bytes,
                    "a repack must not change an object byte"
                );
                let after = reader.locate_object(digest).unwrap().unwrap();
                if after != before_locators[&digest] {
                    relocated += 1;
                }
            }
        }
        assert!(
            relocated > 0,
            "the repack must actually move records, else it proves nothing"
        );
        // Publish-new-before-retire-old: the predecessor packs are still there.
        assert!(
            cold_pack_names(&cold_directory(&archive.store).unwrap())
                .unwrap()
                .len()
                >= 2
        );
    }

    // -----------------------------------------------------------------------
    // Manager negative controls (evidence/rebaselining-2026-09-07/
    // p2-manager-negative-controls/regression-tests.rs). Both reproduced on the
    // pass-1 source; they are retained verbatim in intent here.
    // -----------------------------------------------------------------------

    #[test]
    fn manager_cold_duplicate_batch_id_requires_exact_manifest_bytes() {
        let archive = TestArchive::open("manager-conflicting-manifest");
        let batches = publish_batches(&archive, 0..1);
        relocate(&archive, &batches);
        let manifest = batches[0].manifest();
        let original = manifest.encode().unwrap();
        let changed = crate::oplog::OperationBatch::new_with_causality(
            manifest.workspace_id(),
            manifest.lineage_digest(),
            manifest.batch_id(),
            manifest.author_device_id(),
            SessionId::new(),
            manifest.origin(),
            manifest.causal_dot(),
            manifest.causal_dependency_heads().to_vec(),
            manifest.dependency_frontier().clone(),
            manifest.semantic_effect_digest(),
            manifest.required_objects().to_vec(),
        )
        .unwrap();
        let changed_bytes = changed.encode().unwrap();
        assert_eq!(manifest.batch_id(), changed.batch_id());
        assert_ne!(original, changed_bytes);
        let result = publish_cold_history(
            &archive.store,
            &BTreeMap::new(),
            &BTreeMap::from([(changed.batch_id(), changed_bytes)]),
        );
        assert!(
            result.is_err(),
            "a conflicting manifest is not an already archived exact copy"
        );
        assert_eq!(
            ColdHistoryReader::open(&archive.store)
                .unwrap()
                .unwrap()
                .manifest_bytes(manifest.batch_id())
                .unwrap()
                .unwrap(),
            original
        );
    }

    #[test]
    fn manager_torn_cold_root_is_rebuildable_from_preserved_packs() {
        let archive = TestArchive::open("manager-torn-root-repair");
        let batches = publish_batches(&archive, 0..1);
        relocate(&archive, &batches);
        let batch_id = batches[0].manifest().batch_id();
        let original = batches[0].manifest().encode().unwrap();
        archive.remove_hot_originals(&batches[0]);
        assert!(!archive.store.repair_cold_history_root().unwrap().repaired);
        let original_object = batches[0].objects()[0].encode().unwrap();
        let object_digest = ContentDigest::of(&original_object);
        // Named in-scope fault: torn derived marker; original pack bytes survive.
        std::fs::write(archive.cold_directory().join(COLD_ROOT_MARKER), b"torn").unwrap();
        assert!(ColdHistoryReader::open(&archive.store).is_err());
        let repaired = archive.store.repair_cold_history_root();
        assert!(
            repaired.is_ok(),
            "derived marker damage must be repairable from intact original packs: {repaired:?}"
        );
        assert!(repaired.unwrap().repaired);
        assert_eq!(
            ColdHistoryReader::open(&archive.store)
                .unwrap()
                .unwrap()
                .manifest_bytes(batch_id)
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(
            archive
                .store
                .resolve_logical_object_bytes(object_digest)
                .unwrap(),
            original_object
        );
        std::fs::remove_file(archive.cold_directory().join(COLD_ROOT_MARKER)).unwrap();
        assert!(archive.store.repair_cold_history_root().unwrap().repaired);
        assert_eq!(
            ColdHistoryReader::open(&archive.store)
                .unwrap()
                .unwrap()
                .manifest_bytes(batch_id)
                .unwrap()
                .unwrap(),
            original
        );
    }

    #[test]
    fn a_damaged_root_with_no_preserved_packs_is_never_replaced_with_empty_history() {
        let archive = TestArchive::open("torn-root-no-packs");
        let directory = cold_directory(&archive.store).unwrap();
        assert!(cold_pack_names(&directory).unwrap().is_empty());
        std::fs::write(
            archive.cold_directory().join(COLD_ROOT_MARKER),
            b"torn with nothing behind it",
        )
        .unwrap();
        assert!(matches!(
            archive.store.repair_cold_history_root(),
            Err(StoreError::ColdHistoryIndexUnavailable(_))
        ));
        assert_eq!(
            std::fs::read(archive.cold_directory().join(COLD_ROOT_MARKER)).unwrap(),
            b"torn with nothing behind it"
        );
    }

    #[test]
    fn manager_cold_missing_root_must_not_report_never_published() {
        let archive = TestArchive::open("manager-missing-root");
        let batches = publish_batches(&archive, 0..1);
        relocate(&archive, &batches);
        archive.remove_hot_originals(&batches[0]);
        std::fs::remove_file(archive.cold_directory().join(COLD_ROOT_MARKER)).unwrap();
        let reopened = ColdHistoryReader::open(&archive.store);
        assert!(
            !matches!(reopened, Ok(None)),
            "preserved cold packs with a lost derived marker need repair or a named error, not ordinary absence"
        );
    }

    #[test]
    fn cold_publication_reuses_the_shared_publication_and_index_primitives() {
        let source = include_str!("cold_object_store.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("the module has a production region");
        for forbidden in ["fs::write", "fs::rename", "OpenOptions", "create_new"] {
            assert!(
                !production.contains(forbidden),
                "cold publication must not reimplement a durable write primitive: {forbidden}"
            );
        }
        for required in [
            "DurableDirectoryPublication",
            "SealedGenerationStagingStore",
            "SealedAcceptedIndexWriter",
            "SealedAcceptedIndexReader",
        ] {
            assert!(
                production.contains(required),
                "missing primitive: {required}"
            );
        }
    }

    #[test]
    fn contract_names_the_current_cold_representation_and_read_resolution() {
        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        for required in [
            "cold-history-v1",
            "pack-v1-<uuid>",
            "ColdLocatorV1",
            "inner-root descriptor",
            "ColdHistoryRootMissing",
            "repair_cold_history_root",
            "inspect_batch_with_cold_history",
            "resolve_logical_object_bytes",
        ] {
            assert!(contract.contains(required), "missing contract: {required}");
        }
    }
}
