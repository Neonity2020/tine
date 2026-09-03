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

    pub(crate) fn prose(message: impl Into<String>) -> Self {
        Self::Prose(message.into())
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

    fn variant_name(error: &CommandError) -> &'static str {
        match error {
            CommandError::Tagged { .. } => "Tagged",
            CommandError::Coded { .. } => "Coded",
            CommandError::Io { .. } => "Io",
            CommandError::Worker { .. } => "Worker",
            CommandError::Tauri { .. } => "Tauri",
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
}
