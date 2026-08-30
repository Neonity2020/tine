use std::collections::{BTreeMap, BTreeSet};

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::absence_decision::{AbsenceDecisionMap, ReceiverAbsenceSummaryEntry};
use super::object_store::{
    ensure_reconstructible_directory_nofollow, open_dir_nofollow, read_optional_regular,
    require_regular_entry, sync_dir_required,
};
use super::projection_store::ProjectionCatalogEntry;
use super::{
    ContentDigest, ObjectStore, ProjectionIntent, ProjectionIntentId, ProjectionReceiptStore,
    ProjectionStoreError, WorkspaceId,
};

const ABSENCE_NAMESPACE: &str = "sweeps";
const SUMMARY_NAMESPACE: &str = "receiver-absence-summary-v1";
const SUMMARY_PREFIX: &str = "receiver-absence-summary-";
const SUMMARY_SUFFIX: &str = ".summary";
const SUMMARY_SCHEMA_VERSION: u32 = 1;
const SUFFIX_COMPLETION: &str = ".completion";
const SUFFIX_INTENT: &str = ".intent";
const MAX_SUMMARY_OBJECT_BYTES: u64 = 512 * 1024 * 1024;

#[cfg(test)]
static SKIP_NEXT_SUMMARY_UPDATE: std::sync::LazyLock<std::sync::Mutex<BTreeSet<WorkspaceId>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeSet::new()));

