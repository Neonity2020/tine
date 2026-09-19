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
