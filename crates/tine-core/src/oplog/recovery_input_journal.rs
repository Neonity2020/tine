//! Durable custody for provider originals that cannot be admitted above a
//! shallow document floor.
//!
//! This is a third `LocalJournalSegmentV2` sequence domain. It is independent
//! of foreground semantic sequences and projection turns: a frame records only
//! transport custody, never acceptance. The selected segment is itself the
//! restart trigger; there is deliberately no second durable recovery flag.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use tine_storage::{
    DurableDirectoryPublication, LocalJournalAppendError, LocalJournalFrame, LocalJournalSegmentV2,
    LocalJournalSegmentV2Selection,
};
use uuid::Uuid;

use super::object_store::{ensure_directory_nofollow, open_dir_nofollow, read_optional_regular};
use super::sync_layout::MANAGED_LOCAL_JOURNAL_DIR;
use super::{
    BatchId, DeviceId, LineageDigest, ManifestedProjectionIntent, ObjectKind, OperationBatch,
    OperationObject, PreparedBatch, ProjectionEndpointId, WorkspaceId,
};

const RECOVERY_INPUT_SCHEMA_VERSION: u32 = 1;
const RECOVERY_INPUT_ANCHOR_SCHEMA_VERSION: u32 = 1;
const RECOVERY_INPUT_ANCHOR_FILE: &str = "recovery-input.anchor-v1";
const RECOVERY_INPUT_ANCHOR_MAX_BYTES: u64 = 16 * 1024;
const RECOVERY_INPUT_WORKSPACE_PREFIX: &str = "recovery-inputs";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryInputPayloadKind {
    PendingOriginalV1,
}

/// One current envelope: exact canonical transport bytes plus both ends of
/// their graph/endpoint binding. Object order is manifest order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryInputEnvelopeV1 {
    schema_version: u32,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    receiving_endpoint_id: ProjectionEndpointId,
    receiving_device_id: DeviceId,
    pub(crate) source_endpoint_id: ProjectionEndpointId,
    pub(crate) batch_id: BatchId,
    pub(crate) manifest: Vec<u8>,
    pub(crate) objects: Vec<Vec<u8>>,
}

