use serde::ser::Serializer;
use serde::Serialize;
use std::fmt;

/// The one error type accepted at Tine's Tauri command boundary.
///
/// Serialization deliberately remains a JSON string so the frontend receives
/// byte-for-byte the rejection values it classified before this type existed.
#[derive(Debug)]
pub(crate) enum CommandError {
    Tagged {
        kind: &'static str,
        reason_code: Option<String>,
        detail: Option<serde_json::Value>,
    },
    Coded {
        code: &'static str,
        detail: String,
    },
    Io {
        kind: std::io::ErrorKind,
        message: String,
    },
    Worker {
        message: String,
    },
    Tauri {
        message: String,
    },
    Json {
        message: String,
    },
    Plugin {
        message: String,
    },
    Clipboard {
        message: String,
    },
    Platform {
        message: String,
    },
    GraphVerification {
        message: String,
    },
    Graph {
        message: String,
    },
    SyncRuntime {
        message: String,
    },
    Settings {
        message: String,
    },
    Diagnostic {
        message: String,
    },
    Backup {
        message: String,
    },
    Core(CoreCommandError),
    Prose(String),
}

#[derive(Debug)]
pub(crate) enum CoreCommandError {
    SyncApplicationPageRequest(tine_core::sync_runtime::SyncApplicationPageRequestError),
    SyncEditorRequest(tine_core::sync_runtime::SyncEditorRequestError),
    SyncLocalMutationRequest(tine_core::sync_runtime::SyncLocalMutationRequestError),
    SyncRuntimeRequest(tine_core::sync_runtime::SyncRuntimeRequestError),
    FastCommit(tine_core::fast_commit::FastCommitError),
    MergeRefused(tine_core::sync_diff::MergeRefused),
}

pub(crate) trait IntoCommandErrorProse {
    fn into_command_error(self) -> CommandError;
}

impl IntoCommandErrorProse for CommandError {
    fn into_command_error(self) -> CommandError {
        self
    }
}

impl IntoCommandErrorProse for String {
    fn into_command_error(self) -> CommandError {
        CommandError::Prose(self)
    }
}

impl IntoCommandErrorProse for &'static str {
    fn into_command_error(self) -> CommandError {
        CommandError::Prose(self.to_owned())
    }
}

impl CommandError {
    pub(crate) fn tagged(
        kind: &'static str,
        reason_code: Option<impl Into<String>>,
        detail: Option<serde_json::Value>,
    ) -> Self {
        Self::Tagged {
            kind,
            reason_code: reason_code.map(Into::into),
            detail,
        }
    }

    pub(crate) fn coded(code: &'static str, detail: impl Into<String>) -> Self {
        Self::Coded {
            code,
            detail: detail.into(),
        }
    }

    pub(crate) fn worker(error: tauri::Error) -> Self {
        Self::Worker {
            message: error.to_string(),
        }
    }

    pub(crate) fn prose(message: impl IntoCommandErrorProse) -> Self {
        message.into_command_error()
    }

    fn family(message: impl fmt::Display) -> String {
        message.to_string()
    }

    pub(crate) fn json(error: impl fmt::Display) -> Self {
        Self::Json {
            message: Self::family(error),
        }
    }

    pub(crate) fn plugin(error: impl fmt::Display) -> Self {
        Self::Plugin {
            message: Self::family(error),
        }
    }

    pub(crate) fn clipboard(error: impl fmt::Display) -> Self {
        Self::Clipboard {
            message: Self::family(error),
        }
    }

    pub(crate) fn platform(error: impl fmt::Display) -> Self {
        Self::Platform {
            message: Self::family(error),
        }
    }

    pub(crate) fn graph_verification(error: impl fmt::Display) -> Self {
        Self::GraphVerification {
            message: Self::family(error),
        }
    }

    pub(crate) fn graph(error: impl fmt::Display) -> Self {
        Self::Graph {
            message: Self::family(error),
        }
    }

