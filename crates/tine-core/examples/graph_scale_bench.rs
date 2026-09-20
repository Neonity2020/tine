use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tine_core::{Graph, PageKind, RefGroup};

const DEFAULT_SCALES: &[usize] = &[2_000, 10_000, 20_000];
const BLOCKS_PER_PAGE: usize = 5;
const JOURNAL_COUNT: usize = 30;
const FIND_ENTRY_LOOKUPS: usize = 500;
const FIND_ENTRY_RUNS: usize = 3;
const COLD_RUNS: usize = 3;
const CACHE_BUILD_RUNS: usize = 3;
const SWITCHER_RUNS: usize = FIND_ENTRY_LOOKUPS;
const WARM_QUERY_RUNS: usize = 5;
const PUBLISH_RUNS: usize = 2;

const PRIMARY_QUERY: &str = "(task TODO)";
const COMPOUND_QUERY: &str = "(and (task TODO DOING) #SomeTag)";

const WORDS: &[&str] = &[
    "alpha", "archive", "board", "budget", "cache", "canvas", "cluster", "column", "context",
    "delta", "design", "draft", "entry", "event", "field", "filter", "focus", "graph", "grid",
    "index", "journal", "layout", "link", "marker", "memo", "metric", "note", "outline", "page",
    "panel", "phase", "priority", "project", "query", "record", "ref", "review", "row", "scan",
    "sheet", "signal", "source", "status", "sync", "tag", "task", "thread", "value", "view",
    "workflow",
];

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Some(at) = args.iter().position(|arg| arg == "--root") {
        let root = args.get(at + 1).map(PathBuf::from).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "--root needs a graph path")
        })?;
        let flag = |name: &str| {
            args.iter()
                .position(|arg| arg == name)
                .and_then(|at| args.get(at + 1))
                .cloned()
        };
        let corpus = flag("--corpus").unwrap_or_else(|| "anon".to_owned());
        let json = flag("--json").map(PathBuf::from);
        return measure::run(&root, &corpus, json.as_deref());
    }
    let scales = parse_scales()?;
    println!("Graph scale benchmark");
    println!("primary_query={PRIMARY_QUERY}");
    println!("compound_query={COMPOUND_QUERY}");
    println!(
        "runs=cold:{COLD_RUNS} cache_build:{CACHE_BUILD_RUNS} find_entry:{FIND_ENTRY_RUNS} switcher:{SWITCHER_RUNS} warm_query:{WARM_QUERY_RUNS} publish:{PUBLISH_RUNS}"
    );
    println!();

    let mut rows = Vec::new();
    for scale in scales {
        let root = graph_path(scale);
        let generated = generate_graph(&root, scale)?;
        println!(
            "generated scale={} path={} pages={} nested={} journals={} blocks={}",
            scale,
            root.display(),
            generated.pages,
            generated.nested,
            generated.journals,
            generated.blocks
        );
        let row = bench_scale(scale, &root, generated)?;
        println!(
            "result scale={} cold_open_ms={:.3} cache_build_ms={:.3} find_entry_total_ms={:.3} find_entry_us={:.3} switcher_ms={:.3} warm_query_ms={:.3} publish_ms={:.3} publish_pages={}",
            row.scale,
            row.cold_open_ms,
            row.cache_build_ms,
            row.find_entry_total_ms,
            row.find_entry_per_us,
            row.switcher_ms,
            row.warm_query_ms,
            row.publish_ms,
            row.publish_pages
        );
        println!();
        rows.push(row);
    }

    print_table(&rows);
    Ok(())
}

fn parse_scales() -> io::Result<Vec<usize>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        return Ok(DEFAULT_SCALES.to_vec());
    }
    let mut scales = Vec::with_capacity(args.len());
    for arg in args {
        let cleaned = arg.replace('_', "");
        let scale = cleaned.parse::<usize>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid scale argument: {arg}"),
            )
        })?;
        if scale == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scale must be greater than zero",
            ));
        }
        scales.push(scale);
    }
    Ok(scales)
}

fn graph_path(scale: usize) -> PathBuf {
    PathBuf::from(format!("/tmp/graph-scale-bench-{scale}"))
}

#[derive(Clone)]
struct GeneratedGraph {
    blocks: usize,
    pages: usize,
    nested: usize,
    journals: usize,
    page_names: Vec<String>,
}

