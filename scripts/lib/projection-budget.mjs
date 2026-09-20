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

const SEARCH_REPORT_SCHEMA = "tine.search_scaling.v2";
const SEARCH_POLICY_SCHEMA = "tine.search_scaling_budget.v2";
const RAW_SEARCH_REPORT_SCHEMA = "tine.s7_page_search_probe.v1";
const EXHAUSTIVE_NAME_EXECUTOR = "exhaustive navigation owner inventory (no candidate cap)";
const SEARCH_ROLES = ["small", "blocksLarge", "namesLarge", "large", "pages", "sentinel"];
const AXIS_ROLES = ["small", "blocksLarge", "namesLarge", "large"];
const SEARCH_ROW_IDS = ["T2-nohit", "T2-broad", "T2-sparse", "T2-fp", "T2-names", "T2-pages"];
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
  requireString(inventory.method, `${path}.navigation_name_inventory.method`);
  requirePositiveInteger(inventory.count, `${path}.navigation_name_inventory.count`);
  if (inventory.reported_count_scope !== "original_corpus_before_scratch_augmentation") {
    searchInvalid(`${path}.navigation_name_inventory.reported_count_scope must identify the original corpus before scratch augmentation`);
  }
  requireNonNegativeInteger(inventory.augmentation_added_owner_rows, `${path}.navigation_name_inventory.augmentation_added_owner_rows`);
  requirePositiveInteger(inventory.actual_count_after_augmentation, `${path}.navigation_name_inventory.actual_count_after_augmentation`);
  if (inventory.count + inventory.augmentation_added_owner_rows !== inventory.actual_count_after_augmentation) {
    searchInvalid(`${path}.navigation_name_inventory actual count must equal original count plus augmentation owner rows`);
  }
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

function validateSameSourceAndProbe(reports) {
  const [firstRole, first] = reports[0];
  const comparable = (report) => ({
    measurement_kind: report.measurement_kind,
    backend_under_test: report.backend_under_test,
    repository_head: report.repository_head,
    public_base_sha: report.current_backend_provenance.public_base_sha,
    production_source_hash: report.current_backend_provenance.production_source_hash,
    production_source_scope: report.current_backend_provenance.production_source_scope,
    probe_source_sha256: report.current_backend_provenance.probe_source_sha256,
    paired_storage_commit: report.current_backend_provenance.paired_storage_commit,
  });
  for (const [role, report] of reports.slice(1)) {
    if (!sameValue(comparable(first), comparable(report))) {
      searchInvalid(`${role} production/backend/probe identity does not match ${firstRole}`);
    }
  }
}

function validateSameRunConfiguration(reports) {
  const [firstRole, first] = reports[0];
  for (const [role, report] of reports.slice(1)) {
    if (!sameValue(first.limits, report.limits)) {
      searchInvalid(`${role} limits must match ${firstRole}`);
    }
    if (report.runs !== first.runs) {
      searchInvalid(`${role} runs must match ${firstRole}`);
    }
    if (report.warmups_per_surface !== first.warmups_per_surface) {
      searchInvalid(`${role} warmups_per_surface must match ${firstRole}`);
    }
  }
}

function workloadIdentity(report) {
  return report.queries.map((query) => ({
    label: query.label,
    needle: query.needle,
    surfaces: query.surfaces.map((surface) => ({
      surface: surface.surface,
      page_limit: surface.page_limit,
      block_limit: surface.block_limit,
    })),
  }));
}

