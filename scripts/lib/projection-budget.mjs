// Compact-projection budget (tine-agents/specs/campaigns/2026-09-compact-projection/SPEC.md §3).
// Pure evaluation of one measurement (the JSON `graph_scale_bench --root … --json`
// writes) against `scripts/projection-budget-policy.json`. S1 and S2 are absolute
// ceilings with no band. The relative rows (T1, T2, T3, M1, U1) compare against
// the recorded baseline times a multiplier, and a value within the noise band of
// that target counts as meeting it — U1 included, because its checkpoint share
// still moves a bracketed 10-edit mean by ~10% between identical runs.

export function evaluateBudget(measurement, policy) {
  const corpus = policy.corpora[measurement.corpus];
  if (!corpus) {
    throw new Error(`projection budget policy has no corpus "${measurement.corpus}"`);
  }
  const { ceilings, baseline } = corpus;
  const band = policy.noiseBandFraction ?? 0;
  const rows = [];
  const row = (id, label, value, ceiling, unit, kind) => {
    const limit = ceiling == null ? null : kind === "relative" ? ceiling * (1 + band) : ceiling;
    const ok = limit == null ? null : value <= limit;
    rows.push({ id, label, value, ceiling, unit, ok });
  };
  row("S1", "projection bytes / Markdown bytes", measurement.s1_ratio, ceilings.s1, "x", "absolute");
  row("S2", "bytes written by the build / final file", measurement.s2_write_ratio, ceilings.s2, "x", "absolute");
  row(
    "T1",
    "full build wall time",
    measurement.t1_build_ms,
    baseline ? baseline.t1_build_ms * ceilings.t1_multiplier : null,
    "ms",
    "relative",
  );
  row(
    "M1",
    "peak RSS delta during the build",
    measurement.m1_peak_rss_delta_kb,
    baseline ? baseline.m1_peak_rss_delta_kb * ceilings.m1_multiplier : null,
    "kB",
    "relative",
  );
  for (const page of ["one_block", "sixty_block"]) {
    row(
      `U1/${page}`,
      `bytes written per single-block edit, ${page.replace("_", "-")} page`,
      measurement.u1[page].wchar,
      baseline ? baseline.u1[page].wchar * ceilings.u1_multiplier : null,
      "B",
      "relative",
    );
  }
  for (const search of measurement.t2_search) {
    const base = baseline?.t2_search?.find((entry) => entry.chars === search.chars);
    const relative = base ? base.p95_ms * ceilings.t2_multiplier : null;
    const absolute = ceilings.t2_absolute_ms ?? null;
    const ceiling = [relative, absolute].filter((x) => x != null).reduce((a, b) => Math.min(a, b), Infinity);
    row(
      `T2/${search.chars}ch`,
      `Ctrl+K p95, ${search.chars}-char needle`,
      search.p95_ms,
      Number.isFinite(ceiling) ? ceiling : null,
      "ms",
      "relative",
    );
  }
  for (const query of measurement.t3_queries) {
    const base = baseline?.t3_queries?.find((entry) => entry.query === query.query);
    row(
      `T3/${query.query}`,
      "{{query}} p95",
      query.p95_ms,
      base ? base.p95_ms * ceilings.t3_multiplier : null,
      "ms",
      "relative",
    );
  }
  const breaches = rows.filter((entry) => entry.ok === false);
  return { rows, breaches };
}

/// The baseline record the policy keeps for a corpus: today's numbers for the
/// rows whose ceilings are relative (T1, T2, T3, M1, U1).
export function baselineFrom(measurement) {
  return {
    recorded: new Date().toISOString().slice(0, 10),
    t1_build_ms: measurement.t1_build_ms,
    m1_peak_rss_delta_kb: measurement.m1_peak_rss_delta_kb,
    u1: {
      one_block: { wchar: measurement.u1.one_block.wchar },
      sixty_block: { wchar: measurement.u1.sixty_block.wchar },
    },
    t2_search: measurement.t2_search.map(({ chars, p95_ms }) => ({ chars, p95_ms })),
    t3_queries: measurement.t3_queries.map(({ query, p95_ms }) => ({ query, p95_ms })),
    s1_ratio: measurement.s1_ratio,
    s2_write_ratio: measurement.s2_write_ratio,
  };
}

