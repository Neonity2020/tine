#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};

use super::object_store::{
    publish_immutable_exact, read_optional_regular, DetachedBootstrapImmutablePublisher, StoreError,
};
use super::ContentDigest;

const NODE_SCHEMA_VERSION: u32 = 1;
const MAX_KEY_BYTES: usize = 96;
const MAX_KEY_BITS: usize = MAX_KEY_BYTES * 8;
// Values are one immutable introduction each. Accumulated per-UUID history is
// structurally sharded across Patricia leaves and therefore never approaches
// this per-event corruption bound.
const MAX_VALUE_BYTES: usize = 4 * 1024;
const MAX_NODE_BYTES: u64 = 128 * 1024;
const NODE_SUFFIX: &str = ".patricia-node";

// Private bootstrap construction keeps newly addressed nodes hot across part
// boundaries. Once this conservative encoded-size budget is crossed, every
// staged node is immutable-published and the buffer is cleared. This is a
// single-use construction buffer, not a second persistent cache.
pub(crate) const MAX_PATRICIA_CONSTRUCTION_RESIDENT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PatriciaIndexRoot(ContentDigest);

impl PatriciaIndexRoot {
    pub fn empty() -> Self {
        Self(ContentDigest::of(
            b"tine/authenticated-content-addressed-patricia/v1/empty",
        ))
    }

    pub const fn digest(self) -> ContentDigest {
        self.0
    }

    pub(crate) const fn from_digest(digest: ContentDigest) -> Self {
        Self(digest)
    }
}

