//! Committed registry cache state. The producer synchronizes this owner with
//! snapshot capture; SQL construction runs after releasing that synchronization.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tine_storage::sqlite::PhysicalProjectionQuerySnapshot;
use tine_storage::ContentDigest;

use crate::config::ParseConfig;
use crate::doc::property_key_norm;
use crate::query::registry::{is_internal_key, Registry};
use crate::query::{QueryExecutionError, QueryUnavailableReason};

struct Published {
    revision: u64,
    registry: Arc<Registry>,
}

pub(crate) struct CommittedRegistryCache {
    incarnation: Arc<()>,
    revision: u64,
    config: ContentDigest,
    generation: u64,
    published: Option<Published>,
    dirty_keys: BTreeMap<String, u64>,
    dirty_declarations: BTreeMap<String, u64>,
}

/// Immutable input captured beside the SQL snapshot with this exact revision.
pub(crate) struct RegistryCapture {
    incarnation: Arc<()>,
    revision: u64,
    config: ContentDigest,
    base: Option<Arc<Registry>>,
    affected: BTreeSet<String>,
}

fn invalid() -> QueryExecutionError {
    QueryExecutionError::Unavailable(QueryUnavailableReason::InvalidSnapshot)
}

impl CommittedRegistryCache {
    pub(crate) fn new(revision: u64, config: &ParseConfig) -> Self {
        Self {
            incarnation: Arc::new(()),
            revision,
            config: config.digest(),
            generation: 0,
            published: None,
            dirty_keys: BTreeMap::new(),
            dirty_declarations: BTreeMap::new(),
        }
    }

    /// Called for replacement, config change, or incomplete/failed producer
    /// turns. Old captures still own their inputs but cannot publish here.
    pub(crate) fn reset(&mut self, revision: u64, config: &ParseConfig) {
        let generation = self.generation;
        *self = Self::new(revision, config);
        self.generation = generation;
    }

    /// Only successful, complete producer turns call this. No SQL or locks are
    /// taken here; the producer supplies its actual committed image revision.
    pub(crate) fn committed(
        &mut self,
        revision: u64,
        keys: BTreeSet<String>,
        declaration_names: BTreeSet<String>,
    ) -> Result<(), QueryExecutionError> {
        if revision < self.revision
            || (revision == self.revision && (!keys.is_empty() || !declaration_names.is_empty()))
        {
            return Err(invalid());
        }
        self.revision = revision;
        for key in keys {
            self.dirty_keys.insert(property_key_norm(&key), revision);
        }
        for name in declaration_names {
            self.dirty_declarations
                .insert(crate::refs::page_key(&name), revision);
        }
        Ok(())
    }

    pub(crate) fn capture(
        &self,
        revision: u64,
        config: &ParseConfig,
    ) -> Result<RegistryCapture, QueryExecutionError> {
        if revision != self.revision || config.digest() != self.config {
            return Err(invalid());
        }
        let base = self.published.as_ref().map(|p| Arc::clone(&p.registry));
        let mut affected: BTreeSet<String> = self.dirty_keys.keys().cloned().collect();
        if let Some(base) = base
            .as_ref()
            .filter(|_| !self.dirty_declarations.is_empty())
        {
            for key in base.rows().iter().map(|row| &row.normalized_name) {
                if self
                    .dirty_declarations
                    .contains_key(&crate::refs::page_key(key))
                {
                    affected.insert(key.clone());
                }
            }
        }
        // New keys since the base are already included above, so declaration
        // changes cannot miss a key introduced by an earlier unpublished turn.
        affected.retain(|key| !key.is_empty() && !is_internal_key(key, config));
        Ok(RegistryCapture {
            incarnation: Arc::clone(&self.incarnation),
            revision,
            config: self.config,
            base,
            affected,
        })
    }

    /// Publish a successful off-producer build. Obsolete captures return their
    /// own coherent result without replacing a newer or different cache owner.
    pub(crate) fn publish(
        &mut self,
        capture: RegistryCapture,
        built: Arc<Registry>,
    ) -> Result<Arc<Registry>, QueryExecutionError> {
        if built.config_digest() != capture.config {
            return Err(invalid());
        }
        let local_generation = next_generation(capture.base.as_deref(), &built);
        if !Arc::ptr_eq(&self.incarnation, &capture.incarnation)
            || self.config != capture.config
            || self
                .published
                .as_ref()
                .is_some_and(|p| p.revision > capture.revision)
        {
            return Ok(at_generation(built, local_generation));
        }
        if capture.revision > self.revision {
            return Err(invalid());
        }
        if let Some(published) = &self.published {
            if published.revision == capture.revision {
                return if Arc::ptr_eq(&published.registry, &built)
                    || published.registry.rows_equal(&built)
                {
                    Ok(Arc::clone(&published.registry))
                } else {
                    Err(invalid())
                };
            }
        }
        let generation = self.published.as_ref().map_or_else(
            || self.generation.saturating_add(1),
            |p| next_generation(Some(p.registry.as_ref()), &built),
        );
        self.generation = generation;
        let registry = at_generation(built, generation);
        self.dirty_keys
            .retain(|_, changed| *changed > capture.revision);
        self.dirty_declarations
            .retain(|_, changed| *changed > capture.revision);
        self.published = Some(Published {
            revision: capture.revision,
            registry: Arc::clone(&registry),
        });
        Ok(registry)
    }
}