fn generate_graph(root: &Path, page_count: usize) -> io::Result<GeneratedGraph> {
    if root.exists() {
        fs::remove_dir_all(root)?;
    }
    fs::create_dir_all(root.join("pages"))?;
    fs::create_dir_all(root.join("journals"))?;
    fs::create_dir_all(root.join("logseq"))?;
    fs::write(
        root.join("logseq").join("config.edn"),
        "{:publishing/all-pages-public? true}\n",
    )?;

    let page_names: Vec<String> = (0..page_count).map(page_name).collect();
    let mut rng = Lcg::new(0x5eed_cafe_d00d_f00d ^ page_count as u64);
    let mut blocks = 0usize;
    let mut nested = 0usize;

    for (idx, name) in page_names.iter().enumerate() {
        let is_nested = is_nested_page(idx);
        if is_nested {
            nested += 1;
        }
        let path = page_path(root, name, idx, is_nested);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);

        if idx % 7 == 0 {
            writeln!(writer, "benchmark:: graph-scale")?;
            writeln!(writer, "tags:: Bench, SomeTag")?;
            writeln!(writer)?;
        }

        for local_idx in 0..BLOCKS_PER_PAGE {
            let global_idx = idx * BLOCKS_PER_PAGE + local_idx;
            let raw = block_raw(global_idx, local_idx, idx, &page_names, &mut rng);
            write_block(&mut writer, depth_for(local_idx), &raw)?;
            blocks += 1;
        }
    }

    for journal_idx in 0..JOURNAL_COUNT {
        let path = root
            .join("journals")
            .join(format!("2026_01_{:02}.md", journal_idx + 1));
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "benchmark:: graph-scale")?;
        if journal_idx % 3 == 0 {
            writeln!(writer, "tags:: SomeTag")?;
        }
        writeln!(writer)?;
        for local_idx in 0..BLOCKS_PER_PAGE {
            let global_idx =
                page_count * BLOCKS_PER_PAGE + journal_idx * BLOCKS_PER_PAGE + local_idx;
            let raw = block_raw(
                global_idx,
                local_idx,
                page_count + journal_idx,
                &page_names,
                &mut rng,
            );
            write_block(&mut writer, depth_for(local_idx), &raw)?;
            blocks += 1;
        }
    }

    write_dashboard(root, &page_names)?;
    blocks += 60;

    Ok(GeneratedGraph {
        blocks,
        pages: page_count + 1,
        nested,
        journals: JOURNAL_COUNT,
        page_names,
    })
}

fn page_name(i: usize) -> String {
    format!("Page {i:06}")
}

fn is_nested_page(i: usize) -> bool {
    i % 20 < 3
}

fn page_path(root: &Path, name: &str, idx: usize, nested: bool) -> PathBuf {
    if nested {
        root.join("pages")
            .join(format!("ns-{}", idx % 16))
            .join(format!("{name}.md"))
    } else {
        root.join("pages").join(format!("{name}.md"))
    }
}

fn write_dashboard(root: &Path, page_names: &[String]) -> io::Result<()> {
    let file = File::create(root.join("pages").join("Dashboard.md"))?;
    let mut writer = BufWriter::new(file);
    writeln!(writer, "benchmark:: graph-scale")?;
    writeln!(writer, "tags:: Bench, SomeTag")?;
    writeln!(writer)?;
    for _ in 0..20 {
        write_block(&mut writer, 0, &format!("{{{{query {PRIMARY_QUERY}}}}}"))?;
    }
    for _ in 0..20 {
        write_block(&mut writer, 0, &format!("{{{{query {COMPOUND_QUERY}}}}}"))?;
    }
    for i in 0..20 {
        let name = &page_names[(i * 37) % page_names.len()];
        write_block(&mut writer, 0, &format!("{{{{embed [[{name}]]}}}}"))?;
    }
    Ok(())
}

fn depth_for(local_idx: usize) -> usize {
    match local_idx {
        0 | 3 => 0,
        1 | 4 => 1,
        _ => 2,
    }
}

fn block_raw(
    global_idx: usize,
    local_idx: usize,
    file_idx: usize,
    page_names: &[String],
    rng: &mut Lcg,
) -> String {
    match local_idx {
        0 | 3 => task_raw(global_idx, local_idx, file_idx, rng),
        1 | 2 => ref_raw(global_idx, page_names, rng),
        _ => property_raw(global_idx, rng),
    }
}

