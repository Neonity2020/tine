use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
#[cfg(test)]
use std::time::Instant;

use loro::{LoroDoc, VersionVector};
use serde::{Deserialize, Serialize};

use super::loro_store::{
    AuthenticatedLoroStore, LoroHistoryWitness, LoroStoreRoot, LoroStoreStats,
};
use super::scratch_store::{
    ScratchBlobRef, ScratchLsmRoot, ScratchPageKind, ScratchRoots, ScratchStore,
};
use super::{
    BatchId, ContentDigest, CrdtPeerCounter, CrdtPeerId, DocumentCausalDigest,
    DocumentDependencies, DocumentId,
};

const DOCUMENT_STATE_SCHEMA_VERSION: u32 = 1;
const EXTERNAL_DOCUMENT_STATE_SCHEMA_VERSION: u32 = 1;
const CHECKPOINT_CHUNK_BYTES: usize = 64 * 1024;
const CHECKPOINT_BLOB_DEDUP_FLUSH_CAP: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum DocumentLane {
    Visible = 0,
    Terminal = 1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobTree {
    schema_version: u32,
    encoded_len: u64,
    digest: ContentDigest,
    chunks: Vec<ScratchBlobRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalDocumentStateRecord {
    schema_version: u32,
    document_id: DocumentId,
    lane: DocumentLane,
    causal_digest: DocumentCausalDigest,
    state_checkpoint: BlobTree,
    history_root: LoroStoreRoot,
    peer_counters: Vec<CrdtPeerCounter>,
    exact_direct_heads: Vec<BatchId>,
    latest_source_batch: BatchId,
    latest_manifest_fingerprint: ContentDigest,
    latest_update_digest: ContentDigest,
}

impl ExternalDocumentStateRecord {
    #[cfg(test)]
    pub(crate) fn history_root(&self) -> &LoroStoreRoot {
        &self.history_root
    }

    pub(crate) fn peer_counters(&self) -> &[CrdtPeerCounter] {
        &self.peer_counters
    }

    pub(crate) fn exact_direct_heads(&self) -> &[BatchId] {
        &self.exact_direct_heads
    }

    pub(crate) const fn latest_source_batch(&self) -> BatchId {
        self.latest_source_batch
    }

    pub(crate) const fn latest_manifest_fingerprint(&self) -> ContentDigest {
        self.latest_manifest_fingerprint
    }

    pub(crate) const fn latest_update_digest(&self) -> ContentDigest {
        self.latest_update_digest
    }
}

#[derive(Debug)]
pub(crate) struct ExternalDocument {
    document: LoroDoc,
    history_store: AuthenticatedLoroStore,
}

impl ExternalDocument {
    pub(crate) fn empty(store: Arc<ScratchStore>) -> Result<Self, DocumentStateError> {
        let history_store = AuthenticatedLoroStore::empty(store);
        let document = LoroDoc::from_external_store(None, history_store.handle())
            .map_err(|error| DocumentStateError::InvalidCrdt(error.to_string()))?;
        Ok(Self {
            document,
            history_store,
        })
    }

    pub(crate) const fn document(&self) -> &LoroDoc {
        &self.document
    }

    pub(crate) fn into_document(self) -> LoroDoc {
        self.document
    }

    #[cfg(test)]
    fn store_stats(&self) -> LoroStoreStats {
        self.history_store.stats()
    }

    #[cfg(test)]
    pub(crate) fn poison_store_for_test(&self, message: &str) {
        self.history_store.poison_for_test(message);
    }
}

/// Opaque proof that one exact document checkpoint was authenticated against
/// the scratch point root and its immutable Loro-history witness.
///
/// The record and document deliberately cannot be separated by callers.  This
/// is the trust-bearing input used by derived authenticated indexes that need
/// to inspect bounded values at an exact causal frontier.
pub(crate) struct AuthenticatedExternalExactCheckpoint {
    document_id: DocumentId,
    causal_digest: DocumentCausalDigest,
    peer_counters: Vec<CrdtPeerCounter>,
    exact_direct_heads: Vec<BatchId>,
    document: ExternalDocument,
    state_work: DocumentStateWork,
    checkpoint_binding: ContentDigest,
    checkpoint_content_digest: ContentDigest,
    archive_anchor: Option<(BatchId, ContentDigest, ContentDigest)>,
}

impl AuthenticatedExternalExactCheckpoint {
    pub(crate) const fn document_id(&self) -> DocumentId {
        self.document_id
    }

    pub(crate) const fn causal_digest(&self) -> DocumentCausalDigest {
        self.causal_digest
    }

    pub(crate) fn peer_counters(&self) -> &[CrdtPeerCounter] {
        &self.peer_counters
    }

    pub(crate) fn exact_direct_heads(&self) -> &[BatchId] {
        &self.exact_direct_heads
    }

    pub(crate) const fn checkpoint_binding(&self) -> ContentDigest {
        self.checkpoint_binding
    }

    pub(crate) const fn checkpoint_content_digest(&self) -> ContentDigest {
        self.checkpoint_content_digest
    }

    pub(crate) const fn archive_anchor(&self) -> Option<(BatchId, ContentDigest, ContentDigest)> {
        self.archive_anchor
    }

    pub(crate) const fn document(&self) -> &LoroDoc {
        self.document.document()
    }

    /// Consume the trust-bearing checkpoint only after all derived evidence
    /// has been extracted, preserving the load work that must be charged by
    /// the caller retaining the authenticated document state.
    pub(crate) fn into_external_document_with_work(self) -> (ExternalDocument, DocumentStateWork) {
        (self.document, self.state_work)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DocumentStateWork {
    pub document_point_reads: usize,
    pub state_page_bytes_read: usize,
    pub state_page_bytes_written: usize,
    pub external_flushes: usize,
    pub external_point_reads: usize,
    pub external_range_scans: usize,
    pub external_history_page_reads: usize,
    pub external_history_blob_reads: usize,
    #[cfg(test)]
    pub external_checkpoint_phase_calls: usize,
    #[cfg(test)]
    pub external_checkpoint_nonempty_calls: usize,
    #[cfg(test)]
    pub external_checkpoint_documents: usize,
    #[cfg(test)]
    pub checkpoint_chunks: usize,
    #[cfg(test)]
    pub checkpoint_existing_hits: usize,
    #[cfg(test)]
    pub checkpoint_staged_hits: usize,
    #[cfg(test)]
    pub checkpoint_new_chunks: usize,
    #[cfg(test)]
    pub blob_dedup_lsm_flushes: usize,
    #[cfg(test)]
    pub blob_dedup_lsm_insert_nanos: u128,
    #[cfg(test)]
    pub checkpoint_chunk_digest_nanos: u128,
    #[cfg(test)]
    pub blob_dedup_lookup_nanos: u128,
    #[cfg(test)]
    pub checkpoint_blob_append_nanos: u128,
    #[cfg(test)]
    pub checkpoint_whole_digest_nanos: u128,
    #[cfg(test)]
    pub loro_external_flush_nanos: u128,
    #[cfg(test)]
    pub state_checkpoint_export_nanos: u128,
    #[cfg(test)]
    pub external_witness_root_nanos: u128,
    #[cfg(test)]
    pub blob_tree_nanos: u128,
    #[cfg(test)]
    pub external_record_validation_nanos: u128,
    #[cfg(test)]
    pub external_record_encoding_nanos: u128,
    #[cfg(test)]
    pub external_exact_map_insert_nanos: u128,
    #[cfg(test)]
    pub external_current_map_insert_nanos: u128,
}

pub(crate) struct ExternalCheckpointInput<'a> {
    pub document_id: DocumentId,
    pub document: &'a ExternalDocument,
    pub exact_direct_heads: Vec<BatchId>,
    pub latest_update_digest: ContentDigest,
}

pub(crate) fn commit_external_current_batch(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    lane: DocumentLane,
    latest_source_batch: BatchId,
    latest_manifest_fingerprint: ContentDigest,
    inputs: Vec<ExternalCheckpointInput<'_>>,
) -> Result<(ScratchRoots, DocumentStateWork), DocumentStateError> {
    commit_external_batch(
        store,
        roots,
        lane,
        latest_source_batch,
        latest_manifest_fingerprint,
        inputs,
        true,
    )
}

pub(crate) fn commit_external_exact_batch(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    lane: DocumentLane,
    latest_source_batch: BatchId,
    latest_manifest_fingerprint: ContentDigest,
    inputs: Vec<ExternalCheckpointInput<'_>>,
) -> Result<(ScratchRoots, DocumentStateWork), DocumentStateError> {
    commit_external_batch(
        store,
        roots,
        lane,
        latest_source_batch,
        latest_manifest_fingerprint,
        inputs,
        false,
    )
}

fn commit_external_batch(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    lane: DocumentLane,
    latest_source_batch: BatchId,
    latest_manifest_fingerprint: ContentDigest,
    inputs: Vec<ExternalCheckpointInput<'_>>,
    publish_current: bool,
) -> Result<(ScratchRoots, DocumentStateWork), DocumentStateError> {
    let mut candidate = roots.clone();
    let mut exact_changes = BTreeMap::new();
    let mut current_changes = BTreeMap::new();
    let mut work = DocumentStateWork::default();
    #[cfg(test)]
    {
        work.external_checkpoint_phase_calls = 1;
        work.external_checkpoint_nonempty_calls = usize::from(!inputs.is_empty());
        work.external_checkpoint_documents = inputs.len();
    }
    let mut blob_writer = CheckpointBlobWriter::new(store, &roots.blob_dedup_root);
    for input in inputs {
        let (record, bytes, record_work) = prepare_external_record(
            store,
            &mut blob_writer,
            lane,
            input.document_id,
            input.document,
            input.exact_direct_heads,
            latest_source_batch,
            latest_manifest_fingerprint,
            input.latest_update_digest,
        )?;
        work.state_page_bytes_written = work
            .state_page_bytes_written
            .saturating_add(record_work.state_page_bytes_written);
        work.external_flushes = work
            .external_flushes
            .saturating_add(record_work.external_flushes);
        work.external_point_reads = work
            .external_point_reads
            .saturating_add(record_work.external_point_reads);
        work.external_range_scans = work
            .external_range_scans
            .saturating_add(record_work.external_range_scans);
        work.external_history_page_reads = work
            .external_history_page_reads
            .saturating_add(record_work.external_history_page_reads);
        work.external_history_blob_reads = work
            .external_history_blob_reads
            .saturating_add(record_work.external_history_blob_reads);
        #[cfg(test)]
        accumulate_test_timing(&mut work, &record_work);
        exact_changes.insert(
            external_exact_key(lane, input.document_id, record.causal_digest),
            Some(bytes.clone()),
        );
        if publish_current {
            current_changes.insert(current_key(lane, input.document_id), Some(bytes));
        }
    }
    blob_writer.flush_staged()?;
    candidate.blob_dedup_root = blob_writer.candidate_root.clone();
    #[cfg(test)]
    blob_writer.record_test_work(&mut work);
    if !exact_changes.is_empty() {
        #[cfg(test)]
        let exact_map_started = Instant::now();
        candidate.external_document_state_root = store.insert_many(
            &candidate.external_document_state_root,
            ScratchPageKind::DocumentExternalExact,
            &exact_changes,
        )?;
        #[cfg(test)]
        {
            work.external_exact_map_insert_nanos = work
                .external_exact_map_insert_nanos
                .saturating_add(exact_map_started.elapsed().as_nanos());
        }
    }
    if !current_changes.is_empty() {
        #[cfg(test)]
        let current_map_started = Instant::now();
        candidate.external_document_current_root = store.insert_many(
            &candidate.external_document_current_root,
            ScratchPageKind::DocumentExternalCurrent,
            &current_changes,
        )?;
        #[cfg(test)]
        {
            work.external_current_map_insert_nanos = work
                .external_current_map_insert_nanos
                .saturating_add(current_map_started.elapsed().as_nanos());
        }
    }
    Ok((candidate, work))
}

#[allow(clippy::too_many_arguments)]
fn prepare_external_record(
    store: &Arc<ScratchStore>,
    blob_writer: &mut CheckpointBlobWriter<'_>,
    lane: DocumentLane,
    document_id: DocumentId,
    external: &ExternalDocument,
    mut exact_direct_heads: Vec<BatchId>,
    latest_source_batch: BatchId,
    latest_manifest_fingerprint: ContentDigest,
    latest_update_digest: ContentDigest,
) -> Result<(ExternalDocumentStateRecord, Vec<u8>, DocumentStateWork), DocumentStateError> {
    let document = external.document();
    let history_before = external.history_store.stats();
    exact_direct_heads.sort_unstable();
    exact_direct_heads.dedup();
    let peer_counters = canonical_peer_counters(&document.oplog_vv())?;
    let dependencies = DocumentDependencies::new(
        document_id,
        peer_counters.clone(),
        exact_direct_heads.clone(),
    )
    .map_err(|error| DocumentStateError::InvalidCrdt(error.to_string()))?;
    let causal_digest = dependencies.causal_state_digest();
    #[cfg(test)]
    let checkpoint_export_started = Instant::now();
    let state_checkpoint = document
        .flush_external_store()
        .map_err(|error| DocumentStateError::InvalidCrdt(error.to_string()))?;
    #[cfg(test)]
    let checkpoint_export_nanos = checkpoint_export_started.elapsed().as_nanos();
    #[cfg(test)]
    let witness_root_started = Instant::now();
    let witness = LoroHistoryWitness::new(
        store.workspace_id(),
        document_id,
        lane as u8,
        causal_digest,
        latest_source_batch,
        latest_manifest_fingerprint,
        latest_update_digest,
    )?;
    let history_root = external.history_store.publish_root(witness)?;
    #[cfg(test)]
    let witness_root_nanos = witness_root_started.elapsed().as_nanos();
    let history_after = external.history_store.stats();
    #[cfg(test)]
    let blob_tree_started = Instant::now();
    #[cfg(test)]
    let blob_lsm_insert_before = blob_writer.lsm_insert_nanos;
    let (state_checkpoint, state_page_bytes_written) =
        blob_writer.put_blob_tree(&state_checkpoint)?;
    #[cfg(test)]
    let blob_tree_nanos = blob_tree_started.elapsed().as_nanos().saturating_sub(
        blob_writer
            .lsm_insert_nanos
            .saturating_sub(blob_lsm_insert_before),
    );
    let record = ExternalDocumentStateRecord {
        schema_version: EXTERNAL_DOCUMENT_STATE_SCHEMA_VERSION,
        document_id,
        lane,
        causal_digest,
        state_checkpoint,
        history_root,
        peer_counters,
        exact_direct_heads,
        latest_source_batch,
        latest_manifest_fingerprint,
        latest_update_digest,
    };
    #[cfg(test)]
    let record_validation_started = Instant::now();
    validate_external_record(store, &record)?;
    #[cfg(test)]
    let record_validation_nanos = record_validation_started.elapsed().as_nanos();
    #[cfg(test)]
    let record_encoding_started = Instant::now();
    let bytes = encode_canonical(&record)?;
    #[cfg(test)]
    let record_encoding_nanos = record_encoding_started.elapsed().as_nanos();
    let mut work = DocumentStateWork {
        state_page_bytes_written,
        ..DocumentStateWork::default()
    };
    record_loro_store_work(&mut work, history_before, history_after);
    #[cfg(test)]
    {
        work.loro_external_flush_nanos = history_after
            .flush_nanos
            .saturating_sub(history_before.flush_nanos);
        work.state_checkpoint_export_nanos =
            checkpoint_export_nanos.saturating_sub(work.loro_external_flush_nanos);
        work.external_witness_root_nanos = witness_root_nanos;
        work.blob_tree_nanos = blob_tree_nanos;
        work.external_record_validation_nanos = record_validation_nanos;
        work.external_record_encoding_nanos = record_encoding_nanos;
    }
    Ok((record, bytes, work))
}

#[cfg(test)]
fn accumulate_test_timing(total: &mut DocumentStateWork, record: &DocumentStateWork) {
    total.loro_external_flush_nanos = total
        .loro_external_flush_nanos
        .saturating_add(record.loro_external_flush_nanos);
    total.state_checkpoint_export_nanos = total
        .state_checkpoint_export_nanos
        .saturating_add(record.state_checkpoint_export_nanos);
    total.external_witness_root_nanos = total
        .external_witness_root_nanos
        .saturating_add(record.external_witness_root_nanos);
    total.blob_tree_nanos = total.blob_tree_nanos.saturating_add(record.blob_tree_nanos);
    total.external_record_validation_nanos = total
        .external_record_validation_nanos
        .saturating_add(record.external_record_validation_nanos);
    total.external_record_encoding_nanos = total
        .external_record_encoding_nanos
        .saturating_add(record.external_record_encoding_nanos);
}

struct CheckpointBlobWriter<'a> {
    store: &'a ScratchStore,
    candidate_root: ScratchLsmRoot,
    staged: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    #[cfg(test)]
    chunks: usize,
    #[cfg(test)]
    existing_hits: usize,
    #[cfg(test)]
    staged_hits: usize,
    #[cfg(test)]
    new_chunks: usize,
    #[cfg(test)]
    lsm_flushes: usize,
    #[cfg(test)]
    lsm_insert_nanos: u128,
    #[cfg(test)]
    chunk_digest_nanos: u128,
    #[cfg(test)]
    dedup_lookup_nanos: u128,
    #[cfg(test)]
    blob_append_nanos: u128,
    #[cfg(test)]
    whole_checkpoint_digest_nanos: u128,
}

impl<'a> CheckpointBlobWriter<'a> {
    fn new(store: &'a ScratchStore, root: &ScratchLsmRoot) -> Self {
        Self {
            store,
            candidate_root: root.clone(),
            staged: BTreeMap::new(),
            #[cfg(test)]
            chunks: 0,
            #[cfg(test)]
            existing_hits: 0,
            #[cfg(test)]
            staged_hits: 0,
            #[cfg(test)]
            new_chunks: 0,
            #[cfg(test)]
            lsm_flushes: 0,
            #[cfg(test)]
            lsm_insert_nanos: 0,
            #[cfg(test)]
            chunk_digest_nanos: 0,
            #[cfg(test)]
            dedup_lookup_nanos: 0,
            #[cfg(test)]
            blob_append_nanos: 0,
            #[cfg(test)]
            whole_checkpoint_digest_nanos: 0,
        }
    }

    fn put_blob_tree(&mut self, bytes: &[u8]) -> Result<(BlobTree, usize), DocumentStateError> {
        if bytes.is_empty() {
            return Err(DocumentStateError::InvalidCrdt(
                "empty document checkpoint".into(),
            ));
        }
        let mut chunks = Vec::new();
        let mut new_blob_bytes = 0_usize;
        for chunk in bytes.chunks(CHECKPOINT_CHUNK_BYTES) {
            #[cfg(test)]
            {
                self.chunks = self.chunks.saturating_add(1);
            }
            #[cfg(test)]
            let chunk_digest_started = Instant::now();
            let digest = ContentDigest::of(chunk);
            #[cfg(test)]
            {
                self.chunk_digest_nanos = self
                    .chunk_digest_nanos
                    .saturating_add(chunk_digest_started.elapsed().as_nanos());
            }
            let key = digest.as_bytes().to_vec();
            let chunk_ref = if let Some(encoded) = self.staged.get(&key) {
                let encoded = encoded
                    .as_deref()
                    .ok_or(DocumentStateError::MalformedRecord)?;
                let chunk_ref = self.validate_candidate(encoded, digest, chunk)?;
                #[cfg(test)]
                {
                    self.staged_hits = self.staged_hits.saturating_add(1);
                }
                chunk_ref
            } else if let Some(encoded) = self.lookup_dedup(&key)? {
                let chunk_ref = self.validate_candidate(&encoded, digest, chunk)?;
                #[cfg(test)]
                {
                    self.existing_hits = self.existing_hits.saturating_add(1);
                }
                chunk_ref
            } else {
                let chunk_ref = self.append_blob(chunk)?;
                self.staged.insert(key, Some(encode_canonical(&chunk_ref)?));
                new_blob_bytes = new_blob_bytes.saturating_add(chunk.len());
                #[cfg(test)]
                {
                    self.new_chunks = self.new_chunks.saturating_add(1);
                }
                if self.staged.len() == CHECKPOINT_BLOB_DEDUP_FLUSH_CAP {
                    self.flush_staged()?;
                }
                chunk_ref
            };
            chunks.push(chunk_ref);
        }
        #[cfg(test)]
        let whole_digest_started = Instant::now();
        let whole_digest = ContentDigest::of(bytes);
        #[cfg(test)]
        {
            self.whole_checkpoint_digest_nanos = self
                .whole_checkpoint_digest_nanos
                .saturating_add(whole_digest_started.elapsed().as_nanos());
        }
        Ok((
            BlobTree {
                schema_version: DOCUMENT_STATE_SCHEMA_VERSION,
                encoded_len: bytes.len() as u64,
                digest: whole_digest,
                chunks,
            },
            new_blob_bytes,
        ))
    }

    fn lookup_dedup(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, DocumentStateError> {
        #[cfg(test)]
        let lookup_started = Instant::now();
        let result = self
            .store
            .lookup(&self.candidate_root, ScratchPageKind::BlobDedup, key);
        #[cfg(test)]
        {
            self.dedup_lookup_nanos = self
                .dedup_lookup_nanos
                .saturating_add(lookup_started.elapsed().as_nanos());
        }
        Ok(result?)
    }

    fn append_blob(&mut self, chunk: &[u8]) -> Result<ScratchBlobRef, DocumentStateError> {
        #[cfg(test)]
        let append_started = Instant::now();
        let result = self.store.append_blob(chunk);
        #[cfg(test)]
        {
            self.blob_append_nanos = self
                .blob_append_nanos
                .saturating_add(append_started.elapsed().as_nanos());
        }
        Ok(result?)
    }

    fn validate_candidate(
        &self,
        encoded: &[u8],
        digest: ContentDigest,
        chunk: &[u8],
    ) -> Result<ScratchBlobRef, DocumentStateError> {
        let chunk_ref: ScratchBlobRef = decode_canonical(encoded)?;
        if chunk_ref.digest() != digest || self.store.read_blob(&chunk_ref)? != chunk {
            return Err(DocumentStateError::MisboundRecord);
        }
        Ok(chunk_ref)
    }

    fn flush_staged(&mut self) -> Result<(), DocumentStateError> {
        if self.staged.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        let insert_started = Instant::now();
        self.candidate_root = self.store.insert_many(
            &self.candidate_root,
            ScratchPageKind::BlobDedup,
            &self.staged,
        )?;
        #[cfg(test)]
        {
            self.lsm_flushes = self.lsm_flushes.saturating_add(1);
            self.lsm_insert_nanos = self
                .lsm_insert_nanos
                .saturating_add(insert_started.elapsed().as_nanos());
        }
        self.staged.clear();
        Ok(())
    }

    #[cfg(test)]
    fn record_test_work(&self, work: &mut DocumentStateWork) {
        work.checkpoint_chunks = work.checkpoint_chunks.saturating_add(self.chunks);
        work.checkpoint_existing_hits = work
            .checkpoint_existing_hits
            .saturating_add(self.existing_hits);
        work.checkpoint_staged_hits = work.checkpoint_staged_hits.saturating_add(self.staged_hits);
        work.checkpoint_new_chunks = work.checkpoint_new_chunks.saturating_add(self.new_chunks);
        work.blob_dedup_lsm_flushes = work.blob_dedup_lsm_flushes.saturating_add(self.lsm_flushes);
        work.blob_dedup_lsm_insert_nanos = work
            .blob_dedup_lsm_insert_nanos
            .saturating_add(self.lsm_insert_nanos);
        work.checkpoint_chunk_digest_nanos = work
            .checkpoint_chunk_digest_nanos
            .saturating_add(self.chunk_digest_nanos);
        work.blob_dedup_lookup_nanos = work
            .blob_dedup_lookup_nanos
            .saturating_add(self.dedup_lookup_nanos);
        work.checkpoint_blob_append_nanos = work
            .checkpoint_blob_append_nanos
            .saturating_add(self.blob_append_nanos);
        work.checkpoint_whole_digest_nanos = work
            .checkpoint_whole_digest_nanos
            .saturating_add(self.whole_checkpoint_digest_nanos);
    }
}

pub(crate) fn load_external_current(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    lane: DocumentLane,
    document_id: DocumentId,
) -> Result<
    Option<(
        ExternalDocumentStateRecord,
        ExternalDocument,
        DocumentStateWork,
    )>,
    DocumentStateError,
> {
    load_external_record(
        store,
        roots,
        ScratchPageKind::DocumentExternalCurrent,
        &roots.external_document_current_root,
        current_key(lane, document_id),
        |record| record.document_id == document_id && record.lane == lane,
    )
}

pub(crate) fn load_external_current_many(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    lane: DocumentLane,
    document_ids: &[DocumentId],
) -> Result<
    Vec<
        Option<(
            ExternalDocumentStateRecord,
            ExternalDocument,
            DocumentStateWork,
        )>,
    >,
    DocumentStateError,
> {
    load_external_records_many(
        store,
        roots,
        ScratchPageKind::DocumentExternalCurrent,
        &roots.external_document_current_root,
        document_ids
            .iter()
            .map(|document_id| current_key(lane, *document_id))
            .collect(),
        |index, record| record.document_id == document_ids[index] && record.lane == lane,
    )
}

pub(crate) fn load_external_exact(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    lane: DocumentLane,
    document_id: DocumentId,
    causal_digest: DocumentCausalDigest,
) -> Result<
    Option<(
        ExternalDocumentStateRecord,
        ExternalDocument,
        DocumentStateWork,
    )>,
    DocumentStateError,
> {
    load_external_record(
        store,
        roots,
        ScratchPageKind::DocumentExternalExact,
        &roots.external_document_state_root,
        external_exact_key(lane, document_id, causal_digest),
        |record| {
            record.document_id == document_id
                && record.lane == lane
                && record.causal_digest == causal_digest
        },
    )
}

pub(crate) fn load_external_exact_many(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    lane: DocumentLane,
    documents: &[(DocumentId, DocumentCausalDigest)],
) -> Result<
    Vec<
        Option<(
            ExternalDocumentStateRecord,
            ExternalDocument,
            DocumentStateWork,
        )>,
    >,
    DocumentStateError,
> {
    load_external_records_many(
        store,
        roots,
        ScratchPageKind::DocumentExternalExact,
        &roots.external_document_state_root,
        documents
            .iter()
            .map(|(document_id, causal_digest)| {
                external_exact_key(lane, *document_id, *causal_digest)
            })
            .collect(),
        |index, record| {
            record.document_id == documents[index].0
                && record.lane == lane
                && record.causal_digest == documents[index].1
        },
    )
}

/// Load an authenticated exact checkpoint, including the true empty baseline.
///
/// An empty baseline is minted only after an authenticated absence lookup at
/// the exact-state point root and only for canonical empty dependencies.  A
/// missing non-empty checkpoint remains unavailable.
pub(crate) fn load_authenticated_external_exact(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    lane: DocumentLane,
    document_id: DocumentId,
    dependencies: Option<&DocumentDependencies>,
) -> Result<Option<AuthenticatedExternalExactCheckpoint>, DocumentStateError> {
    if dependencies.is_some_and(|dependencies| dependencies.document_id() != document_id) {
        return Err(DocumentStateError::MisboundRecord);
    }
    let peer_counters = dependencies
        .map(DocumentDependencies::peer_counters)
        .unwrap_or_default();
    let direct_dependency_heads = dependencies
        .map(DocumentDependencies::direct_dependency_heads)
        .unwrap_or_default();
    let causal_digest = dependencies
        .map(DocumentDependencies::causal_state_digest)
        .unwrap_or_else(|| DocumentCausalDigest::of(document_id, &[], &[]));
    if let Some((record, document, state_work)) =
        load_external_exact(store, roots, lane, document_id, causal_digest)?
    {
        if record.peer_counters != peer_counters
            || record.exact_direct_heads != direct_dependency_heads
        {
            return Err(DocumentStateError::MisboundRecord);
        }
        let checkpoint_content_digest = record.state_checkpoint.digest;
        let checkpoint_binding = exact_checkpoint_binding(
            store,
            roots,
            lane,
            document_id,
            causal_digest,
            peer_counters,
            direct_dependency_heads,
            checkpoint_content_digest,
            record.state_checkpoint.encoded_len,
            Some(&record.history_root),
        )?;
        return Ok(Some(AuthenticatedExternalExactCheckpoint {
            document_id: record.document_id,
            causal_digest: record.causal_digest,
            peer_counters: record.peer_counters.clone(),
            exact_direct_heads: record.exact_direct_heads.clone(),
            archive_anchor: Some((
                record.latest_source_batch,
                record.latest_manifest_fingerprint,
                record.latest_update_digest,
            )),
            document,
            state_work,
            checkpoint_binding,
            checkpoint_content_digest,
        }));
    }
    if !peer_counters.is_empty() || !direct_dependency_heads.is_empty() {
        return Ok(None);
    }
    let document = ExternalDocument::empty(Arc::clone(store))?;
    let checkpoint_content_digest = ContentDigest::of(&[]);
    let checkpoint_binding = exact_checkpoint_binding(
        store,
        roots,
        lane,
        document_id,
        causal_digest,
        peer_counters,
        direct_dependency_heads,
        checkpoint_content_digest,
        0,
        None,
    )?;
    Ok(Some(AuthenticatedExternalExactCheckpoint {
        document_id,
        causal_digest,
        peer_counters: Vec::new(),
        exact_direct_heads: Vec::new(),
        archive_anchor: None,
        document,
        state_work: DocumentStateWork::default(),
        checkpoint_binding,
        checkpoint_content_digest,
    }))
}

fn exact_checkpoint_binding(
    store: &ScratchStore,
    roots: &ScratchRoots,
    lane: DocumentLane,
    document_id: DocumentId,
    causal_digest: DocumentCausalDigest,
    peer_counters: &[CrdtPeerCounter],
    direct_dependency_heads: &[BatchId],
    checkpoint_content_digest: ContentDigest,
    checkpoint_encoded_len: u64,
    history_root: Option<&LoroStoreRoot>,
) -> Result<ContentDigest, DocumentStateError> {
    let bytes = encode_canonical(&(
        b"tine/authenticated-exact-document-checkpoint/v1".as_slice(),
        store.workspace_id(),
        lane as u8,
        document_id,
        causal_digest,
        peer_counters,
        direct_dependency_heads,
        checkpoint_content_digest,
        checkpoint_encoded_len,
        history_root,
        &roots.external_document_state_root,
    ))?;
    Ok(ContentDigest::of(&bytes))
}

fn load_external_record(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    kind: ScratchPageKind,
    root: &super::scratch_store::ScratchLsmRoot,
    key: Vec<u8>,
    binding: impl Fn(&ExternalDocumentStateRecord) -> bool,
) -> Result<
    Option<(
        ExternalDocumentStateRecord,
        ExternalDocument,
        DocumentStateWork,
    )>,
    DocumentStateError,
> {
    let Some(bytes) = store.lookup(root, kind, &key)? else {
        return Ok(None);
    };
    Ok(Some(load_external_record_value(
        store, roots, &bytes, binding,
    )?))
}

fn load_external_records_many(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    kind: ScratchPageKind,
    root: &super::scratch_store::ScratchLsmRoot,
    keys: Vec<Vec<u8>>,
    binding: impl Fn(usize, &ExternalDocumentStateRecord) -> bool,
) -> Result<
    Vec<
        Option<(
            ExternalDocumentStateRecord,
            ExternalDocument,
            DocumentStateWork,
        )>,
    >,
    DocumentStateError,
> {
    store
        .lookup_many(root, kind, &keys)?
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| {
            bytes
                .map(|bytes| {
                    load_external_record_value(store, roots, &bytes, |record| {
                        binding(index, record)
                    })
                })
                .transpose()
        })
        .collect()
}

fn load_external_record_value(
    store: &Arc<ScratchStore>,
    roots: &ScratchRoots,
    bytes: &[u8],
    binding: impl Fn(&ExternalDocumentStateRecord) -> bool,
) -> Result<
    (
        ExternalDocumentStateRecord,
        ExternalDocument,
        DocumentStateWork,
    ),
    DocumentStateError,
> {
    let record: ExternalDocumentStateRecord = decode_canonical(&bytes)?;
    validate_external_record(store, &record)?;
    if !binding(&record) {
        return Err(DocumentStateError::MisboundRecord);
    }
    let checkpoint = read_blob_tree(store, roots, &record.state_checkpoint)?;
    let history_store = AuthenticatedLoroStore::reopen(
        Arc::clone(store),
        &record.history_root,
        record.history_root.witness(),
    )?;
    let history_before = history_store.stats();
    let document = LoroDoc::from_external_store(Some(&checkpoint), history_store.handle())
        .map_err(|error| DocumentStateError::InvalidCrdt(error.to_string()))?;
    if canonical_peer_counters(&document.oplog_vv())? != record.peer_counters {
        return Err(DocumentStateError::MisboundRecord);
    }
    let history_after = history_store.stats();
    let mut work = DocumentStateWork {
        document_point_reads: 1,
        state_page_bytes_read: checkpoint.len(),
        ..DocumentStateWork::default()
    };
    record_loro_store_work(&mut work, history_before, history_after);
    Ok((
        record,
        ExternalDocument {
            document,
            history_store,
        },
        work,
    ))
}

fn record_loro_store_work(
    work: &mut DocumentStateWork,
    before: LoroStoreStats,
    after: LoroStoreStats,
) {
    work.external_flushes = work
        .external_flushes
        .saturating_add(after.flush_calls.saturating_sub(before.flush_calls));
    work.external_point_reads = work
        .external_point_reads
        .saturating_add(after.point_reads.saturating_sub(before.point_reads));
    work.external_range_scans = work
        .external_range_scans
        .saturating_add(after.range_scans.saturating_sub(before.range_scans));
    work.external_history_page_reads = work.external_history_page_reads.saturating_add(
        after
            .history_page_reads
            .saturating_sub(before.history_page_reads),
    );
    work.external_history_blob_reads = work.external_history_blob_reads.saturating_add(
        after
            .history_blob_reads
            .saturating_sub(before.history_blob_reads),
    );
}

fn read_blob_tree(
    store: &ScratchStore,
    _roots: &ScratchRoots,
    tree: &BlobTree,
) -> Result<Vec<u8>, DocumentStateError> {
    if tree.schema_version != DOCUMENT_STATE_SCHEMA_VERSION
        || tree.encoded_len == 0
        || tree.chunks.is_empty()
    {
        return Err(DocumentStateError::MalformedRecord);
    }
    let expected_len =
        usize::try_from(tree.encoded_len).map_err(|_| DocumentStateError::MalformedRecord)?;
    let mut bytes = Vec::with_capacity(expected_len);
    for chunk in &tree.chunks {
        bytes.extend_from_slice(&store.read_blob(chunk)?);
        if bytes.len() > expected_len {
            return Err(DocumentStateError::MalformedRecord);
        }
    }
    bytes.truncate(expected_len);
    if bytes.len() != expected_len || ContentDigest::of(&bytes) != tree.digest {
        return Err(DocumentStateError::MalformedRecord);
    }
    Ok(bytes)
}

fn validate_external_record(
    store: &ScratchStore,
    record: &ExternalDocumentStateRecord,
) -> Result<(), DocumentStateError> {
    if record.schema_version != EXTERNAL_DOCUMENT_STATE_SCHEMA_VERSION
        || record
            .peer_counters
            .windows(2)
            .any(|pair| pair[0].peer_id() >= pair[1].peer_id())
        || record
            .exact_direct_heads
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        return Err(DocumentStateError::MalformedRecord);
    }
    let dependencies = DocumentDependencies::new(
        record.document_id,
        record.peer_counters.clone(),
        record.exact_direct_heads.clone(),
    )
    .map_err(|error| DocumentStateError::InvalidCrdt(error.to_string()))?;
    if dependencies.causal_state_digest() != record.causal_digest {
        return Err(DocumentStateError::MisboundRecord);
    }
    let witness = record.history_root.witness();
    if witness.workspace_id() != store.workspace_id()
        || witness.document_id() != record.document_id
        || witness.lane() != record.lane as u8
        || witness.causal_digest() != record.causal_digest
        || witness.latest_source_batch() != record.latest_source_batch
        || witness.latest_manifest_fingerprint() != record.latest_manifest_fingerprint
        || witness.latest_update_digest() != record.latest_update_digest
    {
        return Err(DocumentStateError::MisboundRecord);
    }
    record.history_root.validate_for(store, witness)?;
    Ok(())
}

fn canonical_peer_counters(
    version: &VersionVector,
) -> Result<Vec<CrdtPeerCounter>, DocumentStateError> {
    let mut counters = Vec::new();
    for (peer, end) in version.iter() {
        if *end <= 0 {
            continue;
        }
        let max_counter = u64::try_from(*end - 1)
            .map_err(|_| DocumentStateError::InvalidCrdt("negative CRDT counter".into()))?;
        counters.push(CrdtPeerCounter::new(
            CrdtPeerId::from_u64(*peer),
            max_counter,
        ));
    }
    counters.sort_unstable_by_key(|counter| counter.peer_id());
    Ok(counters)
}

fn current_key(lane: DocumentLane, document_id: DocumentId) -> Vec<u8> {
    let mut key = vec![lane as u8];
    key.extend_from_slice(document_id.as_uuid().as_bytes());
    key
}

fn exact_key(document_id: DocumentId, digest: DocumentCausalDigest) -> Vec<u8> {
    let mut key = document_id.as_uuid().as_bytes().to_vec();
    key.extend_from_slice(digest.as_bytes());
    key
}

fn external_exact_key(
    lane: DocumentLane,
    document_id: DocumentId,
    digest: DocumentCausalDigest,
) -> Vec<u8> {
    let mut key = vec![lane as u8];
    key.extend_from_slice(&exact_key(document_id, digest));
    key
}

fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, DocumentStateError> {
    postcard::to_allocvec(value).map_err(|_| DocumentStateError::MalformedRecord)
}

fn decode_canonical<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
) -> Result<T, DocumentStateError> {
    let value: T = postcard::from_bytes(bytes).map_err(|_| DocumentStateError::MalformedRecord)?;
    if encode_canonical(&value)? != bytes {
        return Err(DocumentStateError::MalformedRecord);
    }
    Ok(value)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DocumentStateError {
    Scratch(String),
    ExternalStore(String),
    InvalidCrdt(String),
    MisboundRecord,
    MalformedRecord,
}

impl fmt::Display for DocumentStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scratch(error) => write!(f, "document scratch index failed: {error}"),
            Self::ExternalStore(error) => {
                write!(f, "document external history store failed: {error}")
            }
            Self::InvalidCrdt(error) => write!(f, "invalid CRDT checkpoint: {error}"),
            Self::MisboundRecord => f.write_str("misbound document checkpoint record"),
            Self::MalformedRecord => f.write_str("malformed document checkpoint record"),
        }
    }
}

