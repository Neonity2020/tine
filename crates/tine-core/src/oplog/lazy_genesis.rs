//! Versioned lazy-genesis semantic pack for managed-storage activation.
//!
//! This module owns the durable byte format, not activation's process-only
//! parser handoff. It intentionally contains no search/query/reference facets:
//! those are disposable SQLite projection facts. Ordinary CRDT checkpoints are
//! added by the terminal constructor before this candidate becomes publishable.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    BlobDescription, BlockId, ContentDigest, DocumentDependencies, DocumentId, LineageDigest,
    LogseqUuid, ManagedPath, ManagedTextKind, PageId, WorkspaceId,
};

const LAZY_GENESIS_SCHEMA_VERSION: u32 = 2;
const LAZY_GENESIS_COMMIT_SCHEMA_VERSION: u32 = 1;
const LAZY_GENESIS_ACTIVATION_MARKER_SCHEMA_VERSION: u32 = 1;
const LAZY_GENESIS_ACTIVATION_MARKER_MAGIC: &[u8] = b"TINE-LAZY-GENESIS-ACTIVATION\0";
const LAZY_GENESIS_MANIFEST_FILE: &str = "manifest.postcard";
const LAZY_GENESIS_COMMIT_FILE: &str = "commit.postcard";
const MAX_LAZY_GENESIS_MANIFEST_BYTES: usize = 256 * 1024 * 1024;
const LAZY_GENESIS_SEGMENT_TARGET_BYTES: usize = 32 * 1024 * 1024;
const MAX_LAZY_GENESIS_CAPSULE_BYTES: usize = 256 * 1024 * 1024;
const MAX_LAZY_GENESIS_CATALOG_CHECKPOINT_BYTES: usize = 512 * 1024 * 1024;
const MAX_LAZY_GENESIS_PAGES: usize = 1_000_000;
const MAX_LAZY_GENESIS_BLOCKS: u64 = 100_000_000;
const LAZY_GENESIS_FRONTIER_BINDING_SCHEMA_VERSION: u32 = 1;

/// Constant-size durable identity of the immutable baseline carried by an
/// accepted frontier. The manifest root transitively binds the source capture,
/// every page capsule/checkpoint, and the catalog checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LazyGenesisFrontierBindingV1 {
    schema_version: u32,
    root: ContentDigest,
    source_capture: BlobDescription,
    document_count: u64,
    block_count: u64,
}