fn task_raw(global_idx: usize, local_idx: usize, file_idx: usize, rng: &mut Lcg) -> String {
    let marker = match (global_idx / 5) % 4 {
        0 => "TODO",
        1 => "DOING",
        2 => "DONE",
        _ => "LATER",
    };

    let mut raw = String::new();
    raw.push_str(marker);
    raw.push(' ');
    if global_idx % 15 == 0 {
        raw.push_str("[#A] ");
    } else if global_idx % 35 == 0 {
        raw.push_str("[#B] ");
    }
    raw.push_str(&prose_raw(rng, 5, 14));
    if local_idx == 0 || file_idx % 6 == 0 {
        raw.push_str(" #SomeTag");
    } else if global_idx % 3 == 0 {
        raw.push_str(" #task-tag");
    }
    if global_idx % 4 == 0 {
        raw.push('\n');
        raw.push_str(&format!(
            "SCHEDULED: <2026-{:02}-{:02} Mon>",
            (global_idx % 12) + 1,
            (global_idx % 28) + 1
        ));
    }
    if global_idx % 17 == 0 {
        raw.push('\n');
        raw.push_str(&format!("owner:: team-{}", global_idx % 9));
    }
    raw
}

fn ref_raw(global_idx: usize, page_names: &[String], rng: &mut Lcg) -> String {
    let target = &page_names[(global_idx + rng.range(page_names.len())) % page_names.len()];
    let mut raw = prose_raw(rng, 5, 16);
    raw.push(' ');
    if global_idx % 2 == 0 {
        raw.push_str("#SomeTag");
    } else {
        raw.push_str(&format!("#topic-{}", global_idx % 23));
    }
    raw.push(' ');
    raw.push_str(&format!("[[{target}]]"));
    raw
}

fn property_raw(global_idx: usize, rng: &mut Lcg) -> String {
    format!(
        "{}\nmetric:: {}\nowner:: team-{}",
        prose_raw(rng, 5, 12),
        global_idx % 101,
        global_idx % 9
    )
}

fn prose_raw(rng: &mut Lcg, min_words: usize, max_words: usize) -> String {
    let len = min_words + rng.range(max_words - min_words + 1);
    let mut out = String::new();
    for i in 0..len {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(WORDS[rng.range(WORDS.len())]);
    }
    out
}

fn write_block(writer: &mut impl Write, depth: usize, raw: &str) -> io::Result<()> {
    let indent = "  ".repeat(depth);
    for (line_idx, line) in raw.lines().enumerate() {
        if line_idx == 0 {
            writeln!(writer, "{indent}- {line}")?;
        } else {
            writeln!(writer, "{indent}  {line}")?;
        }
    }
    Ok(())
}

struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn range(&mut self, upper: usize) -> usize {
        debug_assert!(upper > 0);
        (self.next() as usize) % upper
    }
}

struct BenchRow {
    scale: usize,
    pages: usize,
    nested: usize,
    journals: usize,
    cold_open_ms: f64,
    cache_build_ms: f64,
    find_entry_total_ms: f64,
    find_entry_per_us: f64,
    switcher_ms: f64,
    warm_query_ms: f64,
    publish_ms: f64,
    publish_pages: usize,
}

fn bench_scale(scale: usize, root: &Path, generated: GeneratedGraph) -> io::Result<BenchRow> {
    let mut cold_open = Vec::with_capacity(COLD_RUNS);
    for _ in 0..COLD_RUNS {
        let started = Instant::now();
        let graph = Graph::open(root);
        let page_count = graph.with_pages(|pages| pages.len());
        cold_open.push(started.elapsed());
        assert_eq!(page_count, generated.pages + generated.journals);
        black_box(page_count);
    }

    let mut cache_build = Vec::with_capacity(CACHE_BUILD_RUNS);
    for _ in 0..CACHE_BUILD_RUNS {
        let graph = Graph::open(root);
        let started = Instant::now();
        let page_count = graph.with_pages(|pages| pages.len());
        cache_build.push(started.elapsed());
        assert_eq!(page_count, generated.pages + generated.journals);
        black_box(page_count);
    }

    let lookup_names = lookup_sample(&generated.page_names, FIND_ENTRY_LOOKUPS);
    let (find_entry_total_ms, find_entry_per_us) = bench_find_entry(root, &lookup_names)?;
    let switcher_ms = bench_switcher(root)?;
    let warm_query_ms = bench_warm_query(root)?;
    let (publish_ms, publish_pages) = bench_publish(root)?;

    Ok(BenchRow {
        scale,
        pages: generated.pages,
        nested: generated.nested,
        journals: generated.journals,
        cold_open_ms: ms(median(&cold_open)),
        cache_build_ms: ms(median(&cache_build)),
        find_entry_total_ms,
        find_entry_per_us,
        switcher_ms,
        warm_query_ms,
        publish_ms,
        publish_pages,
    })
}