    pub(crate) fn sync_runtime(error: impl fmt::Display) -> Self {
        Self::SyncRuntime {
            message: Self::family(error),
        }
    }

    pub(crate) fn settings(error: impl fmt::Display) -> Self {
        Self::Settings {
            message: Self::family(error),
        }
    }

    pub(crate) fn diagnostic(error: impl fmt::Display) -> Self {
        Self::Diagnostic {
            message: Self::family(error),
        }
    }

    pub(crate) fn backup(error: impl fmt::Display) -> Self {
        Self::Backup {
            message: Self::family(error),
        }
    }

    pub(crate) fn contains(&self, needle: &str) -> bool {
        self.wire().contains(needle)
    }

    fn wire(&self) -> String {
        match self {
            Self::Tagged {
                kind,
                reason_code,
                detail,
            } => match (reason_code.as_deref(), detail.clone()) {
                (Some(reason_code), Some(detail)) => {
                    tine_core::sync_runtime::tagged_backend_error_with_reason_and_detail(
                        kind,
                        reason_code,
                        detail,
                    )
                }
                (None, Some(detail)) => {
                    tine_core::sync_runtime::tagged_backend_error_with_detail(kind, detail)
                }
                (reason_code, None) => {
                    tine_core::sync_runtime::tagged_backend_error(kind, reason_code)
                }
            },
            Self::Coded { code, detail } => format!("{code}: {detail}"),
            Self::Io { message, .. }
            | Self::Worker { message }
            | Self::Tauri { message }
            | Self::Json { message }
            | Self::Plugin { message }
            | Self::Clipboard { message }
            | Self::Platform { message }
            | Self::GraphVerification { message }
            | Self::Graph { message }
            | Self::SyncRuntime { message }
            | Self::Settings { message }
            | Self::Diagnostic { message }
            | Self::Backup { message }
            | Self::Prose(message) => message.clone(),
            Self::Core(error) => error.wire(),
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.wire())
    }
}

impl std::error::Error for CommandError {}

impl PartialEq for CommandError {
    fn eq(&self, other: &Self) -> bool {
        self.wire() == other.wire()
    }
}

impl PartialEq<str> for CommandError {
    fn eq(&self, other: &str) -> bool {
        self.wire() == other
    }
}

impl PartialEq<&str> for CommandError {
    fn eq(&self, other: &&str) -> bool {
        self.wire() == *other
    }
}

impl Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.wire())
    }
}

impl fmt::Display for CoreCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SyncApplicationPageRequest(error) => error.fmt(formatter),
            Self::SyncEditorRequest(error) => error.fmt(formatter),
            Self::SyncLocalMutationRequest(error) => error.fmt(formatter),
            Self::SyncRuntimeRequest(error) => error.fmt(formatter),
            Self::FastCommit(error) => error.fmt(formatter),
            Self::MergeRefused(error) => error.fmt(formatter),
        }
    }
}

impl CoreCommandError {
    fn wire(&self) -> String {
        match self {
            Self::SyncApplicationPageRequest(error) => error.backend_wire_string(),
            _ => self.to_string(),
        }
    }
}

