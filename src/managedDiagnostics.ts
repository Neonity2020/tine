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
    .replace(/\s+/g, " ")
    .trim();
  const hasDiagnosticClass = /\b(?:activation|archive|actor|binding|blocked|bridge|close|command|conflict|corrupt|database|denied|dispatch|drain|error|failed|failure|invalid|join|lookup|malformed|managed|materialization|open|operation|parse|parser|permission|projection|provider|read-only|reason|recovery|refused|save|scratch|serialization|setup|source|sqlite|storage|sync|timeout|unavailable|unresponsive)\b/iu.test(safe) || /lease|contended/iu.test(safe);
  if (
    !safe ||
    !hasDiagnosticClass ||
    /[\u0000-\u001f\u007f]/u.test(safe) ||
    /[{}<>\\/]/u.test(safe) ||
    /\S{80,}/u.test(safe)
  ) {
    return "The command failed without a safe diagnostic detail.";
  }
  const maxLength = 280;
  if (safe.length > maxLength) safe = `${safe.slice(0, maxLength - 1).trimEnd()}…`;
  return safe;
}