const SEARCH_REPORT_SCHEMA = "tine.search_scaling.v1";
const RAW_SEARCH_REPORT_SCHEMA = "tine.s7_page_search_probe.v1";
const SEARCH_ROW_IDS = ["T2-nohit", "T2-broad", "T2-sparse", "T2-fp", "T2-pages"];
const CROSS_SCALE_ROW_IDS = SEARCH_ROW_IDS.slice(0, 4);

const searchInvalid = (message) => {
  throw new Error(`invalid paired search report: ${message}`);
};

const isObject = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const isPositiveFinite = (value) => typeof value === "number" && Number.isFinite(value) && value > 0;
const isNonNegativeInteger = (value) => Number.isInteger(value) && value >= 0;
const isPositiveInteger = (value) => Number.isInteger(value) && value > 0;
const requireObject = (value, path) => {
  if (!isObject(value)) searchInvalid(`${path} must be an object`);
  return value;
};
const requireString = (value, path) => {
  if (typeof value !== "string" || value.length === 0) searchInvalid(`${path} must be a non-empty string`);
  return value;
};
const requirePositiveFinite = (value, path) => {
  if (!isPositiveFinite(value)) searchInvalid(`${path} must be a positive finite number`);
  return value;
};
const requirePositiveInteger = (value, path) => {
  if (!isPositiveInteger(value)) searchInvalid(`${path} must be a positive integer`);
  return value;
};
const requireNonNegativeInteger = (value, path) => {
  if (!isNonNegativeInteger(value)) searchInvalid(`${path} must be a non-negative integer`);
  return value;
};

function sameValue(left, right) {
  if (left === right) return true;
  if (Array.isArray(left) && Array.isArray(right)) {
    return left.length === right.length && left.every((value, index) => sameValue(value, right[index]));
  }
  if (isObject(left) && isObject(right)) {
    const leftKeys = Object.keys(left).sort();
    const rightKeys = Object.keys(right).sort();
    return sameValue(leftKeys, rightKeys) && leftKeys.every((key) => sameValue(left[key], right[key]));
  }
  return false;
}

function validateLimits(report, path) {
  const limits = requireObject(report.limits, `${path}.limits`);
  for (const [surface, expected] of Object.entries({
    page_only: { page: 100, block: 0 },
    block_only: { page: 0, block: 100 },
    combined: { page: 100, block: 100 },
  })) {
    const actual = requireObject(limits[surface], `${path}.limits.${surface}`);
    for (const dimension of ["page", "block"]) {
      requireNonNegativeInteger(actual[dimension], `${path}.limits.${surface}.${dimension}`);
      if (actual[dimension] !== expected[dimension]) {
        searchInvalid(`${path}.limits.${surface}.${dimension} must be ${expected[dimension]}`);
      }
    }
  }
  if (limits.quick_switch !== 100) searchInvalid(`${path}.limits.quick_switch must be 100`);
  return limits;
}

