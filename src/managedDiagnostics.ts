/**
 * Reduce a native managed-storage failure to one bounded, shareable line.
 *
 * Backend errors can contain private graph paths, quoted page text, URLs,
 * credentials, or an arbitrarily long debug chain. Recovery UI may preserve
 * the causal class, but must never echo those values to the screen or clipboard.
 */
export function safeManagedErrorDetail(error: unknown): string {
  const message = error instanceof Error ? error.message : typeof error === "string" ? error : "";
  const firstLine = message.split(/[\r\n]/, 1)[0]?.trim() ?? "";
  if (!firstLine) return "The command did not provide a safe diagnostic detail.";
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

const CLEAN_JOIN_SUMMARY = "sync join refused: notes not in the shared provider frontier";
const CLEAN_JOIN_DETAIL = "clean join mismatch detail: ";
const QUOTED_RUST_PATH = String.raw`("(?:\\.|[^"\\])*")`;
const SIDE_PATH_RE = new RegExp(
  String.raw`^(local-only|shared-only) path=${QUOTED_RUST_PATH}$`,
  "u",
);
const CHANGED_PATH_RE = new RegExp(
  String.raw`^changed path=${QUOTED_RUST_PATH} categories=([a-z-]+(?:,[a-z-]+)*)$`,
  "u",
);
const OMITTED_RE = /^(\d+) additional mismatches omitted$/u;

function readableJoinCategory(category: string): string {
  return {
    kind: "file format",
    preamble: "text before the first block",
    outline: "blocks, content, or order",
    "explicit-ids": "explicit block IDs",
  }[category] ?? category;
}

/**
 * Keep the general diagnostic/report boundary path-free, but expose the core's
 * narrowly-authored mismatch records to the user who explicitly attempted the
 * join. Paths are required to reconcile a refusal; Solid renders this as text,
 * and the core already bounds the list to 32 records.
 */
export function managedJoinErrorDetail(error: unknown): ManagedJoinErrorDetail {
  const summary = safeManagedErrorDetail(error);
  const message = error instanceof Error ? error.message : typeof error === "string" ? error : "";
  if (!message.split(/[\r\n]/u, 1)[0]?.includes(CLEAN_JOIN_SUMMARY)) {
    return { visible: summary, copy: summary };
  }

  const details: string[] = [];
  for (const rawLine of message.split(/\r?\n/u).slice(1, 35)) {
    if (!rawLine.startsWith(CLEAN_JOIN_DETAIL)) continue;
    const line = rawLine.slice(CLEAN_JOIN_DETAIL.length);
    const side = SIDE_PATH_RE.exec(line);
    if (side) {
      const sideLabel = side[1] === "local-only" ? "Only on this device" : "Only in shared notes";
      details.push(`${sideLabel}: ${side[2]}`);
      continue;
    }
    const changed = CHANGED_PATH_RE.exec(line);
    if (changed) {
      const categories = changed[2]!.split(",").map(readableJoinCategory).join(", ");
      details.push(`Changed (${categories}): ${changed[1]}`);
      continue;
    }
    const omitted = OMITTED_RE.exec(line);
    if (omitted) details.push(`${omitted[1]} additional affected notes omitted by the backend.`);
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