impl Default for PatriciaIndexRoot {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PatriciaIndexStats {
    pub reads: usize,
    pub writes: usize,
    pub bytes_read: usize,
    pub bytes_written: usize,
}

#[derive(Debug, Default)]
struct Counters {
    reads: AtomicUsize,
    writes: AtomicUsize,
    bytes_read: AtomicUsize,
    bytes_written: AtomicUsize,
}

#[derive(Debug)]
pub(crate) struct PatriciaIndexStore {
    nodes: Dir,
    detached_publisher: Option<DetachedBootstrapImmutablePublisher>,
    counters: Counters,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Node {
    Leaf {
        schema_version: u32,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Branch {
        schema_version: u32,
        prefix: Vec<u8>,
        prefix_bit_len: u16,
        left: ContentDigest,
        right: ContentDigest,
    },
}

#[derive(Clone, Debug)]
struct ChildPathConstraint {
    parent_prefix: Vec<u8>,
    parent_prefix_bit_len: usize,
    right: bool,
}

#[derive(Debug)]
struct BranchFrame {
    prefix: Vec<u8>,
    prefix_bit_len: u16,
    left: ContentDigest,
    right: ContentDigest,
    rightward: bool,
}

#[derive(Debug, Default)]
struct StagedNodes {
    nodes: BTreeMap<ContentDigest, Node>,
    encoded_bytes: usize,
}

impl StagedNodes {
    fn stage(&mut self, node: Node) -> Result<ContentDigest, StoreError> {
        validate_node(&node)?;
        let bytes =
            postcard::to_allocvec(&node).map_err(|_| StoreError::MalformedLogseqClaimIndex)?;
        if bytes.len() as u64 > MAX_NODE_BYTES {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        let digest = ContentDigest::of(&bytes);
        if let std::collections::btree_map::Entry::Vacant(entry) = self.nodes.entry(digest) {
            self.encoded_bytes = self
                .encoded_bytes
                .checked_add(bytes.len())
                .ok_or(StoreError::MalformedLogseqClaimIndex)?;
            entry.insert(node);
        }
        Ok(digest)
    }

    fn clear(&mut self) {
        self.nodes.clear();
        self.encoded_bytes = 0;
    }
}

/// Single-use node construction shared by every checkpoint of one private
/// bootstrap session. Roots remain ordinary Patricia roots; only publication
/// timing changes.
#[derive(Debug, Default)]
pub(crate) struct PatriciaIndexConstruction {
    staged: StagedNodes,
    checkpoint_roots: BTreeSet<ContentDigest>,
    peak_resident_bytes: usize,
    flushes: usize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PatriciaIndexConstructionStats {
    pub(crate) peak_resident_bytes: usize,
    pub(crate) flushes: usize,
}

impl PatriciaIndexConstruction {
    pub(crate) fn checkpoint(&mut self, roots: impl IntoIterator<Item = PatriciaIndexRoot>) {
        self.checkpoint_roots
            .extend(roots.into_iter().map(PatriciaIndexRoot::digest));
    }

    fn note_residency(&mut self) {
        self.peak_resident_bytes = self.peak_resident_bytes.max(self.staged.encoded_bytes);
    }

    fn flush_if_over_budget(&mut self, store: &PatriciaIndexStore) -> Result<(), StoreError> {
        self.note_residency();
        if self.staged.encoded_bytes > MAX_PATRICIA_CONSTRUCTION_RESIDENT_BYTES {
            store.publish_all_staged(&self.staged)?;
            self.staged.clear();
            self.flushes = self.flushes.saturating_add(1);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> PatriciaIndexConstructionStats {
        PatriciaIndexConstructionStats {
            peak_resident_bytes: self.peak_resident_bytes,
            flushes: self.flushes,
        }
    }
}

impl PatriciaIndexStore {
    pub(crate) fn new(nodes: Dir) -> Self {
        Self {
            nodes,
            detached_publisher: None,
            counters: Counters::default(),
        }
    }

    pub(crate) fn for_detached_bootstrap(
        &self,
        publisher: DetachedBootstrapImmutablePublisher,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            nodes: self.nodes.try_clone()?,
            detached_publisher: Some(publisher),
            counters: Counters::default(),
        })
    }

    pub(crate) fn stats(&self) -> PatriciaIndexStats {
        PatriciaIndexStats {
            reads: self.counters.reads.load(Ordering::Relaxed),
            writes: self.counters.writes.load(Ordering::Relaxed),
            bytes_read: self.counters.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.counters.bytes_written.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn validate_root(&self, root: PatriciaIndexRoot) -> Result<(), StoreError> {
        if root == PatriciaIndexRoot::empty() {
            return Ok(());
        }
        self.read_node(root.digest()).map(|_| ())
    }

    pub(crate) fn lookup(
        &self,
        root: PatriciaIndexRoot,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StoreError> {
        validate_key(key)?;
        if root == PatriciaIndexRoot::empty() {
            return Ok(None);
        }
        let mut digest = root.digest();
        let mut constraint = None;
        let mut remaining_nodes = traversal_node_budget(key.len())?;
        loop {
            remaining_nodes = consume_node_budget(remaining_nodes)?;
            let node = self.read_node(digest)?;
            validate_node_path(&node, constraint.as_ref())?;
            match node {
                Node::Leaf {
                    key: found, value, ..
                } => return Ok((found == key).then_some(value)),
                Node::Branch {
                    prefix,
                    prefix_bit_len,
                    left,
                    right,
                    ..
                } => {
                    let split = prefix_bit_len as usize;
                    if !prefix_matches(key, &prefix, split)? {
                        return Ok(None);
                    }
                    let rightward = key_bit(key, split)?;
                    digest = if rightward { right } else { left };
                    constraint = Some(ChildPathConstraint {
                        parent_prefix: prefix,
                        parent_prefix_bit_len: split,
                        right: rightward,
                    });
                }
            }
        }
    }

    #[allow(dead_code)] // consumed by the intentionally unwired P2N2 foundation
    pub(crate) fn lookup_many(
        &self,
        root: PatriciaIndexRoot,
        keys: &[Vec<u8>],
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        keys.iter()
            .filter_map(|key| {
                self.lookup(root, key)
                    .transpose()
                    .map(|result| result.map(|value| (key.clone(), value)))
            })
            .collect()
    }

    pub(crate) fn lookup_prefix(
        &self,
        root: PatriciaIndexRoot,
        prefix: &[u8],
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
        self.lookup_prefix_limited(root, prefix, usize::MAX)
    }

    pub(crate) fn lookup_prefix_limited(
        &self,
        root: PatriciaIndexRoot,
        prefix: &[u8],
        limit: usize,
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
        validate_key(prefix)?;
        let mut found = BTreeMap::new();
        if root == PatriciaIndexRoot::empty() || limit == 0 {
            return Ok(found);
        }
        self.collect_prefix(root.digest(), prefix, limit, &mut found)?;
        Ok(found)
    }

    pub(crate) fn visit_all(
        &self,
        root: PatriciaIndexRoot,
        mut visit: impl FnMut(&[u8], &[u8]) -> bool,
    ) -> Result<(), StoreError> {
        if root == PatriciaIndexRoot::empty() {
            return Ok(());
        }
        let budget = traversal_node_budget(MAX_KEY_BYTES)?;
        let mut pending = vec![(root.digest(), None, budget)];
        while let Some((digest, constraint, remaining_nodes)) = pending.pop() {
            let remaining_nodes = consume_node_budget(remaining_nodes)?;
            let node = self.read_node(digest)?;
            validate_node_path(&node, constraint.as_ref())?;
            match node {
                Node::Leaf { key, value, .. } => {
                    if !visit(&key, &value) {
                        return Ok(());
                    }
                }
                Node::Branch {
                    prefix,
                    prefix_bit_len,
                    left,
                    right,
                    ..
                } => {
                    let split = prefix_bit_len as usize;
                    pending.push((
                        right,
                        Some(ChildPathConstraint {
                            parent_prefix: prefix.clone(),
                            parent_prefix_bit_len: split,
                            right: true,
                        }),
                        remaining_nodes,
                    ));
                    pending.push((
                        left,
                        Some(ChildPathConstraint {
                            parent_prefix: prefix,
                            parent_prefix_bit_len: split,
                            right: false,
                        }),
                        remaining_nodes,
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn insert_many(
        &self,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        let (root, staged) = self.stage_many(root, records)?;
        self.publish_staged_reachable(root, &staged)?;
        Ok(root)
    }

    pub(crate) fn insert_many_verify_existing(
        &self,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        let (root, staged) = self.stage_many(root, records)?;
        self.verify_staged_reachable(root, &staged)?;
        Ok(root)
    }

    fn stage_many(
        &self,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<(PatriciaIndexRoot, StagedNodes), StoreError> {
        for (key, value) in records {
            validate_record(key, value)?;
        }
        let mut root = root;
        let mut staged = StagedNodes::default();
        for (key, value) in records {
            root = PatriciaIndexRoot(self.insert_staged(root, key, value, &mut staged)?);
        }
        Ok((root, staged))
    }

    pub(crate) fn construction_lookup(
        &self,
        construction: &PatriciaIndexConstruction,
        root: PatriciaIndexRoot,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StoreError> {
        validate_key(key)?;
        if root == PatriciaIndexRoot::empty() {
            return Ok(None);
        }
        let mut digest = root.digest();
        let mut constraint = None;
        let mut remaining_nodes = traversal_node_budget(key.len())?;
        loop {
            remaining_nodes = consume_node_budget(remaining_nodes)?;
            let node = self.read_staged_or_persisted(digest, &construction.staged)?;
            validate_node_path(&node, constraint.as_ref())?;
            match node {
                Node::Leaf {
                    key: found, value, ..
                } => return Ok((found == key).then_some(value)),
                Node::Branch {
                    prefix,
                    prefix_bit_len,
                    left,
                    right,
                    ..
                } => {
                    let split = prefix_bit_len as usize;
                    if !prefix_matches(key, &prefix, split)? {
                        return Ok(None);
                    }
                    let rightward = key_bit(key, split)?;
                    digest = if rightward { right } else { left };
                    constraint = Some(ChildPathConstraint {
                        parent_prefix: prefix,
                        parent_prefix_bit_len: split,
                        right: rightward,
                    });
                }
            }
        }
    }

    pub(crate) fn construction_insert_many(
        &self,
        construction: &mut PatriciaIndexConstruction,
        mut root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        for (key, value) in records {
            validate_record(key, value)?;
            root = PatriciaIndexRoot(self.insert_staged(
                root,
                key,
                value,
                &mut construction.staged,
            )?);
            construction.flush_if_over_budget(self)?;
        }
        Ok(root)
    }

    pub(crate) fn construction_remove_many(
        &self,
        construction: &mut PatriciaIndexConstruction,
        mut root: PatriciaIndexRoot,
        keys: &[Vec<u8>],
    ) -> Result<PatriciaIndexRoot, StoreError> {
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        for key in keys {
            validate_key(key)?;
            root = self.remove_constructed(construction, root, key)?;
            construction.flush_if_over_budget(self)?;
        }
        Ok(root)
    }

    pub(crate) fn finish_construction(
        &self,
        construction: &mut PatriciaIndexConstruction,
    ) -> Result<(), StoreError> {
        construction.note_residency();
        self.publish_staged_roots(&construction.checkpoint_roots, &construction.staged)?;
        construction.staged.clear();
        Ok(())
    }

    pub(crate) fn remove_many(
        &self,
        root: PatriciaIndexRoot,
        keys: &[Vec<u8>],
    ) -> Result<PatriciaIndexRoot, StoreError> {
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        let mut root = root;
        for key in keys {
            validate_key(key)?;
            root = self.remove(root, key)?;
        }
        Ok(root)
    }

    fn remove(&self, root: PatriciaIndexRoot, key: &[u8]) -> Result<PatriciaIndexRoot, StoreError> {
        if root == PatriciaIndexRoot::empty() {
            return Ok(root);
        }
        let mut digest = root.digest();
        let mut constraint = None;
        let mut remaining_nodes = traversal_node_budget(key.len())?;
        let mut ancestors = Vec::new();
        loop {
            remaining_nodes = consume_node_budget(remaining_nodes)?;
            let node = self.read_node(digest)?;
            validate_node_path(&node, constraint.as_ref())?;
            match node {
                Node::Leaf { key: found, .. } => {
                    if found != key {
                        return Ok(root);
                    }
                    break;
                }
                Node::Branch {
                    prefix,
                    prefix_bit_len,
                    left,
                    right,
                    ..
                } => {
                    let split = prefix_bit_len as usize;
                    if !prefix_matches(key, &prefix, split)? {
                        return Ok(root);
                    }
                    let rightward = key_bit(key, split)?;
                    digest = if rightward { right } else { left };
                    constraint = Some(ChildPathConstraint {
                        parent_prefix: prefix.clone(),
                        parent_prefix_bit_len: split,
                        right: rightward,
                    });
                    ancestors.push(BranchFrame {
                        prefix,
                        prefix_bit_len,
                        left,
                        right,
                        rightward,
                    });
                }
            }
        }

        let Some(parent) = ancestors.pop() else {
            return Ok(PatriciaIndexRoot::empty());
        };
        let replacement = if parent.rightward {
            parent.left
        } else {
            parent.right
        };
        let rebuilt = ancestors
            .into_iter()
            .rev()
            .try_fold(replacement, |child, ancestor| {
                let (left, right) = if ancestor.rightward {
                    (ancestor.left, child)
                } else {
                    (child, ancestor.right)
                };
                self.publish_node(&Node::Branch {
                    schema_version: NODE_SCHEMA_VERSION,
                    prefix: ancestor.prefix,
                    prefix_bit_len: ancestor.prefix_bit_len,
                    left,
                    right,
                })
            })?;
        Ok(PatriciaIndexRoot(rebuilt))
    }

    fn remove_constructed(
        &self,
        construction: &mut PatriciaIndexConstruction,
        root: PatriciaIndexRoot,
        key: &[u8],
    ) -> Result<PatriciaIndexRoot, StoreError> {
        if root == PatriciaIndexRoot::empty() {
            return Ok(root);
        }
        let mut digest = root.digest();
        let mut constraint = None;
        let mut remaining_nodes = traversal_node_budget(key.len())?;
        let mut ancestors = Vec::new();
        loop {
            remaining_nodes = consume_node_budget(remaining_nodes)?;
            let node = self.read_staged_or_persisted(digest, &construction.staged)?;
            validate_node_path(&node, constraint.as_ref())?;
            match node {
                Node::Leaf { key: found, .. } => {
                    if found != key {
                        return Ok(root);
                    }
                    break;
                }
                Node::Branch {
                    prefix,
                    prefix_bit_len,
                    left,
                    right,
                    ..
                } => {
                    let split = prefix_bit_len as usize;
                    if !prefix_matches(key, &prefix, split)? {
                        return Ok(root);
                    }
                    let rightward = key_bit(key, split)?;
                    digest = if rightward { right } else { left };
                    constraint = Some(ChildPathConstraint {
                        parent_prefix: prefix.clone(),
                        parent_prefix_bit_len: split,
                        right: rightward,
                    });
                    ancestors.push(BranchFrame {
                        prefix,
                        prefix_bit_len,
                        left,
                        right,
                        rightward,
                    });
                }
            }
        }

        let Some(parent) = ancestors.pop() else {
            return Ok(PatriciaIndexRoot::empty());
        };
        let replacement = if parent.rightward {
            parent.left
        } else {
            parent.right
        };
        let rebuilt = ancestors
            .into_iter()
            .rev()
            .try_fold(replacement, |child, ancestor| {
                let (left, right) = if ancestor.rightward {
                    (ancestor.left, child)
                } else {
                    (child, ancestor.right)
                };
                construction.staged.stage(Node::Branch {
                    schema_version: NODE_SCHEMA_VERSION,
                    prefix: ancestor.prefix,
                    prefix_bit_len: ancestor.prefix_bit_len,
                    left,
                    right,
                })
            })?;
        Ok(PatriciaIndexRoot(rebuilt))
    }

    fn insert_staged(
        &self,
        root: PatriciaIndexRoot,
        key: &[u8],
        value: &[u8],
        staged: &mut StagedNodes,
    ) -> Result<ContentDigest, StoreError> {
        if root == PatriciaIndexRoot::empty() {
            return staged.stage(Node::Leaf {
                schema_version: NODE_SCHEMA_VERSION,
                key: key.to_vec(),
                value: value.to_vec(),
            });
        }
        self.insert_at_staged(root.digest(), key, value, staged)
    }

    fn insert_at_staged(
        &self,
        root: ContentDigest,
        key: &[u8],
        value: &[u8],
        staged: &mut StagedNodes,
    ) -> Result<ContentDigest, StoreError> {
        let mut digest = root;
        let mut constraint = None;
        let mut remaining_nodes = traversal_node_budget(key.len())?;
        let mut ancestors = Vec::new();
        let replacement = loop {
            remaining_nodes = consume_node_budget(remaining_nodes)?;
            let node = self.read_staged_or_persisted(digest, staged)?;
            validate_node_path(&node, constraint.as_ref())?;
            let node_prefix = node_prefix(&node);
            let node_prefix_bits = node_prefix_bits(&node)?;
            let shared = common_prefix_bits(key, node_prefix, node_prefix_bits)?;
            if shared < node_prefix_bits {
                let leaf = staged.stage(Node::Leaf {
                    schema_version: NODE_SCHEMA_VERSION,
                    key: key.to_vec(),
                    value: value.to_vec(),
                })?;
                break Self::stage_split(staged, key, shared, digest, node_prefix, leaf)?;
            }

            match node {
                Node::Leaf {
                    key: found_key,
                    value: found_value,
                    ..
                } => {
                    if found_key == key {
                        if found_value == value {
                            break digest;
                        }
                        break staged.stage(Node::Leaf {
                            schema_version: NODE_SCHEMA_VERSION,
                            key: key.to_vec(),
                            value: value.to_vec(),
                        })?;
                    }
                    let shared = common_prefix_bits(key, &found_key, key_bit_len(key)?)?;
                    let leaf = staged.stage(Node::Leaf {
                        schema_version: NODE_SCHEMA_VERSION,
                        key: key.to_vec(),
                        value: value.to_vec(),
                    })?;
                    break Self::stage_split(staged, key, shared, digest, &found_key, leaf)?;
                }
                Node::Branch {
                    prefix,
                    prefix_bit_len,
                    left,
                    right,
                    ..
                } => {
                    let split = prefix_bit_len as usize;
                    let rightward = key_bit(key, split)?;
                    digest = if rightward { right } else { left };
                    constraint = Some(ChildPathConstraint {
                        parent_prefix: prefix.clone(),
                        parent_prefix_bit_len: split,
                        right: rightward,
                    });
                    ancestors.push(BranchFrame {
                        prefix,
                        prefix_bit_len,
                        left,
                        right,
                        rightward,
                    });
                }
            }
        };

        ancestors
            .into_iter()
            .rev()
            .try_fold(replacement, |child, ancestor| {
                let (left, right) = if ancestor.rightward {
                    (ancestor.left, child)
                } else {
                    (child, ancestor.right)
                };
                staged.stage(Node::Branch {
                    schema_version: NODE_SCHEMA_VERSION,
                    prefix: ancestor.prefix,
                    prefix_bit_len: ancestor.prefix_bit_len,
                    left,
                    right,
                })
            })
    }

    fn stage_split(
        staged: &mut StagedNodes,
        key: &[u8],
        shared: usize,
        existing: ContentDigest,
        existing_prefix: &[u8],
        leaf: ContentDigest,
    ) -> Result<ContentDigest, StoreError> {
        let key_right = key_bit(key, shared)?;
        let existing_right = key_bit(existing_prefix, shared)?;
        if key_right == existing_right {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        let (left, right) = if key_right {
            (existing, leaf)
        } else {
            (leaf, existing)
        };
        staged.stage(Node::Branch {
            schema_version: NODE_SCHEMA_VERSION,
            prefix: masked_prefix(key, shared),
            prefix_bit_len: u16::try_from(shared)
                .map_err(|_| StoreError::MalformedLogseqClaimIndex)?,
            left,
            right,
        })
    }

    fn read_staged_or_persisted(
        &self,
        digest: ContentDigest,
        staged: &StagedNodes,
    ) -> Result<Node, StoreError> {
        match staged.nodes.get(&digest) {
            Some(node) => Ok(node.clone()),
            None => self.read_node(digest),
        }
    }

    fn publish_staged_reachable(
        &self,
        root: PatriciaIndexRoot,
        staged: &StagedNodes,
    ) -> Result<(), StoreError> {
        let mut pending = vec![root.digest()];
        let mut visited = BTreeSet::new();
        while let Some(digest) = pending.pop() {
            if !visited.insert(digest) {
                continue;
            }
            let Some(node) = staged.nodes.get(&digest) else {
                continue;
            };
            if let Node::Branch { left, right, .. } = node {
                pending.push(*left);
                pending.push(*right);
            }
            self.publish_node(node)?;
        }
        Ok(())
    }

    fn publish_all_staged(&self, staged: &StagedNodes) -> Result<(), StoreError> {
        for node in staged.nodes.values() {
            self.publish_node(node)?;
        }
        Ok(())
    }

    fn publish_staged_roots(
        &self,
        roots: &BTreeSet<ContentDigest>,
        staged: &StagedNodes,
    ) -> Result<(), StoreError> {
        let mut pending = roots.iter().copied().collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        while let Some(digest) = pending.pop() {
            if !visited.insert(digest) {
                continue;
            }
            let Some(node) = staged.nodes.get(&digest) else {
                continue;
            };
            if let Node::Branch { left, right, .. } = node {
                pending.push(*left);
                pending.push(*right);
            }
            self.publish_node(node)?;
        }
        Ok(())
    }

    fn verify_staged_reachable(
        &self,
        root: PatriciaIndexRoot,
        staged: &StagedNodes,
    ) -> Result<(), StoreError> {
        let mut pending = vec![root.digest()];
        let mut visited = BTreeSet::new();
        while let Some(digest) = pending.pop() {
            if !visited.insert(digest) {
                continue;
            }
            let Some(node) = staged.nodes.get(&digest) else {
                continue;
            };
            if let Node::Branch { left, right, .. } = node {
                pending.push(*left);
                pending.push(*right);
            }
            self.read_node(digest)?;
        }
        Ok(())
    }

    fn collect_prefix(
        &self,
        root: ContentDigest,
        requested: &[u8],
        limit: usize,
        found: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<(), StoreError> {
        let budget = traversal_node_budget(MAX_KEY_BYTES)?;
        let mut pending = vec![(root, None, budget)];
        while let Some((digest, constraint, remaining_nodes)) = pending.pop() {
            let remaining_nodes = consume_node_budget(remaining_nodes)?;
            let node = self.read_node(digest)?;
            validate_node_path(&node, constraint.as_ref())?;
            match node {
                Node::Leaf { key, value, .. } => {
                    if key.starts_with(requested) {
                        found.insert(key, value);
                        if found.len() == limit {
                            return Ok(());
                        }
                    }
                }
                Node::Branch {
                    prefix,
                    prefix_bit_len,
                    left,
                    right,
                    ..
                } => {
                    let split = prefix_bit_len as usize;
                    let requested_bits = key_bit_len(requested)?;
                    let compared = split.min(requested_bits);
                    if !prefix_matches(requested, &prefix, compared)? {
                        continue;
                    }
                    if requested_bits <= split {
                        pending.push((
                            right,
                            Some(ChildPathConstraint {
                                parent_prefix: prefix.clone(),
                                parent_prefix_bit_len: split,
                                right: true,
                            }),
                            remaining_nodes,
                        ));
                        pending.push((
                            left,
                            Some(ChildPathConstraint {
                                parent_prefix: prefix,
                                parent_prefix_bit_len: split,
                                right: false,
                            }),
                            remaining_nodes,
                        ));
                    } else {
                        let rightward = key_bit(requested, split)?;
                        pending.push((
                            if rightward { right } else { left },
                            Some(ChildPathConstraint {
                                parent_prefix: prefix,
                                parent_prefix_bit_len: split,
                                right: rightward,
                            }),
                            remaining_nodes,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn publish_node(&self, node: &Node) -> Result<ContentDigest, StoreError> {
        validate_node(node)?;
        let bytes =
            postcard::to_allocvec(node).map_err(|_| StoreError::MalformedLogseqClaimIndex)?;
        if bytes.len() as u64 > MAX_NODE_BYTES {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        let digest = ContentDigest::of(&bytes);
        let filename = node_filename(digest);
        if let Some(publisher) = &self.detached_publisher {
            publisher.publish(
                &self.nodes,
                &filename,
                &bytes,
                "authenticated Patricia index node",
            )?;
        } else {
            publish_immutable_exact(
                &self.nodes,
                &filename,
                &bytes,
                "Logseq UUID claim index node",
            )?;
        }
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_written
            .fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(digest)
    }

    fn read_node(&self, digest: ContentDigest) -> Result<Node, StoreError> {
        let bytes =
            read_optional_regular(&self.nodes, &node_filename(digest), MAX_NODE_BYTES, None)?
                .ok_or(StoreError::MissingLogseqClaimIndexNode(digest))?;
        if ContentDigest::of(&bytes) != digest {
            return Err(StoreError::LogseqClaimIndexPathMismatch(digest));
        }
        let node: Node =
            postcard::from_bytes(&bytes).map_err(|_| StoreError::MalformedLogseqClaimIndex)?;
        validate_node(&node)?;
        if postcard::to_allocvec(&node).map_err(|_| StoreError::MalformedLogseqClaimIndex)? != bytes
        {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_read
            .fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(node)
    }
}

fn validate_record(key: &[u8], value: &[u8]) -> Result<(), StoreError> {
    validate_key(key)?;
    if value.is_empty() || value.len() > MAX_VALUE_BYTES {
        return Err(StoreError::MalformedLogseqClaimIndex);
    }
    Ok(())
}

fn validate_key(key: &[u8]) -> Result<(), StoreError> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(StoreError::MalformedLogseqClaimIndex);
    }
    key_bit_len(key)?;
    Ok(())
}

fn validate_node(node: &Node) -> Result<(), StoreError> {
    match node {
        Node::Leaf {
            schema_version,
            key,
            value,
        } => {
            if *schema_version != NODE_SCHEMA_VERSION {
                return Err(StoreError::MalformedLogseqClaimIndex);
            }
            validate_record(key, value)
        }
        Node::Branch {
            schema_version,
            prefix,
            prefix_bit_len,
            left,
            right,
        } => {
            let bits = *prefix_bit_len as usize;
            if *schema_version != NODE_SCHEMA_VERSION
                || bits >= MAX_KEY_BITS
                || prefix.len() != bits.div_ceil(8)
                || masked_prefix(prefix, bits) != *prefix
                || left == right
                || *left == PatriciaIndexRoot::empty().digest()
                || *right == PatriciaIndexRoot::empty().digest()
            {
                return Err(StoreError::MalformedLogseqClaimIndex);
            }
            Ok(())
        }
    }
}

fn validate_node_path(
    node: &Node,
    constraint: Option<&ChildPathConstraint>,
) -> Result<(), StoreError> {
    let Some(constraint) = constraint else {
        return Ok(());
    };
    let prefix = node_prefix(node);
    let bits = node_prefix_bits(node)?;
    if bits <= constraint.parent_prefix_bit_len
        || !prefix_matches(
            prefix,
            &constraint.parent_prefix,
            constraint.parent_prefix_bit_len,
        )?
        || key_bit(prefix, constraint.parent_prefix_bit_len)? != constraint.right
    {
        return Err(StoreError::MalformedLogseqClaimIndex);
    }
    Ok(())
}

fn node_prefix(node: &Node) -> &[u8] {
    match node {
        Node::Leaf { key, .. } => key,
        Node::Branch { prefix, .. } => prefix,
    }
}

fn node_prefix_bits(node: &Node) -> Result<usize, StoreError> {
    match node {
        Node::Leaf { key, .. } => key_bit_len(key),
        Node::Branch { prefix_bit_len, .. } => Ok(*prefix_bit_len as usize),
    }
}

fn common_prefix_bits(left: &[u8], right: &[u8], limit: usize) -> Result<usize, StoreError> {
    let limit = limit.min(key_bit_len(left)?).min(key_bit_len(right)?);
    Ok((0..limit)
        .find(|bit| key_bit_unchecked(left, *bit) != key_bit_unchecked(right, *bit))
        .unwrap_or(limit))
}

fn prefix_matches(key: &[u8], prefix: &[u8], bits: usize) -> Result<bool, StoreError> {
    Ok(key_bit_len(key)? >= bits
        && key_bit_len(prefix)? >= bits
        && common_prefix_bits(key, prefix, bits)? == bits)
}

fn key_bit(key: &[u8], bit: usize) -> Result<bool, StoreError> {
    if bit >= key_bit_len(key)? {
        return Err(StoreError::MalformedLogseqClaimIndex);
    }
    Ok(key_bit_unchecked(key, bit))
}

fn key_bit_len(key: &[u8]) -> Result<usize, StoreError> {
    key.len()
        .checked_mul(8)
        .ok_or(StoreError::MalformedLogseqClaimIndex)
}

fn traversal_node_budget(key_bytes: usize) -> Result<usize, StoreError> {
    key_bytes
        .checked_mul(8)
        .and_then(|bits| bits.checked_add(1))
        .ok_or(StoreError::MalformedLogseqClaimIndex)
}

fn consume_node_budget(remaining_nodes: usize) -> Result<usize, StoreError> {
    remaining_nodes
        .checked_sub(1)
        .ok_or(StoreError::MalformedLogseqClaimIndex)
}

fn key_bit_unchecked(key: &[u8], bit: usize) -> bool {
    key[bit / 8] & (0x80 >> (bit % 8)) != 0
}

fn masked_prefix(key: &[u8], bits: usize) -> Vec<u8> {
    let mut prefix = key[..bits.div_ceil(8).min(key.len())].to_vec();
    if !bits.is_multiple_of(8) {
        let mask = 0xff << (8 - bits % 8);
        if let Some(last) = prefix.last_mut() {
            *last &= mask;
        }
    }
    prefix
}

fn node_filename(digest: ContentDigest) -> String {
    format!("{digest}{NODE_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cap_std::ambient_authority;
    use uuid::Uuid;

    use super::*;
    use crate::oplog::object_store::{ensure_directory_nofollow, open_dir_nofollow};

    fn store(name: &str) -> (std::path::PathBuf, PatriciaIndexStore) {
        let path = std::env::temp_dir().join(format!("tine-claim-index-{name}-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        let root = Dir::open_ambient_dir(&path, ambient_authority()).unwrap();
        ensure_directory_nofollow(&root, "nodes").unwrap();
        let nodes = open_dir_nofollow(&root, "nodes").unwrap();
        (path, PatriciaIndexStore::new(nodes))
    }

    fn publish_leaf(store: &PatriciaIndexStore, key: &[u8]) -> ContentDigest {
        store
            .publish_node(&Node::Leaf {
                schema_version: NODE_SCHEMA_VERSION,
                key: key.to_vec(),
                value: b"value".to_vec(),
            })
            .unwrap()
    }

    fn publish_branch(
        store: &PatriciaIndexStore,
        prefix_source: &[u8],
        split: usize,
        left: ContentDigest,
        right: ContentDigest,
    ) -> ContentDigest {
        store
            .publish_node(&Node::Branch {
                schema_version: NODE_SCHEMA_VERSION,
                prefix: masked_prefix(prefix_source, split),
                prefix_bit_len: u16::try_from(split).unwrap(),
                left,
                right,
            })
            .unwrap()
    }

    fn assert_point_traversals_reject(
        store: &PatriciaIndexStore,
        root: PatriciaIndexRoot,
        key: &[u8],
    ) {
        assert!(matches!(
            store.lookup(root, key),
            Err(StoreError::MalformedLogseqClaimIndex)
        ));
        assert!(matches!(
            store.insert_many(
                root,
                &BTreeMap::from([(key.to_vec(), b"replacement".to_vec())])
            ),
            Err(StoreError::MalformedLogseqClaimIndex)
        ));
        assert!(matches!(
            store.lookup_prefix(root, key),
            Err(StoreError::MalformedLogseqClaimIndex)
        ));
    }

    #[test]
    fn insertion_is_canonical_and_historical_roots_remain_queryable() {
        let (path, store) = store("canonical");
        let records = BTreeMap::from([
            (b"a/one".to_vec(), b"1".to_vec()),
            (b"a/two".to_vec(), b"2".to_vec()),
            (b"b/one".to_vec(), b"3".to_vec()),
        ]);
        let forward = store
            .insert_many(PatriciaIndexRoot::empty(), &records)
            .unwrap();
        let reverse =
            records
                .iter()
                .rev()
                .fold(PatriciaIndexRoot::empty(), |root, (key, value)| {
                    store
                        .insert_many(root, &BTreeMap::from([(key.clone(), value.clone())]))
                        .unwrap()
                });
        assert_eq!(forward, reverse);
        assert_eq!(
            store.lookup_prefix(forward, b"a/").unwrap(),
            BTreeMap::from([
                (b"a/one".to_vec(), b"1".to_vec()),
                (b"a/two".to_vec(), b"2".to_vec()),
            ])
        );

        let advanced = store
            .insert_many(
                forward,
                &BTreeMap::from([(b"a/one".to_vec(), b"new".to_vec())]),
            )
            .unwrap();
        assert_eq!(
            store.lookup(forward, b"a/one").unwrap(),
            Some(b"1".to_vec())
        );
        assert_eq!(
            store.lookup(advanced, b"a/one").unwrap(),
            Some(b"new".to_vec())
        );
        assert!(store.stats().reads < 100);
        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn duplicate_heavy_prefix_is_sharded_beyond_the_old_record_ceiling() {
        const INTRODUCTIONS: usize = 1_200;

        let (path, store) = store("duplicate-heavy");
        let prefix = [0x5a; 16];
        let records = (0..INTRODUCTIONS)
            .map(|index| {
                let mut key = prefix.to_vec();
                key.extend_from_slice(&(index as u128).to_be_bytes());
                (key, vec![index as u8; 96])
            })
            .collect::<BTreeMap<_, _>>();
        assert!(
            records.values().map(Vec::len).sum::<usize>() > 64 * 1024,
            "fixture must exceed the former monolithic record ceiling"
        );
        let root = store
            .insert_many(PatriciaIndexRoot::empty(), &records)
            .unwrap();
        let before = store.stats();
        let found = store.lookup_prefix(root, &prefix).unwrap();
        let after = store.stats();
        assert_eq!(found, records);
        assert!(
            after.reads - before.reads <= INTRODUCTIONS * 3,
            "prefix lookup must read only the participant subtree"
        );
        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn repeated_nonprogressing_branches_and_wrong_path_leaves_refuse() {
        let (path, store) = store("malformed-paths");
        let key = [0_u8];
        let left = publish_leaf(&store, &key);
        let right = publish_leaf(&store, &[0x80]);

        let repeated_child = publish_branch(&store, &key, 0, left, right);
        let repeated_root =
            PatriciaIndexRoot::from_digest(publish_branch(&store, &key, 0, repeated_child, right));
        assert_point_traversals_reject(&store, repeated_root, &key);

        let shallower_child = publish_branch(&store, &key, 1, left, right);
        let nonprogressing_root =
            PatriciaIndexRoot::from_digest(publish_branch(&store, &key, 2, shallower_child, right));
        assert_point_traversals_reject(&store, nonprogressing_root, &key);

        let wrong_direction_leaf = publish_leaf(&store, &[0x40]);
        let wrong_leaf_root = PatriciaIndexRoot::from_digest(publish_branch(
            &store,
            &key,
            1,
            wrong_direction_leaf,
            right,
        ));
        assert_point_traversals_reject(&store, wrong_leaf_root, &key);

        crate::test_support::remove_dir_all(path);
    }

    #[test]
    fn overdeep_content_addressed_branch_chain_refuses_within_key_bound() {
        let (path, store) = store("overdeep");
        let key = vec![0_u8; MAX_KEY_BYTES];
        let matching_leaf = publish_leaf(&store, &key);
        let other_leaf = publish_leaf(&store, &vec![0xff; MAX_KEY_BYTES]);

        let mut chain = publish_branch(&store, &key, MAX_KEY_BITS - 1, matching_leaf, other_leaf);
        for split in (0..MAX_KEY_BITS).rev() {
            chain = publish_branch(&store, &key, split, chain, other_leaf);
        }
        let root = PatriciaIndexRoot::from_digest(chain);
        let hard_bound = traversal_node_budget(key.len()).unwrap();

        let before = store.stats();
        assert!(matches!(
            store.lookup(root, &key),
            Err(StoreError::MalformedLogseqClaimIndex)
        ));
        let after_lookup = store.stats();
        assert!(after_lookup.reads - before.reads <= hard_bound);

        assert!(matches!(
            store.insert_many(
                root,
                &BTreeMap::from([(key.clone(), b"replacement".to_vec())])
            ),
            Err(StoreError::MalformedLogseqClaimIndex)
        ));
        let after_insert = store.stats();
        assert!(after_insert.reads - after_lookup.reads <= hard_bound);

        assert!(matches!(
            store.lookup_prefix(root, &key),
            Err(StoreError::MalformedLogseqClaimIndex)
        ));
        let after_prefix = store.stats();
        assert!(after_prefix.reads - after_insert.reads <= hard_bound);

        crate::test_support::remove_dir_all(path);
    }
}
