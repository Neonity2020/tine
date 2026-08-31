//! Exact, read-only external inventory and conservative identity matching.
//!
//! This module plans reconciliation only. It does not publish semantic
//! operations, write a graph, or activate managed sync. The clean runtime
//! reads its disposable SQLite path ownership instead of recreating a native
//! path index beside SQLite.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use cap_std::{ambient_authority, fs::Dir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::external_import::{
    ExternalImportObservationEntry, ExternalImportObservationMaterial,
    ExternalImportObservationMaterialError, ExternalImportObservationState,
};
use super::hot_engine::{
    AcceptedFrontierRoot, AuthorBatch, CleanImportProjectionPredecessor,
    LazyGenesisCheckpointBuilder, MAX_TRANSACTION_OPERATIONS,
};
use super::lazy_genesis::{
    publish_activation_marker, read_activation_marker, LazyGenesisActivationMarkerV1,
    LazyGenesisBlockInput, LazyGenesisCandidate, LazyGenesisCommitV1, LazyGenesisPackBuilder,
    LazyGenesisPageInput,
};
use super::object_store::StoreError;
use super::receipt::ImportIdDerivation;
use super::{
    plan_projection, AcceptedBatchEvent, AnnotatedIdentity, BatchId, BatchOrigin, BlobDescription,
    BlockId, BlockLocation, ContentDigest, CrdtPeerId, CurrentPageAtPath, DeviceId, DocumentId,
    ImportId, ImportInventoryEntry, ImportInventoryState, ImportLocator, LineageDigest,
    LogicalCompletionId, LogicalPageName, LogseqIdentityMutation, LogseqUuid, ManagedPath,
    ManagedTextKind, ObjectKind, OperationTransaction, PageId, ProjectionCompletedReceipt,
    ProjectionCompletion, ProjectionIntent, ProjectionReceiptStore, ProjectionStoreError,
    ProjectionWorkId, ProjectionWorkTarget, ReferenceCatalogPolicyV1, SemanticOperation, SessionId,
    ShardedHotEngine, SqliteFrontier, StructuralLocator, StructuralSpan, WorkspaceId,
    DIFF_SCHEMA_VERSION,
};
#[cfg(test)]
use super::{OperationBatch, OperationObject};
use crate::model::{
    path_is_sync_conflict, resolve_external_document_identity, AcceptedExternalDocumentIdentity,
    BootstrapSourceCapture, BootstrapSourceCaptureInstrumentation, BootstrapSourceChunk,
    BootstrapSourceEntry, Graph, PageEntry, PageKind,
};

#[cfg(test)]
thread_local! {
    static SNAPSHOT_REVALIDATION_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static POST_FRONTIER_OVERRIDE:
        std::cell::RefCell<Option<AcceptedFrontierRoot>> = const { std::cell::RefCell::new(None) };
    static POST_CLEAN_PREDECESSOR_OVERRIDE:
        std::cell::RefCell<Option<CatalogAuthority>> = const { std::cell::RefCell::new(None) };
    static DERANGE_NEXT_CLEAN_PREDECESSOR_PATH:
        std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static NEXT_ACTIVATION_PAGE_RECORD_MEMORY_LIMIT:
        std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn force_next_activation_page_record_memory_limit(bytes: usize) {
    NEXT_ACTIVATION_PAGE_RECORD_MEMORY_LIMIT.with(|limit| limit.set(Some(bytes)));
}

fn activation_page_record_memory_limit() -> usize {
    #[cfg(test)]
    if let Some(limit) = NEXT_ACTIVATION_PAGE_RECORD_MEMORY_LIMIT.with(|limit| limit.take()) {
        return limit;
    }
    MAX_TERMINAL_PROJECTION_HINT_BYTES
}

/// The 1M-block program target is expected to fit below these aggregate
/// ceilings for ordinary shallow documents. Inputs beyond them remain exact
/// raw evidence but are not parsed into an authoritative import plan.
pub const MAX_IMPORT_FILES: usize = 1_000_000;
pub const MAX_IMPORT_RAW_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_IMPORT_PARSED_NODES: usize = 2_000_000;
pub const MAX_IMPORT_DEPTH: usize = 256;
pub const MAX_IMPORT_LOCATOR_COMPONENTS: usize = 16_000_000;
pub const MAX_IMPORT_CATALOG_ENTRIES: usize = 2_000_000;
pub const MAX_IMPORT_PATH_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_IMPORT_REPLAY_ENTRIES: usize = 1_000_000;
pub const MAX_IMPORT_REPLAY_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_IMPORT_RENDERED_TARGET_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_IMPORT_STRUCTURAL_KEY_WORK: usize = 64_000_000;

const MAX_TERMINAL_PROJECTION_HINT_BYTES: usize = 128 * 1024 * 1024;
const SOURCE_LEAF_SCHEMA_VERSION: u32 = 1;
const MAX_SOURCE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PARSED_NODES_PER_SOURCE_FILE: u32 = 1_000_000;
const MAX_SOURCE_INVENTORY_LEAVES: u32 = 1_000_000;
const MAX_SOURCE_LOCATOR_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BootstrapStreamingImportInstrumentation {
    pub(crate) source_files: u64,
    pub(crate) source_chunks: u64,
    pub(crate) source_bytes: u64,
    pub(crate) source_bytes_read: u64,
    pub(crate) parser_nodes: u64,
    pub(crate) operations: u64,
    pub(crate) source_spans: u64,
    pub(crate) parts: u32,
    pub(crate) page_declarations: u64,
    pub(crate) page_capsules: u64,
    pub(crate) huge_page_splits: u64,
    pub(crate) max_part_documents: u64,
    pub(crate) max_part_manifest_bytes: u64,
    pub(crate) max_part_payload_descriptors: u64,
    pub(crate) operation_spool_bytes: u64,
    pub(crate) operation_builder_retained_bytes: u64,
    pub(crate) operation_builder_spilled: bool,
    pub(crate) terminal_projection_hint_pages: u64,
    pub(crate) terminal_projection_hint_bytes: u64,
    pub(crate) terminal_projection_hint_spilled: bool,
    pub(crate) prepared_bytes: u64,
    pub(crate) external_sort_runs: u64,
    pub(crate) capture_passes: u64,
    pub(crate) peak_owned_source_bytes: u64,
    pub(crate) peak_owned_parser_nodes: u64,
    pub(crate) peak_owned_part_operations: u64,
    pub(crate) peak_owned_part_bytes: u64,
    pub(crate) peak_owned_sort_buffer_bytes: u64,
    pub(crate) source_protocol_micros: u64,
    pub(crate) operation_spool_micros: u64,
    pub(crate) partition_micros: u64,
    pub(crate) preparation_sealing_micros: u64,
}

#[derive(Debug)]
pub(crate) enum BootstrapStreamingImportError {
    Io(io::Error),
    Store(StoreError),
    Engine(super::hot_engine::EngineError),
    Projection(super::sqlite::ProjectionError),
    InvalidSource(String),
    InvalidOperation(String),
    ResourceLimit {
        resource: &'static str,
        observed: u64,
        limit: u64,
    },
    SingletonOverLimit(&'static str),
    ConflictingSeal,
}

impl fmt::Display for BootstrapStreamingImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Engine(error) => error.fmt(formatter),
            Self::Projection(error) => error.fmt(formatter),
            Self::InvalidSource(detail) | Self::InvalidOperation(detail) => {
                formatter.write_str(detail)
            }
            Self::ResourceLimit {
                resource,
                observed,
                limit,
            } => write!(
                formatter,
                "{resource} limit exceeded: observed {observed}, limit {limit}"
            ),
            Self::SingletonOverLimit(resource) => {
                write!(
                    formatter,
                    "one bootstrap operation cannot fit the {resource} limit"
                )
            }
            Self::ConflictingSeal => {
                formatter.write_str("conflicting sealed bootstrap preparation")
            }
        }
    }
}

impl std::error::Error for BootstrapStreamingImportError {}

impl From<io::Error> for BootstrapStreamingImportError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<StoreError> for BootstrapStreamingImportError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<super::hot_engine::EngineError> for BootstrapStreamingImportError {
    fn from(error: super::hot_engine::EngineError) -> Self {
        Self::Engine(error)
    }
}

impl From<super::sqlite::ProjectionError> for BootstrapStreamingImportError {
    fn from(error: super::sqlite::ProjectionError) -> Self {
        Self::Projection(error)
    }
}

const ACTIVATION_PAGE_RECORD_SCHEMA_VERSION: u32 = 2;
const MAX_ACTIVATION_PAGE_RECORD_BYTES: usize = 256 * 1024 * 1024;

/// Source-only facts needed by the temporary old-operation oracle but not by
/// SQLite. Keeping them beside the terminal page makes one parsed page record
/// sufficient for both consumers during the genesis differential phase.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationBlockSourceV1 {
    span: StructuralSpan,
    raw_ids: Vec<String>,
}

/// One parser-owned page record for the activation fan-out.
///
/// The record is process-only in Packet 1. It is neither managed authority nor
/// a durable format commitment; the later genesis packet will choose the
/// durable capsule codec. Its strict schema and bounds nevertheless make the
/// shadow differential exercise the same streaming boundary production will
/// consume.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivationPageRecordV1 {
    schema_version: u32,
    source_leaf: [u8; 32],
    exact_source_bytes: Vec<u8>,
    full_span: Option<StructuralSpan>,
    page: super::MaterializedPageInput,
    block_sources: Vec<ActivationBlockSourceV1>,
}