function validateSameAxisWorkload(reports) {
  const [firstRole, first] = reports[0];
  const firstWorkload = workloadIdentity(first);
  for (const [role, report] of reports.slice(1)) {
    if (!sameValue(workloadIdentity(report), firstWorkload)) {
      searchInvalid(`${role} query workload must match ${firstRole}`);
    }
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
  if (report.navigation_name_inventory.augmentation_added_owner_rows !== 2) {
    searchInvalid(`${role}.navigation_name_inventory must report 2 scratch-augmentation owner rows`);
  }
  return augmentation;
}

function validateNoAugmentation(report, role) {
  const augmentation = requireObject(report.scratch_augmentation, `${role}.scratch_augmentation`);
  if (augmentation.provided !== false) searchInvalid(`${role}.scratch_augmentation.provided must be false`);
  const counts = report.corpus_counts;
  if (counts.augmentation_added_page_count !== 0 || counts.augmentation_added_block_count !== 0) {
    searchInvalid(`${role}.corpus_counts must report no scratch augmentation`);
  }
  if (counts.actual_page_count_after_augmentation !== report.page_count
      || counts.actual_block_count_after_augmentation !== report.block_count) {
    searchInvalid(`${role}.corpus_counts totals must equal the unaugmented original counts`);
  }
  if (report.navigation_name_inventory.augmentation_added_owner_rows !== 0) {
    searchInvalid(`${role}.navigation_name_inventory must report no scratch-augmentation owner rows`);
  }
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
  if (searchPolicy.schema !== SEARCH_POLICY_SCHEMA) {
    searchInvalid(`policy.searchScaling.schema must be ${SEARCH_POLICY_SCHEMA}`);
  }
  const corpora = requireObject(searchPolicy.corpora, "policy.searchScaling.corpora");
  const rowPolicy = requireObject(searchPolicy.rows, "policy.searchScaling.rows");
  for (const id of SEARCH_ROW_IDS) requireObject(rowPolicy[id], `policy.searchScaling.rows.${id}`);

  const reports = Object.fromEntries(SEARCH_ROLES.map((role) => {
    const expectedCorpus = requireString(corpora[role], `policy.searchScaling.corpora.${role}`);
    return [role, validateRawReport(input[role], role, expectedCorpus)];
  }));
  const reportEntries = SEARCH_ROLES.map((role) => [role, reports[role]]);
  const axisEntries = AXIS_ROLES.map((role) => [role, reports[role]]);
  validateSameSourceAndProbe(reportEntries);
  validateSameRunConfiguration(reportEntries);
  validateSameAxisWorkload(axisEntries);

  const expectedAxisCounts = {
    small: { pages: 1_000, blocks: 60_000 },
    blocksLarge: { pages: 1_000, blocks: 600_000 },
    namesLarge: { pages: 10_000, blocks: 60_000 },
    large: { pages: 10_000, blocks: 600_000 },
  };
  for (const [role, expected] of Object.entries(expectedAxisCounts)) {
    const counts = reports[role].corpus_counts;
    if (counts.original_page_count !== expected.pages) {
      searchInvalid(`${role} original_page_count must be ${expected.pages}`);
    }
    if (counts.original_block_count !== expected.blocks) {
      searchInvalid(`${role} original_block_count must be ${expected.blocks}`);
    }
  }
  if (reports.small.navigation_name_inventory.count !== reports.blocksLarge.navigation_name_inventory.count) {
    searchInvalid("small and blocksLarge navigation owner inventory counts must match for fixed-name block growth");
  }
  if (reports.small.navigation_name_inventory.actual_count_after_augmentation
      !== reports.blocksLarge.navigation_name_inventory.actual_count_after_augmentation) {
    searchInvalid("small and blocksLarge actual navigation owner inventory counts must match for fixed-name block growth");
  }
  if (reports.namesLarge.navigation_name_inventory.count !== reports.large.navigation_name_inventory.count) {
    searchInvalid("namesLarge and large navigation owner inventory counts must match for fixed-name block growth");
  }
  if (reports.namesLarge.navigation_name_inventory.actual_count_after_augmentation
      !== reports.large.navigation_name_inventory.actual_count_after_augmentation) {
    searchInvalid("namesLarge and large actual navigation owner inventory counts must match for fixed-name block growth");
  }
  if (reports.namesLarge.navigation_name_inventory.count - reports.small.navigation_name_inventory.count !== 9_000) {
    searchInvalid("namesLarge navigation owner inventory count minus small must equal 9000 added physical pages");
  }
  if (reports.namesLarge.navigation_name_inventory.actual_count_after_augmentation
      - reports.small.navigation_name_inventory.actual_count_after_augmentation !== 9_000) {
    searchInvalid("namesLarge actual navigation owner inventory count minus small must equal 9000 added physical pages");
  }
  for (const role of AXIS_ROLES.slice(1)) {
    for (const field of ["method", "count_unit"]) {
      if (reports[role].navigation_name_inventory[field] !== reports.small.navigation_name_inventory[field]) {
        searchInvalid(`${role} navigation_name_inventory.${field} must match small`);
      }
    }
  }

  const augmentations = Object.fromEntries(axisEntries.map(([role, report]) => [role, validateAugmentation(report, role)]));
  for (const role of AXIS_ROLES.slice(1)) {
    if (!sameValue(augmentations[role], augmentations.small)) {
      searchInvalid(`${role} scratch augmentation must match small`);
    }
  }
  validateNoAugmentation(reports.pages, "pages");
  validateNoAugmentation(reports.sentinel, "sentinel");

  const rows = {};
  for (const id of CROSS_SCALE_ROW_IDS) {
    const config = rowPolicy[id];
    const sourceLabel = requireString(config.sourceLabel, `policy.searchScaling.rows.${id}.sourceLabel`);
    const surfaceName = requireString(config.surface, `policy.searchScaling.rows.${id}.surface`);
    const queries = Object.fromEntries(AXIS_ROLES.map((role) => [role, findQuery(reports[role], sourceLabel, role)]));
    const surfaces = Object.fromEntries(AXIS_ROLES.map((role) => [role, findSurface(queries[role], surfaceName, role)]));
    rows[id] = {
      needle: queries.small.needle,
      smallP95Ms: surfaces.small.p95_ms,
      blocksLargeP95Ms: surfaces.blocksLarge.p95_ms,
      namesLargeP95Ms: surfaces.namesLarge.p95_ms,
      largeP95Ms: surfaces.large.p95_ms,
      blockRatio: surfaces.blocksLarge.p95_ms / surfaces.small.p95_ms,
      bothGrowingRatio: surfaces.large.p95_ms / surfaces.small.p95_ms,
      surfaces,
    };
  }

  if ([...rows["T2-nohit"].needle].length < 3) searchInvalid("T2-nohit needle must contain at least three Unicode scalars");
  for (const role of AXIS_ROLES) {
    const report = reports[role];
    const nohit = findQuery(report, rowPolicy["T2-nohit"].sourceLabel, role);
    for (const surface of nohit.surfaces) assertHits(surface, { total: 0 }, `${role}.T2-nohit.${surface.surface}`);
  }
  if ([...rows["T2-broad"].needle].length !== 2) searchInvalid("T2-broad needle must contain exactly two Unicode scalars");
  for (const role of AXIS_ROLES) {
    assertHits(rows["T2-broad"].surfaces[role], { blocks: 100 }, `${role}.T2-broad.combined`);
    assertHits(rows["T2-sparse"].surfaces[role], { total: 7, pages: 0, blocks: 7 }, `${role}.T2-sparse.combined`);
    assertHits(rows["T2-fp"].surfaces[role], { total: 1, pages: 0, blocks: 1 }, `${role}.T2-fp.combined`);
  }
  for (const role of AXIS_ROLES) {
    if (rows["T2-sparse"].needle !== augmentations[role].sparse_query) {
      searchInvalid(`T2-sparse needle must match ${role} augmentation sparse_query`);
    }
  }

  const namesConfig = rowPolicy["T2-names"];
  if (!Array.isArray(namesConfig.sourceLabels) || namesConfig.sourceLabels.length !== 2) {
    searchInvalid("policy.searchScaling.rows.T2-names.sourceLabels must contain no-hit and broad labels");
  }
  const namesSurfaceName = requireString(namesConfig.surface, "policy.searchScaling.rows.T2-names.surface");
  if (namesSurfaceName !== "page_only") searchInvalid("policy.searchScaling.rows.T2-names.surface must be page_only");
  if (namesConfig.executor !== EXHAUSTIVE_NAME_EXECUTOR) {
    searchInvalid(`policy.searchScaling.rows.T2-names.executor must be ${JSON.stringify(EXHAUSTIVE_NAME_EXECUTOR)}`);
  }
  const nameCounts = {
    small: reports.small.navigation_name_inventory.actual_count_after_augmentation,
    namesLarge: reports.namesLarge.navigation_name_inventory.actual_count_after_augmentation,
    smallOriginal: reports.small.navigation_name_inventory.count,
    namesLargeOriginal: reports.namesLarge.navigation_name_inventory.count,
    smallPhysicalPages: reports.small.corpus_counts.original_page_count,
    namesLargePhysicalPages: reports.namesLarge.corpus_counts.original_page_count,
    smallAugmentationOwners: reports.small.navigation_name_inventory.augmentation_added_owner_rows,
    namesLargeAugmentationOwners: reports.namesLarge.navigation_name_inventory.augmentation_added_owner_rows,
  };
  nameCounts.ratio = nameCounts.namesLarge / nameCounts.small;
  nameCounts.smallFixedOwners = nameCounts.smallOriginal - nameCounts.smallPhysicalPages;
  nameCounts.namesLargeFixedOwners = nameCounts.namesLargeOriginal - nameCounts.namesLargePhysicalPages;
  const nameCases = namesConfig.sourceLabels.map((sourceLabel, index) => {
    requireString(sourceLabel, `policy.searchScaling.rows.T2-names.sourceLabels[${index}]`);
    const smallSurface = findSurface(findQuery(reports.small, sourceLabel, "small"), namesSurfaceName, "small");
    const namesLargeSurface = findSurface(findQuery(reports.namesLarge, sourceLabel, "namesLarge"), namesSurfaceName, "namesLarge");
    const timeRatio = namesLargeSurface.p95_ms / smallSurface.p95_ms;
    return {
      sourceLabel,
      smallP95Ms: smallSurface.p95_ms,
      namesLargeP95Ms: namesLargeSurface.p95_ms,
      timeRatio,
      normalizedLinearity: timeRatio / nameCounts.ratio,
    };
  });
  const expectedNameLabels = [rowPolicy["T2-nohit"].sourceLabel, rowPolicy["T2-broad"].sourceLabel];
  if (!sameValue(namesConfig.sourceLabels, expectedNameLabels)) {
    searchInvalid("policy.searchScaling.rows.T2-names.sourceLabels must be the no-hit and broad labels in that order");
  }
  assertHits(findSurface(findQuery(reports.namesLarge, rowPolicy["T2-nohit"].sourceLabel, "namesLarge"), "page_only", "namesLarge"), { total: 0 }, "namesLarge.T2-nohit.page_only");
  assertHits(findSurface(findQuery(reports.small, rowPolicy["T2-broad"].sourceLabel, "small"), "page_only", "small"), { pages: 100, blocks: 0 }, "small.T2-broad.page_only");
  assertHits(findSurface(findQuery(reports.namesLarge, rowPolicy["T2-broad"].sourceLabel, "namesLarge"), "page_only", "namesLarge"), { pages: 100, blocks: 0 }, "namesLarge.T2-broad.page_only");

  const pagesQuery = findQuery(reports.pages, rowPolicy["T2-pages"].sourceLabel, "pages");
  if (reports.pages.queries.length !== 1) searchInvalid("pages query workload must contain only the page-search no-hit probe");
  if (pagesQuery.needle !== rows["T2-nohit"].needle) searchInvalid("T2-pages no-hit needle must match the paired T2-nohit needle");
  const pagesSurface = findSurface(pagesQuery, rowPolicy["T2-pages"].surface, "pages");
  assertHits(pagesSurface, { total: 0, pages: 0, blocks: 0 }, "pages.T2-pages.quick_switch_100");

  const checks = requireObject(reports.sentinel.sentinel_checks, "sentinel.sentinel_checks");
  if (reports.sentinel.queries.length !== 1 || reports.sentinel.queries[0].label !== "exact_old_page") {
    searchInvalid("sentinel query workload must contain only exact_old_page");
  }
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
    ...reports,
    rows,
    nameCounts,
    nameCases,
    pagesNeedle: pagesQuery.needle,
    pagesP95Ms: pagesSurface.p95_ms,
    augmentationHash: augmentations.small.manifest_sha256,
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
// All three scaling gates are literal: neither the legacy policy noise band nor
// any second margin applies to the strict ratio, hard-ms, or normalized gates.
export function evaluateSearchScaling(input, policy) {
  const summary = searchScalingSummary(input, policy);
  const searchPolicy = policy.searchScaling;
  const rows = CROSS_SCALE_ROW_IDS.map((id) => {
    const current = summary.rows[id];
    const config = searchPolicy.rows[id];
    const gated = id === "T2-broad" || id === "T2-sparse";
    const ceiling = config.ratioCeiling;
    if (gated) requirePositiveFinite(ceiling, `policy.searchScaling.rows.${id}.ratioCeiling`);
    else if (ceiling !== null) searchInvalid(`policy.searchScaling.rows.${id}.ratioCeiling must be null`);
    const hardCeilingMs = gated
      ? requirePositiveFinite(config.bothGrowingHardCeilingMs, `policy.searchScaling.rows.${id}.bothGrowingHardCeilingMs`)
      : null;
    const ratio = gated ? current.blockRatio : current.bothGrowingRatio;
    const ratioOk = gated ? ratio <= ceiling : null;
    const hardCeilingOk = gated ? current.largeP95Ms <= hardCeilingMs : null;
    const ok = gated ? ratioOk && hardCeilingOk : null;
    const historicalCurrent = {
      smallP95Ms: current.smallP95Ms,
      largeP95Ms: current.largeP95Ms,
      ratio: current.bothGrowingRatio,
    };
    return {
      id,
      label: id,
      smallP95Ms: current.smallP95Ms,
      blocksLargeP95Ms: current.blocksLargeP95Ms,
      namesLargeP95Ms: current.namesLargeP95Ms,
      largeP95Ms: current.largeP95Ms,
      pageP95Ms: null,
      ratio,
      bothGrowingRatio: current.bothGrowingRatio,
      ceiling,
      ratioOk,
      hardCeilingMs,
      hardCeilingOk,
      nameGrowth: null,
      ok,
      status: ok == null ? "DIAGNOSTIC" : ok ? "PASS" : "BREACH",
      exception: config.exception ?? null,
      baselineComparison: baselineComparison(id, historicalCurrent, searchPolicy.baseline),
    };
  });
  const namesConfig = searchPolicy.rows["T2-names"];
  const namesCeiling = requirePositiveFinite(
    namesConfig.normalizedLinearityCeiling,
    "policy.searchScaling.rows.T2-names.normalizedLinearityCeiling",
  );
  const maxNormalizedLinearity = Math.max(...summary.nameCases.map((entry) => entry.normalizedLinearity));
  const namesOk = maxNormalizedLinearity <= namesCeiling;
  rows.push({
    id: "T2-names",
    label: "T2-names",
    smallP95Ms: null,
    blocksLargeP95Ms: null,
    namesLargeP95Ms: null,
    largeP95Ms: null,
    pageP95Ms: null,
    ratio: maxNormalizedLinearity,
    bothGrowingRatio: null,
    ceiling: namesCeiling,
    ratioOk: namesOk,
    hardCeilingMs: null,
    hardCeilingOk: null,
    nameGrowth: {
      executor: namesConfig.executor,
      smallOwnerCount: summary.nameCounts.small,
      namesLargeOwnerCount: summary.nameCounts.namesLarge,
      ownerCountRatio: summary.nameCounts.ratio,
      smallPhysicalPageCount: summary.nameCounts.smallPhysicalPages,
      namesLargePhysicalPageCount: summary.nameCounts.namesLargePhysicalPages,
      smallFixedOwnerCount: summary.nameCounts.smallFixedOwners,
      namesLargeFixedOwnerCount: summary.nameCounts.namesLargeFixedOwners,
      smallAugmentationOwnerCount: summary.nameCounts.smallAugmentationOwners,
      namesLargeAugmentationOwnerCount: summary.nameCounts.namesLargeAugmentationOwners,
      cases: summary.nameCases,
      maxNormalizedLinearity,
    },
    ok: namesOk,
    status: namesOk ? "PASS" : "BREACH",
    exception: null,
    baselineComparison: null,
  });
  const pageCurrent = { pageP95Ms: summary.pagesP95Ms };
  rows.push({
    id: "T2-pages",
    label: "T2-pages",
    smallP95Ms: null,
    blocksLargeP95Ms: null,
    namesLargeP95Ms: null,
    largeP95Ms: null,
    pageP95Ms: summary.pagesP95Ms,
    ratio: null,
    bothGrowingRatio: null,
    ceiling: null,
    ratioOk: null,
    hardCeilingMs: null,
    hardCeilingOk: null,
    nameGrowth: null,
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
        SEARCH_ROLES.map((role) => [
          role,
          summary[role].current_backend_provenance.probe_source_sha256,
        ]),
      ),
    },
    counts: Object.fromEntries(SEARCH_ROLES.filter((role) => role !== "sentinel").map((role) => [role, {
      original_pages: summary[role].corpus_counts.original_page_count,
      original_blocks: summary[role].corpus_counts.original_block_count,
      navigation_owner_rows: summary[role].navigation_name_inventory.count,
    }])),
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
        ratio: summary.rows[id].bothGrowingRatio,
        blocks_large_p95_ms: summary.rows[id].blocksLargeP95Ms,
        block_ratio: summary.rows[id].blockRatio,
      }])),
      "T2-names": {
        small_owner_count: summary.nameCounts.small,
        names_large_owner_count: summary.nameCounts.namesLarge,
        owner_count_ratio: summary.nameCounts.ratio,
        cases: summary.nameCases.map((entry) => ({
          source_label: entry.sourceLabel,
          small_p95_ms: entry.smallP95Ms,
          names_large_p95_ms: entry.namesLargeP95Ms,
          time_ratio: entry.timeRatio,
          normalized_linearity: entry.normalizedLinearity,
        })),
      },
      "T2-pages": { page_p95_ms: summary.pagesP95Ms },
    },
  };
}

