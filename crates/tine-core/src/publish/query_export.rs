//! Publish a query: export the complete pages that own a query's result rows
//! as one static site under `<graph root>/published-queries/<folder>/`.
//!
//! Selection is renderer-independent: [`plan_query_publication`] turns a
//! query into a reviewed page set with a fingerprint, and
//! [`publish_query_documents`] re-derives that set, refuses if the fingerprint
//! moved, and hands a closed [`PublicationSelection::Paths`] to the same
//! renderer the graph site uses. Nothing is persisted between the two calls:
//! the fingerprint is the only thing the dialog carries back.
//!
//! Spec: `tine-agents/specs/notes/2026-09-14-publish-query-stage1.md`.

use super::{
    slug, PublicationOutput, PublicationQueryRead, PublicationSelection, PublicationTarget,
    PublishOutcome,
};
use crate::doc;
use crate::model::{Graph, PageEntry, PageKind, RefGroup};
use crate::query::ir::{Anchor, Bounds, ExecutionContext, QueryRows, ViewSettings};
use crate::query::{QueryDialect, QueryInput};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;

/// Sibling of `publish/`; every query export is one leaf directory under it.
pub use crate::vocab::PUBLISHED_QUERIES_DIR;
/// Folder names are portable slugs; longer titles are truncated.
pub const FOLDER_MAX_CHARS: usize = 64;
const FOLDER_FALLBACK: &str = "query";
const SELECTION_MAX_ROWS: usize = 20_000;
const SELECTION_MAX_BYTES: usize = 32 * 1024 * 1024;

/// What the query surface executed, plus the user's export choices. `query`
/// is the EFFECTIVE source the surface ran (after any focused-page
/// substitution), so re-running it here reproduces the displayed answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPublicationRequest {
    pub query: String,
    pub advanced: bool,
    #[serde(default)]
    pub simple_dialect: Option<QueryDialect>,
    /// The `?current-page` binding the surface used, if any.
    #[serde(default)]
    pub current_page: Option<String>,
    /// The effective anchor-specific view (sampling lives here). `None` keeps
    /// whatever the source itself declares.
    #[serde(default)]
    pub view: Option<ViewSettings>,
    /// The block the query is written in; excluded from results exactly as
    /// the UI excludes it (GH #469).
    #[serde(default)]
    pub host_block_id: Option<String>,
    /// The host block's `tine.*` properties — the ones the app handed
    /// `parseQuery` — so the export's home block carries the same display
    /// settings (view, sample, sort) the query was exported under.
    #[serde(default)]
    pub host_properties: Vec<(String, String)>,
    /// Export name; the folder derives from it unless `folder` is given.
    pub name: String,
    /// The exact reviewed leaf directory name. Carried back unchanged from the
    /// plan so suffix allocation never silently runs again at commit.
    #[serde(default)]
    pub folder: Option<String>,
    /// Replace an existing export at `folder` (retired into recovery) instead
    /// of failing when it exists.
    #[serde(default)]
    pub replace: bool,
    /// Byte budget for copied assets (Settings → Graph); `None` = the default.
    #[serde(default)]
    pub asset_budget_bytes: Option<u64>,
    /// The frontend bundle the exporting binary embeds, attached by the Tauri
    /// layer (never by the wire): when present, the export also carries the
    /// read-only app under `app/` (Stage 2). `None` = static site only.
    #[serde(skip)]
    pub app_bundle: Option<std::sync::Arc<super::app_export::PublishedAppBundle>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPublicationPage {
    pub path: String,
    pub name: String,
    pub journal: bool,
}

/// The reviewed plan shown by the dialog and echoed back on confirm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPublicationPlan {
    /// `"block"` or `"page"`: a block-anchored query exports whole pages, and
    /// the dialog must say so.
    pub anchor: String,
    pub row_count: usize,
    pub sampled: bool,
    pub bound_page: Option<String>,
    pub pages: Vec<QueryPublicationPage>,
    pub folder: String,
    pub path: String,
    /// The reviewed folder already exists (Replace or Separate needed).
    pub exists: bool,
    /// First free `<folder>-N` when `exists`.
    pub suggested_folder: Option<String>,
    pub fingerprint: String,
}