impl std::error::Error for DocumentStateError {}

impl From<super::scratch_store::ScratchError> for DocumentStateError {
    fn from(error: super::scratch_store::ScratchError) -> Self {
        Self::Scratch(error.to_string())
    }
}

impl From<super::loro_store::LoroStoreError> for DocumentStateError {
    fn from(error: super::loro_store::LoroStoreError) -> Self {
        Self::ExternalStore(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::{ambient_authority, fs::Dir};
    use std::fs;
    use std::path::Path;
    use uuid::Uuid;

    fn workspace(value: u128) -> super::super::WorkspaceId {
        super::super::WorkspaceId::from_uuid(Uuid::from_u128(value))
    }

    fn document(value: u128) -> DocumentId {
        DocumentId::from_uuid(Uuid::from_u128(value))
    }

    fn batch(value: u128) -> BatchId {
        BatchId::from_uuid(Uuid::from_u128(value))
    }

    fn archive(root: &Path) -> Dir {
        fs::create_dir_all(root).unwrap();
        Dir::open_ambient_dir(root, ambient_authority()).unwrap()
    }

    fn external_document(
        scratch: &Arc<ScratchStore>,
        peer: u64,
        key: &str,
        value: &str,
    ) -> ExternalDocument {
        let external = ExternalDocument::empty(Arc::clone(scratch)).unwrap();
        external.document().set_peer_id(peer).unwrap();
        external
            .document()
            .get_map("map")
            .insert(key, value)
            .unwrap();
        external
            .document()
            .get_text("text")
            .insert(0, value)
            .unwrap();
        external.document().commit();
        external
    }

    fn causal_digest(
        document_id: DocumentId,
        external: &ExternalDocument,
        heads: Vec<BatchId>,
    ) -> DocumentCausalDigest {
        DocumentDependencies::new(
            document_id,
            canonical_peer_counters(&external.document().oplog_vv()).unwrap(),
            heads,
        )
        .unwrap()
        .causal_state_digest()
    }

    #[test]
    fn external_checkpoint_round_trip_binds_history_to_document_lane_and_causality() {
        let path = std::env::temp_dir().join(format!("tine-external-document-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let scratch = Arc::new(ScratchStore::open(&archive, workspace(1)).unwrap());
        let document_id = document(2);
        let source_batch = batch(3);
        let external = external_document(&scratch, 7, "key", "checkpoint");
        let digest = causal_digest(document_id, &external, vec![source_batch]);
        let (roots, work) = commit_external_current_batch(
            &scratch,
            &ScratchRoots::default(),
            DocumentLane::Visible,
            source_batch,
            ContentDigest::of(b"manifest"),
            vec![ExternalCheckpointInput {
                document_id,
                document: &external,
                exact_direct_heads: vec![source_batch],
                latest_update_digest: ContentDigest::of(b"update"),
            }],
        )
        .unwrap();
        assert_eq!(work.external_flushes, 1);
        assert_eq!(external.store_stats().flush_calls, 1);
        assert!(work.state_page_bytes_written > 0);
        let current_bytes = scratch
            .lookup(
                &roots.external_document_current_root,
                ScratchPageKind::DocumentExternalCurrent,
                &current_key(DocumentLane::Visible, document_id),
            )
            .unwrap()
            .unwrap();
        let exact_bytes = scratch
            .lookup(
                &roots.external_document_state_root,
                ScratchPageKind::DocumentExternalExact,
                &external_exact_key(DocumentLane::Visible, document_id, digest),
            )
            .unwrap()
            .unwrap();
        assert_eq!(current_bytes, exact_bytes);
        let expected = external.document().get_deep_value();

        let (loaded_record, reopened, read_work) =
            load_external_current(&scratch, &roots, DocumentLane::Visible, document_id)
                .unwrap()
                .expect("external checkpoint");
        let (exact_record, exact, _) =
            load_external_exact(&scratch, &roots, DocumentLane::Visible, document_id, digest)
                .unwrap()
                .expect("co-published exact checkpoint");
        assert_eq!(loaded_record, exact_record);
        let witness = loaded_record.history_root().witness();
        assert_eq!(witness.workspace_id(), scratch.workspace_id());
        assert_eq!(witness.document_id(), document_id);
        assert_eq!(witness.lane(), DocumentLane::Visible as u8);
        assert_eq!(witness.causal_digest(), digest);
        assert_eq!(reopened.document().get_deep_value(), expected);
        assert_eq!(exact.document().get_deep_value(), expected);
        assert_eq!(read_work.document_point_reads, 1);
        assert!(
            load_external_exact(
                &scratch,
                &roots,
                DocumentLane::Terminal,
                document_id,
                digest,
            )
            .unwrap()
            .is_none(),
            "visible exact state must not be addressable through the terminal lane"
        );
        let mut forged_roots = roots.clone();
        let mut forged_exact = BTreeMap::new();
        forged_exact.insert(
            external_exact_key(DocumentLane::Terminal, document_id, digest),
            Some(exact_bytes),
        );
        forged_roots.external_document_state_root = scratch
            .insert_many(
                &forged_roots.external_document_state_root,
                ScratchPageKind::DocumentExternalExact,
                &forged_exact,
            )
            .unwrap();
        assert!(
            matches!(
                load_external_exact(
                    &scratch,
                    &forged_roots,
                    DocumentLane::Terminal,
                    document_id,
                    digest,
                ),
                Err(DocumentStateError::MisboundRecord)
            ),
            "an exact record must authenticate the lane encoded by its index key"
        );
        assert_eq!(scratch.stats().scratch_syncs, 0);
        drop(reopened);
        drop(exact);
        drop(external);
        drop(scratch);
        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn identical_checkpoints_share_staged_blob_refs_and_one_dedup_flush() {
        let path = std::env::temp_dir().join(format!("tine-external-shared-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let scratch = Arc::new(ScratchStore::open(&archive, workspace(31)).unwrap());
        let document_a = document(32);
        let document_b = document(33);
        let source_batch = batch(34);
        let external_a = external_document(&scratch, 7, "same", "checkpoint");
        let external_b = external_document(&scratch, 7, "same", "checkpoint");
        let expected = external_a.document().get_deep_value();
        assert_eq!(external_b.document().get_deep_value(), expected);

        let (roots, work) = commit_external_current_batch(
            &scratch,
            &ScratchRoots::default(),
            DocumentLane::Visible,
            source_batch,
            ContentDigest::of(b"shared-manifest"),
            vec![
                ExternalCheckpointInput {
                    document_id: document_a,
                    document: &external_a,
                    exact_direct_heads: vec![source_batch],
                    latest_update_digest: ContentDigest::of(b"shared-a"),
                },
                ExternalCheckpointInput {
                    document_id: document_b,
                    document: &external_b,
                    exact_direct_heads: vec![source_batch],
                    latest_update_digest: ContentDigest::of(b"shared-b"),
                },
            ],
        )
        .unwrap();

        assert_eq!(work.external_checkpoint_phase_calls, 1);
        assert_eq!(work.external_checkpoint_nonempty_calls, 1);
        assert_eq!(work.external_checkpoint_documents, 2);
        assert_eq!(work.external_flushes, 2);
        assert_eq!(work.checkpoint_new_chunks, 1);
        assert_eq!(work.checkpoint_existing_hits, 0);
        assert_eq!(work.checkpoint_staged_hits, work.checkpoint_new_chunks);
        assert_eq!(
            work.checkpoint_chunks,
            work.checkpoint_new_chunks.saturating_mul(2)
        );
        assert_eq!(work.blob_dedup_lsm_flushes, 1);

        let (record_a, reopened_a, _) =
            load_external_current(&scratch, &roots, DocumentLane::Visible, document_a)
                .unwrap()
                .unwrap();
        let (record_b, reopened_b, _) =
            load_external_current(&scratch, &roots, DocumentLane::Visible, document_b)
                .unwrap()
                .unwrap();
        assert_eq!(
            record_a.state_checkpoint.chunks,
            record_b.state_checkpoint.chunks
        );
        assert_ne!(record_a, record_b);
        assert_eq!(record_a.history_root().witness().document_id(), document_a);
        assert_eq!(record_b.history_root().witness().document_id(), document_b);
        assert_eq!(reopened_a.document().get_deep_value(), expected);
        assert_eq!(reopened_b.document().get_deep_value(), expected);

        drop(reopened_a);
        drop(reopened_b);
        drop(external_a);
        drop(external_b);
        drop(scratch);
        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn distinct_checkpoints_do_not_share_blob_refs_and_reopen_exactly() {
        let path = std::env::temp_dir().join(format!("tine-external-distinct-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let scratch = Arc::new(ScratchStore::open(&archive, workspace(41)).unwrap());
        let document_a = document(42);
        let document_b = document(43);
        let source_batch = batch(44);
        let external_a = external_document(&scratch, 1, "state", "left");
        let external_b = external_document(&scratch, 2, "state", "right");
        let expected_a = external_a.document().get_deep_value();
        let expected_b = external_b.document().get_deep_value();

        let (roots, work) = commit_external_current_batch(
            &scratch,
            &ScratchRoots::default(),
            DocumentLane::Visible,
            source_batch,
            ContentDigest::of(b"distinct-manifest"),
            vec![
                ExternalCheckpointInput {
                    document_id: document_a,
                    document: &external_a,
                    exact_direct_heads: vec![source_batch],
                    latest_update_digest: ContentDigest::of(b"distinct-a"),
                },
                ExternalCheckpointInput {
                    document_id: document_b,
                    document: &external_b,
                    exact_direct_heads: vec![source_batch],
                    latest_update_digest: ContentDigest::of(b"distinct-b"),
                },
            ],
        )
        .unwrap();

        assert_eq!(work.checkpoint_staged_hits, 0);
        assert_eq!(work.checkpoint_existing_hits, 0);
        assert_eq!(work.checkpoint_new_chunks, work.checkpoint_chunks);
        assert_eq!(work.blob_dedup_lsm_flushes, 1);
        let (record_a, reopened_a, _) =
            load_external_current(&scratch, &roots, DocumentLane::Visible, document_a)
                .unwrap()
                .unwrap();
        let (record_b, reopened_b, _) =
            load_external_current(&scratch, &roots, DocumentLane::Visible, document_b)
                .unwrap()
                .unwrap();
        assert_ne!(
            record_a.state_checkpoint.chunks,
            record_b.state_checkpoint.chunks
        );
        assert_eq!(reopened_a.document().get_deep_value(), expected_a);
        assert_eq!(reopened_b.document().get_deep_value(), expected_b);

        drop(reopened_a);
        drop(reopened_b);
        drop(external_a);
        drop(external_b);
        drop(scratch);
        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn misbound_existing_dedup_ref_fails_closed() {
        let path = std::env::temp_dir().join(format!("tine-external-misbound-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let scratch = Arc::new(ScratchStore::open(&archive, workspace(51)).unwrap());
        let requested = b"requested checkpoint bytes";
        let wrong_ref = scratch.append_blob(b"different checkpoint bytes").unwrap();
        let key = ContentDigest::of(requested).as_bytes().to_vec();
        let mut changes = BTreeMap::new();
        changes.insert(key, Some(encode_canonical(&wrong_ref).unwrap()));
        let mut roots = ScratchRoots::default();
        roots.blob_dedup_root = scratch
            .insert_many(&roots.blob_dedup_root, ScratchPageKind::BlobDedup, &changes)
            .unwrap();
        let original_root = roots.blob_dedup_root.clone();
        let mut writer = CheckpointBlobWriter::new(&scratch, &roots.blob_dedup_root);

        assert_eq!(
            writer.put_blob_tree(requested).unwrap_err(),
            DocumentStateError::MisboundRecord
        );
        assert_eq!(writer.candidate_root, original_root);
        assert!(writer.staged.is_empty());

        drop(scratch);
        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn capped_dedup_flush_exposes_later_duplicates_through_candidate_root() {
        let path = std::env::temp_dir().join(format!("tine-external-cap-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let scratch = Arc::new(ScratchStore::open(&archive, workspace(61)).unwrap());
        let requested = b"checkpoint at deterministic cap";
        let mut writer =
            CheckpointBlobWriter::new(&scratch, &ScratchRoots::default().blob_dedup_root);
        for value in 0..CHECKPOINT_BLOB_DEDUP_FLUSH_CAP - 1 {
            writer
                .staged
                .insert((value as u64).to_be_bytes().to_vec(), Some(vec![0]));
        }

        let (first, _) = writer.put_blob_tree(requested).unwrap();
        assert!(writer.staged.is_empty());
        assert_eq!(writer.lsm_flushes, 1);
        let (second, _) = writer.put_blob_tree(requested).unwrap();
        assert_eq!(first.chunks, second.chunks);
        assert_eq!(writer.new_chunks, 1);
        assert_eq!(writer.existing_hits, 1);
        assert_eq!(writer.staged_hits, 0);

        drop(scratch);
        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn divergent_exact_and_current_states_flush_and_reopen_independently() {
        let path = std::env::temp_dir().join(format!("tine-external-divergent-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let scratch = Arc::new(ScratchStore::open(&archive, workspace(11)).unwrap());
        let document_id = document(12);
        let base_batch = batch(13);
        let branch_batch = batch(14);
        let base = external_document(&scratch, 1, "base", "base");
        let (base_roots, _) = commit_external_current_batch(
            &scratch,
            &ScratchRoots::default(),
            DocumentLane::Visible,
            base_batch,
            ContentDigest::of(b"base-manifest"),
            vec![ExternalCheckpointInput {
                document_id,
                document: &base,
                exact_direct_heads: vec![base_batch],
                latest_update_digest: ContentDigest::of(b"base-update"),
            }],
        )
        .unwrap();
        let (_, exact_branch, _) =
            load_external_current(&scratch, &base_roots, DocumentLane::Visible, document_id)
                .unwrap()
                .unwrap();
        let (_, current_join, _) =
            load_external_current(&scratch, &base_roots, DocumentLane::Visible, document_id)
                .unwrap()
                .unwrap();
        exact_branch.document().set_peer_id(2).unwrap();
        exact_branch
            .document()
            .get_map("map")
            .insert("branch", "exact")
            .unwrap();
        exact_branch.document().commit();
        current_join.document().set_peer_id(3).unwrap();
        current_join
            .document()
            .get_map("map")
            .insert("branch", "current")
            .unwrap();
        current_join.document().commit();
        let exact_heads = vec![branch_batch];
        let current_heads = vec![base_batch, branch_batch];
        let exact_digest = causal_digest(document_id, &exact_branch, exact_heads.clone());
        let current_digest = causal_digest(document_id, &current_join, current_heads.clone());

        let (exact_roots, exact_work) = commit_external_exact_batch(
            &scratch,
            &base_roots,
            DocumentLane::Visible,
            branch_batch,
            ContentDigest::of(b"branch-manifest"),
            vec![ExternalCheckpointInput {
                document_id,
                document: &exact_branch,
                exact_direct_heads: exact_heads,
                latest_update_digest: ContentDigest::of(b"branch-update"),
            }],
        )
        .unwrap();
        let (roots, current_work) = commit_external_current_batch(
            &scratch,
            &exact_roots,
            DocumentLane::Visible,
            branch_batch,
            ContentDigest::of(b"branch-manifest"),
            vec![ExternalCheckpointInput {
                document_id,
                document: &current_join,
                exact_direct_heads: current_heads,
                latest_update_digest: ContentDigest::of(b"branch-update"),
            }],
        )
        .unwrap();
        assert_eq!(
            exact_work.external_flushes + current_work.external_flushes,
            2
        );

        let (_, reopened_exact, _) = load_external_exact(
            &scratch,
            &roots,
            DocumentLane::Visible,
            document_id,
            exact_digest,
        )
        .unwrap()
        .unwrap();
        let (_, reopened_current_exact, _) = load_external_exact(
            &scratch,
            &roots,
            DocumentLane::Visible,
            document_id,
            current_digest,
        )
        .unwrap()
        .unwrap();
        let (_, reopened_current, _) =
            load_external_current(&scratch, &roots, DocumentLane::Visible, document_id)
                .unwrap()
                .unwrap();
        assert_eq!(
            reopened_exact.document().get_deep_value(),
            exact_branch.document().get_deep_value()
        );
        assert_eq!(
            reopened_current_exact.document().get_deep_value(),
            current_join.document().get_deep_value()
        );
        assert_eq!(
            reopened_current.document().get_deep_value(),
            reopened_current_exact.document().get_deep_value()
        );
        drop(reopened_exact);
        drop(reopened_current_exact);
        drop(reopened_current);
        drop(exact_branch);
        drop(current_join);
        drop(base);
        drop(scratch);
        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn two_document_late_failure_leaves_only_prior_state_reachable() {
        let path = std::env::temp_dir().join(format!("tine-external-atomic-{}", Uuid::new_v4()));
        let archive = archive(&path);
        let scratch = Arc::new(ScratchStore::open(&archive, workspace(21)).unwrap());
        let document_a = document(22);
        let document_b = document(23);
        let base_batch = batch(24);
        let next_batch = batch(25);
        let base_a = external_document(&scratch, 1, "state", "old-a");
        let base_b = external_document(&scratch, 2, "state", "old-b");
        let (roots, _) = commit_external_current_batch(
            &scratch,
            &ScratchRoots::default(),
            DocumentLane::Visible,
            base_batch,
            ContentDigest::of(b"base-manifest"),
            vec![
                ExternalCheckpointInput {
                    document_id: document_a,
                    document: &base_a,
                    exact_direct_heads: vec![base_batch],
                    latest_update_digest: ContentDigest::of(b"base-a"),
                },
                ExternalCheckpointInput {
                    document_id: document_b,
                    document: &base_b,
                    exact_direct_heads: vec![base_batch],
                    latest_update_digest: ContentDigest::of(b"base-b"),
                },
            ],
        )
        .unwrap();
        let original_roots = roots.clone();
        let (_, next_a, _) =
            load_external_current(&scratch, &roots, DocumentLane::Visible, document_a)
                .unwrap()
                .unwrap();
        let (_, next_b, _) =
            load_external_current(&scratch, &roots, DocumentLane::Visible, document_b)
                .unwrap()
                .unwrap();
        next_a.document().set_peer_id(3).unwrap();
        next_a
            .document()
            .get_map("map")
            .insert("state", "new-a")
            .unwrap();
        next_a.document().commit();
        next_b.document().set_peer_id(4).unwrap();
        next_b
            .document()
            .get_map("map")
            .insert("state", "new-b")
            .unwrap();
        next_b.document().commit();
        let next_a_digest = causal_digest(document_a, &next_a, vec![next_batch]);
        let next_b_digest = causal_digest(document_b, &next_b, vec![next_batch]);
        next_b.poison_store_for_test("second prepared record fails");

        assert!(commit_external_current_batch(
            &scratch,
            &roots,
            DocumentLane::Visible,
            next_batch,
            ContentDigest::of(b"next-manifest"),
            vec![
                ExternalCheckpointInput {
                    document_id: document_a,
                    document: &next_a,
                    exact_direct_heads: vec![next_batch],
                    latest_update_digest: ContentDigest::of(b"next-a"),
                },
                ExternalCheckpointInput {
                    document_id: document_b,
                    document: &next_b,
                    exact_direct_heads: vec![next_batch],
                    latest_update_digest: ContentDigest::of(b"next-b"),
                },
            ],
        )
        .is_err());
        assert_eq!(roots, original_roots);
        for (document_id, expected) in [
            (document_a, base_a.document().get_deep_value()),
            (document_b, base_b.document().get_deep_value()),
        ] {
            let (_, reopened, _) =
                load_external_current(&scratch, &roots, DocumentLane::Visible, document_id)
                    .unwrap()
                    .unwrap();
            assert_eq!(reopened.document().get_deep_value(), expected);
        }
        assert!(load_external_exact(
            &scratch,
            &roots,
            DocumentLane::Visible,
            document_a,
            next_a_digest,
        )
        .unwrap()
        .is_none());
        assert!(load_external_exact(
            &scratch,
            &roots,
            DocumentLane::Visible,
            document_b,
            next_b_digest,
        )
        .unwrap()
        .is_none());
        drop(next_a);
        drop(next_b);
        drop(base_a);
        drop(base_b);
        drop(scratch);
        crate::test_support::remove_dir_all(path);
    }
}