const formatSearchNumber = (value, digits = 6) => Number(value).toFixed(digits).replace(/\.?0+$/, "");

export function formatSearchScalingRows(rows) {
  const lines = [
    "| row | measurements | scaling metric | budget | status | historical baseline |",
    "|---|---|---|---|:-:|---|",
  ];
  for (const row of rows) {
    let measurements;
    let metric;
    let budget;
    if (row.nameGrowth) {
      const cases = row.nameGrowth.cases.map((entry) => `${entry.sourceLabel}: ${formatSearchNumber(entry.smallP95Ms)}→${formatSearchNumber(entry.namesLargeP95Ms)} ms (${formatSearchNumber(entry.timeRatio)}x time, ${formatSearchNumber(entry.normalizedLinearity)}x normalized)`).join("; ");
      const inventory = `${row.nameGrowth.smallPhysicalPageCount}→${row.nameGrowth.namesLargePhysicalPageCount} physical pages + ${row.nameGrowth.smallFixedOwnerCount}→${row.nameGrowth.namesLargeFixedOwnerCount} fixed owner rows + ${row.nameGrowth.smallAugmentationOwnerCount}→${row.nameGrowth.namesLargeAugmentationOwnerCount} scratch-augmentation owner rows = ${row.nameGrowth.smallOwnerCount}→${row.nameGrowth.namesLargeOwnerCount} timed owner rows (${formatSearchNumber(row.nameGrowth.ownerCountRatio)}x)`;
      measurements = `${row.nameGrowth.executor}; ${inventory}; ${cases}`;
      metric = `max normalized time/name growth ${formatSearchNumber(row.nameGrowth.maxNormalizedLinearity)}x`;
      budget = `normalized ≤${formatSearchNumber(row.ceiling)}x`;
    } else if (row.hardCeilingMs != null) {
      measurements = `small ${formatSearchNumber(row.smallP95Ms)} ms; blocksLarge ${formatSearchNumber(row.blocksLargeP95Ms)} ms; both-growing ${formatSearchNumber(row.largeP95Ms)} ms`;
      metric = `fixed-name block ratio ${formatSearchNumber(row.ratio)}x; both-growing ratio ${formatSearchNumber(row.bothGrowingRatio)}x`;
      budget = `ratio ≤${formatSearchNumber(row.ceiling)}x; both-growing HARD ≤${formatSearchNumber(row.hardCeilingMs)} ms`;
    } else if (row.pageP95Ms != null) {
      measurements = `${formatSearchNumber(row.pageP95Ms)} ms (page corpus)`;
      metric = "–";
      budget = "diagnostic";
    } else {
      measurements = `small ${formatSearchNumber(row.smallP95Ms)} ms; both-growing ${formatSearchNumber(row.largeP95Ms)} ms`;
      metric = `${formatSearchNumber(row.ratio)}x`;
      budget = "diagnostic";
    }
    let comparison = "(no baseline)";
    if (row.baselineComparison?.currentToBaseline != null) {
      comparison = `page ${formatSearchNumber(row.baselineComparison.currentToBaseline)}x baseline`;
    } else if (row.baselineComparison) {
      comparison = `small ${formatSearchNumber(row.baselineComparison.smallCurrentToBaseline)}x; large ${formatSearchNumber(row.baselineComparison.largeCurrentToBaseline)}x; ratio ${formatSearchNumber(row.baselineComparison.ratioCurrentToBaseline)}x baseline`;
    }
    const status = row.exception ? `${row.status}: ${row.exception}` : row.status;
    lines.push(`| ${row.id} | ${measurements} | ${metric} | ${budget} | ${status} | ${comparison} |`);
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
