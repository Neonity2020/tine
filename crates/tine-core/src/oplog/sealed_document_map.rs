//! Tagged document index composed from the shared UUID map. Entity UUIDs and
//! full (block UUID, page UUID) membership pairs have separate root domains.
//! No tuple hashing, tombstone entries, or second tree implementation.
use super::{capsule_blob_name, SealedGenerationDirectory, SealedGenerationStagingStore};
use crate::oplog::{ContentDigest, DocumentId, DocumentKey};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tine_storage::sealed_accepted_index::{
    authenticated_map_root, AuthenticatedMapLinkV1, AuthenticatedMapRootV1,
    SealedAcceptedIndexObjectStore, SealedAcceptedIndexReader, SealedAcceptedIndexWriter,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SealedDocumentMap {
    entities: AuthenticatedMapRootV1,
    membership_blocks: AuthenticatedMapRootV1,
    membership_count: u64,
}

// Fixed-size root descriptor, encoded with the existing postcard serializer.
// The outer UUID map binds the block ID to these exact root bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MembershipRootRecord {
    schema: u8,
    count: u64,
    root: Option<([u8; 16], [u8; 32])>,
}

impl MembershipRootRecord {
    fn bytes(root: AuthenticatedMapRootV1) -> Result<Vec<u8>, String> {
        postcard::to_stdvec(&Self {
            schema: 1,
            count: root.count,
            root: root.root.map(|link| (link.key, *link.digest.as_bytes())),
        })
        .map_err(|error| error.to_string())
    }

    fn decode(bytes: &[u8]) -> Result<AuthenticatedMapRootV1, String> {
        let (record, remaining): (Self, &[u8]) =
            postcard::take_from_bytes(bytes).map_err(|error| error.to_string())?;
        let root = AuthenticatedMapRootV1 {
            count: record.count,
            root: record.root.map(|(key, digest)| AuthenticatedMapLinkV1 {
                key,
                digest: ContentDigest::from_bytes(digest),
            }),
        };
        if record.schema != 1
            || !remaining.is_empty()
            || (root.count == 0) != root.root.is_none()
            || Self::bytes(root)? != bytes
        {
            return Err("membership document map root is not canonical".into());
        }
        Ok(root)
    }
}

// Construction reads must see the bounded unpublished batch as well as disk.
// Reuse the staging store's sealed-node trait, including its sticky failure.
trait DocumentMapRead: SealedAcceptedIndexObjectStore {
    fn root_record_bytes(&self, address: ContentDigest) -> Result<Vec<u8>, String>;
}
impl DocumentMapRead for SealedGenerationDirectory {
    fn root_record_bytes(&self, address: ContentDigest) -> Result<Vec<u8>, String> {
        tine_storage::read_optional_regular(&self.directory, &capsule_blob_name(address), 128, None)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "membership document map root bytes are missing".into())
    }
}
impl DocumentMapRead for SealedGenerationStagingStore {
    fn root_record_bytes(&self, address: ContentDigest) -> Result<Vec<u8>, String> {
        if self.failed {
            return Err("sealed generation staging previously failed".into());
        }
        if let Some(bytes) = self.pending.objects.get(&(0, address)) {
            if bytes.len() > 128 {
                return Err("membership root exceeds its fixed codec size".into());
            }
            return Ok(bytes.clone());
        }
        self.reader.root_record_bytes(address)
    }
}