function validateSurface(surface, report, path) {
  const name = requireString(surface.surface, `${path}.surface`);
  const expectedLimits = name === "quick_switch_100"
    ? { page: report.limits.quick_switch, block: 0 }
    : report.limits[name];
  if (!expectedLimits) searchInvalid(`${path}.surface has unknown surface ${JSON.stringify(name)}`);
  if (surface.page_limit !== expectedLimits.page || surface.block_limit !== expectedLimits.block) {
    searchInvalid(`${path} limits do not match the report limits`);
  }
  const hits = requireObject(surface.actual_hits, `${path}.actual_hits`);
  for (const field of ["total", "pages", "blocks"]) {
    requireNonNegativeInteger(hits[field], `${path}.actual_hits.${field}`);
  }
  if (hits.total !== hits.pages + hits.blocks) searchInvalid(`${path}.actual_hits.total must equal pages + blocks`);
  requirePositiveFinite(surface.p95_ms, `${path}.p95_ms`);
  if (!Array.isArray(surface.raw_ns)) searchInvalid(`${path}.raw_ns must be an array`);
  if (surface.raw_ns.length !== report.runs) {
    searchInvalid(`${path}.raw_ns length ${surface.raw_ns.length} does not match runs ${report.runs}`);
  }
  surface.raw_ns.forEach((value, index) => requirePositiveFinite(value, `${path}.raw_ns[${index}]`));
}

function validateRawReport(report, role, expectedCorpus) {
  const path = role;
  requireObject(report, path);
  if (report.schema !== RAW_SEARCH_REPORT_SCHEMA) searchInvalid(`${path}.schema must be ${RAW_SEARCH_REPORT_SCHEMA}`);
  if (report.proposed_compact_backend !== false) searchInvalid(`${path}.proposed_compact_backend must be false`);
  if (report.prototype_claims !== false) searchInvalid(`${path}.prototype_claims must be false`);
  if (report.production_source_dirty !== false) searchInvalid(`${path}.production_source_dirty must be false`);
  requireString(report.measurement_kind, `${path}.measurement_kind`);
  requireString(report.backend_under_test, `${path}.backend_under_test`);
  requireString(report.repository_head, `${path}.repository_head`);
  if (report.corpus !== expectedCorpus) searchInvalid(`${path}.corpus must be ${JSON.stringify(expectedCorpus)}`);
  const expectedFixtureMode = role === "sentinel" ? "page_sentinel" : "copied_corpus";
  if (report.fixture_mode !== expectedFixtureMode) {
    searchInvalid(`${path}.fixture_mode must be ${JSON.stringify(expectedFixtureMode)}`);
  }
  if (report.graph_count !== 1) searchInvalid(`${path}.graph_count must be 1`);
  requirePositiveInteger(report.runs, `${path}.runs`);
  requirePositiveInteger(report.warmups_per_surface, `${path}.warmups_per_surface`);
  if (report.warmups_excluded !== true) searchInvalid(`${path}.warmups_excluded must be true`);
  validateLimits(report, path);

  const provenance = requireObject(report.current_backend_provenance, `${path}.current_backend_provenance`);
  requireString(provenance.public_base_sha, `${path}.current_backend_provenance.public_base_sha`);
  requireString(provenance.probe_source_sha256, `${path}.current_backend_provenance.probe_source_sha256`);
  requireString(provenance.production_source_scope, `${path}.current_backend_provenance.production_source_scope`);
  const sourceHash = requireObject(provenance.production_source_hash, `${path}.current_backend_provenance.production_source_hash`);
  requireString(sourceHash.kind, `${path}.current_backend_provenance.production_source_hash.kind`);
  requireString(sourceHash.value, `${path}.current_backend_provenance.production_source_hash.value`);

  const counts = requireObject(report.corpus_counts, `${path}.corpus_counts`);
  requirePositiveInteger(counts.original_page_count, `${path}.corpus_counts.original_page_count`);
  requirePositiveInteger(counts.original_block_count, `${path}.corpus_counts.original_block_count`);
  if (report.page_count !== counts.original_page_count || report.block_count !== counts.original_block_count) {
    searchInvalid(`${path} top-level page_count/block_count must equal the original corpus counts`);
  }
  const inventory = requireObject(report.navigation_name_inventory, `${path}.navigation_name_inventory`);
  requirePositiveInteger(inventory.count, `${path}.navigation_name_inventory.count`);
  if (typeof inventory.count_unit !== "string" || !inventory.count_unit.includes("owner rows")) {
    searchInvalid(`${path}.navigation_name_inventory.count_unit must identify navigable owner rows`);
  }
  if (!Array.isArray(report.queries)) searchInvalid(`${path}.queries must be an array`);
  const labels = new Set();
  report.queries.forEach((query, queryIndex) => {
    requireObject(query, `${path}.queries[${queryIndex}]`);
    const label = requireString(query.label, `${path}.queries[${queryIndex}].label`);
    requireString(query.needle, `${path}.queries[${queryIndex}].needle`);
    if (labels.has(label)) searchInvalid(`${path}.queries has duplicate label ${JSON.stringify(label)}`);
    labels.add(label);
    if (!Array.isArray(query.surfaces) || query.surfaces.length !== 4) {
      searchInvalid(`${path}.queries[${queryIndex}].surfaces must contain all four surfaces`);
    }
    const surfaces = new Set();
    query.surfaces.forEach((surface, surfaceIndex) => {
      requireObject(surface, `${path}.queries[${queryIndex}].surfaces[${surfaceIndex}]`);
      validateSurface(surface, report, `${path}.queries[${queryIndex}].surfaces[${surfaceIndex}]`);
      if (surfaces.has(surface.surface)) searchInvalid(`${path}.${label} has duplicate surface ${JSON.stringify(surface.surface)}`);
      surfaces.add(surface.surface);
    });
    for (const required of ["page_only", "block_only", "combined", "quick_switch_100"]) {
      if (!surfaces.has(required)) searchInvalid(`${path}.${label} is missing surface ${JSON.stringify(required)}`);
    }
  });
  return report;
}