impl LazyGenesisFrontierBindingV1 {
    fn new(candidate: &LazyGenesisCandidate) -> io::Result<Self> {
        let document_count = candidate
            .manifest
            .page_count
            .checked_add(u64::from(candidate.manifest.catalog_dependencies.is_some()))
            .ok_or_else(|| invalid("lazy genesis document count overflowed"))?;
        let binding = Self {
            schema_version: LAZY_GENESIS_FRONTIER_BINDING_SCHEMA_VERSION,
            root: candidate.root,
            source_capture: candidate.manifest.source_capture,
            document_count,
            block_count: candidate.manifest.block_count,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub(crate) fn validate(self) -> io::Result<()> {
        if self.schema_version != LAZY_GENESIS_FRONTIER_BINDING_SCHEMA_VERSION {
            return Err(invalid("lazy genesis frontier binding is malformed"));
        }
        Ok(())
    }

    pub(crate) const fn root(self) -> ContentDigest {
        self.root
    }

    pub(crate) const fn document_count(self) -> u64 {
        self.document_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LazyGenesisBlockInput {
    pub(crate) block_id: BlockId,
    pub(crate) home_document_id: DocumentId,
    pub(crate) parent: Option<BlockId>,
    pub(crate) order: String,
    pub(crate) content: String,
    pub(crate) external_uuid_claims: Vec<LogseqUuid>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LazyGenesisPageInput {
    pub(crate) source_leaf: [u8; 32],
    pub(crate) exact_source_bytes: u64,
    pub(crate) page_id: PageId,
    pub(crate) home_document_id: DocumentId,
    pub(crate) name: String,
    pub(crate) path: ManagedPath,
    pub(crate) kind: ManagedTextKind,
    pub(crate) preamble: Option<String>,
    pub(crate) blocks: Vec<LazyGenesisBlockInput>,
    pub(crate) document_checkpoint: Vec<u8>,
    pub(crate) document_dependencies: Option<DocumentDependencies>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LazyGenesisPageCapsuleV1 {
    schema_version: u32,
    source_leaf: [u8; 32],
    exact_source_bytes: u64,
    page_id: PageId,
    home_document_id: DocumentId,
    name: String,
    path: ManagedPath,
    kind: ManagedTextKind,
    preamble: Option<String>,
    blocks: Vec<LazyGenesisBlockInput>,
    document_checkpoint: Vec<u8>,
}

impl LazyGenesisPageCapsuleV1 {
    fn from_input(input: LazyGenesisPageInput) -> io::Result<Self> {
        let capsule = Self {
            schema_version: LAZY_GENESIS_SCHEMA_VERSION,
            source_leaf: input.source_leaf,
            exact_source_bytes: input.exact_source_bytes,
            page_id: input.page_id,
            home_document_id: input.home_document_id,
            name: input.name,
            path: input.path,
            kind: input.kind,
            preamble: input.preamble,
            blocks: input.blocks,
            document_checkpoint: input.document_checkpoint,
        };
        capsule.validate()?;
        Ok(capsule)
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema_version != LAZY_GENESIS_SCHEMA_VERSION
            || self.name.is_empty()
            || self.document_checkpoint.is_empty()
        {
            return Err(invalid("lazy genesis page capsule has an invalid header"));
        }
        let mut block_ids = BTreeSet::new();
        for block in &self.blocks {
            if block.home_document_id != self.home_document_id
                || !block_ids.insert(block.block_id)
                || block.parent.is_some_and(|parent| parent == block.block_id)
            {
                return Err(invalid(
                    "lazy genesis page capsule has invalid block identity",
                ));
            }
            if block
                .external_uuid_claims
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            {
                return Err(invalid(
                    "lazy genesis external UUID claims are not canonical",
                ));
            }
        }
        if self
            .blocks
            .iter()
            .filter_map(|block| block.parent)
            .any(|parent| !block_ids.contains(&parent))
        {
            return Err(invalid("lazy genesis block parent is outside its page"));
        }
        Ok(())
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let bytes = postcard::to_allocvec(self).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_LAZY_GENESIS_CAPSULE_BYTES {
            return Err(invalid("lazy genesis page capsule exceeds its fixed cap"));
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_LAZY_GENESIS_CAPSULE_BYTES {
            return Err(invalid("lazy genesis page capsule exceeds its fixed cap"));
        }
        let capsule: Self =
            postcard::from_bytes(bytes).map_err(|error| invalid(error.to_string()))?;
        capsule.validate()?;
        Ok(capsule)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LazyGenesisPageDescriptorV1 {
    page_id: PageId,
    home_document_id: DocumentId,
    path: ManagedPath,
    capsule: BlobDescription,
    segment: u32,
    offset: u64,
    length: u64,
    blocks: u32,
    document_dependencies: DocumentDependencies,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LazyGenesisManifestV1 {
    schema_version: u32,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    source_capture: BlobDescription,
    catalog_checkpoint: BlobDescription,
    catalog_dependencies: Option<DocumentDependencies>,
    pages: Vec<LazyGenesisPageDescriptorV1>,
    segments: Vec<BlobDescription>,
    page_count: u64,
    block_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LazyGenesisCommitV1 {
    schema_version: u32,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    source_capture: BlobDescription,
    manifest: BlobDescription,
    root: ContentDigest,
}

/// The sole durable authority transition for the next activation generation.
///
/// The Tauri binding records the user's intent to use managed storage; it is
/// not this marker and cannot make a partially built baseline authoritative.
/// This record is published only after the baseline and its accepted frontier
/// are durable and a final byte-exact source observation has completed. SQLite
/// is deliberately absent because it is a disposable projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LazyGenesisActivationMarkerV1 {
    schema_version: u32,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    baseline_root: ContentDigest,
    source_capture: BlobDescription,
    accepted_frontier_digest: ContentDigest,
    watcher_fence: u64,
}

impl LazyGenesisActivationMarkerV1 {
    pub(crate) fn new(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        baseline_root: ContentDigest,
        source_capture: BlobDescription,
        accepted_frontier_digest: ContentDigest,
        watcher_fence: u64,
    ) -> io::Result<Self> {
        let marker = Self {
            schema_version: LAZY_GENESIS_ACTIVATION_MARKER_SCHEMA_VERSION,
            workspace_id,
            lineage_digest,
            baseline_root,
            source_capture,
            accepted_frontier_digest,
            watcher_fence,
        };
        marker.validate()?;
        Ok(marker)
    }

    pub(crate) fn encode(self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let body = postcard::to_allocvec(&self).map_err(|error| invalid(error.to_string()))?;
        let mut bytes = Vec::with_capacity(LAZY_GENESIS_ACTIVATION_MARKER_MAGIC.len() + body.len());
        bytes.extend_from_slice(LAZY_GENESIS_ACTIVATION_MARKER_MAGIC);
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        let body = bytes
            .strip_prefix(LAZY_GENESIS_ACTIVATION_MARKER_MAGIC)
            .ok_or_else(|| invalid("lazy genesis activation marker has invalid magic"))?;
        let marker: Self =
            postcard::from_bytes(body).map_err(|error| invalid(error.to_string()))?;
        marker.validate()?;
        if marker.encode()? != bytes {
            return Err(invalid(
                "lazy genesis activation marker is not canonically encoded",
            ));
        }
        Ok(marker)
    }

    fn validate(self) -> io::Result<()> {
        if self.schema_version != LAZY_GENESIS_ACTIVATION_MARKER_SCHEMA_VERSION {
            return Err(invalid(
                "lazy genesis activation marker has an unsupported schema",
            ));
        }
        Ok(())
    }

    pub(crate) const fn workspace_id(self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn lineage_digest(self) -> LineageDigest {
        self.lineage_digest
    }

    pub(crate) const fn baseline_root(self) -> ContentDigest {
        self.baseline_root
    }

    pub(crate) const fn source_capture(self) -> BlobDescription {
        self.source_capture
    }

    pub(crate) const fn accepted_frontier_digest(self) -> ContentDigest {
        self.accepted_frontier_digest
    }

    pub(crate) const fn watcher_fence(self) -> u64 {
        self.watcher_fence
    }
}

impl LazyGenesisCommitV1 {
    pub(crate) const fn root(self) -> ContentDigest {
        self.root
    }

    fn for_manifest(manifest: &LazyGenesisManifestV1, bytes: &[u8]) -> Self {
        Self {
            schema_version: LAZY_GENESIS_COMMIT_SCHEMA_VERSION,
            workspace_id: manifest.workspace_id,
            lineage_digest: manifest.lineage_digest,
            source_capture: manifest.source_capture,
            manifest: BlobDescription::of(bytes),
            root: lazy_genesis_manifest_root(bytes),
        }
    }

    fn encode(self) -> io::Result<Vec<u8>> {
        postcard::to_allocvec(&self).map_err(|error| invalid(error.to_string()))
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let commit: Self =
            postcard::from_bytes(bytes).map_err(|error| invalid(error.to_string()))?;
        if commit.schema_version != LAZY_GENESIS_COMMIT_SCHEMA_VERSION || commit.encode()? != bytes
        {
            return Err(invalid("lazy genesis commit is malformed or non-canonical"));
        }
        Ok(commit)
    }
}

pub(crate) struct LazyGenesisPackBuilder {
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    source_capture: BlobDescription,
    scratch: PathBuf,
    current: Vec<u8>,
    descriptors: Vec<LazyGenesisPageDescriptorV1>,
    segments: Vec<BlobDescription>,
    block_count: u64,
    seen_pages: BTreeSet<PageId>,
    seen_homes: BTreeSet<DocumentId>,
    seen_paths: BTreeSet<ManagedPath>,
    last_path: Option<ManagedPath>,
}

impl LazyGenesisPackBuilder {
    pub(crate) fn new(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        source_capture: BlobDescription,
        scratch_parent: &Path,
    ) -> io::Result<Self> {
        let scratch = scratch_parent.join(format!("tine-lazy-genesis-{}", Uuid::new_v4().simple()));
        fs::create_dir(&scratch)?;
        Ok(Self {
            workspace_id,
            lineage_digest,
            source_capture,
            scratch,
            current: Vec::with_capacity(LAZY_GENESIS_SEGMENT_TARGET_BYTES),
            descriptors: Vec::new(),
            segments: Vec::new(),
            block_count: 0,
            seen_pages: BTreeSet::new(),
            seen_homes: BTreeSet::new(),
            seen_paths: BTreeSet::new(),
            last_path: None,
        })
    }

    pub(crate) fn push(&mut self, input: LazyGenesisPageInput) -> io::Result<()> {
        if self.descriptors.len() == MAX_LAZY_GENESIS_PAGES {
            return Err(invalid("lazy genesis page-count cap exceeded"));
        }
        let document_dependencies = input
            .document_dependencies
            .clone()
            .ok_or_else(|| invalid("lazy genesis page has no sealed causal dependencies"))?;
        let capsule = LazyGenesisPageCapsuleV1::from_input(input)?;
        if document_dependencies.document_id() != capsule.home_document_id
            || !document_dependencies.direct_dependency_heads().is_empty()
        {
            return Err(invalid(
                "lazy genesis page dependencies do not name an unheaded home document",
            ));
        }
        if self
            .last_path
            .as_ref()
            .is_some_and(|last_path| last_path >= &capsule.path)
        {
            return Err(invalid(
                "lazy genesis pages are not in canonical path order",
            ));
        }
        if !self.seen_pages.insert(capsule.page_id)
            || !self.seen_homes.insert(capsule.home_document_id)
            || !self.seen_paths.insert(capsule.path.clone())
        {
            return Err(invalid("lazy genesis repeats page, home, or path identity"));
        }
        self.last_path = Some(capsule.path.clone());
        self.block_count = self
            .block_count
            .checked_add(capsule.blocks.len() as u64)
            .filter(|count| *count <= MAX_LAZY_GENESIS_BLOCKS)
            .ok_or_else(|| invalid("lazy genesis block-count cap exceeded"))?;
        let encoded = capsule.encode()?;
        let frame_bytes = 8_usize
            .checked_add(encoded.len())
            .ok_or_else(|| invalid("lazy genesis frame length overflow"))?;
        if !self.current.is_empty()
            && self.current.len().saturating_add(frame_bytes) > LAZY_GENESIS_SEGMENT_TARGET_BYTES
        {
            self.flush_segment()?;
        }
        let segment = u32::try_from(self.segments.len())
            .map_err(|_| invalid("lazy genesis segment count overflow"))?;
        let offset = self.current.len() as u64 + 8;
        self.current
            .extend_from_slice(&(encoded.len() as u64).to_be_bytes());
        self.current.extend_from_slice(&encoded);
        self.descriptors.push(LazyGenesisPageDescriptorV1 {
            page_id: capsule.page_id,
            home_document_id: capsule.home_document_id,
            path: capsule.path,
            capsule: BlobDescription::of(&encoded),
            segment,
            offset,
            length: encoded.len() as u64,
            blocks: capsule.blocks.len() as u32,
            document_dependencies,
        });
        if self.current.len() >= LAZY_GENESIS_SEGMENT_TARGET_BYTES {
            self.flush_segment()?;
        }
        Ok(())
    }

    fn flush_segment(&mut self) -> io::Result<()> {
        if self.current.is_empty() {
            return Ok(());
        }
        let description = BlobDescription::of(&self.current);
        let path = segment_path(&self.scratch, self.segments.len());
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(&self.current)?;
        self.current.clear();
        self.segments.push(description);
        Ok(())
    }

    pub(crate) fn finish(
        mut self,
        catalog_checkpoint: Vec<u8>,
        catalog_dependencies: Option<DocumentDependencies>,
    ) -> io::Result<LazyGenesisCandidate> {
        if catalog_checkpoint.is_empty()
            || catalog_checkpoint.len() > MAX_LAZY_GENESIS_CATALOG_CHECKPOINT_BYTES
        {
            return Err(invalid(
                "lazy genesis catalog checkpoint is empty or exceeds its fixed cap",
            ));
        }
        if catalog_dependencies.as_ref().is_some_and(|dependencies| {
            !dependencies.direct_dependency_heads().is_empty()
                || self
                    .descriptors
                    .iter()
                    .any(|page| page.home_document_id == dependencies.document_id())
        }) {
            return Err(invalid(
                "lazy genesis catalog dependencies are headed or alias a page home",
            ));
        }
        self.flush_segment()?;
        let catalog_description = BlobDescription::of(&catalog_checkpoint);
        let catalog_path = self.scratch.join("catalog.snapshot");
        let mut catalog_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(catalog_path)?;
        catalog_file.write_all(&catalog_checkpoint)?;
        let manifest = LazyGenesisManifestV1 {
            schema_version: LAZY_GENESIS_SCHEMA_VERSION,
            workspace_id: self.workspace_id,
            lineage_digest: self.lineage_digest,
            source_capture: self.source_capture,
            catalog_checkpoint: catalog_description,
            catalog_dependencies,
            page_count: self.descriptors.len() as u64,
            block_count: self.block_count,
            pages: std::mem::take(&mut self.descriptors),
            segments: self.segments.clone(),
        };
        validate_manifest(&manifest)?;
        let manifest_bytes =
            postcard::to_allocvec(&manifest).map_err(|error| invalid(error.to_string()))?;
        let root = lazy_genesis_manifest_root(&manifest_bytes);
        let index = manifest
            .pages
            .iter()
            .enumerate()
            .map(|(index, descriptor)| (descriptor.page_id, index))
            .collect();
        let home_index = manifest
            .pages
            .iter()
            .enumerate()
            .map(|(index, descriptor)| (descriptor.home_document_id, index))
            .collect();
        let scratch = std::mem::take(&mut self.scratch);
        Ok(LazyGenesisCandidate {
            scratch,
            manifest,
            manifest_bytes,
            root,
            index,
            home_index,
            cleanup_on_drop: true,
        })
    }
}

impl Drop for LazyGenesisPackBuilder {
    fn drop(&mut self) {
        if !self.scratch.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.scratch);
        }
    }
}

pub(crate) struct LazyGenesisCandidate {
    scratch: PathBuf,
    manifest: LazyGenesisManifestV1,
    manifest_bytes: Vec<u8>,
    root: ContentDigest,
    index: BTreeMap<PageId, usize>,
    home_index: BTreeMap<DocumentId, usize>,
    cleanup_on_drop: bool,
}

impl LazyGenesisCandidate {
    pub(crate) const fn root(&self) -> ContentDigest {
        self.root
    }

    pub(crate) fn frontier_binding(&self) -> io::Result<LazyGenesisFrontierBindingV1> {
        LazyGenesisFrontierBindingV1::new(self)
    }

    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.manifest.workspace_id
    }

    pub(crate) const fn lineage_digest(&self) -> LineageDigest {
        self.manifest.lineage_digest
    }

    pub(crate) fn manifest_bytes(&self) -> &[u8] {
        &self.manifest_bytes
    }

    pub(crate) fn page_count(&self) -> usize {
        self.manifest.pages.len()
    }

    pub(crate) fn page_ids(&self) -> impl Iterator<Item = PageId> + '_ {
        self.manifest.pages.iter().map(|page| page.page_id)
    }

    /// The complete causal baseline bound by the sealed manifest. These rows
    /// are small dependency records, not eagerly opened CRDT documents.
    pub(crate) fn frontier_documents(&self) -> Vec<DocumentDependencies> {
        let mut documents = Vec::with_capacity(self.manifest.pages.len() + 1);
        documents.extend(self.manifest.catalog_dependencies.iter().cloned());
        documents.extend(
            self.manifest
                .pages
                .iter()
                .map(|page| page.document_dependencies.clone()),
        );
        documents.sort_unstable_by_key(DocumentDependencies::document_id);
        documents
    }

    /// Resolve one causal baseline row without opening a CRDT checkpoint or a
    /// page capsule. Ordinary accepted events keep only rows that supersede
    /// this immutable baseline.
    pub(crate) fn frontier_document(
        &self,
        document_id: DocumentId,
    ) -> Option<DocumentDependencies> {
        if let Some(dependencies) = &self.manifest.catalog_dependencies {
            if dependencies.document_id() == document_id {
                return Some(dependencies.clone());
            }
        }
        self.home_index
            .get(&document_id)
            .map(|index| self.manifest.pages[*index].document_dependencies.clone())
    }

    /// Resolve immutable page ownership without decoding its capsule or the
    /// graph-wide catalog checkpoint. The sealed manifest has already proved
    /// uniqueness of both page and home identities.
    pub(crate) fn page_home_document_id(&self, page_id: PageId) -> Option<DocumentId> {
        self.index
            .get(&page_id)
            .map(|index| self.manifest.pages[*index].home_document_id)
    }

    pub(crate) const fn block_count(&self) -> u64 {
        self.manifest.block_count
    }

    pub(crate) fn page(&self, page_id: PageId) -> io::Result<Option<LazyGenesisPageInput>> {
        let Some(&index) = self.index.get(&page_id) else {
            return Ok(None);
        };
        let descriptor = &self.manifest.pages[index];
        let expected_segment = *self
            .manifest
            .segments
            .get(descriptor.segment as usize)
            .ok_or_else(|| invalid("lazy genesis descriptor names a missing segment"))?;
        let path = segment_path(&self.scratch, descriptor.segment as usize);
        if describe_file(&path)? != expected_segment {
            return Err(invalid("lazy genesis segment bytes changed"));
        }
        let mut file = fs::File::open(path)?;
        file.seek(SeekFrom::Start(descriptor.offset))?;
        let mut bytes = vec![0_u8; descriptor.length as usize];
        file.read_exact(&mut bytes)?;
        if BlobDescription::of(&bytes) != descriptor.capsule {
            return Err(invalid("lazy genesis page capsule bytes changed"));
        }
        let capsule = LazyGenesisPageCapsuleV1::decode(&bytes)?;
        if capsule.page_id != descriptor.page_id
            || capsule.home_document_id != descriptor.home_document_id
            || capsule.path != descriptor.path
            || capsule.blocks.len() as u32 != descriptor.blocks
        {
            return Err(invalid("lazy genesis descriptor and capsule disagree"));
        }
        Ok(Some(LazyGenesisPageInput {
            source_leaf: capsule.source_leaf,
            exact_source_bytes: capsule.exact_source_bytes,
            page_id: capsule.page_id,
            home_document_id: capsule.home_document_id,
            name: capsule.name,
            path: capsule.path,
            kind: capsule.kind,
            preamble: capsule.preamble,
            blocks: capsule.blocks,
            document_checkpoint: capsule.document_checkpoint,
            document_dependencies: Some(descriptor.document_dependencies.clone()),
        }))
    }

    pub(crate) fn catalog_checkpoint(&self) -> io::Result<Vec<u8>> {
        let bytes = fs::read(self.scratch.join("catalog.snapshot"))?;
        if bytes.len() > MAX_LAZY_GENESIS_CATALOG_CHECKPOINT_BYTES
            || BlobDescription::of(&bytes) != self.manifest.catalog_checkpoint
        {
            return Err(invalid("lazy genesis catalog checkpoint bytes changed"));
        }
        Ok(bytes)
    }

    pub(crate) fn document_checkpoint(
        &self,
        document_id: DocumentId,
    ) -> io::Result<Option<Vec<u8>>> {
        let Some(&index) = self.home_index.get(&document_id) else {
            return Ok(None);
        };
        let page_id = self.manifest.pages[index].page_id;
        Ok(self.page(page_id)?.map(|page| page.document_checkpoint))
    }

    pub(crate) fn stage_into(
        mut self,
        destination: &Path,
    ) -> io::Result<(Self, LazyGenesisCommitV1)> {
        if destination.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "lazy genesis destination already exists",
            ));
        }
        if self.manifest_bytes.len() > MAX_LAZY_GENESIS_MANIFEST_BYTES {
            return Err(invalid("lazy genesis manifest exceeds its fixed cap"));
        }
        let commit = LazyGenesisCommitV1::for_manifest(&self.manifest, &self.manifest_bytes);
        if commit.root() != self.root {
            return Err(invalid(
                "lazy genesis candidate root changed before staging",
            ));
        }
        write_new(
            &self.scratch.join(LAZY_GENESIS_MANIFEST_FILE),
            &self.manifest_bytes,
        )?;
        write_new(
            &self.scratch.join(LAZY_GENESIS_COMMIT_FILE),
            &commit.encode()?,
        )?;
        fs::rename(&self.scratch, destination)?;
        self.scratch = destination.to_path_buf();
        Ok((self, commit))
    }

    pub(crate) fn relocate_after_parent_move(mut self, destination: &Path) -> io::Result<Self> {
        if self.scratch.exists() || !destination.is_dir() {
            return Err(invalid(
                "lazy genesis same-process relocation does not match the sealed parent move",
            ));
        }
        self.scratch = destination.to_path_buf();
        self.cleanup_on_drop = false;
        Ok(self)
    }

    #[allow(dead_code)]
    pub(crate) fn open_sealed(directory: &Path, expected: LazyGenesisCommitV1) -> io::Result<Self> {
        let manifest_bytes = read_bounded(
            &directory.join(LAZY_GENESIS_MANIFEST_FILE),
            MAX_LAZY_GENESIS_MANIFEST_BYTES,
        )?;
        let commit_bytes = read_bounded(&directory.join(LAZY_GENESIS_COMMIT_FILE), 1024)?;
        let commit = LazyGenesisCommitV1::decode(&commit_bytes)?;
        if commit != expected
            || commit.manifest != BlobDescription::of(&manifest_bytes)
            || commit.root != lazy_genesis_manifest_root(&manifest_bytes)
        {
            return Err(invalid(
                "lazy genesis sealed commit does not bind its manifest",
            ));
        }
        let manifest: LazyGenesisManifestV1 =
            postcard::from_bytes(&manifest_bytes).map_err(|error| invalid(error.to_string()))?;
        validate_manifest(&manifest)?;
        if manifest.workspace_id != commit.workspace_id
            || manifest.lineage_digest != commit.lineage_digest
            || manifest.source_capture != commit.source_capture
        {
            return Err(invalid(
                "lazy genesis commit and manifest identity disagree",
            ));
        }
        let index = manifest
            .pages
            .iter()
            .enumerate()
            .map(|(index, page)| (page.page_id, index))
            .collect();
        let home_index = manifest
            .pages
            .iter()
            .enumerate()
            .map(|(index, descriptor)| (descriptor.home_document_id, index))
            .collect();
        Ok(Self {
            scratch: directory.to_path_buf(),
            manifest,
            manifest_bytes,
            root: commit.root,
            index,
            home_index,
            cleanup_on_drop: false,
        })
    }
}

impl Drop for LazyGenesisCandidate {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = fs::remove_dir_all(&self.scratch);
        }
    }
}

fn validate_manifest(manifest: &LazyGenesisManifestV1) -> io::Result<()> {
    let declared_blocks = manifest.pages.iter().try_fold(0_u64, |total, page| {
        total
            .checked_add(u64::from(page.blocks))
            .ok_or_else(|| invalid("lazy genesis manifest block count overflows"))
    })?;
    if manifest.schema_version != LAZY_GENESIS_SCHEMA_VERSION
        || manifest.page_count != manifest.pages.len() as u64
        || manifest
            .pages
            .windows(2)
            .any(|pair| pair[0].path >= pair[1].path)
        || declared_blocks != manifest.block_count
    {
        return Err(invalid("lazy genesis manifest is malformed"));
    }
    let mut pages = BTreeSet::new();
    let mut homes = BTreeSet::new();
    let mut dependency_documents = BTreeSet::new();
    if manifest
        .catalog_dependencies
        .as_ref()
        .is_some_and(|dependencies| {
            !dependencies.direct_dependency_heads().is_empty()
                || !dependency_documents.insert(dependencies.document_id())
        })
    {
        return Err(invalid("lazy genesis catalog dependencies are headed"));
    }
    for page in &manifest.pages {
        if !pages.insert(page.page_id)
            || !homes.insert(page.home_document_id)
            || page.document_dependencies.document_id() != page.home_document_id
            || !page
                .document_dependencies
                .direct_dependency_heads()
                .is_empty()
            || !dependency_documents.insert(page.document_dependencies.document_id())
            || page.segment as usize >= manifest.segments.len()
            || page.length > MAX_LAZY_GENESIS_CAPSULE_BYTES as u64
        {
            return Err(invalid("lazy genesis manifest identity is malformed"));
        }
    }
    Ok(())
}

fn segment_path(root: &Path, index: usize) -> PathBuf {
    root.join(format!("segment-{index:08}.pack"))
}

fn lazy_genesis_manifest_root(bytes: &[u8]) -> ContentDigest {
    let mut hasher = Sha256::new();
    hasher.update(b"tine/lazy-genesis/root/v1\0");
    hasher.update(bytes);
    ContentDigest::from_bytes(hasher.finalize().into())
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)
}

fn read_bounded(path: &Path, cap: usize) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > cap as u64 {
        return Err(invalid(
            "lazy genesis sealed file is not a bounded regular file",
        ));
    }
    fs::read(path)
}