fn lookup_sample(names: &[String], requested: usize) -> Vec<String> {
    let limit = requested.min(names.len());
    let mut sample = Vec::with_capacity(limit);
    let mut seen = HashSet::with_capacity(limit);
    let mut i = 0usize;
    while sample.len() < limit {
        let idx = (i * 37) % names.len();
        if seen.insert(idx) {
            sample.push(names[idx].clone());
        }
        i += 1;
    }
    assert!(sample.iter().any(|name| {
        name.strip_prefix("Page ")
            .and_then(|s| s.parse::<usize>().ok())
            .is_some_and(is_nested_page)
    }));
    sample
}

fn bench_find_entry(root: &Path, names: &[String]) -> io::Result<(f64, f64)> {
    let mut totals = Vec::with_capacity(FIND_ENTRY_RUNS);
    let mut per_lookup = Vec::with_capacity(FIND_ENTRY_RUNS * names.len());
    for _ in 0..FIND_ENTRY_RUNS {
        let graph = Graph::open(root);
        let page_count = graph.with_pages(|pages| pages.len());
        black_box(page_count);
        let started = Instant::now();
        for name in names {
            let call_started = Instant::now();
            let page = graph
                .load_named(name, PageKind::Page)?
                .unwrap_or_else(|| panic!("generated page not found: {name}"));
            per_lookup.push(call_started.elapsed());
            black_box(page.blocks.len());
        }
        totals.push(started.elapsed());
    }
    Ok((ms(median(&totals)), us(median(&per_lookup))))
}

fn bench_switcher(root: &Path) -> io::Result<f64> {
    let graph = Graph::open(root);
    let page_count = graph.with_pages(|pages| pages.len());
    black_box(page_count);
    let mut durations = Vec::with_capacity(SWITCHER_RUNS);
    for _ in 0..SWITCHER_RUNS {
        let started = Instant::now();
        let results = graph.quick_switch("pa", 12);
        durations.push(started.elapsed());
        assert!(!results.is_empty(), "quick_switch returned no results");
        black_box(results.len());
    }
    Ok(ms(median(&durations)))
}

fn bench_warm_query(root: &Path) -> io::Result<f64> {
    let projection_dir = tempfile::tempdir()?;
    let graph = Graph::open(root);
    graph.attach_direct_projection(projection_dir.path().join("direct.sqlite"))?;
    graph.warm_cache();
    let page_count = graph.with_pages(|pages| pages.len());
    black_box(page_count);
    let mut durations = Vec::with_capacity(WARM_QUERY_RUNS);
    for i in 0..WARM_QUERY_RUNS {
        let query = primary_query_variant(i);
        let started = Instant::now();
        let groups = graph.run_query(&query).map_err(io::Error::other)?;
        durations.push(started.elapsed());
        assert_nonzero(result_count(groups.as_ref()), &query);
        black_box(groups.len());
    }
    Ok(ms(median(&durations)))
}

fn bench_publish(root: &Path) -> io::Result<(f64, usize)> {
    let mut durations = Vec::with_capacity(PUBLISH_RUNS);
    let mut publish_pages = 0usize;
    // Graph initialization is outside the publication timing. Each reopen uses
    // the same ordinary disposable projection, including its warm validation.
    let projection_dir = tempfile::tempdir()?;
    for _ in 0..PUBLISH_RUNS {
        let graph = Graph::open(root);
        graph.attach_direct_projection(projection_dir.path().join("direct.sqlite"))?;
        graph.warm_cache();
        let page_count = graph.with_pages(|pages| pages.len());
        black_box(page_count);
        let started = Instant::now();
        let (out, count) = tine_core::publish::publish_graph(&graph)?;
        durations.push(started.elapsed());
        assert!(count > 0, "publish_graph returned no pages");
        publish_pages = count;
        black_box(out.len());
        black_box(count);
    }
    Ok((ms(median(&durations)), publish_pages))
}

fn primary_query_variant(i: usize) -> String {
    if i == 0 {
        PRIMARY_QUERY.to_string()
    } else {
        format!("(task TODO{})", " ".repeat(i))
    }
}

fn result_count(groups: &[RefGroup]) -> usize {
    groups.iter().map(|group| group.blocks.len()).sum()
}

fn assert_nonzero(count: usize, query: &str) {
    assert!(count > 0, "query returned no results: {query}");
}

