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
    BlobDescription, BlockId, ContentDigest, DocumentId, LineageDigest, LogseqUuid, ManagedPath,
    ManagedTextKind, PageId, WorkspaceId,
};

const LAZY_GENESIS_SCHEMA_VERSION: u32 = 1;
const LAZY_GENESIS_SEGMENT_TARGET_BYTES: usize = 32 * 1024 * 1024;
const MAX_LAZY_GENESIS_CAPSULE_BYTES: usize = 256 * 1024 * 1024;
const MAX_LAZY_GENESIS_PAGES: usize = 1_000_000;
const MAX_LAZY_GENESIS_BLOCKS: u64 = 100_000_000;

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
        };
        capsule.validate()?;
        Ok(capsule)
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema_version != LAZY_GENESIS_SCHEMA_VERSION || self.name.is_empty() {
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LazyGenesisManifestV1 {
    schema_version: u32,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    source_capture: BlobDescription,
    pages: Vec<LazyGenesisPageDescriptorV1>,
    segments: Vec<BlobDescription>,
    page_count: u64,
    block_count: u64,
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
    ) -> io::Result<Self> {
        let scratch =
            std::env::temp_dir().join(format!("tine-lazy-genesis-{}", Uuid::new_v4().simple()));
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
        let capsule = LazyGenesisPageCapsuleV1::from_input(input)?;
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

    pub(crate) fn finish(mut self) -> io::Result<LazyGenesisCandidate> {
        self.flush_segment()?;
        let manifest = LazyGenesisManifestV1 {
            schema_version: LAZY_GENESIS_SCHEMA_VERSION,
            workspace_id: self.workspace_id,
            lineage_digest: self.lineage_digest,
            source_capture: self.source_capture,
            page_count: self.descriptors.len() as u64,
            block_count: self.block_count,
            pages: std::mem::take(&mut self.descriptors),
            segments: self.segments.clone(),
        };
        validate_manifest(&manifest)?;
        let manifest_bytes =
            postcard::to_allocvec(&manifest).map_err(|error| invalid(error.to_string()))?;
        let root = ContentDigest::from_bytes(
            Sha256::digest(
                [
                    b"tine/lazy-genesis/root/v1\0".as_slice(),
                    manifest_bytes.as_slice(),
                ]
                .concat(),
            )
            .into(),
        );
        let index = manifest
            .pages
            .iter()
            .enumerate()
            .map(|(index, descriptor)| (descriptor.page_id, index))
            .collect();
        let scratch = std::mem::take(&mut self.scratch);
        Ok(LazyGenesisCandidate {
            scratch,
            manifest,
            manifest_bytes,
            root,
            index,
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
}

impl LazyGenesisCandidate {
    pub(crate) const fn root(&self) -> ContentDigest {
        self.root
    }

    pub(crate) fn manifest_bytes(&self) -> &[u8] {
        &self.manifest_bytes
    }

    pub(crate) fn page_count(&self) -> usize {
        self.manifest.pages.len()
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
        }))
    }
}

impl Drop for LazyGenesisCandidate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.scratch);
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
    for page in &manifest.pages {
        if !pages.insert(page.page_id)
            || !homes.insert(page.home_document_id)
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

    #[test]
    fn lazy_genesis_pack_is_deterministic_bounded_and_point_readable() {
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
        let lineage = LineageDigest::of(b"lazy-genesis-test");
        let source = BlobDescription::of(b"capture");
        let build = || {
            let mut builder = LazyGenesisPackBuilder::new(workspace, lineage, source).unwrap();
            let pages = vec![page(1, "pages/a.md", 2), page(2, "pages/b.org", 1)];
            for page in pages {
                builder.push(page).unwrap();
            }
            builder.finish().unwrap()
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
    fn lazy_genesis_rejects_cross_page_parent_and_duplicate_identity() {
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
        let lineage = LineageDigest::of(b"lazy-genesis-test");
        let mut builder =
            LazyGenesisPackBuilder::new(workspace, lineage, BlobDescription::of(b"capture"))
                .unwrap();
        let mut invalid_page = page(1, "pages/a.md", 1);
        invalid_page.blocks[0].parent = Some(BlockId::from_uuid(Uuid::from_u128(999)));
        assert!(builder.push(invalid_page).is_err());

        let mut builder =
            LazyGenesisPackBuilder::new(workspace, lineage, BlobDescription::of(b"capture"))
                .unwrap();
        builder.push(page(2, "pages/b.md", 0)).unwrap();
        assert!(builder.push(page(1, "pages/a.md", 0)).is_err());
    }
}