fn describe_file(path: &Path) -> io::Result<BlobDescription> {
    let bytes = fs::read(path)?;
    Ok(BlobDescription::of(&bytes))
}

fn invalid(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(ordinal: u128, path: &str, blocks: usize) -> LazyGenesisPageInput {
        let home = DocumentId::from_uuid(Uuid::from_u128(10_000 + ordinal));
        let source_digest = ContentDigest::of(path.as_bytes());
        LazyGenesisPageInput {
            source_leaf: *source_digest.as_bytes(),
            exact_source_bytes: blocks as u64 * 8,
            page_id: PageId::from_uuid(Uuid::from_u128(ordinal)),
            home_document_id: home,
            name: path.to_owned(),
            path: ManagedPath::parse(path).unwrap(),
            kind: ManagedTextKind::Page,
            preamble: None,
            document_checkpoint: vec![ordinal as u8, blocks as u8, 0x47],
            document_dependencies: Some(
                DocumentDependencies::new(
                    home,
                    vec![super::super::CrdtPeerCounter::new(
                        super::super::CrdtPeerId::from_u64(7),
                        ordinal as u64,
                    )],
                    Vec::new(),
                )
                .unwrap(),
            ),
            blocks: (0..blocks)
                .map(|index| LazyGenesisBlockInput {
                    block_id: BlockId::from_uuid(Uuid::from_u128(
                        100_000 + ordinal * 100 + index as u128,
                    )),
                    home_document_id: home,
                    parent: None,
                    order: format!("{index:08}"),
                    content: format!("block {index}"),
                    external_uuid_claims: Vec::new(),
                })
                .collect(),
        }
    }

    fn catalog_dependencies() -> DocumentDependencies {
        DocumentDependencies::new(
            DocumentId::from_uuid(Uuid::from_u128(99_999)),
            vec![super::super::CrdtPeerCounter::new(
                super::super::CrdtPeerId::from_u64(7),
                1,
            )],
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn lazy_genesis_pack_is_deterministic_bounded_and_point_readable() {
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
        let lineage = LineageDigest::of(b"lazy-genesis-test");
        let source = BlobDescription::of(b"capture");
        let build = || {
            let mut builder =
                LazyGenesisPackBuilder::new(workspace, lineage, source, &std::env::temp_dir())
                    .unwrap();
            let pages = vec![page(1, "pages/a.md", 2), page(2, "pages/b.org", 1)];
            for page in pages {
                builder.push(page).unwrap();
            }
            builder
                .finish(vec![0x43, 0x41, 0x54], Some(catalog_dependencies()))
                .unwrap()
        };
        let first = build();
        let second = build();
        assert_eq!(first.root(), second.root());
        assert_eq!(first.manifest_bytes(), second.manifest_bytes());
        assert_eq!(first.page_count(), 2);
        assert_eq!(first.block_count(), 3);
        let read = first
            .page(PageId::from_uuid(Uuid::from_u128(2)))
            .unwrap()
            .unwrap();
        assert_eq!(read.path.as_str(), "pages/b.org");
        assert_eq!(read.blocks.len(), 1);
    }

    #[test]
    fn activation_marker_is_minimal_canonical_and_excludes_sqlite() {
        let marker = LazyGenesisActivationMarkerV1::new(
            WorkspaceId::from_uuid(Uuid::from_u128(1)),
            LineageDigest::of(b"activation-marker-lineage"),
            ContentDigest::of(b"baseline"),
            BlobDescription::of(b"source capture"),
            ContentDigest::of(b"accepted frontier"),
            73,
        )
        .unwrap();
        let encoded = marker.encode().unwrap();
        assert_eq!(
            LazyGenesisActivationMarkerV1::decode(&encoded).unwrap(),
            marker
        );
        assert_eq!(marker.watcher_fence(), 73);
        assert_eq!(
            marker.workspace_id(),
            WorkspaceId::from_uuid(Uuid::from_u128(1))
        );
        assert_eq!(marker.baseline_root(), ContentDigest::of(b"baseline"));
        assert_eq!(
            marker.source_capture(),
            BlobDescription::of(b"source capture")
        );
        assert_eq!(
            marker.accepted_frontier_digest(),
            ContentDigest::of(b"accepted frontier")
        );
        assert_eq!(
            marker.lineage_digest(),
            LineageDigest::of(b"activation-marker-lineage")
        );

        let source = include_str!("lazy_genesis.rs");
        let marker_source = source
            .split_once("pub(crate) struct LazyGenesisActivationMarkerV1")
            .and_then(|(_, tail)| tail.split_once("impl LazyGenesisActivationMarkerV1"))
            .map(|(body, _)| body)
            .expect("activation marker definition must remain identifiable");
        assert!(!marker_source.contains("sqlite"));
        assert!(!marker_source.contains("database"));

        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        assert!(contract.contains("one final lazy-genesis authority marker"));
        assert!(contract.contains("SQLite identity is deliberately absent"));
        assert!(contract.contains("Tauri binding records opt-in\nintent"));
    }

    #[test]
    fn lazy_genesis_rejects_cross_page_parent_and_duplicate_identity() {
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
        let lineage = LineageDigest::of(b"lazy-genesis-test");
        let mut builder = LazyGenesisPackBuilder::new(
            workspace,
            lineage,
            BlobDescription::of(b"capture"),
            &std::env::temp_dir(),
        )
        .unwrap();
        let mut invalid_page = page(1, "pages/a.md", 1);
        invalid_page.blocks[0].parent = Some(BlockId::from_uuid(Uuid::from_u128(999)));
        assert!(builder.push(invalid_page).is_err());

        let mut builder = LazyGenesisPackBuilder::new(
            workspace,
            lineage,
            BlobDescription::of(b"capture"),
            &std::env::temp_dir(),
        )
        .unwrap();
        builder.push(page(2, "pages/b.md", 0)).unwrap();
        assert!(builder.push(page(1, "pages/a.md", 0)).is_err());
    }

    #[test]
    fn lazy_genesis_seal_reopens_and_detects_payload_corruption() {
        let parent = std::env::temp_dir().join(format!(
            "tine-lazy-genesis-seal-test-{}",
            Uuid::new_v4().simple()
        ));
        let moved_parent = parent.with_extension("sealed");
        fs::create_dir(&parent).unwrap();
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
        let lineage = LineageDigest::of(b"lazy-genesis-seal-test");
        let mut builder = LazyGenesisPackBuilder::new(
            workspace,
            lineage,
            BlobDescription::of(b"capture"),
            &parent,
        )
        .unwrap();
        builder.push(page(1, "pages/a.md", 1)).unwrap();
        let candidate = builder
            .finish(vec![0x43, 0x41, 0x54], Some(catalog_dependencies()))
            .unwrap();
        let (candidate, commit) = candidate.stage_into(&parent.join("genesis")).unwrap();
        fs::rename(&parent, &moved_parent).unwrap();
        let candidate = candidate
            .relocate_after_parent_move(&moved_parent.join("genesis"))
            .unwrap();
        assert_eq!(candidate.root(), commit.root());
        drop(candidate);

        let reopened =
            LazyGenesisCandidate::open_sealed(&moved_parent.join("genesis"), commit).unwrap();
        assert_eq!(reopened.page_count(), 1);
        let segment = segment_path(&moved_parent.join("genesis"), 0);
        fs::OpenOptions::new()
            .append(true)
            .open(segment)
            .unwrap()
            .write_all(b"corrupt")
            .unwrap();
        assert!(reopened
            .page(PageId::from_uuid(Uuid::from_u128(1)))
            .is_err());
        drop(reopened);
        fs::remove_dir_all(moved_parent).unwrap();
    }
}
