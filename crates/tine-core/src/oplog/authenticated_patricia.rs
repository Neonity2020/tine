#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;

use cap_std::fs::Dir;

use super::object_store::{
    filesystem_error_without_collision, publish_immutable_exact,
    DetachedBootstrapImmutablePublisher, StoreError,
};

#[allow(unused_imports)]
pub(crate) use tine_storage::{
    PatriciaIndexConstruction, PatriciaIndexConstructionStats, PatriciaIndexRoot,
    PatriciaIndexStats, MAX_PATRICIA_CONSTRUCTION_RESIDENT_BYTES,
};

#[derive(Debug)]
enum CorePatriciaPublisher {
    Ordinary,
    Detached(DetachedBootstrapImmutablePublisher),
}

impl tine_storage::PatriciaNodePublisher for CorePatriciaPublisher {
    fn publish(
        &self,
        dir: &Dir,
        filename: &str,
        bytes: &[u8],
    ) -> Result<(), tine_storage::PatriciaPublicationError> {
        let result = match self {
            Self::Ordinary => {
                publish_immutable_exact(dir, filename, bytes, "Logseq UUID claim index node")
            }
            Self::Detached(publisher) => {
                publisher.publish(dir, filename, bytes, "authenticated Patricia index node")
            }
        };
        result.map_err(tine_storage::PatriciaPublicationError::new)
    }

    fn permits_packed_head_transition(&self) -> bool {
        // Ordinary archive mutation is serialized by the archive-rooted
        // workspace writer lease. Detached bootstrap publication is one
        // unflushed immutable batch and therefore cannot make a mutable head
        // durable before its prerequisites at this boundary.
        matches!(self, Self::Ordinary)
    }
}

#[derive(Debug)]
pub(crate) struct PatriciaIndexStore {
    storage: tine_storage::PatriciaIndexStore,
}

impl PatriciaIndexStore {
    pub(crate) fn new(nodes: Dir) -> Self {
        Self {
            storage: tine_storage::PatriciaIndexStore::new(nodes, CorePatriciaPublisher::Ordinary),
        }
    }

    pub(crate) fn for_detached_bootstrap(
        &self,
        publisher: DetachedBootstrapImmutablePublisher,
    ) -> Result<Self, StoreError> {
        self.storage
            .with_publisher(CorePatriciaPublisher::Detached(publisher))
            .map(|storage| Self { storage })
            .map_err(map_storage_error)
    }

    pub(crate) fn stats(&self) -> PatriciaIndexStats {
        self.storage.stats()
    }

    pub(crate) fn validate_root(&self, root: PatriciaIndexRoot) -> Result<(), StoreError> {
        self.storage.validate_root(root).map_err(map_storage_error)
    }

    pub(crate) fn lookup(
        &self,
        root: PatriciaIndexRoot,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.storage.lookup(root, key).map_err(map_storage_error)
    }

    #[allow(dead_code)]
    pub(crate) fn lookup_many(
        &self,
        root: PatriciaIndexRoot,
        keys: &[Vec<u8>],
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
        self.storage
            .lookup_many(root, keys)
            .map_err(map_storage_error)
    }

    pub(crate) fn lookup_prefix(
        &self,
        root: PatriciaIndexRoot,
        prefix: &[u8],
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
        self.storage
            .lookup_prefix(root, prefix)
            .map_err(map_storage_error)
    }

    pub(crate) fn lookup_prefix_limited(
        &self,
        root: PatriciaIndexRoot,
        prefix: &[u8],
        limit: usize,
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
        self.storage
            .lookup_prefix_limited(root, prefix, limit)
            .map_err(map_storage_error)
    }

    pub(crate) fn visit_all(
        &self,
        root: PatriciaIndexRoot,
        visit: impl FnMut(&[u8], &[u8]) -> bool,
    ) -> Result<(), StoreError> {
        self.storage
            .visit_all(root, visit)
            .map_err(map_storage_error)
    }

    pub(crate) fn insert_many(
        &self,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        self.storage
            .insert_many(root, records)
            .map_err(map_storage_error)
    }

    pub(crate) fn insert_many_verify_existing(
        &self,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        self.storage
            .insert_many_verify_existing(root, records)
            .map_err(map_storage_error)
    }

    pub(crate) fn construction_lookup(
        &self,
        construction: &PatriciaIndexConstruction,
        root: PatriciaIndexRoot,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.storage
            .construction_lookup(construction, root, key)
            .map_err(map_storage_error)
    }