impl ActivationPageRecordV1 {
    fn new(
        source_leaf: [u8; 32],
        exact_source_bytes: Vec<u8>,
        full_span: Option<StructuralSpan>,
        page: super::MaterializedPageInput,
        block_sources: Vec<ActivationBlockSourceV1>,
    ) -> Result<Self, BootstrapStreamingImportError> {
        let record = Self {
            schema_version: ACTIVATION_PAGE_RECORD_SCHEMA_VERSION,
            source_leaf,
            exact_source_bytes,
            full_span,
            page,
            block_sources,
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), BootstrapStreamingImportError> {
        if self.schema_version != ACTIVATION_PAGE_RECORD_SCHEMA_VERSION
            || self.page.blocks.len() != self.block_sources.len()
            || self
                .full_span
                .map(|span| span.end().saturating_sub(span.start()))
                != (!self.exact_source_bytes.is_empty())
                    .then_some(self.exact_source_bytes.len() as u64)
        {
            return Err(BootstrapStreamingImportError::InvalidOperation(
                "activation page record shape is malformed".into(),
            ));
        }
        if let Some(full) = self.full_span {
            for source in &self.block_sources {
                if source.span.start() < full.start() || source.span.end() > full.end() {
                    return Err(BootstrapStreamingImportError::InvalidOperation(
                        "activation block span escapes its source page".into(),
                    ));
                }
            }
        } else if !self.block_sources.is_empty() {
            return Err(BootstrapStreamingImportError::InvalidOperation(
                "nonempty activation page has no exact source span".into(),
            ));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>, BootstrapStreamingImportError> {
        self.validate()?;
        let bytes = postcard::to_allocvec(self)
            .map_err(|error| BootstrapStreamingImportError::InvalidOperation(error.to_string()))?;
        if bytes.len() > MAX_ACTIVATION_PAGE_RECORD_BYTES {
            return Err(BootstrapStreamingImportError::ResourceLimit {
                resource: "activation page record bytes",
                observed: bytes.len() as u64,
                limit: MAX_ACTIVATION_PAGE_RECORD_BYTES as u64,
            });
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, BootstrapStreamingImportError> {
        if bytes.len() > MAX_ACTIVATION_PAGE_RECORD_BYTES {
            return Err(BootstrapStreamingImportError::ResourceLimit {
                resource: "activation page record bytes",
                observed: bytes.len() as u64,
                limit: MAX_ACTIVATION_PAGE_RECORD_BYTES as u64,
            });
        }
        let record: Self = postcard::from_bytes(bytes)
            .map_err(|error| BootstrapStreamingImportError::InvalidOperation(error.to_string()))?;
        record.validate()?;
        Ok(record)
    }

    fn sqlite_page(&self) -> super::MaterializedPageInput {
        let mut page = self.page.clone();
        for (block, source) in page.blocks.iter_mut().zip(&self.block_sources) {
            let logseq_uuid = (source.raw_ids.len() == 1)
                .then(|| LogseqUuid::parse(source.raw_ids[0].trim()).ok())
                .flatten();
            block.logseq_uuid = logseq_uuid;
            block.logseq_identity_origin =
                logseq_uuid.map(|_| super::LogseqIdentityOrigin::ExternalImported);
        }
        page
    }
}

fn clean_source_leaf_digest(
    kind: ManagedTextKind,
    path: &ManagedPath,
    content_digest: &[u8; 32],
    byte_length: u64,
) -> Result<[u8; 32], BootstrapStreamingImportError> {
    if path.as_str().len() > MAX_SOURCE_LOCATOR_BYTES {
        return Err(BootstrapStreamingImportError::ResourceLimit {
            resource: "source locator bytes",
            observed: path.as_str().len() as u64,
            limit: MAX_SOURCE_LOCATOR_BYTES as u64,
        });
    }
    if byte_length > MAX_SOURCE_FILE_BYTES {
        return Err(BootstrapStreamingImportError::ResourceLimit {
            resource: "source file bytes",
            observed: byte_length,
            limit: MAX_SOURCE_FILE_BYTES,
        });
    }
    let schema = SOURCE_LEAF_SCHEMA_VERSION.to_be_bytes();
    let kind = [match kind {
        ManagedTextKind::Page => 1,
        ManagedTextKind::Journal => 2,
    }];
    let byte_length = byte_length.to_be_bytes();
    let mut hasher = Sha256::new();
    hasher.update(b"tine/bootstrap-import/source-leaf/v1\0");
    for field in [
        schema.as_slice(),
        kind.as_slice(),
        path.as_str().as_bytes(),
        content_digest.as_slice(),
        byte_length.as_slice(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    Ok(hasher.finalize().into())
}

enum ActivationPageRecordBuildMode {
    Memory {
        records: Vec<ActivationPageRecordV1>,
        encoded_bytes: usize,
    },
    Spilled {
        writer: BufWriter<File>,
        path: PathBuf,
        index: BTreeMap<PageId, (u64, u64)>,
        encoded_bytes: usize,
    },
    Transitioning,
}

struct ActivationPageRecordBuilder {
    memory_limit: usize,
    mode: ActivationPageRecordBuildMode,
    path_order: Vec<PageId>,
}

impl ActivationPageRecordBuilder {
    fn new() -> Self {
        Self {
            memory_limit: activation_page_record_memory_limit(),
            mode: ActivationPageRecordBuildMode::Memory {
                records: Vec::new(),
                encoded_bytes: 0,
            },
            path_order: Vec::new(),
        }
    }

    fn push(
        &mut self,
        record: ActivationPageRecordV1,
    ) -> Result<(), BootstrapStreamingImportError> {
        let encoded = record.encode()?;
        let transition = match &self.mode {
            ActivationPageRecordBuildMode::Memory { encoded_bytes, .. } => encoded_bytes
                .checked_add(encoded.len())
                .is_none_or(|next| next > self.memory_limit),
            _ => false,
        };
        if transition {
            self.spill_memory_records()?;
        }
        let page_id = record.page.page_id;
        let result = match &mut self.mode {
            ActivationPageRecordBuildMode::Memory {
                records,
                encoded_bytes,
            } => {
                *encoded_bytes = encoded_bytes.checked_add(encoded.len()).ok_or_else(|| {
                    BootstrapStreamingImportError::InvalidOperation(
                        "activation page record byte count overflow".into(),
                    )
                })?;
                records.push(record);
                Ok(())
            }
            ActivationPageRecordBuildMode::Spilled {
                writer,
                index,
                encoded_bytes,
                ..
            } => {
                write_activation_page_record_frame(writer, index, &record, &encoded)?;
                *encoded_bytes = encoded_bytes.checked_add(encoded.len()).ok_or_else(|| {
                    BootstrapStreamingImportError::InvalidOperation(
                        "activation page record byte count overflow".into(),
                    )
                })?;
                Ok(())
            }
            ActivationPageRecordBuildMode::Transitioning => unreachable!(),
        };
        if result.is_ok() {
            self.path_order.push(page_id);
        }
        result
    }

    fn spill_memory_records(&mut self) -> Result<(), BootstrapStreamingImportError> {
        let ActivationPageRecordBuildMode::Memory {
            records,
            encoded_bytes,
        } = std::mem::replace(&mut self.mode, ActivationPageRecordBuildMode::Transitioning)
        else {
            unreachable!("activation page records spill only once")
        };
        let path = std::env::temp_dir().join(format!(
            "tine-activation-page-records-{}.spool",
            Uuid::new_v4().simple()
        ));
        let prepared = (|| {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?;
            let mut writer = BufWriter::new(file);
            let mut index = BTreeMap::new();
            for record in records {
                let encoded = record.encode()?;
                write_activation_page_record_frame(&mut writer, &mut index, &record, &encoded)?;
            }
            Ok::<_, BootstrapStreamingImportError>((writer, index))
        })();
        let (writer, index) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        self.mode = ActivationPageRecordBuildMode::Spilled {
            writer,
            path,
            index,
            encoded_bytes,
        };
        Ok(())
    }

    fn finish(self) -> Result<ActivationPageRecordStore, BootstrapStreamingImportError> {
        let Self {
            mode, path_order, ..
        } = self;
        match mode {
            ActivationPageRecordBuildMode::Memory {
                mut records,
                encoded_bytes,
            } => {
                records.sort_unstable_by_key(|record| record.page.page_id);
                if records
                    .windows(2)
                    .any(|pair| pair[0].page.page_id == pair[1].page.page_id)
                {
                    return Err(BootstrapStreamingImportError::InvalidOperation(
                        "activation page records repeat a page identity".into(),
                    ));
                }
                Ok(ActivationPageRecordStore::Memory {
                    records,
                    encoded_bytes,
                    path_order,
                })
            }
            ActivationPageRecordBuildMode::Spilled {
                mut writer,
                path,
                index,
                encoded_bytes,
            } => {
                if let Err(error) = writer.flush() {
                    drop(writer);
                    let _ = fs::remove_file(&path);
                    return Err(error.into());
                }
                let file = match writer.into_inner() {
                    Ok(file) => file,
                    Err(error) => {
                        let failure =
                            io::Error::new(error.error().kind(), error.error().to_string());
                        drop(error.into_inner());
                        let _ = fs::remove_file(&path);
                        return Err(failure.into());
                    }
                };
                Ok(ActivationPageRecordStore::Spilled(
                    SpilledActivationPageRecords {
                        file: RefCell::new(Some(file)),
                        path,
                        index,
                        encoded_bytes,
                        path_order,
                    },
                ))
            }
            ActivationPageRecordBuildMode::Transitioning => unreachable!(),
        }
    }
}

fn write_activation_page_record_frame(
    writer: &mut BufWriter<File>,
    index: &mut BTreeMap<PageId, (u64, u64)>,
    record: &ActivationPageRecordV1,
    encoded: &[u8],
) -> Result<(), BootstrapStreamingImportError> {
    let start = writer.stream_position()?;
    let length = u64::try_from(encoded.len()).map_err(|_| {
        BootstrapStreamingImportError::InvalidOperation(
            "activation page record length cannot be represented".into(),
        )
    })?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(encoded)?;
    let offset = start.checked_add(8).ok_or_else(|| {
        BootstrapStreamingImportError::InvalidOperation(
            "activation page record offset overflow".into(),
        )
    })?;
    if index
        .insert(record.page.page_id, (offset, length))
        .is_some()
    {
        return Err(BootstrapStreamingImportError::InvalidOperation(
            "activation page records repeat a page identity".into(),
        ));
    }
    Ok(())
}

pub(crate) enum ActivationPageRecordStore {
    Memory {
        records: Vec<ActivationPageRecordV1>,
        encoded_bytes: usize,
        path_order: Vec<PageId>,
    },
    Spilled(SpilledActivationPageRecords),
}

impl ActivationPageRecordStore {
    pub(crate) fn page_count(&self) -> usize {
        match self {
            Self::Memory { records, .. } => records.len(),
            Self::Spilled(spilled) => spilled.index.len(),
        }
    }

    pub(crate) fn path_order(&self) -> &[PageId] {
        match self {
            Self::Memory { path_order, .. } => path_order,
            Self::Spilled(spilled) => &spilled.path_order,
        }
    }

    fn encoded_bytes(&self) -> usize {
        match self {
            Self::Memory { encoded_bytes, .. } => *encoded_bytes,
            Self::Spilled(spilled) => spilled.encoded_bytes,
        }
    }

    fn spilled(&self) -> bool {
        matches!(self, Self::Spilled(_))
    }

    fn page(
        &self,
        page_id: PageId,
    ) -> Result<Option<ActivationPageRecordV1>, BootstrapStreamingImportError> {
        match self {
            Self::Memory { records, .. } => Ok(records
                .binary_search_by_key(&page_id, |record| record.page.page_id)
                .ok()
                .map(|index| records[index].clone())),
            Self::Spilled(spilled) => spilled.page(page_id),
        }
    }

    /// Return the parser-owned terminal page for SQLite construction.
    ///
    /// Every syntactically valid single `id::` claim is retained here,
    /// including graph-wide ambiguity. The immutable CRDT baseline separately
    /// installs only a uniquely claimed UUID; SQLite must preserve all
    /// claimants so the application can make that decision without Patricia.
    pub(crate) fn sqlite_page(
        &self,
        page_id: PageId,
    ) -> Result<Option<super::MaterializedPageInput>, BootstrapStreamingImportError> {
        let Some(record) = self.page(page_id)? else {
            return Ok(None);
        };
        Ok(Some(record.sqlite_page()))
    }
}

pub(crate) struct SpilledActivationPageRecords {
    file: RefCell<Option<File>>,
    path: PathBuf,
    index: BTreeMap<PageId, (u64, u64)>,
    encoded_bytes: usize,
    path_order: Vec<PageId>,
}

impl SpilledActivationPageRecords {
    fn page(
        &self,
        page_id: PageId,
    ) -> Result<Option<ActivationPageRecordV1>, BootstrapStreamingImportError> {
        let Some(&(offset, length)) = self.index.get(&page_id) else {
            return Ok(None);
        };
        if length > MAX_ACTIVATION_PAGE_RECORD_BYTES as u64 {
            return Err(BootstrapStreamingImportError::ResourceLimit {
                resource: "activation page record bytes",
                observed: length,
                limit: MAX_ACTIVATION_PAGE_RECORD_BYTES as u64,
            });
        }
        let mut file = self.file.borrow_mut();
        let file = file.as_mut().ok_or_else(|| {
            BootstrapStreamingImportError::InvalidOperation(
                "activation page record spool is closed".into(),
            )
        })?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0_u8; length as usize];
        file.read_exact(&mut bytes)?;
        let record = ActivationPageRecordV1::decode(&bytes)?;
        if record.page.page_id != page_id {
            return Err(BootstrapStreamingImportError::InvalidOperation(
                "activation page record index points to another page".into(),
            ));
        }
        Ok(Some(record))
    }
}

impl Drop for SpilledActivationPageRecords {
    fn drop(&mut self) {
        self.file.get_mut().take();
        let _ = fs::remove_file(&self.path);
    }
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn invalid_activation_data(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(invalid_activation_data(
            "bootstrap scratch path is not a real directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(path),
        Err(error) => Err(error),
    }
}

struct BootstrapSourceReader<'a> {
    capture: &'a BootstrapSourceCapture,
    chunks: crate::model::BootstrapSourceChunkCursor,
    next_chunk: Option<BootstrapSourceChunk>,
}

impl<'a> BootstrapSourceReader<'a> {
    fn new(capture: &'a BootstrapSourceCapture) -> io::Result<Self> {
        let mut chunks = capture.chunks_cursor()?;
        let next_chunk = chunks.next()?;
        Ok(Self {
            capture,
            chunks,
            next_chunk,
        })
    }

    fn read_entry(
        &mut self,
        entry: &BootstrapSourceEntry,
        instrumentation: &mut BootstrapStreamingImportInstrumentation,
    ) -> Result<Vec<u8>, BootstrapStreamingImportError> {
        let declared = entry.description().byte_length();
        if declared > MAX_SOURCE_FILE_BYTES {
            return Err(BootstrapStreamingImportError::ResourceLimit {
                resource: "source file bytes",
                observed: declared,
                limit: MAX_SOURCE_FILE_BYTES,
            });
        }
        let capacity = usize::try_from(declared).map_err(|_| {
            BootstrapStreamingImportError::InvalidSource(
                "source file length cannot be represented".into(),
            )
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        for ordinal in 0..entry.chunk_count() {
            let chunk = self.next_chunk.take().ok_or_else(|| {
                BootstrapStreamingImportError::InvalidSource(format!(
                    "sealed source {} is missing chunk {ordinal}",
                    entry.path()
                ))
            })?;
            if chunk.path() != entry.path() || chunk.ordinal() != ordinal {
                return Err(BootstrapStreamingImportError::InvalidSource(format!(
                    "sealed source chunk order differs for {}",
                    entry.path()
                )));
            }
            let mut reader = self.capture.open_chunk(&chunk)?;
            reader.read_to_end(&mut bytes)?;
            reader.finish()?;
            self.next_chunk = self.chunks.next()?;
        }
        if bytes.len() as u64 != declared || BlobDescription::of(&bytes) != entry.description() {
            return Err(BootstrapStreamingImportError::InvalidSource(format!(
                "sealed source bytes differ for {}",
                entry.path()
            )));
        }
        instrumentation.source_bytes_read = instrumentation
            .source_bytes_read
            .checked_add(declared)
            .ok_or_else(|| {
                BootstrapStreamingImportError::InvalidSource(
                    "source byte instrumentation overflow".into(),
                )
            })?;
        instrumentation.peak_owned_source_bytes =
            instrumentation.peak_owned_source_bytes.max(declared);
        Ok(bytes)
    }

    fn finish(self) -> Result<(), BootstrapStreamingImportError> {
        if self.next_chunk.is_some() {
            return Err(BootstrapStreamingImportError::InvalidSource(
                "sealed source capture has trailing chunks".into(),
            ));
        }
        Ok(())
    }
}

/// Select the deterministic bootstrap page set while retaining the capture's
/// complete exact source inventory and blob evidence.
///
/// Entries are sealed in exact-path order. The first member that does not
/// collide with an already selected effective name or portable path wins,
/// matching graph discovery's deterministic path ordering. Later members stay
/// in the authenticated source protocol but acquire no semantic operations.
pub(crate) fn bootstrap_authoritative_source_paths(
    capture: &BootstrapSourceCapture,
) -> Result<HashSet<ManagedPath>, BootstrapStreamingImportError> {
    let mut logical_names = HashSet::new();
    let mut portable_paths = HashSet::new();
    let mut authoritative = HashSet::new();
    let mut entries = capture.entries_cursor()?;
    while let Some(entry) = entries.next()? {
        let logical_name = LogicalPageName::parse(entry.logical_name().to_owned())
            .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;
        let logical_key = logical_name.key_digest();
        let portable_key = entry.path().portable_key();
        if logical_names.contains(&logical_key) || portable_paths.contains(&portable_key) {
            continue;
        }
        logical_names.insert(logical_key);
        portable_paths.insert(portable_key);
        authoritative.insert(entry.path().clone());
    }
    Ok(authoritative)
}

/// Parse each authoritative source page exactly once into the regime-neutral
/// terminal record consumed by both the clean baseline pack and SQLite. This
/// function deliberately knows nothing about semantic operations, parts,
/// receipts, or accepted history. The legacy bootstrap oracle translates the
/// returned records afterwards; the production redesign does not.
fn capture_activation_page_records(
    capture: &BootstrapSourceCapture,
    import_id: ImportId,
    workspace_id: WorkspaceId,
    instrumentation: &mut BootstrapStreamingImportInstrumentation,
) -> Result<ActivationPageRecordStore, BootstrapStreamingImportError> {
    let authoritative_paths = bootstrap_authoritative_source_paths(capture)?;
    let mut source_reader = BootstrapSourceReader::new(capture)?;
    let mut entries = capture.entries_cursor()?;
    let mut activation_pages = ActivationPageRecordBuilder::new();

    while let Some(entry) = entries.next()? {
        let bytes = source_reader.read_entry(&entry, instrumentation)?;
        if !authoritative_paths.contains(entry.path()) {
            continue;
        }
        let logical_name = LogicalPageName::parse(entry.logical_name().to_owned())
            .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;
        let source_leaf = clean_source_leaf_digest(
            entry.kind(),
            entry.path(),
            entry.description().sha256(),
            entry.description().byte_length(),
        )?;
        let page_id = import_id.unmatched_page_id(&ImportLocator::page(entry.path().clone()));
        let home_document_id =
            DocumentId::for_unmatched_import_page(workspace_id, entry.path().as_str().as_bytes());
        let full_span = (!bytes.is_empty())
            .then(|| StructuralSpan::new(0, bytes.len() as u64))
            .transpose()
            .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;

        let mut parser_instrumentation = ImportInstrumentation::default();
        let captured_page = capture.read_activation_page(&entry)?;
        let mut tree =
            decode_captured_activation_page_record(entry.path(), captured_page.as_slice())?;
        if tree.nodes.len() as u32 > MAX_PARSED_NODES_PER_SOURCE_FILE {
            return Err(BootstrapStreamingImportError::ResourceLimit {
                resource: "parser nodes per source file",
                observed: tree.nodes.len() as u64,
                limit: u64::from(MAX_PARSED_NODES_PER_SOURCE_FILE),
            });
        }
        // Source admission and parsing complete before the first semantic
        // operation for this external document is constructible.
        let page_name = logical_name.as_str().to_owned();
        let page_name_key = logical_name.canonical_key();
        instrumentation.parser_nodes = instrumentation
            .parser_nodes
            .checked_add(tree.nodes.len() as u64)
            .ok_or_else(|| {
                BootstrapStreamingImportError::InvalidSource(
                    "parser-node instrumentation overflow".into(),
                )
            })?;
        instrumentation.peak_owned_parser_nodes = instrumentation
            .peak_owned_parser_nodes
            .max(tree.nodes.len() as u64);
        let mut node_ids = Vec::with_capacity(tree.nodes.len());
        let mut terminal_blocks = Vec::with_capacity(tree.nodes.len());
        let mut activation_block_sources = Vec::with_capacity(tree.nodes.len());
        for index in 0..tree.nodes.len() {
            let locator = materialize_locator(&tree, index, &mut parser_instrumentation).map_err(
                |block| {
                    BootstrapStreamingImportError::InvalidSource(format!(
                        "{}: {}",
                        entry.path(),
                        block.detail
                    ))
                },
            )?;
            let block_id =
                import_id.unmatched_block_id(&ImportLocator::block(entry.path().clone(), locator));
            let parent = tree.nodes[index].parent.map(|parent| node_ids[parent]);
            node_ids.push(block_id);
            let facets = std::mem::take(&mut tree.nodes[index].projection_facets);
            terminal_blocks.push(super::MaterializedBlockInput {
                block_id,
                home_document_id,
                parent,
                order: imported_order(tree.nodes[index].sibling_position),
                content: std::mem::take(&mut tree.nodes[index].raw),
                searchable_text: facets.searchable_text,
                heading_level: facets.heading_level,
                collapsed: facets.collapsed,
                logseq_uuid: None,
                logseq_identity_origin: None,
                references: Vec::new(),
                properties: facets.properties,
                tags: facets.tags,
                task: facets.task,
            });
            activation_block_sources.push(ActivationBlockSourceV1 {
                span: tree.nodes[index].span,
                raw_ids: std::mem::take(&mut tree.nodes[index].raw_ids),
            });
        }
        let is_org = super::reference_catalog::reference_source_is_org(entry.path());
        let (preamble_search, _, _, properties, tags, _) = tree
            .preamble
            .as_deref()
            .map(|preamble| super::sqlite::document_facets(preamble, is_org))
            .unwrap_or_default();
        let mut searchable = page_name.clone();
        if !preamble_search.is_empty() {
            searchable.push(' ');
            searchable.push_str(&preamble_search);
        }
        let record = ActivationPageRecordV1::new(
            source_leaf,
            bytes,
            full_span,
            super::MaterializedPageInput {
                page_id,
                home_document_id,
                name: page_name,
                name_key: page_name_key,
                path: entry.path().clone(),
                kind: entry.kind(),
                preamble: tree.preamble,
                searchable_text: searchable,
                references: Vec::new(),
                properties,
                tags,
                blocks: terminal_blocks,
            },
            activation_block_sources,
        )?;
        activation_pages.push(record)?;
    }
    source_reader.finish()?;
    activation_pages.finish()
}

fn lazy_genesis_page_input(
    record: &ActivationPageRecordV1,
    sqlite_page: &super::MaterializedPageInput,
) -> Result<LazyGenesisPageInput, BootstrapStreamingImportError> {
    let blocks = record
        .page
        .blocks
        .iter()
        .zip(&record.block_sources)
        .map(|(block, source)| {
            let external_uuid_claims = if source.raw_ids.len() == 1 {
                LogseqUuid::parse(source.raw_ids[0].trim())
                    .ok()
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            LazyGenesisBlockInput {
                block_id: block.block_id,
                home_document_id: block.home_document_id,
                parent: block.parent,
                order: block.order.clone(),
                content: block.content.clone(),
                external_uuid_claims,
            }
        })
        .collect();
    Ok(LazyGenesisPageInput {
        source_leaf: record.source_leaf,
        exact_source_bytes: record.exact_source_bytes.clone(),
        page_id: record.page.page_id,
        home_document_id: record.page.home_document_id,
        name: record.page.name.clone(),
        path: record.page.path.clone(),
        kind: record.page.kind,
        preamble: record.page.preamble.clone(),
        blocks,
        document_checkpoint: Vec::new(),
        document_dependencies: None,
        sqlite_receipt: crate::oplog::lazy_genesis::LazyGenesisSqliteReceiptV1::new(
            &record.exact_source_bytes,
            sqlite_page,
        )?,
    })
}

/// Resolve the only graph-wide decision needed while constructing baseline
/// page checkpoints: an external Logseq UUID is installed when and only when
/// exactly one block claims it. Ambiguous claims remain visible in SQLite but
/// do not become CRDT block identity. This is the same deterministic policy as
/// the legacy identity-operation collapse, expressed without manufacturing an
/// operation.
fn unique_baseline_external_uuids(
    pages: &ActivationPageRecordStore,
) -> Result<BTreeMap<BlockId, LogseqUuid>, BootstrapStreamingImportError> {
    let mut claims = BTreeMap::<LogseqUuid, Option<BlockId>>::new();
    for page_id in pages.path_order() {
        let record = pages.page(*page_id)?.ok_or_else(|| {
            BootstrapStreamingImportError::InvalidOperation(
                "canonical activation page order names a missing page".into(),
            )
        })?;
        for (block, source) in record.page.blocks.iter().zip(&record.block_sources) {
            let Some(logseq_uuid) = (source.raw_ids.len() == 1)
                .then(|| LogseqUuid::parse(source.raw_ids[0].trim()).ok())
                .flatten()
            else {
                continue;
            };
            match claims.entry(logseq_uuid) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(Some(block.block_id));
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    entry.insert(None);
                }
            }
        }
    }
    Ok(claims
        .into_iter()
        .filter_map(|(logseq_uuid, block_id)| block_id.map(|block_id| (block_id, logseq_uuid)))
        .collect())
}

/// Construct the immutable baseline directly from terminal activation records.
/// It does not create semantic operations, batches, receipts, parts, or a
/// Patricia identity index. The record store may be memory-backed or spilled;
/// source pages are never reparsed.
fn build_lazy_genesis_from_activation_records(
    pages: &ActivationPageRecordStore,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    catalog_document_id: DocumentId,
    source_capture: BlobDescription,
    working: &Path,
) -> Result<LazyGenesisCandidate, BootstrapStreamingImportError> {
    let accepted_external_uuids = unique_baseline_external_uuids(pages)?;
    let mut lazy_genesis = LazyGenesisPackBuilder::new(
        workspace_id,
        lineage_digest,
        catalog_document_id,
        source_capture,
        working,
    )?;
    let mut checkpoints = LazyGenesisCheckpointBuilder::new(catalog_document_id)?;
    for page_id in pages.path_order() {
        let record = pages.page(*page_id)?.ok_or_else(|| {
            BootstrapStreamingImportError::InvalidOperation(
                "canonical activation page order names a missing page".into(),
            )
        })?;
        let page_assignments = record
            .page
            .blocks
            .iter()
            .filter_map(|block| {
                accepted_external_uuids
                    .get(&block.block_id)
                    .copied()
                    .map(|logseq_uuid| (block.block_id, logseq_uuid))
            })
            .collect();
        let sqlite_page = record.sqlite_page();
        let mut page = lazy_genesis_page_input(&record, &sqlite_page)?;
        let (checkpoint, dependencies) = checkpoints.push_page(&page, &page_assignments)?;
        page.document_checkpoint = checkpoint;
        page.document_dependencies = Some(dependencies);
        lazy_genesis.push(page)?;
    }
    let (catalog_checkpoint, catalog_dependencies) = checkpoints.finish()?;
    lazy_genesis
        .finish(catalog_checkpoint, catalog_dependencies)
        .map_err(Into::into)
}

/// The two unpublished products of one parser-owned activation-record pass.
/// Neither value is authority; dropping either removes its private candidate.
pub(crate) struct CleanActivationCandidates {
    baseline: LazyGenesisCandidate,
    sqlite: super::sqlite::CleanGenesisSqliteCandidate,
    accepted_frontier: AcceptedFrontierRoot,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CleanActivationInstrumentation {
    pub(crate) source_files: u64,
    pub(crate) source_bytes: u64,
    pub(crate) parser_nodes: u64,
    pub(crate) activation_record_bytes: u64,
    pub(crate) activation_records_spilled: bool,
    /// Largest source page observed while deriving the canonical activation
    /// inventory.  This is retained so the application readiness proof can
    /// exercise a worst-shaped page without walking the source tree again.
    pub(crate) largest_source_path: Option<String>,
    pub(crate) identity_scan_micros: u64,
    pub(crate) activation_record_micros: u64,
    pub(crate) candidate_fanout_micros: u64,
    pub(crate) candidate: CleanCandidateFanoutInstrumentation,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CleanCandidateFanoutInstrumentation {
    pub(crate) identity_claim_scan_micros: u64,
    pub(crate) record_and_input_micros: u64,
    pub(crate) checkpoint: super::hot_engine::LazyGenesisCheckpointInstrumentation,
    pub(crate) baseline_pack_micros: u64,
    pub(crate) sqlite_push_micros: u64,
    pub(crate) sqlite: super::sqlite::CleanGenesisProjectionInstrumentation,
    pub(crate) checkpoint_finish_micros: u64,
    pub(crate) baseline_finish_micros: u64,
    pub(crate) frontier_finish_micros: u64,
    pub(crate) sqlite_finish_micros: u64,
}

/// Move-only clean activation preparation. It retains the exact initial source
/// capture beside both unpublished products so the final source comparison
/// cannot accidentally verify a different activation episode.
pub(crate) struct CleanActivationPreparation {
    capture: BootstrapSourceCapture,
    candidates: CleanActivationCandidates,
    instrumentation: CleanActivationInstrumentation,
}

impl CleanActivationPreparation {
    pub(crate) const fn capture(&self) -> &BootstrapSourceCapture {
        &self.capture
    }

    pub(crate) const fn candidates(&self) -> &CleanActivationCandidates {
        &self.candidates
    }

    pub(crate) const fn instrumentation(&self) -> &CleanActivationInstrumentation {
        &self.instrumentation
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        BootstrapSourceCapture,
        LazyGenesisCandidate,
        super::sqlite::CleanGenesisSqliteCandidate,
        AcceptedFrontierRoot,
    ) {
        let (baseline, sqlite, accepted_frontier) = self.candidates.into_parts();
        (self.capture, baseline, sqlite, accepted_frontier)
    }
}

impl CleanActivationCandidates {
    pub(crate) const fn baseline(&self) -> &LazyGenesisCandidate {
        &self.baseline
    }

    pub(crate) const fn accepted_frontier(&self) -> &AcceptedFrontierRoot {
        &self.accepted_frontier
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        LazyGenesisCandidate,
        super::sqlite::CleanGenesisSqliteCandidate,
        AcceptedFrontierRoot,
    ) {
        (self.baseline, self.sqlite, self.accepted_frontier)
    }
}

/// Fan every canonical activation record into the immutable baseline and the
/// disposable SQLite projection in one pass. The preliminary UUID-claim scan
/// is the only graph-wide decision and retains only a bounded identity map; it
/// performs no parsing and emits no semantic operation.
fn build_clean_activation_candidates(
    pages: &ActivationPageRecordStore,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    catalog_document_id: DocumentId,
    source_capture: BlobDescription,
    working: &Path,
    database_path: &Path,
    policy: &ReferenceCatalogPolicyV1,
) -> Result<
    (
        CleanActivationCandidates,
        CleanCandidateFanoutInstrumentation,
    ),
    BootstrapStreamingImportError,
> {
    let mut instrumentation = CleanCandidateFanoutInstrumentation::default();
    let identity_started = Instant::now();
    let accepted_external_uuids = unique_baseline_external_uuids(pages)?;
    instrumentation.identity_claim_scan_micros = elapsed_micros(identity_started);
    let mut baseline = LazyGenesisPackBuilder::new(
        workspace_id,
        lineage_digest,
        catalog_document_id,
        source_capture,
        working,
    )?;
    let mut checkpoints = LazyGenesisCheckpointBuilder::new(catalog_document_id)?;
    let mut sqlite = super::sqlite::CleanGenesisProjectionBuilder::new(
        database_path,
        super::ProjectionClaim::current(workspace_id, lineage_digest),
        pages.page_count(),
    )?;

    for page_id in pages.path_order() {
        let record_started = Instant::now();
        let record = pages.page(*page_id)?.ok_or_else(|| {
            BootstrapStreamingImportError::InvalidOperation(
                "canonical activation page order names a missing page".into(),
            )
        })?;
        let page_assignments = record
            .page
            .blocks
            .iter()
            .filter_map(|block| {
                accepted_external_uuids
                    .get(&block.block_id)
                    .copied()
                    .map(|logseq_uuid| (block.block_id, logseq_uuid))
            })
            .collect();
        let sqlite_page = record.sqlite_page();
        let mut capsule = lazy_genesis_page_input(&record, &sqlite_page)?;
        instrumentation.record_and_input_micros = instrumentation
            .record_and_input_micros
            .saturating_add(elapsed_micros(record_started));
        let (checkpoint, dependencies) = checkpoints.push_page_with_instrumentation(
            &capsule,
            &page_assignments,
            &mut instrumentation.checkpoint,
        )?;
        capsule.document_checkpoint = checkpoint;
        capsule.document_dependencies = Some(dependencies);
        let baseline_started = Instant::now();
        baseline.push(capsule)?;
        instrumentation.baseline_pack_micros = instrumentation
            .baseline_pack_micros
            .saturating_add(elapsed_micros(baseline_started));
        let sqlite_started = Instant::now();
        sqlite.push_page(sqlite_page, policy)?;
        instrumentation.sqlite_push_micros = instrumentation
            .sqlite_push_micros
            .saturating_add(elapsed_micros(sqlite_started));
    }
    let checkpoint_finish_started = Instant::now();
    let (catalog_checkpoint, catalog_dependencies) = checkpoints.finish()?;
    instrumentation.checkpoint_finish_micros = elapsed_micros(checkpoint_finish_started);
    let baseline_finish_started = Instant::now();
    let baseline = baseline.finish(catalog_checkpoint, catalog_dependencies)?;
    instrumentation.baseline_finish_micros = elapsed_micros(baseline_finish_started);
    let frontier_finish_started = Instant::now();
    let accepted_frontier = super::hot_engine::accepted_frontier_root_for_lazy_genesis(&baseline)?;
    instrumentation.frontier_finish_micros = elapsed_micros(frontier_finish_started);
    let sqlite_finish_started = Instant::now();
    let sqlite = sqlite.finish(&accepted_frontier)?;
    instrumentation.sqlite_finish_micros = elapsed_micros(sqlite_finish_started);
    instrumentation.sqlite = sqlite.instrumentation();
    Ok((
        CleanActivationCandidates {
            baseline,
            sqlite,
            accepted_frontier,
        },
        instrumentation,
    ))
}

fn derive_clean_activation_import_id(
    capture: &BootstrapSourceCapture,
    workspace_id: WorkspaceId,
) -> Result<(ImportId, Option<String>), BootstrapStreamingImportError> {
    let source_count = usize::try_from(capture.source_file_count()).map_err(|_| {
        BootstrapStreamingImportError::ResourceLimit {
            resource: "source files",
            observed: capture.source_file_count(),
            limit: u64::from(MAX_SOURCE_INVENTORY_LEAVES),
        }
    })?;
    if source_count > MAX_SOURCE_INVENTORY_LEAVES as usize {
        return Err(BootstrapStreamingImportError::ResourceLimit {
            resource: "source files",
            observed: capture.source_file_count(),
            limit: u64::from(MAX_SOURCE_INVENTORY_LEAVES),
        });
    }
    let mut derivation =
        ImportIdDerivation::new(workspace_id, 0, source_count, DIFF_SCHEMA_VERSION)
            .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;
    derivation
        .begin_inventory()
        .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;
    let mut entries = capture.entries_cursor()?;
    let mut observed = 0_usize;
    let mut largest_source: Option<(u64, String)> = None;
    while let Some(entry) = entries.next()? {
        observed = observed.checked_add(1).ok_or_else(|| {
            BootstrapStreamingImportError::InvalidSource("source count overflow".into())
        })?;
        derivation
            .push_inventory(&ImportInventoryEntry::with_kind(
                entry.kind(),
                entry.path().clone(),
                ImportInventoryState::Present(entry.description()),
            ))
            .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;
        let path = entry.path().as_str().to_owned();
        let bytes = entry.description().byte_length();
        if largest_source
            .as_ref()
            .is_none_or(|(largest, largest_path)| {
                bytes > *largest || (bytes == *largest && path < *largest_path)
            })
        {
            largest_source = Some((bytes, path));
        }
    }
    if observed != source_count {
        return Err(BootstrapStreamingImportError::InvalidSource(
            "sealed source entry count differs from its capture".into(),
        ));
    }
    let import_id = derivation
        .finish()
        .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;
    Ok((import_id, largest_source.map(|(_, path)| path)))
}

/// Prepare the new operation-free activation episode. Source metadata is
/// scanned once to derive stable imported identities, each source page is then
/// parsed exactly once into a terminal activation record, and that record is
/// fanned out to the baseline pack and SQLite in one pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_clean_activation(
    graph: &Graph,
    capture: BootstrapSourceCapture,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    catalog_document_id: DocumentId,
    scratch_parent: &Path,
    database_path: &Path,
    policy: &ReferenceCatalogPolicyV1,
) -> Result<CleanActivationPreparation, BootstrapStreamingImportError> {
    fs::create_dir_all(scratch_parent)?;
    let canonical_scratch = fs::canonicalize(scratch_parent)?;
    let canonical_graph = fs::canonicalize(&graph.root)?;
    if canonical_scratch == canonical_graph || canonical_scratch.starts_with(&canonical_graph) {
        return Err(BootstrapStreamingImportError::InvalidSource(
            "clean activation scratch must be outside the graph".into(),
        ));
    }
    let working = canonical_scratch.join(format!("clean-activation-{}", Uuid::new_v4().simple()));
    create_private_directory(&working)?;
    let mut legacy_instrumentation = BootstrapStreamingImportInstrumentation::default();

    let started = Instant::now();
    let (import_id, largest_source_path) =
        derive_clean_activation_import_id(&capture, workspace_id)?;
    let identity_scan_micros = elapsed_micros(started);

    let started = Instant::now();
    let pages = capture_activation_page_records(
        &capture,
        import_id,
        workspace_id,
        &mut legacy_instrumentation,
    )?;
    let activation_record_micros = elapsed_micros(started);

    let started = Instant::now();
    let (candidates, candidate) = build_clean_activation_candidates(
        &pages,
        workspace_id,
        lineage_digest,
        catalog_document_id,
        capture.portable_capture_identity()?,
        &working,
        database_path,
        policy,
    )?;
    let candidate_fanout_micros = elapsed_micros(started);
    let instrumentation = CleanActivationInstrumentation {
        source_files: capture.source_file_count(),
        source_bytes: legacy_instrumentation.source_bytes_read,
        parser_nodes: legacy_instrumentation.parser_nodes,
        activation_record_bytes: pages.encoded_bytes() as u64,
        activation_records_spilled: pages.spilled(),
        largest_source_path,
        identity_scan_micros,
        activation_record_micros,
        candidate_fanout_micros,
        candidate,
    };
    Ok(CleanActivationPreparation {
        capture,
        candidates,
        instrumentation,
    })
}

/// Same-process ownership of a clean activation after the marker has made the
/// baseline authoritative. No fallible work occurs between marker publication
/// and construction of this receipt.
pub(crate) struct CommittedCleanActivation {
    baseline: LazyGenesisCandidate,
    projection: super::sqlite::CleanGenesisPhysicalProjection,
    accepted_frontier: AcceptedFrontierRoot,
    marker: LazyGenesisActivationMarkerV1,
    final_scan: BootstrapSourceCaptureInstrumentation,
}

impl CommittedCleanActivation {
    pub(crate) const fn baseline(&self) -> &LazyGenesisCandidate {
        &self.baseline
    }

    pub(crate) const fn accepted_frontier(&self) -> &AcceptedFrontierRoot {
        &self.accepted_frontier
    }

    pub(crate) const fn marker(&self) -> LazyGenesisActivationMarkerV1 {
        self.marker
    }

    pub(crate) const fn final_scan(&self) -> &BootstrapSourceCaptureInstrumentation {
        &self.final_scan
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        LazyGenesisCandidate,
        super::sqlite::CleanGenesisPhysicalProjection,
        AcceptedFrontierRoot,
        LazyGenesisActivationMarkerV1,
    ) {
        (
            self.baseline,
            self.projection,
            self.accepted_frontier,
            self.marker,
        )
    }
}

/// Publish both completed candidates, compare the live source bytes one final
/// time without parsing, and write the authority marker last. A failure before
/// the marker removes the disposable baseline and SQLite file set.
pub(crate) fn commit_clean_activation(
    graph: &Graph,
    preparation: CleanActivationPreparation,
    baseline_destination: &Path,
    enrollment_root: &Path,
) -> Result<CommittedCleanActivation, BootstrapStreamingImportError> {
    let (capture, baseline, sqlite, accepted_frontier) = preparation.into_parts();
    let database_path = sqlite.target_path().to_path_buf();
    let (baseline, _) = baseline.publish_durable(baseline_destination)?;
    let projection = sqlite.publish()?;

    let watcher_fence = graph.cache_generation();
    let final_scan = match capture.verify_before_inactive_bootstrap_authoring(graph) {
        Ok(scan) if graph.cache_generation() == watcher_fence => scan,
        Ok(_) => {
            drop(projection);
            super::sqlite::remove_disposable_projection(&database_path)?;
            return Err(BootstrapStreamingImportError::Io(io::Error::new(
                io::ErrorKind::Interrupted,
                "graph watcher generation changed during the final activation comparison",
            )));
        }
        Err(error) => {
            drop(projection);
            super::sqlite::remove_disposable_projection(&database_path)?;
            return Err(error.into());
        }
    };
    let marker = LazyGenesisActivationMarkerV1::new(
        baseline.workspace_id(),
        baseline.lineage_digest(),
        baseline.root(),
        baseline.source_capture(),
        super::sqlite::canonical_frontier_root_digest(&accepted_frontier)?,
        watcher_fence,
    )?;
    if let Err(error) = publish_activation_marker(enrollment_root, marker) {
        drop(projection);
        super::sqlite::remove_disposable_projection(&database_path)?;
        return Err(error.into());
    }
    // The marker makes the baseline's exact source bytes authoritative. The
    // sealed capture is now only a disposable construction duplicate. Cleanup
    // is best-effort because no post-marker failure may revoke the committed
    // activation; a later private-state sweep may remove any residue.
    let _ = capture.discard();
    Ok(CommittedCleanActivation {
        baseline: baseline.retain_as_authoritative(),
        projection,
        accepted_frontier,
        marker,
        final_scan,
    })
}

/// Cold-opened clean baseline and its matching disposable projection. This
/// proof deliberately stops before mutation authority; the runtime cutover
/// separately binds the workspace writer lease and ordinary operation tail.
pub(crate) struct OpenedCleanActivation {
    engine: ShardedHotEngine,
    projection: super::sqlite::CleanGenesisPhysicalProjection,
    marker: LazyGenesisActivationMarkerV1,
}

/// Cold-opened clean authority before choosing how to reconstruct its
/// disposable SQLite projection.  Keeping this boundary projection-free is
/// load bearing: a runtime with an accepted operation tail must rebuild SQLite
/// once at the final frontier, not first rebuild sequence zero and immediately
/// replace that temporary database with a second full rebuild.
pub(crate) struct OpenedCleanActivationAuthority {
    engine: ShardedHotEngine,
    baseline: std::sync::Arc<LazyGenesisCandidate>,
    marker: LazyGenesisActivationMarkerV1,
}

impl OpenedCleanActivationAuthority {
    pub(crate) const fn marker(&self) -> LazyGenesisActivationMarkerV1 {
        self.marker
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ShardedHotEngine,
        std::sync::Arc<LazyGenesisCandidate>,
        LazyGenesisActivationMarkerV1,
    ) {
        (self.engine, self.baseline, self.marker)
    }
}

/// Authenticate and open only the immutable clean authority. SQLite is a
/// disposable projection and is deliberately left unopened until the caller
/// has replayed the committed tail and knows the exact frontier it needs.
pub(crate) fn open_clean_activation_authority(
    enrollment_root: &Path,
    baseline_directory: &Path,
    catalog_document_id: DocumentId,
    policy: ReferenceCatalogPolicyV1,
) -> Result<Option<OpenedCleanActivationAuthority>, BootstrapStreamingImportError> {
    let Some(marker) = read_activation_marker(enrollment_root)? else {
        return Ok(None);
    };
    let baseline = std::sync::Arc::new(LazyGenesisCandidate::open_sealed_for_marker(
        baseline_directory,
        marker,
    )?);
    if baseline.catalog_document_id() != catalog_document_id {
        return Err(BootstrapStreamingImportError::InvalidSource(
            "clean activation marker names a different catalog document".into(),
        ));
    }
    let mut engine = ShardedHotEngine::new(
        marker.workspace_id(),
        marker.lineage_digest(),
        catalog_document_id,
    );
    engine.configure_reference_catalog_policy(policy)?;
    engine.install_lazy_genesis_baseline(std::sync::Arc::clone(&baseline))?;
    let accepted_frontier = engine.accepted_frontier_root()?;
    if super::sqlite::canonical_frontier_root_digest(&accepted_frontier)?
        != marker.accepted_frontier_digest()
    {
        return Err(BootstrapStreamingImportError::InvalidSource(
            "clean activation marker accepted frontier differs from its baseline".into(),
        ));
    }
    Ok(Some(OpenedCleanActivationAuthority {
        engine,
        baseline,
        marker,
    }))
}

impl OpenedCleanActivation {
    pub(crate) const fn engine(&self) -> &ShardedHotEngine {
        &self.engine
    }

    pub(crate) const fn marker(&self) -> LazyGenesisActivationMarkerV1 {
        self.marker
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ShardedHotEngine,
        super::sqlite::CleanGenesisPhysicalProjection,
        LazyGenesisActivationMarkerV1,
    ) {
        (self.engine, self.projection, self.marker)
    }
}

pub(crate) fn open_clean_activation(
    enrollment_root: &Path,
    baseline_directory: &Path,
    database_path: &Path,
    catalog_document_id: DocumentId,
    policy: ReferenceCatalogPolicyV1,
) -> Result<Option<OpenedCleanActivation>, BootstrapStreamingImportError> {
    let Some(opened) = open_clean_activation_authority(
        enrollment_root,
        baseline_directory,
        catalog_document_id,
        policy.clone(),
    )?
    else {
        return Ok(None);
    };
    let (engine, baseline, marker) = opened.into_parts();
    let projection = open_or_rebuild_clean_genesis_projection(
        database_path,
        super::ProjectionClaim::current(marker.workspace_id(), marker.lineage_digest()),
        &baseline,
        policy,
    )?;
    Ok(Some(OpenedCleanActivation {
        engine,
        projection,
        marker,
    }))
}

pub(crate) fn open_or_rebuild_clean_genesis_projection(
    database_path: &Path,
    claim: super::ProjectionClaim,
    baseline: &LazyGenesisCandidate,
    policy: ReferenceCatalogPolicyV1,
) -> Result<super::sqlite::CleanGenesisPhysicalProjection, BootstrapStreamingImportError> {
    let accepted_frontier = super::hot_engine::accepted_frontier_root_for_lazy_genesis(baseline)?;
    match super::sqlite::open_clean_genesis_projection(database_path, claim, &accepted_frontier) {
        Ok(projection) => {
            if matches!(std::env::var("TINE_DEBUG"), Ok(value) if !value.is_empty() && value != "0")
            {
                eprintln!("[tine] clean genesis projection recovery: opened-existing");
            }
            super::sqlite::record_projection_open_test_observation(
                claim.workspace_id(),
                "opened-existing",
                "",
                super::sqlite::RebuildInstrumentation::default(),
            );
            Ok(projection)
        }
        Err(error) => {
            if matches!(std::env::var("TINE_DEBUG"), Ok(value) if !value.is_empty() && value != "0")
            {
                eprintln!("[tine] clean genesis projection recovery: rebuilt");
            }
            let reason = error.to_string();
            let projection =
                rebuild_clean_genesis_projection(database_path, claim, baseline, policy)?;
            super::sqlite::record_projection_open_test_observation(
                claim.workspace_id(),
                "rebuilt-missing",
                &reason,
                super::sqlite::RebuildInstrumentation::default(),
            );
            Ok(projection)
        }
    }
}

fn materialize_lazy_genesis_page(
    page: LazyGenesisPageInput,
) -> Result<super::MaterializedPageInput, BootstrapStreamingImportError> {
    if let Some(receipt) = page.sqlite_receipt.as_ref() {
        if let Some(payload) = receipt.verified_payload(&page)? {
            return Ok(payload);
        }
    }
    let mut parser_instrumentation = ImportInstrumentation::default();
    let mut tree = parse_nodes(
        &page.path,
        &page.exact_source_bytes,
        &mut parser_instrumentation,
    )
    .map_err(|block| {
        BootstrapStreamingImportError::InvalidSource(format!("{}: {}", page.path, block.detail))
    })?;
    if tree.nodes.len() != page.blocks.len() || tree.preamble != page.preamble {
        return Err(BootstrapStreamingImportError::InvalidSource(
            "lazy genesis source bytes differ from their semantic capsule".into(),
        ));
    }
    let mut blocks = Vec::with_capacity(page.blocks.len());
    for index in 0..tree.nodes.len() {
        let stored = &page.blocks[index];
        let expected_parent = tree.nodes[index]
            .parent
            .map(|parent| page.blocks[parent].block_id);
        let expected_order = imported_order(tree.nodes[index].sibling_position);
        if stored.home_document_id != page.home_document_id
            || stored.parent != expected_parent
            || stored.order != expected_order
            || stored.content != tree.nodes[index].raw
        {
            return Err(BootstrapStreamingImportError::InvalidSource(
                "lazy genesis block identity differs from its exact source bytes".into(),
            ));
        }
        let facets = std::mem::take(&mut tree.nodes[index].projection_facets);
        let logseq_uuid = if stored.external_uuid_claims.len() == 1 {
            Some(stored.external_uuid_claims[0])
        } else {
            None
        };
        blocks.push(super::MaterializedBlockInput {
            block_id: stored.block_id,
            home_document_id: stored.home_document_id,
            parent: stored.parent,
            order: stored.order.clone(),
            content: stored.content.clone(),
            searchable_text: facets.searchable_text,
            heading_level: facets.heading_level,
            collapsed: facets.collapsed,
            logseq_uuid,
            logseq_identity_origin: logseq_uuid
                .map(|_| super::LogseqIdentityOrigin::ExternalImported),
            references: Vec::new(),
            properties: facets.properties,
            tags: facets.tags,
            task: facets.task,
        });
    }
    let is_org = super::reference_catalog::reference_source_is_org(&page.path);
    let (preamble_search, _, _, properties, tags, _) = page
        .preamble
        .as_deref()
        .map(|preamble| super::sqlite::document_facets(preamble, is_org))
        .unwrap_or_default();
    let logical_name = LogicalPageName::parse(page.name.clone())
        .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;
    let mut searchable = page.name.clone();
    if !preamble_search.is_empty() {
        searchable.push(' ');
        searchable.push_str(&preamble_search);
    }
    Ok(super::MaterializedPageInput {
        page_id: page.page_id,
        home_document_id: page.home_document_id,
        name: page.name,
        name_key: logical_name.canonical_key(),
        path: page.path,
        kind: page.kind,
        preamble: page.preamble,
        searchable_text: searchable,
        references: Vec::new(),
        properties,
        tags,
        blocks,
    })
}

fn rebuild_clean_genesis_projection(
    database_path: &Path,
    claim: super::ProjectionClaim,
    baseline: &LazyGenesisCandidate,
    policy: ReferenceCatalogPolicyV1,
) -> Result<super::sqlite::CleanGenesisPhysicalProjection, BootstrapStreamingImportError> {
    super::sqlite::remove_disposable_projection(database_path)?;
    let accepted_frontier = super::hot_engine::accepted_frontier_root_for_lazy_genesis(baseline)?;
    let mut builder = super::sqlite::CleanGenesisProjectionBuilder::new(
        database_path,
        claim,
        baseline.page_count(),
    )?;
    for page_id in baseline.page_ids() {
        let page = baseline.page(page_id)?.ok_or_else(|| {
            BootstrapStreamingImportError::InvalidSource(
                "lazy genesis manifest page disappeared during SQLite rebuild".into(),
            )
        })?;
        builder.push_page(materialize_lazy_genesis_page(page)?, &policy)?;
    }
    builder
        .finish(&accepted_frontier)?
        .publish()
        .map_err(Into::into)
}

#[cfg(not(test))]
fn record_capture_instrumentation(
    target: &mut BootstrapStreamingImportInstrumentation,
    source: &BootstrapSourceCaptureInstrumentation,
) {
    target.capture_passes = target.capture_passes.saturating_add(source.passes);
    target.external_sort_runs = target.external_sort_runs.saturating_add(source.sort_runs);
    target.source_bytes_read = target
        .source_bytes_read
        .saturating_add(source.physical_bytes);
    target.operation_spool_bytes = target
        .operation_spool_bytes
        .saturating_add(source.spool_bytes);
    target.peak_owned_sort_buffer_bytes = target
        .peak_owned_sort_buffer_bytes
        .max(source.peak_owned_buffer_bytes);
}

#[cfg(not(test))]
fn inactive_bootstrap_preparation_before_seal_hook() -> io::Result<()> {
    Ok(())
}

#[derive(Clone, Copy)]
struct ImportReplayLimits {
    entries: usize,
    base_bytes: u64,
    rendered_bytes: u64,
}

const IMPORT_REPLAY_LIMITS: ImportReplayLimits = ImportReplayLimits {
    entries: MAX_IMPORT_REPLAY_ENTRIES,
    base_bytes: MAX_IMPORT_REPLAY_BYTES,
    rendered_bytes: MAX_IMPORT_RENDERED_TARGET_BYTES,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactBytes {
    bytes: Vec<u8>,
    description: BlobDescription,
}

impl ExactBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        let description = BlobDescription::of(&bytes);
        Self { bytes, description }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn description(&self) -> BlobDescription {
        self.description
    }

    fn from_description(bytes: Vec<u8>, description: BlobDescription) -> Self {
        Self { bytes, description }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawObservation {
    Present(ExactBytes),
    Absent,
}

impl RawObservation {
    pub fn present(bytes: Vec<u8>) -> Self {
        Self::Present(ExactBytes::new(bytes))
    }

    pub const fn description(&self) -> Option<BlobDescription> {
        match self {
            Self::Present(bytes) => Some(bytes.description()),
            Self::Absent => None,
        }
    }
}

/// Exact graph observations keyed by exact, case-preserved managed paths.
///
/// Construction rejects duplicate requested paths instead of silently
/// overwriting one BTreeMap value with another.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RawInventory {
    entries: BTreeMap<ManagedPath, RawObservation>,
}

impl RawInventory {
    pub fn from_entries(
        entries: impl IntoIterator<Item = (ManagedPath, RawObservation)>,
    ) -> Result<Self, InventoryError> {
        let mut inventory = BTreeMap::new();
        let mut path_bytes = 0_u64;
        for (path, observation) in entries {
            if inventory.len() == MAX_IMPORT_FILES {
                return Err(InventoryError::ResourceBudgetExceeded {
                    resource: "managed file count",
                    observed: inventory.len().saturating_add(1) as u64,
                    limit: MAX_IMPORT_FILES as u64,
                });
            }
            path_bytes = charge_budget(
                "aggregate managed path bytes",
                path_bytes,
                path.as_str().len() as u64,
                MAX_IMPORT_PATH_BYTES,
            )?;
            if inventory.insert(path.clone(), observation).is_some() {
                return Err(InventoryError::DuplicateRequestedPath(
                    path.as_str().to_owned(),
                ));
            }
        }
        require_portable_unique(inventory.keys())?;
        Ok(Self { entries: inventory })
    }

    pub fn entries(&self) -> &BTreeMap<ManagedPath, RawObservation> {
        &self.entries
    }

    pub fn present(&self, path: &str) -> Option<&ExactBytes> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate.as_str() == path)
            .and_then(|(_, observation)| match observation {
                RawObservation::Present(bytes) => Some(bytes),
                RawObservation::Absent => None,
            })
    }

    fn derivation_entries(
        &self,
        path_identities: &BTreeMap<ManagedPath, ImportedPathIdentity>,
    ) -> Result<Vec<ImportInventoryEntry>, ImportExecutionError> {
        self.entries
            .iter()
            .map(|(path, observation)| {
                let identity =
                    path_identities
                        .get(path)
                        .ok_or(ImportExecutionError::IncompletePlan(
                            "sealed inventory path has no Graph-decoded managed kind",
                        ))?;
                Ok(ImportInventoryEntry::with_kind(
                    identity.kind,
                    path.clone(),
                    match observation {
                        RawObservation::Present(bytes) => {
                            ImportInventoryState::Present(bytes.description())
                        }
                        RawObservation::Absent => ImportInventoryState::Absent,
                    },
                ))
            })
            .collect()
    }
}

#[derive(Debug)]
pub enum InventoryError {
    UnsafePath(String),
    DuplicateRequestedPath(String),
    PortablePathCollision {
        first: String,
        second: String,
    },
    ResourceBudgetExceeded {
        resource: &'static str,
        observed: u64,
        limit: u64,
    },
    UnsupportedManagedLayout {
        pages_directory: String,
        journals_directory: String,
    },
    UnsafeEntry {
        path: Option<String>,
        message: String,
    },
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath(path) => write!(f, "unsafe managed path: {path:?}"),
            Self::DuplicateRequestedPath(path) => {
                write!(f, "managed path was requested more than once: {path}")
            }
            Self::PortablePathCollision { first, second } => write!(
                f,
                "managed paths share one portable key: {first} and {second}"
            ),
            Self::ResourceBudgetExceeded {
                resource,
                observed,
                limit,
            } => write!(
                f,
                "{resource} budget exceeded: observed {observed}, limit {limit}"
            ),
            Self::UnsupportedManagedLayout {
                pages_directory,
                journals_directory,
            } => write!(
                f,
                "unsupported managed layout: pages={pages_directory:?}, journals={journals_directory:?}"
            ),
            Self::UnsafeEntry { path, message } => match path {
                Some(path) => write!(f, "unsafe managed input {path}: {message}"),
                None => write!(f, "unsafe managed input: {message}"),
            },
        }
    }
}

impl std::error::Error for InventoryError {}

fn require_portable_unique<'a>(
    paths: impl IntoIterator<Item = &'a ManagedPath>,
) -> Result<(), InventoryError> {
    let mut portable = BTreeMap::new();
    for path in paths {
        if let Some(first) = portable.insert(path.portable_key(), path.as_str().to_owned()) {
            return Err(InventoryError::PortablePathCollision {
                first,
                second: path.as_str().to_owned(),
            });
        }
    }
    Ok(())
}

fn charge_budget(
    resource: &'static str,
    current: u64,
    amount: u64,
    limit: u64,
) -> Result<u64, InventoryError> {
    let observed = current.checked_add(amount).unwrap_or(u64::MAX);
    if observed > limit {
        Err(InventoryError::ResourceBudgetExceeded {
            resource,
            observed,
            limit,
        })
    } else {
        Ok(observed)
    }
}

fn reserve_base_replay(
    instrumentation: &mut ImportInstrumentation,
    declared_base_bytes: u64,
    limits: ImportReplayLimits,
    path: &ManagedPath,
) -> Result<(), ImportBlock> {
    if instrumentation.base_replay_entries == limits.entries {
        return Err(authority_block(
            ImportBlockReason::ResourceLimit,
            Some(path),
            format!(
                "base replay entry budget exceeded: limit {}",
                limits.entries
            ),
        ));
    }
    let replay_bytes = instrumentation
        .base_replay_bytes
        .checked_add(declared_base_bytes)
        .unwrap_or(u64::MAX);
    if replay_bytes > limits.base_bytes {
        return Err(authority_block(
            ImportBlockReason::ResourceLimit,
            Some(path),
            format!(
                "base replay byte budget exceeded: observed {replay_bytes}, limit {}",
                limits.base_bytes
            ),
        ));
    }
    instrumentation.base_replay_entries = instrumentation.base_replay_entries.saturating_add(1);
    instrumentation.base_replay_bytes = replay_bytes;
    Ok(())
}

fn retain_rendered_target(
    instrumentation: &mut ImportInstrumentation,
    bytes: u64,
    limits: ImportReplayLimits,
    path: &ManagedPath,
) -> Result<(), ImportBlock> {
    let rendered_bytes = instrumentation
        .rendered_target_bytes
        .checked_add(bytes)
        .unwrap_or(u64::MAX);
    if rendered_bytes > limits.rendered_bytes {
        return Err(authority_block(
            ImportBlockReason::ResourceLimit,
            Some(path),
            format!(
                "rendered target byte budget exceeded: observed {rendered_bytes}, limit {}",
                limits.rendered_bytes
            ),
        ));
    }
    instrumentation.rendered_target_bytes = rendered_bytes;
    Ok(())
}

/// Read only the explicitly named affected paths. No directory enumeration is
/// performed, including when a requested path is absent.
pub fn inventory_affected(
    graph: &Graph,
    requested_paths: &[&str],
) -> Result<RawInventory, InventoryError> {
    if requested_paths.len() > MAX_IMPORT_FILES {
        return Err(InventoryError::ResourceBudgetExceeded {
            resource: "requested managed path count",
            observed: requested_paths.len() as u64,
            limit: MAX_IMPORT_FILES as u64,
        });
    }
    let mut entries = Vec::with_capacity(requested_paths.len());
    let mut seen = BTreeSet::new();
    let mut portable = BTreeMap::new();
    let mut raw_bytes = 0_u64;
    for requested in requested_paths {
        let path = ManagedPath::parse((*requested).to_owned())
            .map_err(|_| InventoryError::UnsafePath((*requested).to_owned()))?;
        if !seen.insert(path.clone()) {
            return Err(InventoryError::DuplicateRequestedPath(
                path.as_str().to_owned(),
            ));
        }
        if let Some(first) = portable.insert(path.portable_key(), path.as_str().to_owned()) {
            return Err(InventoryError::PortablePathCollision {
                first,
                second: path.as_str().to_owned(),
            });
        }
        let observation = match graph.read_raw_managed_text(&path).map_err(|error| {
            InventoryError::UnsafeEntry {
                path: Some(path.as_str().to_owned()),
                message: error.to_string(),
            }
        })? {
            Some(observation) => {
                raw_bytes = charge_budget(
                    "aggregate raw bytes",
                    raw_bytes,
                    observation.bytes().len() as u64,
                    MAX_IMPORT_RAW_BYTES,
                )?;
                RawObservation::present(observation.into_bytes())
            }
            None => RawObservation::Absent,
        };
        entries.push((path, observation));
    }
    RawInventory::from_entries(entries)
}

/// The only whole-graph raw inventory entry point. It is intentionally named
/// for initial shadow import so ordinary reconciliation cannot obtain a global
/// scan accidentally.
///
/// This is capture evidence, not semantic publication authority. Shadow import
/// must repeat the same capability-bound inventory/semantic comparison at its
/// later import boundary; a caller may not retain this snapshot and assume the
/// live graph remained unchanged.
pub fn inventory_initial_shadow(graph: &Graph) -> Result<RawInventory, InventoryError> {
    let captured = graph
        .fresh_initial_shadow_raw_managed_text_inventory()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData
                && error.to_string().contains("bound exceeded")
            {
                InventoryError::ResourceBudgetExceeded {
                    resource: "initial shadow resources",
                    observed: 1,
                    limit: 0,
                }
            } else {
                InventoryError::UnsafeEntry {
                    path: None,
                    message: error.to_string(),
                }
            }
        })?;
    RawInventory::from_entries(
        captured
            .into_iter()
            .map(|(path, bytes)| (path, RawObservation::present(bytes))),
    )
}

/// Sealed import base. Only `capture_clean_import_scope` can mint one after the
/// enrolled receipt store and accepted engine jointly authenticate it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ReceiptBackedPage {
    intent: ProjectionIntent,
    completion: ProjectionCompletion,
    replayed_target: ExactBytes,
    page: super::MaterializedPage,
    source: ReceiptBaseSource,
}

impl ReceiptBackedPage {
    const fn page_id(&self) -> PageId {
        self.intent.page_id()
    }

    fn path(&self) -> &ManagedPath {
        self.intent.path()
    }

    const fn logical_completion_id(&self) -> LogicalCompletionId {
        self.completion.logical_completion_id()
    }

    fn bytes(&self) -> &[u8] {
        self.replayed_target.bytes()
    }

    const fn description(&self) -> BlobDescription {
        self.replayed_target.description()
    }

    fn annotations(&self) -> &[AnnotatedIdentity] {
        self.intent.annotations()
    }

    fn materialized_page(&self) -> &super::MaterializedPage {
        &self.page
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ScopedPathEvidence {
    Existing(ReceiptBackedPage),
    Released(LogicalCompletionId),
    New,
}

/// One complete affected-scope authority snapshot. Its fields and constructor
/// are private, so downstream code cannot omit an existing receipt, mix
/// frontiers, or relabel an engine-owned path as new.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportScopeSnapshot {
    workspace_id: WorkspaceId,
    paths: BTreeMap<ManagedPath, ScopedPathEvidence>,
    path_identities: BTreeMap<ManagedPath, ImportedPathIdentity>,
}

/// Exact logical identity of one requested external path, captured through the
/// Graph's normal managed-entry decoder before the sealed plan is built.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportedPathIdentity {
    name: LogicalPageName,
    kind: ManagedTextKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InventoryPathFingerprint {
    state: ImportInventoryState,
    file_resource_id: Option<ContentDigest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AffectedReceiptEntry {
    completed: Option<ProjectionCompletedReceipt>,
    bootstrap_base: Option<ExactBytes>,
    intent: ProjectionIntent,
    completion: ProjectionCompletion,
    source: ReceiptBaseSource,
}

/// In-memory provenance for a sealed import predecessor.  The correlated
/// Blocked variant is deliberately not serialized and can only be created
/// from the registry-gated hot-engine packet.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ReceiptBaseSource {
    Completed,
    Bootstrap,
    CleanBaseline,
    CleanManifest,
    CorrelatedBlocked {
        work_id: ProjectionWorkId,
        observed: BlobDescription,
    },
    CorrelatedReady {
        work_id: ProjectionWorkId,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AffectedReceiptCatalog {
    by_path: BTreeMap<ManagedPath, Vec<AffectedReceiptEntry>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CatalogAuthority {
    digest: ContentDigest,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ImportBlockReason {
    MissingBase,
    CorruptBase,
    AuthorityUnavailable,
    ConflictingLocalTail,
    StaleScope,
    DuplicateAnchorDependent,
    AmbiguousStructuralMatch,
    AmbiguousDestructiveMatch,
    TwoSidedDivergence,
    UnsafeInput,
    UnsupportedManagedLayout,
    ResourceLimit,
    PortablePathCollision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportBlock {
    pub reason: ImportBlockReason,
    pub paths: Vec<String>,
    pub logical_completion_ids: Vec<LogicalCompletionId>,
    pub observation: Option<(ManagedPath, ImportInventoryState)>,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportInstrumentation {
    pub requested_paths: usize,
    pub inventory_passes: usize,
    pub bytes_read: u64,
    pub bytes_hashed: u64,
    pub peak_owned_raw_bytes: u64,
    pub path_bytes: u64,
    pub catalog_entries: usize,
    pub catalog_bytes_hashed: u64,
    pub base_replay_entries: usize,
    pub base_replay_bytes: u64,
    pub rendered_target_bytes: u64,
    pub catalog_path_inserts: usize,
    pub catalog_path_lookups: usize,
    pub inventory_path_lookups: usize,
    pub present_document_parses: usize,
    pub authenticated_base_document_parses: usize,
    pub parsed_nodes: usize,
    pub max_depth: usize,
    pub locator_components_materialized: usize,
    pub structural_class_nodes: usize,
    pub structural_class_allocations: usize,
    pub structural_key_components: usize,
    pub structural_key_comparisons: usize,
    pub exact_bucket_inserts: usize,
    pub exact_bucket_lookups: usize,
    pub ordered_alignment_visits: usize,
    pub retained_block_matches: usize,
    pub anchored_page_match_set_inserts: usize,
    pub anchored_page_match_set_lookups: usize,
    pub anchored_page_owner_inserts: usize,
    pub anchored_page_owner_lookups: usize,
    pub anchored_page_uuid_owner_inserts: usize,
    pub anchored_page_uuid_owner_lookups: usize,
    pub anchored_page_edge_inserts: usize,
    pub rejected_raw_id_occurrences: usize,
}

impl ImportInstrumentation {
    /// Sum of explicitly recorded byte/component/event counters. This is a
    /// regression signal, not a claim that every platform/library comparison
    /// has one portable unit cost; independent hard ceilings remain authoritative.
    pub fn recorded_work_units(self) -> usize {
        let byte_work = self
            .bytes_read
            .saturating_add(self.bytes_hashed)
            .saturating_add(self.path_bytes)
            .saturating_add(self.catalog_bytes_hashed)
            .saturating_add(self.base_replay_bytes)
            .saturating_add(self.rendered_target_bytes);
        let byte_work = usize::try_from(byte_work).unwrap_or(usize::MAX);
        self.requested_paths
            .saturating_add(byte_work)
            .saturating_add(self.catalog_entries)
            .saturating_add(self.base_replay_entries)
            .saturating_add(self.catalog_path_inserts)
            .saturating_add(self.catalog_path_lookups)
            .saturating_add(self.inventory_path_lookups)
            .saturating_add(self.present_document_parses)
            .saturating_add(self.authenticated_base_document_parses)
            .saturating_add(self.parsed_nodes)
            .saturating_add(self.locator_components_materialized)
            .saturating_add(self.structural_class_nodes)
            .saturating_add(self.structural_class_allocations)
            .saturating_add(self.structural_key_components)
            .saturating_add(self.structural_key_comparisons)
            .saturating_add(self.exact_bucket_inserts)
            .saturating_add(self.exact_bucket_lookups)
            .saturating_add(self.ordered_alignment_visits)
            .saturating_add(self.retained_block_matches)
            .saturating_add(self.anchored_page_match_set_inserts)
            .saturating_add(self.anchored_page_match_set_lookups)
            .saturating_add(self.anchored_page_owner_inserts)
            .saturating_add(self.anchored_page_owner_lookups)
            .saturating_add(self.anchored_page_uuid_owner_inserts)
            .saturating_add(self.anchored_page_uuid_owner_lookups)
            .saturating_add(self.anchored_page_edge_inserts)
            .saturating_add(self.rejected_raw_id_occurrences)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageMatchBasis {
    SamePathCompletion,
    ReceiptBackedExactRename,
    ReceiptBackedAnchoredRename,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageImportMatch {
    path: ManagedPath,
    previous_path: ManagedPath,
    page_id: PageId,
    basis: PageMatchBasis,
}

impl PageImportMatch {
    pub fn path(&self) -> &ManagedPath {
        &self.path
    }

    pub fn previous_path(&self) -> &ManagedPath {
        &self.previous_path
    }

    pub const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub const fn basis(&self) -> PageMatchBasis {
        self.basis
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockMatchBasis {
    UniqueLogseqUuid,
    ReceiptStructuralExact,
    ReceiptOrderedTreeAlignment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockImportMatch {
    path: ManagedPath,
    locator: StructuralLocator,
    block_id: BlockId,
    basis: BlockMatchBasis,
}

impl BlockImportMatch {
    pub fn path(&self) -> &ManagedPath {
        &self.path
    }

    pub fn locator(&self) -> &StructuralLocator {
        &self.locator
    }

    pub const fn block_id(&self) -> BlockId {
        self.block_id
    }

    pub const fn basis(&self) -> BlockMatchBasis {
        self.basis
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectedRawIdReason {
    InvalidSyntax,
    Duplicate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedRawId {
    path: ManagedPath,
    locator: StructuralLocator,
    raw_value: String,
    reason: RejectedRawIdReason,
}

impl RejectedRawId {
    pub fn path(&self) -> &ManagedPath {
        &self.path
    }

    pub fn locator(&self) -> &StructuralLocator {
        &self.locator
    }

    pub fn raw_value(&self) -> &str {
        &self.raw_value
    }

    pub const fn reason(&self) -> RejectedRawIdReason {
        self.reason
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportMatches {
    pages: Vec<PageImportMatch>,
    blocks: Vec<BlockImportMatch>,
    rejected_raw_ids: Vec<RejectedRawId>,
}

impl ImportMatches {
    pub fn pages(&self) -> &[PageImportMatch] {
        &self.pages
    }

    pub fn blocks(&self) -> &[BlockImportMatch] {
        &self.blocks
    }

    pub fn rejected_raw_ids(&self) -> &[RejectedRawId] {
        &self.rejected_raw_ids
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportPlanStatus {
    Noop,
    Reconcile,
    Blocked,
}

/// Opaque diagnostic import result.
///
/// This read-only checkpoint deliberately carries no publication witness,
/// mutation capability, or reusable preflight authority. A later checkpoint
/// must recapture its predicates inside a one-shot semantic publisher.
///
/// ```compile_fail
/// use tine_core::oplog::{ImportPlan, ImportPlanStatus};
///
/// fn forge() -> ImportPlan {
///     ImportPlan {
///         status: ImportPlanStatus::Reconcile,
///         import_id: None,
///         inventory: None,
///         matches: None,
///         blocks: Vec::new(),
///         instrumentation: Default::default(),
///     }
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportPlan {
    status: ImportPlanStatus,
    import_id: Option<ImportId>,
    inventory: Option<RawInventory>,
    matches: Option<ImportMatches>,
    scope: Option<ImportScopeSnapshot>,
    execution: Option<ImportExecutionMaterial>,
    formatting: Option<ImportFormattingMaterial>,
    blocks: Vec<ImportBlock>,
    instrumentation: ImportInstrumentation,
}

impl ImportPlan {
    pub const fn status(&self) -> ImportPlanStatus {
        self.status
    }

    pub const fn import_id(&self) -> Option<ImportId> {
        self.import_id
    }

    pub fn inventory(&self) -> Option<&RawInventory> {
        self.inventory.as_ref()
    }

    pub fn matches(&self) -> Option<&ImportMatches> {
        self.matches.as_ref()
    }

    pub fn blocks(&self) -> &[ImportBlock] {
        &self.blocks
    }

    pub const fn instrumentation(&self) -> ImportInstrumentation {
        self.instrumentation
    }

    /// Return the sealed, non-authorizing execution material for one accepted
    /// reconciliation. The hot-engine adapter must still recapture all live
    /// predicates before it drafts or publishes a batch.
    pub(crate) fn execution_material(
        &self,
    ) -> Result<&ImportExecutionMaterial, ImportExecutionError> {
        match self.status {
            ImportPlanStatus::Noop | ImportPlanStatus::Blocked => {
                Err(ImportExecutionError::RefusedStatus(self.status))
            }
            ImportPlanStatus::Reconcile => {
                self.scope
                    .as_ref()
                    .ok_or(ImportExecutionError::IncompletePlan(
                        "reconcile plan has no sealed import scope",
                    ))?;
                self.execution
                    .as_ref()
                    .ok_or(ImportExecutionError::IncompletePlan(
                        "reconcile plan has no sealed execution material",
                    ))
            }
        }
    }

    /// Consume a reconciliable plan at the engine handoff boundary.  This
    /// deliberately moves the observation bytes: the execution adapter must
    /// not clone an already-bounded external observation just to author it.
    pub(crate) fn into_execution_material(
        mut self,
    ) -> Result<ImportExecutionMaterial, ImportExecutionError> {
        match self.status {
            ImportPlanStatus::Noop | ImportPlanStatus::Blocked => {
                Err(ImportExecutionError::RefusedStatus(self.status))
            }
            ImportPlanStatus::Reconcile => {
                self.scope
                    .take()
                    .ok_or(ImportExecutionError::IncompletePlan(
                        "reconcile plan has no sealed import scope",
                    ))?;
                self.execution
                    .take()
                    .ok_or(ImportExecutionError::IncompletePlan(
                        "reconcile plan has no sealed execution material",
                    ))
            }
        }
    }

    pub(crate) fn into_formatting_material(mut self) -> Option<ImportFormattingMaterial> {
        (self.status == ImportPlanStatus::Noop)
            .then(|| self.formatting.take())
            .flatten()
    }
}

/// Minimal crate-internal handoff from receipt-backed import planning to the
/// hot-engine draft adapter. It carries no write capability, engine reference,
/// captured projection input, or publish authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImportExecutionMaterial {
    import_id: ImportId,
    transaction: OperationTransaction,
    observation: ExternalImportObservationMaterial,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImportFormattingPage {
    page_id: PageId,
    path: ManagedPath,
    bytes: Vec<u8>,
    annotations: Vec<AnnotatedIdentity>,
}

impl ImportFormattingPage {
    pub(crate) const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub(crate) fn path(&self) -> &ManagedPath {
        &self.path
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn annotations(&self) -> &[AnnotatedIdentity] {
        &self.annotations
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImportFormattingMaterial {
    pages: Vec<ImportFormattingPage>,
}

impl ImportFormattingMaterial {
    pub(crate) fn pages(&self) -> &[ImportFormattingPage] {
        &self.pages
    }
}

// The hot-engine adapter consumes this sealed material for drafting only;
// capability recapture and publication remain separate authority boundaries.
impl ImportExecutionMaterial {
    pub(crate) const fn import_id(&self) -> ImportId {
        self.import_id
    }

    pub(crate) fn batch_id(&self) -> BatchId {
        self.import_id.batch_id()
    }

    pub(crate) const fn origin(&self) -> BatchOrigin {
        BatchOrigin::ExternalReconciliation {
            import_id: self.import_id,
        }
    }

    pub(crate) fn transaction(&self) -> &OperationTransaction {
        &self.transaction
    }

    pub(crate) fn observation(&self) -> &ExternalImportObservationMaterial {
        &self.observation
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ImportId,
        OperationTransaction,
        ExternalImportObservationMaterial,
    ) {
        (self.import_id, self.transaction, self.observation)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ImportExecutionError {
    RefusedStatus(ImportPlanStatus),
    IncompletePlan(&'static str),
    InvalidMaterial(String),
    OperationLimit,
    Observation(ExternalImportObservationMaterialError),
}

impl fmt::Display for ImportExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RefusedStatus(status) => {
                write!(
                    formatter,
                    "import plan status {status:?} cannot produce execution material"
                )
            }
            Self::IncompletePlan(detail) => formatter.write_str(detail),
            Self::InvalidMaterial(detail) => formatter.write_str(detail),
            Self::OperationLimit => write!(
                formatter,
                "external reconciliation operation count exceeds {MAX_TRANSACTION_OPERATIONS}"
            ),
            Self::Observation(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ImportExecutionError {}

impl From<ExternalImportObservationMaterialError> for ImportExecutionError {
    fn from(error: ExternalImportObservationMaterialError) -> Self {
        Self::Observation(error)
    }
}

/// Plan one exact external reconciliation against the clean
/// baseline-plus-manifest runtime.  This preserves the established structural
/// page/block matcher, but obtains every predecessor from the immutable clean
/// baseline or accepted manifests rather than the persistent Patricia
/// `ProjectionWorkIndex` and completed-receipt catalog.
pub(crate) fn plan_clean_affected_import(
    graph: &Graph,
    engine: &ShardedHotEngine,
    database: &SqliteFrontier,
    requested_paths: &[&str],
) -> ImportPlan {
    let mut instrumentation = ImportInstrumentation {
        requested_paths: requested_paths.len(),
        ..ImportInstrumentation::default()
    };
    let paths = match parse_requested_paths_with_portable_policy(
        requested_paths,
        PortablePathPolicy::SelectFirstExactPath,
    ) {
        Ok(paths) => paths,
        Err(error) => return blocked_inventory_error(error, instrumentation),
    };
    instrumentation.path_bytes = paths.iter().map(|path| path.as_str().len() as u64).sum();
    let accepted_frontier = match engine.accepted_frontier_root() {
        Ok(root) => root,
        Err(error) => {
            return blocked_authority_error(
                None,
                authority_block(
                    ImportBlockReason::AuthorityUnavailable,
                    None,
                    error.to_string(),
                ),
                instrumentation,
            )
        }
    };
    let (inventory, inventory_fingerprints, first_raw_bytes) =
        match capture_inventory(graph, &paths, true, 0, &mut instrumentation) {
            Ok((Some(inventory), fingerprints, raw_bytes)) => (inventory, fingerprints, raw_bytes),
            Ok((None, _, _)) => unreachable!("retaining capture returns inventory"),
            Err(error) => return blocked_inventory_error(error, instrumentation),
        };
    let (scope, predecessor_authority) = match capture_clean_import_scope(
        graph,
        engine,
        database,
        &paths,
        &inventory,
        &mut instrumentation,
    ) {
        Ok(scope) => scope,
        Err(block) => return blocked_authority_error(Some(inventory), block, instrumentation),
    };
    snapshot_revalidation_hook();
    let (_, second_fingerprints, _) =
        match capture_inventory(graph, &paths, false, first_raw_bytes, &mut instrumentation) {
            Ok(capture) => capture,
            Err(error) => {
                return blocked_authority_error(
                    Some(inventory),
                    authority_block(ImportBlockReason::StaleScope, None, error.to_string()),
                    instrumentation,
                )
            }
        };
    let post_predecessor_authority =
        match post_clean_import_predecessor_authority(engine, database, &paths) {
            Ok(authority) => authority,
            Err(block) => return blocked_authority_error(Some(inventory), block, instrumentation),
        };
    let post_frontier = match post_snapshot_frontier(engine) {
        Ok(root) => root,
        Err(error) => {
            return blocked_authority_error(
                Some(inventory),
                authority_block(ImportBlockReason::StaleScope, None, error.to_string()),
                instrumentation,
            )
        }
    };
    if inventory_fingerprints != second_fingerprints
        || predecessor_authority != post_predecessor_authority
        || accepted_frontier != post_frontier
    {
        return blocked_authority_error(
            Some(inventory),
            authority_block(
                ImportBlockReason::StaleScope,
                None,
                "inventory, clean manifest predecessor, or accepted frontier changed between snapshot passes",
            ),
            instrumentation,
        );
    }
    plan_import(
        graph,
        inventory,
        scope,
        engine,
        Some(database),
        instrumentation,
    )
}

#[cfg(test)]
fn post_clean_import_predecessor_authority(
    engine: &ShardedHotEngine,
    database: &SqliteFrontier,
    paths: &[ManagedPath],
) -> Result<CatalogAuthority, ImportBlock> {
    POST_CLEAN_PREDECESSOR_OVERRIDE
        .with(|authority| authority.borrow_mut().take())
        .map_or_else(
            || clean_import_predecessor_authority(engine, database, paths),
            Ok,
        )
}

#[cfg(not(test))]
fn post_clean_import_predecessor_authority(
    engine: &ShardedHotEngine,
    database: &SqliteFrontier,
    paths: &[ManagedPath],
) -> Result<CatalogAuthority, ImportBlock> {
    clean_import_predecessor_authority(engine, database, paths)
}

#[cfg(test)]
fn snapshot_revalidation_hook() {
    SNAPSHOT_REVALIDATION_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn snapshot_revalidation_hook() {}

#[cfg(test)]
fn post_snapshot_frontier(
    engine: &ShardedHotEngine,
) -> Result<AcceptedFrontierRoot, super::EngineError> {
    POST_FRONTIER_OVERRIDE
        .with(|root| root.borrow_mut().take())
        .map_or_else(|| engine.accepted_frontier_root(), Ok)
}

#[cfg(not(test))]
fn post_snapshot_frontier(
    engine: &ShardedHotEngine,
) -> Result<AcceptedFrontierRoot, super::EngineError> {
    engine.accepted_frontier_root()
}

/// What a requested set does with two exact paths that fold to one portable
/// identity (`Caf\u{e9}.md` and `Cafe\u{301}.md`, `Foo.md` and `foo.md`).
#[derive(Clone, Copy, Eq, PartialEq)]
enum PortablePathPolicy {
    /// The authenticated engine keeps ONE Patricia entry per portable path, so
    /// a requested set that folds is refused rather than half-applied.
    Refuse,
    /// Clean managed storage has no such index: activation already accepts a
    /// graph holding both spellings and selects the first exact path as its one
    /// authoritative source (`bootstrap_authoritative_source_paths`). The
    /// reconciler makes the SAME selection, because refusing the requested set
    /// denied every path in the graph for as long as both files existed — and
    /// two spellings of one name is the ordinary result of syncing a graph
    /// between a normalizing or case-folding filesystem and this one.
    SelectFirstExactPath,
}

fn parse_requested_paths(requested_paths: &[&str]) -> Result<Vec<ManagedPath>, InventoryError> {
    parse_requested_paths_with_portable_policy(requested_paths, PortablePathPolicy::Refuse)
}

fn parse_requested_paths_with_portable_policy(
    requested_paths: &[&str],
    portable: PortablePathPolicy,
) -> Result<Vec<ManagedPath>, InventoryError> {
    if requested_paths.len() > MAX_IMPORT_FILES {
        return Err(InventoryError::ResourceBudgetExceeded {
            resource: "requested managed path count",
            observed: requested_paths.len() as u64,
            limit: MAX_IMPORT_FILES as u64,
        });
    }
    let mut paths = Vec::with_capacity(requested_paths.len());
    let mut exact = BTreeSet::new();
    let mut path_bytes = 0_u64;
    for requested in requested_paths {
        path_bytes = charge_budget(
            "aggregate requested path bytes",
            path_bytes,
            requested.len() as u64,
            MAX_IMPORT_PATH_BYTES,
        )?;
        let path = ManagedPath::parse((*requested).to_owned())
            .map_err(|_| InventoryError::UnsafePath((*requested).to_owned()))?;
        if !exact.insert(path.clone()) {
            return Err(InventoryError::DuplicateRequestedPath(
                path.as_str().to_owned(),
            ));
        }
        paths.push(path);
    }
    paths.sort_unstable();
    match portable {
        PortablePathPolicy::Refuse => require_portable_unique(&paths)?,
        PortablePathPolicy::SelectFirstExactPath => {
            let mut selected = BTreeSet::new();
            paths.retain(|path| selected.insert(path.portable_key()));
        }
    }
    Ok(paths)
}

fn capture_inventory(
    graph: &Graph,
    paths: &[ManagedPath],
    retain: bool,
    retained_raw_bytes: u64,
    instrumentation: &mut ImportInstrumentation,
) -> Result<
    (
        Option<RawInventory>,
        BTreeMap<ManagedPath, InventoryPathFingerprint>,
        u64,
    ),
    InventoryError,
> {
    instrumentation.inventory_passes = instrumentation.inventory_passes.saturating_add(1);
    let mut entries = retain.then(|| Vec::with_capacity(paths.len()));
    let mut fingerprints = BTreeMap::new();
    let mut raw_bytes = 0_u64;
    for path in paths {
        let observation =
            graph
                .read_raw_managed_text(path)
                .map_err(|error| InventoryError::UnsafeEntry {
                    path: Some(path.as_str().to_owned()),
                    message: error.to_string(),
                })?;
        let (raw, fingerprint) = match observation {
            Some(observation) => {
                let description = observation.description();
                raw_bytes = charge_budget(
                    "aggregate raw bytes",
                    raw_bytes,
                    observation.bytes().len() as u64,
                    MAX_IMPORT_RAW_BYTES,
                )?;
                instrumentation.bytes_read = instrumentation
                    .bytes_read
                    .saturating_add(observation.physical_bytes_read());
                instrumentation.bytes_hashed = instrumentation
                    .bytes_hashed
                    .saturating_add(observation.physical_bytes_read());
                instrumentation.peak_owned_raw_bytes = instrumentation.peak_owned_raw_bytes.max(
                    retained_raw_bytes.saturating_add(observation.peak_capture_buffer_bytes()),
                );
                let fingerprint = InventoryPathFingerprint {
                    state: ImportInventoryState::Present(description),
                    file_resource_id: Some(observation.file_resource_id()),
                };
                let (bytes, description) = observation.into_parts();
                let raw = RawObservation::Present(ExactBytes::from_description(bytes, description));
                (raw, fingerprint)
            }
            None => (
                RawObservation::Absent,
                InventoryPathFingerprint {
                    state: ImportInventoryState::Absent,
                    file_resource_id: None,
                },
            ),
        };
        if let Some(entries) = &mut entries {
            entries.push((path.clone(), raw));
            instrumentation.peak_owned_raw_bytes =
                instrumentation.peak_owned_raw_bytes.max(raw_bytes);
        }
        fingerprints.insert(path.clone(), fingerprint);
    }
    let inventory = entries.map(RawInventory::from_entries).transpose()?;
    Ok((inventory, fingerprints, raw_bytes))
}

fn clean_sqlite_path_owner(
    read: &super::SqliteMaterializedRead<'_>,
    path: &ManagedPath,
) -> Result<Option<PageId>, ImportBlock> {
    let owners = read.pages_by_path(path, 2).map_err(|error| {
        authority_block(
            ImportBlockReason::AuthorityUnavailable,
            Some(path),
            format!("clean SQLite path lookup failed: {error}"),
        )
    })?;
    match owners.as_slice() {
        [] => Ok(None),
        [owner] => Ok(Some(owner.page_id)),
        _ => Err(authority_block(
            ImportBlockReason::CorruptBase,
            Some(path),
            "clean SQLite contains more than one exact path owner",
        )),
    }
}

fn clean_import_predecessor_authority(
    engine: &ShardedHotEngine,
    database: &SqliteFrontier,
    paths: &[ManagedPath],
) -> Result<CatalogAuthority, ImportBlock> {
    let read = database.materialized_read().map_err(|error| {
        authority_block(
            ImportBlockReason::AuthorityUnavailable,
            None,
            format!("clean SQLite predecessor authority is unavailable: {error}"),
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"tine/clean-import-predecessor-snapshot/v1\0");
    for path in paths {
        hasher.update((path.as_str().len() as u64).to_be_bytes());
        hasher.update(path.as_str().as_bytes());
        let sqlite_owner = clean_sqlite_path_owner(&read, path)?;
        let predecessor = engine
            .clean_import_projection_predecessor(path, sqlite_owner, &read)
            .map_err(|error| {
                authority_block(
                    ImportBlockReason::AuthorityUnavailable,
                    Some(path),
                    format!("clean projection predecessor is unavailable: {error}"),
                )
            })?;
        match predecessor {
            None => hasher.update(b"unowned\0"),
            Some(CleanImportProjectionPredecessor::Present {
                page,
                bytes,
                intent,
                completion,
                from_baseline,
            }) => {
                hasher.update(if from_baseline {
                    b"baseline-present\0".as_slice()
                } else {
                    b"manifest-present\0".as_slice()
                });
                hasher.update(page.page_id.as_uuid().as_bytes());
                hasher.update(BlobDescription::of(&bytes).sha256());
                hasher.update((bytes.len() as u64).to_be_bytes());
                let intent = intent.encode().map_err(|error| {
                    authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        error.to_string(),
                    )
                })?;
                let completion = completion.encode().map_err(|error| {
                    authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        error.to_string(),
                    )
                })?;
                hasher.update((intent.len() as u64).to_be_bytes());
                hasher.update(intent);
                hasher.update((completion.len() as u64).to_be_bytes());
                hasher.update(completion);
            }
            Some(CleanImportProjectionPredecessor::Released {
                prior_page_id,
                intent,
                completion,
            }) => {
                hasher.update(b"manifest-released\0");
                hasher.update(prior_page_id.as_uuid().as_bytes());
                let intent = intent.encode().map_err(|error| {
                    authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        error.to_string(),
                    )
                })?;
                let completion = completion.encode().map_err(|error| {
                    authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        error.to_string(),
                    )
                })?;
                hasher.update((intent.len() as u64).to_be_bytes());
                hasher.update(intent);
                hasher.update((completion.len() as u64).to_be_bytes());
                hasher.update(completion);
            }
        }
    }
    Ok(CatalogAuthority {
        digest: ContentDigest::from_bytes(hasher.finalize().into()),
    })
}

fn capture_clean_import_scope(
    graph: &Graph,
    engine: &ShardedHotEngine,
    database: &SqliteFrontier,
    requested_paths: &[ManagedPath],
    inventory: &RawInventory,
    instrumentation: &mut ImportInstrumentation,
) -> Result<(ImportScopeSnapshot, CatalogAuthority), ImportBlock> {
    let read = database.materialized_read().map_err(|error| {
        authority_block(
            ImportBlockReason::AuthorityUnavailable,
            None,
            format!("clean SQLite scope authority is unavailable: {error}"),
        )
    })?;
    let endpoint = engine.projection_endpoint_binding().ok_or_else(|| {
        authority_block(
            ImportBlockReason::AuthorityUnavailable,
            None,
            "clean import authority has no projection endpoint",
        )
    })?;
    if graph.canonical_resource_id().map_err(|error| {
        authority_block(
            ImportBlockReason::AuthorityUnavailable,
            None,
            error.to_string(),
        )
    })? != endpoint.graph_resource_id()
        || engine
            .require_index_free_clean_projection_runtime()
            .is_err()
    {
        return Err(authority_block(
            ImportBlockReason::AuthorityUnavailable,
            None,
            "graph or clean index-free engine binding differs",
        ));
    }

    let mut paths = BTreeMap::new();
    let mut path_identities = BTreeMap::new();
    for path in requested_paths {
        let entry = graph
            .managed_entry_for_managed_path(path)
            .map_err(|error| {
                authority_block(
                    ImportBlockReason::UnsafeInput,
                    Some(path),
                    format!("managed path cannot be decoded with Graph loading semantics: {error}"),
                )
            })?;
        let decoded_name = LogicalPageName::parse(entry.name).map_err(|error| {
            authority_block(
                ImportBlockReason::UnsafeInput,
                Some(path),
                format!("managed path has an invalid logical page name: {error}"),
            )
        })?;
        let decoded_kind = match entry.kind {
            PageKind::Page => ManagedTextKind::Page,
            PageKind::Journal => ManagedTextKind::Journal,
        };
        instrumentation.catalog_path_lookups =
            instrumentation.catalog_path_lookups.saturating_add(1);
        let sqlite_owner = clean_sqlite_path_owner(&read, path)?;
        let predecessor = engine
            .clean_import_projection_predecessor(path, sqlite_owner, &read)
            .map_err(|error| {
                authority_block(
                    ImportBlockReason::AuthorityUnavailable,
                    Some(path),
                    format!("clean projection predecessor is unavailable: {error}"),
                )
            })?;
        #[cfg(test)]
        let mut predecessor = predecessor;
        #[cfg(test)]
        DERANGE_NEXT_CLEAN_PREDECESSOR_PATH.with(|derange| {
            if derange.replace(false) {
                let Some(CleanImportProjectionPredecessor::Present { page, .. }) =
                    predecessor.as_mut()
                else {
                    panic!("clean predecessor derangement requires a present page");
                };
                page.path = ManagedPath::parse("pages/semantically-wrong.md").unwrap();
            }
        });
        match predecessor {
            None => {
                path_identities.insert(
                    path.clone(),
                    ImportedPathIdentity {
                        name: decoded_name,
                        kind: decoded_kind,
                    },
                );
                paths.insert(path.clone(), ScopedPathEvidence::New);
            }
            Some(CleanImportProjectionPredecessor::Released {
                prior_page_id: _,
                intent,
                completion,
            }) => {
                completion.validate_against(&intent).map_err(|error| {
                    authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        error.to_string(),
                    )
                })?;
                path_identities.insert(
                    path.clone(),
                    ImportedPathIdentity {
                        name: decoded_name,
                        kind: decoded_kind,
                    },
                );
                paths.insert(
                    path.clone(),
                    ScopedPathEvidence::Released(completion.logical_completion_id()),
                );
            }
            Some(CleanImportProjectionPredecessor::Present {
                page,
                bytes,
                intent,
                completion,
                from_baseline,
            }) => {
                completion.validate_against(&intent).map_err(|error| {
                    authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        error.to_string(),
                    )
                })?;
                if intent.workspace_id() != engine.workspace_id()
                    || intent.page_id() != page.page_id
                    || intent.path() != path
                    || intent.target() != BlobDescription::of(&bytes)
                    || page.path != *path
                {
                    return Err(authority_block(
                        ImportBlockReason::ConflictingLocalTail,
                        Some(path),
                        "clean projection predecessor differs from current accepted page",
                    ));
                }
                instrumentation.catalog_entries = instrumentation.catalog_entries.saturating_add(1);
                instrumentation.catalog_bytes_hashed = instrumentation
                    .catalog_bytes_hashed
                    .saturating_add(bytes.len() as u64);
                path_identities.insert(
                    path.clone(),
                    ImportedPathIdentity {
                        name: page.name.clone(),
                        kind: page.kind,
                    },
                );
                paths.insert(
                    path.clone(),
                    ScopedPathEvidence::Existing(ReceiptBackedPage {
                        replayed_target: ExactBytes::from_description(bytes, intent.target()),
                        page,
                        source: if from_baseline {
                            ReceiptBaseSource::CleanBaseline
                        } else {
                            ReceiptBaseSource::CleanManifest
                        },
                        intent,
                        completion,
                    }),
                );
            }
        }
    }
    if paths.len() != inventory.entries().len() {
        return Err(authority_block(
            ImportBlockReason::StaleScope,
            None,
            "clean predecessor and retained inventory path sets differ",
        ));
    }
    let authority = clean_import_predecessor_authority(engine, database, requested_paths)?;
    Ok((
        ImportScopeSnapshot {
            workspace_id: engine.workspace_id(),
            paths,
            path_identities,
        },
        authority,
    ))
}

fn authority_block(
    reason: ImportBlockReason,
    path: Option<&ManagedPath>,
    detail: impl Into<String>,
) -> ImportBlock {
    ImportBlock {
        reason,
        paths: path
            .into_iter()
            .map(|path| path.as_str().to_owned())
            .collect(),
        logical_completion_ids: Vec::new(),
        observation: None,
        detail: detail.into(),
    }
}

fn plan_import(
    graph: &Graph,
    inventory: RawInventory,
    mut scope: ImportScopeSnapshot,
    engine: &ShardedHotEngine,
    clean_database: Option<&SqliteFrontier>,
    mut instrumentation: ImportInstrumentation,
) -> ImportPlan {
    if scope.paths.len() != inventory.entries().len()
        || scope.path_identities.len() != inventory.entries().len()
        || scope
            .paths
            .keys()
            .zip(inventory.entries().keys())
            .any(|(left, right)| left != right)
        || scope
            .path_identities
            .keys()
            .zip(inventory.entries().keys())
            .any(|(left, right)| left != right)
    {
        return blocked_authority_error(
            Some(inventory),
            authority_block(
                ImportBlockReason::StaleScope,
                None,
                "sealed scope and exact inventory path sets differ",
            ),
            instrumentation,
        );
    }
    let completed = scope
        .paths
        .values()
        .filter_map(|evidence| match evidence {
            ScopedPathEvidence::Existing(page) => Some(page),
            ScopedPathEvidence::Released(_) | ScopedPathEvidence::New => None,
        })
        .collect::<Vec<_>>();

    let conflict_copy = inventory
        .entries()
        .keys()
        .find(|path| path_is_sync_conflict(Path::new(path.as_str())))
        .cloned();
    if let Some(path) = conflict_copy {
        return blocked_authority_error(
            Some(inventory),
            ImportBlock {
                reason: ImportBlockReason::UnsafeInput,
                paths: vec![path.as_str().to_owned()],
                logical_completion_ids: Vec::new(),
                observation: None,
                detail: "provider conflict copies are diagnostic inputs and cannot authorize import identity or deletion".into(),
            },
            instrumentation,
        );
    }

    let invalid_inventory = inventory.entries().iter().find_map(|(path, observation)| {
        instrumentation.inventory_path_lookups =
            instrumentation.inventory_path_lookups.saturating_add(1);
        matches!(observation, RawObservation::Present(bytes) if std::str::from_utf8(bytes.bytes()).is_err())
            .then(|| path.clone())
    });
    if let Some(path) = invalid_inventory {
        let block = ImportBlock {
            reason: ImportBlockReason::UnsafeInput,
            paths: vec![path.as_str().to_owned()],
            logical_completion_ids: Vec::new(),
            observation: inventory_observation(&inventory, path.as_str()),
            detail: "raw bytes were retained, but semantic import requires valid UTF-8".into(),
        };
        return blocked_authority_error(Some(inventory), block, instrumentation);
    }
    if let Some(page) = completed
        .iter()
        .find(|page| std::str::from_utf8(page.bytes()).is_err())
    {
        let block = receipt_block(
            ImportBlockReason::CorruptBase,
            page.path(),
            Some(page.logical_completion_id()),
            &inventory,
            "receipt-backed replay target is not UTF-8",
        );
        return blocked_authority_error(Some(inventory), block, instrumentation);
    }

    let page_matches = match match_pages(&inventory, &completed, &mut instrumentation) {
        Ok(matches) => matches,
        Err(block) => return blocked_authority_error(Some(inventory), block, instrumentation),
    };
    let mut matches = ImportMatches {
        pages: page_matches,
        ..ImportMatches::default()
    };
    let parsed_documents = match match_blocks(
        graph,
        &inventory,
        &completed,
        &mut matches,
        &mut instrumentation,
    ) {
        Ok(parsed) => parsed,
        Err(block) => {
            return blocked_authority_error(Some(inventory), block, instrumentation);
        }
    };
    if let Err(block) =
        match_anchored_page_moves(&inventory, &completed, &mut matches, &mut instrumentation)
    {
        return blocked_authority_error(Some(inventory), block, instrumentation);
    }
    let resolved_path_identities =
        match resolve_import_path_identities(&inventory, &matches, &scope, &parsed_documents) {
            Ok(identities) => identities,
            Err(block) => {
                return blocked_authority_error(Some(inventory), block, instrumentation);
            }
        };

    let mut completion_ids = completed
        .iter()
        .map(|page| page.logical_completion_id())
        .collect::<Vec<_>>();
    completion_ids.extend(scope.paths.values().filter_map(|evidence| match evidence {
        ScopedPathEvidence::Released(completion_id) => Some(*completion_id),
        ScopedPathEvidence::Existing(_) | ScopedPathEvidence::New => None,
    }));
    completion_ids.sort_unstable();
    completion_ids.dedup();
    let derivation_entries = match inventory.derivation_entries(&resolved_path_identities) {
        Ok(entries) => entries,
        Err(error) => {
            return blocked_authority_error(
                Some(inventory),
                ImportBlock {
                    reason: ImportBlockReason::StaleScope,
                    paths: Vec::new(),
                    logical_completion_ids: completion_ids,
                    observation: None,
                    detail: error.to_string(),
                },
                instrumentation,
            );
        }
    };
    let import_id = match ImportId::derive(
        scope.workspace_id,
        &completion_ids,
        &derivation_entries,
        DIFF_SCHEMA_VERSION,
    ) {
        Ok(import_id) => import_id,
        Err(error) => {
            return blocked_authority_error(
                Some(inventory),
                ImportBlock {
                    reason: ImportBlockReason::CorruptBase,
                    paths: Vec::new(),
                    logical_completion_ids: completion_ids,
                    observation: None,
                    detail: error.to_string(),
                },
                instrumentation,
            );
        }
    };

    let mut page_transition = match build_desired_page_transition(
        &inventory,
        &matches,
        &scope,
        &resolved_path_identities,
        import_id,
    ) {
        Ok(transition) => transition,
        Err(block) => {
            return blocked_authority_error(Some(inventory), block, instrumentation);
        }
    };
    let authority = match PageNameAuthority::open(engine, clean_database) {
        Ok(authority) => authority,
        Err(block) => return blocked_authority_error(Some(inventory), block, instrumentation),
    };
    let deduplicated = match retain_authoritative_desired_pages(&mut page_transition, &authority) {
        Ok(deduplicated) => deduplicated,
        Err(block) => return blocked_authority_error(Some(inventory), block, instrumentation),
    };
    if let Err(block) = preflight_desired_page_names(&inventory, &page_transition, &authority) {
        return blocked_authority_error(Some(inventory), block, instrumentation);
    }

    let completed_paths = completed
        .iter()
        .map(|page| page.path().clone())
        .collect::<BTreeSet<_>>();
    let deferred_absences = completed
        .iter()
        .filter(|page| {
            matches!(
                inventory.entries().get(page.path()),
                Some(RawObservation::Absent)
            ) && engine.restored_generation_requires_absence_deferral(page.page_id(), page.path())
        })
        .map(|page| (page.page_id(), page.path().clone()))
        .collect::<BTreeSet<_>>();
    for (page_id, path) in &deferred_absences {
        engine.note_deferred_absence_observation(*page_id, path);
    }
    let deferred_page_ids = deferred_absences
        .iter()
        .map(|(page_id, _)| *page_id)
        .collect::<BTreeSet<_>>();
    let deferred_paths = deferred_absences
        .iter()
        .map(|(_, path)| path.clone())
        .collect::<BTreeSet<_>>();
    let changed = completed.iter().any(|page| {
        instrumentation.inventory_path_lookups =
            instrumentation.inventory_path_lookups.saturating_add(1);
        !deferred_absences.contains(&(page.page_id(), page.path().clone()))
            && !matches!(
                inventory.entries().get(page.path()),
                Some(RawObservation::Present(bytes)) if bytes.description() == page.description()
            )
    }) || inventory.entries().iter().any(|(path, observation)| {
        matches!(observation, RawObservation::Present(_))
            && !completed_paths.contains(path)
            // A source this transaction deliberately does not import is not
            // evidence of a change. Without this it would be "new" on every
            // pass, and every quiet tick would author an empty batch.
            && !deduplicated.contains(path)
    });
    drop(completed);
    scope.path_identities = resolved_path_identities;
    let (status, scope, execution, formatting) = if changed {
        match build_execution_material(
            import_id,
            &inventory,
            &matches,
            &scope,
            &page_transition,
            &parsed_documents,
            &deferred_page_ids,
            &deferred_paths,
            &mut instrumentation,
        ) {
            Ok(BuiltImportMaterial::Semantic(execution)) => (
                ImportPlanStatus::Reconcile,
                Some(scope),
                Some(execution),
                None,
            ),
            Ok(BuiltImportMaterial::Formatting(formatting)) => {
                (ImportPlanStatus::Noop, None, None, Some(formatting))
            }
            Err(error) => {
                return blocked_authority_error(
                    Some(inventory),
                    ImportBlock {
                        reason: if matches!(&error, ImportExecutionError::OperationLimit) {
                            ImportBlockReason::ResourceLimit
                        } else {
                            ImportBlockReason::UnsafeInput
                        },
                        paths: Vec::new(),
                        logical_completion_ids: completion_ids,
                        observation: None,
                        detail: format!(
                            "sealed external reconciliation cannot produce canonical execution material: {error}"
                        ),
                    },
                    instrumentation,
                );
            }
        }
    } else {
        (ImportPlanStatus::Noop, None, None, None)
    };
    ImportPlan {
        status,
        import_id: Some(import_id),
        inventory: Some(inventory),
        matches: Some(matches),
        scope,
        execution,
        formatting,
        blocks: Vec::new(),
        instrumentation,
    }
}

/// The one current owner of a canonical page name, read from whichever
/// authority this runtime keeps it in.
///
/// Clean managed storage deliberately has no second resident page-name index.
/// Its disposable SQLite projection is the current name authority, just as it
/// is for path ownership. Consulting the empty run-local fallback there used to
/// let an ordinary collision pass preflight and fail only after authoring,
/// poisoning the actor with an unpublished manifest.
struct PageNameAuthority<'a> {
    clean: Option<super::SqliteMaterializedRead<'a>>,
    engine: &'a ShardedHotEngine,
}

impl<'a> PageNameAuthority<'a> {
    fn open(
        engine: &'a ShardedHotEngine,
        clean_database: Option<&'a SqliteFrontier>,
    ) -> Result<Self, ImportBlock> {
        let clean = clean_database
            .map(SqliteFrontier::materialized_read)
            .transpose()
            .map_err(|error| {
                authority_block(
                    ImportBlockReason::AuthorityUnavailable,
                    None,
                    format!("clean SQLite page-name authority is unavailable: {error}"),
                )
            })?;
        Ok(Self { clean, engine })
    }

    fn owner(
        &self,
        name: &LogicalPageName,
        path: &ManagedPath,
    ) -> Result<Option<PageId>, ImportBlock> {
        match self.clean.as_ref() {
            Some(read) => Ok(read
                .causal_page_name_identity_record(name.key_digest())
                .map_err(|error| {
                    authority_block(
                        ImportBlockReason::AuthorityUnavailable,
                        Some(path),
                        format!("clean SQLite logical page-name lookup failed: {error}"),
                    )
                })?
                .and_then(|record| record.occupied().map(|owner| owner.page_id()))),
            None => self
                .engine
                .current_page_for_logical_name(name)
                .map_err(|error| {
                    authority_block(
                        ImportBlockReason::AuthorityUnavailable,
                        Some(path),
                        format!("authenticated logical page-name lookup failed: {error}"),
                    )
                }),
        }
    }
}

/// Apply activation's deterministic source selection to the affected set, so a
/// graph that already holds two physical files for one canonical page name
/// keeps reconciling instead of refusing every path in the transaction.
///
/// `bootstrap_authoritative_source_paths` selects ONE authoritative source per
/// canonical page name and per portable path at activation, matching OG's
/// "retain the first, skip the later collision"
/// (`frontend.handler.repo/parse-files-and-load-to-db!`). Every later member
/// stays on disk as ordinary graph text with no page of its own. Reconciliation
/// then met that same file as a brand-new page whose decoded name was already
/// owned, and refused the whole transaction — for every path, on every tick,
/// with no way for the user to make progress (GH: Android, 2026-08-18).
///
/// A source that carries no accepted page identity and cannot acquire the name
/// it decodes to therefore acquires no identity at all here either. Its exact
/// bytes stay on disk and are still observed; only the semantic page is
/// withheld. An accepted page is never withdrawn this way: it keeps whatever
/// identity it already has, and a real title change into a taken name remains
/// the visible ambiguity the preflight below refuses.
fn retain_authoritative_desired_pages(
    transition: &mut DesiredPageTransition,
    authority: &PageNameAuthority<'_>,
) -> Result<BTreeSet<ManagedPath>, ImportBlock> {
    let mut claimed = BTreeMap::<super::PageNameKeyDigest, PageId>::new();
    let mut deduplicated = BTreeSet::new();
    for (path, page) in transition
        .pages
        .iter()
        .filter(|(_, page)| page.acquires_name)
    {
        let key = page.name.key_digest();
        let claimant = claimed
            .get(&key)
            .copied()
            .filter(|claimant| *claimant != page.page_id);
        // An authority that cannot answer is never read as "the name is free".
        let owner = authority.owner(&page.name, path)?.filter(|owner| {
            *owner != page.page_id && !transition.released_name_owners.contains(owner)
        });
        match claimant.or(owner) {
            Some(_) if !page.existing => {
                deduplicated.insert(path.clone());
            }
            Some(_) => {}
            None => {
                claimed.insert(key, page.page_id);
            }
        }
    }
    for path in &deduplicated {
        transition.pages.remove(path);
    }
    Ok(deduplicated)
}

/// Refuse a transaction before authoring when two affected files would acquire
/// one logical page name, or when an affected destination name is already
/// owned by another authenticated page.  Paths are deliberately not used as a
/// name namespace: duplicate basenames at different paths remain visible
/// ambiguity instead of a silently successful reconciliation.
fn preflight_desired_page_names(
    inventory: &RawInventory,
    transition: &DesiredPageTransition,
    authority: &PageNameAuthority<'_>,
) -> Result<(), ImportBlock> {
    let mut desired = BTreeMap::new();
    for (path, page) in transition
        .pages
        .iter()
        .filter(|(_, page)| page.acquires_name)
    {
        if let Some((prior_path, prior_page_id, prior_name)) = desired.insert(
            page.name.key_digest(),
            (path.clone(), page.page_id, page.name.clone()),
        ) {
            if prior_page_id != page.page_id {
                return Err(ImportBlock {
                    reason: ImportBlockReason::ConflictingLocalTail,
                    paths: vec![prior_path.as_str().to_owned(), path.as_str().to_owned()],
                    logical_completion_ids: Vec::new(),
                    observation: inventory_observation(inventory, path.as_str()),
                    detail: format!(
                        "affected paths decode to the same logical page name: {} and {}",
                        prior_name.as_str(),
                        page.name.as_str()
                    ),
                });
            }
        }
    }
    for (_, (path, page_id, name)) in desired {
        let owner = authority.owner(&name, &path)?;
        if owner.is_some_and(|owner| {
            owner != page_id && !transition.released_name_owners.contains(&owner)
        }) {
            // Said in the user's words, because this is the text a device
            // showed the user once per tick: a page name and a UUID, neither of
            // which appears anywhere in the app. Page names fold case and
            // Unicode normalization here exactly as they do in Logseq
            // (`canonical_page_name_key`), so "differ by more than
            // capitalisation, accent spelling, or # vs %23" is the action that
            // actually resolves it. The escape case is not hypothetical: a
            // reported graph held one title twice, once with a literal `#` and
            // once with `%23`, both files written by Logseq years earlier — so
            // a message naming only capitalisation would read as "not my
            // problem" to the person actually hitting it. The sentence is the
            // same on a filesystem that folds those names into
            // one file (`docs/storage-sync-contract.md` §2.10d). The owning
            // page id stays at the end for diagnosis.
            return Err(authority_block(
                ImportBlockReason::ConflictingLocalTail,
                Some(&path),
                format!(
                    "another file in this graph is already the page \u{201c}{}\u{201d}, so \
                     {} cannot take that name too — the two file names differ only in a way \
                     Tine and Logseq both ignore when they read a page name: capitalisation, \
                     accent spelling, or writing a character literally where the other escapes \
                     it (a title containing # can be stored as # or as %23). Rename one file \
                     if you meant two different pages (decoded destination logical page name \
                     is already owned by page {})",
                    name.as_str(),
                    path.as_str(),
                    owner.expect("checked above")
                ),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct CurrentImportBlock {
    page_id: PageId,
    block: super::MaterializedBlock,
}

#[derive(Clone, Debug)]
struct DesiredImportPage {
    page_id: PageId,
    home_document_id: DocumentId,
    name: LogicalPageName,
    path: ManagedPath,
    kind: ManagedTextKind,
    existing: bool,
    acquires_name: bool,
}

#[derive(Clone, Debug)]
struct DesiredPageTransition {
    pages: BTreeMap<ManagedPath, DesiredImportPage>,
    /// Current affected owners whose present logical name is absent from the
    /// final ownership set for that same page identity. The page-name index
    /// validates a transaction's final catalog atomically, so chains and cycles
    /// may consume these released names without exposing an intermediate state.
    released_name_owners: BTreeSet<PageId>,
}

fn imported_identity(
    name: &str,
    kind: PageKind,
    path: &ManagedPath,
) -> Result<ImportedPathIdentity, ImportBlock> {
    let name = LogicalPageName::parse(name.to_owned()).map_err(|error| {
        authority_block(
            ImportBlockReason::UnsafeInput,
            Some(path),
            format!("parsed external document has an invalid logical page name: {error}"),
        )
    })?;
    let kind = match kind {
        PageKind::Page => ManagedTextKind::Page,
        PageKind::Journal => ManagedTextKind::Journal,
    };
    Ok(ImportedPathIdentity { name, kind })
}

/// Resolve current document semantics without collapsing filename fallback,
/// accepted identity, and parser-declared title authority.
fn resolve_import_path_identities(
    inventory: &RawInventory,
    matches: &ImportMatches,
    scope: &ImportScopeSnapshot,
    parsed: &ParsedImportDocuments,
) -> Result<BTreeMap<ManagedPath, ImportedPathIdentity>, ImportBlock> {
    let mut identities = scope.path_identities.clone();
    let page_matches = matches
        .pages()
        .iter()
        .map(|matched| (matched.path(), matched))
        .collect::<BTreeMap<_, _>>();
    for (path, observation) in inventory.entries() {
        if !matches!(observation, RawObservation::Present(_)) {
            continue;
        }
        let current = parsed.current.get(path).ok_or_else(|| {
            authority_block(
                ImportBlockReason::CorruptBase,
                Some(path),
                "present external document has no parser-owned semantic result",
            )
        })?;
        let accepted = match page_matches.get(path) {
            None => None,
            Some(matched) if matched.basis() != PageMatchBasis::SamePathCompletion => None,
            Some(matched) => {
                let Some(ScopedPathEvidence::Existing(existing)) =
                    scope.paths.get(matched.previous_path())
                else {
                    return Err(authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        "same-path match has no authenticated accepted predecessor",
                    ));
                };
                let base = parsed.base.get(matched.previous_path()).ok_or_else(|| {
                    receipt_block(
                        ImportBlockReason::CorruptBase,
                        matched.previous_path(),
                        Some(existing.logical_completion_id()),
                        inventory,
                        "authenticated completed-base document has no parser-owned semantic result",
                    )
                })?;
                Some(AcceptedExternalDocumentIdentity {
                    name: existing.materialized_page().name.as_str(),
                    kind: match existing.materialized_page().kind {
                        ManagedTextKind::Page => PageKind::Page,
                        ManagedTextKind::Journal => PageKind::Journal,
                    },
                    explicit_title: base.explicit_title.as_deref(),
                })
            }
        };
        let identity = resolve_external_document_identity(
            current.explicit_title.as_deref(),
            &current.filename_fallback,
            &current.effective,
            accepted,
        );
        let identity = imported_identity(&identity.name, identity.kind, path)?;
        identities.insert(path.clone(), identity);
    }
    Ok(identities)
}

fn build_desired_page_transition(
    inventory: &RawInventory,
    matches: &ImportMatches,
    scope: &ImportScopeSnapshot,
    path_identities: &BTreeMap<ManagedPath, ImportedPathIdentity>,
    import_id: ImportId,
) -> Result<DesiredPageTransition, ImportBlock> {
    let mut page_matches = BTreeMap::<ManagedPath, &PageImportMatch>::new();
    for page_match in matches.pages() {
        if page_matches
            .insert(page_match.path().clone(), page_match)
            .is_some()
        {
            return Err(authority_block(
                ImportBlockReason::CorruptBase,
                Some(page_match.path()),
                "sealed import matches contain duplicate external page paths",
            ));
        }
    }

    let mut pages = BTreeMap::new();
    let mut desired_paths_by_page = BTreeMap::new();
    for (path, observation) in inventory.entries() {
        if !matches!(observation, RawObservation::Present(_)) {
            continue;
        }
        let path_identity = path_identities.get(path).ok_or_else(|| {
            authority_block(
                ImportBlockReason::StaleScope,
                Some(path),
                "present inventory path has no Graph-decoded logical identity",
            )
        })?;
        let desired = match page_matches.get(path) {
            Some(page_match) => {
                let Some(ScopedPathEvidence::Existing(existing)) =
                    scope.paths.get(page_match.previous_path())
                else {
                    return Err(authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        "matched external page has no sealed receipt-backed predecessor",
                    ));
                };
                let current = existing.materialized_page();
                if current.page_id != page_match.page_id() {
                    return Err(authority_block(
                        ImportBlockReason::CorruptBase,
                        Some(path),
                        "matched external page identity differs from its sealed predecessor",
                    ));
                }
                DesiredImportPage {
                    page_id: current.page_id,
                    home_document_id: current.home_document_id,
                    name: path_identity.name.clone(),
                    path: path.clone(),
                    kind: path_identity.kind,
                    existing: true,
                    acquires_name: current.name != path_identity.name,
                }
            }
            None => {
                let home_document_id = match scope.paths.get(path) {
                    Some(ScopedPathEvidence::Released(completion_id)) => {
                        DocumentId::for_released_import_page(
                            scope.workspace_id,
                            path.as_str().as_bytes(),
                            *completion_id,
                        )
                    }
                    _ => DocumentId::for_unmatched_import_page(
                        scope.workspace_id,
                        path.as_str().as_bytes(),
                    ),
                };
                DesiredImportPage {
                    page_id: import_id.unmatched_page_id(&ImportLocator::page(path.clone())),
                    home_document_id,
                    name: path_identity.name.clone(),
                    path: path.clone(),
                    kind: path_identity.kind,
                    existing: false,
                    acquires_name: true,
                }
            }
        };
        if let Some(prior_path) = desired_paths_by_page.insert(desired.page_id, path.clone()) {
            return Err(ImportBlock {
                reason: ImportBlockReason::ConflictingLocalTail,
                paths: vec![prior_path.as_str().to_owned(), path.as_str().to_owned()],
                logical_completion_ids: Vec::new(),
                observation: inventory_observation(inventory, path.as_str()),
                detail: "one affected page identity would survive at more than one path".into(),
            });
        }
        pages.insert(path.clone(), desired);
    }

    let final_names_by_page = pages
        .values()
        .map(|page| (page.page_id, page.name.key_digest()))
        .collect::<BTreeMap<_, _>>();
    let released_name_owners = scope
        .paths
        .values()
        .filter_map(|evidence| {
            let ScopedPathEvidence::Existing(existing) = evidence else {
                return None;
            };
            let current = existing.materialized_page();
            (final_names_by_page.get(&current.page_id).copied() != Some(current.name.key_digest()))
                .then_some(current.page_id)
        })
        .collect();
    Ok(DesiredPageTransition {
        pages,
        released_name_owners,
    })
}

#[derive(Clone, Debug)]
struct DesiredImportBlock {
    block_id: BlockId,
    page_id: PageId,
    home_document_id: DocumentId,
    parent: Option<BlockId>,
    order: String,
    content: String,
    logseq_uuid: Option<LogseqUuid>,
    existing: bool,
}

enum BuiltImportMaterial {
    Semantic(ImportExecutionMaterial),
    Formatting(ImportFormattingMaterial),
}

fn push_operation(
    operations: &mut Vec<SemanticOperation>,
    operation: SemanticOperation,
) -> Result<(), ImportExecutionError> {
    if operations.len() == MAX_TRANSACTION_OPERATIONS {
        return Err(ImportExecutionError::OperationLimit);
    }
    operations.push(operation);
    Ok(())
}

fn build_execution_material(
    import_id: ImportId,
    inventory: &RawInventory,
    matches: &ImportMatches,
    scope: &ImportScopeSnapshot,
    page_transition: &DesiredPageTransition,
    parsed_documents: &ParsedImportDocuments,
    deferred_page_ids: &BTreeSet<PageId>,
    deferred_paths: &BTreeSet<ManagedPath>,
    instrumentation: &mut ImportInstrumentation,
) -> Result<BuiltImportMaterial, ImportExecutionError> {
    let mut current_pages = BTreeMap::<PageId, &ReceiptBackedPage>::new();
    let mut current_blocks = BTreeMap::<BlockId, CurrentImportBlock>::new();
    for evidence in scope.paths.values() {
        let ScopedPathEvidence::Existing(page) = evidence else {
            continue;
        };
        let materialized = page.materialized_page();
        if materialized.page_id != page.page_id() || materialized.path != *page.path() {
            return Err(ImportExecutionError::InvalidMaterial(
                "receipt-backed page identity does not match its materialized accepted state"
                    .into(),
            ));
        }
        if current_pages.insert(materialized.page_id, page).is_some() {
            return Err(ImportExecutionError::InvalidMaterial(
                "sealed import scope contains one page more than once".into(),
            ));
        }
        for block in &materialized.blocks {
            if current_blocks
                .insert(
                    block.block_id,
                    CurrentImportBlock {
                        page_id: materialized.page_id,
                        block: block.clone(),
                    },
                )
                .is_some()
            {
                return Err(ImportExecutionError::InvalidMaterial(
                    "sealed import scope contains one visible block more than once".into(),
                ));
            }
        }
    }

    let desired_pages = &page_transition.pages;

    let trees = &parsed_documents.current;

    let mut block_matches = BTreeMap::<(ManagedPath, StructuralLocator), BlockId>::new();
    for block_match in matches.blocks() {
        if !trees.contains_key(block_match.path()) {
            return Err(ImportExecutionError::InvalidMaterial(
                "sealed block match refers to an absent external path".into(),
            ));
        }
        if block_matches
            .insert(
                (block_match.path().clone(), block_match.locator().clone()),
                block_match.block_id(),
            )
            .is_some()
        {
            return Err(ImportExecutionError::InvalidMaterial(
                "sealed import matches contain duplicate external block locators".into(),
            ));
        }
    }
    let rejected_raw_ids = matches
        .rejected_raw_ids()
        .iter()
        .map(|rejected| (rejected.path().clone(), rejected.locator().clone()))
        .collect::<BTreeSet<_>>();

    let mut desired_blocks = BTreeMap::<BlockId, DesiredImportBlock>::new();
    let mut desired_node_ids = BTreeMap::<(ManagedPath, usize), BlockId>::new();
    let mut observation_entries = Vec::with_capacity(inventory.entries().len());
    for (path, observation) in inventory.entries() {
        if deferred_paths.contains(path) {
            continue;
        }
        let kind = scope
            .path_identities
            .get(path)
            .ok_or(ImportExecutionError::IncompletePlan(
                "sealed inventory path has no Graph-decoded managed kind",
            ))?
            .kind;
        let state = match observation {
            RawObservation::Absent => ExternalImportObservationState::Absent,
            RawObservation::Present(bytes) => {
                let Some(page) = desired_pages.get(path) else {
                    // A source this transaction deliberately does not import:
                    // another physical file already owns its canonical page
                    // name, exactly as activation decided
                    // (`retain_authoritative_desired_pages`). Its exact bytes
                    // are still observed, so the transaction still proves what
                    // was on disk; no block identity is assigned to them and no
                    // operation touches the file.
                    observation_entries.push(
                        ExternalImportObservationEntry::new(
                            path.clone(),
                            kind,
                            ExternalImportObservationState::present(
                                bytes.bytes().to_vec(),
                                Vec::new(),
                            )
                            .map_err(|error| {
                                ImportExecutionError::Observation(
                                    ExternalImportObservationMaterialError::Observation(error),
                                )
                            })?,
                        )
                        .map_err(|error| {
                            ImportExecutionError::Observation(
                                ExternalImportObservationMaterialError::Observation(error),
                            )
                        })?,
                    );
                    continue;
                };
                let tree = trees.get(path).ok_or_else(|| {
                    ImportExecutionError::InvalidMaterial(
                        "sealed present inventory path has no parsed tree".into(),
                    )
                })?;
                let mut annotations = Vec::with_capacity(tree.nodes.len());
                for index in 0..tree.nodes.len() {
                    let locator = materialize_locator(tree, index, instrumentation)
                        .map_err(|block| ImportExecutionError::InvalidMaterial(block.detail))?;
                    let matched = block_matches.get(&(path.clone(), locator.clone())).copied();
                    let (block_id, existing, home_document_id) = match matched {
                        Some(block_id) => {
                            let current = current_blocks.get(&block_id).ok_or_else(|| {
                                ImportExecutionError::InvalidMaterial(
                                    "sealed block match has no accepted current block".into(),
                                )
                            })?;
                            (block_id, true, current.block.home_document_id)
                        }
                        None => (
                            import_id.unmatched_block_id(&ImportLocator::block(
                                path.clone(),
                                locator.clone(),
                            )),
                            false,
                            page.home_document_id,
                        ),
                    };
                    if desired_node_ids
                        .insert((path.clone(), index), block_id)
                        .is_some()
                    {
                        return Err(ImportExecutionError::InvalidMaterial(
                            "sealed parsed tree contains a duplicate block node".into(),
                        ));
                    }
                    let logseq_uuid =
                        external_logseq_uuid(path, &locator, &tree.nodes[index], &rejected_raw_ids);
                    annotations.push(AnnotatedIdentity::new(
                        locator.clone(),
                        tree.nodes[index].span,
                        block_id,
                        logseq_uuid,
                    ));
                    let parent = tree.nodes[index].parent.map(|parent| {
                        desired_node_ids
                            .get(&(path.clone(), parent))
                            .expect("parsed tree parents precede their children")
                            .to_owned()
                    });
                    let desired = DesiredImportBlock {
                        block_id,
                        page_id: page.page_id,
                        home_document_id,
                        parent,
                        order: imported_order(tree.nodes[index].sibling_position),
                        content: tree.nodes[index].raw.clone(),
                        logseq_uuid,
                        existing,
                    };
                    if desired_blocks.insert(block_id, desired).is_some() {
                        return Err(ImportExecutionError::InvalidMaterial(
                            "sealed matches assign one block identity more than once".into(),
                        ));
                    }
                }
                ExternalImportObservationState::present(bytes.bytes().to_vec(), annotations)
                    .map_err(|error| {
                        ImportExecutionError::Observation(
                            ExternalImportObservationMaterialError::Observation(error),
                        )
                    })?
            }
        };
        observation_entries.push(
            ExternalImportObservationEntry::new(path.clone(), kind, state).map_err(|error| {
                ImportExecutionError::Observation(
                    ExternalImportObservationMaterialError::Observation(error),
                )
            })?,
        );
    }
    let observation =
        ExternalImportObservationMaterial::new(scope.workspace_id, import_id, observation_entries)
            .map_err(|error| {
                ImportExecutionError::Observation(
                    ExternalImportObservationMaterialError::Observation(error),
                )
            })?;

    let mut operations = Vec::new();
    for page in desired_pages.values().filter(|page| !page.existing) {
        push_operation(
            &mut operations,
            SemanticOperation::CreatePage {
                page_id: page.page_id,
                home_document_id: page.home_document_id,
                name: page.name.clone(),
                path: page.path.clone(),
                kind: page.kind,
            },
        )?;
    }
    for page in desired_pages.values().filter(|page| page.existing) {
        let current = current_pages.get(&page.page_id).ok_or_else(|| {
            ImportExecutionError::InvalidMaterial(
                "desired existing page is absent from sealed accepted state".into(),
            )
        })?;
        let current = current.materialized_page();
        if current.name != page.name || current.path != page.path || current.kind != page.kind {
            push_operation(
                &mut operations,
                SemanticOperation::ReconcileExternalPageState {
                    page_id: page.page_id,
                    name: page.name.clone(),
                    path: page.path.clone(),
                    kind: page.kind,
                },
            )?;
        }
    }
    let mut new_blocks = desired_blocks
        .values()
        .filter(|block| !block.existing)
        .collect::<Vec<_>>();
    new_blocks.sort_unstable_by(|left, right| {
        desired_block_depth(left.block_id, &desired_blocks)
            .cmp(&desired_block_depth(right.block_id, &desired_blocks))
            .then_with(|| left.block_id.cmp(&right.block_id))
    });
    for desired in new_blocks {
        push_operation(
            &mut operations,
            SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id: desired.block_id,
                    home_document_id: desired.home_document_id,
                },
                page_id: desired.page_id,
                parent: desired.parent,
                order: desired.order.clone(),
                content: desired.content.clone(),
            },
        )?;
    }

    let mut moves = desired_blocks
        .iter()
        .filter_map(|(block_id, desired)| {
            let current = current_blocks.get(block_id)?;
            (current.page_id != desired.page_id).then_some((*block_id, desired, current))
        })
        .collect::<Vec<_>>();
    moves.sort_unstable_by(|(left_id, _, left), (right_id, _, right)| {
        current_block_depth(*left_id, &current_blocks)
            .cmp(&current_block_depth(*right_id, &current_blocks))
            .reverse()
            .then_with(|| left_id.cmp(right_id))
            .then_with(|| left.page_id.cmp(&right.page_id))
    });
    for (block_id, desired, current) in &moves {
        push_operation(
            &mut operations,
            SemanticOperation::MoveSubtree {
                root: BlockLocation {
                    block_id: *block_id,
                    home_document_id: current.block.home_document_id,
                },
                from_page_id: current.page_id,
                to_page_id: desired.page_id,
                parent: desired.parent,
                order: desired.order.clone(),
            },
        )?;
    }

    let moved_blocks = moves
        .iter()
        .map(|(block_id, _, _)| *block_id)
        .collect::<BTreeSet<_>>();
    for (block_id, desired) in &desired_blocks {
        let Some(current) = current_blocks.get(block_id) else {
            continue;
        };
        if !moved_blocks.contains(block_id)
            && (current.block.parent != desired.parent || current.block.order != desired.order)
        {
            push_operation(
                &mut operations,
                SemanticOperation::ReorderBlock {
                    block_id: *block_id,
                    page_id: desired.page_id,
                    parent: desired.parent,
                    order: desired.order.clone(),
                },
            )?;
        }
    }

    let mut deletions = current_blocks
        .iter()
        .filter_map(|(block_id, current)| {
            if deferred_page_ids.contains(&current.page_id) {
                return None;
            }
            (!desired_blocks.contains_key(block_id)
                && current
                    .block
                    .parent
                    .is_none_or(|parent| desired_blocks.contains_key(&parent)))
            .then_some((*block_id, current.page_id))
        })
        .collect::<Vec<_>>();
    deletions.sort_unstable();
    for (block_id, page_id) in deletions {
        push_operation(
            &mut operations,
            SemanticOperation::DeleteSubtree {
                root_block_id: block_id,
                page_id,
            },
        )?;
    }

    for (block_id, desired) in &desired_blocks {
        let Some(current) = current_blocks.get(block_id) else {
            continue;
        };
        if current.block.content != desired.content {
            push_operation(
                &mut operations,
                SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: *block_id,
                        home_document_id: current.block.home_document_id,
                    },
                    content: desired.content.clone(),
                },
            )?;
        }
    }
    for (block_id, desired) in &desired_blocks {
        let current_uuid = current_blocks
            .get(block_id)
            .map(|current| current.block.logseq_uuid);
        let mutation = match (current_uuid.flatten(), desired.logseq_uuid) {
            (None, Some(logseq_uuid)) => {
                Some(LogseqIdentityMutation::AssignExternal { logseq_uuid })
            }
            (Some(current), Some(logseq_uuid)) if current != logseq_uuid => {
                Some(LogseqIdentityMutation::ReplaceExternal { logseq_uuid })
            }
            (Some(_), None) => Some(LogseqIdentityMutation::RemoveExternal),
            (None, None) | (Some(_), Some(_)) => None,
        };
        if let Some(mutation) = mutation {
            push_operation(
                &mut operations,
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: BlockLocation {
                        block_id: *block_id,
                        home_document_id: desired.home_document_id,
                    },
                    mutation,
                },
            )?;
        }
    }

    for (path, page) in desired_pages {
        let Some(tree) = trees.get(path) else {
            continue;
        };
        let current_preamble = current_pages
            .get(&page.page_id)
            .map(|current| current.materialized_page().preamble.clone());
        if current_preamble != Some(tree.preamble.clone())
            && (page.existing || tree.preamble.is_some())
        {
            push_operation(
                &mut operations,
                SemanticOperation::SetPagePreamble {
                    page_id: page.page_id,
                    preamble: tree.preamble.clone(),
                },
            )?;
        }
    }

    let desired_page_ids = desired_pages
        .values()
        .map(|page| page.page_id)
        .collect::<BTreeSet<_>>();
    for page_id in current_pages.keys() {
        if !desired_page_ids.contains(page_id) && !deferred_page_ids.contains(page_id) {
            push_operation(
                &mut operations,
                SemanticOperation::DeletePage { page_id: *page_id },
            )?;
        }
    }

    if operations.is_empty() {
        let mut pages = Vec::new();
        for entry in observation.entries() {
            let Some(bytes) = entry.state().bytes() else {
                return Err(ImportExecutionError::InvalidMaterial(
                    "operation-free reconciliation contains an absent source".into(),
                ));
            };
            let desired = desired_pages.get(entry.path()).ok_or_else(|| {
                ImportExecutionError::InvalidMaterial(
                    "operation-free reconciliation has no desired page".into(),
                )
            })?;
            let current = current_pages.get(&desired.page_id).ok_or_else(|| {
                ImportExecutionError::InvalidMaterial(
                    "operation-free reconciliation has no accepted page".into(),
                )
            })?;
            if current.description() == super::BlobDescription::of(bytes) {
                continue;
            }
            pages.push(ImportFormattingPage {
                page_id: desired.page_id,
                path: entry.path().clone(),
                bytes: bytes.to_vec(),
                annotations: entry.state().annotations().to_vec(),
            });
        }
        if pages.is_empty() {
            return Err(ImportExecutionError::InvalidMaterial(
                "changed operation-free reconciliation has no formatting baseline to adopt".into(),
            ));
        }
        return Ok(BuiltImportMaterial::Formatting(ImportFormattingMaterial {
            pages,
        }));
    }
    let transaction = OperationTransaction::new(operations)
        .map_err(|error| ImportExecutionError::InvalidMaterial(error.to_string()))?;
    Ok(BuiltImportMaterial::Semantic(ImportExecutionMaterial {
        import_id,
        transaction,
        observation,
    }))
}

pub(crate) fn imported_order(sibling_position: u32) -> String {
    format!("{sibling_position:010}")
}

fn external_logseq_uuid(
    path: &ManagedPath,
    locator: &StructuralLocator,
    node: &ParsedNode,
    rejected_raw_ids: &BTreeSet<(ManagedPath, StructuralLocator)>,
) -> Option<LogseqUuid> {
    if rejected_raw_ids.contains(&(path.clone(), locator.clone())) || node.raw_ids.len() != 1 {
        return None;
    }
    LogseqUuid::parse(node.raw_ids[0].trim()).ok()
}

fn current_block_depth(
    block_id: BlockId,
    current_blocks: &BTreeMap<BlockId, CurrentImportBlock>,
) -> usize {
    let mut depth = 0_usize;
    let mut cursor = Some(block_id);
    let mut visited = BTreeSet::new();
    while let Some(block_id) = cursor {
        if !visited.insert(block_id) {
            return usize::MAX;
        }
        let Some(block) = current_blocks.get(&block_id) else {
            return usize::MAX;
        };
        depth = depth.saturating_add(1);
        cursor = block.block.parent;
    }
    depth
}

fn desired_block_depth(
    block_id: BlockId,
    desired_blocks: &BTreeMap<BlockId, DesiredImportBlock>,
) -> usize {
    let mut depth = 0_usize;
    let mut cursor = Some(block_id);
    let mut visited = BTreeSet::new();
    while let Some(block_id) = cursor {
        if !visited.insert(block_id) {
            return usize::MAX;
        }
        let Some(block) = desired_blocks.get(&block_id) else {
            return usize::MAX;
        };
        depth = depth.saturating_add(1);
        cursor = block.parent;
    }
    depth
}

fn blocked_inventory_error(
    error: InventoryError,
    instrumentation: ImportInstrumentation,
) -> ImportPlan {
    let (reason, paths) = match &error {
        InventoryError::UnsupportedManagedLayout { .. } => {
            (ImportBlockReason::UnsupportedManagedLayout, Vec::new())
        }
        InventoryError::UnsafePath(path) | InventoryError::DuplicateRequestedPath(path) => {
            (ImportBlockReason::UnsafeInput, vec![path.clone()])
        }
        InventoryError::PortablePathCollision { first, second } => (
            ImportBlockReason::PortablePathCollision,
            vec![first.clone(), second.clone()],
        ),
        InventoryError::ResourceBudgetExceeded { .. } => {
            (ImportBlockReason::ResourceLimit, Vec::new())
        }
        InventoryError::UnsafeEntry { path, .. } => (
            ImportBlockReason::UnsafeInput,
            path.iter().cloned().collect(),
        ),
    };
    ImportPlan {
        status: ImportPlanStatus::Blocked,
        import_id: None,
        inventory: None,
        matches: None,
        scope: None,
        execution: None,
        formatting: None,
        blocks: vec![ImportBlock {
            reason,
            paths,
            logical_completion_ids: Vec::new(),
            observation: None,
            detail: error.to_string(),
        }],
        instrumentation,
    }
}

fn blocked_authority_error(
    inventory: Option<RawInventory>,
    block: ImportBlock,
    instrumentation: ImportInstrumentation,
) -> ImportPlan {
    ImportPlan {
        status: ImportPlanStatus::Blocked,
        import_id: None,
        inventory,
        matches: None,
        scope: None,
        execution: None,
        formatting: None,
        blocks: vec![block],
        instrumentation,
    }
}

fn receipt_block(
    reason: ImportBlockReason,
    path: &ManagedPath,
    completion_id: Option<LogicalCompletionId>,
    inventory: &RawInventory,
    detail: impl Into<String>,
) -> ImportBlock {
    ImportBlock {
        reason,
        paths: vec![path.as_str().to_owned()],
        logical_completion_ids: completion_id.into_iter().collect(),
        observation: inventory_observation(inventory, path.as_str()),
        detail: detail.into(),
    }
}

fn inventory_observation(
    inventory: &RawInventory,
    path: &str,
) -> Option<(ManagedPath, ImportInventoryState)> {
    inventory
        .entries()
        .iter()
        .find(|(candidate, _)| candidate.as_str() == path)
        .map(|(path, observation)| {
            let state = match observation {
                RawObservation::Present(bytes) => {
                    ImportInventoryState::Present(bytes.description())
                }
                RawObservation::Absent => ImportInventoryState::Absent,
            };
            (path.clone(), state)
        })
}

fn match_pages(
    inventory: &RawInventory,
    completed: &[&ReceiptBackedPage],
    instrumentation: &mut ImportInstrumentation,
) -> Result<Vec<PageImportMatch>, ImportBlock> {
    let completed_paths = completed
        .iter()
        .map(|page| page.path().clone())
        .collect::<BTreeSet<_>>();
    let mut new_by_description = BTreeMap::<BlobDescription, Vec<&ManagedPath>>::new();
    for (path, observation) in inventory.entries() {
        instrumentation.inventory_path_lookups =
            instrumentation.inventory_path_lookups.saturating_add(1);
        if completed_paths.contains(path) {
            continue;
        }
        if let RawObservation::Present(bytes) = observation {
            new_by_description
                .entry(bytes.description())
                .or_default()
                .push(path);
        }
    }

    let mut source_to_candidate = BTreeMap::<ManagedPath, ManagedPath>::new();
    let mut candidate_to_sources = BTreeMap::<ManagedPath, Vec<&ReceiptBackedPage>>::new();
    for page in completed {
        instrumentation.inventory_path_lookups =
            instrumentation.inventory_path_lookups.saturating_add(1);
        if !matches!(
            inventory.entries().get(page.path()),
            Some(RawObservation::Absent)
        ) {
            continue;
        }
        let candidates = new_by_description
            .get(&page.description())
            .into_iter()
            .flatten()
            .filter(|path| {
                instrumentation.inventory_path_lookups =
                    instrumentation.inventory_path_lookups.saturating_add(1);
                inventory.entries().get(*path).is_some_and(|observation| {
                    matches!(observation, RawObservation::Present(bytes) if bytes.bytes() == page.bytes())
                })
            })
            .copied()
            .collect::<Vec<_>>();
        if candidates.len() > 1 {
            return Err(ImportBlock {
                reason: ImportBlockReason::AmbiguousDestructiveMatch,
                paths: std::iter::once(page.path().as_str().to_owned())
                    .chain(
                        candidates
                            .iter()
                            .map(|candidate| candidate.as_str().to_owned()),
                    )
                    .collect(),
                logical_completion_ids: vec![page.logical_completion_id()],
                observation: inventory_observation(inventory, page.path().as_str()),
                detail: "one absent receipt path has multiple exact new-path candidates".into(),
            });
        }
        if let Some(candidate) = candidates.first() {
            source_to_candidate.insert(page.path().clone(), (*candidate).clone());
            candidate_to_sources
                .entry((*candidate).clone())
                .or_default()
                .push(page);
        }
    }
    if let Some((candidate, sources)) = candidate_to_sources
        .iter()
        .find(|(_, sources)| sources.len() > 1)
    {
        return Err(ImportBlock {
            reason: ImportBlockReason::AmbiguousDestructiveMatch,
            paths: sources
                .iter()
                .map(|page| page.path().as_str().to_owned())
                .chain(std::iter::once(candidate.as_str().to_owned()))
                .collect(),
            logical_completion_ids: sources
                .iter()
                .map(|page| page.logical_completion_id())
                .collect(),
            observation: inventory_observation(inventory, candidate.as_str()),
            detail: "multiple absent receipt paths claim one exact new path".into(),
        });
    }

    let mut matches = Vec::new();
    for page in completed {
        instrumentation.inventory_path_lookups =
            instrumentation.inventory_path_lookups.saturating_add(1);
        match inventory.entries().get(page.path()) {
            Some(RawObservation::Present(_)) => matches.push(PageImportMatch {
                path: page.path().clone(),
                previous_path: page.path().clone(),
                page_id: page.page_id(),
                basis: PageMatchBasis::SamePathCompletion,
            }),
            Some(RawObservation::Absent) => {
                if let Some(path) = source_to_candidate.get(page.path()) {
                    matches.push(PageImportMatch {
                        path: path.clone(),
                        previous_path: page.path().clone(),
                        page_id: page.page_id(),
                        basis: PageMatchBasis::ReceiptBackedExactRename,
                    });
                }
            }
            None => unreachable!("receipt paths are required in the affected inventory"),
        }
    }
    matches.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(matches)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedNode {
    parent: Option<usize>,
    sibling_position: u32,
    depth: usize,
    children: Vec<usize>,
    span: StructuralSpan,
    raw: String,
    raw_ids: Vec<String>,
    projection_facets: ParsedBlockProjectionFacets,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedBlockProjectionFacets {
    searchable_text: String,
    heading_level: Option<u8>,
    collapsed: bool,
    properties: Vec<super::MaterializedProperty>,
    tags: Vec<String>,
    task: Option<super::MaterializedTask>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedTree {
    path: ManagedPath,
    preamble: Option<String>,
    roots: Vec<usize>,
    nodes: Vec<ParsedNode>,
}

struct ParsedExternalTree {
    tree: ParsedTree,
    explicit_title: Option<String>,
    filename_fallback: PageEntry,
    effective: PageEntry,
}

const CAPTURED_ACTIVATION_PAGE_SCHEMA_VERSION: u32 = 1;
const MAX_CAPTURED_ACTIVATION_PAGE_BYTES: usize = 256 * 1024 * 1024;

/// Parser-owned, process-local handoff from the exact source capture to the
/// activation constructor. This is deliberately not the durable genesis
/// capsule codec: it may change with the parser and disappears with an
/// uncommitted activation episode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedActivationPageV1 {
    schema_version: u32,
    tree: ParsedTree,
}

pub(crate) struct CapturedActivationPageRecord {
    encoded: Vec<u8>,
    logical_name: String,
    kind: ManagedTextKind,
    node_count: usize,
}

impl CapturedActivationPageRecord {
    pub(crate) fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    pub(crate) fn logical_name(&self) -> &str {
        &self.logical_name
    }

    pub(crate) fn kind(&self) -> ManagedTextKind {
        self.kind
    }

    pub(crate) fn node_count(&self) -> usize {
        self.node_count
    }
}

pub(crate) fn capture_activation_page_record(
    graph: &Graph,
    path: &ManagedPath,
    bytes: &[u8],
) -> io::Result<CapturedActivationPageRecord> {
    let mut instrumentation = ImportInstrumentation::default();
    let parsed =
        parse_external_nodes(graph, path, bytes, false, &mut instrumentation).map_err(|block| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {}", path, block.detail),
            )
        })?;
    let logical_name = parsed.effective.name.clone();
    let kind = match parsed.effective.kind {
        PageKind::Page => ManagedTextKind::Page,
        PageKind::Journal => ManagedTextKind::Journal,
    };
    let node_count = parsed.tree.nodes.len();
    let record = CapturedActivationPageV1 {
        schema_version: CAPTURED_ACTIVATION_PAGE_SCHEMA_VERSION,
        tree: parsed.tree,
    };
    let encoded = postcard::to_allocvec(&record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if encoded.len() > MAX_CAPTURED_ACTIVATION_PAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "captured activation page exceeds its fixed record cap",
        ));
    }
    Ok(CapturedActivationPageRecord {
        encoded,
        logical_name,
        kind,
        node_count,
    })
}

fn decode_captured_activation_page_record(
    expected_path: &ManagedPath,
    bytes: &[u8],
) -> Result<ParsedTree, BootstrapStreamingImportError> {
    if bytes.len() > MAX_CAPTURED_ACTIVATION_PAGE_BYTES {
        return Err(BootstrapStreamingImportError::ResourceLimit {
            resource: "captured activation page bytes",
            observed: bytes.len() as u64,
            limit: MAX_CAPTURED_ACTIVATION_PAGE_BYTES as u64,
        });
    }
    let record: CapturedActivationPageV1 = postcard::from_bytes(bytes)
        .map_err(|error| BootstrapStreamingImportError::InvalidSource(error.to_string()))?;
    if record.schema_version != CAPTURED_ACTIVATION_PAGE_SCHEMA_VERSION
        || &record.tree.path != expected_path
    {
        return Err(BootstrapStreamingImportError::InvalidSource(format!(
            "captured activation page does not bind {}",
            expected_path
        )));
    }
    if record.tree.nodes.len() as u32 > MAX_PARSED_NODES_PER_SOURCE_FILE {
        return Err(BootstrapStreamingImportError::ResourceLimit {
            resource: "parser nodes per source file",
            observed: record.tree.nodes.len() as u64,
            limit: u64::from(MAX_PARSED_NODES_PER_SOURCE_FILE),
        });
    }
    Ok(record.tree)
}

impl std::ops::Deref for ParsedExternalTree {
    type Target = ParsedTree;

    fn deref(&self) -> &Self::Target {
        &self.tree
    }
}

struct ParsedImportDocuments {
    current: BTreeMap<ManagedPath, ParsedExternalTree>,
    base: BTreeMap<ManagedPath, ParsedExternalTree>,
}

/// Preserve a moved page's receipt-backed identity after block matching only
/// when a unique Logseq UUID joins an absent source to an unmatched present
/// destination. Content similarity is deliberately not page-move evidence:
/// it cannot distinguish a move from an unrelated delete/create.
fn match_anchored_page_moves(
    inventory: &RawInventory,
    completed: &[&ReceiptBackedPage],
    matches: &mut ImportMatches,
    instrumentation: &mut ImportInstrumentation,
) -> Result<(), ImportBlock> {
    instrumentation.anchored_page_match_set_inserts = instrumentation
        .anchored_page_match_set_inserts
        .saturating_add(matches.pages.len().saturating_mul(2));
    let matched_sources = matches
        .pages
        .iter()
        .map(|matched| matched.previous_path.clone())
        .collect::<BTreeSet<_>>();
    let matched_destinations = matches
        .pages
        .iter()
        .map(|matched| matched.path.clone())
        .collect::<BTreeSet<_>>();

    let mut block_owners = BTreeMap::<BlockId, Vec<&ReceiptBackedPage>>::new();
    let mut uuid_owners = BTreeMap::<LogseqUuid, Vec<&ReceiptBackedPage>>::new();
    for page in completed {
        for annotation in page.annotations() {
            instrumentation.anchored_page_owner_inserts = instrumentation
                .anchored_page_owner_inserts
                .saturating_add(1);
            block_owners
                .entry(annotation.block_id())
                .or_default()
                .push(page);
            if let Some(uuid) = annotation.logseq_uuid() {
                instrumentation.anchored_page_uuid_owner_inserts = instrumentation
                    .anchored_page_uuid_owner_inserts
                    .saturating_add(1);
                uuid_owners.entry(uuid).or_default().push(page);
            }
        }
    }

    for rejected in matches
        .rejected_raw_ids
        .iter()
        .filter(|rejected| rejected.reason == RejectedRawIdReason::Duplicate)
    {
        let Ok(uuid) = LogseqUuid::parse(rejected.raw_value.trim()) else {
            continue;
        };
        instrumentation.anchored_page_uuid_owner_lookups = instrumentation
            .anchored_page_uuid_owner_lookups
            .saturating_add(1);
        let Some(owners) = uuid_owners.get(&uuid) else {
            continue;
        };
        let destructive_owners = owners
            .iter()
            .copied()
            .filter(|page| {
                instrumentation.anchored_page_match_set_lookups = instrumentation
                    .anchored_page_match_set_lookups
                    .saturating_add(2);
                instrumentation.inventory_path_lookups =
                    instrumentation.inventory_path_lookups.saturating_add(2);
                page.path() != &rejected.path
                    && !matched_sources.contains(page.path())
                    && !matched_destinations.contains(&rejected.path)
                    && matches!(
                        inventory.entries().get(page.path()),
                        Some(RawObservation::Absent)
                    )
                    && matches!(
                        inventory.entries().get(&rejected.path),
                        Some(RawObservation::Present(_))
                    )
            })
            .collect::<Vec<_>>();
        if !destructive_owners.is_empty() {
            return Err(ImportBlock {
                reason: ImportBlockReason::AmbiguousDestructiveMatch,
                paths: destructive_owners
                    .iter()
                    .map(|page| page.path().as_str().to_owned())
                    .chain(std::iter::once(rejected.path.as_str().to_owned()))
                    .collect(),
                logical_completion_ids: destructive_owners
                    .iter()
                    .map(|page| page.logical_completion_id())
                    .collect(),
                observation: inventory_observation(inventory, rejected.path.as_str()),
                detail: format!(
                    "duplicate UUID {uuid} is ambiguous destructive page-move evidence"
                ),
            });
        }
    }

    let mut source_destinations =
        BTreeMap::<ManagedPath, (PageId, LogicalCompletionId, BTreeSet<ManagedPath>)>::new();
    let mut destination_sources =
        BTreeMap::<ManagedPath, BTreeSet<(ManagedPath, PageId, LogicalCompletionId)>>::new();
    for block_match in matches
        .blocks
        .iter()
        .filter(|matched| matched.basis == BlockMatchBasis::UniqueLogseqUuid)
    {
        instrumentation.anchored_page_owner_lookups = instrumentation
            .anchored_page_owner_lookups
            .saturating_add(1);
        let Some(owners) = block_owners.get(&block_match.block_id) else {
            continue;
        };
        if owners.len() != 1 {
            return Err(ImportBlock {
                reason: ImportBlockReason::DuplicateAnchorDependent,
                paths: owners
                    .iter()
                    .map(|page| page.path().as_str().to_owned())
                    .collect(),
                logical_completion_ids: owners
                    .iter()
                    .map(|page| page.logical_completion_id())
                    .collect(),
                observation: inventory_observation(inventory, block_match.path.as_str()),
                detail: format!(
                    "block {} has multiple receipt-backed page owners",
                    block_match.block_id
                ),
            });
        }
        let page = owners[0];
        let source = page.path();
        let destination = &block_match.path;
        instrumentation.anchored_page_match_set_lookups = instrumentation
            .anchored_page_match_set_lookups
            .saturating_add(2);
        instrumentation.inventory_path_lookups =
            instrumentation.inventory_path_lookups.saturating_add(2);
        if source == destination
            || matched_sources.contains(source)
            || matched_destinations.contains(destination)
            || !matches!(
                inventory.entries().get(source),
                Some(RawObservation::Absent)
            )
            || !matches!(
                inventory.entries().get(destination),
                Some(RawObservation::Present(_))
            )
        {
            continue;
        }
        instrumentation.anchored_page_edge_inserts =
            instrumentation.anchored_page_edge_inserts.saturating_add(2);
        source_destinations
            .entry(source.clone())
            .or_insert_with(|| {
                (
                    page.page_id(),
                    page.logical_completion_id(),
                    BTreeSet::new(),
                )
            })
            .2
            .insert(destination.clone());
        destination_sources
            .entry(destination.clone())
            .or_default()
            .insert((source.clone(), page.page_id(), page.logical_completion_id()));
    }

    if let Some((source, (_, completion_id, destinations))) = source_destinations
        .iter()
        .find(|(_, (_, _, destinations))| destinations.len() > 1)
    {
        return Err(ImportBlock {
            reason: ImportBlockReason::AmbiguousDestructiveMatch,
            paths: std::iter::once(source.as_str().to_owned())
                .chain(
                    destinations
                        .iter()
                        .map(|destination| destination.as_str().to_owned()),
                )
                .collect(),
            logical_completion_ids: vec![*completion_id],
            observation: inventory_observation(inventory, source.as_str()),
            detail: "one absent receipt page anchors to multiple present destinations".into(),
        });
    }
    if let Some((destination, sources)) = destination_sources
        .iter()
        .find(|(_, sources)| sources.len() > 1)
    {
        return Err(ImportBlock {
            reason: ImportBlockReason::AmbiguousDestructiveMatch,
            paths: sources
                .iter()
                .map(|(source, _, _)| source.as_str().to_owned())
                .chain(std::iter::once(destination.as_str().to_owned()))
                .collect(),
            logical_completion_ids: sources
                .iter()
                .map(|(_, _, completion_id)| *completion_id)
                .collect(),
            observation: inventory_observation(inventory, destination.as_str()),
            detail: "multiple absent receipt pages anchor to one present destination".into(),
        });
    }

    for (source, (page_id, _, destinations)) in source_destinations {
        let destination = destinations
            .into_iter()
            .next()
            .expect("empty anchor sets are never inserted");
        matches.pages.push(PageImportMatch {
            path: destination,
            previous_path: source,
            page_id,
            basis: PageMatchBasis::ReceiptBackedAnchoredRename,
        });
    }
    matches
        .pages
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(())
}

fn parse_external_nodes(
    graph: &Graph,
    path: &ManagedPath,
    bytes: &[u8],
    require_round_trip: bool,
    instrumentation: &mut ImportInstrumentation,
) -> Result<ParsedExternalTree, ImportBlock> {
    let parsed = graph
        .parse_external_document(path, bytes, require_round_trip)
        .map_err(|error| {
            authority_block(
                ImportBlockReason::UnsafeInput,
                Some(path),
                format!("external document parser rejected source: {error}"),
            )
        })?;
    enforce_outline_limits(path, &parsed.parsed, instrumentation.parsed_nodes)?;
    let tree = flatten_document(path, parsed.parsed, instrumentation)?;
    Ok(ParsedExternalTree {
        tree,
        explicit_title: parsed.explicit_title,
        filename_fallback: parsed.filename_fallback,
        effective: parsed.effective,
    })
}

fn parse_nodes(
    path: &ManagedPath,
    bytes: &[u8],
    instrumentation: &mut ImportInstrumentation,
) -> Result<ParsedTree, ImportBlock> {
    let text = std::str::from_utf8(bytes).expect("UTF-8 checked before semantic parsing");
    let is_org = path.is_org();
    let parsed = if is_org {
        crate::org::try_parse_org_with_source_spans(text)
    } else {
        crate::doc::try_parse_with_source_spans(text)
    }
    .map_err(|error| {
        authority_block(
            ImportBlockReason::UnsafeInput,
            Some(path),
            format!("lsdoc outline cannot be represented safely: {error}"),
        )
    })?;
    enforce_outline_limits(path, &parsed, instrumentation.parsed_nodes)?;
    // Parseable sources are admissible even when Tine's serializer cannot
    // reproduce their structure. The exact-source projection preserves their
    // bytes, and the application DTO marks either Markdown or Org read-only at
    // the parser boundary so no editor/save path can reserialize them.
    flatten_document(path, parsed, instrumentation)
}

fn enforce_outline_limits(
    path: &ManagedPath,
    parsed: &crate::doc::ParsedDocument,
    parsed_nodes: usize,
) -> Result<(), ImportBlock> {
    if parsed.outline_depth > MAX_IMPORT_DEPTH {
        return Err(authority_block(
            ImportBlockReason::ResourceLimit,
            Some(path),
            format!(
                "parser-owned document nesting depth {} exceeds import limit {MAX_IMPORT_DEPTH}",
                parsed.outline_depth
            ),
        ));
    }
    let observed = parsed_nodes.saturating_add(parsed.outline_nodes);
    if observed > MAX_IMPORT_PARSED_NODES {
        return Err(authority_block(
            ImportBlockReason::ResourceLimit,
            Some(path),
            format!(
                "parser-owned outline exceeds parsed-node budget: observed {observed}, limit {MAX_IMPORT_PARSED_NODES}"
            ),
        ));
    }
    Ok(())
}

fn flatten_document(
    path: &ManagedPath,
    parsed: crate::doc::ParsedDocument,
    instrumentation: &mut ImportInstrumentation,
) -> Result<ParsedTree, ImportBlock> {
    let spans = parsed
        .block_spans
        .into_iter()
        .map(|span| {
            StructuralSpan::new(span.start as u64, span.end as u64).map_err(|error| {
                authority_block(
                    ImportBlockReason::UnsafeInput,
                    Some(path),
                    error.to_string(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let document = parsed.document;
    let mut nodes = Vec::<ParsedNode>::new();
    let mut roots = Vec::new();
    let mut pending = document
        .roots
        .iter()
        .enumerate()
        .rev()
        .map(|(position, block)| (block, None, position as u32, 1_usize))
        .collect::<Vec<_>>();
    while let Some((block, parent, sibling_position, depth)) = pending.pop() {
        if depth > MAX_IMPORT_DEPTH {
            return Err(authority_block(
                ImportBlockReason::ResourceLimit,
                Some(path),
                format!("parsed document depth exceeds import limit {MAX_IMPORT_DEPTH}"),
            ));
        }
        if instrumentation.parsed_nodes == MAX_IMPORT_PARSED_NODES {
            return Err(authority_block(
                ImportBlockReason::ResourceLimit,
                Some(path),
                format!("parsed-node budget exceeded: limit {MAX_IMPORT_PARSED_NODES}"),
            ));
        }
        let raw_ids = block
            .properties()
            .into_iter()
            .filter_map(|(key, value)| {
                (crate::doc::property_key_norm(&key) == "id").then_some(value)
            })
            .collect();
        let (searchable_text, heading_level, collapsed, properties, tags, task) =
            super::sqlite::document_facets_from_parsed_block(block);
        let index = nodes.len();
        let span = spans.get(index).copied().ok_or_else(|| {
            authority_block(
                ImportBlockReason::UnsafeInput,
                Some(path),
                "parser tree has more blocks than exact source-span capture",
            )
        })?;
        nodes.push(ParsedNode {
            parent,
            sibling_position,
            depth,
            children: Vec::with_capacity(block.children.len()),
            span,
            raw: block.raw.clone(),
            raw_ids,
            projection_facets: ParsedBlockProjectionFacets {
                searchable_text,
                heading_level,
                collapsed,
                properties,
                tags,
                task,
            },
        });
        instrumentation.parsed_nodes = instrumentation.parsed_nodes.saturating_add(1);
        instrumentation.max_depth = instrumentation.max_depth.max(depth);
        if let Some(parent) = parent {
            nodes[parent].children.push(index);
        } else {
            roots.push(index);
        }
        for (position, child) in block.children.iter().enumerate().rev() {
            pending.push((child, Some(index), position as u32, depth.saturating_add(1)));
        }
    }
    if spans.len() != nodes.len() {
        return Err(authority_block(
            ImportBlockReason::UnsafeInput,
            Some(path),
            "exact source-span capture disagrees with the parsed block tree",
        ));
    }
    Ok(ParsedTree {
        path: path.clone(),
        preamble: document.pre_block.clone(),
        roots,
        nodes,
    })
}

fn materialize_locator(
    tree: &ParsedTree,
    index: usize,
    instrumentation: &mut ImportInstrumentation,
) -> Result<StructuralLocator, ImportBlock> {
    let depth = tree.nodes[index].depth;
    let next = instrumentation
        .locator_components_materialized
        .saturating_add(depth);
    if next > MAX_IMPORT_LOCATOR_COMPONENTS {
        return Err(authority_block(
            ImportBlockReason::ResourceLimit,
            Some(&tree.path),
            format!(
                "structural-locator component budget exceeded: observed {next}, limit {MAX_IMPORT_LOCATOR_COMPONENTS}"
            ),
        ));
    }
    instrumentation.locator_components_materialized = next;
    let mut components = Vec::with_capacity(depth);
    let mut cursor = Some(index);
    while let Some(node) = cursor {
        components.push(tree.nodes[node].sibling_position);
        cursor = tree.nodes[node].parent;
    }
    components.reverse();
    StructuralLocator::new(components).map_err(|error| {
        authority_block(
            ImportBlockReason::CorruptBase,
            Some(&tree.path),
            error.to_string(),
        )
    })
}

fn resolve_locator(
    tree: &ParsedTree,
    locator: &StructuralLocator,
    instrumentation: &mut ImportInstrumentation,
) -> Result<Option<usize>, ImportBlock> {
    let next = instrumentation
        .locator_components_materialized
        .saturating_add(locator.components().len());
    if next > MAX_IMPORT_LOCATOR_COMPONENTS {
        return Err(authority_block(
            ImportBlockReason::ResourceLimit,
            Some(&tree.path),
            format!(
                "structural-locator component budget exceeded: observed {next}, limit {MAX_IMPORT_LOCATOR_COMPONENTS}"
            ),
        ));
    }
    instrumentation.locator_components_materialized = next;
    let mut components = locator.components().iter().copied();
    let Some(root) = components.next() else {
        return Ok(None);
    };
    let Some(mut current) = tree.roots.get(root as usize).copied() else {
        return Ok(None);
    };
    for component in components {
        let Some(child) = tree.nodes[current]
            .children
            .get(component as usize)
            .copied()
        else {
            return Ok(None);
        };
        current = child;
    }
    Ok(Some(current))
}

struct StructuralClassEntry {
    raw: String,
    child_classes: Vec<usize>,
    class: usize,
}

#[derive(Default)]
struct StructuralInterner {
    buckets: HashMap<ContentDigest, Vec<StructuralClassEntry>>,
    next_class: usize,
}

impl StructuralInterner {
    fn new() -> Self {
        Self::default()
    }
}

/// Assign exact structural classes through a digest index whose candidates are
/// always collision-checked against raw bytes and child classes. Hash-table
/// lookup avoids ordered vector-key comparisons with adversarial common
/// prefixes, and every candidate comparison is charged.
fn structural_classes(
    tree: &ParsedTree,
    interner: &mut StructuralInterner,
    instrumentation: &mut ImportInstrumentation,
) -> Result<Vec<usize>, ImportBlock> {
    let mut classes = vec![0; tree.nodes.len()];
    for index in (0..tree.nodes.len()).rev() {
        let child_classes = tree.nodes[index]
            .children
            .iter()
            .map(|child| classes[*child])
            .collect::<Vec<_>>();
        instrumentation.structural_key_components = instrumentation
            .structural_key_components
            .saturating_add(1)
            .saturating_add(child_classes.len());
        if instrumentation.structural_key_components > MAX_IMPORT_STRUCTURAL_KEY_WORK {
            return Err(authority_block(
                ImportBlockReason::ResourceLimit,
                Some(&tree.path),
                format!(
                    "structural key component budget exceeded: limit {MAX_IMPORT_STRUCTURAL_KEY_WORK}"
                ),
            ));
        }
        let node = &tree.nodes[index];
        let mut hasher = Sha256::new();
        hasher.update(b"tine/import-structural-class/v1\0");
        hasher.update((node.raw.len() as u64).to_be_bytes());
        hasher.update(node.raw.as_bytes());
        hasher.update((child_classes.len() as u64).to_be_bytes());
        for class in &child_classes {
            hasher.update((*class as u64).to_be_bytes());
        }
        instrumentation.bytes_hashed = instrumentation
            .bytes_hashed
            .saturating_add(node.raw.len() as u64)
            .saturating_add((child_classes.len() as u64).saturating_mul(8));
        let digest = ContentDigest::from_bytes(hasher.finalize().into());
        let bucket = interner.buckets.entry(digest).or_default();
        let mut class = None;
        for candidate in bucket.iter() {
            instrumentation.structural_key_comparisons = instrumentation
                .structural_key_comparisons
                .saturating_add(node.raw.len())
                .saturating_add(child_classes.len());
            if instrumentation.structural_key_comparisons > MAX_IMPORT_STRUCTURAL_KEY_WORK {
                return Err(authority_block(
                    ImportBlockReason::ResourceLimit,
                    Some(&tree.path),
                    format!(
                        "structural key comparison budget exceeded: limit {MAX_IMPORT_STRUCTURAL_KEY_WORK}"
                    ),
                ));
            }
            if candidate.raw == node.raw && candidate.child_classes == child_classes {
                class = Some(candidate.class);
                break;
            }
        }
        let class = match class {
            Some(class) => class,
            None => {
                let class = interner.next_class;
                interner.next_class = interner.next_class.saturating_add(1);
                instrumentation.structural_class_allocations = instrumentation
                    .structural_class_allocations
                    .saturating_add(1);
                bucket.push(StructuralClassEntry {
                    raw: node.raw.clone(),
                    child_classes,
                    class,
                });
                class
            }
        };
        classes[index] = class;
    }
    Ok(classes)
}

fn match_blocks(
    graph: &Graph,
    inventory: &RawInventory,
    completed: &[&ReceiptBackedPage],
    matches: &mut ImportMatches,
    instrumentation: &mut ImportInstrumentation,
) -> Result<ParsedImportDocuments, ImportBlock> {
    let mut external_by_path = BTreeMap::<ManagedPath, ParsedExternalTree>::new();
    for (path, observation) in inventory.entries() {
        if let RawObservation::Present(bytes) = observation {
            instrumentation.present_document_parses =
                instrumentation.present_document_parses.saturating_add(1);
            external_by_path.insert(
                path.clone(),
                parse_external_nodes(graph, path, bytes.bytes(), true, instrumentation)?,
            );
        }
    }
    let mut base_by_path = BTreeMap::<ManagedPath, ParsedExternalTree>::new();
    for page in completed {
        instrumentation.authenticated_base_document_parses = instrumentation
            .authenticated_base_document_parses
            .saturating_add(1);
        base_by_path.insert(
            page.path().clone(),
            parse_external_nodes(graph, page.path(), page.bytes(), true, instrumentation).map_err(
                |mut block| {
                    block.reason = ImportBlockReason::CorruptBase;
                    block.logical_completion_ids = vec![page.logical_completion_id()];
                    block.detail = format!(
                        "authenticated completed-base document is not parseable within import limits: {}",
                        block.detail
                    );
                    block
                },
            )?,
        );
    }

    let mut external_anchors = BTreeMap::<LogseqUuid, Vec<(ManagedPath, usize, String)>>::new();
    let mut rejected = BTreeSet::<(ManagedPath, usize)>::new();
    for tree in external_by_path.values() {
        for (index, node) in tree.nodes.iter().enumerate() {
            if node.raw_ids.is_empty() {
                continue;
            }
            if node.raw_ids.len() != 1 {
                rejected.insert((tree.path.clone(), index));
                for raw_id in &node.raw_ids {
                    let reason = if LogseqUuid::parse(raw_id.trim()).is_ok() {
                        RejectedRawIdReason::Duplicate
                    } else {
                        RejectedRawIdReason::InvalidSyntax
                    };
                    matches.rejected_raw_ids.push(RejectedRawId {
                        path: tree.path.clone(),
                        locator: materialize_locator(tree, index, instrumentation)?,
                        raw_value: raw_id.clone(),
                        reason,
                    });
                }
                continue;
            }
            let raw_id = &node.raw_ids[0];
            match LogseqUuid::parse(raw_id.trim()) {
                Ok(uuid) => external_anchors.entry(uuid).or_default().push((
                    tree.path.clone(),
                    index,
                    raw_id.clone(),
                )),
                Err(_) => {
                    rejected.insert((tree.path.clone(), index));
                    matches.rejected_raw_ids.push(RejectedRawId {
                        path: tree.path.clone(),
                        locator: materialize_locator(tree, index, instrumentation)?,
                        raw_value: raw_id.clone(),
                        reason: RejectedRawIdReason::InvalidSyntax,
                    });
                }
            }
        }
    }
    for owners in external_anchors.values().filter(|owners| owners.len() > 1) {
        for (path, index, raw_value) in owners {
            rejected.insert((path.clone(), *index));
            let tree = &external_by_path[path];
            matches.rejected_raw_ids.push(RejectedRawId {
                path: path.clone(),
                locator: materialize_locator(tree, *index, instrumentation)?,
                raw_value: raw_value.clone(),
                reason: RejectedRawIdReason::Duplicate,
            });
        }
    }
    instrumentation.rejected_raw_id_occurrences = matches.rejected_raw_ids.len();
    matches.rejected_raw_ids.sort_unstable_by(|left, right| {
        (&left.path, &left.locator, &left.raw_value).cmp(&(
            &right.path,
            &right.locator,
            &right.raw_value,
        ))
    });

    let mut receipt_anchors =
        BTreeMap::<LogseqUuid, Vec<(BlockId, LogicalCompletionId, ManagedPath, usize)>>::new();
    let mut annotations_by_path = BTreeMap::<ManagedPath, BTreeMap<usize, BlockId>>::new();
    for page in completed {
        let tree = &base_by_path[page.path()];
        let mut annotations = BTreeMap::new();
        for annotation in page.annotations() {
            let Some(index) = resolve_locator(tree, annotation.locator(), instrumentation)? else {
                continue;
            };
            annotations.insert(index, annotation.block_id());
            if let Some(uuid) = annotation.logseq_uuid() {
                receipt_anchors.entry(uuid).or_default().push((
                    annotation.block_id(),
                    page.logical_completion_id(),
                    page.path().clone(),
                    index,
                ));
            }
        }
        annotations_by_path.insert(page.path().clone(), annotations);
    }
    let mut matched_external = BTreeSet::<(ManagedPath, usize)>::new();
    let mut matched_base = BTreeMap::<(ManagedPath, usize), (ManagedPath, usize)>::new();
    let mut used_blocks = BTreeSet::<BlockId>::new();
    for (uuid, owners) in external_anchors
        .iter()
        .filter(|(_, owners)| owners.len() == 1)
    {
        let Some(receipt_owners) = receipt_anchors.get(uuid) else {
            continue;
        };
        if receipt_owners.len() != 1 {
            let (path, _, _) = &owners[0];
            return Err(ImportBlock {
                reason: ImportBlockReason::DuplicateAnchorDependent,
                paths: vec![path.as_str().to_owned()],
                logical_completion_ids: receipt_owners
                    .iter()
                    .map(|(_, completion, _, _)| *completion)
                    .collect(),
                observation: inventory_observation(inventory, path.as_str()),
                detail: format!("UUID {uuid} has multiple receipt-backed owners"),
            });
        }
        let (path, external_index, _) = &owners[0];
        let (block_id, _, base_path, base_index) = &receipt_owners[0];
        let external_tree = &external_by_path[path];
        matches.blocks.push(BlockImportMatch {
            path: path.clone(),
            locator: materialize_locator(external_tree, *external_index, instrumentation)?,
            block_id: *block_id,
            basis: BlockMatchBasis::UniqueLogseqUuid,
        });
        used_blocks.insert(*block_id);
        matched_external.insert((path.clone(), *external_index));
        matched_base.insert(
            (base_path.clone(), *base_index),
            (path.clone(), *external_index),
        );
    }

    let mut structural_interner = StructuralInterner::new();
    let mut base_classes_by_path = BTreeMap::new();
    for (path, tree) in &base_by_path {
        let classes = structural_classes(tree, &mut structural_interner, instrumentation)?;
        instrumentation.structural_class_nodes = instrumentation
            .structural_class_nodes
            .saturating_add(tree.nodes.len());
        base_classes_by_path.insert(path.clone(), classes);
    }
    let mut external_classes_by_path = BTreeMap::new();
    for (path, tree) in &external_by_path {
        let classes = structural_classes(tree, &mut structural_interner, instrumentation)?;
        instrumentation.structural_class_nodes = instrumentation
            .structural_class_nodes
            .saturating_add(tree.nodes.len());
        external_classes_by_path.insert(path.clone(), classes);
    }

    let mut base_exact = BTreeMap::<usize, Vec<(ManagedPath, usize, BlockId)>>::new();
    for (path, tree) in &base_by_path {
        let annotations = &annotations_by_path[path];
        let classes = &base_classes_by_path[path];
        for index in 0..tree.nodes.len() {
            if annotations.contains_key(&index)
                && !matched_base.contains_key(&(path.clone(), index))
            {
                instrumentation.exact_bucket_inserts =
                    instrumentation.exact_bucket_inserts.saturating_add(1);
                base_exact.entry(classes[index]).or_default().push((
                    path.clone(),
                    index,
                    annotations[&index],
                ));
            }
        }
    }
    let mut external_exact = BTreeMap::<usize, Vec<(ManagedPath, usize)>>::new();
    for (path, tree) in &external_by_path {
        let classes = &external_classes_by_path[path];
        for index in 0..tree.nodes.len() {
            let key = (path.clone(), index);
            if !rejected.contains(&key) && !matched_external.contains(&key) {
                instrumentation.exact_bucket_inserts =
                    instrumentation.exact_bucket_inserts.saturating_add(1);
                external_exact
                    .entry(classes[index])
                    .or_default()
                    .push((path.clone(), index));
            }
        }
    }
    let base_class_counts = base_exact
        .iter()
        .map(|(class, candidates)| (*class, candidates.len()))
        .collect::<BTreeMap<_, _>>();
    let external_class_counts = external_exact
        .iter()
        .map(|(class, candidates)| (*class, candidates.len()))
        .collect::<BTreeMap<_, _>>();
    for (class, base_candidates) in &base_exact {
        instrumentation.exact_bucket_lookups =
            instrumentation.exact_bucket_lookups.saturating_add(1);
        let Some(external_candidates) = external_exact.get(class) else {
            continue;
        };
        if base_candidates.len() != 1 || external_candidates.len() != 1 {
            continue;
        }
        let (base_path, base_index, block_id) = &base_candidates[0];
        let (external_path, external_index) = &external_candidates[0];
        if used_blocks.insert(*block_id) {
            record_block_match(
                matches,
                &mut matched_external,
                &mut matched_base,
                base_path,
                *base_index,
                &external_by_path[external_path],
                *external_index,
                *block_id,
                BlockMatchBasis::ReceiptStructuralExact,
                instrumentation,
            )?;
        }
    }

    let page_matches = matches.pages.clone();
    for page_match in &page_matches {
        let base_tree = &base_by_path[&page_match.previous_path];
        let external_tree = &external_by_path[&page_match.path];
        let annotations = &annotations_by_path[&page_match.previous_path];
        align_ordered_tree(
            &page_match.previous_path,
            base_tree,
            external_tree,
            &base_classes_by_path[&page_match.previous_path],
            &external_classes_by_path[&page_match.path],
            &base_class_counts,
            &external_class_counts,
            annotations,
            &rejected,
            &mut used_blocks,
            &mut matched_external,
            &mut matched_base,
            matches,
            instrumentation,
        )?;
    }
    matches.blocks.sort_unstable_by(|left, right| {
        (&left.path, &left.locator).cmp(&(&right.path, &right.locator))
    });
    Ok(ParsedImportDocuments {
        current: external_by_path,
        base: base_by_path,
    })
}

#[allow(clippy::too_many_arguments)]
fn record_block_match(
    matches: &mut ImportMatches,
    matched_external: &mut BTreeSet<(ManagedPath, usize)>,
    matched_base: &mut BTreeMap<(ManagedPath, usize), (ManagedPath, usize)>,
    base_path: &ManagedPath,
    base_index: usize,
    external_tree: &ParsedTree,
    external_index: usize,
    block_id: BlockId,
    basis: BlockMatchBasis,
    instrumentation: &mut ImportInstrumentation,
) -> Result<(), ImportBlock> {
    matches.blocks.push(BlockImportMatch {
        path: external_tree.path.clone(),
        locator: materialize_locator(external_tree, external_index, instrumentation)?,
        block_id,
        basis,
    });
    matched_external.insert((external_tree.path.clone(), external_index));
    matched_base.insert(
        (base_path.clone(), base_index),
        (external_tree.path.clone(), external_index),
    );
    instrumentation.retained_block_matches =
        instrumentation.retained_block_matches.saturating_add(1);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn align_ordered_tree(
    base_path: &ManagedPath,
    base_tree: &ParsedTree,
    external_tree: &ParsedTree,
    base_classes: &[usize],
    external_classes: &[usize],
    base_class_counts: &BTreeMap<usize, usize>,
    external_class_counts: &BTreeMap<usize, usize>,
    annotations: &BTreeMap<usize, BlockId>,
    rejected: &BTreeSet<(ManagedPath, usize)>,
    used_blocks: &mut BTreeSet<BlockId>,
    matched_external: &mut BTreeSet<(ManagedPath, usize)>,
    matched_base: &mut BTreeMap<(ManagedPath, usize), (ManagedPath, usize)>,
    matches: &mut ImportMatches,
    instrumentation: &mut ImportInstrumentation,
) -> Result<(), ImportBlock> {
    let mut pending = vec![(None, None)];
    pending.extend(
        matched_base
            .iter()
            .filter(|((path, _), (external_path, _))| {
                path == base_path && external_path == &external_tree.path
            })
            .map(|((_, base), (_, external))| (Some(*base), Some(*external))),
    );
    let mut visited = BTreeSet::new();
    while let Some((base_parent, external_parent)) = pending.pop() {
        if !visited.insert((base_parent, external_parent)) {
            continue;
        }
        let base_sequence = base_parent
            .map(|parent| base_tree.nodes[parent].children.as_slice())
            .unwrap_or(&base_tree.roots);
        let external_sequence = external_parent
            .map(|parent| external_tree.nodes[parent].children.as_slice())
            .unwrap_or(&external_tree.roots);
        instrumentation.ordered_alignment_visits = instrumentation
            .ordered_alignment_visits
            .saturating_add(base_sequence.len())
            .saturating_add(external_sequence.len());

        let external_positions = external_sequence
            .iter()
            .enumerate()
            .map(|(position, index)| (*index, position))
            .collect::<BTreeMap<_, _>>();
        let mut boundaries = Vec::new();
        let mut last_external = None;
        for (base_position, base_index) in base_sequence.iter().enumerate() {
            let Some((external_path, external_index)) =
                matched_base.get(&(base_path.clone(), *base_index))
            else {
                continue;
            };
            if external_path != &external_tree.path {
                continue;
            }
            let Some(external_position) = external_positions.get(external_index).copied() else {
                continue;
            };
            if last_external.is_some_and(|last| external_position <= last) {
                boundaries.clear();
                break;
            }
            boundaries.push((base_position, external_position));
            last_external = Some(external_position);
        }
        let trusted_anchor_count = base_sequence
            .iter()
            .filter(|base_index| matched_base.contains_key(&(base_path.clone(), **base_index)))
            .count();
        if trusted_anchor_count > 0 && boundaries.len() != trusted_anchor_count {
            continue;
        }

        let mut previous_base = 0;
        let mut previous_external = 0;
        for (next_base, next_external) in boundaries.into_iter().chain(std::iter::once((
            base_sequence.len(),
            external_sequence.len(),
        ))) {
            let base_gap = base_sequence[previous_base..next_base]
                .iter()
                .copied()
                .filter(|index| {
                    annotations.get(index).is_some_and(|block_id| {
                        !used_blocks.contains(block_id)
                            && !matched_base.contains_key(&(base_path.clone(), *index))
                    })
                })
                .collect::<Vec<_>>();
            let external_gap = external_sequence[previous_external..next_external]
                .iter()
                .copied()
                .filter(|index| {
                    let key = (external_tree.path.clone(), *index);
                    !rejected.contains(&key) && !matched_external.contains(&key)
                })
                .collect::<Vec<_>>();
            if let ([base_index], [external_index]) = (base_gap.as_slice(), external_gap.as_slice())
            {
                if base_class_counts.get(&base_classes[*base_index]) != Some(&1)
                    || external_class_counts.get(&external_classes[*external_index]) != Some(&1)
                {
                    if next_base < base_sequence.len() && next_external < external_sequence.len() {
                        previous_base = next_base.saturating_add(1);
                        previous_external = next_external.saturating_add(1);
                    }
                    continue;
                }
                let block_id = annotations[base_index];
                if used_blocks.insert(block_id) {
                    record_block_match(
                        matches,
                        matched_external,
                        matched_base,
                        base_path,
                        *base_index,
                        external_tree,
                        *external_index,
                        block_id,
                        BlockMatchBasis::ReceiptOrderedTreeAlignment,
                        instrumentation,
                    )?;
                    pending.push((Some(*base_index), Some(*external_index)));
                }
            }
            if next_base < base_sequence.len() && next_external < external_sequence.len() {
                previous_base = next_base.saturating_add(1);
                previous_external = next_external.saturating_add(1);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictCopyClass {
    GeneratedExact,
    External,
    MixedUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictClassificationError {
    path: ManagedPath,
}

impl fmt::Display for ConflictClassificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not a recognized sync conflict copy", self.path)
    }
}

impl std::error::Error for ConflictClassificationError {}

/// Diagnostic classification from caller-supplied exact hashes.
///
/// The function is read-only and never removes the inventory entry or file.
/// Its result is not sealed generated-output evidence and must never authorize
/// deletion; a later deletion path must obtain its own authoritative proof.
pub fn classify_conflict_copy(
    path: ManagedPath,
    observed: &ExactBytes,
    generated_target: BlobDescription,
    exact_external: Option<BlobDescription>,
) -> Result<ConflictCopyClass, ConflictClassificationError> {
    if !path_is_sync_conflict(Path::new(path.as_str())) {
        return Err(ConflictClassificationError { path });
    }
    Ok(if observed.description() == generated_target {
        ConflictCopyClass::GeneratedExact
    } else if exact_external.is_some_and(|external| external == observed.description()) {
        ConflictCopyClass::External
    } else {
        ConflictCopyClass::MixedUnknown
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use rusqlite::Connection;
    use uuid::Uuid;

    use super::*;
    use crate::oplog::local_active::CleanLocalRuntime;
    use crate::oplog::operational_coordinator::{
        fail_next_clean_after_manifest_for_harness, CleanExternalMutationState,
        CleanLocalMutationState, OperationalCoordinator, OperationalPhase,
    };
    use crate::oplog::sqlite::{LeasedWorkspaceProjection, WorkspaceRuntimeLease};
    use crate::oplog::{
        AcceptedBatchEvent, ApplicationRuntimeRoot, AuthorBatch, BatchDisposition, BatchId,
        BatchOrigin, BlockLocation, CrdtPeerId, DeviceId, DocumentId, LineageDigest,
        LogseqUuidResolution, ManagedTextKind, ObjectStore, OperationTransaction,
        PortablePathIndexRoot, PreparedBatch, ProjectionClaim, ProjectionEndpointBinding,
        ProjectionEndpointId, ProjectionReceiptStore, ProjectionRecovery, RebuildSource,
        SemanticEffect, SemanticOperation, SessionId, SqliteFrontier,
        MAX_MATERIALIZATION_QUERY_ROWS,
    };

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("tine-import-snapshot-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(path.join("graph/pages")).unwrap();
            fs::create_dir_all(path.join("graph/journals")).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The clean baseline-plus-manifest composition used by the import corpus.
    /// Source files are the genesis; no enrolled projection index, scratch
    /// store, history store, or recovery-replay block participates.
    struct CleanSnapshotFixture {
        _root: TestRoot,
        graph_root: PathBuf,
        graph: Graph,
        receipts: ProjectionReceiptStore,
        runtime: CleanLocalRuntime,
        page_ids: Vec<PageId>,
        database: PathBuf,
    }

    impl CleanSnapshotFixture {
        fn new(label: &str, paths: &[&str]) -> Self {
            Self::new_with_initial_uuid_and_config(label, paths, None, None, None, None, None)
        }

        fn new_with_initial_uuid(
            label: &str,
            paths: &[&str],
            initial_uuid: Option<LogseqUuid>,
        ) -> Self {
            Self::new_with_initial_uuid_and_config(
                label,
                paths,
                initial_uuid,
                None,
                None,
                None,
                None,
            )
        }

        fn new_with_graph_config(label: &str, paths: &[&str], config: &str) -> Self {
            Self::new_with_initial_uuid_and_config(
                label,
                paths,
                None,
                Some(config),
                None,
                None,
                None,
            )
        }

        fn new_with_graph_config_names_and_contents(
            label: &str,
            paths: &[&str],
            config: &str,
            names: &[&str],
            contents: &[&str],
        ) -> Self {
            Self::new_with_initial_uuid_and_config(
                label,
                paths,
                None,
                Some(config),
                Some(names),
                Some(contents),
                None,
            )
        }

        fn new_with_graph_config_names_contents_and_preambles(
            label: &str,
            paths: &[&str],
            config: &str,
            names: &[&str],
            contents: &[&str],
            preambles: &[&str],
        ) -> Self {
            Self::new_with_initial_uuid_and_config(
                label,
                paths,
                None,
                Some(config),
                Some(names),
                Some(contents),
                Some(preambles),
            )
        }

        #[allow(clippy::too_many_arguments)]
        fn new_with_initial_uuid_and_config(
            label: &str,
            paths: &[&str],
            initial_uuid: Option<LogseqUuid>,
            config: Option<&str>,
            names: Option<&[&str]>,
            contents: Option<&[&str]>,
            preambles: Option<&[&str]>,
        ) -> Self {
            assert!(names.is_none_or(|values| values.len() == paths.len()));
            assert!(contents.is_none_or(|values| values.len() == paths.len()));
            assert!(preambles.is_none_or(|values| values.len() == paths.len()));
            let root = TestRoot::new(&format!("{label}-clean"));
            let graph_root = root.path().join("graph");
            if let Some(config) = config {
                fs::create_dir_all(graph_root.join("logseq")).unwrap();
                fs::write(graph_root.join("logseq/config.edn"), config).unwrap();
            }
            let graph = Graph::open(&graph_root);
            for (index, path) in paths.iter().enumerate() {
                let target = graph_root.join(path);
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                let content = contents
                    .map(|values| values[index].to_owned())
                    .unwrap_or_else(|| format!("page {index}"));
                let explicit_preamble = preambles.map(|values| values[index]);
                let provisional = clean_snapshot_source(
                    path,
                    explicit_preamble,
                    &content,
                    (index == 0).then_some(initial_uuid).flatten(),
                );
                fs::write(&target, provisional).unwrap();
                if explicit_preamble.is_none() {
                    let decoded = graph
                        .managed_entry_for_managed_path(
                            &ManagedPath::parse((*path).to_owned()).unwrap(),
                        )
                        .unwrap()
                        .name;
                    let desired = names
                        .map(|values| values[index].to_owned())
                        .unwrap_or_else(|| format!("Snapshot Page {index}"));
                    if decoded != desired {
                        let generated_preamble = if path.ends_with(".org") {
                            format!("#+TITLE: {desired}")
                        } else {
                            format!("title:: {desired}")
                        };
                        fs::write(
                            &target,
                            clean_snapshot_source(
                                path,
                                Some(&generated_preamble),
                                &content,
                                (index == 0).then_some(initial_uuid).flatten(),
                            ),
                        )
                        .unwrap();
                    }
                }
            }

            let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
            let lineage = LineageDigest::of(b"snapshot-test");
            let catalog = DocumentId::from_uuid(Uuid::from_u128(4));
            let database = root.path().join("clean-projection.sqlite");
            let archive = root.path().join("clean-archive");
            let enrollment = root.path().join("clean-enrollment");
            fs::create_dir(&archive).unwrap();
            let capture_root = root.path().join("clean-capture");
            fs::create_dir(&capture_root).unwrap();
            let capture = graph
                .capture_inactive_bootstrap_sources(&capture_root)
                .unwrap();
            let preparation = prepare_clean_activation(
                &graph,
                capture,
                workspace,
                lineage,
                catalog,
                &root.path().join("clean-preparation"),
                &database,
                &ReferenceCatalogPolicyV1::default(),
            )
            .unwrap();
            let page_ids = paths
                .iter()
                .map(|path| {
                    preparation
                        .candidates()
                        .baseline()
                        .page_ids()
                        .find(|page_id| {
                            preparation
                                .candidates()
                                .baseline()
                                .page(*page_id)
                                .unwrap()
                                .is_some_and(|page| page.path.as_str() == *path)
                        })
                        .unwrap_or_else(|| panic!("clean baseline has no {path}"))
                })
                .collect::<Vec<_>>();
            let committed = commit_clean_activation(
                &graph,
                preparation,
                &archive.join(crate::oplog::lazy_genesis::LAZY_GENESIS_BASELINE_DIRECTORY),
                &enrollment,
            )
            .unwrap();
            let (baseline, physical, baseline_frontier, _) = committed.into_parts();
            drop(physical);
            drop(baseline);
            let reopened = open_clean_activation(
                &enrollment,
                &archive.join(crate::oplog::lazy_genesis::LAZY_GENESIS_BASELINE_DIRECTORY),
                &database,
                catalog,
                ReferenceCatalogPolicyV1::default(),
            )
            .unwrap()
            .expect("published clean snapshot activation reopens");
            let (mut engine, projection, _) = reopened.into_parts();
            let operations = archive.join("operations");
            engine
                .attach_clean_archive_store(ObjectStore::open(&operations, workspace).unwrap())
                .unwrap();
            let store = ObjectStore::open(&operations, workspace).unwrap();
            let lease = WorkspaceRuntimeLease::acquire(&store, workspace).unwrap();
            let projection = LeasedWorkspaceProjection::adopt_clean_genesis(
                lease,
                &database,
                ProjectionClaim::current(workspace, lineage),
                &baseline_frontier,
                &store,
                &engine,
                projection,
            )
            .map_err(|(_, error)| error)
            .unwrap();
            let endpoint = ProjectionEndpointBinding::enroll_graph(
                &graph,
                ProjectionEndpointId::from_uuid(Uuid::from_u128(2)),
                DeviceId::from_uuid(Uuid::from_u128(3)),
            )
            .unwrap();
            let receipts = ProjectionReceiptStore::open_for_endpoint(
                &root.path().join("clean-receipts"),
                workspace,
                endpoint,
            )
            .unwrap();
            engine
                .attach_clean_projection_endpoint(&graph, &receipts)
                .unwrap();
            let runtime = CleanLocalRuntime::from_open_parts(
                SessionId::from_uuid(Uuid::from_u128(7)),
                endpoint,
                engine,
                projection,
            )
            .unwrap();
            Self {
                _root: root,
                graph_root,
                graph,
                receipts,
                runtime,
                page_ids,
                database,
            }
        }

        fn plan(&self, paths: &[&str]) -> ImportPlan {
            plan_clean_affected_import(
                &self.graph,
                self.runtime.engine(),
                self.runtime.database(),
                paths,
            )
        }

        fn engine(&self) -> &ShardedHotEngine {
            self.runtime.engine()
        }

        fn page_id(&self, index: usize) -> PageId {
            self.page_ids[index]
        }

        fn apply_external_paths(&mut self, paths: &[&str]) -> BatchId {
            let mut projection_turns =
                crate::oplog::projection_turn_journal::open_scratch_projection_turn_journal_for(
                    self.runtime.engine(),
                );
            let mut session = self.runtime.admit_clean_mutation(&self.graph).unwrap();
            match OperationalCoordinator::execute_clean_external(
                &mut session,
                &self.graph,
                &self.receipts,
                paths,
                &mut crate::oplog::absence_sweep::NoopSweepRecorder,
                &mut projection_turns,
            )
            .unwrap()
            {
                CleanExternalMutationState::Complete(batch_id) => batch_id,
                CleanExternalMutationState::Noop => {
                    panic!("clean external reconciliation unexpectedly became a no-op")
                }
                CleanExternalMutationState::DurablePending(pending) => {
                    panic!(
                        "clean external reconciliation remained pending: {}",
                        pending.failure()
                    )
                }
            }
        }

        fn reopen_after_config_change(self) -> Self {
            let Self {
                _root,
                graph_root,
                graph,
                receipts,
                runtime,
                page_ids,
                database,
            } = self;
            drop(graph);
            drop(runtime);
            let graph = Graph::open(&graph_root);
            let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
            let lineage = LineageDigest::of(b"snapshot-test");
            let catalog = DocumentId::from_uuid(Uuid::from_u128(4));
            let archive = _root.path().join("clean-archive");
            let enrollment = _root.path().join("clean-enrollment");
            let reopened = open_clean_activation(
                &enrollment,
                &archive.join(crate::oplog::lazy_genesis::LAZY_GENESIS_BASELINE_DIRECTORY),
                &database,
                catalog,
                ReferenceCatalogPolicyV1::default(),
            )
            .unwrap()
            .expect("published clean snapshot activation reopens after config change");
            let (mut engine, baseline_projection, _) = reopened.into_parts();
            let operations = archive.join("operations");
            engine
                .attach_clean_archive_store(ObjectStore::open(&operations, workspace).unwrap())
                .unwrap();
            let baseline_root = engine.accepted_frontier_root().unwrap();
            let baseline_claim_source = crate::oplog::sqlite::clean_genesis_materialized_read(
                &baseline_projection,
                &baseline_root,
            )
            .unwrap();
            let replayed = engine
                .replay_clean_committed_tail(&baseline_claim_source)
                .unwrap();
            drop(baseline_claim_source);
            let store = ObjectStore::open(&operations, workspace).unwrap();
            let lease = WorkspaceRuntimeLease::acquire(&store, workspace).unwrap();
            let projection = if replayed == 0 {
                let expected = engine.accepted_frontier_root().unwrap();
                LeasedWorkspaceProjection::adopt_clean_genesis(
                    lease,
                    &database,
                    ProjectionClaim::current(workspace, lineage),
                    &expected,
                    &store,
                    &engine,
                    baseline_projection,
                )
                .map_err(|(_, error)| error)
                .unwrap()
            } else {
                drop(baseline_projection);
                let application_runtime = ApplicationRuntimeRoot::open_for_test(
                    &_root.path().join("clean-application-runtime"),
                )
                .unwrap();
                let source = RebuildSource::new(&engine, &store).unwrap();
                LeasedWorkspaceProjection::open_under(lease, |slot| {
                    let opened = SqliteFrontier::open_or_rebuild_with_applier_slot(
                        &database,
                        &application_runtime,
                        ProjectionClaim::current(workspace, lineage),
                        source,
                        slot,
                    )?;
                    Ok::<_, crate::oplog::SqliteProjectionError>((opened, ()))
                })
                .map(|(projection, ())| projection)
                .map_err(|(_, error)| error)
                .unwrap()
            };
            let endpoint = ProjectionEndpointBinding::enroll_graph(
                &graph,
                ProjectionEndpointId::from_uuid(Uuid::from_u128(2)),
                DeviceId::from_uuid(Uuid::from_u128(3)),
            )
            .unwrap();
            engine
                .attach_clean_projection_endpoint(&graph, &receipts)
                .unwrap();
            let runtime = CleanLocalRuntime::from_open_parts(
                SessionId::from_uuid(Uuid::from_u128(7)),
                endpoint,
                engine,
                projection,
            )
            .unwrap();
            Self {
                _root,
                graph_root,
                graph,
                receipts,
                runtime,
                page_ids,
                database,
            }
        }
    }

    fn clean_snapshot_source(
        path: &str,
        preamble: Option<&str>,
        content: &str,
        initial_uuid: Option<LogseqUuid>,
    ) -> Vec<u8> {
        let mut content = content.to_owned();
        if let Some(logseq_uuid) = initial_uuid {
            let uuid = logseq_uuid.to_string();
            if !content.contains(&uuid) {
                content.push_str("\nid:: ");
                content.push_str(&uuid);
            }
        }
        let mut source = String::new();
        if let Some(preamble) = preamble {
            source.push_str(preamble);
            source.push_str("\n\n");
        }
        let mut lines = content.lines();
        let first = lines.next().unwrap_or_default();
        if path.ends_with(".org") {
            source.push_str("* ");
            source.push_str(first);
            source.push('\n');
            if let Some(logseq_uuid) = initial_uuid {
                source.push_str(":PROPERTIES:\n:id: ");
                source.push_str(&logseq_uuid.to_string());
                source.push_str("\n:END:\n");
            }
            for line in lines.filter(|line| !line.starts_with("id::")) {
                source.push_str(line);
                source.push('\n');
            }
        } else {
            source.push_str("- ");
            source.push_str(first);
            source.push('\n');
            for line in lines {
                source.push_str("  ");
                source.push_str(line);
                source.push('\n');
            }
        }
        source.into_bytes()
    }

    #[test]
    fn snapshot_revalidation_rejects_content_replacement_between_passes_clean() {
        let fixture = CleanSnapshotFixture::new("content", &["pages/a.md"]);
        let target = fixture.graph_root.join("pages/a.md");
        SNAPSHOT_REVALIDATION_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(target, b"- replaced\n").unwrap();
            }));
        });
        let plan = fixture.plan(&["pages/a.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Blocked);
        assert_eq!(plan.blocks()[0].reason, ImportBlockReason::StaleScope);
    }

    #[test]
    fn source_admission_accepts_non_round_tripping_org_as_exact_read_only_source_clean() {
        for (label, path, source) in [(
            "skipped-org-admission",
            "pages/a.org",
            "* changed\n*** skipped\n",
        )] {
            let fixture = CleanSnapshotFixture::new(label, &[path]);
            let target = fixture.graph_root.join(path);
            fs::write(&target, source).unwrap();
            let plan = fixture.plan(&[path]);
            assert_eq!(
                plan.status(),
                ImportPlanStatus::Reconcile,
                "{label}: {plan:?}"
            );
            assert!(
                plan.execution_material().is_ok(),
                "{label} did not expose semantic execution material"
            );
            assert_eq!(fs::read(target).unwrap(), source.as_bytes());
        }
    }

    #[test]
    fn source_admission_accepts_non_round_tripping_markdown_as_exact_read_only_source_clean() {
        let source = "- root\r  ```\r  - fake\r  ```";
        let fixture = CleanSnapshotFixture::new("non-round-tripping-markdown", &["pages/a.md"]);
        let target = fixture.graph_root.join("pages/a.md");
        fs::write(&target, source).unwrap();

        let plan = fixture.plan(&["pages/a.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
        assert!(plan.execution_material().is_ok());
        assert_eq!(fs::read(target).unwrap(), source.as_bytes());
    }

    #[test]
    fn source_admission_accepts_structurally_round_tripping_markdown_before_material_clean() {
        let source = "- changed\n\t- child\n  - grandchild\n";
        let fixture = CleanSnapshotFixture::new("mixed-markdown-admission", &["pages/a.md"]);
        let target = fixture.graph_root.join("pages/a.md");
        fs::write(&target, source).unwrap();

        let plan = fixture.plan(&["pages/a.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
        assert!(
            plan.execution_material().is_ok(),
            "structurally stable Markdown must expose execution material"
        );
        assert_eq!(fs::read(target).unwrap(), source.as_bytes());
    }

    #[test]
    fn source_admission_refuses_overlapping_lsdoc_events_without_touching_bytes_clean() {
        let source = "- $$x$$ # #+BEGIN_NOTE\r\nx\r\n#+END_NOTE";
        let fixture =
            CleanSnapshotFixture::new("overlapping-outline-admission", &["pages/overlap.md"]);
        let target = fixture.graph_root.join("pages/overlap.md");
        fs::write(&target, source).unwrap();

        let plan = fixture.plan(&["pages/overlap.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Blocked);
        assert_eq!(
            plan.blocks()
                .iter()
                .map(|block| block.reason)
                .collect::<Vec<_>>(),
            vec![ImportBlockReason::UnsafeInput]
        );
        assert!(
            plan.blocks()[0]
                .detail
                .contains("external document parser rejected source"),
            "{:?}",
            plan.blocks()[0]
        );
        assert_eq!(fs::read(target).unwrap(), source.as_bytes());
    }

    #[test]
    fn parser_owned_markdown_and_org_admission_preserves_exact_source_bytes_clean() {
        for (label, path, source) in [
            (
                "parser-owned-markdown-source",
                "pages/parser-owned.md",
                "title:: café\r\n\r\n# Project Ω\r\n\t- child\r\n- sibling\r\n",
            ),
            (
                "parser-owned-org-source",
                "pages/parser-owned.org",
                "#+TITLE: café\r\n\r\n* Project Ω\r\n** child\r\n* sibling\r\n",
            ),
        ] {
            let fixture = CleanSnapshotFixture::new(label, &[path]);
            let target = fixture.graph_root.join(path);
            fs::write(&target, source).unwrap();

            let plan = fixture.plan(&[path]);
            assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
            assert!(plan.execution_material().is_ok(), "{plan:?}");
            assert_eq!(fs::read(target).unwrap(), source.as_bytes());
        }
    }

    #[test]
    fn snapshot_revalidation_rejects_two_path_rename_between_passes_clean() {
        let fixture = CleanSnapshotFixture::new("rename", &["pages/a.md", "pages/b.md"]);
        let a = fixture.graph_root.join("pages/a.md");
        let b = fixture.graph_root.join("pages/b.md");
        let temporary = fixture.graph_root.join("pages/swap.tmp");
        SNAPSHOT_REVALIDATION_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::rename(&a, &temporary).unwrap();
                fs::rename(&b, &a).unwrap();
                fs::rename(&temporary, &b).unwrap();
            }));
        });
        let plan = fixture.plan(&["pages/a.md", "pages/b.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Blocked);
        assert_eq!(plan.blocks()[0].reason, ImportBlockReason::StaleScope);
    }

    #[test]
    fn snapshot_revalidation_rejects_catalog_change_between_passes_clean() {
        let fixture = CleanSnapshotFixture::new("catalog", &["pages/a.md"]);
        let other = CleanSnapshotFixture::new_with_initial_uuid_and_config(
            "catalog-other",
            &["pages/a.md"],
            None,
            None,
            None,
            Some(&["different predecessor"]),
            None,
        );
        let paths = vec![ManagedPath::parse("pages/a.md").unwrap()];
        let changed = clean_import_predecessor_authority(
            other.runtime.engine(),
            other.runtime.database(),
            &paths,
        )
        .unwrap();
        POST_CLEAN_PREDECESSOR_OVERRIDE.with(|authority| {
            *authority.borrow_mut() = Some(changed);
        });
        let plan = fixture.plan(&["pages/a.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Blocked);
        assert_eq!(plan.blocks()[0].reason, ImportBlockReason::StaleScope);
    }

    #[test]
    fn snapshot_revalidation_rejects_accepted_frontier_change_between_passes_clean() {
        let fixture = CleanSnapshotFixture::new("frontier", &["pages/a.md"]);
        let other = CleanSnapshotFixture::new("frontier-other", &["pages/a.md", "pages/b.md"]);
        POST_FRONTIER_OVERRIDE.with(|root| {
            *root.borrow_mut() = Some(other.engine().accepted_frontier_root().unwrap());
        });
        let plan = fixture.plan(&["pages/a.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Blocked);
        assert_eq!(plan.blocks()[0].reason, ImportBlockReason::StaleScope);
    }

    #[test]
    fn execution_material_refuses_noop_and_blocked_plans_clean() {
        let fixture = CleanSnapshotFixture::new("execution-refusal", &["pages/a.md"]);
        let noop = fixture.plan(&["pages/a.md"]);
        assert_eq!(noop.status(), ImportPlanStatus::Noop);
        assert_eq!(
            noop.execution_material().unwrap_err(),
            ImportExecutionError::RefusedStatus(ImportPlanStatus::Noop)
        );

        fs::write(fixture.graph_root.join("pages/a.md"), [0xff]).unwrap();
        let blocked = fixture.plan(&["pages/a.md"]);
        assert_eq!(blocked.status(), ImportPlanStatus::Blocked);
        assert_eq!(
            blocked.execution_material().unwrap_err(),
            ImportExecutionError::RefusedStatus(ImportPlanStatus::Blocked)
        );

        fs::write(
            fixture
                .graph_root
                .join("pages/a.sync-conflict-20260725-120000-AAAAAAA.md"),
            b"- diagnostic only\n",
        )
        .unwrap();
        let conflict = fixture.plan(&["pages/a.sync-conflict-20260725-120000-AAAAAAA.md"]);
        assert_eq!(conflict.status(), ImportPlanStatus::Blocked);
        assert!(conflict
            .blocks()
            .iter()
            .any(|block| block.detail.contains("diagnostic inputs")));
    }

    #[test]
    fn identical_sealed_reconciliations_produce_identical_execution_and_observation_bytes_clean() {
        let left = CleanSnapshotFixture::new("execution-identical-left", &["pages/a.md"]);
        let right = CleanSnapshotFixture::new("execution-identical-right", &["pages/a.md"]);
        fs::write(left.graph_root.join("pages/a.md"), b"- changed\n").unwrap();
        fs::write(right.graph_root.join("pages/a.md"), b"- changed\n").unwrap();

        let left_plan = left.plan(&["pages/a.md"]);
        let right_plan = right.plan(&["pages/a.md"]);
        assert_eq!(left_plan.status(), ImportPlanStatus::Reconcile);
        assert_eq!(right_plan.status(), ImportPlanStatus::Reconcile);
        assert_eq!(left_plan.import_id(), right_plan.import_id());
        let left_material = left_plan.execution_material().unwrap();
        let right_material = right_plan.execution_material().unwrap();
        assert_eq!(left_material, right_material);
        assert_eq!(
            left_material.batch_id(),
            left_material.import_id().batch_id()
        );
        assert_eq!(
            left_material.origin(),
            BatchOrigin::ExternalReconciliation {
                import_id: left_material.import_id()
            }
        );

        let left_object = left_material
            .observation()
            .clone()
            .into_operation_object(PortablePathIndexRoot::empty())
            .unwrap();
        let right_object = right_material
            .observation()
            .clone()
            .into_operation_object(PortablePathIndexRoot::empty())
            .unwrap();
        assert_eq!(left_object, right_object);
        assert_eq!(
            left_object.descriptor().unwrap(),
            right_object.descriptor().unwrap()
        );
        let mut malformed = left_object.payload().to_vec();
        malformed[0] ^= 0xff;
        assert!(
            super::super::external_import::ExternalImportObservation::decode(&malformed).is_err()
        );
    }

    #[test]
    fn execution_material_preserves_explicit_external_id_change_and_removal_clean() {
        let old = LogseqUuid::from_uuid(Uuid::from_u128(910));
        let changed = LogseqUuid::from_uuid(Uuid::from_u128(911));
        let replacement = CleanSnapshotFixture::new_with_initial_uuid(
            "execution-id-replacement",
            &["pages/a.md"],
            Some(old),
        );
        fs::write(
            replacement.graph_root.join("pages/a.md"),
            format!("- page 0\n  id:: {changed}\n"),
        )
        .unwrap();
        let replacement_plan = replacement.plan(&["pages/a.md"]);
        let replacement_transaction = &replacement_plan
            .execution_material()
            .unwrap()
            .transaction()
            .operations;
        assert!(replacement_transaction.iter().any(|operation| {
            matches!(
                operation,
                SemanticOperation::MutateBlockLogseqIdentity {
                    mutation: LogseqIdentityMutation::ReplaceExternal { logseq_uuid },
                    ..
                } if *logseq_uuid == changed
            )
        }));

        let removal = CleanSnapshotFixture::new_with_initial_uuid(
            "execution-id-removal",
            &["pages/a.md"],
            Some(old),
        );
        fs::write(removal.graph_root.join("pages/a.md"), b"- page 0\n").unwrap();
        let removal_plan = removal.plan(&["pages/a.md"]);
        let removal_transaction = &removal_plan
            .execution_material()
            .unwrap()
            .transaction()
            .operations;
        assert!(removal_transaction.iter().any(|operation| {
            matches!(
                operation,
                SemanticOperation::MutateBlockLogseqIdentity {
                    mutation: LogseqIdentityMutation::RemoveExternal,
                    ..
                }
            )
        }));
    }

    #[test]
    fn execution_material_retains_invalid_and_duplicate_raw_ids_without_identity_authority_clean() {
        let duplicate = CleanSnapshotFixture::new("execution-duplicate-id", &["pages/a.md"]);
        let duplicate_bytes = format!(
            "- page 0\n  id:: {}\n  id:: {}\n",
            LogseqUuid::from_uuid(Uuid::from_u128(920)),
            LogseqUuid::from_uuid(Uuid::from_u128(920)),
        )
        .into_bytes();
        fs::write(duplicate.graph_root.join("pages/a.md"), &duplicate_bytes).unwrap();
        let duplicate_plan = duplicate.plan(&["pages/a.md"]);
        let duplicate_object = duplicate_plan
            .execution_material()
            .unwrap()
            .observation()
            .clone()
            .into_operation_object(PortablePathIndexRoot::empty())
            .unwrap();
        let duplicate_observation =
            super::super::external_import::ExternalImportObservation::decode(
                duplicate_object.payload(),
            )
            .unwrap();
        let duplicate_entry = &duplicate_observation.entries()[0];
        assert_eq!(
            duplicate_entry.state().bytes(),
            Some(duplicate_bytes.as_slice())
        );
        assert!(duplicate_entry
            .state()
            .annotations()
            .iter()
            .all(|annotation| annotation.logseq_uuid().is_none()));

        let invalid = CleanSnapshotFixture::new("execution-invalid-id", &["pages/a.md"]);
        let invalid_bytes = b"- page 0\n  id:: definitely-not-a-uuid\n";
        fs::write(invalid.graph_root.join("pages/a.md"), invalid_bytes).unwrap();
        let invalid_plan = invalid.plan(&["pages/a.md"]);
        let invalid_object = invalid_plan
            .execution_material()
            .unwrap()
            .observation()
            .clone()
            .into_operation_object(PortablePathIndexRoot::empty())
            .unwrap();
        let invalid_observation = super::super::external_import::ExternalImportObservation::decode(
            invalid_object.payload(),
        )
        .unwrap();
        let invalid_entry = &invalid_observation.entries()[0];
        assert_eq!(
            invalid_entry.state().bytes(),
            Some(invalid_bytes.as_slice())
        );
        assert!(invalid_entry
            .state()
            .annotations()
            .iter()
            .all(|annotation| annotation.logseq_uuid().is_none()));
    }

    #[test]
    fn clean_uuid_history_edit_reopens_with_the_same_materialized_identity() {
        let uuid = LogseqUuid::from_uuid(Uuid::from_u128(0x3700_0001));
        let mut fixture = CleanSnapshotFixture::new_with_initial_uuid(
            "uuid-history-edit",
            &["pages/a.md"],
            Some(uuid),
        );
        let block_id = fixture
            .engine()
            .materialize_page(fixture.page_id(0))
            .unwrap()
            .blocks[0]
            .block_id;
        fs::write(
            fixture.graph_root.join("pages/a.md"),
            format!("- externally edited\n  id:: {uuid}\n"),
        )
        .unwrap();
        fixture.apply_external_paths(&["pages/a.md"]);

        let reopened = fixture.reopen_after_config_change();
        let page = reopened
            .engine()
            .materialize_page(reopened.page_id(0))
            .unwrap();
        let block = page
            .blocks
            .iter()
            .find(|block| block.block_id == block_id)
            .expect("edited UUID-bearing block survives clean reopen");
        assert_eq!(block.logseq_uuid, Some(uuid));
        assert_eq!(block.content, format!("externally edited\nid:: {uuid}"));
    }

    #[test]
    fn clean_uuid_history_cross_page_move_reopens_with_the_stable_block() {
        let uuid = LogseqUuid::from_uuid(Uuid::from_u128(0x3700_0002));
        let mut fixture = CleanSnapshotFixture::new_with_initial_uuid_and_config(
            "uuid-history-cross-page-move",
            &["pages/a.md", "pages/b.md"],
            Some(uuid),
            None,
            None,
            Some(&["anchored", "destination"]),
            None,
        );
        let block_id = fixture
            .engine()
            .materialize_page(fixture.page_id(0))
            .unwrap()
            .blocks[0]
            .block_id;
        fs::write(
            fixture.graph_root.join("pages/a.md"),
            b"- source remainder\n",
        )
        .unwrap();
        fs::write(
            fixture.graph_root.join("pages/b.md"),
            format!("- destination\n- anchored\n  id:: {uuid}\n"),
        )
        .unwrap();
        fixture.apply_external_paths(&["pages/a.md", "pages/b.md"]);

        let reopened = fixture.reopen_after_config_change();
        let destination = reopened
            .engine()
            .materialize_page(reopened.page_id(1))
            .unwrap();
        let moved = destination
            .blocks
            .iter()
            .find(|block| block.block_id == block_id)
            .expect("UUID-bearing block keeps its stable identity across the move");
        assert_eq!(moved.logseq_uuid, Some(uuid));
        assert_eq!(moved.content, format!("anchored\nid:: {uuid}"));
    }

    #[test]
    fn clean_uuid_history_replacement_and_removal_reopen_without_stale_claims() {
        let original = LogseqUuid::from_uuid(Uuid::from_u128(0x3700_0003));
        let replacement = LogseqUuid::from_uuid(Uuid::from_u128(0x3700_0004));
        let mut replaced = CleanSnapshotFixture::new_with_initial_uuid(
            "uuid-history-replacement",
            &["pages/a.md"],
            Some(original),
        );
        fs::write(
            replaced.graph_root.join("pages/a.md"),
            format!("- page 0\n  id:: {replacement}\n"),
        )
        .unwrap();
        replaced.apply_external_paths(&["pages/a.md"]);
        let replaced = replaced.reopen_after_config_change();
        let block = &replaced
            .engine()
            .materialize_page(replaced.page_id(0))
            .unwrap()
            .blocks[0];
        assert_eq!(block.logseq_uuid, Some(replacement));
        assert_eq!(
            replaced.engine().resolve_logseq_uuid(original).unwrap(),
            LogseqUuidResolution::Unclaimed
        );

        let mut removed = CleanSnapshotFixture::new_with_initial_uuid(
            "uuid-history-removal",
            &["pages/a.md"],
            Some(original),
        );
        fs::write(removed.graph_root.join("pages/a.md"), b"- page 0\n").unwrap();
        removed.apply_external_paths(&["pages/a.md"]);
        let removed = removed.reopen_after_config_change();
        let block = &removed
            .engine()
            .materialize_page(removed.page_id(0))
            .unwrap()
            .blocks[0];
        assert_eq!(block.logseq_uuid, None);
        assert_eq!(
            removed.engine().resolve_logseq_uuid(original).unwrap(),
            LogseqUuidResolution::Unclaimed
        );
    }

    #[test]
    fn clean_uuid_history_duplicate_copy_and_reference_reopen_fail_closed() {
        let uuid = LogseqUuid::from_uuid(Uuid::from_u128(0x3700_0005));
        let mut fixture = CleanSnapshotFixture::new_with_initial_uuid(
            "uuid-history-duplicate-copy-reference",
            &["pages/a.md"],
            Some(uuid),
        );
        fs::write(
            fixture.graph_root.join("pages/copy.md"),
            format!("- copied raw block\n  id:: {uuid}\n- reference (({uuid}))\n"),
        )
        .unwrap();
        let error = {
            let mut projection_turns =
                crate::oplog::projection_turn_journal::open_scratch_projection_turn_journal_for(
                    fixture.runtime.engine(),
                );
            let mut session = fixture
                .runtime
                .admit_clean_mutation(&fixture.graph)
                .unwrap();
            match OperationalCoordinator::execute_clean_external(
                &mut session,
                &fixture.graph,
                &fixture.receipts,
                &["pages/copy.md"],
                &mut crate::oplog::absence_sweep::NoopSweepRecorder,
                &mut projection_turns,
            ) {
                Err(error) => error,
                Ok(_) => panic!("duplicate UUID copy unexpectedly entered durable history"),
            }
        };
        assert_eq!(error.phase(), OperationalPhase::Draft);
        assert!(error.to_string().contains("2 live authoritative claims"));

        let reopened = fixture.reopen_after_config_change();
        let claim_source = reopened.runtime.database().materialized_read().unwrap();
        let original = reopened
            .engine()
            .materialize_page_with_claim_source(reopened.page_id(0), &claim_source)
            .unwrap();
        assert_eq!(original.blocks[0].logseq_uuid, Some(uuid));
        drop(claim_source);
        assert_eq!(
            fs::read_to_string(reopened.graph_root.join("pages/copy.md")).unwrap(),
            format!("- copied raw block\n  id:: {uuid}\n- reference (({uuid}))\n")
        );
    }

    #[test]
    fn clean_uuid_history_manifest_cut_replays_the_uuid_bearing_block() {
        let uuid = LogseqUuid::from_uuid(Uuid::from_u128(0x3700_0006));
        let mut fixture = CleanSnapshotFixture::new_with_initial_uuid(
            "uuid-history-manifest-cut",
            &["pages/a.md"],
            Some(uuid),
        );
        let block_id = fixture
            .engine()
            .materialize_page(fixture.page_id(0))
            .unwrap()
            .blocks[0]
            .block_id;
        fs::write(
            fixture.graph_root.join("pages/a.md"),
            format!("- edited before manifest cut\n  id:: {uuid}\n"),
        )
        .unwrap();
        fail_next_clean_after_manifest_for_harness();
        let pending = {
            let mut projection_turns =
                crate::oplog::projection_turn_journal::open_scratch_projection_turn_journal_for(
                    fixture.runtime.engine(),
                );
            let mut session = fixture
                .runtime
                .admit_clean_mutation(&fixture.graph)
                .unwrap();
            match OperationalCoordinator::execute_clean_external(
                &mut session,
                &fixture.graph,
                &fixture.receipts,
                &["pages/a.md"],
                &mut crate::oplog::absence_sweep::NoopSweepRecorder,
                &mut projection_turns,
            )
            .unwrap()
            {
                CleanExternalMutationState::DurablePending(pending) => pending,
                CleanExternalMutationState::Complete(_) => {
                    panic!("post-manifest fault unexpectedly completed UUID-bearing edit")
                }
                CleanExternalMutationState::Noop => {
                    panic!("UUID-bearing external edit unexpectedly became a no-op")
                }
            }
        };
        drop(pending);

        let reopened = fixture.reopen_after_config_change();
        let page = reopened
            .engine()
            .materialize_page(reopened.page_id(0))
            .unwrap();
        let block = page
            .blocks
            .iter()
            .find(|candidate| candidate.block_id == block_id)
            .expect("UUID-bearing block survives cold committed-tail replay");
        assert_eq!(block.logseq_uuid, Some(uuid));
        assert_eq!(
            block.content,
            format!("edited before manifest cut\nid:: {uuid}")
        );
    }

    #[test]
    fn execution_material_retains_nested_rename_and_delete_semantics_clean() {
        let renamed =
            CleanSnapshotFixture::new("execution-nested-rename", &["pages/topic/old-name.md"]);
        fs::create_dir_all(renamed.graph_root.join("pages/topic/next")).unwrap();
        fs::rename(
            renamed.graph_root.join("pages/topic/old-name.md"),
            renamed.graph_root.join("pages/topic/next/new-name.md"),
        )
        .unwrap();
        let rename_plan =
            renamed.plan(&["pages/topic/old-name.md", "pages/topic/next/new-name.md"]);
        let rename_transaction = &rename_plan
            .execution_material()
            .unwrap()
            .transaction()
            .operations;
        assert!(rename_transaction.iter().any(|operation| {
            matches!(
                operation,
                SemanticOperation::ReconcileExternalPageState { path, .. }
                    if path.as_str() == "pages/topic/next/new-name.md"
            )
        }));

        let deleted =
            CleanSnapshotFixture::new("execution-nested-delete", &["pages/topic/delete-me.md"]);
        fs::remove_file(deleted.graph_root.join("pages/topic/delete-me.md")).unwrap();
        let delete_plan = deleted.plan(&["pages/topic/delete-me.md"]);
        let delete_transaction = &delete_plan
            .execution_material()
            .unwrap()
            .transaction()
            .operations;
        assert!(delete_transaction
            .iter()
            .any(|operation| matches!(operation, SemanticOperation::DeleteSubtree { .. })));
        assert!(delete_transaction
            .iter()
            .any(|operation| matches!(operation, SemanticOperation::DeletePage { .. })));
    }

    #[test]
    fn execution_material_uses_graph_filename_decoding_for_affected_new_paths_clean() {
        let legacy = CleanSnapshotFixture::new_with_graph_config(
            "execution-path-names-legacy",
            &["pages/seed.md"],
            "{:journal/file-name-format \"dd-MM-yyyy\" :journal/page-title-format \"yyyy-MM-dd\"}\n",
        );
        for path in [
            "pages/first/second/Project%2FPlan.md",
            "pages/left/shared.md",
            "pages/right/shared.md",
            "journals/archive/deep/25-07-2026.md",
        ] {
            let target = legacy.graph_root.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, b"- external\n").unwrap();
        }
        let legacy_plan = legacy.plan(&[
            "pages/first/second/Project%2FPlan.md",
            "journals/archive/deep/25-07-2026.md",
        ]);
        assert_eq!(legacy_plan.status(), ImportPlanStatus::Reconcile);
        let legacy_creates = legacy_plan
            .execution_material()
            .unwrap()
            .transaction()
            .operations
            .iter()
            .filter_map(|operation| match operation {
                SemanticOperation::CreatePage {
                    name, path, kind, ..
                } => Some((path.as_str(), (name.as_str(), *kind))),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            legacy_creates["pages/first/second/Project%2FPlan.md"],
            ("Project/Plan", ManagedTextKind::Page),
            "nested directories select the managed root but never become a page namespace"
        );
        assert_eq!(
            legacy_creates["journals/archive/deep/25-07-2026.md"],
            ("2026-07-25", ManagedTextKind::Journal),
            "nested journals use the configured JournalFormat title"
        );

        let duplicate_names = legacy.plan(&["pages/left/shared.md", "pages/right/shared.md"]);
        assert!(
            duplicate_names.blocks().is_empty(),
            "same basenames in distinct paths must not deny the transaction: {:?}",
            duplicate_names.blocks()
        );
        let duplicate_creates = duplicate_names
            .execution_material()
            .unwrap()
            .transaction()
            .operations
            .iter()
            .filter_map(|operation| match operation {
                SemanticOperation::CreatePage { name, path, .. } => {
                    Some((path.as_str().to_owned(), name.as_str().to_owned()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            duplicate_creates,
            vec![("pages/left/shared.md".to_owned(), "shared".to_owned())],
            "exactly one deterministic exact path carries the shared name, as at activation"
        );
        assert!(
            duplicate_names
                .execution_material()
                .unwrap()
                .observation()
                .entries()
                .iter()
                .any(|entry| entry.path().as_str() == "pages/right/shared.md"
                    && entry.state().bytes() == Some(b"- external\n".as_slice())
                    && entry.state().annotations().is_empty()),
            "the withheld source is still observed exactly, with no identity assigned"
        );

        let triple_lowbar = CleanSnapshotFixture::new_with_graph_config(
            "execution-path-names-triple-lowbar",
            &["pages/seed.md"],
            "{:file/name-format :triple-lowbar}\n",
        );
        for path in [
            "pages/deep/Team___Planning.md",
            "pages/deep/literal%5F%5F%5Fmarker.md",
        ] {
            let target = triple_lowbar.graph_root.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, b"- external\n").unwrap();
        }
        let triple_plan = triple_lowbar.plan(&[
            "pages/deep/Team___Planning.md",
            "pages/deep/literal%5F%5F%5Fmarker.md",
        ]);
        assert_eq!(triple_plan.status(), ImportPlanStatus::Reconcile);
        let triple_creates = triple_plan
            .execution_material()
            .unwrap()
            .transaction()
            .operations
            .iter()
            .filter_map(|operation| match operation {
                SemanticOperation::CreatePage {
                    name, path, kind, ..
                } => Some((path.as_str(), (name.as_str(), *kind))),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            triple_creates["pages/deep/Team___Planning.md"],
            ("Team/Planning", ManagedTextKind::Page)
        );
        assert_eq!(
            triple_creates["pages/deep/literal%5F%5F%5Fmarker.md"],
            ("literal___marker", ManagedTextKind::Page),
            "TripleLowbar decodes separators before percent escapes, preserving encoded literals"
        );
    }

    #[test]
    fn accepted_page_name_survives_filename_policy_reopen_while_new_pages_use_new_policy_clean() {
        let fixture = CleanSnapshotFixture::new_with_graph_config_names_and_contents(
            "accepted-page-name-policy-reopen",
            &["pages/A.B.md", "pages/referrer.md"],
            "{:file/name-format :legacy}\n",
            &["A/B", "Referrer"],
            &["old", "see [[A/B]]"],
        );
        let accepted_page_id = fixture.page_id(0);
        let referrer_page_id = fixture.page_id(1);
        let accepted_referrer = fixture.engine().materialize_page(referrer_page_id).unwrap();
        let referrer_block_id = accepted_referrer.blocks[0].block_id;

        fs::write(
            fixture.graph_root.join("logseq/config.edn"),
            "{:file/name-format :triple-lowbar}\n",
        )
        .unwrap();
        let mut fixture = fixture.reopen_after_config_change();
        fs::write(fixture.graph_root.join("pages/A.B.md"), b"- changed\n").unwrap();
        fs::write(fixture.graph_root.join("pages/New___Page.md"), b"- new\n").unwrap();

        let paths = ["pages/A.B.md", "pages/New___Page.md"];
        let plan = fixture.plan(&paths);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile);
        let operations = &plan.execution_material().unwrap().transaction().operations;
        assert!(
            !operations.iter().any(|operation| matches!(
                operation,
                SemanticOperation::ReconcileExternalPageState { page_id, .. }
                    if *page_id == accepted_page_id
            )),
            "an edit at an exactly owned path must not reinterpret its accepted logical name"
        );
        assert!(operations.iter().any(|operation| matches!(
            operation,
            SemanticOperation::CreatePage { name, path, kind, .. }
                if name.as_str() == "New/Page"
                    && path.as_str() == "pages/New___Page.md"
                    && *kind == ManagedTextKind::Page
        )));
        assert!(
            !operations.iter().any(|operation| matches!(
                operation,
                SemanticOperation::EditBlockContent { block, .. }
                    if block.block_id == referrer_block_id
            )),
            "preserving the accepted page name must leave existing textual referrers untouched"
        );
        assert_eq!(
            fixture
                .engine()
                .materialize_page(accepted_page_id)
                .unwrap()
                .name
                .as_str(),
            "A/B"
        );
        assert_eq!(accepted_referrer.blocks[0].content, "see [[A/B]]");

        let new_page_id = operations
            .iter()
            .find_map(|operation| match operation {
                SemanticOperation::CreatePage { page_id, path, .. }
                    if path.as_str() == "pages/New___Page.md" =>
                {
                    Some(*page_id)
                }
                _ => None,
            })
            .unwrap();
        fixture.apply_external_paths(&paths);
        let fixture = fixture.reopen_after_config_change();
        let accepted = fixture.engine().materialize_page(accepted_page_id).unwrap();
        assert_eq!(accepted.page_id, accepted_page_id);
        assert_eq!(accepted.name.as_str(), "A/B");
        assert_eq!(accepted.path.as_str(), "pages/A.B.md");
        assert_eq!(accepted.kind, ManagedTextKind::Page);
        let referrer = fixture.engine().materialize_page(referrer_page_id).unwrap();
        assert_eq!(referrer.page_id, referrer_page_id);
        assert_eq!(referrer.name.as_str(), "Referrer");
        assert_eq!(referrer.kind, ManagedTextKind::Page);
        assert_eq!(referrer.blocks[0].block_id, referrer_block_id);
        assert_eq!(referrer.blocks[0].content, "see [[A/B]]");
        let created = fixture.engine().materialize_page(new_page_id).unwrap();
        assert_eq!(created.page_id, new_page_id);
        assert_eq!(created.name.as_str(), "New/Page");
        assert_eq!(created.path.as_str(), "pages/New___Page.md");
        assert_eq!(created.kind, ManagedTextKind::Page);
    }

    #[test]
    fn accepted_journal_name_survives_journal_policy_reopen_while_new_journals_use_new_policy_clean(
    ) {
        let mut fixture = CleanSnapshotFixture::new_with_graph_config_names_and_contents(
            "accepted-journal-name-policy-reopen",
            &["pages/referrer.md"],
            "{:journal/file-name-format \"dd-MM-yyyy\"\n\
              :journal/page-title-format \"yyyy-MM-dd\"}\n",
            &["Referrer"],
            &["referrer"],
        );
        let referrer_page_id = fixture.page_id(0);
        fs::write(
            fixture.graph_root.join("journals/25-07-2026.md"),
            b"- old journal\n",
        )
        .unwrap();
        let initial_plan = fixture.plan(&["journals/25-07-2026.md"]);
        let initial_material = initial_plan.execution_material().unwrap();
        let initial_operations = &initial_material.transaction().operations;
        let accepted_page_id = initial_operations
            .iter()
            .find_map(|operation| match operation {
                SemanticOperation::CreatePage { page_id, .. } => Some(*page_id),
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!("old-policy journal was not created: {initial_operations:#?}")
            });
        assert!(
            initial_operations.iter().any(|operation| matches!(
                operation,
                SemanticOperation::CreatePage { name, kind, .. }
                    if name.as_str() == "2026-07-25" && *kind == ManagedTextKind::Journal
            )),
            "old-policy journal was not accepted as a journal: {initial_operations:#?}"
        );
        fixture.apply_external_paths(&["journals/25-07-2026.md"]);
        fs::write(
            fixture.graph_root.join("pages/referrer.md"),
            b"title:: Referrer\n\n- see [[2026-07-25]]\n",
        )
        .unwrap();
        fixture.apply_external_paths(&["pages/referrer.md"]);
        let accepted_referrer = fixture.engine().materialize_page(referrer_page_id).unwrap();
        let referrer_block_id = accepted_referrer.blocks[0].block_id;

        fs::write(
            fixture.graph_root.join("logseq/config.edn"),
            "{:journal/file-name-format \"MM~dd~yyyy\"\n\
              :journal/page-title-format \"yyyy-MM-dd\"}\n",
        )
        .unwrap();
        let mut fixture = fixture.reopen_after_config_change();
        fs::write(
            fixture.graph_root.join("journals/25-07-2026.md"),
            b"- changed journal\n",
        )
        .unwrap();
        fs::write(
            fixture.graph_root.join("journals/07~26~2026.md"),
            b"- new journal\n",
        )
        .unwrap();

        let paths = ["journals/25-07-2026.md", "journals/07~26~2026.md"];
        let plan = fixture.plan(&paths);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile);
        let operations = &plan.execution_material().unwrap().transaction().operations;
        assert!(
            !operations.iter().any(|operation| matches!(
                operation,
                SemanticOperation::ReconcileExternalPageState { page_id, .. }
                    if *page_id == accepted_page_id
            )),
            "an edit at an exactly owned journal path must not reinterpret its accepted name"
        );
        assert!(operations.iter().any(|operation| matches!(
            operation,
            SemanticOperation::CreatePage { name, path, kind, .. }
                if name.as_str() == "2026-07-26"
                    && path.as_str() == "journals/07~26~2026.md"
                    && *kind == ManagedTextKind::Journal
        )));
        assert!(
            !operations.iter().any(|operation| matches!(
                operation,
                SemanticOperation::EditBlockContent { block, .. }
                    if block.block_id == referrer_block_id
            )),
            "preserving the accepted journal name must leave existing textual referrers untouched"
        );
        assert_eq!(
            fixture
                .engine()
                .materialize_page(accepted_page_id)
                .unwrap()
                .name
                .as_str(),
            "2026-07-25"
        );
        assert_eq!(accepted_referrer.blocks[0].content, "see [[2026-07-25]]");

        let new_journal_id = operations
            .iter()
            .find_map(|operation| match operation {
                SemanticOperation::CreatePage { page_id, path, .. }
                    if path.as_str() == "journals/07~26~2026.md" =>
                {
                    Some(*page_id)
                }
                _ => None,
            })
            .unwrap();
        fixture.apply_external_paths(&paths);
        let fixture = fixture.reopen_after_config_change();
        let accepted = fixture.engine().materialize_page(accepted_page_id).unwrap();
        assert_eq!(accepted.page_id, accepted_page_id);
        assert_eq!(accepted.name.as_str(), "2026-07-25");
        assert_eq!(accepted.path.as_str(), "journals/25-07-2026.md");
        assert_eq!(accepted.kind, ManagedTextKind::Journal);
        let referrer = fixture.engine().materialize_page(referrer_page_id).unwrap();
        assert_eq!(referrer.page_id, referrer_page_id);
        assert_eq!(referrer.name.as_str(), "Referrer");
        assert_eq!(referrer.kind, ManagedTextKind::Page);
        assert_eq!(referrer.blocks[0].block_id, referrer_block_id);
        assert_eq!(referrer.blocks[0].content, "see [[2026-07-25]]");
        let created = fixture.engine().materialize_page(new_journal_id).unwrap();
        assert_eq!(created.page_id, new_journal_id);
        assert_eq!(created.name.as_str(), "2026-07-26");
        assert_eq!(created.path.as_str(), "journals/07~26~2026.md");
        assert_eq!(created.kind, ManagedTextKind::Journal);
    }

    #[test]
    fn unchanged_explicit_date_title_preserves_accepted_identity_across_journal_format_change_clean(
    ) {
        let fixture = CleanSnapshotFixture::new_with_graph_config_names_contents_and_preambles(
            "accepted-explicit-journal-title-policy-reopen",
            &["journals/physical.md"],
            "{:journal/file-name-format \"dd-MM-yyyy\"\n\
                  :journal/page-title-format \"yyyy-MM-dd\"}\n",
            &["2026-07-25"],
            &["old journal"],
            &["title:: 25-07-2026"],
        );
        let page_id = fixture.page_id(0);
        fs::write(
            fixture.graph_root.join("logseq/config.edn"),
            "{:journal/file-name-format \"dd-MM-yyyy\"\n\
              :journal/page-title-format \"EEEE, dd-MM-yyyy\"}\n",
        )
        .unwrap();
        let fixture = fixture.reopen_after_config_change();
        fs::write(
            fixture.graph_root.join("journals/physical.md"),
            b"title:: 25-07-2026\n\n- changed journal\n",
        )
        .unwrap();

        let plan = fixture.plan(&["journals/physical.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
        let operations = &plan.execution_material().unwrap().transaction().operations;
        assert!(
            !operations.iter().any(|operation| matches!(
                operation,
                SemanticOperation::ReconcileExternalPageState {
                    page_id: candidate,
                    ..
                } if *candidate == page_id
            )),
            "unchanged explicit title evidence must preserve the accepted name/kind despite a new journal renderer: {operations:#?}"
        );
        assert!(operations.iter().any(|operation| matches!(
            operation,
            SemanticOperation::EditBlockContent { content, .. }
                if content == "changed journal"
        )));
    }

    #[test]
    fn semantically_wrong_authenticated_current_path_identity_blocks_before_external_draft_clean() {
        let fixture = CleanSnapshotFixture::new_with_graph_config_names_and_contents(
            "semantically-wrong-current-path-identity",
            &["pages/accepted.md"],
            "{:file/name-format :legacy}\n",
            &["Accepted Name"],
            &["old"],
        );
        fs::write(
            fixture.graph_root.join("pages/accepted.md"),
            b"- external edit\n",
        )
        .unwrap();

        DERANGE_NEXT_CLEAN_PREDECESSOR_PATH.with(|derange| derange.set(true));
        let plan = fixture.plan(&["pages/accepted.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Blocked);
        assert_eq!(
            plan.blocks()[0].reason,
            ImportBlockReason::ConflictingLocalTail
        );
        assert!(
            plan.blocks()[0]
                .detail
                .contains("clean projection predecessor differs from current accepted page"),
            "{}",
            plan.blocks()[0].detail
        );
        assert!(matches!(
            plan.into_execution_material(),
            Err(ImportExecutionError::RefusedStatus(
                ImportPlanStatus::Blocked
            ))
        ));
    }

    #[test]
    fn external_title_rename_updates_accepted_owner_after_restart_without_rewriting_referrers_clean(
    ) {
        let mut fixture = CleanSnapshotFixture::new_with_graph_config_names_and_contents(
            "external-title-rename-referrers",
            &["pages/physical.md", "pages/referrer.md"],
            "{:file/name-format :legacy}\n",
            &["Old Logical", "Referrer"],
            &["target body", "see [[Old Logical]] and [[New Logical]]"],
        );
        let target_page_id = fixture.page_id(0);
        let referrer_page_id = fixture.page_id(1);
        let referrer_path = fixture.graph_root.join("pages/referrer.md");
        let referrer_bytes = fs::read(&referrer_path).unwrap();
        fs::write(
            fixture.graph_root.join("pages/physical.md"),
            b"title:: New Logical\n\n- target body\n",
        )
        .unwrap();

        let paths = ["pages/physical.md"];
        let plan = fixture.plan(&paths);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
        let operations = &plan.execution_material().unwrap().transaction().operations;
        assert!(operations.iter().any(|operation| matches!(
            operation,
            SemanticOperation::ReconcileExternalPageState {
                page_id,
                name,
                ..
            } if *page_id == target_page_id && name.as_str() == "New Logical"
        )));
        assert!(!operations.iter().any(|operation| matches!(
            operation,
            SemanticOperation::EditBlockContent { block, .. }
                if fixture
                    .engine()
                    .materialize_page(referrer_page_id)
                    .unwrap()
                    .blocks
                    .iter()
                    .any(|candidate| candidate.block_id == block.block_id)
        )));

        fixture.apply_external_paths(&paths);
        assert_eq!(fs::read(&referrer_path).unwrap(), referrer_bytes);
        let fixture = fixture.reopen_after_config_change();
        assert_eq!(
            fixture
                .engine()
                .current_page_for_logical_name(&LogicalPageName::parse("New Logical").unwrap())
                .unwrap(),
            Some(target_page_id)
        );
        assert_eq!(
            fixture
                .engine()
                .current_page_for_logical_name(&LogicalPageName::parse("Old Logical").unwrap())
                .unwrap(),
            None
        );
        assert_eq!(
            fixture
                .engine()
                .materialize_page(referrer_page_id)
                .unwrap()
                .blocks[0]
                .content,
            "see [[Old Logical]] and [[New Logical]]"
        );
        assert_eq!(
            Graph::open(&fixture.graph_root)
                .list_pages()
                .into_iter()
                .find(|entry| entry.rel_path == "pages/physical.md")
                .unwrap()
                .name,
            "New Logical"
        );
        assert_eq!(
            fixture.plan(&["pages/physical.md"]).status(),
            ImportPlanStatus::Noop,
            "projection receipt and restart must not oscillate the accepted title"
        );
    }

    #[test]
    fn configured_nested_managed_roots_use_graph_kind_and_filename_decoding_clean() {
        let fixture = CleanSnapshotFixture::new_with_graph_config(
            "configured-nested-roots",
            &["content/pages/seed.md"],
            "{:pages-directory \"content/pages\"\n\
              :journals-directory \"content/journals\"\n\
              :journal/file-name-format \"dd-MM-yyyy\"\n\
              :journal/page-title-format \"yyyy-MM-dd\"}\n",
        );
        for path in [
            "content/pages/deep/Project%2FPlan.md",
            "content/journals/archive/deep/25-07-2026.md",
        ] {
            let target = fixture.graph_root.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, b"- external\n").unwrap();
        }
        let plan = fixture.plan(&[
            "content/pages/deep/Project%2FPlan.md",
            "content/journals/archive/deep/25-07-2026.md",
        ]);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile);
        let created = plan
            .execution_material()
            .unwrap()
            .transaction()
            .operations
            .iter()
            .filter_map(|operation| match operation {
                SemanticOperation::CreatePage {
                    path, name, kind, ..
                } => Some((path.as_str(), (name.as_str(), *kind))),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            created["content/pages/deep/Project%2FPlan.md"],
            ("Project/Plan", ManagedTextKind::Page)
        );
        assert_eq!(
            created["content/journals/archive/deep/25-07-2026.md"],
            ("2026-07-25", ManagedTextKind::Journal)
        );
    }

    #[test]
    fn exact_rename_adopts_graph_decoded_destination_name_before_authoring_clean() {
        let fixture = CleanSnapshotFixture::new_with_initial_uuid_and_config(
            "rename-destination-name",
            &["pages/old.md"],
            None,
            None,
            Some(&["old"]),
            None,
            None,
        );
        let destination = fixture.graph_root.join("pages/Project%2FPlan.md");
        fs::rename(fixture.graph_root.join("pages/old.md"), &destination).unwrap();
        let plan = fixture.plan(&["pages/old.md", "pages/Project%2FPlan.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile);
        assert!(plan
            .execution_material()
            .unwrap()
            .transaction()
            .operations
            .iter()
            .any(|operation| matches!(
                operation,
                SemanticOperation::ReconcileExternalPageState { name, path, .. }
                    if name.as_str() == "Project/Plan" && path.as_str() == "pages/Project%2FPlan.md"
            )));
    }

    #[test]
    fn sealed_external_execution_drafts_through_the_engine_in_parent_before_child_order_clean() {
        let fixture = CleanSnapshotFixture::new_with_initial_uuid_and_config(
            "external-engine-draft",
            &["pages/old.md"],
            None,
            None,
            Some(&["old"]),
            None,
            None,
        );
        let destination = fixture.graph_root.join("pages/Project%2FPlan.md");
        fs::rename(fixture.graph_root.join("pages/old.md"), &destination).unwrap();
        fs::write(
            fixture.graph_root.join("pages/new.md"),
            b"- parent\n\t- child\n",
        )
        .unwrap();

        let plan = fixture.plan(&["pages/old.md", "pages/Project%2FPlan.md", "pages/new.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile);
        let material = plan.into_execution_material().unwrap();
        let operations = &material.transaction().operations;
        assert!(operations.iter().any(|operation| matches!(
            operation,
            SemanticOperation::ReconcileExternalPageState { name, path, .. }
                if name.as_str() == "Project/Plan" && path.as_str() == "pages/Project%2FPlan.md"
        )));

        let created = operations
            .iter()
            .enumerate()
            .filter_map(|(index, operation)| match operation {
                SemanticOperation::CreateBlock { block, parent, .. } => {
                    Some((index, block.block_id, *parent))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let (child_index, _, parent) = created
            .iter()
            .copied()
            .find(|(_, _, parent)| parent.is_some())
            .expect("new nested child must be created");
        let parent = parent.expect("selected child has a parent");
        let parent_index = created
            .iter()
            .find_map(|(index, block_id, _)| (*block_id == parent).then_some(*index))
            .expect("new child parent must be created in this transaction");
        assert!(parent_index < child_index);

        fixture
            .engine()
            .draft_external_import_transaction(
                AuthorBatch {
                    batch_id: material.batch_id(),
                    author_device_id: DeviceId::from_uuid(Uuid::from_u128(3)),
                    author_session_id: SessionId::from_uuid(Uuid::from_u128(9_412)),
                    crdt_peer_id: CrdtPeerId::from_u64(9_413),
                },
                material,
            )
            .unwrap();

        let prior = LogseqUuid::from_uuid(Uuid::from_u128(9_410));
        let replacement = LogseqUuid::from_uuid(Uuid::from_u128(9_411));
        let mut replacement_fixture =
            CleanSnapshotFixture::new("external-engine-id-replacement", &["pages/a.md"]);
        fs::write(
            replacement_fixture.graph_root.join("pages/a.md"),
            format!("- page 0\n  id:: {prior}\n"),
        )
        .unwrap();
        replacement_fixture.apply_external_paths(&["pages/a.md"]);
        fs::write(
            replacement_fixture.graph_root.join("pages/a.md"),
            format!("- page 0\n  id:: {replacement}\n"),
        )
        .unwrap();
        let replacement_material = replacement_fixture
            .plan(&["pages/a.md"])
            .into_execution_material()
            .unwrap();
        assert!(replacement_material
            .transaction()
            .operations
            .iter()
            .any(|operation| matches!(
                operation,
                SemanticOperation::MutateBlockLogseqIdentity {
                    mutation: LogseqIdentityMutation::ReplaceExternal { logseq_uuid },
                    ..
                } if *logseq_uuid == replacement
            )));
        replacement_fixture
            .engine()
            .draft_external_import_transaction(
                AuthorBatch {
                    batch_id: replacement_material.batch_id(),
                    author_device_id: DeviceId::from_uuid(Uuid::from_u128(3)),
                    author_session_id: SessionId::from_uuid(Uuid::from_u128(9_414)),
                    crdt_peer_id: CrdtPeerId::from_u64(9_415),
                },
                replacement_material,
            )
            .unwrap();

        let mut removal_fixture =
            CleanSnapshotFixture::new("external-engine-id-removal", &["pages/a.md"]);
        fs::write(
            removal_fixture.graph_root.join("pages/a.md"),
            format!("- page 0\n  id:: {prior}\n"),
        )
        .unwrap();
        removal_fixture.apply_external_paths(&["pages/a.md"]);
        fs::write(removal_fixture.graph_root.join("pages/a.md"), b"- page 0\n").unwrap();
        let removal_material = removal_fixture
            .plan(&["pages/a.md"])
            .into_execution_material()
            .unwrap();
        assert!(removal_material
            .transaction()
            .operations
            .iter()
            .any(|operation| matches!(
                operation,
                SemanticOperation::MutateBlockLogseqIdentity {
                    mutation: LogseqIdentityMutation::RemoveExternal,
                    ..
                }
            )));
        removal_fixture
            .engine()
            .draft_external_import_transaction(
                AuthorBatch {
                    batch_id: removal_material.batch_id(),
                    author_device_id: DeviceId::from_uuid(Uuid::from_u128(3)),
                    author_session_id: SessionId::from_uuid(Uuid::from_u128(9_416)),
                    crdt_peer_id: CrdtPeerId::from_u64(9_417),
                },
                removal_material,
            )
            .unwrap();
    }

    #[test]
    fn atomic_page_name_transition_allows_chains_deletion_reuse_and_cycles_clean() {
        let chain = CleanSnapshotFixture::new("name-chain", &["pages/a.md", "pages/b.md"]);
        fs::rename(
            chain.graph_root.join("pages/a.md"),
            chain.graph_root.join("pages/Snapshot%20Page%201.md"),
        )
        .unwrap();
        fs::rename(
            chain.graph_root.join("pages/b.md"),
            chain.graph_root.join("pages/final.md"),
        )
        .unwrap();
        let chain_plan = chain.plan(&[
            "pages/a.md",
            "pages/b.md",
            "pages/Snapshot%20Page%201.md",
            "pages/final.md",
        ]);
        assert_eq!(chain_plan.status(), ImportPlanStatus::Reconcile);
        let chain_material = chain_plan.into_execution_material().unwrap();
        let chain_renames = chain_material
            .transaction()
            .operations
            .iter()
            .filter_map(|operation| match operation {
                SemanticOperation::ReconcileExternalPageState {
                    page_id,
                    name,
                    path,
                    ..
                } => Some((*page_id, name.as_str(), path.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(chain_renames.len(), 2);
        assert!(chain_renames
            .iter()
            .any(|(_, name, _)| *name == "Snapshot Page 1"));
        chain
            .engine()
            .draft_external_import_transaction(
                AuthorBatch {
                    batch_id: chain_material.batch_id(),
                    author_device_id: DeviceId::from_uuid(Uuid::from_u128(3)),
                    author_session_id: SessionId::from_uuid(Uuid::from_u128(9_420)),
                    crdt_peer_id: CrdtPeerId::from_u64(9_421),
                },
                chain_material,
            )
            .unwrap();

        let reuse = CleanSnapshotFixture::new("delete-name-reuse", &["pages/a.md", "pages/b.md"]);
        fs::remove_file(reuse.graph_root.join("pages/b.md")).unwrap();
        fs::write(
            reuse.graph_root.join("pages/Snapshot%20Page%201.md"),
            b"- replacement identity\n",
        )
        .unwrap();
        let reuse_plan = reuse.plan(&["pages/b.md", "pages/Snapshot%20Page%201.md"]);
        assert_eq!(reuse_plan.status(), ImportPlanStatus::Reconcile);
        let reuse_material = reuse_plan.into_execution_material().unwrap();
        assert!(reuse_material.transaction().operations.iter().any(
            |operation| matches!(operation, SemanticOperation::CreatePage { name, .. }
                if name.as_str() == "Snapshot Page 1")
        ));
        assert!(reuse_material
            .transaction()
            .operations
            .iter()
            .any(|operation| matches!(operation, SemanticOperation::DeletePage { .. })));
        reuse
            .engine()
            .draft_external_import_transaction(
                AuthorBatch {
                    batch_id: reuse_material.batch_id(),
                    author_device_id: DeviceId::from_uuid(Uuid::from_u128(3)),
                    author_session_id: SessionId::from_uuid(Uuid::from_u128(9_422)),
                    crdt_peer_id: CrdtPeerId::from_u64(9_423),
                },
                reuse_material,
            )
            .unwrap();

        let cycle = CleanSnapshotFixture::new("name-cycle", &["pages/a.md", "pages/b.md"]);
        let temporary = cycle.graph_root.join("pages/cycle.tmp");
        fs::rename(cycle.graph_root.join("pages/a.md"), &temporary).unwrap();
        fs::rename(
            cycle.graph_root.join("pages/b.md"),
            cycle.graph_root.join("pages/Snapshot%20Page%200.md"),
        )
        .unwrap();
        fs::rename(
            temporary,
            cycle.graph_root.join("pages/Snapshot%20Page%201.md"),
        )
        .unwrap();
        let cycle_plan = cycle.plan(&[
            "pages/a.md",
            "pages/b.md",
            "pages/Snapshot%20Page%200.md",
            "pages/Snapshot%20Page%201.md",
        ]);
        assert_eq!(cycle_plan.status(), ImportPlanStatus::Reconcile);
        let cycle_material = cycle_plan.into_execution_material().unwrap();
        cycle
            .engine()
            .draft_external_import_transaction(
                AuthorBatch {
                    batch_id: cycle_material.batch_id(),
                    author_device_id: DeviceId::from_uuid(Uuid::from_u128(3)),
                    author_session_id: SessionId::from_uuid(Uuid::from_u128(9_424)),
                    crdt_peer_id: CrdtPeerId::from_u64(9_425),
                },
                cycle_material,
            )
            .unwrap();
    }

    #[test]
    fn external_observation_annotations_use_each_nested_block_exact_byte_span_clean() {
        let fixture = CleanSnapshotFixture::new("nested-exact-spans", &["pages/a.md"]);
        let bytes = b"- parent\n\t- child\n";
        fs::write(fixture.graph_root.join("pages/a.md"), bytes).unwrap();
        let plan = fixture.plan(&["pages/a.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile);
        let entry = &plan.execution_material().unwrap().observation().entries()[0];
        let spans = entry
            .state()
            .annotations()
            .iter()
            .map(|annotation| (annotation.span().start(), annotation.span().end()))
            .collect::<Vec<_>>();
        assert_eq!(spans, vec![(0, 9), (9, bytes.len() as u64)]);
    }

    #[test]
    fn sparse_observation_accepts_promoted_heading_spans_and_locators_in_source_order_clean() {
        let fixture = CleanSnapshotFixture::new("promoted-heading-sparse-spans", &["pages/a.md"]);
        let bytes = b"# Project\n\t- child one\n\t- child two\n- sibling\n\t- nested sibling child";
        fs::write(fixture.graph_root.join("pages/a.md"), bytes).unwrap();
        let plan = fixture.plan(&["pages/a.md"]);
        assert_eq!(plan.status(), ImportPlanStatus::Reconcile);
        let annotations = plan.execution_material().unwrap().observation().entries()[0]
            .state()
            .annotations();
        let starts = [
            b"# Project".as_slice(),
            b"\t- child one".as_slice(),
            b"\t- child two".as_slice(),
            b"- sibling".as_slice(),
            b"\t- nested sibling child".as_slice(),
        ]
        .map(|needle| {
            bytes
                .windows(needle.len())
                .position(|window| window == needle)
                .unwrap() as u64
        });
        assert_eq!(
            annotations
                .iter()
                .map(|annotation| (annotation.span().start(), annotation.span().end()))
                .collect::<Vec<_>>(),
            vec![
                (starts[0], starts[1]),
                (starts[1], starts[2]),
                (starts[2], starts[3]),
                (starts[3], starts[4]),
                (starts[4], bytes.len() as u64),
            ]
        );
        assert_eq!(
            annotations
                .iter()
                .map(|annotation| annotation.locator().components().to_vec())
                .collect::<Vec<_>>(),
            vec![vec![0], vec![0, 0], vec![0, 1], vec![1], vec![1, 0]]
        );
    }

    #[test]
    fn affected_import_never_scans_unrelated_pages_for_home_documents_clean() {
        let paths = (0..32)
            .map(|index| format!("pages/unrelated/{index:02}.md"))
            .collect::<Vec<_>>();
        let path_refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let fixture = CleanSnapshotFixture::new("affected-home-scope", &path_refs);
        fs::write(
            fixture.graph_root.join("pages/unrelated/00.md"),
            b"- externally changed\n",
        )
        .unwrap();

        let plan = fixture.plan(&["pages/unrelated/00.md"]);

        assert_eq!(plan.status(), ImportPlanStatus::Reconcile);
        assert_eq!(plan.instrumentation().catalog_path_lookups, 1);
    }

    #[test]
    fn parser_owned_spans_cover_literal_regions_crlf_preambles_and_multiple_roots() {
        fn offsets(text: &[u8], needles: &[&[u8]]) -> Vec<u64> {
            needles
                .iter()
                .map(|needle| {
                    text.windows(needle.len())
                        .position(|window| window == *needle)
                        .unwrap() as u64
                })
                .collect()
        }

        let markdown = b"title:: Page\r\n\r\n- parent\r\n  continuation\r\n  ```\r\n  - literal\r\n  ```\r\n  - child\r\n- root two\r\n";
        let markdown_path = ManagedPath::parse("pages/literal.md").unwrap();
        let mut instrumentation = ImportInstrumentation::default();
        let tree = parse_nodes(&markdown_path, markdown, &mut instrumentation).unwrap();
        let starts = offsets(markdown, &[b"- parent", b"  - child", b"- root two"]);
        assert_eq!(tree.nodes.len(), 3);
        assert_eq!(
            tree.nodes
                .iter()
                .map(|node| (node.span.start(), node.span.end()))
                .collect::<Vec<_>>(),
            vec![
                (starts[0], starts[1]),
                (starts[1], starts[2]),
                (starts[2], markdown.len() as u64),
            ]
        );
        assert!(tree.nodes[0].raw.contains("- literal"));
        assert_eq!(tree.nodes[0].children, vec![1]);
        assert_eq!(tree.roots, vec![0, 2]);

        let org = b"#+TITLE: Page\r\n* parent\r\n#+BEGIN_SRC\r\n* literal\r\n#+END_SRC\r\n** child\r\n* root two\r\n";
        let org_path = ManagedPath::parse("pages/literal.org").unwrap();
        let mut instrumentation = ImportInstrumentation::default();
        let tree = parse_nodes(&org_path, org, &mut instrumentation).unwrap();
        let starts = offsets(org, &[b"* parent", b"** child", b"* root two"]);
        assert_eq!(tree.nodes.len(), 3);
        assert_eq!(
            tree.nodes
                .iter()
                .map(|node| (node.span.start(), node.span.end()))
                .collect::<Vec<_>>(),
            vec![
                (starts[0], starts[1]),
                (starts[1], starts[2]),
                (starts[2], org.len() as u64),
            ]
        );
        assert!(tree.nodes[0].raw.contains("* literal"));
        assert_eq!(tree.nodes[0].children, vec![1]);
        assert_eq!(tree.roots, vec![0, 2]);
    }

    #[test]
    fn import_admission_reuses_the_parser_owned_document() {
        let markdown_path = ManagedPath::parse("pages/reused.md").unwrap();
        let mut instrumentation = ImportInstrumentation::default();
        crate::outline::reset_parse_attempts();
        parse_nodes(
            &markdown_path,
            b"- parent\n  - child\n",
            &mut instrumentation,
        )
        .unwrap();
        assert_eq!(
            crate::outline::parse_attempts(),
            1,
            "Markdown admission retains exact source bytes and reuses the original parse; non-round-tripping syntax is exposed read-only"
        );

        let org_path = ManagedPath::parse("pages/reused.org").unwrap();
        let mut instrumentation = ImportInstrumentation::default();
        crate::outline::reset_parse_attempts();
        parse_nodes(&org_path, b"* parent\n** child\n", &mut instrumentation).unwrap();
        assert_eq!(
            crate::outline::parse_attempts(),
            1,
            "Org admission reproduces bytes from the original parse"
        );

        let root = TestRoot::new("activation-capture-single-parse");
        fs::write(
            root.path().join("graph/pages/captured.md"),
            b"- parent\n  - child\n",
        )
        .unwrap();
        fs::write(
            root.path().join("graph/pages/captured.org"),
            b"* parent\n** child\n",
        )
        .unwrap();
        let graph = Graph::open(&root.path().join("graph"));
        let markdown_path = ManagedPath::parse("pages/captured.md").unwrap();
        crate::outline::reset_parse_attempts();
        capture_activation_page_record(&graph, &markdown_path, b"- parent\n  - child\n").unwrap();
        assert_eq!(
            crate::outline::parse_attempts(),
            1,
            "activation capture must not spend a second parser pass on a round-trip admission check"
        );

        let org_path = ManagedPath::parse("pages/captured.org").unwrap();
        crate::outline::reset_parse_attempts();
        capture_activation_page_record(&graph, &org_path, b"* parent\n** child\n").unwrap();
        assert_eq!(
            crate::outline::parse_attempts(),
            1,
            "Org activation capture must retain the same one-pass parser budget"
        );
    }

    #[test]
    fn collapsed_heading_and_flat_bullets_have_exact_parser_owned_sibling_spans() {
        let bytes =
            b"page:: property\r\n\r\n# Collapsed\r\ncollapsed:: true\r\n- child\r\n- sibling\r\n";
        let path = ManagedPath::parse("pages/promoted.md").unwrap();
        let mut instrumentation = ImportInstrumentation::default();
        let tree = parse_nodes(&path, bytes, &mut instrumentation).unwrap();
        let heading = bytes
            .windows(b"# Collapsed".len())
            .position(|window| window == b"# Collapsed")
            .unwrap() as u64;
        let child = bytes
            .windows(b"- child".len())
            .position(|window| window == b"- child")
            .unwrap() as u64;
        let sibling = bytes
            .windows(b"- sibling".len())
            .position(|window| window == b"- sibling")
            .unwrap() as u64;
        assert_eq!(
            tree.nodes
                .iter()
                .map(|node| (node.span.start(), node.span.end()))
                .collect::<Vec<_>>(),
            vec![
                (heading, child),
                (child, sibling),
                (sibling, bytes.len() as u64),
            ]
        );
        assert_eq!(tree.roots, vec![0, 1, 2]);
        assert!(tree.nodes.iter().all(|node| node.children.is_empty()));
    }

    #[test]
    fn lsdoc_promoted_heading_nested_run_has_exact_crlf_parser_owned_spans() {
        let bytes = b"title:: Synthetic\r\n\r\n# Project \xce\xa9\r\n\t- child one\r\n\t- child two\r\n- sibling\r\n\t- nested sibling child\r\n";
        let path = ManagedPath::parse("pages/promoted-nested.md").unwrap();
        let mut instrumentation = ImportInstrumentation::default();
        let tree = parse_nodes(&path, bytes, &mut instrumentation).unwrap();
        let starts = [
            b"# Project \xce\xa9".as_slice(),
            b"\t- child one".as_slice(),
            b"\t- child two".as_slice(),
            b"- sibling".as_slice(),
            b"\t- nested sibling child".as_slice(),
        ]
        .map(|needle| {
            bytes
                .windows(needle.len())
                .position(|window| window == needle)
                .unwrap() as u64
        });
        assert_eq!(
            tree.nodes
                .iter()
                .map(|node| (node.span.start(), node.span.end()))
                .collect::<Vec<_>>(),
            vec![
                (starts[0], starts[1]),
                (starts[1], starts[2]),
                (starts[2], starts[3]),
                (starts[3], starts[4]),
                (starts[4], bytes.len() as u64),
            ]
        );
        assert_eq!(tree.roots, vec![0, 3]);
        assert_eq!(tree.nodes[0].children, vec![1, 2]);
        assert_eq!(tree.nodes[3].children, vec![4]);
    }

    #[test]
    fn aggregate_budget_refuses_before_overflow_or_allocation() {
        assert_eq!(
            charge_budget(
                "aggregate raw bytes",
                MAX_IMPORT_RAW_BYTES - 1,
                1,
                MAX_IMPORT_RAW_BYTES
            )
            .unwrap(),
            MAX_IMPORT_RAW_BYTES
        );
        assert!(matches!(
            charge_budget(
                "aggregate raw bytes",
                MAX_IMPORT_RAW_BYTES,
                1,
                MAX_IMPORT_RAW_BYTES
            ),
            Err(InventoryError::ResourceBudgetExceeded {
                resource: "aggregate raw bytes",
                ..
            })
        ));
        assert!(charge_budget("aggregate raw bytes", u64::MAX, 1, MAX_IMPORT_RAW_BYTES).is_err());

        let path = ManagedPath::parse("pages/a.md").unwrap();
        let parsed = crate::doc::try_parse_with_source_spans("- one more\n").unwrap();
        assert_eq!(
            enforce_outline_limits(&path, &parsed, MAX_IMPORT_PARSED_NODES)
                .unwrap_err()
                .reason,
            ImportBlockReason::ResourceLimit
        );
        let tree = ParsedTree {
            path,
            preamble: None,
            roots: vec![0],
            nodes: vec![ParsedNode {
                parent: None,
                sibling_position: 0,
                depth: 1,
                children: Vec::new(),
                span: StructuralSpan::new(0, 0).unwrap(),
                raw: "node".into(),
                raw_ids: Vec::new(),
                projection_facets: ParsedBlockProjectionFacets::default(),
            }],
        };
        let mut instrumentation = ImportInstrumentation {
            locator_components_materialized: MAX_IMPORT_LOCATOR_COMPONENTS,
            ..ImportInstrumentation::default()
        };
        assert_eq!(
            materialize_locator(&tree, 0, &mut instrumentation)
                .unwrap_err()
                .reason,
            ImportBlockReason::ResourceLimit
        );

        let mut replay = ImportInstrumentation::default();
        let replay_limits = ImportReplayLimits {
            entries: 2,
            base_bytes: 8,
            rendered_bytes: 8,
        };
        let path = ManagedPath::parse("pages/replay.md").unwrap();
        reserve_base_replay(&mut replay, 4, replay_limits, &path).unwrap();
        reserve_base_replay(&mut replay, 4, replay_limits, &path).unwrap();
        assert_eq!(
            reserve_base_replay(&mut replay, 0, replay_limits, &path)
                .unwrap_err()
                .reason,
            ImportBlockReason::ResourceLimit
        );
        retain_rendered_target(&mut replay, 8, replay_limits, &path).unwrap();
        assert_eq!(
            retain_rendered_target(&mut replay, 1, replay_limits, &path)
                .unwrap_err()
                .reason,
            ImportBlockReason::ResourceLimit
        );
    }

    #[test]
    fn operation_count_bound_refuses_the_100001st_operation_before_publication() {
        let mut operations = Vec::with_capacity(MAX_TRANSACTION_OPERATIONS);
        for index in 0..MAX_TRANSACTION_OPERATIONS {
            push_operation(
                &mut operations,
                SemanticOperation::DeletePage {
                    page_id: PageId::from_uuid(Uuid::from_u128(index as u128 + 1)),
                },
            )
            .unwrap();
        }
        assert_eq!(operations.len(), MAX_TRANSACTION_OPERATIONS);
        assert!(matches!(
            push_operation(
                &mut operations,
                SemanticOperation::DeletePage {
                    page_id: PageId::from_uuid(Uuid::from_u128(100_001)),
                },
            ),
            Err(ImportExecutionError::OperationLimit)
        ));
    }

    #[derive(Debug, Eq, PartialEq)]
    struct BootstrapConstructionShape {
        pages: usize,
        parts: u32,
        page_part_touches: usize,
        publication_durability_syncs: usize,
    }

    #[test]
    fn structural_common_prefix_work_and_repeated_deep_locators_are_charged() {
        let path = ManagedPath::parse("pages/structural.md").unwrap();
        let mut text = String::new();
        for _ in 0..64 {
            text.push_str("- parent\n");
            for _ in 0..16 {
                text.push_str("\t- same child\n");
            }
        }
        let mut instrumentation = ImportInstrumentation::default();
        let tree = parse_nodes(&path, text.as_bytes(), &mut instrumentation).unwrap();
        let mut interner = StructuralInterner::new();
        structural_classes(&tree, &mut interner, &mut instrumentation).unwrap();
        structural_classes(&tree, &mut interner, &mut instrumentation).unwrap();
        assert!(instrumentation.structural_key_components >= tree.nodes.len() * 2);
        assert!(instrumentation.structural_key_comparisons > tree.nodes.len());

        let mut nodes = Vec::new();
        for depth in 1..=MAX_IMPORT_DEPTH {
            nodes.push(ParsedNode {
                parent: if depth > 1 { Some(depth - 2) } else { None },
                sibling_position: 0,
                depth,
                children: (depth < MAX_IMPORT_DEPTH)
                    .then_some(depth)
                    .into_iter()
                    .collect(),
                span: StructuralSpan::new(0, 0).unwrap(),
                raw: "node".into(),
                raw_ids: vec!["duplicate".into(), "duplicate".into()],
                projection_facets: ParsedBlockProjectionFacets::default(),
            });
        }
        let deep = ParsedTree {
            path,
            preamble: None,
            roots: vec![0],
            nodes,
        };
        let before = instrumentation.locator_components_materialized;
        materialize_locator(&deep, MAX_IMPORT_DEPTH - 1, &mut instrumentation).unwrap();
        materialize_locator(&deep, MAX_IMPORT_DEPTH - 1, &mut instrumentation).unwrap();
        assert_eq!(
            instrumentation.locator_components_materialized - before,
            MAX_IMPORT_DEPTH * 2
        );
    }

    #[test]
    fn structural_class_allocation_work_is_linear_across_many_pages() {
        fn measured(page_count: usize) -> ImportInstrumentation {
            let mut interner = StructuralInterner::new();
            let mut instrumentation = ImportInstrumentation::default();
            for index in 0..page_count {
                let tree = ParsedTree {
                    path: ManagedPath::parse(&format!("pages/p{index:08}.md")).unwrap(),
                    preamble: None,
                    roots: vec![0],
                    nodes: vec![ParsedNode {
                        parent: None,
                        sibling_position: 0,
                        depth: 1,
                        children: Vec::new(),
                        span: StructuralSpan::new(0, 0).unwrap(),
                        raw: format!("unique-{index:08}"),
                        raw_ids: Vec::new(),
                        projection_facets: ParsedBlockProjectionFacets::default(),
                    }],
                };
                structural_classes(&tree, &mut interner, &mut instrumentation).unwrap();
                instrumentation.structural_class_nodes = instrumentation
                    .structural_class_nodes
                    .saturating_add(tree.nodes.len());
            }
            instrumentation
        }

        let small = measured(1_024);
        let large = measured(8_192);
        assert_eq!(small.structural_class_allocations, 1_024);
        assert_eq!(large.structural_class_allocations, 8_192);
        assert_eq!(small.structural_key_comparisons, 0);
        assert_eq!(large.structural_key_comparisons, 0);
        assert!(
            large.recorded_work_units() <= small.recorded_work_units().saturating_mul(8),
            "structural work did not scale linearly: small={small:?}, large={large:?}"
        );
    }
}