function findQuery(report, label, role) {
  const query = report.queries.find((entry) => entry.label === label);
  if (!query) searchInvalid(`${role}.queries is missing required label ${JSON.stringify(label)}`);
  return query;
}

function findSurface(query, surface, role) {
  const result = query.surfaces.find((entry) => entry.surface === surface);
  if (!result) searchInvalid(`${role}.${query.label} is missing surface ${JSON.stringify(surface)}`);
  return result;
}

function validateSameSource(reports) {
  const [firstRole, first] = reports[0];
  const comparable = (report) => ({
    measurement_kind: report.measurement_kind,
    backend_under_test: report.backend_under_test,
    repository_head: report.repository_head,
    public_base_sha: report.current_backend_provenance.public_base_sha,
    production_source_hash: report.current_backend_provenance.production_source_hash,
    production_source_scope: report.current_backend_provenance.production_source_scope,
  });
  for (const [role, report] of reports.slice(1)) {
    if (!sameValue(comparable(first), comparable(report))) {
      searchInvalid(`${role} production/backend identity does not match ${firstRole}`);
    }
  }
}

function validateSameProbeHarness(leftRole, left, rightRole, right) {
  if (left.current_backend_provenance.probe_source_sha256 !== right.current_backend_provenance.probe_source_sha256) {
    searchInvalid(`${leftRole} and ${rightRole} probe harness SHA-256 values must match`);
  }
}