impl From<std::io::Error> for CommandError {
    fn from(error: std::io::Error) -> Self {
        if error
            .get_ref()
            .is_some_and(|source| source.is::<serde_json::Error>())
        {
            return Self::json(error);
        }
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl From<tauri::Error> for CommandError {
    fn from(error: tauri::Error) -> Self {
        Self::Tauri {
            message: error.to_string(),
        }
    }
}

impl From<serde_json::Error> for CommandError {
    fn from(error: serde_json::Error) -> Self {
        Self::json(error)
    }
}

macro_rules! core_conversion {
    ($source:path, $variant:ident) => {
        impl From<$source> for CommandError {
            fn from(error: $source) -> Self {
                Self::Core(CoreCommandError::$variant(error))
            }
        }
    };
}

core_conversion!(
    tine_core::sync_runtime::SyncApplicationPageRequestError,
    SyncApplicationPageRequest
);
core_conversion!(
    tine_core::sync_runtime::SyncEditorRequestError,
    SyncEditorRequest
);
core_conversion!(
    tine_core::sync_runtime::SyncLocalMutationRequestError,
    SyncLocalMutationRequest
);
core_conversion!(
    tine_core::sync_runtime::SyncRuntimeRequestError,
    SyncRuntimeRequest
);
core_conversion!(tine_core::fast_commit::FastCommitError, FastCommit);
core_conversion!(tine_core::sync_diff::MergeRefused, MergeRefused);

#[cfg(test)]
mod tests {
    use super::{CommandError, CoreCommandError};

    #[derive(Clone, Copy)]
    struct ProducerManifestRow {
        targets: &'static str,
        file: &'static str,
        enclosing_symbol: &'static str,
        source_error_type: &'static str,
        required_variant: &'static str,
        production_mapper: &'static str,
        format_family: &'static str,
        legacy_wire_template: &'static str,
        golden_test: &'static str,
    }

    const PRODUCER_MANIFEST: &[ProducerManifestRow] = &[
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs",
            enclosing_symbol: "direct_save_error_message(conflict branch)",
            source_error_type: "DirectSaveError",
            required_variant: "Tagged",
            production_mapper: "direct_save_error_message",
            format_family: "tagged/reason/detail",
            legacy_wire_template: r#"{"detail":{"epoch":7,"io_error_kind":"AlreadyExists"},"kind":"save-conflict","reason_code":"conflict.base_rev"}"#,
            golden_test: "phase_a_production_wire_matches_legacy",
        },
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs",
            enclosing_symbol: "direct_save_error_message(failure branch)",
            source_error_type: "DirectSaveError",
            required_variant: "Tagged",
            production_mapper: "direct_save_error_message",
            format_family: "tagged/reason/detail",
            legacy_wire_template: r#"{"detail":{"io_error_kind":"PermissionDenied"},"kind":"direct-save-failure","reason_code":"unknown"}"#,
            golden_test: "phase_a_production_wire_matches_legacy",
        },
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs",
            enclosing_symbol: "close_graph_window",
            source_error_type: "CleanShutdownSlot refusal",
            required_variant: "Tagged",
            production_mapper: "CommandError::tagged",
            format_family: "tagged/kind",
            legacy_wire_template: r#"{"kind":"sparse-shutdown-refused"}"#,
            golden_test: "phase_a_production_wire_matches_legacy",
        },
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs,state.rs",
            enclosing_symbol: "io-returning Graph/filesystem sites",
            source_error_type: "std::io::Error",
            required_variant: "Io",
            production_mapper: "CommandError::from",
            format_family: "display",
            legacy_wire_template: "denied",
            golden_test: "phase_a_production_wire_matches_legacy",
        },
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs",
            enclosing_symbol: "spawn_blocking await sites",
            source_error_type: "tauri::Error",
            required_variant: "Worker",
            production_mapper: "CommandError::worker",
            format_family: "display",
            legacy_wire_template: "task 7 panicked",
            golden_test: "phase_a_production_wire_matches_legacy",
        },
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs",
            enclosing_symbol: "non-worker Tauri calls",
            source_error_type: "tauri::Error",
            required_variant: "Tauri",
            production_mapper: "CommandError::from",
            format_family: "display",
            legacy_wire_template: "tauri failure",
            golden_test: "phase_a_production_wire_matches_legacy",
        },
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs",
            enclosing_symbol: "typed sync request sites",
            source_error_type: "tine_core typed errors",
            required_variant: "Core",
            production_mapper: "CommandError::from",
            format_family: "core display/tagged",
            legacy_wire_template: "sync actor is unavailable",
            golden_test: "phase_a_production_wire_matches_legacy",
        },
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs",
            enclosing_symbol: "bounded refusal sites",
            source_error_type: "bounded domain refusal",
            required_variant: "Coded",
            production_mapper: "CommandError::coded",
            format_family: "code-colon-detail",
            legacy_wire_template: "query-too-large: 2049 bytes",
            golden_test: "phase_a_production_wire_matches_legacy",
        },
        ProducerManifestRow {
            targets: "all",
            file: "commands.rs,state.rs",
            enclosing_symbol: "literal/context-only remainder",
            source_error_type: "no typed source",
            required_variant: "Prose",
            production_mapper: "CommandError::prose",
            format_family: "prose",
            legacy_wire_template: "legacy prose",
            golden_test: "phase_a_production_wire_matches_legacy",
        },
    ];

    const PHASE_B_PRODUCER_MANIFEST: &[ProducerManifestRow] = &[
        ProducerManifestRow { targets: "all", file: "backup.rs,conflict_capsule.rs,graph.rs,settings.rs,sync_runtime.rs", enclosing_symbol: "filesystem and audited-publication sites", source_error_type: "std::io::Error", required_variant: "Io", production_mapper: "CommandError::from", format_family: "source-display", legacy_wire_template: "denied", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "backup.rs,graph.rs,platform.rs,sync_runtime.rs", enclosing_symbol: "spawn_blocking await sites", source_error_type: "tauri::Error::JoinError", required_variant: "Worker", production_mapper: "CommandError::worker", format_family: "worker-display", legacy_wire_template: "asset not found: worker", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "android_folder_picker.rs,android_media.rs,android_system_bars.rs,ios_folder_picker.rs,lib.rs", enclosing_symbol: "non-worker Tauri/mobile calls", source_error_type: "tauri::Error", required_variant: "Tauri", production_mapper: "CommandError::from", format_family: "tauri-display", legacy_wire_template: "asset not found: tauri", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "conflict_capsule.rs,graph_verification.rs,plugins.rs,settings.rs,sync_runtime.rs", enclosing_symbol: "JSON encode/decode sites", source_error_type: "serde_json::Error", required_variant: "Json", production_mapper: "CommandError::from or CommandError::json", format_family: "json-display", legacy_wire_template: "json failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "plugins.rs", enclosing_symbol: "plugin package/registry producers", source_error_type: "PackageStoreError or plugin validation", required_variant: "Plugin", production_mapper: "CommandError::plugin", format_family: "plugin-display", legacy_wire_template: "plugin failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "desktop,android,ios", file: "platform.rs", enclosing_symbol: "read_clipboard_files,copy_image_to_clipboard", source_error_type: "clipboard provider error", required_variant: "Clipboard", production_mapper: "CommandError::clipboard", format_family: "clipboard-display", legacy_wire_template: "clipboard failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "platform.rs,lib.rs,android_*.rs,ios_folder_picker.rs", enclosing_symbol: "platform and non-worker host producers", source_error_type: "platform error", required_variant: "Platform", production_mapper: "CommandError::platform", format_family: "platform-display", legacy_wire_template: "platform failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "graph_verification.rs", enclosing_symbol: "graph verification format/dialog producers", source_error_type: "graph-verification source error", required_variant: "GraphVerification", production_mapper: "CommandError::graph_verification", format_family: "graph-verification-display", legacy_wire_template: "graph verification failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "graph.rs,watcher.rs", enclosing_symbol: "graph lifecycle producers", source_error_type: "graph source error", required_variant: "Graph", production_mapper: "CommandError::graph", format_family: "graph-display", legacy_wire_template: "graph failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "storage_mode_supervisor.rs,sync_runtime.rs", enclosing_symbol: "managed lifecycle/context producers", source_error_type: "sync/runtime source error", required_variant: "SyncRuntime", production_mapper: "CommandError::sync_runtime", format_family: "sync-runtime-display", legacy_wire_template: "sync runtime failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "settings.rs", enclosing_symbol: "settings UUID/lock/context producers", source_error_type: "settings source error", required_variant: "Settings", production_mapper: "CommandError::settings", format_family: "settings-display", legacy_wire_template: "settings failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "debug.rs", enclosing_symbol: "diagnostic dialog/recorder producers", source_error_type: "diagnostic source error", required_variant: "Diagnostic", production_mapper: "CommandError::diagnostic", format_family: "diagnostic-display", legacy_wire_template: "diagnostic failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "backup.rs", enclosing_symbol: "backup validation/restore producers", source_error_type: "backup source error", required_variant: "Backup", production_mapper: "CommandError::backup", format_family: "backup-display", legacy_wire_template: "backup failure", golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "graph_verification.rs", enclosing_symbol: "graph_verification_cancelled_error", source_error_type: "cancellation state", required_variant: "Tagged", production_mapper: "graph_verification_cancelled_error", format_family: "tagged-operation-cancelled", legacy_wire_template: r#"{"kind":"operation-cancelled"}"#, golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "sync_runtime.rs", enclosing_symbol: "shared_enrollment_not_here_yet", source_error_type: "absent shared enrollment", required_variant: "Tagged", production_mapper: "shared_enrollment_not_here_yet", format_family: "tagged-sync-data-unavailable", legacy_wire_template: r#"{"kind":"sync-data-unavailable"}"#, golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "sync_runtime.rs", enclosing_symbol: "adoption_archived_error", source_error_type: "post-archive join failure", required_variant: "Tagged", production_mapper: "adoption_archived_error", format_family: "tagged-adoption-archived", legacy_wire_template: r#"{"kind":"adoption-archived"}"#, golden_test: "phase_b_production_wire_matches_legacy" },
        ProducerManifestRow { targets: "all", file: "phase-B files", enclosing_symbol: "literal/context-only remainder", source_error_type: "no typed source", required_variant: "Prose", production_mapper: "CommandError::prose", format_family: "prose", legacy_wire_template: "phase-B prose", golden_test: "phase_b_production_wire_matches_legacy" },
    ];

    fn variant_name(error: &CommandError) -> &'static str {
        match error {
            CommandError::Tagged { .. } => "Tagged",
            CommandError::Coded { .. } => "Coded",
            CommandError::Io { .. } => "Io",
            CommandError::Worker { .. } => "Worker",
            CommandError::Tauri { .. } => "Tauri",
            CommandError::Json { .. } => "Json",
            CommandError::Plugin { .. } => "Plugin",
            CommandError::Clipboard { .. } => "Clipboard",
            CommandError::Platform { .. } => "Platform",
            CommandError::GraphVerification { .. } => "GraphVerification",
            CommandError::Graph { .. } => "Graph",
            CommandError::SyncRuntime { .. } => "SyncRuntime",
            CommandError::Settings { .. } => "Settings",
            CommandError::Diagnostic { .. } => "Diagnostic",
            CommandError::Backup { .. } => "Backup",
            CommandError::Core(_) => "Core",
            CommandError::Prose(_) => "Prose",
        }
    }

    #[test]
    fn phase_a_production_wire_matches_legacy() {
        use std::io;
        use tine_core::model::{DirectSaveError, DirectSaveFailureCode};

        let conflict = DirectSaveError::into_io_with_conflict_epoch(
            DirectSaveFailureCode::ConflictBaseRev,
            Some(7),
            io::Error::new(io::ErrorKind::AlreadyExists, "conflict"),
        );
        let denied = DirectSaveError::into_io(
            DirectSaveFailureCode::Unknown,
            io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        );
        let families = [
            (
                crate::commands::direct_save_error_message(conflict),
                PRODUCER_MANIFEST[0],
            ),
            (
                crate::commands::direct_save_error_message(denied),
                PRODUCER_MANIFEST[1],
            ),
            (
                CommandError::tagged("sparse-shutdown-refused", None::<String>, None),
                PRODUCER_MANIFEST[2],
            ),
            (
                CommandError::coded("query-too-large", "2049 bytes"),
                PRODUCER_MANIFEST[7],
            ),
            (
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into(),
                PRODUCER_MANIFEST[3],
            ),
            (
                CommandError::Worker {
                    message: "task 7 panicked".into(),
                },
                PRODUCER_MANIFEST[4],
            ),
            (
                CommandError::Tauri {
                    message: "tauri failure".into(),
                },
                PRODUCER_MANIFEST[5],
            ),
            (
                CommandError::Core(CoreCommandError::SyncRuntimeRequest(
                    tine_core::sync_runtime::SyncRuntimeRequestError::ActorUnavailable,
                )),
                PRODUCER_MANIFEST[6],
            ),
            (CommandError::prose("legacy prose"), PRODUCER_MANIFEST[8]),
        ];
        let mut proven = std::collections::BTreeSet::new();
        for (error, row) in families {
            assert_eq!(
                variant_name(&error),
                row.required_variant,
                "{}",
                row.enclosing_symbol
            );
            assert_eq!(
                serde_json::to_value(error).unwrap(),
                row.legacy_wire_template,
                "{} / {}",
                row.production_mapper,
                row.format_family,
            );
            assert_eq!(row.golden_test, "phase_a_production_wire_matches_legacy");
            assert!(
                !row.targets.is_empty()
                    && !row.file.is_empty()
                    && !row.source_error_type.is_empty()
            );
            proven.insert(row.format_family);
        }
        assert_eq!(
            proven,
            PRODUCER_MANIFEST
                .iter()
                .map(|row| row.format_family)
                .collect(),
            "every mechanically derived format family has a golden production case",
        );
    }

    #[test]
    fn phase_b_production_wire_matches_legacy() {
        let json_through_atomic_io = CommandError::from(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            serde_json::Error::io(std::io::Error::other("json failure")),
        ));
        assert_eq!(variant_name(&json_through_atomic_io), "Json");
        assert_eq!(
            tauri::ipc::InvokeError::from(json_through_atomic_io).0,
            "json failure"
        );
        let cases = [
            CommandError::from(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            )),
            CommandError::worker(tauri::Error::AssetNotFound("worker".into())),
            CommandError::from(tauri::Error::AssetNotFound("tauri".into())),
            CommandError::from(serde_json::Error::io(std::io::Error::other("json failure"))),
            CommandError::plugin("plugin failure"),
            CommandError::clipboard("clipboard failure"),
            CommandError::platform("platform failure"),
            CommandError::graph_verification("graph verification failure"),
            CommandError::graph("graph failure"),
            CommandError::sync_runtime("sync runtime failure"),
            CommandError::settings("settings failure"),
            CommandError::diagnostic("diagnostic failure"),
            CommandError::backup("backup failure"),
            crate::graph_verification::graph_verification_cancelled_error(),
            crate::sync_runtime::shared_enrollment_not_here_yet(std::path::Path::new(
                "/not-on-wire",
            )),
            crate::sync_runtime::adoption_archived_error(),
            CommandError::prose("phase-B prose"),
        ];
        assert_eq!(cases.len(), PHASE_B_PRODUCER_MANIFEST.len());
        let mut proven = std::collections::BTreeSet::new();
        for (error, row) in cases.into_iter().zip(PHASE_B_PRODUCER_MANIFEST) {
            assert_eq!(
                variant_name(&error),
                row.required_variant,
                "{}",
                row.enclosing_symbol
            );
            let invoke: tauri::ipc::InvokeError = error.into();
            assert_eq!(
                invoke.0, row.legacy_wire_template,
                "{}",
                row.production_mapper
            );
            assert_eq!(row.golden_test, "phase_b_production_wire_matches_legacy");
            assert!(
                !row.targets.is_empty()
                    && !row.file.is_empty()
                    && !row.source_error_type.is_empty()
            );
            proven.insert(row.format_family);
        }
        assert_eq!(proven.len(), PHASE_B_PRODUCER_MANIFEST.len());
    }
}