#[derive(Debug)]
pub enum QueryPublicationError {
    /// A user-facing reason the export cannot proceed as requested.
    Refused(String),
    /// Copied assets would exceed the export's byte budget; the message names
    /// the asset, the limit and the setting that raises it. Typed so the UI
    /// can offer the setting in one click.
    AssetBudget(String),
    Io(io::Error),
}

impl std::fmt::Display for QueryPublicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(message) | Self::AssetBudget(message) => f.write_str(message),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for QueryPublicationError {}

impl From<io::Error> for QueryPublicationError {
    fn from(error: io::Error) -> Self {
        if let Some(exceeded) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<super::AssetBudgetExceeded>())
        {
            return Self::AssetBudget(exceeded.0.clone());
        }
        Self::Io(error)
    }
}

/// Portable folder name for an export: the publisher's slug, capped, never
/// empty.
pub fn query_export_folder(name: &str) -> String {
    let mut folder: String = slug(name).chars().take(FOLDER_MAX_CHARS).collect();
    while folder.ends_with('-') {
        folder.pop();
    }
    if folder.is_empty() {
        FOLDER_FALLBACK.to_string()
    } else {
        folder
    }
}

fn folder_is_well_formed(folder: &str) -> bool {
    !folder.is_empty()
        && folder.chars().count() <= FOLDER_MAX_CHARS + 8
        && folder
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !folder.starts_with('-')
        && !folder.ends_with('-')
}

/// One captured source page: entry + the revision the publisher recorded.
pub(crate) type CapturedSource = (PageEntry, String);

/// Remove the host block from the groups, keeping its children, exactly as the
/// frontend's `withoutHostBlock` does.
pub(super) fn without_host_block(groups: Vec<RefGroup>, host: Option<&str>) -> Vec<RefGroup> {
    let Some(host) = host else {
        return groups;
    };
    groups
        .into_iter()
        .filter_map(|mut group| {
            group.blocks.retain(|block| block.id != host);
            (!group.blocks.is_empty()).then_some(group)
        })
        .collect()
}

fn refusal(message: impl Into<String>) -> QueryPublicationError {
    QueryPublicationError::Refused(message.into())
}