function validateAugmentation(report, role) {
  const augmentation = requireObject(report.scratch_augmentation, `${role}.scratch_augmentation`);
  if (augmentation.provided !== true) searchInvalid(`${role}.scratch_augmentation.provided must be true`);
  const hash = requireString(augmentation.manifest_sha256, `${role}.scratch_augmentation.manifest_sha256`);
  if (!/^[0-9a-f]{64}$/i.test(hash)) searchInvalid(`${role}.scratch_augmentation.manifest_sha256 must be a SHA-256 hex digest`);
  for (const [field, expected] of [["older_true_raw_block_count", 7], ["newer_false_raw_block_count", 1201], ["added_page_count", 2], ["added_block_count", 1208]]) {
    if (augmentation[field] !== expected) searchInvalid(`${role}.scratch_augmentation.${field} must be ${expected}`);
  }
  if (augmentation.exact_raw_blocks_retained_in_array_order !== true) {
    searchInvalid(`${role}.scratch_augmentation must retain the exact raw blocks in array order`);
  }
  if (augmentation.raw_block_text_transformed !== false || augmentation.source_graph_modified !== false) {
    searchInvalid(`${role}.scratch_augmentation reports transformed input or a modified source graph`);
  }
  if (report.corpus_counts.augmentation_added_page_count !== 2 || report.corpus_counts.augmentation_added_block_count !== 1208) {
    searchInvalid(`${role}.corpus_counts must report the 2-page/1208-block augmentation`);
  }
  if (report.corpus_counts.actual_page_count_after_augmentation !== report.page_count + 2
      || report.corpus_counts.actual_block_count_after_augmentation !== report.block_count + 1208) {
    searchInvalid(`${role}.corpus_counts post-augmentation totals do not derive from the original counts`);
  }
  return augmentation;
}

function assertHits(surface, expected, path) {
  for (const [field, value] of Object.entries(expected)) {
    if (surface.actual_hits[field] !== value) searchInvalid(`${path}.actual_hits.${field} must be ${value}`);
  }
}