impl RecoveryInputEnvelopeV1 {
    fn from_prepared(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        receiving_endpoint_id: ProjectionEndpointId,
        receiving_device_id: DeviceId,
        prepared: &PreparedBatch,
    ) -> Result<Self, String> {
        let manifest = prepared
            .manifest()
            .encode()
            .map_err(|error| format!("cannot encode recovery-input manifest: {error}"))?;
        let objects = prepared
            .objects()
            .iter()
            .map(|object| {
                object
                    .encode()
                    .map_err(|error| format!("cannot encode recovery-input object: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let source_endpoint_id = source_endpoint(prepared)?;
        let envelope = Self {
            schema_version: RECOVERY_INPUT_SCHEMA_VERSION,
            workspace_id,
            lineage_digest,
            receiving_endpoint_id,
            receiving_device_id,
            source_endpoint_id,
            batch_id: prepared.manifest().batch_id(),
            manifest,
            objects,
        };
        envelope.validate(
            workspace_id,
            lineage_digest,
            receiving_endpoint_id,
            receiving_device_id,
        )?;
        Ok(envelope)
    }

    fn encode(&self) -> Result<Vec<u8>, String> {
        postcard::to_allocvec(self)
            .map_err(|error| format!("cannot encode recovery-input envelope: {error}"))
    }

    fn decode(
        bytes: &[u8],
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        receiving_endpoint_id: ProjectionEndpointId,
        receiving_device_id: DeviceId,
    ) -> Result<Self, String> {
        let envelope: Self = postcard::from_bytes(bytes)
            .map_err(|error| format!("cannot decode recovery-input envelope: {error}"))?;
        if envelope.encode()? != bytes {
            return Err("recovery-input envelope is not canonical".into());
        }
        envelope.validate(
            workspace_id,
            lineage_digest,
            receiving_endpoint_id,
            receiving_device_id,
        )?;
        Ok(envelope)
    }

    fn validate(
        &self,
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        receiving_endpoint_id: ProjectionEndpointId,
        receiving_device_id: DeviceId,
    ) -> Result<(), String> {
        if self.schema_version != RECOVERY_INPUT_SCHEMA_VERSION
            || self.workspace_id != workspace_id
            || self.lineage_digest != lineage_digest
            || self.receiving_endpoint_id != receiving_endpoint_id
            || self.receiving_device_id != receiving_device_id
        {
            return Err("recovery-input envelope binding is invalid".into());
        }
        let manifest = OperationBatch::decode(&self.manifest)
            .map_err(|error| format!("recovery-input manifest is invalid: {error}"))?;
        if manifest.batch_id() != self.batch_id
            || manifest.workspace_id() != workspace_id
            || manifest.lineage_digest() != lineage_digest
        {
            return Err("recovery-input manifest binding is invalid".into());
        }
        let objects = self
            .objects
            .iter()
            .map(|bytes| {
                OperationObject::decode(bytes)
                    .map_err(|error| format!("recovery-input object is invalid: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let prepared = PreparedBatch::new(manifest, objects)
            .map_err(|error| format!("recovery-input batch is invalid: {error}"))?;
        if source_endpoint(&prepared)? != self.source_endpoint_id {
            return Err("recovery-input source endpoint is invalid".into());
        }
        Ok(())
    }

    pub(crate) fn prepared_batch(&self) -> Result<PreparedBatch, String> {
        let manifest = OperationBatch::decode(&self.manifest)
            .map_err(|error| format!("recovery-input manifest is invalid: {error}"))?;
        let objects = self
            .objects
            .iter()
            .map(|bytes| {
                OperationObject::decode(bytes)
                    .map_err(|error| format!("recovery-input object is invalid: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        PreparedBatch::new(manifest, objects)
            .map_err(|error| format!("recovery-input batch is invalid: {error}"))
    }
}

fn source_endpoint(prepared: &PreparedBatch) -> Result<ProjectionEndpointId, String> {
    let mut endpoints = BTreeSet::new();
    for object in prepared
        .objects()
        .iter()
        .filter(|object| object.kind() == ObjectKind::ProjectionIntent)
    {
        let intent = ManifestedProjectionIntent::decode(object.payload())
            .map_err(|error| format!("cannot decode recovery-input projection intent: {error}"))?;
        endpoints.insert(intent.source_endpoint_id());
    }
    match endpoints.into_iter().collect::<Vec<_>>().as_slice() {
        [endpoint] => Ok(*endpoint),
        [] => Err("below-floor original has no source endpoint".into()),
        _ => Err("below-floor original names multiple source endpoints".into()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryInputAnchorV1 {
    schema_version: u32,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    endpoint_id: ProjectionEndpointId,
    device_id: DeviceId,
    segment_id: Uuid,
    segment_name: String,
}

impl RecoveryInputAnchorV1 {
    fn new(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        endpoint_id: ProjectionEndpointId,
        device_id: DeviceId,
    ) -> Self {
        let segment_id = Uuid::new_v4();
        Self {
            schema_version: RECOVERY_INPUT_ANCHOR_SCHEMA_VERSION,
            workspace_id,
            lineage_digest,
            endpoint_id,
            device_id,
            segment_id,
            segment_name: recovery_input_segment_name(endpoint_id, segment_id),
        }
    }

    fn selection(&self) -> Result<LocalJournalSegmentV2Selection, String> {
        LocalJournalSegmentV2Selection::new(
            self.segment_name.clone(),
            self.segment_id,
            self.device_id.as_uuid(),
            0,
        )
        .map_err(|error| format!("cannot select recovery-input segment: {error}"))
    }

    fn encode(&self) -> Result<Vec<u8>, String> {
        postcard::to_allocvec(self)
            .map_err(|error| format!("cannot encode recovery-input anchor: {error}"))
    }

    fn decode(
        bytes: &[u8],
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        endpoint_id: ProjectionEndpointId,
        device_id: DeviceId,
    ) -> Result<Self, String> {
        let anchor: Self = postcard::from_bytes(bytes)
            .map_err(|error| format!("cannot decode recovery-input anchor: {error}"))?;
        if anchor.schema_version != RECOVERY_INPUT_ANCHOR_SCHEMA_VERSION
            || anchor.workspace_id != workspace_id
            || anchor.lineage_digest != lineage_digest
            || anchor.endpoint_id != endpoint_id
            || anchor.device_id != device_id
            || anchor.segment_id.is_nil()
            || anchor.segment_name != recovery_input_segment_name(endpoint_id, anchor.segment_id)
            || anchor.encode()? != bytes
        {
            return Err("recovery-input anchor binding is invalid".into());
        }
        Ok(anchor)
    }
}

fn recovery_input_segment_name(endpoint_id: ProjectionEndpointId, segment_id: Uuid) -> String {
    format!(
        "endpoint-{}-segment-{}.recovery-input.journal-v2",
        endpoint_id.as_uuid().simple(),
        segment_id.simple()
    )
}

fn recovery_input_workspace_name(
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    endpoint_id: ProjectionEndpointId,
) -> String {
    format!("{RECOVERY_INPUT_WORKSPACE_PREFIX}-{workspace_id}-{lineage_digest}-{endpoint_id}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryInputCustody {
    Appended,
    AlreadyPresent,
}

pub(crate) struct RecoveryInputJournal {
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    endpoint_id: ProjectionEndpointId,
    device_id: DeviceId,
    directory: Dir,
    selection: Option<LocalJournalSegmentV2Selection>,
    segment: Option<LocalJournalSegmentV2<RecoveryInputPayloadKind>>,
    payloads: BTreeMap<BatchId, Vec<u8>>,
    envelopes: BTreeMap<BatchId, RecoveryInputEnvelopeV1>,
}

impl RecoveryInputJournal {
    pub(crate) fn open(
        application_runtime_root: &Path,
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        endpoint_id: ProjectionEndpointId,
        device_id: DeviceId,
    ) -> Result<Self, String> {
        let root = Dir::open_ambient_dir(application_runtime_root, ambient_authority())
            .map_err(|error| format!("cannot retain recovery-input root: {error}"))?;
        ensure_directory_nofollow(&root, MANAGED_LOCAL_JOURNAL_DIR)
            .map_err(|error| error.to_string())?;
        let namespace = open_dir_nofollow(&root, MANAGED_LOCAL_JOURNAL_DIR)
            .map_err(|error| error.to_string())?;
        let workspace_name =
            recovery_input_workspace_name(workspace_id, lineage_digest, endpoint_id);
        ensure_directory_nofollow(&namespace, &workspace_name)
            .map_err(|error| error.to_string())?;
        let directory =
            open_dir_nofollow(&namespace, &workspace_name).map_err(|error| error.to_string())?;
        let anchor = read_optional_regular(
            &directory,
            RECOVERY_INPUT_ANCHOR_FILE,
            RECOVERY_INPUT_ANCHOR_MAX_BYTES,
            None,
        )
        .map_err(|error| format!("cannot read recovery-input anchor: {error}"))?
        .map(|bytes| {
            RecoveryInputAnchorV1::decode(
                &bytes,
                workspace_id,
                lineage_digest,
                endpoint_id,
                device_id,
            )
        })
        .transpose()?;
        let (selection, segment) = match anchor {
            Some(anchor) => {
                let selection = anchor.selection()?;
                let (segment, _) = LocalJournalSegmentV2::open_selected(&directory, &selection)
                    .map_err(|error| format!("cannot open recovery-input segment: {error}"))?;
                (Some(selection), Some(segment))
            }
            // Empty custody must be byte-for-byte inert. The first below-floor
            // original creates the anchored segment before attempting append.
            None => (None, None),
        };
        let mut journal = Self {
            workspace_id,
            lineage_digest,
            endpoint_id,
            device_id,
            directory,
            selection,
            segment,
            payloads: BTreeMap::new(),
            envelopes: BTreeMap::new(),
        };
        if journal.segment.is_some() {
            journal.reindex()?;
        }
        Ok(journal)
    }

    pub(crate) fn is_pending(&self) -> bool {
        !self.envelopes.is_empty()
    }

    pub(crate) fn pending_envelopes(&self) -> Vec<RecoveryInputEnvelopeV1> {
        self.envelopes.values().cloned().collect()
    }

    /// The authenticated segment and durable append prefix selected by the
    /// recovery-input anchor. Pending custody always has a selection; empty
    /// custody deliberately has none and creates no storage.
    pub(crate) fn recovery_fence(&self) -> Result<(&str, u64, u64), String> {
        let selection = self
            .selection
            .as_ref()
            .ok_or_else(|| "pending recovery-input has no selected segment".to_owned())?;
        let segment = self
            .segment
            .as_ref()
            .ok_or_else(|| "pending recovery-input has no open segment".to_owned())?;
        Ok((
            selection.segment_name(),
            selection.base_sequence(),
            segment.next_sequence(),
        ))
    }

    /// Retire every currently selected input only after the caller has opened
    /// the newly published checkpoint and proved exact archive coverage. The
    /// replacement anchor selects a fresh empty segment first; the predecessor
    /// segment then becomes disposable crash residue and is unlinked only as
    /// best-effort cleanup.
    pub(crate) fn complete_successful_reinstall(&mut self) -> Result<(), String> {
        if !self.is_pending() {
            return Ok(());
        }
        let old_selection = self
            .selection
            .clone()
            .ok_or_else(|| "pending recovery-input has no selected segment".to_owned())?;
        let expected_anchor = read_optional_regular(
            &self.directory,
            RECOVERY_INPUT_ANCHOR_FILE,
            RECOVERY_INPUT_ANCHOR_MAX_BYTES,
            None,
        )
        .map_err(|error| format!("cannot read installed recovery-input anchor: {error}"))?
        .ok_or_else(|| "pending recovery-input anchor is missing".to_owned())?;
        RecoveryInputAnchorV1::decode(
            &expected_anchor,
            self.workspace_id,
            self.lineage_digest,
            self.endpoint_id,
            self.device_id,
        )?;

        let replacement = RecoveryInputAnchorV1::new(
            self.workspace_id,
            self.lineage_digest,
            self.endpoint_id,
            self.device_id,
        );
        let replacement_selection = replacement.selection()?;
        LocalJournalSegmentV2::<RecoveryInputPayloadKind>::prepare_single_writer(
            &self.directory,
            &replacement_selection,
        )
        .map_err(|error| format!("cannot prepare empty recovery-input successor: {error}"))?;
        let replacement_bytes = replacement.encode()?;
        DurableDirectoryPublication::open(&self.directory)
            .map_err(|error| format!("recovery-input retirement is unavailable: {error}"))?
            .replace_exact(
                RECOVERY_INPUT_ANCHOR_FILE,
                &expected_anchor,
                &replacement_bytes,
            )
            .map_err(|error| format!("cannot retire installed recovery inputs: {error}"))?;

        drop(self.segment.take());
        let (segment, recovery) =
            LocalJournalSegmentV2::open_selected(&self.directory, &replacement_selection)
                .map_err(|error| format!("cannot open empty recovery-input successor: {error}"))?;
        if recovery.frames_recovered != 0 || segment.next_sequence() != 0 {
            return Err("recovery-input successor is unexpectedly nonempty".into());
        }
        self.selection = Some(replacement_selection);
        self.segment = Some(segment);
        self.payloads.clear();
        self.envelopes.clear();

        let _ = self.directory.remove_file(old_selection.segment_name());
        let _ = self.directory.remove_file(old_selection.frontier_name());
        Ok(())
    }

    pub(crate) fn retain(
        &mut self,
        prepared: &PreparedBatch,
        inject_uncertain_after_append: bool,
    ) -> Result<RecoveryInputCustody, String> {
        let envelope = RecoveryInputEnvelopeV1::from_prepared(
            self.workspace_id,
            self.lineage_digest,
            self.endpoint_id,
            self.device_id,
            prepared,
        )?;
        let payload = envelope.encode()?;
        if let Some(existing) = self.payloads.get(&envelope.batch_id) {
            return if existing == &payload {
                Ok(RecoveryInputCustody::AlreadyPresent)
            } else {
                Err(format!(
                    "recovery-input batch {} collides with different original bytes",
                    envelope.batch_id
                ))
            };
        }

        self.ensure_segment()?;

        let mut retry_after_reopen = true;
        let mut inject = inject_uncertain_after_append;
        loop {
            let append = self
                .segment
                .as_mut()
                .expect("recovery-input segment is open")
                .append(RecoveryInputPayloadKind::PendingOriginalV1, &payload);
            let uncertain = match append {
                Ok(_) if inject => {
                    inject = false;
                    true
                }
                Ok(_) => {
                    self.payloads.insert(envelope.batch_id, payload);
                    self.envelopes.insert(envelope.batch_id, envelope);
                    return Ok(RecoveryInputCustody::Appended);
                }
                Err(LocalJournalAppendError::DefinitelyNotAppended(error)) => {
                    return Err(format!("recovery-input append did not start: {error}"));
                }
                Err(LocalJournalAppendError::AppendOutcomeUnknown(_)) => true,
            };
            debug_assert!(uncertain);
            self.reopen_and_reindex()?;
            if let Some(existing) = self.payloads.get(&envelope.batch_id) {
                return if existing == &payload {
                    Ok(RecoveryInputCustody::Appended)
                } else {
                    Err(format!(
                        "recovery-input batch {} collides after uncertain append",
                        envelope.batch_id
                    ))
                };
            }
            if !retry_after_reopen {
                return Err(format!(
                    "recovery-input append for {} remains uncertain after exact reopen",
                    envelope.batch_id
                ));
            }
            retry_after_reopen = false;
        }
    }

    #[cfg(test)]
    pub(crate) fn envelopes_for_test(&self) -> Vec<RecoveryInputEnvelopeV1> {
        // Replay the physical frames instead of returning the deduplicated
        // lookup index: the uncertain-append boundary test must fail if code
        // ever retries blindly and writes a second identical frame.
        let Some(segment) = self.segment.as_ref() else {
            return Vec::new();
        };
        let mut envelopes = Vec::new();
        segment
            .replay(|frame| {
                envelopes.push(
                    RecoveryInputEnvelopeV1::decode(
                        frame.payload(),
                        self.workspace_id,
                        self.lineage_digest,
                        self.endpoint_id,
                        self.device_id,
                    )
                    .expect("test probe replays already validated recovery-input frames"),
                );
            })
            .expect("test probe replays an already opened recovery-input segment");
        envelopes
    }

    #[cfg(test)]
    pub(crate) fn redeliver_first_for_test(&mut self) -> Result<RecoveryInputCustody, String> {
        let envelope = self
            .envelopes
            .values()
            .next()
            .cloned()
            .ok_or_else(|| "recovery-input test has no durable envelope".to_owned())?;
        let manifest = OperationBatch::decode(&envelope.manifest)
            .map_err(|error| format!("cannot decode test redelivery manifest: {error}"))?;
        let objects = envelope
            .objects
            .iter()
            .map(|bytes| OperationObject::decode(bytes).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let prepared = PreparedBatch::new(manifest, objects).map_err(|error| error.to_string())?;
        self.retain(&prepared, false)
    }

    fn reopen_and_reindex(&mut self) -> Result<(), String> {
        drop(self.segment.take());
        let selection = self
            .selection
            .as_ref()
            .ok_or_else(|| "recovery-input segment has no durable selector".to_owned())?;
        let (segment, _) = LocalJournalSegmentV2::open_selected(&self.directory, selection)
            .map_err(|error| format!("cannot resolve uncertain recovery-input append: {error}"))?;
        self.segment = Some(segment);
        self.reindex()
    }

    fn ensure_segment(&mut self) -> Result<(), String> {
        if self.segment.is_some() {
            return Ok(());
        }

        // Resolve a prior uncertain anchor publication before allocating a new
        // identity. Exact reread keeps retries from colliding with themselves.
        if let Some(bytes) = read_optional_regular(
            &self.directory,
            RECOVERY_INPUT_ANCHOR_FILE,
            RECOVERY_INPUT_ANCHOR_MAX_BYTES,
            None,
        )
        .map_err(|error| format!("cannot resolve recovery-input anchor: {error}"))?
        {
            let anchor = RecoveryInputAnchorV1::decode(
                &bytes,
                self.workspace_id,
                self.lineage_digest,
                self.endpoint_id,
                self.device_id,
            )?;
            let selection = anchor.selection()?;
            let (segment, _) = LocalJournalSegmentV2::open_selected(&self.directory, &selection)
                .map_err(|error| format!("cannot reopen recovery-input segment: {error}"))?;
            self.selection = Some(selection);
            self.segment = Some(segment);
            return self.reindex();
        }

        let anchor = RecoveryInputAnchorV1::new(
            self.workspace_id,
            self.lineage_digest,
            self.endpoint_id,
            self.device_id,
        );
        let selection = anchor.selection()?;
        let anchor_bytes = anchor.encode()?;
        LocalJournalSegmentV2::<RecoveryInputPayloadKind>::prepare_single_writer(
            &self.directory,
            &selection,
        )
        .map_err(|error| format!("cannot prepare recovery-input segment: {error}"))?;
        let publication = DurableDirectoryPublication::open(&self.directory).map_err(|error| {
            format!("recovery-input anchor publication is unavailable: {error}")
        })?;
        let publication_result =
            publication.publish_new_exact_single_writer(RECOVERY_INPUT_ANCHOR_FILE, &anchor_bytes);
        let confirmed = read_optional_regular(
            &self.directory,
            RECOVERY_INPUT_ANCHOR_FILE,
            RECOVERY_INPUT_ANCHOR_MAX_BYTES,
            None,
        )
        .map_err(|error| format!("cannot confirm recovery-input anchor: {error}"))?;
        match confirmed {
            Some(bytes) if bytes == anchor_bytes => {}
            Some(_) => {
                return Err("recovery-input anchor collides with different durable bytes".into())
            }
            None => {
                return Err(match publication_result {
                    Ok(()) => "recovery-input anchor publication was not durable".into(),
                    Err(error) => format!("cannot publish recovery-input anchor: {error}"),
                })
            }
        }
        let (segment, recovery) = LocalJournalSegmentV2::open_selected(&self.directory, &selection)
            .map_err(|error| format!("cannot open new recovery-input segment: {error}"))?;
        if recovery.frames_recovered != 0
            || recovery.discarded_tail_bytes != 0
            || segment.next_sequence() != 0
        {
            return Err("new recovery-input segment is unexpectedly nonempty".into());
        }
        self.selection = Some(selection);
        self.segment = Some(segment);
        self.reindex()
    }

    fn reindex(&mut self) -> Result<(), String> {
        let mut frames = Vec::<LocalJournalFrame<RecoveryInputPayloadKind>>::new();
        self.segment
            .as_ref()
            .expect("recovery-input segment is open")
            .replay(|frame| frames.push(frame))
            .map_err(|error| format!("cannot replay recovery-input segment: {error}"))?;
        self.payloads.clear();
        self.envelopes.clear();
        for frame in frames {
            if frame.payload_kind() != RecoveryInputPayloadKind::PendingOriginalV1 {
                return Err("recovery-input segment contains an unknown payload kind".into());
            }
            let envelope = RecoveryInputEnvelopeV1::decode(
                frame.payload(),
                self.workspace_id,
                self.lineage_digest,
                self.endpoint_id,
                self.device_id,
            )?;
            match self.payloads.get(&envelope.batch_id) {
                Some(existing) if existing == frame.payload() => continue,
                Some(_) => {
                    return Err(format!(
                        "recovery-input batch {} has conflicting durable frames",
                        envelope.batch_id
                    ))
                }
                None => {
                    self.payloads
                        .insert(envelope.batch_id, frame.payload().to_vec());
                    self.envelopes.insert(envelope.batch_id, envelope);
                }
            }
        }
        Ok(())
    }
}

// Kept cheap and source-level because these values are promises in the living
// contract, not tunables.
#[cfg(test)]
mod tests {
    #[test]
    fn recovery_input_contract_names_the_current_format_and_restart_trigger() {
        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        assert!(contract.contains("recovery-input envelope schema 1"));
        assert!(contract.contains("The recovery-input segment itself is the restart trigger"));
    }
}
