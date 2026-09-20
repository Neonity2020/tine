// Non-authoritative search matching for the browser preview and frontend tests.
// Production search sends raw syntax to Rust, whose A6 fold and match evidence
// are authoritative. Keep this small adapter's legacy lowercase-plus-NFC
// behavior; it intentionally does not claim Unicode parity with native search.

import { simpleTerm, type SearchMatcher, type Term } from "./editor/searchQuery";

interface SourceSpan { start: number; end: number }

const graphemeSegmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });

export function mockSearchFold(value: string): string {
  return value.toLowerCase().normalize("NFC");
}

function foldedWithMap(original: string): { scalars: string[]; spans: SourceSpan[] } {
  const lowered = original.toLowerCase();
  const loweredSources: SourceSpan[] = [];
  let originalUtf16 = 0;
  for (const scalar of original) {
    const start = originalUtf16;
    originalUtf16 += scalar.length;
    for (const _ of scalar.toLowerCase()) loweredSources.push({ start, end: originalUtf16 });
  }

  const scalars: string[] = [];
  const spans: SourceSpan[] = [];
  let sourceAt = 0;
  for (const part of graphemeSegmenter.segment(lowered)) {
    const count = Array.from(part.segment).length;
    const contributors = loweredSources.slice(sourceAt, sourceAt + count);
    sourceAt += count;
    const source = {
      start: contributors[0]?.start ?? 0,
      end: contributors.at(-1)?.end ?? 0,
    };
    for (const scalar of part.segment.normalize("NFC")) {
      scalars.push(scalar);
      spans.push(source);
    }
  }
  return { scalars, spans };
}

function substringSpans(text: string, needle: string, limit: number): SourceSpan[] {
  const hay = foldedWithMap(text);
  const wanted = Array.from(mockSearchFold(needle));
  if (!wanted.length || wanted.length > hay.scalars.length) return [];
  const out: SourceSpan[] = [];
  for (let at = 0; at <= hay.scalars.length - wanted.length && out.length < limit; at += 1) {
    if (!wanted.every((scalar, index) => hay.scalars[at + index] === scalar)) continue;
    const span = {
      start: hay.spans[at].start,
      end: hay.spans[at + wanted.length - 1].end,
    };
    if (!out.some((existing) => existing.start === span.start && existing.end === span.end)) out.push(span);
  }
  return out;
}

function groupMatches(group: Term[], foldedBody: string): boolean {
  return group.every((term) => {
    const needle = mockSearchFold(term.text);
    const present = needle !== "" && foldedBody.includes(needle);
    return present !== term.negated;
  });
}

export function mockSearchMatches(matcher: SearchMatcher, original: string): boolean {
  switch (matcher.kind) {
    case "regex":
      return matcher.re.test(original);
    case "boolean":
      return matcher.groups.some((group) => groupMatches(group, mockSearchFold(original)));
    default:
      return false;
  }
}

export function mockSearchSimpleTerm(matcher: SearchMatcher): string | null {
  const raw = simpleTerm(matcher);
  return raw === null ? null : mockSearchFold(raw);
}

export function mockSearchMatchHighlight(
  matcher: SearchMatcher,
  text: string,
): { start: number; len: number } | null {
  if (matcher.kind === "regex") {
    const hit = matcher.re.exec(text);
    return hit ? { start: hit.index, len: hit[0].length } : null;
  }
  if (matcher.kind === "boolean") {
    let best: { start: number; len: number } | null = null;
    for (const group of matcher.groups) {
      for (const term of group) {
        if (term.negated || !term.text) continue;
        const span = substringSpans(text, term.text, 1)[0];
        if (span && (!best || span.start < best.start)) {
          best = { start: span.start, len: span.end - span.start };
        }
      }
    }
    return best;
  }
  return null;
}

export function mockSearchHighlights(
  matcher: SearchMatcher,
  text: string,
  limit = 24,
): { start: number; end: number }[] {
  if (matcher.kind === "regex") {
    const flags = matcher.re.flags.includes("g") ? matcher.re.flags : `${matcher.re.flags}g`;
    const re = new RegExp(matcher.re.source, flags);
    const out: { start: number; end: number }[] = [];
    for (const hit of text.matchAll(re)) {
      const start = hit.index ?? 0;
      const end = start + hit[0].length;
      if (end > start) out.push({ start, end });
      if (out.length >= limit) break;
    }
    return out;
  }
  if (matcher.kind !== "boolean") return [];
  const foldedBody = mockSearchFold(text);
  const group = matcher.groups.find((candidate) => groupMatches(candidate, foldedBody));
  if (!group) return [];
  const out: { start: number; end: number }[] = [];
  for (const term of group) {
    if (term.negated || !term.text) continue;
    out.push(...substringSpans(text, term.text, limit - out.length));
  }
  return out.sort((a, b) => a.start - b.start || a.end - b.end);
}