function searchScalingSummary(input, policy) {
  requireObject(input, "wrapper");
  if (input.schema !== SEARCH_REPORT_SCHEMA) searchInvalid(`wrapper.schema must be ${SEARCH_REPORT_SCHEMA}`);
  const searchPolicy = requireObject(policy?.searchScaling, "policy.searchScaling");
  const corpora = requireObject(searchPolicy.corpora, "policy.searchScaling.corpora");
  const rowPolicy = requireObject(searchPolicy.rows, "policy.searchScaling.rows");
  for (const id of SEARCH_ROW_IDS) requireObject(rowPolicy[id], `policy.searchScaling.rows.${id}`);

  const small = validateRawReport(input.small, "small", corpora.small);
  const large = validateRawReport(input.large, "large", corpora.large);
  const pages = validateRawReport(input.pages, "pages", corpora.pages);
  const sentinel = validateRawReport(input.sentinel, "sentinel", corpora.sentinel);
  validateSameSource([["small", small], ["large", large], ["pages", pages], ["sentinel", sentinel]]);
  validateSameProbeHarness("small", small, "large", large);
  validateSameProbeHarness("pages", pages, "sentinel", sentinel);
  if (small.corpus_counts.original_block_count !== 60_000) searchInvalid("small original_block_count must be 60000");
  if (large.corpus_counts.original_block_count !== 600_000) searchInvalid("large original_block_count must be 600000");
  if (!sameValue(small.limits, large.limits)) searchInvalid("small and large limits must match");
  if (small.runs !== large.runs) searchInvalid("small and large runs must match");
  if (small.warmups_per_surface !== large.warmups_per_surface) {
    searchInvalid("small and large warmups_per_surface must match");
  }

  const smallAugmentation = validateAugmentation(small, "small");
  const largeAugmentation = validateAugmentation(large, "large");
  if (smallAugmentation.manifest_sha256 !== largeAugmentation.manifest_sha256) {
    searchInvalid("small and large augmentation manifest SHA-256 values must match");
  }

  const rows = {};
  for (const id of CROSS_SCALE_ROW_IDS) {
    const config = rowPolicy[id];
    const smallQuery = findQuery(small, requireString(config.sourceLabel, `policy.searchScaling.rows.${id}.sourceLabel`), "small");
    const largeQuery = findQuery(large, config.sourceLabel, "large");
    if (smallQuery.needle !== largeQuery.needle) searchInvalid(`${id} small and large needles must match`);
    const smallSurface = findSurface(smallQuery, config.surface, "small");
    const largeSurface = findSurface(largeQuery, config.surface, "large");
    rows[id] = {
      needle: smallQuery.needle,
      smallP95Ms: smallSurface.p95_ms,
      largeP95Ms: largeSurface.p95_ms,
      ratio: largeSurface.p95_ms / smallSurface.p95_ms,
      smallSurface,
      largeSurface,
    };
  }

  if ([...rows["T2-nohit"].needle].length < 3) searchInvalid("T2-nohit needle must contain at least three Unicode scalars");
  for (const [role, report] of [["small", small], ["large", large]]) {
    const nohit = findQuery(report, rowPolicy["T2-nohit"].sourceLabel, role);
    for (const surface of nohit.surfaces) assertHits(surface, { total: 0 }, `${role}.T2-nohit.${surface.surface}`);
  }
  if ([...rows["T2-broad"].needle].length !== 2) searchInvalid("T2-broad needle must contain exactly two Unicode scalars");
  for (const role of ["small", "large"]) {
    const surface = rows["T2-broad"][`${role}Surface`];
    assertHits(surface, { blocks: 100 }, `${role}.T2-broad.combined`);
  }
  for (const role of ["small", "large"]) {
    assertHits(rows["T2-sparse"][`${role}Surface`], { total: 7, pages: 0, blocks: 7 }, `${role}.T2-sparse.combined`);
    assertHits(rows["T2-fp"][`${role}Surface`], { total: 1, pages: 0, blocks: 1 }, `${role}.T2-fp.combined`);
  }
  if (rows["T2-sparse"].needle !== smallAugmentation.sparse_query || rows["T2-sparse"].needle !== largeAugmentation.sparse_query) {
    searchInvalid("T2-sparse needle must match both augmentation sparse_query values");
  }

  const pagesQuery = findQuery(pages, rowPolicy["T2-pages"].sourceLabel, "pages");
  if (pagesQuery.needle !== rows["T2-nohit"].needle) searchInvalid("T2-pages no-hit needle must match the paired T2-nohit needle");
  const pagesSurface = findSurface(pagesQuery, rowPolicy["T2-pages"].surface, "pages");
  assertHits(pagesSurface, { total: 0, pages: 0, blocks: 0 }, "pages.T2-pages.quick_switch_100");

  const checks = requireObject(sentinel.sentinel_checks, "sentinel.sentinel_checks");
  for (const field of ["sentinel_created_before_later_pages", "quick_switch_returned_exact", "page_only_returned_exact", "passed"]) {
    if (checks[field] !== true) searchInvalid(`sentinel.sentinel_checks.${field} must be true`);
  }
  if (checks.actual_projection_rowid_recency_asserted !== false) {
    searchInvalid("sentinel correctness must not claim projection-rowid recency proof for the current backend");
  }
  requirePositiveInteger(checks.matching_candidate_count, "sentinel.sentinel_checks.matching_candidate_count");
  requirePositiveInteger(checks.later_page_count, "sentinel.sentinel_checks.later_page_count");
  if (checks.matching_candidate_count_exceeds_1000 !== true) {
    searchInvalid("sentinel.sentinel_checks.matching_candidate_count_exceeds_1000 must be true");
  }
  if (checks.matching_candidate_count <= 1_000) {
    searchInvalid("sentinel.sentinel_checks.matching_candidate_count must exceed 1000");
  }
  if (checks.later_page_count <= 1_000) {
    searchInvalid("sentinel.sentinel_checks.later_page_count must exceed 1000");
  }
  if (checks.matching_candidate_count !== checks.later_page_count + 1) {
    searchInvalid("sentinel.sentinel_checks.matching_candidate_count must equal later_page_count plus the old sentinel");
  }

  return {
    small,
    large,
    pages,
    sentinel,
    rows,
    pagesNeedle: pagesQuery.needle,
    pagesP95Ms: pagesSurface.p95_ms,
    augmentationHash: smallAugmentation.manifest_sha256,
  };
}

