//! Immutable projection work locators retained by the clean runtime.
//!
//! Work status, queueing, and completed-path authority live in accepted
//! history plus disposable SQLite. This module deliberately contains no
//! persistent work-index control plane.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::identity::{parse_digest, write_hex};
use super::{
    BatchId, BlobDescription, FrontierV2, LogicalCompletionId, ManagedPath, ManifestObjectRef,
    PageId, PortablePathIndexRoot, PortablePathKeyDigest, ProjectionEndpointId, ProjectionIntentId,
    WorkspaceId, PORTABLE_PATH_KEY_VERSION,
};

const WORK_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectionWorkId([u8; 32]);

impl ProjectionWorkId {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ProjectionWorkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProjectionWorkId({self})")
    }
}

impl fmt::Display for ProjectionWorkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl FromStr for ProjectionWorkId {
    type Err = super::identity::DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value).map(Self)
    }
}

impl Serialize for ProjectionWorkId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ProjectionWorkId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionWorkTarget {
    Absent,
    Present(BlobDescription),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionWork {
    schema_version: u32,
    work_id: ProjectionWorkId,
    workspace_id: WorkspaceId,
    endpoint_id: ProjectionEndpointId,
    graph_resource_id: super::CanonicalGraphResourceId,
    batch_id: BatchId,
    page_id: PageId,
    path: ManagedPath,
    portable_path_key_version: u32,
    portable_path_key_digest: PortablePathKeyDigest,
    portable_path_index_root: PortablePathIndexRoot,
    intent: ManifestObjectRef,
    post_frontier: FrontierV2,
    target: ProjectionWorkTarget,
}

impl ProjectionWork {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace_id: WorkspaceId,
        endpoint_id: ProjectionEndpointId,
        graph_resource_id: super::CanonicalGraphResourceId,
        batch_id: BatchId,
        page_id: PageId,
        path: ManagedPath,
        portable_path_index_root: PortablePathIndexRoot,
        intent: ManifestObjectRef,
        post_frontier: FrontierV2,
        target: ProjectionWorkTarget,
    ) -> Self {
        let portable_path_key_digest = path.portable_key().digest();
        let work_id = work_id(
            endpoint_id,
            graph_resource_id,
            batch_id,
            page_id,
            &path,
            portable_path_key_digest,
            portable_path_index_root,
        );
        Self {
            schema_version: WORK_SCHEMA_VERSION,
            work_id,
            workspace_id,
            endpoint_id,
            graph_resource_id,
            batch_id,
            page_id,
            path,
            portable_path_key_version: PORTABLE_PATH_KEY_VERSION,
            portable_path_key_digest,
            portable_path_index_root,
            intent,
            post_frontier,
            target,
        }
    }

    pub const fn work_id(&self) -> ProjectionWorkId {
        self.work_id
    }
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }
    pub const fn endpoint_id(&self) -> ProjectionEndpointId {
        self.endpoint_id
    }
    pub const fn graph_resource_id(&self) -> super::CanonicalGraphResourceId {
        self.graph_resource_id
    }
    pub const fn batch_id(&self) -> BatchId {
        self.batch_id
    }
    pub const fn page_id(&self) -> PageId {
        self.page_id
    }
    pub fn path(&self) -> &ManagedPath {
        &self.path
    }
    pub const fn portable_path_key_version(&self) -> u32 {
        self.portable_path_key_version
    }
    pub const fn portable_path_key_digest(&self) -> PortablePathKeyDigest {
        self.portable_path_key_digest
    }
    pub const fn portable_path_index_root(&self) -> PortablePathIndexRoot {
        self.portable_path_index_root
    }
    pub const fn intent(&self) -> &ManifestObjectRef {
        &self.intent
    }
    pub const fn post_frontier(&self) -> &FrontierV2 {
        &self.post_frontier
    }
    pub const fn target(&self) -> ProjectionWorkTarget {
        self.target
    }
}

/// A completed receipt selected through accepted clean history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionCompletedReceipt {
    page_id: PageId,
    path: ManagedPath,
    frontier: FrontierV2,
    target: ProjectionWorkTarget,
    intent_id: ProjectionIntentId,
    logical_completion_id: LogicalCompletionId,
}

impl ProjectionCompletedReceipt {}

fn work_id(
    endpoint_id: ProjectionEndpointId,
    graph_resource_id: super::CanonicalGraphResourceId,
    batch_id: BatchId,
    page_id: PageId,
    path: &ManagedPath,
    portable_path_key_digest: PortablePathKeyDigest,
    portable_path_index_root: PortablePathIndexRoot,
) -> ProjectionWorkId {
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-work-id/v3\0");
    for part in [
        endpoint_id.as_uuid().as_bytes().as_slice(),
        graph_resource_id.as_bytes(),
        batch_id.as_uuid().as_bytes().as_slice(),
        page_id.as_uuid().as_bytes().as_slice(),
        path.as_str().as_bytes(),
        portable_path_key_digest.as_bytes(),
        portable_path_index_root.digest().as_bytes(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    ProjectionWorkId(hasher.finalize().into())
}