    pub(crate) fn construction_insert_many(
        &self,
        construction: &mut PatriciaIndexConstruction,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        self.storage
            .construction_insert_many(construction, root, records)
            .map_err(map_storage_error)
    }

    pub(crate) fn construction_remove_many(
        &self,
        construction: &mut PatriciaIndexConstruction,
        root: PatriciaIndexRoot,
        keys: &[Vec<u8>],
    ) -> Result<PatriciaIndexRoot, StoreError> {
        self.storage
            .construction_remove_many(construction, root, keys)
            .map_err(map_storage_error)
    }

    pub(crate) fn finish_construction(
        &self,
        construction: &mut PatriciaIndexConstruction,
    ) -> Result<(), StoreError> {
        self.storage
            .finish_construction(construction)
            .map_err(map_storage_error)
    }

    pub(crate) fn remove_many(
        &self,
        root: PatriciaIndexRoot,
        keys: &[Vec<u8>],
    ) -> Result<PatriciaIndexRoot, StoreError> {
        self.storage
            .remove_many(root, keys)
            .map_err(map_storage_error)
    }
}

fn map_storage_error(error: tine_storage::PatriciaError) -> StoreError {
    match error {
        tine_storage::PatriciaError::Filesystem(error) => filesystem_error_without_collision(error),
        tine_storage::PatriciaError::Publication(error) => {
            error.downcast::<StoreError>().unwrap_or_else(|_| {
                StoreError::Bootstrap(
                    "authenticated Patricia publisher returned an unknown error".into(),
                )
            })
        }
        tine_storage::PatriciaError::MissingNode(digest) => {
            StoreError::MissingLogseqClaimIndexNode(digest)
        }
        tine_storage::PatriciaError::PathMismatch(digest) => {
            StoreError::LogseqClaimIndexPathMismatch(digest)
        }
        tine_storage::PatriciaError::Malformed => StoreError::MalformedLogseqClaimIndex,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cap_std::ambient_authority;
    use uuid::Uuid;

    use super::*;
    use crate::oplog::object_store::{ensure_directory_nofollow, open_dir_nofollow};

    struct ExactStoragePublisher;

    impl tine_storage::PatriciaNodePublisher for ExactStoragePublisher {
        fn publish(
            &self,
            dir: &Dir,
            filename: &str,
            bytes: &[u8],
        ) -> Result<(), tine_storage::PatriciaPublicationError> {
            tine_storage::publish_immutable_exact(dir, filename, bytes)
                .map_err(tine_storage::PatriciaPublicationError::new)
        }
    }

    #[test]
    fn one_fixture_reopens_through_storage_and_the_core_facade() {
        let path = std::env::temp_dir().join(format!(
            "tine-core-storage-patricia-reopen-{}",
            Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        let root_dir = Dir::open_ambient_dir(&path, ambient_authority()).unwrap();
        ensure_directory_nofollow(&root_dir, "nodes").unwrap();

        let facade = PatriciaIndexStore::new(open_dir_nofollow(&root_dir, "nodes").unwrap());
        let root = facade
            .insert_many(
                PatriciaIndexRoot::empty(),
                &BTreeMap::from([
                    (b"alpha".to_vec(), b"one".to_vec()),
                    (b"beta".to_vec(), b"two".to_vec()),
                    (b"gamma".to_vec(), b"three".to_vec()),
                ]),
            )
            .unwrap();
        assert_eq!(
            root.digest().to_string(),
            "9976fbe04eaa635f6abadec835be4dc410cb8b12b0ee519addf5b1579aa32d84"
        );

        let storage = tine_storage::PatriciaIndexStore::new(
            open_dir_nofollow(&root_dir, "nodes").unwrap(),
            ExactStoragePublisher,
        );
        assert_eq!(
            storage.lookup(root, b"beta").unwrap(),
            Some(b"two".to_vec())
        );

        let reopened = PatriciaIndexStore::new(open_dir_nofollow(&root_dir, "nodes").unwrap());
        assert_eq!(
            reopened.lookup(root, b"gamma").unwrap(),
            Some(b"three".to_vec())
        );
        drop(reopened);
        drop(storage);
        drop(facade);
        drop(root_dir);
        fs::remove_dir_all(path).unwrap();
    }
}