function baselineComparison(id, current, baseline) {
  if (baseline == null) return null;
  const rows = requireObject(baseline.rows, "policy.searchScaling.baseline.rows");
  const prior = requireObject(rows[id], `policy.searchScaling.baseline.rows.${id}`);
  if (id === "T2-pages") {
    const pageP95Ms = requirePositiveFinite(prior.page_p95_ms, `policy.searchScaling.baseline.rows.${id}.page_p95_ms`);
    return { pageP95Ms, currentToBaseline: current.pageP95Ms / pageP95Ms };
  }
  const smallP95Ms = requirePositiveFinite(prior.small_p95_ms, `policy.searchScaling.baseline.rows.${id}.small_p95_ms`);
  const largeP95Ms = requirePositiveFinite(prior.large_p95_ms, `policy.searchScaling.baseline.rows.${id}.large_p95_ms`);
  const ratio = requirePositiveFinite(prior.ratio, `policy.searchScaling.baseline.rows.${id}.ratio`);
  return {
    smallP95Ms,
    largeP95Ms,
    ratio,
    smallCurrentToBaseline: current.smallP95Ms / smallP95Ms,
    largeCurrentToBaseline: current.largeP95Ms / largeP95Ms,
    ratioCurrentToBaseline: current.ratio / ratio,
  };
}

// Evaluate the manager-provided wrapper without building or rerunning a probe.
// T2-broad and T2-sparse use their hard ratio ceilings directly: the legacy
// policy noise band deliberately does not apply. The other rows are named,
// unjudged diagnostics.
export function evaluateSearchScaling(input, policy) {
  const summary = searchScalingSummary(input, policy);
  const searchPolicy = policy.searchScaling;
  const rows = CROSS_SCALE_ROW_IDS.map((id) => {
    const current = summary.rows[id];
    const config = searchPolicy.rows[id];
    const ceiling = config.ratioCeiling;
    if (ceiling !== null) requirePositiveFinite(ceiling, `policy.searchScaling.rows.${id}.ratioCeiling`);
    const ok = ceiling == null ? null : current.ratio <= ceiling;
    return {
      id,
      label: id,
      smallP95Ms: current.smallP95Ms,
      largeP95Ms: current.largeP95Ms,
      pageP95Ms: null,
      ratio: current.ratio,
      ceiling,
      ok,
      status: ok == null ? "DIAGNOSTIC" : ok ? "PASS" : "BREACH",
      exception: config.exception ?? null,
      baselineComparison: baselineComparison(id, current, searchPolicy.baseline),
    };
  });
  const pageCurrent = { pageP95Ms: summary.pagesP95Ms };
  rows.push({
    id: "T2-pages",
    label: "T2-pages",
    smallP95Ms: null,
    largeP95Ms: null,
    pageP95Ms: summary.pagesP95Ms,
    ratio: null,
    ceiling: null,
    ok: null,
    status: "DIAGNOSTIC",
    exception: searchPolicy.rows["T2-pages"].exception ?? null,
    baselineComparison: baselineComparison("T2-pages", pageCurrent, searchPolicy.baseline),
  });
  return { rows, breaches: rows.filter((row) => row.ok === false) };
}

