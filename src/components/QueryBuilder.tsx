import {
  For,
  Show,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  createUniqueId,
  onCleanup,
  onMount,
  type JSX,
} from "solid-js";
import { Portal } from "solid-js/web";
import { backend, OperationCancelledError, QueryNotReadyError } from "../backend";
import {
  builderRoot,
  filterLabel,
  removeAt,
} from "../editor/queryBuilder";
import type {
  Anchor,
  Diagnostic,
  Filter,
  ParsedQuery,
  Query,
  QueryPrintDialect,
  RegistrySnapshot,
  RegistryRow,
  Span,
  ViewSettings,
} from "../editor/queryIr";
import {
  QuerySentence,
  QuerySheet,
  countConditions,
  rawLeaves,
  stop,
  type AnchorPrompt,
  type RegistryAccess,
} from "./QuerySheet";
import { sharedQueryResult, sharedQueryScope } from "../queryResultCache";
import { createReadyQueryResource } from "../createReadyQueryResource";
import { runQueryWhenCurrent } from "../queryReadiness";
import { graphBinding } from "../persistence";
import { QueryDisplay } from "./QueryDisplay";
import type { QueryDisplayControl } from "../editor/queryViewProperties";
import { dataRev, graphEpoch, graphMeta, queryBuilderAutoOpen, setQueryBuilderAutoOpen } from "../ui";
import { dismissOnOutsidePointer, registerTransientLayer } from "../transientLayers";

// **The visual query builder: a resting SENTENCE that expands into a SHEET**
// (SPEC §7.2–§7.4).
//
// It used to be a chip bar over a DSL STRING — parse the text, edit a private
// `Clause` tree, print the text back. Both ends of that round trip were a second
// implementation of a language Rust already owns (I-12). Then it was a chip bar
// over the IR. It is now two states: at rest one plain-English line with the
// result count and a ⚙, and while editing a sheet of rows over the same IR.
//
// This file is the HOST. It owns the session, the two graph-level reads, the
// anchor-switch preview and the dismissal layer; `QuerySheet.tsx` owns what the
// sheet draws. The sort/summarize controls and the text pane below are P5's and
// P4's respectively and are unchanged — they simply moved into the sheet's
// footer.

export type { RegistryAccess };

export type QueryBuilderChange =
  | ((next: BuilderSession) => void)
  | ((next: BuilderSession) => Promise<boolean>);

/** One landed registry read, tagged with the graph scope and declaration
 *  revision it answers — and carrying either the snapshot or the terminal
 *  failure that replaced it. Both are scoped, so neither can be published for a
 *  graph or a revision that has since been replaced. */
interface RegistryRead {
  scope: string;
  key: string;
  snapshot: RegistrySnapshot | null;
  failure: Error | null;
}

/** The pair an edit session holds (§4.3.1). `query` carries the anchor, the
 *  filter, the diagnostics and the authored source — including the opaque
 *  options map, which only Rust ever splits or appends. */
export interface BuilderSession {
  query: Query;
  view: ViewSettings;
}

const errorMessage = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

// **`+ sort` and `+ summarize` are gone** (P5B, Q3). They were two pills that
// each stated a FRACTION of one display fact — one sort pair, one aggregate —
// and each rewrote a longer authored list as a one-element one. The shared
// Display panel states all six facts, so the sheet mounts that instead.

// ---------------------------------------------------------------------------
// The text pane (§4.3.1, §7.1)
// ---------------------------------------------------------------------------

/** How long the pane waits after the last keystroke before asking the engine.
 *  §7.1's ~150 ms: long enough that ordinary typing is one parse, short enough
 *  that the rows follow the text rather than trailing it. */
const PANE_DEBOUNCE_MS = 150;

/** The handle the sheet's `⟨advanced⟩` control and its retained rows use to
 *  reach the text. Held by the host, filled in by the pane while it is mounted,
 *  and cleared when it is not — a route that leads nowhere is not offered. */
export interface PaneHandle {
  focus: () => void;
  /** Put the selection on a span of the CURRENT draft, when there is one that
   *  belongs to this exact text. */
  select: (span: Span, forText: string) => void;
}

/** One diagnostic, with the fields §4.3.2 gives it — not a joined string.
 *
 *  `span`, `suggestions` and `disabled` are the difference between "something is
 *  wrong" and "this token, here, and here is what the parser knows instead".
 *  Flattening them into one message was cheap and it is why an unknown property
 *  read the same as a missing bracket. */
interface PaneDiagnostics {
  /** The revision these belong to. A diagnostic never outlives its draft. */
  revision: number;
  /** The exact text they were computed for; a span is only offered while the
   *  textarea still holds this. */
  text: string;
  items: Diagnostic[];
}

/** The query text pane.
 *
 *  **One implementation, two dialects.** A query block edits TQL and the query
 *  workspace edits the OG DSL, because that is the text each of them persists —
 *  but "debounce, parse, keep the last good reading, drop a stale response" is
 *  the same question in both, so it is answered once (I-12). The dialect is an
 *  input, not a second pane.
 *
 *  The contract it implements, from §4.3.1:
 *
 *  - A failed parse **keeps the draft and the last-good session**, shows the
 *    parser's OWN message, and disables save. It never blanks the pane and never
 *    writes to disk. The bar above renders greyed while this holds, so the rows
 *    on screen are visibly "what still ran", not "what you just typed".
 *  - **I-20, tightened.** The pane used to accept any response NEWER than the
 *    last one it settled. That is not enough: with revisions 1 and 2 both in
 *    flight, 1's answer arrives, is newer than the watermark, and lands — so the
 *    rows and the error briefly describe text the user has already replaced, and
 *    a *successful* 1 clears the error 2 is about to raise. A response is now
 *    accepted only when it is the answer to the CURRENT revision, on a session
 *    that is still the pane's, in a pane that has not been disposed. Everything
 *    else is dropped, on the success path and the failure path alike.
 *  - Saving waits for a successful parse of the CURRENT revision, so a pending
 *    or invalid draft disables save. **A save can never write an earlier good
 *    parse**: the last-good reading is remembered for the rows, but the button
 *    is enabled only while the good parse IS the current revision.
 *  - Closing the sheet disposes the pane, and a pending callback that lands
 *    afterwards calls no host prop.
 *  - The pane is not an options editor. */