impl SealedDocumentMap {
    pub(super) fn empty() -> Self {
        Self {
            entities: AuthenticatedMapRootV1::empty(),
            membership_blocks: AuthenticatedMapRootV1::empty(),
            membership_count: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn entity_root(self) -> AuthenticatedMapRootV1 {
        self.entities
    }

    #[cfg(test)]
    pub(super) fn with_entity_root_for_test(self, entities: AuthenticatedMapRootV1) -> Self {
        Self { entities, ..self }
    }

    pub(super) fn count(self) -> u64 {
        // Every construction update checks the combined count before returning.
        self.entities
            .count
            .checked_add(self.membership_count)
            .expect("validated document map count")
    }

    fn membership_root(
        self,
        store: &impl DocumentMapRead,
        block: DocumentId,
    ) -> Result<AuthenticatedMapRootV1, String> {
        let Some(address) = SealedAcceptedIndexReader::new(store)
            .map_value(self.membership_blocks, block.as_uuid().into_bytes())
            .map_err(|error| error.to_string())?
        else {
            return Ok(AuthenticatedMapRootV1::empty());
        };
        // This fixed descriptor is at most 60 bytes; the read limit is a codec
        // bound, independent of the number of documents or retained history.
        let bytes = store.root_record_bytes(address)?;
        if ContentDigest::of(&bytes) != address {
            return Err("membership document map root digest differs".into());
        }
        let root = MembershipRootRecord::decode(&bytes)?;
        if root.count == 0 {
            return Err("document map retains an empty membership group".into());
        }
        Ok(root)
    }

    pub(super) fn value(
        self,
        store: &SealedGenerationDirectory,
        key: DocumentKey,
    ) -> Result<Option<ContentDigest>, String> {
        let (root, uuid) = match key {
            DocumentKey::Entity(id) => (self.entities, id),
            DocumentKey::Membership {
                block_document_id,
                page_document_id,
            } => (
                self.membership_root(store, block_document_id)?,
                page_document_id,
            ),
        };
        SealedAcceptedIndexReader::new(store)
            .map_value(root, uuid.as_uuid().into_bytes())
            .map_err(|error| error.to_string())
    }

    pub(super) fn upsert(
        self,
        store: &mut SealedGenerationStagingStore,
        key: DocumentKey,
        value: ContentDigest,
    ) -> Result<Self, String> {
        let next = match key {
            DocumentKey::Entity(id) => Self {
                entities: SealedAcceptedIndexWriter::new(store)
                    .upsert_map(self.entities, id.as_uuid().into_bytes(), value)
                    .map_err(|error| error.to_string())?,
                ..self
            },
            DocumentKey::Membership {
                block_document_id,
                page_document_id,
            } => {
                let old = self.membership_root(store, block_document_id)?;
                let root = SealedAcceptedIndexWriter::new(store)
                    .upsert_map(old, page_document_id.as_uuid().into_bytes(), value)
                    .map_err(|error| error.to_string())?;
                let blob = store.stage_capsule_blob(&MembershipRootRecord::bytes(root)?)?;
                let membership_blocks = SealedAcceptedIndexWriter::new(store)
                    .upsert_map(
                        self.membership_blocks,
                        block_document_id.as_uuid().into_bytes(),
                        ContentDigest::from_bytes(*blob.sha256()),
                    )
                    .map_err(|error| error.to_string())?;
                Self {
                    membership_blocks,
                    membership_count: self
                        .membership_count
                        .checked_add(root.count - old.count)
                        .ok_or("membership document count overflow")?,
                    ..self
                }
            }
        };
        next.entities
            .count
            .checked_add(next.membership_count)
            .ok_or("document map count overflow")?;
        Ok(next)
    }

    pub(super) fn remove(
        self,
        store: &mut SealedGenerationStagingStore,
        key: DocumentKey,
    ) -> Result<Self, String> {
        match key {
            DocumentKey::Entity(id) => Ok(Self {
                entities: SealedAcceptedIndexWriter::new(store)
                    .remove_map(self.entities, id.as_uuid().into_bytes())
                    .map_err(|error| error.to_string())?,
                ..self
            }),
            DocumentKey::Membership {
                block_document_id,
                page_document_id,
            } => {
                let old = self.membership_root(store, block_document_id)?;
                let root = SealedAcceptedIndexWriter::new(store)
                    .remove_map(old, page_document_id.as_uuid().into_bytes())
                    .map_err(|error| error.to_string())?;
                if root == old {
                    return Ok(self);
                }
                let membership_blocks = if root.count == 0 {
                    SealedAcceptedIndexWriter::new(store)
                        .remove_map(
                            self.membership_blocks,
                            block_document_id.as_uuid().into_bytes(),
                        )
                        .map_err(|error| error.to_string())?
                } else {
                    let blob = store.stage_capsule_blob(&MembershipRootRecord::bytes(root)?)?;
                    SealedAcceptedIndexWriter::new(store)
                        .upsert_map(
                            self.membership_blocks,
                            block_document_id.as_uuid().into_bytes(),
                            ContentDigest::from_bytes(*blob.sha256()),
                        )
                        .map_err(|error| error.to_string())?
                };
                Ok(Self {
                    membership_blocks,
                    membership_count: self
                        .membership_count
                        .checked_sub(1)
                        .ok_or("membership document count underflow")?,
                    ..self
                })
            }
        }
    }

    pub(super) fn qualify_complete_keys(
        self,
        store: &SealedGenerationDirectory,
        keys: impl Iterator<Item = DocumentKey>,
    ) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        let mut entities = Vec::new();
        let mut memberships = BTreeMap::<DocumentId, Vec<_>>::new();
        let mut membership_count = 0u64;
        for key in keys {
            if !seen.insert(key) {
                return Err("document key census repeats an identity".into());
            }
            let value = self
                .value(store, key)?
                .ok_or("document roster omits an accepted document")?;
            match key {
                DocumentKey::Entity(id) => entities.push((id.as_uuid().into_bytes(), value)),
                DocumentKey::Membership {
                    block_document_id,
                    page_document_id,
                } => {
                    memberships
                        .entry(block_document_id)
                        .or_default()
                        .push((page_document_id.as_uuid().into_bytes(), value));
                    membership_count = membership_count
                        .checked_add(1)
                        .ok_or("membership census count overflow")?;
                }
            }
        }
        let mut groups = Vec::new();
        for (block, entries) in memberships {
            let root = authenticated_map_root(&entries).map_err(|error| error.to_string())?;
            groups.push((
                block.as_uuid().into_bytes(),
                ContentDigest::of(&MembershipRootRecord::bytes(root)?),
            ));
        }
        if authenticated_map_root(&entities).map_err(|error| error.to_string())? != self.entities
            || authenticated_map_root(&groups).map_err(|error| error.to_string())?
                != self.membership_blocks
            || membership_count != self.membership_count
        {
            return Err("document roster is not exactly the accepted document key set".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_names_shared_map_composition_and_retirement() {
        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        for required in [
            "SealedDocumentMap",
            "DocumentKey::Entity",
            "DocumentKey::Membership",
            "(block_document_id, page_document_id)",
            "remove_map",
            "128 bytes",
        ] {
            assert!(contract.contains(required), "missing contract: {required}");
        }
    }

    fn id(value: u128) -> DocumentId {
        DocumentId::from_uuid(uuid::Uuid::from_u128(value))
    }
    fn member(block: u128, page: u128) -> DocumentKey {
        DocumentKey::Membership {
            block_document_id: id(block),
            page_document_id: id(page),
        }
    }
    fn with_directory(test: impl FnOnce(&cap_std::fs::Dir)) {
        let path = std::env::temp_dir().join(format!("tine-document-map-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&path, cap_std::ambient_authority()).unwrap();
        test(&directory);
        drop(directory);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn tagged_document_map_preserves_full_pairs_and_canonical_retirement() {
        for publication_budget in [1, 64] {
            with_directory(|directory| {
                let keys = [
                    DocumentKey::Entity(id(1)),
                    member(1, 2),
                    member(2, 1),
                    member(1, 3),
                    member(2, 3),
                ];
                let mut store = SealedGenerationStagingStore::open(directory).unwrap();
                store.batch_object_budget = publication_budget;
                let mut map = SealedDocumentMap::empty();
                for (index, key) in keys.iter().enumerate() {
                    map = map
                        .upsert(&mut store, *key, ContentDigest::of(&[index as u8]))
                        .unwrap();
                }
                // Updates and removals must read unpublished root descriptors
                // correctly, as well as descriptors already flushed to disk.
                let changed_value = ContentDigest::of(b"changed");
                map = map.upsert(&mut store, member(1, 2), changed_value).unwrap();
                assert_eq!(map.count(), 5);
                let original = map;
                map = map.remove(&mut store, member(1, 2)).unwrap();
                map = map.remove(&mut store, member(1, 3)).unwrap();
                assert_eq!(map.count(), 3);
                assert_eq!(map.membership_blocks.count, 1);
                let reader = store.finish().unwrap();
                original
                    .qualify_complete_keys(&reader, keys.into_iter())
                    .unwrap();
                map.qualify_complete_keys(
                    &reader,
                    [DocumentKey::Entity(id(1)), member(2, 1), member(2, 3)].into_iter(),
                )
                .unwrap();
                assert_eq!(
                    original.value(&reader, member(1, 2)).unwrap(),
                    Some(changed_value)
                );
                assert_eq!(map.value(&reader, member(1, 2)).unwrap(), None);
                assert_eq!(
                    map.value(&reader, DocumentKey::Entity(id(1))).unwrap(),
                    Some(ContentDigest::of(&[0]))
                );
                assert!(map
                    .qualify_complete_keys(&reader, [member(2, 1), member(2, 3)].into_iter())
                    .is_err());
                assert!(original
                    .qualify_complete_keys(&reader, keys.into_iter().chain([keys[0]]))
                    .is_err());
                drop(reader);
            });
        }
    }

    #[test]
    fn retired_membership_history_does_not_remain_in_active_roots() {
        with_directory(|directory| {
            let mut store = SealedGenerationStagingStore::open(directory).unwrap();
            let keys = [DocumentKey::Entity(id(1)), member(1, 2)];
            let mut map = SealedDocumentMap::empty();
            for key in keys {
                map = map
                    .upsert(&mut store, key, ContentDigest::of(b"live"))
                    .unwrap();
            }
            let fixed_live_root = map;
            for cycle in 0..512u128 {
                // Distinct pages create distinct historical membership facts.
                let key = member(1, 1000 + cycle);
                map = map
                    .upsert(&mut store, key, ContentDigest::of(&cycle.to_be_bytes()))
                    .unwrap();
                assert_eq!(map.count(), 3);
                map = map.remove(&mut store, key).unwrap();
                assert_eq!(map, fixed_live_root);
            }
            // Removing the last pair drops its outer group, not a tombstone.
            map = map.remove(&mut store, member(1, 2)).unwrap();
            assert_eq!(map.membership_blocks, AuthenticatedMapRootV1::empty());
            assert_eq!(map.count(), 1);
            let reader = store.finish().unwrap();
            fixed_live_root
                .qualify_complete_keys(&reader, keys.into_iter())
                .unwrap();
            map.qualify_complete_keys(&reader, [keys[0]].into_iter())
                .unwrap();
        });
    }
}