// Keep only scalar summaries and reproducibility identities. In particular,
// source paths, raw timing arrays, and corpus text never enter the policy file.
export function baselineFromSearchScaling(input, policy) {
  const summary = searchScalingSummary(input, policy);
  return {
    recorded: new Date().toISOString().slice(0, 10),
    source_identity: {
      public_base_sha: summary.small.current_backend_provenance.public_base_sha,
      production_source_hash: summary.small.current_backend_provenance.production_source_hash,
      production_source_scope: summary.small.current_backend_provenance.production_source_scope,
      probe_source_sha256_by_role: Object.fromEntries(
        ["small", "large", "pages", "sentinel"].map((role) => [
          role,
          summary[role].current_backend_provenance.probe_source_sha256,
        ]),
      ),
    },
    counts: {
      small: {
        original_pages: summary.small.corpus_counts.original_page_count,
        original_blocks: summary.small.corpus_counts.original_block_count,
        navigation_owner_rows: summary.small.navigation_name_inventory.count,
      },
      large: {
        original_pages: summary.large.corpus_counts.original_page_count,
        original_blocks: summary.large.corpus_counts.original_block_count,
        navigation_owner_rows: summary.large.navigation_name_inventory.count,
      },
      pages: {
        original_pages: summary.pages.corpus_counts.original_page_count,
        original_blocks: summary.pages.corpus_counts.original_block_count,
        navigation_owner_rows: summary.pages.navigation_name_inventory.count,
      },
    },
    queries: {
      ...Object.fromEntries(CROSS_SCALE_ROW_IDS.map((id) => [id, summary.rows[id].needle])),
      "T2-pages": summary.pagesNeedle,
    },
    limits: summary.small.limits,
    augmentation_sha256: summary.augmentationHash,
    rows: {
      ...Object.fromEntries(CROSS_SCALE_ROW_IDS.map((id) => [id, {
        small_p95_ms: summary.rows[id].smallP95Ms,
        large_p95_ms: summary.rows[id].largeP95Ms,
        ratio: summary.rows[id].ratio,
      }])),
      "T2-pages": { page_p95_ms: summary.pagesP95Ms },
    },
  };
}

const formatSearchNumber = (value, digits = 6) => Number(value).toFixed(digits).replace(/\.?0+$/, "");

export function formatSearchScalingRows(rows) {
  const lines = [
    "| row | small p95 | large / page p95 | ratio | ceiling | status | baseline comparison |",
    "|---|---:|---:|---:|---:|:-:|---|",
  ];
  for (const row of rows) {
    const small = row.smallP95Ms == null ? "–" : `${formatSearchNumber(row.smallP95Ms)} ms`;
    const large = row.pageP95Ms != null
      ? `${formatSearchNumber(row.pageP95Ms)} ms (page corpus)`
      : `${formatSearchNumber(row.largeP95Ms)} ms`;
    const ratio = row.ratio == null ? "–" : `${formatSearchNumber(row.ratio)}x`;
    const ceiling = row.ceiling == null ? "diagnostic" : `${formatSearchNumber(row.ceiling)}x`;
    let comparison = "(no baseline)";
    if (row.baselineComparison?.currentToBaseline != null) {
      comparison = `page ${formatSearchNumber(row.baselineComparison.currentToBaseline)}x baseline`;
    } else if (row.baselineComparison) {
      comparison = `small ${formatSearchNumber(row.baselineComparison.smallCurrentToBaseline)}x; large ${formatSearchNumber(row.baselineComparison.largeCurrentToBaseline)}x; ratio ${formatSearchNumber(row.baselineComparison.ratioCurrentToBaseline)}x baseline`;
    }
    const status = row.exception ? `${row.status}: ${row.exception}` : row.status;
    lines.push(`| ${row.id} | ${small} | ${large} | ${ratio} | ${ceiling} | ${status} | ${comparison} |`);
  }
  return lines.join("\n");
}

export function formatRows(rows) {
  const lines = ["| row | value | ceiling | ok |", "|---|---:|---:|:-:|"];
  for (const entry of rows) {
    const value = `${Number(entry.value).toFixed(entry.unit === "x" ? 2 : 0)} ${entry.unit}`;
    const ceiling = entry.ceiling == null ? "(no baseline)" : `${Number(entry.ceiling).toFixed(entry.unit === "x" ? 2 : 0)} ${entry.unit}`;
    const ok = entry.ok == null ? "–" : entry.ok ? "ok" : "BREACH";
    lines.push(`| ${entry.id} ${entry.label} | ${value} | ${ceiling} | ${ok} |`);
  }
  return lines.join("\n");
}
