//! Accepted document index: ONE shared authenticated map keyed by the lossless
//! [`DocumentKey`] bytes. Entity births and full (block, page) membership pairs
//! are rows of the same map, distinguished by the key's own domain tag. There
//! is no tuple hashing, no group descriptor, no nested tree and no second map
//! implementation.
use crate::oplog::{ContentDigest, DocumentKey};
use std::collections::BTreeSet;
use tine_storage::sealed_accepted_index::{
    authenticated_map_root, AuthenticatedMapRootV1, SealedAcceptedIndexObjectStore,
    SealedAcceptedIndexReader, SealedAcceptedIndexWriter,
};

use super::{SealedGenerationDirectory, SealedGenerationStagingStore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SealedDocumentMap {
    documents: AuthenticatedMapRootV1,
}

impl SealedDocumentMap {
    pub(super) fn empty() -> Self {
        Self {
            documents: AuthenticatedMapRootV1::empty(),
        }
    }

    pub(super) fn from_root(documents: AuthenticatedMapRootV1) -> Self {
        Self { documents }
    }

    pub(super) fn root(self) -> AuthenticatedMapRootV1 {
        self.documents
    }

    #[cfg(test)]
    pub(super) fn entity_root(self) -> AuthenticatedMapRootV1 {
        self.documents
    }

    /// The composed root, for proving that the run-local accepted document map
    /// and this sealed one are the same map over the same full-key rows.
    #[cfg(test)]
    pub(crate) fn composed_root(self) -> AuthenticatedMapRootV1 {
        self.documents
    }

    #[cfg(test)]
    pub(super) fn with_entity_root_for_test(self, documents: AuthenticatedMapRootV1) -> Self {
        Self { documents }
    }

    pub(super) fn count(self) -> u64 {
        self.documents.count
    }

    pub(super) fn value(
        self,
        store: &SealedGenerationDirectory,
        key: DocumentKey,
    ) -> Result<Option<ContentDigest>, String> {
        SealedAcceptedIndexReader::new(store)
            .map_value(self.documents, key.authenticated_map_key())
            .map_err(|error| error.to_string())
    }

    pub(super) fn upsert(
        self,
        store: &mut SealedGenerationStagingStore,
        key: DocumentKey,
        value: ContentDigest,
    ) -> Result<Self, String> {
        Ok(Self {
            documents: SealedAcceptedIndexWriter::new(store)
                .upsert_map(self.documents, key.authenticated_map_key(), value)
                .map_err(|error| error.to_string())?,
        })
    }

    pub(super) fn remove(
        self,
        store: &mut SealedGenerationStagingStore,
        key: DocumentKey,
    ) -> Result<Self, String> {
        Ok(Self {
            documents: SealedAcceptedIndexWriter::new(store)
                .remove_map(self.documents, key.authenticated_map_key())
                .map_err(|error| error.to_string())?,
        })
    }

    /// Prove that this census is EXACTLY the accepted document key set: every
    /// named key resolves, and the map rebuilt from those keys is this map.
    pub(super) fn qualify_complete_keys(
        self,
        store: &SealedGenerationDirectory,
        keys: impl Iterator<Item = DocumentKey>,
    ) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        let mut entries = Vec::new();
        for key in keys {
            if !seen.insert(key) {
                return Err("document key census repeats an identity".into());
            }
            let value = self
                .value(store, key)?
                .ok_or("document roster omits an accepted document")?;
            entries.push((key.authenticated_map_key(), value));
        }
        // The census is ordered by `DocumentKey`, which is not the map's
        // lexicographic key order, so sort before rebuilding.
        entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        if authenticated_map_root(&entries).map_err(|error| error.to_string())? != self.documents {
            return Err("document roster is not exactly the accepted document key set".into());
        }
        Ok(())
    }
}

/// Kept so callers that only need the sealed-node trait bound do not have to
/// name the concrete store types.
trait DocumentMapRead: SealedAcceptedIndexObjectStore {}
impl DocumentMapRead for SealedGenerationDirectory {}
impl DocumentMapRead for SealedGenerationStagingStore {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::DocumentId;

    #[test]
    fn contract_names_shared_map_composition_and_retirement() {
        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        for required in [
            "SealedDocumentMap",
            "DocumentKey::Entity",
            "DocumentKey::Membership",
            "(block_document_id, page_document_id)",
            "remove_map",
            "AuthenticatedMapKey",
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
                // A membership pair is addressed by BOTH document ids: the
                // mirrored pair is a different row and never aliases.
                assert_ne!(
                    map.value(&reader, member(2, 1)).unwrap(),
                    map.value(&reader, member(1, 2)).unwrap()
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

    /// The running composition (`hot_engine::RunLocalDocumentMap`) and this
    /// sealed one are the SAME map: identical roots for the exact same full-key
    /// rows, including after the last membership row of a block is deleted and
    /// after the last row of the whole map is deleted.
    #[test]
    fn running_and_sealed_document_maps_agree_on_the_same_full_key_rows() {
        use crate::oplog::hot_engine::run_local_document_map_root;

        with_directory(|directory| {
            let rows = [
                (DocumentKey::Entity(id(1)), ContentDigest::of(b"e1")),
                (member(1, 2), ContentDigest::of(b"m12")),
                (DocumentKey::Entity(id(7)), ContentDigest::of(b"e7")),
                (member(2, 1), ContentDigest::of(b"m21")),
                (member(1, 3), ContentDigest::of(b"m13")),
                (member(2, 3), ContentDigest::of(b"m23")),
            ];
            let mut store = SealedGenerationStagingStore::open(directory).unwrap();
            let mut sealed = SealedDocumentMap::empty();
            for (key, value) in rows {
                sealed = sealed.upsert(&mut store, key, value).unwrap();
            }
            assert_eq!(sealed.composed_root(), run_local_document_map_root(&rows));
            assert_eq!(sealed.count(), 6);

            // Deleting the last membership row of block 1 must leave exactly
            // the root the running map builds from the remaining rows alone.
            let mut trimmed = sealed;
            for key in [member(1, 2), member(1, 3)] {
                trimmed = trimmed.remove(&mut store, key).unwrap();
            }
            let remaining: Vec<_> = rows
                .into_iter()
                .filter(|(key, _)| *key != member(1, 2) && *key != member(1, 3))
                .collect();
            assert_eq!(
                trimmed.composed_root(),
                run_local_document_map_root(&remaining)
            );
            assert_eq!(trimmed.count(), 4);

            // Removing the last entry of the shared map yields the empty root
            // on both sides -- no residual descriptor, no empty group.
            let mut bare = trimmed;
            for (key, _) in remaining {
                bare = bare.remove(&mut store, key).unwrap();
            }
            assert_eq!(bare.composed_root(), run_local_document_map_root(&[]));
            assert_eq!(bare.composed_root(), AuthenticatedMapRootV1::empty());
            assert_eq!(bare.count(), 0);
            drop(store.finish().unwrap());
        });
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
            // Removing the last pair drops the row, not a tombstone.
            map = map.remove(&mut store, member(1, 2)).unwrap();
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
