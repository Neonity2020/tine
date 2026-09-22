#[cfg(test)]
use clap::CommandFactory;
use clap::{ArgAction, Args, Parser, Subcommand};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tine_core::publish::app_export::{AppHome, PublishedAppBundle};
use tine_core::publish::{publish_graph_app_to, publish_graph_to, PublicationOutput};
use tine_core::Graph;

#[derive(Debug, Parser)]
#[command(
    name = "tine",
    about = "A fast, local-first outliner for Logseq-compatible graphs",
    version = env!("CARGO_PKG_VERSION"),
    disable_version_flag = true,
    subcommand_precedence_over_arg = true
)]
struct Cli {
    /// Print the Tine version and exit.
    #[arg(short = 'v', visible_short_alias = 'V', long = "version", action = ArgAction::Version)]
    show_version: Option<bool>,

    /// Enable diagnostic logging for the desktop app.
    #[arg(long, global = true)]
    debug: bool,

    /// Open Quick Capture (legacy spelling; prefer `tine capture`).
    #[arg(long, conflicts_with = "graph")]
    capture: bool,

    /// Internal Guide-copy smoke-test switch.
    #[arg(long, hide = true)]
    tine_ci_copy_guide: bool,

    /// Graph to open in the desktop app (legacy shorthand for `tine open GRAPH`).
    #[arg(value_name = "GRAPH", value_hint = clap::ValueHint::DirPath)]
    graph: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open a graph in the desktop app.
    Open {
        #[arg(value_name = "GRAPH", value_hint = clap::ValueHint::DirPath)]
        graph: PathBuf,
    },
    /// Open Quick Capture in the desktop app.
    Capture,
    /// Export public pages from a graph.
    Export {
        #[command(subcommand)]
        format: ExportFormat,
    },
    /// Check that a graph can be discovered and parsed without changing it.
    Doctor {
        #[arg(value_name = "GRAPH", value_hint = clap::ValueHint::DirPath)]
        graph: PathBuf,
    },
    /// Print the Tine version and exit.
    Version,
}

#[derive(Debug, Subcommand)]
enum ExportFormat {
    /// Write the static HTML site.
    Static(ExportArgs),
    /// Write the read-only Tine app, with the static site as its fallback.
    Live(LiveExportArgs),
}

#[derive(Debug, Args)]
struct ExportArgs {
    #[arg(value_name = "GRAPH", value_hint = clap::ValueHint::DirPath)]
    graph: PathBuf,

    /// Graph-relative output directory.
    #[arg(long, default_value = "publish", value_name = "DIRECTORY")]
    output: PathBuf,

    /// Publish every page, ignoring `public:: true` selection for this run.
    #[arg(long)]
    all_pages: bool,

    /// Retire an existing output into recovery and install the new export.
    #[arg(long)]
    replace: bool,
}

#[derive(Debug, Args)]
struct LiveExportArgs {
    #[command(flatten)]
    export: ExportArgs,

    /// Page to open first. Defaults to configured home, Welcome to Tine, or the first page.
    #[arg(long, value_name = "PAGE")]
    home: Option<String>,

    /// Name shown in the published app. Defaults to the graph directory name.
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaunchRequest {
    Focus,
    Open(PathBuf),
    Capture,
}

pub(crate) enum Startup {
    Gui,
    Exit(i32),
}

#[derive(Debug)]
struct CliError(String);

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        Self(message.to_string())
    }
}

type CliResult<T> = Result<T, CliError>;

fn terminal_stdout(arguments: std::fmt::Arguments<'_>) {
    let mut output = std::io::stdout().lock();
    let _ = writeln!(output, "{arguments}");
}

fn terminal_stderr(arguments: std::fmt::Arguments<'_>) {
    let mut output = std::io::stderr().lock();
    let _ = writeln!(output, "{arguments}");
}

pub(crate) fn dispatch_env() -> Startup {
    let argv = std::env::args_os().collect::<Vec<_>>();
    prepare_console_if_needed(&argv);
    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() { 2 } else { 0 };
            let _ = error.print();
            return Startup::Exit(code);
        }
    };
    match cli.command {
        Some(Command::Version) => {
            terminal_stdout(format_args!("tine {}", env!("CARGO_PKG_VERSION")));
            Startup::Exit(0)
        }
        Some(Command::Doctor { graph }) => command_result(doctor(&graph)),
        Some(Command::Export { format }) => command_result(export(format)),
        _ => Startup::Gui,
    }
}

fn command_result(result: CliResult<()>) -> Startup {
    match result {
        Ok(()) => Startup::Exit(0),
        Err(message) => {
            terminal_stderr(format_args!("tine: {message}"));
            Startup::Exit(1)
        }
    }
}