#[cfg(test)]
pub(crate) fn skip_next_receiver_absence_summary_update_for_test(workspace_id: WorkspaceId) {
    SKIP_NEXT_SUMMARY_UPDATE
        .lock()
        .expect("summary fault mutex")
        .insert(workspace_id);
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceFilenameHorizon {
    count: u64,
    set_digest: ContentDigest,
    names: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiverAbsenceSummaryObject {
    schema_version: u32,
    workspace_id: WorkspaceId,
    generation: u64,
    previous_digest: Option<ContentDigest>,
    horizon: EvidenceFilenameHorizon,
    entries: Vec<ReceiverAbsenceSummaryEntry>,
    incomplete_intents: Vec<ProjectionIntent>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReceiverAbsenceSummaryOpenStats {
    pub(crate) evidence_names_observed: usize,
    pub(crate) receipt_content_reads: usize,
    pub(crate) full_catalog_passes: usize,
    pub(crate) summary_content_reads: usize,
    pub(crate) rebuilt: bool,
    pub(crate) delta_completions: usize,
    pub(crate) delta_intents: usize,
}

pub(crate) struct ReceiverAbsenceSummaryOpen {
    pub(crate) map: AbsenceDecisionMap,
    pub(crate) cache: Option<ReceiverAbsenceSummary>,
    pub(crate) stats: ReceiverAbsenceSummaryOpenStats,
}

pub(crate) struct ReceiverAbsenceSummary {
    store: ObjectStore,
    directory: Dir,
    workspace_id: WorkspaceId,
    generation: u64,
    tail_digest: Option<ContentDigest>,
    names: BTreeMap<u64, SummaryName>,
    evidence_names: BTreeSet<String>,
    receiver_map: AbsenceDecisionMap,
    incomplete_intents: BTreeMap<ProjectionIntentId, ProjectionIntent>,
}

#[derive(Clone, Debug)]
struct SummaryName {
    name: String,
    digest: ContentDigest,
}

impl ReceiverAbsenceSummary {
    pub(crate) fn open(
        store: &ObjectStore,
        receipts: &ProjectionReceiptStore,
    ) -> Result<ReceiverAbsenceSummaryOpen, ProjectionStoreError> {
        let evidence_names = receipts.absence_summary_evidence_names()?;
        let mut stats = ReceiverAbsenceSummaryOpenStats {
            evidence_names_observed: evidence_names.len(),
            ..ReceiverAbsenceSummaryOpenStats::default()
        };

        let opened_cache = Self::open_cache(store, &mut stats).ok().flatten();
        if let Some(mut cache) = opened_cache {
            let covered = cache.evidence_names.clone();
            if covered.is_subset(&evidence_names) {
                let extra_completions = evidence_names
                    .difference(&covered)
                    .filter(|name| name.ends_with(SUFFIX_COMPLETION))
                    .cloned()
                    .collect::<BTreeSet<_>>();
                // A new intent whose completion is also visible is covered by
                // the completion row; only completion-less intents delta-read.
                let extra_intents = evidence_names
                    .difference(&covered)
                    .filter(|name| name.ends_with(SUFFIX_INTENT))
                    .filter(|name| {
                        !name
                            .strip_suffix(SUFFIX_INTENT)
                            .map(|stem| format!("{stem}{SUFFIX_COMPLETION}"))
                            .is_some_and(|twin| evidence_names.contains(&twin))
                    })
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let rows = receipts.absence_summary_catalog_delta(&extra_completions)?;
                let fresh_intents = receipts.absence_summary_intent_delta(&extra_intents)?;
                stats.delta_completions = extra_completions.len();
                stats.delta_intents = extra_intents.len();
                stats.receipt_content_reads = rows
                    .iter()
                    .map(|row| usize::from(row.completion.is_some()) + 1)
                    .sum::<usize>()
                    + fresh_intents.len();
                for row in &rows {
                    if row.completion.is_some() {
                        cache.incomplete_intents.remove(&row.intent.id()?);
                    }
                }
                let incomplete = apply_catalog_rows(&mut cache.receiver_map, rows)?;
                for intent in incomplete {
                    cache.incomplete_intents.insert(intent.id()?, intent);
                }
                for intent in fresh_intents {
                    let intent_id = intent.id()?;
                    if !cache.completion_name_known(&completion_filename(&intent)?) {
                        cache.incomplete_intents.insert(intent_id, intent);
                    }
                }
                cache.evidence_names = evidence_names;
                if (!extra_completions.is_empty() || !extra_intents.is_empty())
                    && cache.install().is_err()
                {
                    let mut map = cache.receiver_map.clone();
                    for intent in cache.incomplete_intents.values() {
                        map.record_receiver_intent(&intent)?;
                    }
                    return Ok(ReceiverAbsenceSummaryOpen {
                        map,
                        cache: None,
                        stats,
                    });
                }
                let mut map = cache.receiver_map.clone();
                for intent in cache.incomplete_intents.values() {
                    map.record_receiver_intent(&intent)?;
                }
                return Ok(ReceiverAbsenceSummaryOpen {
                    map,
                    cache: Some(cache),
                    stats,
                });
            }
            stats.rebuilt = true;
        }

        stats.rebuilt = true;
        stats.full_catalog_passes = 1;
        let catalog = receipts.validated_catalog()?;
        stats.receipt_content_reads = catalog
            .iter()
            .map(|row| usize::from(row.completion.is_some()) + 1)
            .sum();
        let mut receiver_map = AbsenceDecisionMap::default();
        let incomplete = apply_catalog_rows(&mut receiver_map, catalog)?;
        let mut cache = Self::fresh_cache(store, &stats).ok();
        if let Some(candidate) = cache.as_mut() {
            candidate.evidence_names = evidence_names;
            candidate.receiver_map = receiver_map.clone();
            candidate.incomplete_intents = incomplete
                .iter()
                .map(|intent| Ok((intent.id()?, intent.clone())))
                .collect::<Result<_, super::ReceiptError>>()?;
            if candidate.install().is_err() {
                cache = None;
            }
        }
        for intent in incomplete {
            receiver_map.record_receiver_intent(&intent)?;
        }
        Ok(ReceiverAbsenceSummaryOpen {
            map: receiver_map,
            cache,
            stats,
        })
    }

    pub(crate) fn record_completion(&mut self, intent: &ProjectionIntent) -> Result<(), String> {
        #[cfg(test)]
        if SKIP_NEXT_SUMMARY_UPDATE
            .lock()
            .expect("summary fault mutex")
            .remove(&self.workspace_id)
        {
            return Ok(());
        }
        let name = completion_filename(intent).map_err(|error| error.to_string())?;
        let intent_name = intent_only_filename(intent).map_err(|error| error.to_string())?;
        if !self.evidence_names.insert(name) {
            return Ok(());
        }
        self.evidence_names.insert(intent_name);
        self.incomplete_intents
            .remove(&intent.id().map_err(|error| error.to_string())?);
        self.receiver_map
            .record_receiver_completion(intent)
            .map_err(|error| error.to_string())?;
        self.install()
    }

    pub(crate) fn record_intent(&mut self, intent: &ProjectionIntent) -> Result<(), String> {
        #[cfg(test)]
        if SKIP_NEXT_SUMMARY_UPDATE
            .lock()
            .expect("summary fault mutex")
            .remove(&self.workspace_id)
        {
            return Ok(());
        }
        let intent_id = intent.id().map_err(|error| error.to_string())?;
        let completion_known = self
            .evidence_names
            .contains(&completion_filename(intent).map_err(|error| error.to_string())?);
        let intent_name = intent_only_filename(intent).map_err(|error| error.to_string())?;
        let newly_named = self.evidence_names.insert(intent_name);
        if completion_known
            || (!newly_named && self.incomplete_intents.get(&intent_id) == Some(intent))
        {
            return Ok(());
        }
        if !completion_known {
            self.incomplete_intents.insert(intent_id, intent.clone());
        }
        self.install()
    }

    fn completion_name_known(&self, completion_name: &str) -> bool {
        self.evidence_names.contains(completion_name)
    }

    fn open_cache(
        store: &ObjectStore,
        stats: &mut ReceiverAbsenceSummaryOpenStats,
    ) -> Result<Option<Self>, String> {
        let root = store
            .private_derived_root_capability()
            .map_err(|error| error.to_string())?;
        ensure_reconstructible_directory_nofollow(&root, ABSENCE_NAMESPACE)
            .map_err(|error| error.to_string())?;
        let absence =
            open_dir_nofollow(&root, ABSENCE_NAMESPACE).map_err(|error| error.to_string())?;
        ensure_reconstructible_directory_nofollow(&absence, SUMMARY_NAMESPACE)
            .map_err(|error| error.to_string())?;
        let directory =
            open_dir_nofollow(&absence, SUMMARY_NAMESPACE).map_err(|error| error.to_string())?;
        let names = enumerate_names(&directory)?;
        let Some((&generation, name)) = names.last_key_value() else {
            return Ok(None);
        };
        let bytes = read_summary(&directory, name, stats)?;
        let object = decode_bound(&bytes, store.workspace_id(), generation)?;
        match (
            object.previous_digest,
            names.range(..generation).next_back(),
        ) {
            (None, None) => {}
            (Some(expected), Some((_, previous_name))) => {
                let previous = read_summary(&directory, previous_name, stats)?;
                if ContentDigest::of(&previous) != expected {
                    return Err("receiver absence summary chain digest mismatch".into());
                }
            }
            _ => return Err("receiver absence summary chain is torn".into()),
        }
        validate_horizon(&object.horizon)?;
        let mut receiver_map = AbsenceDecisionMap::default();
        for entry in object.entries {
            if entry.anchors.is_empty()
                || entry
                    .anchors
                    .iter()
                    .any(|anchor| anchor.page_id != entry.page_id || anchor.path != entry.path)
            {
                return Err("receiver absence summary row binding mismatch".into());
            }
            receiver_map.record_receiver_summary_entry(entry);
        }
        let mut incomplete_intents = BTreeMap::new();
        for intent in object.incomplete_intents {
            if intent.workspace_id() != store.workspace_id() {
                return Err("receiver absence summary incomplete intent workspace mismatch".into());
            }
            let intent_id = intent.id().map_err(|error| error.to_string())?;
            if incomplete_intents.insert(intent_id, intent).is_some() {
                return Err("duplicate receiver absence summary incomplete intent".into());
            }
        }
        Ok(Some(Self {
            store: store
                .duplicate_retained_capability()
                .map_err(|error| error.to_string())?,
            directory,
            workspace_id: store.workspace_id(),
            generation,
            tail_digest: Some(ContentDigest::of(&bytes)),
            names,
            evidence_names: object.horizon.names.into_iter().collect(),
            receiver_map,
            incomplete_intents,
        }))
    }

    fn fresh_cache(
        store: &ObjectStore,
        _stats: &ReceiverAbsenceSummaryOpenStats,
    ) -> Result<Self, String> {
        let root = store
            .private_derived_root_capability()
            .map_err(|error| error.to_string())?;
        ensure_reconstructible_directory_nofollow(&root, ABSENCE_NAMESPACE)
            .map_err(|error| error.to_string())?;
        let absence =
            open_dir_nofollow(&root, ABSENCE_NAMESPACE).map_err(|error| error.to_string())?;
        ensure_reconstructible_directory_nofollow(&absence, SUMMARY_NAMESPACE)
            .map_err(|error| error.to_string())?;
        let directory =
            open_dir_nofollow(&absence, SUMMARY_NAMESPACE).map_err(|error| error.to_string())?;
        clear_chain(&directory)?;
        Ok(Self {
            store: store
                .duplicate_retained_capability()
                .map_err(|error| error.to_string())?,
            directory,
            workspace_id: store.workspace_id(),
            generation: 0,
            tail_digest: None,
            names: BTreeMap::new(),
            evidence_names: BTreeSet::new(),
            receiver_map: AbsenceDecisionMap::default(),
            incomplete_intents: BTreeMap::new(),
        })
    }

    fn install(&mut self) -> Result<(), String> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| "receiver absence summary generation overflow".to_owned())?;
        let mut entries = self.receiver_map.receiver_summary_entries();
        entries
            .sort_by(|left, right| (left.page_id, &left.path).cmp(&(right.page_id, &right.path)));
        for entry in &mut entries {
            entry.anchors.sort_by(|left, right| {
                (left.intent_id, target_rank(left.target_kind))
                    .cmp(&(right.intent_id, target_rank(right.target_kind)))
            });
        }
        let horizon = evidence_horizon(&self.evidence_names);
        let object = ReceiverAbsenceSummaryObject {
            schema_version: SUMMARY_SCHEMA_VERSION,
            workspace_id: self.workspace_id,
            generation,
            previous_digest: self.tail_digest,
            horizon,
            entries,
            incomplete_intents: self.incomplete_intents.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&object).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_SUMMARY_OBJECT_BYTES {
            return Err("receiver absence summary exceeds its bounded object limit".into());
        }
        let digest = ContentDigest::of(&bytes);
        let name = object_name(generation, digest);
        self.store
            .publish_coalesced_private_derived(
                &self.directory,
                &[(name.as_str(), bytes.as_slice(), MAX_SUMMARY_OBJECT_BYTES)],
                "receiver absence summary object",
            )
            .map_err(|error| error.to_string())?;
        self.generation = generation;
        self.tail_digest = Some(ContentDigest::of(&bytes));
        self.names.insert(generation, SummaryName { name, digest });
        self.prune_old_chain()?;
        Ok(())
    }

    fn prune_old_chain(&mut self) -> Result<(), String> {
        let obsolete = self
            .names
            .iter()
            .rev()
            .skip(2)
            .map(|(generation, name)| (*generation, name.name.clone()))
            .collect::<Vec<_>>();
        for (generation, name) in &obsolete {
            match self.directory.remove_file(name) {
                Ok(()) => {
                    self.names.remove(generation);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.names.remove(generation);
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        if !obsolete.is_empty() {
            sync_dir_required(&self.directory).map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn apply_catalog_rows(
    map: &mut AbsenceDecisionMap,
    rows: Vec<ProjectionCatalogEntry>,
) -> Result<Vec<ProjectionIntent>, ProjectionStoreError> {
    let mut incomplete = Vec::new();
    for row in rows {
        if row.completion.is_some() {
            map.record_receiver_completion(&row.intent)?;
        } else {
            incomplete.push(row.intent);
        }
    }
    Ok(incomplete)
}

fn enumerate_names(directory: &Dir) -> Result<BTreeMap<u64, SummaryName>, String> {
    let mut names = BTreeMap::new();
    for entry in directory.entries().map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF-8 receiver absence summary entry".to_owned())?;
        if !name.starts_with(SUMMARY_PREFIX) {
            continue;
        }
        require_regular_entry(
            &entry.file_type().map_err(|error| error.to_string())?,
            &name,
        )
        .map_err(|error| error.to_string())?;
        let parsed = parse_object_name(&name)?;
        if names.insert(parsed.0, parsed.1).is_some() {
            return Err("receiver absence summary generation twin".into());
        }
    }
    Ok(names)
}

fn parse_object_name(name: &str) -> Result<(u64, SummaryName), String> {
    let body = name
        .strip_prefix(SUMMARY_PREFIX)
        .and_then(|value| value.strip_suffix(SUMMARY_SUFFIX))
        .ok_or_else(|| "invalid receiver absence summary name".to_owned())?;
    let (digits, digest_hex) = body
        .split_once('-')
        .ok_or_else(|| "receiver absence summary name lacks a digest".to_owned())?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("non-canonical receiver absence summary generation".into());
    }
    let digest = decode_digest(digest_hex)?;
    let generation = digits.parse::<u64>().map_err(|error| error.to_string())?;
    if generation == 0 || object_name(generation, digest) != name {
        return Err("non-canonical receiver absence summary name".into());
    }
    Ok((
        generation,
        SummaryName {
            name: name.to_owned(),
            digest,
        },
    ))
}

fn object_name(generation: u64, digest: ContentDigest) -> String {
    format!(
        "{SUMMARY_PREFIX}{generation:020}-{}{SUMMARY_SUFFIX}",
        hex(digest.as_bytes())
    )
}

fn read_summary(
    directory: &Dir,
    name: &SummaryName,
    stats: &mut ReceiverAbsenceSummaryOpenStats,
) -> Result<Vec<u8>, String> {
    let bytes = read_optional_regular(directory, &name.name, MAX_SUMMARY_OBJECT_BYTES, None)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "receiver absence summary disappeared during open".to_owned())?;
    if ContentDigest::of(&bytes) != name.digest {
        return Err("receiver absence summary filename digest mismatch".into());
    }
    stats.summary_content_reads = stats.summary_content_reads.saturating_add(1);
    Ok(bytes)
}

fn decode_bound(
    bytes: &[u8],
    workspace_id: WorkspaceId,
    generation: u64,
) -> Result<ReceiverAbsenceSummaryObject, String> {
    let object: ReceiverAbsenceSummaryObject =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if object.schema_version != SUMMARY_SCHEMA_VERSION
        || object.workspace_id != workspace_id
        || object.generation != generation
        || serde_json::to_vec(&object).map_err(|error| error.to_string())? != bytes
    {
        return Err("receiver absence summary object binding mismatch".into());
    }
    Ok(object)
}

fn evidence_horizon(names: &BTreeSet<String>) -> EvidenceFilenameHorizon {
    EvidenceFilenameHorizon {
        count: names.len() as u64,
        set_digest: evidence_name_set_digest(names.iter().map(String::as_str)),
        names: names.iter().cloned().collect(),
    }
}

fn validate_horizon(horizon: &EvidenceFilenameHorizon) -> Result<(), String> {
    if horizon.names.len() as u64 != horizon.count
        || !horizon.names.windows(2).all(|pair| pair[0] < pair[1])
        || evidence_name_set_digest(horizon.names.iter().map(String::as_str)) != horizon.set_digest
    {
        return Err("receiver absence summary horizon mismatch".into());
    }
    Ok(())
}

fn evidence_name_set_digest<'a>(names: impl Iterator<Item = &'a str>) -> ContentDigest {
    let mut names = names.collect::<Vec<_>>();
    names.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(b"tine/receiver-absence-evidence-name-set/v1\0");
    hasher.update((names.len() as u64).to_be_bytes());
    for name in names {
        hasher.update((name.len() as u64).to_be_bytes());
        hasher.update(name.as_bytes());
    }
    ContentDigest::from_bytes(hasher.finalize().into())
}

fn completion_filename(intent: &ProjectionIntent) -> Result<String, super::ReceiptError> {
    Ok(format!(
        "{}{SUFFIX_COMPLETION}",
        hex(intent.id()?.as_bytes())
    ))
}

fn intent_only_filename(intent: &ProjectionIntent) -> Result<String, super::ReceiptError> {
    Ok(format!("{}{SUFFIX_INTENT}", hex(intent.id()?.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn decode_digest(value: &str) -> Result<ContentDigest, String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("non-canonical receiver absence summary digest".into());
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let nibble = |byte| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("non-canonical receiver absence summary digest".to_owned()),
        };
        bytes[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(ContentDigest::from_bytes(bytes))
}

fn target_rank(kind: super::ProjectionTargetKind) -> u8 {
    match kind {
        super::ProjectionTargetKind::Present => 0,
        super::ProjectionTargetKind::Absent => 1,
    }
}

fn clear_chain(directory: &Dir) -> Result<(), String> {
    let names = enumerate_names(directory)?;
    for name in names.values() {
        match directory.remove_file(&name.name) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    if !names.is_empty() {
        sync_dir_required(directory).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;
    use crate::oplog::{
        absence_decision::AbsenceDecision, BlobDescription, CrdtPeerCounter, CrdtPeerId,
        DocumentDependencies, DocumentId, FrontierV2, ManagedPath, PageId, ProjectionPrecondition,
        ProjectionTargetKind,
    };
    use crate::Graph;

    struct Fixture {
        root: PathBuf,
        graph: Graph,
        receipts: ProjectionReceiptStore,
        archive: ObjectStore,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "tine-receiver-absence-summary-{label}-{}",
                Uuid::new_v4()
            ));
            std::fs::create_dir_all(root.join("graph")).unwrap();
            let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0xc6_1000));
            Self {
                graph: Graph::open(&root.join("graph")),
                receipts: ProjectionReceiptStore::open(&root.join("receipts"), workspace_id)
                    .unwrap(),
                archive: ObjectStore::open(&root.join("operations"), workspace_id).unwrap(),
                root,
            }
        }

        fn copied_graph(label: &str, source: &PathBuf) -> Self {
            let root = std::env::temp_dir().join(format!(
                "tine-receiver-absence-summary-{label}-{}",
                Uuid::new_v4()
            ));
            copy_tree_without_symlinks(source, &root.join("graph"));
            let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0xc6_1000));
            Self {
                graph: Graph::open(&root.join("graph")),
                receipts: ProjectionReceiptStore::open(&root.join("receipts"), workspace_id)
                    .unwrap(),
                archive: ObjectStore::open(&root.join("operations"), workspace_id).unwrap(),
                root,
            }
        }

        fn intent(
            &self,
            page: u128,
            path: &str,
            counter: u64,
            base: Option<&[u8]>,
            target: &[u8],
        ) -> ProjectionIntent {
            let frontier = FrontierV2::new(vec![DocumentDependencies::new(
                DocumentId::from_uuid(Uuid::from_u128(0xc6_1001)),
                vec![CrdtPeerCounter::new(CrdtPeerId::from_u64(7), counter)],
                Vec::new(),
            )
            .unwrap()])
            .unwrap();
            ProjectionIntent::new(
                self.receipts.workspace_id(),
                PageId::from_uuid(Uuid::from_u128(page)),
                ManagedPath::parse(path).unwrap(),
                frontier,
                Vec::new(),
                base.map_or(ProjectionPrecondition::Absent, |bytes| {
                    ProjectionPrecondition::Base(BlobDescription::of(bytes))
                }),
                ProjectionTargetKind::Present,
                BlobDescription::of(target),
                Vec::new(),
            )
            .unwrap()
        }

        fn complete(&self, intent: &ProjectionIntent, base: Option<&[u8]>, target: &[u8]) {
            self.receipts.publish_intent(intent, base).unwrap();
            let reservation = self.receipts.reserve_attempt(intent).unwrap();
            let mut authority = self
                .receipts
                .begin_mutation(intent, Some(&reservation))
                .unwrap();
            let proof = self
                .graph
                .write_page_projection(intent.path().as_str(), base, target, &mut authority)
                .unwrap();
            self.receipts
                .publish_completion(authority, intent, &proof)
                .unwrap();
        }

        fn summary_dir(&self) -> PathBuf {
            self.archive
                .root_path()
                .join(ABSENCE_NAMESPACE)
                .join(SUMMARY_NAMESPACE)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            crate::test_support::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn completion_filename_horizon_catches_the_crash_before_summary_update() {
        let fixture = Fixture::new("stale-horizon");
        let first_bytes = b"- first projected bytes\n";
        let first = fixture.intent(0xc6_1010, "pages/first.md", 1, None, first_bytes);
        fixture.complete(&first, None, first_bytes);
        let opened = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(opened.stats.full_catalog_passes, 1);
        let stale_cache = opened.cache.expect("rebuild installs a summary");

        let second_bytes = b"- second projected bytes\n";
        let second = fixture.intent(0xc6_1020, "pages/second.md", 1, None, second_bytes);
        fixture.complete(&second, None, second_bytes);

        assert_eq!(
            stale_cache
                .receiver_map
                .decision(second.page_id(), second.path()),
            AbsenceDecision::Create,
            "necessity: a structurally valid summary trusted without its horizon recreates"
        );
        drop(stale_cache);

        let healed = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(healed.stats.full_catalog_passes, 0);
        assert_eq!(healed.stats.delta_completions, 1);
        assert_eq!(healed.stats.receipt_content_reads, 2);
        assert_eq!(
            healed.map.decision(second.page_id(), second.path()),
            AbsenceDecision::DeferredAbsence
        );
    }

    #[test]
    fn intent_filename_horizon_catches_a_durable_intent_without_completion() {
        let fixture = Fixture::new("stale-intent-horizon");
        let first_bytes = b"- first projected bytes\n";
        let first = fixture.intent(0xc6_1040, "pages/first.md", 1, None, first_bytes);
        fixture.complete(&first, None, first_bytes);
        let built = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(built.stats.full_catalog_passes, 1);
        drop(built);

        // A receiver intent lands durably, then the process dies before any
        // summary update. The stored summary is structurally valid and its
        // completion set is unchanged; only the intent namespace moved.
        let pending_bytes = b"- pending projected bytes\n";
        let pending = fixture.intent(0xc6_1050, "pages/pending.md", 1, None, pending_bytes);
        fixture.receipts.publish_intent(&pending, None).unwrap();

        let healed = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(
            healed.stats.full_catalog_passes, 0,
            "an intent-only delta must not force a rebuild"
        );
        assert_eq!(
            healed
                .map
                .incomplete_receiver_intents(pending.page_id(), pending.path()),
            vec![pending.clone()],
            "necessity: a horizon blind to intent filenames silently drops durable receiver intents"
        );
        assert!(healed
            .map
            .receiver_history_paths()
            .contains(&(pending.page_id(), pending.path().clone())));
    }

    #[test]
    fn torn_summary_rebuilds_once_then_returns_to_the_steady_path() {
        let fixture = Fixture::new("torn-rebuild");
        let bytes = b"- durable projected bytes\n";
        let intent = fixture.intent(0xc6_1030, "pages/torn.md", 1, None, bytes);
        fixture.complete(&intent, None, bytes);
        let built = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(built.stats.full_catalog_passes, 1);
        drop(built);

        let latest = std::fs::read_dir(fixture.summary_dir())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(SUMMARY_PREFIX))
            })
            .max()
            .expect("summary generation exists");
        std::fs::write(&latest, b"{\"torn\":").unwrap();

        let rebuilt = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(rebuilt.stats.full_catalog_passes, 1);
        assert!(rebuilt.stats.rebuilt);
        assert_eq!(
            rebuilt.map.decision(intent.page_id(), intent.path()),
            AbsenceDecision::DeferredAbsence
        );
        drop(rebuilt);

        let steady = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(steady.stats.full_catalog_passes, 0);
        assert_eq!(steady.stats.delta_completions, 0);
        assert_eq!(
            steady.map.decision(intent.page_id(), intent.path()),
            AbsenceDecision::DeferredAbsence
        );
    }

    #[test]
    fn summary_ahead_of_completion_truth_rebuilds_instead_of_deciding_from_cache() {
        let fixture = Fixture::new("ahead-rebuild");
        let bytes = b"- completion later lost\n";
        let intent = fixture.intent(0xc6_1035, "pages/ahead.md", 1, None, bytes);
        fixture.complete(&intent, None, bytes);
        let built = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(
            built.map.decision(intent.page_id(), intent.path()),
            AbsenceDecision::DeferredAbsence
        );
        drop(built);

        std::fs::remove_file(
            fixture
                .receipts
                .root_path()
                .join("completions")
                .join(completion_filename(&intent).unwrap()),
        )
        .unwrap();
        let rebuilt = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(rebuilt.stats.full_catalog_passes, 1);
        assert_eq!(
            rebuilt.map.decision(intent.page_id(), intent.path()),
            AbsenceDecision::Create,
            "a summary ahead of retained truth must never supply the decision"
        );
    }

    #[test]
    fn repeated_intent_and_completion_updates_install_no_duplicate_generation() {
        let fixture = Fixture::new("idempotent-update");
        let bytes = b"- idempotent completion\n";
        let intent = fixture.intent(0xc6_1038, "pages/idempotent.md", 1, None, bytes);
        fixture.complete(&intent, None, bytes);
        let opened = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        let mut cache = opened.cache.expect("rebuild installs the cache");
        let before = enumerate_names(&cache.directory).unwrap().len();
        cache.record_intent(&intent).unwrap();
        cache.record_completion(&intent).unwrap();
        let after = enumerate_names(&cache.directory).unwrap().len();
        assert_eq!(after, before);
    }

    #[test]
    fn long_history_rebuild_is_one_full_pass_and_steady_open_is_page_bounded() {
        let fixture = Fixture::new("measured-cost");
        let mut prior: Option<Vec<u8>> = None;
        for generation in 1..=32_u64 {
            let target = format!("- projected generation {generation}\n").into_bytes();
            let intent = fixture.intent(
                0xc6_1040,
                "pages/history.md",
                generation,
                prior.as_deref(),
                &target,
            );
            fixture.complete(&intent, prior.as_deref(), &target);
            prior = Some(target);
        }

        let rebuild = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(rebuild.stats.full_catalog_passes, 1);
        assert_eq!(rebuild.map.receiver_summary_entries().len(), 1);
        drop(rebuild);

        let steady = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        assert_eq!(steady.stats.full_catalog_passes, 0);
        assert_eq!(steady.stats.receipt_content_reads, 0);
        assert!(steady.stats.summary_content_reads <= 2);
        assert_eq!(steady.map.receiver_summary_entries().len(), 1);
    }

    #[test]
    #[ignore = "manual measured gate: anonymized-corpus copy plus long receiver history"]
    fn anonymized_corpus_copy_receiver_summary_cost_probe() {
        let source = PathBuf::from(
            std::env::var_os("TINE_MS_AUDIT_GRAPH_COPY")
                .expect("TINE_MS_AUDIT_GRAPH_COPY must name the read-only anonymized corpus"),
        );
        let fixture = Fixture::copied_graph("corpus-cost", &source);
        let mut prior: Option<Vec<u8>> = None;
        for generation in 1..=512_u64 {
            let target = format!("- derived summary cost generation {generation}\n").into_bytes();
            let intent = fixture.intent(
                0xc6_1050,
                "pages/derived-summary-cost-probe.md",
                generation,
                prior.as_deref(),
                &target,
            );
            fixture.complete(&intent, prior.as_deref(), &target);
            prior = Some(target);
        }

        let rebuild = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        eprintln!("receiver-summary rebuild: {:?}", rebuild.stats);
        assert_eq!(rebuild.stats.full_catalog_passes, 1);
        assert_eq!(rebuild.stats.evidence_names_observed, 1024);
        assert_eq!(rebuild.map.receiver_summary_entries().len(), 1);
        drop(rebuild);

        let steady = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        eprintln!("receiver-summary steady: {:?}", steady.stats);
        assert_eq!(steady.stats.full_catalog_passes, 0);
        assert_eq!(steady.stats.receipt_content_reads, 0);
        assert!(steady.stats.summary_content_reads <= 2);
        assert_eq!(steady.map.receiver_summary_entries().len(), 1);
        drop(steady);

        crate::test_support::remove_dir_all(&fixture.summary_dir());
        let missing = ReceiverAbsenceSummary::open(&fixture.archive, &fixture.receipts).unwrap();
        eprintln!(
            "receiver-summary missing-cache rebuild: {:?}",
            missing.stats
        );
        assert_eq!(missing.stats.full_catalog_passes, 1);
        assert_eq!(missing.map.receiver_summary_entries().len(), 1);
    }

    fn copy_tree_without_symlinks(source: &PathBuf, destination: &PathBuf) {
        let metadata = std::fs::symlink_metadata(source).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "corpus copy refuses symlinks"
        );
        if metadata.is_dir() {
            std::fs::create_dir_all(destination).unwrap();
            for entry in std::fs::read_dir(source).unwrap() {
                let entry = entry.unwrap();
                copy_tree_without_symlinks(&entry.path(), &destination.join(entry.file_name()));
            }
        } else {
            std::fs::copy(source, destination).unwrap();
        }
    }
}