fn median(durations: &[Duration]) -> Duration {
    assert!(!durations.is_empty());
    let mut sorted = durations.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        duration_from_nanos((sorted[mid - 1].as_nanos() + sorted[mid].as_nanos()) / 2)
    }
}

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::from_nanos(nanos.min(u64::MAX as u128) as u64)
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn us(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn print_table(rows: &[BenchRow]) {
    println!("| scale | pages | nested | journals | cold_open_ms | cache_build_ms | find_entry_K total_ms/per_us | switcher_ms | warm_query_ms | publish_ms | publish_pages |");
    println!("| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for row in rows {
        println!(
            "| {} | {} | {} | {} | {:.3} | {:.3} | {:.3}/{:.3} | {:.3} | {:.3} | {:.3} | {} |",
            row.scale,
            row.pages,
            row.nested,
            row.journals,
            row.cold_open_ms,
            row.cache_build_ms,
            row.find_entry_total_ms,
            row.find_entry_per_us,
            row.switcher_ms,
            row.warm_query_ms,
            row.publish_ms,
            row.publish_pages
        );
    }
}

/// Real-graph measurement mode (compact-projection campaign P0a): copies the
/// graph at `--root` into a scratch directory, builds the Direct Files
/// projection there, and reports the §3 budget instruments as JSON:
/// S1 (projection bytes / Markdown bytes after a TRUNCATE checkpoint),
/// S2 (bytes handed to `write()` during the build / final projection bytes),
/// T1 (build wall time), M1 (peak RSS delta), U1 (bytes written per
/// single-block edit on a 1-block and a 60-block page), T2 (Ctrl+K p95
/// through `Graph::run_graph_search_latest`) and T3 (`{{query}}` p95 over a
/// fixed query set). Linux-only: it reads `/proc/self/io` and
/// `/proc/self/status`, which are process-wide, so this process does nothing
/// but the measured work while measuring.
mod measure {
    use super::*;
    use std::collections::HashMap;

    const SEARCH_RUNS: usize = 20;
    const QUERY_RUNS: usize = 20;
    const EDIT_RUNS: usize = 10;
    const SIXTY: usize = 60;
    /// One text per operator family the query census counts, over facts every
    /// graph has (tasks, dates, page tags); property and reference names are
    /// corpus-specific and are not fixed here.
    const QUERIES: &[&str] = &[
        "(task TODO)",
        "(task DONE)",
        "(between -3650d today)",
        "(all-page-tags)",
        "(and (task TODO) (between -3650d today))",
    ];

    #[derive(Clone, Copy, Default)]
    struct Io {
        wchar: u64,
        write_bytes: u64,
        cancelled_write_bytes: u64,
    }

    fn io() -> Io {
        let text = fs::read_to_string("/proc/self/io").expect("Linux /proc/self/io");
        let mut out = Io::default();
        for line in text.lines() {
            let mut parts = line.split(':');
            let key = parts.next().unwrap_or("").trim();
            let value = parts
                .next()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(0);
            match key {
                "wchar" => out.wchar = value,
                "write_bytes" => out.write_bytes = value,
                "cancelled_write_bytes" => out.cancelled_write_bytes = value,
                _ => {}
            }
        }
        out
    }

    fn status_kb(key: &str) -> u64 {
        let text = fs::read_to_string("/proc/self/status").expect("Linux /proc/self/status");
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| {
                rest.trim()
                    .trim_start_matches(':')
                    .trim()
                    .split_whitespace()
                    .next()
            })
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    }

    fn file_len(path: &Path) -> u64 {
        fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
    }

    /// Copy the graph, skipping version control and Logseq's own backup dirs.
    /// Returns (files copied, Markdown/Org bytes under pages/ and journals/).
    fn copy_graph(from: &Path, to: &Path) -> io::Result<(usize, u64)> {
        fn walk(
            from: &Path,
            to: &Path,
            rel: &Path,
            files: &mut usize,
            bytes: &mut u64,
        ) -> io::Result<()> {
            fs::create_dir_all(to.join(rel))?;
            for entry in fs::read_dir(from.join(rel))? {
                let entry = entry?;
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let child = rel.join(&name);
                if entry.file_type()?.is_dir() {
                    if name_str == ".git" || name_str == "bak" || name_str == ".recycle" {
                        continue;
                    }
                    walk(from, to, &child, files, bytes)?;
                } else {
                    fs::copy(from.join(&child), to.join(&child))?;
                    *files += 1;
                    let top = child
                        .components()
                        .next()
                        .map(|c| c.as_os_str().to_string_lossy().to_string());
                    let is_text = child
                        .extension()
                        .is_some_and(|ext| ext == "md" || ext == "org");
                    if is_text && matches!(top.as_deref(), Some("pages") | Some("journals")) {
                        *bytes += entry.metadata()?.len();
                    }
                }
            }
            Ok(())
        }
        let mut files = 0;
        let mut bytes = 0;
        walk(from, to, Path::new(""), &mut files, &mut bytes)?;
        Ok((files, bytes))
    }

    /// The most frequent alphabetic tokens of the corpus text: the top token
    /// of at least 4 chars and the top token of at least 8 chars, so the
    /// Ctrl+K needles are representative of this graph and not of a word
    /// list; the 3-char needle is the prefix of the first.
    fn top_tokens(root: &Path) -> Vec<String> {
        let mut freq: HashMap<String, usize> = HashMap::new();
        for dir in ["pages", "journals"] {
            let Ok(entries) = fs::read_dir(root.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(text) = fs::read_to_string(entry.path()) else {
                    continue;
                };
                for token in text
                    .split(|c: char| !c.is_alphabetic())
                    .filter(|token| token.chars().count() >= 4)
                {
                    *freq.entry(token.to_lowercase()).or_insert(0) += 1;
                }
            }
        }
        let mut ranked: Vec<(String, usize)> = freq.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut tokens = Vec::new();
        if let Some((top, _)) = ranked.first() {
            tokens.push(top.clone());
        }
        if let Some((long, _)) = ranked.iter().find(|(token, _)| token.chars().count() >= 8) {
            if !tokens.contains(long) {
                tokens.push(long.clone());
            }
        }
        tokens
    }

    fn p95(durations: &mut [Duration]) -> f64 {
        durations.sort();
        let at = ((durations.len() as f64) * 0.95).ceil() as usize;
        ms(durations[at.saturating_sub(1).min(durations.len() - 1)])
    }

    /// Wait until the projection has no build in flight, `needle` is
    /// searchable (so a queued live delta has been applied), and the process
    /// has stopped writing for 300 ms (a checkpoint after the delta counts).
    fn settle(graph: &Graph, needle: &str) -> io::Result<()> {
        let started = Instant::now();
        loop {
            if started.elapsed() > Duration::from_secs(60) {
                return Err(io::Error::other(format!(
                    "projection did not settle for {needle}"
                )));
            }
            if graph.query_registry_snapshot_ready().is_err() {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let found = graph
                .run_graph_search_latest_displayed_for(
                    "measure",
                    needle,
                    4,
                    4,
                    None,
                    false,
                    Default::default(),
                    tine_core::query_plan::FriendlyConsumer::CtrlK,
                )
                .map(|execution| !execution.hits.is_empty())
                .unwrap_or(false);
            if !found {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let mut last = io().wchar;
            let mut stable = 0;
            while stable < 3 {
                std::thread::sleep(Duration::from_millis(100));
                let now = io().wchar;
                if now == last {
                    stable += 1;
                } else {
                    stable = 0;
                    last = now;
                }
            }
            return Ok(());
        }
    }

    fn bench_page(name: &str, blocks: usize, token: &str) -> tine_core::model::PageDto {
        tine_core::model::PageDto {
            name: name.to_owned(),
            kind: PageKind::Page,
            title: name.to_owned(),
            pre_block: None,
            blocks: (0..blocks)
                .map(|at| tine_core::model::BlockDto {
                    raw: format!("{token} block {at} of {name}"),
                    ..Default::default()
                })
                .collect(),
            rev: None,
            format: Default::default(),
            read_only: false,
            path: String::new(),
            activation: None,
            guide: false,
        }
    }

    /// U1 for one page: median bytes written by a single-block edit.
    /// TRUNCATE-checkpoint the projection from a second connection so the WAL
    /// is empty at the start of a measured series and fully accounted at its
    /// end; without the bracket, whether SQLite's autocheckpoint lands inside
    /// the series moves a 10-edit mean by 3x between identical runs.
    fn checkpoint(projection: &Path) -> io::Result<()> {
        let database =
            tine_storage::sqlite::PhysicalGraphProjectionDatabase::open_writable(projection)
                .map_err(|error| io::Error::other(error.to_string()))?;
        database
            .checkpoint_truncate()
            .map_err(|error| io::Error::other(error.to_string()))
    }

    /// Bytes written per single-block edit of `name`: `EDIT_RUNS` edits, each
    /// settled (projection idle, the new text searchable), bracketed by two
    /// checkpoints, divided by the count — the amortized cost a user pays per
    /// edit, WAL append and checkpoint share included.
    fn edit_cost(graph: &Graph, projection: &Path, name: &str) -> io::Result<(u64, u64, f64)> {
        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| io::Error::other(format!("bench page {name} missing")))?;
        checkpoint(projection)?;
        let before = io();
        let mut elapsed = Vec::new();
        for round in 0..EDIT_RUNS {
            let mut page = graph.load_page(&entry)?;
            let baseline = page.rev.clone();
            let token = format!("benchedit{round}x{}", name.len());
            page.blocks[0].raw = format!("{token} edited block of {name}");
            let started = Instant::now();
            let round_before = io().wchar;
            graph.save_page(&page, baseline.as_deref())?;
            settle(graph, &token)?;
            elapsed.push(started.elapsed());
            if std::env::var_os("TINE_MEASURE_TRACE").is_some() {
                eprintln!(
                    "measure: edit {round} of {name}: {} B",
                    io().wchar - round_before
                );
            }
        }
        let series = io().wchar;
        checkpoint(projection)?;
        let after = io();
        if std::env::var_os("TINE_MEASURE_TRACE").is_some() {
            eprintln!(
                "measure: final checkpoint of {name}: {} B",
                after.wchar - series
            );
        }
        let runs = EDIT_RUNS as u64;
        Ok((
            (after.wchar - before.wchar) / runs,
            (after.write_bytes - before.write_bytes) / runs,
            ms(median(&elapsed)),
        ))
    }

    pub fn run(root: &Path, corpus: &str, json: Option<&Path>) -> io::Result<()> {
        let scratch = tempfile::tempdir()?;
        let graph_root = scratch.path().join("graph");
        let (files, markdown_bytes) = copy_graph(root, &graph_root)?;
        let projection = scratch.path().join("projection.sqlite");
        eprintln!("measure: {files} files, {markdown_bytes} Markdown/Org bytes, corpus={corpus}");

        // T1, S2, M1: the build, with nothing else running in this process.
        let rss_before_kb = status_kb("VmRSS");
        let io_before = io();
        let started = Instant::now();
        let graph = Graph::open(&graph_root);
        graph.attach_direct_projection(projection.clone())?;
        graph.warm_cache();
        // Registry readiness is projection-only; Ctrl-K may legitimately answer
        // from the parsed cache before the complete projection is published.
        while graph.query_registry_snapshot_ready().is_err() {
            if started.elapsed() > Duration::from_secs(900) {
                return Err(io::Error::other(
                    "projection build did not become ready within 900 seconds",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let build_ms = ms(started.elapsed());
        let io_build = io();
        let hwm_after_kb = status_kb("VmHWM");
        let pages = graph.with_pages(|pages| pages.len());
        let projection_bytes_after_build =
            file_len(&projection) + file_len(&projection.with_extension("sqlite-wal"));
        eprintln!("measure: built in {build_ms:.0} ms, {pages} pages, file+wal {projection_bytes_after_build} bytes");

        // T2: Ctrl+K needles from this corpus.
        let tokens = top_tokens(&graph_root);
        let mut needles = Vec::new();
        if let Some(top) = tokens.first() {
            needles.push(top.chars().take(3).collect::<String>());
        }
        needles.extend(tokens.iter().cloned());
        let mut search = Vec::new();
        for needle in &needles {
            let mut durations = Vec::with_capacity(SEARCH_RUNS);
            let mut hits = 0usize;
            for _ in 0..SEARCH_RUNS {
                let started = Instant::now();
                let execution = graph
                    .run_graph_search_latest_displayed_for(
                        "measure",
                        needle,
                        12,
                        12,
                        None,
                        false,
                        Default::default(),
                        tine_core::query_plan::FriendlyConsumer::CtrlK,
                    )
                    .map_err(|error| io::Error::other(format!("search {needle}: {error:?}")))?;
                durations.push(started.elapsed());
                hits = execution.hits.len();
            }
            search.push(serde_json::json!({
                "needle": needle,
                "chars": needle.chars().count(),
                "p95_ms": p95(&mut durations),
                "median_ms": ms(median(&durations)),
                "hits": hits,
            }));
        }

        // T3: the fixed query set.
        let mut queries = Vec::new();
        for query in QUERIES {
            let mut durations = Vec::with_capacity(QUERY_RUNS);
            let mut groups_len = 0usize;
            for _ in 0..QUERY_RUNS {
                let started = Instant::now();
                let groups = graph
                    .run_query(query)
                    .map_err(|error| io::Error::other(format!("query {query}: {error:?}")))?;
                durations.push(started.elapsed());
                groups_len = groups.len();
            }
            queries.push(serde_json::json!({
                "query": query,
                "p95_ms": p95(&mut durations),
                "median_ms": ms(median(&durations)),
                "groups": groups_len,
            }));
        }

        // U1: bytes written per single-block edit, 1-block and 60-block pages.
        graph.save_page(&bench_page("Bench One Block", 1, "benchseedone"), None)?;
        graph.save_page(
            &bench_page("Bench Sixty Blocks", SIXTY, "benchseedsixty"),
            None,
        )?;
        settle(&graph, "benchseedone")?;
        settle(&graph, "benchseedsixty")?;
        let (one_wchar, one_write_bytes, one_ms) =
            edit_cost(&graph, &projection, "Bench One Block")?;
        let (sixty_wchar, sixty_write_bytes, sixty_ms) =
            edit_cost(&graph, &projection, "Bench Sixty Blocks")?;

        // S1: file bytes after detach, TRUNCATE checkpoint and close.
        if !graph.detach_direct_projection(Duration::from_secs(60)) {
            return Err(io::Error::other("projection did not detach"));
        }
        drop(graph);
        let block_count = {
            let database =
                tine_storage::sqlite::PhysicalGraphProjectionDatabase::open_writable(&projection)
                    .map_err(|error| io::Error::other(error.to_string()))?;
            database
                .checkpoint_truncate()
                .map_err(|error| io::Error::other(error.to_string()))?;
            drop(database);
            let mut snapshot = tine_storage::sqlite::PhysicalProjectionQuerySnapshot::open_direct(
                &projection,
                || Ok(()),
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
            let rows = snapshot
                .run_projection_query("SELECT COUNT(*) FROM blocks", &[])
                .map_err(|error| io::Error::other(error.to_string()))?;
            match rows.first().and_then(|row| row.first()) {
                Some(tine_storage::sqlite::PhysicalQueryValue::Integer(count)) => *count as u64,
                _ => 0,
            }
        };
        let projection_bytes = file_len(&projection);
        let wal_bytes = file_len(&projection.with_extension("sqlite-wal"));
        let build_wchar = io_build.wchar - io_before.wchar;
        let build_write_bytes = (io_build.write_bytes - io_before.write_bytes)
            .saturating_sub(io_build.cancelled_write_bytes - io_before.cancelled_write_bytes);

        let report = serde_json::json!({
            "schemaVersion": 1,
            "corpus": corpus,
            "root": root.display().to_string(),
            "files": files,
            "pages": pages,
            "blocks": block_count,
            "markdown_bytes": markdown_bytes,
            "projection_bytes": projection_bytes,
            "wal_bytes_after_checkpoint": wal_bytes,
            "projection_bytes_after_build_with_wal": projection_bytes_after_build,
            "s1_ratio": projection_bytes as f64 / markdown_bytes.max(1) as f64,
            "build_wchar": build_wchar,
            "build_write_bytes_net": build_write_bytes,
            "s2_write_ratio": build_wchar as f64 / projection_bytes.max(1) as f64,
            "t1_build_ms": build_ms,
            "m1_peak_rss_delta_kb": hwm_after_kb.saturating_sub(rss_before_kb),
            "vm_hwm_kb": hwm_after_kb,
            "u1": {
                "one_block": { "wchar": one_wchar, "write_bytes": one_write_bytes, "settle_ms": one_ms },
                "sixty_block": { "wchar": sixty_wchar, "write_bytes": sixty_write_bytes, "settle_ms": sixty_ms },
                "edits_per_page": EDIT_RUNS,
            },
            "t2_search": search,
            "t3_queries": queries,
        });
        let text = serde_json::to_string_pretty(&report)?;
        match json {
            Some(path) => fs::write(path, format!("{text}\n"))?,
            None => println!("{text}"),
        }
        eprintln!(
            "measure: S1 {:.2}x  S2 {:.2}x  T1 {:.0} ms  M1 {} kB  U1 {}/{} B",
            report["s1_ratio"].as_f64().unwrap_or(0.0),
            report["s2_write_ratio"].as_f64().unwrap_or(0.0),
            build_ms,
            report["m1_peak_rss_delta_kb"],
            one_wchar,
            sixty_wchar
        );
        Ok(())
    }
}