fn open_graph(path: &Path) -> CliResult<(Graph, tempfile::TempDir)> {
    let root = path
        .canonicalize()
        .map_err(|error| format!("cannot open graph {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("graph is not a directory: {}", root.display()).into());
    }
    let projection = tempfile::Builder::new()
        .prefix("tine-cli-projection-")
        .tempdir()
        .map_err(|error| format!("cannot create temporary derived state: {error}"))?;
    let graph = Graph::open(&root);
    graph
        .attach_direct_projection(projection.path().join("direct.sqlite"))
        .map_err(|error| format!("cannot prepare graph index: {error}"))?;
    graph.warm_cache();
    Ok((graph, projection))
}

fn export(format: ExportFormat) -> CliResult<()> {
    match format {
        ExportFormat::Static(args) => {
            let (mut graph, _projection) = open_graph(&args.graph)?;
            graph.config_mut().all_pages_public |= args.all_pages;
            let output = publication_output(&args.output, args.replace)?;
            let outcome = publish_graph_to(&graph, output)
                .map_err(|error| format!("static export failed: {error}"))?;
            print_outcome(&outcome);
            Ok(())
        }
        ExportFormat::Live(args) => {
            let (mut graph, _projection) = open_graph(&args.export.graph)?;
            graph.config_mut().all_pages_public |= args.export.all_pages;
            let output = publication_output(&args.export.output, args.export.replace)?;
            let context: tauri::Context<tauri::Wry> = tauri::generate_context!();
            let bundle = embedded_app_bundle(&context);
            if bundle.index().is_none() {
                return Err(
                    "this binary has no embedded frontend; use an installed or release Tine build"
                        .into(),
                );
            }
            let name = args.name.unwrap_or_else(|| {
                graph
                    .root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Tine export")
                    .to_string()
            });
            let home = args.home.map(AppHome::Page).unwrap_or(AppHome::Auto);
            let outcome = publish_graph_app_to(&graph, Arc::new(bundle), &name, home, output)
                .map_err(|error| format!("live export failed: {error}"))?;
            print_outcome(&outcome);
            Ok(())
        }
    }
}

fn print_outcome(outcome: &tine_core::publish::PublishOutcome) {
    terminal_stdout(format_args!(
        "Published {} pages to {}",
        outcome.pages, outcome.path
    ));
    if let Some(retired) = &outcome.retired {
        terminal_stdout(format_args!("Previous output retained at {retired}"));
    }
    for warning in &outcome.warnings {
        terminal_stderr(format_args!("warning: {warning}"));
    }
}

fn embedded_app_bundle(context: &tauri::Context<tauri::Wry>) -> PublishedAppBundle {
    let assets = context.assets();
    let paths = assets
        .iter()
        .map(|(path, _)| path.into_owned())
        .filter(|path| PublishedAppBundle::ships(path))
        .collect::<Vec<_>>();
    let mut files = paths
        .into_iter()
        .filter_map(|path| {
            let key = tauri::utils::assets::AssetKey::from(path.as_str());
            assets
                .get(&key)
                .map(|bytes| (path.trim_start_matches('/').to_string(), bytes.into_owned()))
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    PublishedAppBundle { files }
}

fn publication_output(path: &Path, replace: bool) -> CliResult<PublicationOutput> {
    if path.is_absolute() {
        return Err("--output must be relative to the graph root".into());
    }
    let components = path.components().collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("--output must be a non-empty graph-relative directory".into());
    }
    let leaf = components
        .last()
        .and_then(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .ok_or_else(|| "--output must end in a valid directory name".to_string())?
        .to_string();
    let parent =
        components[..components.len() - 1]
            .iter()
            .fold(PathBuf::new(), |mut path, component| {
                if let Component::Normal(value) = component {
                    path.push(value);
                }
                path
            });
    Ok(PublicationOutput {
        parent,
        leaf,
        replace,
    })
}

fn doctor(path: &Path) -> CliResult<()> {
    let root = path
        .canonicalize()
        .map_err(|error| format!("cannot inspect graph {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("graph is not a directory: {}", root.display()).into());
    }
    let graph = Graph::open(&root);
    let pages = graph
        .try_list_pages()
        .map_err(|error| format!("cannot list pages of {}: {error}", root.display()))?;
    let failures = graph.page_index_failures();
    let mut identities: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for page in &pages {
        identities
            .entry(page.name.to_lowercase())
            .or_default()
            .push(page.rel_path.clone());
    }
    let duplicates = identities
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .collect::<Vec<_>>();

    terminal_stdout(format_args!("Graph: {}", root.display()));
    terminal_stdout(format_args!("Pages and journals: {}", pages.len()));
    terminal_stdout(format_args!(
        "Default home: {}",
        graph.config().default_home.as_deref().unwrap_or("(none)")
    ));
    if failures.is_empty() && duplicates.is_empty() {
        terminal_stdout(format_args!(
            "OK: graph files are readable and parseable; page identities are unique"
        ));
        return Ok(());
    }
    for failure in failures {
        terminal_stderr(format_args!("parse failure: {failure}"));
    }
    for (name, paths) in duplicates {
        terminal_stderr(format_args!(
            "duplicate page identity {name:?}: {}",
            paths.join(", ")
        ));
    }
    Err("graph checks found problems".into())
}

pub(crate) fn launch_request(argv: &[String], cwd: &Path) -> LaunchRequest {
    let Ok(cli) = Cli::try_parse_from(argv) else {
        return LaunchRequest::Focus;
    };
    let request = match cli.command {
        Some(Command::Open { graph }) => LaunchRequest::Open(graph),
        Some(Command::Capture) => LaunchRequest::Capture,
        _ if cli.capture => LaunchRequest::Capture,
        _ => cli
            .graph
            .map(LaunchRequest::Open)
            .unwrap_or(LaunchRequest::Focus),
    };
    match request {
        LaunchRequest::Open(path) if path.is_relative() => LaunchRequest::Open(cwd.join(path)),
        request => request,
    }
}

pub(crate) fn launch_request_env() -> LaunchRequest {
    let argv = std::env::args().collect::<Vec<_>>();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    launch_request(&argv, &cwd)
}

#[cfg(target_os = "windows")]
fn prepare_console_if_needed(argv: &[OsString]) {
    let headless = argv.iter().skip(1).any(|argument| {
        let argument = argument.to_string_lossy();
        let gui_flag = argument
            .strip_prefix("--")
            .is_some_and(|name| matches!(name, "debug" | "capture" | "tine-ci-copy-guide"));
        matches!(
            argument.as_ref(),
            "help" | "version" | "export" | "doctor" | "-h" | "--help" | "-v" | "-V" | "--version"
        ) || (argument.starts_with('-') && !gui_flag)
    });
    if headless {
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(
                windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn prepare_console_if_needed(_: &[OsString]) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_named_gui_launches_share_one_parser() {
        let cwd = Path::new("/graphs");
        assert_eq!(
            launch_request(&["tine".into(), "notes".into()], cwd),
            LaunchRequest::Open(PathBuf::from("/graphs/notes"))
        );
        assert_eq!(
            launch_request(&["tine".into(), "open".into(), "notes".into()], cwd),
            LaunchRequest::Open(PathBuf::from("/graphs/notes"))
        );
        assert_eq!(
            launch_request(&["tine".into(), "capture".into()], cwd),
            LaunchRequest::Capture
        );
        assert_eq!(
            launch_request(&["tine".into(), "--capture".into()], cwd),
            LaunchRequest::Capture
        );
    }

    #[test]
    fn output_is_graph_relative_and_create_only_by_default() {
        let output = publication_output(Path::new("exports/live"), false).unwrap();
        assert_eq!(output.parent, PathBuf::from("exports"));
        assert_eq!(output.leaf, "live");
        assert!(!output.replace);
        assert!(publication_output(Path::new("../outside"), false).is_err());
        assert!(publication_output(Path::new("/outside"), false).is_err());
    }

    #[test]
    fn checked_in_man_page_is_generated_from_the_cli_schema() {
        const MAN_PAGES: &[(&str, &str)] = &[
            ("tine.1", include_str!("../../docs/tine.1")),
            ("tine-open.1", include_str!("../../docs/tine-open.1")),
            ("tine-capture.1", include_str!("../../docs/tine-capture.1")),
            ("tine-export.1", include_str!("../../docs/tine-export.1")),
            (
                "tine-export-static.1",
                include_str!("../../docs/tine-export-static.1"),
            ),
            (
                "tine-export-live.1",
                include_str!("../../docs/tine-export-live.1"),
            ),
            ("tine-doctor.1", include_str!("../../docs/tine-doctor.1")),
            ("tine-version.1", include_str!("../../docs/tine-version.1")),
        ];
        if std::env::var_os("TINE_UPDATE_MAN_PAGE").is_some() {
            clap_mangen::generate_to(
                Cli::command(),
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs"),
            )
            .unwrap();
            return;
        }
        let generated = tempfile::tempdir().unwrap();
        clap_mangen::generate_to(Cli::command(), generated.path()).unwrap();
        for (name, expected) in MAN_PAGES {
            assert_eq!(
                std::fs::read_to_string(generated.path().join(name)).unwrap(),
                *expected,
                "{name} drifted from the CLI schema"
            );
        }
    }

    #[cfg(feature = "custom-protocol")]
    #[test]
    fn embedded_release_frontend_is_exportable() {
        let context: tauri::Context<tauri::Wry> = tauri::generate_context!();
        let bundle = embedded_app_bundle(&context);
        let index = std::str::from_utf8(bundle.index().expect("embedded index.html"))
            .expect("UTF-8 index.html");
        assert!(index.to_ascii_lowercase().contains("<head>"), "{index}");
    }
}