function QueryTextPane(props: {
  session: () => BuilderSession | undefined;
  dialect: Extract<QueryPrintDialect, "og" | "tql">;
  /** Whether the pane is on screen at all. A resting sentence prints nothing:
   *  the text it would show is an IPC round trip per query block on the page,
   *  spent on bytes nobody is looking at (I-13). */
  visible: () => boolean;
  /** A successful parse of the current revision: the new filter/anchor, ready to
   *  be shown. Carry-forward of the view and the opaque options is the caller's
   *  (`QueryBuilder`'s), because it owns the session. */
  onParsed: (query: Query) => void;
  /** Commit the last-good parse. Enabled only when the current revision parsed. */
  onCommit: (query: Query) => void;
  onStale: (stale: boolean) => void;
  /** Published while mounted so the sheet above can bring the user here. */
  handle?: (handle: PaneHandle | null) => void;
  /** The §7.5 crossing notice, hosted here while the sheet is open (N3). */
  notice?: () => JSX.Element;
  /** The registry's keys, for the honest "what vocabulary exists" hint an
   *  unknown identifier gets. Read from the host's ONE snapshot — the pane
   *  never asks the graph anything, and typing here changes no revision. */
  vocabulary?: () => string[];
}): JSX.Element {
  const [draft, setDraft] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [pending, setPending] = createSignal(false);
  const [good, setGood] = createSignal<Query | null>(null);
  const [diagnostics, setDiagnostics] = createSignal<PaneDiagnostics | null>(null);
  // The edit revision, and the revision whose parse currently holds `good`.
  // Both are SIGNALS because "is the good parse the current one" is what the
  // save button renders from.
  const [revision, setRevision] = createSignal(0);
  const [goodRevision, setGoodRevision] = createSignal<number | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  let textarea: HTMLTextAreaElement | undefined;
  onCleanup(() => {
    clearTimeout(timer);
    // Disposal invalidates every outstanding response. A pane that has been
    // closed must not reach back into a host it no longer belongs to.
    disposed = true;
    setRevision((n) => n + 1);
    props.handle?.(null);
  });

  /** **The gate BOTH landing paths go through (I-20).**
   *
   *  One number answers all three questions the dossier separates, because all
   *  three bump it: an input bumps it, a session replacement bumps it (the reset
   *  effect below), and disposal bumps it. So "is this the answer to what is on
   *  screen right now, in a pane that still exists" is `mine === revision()`
   *  with `disposed` as the belt.
   *
   *  Note the difference from what this replaced. The old test was `mine >
   *  settled` — newer than the last response we ACCEPTED — which lets revision 1
   *  land while revision 2 is still in flight. */
  const accepts = (mine: number) => !disposed && mine === revision();

  // The session's own text is PRINTED BY RUST. The pane never renders a query it
  // spelled itself — that was the twin this packet removed. The printer's answer
  // carries the session it was computed for, so a print that lands after the
  // block changed underneath cannot be shown as that block's text.
  const [printed] = createResource(
    () => (props.visible() ? props.session() : undefined),
    async (session) => {
      const key = { query: session.query, view: session.view };
      try {
        return {
          ...key,
          text: await backend().printQuery(session.query, session.view, props.dialect),
          refusal: null as string | null,
        };
      } catch (error) {
        return { ...key, text: null as string | null, refusal: errorMessage(error) };
      }
    },
  );
  /** The printed text is shown only for the pair it was printed FROM. Solid's
   *  resource already discards an out-of-order response; this is the other half
   *  — a response that arrived in order but is now about a previous reading is
   *  not this block's text either. */
  const printedNow = () => {
    const landed = printed.latest;
    const current = props.session();
    if (!landed || !current) return undefined;
    return landed.query === current.query && landed.view === current.view ? landed : undefined;
  };

  // A session arriving from outside the pane (a chip edit, a save landing, a
  // different block) invalidates every outstanding response and the draft with
  // it (§4.3.1: "switching host block or closing the session invalidates
  // outstanding responses").
  createEffect(() => {
    props.session();
    setRevision((n) => n + 1);
    setGoodRevision(null);
    clearTimeout(timer);
    setDraft(null);
    setError(null);
    setDiagnostics(null);
    setPending(false);
    setGood(null);
    props.onStale(false);
  });

  const text = () => draft() ?? printedNow()?.text ?? "";
  const refusal = () => (draft() === null ? printedNow()?.refusal ?? null : null);

  const run = async (source: string, mine: number) => {
    let parsed: ParsedQuery;
    try {
      // RET2-Direct: `query_parse` reads the property registry SQL-only and can
      // report typed readiness. The pane's own revision gate IS this read's
      // cancellation identity, so the shared readiness owner reuses it rather
      // than the pane inventing a second policy.
      parsed = await runQueryWhenCurrent(
        () => backend().parseQuery(source, props.dialect),
        () => accepts(mine),
      );
    } catch (error) {
      // The rejection path takes the SAME gate as the success path. A failure
      // for text the user has already replaced is as wrong to render as a
      // success for it — more so, because it reads as "your current text is
      // broken" about text nobody has judged yet.
      if (!accepts(mine)) return;
      setPending(false);
      setError(errorMessage(error));
      setDiagnostics(null);
      props.onStale(true);
      return;
    }
    if (!accepts(mine)) return;
    setPending(false);
    // A diagnostic inside an `off` subtree carries `disabled` and does not
    // invalidate (§3.5) — a parse with only disabled diagnostics is successful
    // and saveable. The disabled ones are still SHOWN; they are the greyed rows'
    // explanation, and a "disabled-only" diagnostic list is a valid state.
    const all = parsed.query.diagnostics ?? [];
    setDiagnostics(all.length ? { revision: mine, text: source, items: all } : null);
    const blocking = all.filter((d) => !d.disabled);
    if (blocking.length) {
      setError(blocking.map((d) => d.message).join(" · "));
      props.onStale(true);
      return;
    }
    setError(null);
    setGood(parsed.query);
    setGoodRevision(mine);
    props.onStale(false);
    props.onParsed(parsed.query);
  };

  const onInput = (next: string) => {
    setDraft(next);
    const mine = revision() + 1;
    setRevision(mine);
    setGoodRevision(null);
    setDiagnostics(null);
    clearTimeout(timer);
    // The pane is not an options editor (§4.3.1). TQL has no braces and the OG
    // DSL's form never ends in one, so a trailing `}` is an options map that was
    // pasted here — which is a different control, not a parse error. This makes
    // no claim about WHERE the map starts; splitting one is Rust's job and only
    // Rust's.
    if (next.trim().endsWith("}")) {
      setPending(false);
      setError("The options map (title, collapsed) is edited with the title and Display controls, not here.");
      props.onStale(true);
      return;
    }
    setPending(true);
    timer = setTimeout(() => void run(next, mine), PANE_DEBOUNCE_MS);
  };

  /** Only a good parse OF THE CURRENT REVISION is saveable. */
  const savable = () =>
    draft() !== null && !pending() && !error() && good() !== null && goodRevision() === revision();

  // **Spans are already UTF-16 code units** (`queryIr.ts`: converted once, at the
  // Rust boundary, precisely because the consumer is JavaScript). So locating a
  // token is `setSelectionRange` and nothing else — converting again would move
  // every offset past the first non-ASCII character in the draft.
  //
  // A span is offered ONLY for the text it was computed from. A stale span
  // pointing into a different draft selects the wrong words with complete
  // confidence, which is worse than not offering the jump at all.
  const spanFor = (diagnostic: Diagnostic): Span | null => {
    const current = diagnostics();
    const span = diagnostic.span;
    if (!current || !span) return null;
    if (current.revision !== revision() || current.text !== text()) return null;
    if (span.start < 0 || span.end < span.start || span.end > current.text.length) return null;
    return span;
  };
  const select = (span: Span, forText: string) => {
    if (!textarea || textarea.value !== forText) return;
    textarea.focus();
    textarea.setSelectionRange(span.start, span.end);
  };
  onMount(() => props.handle?.({ focus: () => textarea?.focus(), select }));

  /** The registry keys that CONTAIN the token an unknown identifier names.
   *
   *  Labelled as what it is — the property vocabulary of this graph — because
   *  that is the only scope these rows cover. It is a substring scan of a list
   *  the host already holds, not a resolver: matching a name against the
   *  language's identifiers is the parser's job, and Rust's own `suggestions`
   *  are rendered first and separately. */
  const vocabularyNear = (diagnostic: Diagnostic): string[] => {
    if (diagnostic.kind !== "unknown_ident") return [];
    const span = spanFor(diagnostic);
    const token = span ? text().slice(span.start, span.end).replace(/["']/g, "").trim() : "";
    if (token.length < 2) return [];
    const needle = token.toLowerCase();
    return (props.vocabulary?.() ?? [])
      .filter((key) => key.toLowerCase().includes(needle) && key.toLowerCase() !== needle)
      .slice(0, 4);
  };

  return (
    <div class="query-text-pane">
      <label class="query-text-pane-label" for={undefined}>
        {props.dialect === "tql" ? "Query text" : "Raw query DSL"}
      </label>
      <textarea
        ref={textarea}
        class="qb-input query-text-pane-input"
        classList={{ "query-text-pane-invalid": !!error() }}
        rows={3}
        spellcheck={false}
        aria-label={props.dialect === "tql" ? "Query text (TQL)" : "Query expression"}
        aria-invalid={error() ? "true" : undefined}
        value={text()}
        disabled={!!refusal()}
        onInput={(event) => onInput(event.currentTarget.value)}
      />
      <div class="query-text-pane-status">
        <Show when={refusal()}>
          {(message) => <span class="query-text-pane-error" role="alert">{message()}</span>}
        </Show>
        {/* The parser's OWN message, never a catch-all (I-9). The rows above
            stay on screen and greyed; they are the last reading that ran.
            It is shown HERE only when there is no structured list below to
            carry it — a refusal, or a rejection with no diagnostics. Printing
            it in both places said the same sentence twice and made a
            two-problem query look like a three-problem one. */}
        <Show when={error() && !diagnostics()?.items.length}>
          <span class="query-text-pane-error" role="alert">{error()}</span>
        </Show>
        <Show when={pending() && !error()}>
          <span class="query-text-pane-pending">Checking…</span>
        </Show>
        <button
          type="button"
          class="qb-commit query-text-pane-save"
          disabled={!savable()}
          onClick={() => { const q = good(); if (q) props.onCommit(q); }}
        >
          Save query text
        </button>
      </div>
      {/* The structured half of §4.3.2: the kind, the span, the parser's own
          alternatives, and whether the diagnostic is inside an `off` subtree. */}
      <Show when={diagnostics()?.items.length}>
        <ul class="query-text-pane-diagnostics">
          <For each={diagnostics()!.items}>
            {(diagnostic) => (
              <li
                class="query-text-pane-diagnostic"
                classList={{ "is-disabled": diagnostic.disabled === true }}
                data-kind={diagnostic.kind}
              >
                <span class="query-text-pane-diagnostic-message">{diagnostic.message}</span>
                <Show when={diagnostic.disabled}>
                  <span class="query-text-pane-diagnostic-off"> (in a disabled condition — the query still runs)</span>
                </Show>
                <Show when={spanFor(diagnostic)}>
                  {(span) => (
                    <button
                      type="button"
                      class="query-text-pane-locate"
                      onClick={() => select(span(), diagnostics()!.text)}
                    >
                      Show me
                    </button>
                  )}
                </Show>
                <Show when={diagnostic.suggestions?.length}>
                  <span class="query-text-pane-diagnostic-alts">
                    <For each={diagnostic.suggestions!.slice(0, 4)}>
                      {(alternative, index) => (
                        <>
                          <Show when={index() > 0}>, </Show>
                          <code>{alternative}</code>
                        </>
                      )}
                    </For>
                  </span>
                </Show>
                <Show when={vocabularyNear(diagnostic).length}>
                  <span class="query-text-pane-diagnostic-alts">
                    Properties in this graph: <For each={vocabularyNear(diagnostic)}>
                      {(key, index) => (
                        <>
                          <Show when={index() > 0}>, </Show>
                          <code>{key}</code>
                        </>
                      )}
                    </For>
                  </span>
                </Show>
                {/* A syntax error the parser had no alternative for still gets a
                    route: the vocabulary above and the Guide's TQL reference. */}
                <Show when={diagnostic.kind === "syntax" && !diagnostic.suggestions?.length}>
                  <span class="query-text-pane-diagnostic-alts">
                    The fields and properties this graph has are in the picker above; the
                    query language is in the Guide under <em>Find and revisit → Query text (TQL)</em>.
                  </span>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Show>
      {/* §7.5: one notice, hosted here while the sheet is open. It is the same
          component, with the same state, that `Macro.tsx` draws inline while the
          sheet is closed — never a second copy. */}
      <Show when={props.notice}>{(render) => render()()}</Show>
    </div>
  );
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The one registry read, shared by every open builder (§6.4, I-13, N4)
// ---------------------------------------------------------------------------

/** The graph-scoped declaration revision, bumped when a `tine.type` is written.
 *
 *  **One counter, module level, reset by the graph.** A declaration written in
 *  one sheet is a fact about the GRAPH, so every open builder must see it — a
 *  per-builder counter meant the sheet next to the one that declared kept the
 *  old badge until it was closed and reopened. And it is reset rather than
 *  remembered per graph: keeping a map would be a lifetime-growing store of
 *  numbers whose only purpose is to differ from the last one (D-5).
 *
 *  Note what it is NOT: it is not "the sheet opened". Opening a sheet makes the
 *  resource key LIVE, which is a different event; conflating them is what made
 *  the old `registryRequests` counter fire a fresh read per mount. */
const [declarationBump, setDeclarationBump] = createSignal({ epoch: 0, revision: 0 });

/** Re-read the registry for THIS graph, in every open builder. Called after a
 *  declaration lands, and on nothing else. */
export function requestQueryRegistryRefresh(): void {
  const epoch = graphEpoch();
  setDeclarationBump((prior) =>
    prior.epoch === epoch ? { epoch, revision: prior.revision + 1 } : { epoch, revision: 1 },
  );
}

/** The current graph's declaration revision. Reading it is pure: a bump made
 *  under a different graph reads back as 0 rather than leaking that graph's
 *  count into this one's request key. */
function declarationRevision(): number {
  const bump = declarationBump();
  return bump.epoch === graphEpoch() ? bump.revision : 0;
}

/** Tests mount several graphs in one process; the counter is module state. */
export function resetQueryRegistryRevisionForTests(): void {
  setDeclarationBump({ epoch: -1, revision: 0 });
}

/** **One registry read, however many surfaces ask for it** (D-14, I-12).
 *
 *  The builder's sheet, the inline Display panel and each mixed-result section
 *  all need the observed property registry to build a field vocabulary. They
 *  differ only in WHEN they are asking — which is the `active` gate — so the
 *  read, its readiness retry, its terminal-failure capture and its scoping are
 *  stated once here rather than once per surface. A second owner would grow a
 *  second retry policy and a second answer to "does this graph have that
 *  property".
 *
 *  With no surface open there is deliberately no read at all (I-13). */
export function createQueryRegistryAccess(active: () => boolean): RegistryAccess {
  const registryScope = () => sharedQueryScope(graphMeta()?.root, graphEpoch(), graphBinding());
  const registryKey = () =>
    active() ? `${dataRev()}\0${declarationRevision()}` : undefined;
  //  - **Readiness is the shared owner's, not this component's (RET2-Direct).**
  //    `query_registry` is SQL-only on both backends now and reports typed
  //    `query-not-ready` while the index is indexing, recovering or applying a
  //    save, instead of answering from a debounced document walk. The retry and
  //    its binding/epoch cancellation are `createReadyQueryResource`'s, exactly
  //    as for `query_run`; nothing mode-specific is decided here.
  //  - **A read that FAILS is an answer the sheet has to give (RET2-UI).**
  //    A terminal `query-unavailable` used to reject the resource, and the two
  //    things that happen next are both wrong: `latest` RETHROWS a rejected
  //    resource, so the field picker threw out of its own render and could not
  //    be opened at all, and rows stayed `undefined`, which every consumer
  //    reads as "still reading" — an indexing line that never ends. So a
  //    terminal failure is captured as a VALUE here, which is what stops the
  //    automatic retry, and is surfaced through `failure` below.
  //
  //    Readiness and cancellation are deliberately NOT captured. Readiness is
  //    the shared owner's to retry (`runQueryWhenReady`), and a cancellation is
  //    the ABSENCE of an answer for a superseded request — turning either into
  //    a value would invent a second retry policy or publish a supersede as a
  //    result.
  const [registrySnapshot] = createReadyQueryResource(
    () => {
      const key = registryKey();
      return key === undefined ? undefined : { scope: registryScope(), key };
    },
    async (request): Promise<RegistryRead> => {
      try {
        return {
          scope: request.scope,
          key: request.key,
          snapshot: await sharedQueryResult(
            request.scope,
            `query-registry\0${request.key}`,
            () => backend().queryRegistry(),
          ),
          failure: null,
        };
      } catch (error) {
      if (error instanceof QueryNotReadyError || error instanceof OperationCancelledError) throw error;
      return {
        scope: request.scope,
        key: request.key,
        snapshot: null,
        failure: error instanceof Error ? error : new Error(errorMessage(error)),
      };
    }
  },
  );
  // A type declaration changes operator semantics: an answer is usable only for
  // the current graph AND revision, including while a refresh is pending — and
  // that is as true of a failure as of a snapshot, so a stale graph's error can
  // no more be shown than its rows can.
  const registryRead = (): RegistryRead | undefined => {
    // Reading `latest` on an errored resource RETHROWS, which is how a failed
    // read used to take the field picker's own render with it. Terminal
    // failures are values above, so the only error left is a superseded
    // request's cancellation — and both binding bumps (`onGraphRebound`, and
    // `resetSaveState` behind `graphTransitioning`) re-key this source in the
    // same synchronous update, so Solid discards that rejection rather than
    // storing it. This is the net under that, never a state to render: a
    // cancellation is the absence of an answer, not one.
    if (registrySnapshot.error !== undefined) return undefined;
    const landed = registrySnapshot.latest;
    return landed && landed.scope === registryScope() && landed.key === registryKey()
    ? landed : undefined;
  };
  const registryRows = () => registryRead()?.snapshot?.rows;
  const registryFailure = () => registryRead()?.failure ?? null;
  const registry: RegistryAccess = {
    rows: registryRows,
    // **Undefined rows have three different meanings, and the UI must not read
    // the wrong one.** With no sheet open there is deliberately no read at all
    // (I-13), so `undefined` is "nobody asked". With a sheet open it is either
    // "the answer for THIS graph and THIS declaration revision has not landed"
    // or "the read for it failed" — and a surface that treats either as a
    // known-empty graph shows a graph with no properties, or coerces a freshly
    // declared key as text.
    pending: () => registryKey() !== undefined && registryRows() === undefined
      && registryFailure() === null,
    failure: registryFailure,
    unavailable: () => registryKey() !== undefined && registryRows() === undefined,
    request: requestQueryRegistryRefresh,
    // Explicit retry is the SAME owner as the declaration refresh, not a second
    // one: it re-keys the one shared read, so every open builder recovers from
    // one request and no builder grows a backoff or a cache of its own.
    retry: requestQueryRegistryRefresh,
  };
  return registry;
}

/** Deepest-and-rightmost first, so removing several leaves in one pass never
 *  invalidates a `loc` that has not been used yet. */
function compareLocsDescending(a: number[], b: number[]): number {
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const left = a[i] ?? -1;
    const right = b[i] ?? -1;
    if (left !== right) return right - left;
  }
  return 0;
}

/** The four "Try:" suggestions of the empty state (§7.3).
 *
 *  **SPEC §7.3 is the authority, not the design pass:** the four are the most
 *  frequent PROPERTY keys the registry holds. The design pass's marker / page /
 *  date mix has no frequency source in the engine — the registry holds property
 *  rows only, and `referencedPageNames` carries no counts — and a TypeScript
 *  walk of the graph to invent one is exactly the whole-graph-work-per-render
 *  shape I-13 forbids and D-14 says not to build a twin of. Fewer than four
 *  rows shows what exists; an empty registry shows no line at all. */
export function suggestedKeys(rows: RegistryRow[] | undefined): string[] {
  if (!rows?.length) return [];
  return [...rows]
    .sort((a, b) => {
      const byCount =
        b.count_blocks + b.count_pages - (a.count_blocks + a.count_pages);
      return byCount !== 0 ? byCount : a.normalized_name.localeCompare(b.normalized_name);
    })
    .slice(0, 4)
    .map((row) => row.normalized_name);
}

export function QueryBuilder(props: {
  /** The persisted reading of the query. `undefined` while the engine has not
   *  answered yet — the builder renders nothing rather than an empty query it
   *  would then be able to save over the author's text. */
  session: () => BuilderSession | undefined;
  /** Persist an edit. Row edits call this immediately (each one is a complete,
   *  valid IR); the pane calls it only when the user saves a parse. */
  onChange: QueryBuilderChange;
  /** The text pane's language: `tql` for a query block, `og` for the workspace,
   *  which materializes OG text. */
  paneDialect?: Extract<QueryPrintDialect, "og" | "tql">;
  /** The §7.5 crossing notice, drawn inside the pane while the sheet is open.
   *  The HOST owns the notice's state and its undo gating (`Macro.tsx`); this is
   *  a slot, so the one notice MOVES between hosts rather than a second one
   *  appearing. It is a render function, not an element: a prop whose value is
   *  an element gets instantiated once per read, and a `<Show>` reads its
   *  condition and its child separately — which is two notices, one of them
   *  detached and stealing the focus grab from the one on screen. */
  notice?: () => JSX.Element;
  /** Told whether the sheet is open, so the host can take the notice back when
   *  it closes. */
  onOpenChange?: (open: boolean) => void;
  /** The workspace's permanently expanded sheet (§7.2). Inline, not portalled,
   *  and not a dismissable layer of its own. */
  sheetAlwaysOpen?: boolean;
  /** The live result count, rendered beside the sentence (§7.2). */
  total?: JSX.Element;
  /** The pane's text no longer parses, so the rows on screen are the LAST
   *  reading that ran. The host greys them; the builder cannot, because the
   *  results are not its children. */
  onStale?: (stale: boolean) => void;
  blockId?: string;
  parentTransientId?: string;
  /** **The inline Display panel, off by default** (P5B).
   *
   *  A host that opts in gets one control for all six display facts and loses
   *  the two that could only state a fraction of them: `+ sort` wrote one sort
   *  pair and `+ summarize` one aggregate, and both rewrote a longer list as a
   *  one-element one. Two controls writing the same keys with different ideas of
   *  how many entries there are is exactly the disagreement this replaces — so
   *  they are removed here rather than left beside it.
   *
   *  It is opt-in because the panel edits a BLOCK's `tine.*` properties, and a
   *  host with no block (the workspace draft) or one whose display is being
   *  presented some other way has nothing for it to write. */
  inlineDisplay?: boolean;
  /** The block's formula field names, for the Display panel's grouping list. */
  displayFormulas?: () => readonly string[];
  /** The one writer an inline display change goes through. Required when
   *  `inlineDisplay` is set; without it there is nothing to save to. */
  display?: () => QueryDisplayControl | undefined;
}): JSX.Element {
  // The pane's last-good parse, not yet saved. `null` = the builder shows the
  // persisted reading. This is what makes "the rows follow the text you typed"
  // and "nothing reaches disk until you save it" both true (§4.3.1).
  const [paneQuery, setPaneQuery] = createSignal<Query | null>(null);
  const [stale, setStale] = createSignal(false);
  const [open, setOpen] = createSignal(false);
  const [openMenu, setOpenMenu] = createSignal<string | null>(null);
  const [anchorPrompt, setAnchorPrompt] = createSignal<AnchorPrompt | null>(null);
  const [previewError, setPreviewError] = createSignal<string | null>(null);

  // I-20: anchor previews obey the same current-revision rule as text parses.
  // Selecting the original anchor or closing the sheet cancels pending work.
  let anchorRevision = 0;
  const invalidateAnchorPreview = () => {
    anchorRevision += 1;
    setAnchorPrompt(null);
  };
  onCleanup(invalidateAnchorPreview);

  createEffect(() => {
    props.session();
    setPaneQuery(null);
    invalidateAnchorPreview();
    setPreviewError(null);
  });

  const session = createMemo<BuilderSession | undefined>(() => {
    const persisted = props.session();
    if (!persisted) return undefined;
    const pane = paneQuery();
    return pane ? { query: pane, view: persisted.view } : persisted;
  });
  // The sheet always edits an `and`/`or` root, so "+ add condition" has
  // somewhere to add. A single-child `and` prints back as the bare child.
  const root = createMemo(() => builderRoot(session()?.query.filter ?? { kind: "and", items: [] }));
  const view = () => session()?.view ?? {};

  const [displayOpen, setDisplayOpen] = createSignal(false);
  const sheetOpen = () => !!props.sheetAlwaysOpen || open();
  createEffect(() => { if (!sheetOpen()) invalidateAnchorPreview(); });
  // The host needs to know, because the §7.5 notice lives inline under the block
  // while the sheet is shut and inside the pane while it is open (N3).
  createEffect(() => props.onOpenChange?.(sheetOpen()));

  // **The ONE graph-level read the builder makes (§6.4, K20, I-13, N4).**
  //
  // It used to make two: `query_facets(false)` — a whole-graph key/value scan
  // with no counts and no types — for the property chooser, and
  // `query_registry` for the type badge. The chooser is the registry's now, so
  // the facets read is gone; `queryFacets(true)` stays, because raw-block
  // autocomplete asks a genuinely different question.
  //
  // Four properties, and each of them is a defect this replaced:
  //
  //  - **Zero work at rest.** The key is `undefined` while neither sheet nor Display is open, so
  //    a page of resting sentences issues nothing at all.
  //  - **One request per (graph, dataRev, declaration), across builders.**
  //    `sharedQueryResult` collapses identical in-flight and resolved work,
  //    exactly as the page-tag query does. Five open sheets are one call.
  //  - **Freshness the removed facets path used to provide.** The facets read
  //    was keyed on `dataRev`; the registry was not, so a sheet left open across
  //    a save kept a stale vocabulary. It is keyed on `dataRev` now — through
  //    the SAME save-batch signal, not a timer of its own — and on the shared
  //    declaration revision, so declaring a type refreshes every open builder
  //    and cannot be served from an already-resolved stale entry.
  //  - **Rows never cross a graph.** A reply for the previous graph may still
  //    settle; it can never be published, because what is exposed is gated on
  //    the scope the rows were fetched under.
  const registry = createQueryRegistryAccess(() => sheetOpen() || displayOpen());
  const suggestions = createMemo(() => suggestedKeys(registry.rows()));
  const vocabulary = () => (registry.rows() ?? []).map((row) => row.normalized_name);

  // Open the sheet with the field chooser focused when this block was just
  // created via `/query` — consume the one-shot flag so only this block does.
  const autoOpen = !!props.blockId && queryBuilderAutoOpen() === props.blockId;
  if (autoOpen) {
    setQueryBuilderAutoOpen(null);
    setOpen(true);
  }

  /** A row edit: a new filter over the CURRENT reading, saved immediately. */
  const apply = (next: Filter) => {
    const current = session();
    if (!current) return;
    invalidateAnchorPreview();
    const outcome = props.onChange({ query: { ...current.query, filter: next }, view: current.view });
    setOpenMenu(null);
    return outcome;
  };
  const applyView = (next: ViewSettings) => {
    const current = session();
    if (!current) return;
    props.onChange({ query: current.query, view: next });
  };
  /** §4.3.1 carry-forward: a parse replaces only the filter, the anchor and the
   *  diagnostics. The session's view and its OPAQUE options survive — an absent
   *  map in pane text never means "delete the title". */
  const carryForward = (parsed: Query): Query => {
    const current = props.session();
    const options = current && current.query.source.kind !== "builder"
      ? current.query.source.og_options ?? ""
      : "";
    return {
      anchor: parsed.anchor,
      filter: parsed.filter,
      diagnostics: parsed.diagnostics,
      source: parsed.source.kind === "builder"
        ? parsed.source
        : { ...parsed.source, og_options: options },
    };
  };
  const commitQuery = (query: Query) => {
    const current = session();
    if (!current) return;
    props.onChange({ query, view: current.view });
  };

  /**
   * **Switching the anchor re-validates through the ENGINE (§7.4, §3.5, D-14).**
   *
   * The frontend has no "does this leaf apply to this row" oracle and must not
   * grow one — that is the twin this campaign removed. So the preview is the
   * engine answering: print the query under the new anchor, parse it back, and
   * read the `not_applicable` diagnostics the lowering raised. The leaves that
   * do not apply come back RETAINED (`Raw(NotApplicable)`, P3's Rust half), so
   * "keep anyway" keeps the author's condition rather than a memory of it.
   */
  const switchAnchor = async (anchor: Anchor) => {
    const current = session();
    invalidateAnchorPreview();
    setPreviewError(null);
    if (!current || current.query.anchor === anchor) return;
    const mine = anchorRevision;
    const next: Query = { ...current.query, anchor };
    let parsed: ParsedQuery;
    try {
      const text = await backend().printQuery(next, current.view, "tql");
      parsed = await runQueryWhenCurrent(
        () => backend().parseQuery(text, "tql"),
        () => mine === anchorRevision,
      );
    } catch (error) {
      if (mine !== anchorRevision) return;
      setPreviewError(errorMessage(error));
      return;
    }
    if (mine !== anchorRevision) return;
    const carried = carryForward(parsed.query);
    const notApplicable = (carried.diagnostics ?? []).filter((d) => d.kind === "not_applicable");
    const live = notApplicable.filter((d) => d.disabled !== true);
    if (live.length === 0) {
      // Nothing objected: commit the switch itself, not the round trip, so an
      // untouched query keeps the tree it already had.
      if (notApplicable.length === 0) return commitQuery(next);
      // Only DISABLED objections: the grey Off rows were already off, and a
      // prompt about conditions that are not running would be noise.
      return commitQuery(carried);
    }
    const rootFilter = builderRoot(carried.filter);
    const sites = rawLeaves(rootFilter).filter(
      (site) => site.leaf.diagnostic_kind === "not_applicable" && !site.disabled,
    );
    setAnchorPrompt({
      anchor,
      count: live.length,
      total: countConditions(rootFilter),
      names: sites.map((site) => filterLabel(site.leaf)),
      onRemove: () => {
        let filter = rootFilter;
        for (const loc of sites.map((site) => site.loc).sort(compareLocsDescending)) {
          filter = loc.length === 0 ? { kind: "and", items: [] } : removeAt(filter, loc);
        }
        setAnchorPrompt(null);
        commitQuery({ ...carried, filter });
      },
      // The leaves stay as red rows with their message; the query is invalid
      // and returns nothing until they are edited, removed or disabled (§3.5).
      onKeep: () => {
        setAnchorPrompt(null);
        commitQuery(carried);
      },
      onCancel: () => setAnchorPrompt(null),
    });
  };

  // -- the sheet's layer and its position ------------------------------------

  // **A ROOT transient layer whose id is unique per MOUNT, not per block.**
  // `transientLayers` keys by id and a later registration REPLACES an earlier
  // one, and the same block can be mounted twice (main pane + split pane or
  // sidebar) — so a block-derived id would silently unregister the first
  // sheet, and Escape would close the wrong one. The block id is a readable
  // prefix, nothing more.
  const sheetLayerId = `query-sheet:${props.blockId ?? "workspace"}:${createUniqueId()}`;
  let sheetEl: HTMLDivElement | undefined;
  let sentenceEl: HTMLSpanElement | undefined;

  const dismissable = () => open() && !props.sheetAlwaysOpen;
  createEffect(() => {
    if (!dismissable()) return;
    const unregister = registerTransientLayer({
      id: sheetLayerId,
      root: () => sheetEl ?? null,
      trigger: () => sentenceEl ?? null,
      dismiss: () => {
        setOpen(false);
        return true;
      },
    });
    onCleanup(unregister);
  });
  dismissOnOutsidePointer({
    open: dismissable,
    // A press while one of the sheet's own popovers is open belongs to that
    // popover's rung: closing the sheet under it would collapse two levels of
    // the ladder on one press. Treating the whole document as "inside" for that
    // press is what holds the sheet still while the popover's own layer takes
    // it; the NEXT press, with nothing open, closes the sheet.
    //
    // `openMenu()` covers the rows, the anchor and the add chooser. The footer's
    // sort and summarize popovers are P5's and keep their own open state, so the
    // question is asked of the DOM instead of duplicating their signals: a panel
    // rendered inside the sheet right now IS an open popover.
    //
    // The inline Display panel is the one popover on this rung that is NOT
    // rendered inside the sheet: its trigger is in the footer, but it is
    // portalled to <body> because `.query-block`'s `translateZ(0)` would trap a
    // fixed child. So the same DOM question has to be asked of the document,
    // narrowed to the panel that named THIS sheet as its parent layer. Without
    // it, a press on one of the panel's own controls read as a press outside
    // the sheet: the sheet closed, the panel went with it, and the control's
    // own click never landed — every setting in the panel was unreachable with
    // a real pointer.
    inside: () =>
      openMenu() !== null
      || sheetEl?.querySelector(".qs-menu, .qb-picker, .qb-menu")
      || document.querySelector(`[data-transient-parent="${sheetLayerId}"]`)
        ? [document.body]
        : [sheetEl ?? null, sentenceEl ?? null],
    dismiss: () => setOpen(false),
  });

  // The sheet is portalled and positioned from the sentence's rect on wide
  // screens. **Why a portal:** `.query-block` carries `transform: translateZ(0)`
  // plus `position: relative; z-index: 1` (the GH #64 / WebKitGTK flicker fix —
  // read the comment in app.css), which makes it the containing block for any
  // `fixed` descendant, so an in-block sheet could never be viewport-fixed and
  // the narrow bottom sheet would be trapped inside the block.
  const [rect, setRect] = createSignal<{ top: number; left: number; width: number } | null>(null);
  const measure = () => {
    const element = sentenceEl;
    if (!element) return;
    const box = element.getBoundingClientRect();
    setRect({ top: box.bottom, left: box.left, width: Math.max(box.width, 320) });
  };
  createEffect(() => {
    if (!open() || props.sheetAlwaysOpen) return;
    measure();
    if (typeof window === "undefined") return;
    window.addEventListener("scroll", measure, true);
    window.addEventListener("resize", measure);
    onCleanup(() => {
      window.removeEventListener("scroll", measure, true);
      window.removeEventListener("resize", measure);
    });
  });

  // The pane publishes a handle while it is mounted, so the sheet's
  // `⟨advanced⟩` control and its retained rows have somewhere to send the user.
  // It is cleared on unmount: a route that leads nowhere is not offered.
  const [paneHandle, setPaneHandle] = createSignal<PaneHandle | null>(null);

  /** **The ONE display control this sheet offers** (P5B, Q3).
   *
   *  A host that owns the block's `tine.*` writer hands its own control in
   *  (`inlineDisplay`); every other host gets the builder's session writer,
   *  which is the same `ViewSettings` under a different owner. It is one
   *  control either way — `+ sort` and `+ summarize` used to sit here instead,
   *  and each could state only a FRACTION of one fact (one sort pair, one
   *  aggregate), rewriting a longer list as a one-element one. Two controls
   *  writing the same keys with different ideas of how many entries there are
   *  is exactly the disagreement this replaces. */
  const inlineDisplay = (): QueryDisplayControl | undefined =>
    props.inlineDisplay ? props.display?.() : (session() ? { view: view(), apply: applyView } : undefined);
  const displayControl = (parentTransientId?: string) => (
    <Show when={inlineDisplay()}>
      {(control) => (
        <QueryDisplay
          control={control()}
          registry={registry}
          formulas={props.displayFormulas}
          parentTransientId={parentTransientId}
          onOpenChange={setDisplayOpen}
        />
      )}
    </Show>
  );
  const footer = () => (
    <>
      {displayControl(sheetLayerId)}
      {/* **Visible and editable, always, inside an open sheet (§7.5).** It was a
          collapsed `<details>`, which meant the one control that can express
          everything the rows cannot was the one control a user had to know to
          look for. The sheet is what gates the cost: a RESTING sentence still
          mounts no pane and prints nothing. */}
      <QueryTextPane
        session={props.session}
        dialect={props.paneDialect ?? "tql"}
        visible={sheetOpen}
        vocabulary={vocabulary}
        notice={props.notice}
        handle={setPaneHandle}
        onParsed={(parsed) => setPaneQuery(carryForward(parsed))}
        onCommit={(parsed) => {
          const current = props.session();
          if (!current) return;
          props.onChange({ query: carryForward(parsed), view: current.view });
        }}
        onStale={(value) => {
          setStale(value);
          props.onStale?.(value);
        }}
      />
    </>
  );

  const sheet = (extraRef?: (element: HTMLDivElement) => void) => (
    <QuerySheet
      anchor={() => session()?.query.anchor ?? "block"}
      onAnchor={(anchor) => void switchAnchor(anchor)}
      anchorPrompt={anchorPrompt}
      root={root}
      query={() => session()?.query}
      apply={apply}
      registry={registry}
      onEditText={() => paneHandle()?.focus()}
      suggestions={suggestions}
      openMenu={openMenu}
      setOpenMenu={setOpenMenu}
      // In the workspace the sheet is not a layer of its own: its menus parent
      // to the Advanced modal exactly as the chip popovers did, so that
      // dialog's local Tab trap keeps containing them.
      layerId={props.sheetAlwaysOpen ? props.parentTransientId : sheetLayerId}
      autoOpenChooser={autoOpen}
      footer={footer()}
      stale={stale()}
      sheetRef={(element) => {
        sheetEl = element;
        extraRef?.(element);
      }}
    />
  );

  return (
    <Show when={session()}>
      {(current) => (
        <div class="qs-builder">
          <Show when={!props.sheetAlwaysOpen}>
            <QuerySentence
              query={current().query}
              total={props.total}
              open={open()}
              onOpen={() => setOpen(!open())}
              sentenceRef={(element) => {
                sentenceEl = element;
              }}
            />
          </Show>
          <Show when={!sheetOpen()}>{displayControl(props.parentTransientId)}</Show>
          <Show when={previewError()}>
            {(message) => (
              <div class="qs-preview-error" role="alert">
                The anchor wasn't changed: {message()}
              </div>
            )}
          </Show>
          <Show when={props.sheetAlwaysOpen}>{sheet()}</Show>
          <Show when={open() && !props.sheetAlwaysOpen}>
            <Portal>
              <div
                class="qs-overlay"
                onClick={(e) => {
                  stop(e);
                  setOpen(false);
                }}
              />
              <div
                class="qs-sheet-anchor"
                style={
                  rect()
                    ? {
                        top: `${rect()!.top}px`,
                        left: `${rect()!.left}px`,
                        width: `${rect()!.width}px`,
                      }
                    : undefined
                }
              >
                {sheet()}
              </div>
            </Portal>
          </Show>
        </div>
      )}
    </Show>
  );
}
