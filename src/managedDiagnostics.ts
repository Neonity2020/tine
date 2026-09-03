import {
  SharedFrontierMismatchError,
  type SharedFrontierMismatchCategory,
  type SharedFrontierMismatchPath,
} from "./backend";

/**
 * Reduce a native managed-storage failure to one bounded, shareable line.
 *
 * Backend errors can contain private graph paths, quoted page text, URLs,
 * credentials, or an arbitrarily long debug chain. Recovery UI may preserve
 * the causal class, but must never echo those values to the screen or clipboard.
 */
/** A closed backend vocabulary token: no spaces, no punctuation graph text can reach. */
const TAGGED_TOKEN = /^[a-z][a-z0-9_.-]{0,63}$/u;
/** A refusal scenario id, e.g. `MS-REF-PROTOCOL-INCOMPATIBLE`. */
const TAGGED_SCENARIO = /^[A-Z][A-Z0-9-]{0,63}$/u;

/**
 * Render a tagged backend error envelope, or `null` when this is ordinary prose.
 *
 * A typed backend failure arrives as `{"kind":…,"reason_code":…}` — every field
 * a closed vocabulary the backend authored, with nothing left to vouch for. The
 * prose sanitizer below cannot read it: it collapses any `{…}` group to
 * `[details]`, which then carries no diagnostic word, so the whole envelope
 * became "The command failed without a safe diagnostic detail." That is the
 * exact failure the sanitizer's own comments were written about, and typing the
 * error was supposed to make the reason MORE legible, not less. Recognize the
 * envelope by shape and render it from its own vocabulary; anything that is not
 * this exact shape falls through to the prose path and is sanitized as before.
 */
function taggedBackendErrorDetail(firstLine: string): string | null {
  if (!firstLine.startsWith("{")) return null;
  let payload: unknown;
  try {
    payload = JSON.parse(firstLine);
  } catch {
    return null;
  }
  if (typeof payload !== "object" || payload === null) return null;
  const { kind, reason_code: reasonCode, detail } = payload as Record<string, unknown>;
  if (typeof kind !== "string" || !TAGGED_TOKEN.test(kind)) return null;
  if (typeof reasonCode !== "string" || !TAGGED_TOKEN.test(reasonCode)) return null;
  let rendered = `${kind} failure: ${reasonCode}`;
  if (typeof detail === "object" && detail !== null) {
    const scenario = (detail as Record<string, unknown>).scenario;
    if (typeof scenario === "string" && TAGGED_SCENARIO.test(scenario)) {
      rendered = `${rendered} (${scenario})`;
    }
  }
  return rendered;
}

