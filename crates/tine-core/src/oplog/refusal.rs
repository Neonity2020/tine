use std::fmt;

/// Stable threat-model identifier carried by every durable managed-storage
/// refusal that crosses a public runtime boundary. These values are the
/// executable counterpart of `docs/storage-sync-contract.md` section 3.1.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ManagedStorageRefusalScenario {
    CrashTruncated,
    DiskCorrupt,
    SyncConflict,
    ConcurrentWriter,
    StaleGeneration,
    UnsafeFilesystemKind,
    MalformedImport,
    Bounds,
    ProtocolIncompatible,
}

impl ManagedStorageRefusalScenario {
    pub const ALL: [Self; 9] = [
        Self::CrashTruncated,
        Self::DiskCorrupt,
        Self::SyncConflict,
        Self::ConcurrentWriter,
        Self::StaleGeneration,
        Self::UnsafeFilesystemKind,
        Self::MalformedImport,
        Self::Bounds,
        Self::ProtocolIncompatible,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CrashTruncated => "MS-REF-CRASH-TRUNCATED",
            Self::DiskCorrupt => "MS-REF-DISK-CORRUPT",
            Self::SyncConflict => "MS-REF-SYNC-CONFLICT",
            Self::ConcurrentWriter => "MS-REF-CONCURRENT-WRITER",
            Self::StaleGeneration => "MS-REF-STALE-GENERATION",
            Self::UnsafeFilesystemKind => "MS-REF-UNSAFE-FS-KIND",
            Self::MalformedImport => "MS-REF-MALFORMED-IMPORT",
            Self::Bounds => "MS-REF-BOUNDS",
            Self::ProtocolIncompatible => "MS-REF-PROTOCOL-INCOMPATIBLE",
        }
    }

    pub(crate) fn marked_in(detail: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|scenario| detail.contains(scenario.as_str()))
    }

    /// Translate a durable blocked-enrollment reason written by this or an
    /// earlier build. Unknown codes are handled by the caller as protocol
    /// incompatibility; they must never be silently guessed into a recovery.
    pub(crate) fn for_blocked_reason(reason_code: &str) -> Option<Self> {
        BLOCKED_REASON_SCENARIOS
            .iter()
            .find_map(|(known, scenario)| (*known == reason_code).then_some(*scenario))
    }
}

/// Complete vocabulary of blocked reason codes emitted by production managed
/// storage. Keep this table data-shaped so the source guard can compare it to
/// every literal at the two construction boundaries.
pub(crate) const BLOCKED_REASON_SCENARIOS: [(&str, ManagedStorageRefusalScenario); 8] = [
    (
        "explicit_identity_binding_mismatch",
        ManagedStorageRefusalScenario::SyncConflict,
    ),
    (
        "shared.descriptor-conflict",
        ManagedStorageRefusalScenario::SyncConflict,
    ),
    (
        "shared.dirty-unique-tail",
        ManagedStorageRefusalScenario::SyncConflict,
    ),
    (
        "shared.incompatible-descriptor",
        ManagedStorageRefusalScenario::ProtocolIncompatible,
    ),
    (
        "shared.local-proof-mismatch",
        ManagedStorageRefusalScenario::DiskCorrupt,
    ),
    (
        "shared.projection-base-mismatch",
        ManagedStorageRefusalScenario::SyncConflict,
    ),
    (
        "shared.unsafe-handoff",
        ManagedStorageRefusalScenario::StaleGeneration,
    ),
    ("proof.failed", ManagedStorageRefusalScenario::DiskCorrupt),
];

impl fmt::Display for ManagedStorageRefusalScenario {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