fn at_generation(registry: Arc<Registry>, generation: u64) -> Arc<Registry> {
    if registry.generation() == generation {
        registry
    } else {
        Arc::new(Arc::unwrap_or_clone(registry).with_generation(generation))
    }
}

fn next_generation(previous: Option<&Registry>, built: &Registry) -> u64 {
    match previous {
        Some(previous)
            if std::ptr::eq(previous, built)
                || (previous.config_digest() == built.config_digest()
                    && previous.rows_equal(built)) =>
        {
            previous.generation()
        }
        previous => previous.map_or(0, Registry::generation).saturating_add(1),
    }
}

impl RegistryCapture {
    /// Call outside the producer and cache lock, on the captured job snapshot.
    /// A cache hit checks cancellation without copying registry rows or reading SQL.
    pub(crate) fn build(
        &self,
        snapshot: &mut PhysicalProjectionQuerySnapshot,
        config: &ParseConfig,
    ) -> Result<Arc<Registry>, QueryExecutionError> {
        if snapshot.cancellation().is_cancelled() {
            return Err(QueryExecutionError::Cancelled);
        }
        if config.digest() != self.config {
            return Err(invalid());
        }
        match &self.base {
            Some(base) if self.affected.is_empty() => Ok(Arc::clone(base)),
            Some(base) => super::registry_sql::patch_registry_from_snapshot(
                snapshot,
                base,
                &self.affected,
                config,
            )
            .map(Arc::new),
            None => super::registry_sql::read_registry(snapshot, config).map(Arc::new),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::registry::{build_registry, OwnerRow, OwnerType, PageMeta};

    fn keys(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn registry(config: &ParseConfig, values: &[(&str, &str)]) -> Arc<Registry> {
        Arc::new(
            build_registry(
                values
                    .iter()
                    .enumerate()
                    .map(|(ordinal, (key, value))| OwnerRow {
                        owner_type: OwnerType::Page,
                        owner_id: "p".into(),
                        page_id: "p".into(),
                        source_name: (*key).into(),
                        normalized_name: (*key).into(),
                        ordinal: ordinal as u32,
                        value: (*value).into(),
                    }),
                &|_| {
                    Some(PageMeta {
                        format: crate::model::Format::Md.into(),
                        name: "Page".into(),
                    })
                },
                config,
            )
            .unwrap(),
        )
    }

    fn seed(
        cache: &mut CommittedRegistryCache,
        config: &ParseConfig,
        revision: u64,
    ) -> Arc<Registry> {
        let capture = cache.capture(revision, config).unwrap();
        assert!(capture.base.is_none());
        cache
            .publish(
                capture,
                registry(config, &[("score", "1"), ("stable", "x")]),
            )
            .unwrap()
    }

    #[test]
    fn text_only_commit_reuses_semantics_and_requires_exact_image_capture() {
        let config = ParseConfig::default();
        let mut cache = CommittedRegistryCache::new(1, &config);
        let first = seed(&mut cache, &config, 1);
        cache.committed(2, keys(&[]), keys(&[])).unwrap();
        assert!(cache.capture(1, &config).is_err());
        let capture = cache.capture(2, &config).unwrap();
        assert!(capture.affected.is_empty());
        assert!(Arc::ptr_eq(capture.base.as_ref().unwrap(), &first));
        let next = cache.publish(capture, Arc::clone(&first)).unwrap();
        assert_eq!(next.generation(), first.generation());
        assert!(
            Arc::ptr_eq(&next, &first),
            "unchanged publication must not copy registry rows"
        );
    }

    #[test]
    fn newer_same_key_change_survives_older_completion() {
        let config = ParseConfig::default();
        let mut cache = CommittedRegistryCache::new(1, &config);
        seed(&mut cache, &config, 1);
        cache
            .committed(2, keys(&["score"]), keys(&["Score"]))
            .unwrap();
        let old = cache.capture(2, &config).unwrap();
        cache
            .committed(3, keys(&["score"]), keys(&["Score"]))
            .unwrap();
        cache
            .publish(old, registry(&config, &[("score", "2"), ("stable", "x")]))
            .unwrap();
        assert_eq!(cache.dirty_keys.get("score"), Some(&3));
        assert_eq!(
            cache
                .dirty_declarations
                .get(&crate::refs::page_key("Score")),
            Some(&3)
        );
        let new = cache.capture(3, &config).unwrap();
        assert_eq!(new.affected, keys(&["score"]));
        cache
            .publish(new, registry(&config, &[("score", "3"), ("stable", "x")]))
            .unwrap();
        assert!(cache.dirty_keys.is_empty());
        assert!(cache.dirty_declarations.is_empty());
    }

    #[test]
    fn old_query_completion_cannot_overwrite_newer_publication() {
        let config = ParseConfig::default();
        let mut cache = CommittedRegistryCache::new(1, &config);
        seed(&mut cache, &config, 1);
        let old = cache.capture(1, &config).unwrap();
        cache.committed(2, keys(&["score"]), keys(&[])).unwrap();
        let new = cache.capture(2, &config).unwrap();
        let newest = cache
            .publish(new, registry(&config, &[("score", "22")]))
            .unwrap();
        let older = cache
            .publish(old, registry(&config, &[("score", "1"), ("stable", "x")]))
            .unwrap();
        assert!(!older.rows_equal(&newest));
        assert!(Arc::ptr_eq(
            &cache.published.as_ref().unwrap().registry,
            &newest
        ));
    }

    #[test]
    fn reset_and_config_change_fence_old_captures_even_at_equal_revision() {
        let config = ParseConfig::default();
        let mut cache = CommittedRegistryCache::new(1, &config);
        let first = seed(&mut cache, &config, 1);
        let old = cache.capture(1, &config).unwrap();
        let mut changed = config.clone();
        changed.hidden_properties.push("score".into());
        cache.reset(1, &changed);
        cache
            .publish(old, registry(&config, &[("score", "1")]))
            .unwrap();
        assert!(cache.published.is_none());
        assert!(cache.capture(1, &config).is_err());
        let fresh = cache.capture(1, &changed).unwrap();
        let result = cache
            .publish(fresh, Arc::new(Registry::empty(&changed)))
            .unwrap();
        assert!(result.generation() > first.generation());
    }

    #[test]
    fn declaration_changes_include_cached_and_new_keys_and_failed_reads_clear_nothing() {
        let config = ParseConfig::default();
        let mut cache = CommittedRegistryCache::new(1, &config);
        seed(&mut cache, &config, 1);
        cache.committed(2, keys(&["new-key"]), keys(&[])).unwrap();
        cache
            .committed(3, keys(&[]), keys(&["SCORE", "new-key"]))
            .unwrap();
        let capture = cache.capture(3, &config).unwrap();
        assert_eq!(capture.affected, keys(&["score", "new-key"]));
        // Failed SQL drops the capture without calling publish.
        drop(capture);
        assert_eq!(
            cache.capture(3, &config).unwrap().affected,
            keys(&["score", "new-key"])
        );
        assert!(cache.committed(2, keys(&[]), keys(&[])).is_err());
        assert!(cache.committed(3, keys(&["score"]), keys(&[])).is_err());
    }
    #[test]
    fn same_image_parallel_builds_share_publication_and_reject_disagreement() {
        let config = ParseConfig::default();
        let mut cache = CommittedRegistryCache::new(1, &config);
        let a = cache.capture(1, &config).unwrap();
        let b = cache.capture(1, &config).unwrap();
        let c = cache.capture(1, &config).unwrap();
        let first = cache
            .publish(a, registry(&config, &[("score", "1")]))
            .unwrap();
        let second = cache
            .publish(b, registry(&config, &[("score", "1")]))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(cache
            .publish(c, registry(&config, &[("score", "2")]))
            .is_err());
        assert!(Arc::ptr_eq(
            &first,
            &cache.published.as_ref().unwrap().registry
        ));
    }
    #[test]
    fn snapshot_build_and_cache_hit_keep_cancellation_and_config_checks() {
        let root =
            std::env::temp_dir().join(format!("tine-registry-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("projection.sqlite");
        let database =
            tine_storage::sqlite::PhysicalGraphProjectionDatabase::open_writable(&path).unwrap();
        database.initialize_schema().unwrap();
        drop(database);
        let config = ParseConfig::default();
        let mut cache = CommittedRegistryCache::new(1, &config);
        let full = cache.capture(1, &config).unwrap();
        let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
        let built = full.build(&mut snapshot, &config).unwrap();
        assert!(built.rows().is_empty());
        let published = cache.publish(full, built).unwrap();
        drop(snapshot);

        cache.committed(2, keys(&[]), keys(&[])).unwrap();
        let hit = cache.capture(2, &config).unwrap();
        let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
        assert!(Arc::ptr_eq(
            &hit.build(&mut snapshot, &config).unwrap(),
            &published
        ));
        let mut changed = config.clone();
        changed.hidden_properties.push("score".into());
        assert!(hit.build(&mut snapshot, &changed).is_err());
        snapshot.cancellation().cancel();
        assert!(matches!(
            hit.build(&mut snapshot, &config),
            Err(QueryExecutionError::Cancelled)
        ));
        drop(snapshot);
        std::fs::remove_dir_all(root).unwrap();
    }
}
