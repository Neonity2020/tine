use super::*;

pub(super) fn embedded_app_bundle(
    app: &tauri::AppHandle,
) -> tine_core::publish::app_export::PublishedAppBundle {
    use tine_core::publish::app_export::PublishedAppBundle;
    let resolver = app.asset_resolver();
    let paths: Vec<String> = resolver
        .iter()
        .map(|(path, _)| path.into_owned())
        .filter(|path| PublishedAppBundle::ships(path))
        .collect();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        if let Some(asset) = resolver.get(path.clone()) {
            files.push((path.trim_start_matches('/').to_string(), asset.bytes));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    PublishedAppBundle { files }
}

pub(super) fn query_publication_error(
    error: tine_core::publish::query_export::QueryPublicationError,
) -> CommandError {
    use tine_core::publish::query_export::QueryPublicationError;
    match error {
        QueryPublicationError::Refused(message) => CommandError::prose(message),
        QueryPublicationError::AssetBudget(message) => query_export_budget_error(message),
        QueryPublicationError::Io(error) => CommandError::from(error),
    }
}

/// Reason code the export dialog keys its "Adjust limit in Settings" action on.
pub(crate) const QUERY_EXPORT_BUDGET_REASON: &str = "export_asset_budget_exceeded";

fn query_export_budget_error(message: String) -> CommandError {
    CommandError::tagged(
        "query-unavailable",
        Some(QUERY_EXPORT_BUDGET_REASON),
        Some(serde_json::json!({ "message": message })),
    )
}
