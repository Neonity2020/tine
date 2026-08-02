#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use cap_std::fs::Dir;

use super::object_store::{
    filesystem_error_without_collision, publish_immutable_exact,
    DetachedBootstrapImmutablePublisher, StoreError,
};

#[allow(unused_imports)]
pub(crate) use tine_storage::{
    PatriciaIndexConstruction, PatriciaIndexConstructionStats, PatriciaIndexReclamationError,
    PatriciaIndexReclamationReport, PatriciaIndexRoot, PatriciaIndexStats,
    MAX_PATRICIA_CONSTRUCTION_RESIDENT_BYTES,
};

/// Move-only core evidence that one archive-bound Patricia construction has
/// completed all packed heads or its loose fallback.
pub(crate) struct CompletedPatriciaConstruction {
    physical: tine_storage::CompletedPatriciaIndexConstruction,
}

impl CompletedPatriciaConstruction {
    #[cfg(test)]
    pub(crate) const fn stats(&self) -> PatriciaIndexConstructionStats {
        self.physical.stats()
    }
}

#[cfg(test)]
thread_local! {
    static NEXT_RECLAMATION_FAILURE: std::cell::Cell<Option<PatriciaReclamationFailureForTest>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum PatriciaReclamationFailureForTest {
    Busy,
    MalformedAuthority,
}

#[cfg(test)]
pub(crate) fn fail_next_patricia_reclamation_for_test(failure: PatriciaReclamationFailureForTest) {
    NEXT_RECLAMATION_FAILURE.with(|next| next.set(Some(failure)));
}

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

    fn publish_construction_exact(
        &self,
        dir: &Dir,
        filename: &str,
        bytes: &[u8],
    ) -> Result<(), tine_storage::PatriciaPublicationError> {
        // Construction packs and catalogs use an independently durable exact
        // lane. Detached loose fallback remains in the session-wide immutable
        // batch, but no mutable packed head can name those unflushed bytes.
        publish_immutable_exact(
            dir,
            filename,
            bytes,
            "authenticated Patricia construction prerequisite",
        )
        .map_err(tine_storage::PatriciaPublicationError::new)
    }

    fn publish_staged_construction_exact(
        &self,
        publication: tine_storage::StagedExactImmutablePublication,
    ) -> Result<(), tine_storage::PatriciaPublicationError> {
        publication.commit().map_err(|error| {
            let error = match error {
                tine_storage::FilesystemError::ByteCollision => StoreError::ImmutableCollision(
                    "authenticated Patricia construction prerequisite",
                ),
                error => filesystem_error_without_collision(error),
            };
            tine_storage::PatriciaPublicationError::new(error)
        })
    }

    fn permits_construction_packed_head_transition(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub(crate) struct PatriciaIndexStore {
    storage: tine_storage::PatriciaIndexStore,
    construction: Option<Mutex<Option<PatriciaIndexConstruction>>>,
    #[cfg(test)]
    reclamation_attempts: std::sync::atomic::AtomicUsize,
}

impl PatriciaIndexStore {
    pub(crate) fn new(nodes: Dir) -> Self {
        Self {
            storage: tine_storage::PatriciaIndexStore::new(nodes, CorePatriciaPublisher::Ordinary),
            construction: None,
            #[cfg(test)]
            reclamation_attempts: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn for_detached_bootstrap(
        &self,
        publisher: DetachedBootstrapImmutablePublisher,
    ) -> Result<Self, StoreError> {
        self.storage
            .with_publisher(CorePatriciaPublisher::Detached(publisher))
            .map(|storage| Self {
                storage,
                construction: None,
                #[cfg(test)]
                reclamation_attempts: std::sync::atomic::AtomicUsize::new(0),
            })
            .map_err(map_storage_error)
    }

    pub(crate) fn for_detached_bootstrap_construction(
        &self,
        publisher: DetachedBootstrapImmutablePublisher,
    ) -> Result<Self, StoreError> {
        let mut detached = self.for_detached_bootstrap(publisher)?;
        detached.construction = Some(Mutex::new(Some(PatriciaIndexConstruction::default())));
        Ok(detached)
    }

    fn construction_guard(
        &self,
    ) -> Result<Option<std::sync::MutexGuard<'_, Option<PatriciaIndexConstruction>>>, StoreError>
    {
        self.construction
            .as_ref()
            .map(|construction| {
                construction
                    .lock()
                    .map_err(|_| StoreError::MalformedLogseqClaimIndex)
            })
            .transpose()
    }

    pub(crate) fn stats(&self) -> PatriciaIndexStats {
        self.storage.stats()
    }

    #[cfg(test)]
    pub(crate) fn corrupt_packed_node_for_test(
        &self,
        digest: tine_storage::ContentDigest,
    ) -> Result<(), StoreError> {
        self.storage
            .corrupt_packed_node_for_test(digest)
            .map_err(map_storage_error)
    }

    pub(crate) fn reclaim_unreachable_packed_files(
        &self,
    ) -> Result<PatriciaIndexReclamationReport, PatriciaIndexReclamationError> {
        #[cfg(test)]
        {
            self.reclamation_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(failure) = NEXT_RECLAMATION_FAILURE.with(|next| next.take()) {
                return Err(match failure {
                    PatriciaReclamationFailureForTest::Busy => PatriciaIndexReclamationError::Busy,
                    PatriciaReclamationFailureForTest::MalformedAuthority => {
                        PatriciaIndexReclamationError::MalformedAuthority
                    }
                });
            }
        }
        self.storage.reclaim_unreachable_packed_files()
    }

    #[cfg(test)]
    pub(crate) fn reclamation_attempts(&self) -> usize {
        self.reclamation_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn validate_root(&self, root: PatriciaIndexRoot) -> Result<(), StoreError> {
        let Some(construction) = self.construction_guard()? else {
            return self.storage.validate_root(root).map_err(map_storage_error);
        };
        match construction.as_ref() {
            Some(construction) => self
                .storage
                .construction_validate_root(construction, root)
                .map_err(map_storage_error),
            None => self.storage.validate_root(root).map_err(map_storage_error),
        }
    }

    pub(crate) fn lookup(
        &self,
        root: PatriciaIndexRoot,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(construction) = self.construction_guard()? else {
            return self.storage.lookup(root, key).map_err(map_storage_error);
        };
        match construction.as_ref() {
            Some(construction) => self
                .storage
                .construction_lookup(construction, root, key)
                .map_err(map_storage_error),
            None => self.storage.lookup(root, key).map_err(map_storage_error),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn lookup_many(
        &self,
        root: PatriciaIndexRoot,
        keys: &[Vec<u8>],
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        let Some(construction) = self.construction_guard()? else {
            return self
                .storage
                .lookup_many(root, keys)
                .map_err(map_storage_error);
        };
        match construction.as_ref() {
            Some(construction) => keys
                .iter()
                .filter_map(|key| {
                    self.storage
                        .construction_lookup(construction, root, key)
                        .transpose()
                        .map(|result| result.map(|value| (key.clone(), value)))
                })
                .collect::<Result<_, _>>()
                .map_err(map_storage_error),
            None => self
                .storage
                .lookup_many(root, keys)
                .map_err(map_storage_error),
        }
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
        let Some(construction) = self.construction_guard()? else {
            return self
                .storage
                .lookup_prefix_limited(root, prefix, limit)
                .map_err(map_storage_error);
        };
        match construction.as_ref() {
            Some(construction) => self
                .storage
                .construction_lookup_prefix_limited(construction, root, prefix, limit)
                .map_err(map_storage_error),
            None => self
                .storage
                .lookup_prefix_limited(root, prefix, limit)
                .map_err(map_storage_error),
        }
    }

    pub(crate) fn visit_all(
        &self,
        root: PatriciaIndexRoot,
        mut visit: impl FnMut(&[u8], &[u8]) -> bool,
    ) -> Result<(), StoreError> {
        let Some(construction_guard) = self.construction_guard()? else {
            return self
                .storage
                .visit_all(root, visit)
                .map_err(map_storage_error);
        };
        match construction_guard.as_ref() {
            Some(active_construction) => {
                let mut entries = Vec::new();
                self.storage
                    .construction_visit_all(active_construction, root, |key, value| {
                        entries.push((key.to_vec(), value.to_vec()));
                        true
                    })
                    .map_err(map_storage_error)?;
                drop(construction_guard);
                for (key, value) in entries {
                    if !visit(&key, &value) {
                        break;
                    }
                }
                Ok(())
            }
            None => {
                drop(construction_guard);
                self.storage
                    .visit_all(root, visit)
                    .map_err(map_storage_error)
            }
        }
    }

    pub(crate) fn insert_many(
        &self,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        let Some(mut construction) = self.construction_guard()? else {
            return self
                .storage
                .insert_many(root, records)
                .map_err(map_storage_error);
        };
        match construction.as_mut() {
            Some(construction) => {
                construction.set_live_roots([root]);
                let next = self
                    .storage
                    .construction_insert_many(construction, root, records)
                    .map_err(map_storage_error)?;
                construction.set_live_roots([next]);
                construction.checkpoint([next]);
                Ok(next)
            }
            None => self
                .storage
                .insert_many(root, records)
                .map_err(map_storage_error),
        }
    }

    pub(crate) fn insert_many_verify_existing(
        &self,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        let constructing = self
            .construction_guard()?
            .is_some_and(|construction| construction.is_some());
        if constructing {
            return self.insert_many(root, records);
        }
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

    pub(crate) fn construction_insert_many_bulk(
        &self,
        construction: &mut PatriciaIndexConstruction,
        root: PatriciaIndexRoot,
        records: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<PatriciaIndexRoot, StoreError> {
        self.storage
            .construction_insert_many_bulk(construction, root, records)
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
    ) -> Result<CompletedPatriciaConstruction, StoreError> {
        self.storage
            .finish_construction(construction)
            .map(|physical| CompletedPatriciaConstruction { physical })
            .map_err(map_storage_error)
    }

    pub(crate) fn remove_many(
        &self,
        root: PatriciaIndexRoot,
        keys: &[Vec<u8>],
    ) -> Result<PatriciaIndexRoot, StoreError> {
        let Some(mut construction) = self.construction_guard()? else {
            return self
                .storage
                .remove_many(root, keys)
                .map_err(map_storage_error);
        };
        match construction.as_mut() {
            Some(construction) => {
                construction.set_live_roots([root]);
                let next = self
                    .storage
                    .construction_remove_many(construction, root, keys)
                    .map_err(map_storage_error)?;
                construction.set_live_roots([next]);
                construction.checkpoint([next]);
                Ok(next)
            }
            None => self
                .storage
                .remove_many(root, keys)
                .map_err(map_storage_error),
        }
    }

    pub(crate) fn finish_detached_construction(
        &self,
        root: PatriciaIndexRoot,
    ) -> Result<Option<CompletedPatriciaConstruction>, StoreError> {
        let Some(mut construction) = self.construction_guard()? else {
            return Ok(None);
        };
        let Some(mut pending) = construction.take() else {
            return Ok(None);
        };
        pending.set_live_roots([root]);
        pending.checkpoint([root]);
        let physical = self
            .storage
            .finish_construction(&mut pending)
            .map_err(map_storage_error)?;
        self.storage
            .validate_root(root)
            .map_err(map_storage_error)?;
        Ok(Some(CompletedPatriciaConstruction { physical }))
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