/// Resolve the request against one coherent capture. Returns the plan and the
/// physical paths it selected.
pub(crate) fn plan_query_publication(
    graph: &Graph,
    capture: &[CapturedSource],
    reader: &dyn PublicationQueryRead,
    request: &QueryPublicationRequest,
) -> Result<(QueryPublicationPlan, BTreeSet<String>), QueryPublicationError> {
    if request.name.trim().is_empty() {
        return Err(refusal("Give the export a name."));
    }
    if !crate::query::query_source_within_limit(&request.query) {
        return Err(refusal(format!(
            "The query source exceeds the {} KiB export limit.",
            crate::query::QUERY_SOURCE_MAX_BYTES / 1024
        )));
    }
    if !crate::query::query_nesting_within_limit(&request.query) {
        return Err(refusal("The query nests too deeply to export safely."));
    }
    let input = if request.advanced {
        QueryInput::Advanced
    } else {
        match request.simple_dialect.unwrap_or(QueryDialect::Og) {
            QueryDialect::Og => QueryInput::Og,
            QueryDialect::Tql => QueryInput::Tql,
        }
    };
    let registry =
        crate::query::registry::Registry::from_snapshot(&crate::query::ir::RegistrySnapshot {
            rows: Vec::new(),
            generation: 0,
        });
    let (query, parsed_view) = crate::query::parse_query_input(
        &request.query,
        input,
        crate::date::JournalDate::today(),
        &registry,
    );
    let view = request.view.clone().unwrap_or(parsed_view);
    let context = ExecutionContext {
        current_page: request.current_page.clone(),
    };
    let bounds = Bounds {
        max_rows: SELECTION_MAX_ROWS,
        max_bytes: SELECTION_MAX_BYTES,
    };
    let result = reader
        .run(&query, &view, bounds, &context)
        .map_err(|error| refusal(error.to_string()))?;
    if !result.report.supported
        || result
            .diagnostics
            .iter()
            .any(|diagnostic| !diagnostic.disabled)
    {
        return Err(refusal(
            "This query can't be exported: Tine doesn't fully support it yet.",
        ));
    }
    if result.exceeded {
        return Err(refusal(format!(
            "The query has {} matches; narrow it before exporting.",
            result.matched_total.unwrap_or(result.total)
        )));
    }

    // Inventory by logical identity (for block groups, which carry a display
    // name) and by physical path (for page rows).
    let mut by_key: HashMap<String, Vec<&PageEntry>> = HashMap::new();
    let mut by_path: HashMap<&str, &PageEntry> = HashMap::new();
    for (entry, _) in capture {
        by_key
            .entry(crate::refs::page_key(&entry.name))
            .or_default()
            .push(entry);
        by_path.insert(entry.rel_path.as_str(), entry);
    }

    let mut selected: BTreeSet<String> = BTreeSet::new();
    let (anchor, row_count) = match result.rows {
        QueryRows::Block { groups } => {
            let groups = without_host_block(groups, request.host_block_id.as_deref());
            let row_count = groups.iter().map(|group| group.blocks.len()).sum();
            for group in &groups {
                let owners = by_key
                    .get(&crate::refs::page_key(&group.page))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                match owners {
                    [] => {
                        return Err(refusal(format!(
                            "A result belongs to \"{}\", which has no file in this graph; refresh and try again.",
                            group.page
                        )))
                    }
                    [owner] => {
                        selected.insert(owner.rel_path.clone());
                    }
                    _ => {
                        return Err(refusal(format!(
                            "Two files claim the page \"{}\"; resolve the duplicate before exporting it.",
                            group.page
                        )))
                    }
                }
            }
            (Anchor::Block, row_count)
        }
        QueryRows::Page { pages } => {
            let row_count = pages.len();
            for page in &pages {
                let Some(entry) = by_path.get(page.path.as_str()) else {
                    return Err(refusal(format!(
                        "A result page \"{}\" is no longer in this graph; refresh and try again.",
                        page.name
                    )));
                };
                if entry.kind != page.kind || entry.name != page.name {
                    return Err(refusal(format!(
                        "The page \"{}\" changed while the query ran; refresh and try again.",
                        page.name
                    )));
                }
                let twins = by_key
                    .get(&crate::refs::page_key(&entry.name))
                    .map_or(0, Vec::len);
                if twins != 1 {
                    return Err(refusal(format!(
                        "Two files claim the page \"{}\"; resolve the duplicate before exporting it.",
                        entry.name
                    )));
                }
                selected.insert(entry.rel_path.clone());
            }
            (Anchor::Page, row_count)
        }
    };

    let folder = match &request.folder {
        Some(folder) => {
            if !folder_is_well_formed(folder) {
                return Err(refusal("The export folder name is not valid."));
            }
            folder.clone()
        }
        None => query_export_folder(&request.name),
    };
    let parent = graph.root.join(PUBLISHED_QUERIES_DIR);
    let path = parent.join(&folder);
    let exists = std::fs::symlink_metadata(&path).is_ok();
    let suggested_folder = exists.then(|| {
        (2u32..)
            .map(|n| format!("{folder}-{n}"))
            .find(|candidate| std::fs::symlink_metadata(parent.join(candidate)).is_err())
            .unwrap_or_else(|| format!("{folder}-{}", u32::MAX))
    });

    let mut pages = Vec::with_capacity(selected.len());
    let mut hasher = Sha256::new();
    hasher.update(b"tine/query-publication/v1\0");
    hasher.update(match anchor {
        Anchor::Block => b"block\0".as_slice(),
        Anchor::Page => b"page\0".as_slice(),
    });
    hasher.update((row_count as u64).to_be_bytes());
    hasher.update(folder.as_bytes());
    hasher.update(b"\0");
    for rel_path in &selected {
        let (entry, revision) = capture
            .iter()
            .find(|(entry, _)| entry.rel_path == *rel_path)
            .expect("selected paths come from the capture");
        for part in [
            entry.rel_path.as_str(),
            entry.name.as_str(),
            if entry.kind == PageKind::Journal {
                "journal"
            } else {
                "page"
            },
            revision.as_str(),
        ] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        pages.push(QueryPublicationPage {
            path: entry.rel_path.clone(),
            name: entry.name.clone(),
            journal: entry.kind == PageKind::Journal,
        });
    }
    let fingerprint = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    Ok((
        QueryPublicationPlan {
            anchor: match anchor {
                Anchor::Block => "block".into(),
                Anchor::Page => "page".into(),
            },
            row_count,
            sampled: view.sample.is_some(),
            bound_page: request.current_page.clone(),
            pages,
            folder,
            path: path.display().to_string(),
            exists,
            suggested_folder,
            fingerprint,
        },
        selected,
    ))
}