export function safeManagedErrorDetail(error: unknown): string {
  const message = error instanceof Error ? error.message : typeof error === "string" ? error : "";
  const firstLine = message.split(/[\r\n]/, 1)[0]?.trim() ?? "";
  if (!firstLine) return "The command did not provide a safe diagnostic detail.";
  const tagged = taggedBackendErrorDetail(firstLine);
  if (tagged) return tagged;
  let safe = firstLine
    .replace(/\bfile:\/\/\/[^\s"'<>]+/giu, "[path]")
    // A page path is the one path whose LAST segment routinely contains
    // spaces, and every rule below stops at whitespace — so "…/pages/Some
    // Private Page.md" used to redact "…/pages/Some" and print the rest.
    // Consume the whole thing, extension included, before anything else runs.
    .replace(
      /(^|[\s("'=])(?:[A-Za-z]:\\|\.{0,2}\/|\.?[\w.@-]+[\/\\])[^:"'<>]*?\.(?:md|markdown|org)\b/giu,
      "$1[path]",
    )
    .replace(/\\\\(?:[^\\\s"'<>]+\\)+[^\\\s"'<>]*/gu, "[path]")
    .replace(/\b[A-Za-z]:\\(?:[^\\\s"'<>]+\\)*[^\\\s"'<>]*/gu, "[path]")
    .replace(/(^|[\s("'=])\/(?:[^/\s"'<>]+\/)*[^/\s"'<>]*/gu, "$1[path]")
    .replace(/(^|[\s("'=])(?:[^:"'<>]+\/)+[^:"'<>]+\.(?:md|markdown|org)(?=:)/giu, "$1[path]")
    .replace(/\b(?:objects|manifests|enrollment|frontier-heads-v1|publication-intents-v1|manifest-recovery-links-v1|manifest-recovery-blobs-v1|pending-publication-v1|temporary|removed|rename-evidence)(?:\/[^\s"'<>:;,()]+)+/giu, "[provider path]")
    // A RELATIVE path is just as private and was matched by nothing above, so
    // it survived to the structural check below and discarded the whole
    // message — a real device was told "The command failed without a safe
    // diagnostic detail" for `.tine-sync/v2/shared/...`. Redact it like any
    // other path instead of throwing the sentence away.
    .replace(/(^|[\s("'=])\.?[\w.@-]+(?:\/[\w.@-]+)+\/?/gu, "$1[path]")
    .replace(/\b(?:https?|ssh):\/\/[^\s"'<>]+/giu, "[link]")
    .replace(/\b(authorization|bearer|token|secret|password)\b(?:\s*[:=]\s*|\s+)[^\s,;]+/giu, "$1 [redacted]")
    .replace(/"[^"]*"|'[^']*'/gu, (quoted) => {
      if (/\[(?:path|link|redacted)\]/u.test(quoted)) return quoted;
      // A refusal SITE is a bare snake_case identifier the backend authored —
      // `ActorRefusedAt("require_pending_publication_absent")`. It is the whole
      // point of attributing refusals, it cannot carry graph text (no spaces,
      // no punctuation, ASCII only), and redacting it made every attributed
      // refusal read the same.
      if (/^["'][a-z][a-z0-9_]*["']$/u.test(quoted)) return quoted;
      return '"[redacted]"';
    })
    // A debug-formatted value (`Os { code: 2, … }`, a `<…>` type name) carries
    // whatever the OS or the type system put inside it. Collapse the group
    // rather than reasoning about its insides.
    .replace(/\{[^{}]*\}/gu, "[details]")
    .replace(/<[^<>]*>/gu, "[details]")
    .replace(/\s+/g, " ")
    .trim();
  // Whatever still carries a structural character is one token we cannot vouch
  // for — so redact THAT TOKEN. Never the sentence. Throwing the line away is
  // how a phone came to be told "The command failed without a safe diagnostic
  // detail" for an ordinary relative path, and then a second time for
  // something else; the stage name in front of the residue is the entire
  // diagnostic value of the message. This is not a weaker boundary: the same
  // sentence carrying no structural character already reached the screen.
  safe = safe
    .split(" ")
    .map((token) =>
      /[{}<>\\/]|[\u0000-\u001f\u007f]/u.test(token) || token.length >= 80
        ? "[redacted]"
        : token,
    )
    .join(" ")
    .replace(/\[redacted\](?: \[redacted\])+/gu, "[redacted]")
    .trim();
  const hasDiagnosticClass = /\b(?:activation|archive|actor|binding|blocked|bridge|close|command|conflict|corrupt|database|denied|dispatch|drain|error|failed|failure|invalid|join|lookup|malformed|managed|materialization|open|operation|parse|parser|permission|projection|provider|read-only|reason|recovery|refused|save|scratch|serialization|setup|source|sqlite|storage|sync|timeout|unavailable|unresponsive)\b/iu.test(safe) || /lease|contended/iu.test(safe);
  if (!safe || !hasDiagnosticClass) {
    return "The command failed without a safe diagnostic detail.";
  }
  const maxLength = 280;
  if (safe.length > maxLength) safe = `${safe.slice(0, maxLength - 1).trimEnd()}…`;
  return safe;
}

export interface ManagedJoinErrorDetail {
  /** Bounded text suitable for the sticky on-device failure toast. */
  visible: string;
  /** Full bounded affected-path list for the user's explicit Copy details action. */
  copy: string;
}

function readableJoinCategory(category: SharedFrontierMismatchCategory): string {
  return {
    kind: "file format",
    preamble: "text before the first block",
    outline: "blocks, content, or order",
    "explicit-ids": "explicit block IDs",
  }[category];
}

function describeJoinPath(entry: SharedFrontierMismatchPath): string {
  const quoted = JSON.stringify(entry.path);
  switch (entry.side) {
    case "local-only":
      return `Only on this device: ${quoted}`;
    case "shared-only":
      return `Only in shared notes: ${quoted}`;
    case "changed":
      return `Changed (${entry.categories.map(readableJoinCategory).join(", ")}): ${quoted}`;
  }
}

/**
 * Keep the general diagnostic/report boundary path-free, but expose the core's
 * bounded typed mismatch records to the user who explicitly attempted the join.
 * Paths are required to reconcile a refusal; Solid renders this as text, and
 * the core already bounds the list to 32 records. Only the typed
 * `SharedFrontierMismatchError` detail is ever shown; no error prose is parsed.
 */
export function managedJoinErrorDetail(error: unknown): ManagedJoinErrorDetail {
  const summary = safeManagedErrorDetail(error);
  if (!(error instanceof SharedFrontierMismatchError) || !error.detail) {
    return { visible: summary, copy: summary };
  }
  const details = error.detail.paths.map(describeJoinPath);
  if (error.detail.omitted > 0) {
    details.push(`${error.detail.omitted} additional affected notes omitted by the backend.`);
  }
  if (details.length === 0) return { visible: summary, copy: summary };

  const visibleLimit = 3;
  const visibleDetails = details.slice(0, visibleLimit);
  const hidden = details.length - visibleDetails.length;
  const visibleSuffix = hidden > 0 ? `; ${hidden} more in Copy details` : "";
  return {
    visible: `${summary} Affected ${details.length === 1 ? "note" : "notes"}: ${visibleDetails.join("; ")}${visibleSuffix}`,
    copy: `${summary}\nAffected notes:\n${details.map((detail) => `- ${detail}`).join("\n")}`,
  };
}