/// Confirmed export: re-plan on the fresh capture, refuse if anything the user
/// reviewed moved, then render the closed set.
pub(crate) fn publish_query_documents(
    graph: &Graph,
    capture: Vec<(PageEntry, doc::Document, String)>,
    reader: &dyn PublicationQueryRead,
    request: &QueryPublicationRequest,
    expected_fingerprint: &str,
) -> Result<PublishOutcome, QueryPublicationError> {
    let sources: Vec<CapturedSource> = capture
        .iter()
        .map(|(entry, _, revision)| (entry.clone(), revision.clone()))
        .collect();
    let (plan, selected) = plan_query_publication(graph, &sources, reader, request)?;
    if plan.fingerprint != expected_fingerprint {
        return Err(refusal(
            "The results changed since you reviewed them; review the export again.",
        ));
    }
    if selected.is_empty() {
        return Err(refusal("The query has no results; nothing to export."));
    }
    if plan.exists && !request.replace {
        return Err(refusal(format!(
            "An export already exists at {}; replace it or choose another name.",
            plan.path
        )));
    }
    let target = PublicationTarget {
        selection: PublicationSelection::Paths(selected),
        output: PublicationOutput {
            parent: PathBuf::from(PUBLISHED_QUERIES_DIR),
            leaf: plan.folder.clone(),
            replace: request.replace,
        },
        asset_budget_bytes: Some(
            request
                .asset_budget_bytes
                .unwrap_or(super::QUERY_EXPORT_DEFAULT_ASSET_BUDGET_BYTES),
        ),
        app: request
            .app_bundle
            .as_ref()
            .map(|bundle| super::app_export::AppPublication {
                name: request.name.clone(),
                bundle: std::sync::Arc::clone(bundle),
                home: super::app_export::AppHome::Query(super::app_export::HomeQuery {
                    source: request.query.clone(),
                    advanced: request.advanced,
                    simple_dialect: request.simple_dialect,
                    current_page: request.current_page.clone(),
                    view: request.view.clone(),
                    host_block_id: request.host_block_id.clone(),
                    host_properties: request.host_properties.clone(),
                }),
            }),
    };
    let pages = capture
        .into_iter()
        .map(|(entry, document, _)| (entry, document))
        .collect();
    Ok(super::publish_graph_documents_inner(
        graph,
        pages,
        Some(reader),
        &target,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_is_a_capped_nonempty_slug() {
        assert_eq!(
            query_export_folder("Reading list 2026!"),
            "reading-list-2026"
        );
        assert_eq!(query_export_folder("   "), "query");
        assert_eq!(query_export_folder("---"), "query");
        let long = "x".repeat(200);
        assert_eq!(query_export_folder(&long).len(), FOLDER_MAX_CHARS);
        let dash_at_cap = format!("{} tail", "y".repeat(FOLDER_MAX_CHARS - 1));
        assert!(!query_export_folder(&dash_at_cap).ends_with('-'));
    }

    #[test]
    fn reviewed_folder_must_be_well_formed() {
        assert!(folder_is_well_formed("reading-list-2"));
        assert!(!folder_is_well_formed(""));
        assert!(!folder_is_well_formed("../x"));
        assert!(!folder_is_well_formed("a/b"));
        assert!(!folder_is_well_formed("-lead"));
        assert!(!folder_is_well_formed("Upper"));
    }

    #[test]
    fn host_block_is_dropped_but_its_group_survives_when_others_match() {
        use crate::model::BlockDto;
        let block = |id: &str| BlockDto {
            id: id.into(),
            ..Default::default()
        };
        let groups = vec![
            RefGroup {
                page: "a".into(),
                kind: PageKind::Page,
                blocks: vec![block("host"), block("other")],
                evidence: Default::default(),
            },
            RefGroup {
                page: "b".into(),
                kind: PageKind::Page,
                blocks: vec![block("host")],
                evidence: Default::default(),
            },
        ];
        let kept = without_host_block(groups, Some("host"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].page, "a");
        assert_eq!(kept[0].blocks.len(), 1);
    }
}
