import { For, Show, Suspense, createEffect, createMemo, createResource, createSignal, createUniqueId, on, onCleanup, onMount, type JSX } from "solid-js";
import { getHomePageSetting, setHomePageSetting } from "../homePage";
import { AboutTab } from "./AboutTab";
import { DiagnosticsTab } from "./DiagnosticsTab";
import {
  settingsOpen,
  closeSettings,
  settingsTabRequest,
  clearSettingsTabRequest,
  setJournalTemplate,
  setGraphTransitioning,
  theme,
  appearancePreference,
  setAppearancePreference,
  workflow,
  changeWorkflow,
  timetrackingEnabled,
  changeTimetrackingEnabled,
  showBrackets,
  changeShowBrackets,
  changePreferredFormat,
  changeJournalTitleFormat,
  graphMeta,
  shortcutOverrides,
  setShortcutOverride,
  resetShortcutOverride,
  accentColor,
  changeAccent,
  wideMode,
  toggleWideMode,
  documentMode,
  toggleDocumentMode,
  docModeEnterForNewBlock,
  changeDocModeEnterForNewBlock,
  logicalOutdenting,
  changeLogicalOutdenting,
  typographyMode,
  setTypographyMode,
  autoPairing,
  setAutoPairing,
  dimInFocus,
  setDimInFocus,
  changeStartOfWeek,
  carryKeepsContext,
  setCarryKeepsContext,
  carryHeader,
  setCarryHeader,
  carryDays,
  setCarryDays,
  showCarryButtons,
  setShowCarryButtons,
  agendaDaysBack,
  setAgendaDaysBack,
  agendaDaysAhead,
  setAgendaDaysAhead,
  pushToast,
  journalConflicts,
  refreshJournalConflicts,
  syncConflicts,
  refreshSyncConflicts,
  conflictQueue,
  type SettingsTabId,
} from "../ui";
import { interfaceZoom, zoomIn, zoomOut, zoomReset } from "../zoom";
import { smoothScrollEnabled, setSmoothScroll } from "../smoothScroll";
import { isMac, nativeFrameEnabled, setNativeFrame } from "../nativeChrome";
import {
  copyIncludeSubtree,
  setCopyIncludeSubtree,
  copyStripCollapsed,
  setCopyStripCollapsed,
  refClickZoom,
  setRefClickZoom,
} from "../copySettings";
import { navReuseTabs, setNavReuseTabs } from "../navSettings";
import { spaceAfterRefCompletion, setSpaceAfterRefCompletion } from "../refCompletionSettings";
import { allowLocalFileImages, setAllowLocalFileImages } from "../localFileSettings";
import { conflictPolicyAlwaysAsk, setConflictPolicyAlwaysAsk } from "../conflictPolicy";
import { linkAutocompletePolicy, setLinkAutocompletePolicy, type LinkAutocompletePolicy } from "../editor/linkDefault";
import {
  spellcheckEnabled,
  setSpellcheckEnabled,
  spellcheckLanguages,
  spellcheckDictionaries,
  toggleSpellcheckLanguage,
  languageDisplayName,
  loadDictionaries,
  parseLanguages,
} from "../spellcheckSettings";
import {
  assetNameFormat,
  setAssetNameFormat,
  DEFAULT_ASSET_NAME_FORMAT,
  STAMPED_ASSET_NAME_FORMAT,
} from "../assetSettings";
import { MEDIA_EDITORS } from "../mediaEditors";
import { mediaEditorCommand, setMediaEditorCommand } from "../mediaEditorSettings";
import { formatAssetName } from "../media";
import {
  applyThemeColors,
  applyThemeStyle,
  clearThemeSelection,
  galleryThemes,
  selectedThemeColors,
  selectedThemeStyle,
} from "../themeGallery";
import type { GalleryTheme } from "../styles/themes";
import { platformKind } from "../platform";
import {
  installThemePackage,
  installedThemes,
  themeVersionIsRevoked,
  uninstallThemePackage,
} from "../themes/manager";
import { openConflicts, openPage, openFile } from "../router";
import { commandDefaults, eventToBindingString, setKeybindingsSuspended } from "../keybindings";
import { ShortcutsSettingsPane } from "./HelpShortcuts";
import { switchGraph, loadGraphPath } from "../graph";
import { settingsMaximized, setSettingsMaximized } from "../settingsLayout";
import {
  DEFAULT_QUERY_EXPORT_BUDGET_MIB,
  MAX_QUERY_EXPORT_BUDGET_MIB,
  MIN_QUERY_EXPORT_BUDGET_MIB,
  changeQueryExportBudgetMiB,
  queryExportBudgetMiB,
  resetQueryExportBudget,
} from "../queryExportBudget";
import { flushAll } from "../store";
import {
  backend,
  isTauri,
  OperationCancelledError,
  type BackupInfo,
} from "../backend";
import { runQueryWhenCurrent } from "../queryReadiness";
import type { AssetInfo, TrashStats, JournalFile, PageEntry } from "../types";
import { ConflictFileRow } from "./JournalConflictFileRow";
import { formatJournal } from "../journal";
import { installedPlugins, pluginManager, type ManagedPlugin } from "../plugins/manager";
import {
  COMMUNITY_REGISTRY_ENABLED,
  communityPlugins,
  communityThemes,
  installCommunityPlugin,
  installCommunityTheme,
  loadSafetyReport,
  refreshCommunityRegistry,
  registryPersistenceError,
  registryState,
  type PluginSafetyReport,
  type RegistryPlugin,
  type RegistryVersion,
} from "../plugins/registry";
import {
  launcherRankingEnabled,
  resetLauncherRanking,
  setLauncherRankingEnabled,
} from "../launcherRanking";
import { registerTransientLayer } from "../transientLayers";
import { readOr } from "../resourceRead";
import {
  DEFAULT_CUSTOM_WIDE_CONTENT_WIDTH,
  DEFAULT_STANDARD_CONTENT_WIDTH,
  CONTENT_WIDTH_SLIDER_MAX,
  MAX_CONTENT_WIDTH,
  MIN_CONTENT_WIDTH,
  changeStandardContentWidth,
  changeWideContentWidth,
  resetStandardContentWidth,
  standardContentWidth,
  wideContentWidth,
} from "../contentWidth";

// Journal display-title formats offered in the date-format dropdown — OG's
// `journal-title-formatters` set (frontend/date.cljs). Display-only; the on-disk
// journal file name is governed separately by `:journal/file-name-format`.
const DATE_FORMATS = [
  "MMM do, yyyy",
  "MMMM do, yyyy",
  "do MMM yyyy",
  "do MMMM yyyy",
  "E, dd-MM-yyyy",
  "EEE, dd-MM-yyyy",
  "EEEE, dd-MM-yyyy",
  "E, dd.MM.yyyy",
  "EEE, dd.MM.yyyy",
  "EEEE, dd.MM.yyyy",
  "EEE, MM/dd/yyyy",
  "EEEE, MM/dd/yyyy",
  "EEE, yyyy/MM/dd",
  "dd-MM-yyyy",
  "MM/dd/yyyy",
  "MM-dd-yyyy",
  "MM_dd_yyyy",
  "yyyy/MM/dd",
  "yyyy-MM-dd",
  "yyyy-MM-dd EEEE",
  "yyyy_MM_dd",
  "yyyyMMdd",
];

// The one file a joining device waits for, relative to the graph folder.
type Tab = SettingsTabId;
const TABS: { id: Tab; label: string }[] = [
  { id: "appearance", label: "Appearance" },
  { id: "editor", label: "Editor" },
  { id: "journals", label: "Journals" },
  { id: "files", label: "Files" },
  { id: "backups", label: "Backups & recovery" },
  { id: "graph", label: "Graph" },
  { id: "plugins", label: "Plugins" },
  { id: "shortcuts", label: "Keyboard shortcuts" },
  { id: "diagnostics", label: "Help & diagnostics" },
  { id: "about", label: "About" },
];

type SettingSearchEntry = {
  tab: Tab;
  label: string;
  description: string;
  aliases?: string[];
  level?: "advanced";
  /** The setting itself is rendered only on desktop, so search must not offer a
   * result that scrolls to nothing on Android/iOS. */
  desktopOnly?: true;
};
const SETTING_SEARCH: SettingSearchEntry[] = [
  { tab: "diagnostics", label: "Help & diagnostics", description: "bug report flight recorder timings previous run privacy parser divergences anonymize" },
  { tab: "appearance", label: "Theme mode", description: "light dark system" },
  { tab: "appearance", label: "Style", description: "typography journal headings presentation notnote" },
  { tab: "appearance", label: "Color scheme", description: "default nord solarized gruvbox theme package colors" },
  { tab: "appearance", label: "Accent color", description: "interface highlight color" },
  { tab: "appearance", label: "Interface size", description: "zoom scale Ctrl scroll" },
  { tab: "appearance", label: "Wide mode", description: "reading width" },
  { tab: "appearance", label: "Standard page width", description: "reading column pixels cap reset", aliases: ["narrow width"], level: "advanced" },
  { tab: "appearance", label: "Wide page width", description: "fill pane custom pixels cap", aliases: ["wide mode width"], level: "advanced" },
  { tab: "appearance", label: "Document mode", description: "hide bullets prose" },
  { tab: "appearance", label: "Document-mode Enter creates a new block", description: "Enter Shift Enter internal newline config" },
  { tab: "appearance", label: "Show brackets", description: "page references config shortcut" },
  { tab: "appearance", label: "Typographic replacements", description: "arrows dashes glyphs" },
  { tab: "appearance", label: "Auto-pair brackets & quotes", description: "closers selections backspace" },
  { tab: "appearance", label: "Space after inserting a reference", description: "page block autocomplete spacing" },
  { tab: "appearance", label: "Dim in focus mode", description: "inactive blocks" },
  { tab: "appearance", label: "Load local-file images", description: "absolute paths permission security" },
  { tab: "appearance", label: "Smooth scrolling (experimental)", description: "animated journal scrolling WebKit", aliases: ["scroll animation"], level: "advanced" },
  { tab: "appearance", label: "System title bar & window controls", description: "native frame chrome" },
  { tab: "editor", label: "File format", description: "new pages Markdown Org" },
  { tab: "editor", label: "Logical outdenting", description: "Shift Tab following siblings Roam config" },
  { tab: "editor", label: "Link autocomplete default", description: "OG adaptive existing typed page tag completion", level: "advanced" },
  { tab: "editor", label: "Switch to an already-open tab when navigating", description: "reuse tabs", level: "advanced" },
  { tab: "editor", label: "Learn Ctrl+K choices", description: "adaptive launcher ranking reset history", level: "advanced" },
  { tab: "editor", label: "Spell checker", description: "dictionaries languages spelling" },
  { tab: "editor", label: "Copy a parent block's sub-blocks", description: "clipboard subtree", level: "advanced" },
  { tab: "editor", label: "Strip collapsed:: when copying", description: "clipboard properties", level: "advanced" },
  { tab: "editor", label: "Click a block reference to zoom in", description: "reference navigation" },
  { tab: "journals", label: "Journal date format", description: "display titles" },
  { tab: "journals", label: "First day of week", description: "calendar Monday Sunday" },
  { tab: "journals", label: "Carry-over", description: "buttons context header last days" },
  { tab: "journals", label: "Task workflow", description: "TODO DOING NOW LATER" },
  { tab: "journals", label: "Time tracking", description: "LOGBOOK clock" },
  { tab: "journals", label: "New-journal template", description: "default journal template" },
  { tab: "journals", label: "Quick-capture Enter key", description: "capture submit new block", level: "advanced" },
  { tab: "journals", label: "Agenda window", description: "scheduled deadline days" },
  { tab: "files", label: "New asset filename", description: "paste drag media names" },
  { tab: "files", label: "Watch for external edits", description: "inotify polling network filesystem" },
  { tab: "files", label: "Diagram editors", description: "drawio Excalidraw commands", level: "advanced", desktopOnly: true },
  { tab: "backups", label: "Snapshots to keep", description: "recovery retention conflicts" },
  {
    tab: "backups",
    label: "Always ask before applying an external change",
    description: "external edits conflict policy silent reload sync merge ask",
  },
  { tab: "graph", label: "Graph", description: "folder export publish" },
  { tab: "graph", label: "Home page", description: "home start startup open automatically landing" },
  { tab: "shortcuts", label: "Keyboard shortcuts", description: "key bindings commands remap" },
  { tab: "about", label: "About", description: "version licenses updates" },
];

function settingMatches(entry: SettingSearchEntry, query: string): boolean {
  const terms = query.toLowerCase().trim().split(/\s+/).filter(Boolean);
  const haystack = [entry.label, entry.description, ...(entry.aliases ?? [])].join(" ").toLowerCase();
  return terms.every((term) => haystack.includes(term));
}

function advancedMatch(tab: Tab, query: string): boolean {
  return !!query.trim() && SETTING_SEARCH.some((entry) => entry.tab === tab && entry.level === "advanced" && settingMatches(entry, query));
}

export function Settings(): JSX.Element {
  const [tab, setTab] = createSignal<Tab>("appearance");
  const [settingsQuery, setSettingsQuery] = createSignal("");
  const [settingsPlatformResource] = createResource(async () => {
    try {
      return await platformKind();
    } catch {
      // Unknown native platforms fail closed: do not reveal a package host whose
      // platform policy could not be established.
      return undefined;
    }
  });
  // Same "fail closed to unknown" answer the fetcher already gives, now also
  // reachable when the read itself would have thrown.
  const settingsPlatform = () => readOr(settingsPlatformResource, undefined, "settings platform");
  const pluginsAvailable = () => settingsPlatform() === "desktop" || settingsPlatform() === "android";
  const availableTabs = createMemo(() => pluginsAvailable() ? TABS : TABS.filter((entry) => entry.id !== "plugins"));
  const matches = createMemo(() => {
    const query = settingsQuery();
    if (!query.trim()) return [];
    const desktop = settingsPlatform() === "desktop";
    return SETTING_SEARCH.filter((entry) => (desktop || !entry.desktopOnly) && settingMatches(entry, query));
  });
  const openSearchResult = (entry: SettingSearchEntry) => {
    setTab(entry.tab);
    queueMicrotask(() => {
      const fields = [...document.querySelectorAll<HTMLElement>("[data-setting-label]")];
      fields.find((field) => field.dataset.settingLabel === entry.label)?.scrollIntoView({ block: "center" });
    });
  };
  const [publishMsg, setPublishMsg] = createSignal("");
  let publishDisposed = false;
  onCleanup(() => {
    publishDisposed = true;
  });
  const doPublish = async () => {
    setPublishMsg("Exporting…");
    try {
      // Publication reads its queries from the index; while the index is
      // being built it waits for it rather than failing (GH #543, audit R6-05).
      const [dir, n] = await runQueryWhenCurrent(
        () => backend().publishHtml(),
        () => !publishDisposed,
        (pending) => setPublishMsg(pending ? "Waiting for the index to be ready…" : "Exporting…"),
      );
      // A zero is a successful export of nothing, which reads as a broken
      // button (GH #560). Publication is the public-page capability, so say so.
      setPublishMsg(
        n === 0
          ? `Exported 0 pages to ${dir} — only pages with “public:: true” are exported.`
          : `Exported ${n} pages to ${dir}`,
      );
    } catch (e) {
      if (e instanceof OperationCancelledError) return;
      setPublishMsg(`Failed: ${String(e)}`);
    }
  };

  // Effective binding = local override > config.edn > built-in default.
  const shortcuts = () => {
    const cfg = graphMeta()?.shortcuts ?? {};
    const ov = shortcutOverrides();
    return commandDefaults().map((c) => ({
      ...c,
      effective: ov[c.id] ?? cfg[c.id] ?? c.binding,
      overridden: c.id in ov,
    }));
  };

  createEffect(() => {
    if (!settingsPlatformResource.loading && tab() === "plugins" && !pluginsAvailable()) setTab("appearance");
  });

  createEffect(() => {
    if (!settingsOpen()) return;
    const requested = settingsTabRequest();
    if (!requested) return;
    if (requested === "plugins" && settingsPlatformResource.loading) return;
    setTab(requested === "plugins" && !pluginsAvailable() ? "appearance" : requested === "improve" ? "diagnostics" : requested);
    clearSettingsTabRequest();
  });

  // Recording: capture the next chord for the command being remapped.
  const [recording, setRecording] = createSignal<string | null>(null);
  // Settings owns its semantic Escape rungs.  Registering here (rather than in
  // App) keeps shortcut recording/search/disclosures from being skipped by a
  // blanket modal close and ensures disposal follows this component lifetime.
  // Maximize (GH #287): a pure geometry toggle on the dialog, so the selected
  // page and scroll position ride through untouched. GH #427 made it stick —
  // the state lives in settingsLayout.ts and is remembered across a reopen and
  // a restart, because this dialog unmounts on close and someone who wants the
  // wide size wants it every time, not once per open.
  const maximized = settingsMaximized;
  const setMaximized = setSettingsMaximized;

  createEffect(() => {
    if (!settingsOpen()) return;
    const unregister = registerTransientLayer({
      id: "settings",
      root: () => document.querySelector<HTMLElement>(".settings-modal"),
      dismiss: () => {
        if (recording()) { setRecording(null); return true; }
        if (settingsQuery()) { setSettingsQuery(""); return true; }
        closeSettings();
        return true;
      },
    });
    onCleanup(unregister);
  });
  createEffect(() => {
    const id = recording();
    if (!id) {
      setKeybindingsSuspended(false);
      return;
    }
    setKeybindingsSuspended(true);
    onCleanup(() => setKeybindingsSuspended(false));
    const onKey = (e: KeyboardEvent) => {
      // Escape belongs to the one capture dispatcher.  It will dismiss this
      // recording rung before the lower Settings/modal ladder.
      if (e.key === "Escape" || e.isComposing || e.keyCode === 229) return;
      e.preventDefault();
      e.stopPropagation();
      const b = eventToBindingString(e);
      if (!b) return; // bare modifier — keep waiting
      setShortcutOverride(id, b);
      setRecording(null);
    };
    window.addEventListener("keydown", onKey, true);
    onCleanup(() => window.removeEventListener("keydown", onKey, true));
  });

  return (
    <Show when={settingsOpen()}>
      <div class="modal-overlay" classList={{ "settings-maximized": maximized() }} onClick={closeSettings}>
        <div class="settings-modal" onClick={(e) => e.stopPropagation()}>
          <aside class="settings-nav">
            <div class="settings-nav-title">Settings</div>
            <For each={availableTabs()}>
              {(t) => (
                <button
                  class="settings-nav-item"
                  // The tab's identity, so a test or a deep link addresses the tab
                  // rather than its label. Labels are presentation: "Diagnostics"
                  // became "Help & diagnostics" and every label-coupled selector
                  // broke with it.
                  data-settings-tab={t.id}
                  classList={{ active: tab() === t.id }}
                  onClick={() => setTab(t.id)}
                >
                  {t.label}
                </button>
              )}
            </For>
            <div class="settings-nav-foot">Built {buildStamp()}</div>
          </aside>

          <div class="settings-pane">
            <div class="settings-pane-head">
              <span>{availableTabs().find((t) => t.id === tab())?.label}</span>
              <input
                class="settings-search-input"
                type="search"
                placeholder={tab() === "shortcuts" ? "Search shortcuts..." : "Search settings…"}
                aria-label={tab() === "shortcuts" ? "Search shortcuts" : "Search settings"}
                value={settingsQuery()}
                onInput={(event) => setSettingsQuery(event.currentTarget.value)}
                onKeyDown={(event) => {
                  if (event.isComposing || event.keyCode === 229) return;
                  if (event.key === "Escape" && settingsQuery()) {
                    event.preventDefault();
                    setSettingsQuery("");
                  }
                }}
              />
              {/* Desktop-only near-viewport toggle (hidden ≤480px by CSS, where
                  the sheet already owns the viewport) — GH #287. */}
              <button
                class="icon-btn settings-maximize"
                type="button"
                title={maximized() ? "Restore settings size" : "Maximize settings"}
                aria-label={maximized() ? "Restore settings size" : "Maximize settings"}
                aria-pressed={maximized()}
                onClick={() => setMaximized(!maximized())}
              >
                <svg viewBox="0 0 24 24" width="14" height="14" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2">
                  {maximized() ? (
                    <>
                      {/* restore: two overlapping squares while maximized. */}
                      <rect x="7" y="3" width="14" height="14" rx="1.5" />
                      <path d="M3 10v9.5A1.5 1.5 0 0 0 4.5 21H14" />
                    </>
                  ) : (
                    /* maximize: one empty square */
                    <rect x="3.5" y="3.5" width="17" height="17" rx="1.5" />
                  )}
                </svg>
              </button>
              <button class="icon-btn" onClick={closeSettings}>
                ✕
              </button>
            </div>
            <div class="settings-pane-body">
              {/* GH #409: Settings is mounted under a fallback-less <Suspense>
                  in App.tsx (it is lazy()), so a panel that starts loading a
                  resource when it mounts suspended the WHOLE dialog — the user
                  saw Settings vanish and come back on every switch to Journals,
                  Backups and Graph, the three sections whose panels do exactly
                  that (the template list, the journal-filename inventory, the
                  home-page picker). Sections with nothing to load never
                  flickered, which is why the report names only those three.
                  This boundary keeps the suspension inside the pane, so the
                  dialog, its list of sections and the search box stay put. */}
              <Suspense fallback={<div class="settings-pane-pending" aria-hidden="true" />}>
              <Show when={settingsQuery().trim() && tab() !== "shortcuts"}>
                <div class="settings-search-results" aria-live="polite">
                  <Show when={matches().length} fallback={<div class="settings-search-empty">No matching settings</div>}>
                    <For each={matches()}>
                      {(entry) => (
                        <button type="button" class="settings-search-result" onClick={() => openSearchResult(entry)}>
                          <span>{entry.label}</span>
                          <small>
                            {TABS.find((candidate) => candidate.id === entry.tab)?.label}
                            {entry.level === "advanced" ? " › Advanced" : entry.level === "experimental" ? " › Experimental" : ""}
                          </small>
                        </button>
                      )}
                    </For>
                  </Show>
                </div>
              </Show>
              <Show when={tab() === "appearance"}>
                <AppearanceTab search={settingsQuery()} />
              </Show>
              <Show when={tab() === "editor"}>
                <EditorTab search={settingsQuery()} />
              </Show>
              <Show when={tab() === "journals"}>
                <JournalsTab search={settingsQuery()} />
              </Show>
              <Show when={tab() === "files"}>
                <FilesTab search={settingsQuery()} />
              </Show>
              <Show when={tab() === "backups"}>
                <BackupsTab search={settingsQuery()} />
              </Show>
              <Show when={tab() === "graph"}>
                <GraphTab publishMsg={publishMsg()} doPublish={doPublish} />
              </Show>
              <Show when={tab() === "plugins" && pluginsAvailable()}>
                <PluginsTab />
              </Show>
              <Show when={tab() === "shortcuts"}>
                <ShortcutsSettingsPane
                  shortcuts={shortcuts()}
                  search={settingsQuery()}
                  recording={recording()}
                  onRecord={(id) => setRecording(recording() === id ? null : id)}
                  onUnbind={(id) => {
                    setRecording(null);
                    setShortcutOverride(id, "false");
                  }}
                  onReset={resetShortcutOverride}
                />
              </Show>
              <Show when={tab() === "diagnostics"}>
                <DiagnosticsTab />
              </Show>
              <Show when={tab() === "about"}>
                <AboutTab />
              </Show>
              </Suspense>
            </div>
          </div>
        </div>
      </div>
    </Show>
  );
}

// One setting: label + control on a line, with the explanatory hint on its own
// full-width line below (so long hints read cleanly instead of being squeezed
// into the right column). Pass `hint` as JSX to allow inline <code>/markup.
function Field(props: { label: string; hint?: JSX.Element; children: JSX.Element }): JSX.Element {
  return (
    <div class="settings-field" data-setting-label={props.label}>
      <div class="settings-field-row">
        <span class="settings-label">{props.label}</span>
        <div class="settings-field-control">{props.children}</div>
      </div>
      <Show when={props.hint}>
        <div class="settings-hint settings-field-hint">{props.hint}</div>
      </Show>
    </div>
  );
}

function Toggle(props: { on: boolean; onClick: () => void; disabled?: boolean }): JSX.Element {
  return (
    <button
      class="settings-toggle"
      classList={{ on: props.on }}
      role="switch"
      aria-checked={props.on}
      disabled={props.disabled}
      onClick={props.onClick}
    >
      <span class="settings-toggle-knob" />
    </button>
  );
}

function PluginSettingsForm(props: {
  plugin: ManagedPlugin;
  busy: () => string | null;
  setBusy: (value: string | null) => void;
}): JSX.Element {
  const operationKey = () => `${props.plugin.manifest.id}@${props.plugin.manifest.version}:settings`;
  const update = async (key: string, value: string | number | boolean) => {
    const myKey = operationKey();
    props.setBusy(myKey);
    try {
      await pluginManager.setSetting(props.plugin.manifest.id, props.plugin.manifest.version, key, value);
    } catch (error) {
      pushToast(`Plugin setting could not be saved: ${String(error)}`, "error");
    } finally {
      if (props.busy() === myKey) props.setBusy(null);
    }
  };
  const reset = async (key?: string) => {
    const myKey = operationKey();
    props.setBusy(myKey);
    try {
      if (key) await pluginManager.resetSetting(props.plugin.manifest.id, props.plugin.manifest.version, key);
      else await pluginManager.resetSettings(props.plugin.manifest.id, props.plugin.manifest.version);
    } catch (error) {
      pushToast(`Plugin settings could not be reset: ${String(error)}`, "error");
    } finally {
      if (props.busy() === myKey) props.setBusy(null);
    }
  };

  return (
    <Show
      when={(props.plugin.manifest.settings?.length ?? 0) > 0}
      fallback={<p class="settings-hint">This plugin has no configurable settings.</p>}
    >
      <div class="plugin-settings-list">
        <For each={props.plugin.manifest.settings ?? []}>
          {(definition) => {
            const value = () => props.plugin.settings[definition.key] ?? definition.default;
            const changed = () => value() !== definition.default;
            return (
              <div class="settings-field" data-setting-label={definition.label}>
                <div class="settings-field-row">
                  <div>
                    <div class="settings-label">{definition.label}</div>
                    <div class="settings-hint settings-field-hint">{definition.description}</div>
                  </div>
                  <div class="settings-field-control plugin-setting-control">
                    <Show when={definition.type === "boolean"}>
                      <Toggle
                        on={value() === true}
                        disabled={props.busy() !== null}
                        onClick={() => void update(definition.key, value() !== true)}
                      />
                    </Show>
                    <Show when={definition.type === "enum" && definition.type === "enum"}>
                      <select
                        class="settings-input"
                        aria-label={definition.label}
                        disabled={props.busy() !== null}
                        value={String(value())}
                        onChange={(event) => void update(definition.key, event.currentTarget.value)}
                      >
                        <For each={definition.type === "enum" ? definition.choices : []}>
                          {(choice) => <option value={choice.value}>{choice.label}</option>}
                        </For>
                      </select>
                    </Show>
                    <Show when={definition.type === "number" && definition.type === "number"}>
                      <input
                        class="settings-input plugin-setting-number"
                        type="number"
                        aria-label={definition.label}
                        disabled={props.busy() !== null}
                        value={Number(value())}
                        min={definition.type === "number" ? definition.min : undefined}
                        max={definition.type === "number" ? definition.max : undefined}
                        step={definition.type === "number" ? definition.step ?? "any" : undefined}
                        onChange={(event) => {
                          if (Number.isFinite(event.currentTarget.valueAsNumber)) {
                            void update(definition.key, event.currentTarget.valueAsNumber);
                          }
                        }}
                      />
                    </Show>
                    <Show when={definition.type === "string" && definition.type === "string"}>
                      <input
                        class="settings-input"
                        type="text"
                        aria-label={definition.label}
                        disabled={props.busy() !== null}
                        value={String(value())}
                        maxLength={definition.type === "string" ? definition.maxLength : undefined}
                        onChange={(event) => void update(definition.key, event.currentTarget.value)}
                      />
                    </Show>
                    <Show when={changed()}>
                      <button class="settings-link" disabled={props.busy() !== null} onClick={() => void reset(definition.key)}>
                        Reset
                      </button>
                    </Show>
                  </div>
                </div>
              </div>
            );
          }}
        </For>
      </div>
      <button class="settings-btn" disabled={props.busy() !== null} onClick={() => void reset()}>
        Reset all settings
      </button>
      <p class="settings-hint">Stored on this device only. Plugin settings are never written into your graph.</p>
    </Show>
  );
}

function PluginsTab(): JSX.Element {
  let packageInput: HTMLInputElement | undefined;
  const [busy, setBusy] = createSignal<string | null>(null);
  const [view, setView] = createSignal<"browse" | "installed">("browse");
  const [selectedPluginKey, setSelectedPluginKey] = createSignal<string | null>(null);
  const [currentPlatformResource] = createResource(platformKind);
  // Unlike its two siblings in this file, `platformKind` is uncaught here; an
  // unknown platform reads as "not yet known", which is what the buttons below
  // already render.
  const currentPlatform = () => readOr(currentPlatformResource, undefined, "plugin platform");
  const selectedPlugin = () => {
    const key = selectedPluginKey();
    return key ? installedPlugins().find((plugin) => `${plugin.manifest.id}@${plugin.manifest.version}` === key) : undefined;
  };

  const installFiles = async (files: FileList | null) => {
    if (!files?.length) return;
    const selected = Array.from(files);
    const manifestFile = selected.find((file) => file.name === "manifest.json") ?? selected.find((file) => file.name.endsWith(".json"));
    const wasmFile = selected.find((file) => file.name.endsWith(".wasm"));
    if (!manifestFile || !wasmFile) {
      pushToast("Choose both manifest.json and the plugin's .wasm entry.", "error");
      return;
    }
    const myKey = "install";
    setBusy(myKey);
    try {
      const manifest: unknown = JSON.parse(await manifestFile.text());
      const plugin = await pluginManager.install(manifest, new Uint8Array(await wasmFile.arrayBuffer()));
      pushToast(`${plugin.manifest.name} ${plugin.manifest.version} installed disabled. Review it, then enable it here.`, "info");
      setView("installed");
      setSelectedPluginKey(`${plugin.manifest.id}@${plugin.manifest.version}`);
    } catch (error) {
      pushToast(`Plugin installation failed: ${String(error)}`, "error");
    } finally {
      if (busy() === myKey) setBusy(null);
      if (packageInput) packageInput.value = "";
    }
  };

  const togglePlugin = async (id: string, version: string, enabled: boolean) => {
    const myKey = `${id}@${version}`;
    setBusy(myKey);
    try {
      if (enabled) await pluginManager.disable(id);
      else await pluginManager.enable(id, version);
    } catch (error) {
      pushToast(`Plugin could not be ${enabled ? "disabled" : "enabled"}: ${String(error)}`, "error");
    } finally {
      if (busy() === myKey) setBusy(null);
    }
  };

  const uninstallPlugin = async (plugin: ReturnType<typeof installedPlugins>[number]) => {
    const { id, name, version } = plugin.manifest;
    const myKey = `${id}@${version}:uninstall`;
    setBusy(myKey);
    const confirmed = await backend().confirm(
      `Uninstall ${name} ${version}?\n\nThis removes the plugin from this device. It does not change your graph or notes.`,
      "Uninstall plugin?"
    );
    if (!confirmed) {
      if (busy() === myKey) setBusy(null);
      return;
    }
    try {
      await pluginManager.uninstall(id, version);
      pushToast(`${name} ${version} was uninstalled.`, "info");
      if (selectedPluginKey() === `${id}@${version}`) setSelectedPluginKey(null);
    } catch (error) {
      pushToast(`Plugin could not be uninstalled: ${String(error)}`, "error");
    } finally {
      if (busy() === myKey) setBusy(null);
    }
  };

  const findingSeverityLabel = (severity: PluginSafetyReport["findings"][number]["severity"]): string => {
    if (severity === "info") return "Information";
    return `${severity[0].toUpperCase()}${severity.slice(1)}-risk finding`;
  };

  const installCommunity = async (plugin: RegistryPlugin, version: RegistryVersion) => {
    const myKey = `${plugin.id}@${version.version}`;
    setBusy(myKey);
    try {
      const installed = await installCommunityPlugin(plugin, version);
      pushToast(`${installed.manifest.name} installed disabled. Enable it after reviewing its capabilities.`, "info");
      setView("installed");
      setSelectedPluginKey(`${installed.manifest.id}@${installed.manifest.version}`);
    } catch (error) {
      pushToast(`Community plugin installation failed: ${String(error)}`, "error");
    } finally {
      if (busy() === myKey) setBusy(null);
    }
  };

  return (
    <>
      <Show when={selectedPlugin()} keyed>
        {(plugin) => (
          <div class="plugin-detail-page">
            <button class="settings-link plugin-detail-back" onClick={() => setSelectedPluginKey(null)}>← Installed plugins</button>
            <div class="plugin-detail-heading">
              <div>
                <h2>{plugin.manifest.name}</h2>
                <div class="settings-hint"><code>{plugin.manifest.id}</code> · v{plugin.manifest.version}</div>
              </div>
              <Toggle
                on={plugin.enabled && plugin.running}
                disabled={busy() !== null}
                onClick={() => void togglePlugin(plugin.manifest.id, plugin.manifest.version, plugin.enabled)}
              />
            </div>
            <p>{plugin.manifest.description}</p>
            <div class="settings-hint">
              {plugin.manifest.author} · {plugin.manifest.license} · {plugin.manifest.platforms.join(", ")}
              <br />Capabilities: {plugin.manifest.capabilities.length ? plugin.manifest.capabilities.join(", ") : "none"}
            </div>
            <Show when={plugin.manifest.portedFrom} keyed>
              {(origin) => (
                <div class="plugin-origin-card">
                  <strong>{origin.relationship === "behavioral-port" ? "Behavioral port" : "Source-derived port"}</strong>
                  <br /><span>From {origin.name} for {origin.ecosystem}; original authors: {origin.authors.join(", ")}.</span>
                  <br /><button class="settings-link" onClick={() => void backend().openExternal(origin.source)}>Original source at {origin.revision.slice(0, 12)}</button>
                </div>
              )}
            </Show>
            <div class="settings-section">Settings</div>
            <PluginSettingsForm plugin={plugin} busy={busy} setBusy={setBusy} />
            <div class="settings-section">Package</div>
            <div class="plugin-detail-actions">
              <button class="settings-btn" onClick={() => void backend().openExternal(plugin.manifest.source)}>Details &amp; screenshots</button>
              <button
                class="settings-btn settings-btn-danger"
                disabled={busy() !== null}
                onClick={() => void uninstallPlugin(plugin)}
              >
                {busy() === `${plugin.manifest.id}@${plugin.manifest.version}:uninstall` ? "Uninstalling…" : "Uninstall…"}
              </button>
            </div>
            <Show when={plugin.error}>
              <div class="settings-hint" style={{ color: "var(--danger, #c44)" }}>{plugin.error}</div>
            </Show>
          </div>
        )}
      </Show>
      <Show when={!selectedPlugin()}>
        <div class="plugin-settings-nav" role="tablist" aria-label="Plugin settings sections">
          {/* Same reasoning as data-settings-tab above: the view's identity, not
              its wording. This label also carries a COUNT, so a selector keyed
              on it breaks when the fixture installs a different number of
              plugins, not only when the wording changes. */}
          <button role="tab" data-plugin-view="browse" aria-selected={view() === "browse"} classList={{ active: view() === "browse" }} onClick={() => setView("browse")}>Browse</button>
          <button role="tab" data-plugin-view="installed" aria-selected={view() === "installed"} classList={{ active: view() === "installed" }} onClick={() => setView("installed")}>Installed ({installedPlugins().length})</button>
        </div>
      <Show when={view() === "browse"}>
      <div class="settings-section">Experimental plugin platform</div>
      <p class="settings-hint">
        Tine plugins are capability-limited WebAssembly guests, not Logseq or Obsidian plugins. They cannot directly
        access the DOM, Tauri, the network, files, processes, or your graph. A plugin version is installed disabled and
        runs only after its declared capabilities and entry validate.
      </p>
      <div class="settings-row">
        <div>
          <div class="settings-label">Install a local package</div>
          <div class="settings-hint">Select its <code>manifest.json</code> and <code>.wasm</code> file together.</div>
        </div>
        <div>
          <input
            ref={packageInput}
            type="file"
            multiple
            accept="application/json,.json,application/wasm,.wasm"
            style={{ display: "none" }}
            onChange={(event) => void installFiles(event.currentTarget.files)}
          />
          <button class="settings-btn" disabled={busy() !== null} onClick={() => packageInput?.click()}>
            {busy() === "install" ? "Validating…" : "Choose package…"}
          </button>
        </div>
      </div>

      <Show when={COMMUNITY_REGISTRY_ENABLED}>
      <div class="settings-section">Community catalogue</div>
      <div class="settings-hint">
        Signed registry · automated deterministic and no-tools AI audits · immutable version digests.
        <Show when={registryState() === "offline"}> Showing the last verified offline copy.</Show>
        <Show when={registryState() === "unsafe"}> Installed plugins are held until a signed catalogue can be verified.</Show>
        <Show when={registryPersistenceError()}> {registryPersistenceError()}</Show>
      </div>
      <Show
        when={communityPlugins().length > 0}
        fallback={
          <div class="settings-row">
            <span class="settings-hint">
              {registryState() === "loading" ? "Checking the signed catalogue…" : "No verified catalogue is available."}
            </span>
            <button class="settings-btn" disabled={registryState() === "loading"} onClick={() => void refreshCommunityRegistry()}>
              Retry
            </button>
          </div>
        }
      >
        <For each={communityPlugins()}>
          {(plugin) => {
            const version = () => plugin.versions[plugin.versions.length - 1];
            const installed = () =>
              installedPlugins().some((item) => item.manifest.id === plugin.id && item.manifest.version === version().version);
            const available = () => {
              const platform = currentPlatform();
              return platform ? version().platforms.includes(platform) : false;
            };
            const [reportOpen, setReportOpen] = createSignal(false);
            const [reportState, setReportState] = createSignal<"idle" | "loading" | "ready" | "error">("idle");
            const [report, setReport] = createSignal<PluginSafetyReport | null>(null);
            const showReport = async () => {
              if (reportOpen()) {
                setReportOpen(false);
                return;
              }
              setReportOpen(true);
              if (report()) return;
              setReportState("loading");
              try {
                setReport(await loadSafetyReport(plugin, version()));
                setReportState("ready");
              } catch {
                setReportState("error");
              }
            };
            const safetyLabel = () =>
              version().audit.manualApproval
                ? "Human-reviewed before publication"
                : version().audit.risk === "low"
                  ? "Low-risk automated pass"
                  : "Automated review passed";
            return (
              <div class="settings-field">
                <div class="settings-field-row">
                  <span class="settings-label">{plugin.name} <span class="settings-hint">v{version().version}</span></span>
                  <button
                    class="settings-btn"
                    disabled={installed() || busy() !== null || version().audit.status !== "passed" || !available()}
                    onClick={() => void installCommunity(plugin, version())}
                  >
                    {installed()
                      ? "Installed"
                      : !currentPlatform()
                        ? "Checking…"
                        : !available()
                          ? `Unavailable on ${currentPlatform()}`
                          : busy() === `${plugin.id}@${version().version}`
                            ? "Verifying…"
                            : "Install"}
                  </button>
                </div>
                <div class="settings-hint settings-field-hint">
                  {plugin.description}<br />
                  {plugin.license} · {plugin.aiDevelopment === "none" ? "Human-written" : `AI-${plugin.aiDevelopment}`} · {version().platforms.join(", ")}
                  <br />Capabilities: {version().capabilities.length ? version().capabilities.join(", ") : "none"}
                  {" · "}<button class="settings-link" onClick={() => void backend().openExternal(plugin.source)}>Details &amp; screenshots</button>
                </div>
                <div class="plugin-safety-row">
                  <span
                    class="plugin-safety-badge"
                    classList={{ manual: version().audit.manualApproval, low: !version().audit.manualApproval }}
                  >
                    {safetyLabel()}
                  </span>
                  <span class="settings-hint">Checked {version().audit.checkedAt.slice(0, 10)}</span>
                  <button class="settings-link" onClick={() => void showReport()}>
                    {reportOpen() ? "Hide safety report" : "Safety report"}
                  </button>
                </div>
                <Show when={reportOpen()}>
                  <div class="plugin-safety-report">
                    <Show when={reportState() === "loading"}>
                      <div class="settings-hint">Verifying the signed report…</div>
                    </Show>
                    <Show when={reportState() === "error"}>
                      <div class="settings-hint" style={{ color: "var(--danger, #c44)" }}>
                        The report could not be fetched and digest-verified.
                      </div>
                    </Show>
                    <Show when={report()} keyed>
                      {(safety) => (
                        <>
                          <p>{safety.summary}</p>
                          <Show when={safety.manualApproval} keyed>
                            {(approval) => (
                              <div class="plugin-safety-manual">
                                <strong>Why human review was required</strong><br />
                                <Show
                                  when={version().capabilities.includes("graph.write.block")}
                                  fallback={<>An automated check found behavior that Tine requires a person to inspect before publication.</>}
                                >
                                  This plugin can edit the focused block when you run its command. Tine holds every graph-writing
                                  plugin for human review, even when the automated checks otherwise pass.
                                </Show>
                                <Show when={plugin.id === "page.tine.query-filter"}>
                                  <br />The audit also caught that an earlier draft could act on the wrong focused block. The
                                  published plugin was narrowed to query table/board blocks and reviewed again.
                                </Show>
                                <br /><span>Signed review record: {approval.note}</span>
                              </div>
                            )}
                          </Show>
                          <Show when={safety.findings.length > 0}>
                            <div class="plugin-safety-findings">
                              <div class="settings-hint">
                                Severity describes possible impact, not reviewer confidence. “Low-risk” means a contained problem
                                unlikely to affect your notes; “Information” is an observation, not a known harm.
                              </div>
                              <For each={safety.findings}>
                                {(finding) => (
                                  <div class="plugin-safety-finding">
                                    <span class={`plugin-finding-severity severity-${finding.severity}`}>{findingSeverityLabel(finding.severity)}</span>
                                    <div><strong>{finding.title}</strong><br /><span>{finding.impact}</span></div>
                                  </div>
                                )}
                              </For>
                            </div>
                          </Show>
                          <div class="settings-hint">
                            Source <code title={safety.sourceCommit}>{safety.sourceCommit.slice(0, 12)}</code>
                            {" · Package "}<code title={version().sha256}>{version().sha256.slice(0, 12)}</code>
                            {" · Report "}<code title={version().audit.sha256}>{version().audit.sha256.slice(0, 12)}</code>
                            {" · "}{safety.areasReviewed.length} areas reviewed
                            {" · "}<button class="settings-link" onClick={() => void backend().openExternal(version().audit.url)}>Raw report</button>
                          </div>
                          <div class="settings-hint">Automated review is evidence, not a guarantee.</div>
                        </>
                      )}
                    </Show>
                  </div>
                </Show>
              </div>
            );
          }}
        </For>
      </Show>
      </Show>

      </Show>

      <Show when={view() === "installed"}>

      <div class="settings-section">Installed</div>
      <Show when={installedPlugins().length > 0} fallback={<p class="settings-hint">No plugins installed.</p>}>
        <For each={installedPlugins()}>
          {(plugin) => (
            <div class="settings-field">
              <div class="settings-field-row">
                <span class="settings-label">
                  {plugin.manifest.name} <span class="settings-hint">v{plugin.manifest.version}</span>
                </span>
                <div class="settings-field-control">
                  <button
                    class="settings-btn"
                    onClick={() => setSelectedPluginKey(`${plugin.manifest.id}@${plugin.manifest.version}`)}
                  >
                    {(plugin.manifest.settings?.length ?? 0) > 0 ? "Settings…" : "Details…"}
                  </button>
                  <Toggle
                    on={plugin.enabled && plugin.running}
                    disabled={busy() !== null}
                    onClick={() => void togglePlugin(plugin.manifest.id, plugin.manifest.version, plugin.enabled)}
                  />
                  <button
                    class="settings-btn settings-btn-danger"
                    disabled={busy() !== null}
                    onClick={() => void uninstallPlugin(plugin)}
                  >
                    {busy() === `${plugin.manifest.id}@${plugin.manifest.version}:uninstall` ? "Uninstalling…" : "Uninstall…"}
                  </button>
                </div>
              </div>
              <div class="settings-hint settings-field-hint">
                {plugin.manifest.description}<br />
                <code>{plugin.manifest.id}</code> · {plugin.manifest.license} · {plugin.manifest.platforms.join(", ")}
                <Show when={plugin.manifest.aiDevelopment && plugin.manifest.aiDevelopment !== "none"}>
                  {" · "}AI-{plugin.manifest.aiDevelopment}
                </Show>
                <br />Capabilities: {plugin.manifest.capabilities.length ? plugin.manifest.capabilities.join(", ") : "none"}
                {" · "}<button class="settings-link" onClick={() => void backend().openExternal(plugin.manifest.source)}>Details &amp; screenshots</button>
              </div>
              <Show when={plugin.error}>
                <div class="settings-hint" style={{ color: "var(--danger, #c44)" }}>{plugin.error}</div>
              </Show>
            </div>
          )}
        </For>
      </Show>
      </Show>
      </Show>
    </>
  );
}

// A settings row for an option where Tine's behavior can DIFFER from Logseq. Shows
// a "Differs from Logseq" chip + a one-line note on what Logseq does whenever the
// current value isn't the OG one, plus a "Match Logseq" button to flip back. Use
// this (instead of a plain Field) so non-OG defaults are always visible and one
// click from reverting. `ogValue` is the toggle state that matches Logseq.
function OgField(props: {
  label: string;
  hint?: JSX.Element;
  ogNote: string;
  ogValue: boolean;
  on: boolean;
  onToggle: () => void;
}): JSX.Element {
  const diverges = () => props.on !== props.ogValue;
  return (
    <div class="settings-field og-field" data-setting-label={props.label} classList={{ "og-diverges": diverges() }}>
      <div class="settings-field-row">
        <span class="settings-label">
          {props.label}
          <Show when={diverges()}>
            <span class="og-badge" title="Tine's default differs from Logseq here">Differs from Logseq</span>
          </Show>
        </span>
        <div class="settings-field-control">
          <Toggle on={props.on} onClick={props.onToggle} />
        </div>
      </div>
      <Show when={props.hint}>
        <div class="settings-hint settings-field-hint">{props.hint}</div>
      </Show>
      <div class="og-note">
        <span class="og-logseq">Logseq: {props.ogNote}</span>
        <Show when={diverges()}>
          <button class="og-revert" onClick={props.onToggle}>↩ Match Logseq</button>
        </Show>
      </div>
    </div>
  );
}

function SettingsDisclosure(props: {
  label: string;
  storageKey: string;
  layerPrefix: string;
  forceOpen?: boolean;
  className?: string;
  children: JSX.Element;
}): JSX.Element {
  const layerId = `${props.layerPrefix}-${createUniqueId()}`;
  const key = props.storageKey;
  let initial = false;
  try { initial = localStorage.getItem(key) === "1"; } catch {}
  const [open, setOpen] = createSignal(initial);
  let button: HTMLButtonElement | undefined;
  const expanded = () => props.forceOpen || open();
  const persist = (value: boolean) => {
    try { localStorage.setItem(key, value ? "1" : "0"); } catch {}
  };
  const toggle = () => {
    const next = !open();
    setOpen(next);
    persist(next);
  };
  createEffect(() => {
    if (!open() || props.forceOpen) return;
    const unregister = registerTransientLayer({
      id: layerId,
      parentId: "settings",
      root: () => button?.closest<HTMLElement>(".settings-advanced") ?? null,
      trigger: () => button ?? null,
      dismiss: () => { setOpen(false); persist(false); return true; },
    });
    onCleanup(unregister);
  });
  return (
    <section class={`settings-advanced ${props.className ?? ""}`}>
      <button
        ref={button}
        type="button"
        class="settings-advanced-toggle"
        aria-expanded={expanded()}
        onClick={toggle}
        onKeyDown={(event) => {
          if (event.isComposing || event.keyCode === 229) return;
          if (event.key === "Escape" && open() && !props.forceOpen) {
            event.preventDefault();
            setOpen(false);
            persist(false);
            queueMicrotask(() => button?.focus());
          }
        }}
      >
        <span aria-hidden="true">{expanded() ? "▾" : "▸"}</span> {props.label}
      </button>
      <Show when={expanded()}>
        <div class="settings-advanced-body">{props.children}</div>
      </Show>
    </section>
  );
}

function AdvancedSection(props: { tab: Tab; forceOpen: boolean; children: JSX.Element }): JSX.Element {
  return (
    <SettingsDisclosure
      label="Advanced"
      storageKey={`tine.settings.advanced.${props.tab}`}
      layerPrefix="settings-advanced"
      forceOpen={props.forceOpen}
    >
      {props.children}
    </SettingsDisclosure>
  );
}

function galleryBadge(theme: GalleryTheme): string {
  if (theme.modes.length === 1) return theme.modes[0] === "light" ? "Light-only" : "Dark-only";
  return theme.compat === "full" ? "Full" : "Partial";
}

function ThemeGalleryCard(props: {
  id: string;
  name: string;
  author: string;
  badge: string;
  thumbnail: string;
  selected: boolean;
}): JSX.Element {
  return (
    <button
      class="theme-gallery-card"
      classList={{ selected: props.selected }}
      aria-pressed={props.selected}
      onClick={() => applyThemeColors(props.id)}
    >
      <span class="theme-gallery-thumb">
        <img src={props.thumbnail} alt="" loading="lazy" />
      </span>
      <span class="theme-gallery-card-body">
        <span class="theme-gallery-card-top">
          <span class="theme-gallery-name">{props.name}</span>
          <span class="theme-gallery-badge">{props.badge}</span>
        </span>
        <span class="theme-gallery-author">{props.author}</span>
      </span>
    </button>
  );
}

function AppearanceTab(props: { search: string }): JSX.Element {
  let themePackageInput: HTMLInputElement | undefined;
  const [themePackageBusy, setThemePackageBusy] = createSignal<string | null>(null);
  const installThemeFile = async (files: FileList | null) => {
    const file = files?.[0];
    if (!file) return;
    if (file.size > 64 * 1024) {
      pushToast("Theme manifest exceeds the 64 KiB limit.", "error");
      return;
    }
    setThemePackageBusy("install");
    try {
      const installed = await installThemePackage(JSON.parse(await file.text()));
      pushToast(`${installed.manifest.name} ${installed.manifest.version} installed.`, "info");
    } catch (error) {
      pushToast(`Theme installation failed: ${String(error)}`, "error");
    } finally {
      setThemePackageBusy(null);
      if (themePackageInput) themePackageInput.value = "";
    }
  };
  const uninstallTheme = async (key: string, name: string) => {
    const confirmed = await backend().confirm(
      `Uninstall ${name}?\n\nThis removes the theme from this device. It does not change your graph or custom.css.`,
      "Uninstall theme?"
    );
    if (!confirmed) return;
    setThemePackageBusy(key);
    try {
      clearThemeSelection(key);
      await uninstallThemePackage(key);
      pushToast(`${name} was uninstalled.`, "info");
    } catch (error) {
      pushToast(`Theme could not be uninstalled: ${String(error)}`, "error");
    } finally {
      setThemePackageBusy(null);
    }
  };
  const installRegistryTheme = async (themeEntry: ReturnType<typeof communityThemes>[number]) => {
    const version = themeEntry.versions[themeEntry.versions.length - 1];
    setThemePackageBusy(`${themeEntry.id}@${version.version}`);
    try {
      const installed = await installCommunityTheme(themeEntry, version);
      pushToast(`${installed.manifest.name} ${installed.manifest.version} installed.`, "info");
    } catch (error) {
      pushToast(`Community theme installation failed: ${String(error)}`, "error");
    } finally {
      setThemePackageBusy(null);
    }
  };
  const [savingNativeFrame, setSavingNativeFrame] = createSignal(false);
  const changeNativeFrame = async () => {
    if (savingNativeFrame()) return;
    setSavingNativeFrame(true);
    const next = !nativeFrameEnabled();
    try {
      await setNativeFrame(next);
      pushToast("Saved. Restart Tine to apply the window-frame change.", "info");
    } catch (error) {
      pushToast(`Couldn't save the window-frame setting. (${String(error)})`, "error");
    } finally {
      setSavingNativeFrame(false);
    }
  };

  return (
    <>
      <div class="settings-row">
        <span class="settings-label">Mode</span>
        <div
          class="theme-switch theme-switch3"
          classList={{ "is-light": appearancePreference() === "light", "is-system": appearancePreference() === "system", "is-dark": appearancePreference() === "dark" }}
          role="radiogroup"
          aria-label="Appearance"
        >
          <button
            type="button"
            class="theme-opt"
            role="radio"
            aria-checked={appearancePreference() === "light"}
            title="Light theme"
            onClick={() => setAppearancePreference("light")}
          >
            <span class="theme-ico">☀</span>Light
          </button>
          <button
            type="button"
            class="theme-opt"
            role="radio"
            aria-checked={appearancePreference() === "system"}
            title="Follow the OS light/dark setting"
            onClick={() => setAppearancePreference("system")}
          >
            <span class="theme-ico">◐</span>System
          </button>
          <button
            type="button"
            class="theme-opt"
            role="radio"
            aria-checked={appearancePreference() === "dark"}
            title="Dark theme"
            onClick={() => setAppearancePreference("dark")}
          >
            <span class="theme-ico">☾</span>Dark
          </button>
          <span class="theme-knob" />
        </div>
      </div>

      <div class="settings-section">Themes</div>
      <div class="settings-row">
        <div>
          <div class="settings-label">Style</div>
          <div class="settings-hint">Typography, journal headings, and other presentation choices.</div>
        </div>
        <select
          class="settings-select"
          aria-label="Theme style"
          value={selectedThemeStyle()}
          onChange={(event) => applyThemeStyle(event.currentTarget.value)}
        >
          <option value="">Default</option>
          <For each={installedThemes().filter((installed) =>
            !themeVersionIsRevoked(installed.key)
            && Object.keys(installed.manifest.presentation ?? {}).length > 0
          )}>
            {(installed) => <option value={installed.key}>{installed.manifest.name}</option>}
          </For>
        </select>
      </div>

      <div class="settings-section">Color scheme</div>
      <div class="theme-gallery-grid">
        <ThemeGalleryCard
          id=""
          name="Default"
          author="Tine"
          badge="Stock"
          thumbnail="/theme-thumbnails/default.png"
          selected={selectedThemeColors() === ""}
        />
        <For each={galleryThemes}>
          {(theme) => (
            <ThemeGalleryCard
              id={theme.id}
              name={theme.name}
              author={theme.author}
              badge={galleryBadge(theme)}
              thumbnail={theme.thumbnail}
              selected={selectedThemeColors() === theme.id}
            />
          )}
        </For>
      </div>
      <div class="settings-hint theme-gallery-hint">
        Style and colors are independent. Theme packages use validated colors and Tine-owned presentation styles; your <code>logseq/custom.css</code> still takes priority.
      </div>

      <Show when={COMMUNITY_REGISTRY_ENABLED}>
      <div class="settings-section">Theme packages</div>
      <Show when={communityThemes().length > 0}>
        <div class="settings-hint theme-gallery-hint">Signed community themes · inert token manifests · immutable audit digests.</div>
        <For each={communityThemes()}>
          {(themeEntry) => {
            const version = () => themeEntry.versions[themeEntry.versions.length - 1];
            const key = () => `${themeEntry.id}@${version().version}`;
            const installed = () => installedThemes().some((theme) => theme.key === key());
            const revoked = () => themeVersionIsRevoked(key());
            return (
              <div class="settings-field">
                <div class="settings-field-row">
                  <div>
                    <div class="settings-label">{themeEntry.name} <span class="settings-hint">v{version().version}</span></div>
                    <div class="settings-hint settings-field-hint">
                      {themeEntry.description}<br />{themeEntry.license} · {version().modes.join(" + ")} · {revoked() ? "Revoked by the signed registry" : version().audit.manualApproval ? "Human-reviewed" : "Low-risk automated pass"}
                    </div>
                  </div>
                  <div class="settings-field-control">
                    <button class="settings-link" onClick={() => void backend().openExternal(themeEntry.source)}>Details &amp; screenshots</button>
                    <button
                      class="settings-btn"
                      disabled={installed() || revoked() || themePackageBusy() !== null || version().audit.status !== "passed"}
                      onClick={() => void installRegistryTheme(themeEntry)}
                    >
                      {revoked() ? "Revoked" : installed() ? "Installed" : themePackageBusy() === key() ? "Verifying…" : "Install"}
                    </button>
                  </div>
                </div>
              </div>
            );
          }}
        </For>
      </Show>
      </Show>
      <div class="settings-row">
        <div>
          <div class="settings-label">Install a token theme</div>
          <div class="settings-hint">Theme packages contain whitelisted colors, metadata, and optional Tine-owned presentation presets—no scripts, selectors, imports, or remote assets.</div>
        </div>
        <div>
          <input
            ref={themePackageInput}
            type="file"
            accept="application/json,.json"
            style={{ display: "none" }}
            onChange={(event) => void installThemeFile(event.currentTarget.files)}
          />
          <button class="settings-btn" disabled={themePackageBusy() !== null} onClick={() => themePackageInput?.click()}>
            {themePackageBusy() === "install" ? "Validating…" : "Choose theme.json…"}
          </button>
        </div>
      </div>
      <Show when={installedThemes().length > 0} fallback={<p class="settings-hint">No theme packages installed.</p>}>
        <div class="installed-theme-list">
          <For each={installedThemes()}>
            {(installed) => {
              const previewMode = () => installed.manifest.modes[theme()] ?? installed.manifest.modes.light ?? installed.manifest.modes.dark ?? {};
              const revoked = () => themeVersionIsRevoked(installed.key);
              return (
                <div class="settings-field installed-theme-row">
                  <div class="settings-field-row">
                    <div class="installed-theme-identity">
                      <span
                        class="installed-theme-swatch"
                        aria-hidden="true"
                        style={{
                          background: previewMode()["--ls-primary-background-color"] ?? "var(--bg-secondary)",
                          color: previewMode()["--ls-active-primary-color"] ?? "var(--accent)",
                        }}
                      >●</span>
                      <div>
                        <div class="settings-label">{installed.manifest.name} <span class="settings-hint">v{installed.manifest.version}</span></div>
                        <div class="settings-hint">{installed.manifest.author} · {installed.manifest.license} · {Object.keys(installed.manifest.modes).join(" + ")}{revoked() ? " · Revoked and disabled" : ""}</div>
                      </div>
                    </div>
                    <div class="settings-field-control">
                      <button
                        class="settings-btn"
                        disabled={revoked() || selectedThemeColors() === installed.key}
                        onClick={() => applyThemeColors(installed.key)}
                      >
                        {revoked() ? "Revoked" : selectedThemeColors() === installed.key ? "Colors selected" : "Use colors"}
                      </button>
                      <Show when={Object.keys(installed.manifest.presentation ?? {}).length > 0}>
                        <button
                          class="settings-btn"
                          disabled={revoked() || selectedThemeStyle() === installed.key}
                          onClick={() => applyThemeStyle(installed.key)}
                        >
                          {revoked() ? "Revoked" : selectedThemeStyle() === installed.key ? "Style selected" : "Use style"}
                        </button>
                      </Show>
                      <button class="settings-link" onClick={() => void backend().openExternal(installed.manifest.source)}>Details</button>
                      <button
                        class="settings-btn settings-btn-danger"
                        disabled={themePackageBusy() !== null}
                        onClick={() => void uninstallTheme(installed.key, installed.manifest.name)}
                      >
                        {themePackageBusy() === installed.key ? "Uninstalling…" : "Uninstall…"}
                      </button>
                    </div>
                  </div>
                  <div class="settings-hint settings-field-hint">{installed.manifest.description}</div>
                  <Show when={installed.manifest.portedFrom} keyed>
                    {(origin) => <div class="settings-hint">Behavioral port of {origin.name} for {origin.ecosystem}, credited to {origin.authors.join(", ")}.</div>}
                  </Show>
                </div>
              );
            }}
          </For>
        </div>
      </Show>

      <div class="settings-row">
        <span class="settings-label">Accent color</span>
        <div>
          <input
            type="color"
            class="settings-color"
            value={accentColor() ?? "#2563eb"}
            onInput={(e) => changeAccent(e.currentTarget.value)}
          />
          <Show when={accentColor()}>
            <button class="settings-btn" style={{ "margin-left": "8px" }} onClick={() => changeAccent(null)}>
              Reset
            </button>
          </Show>
        </div>
      </div>

      <Field
        label="Interface size"
        hint="Zoom the whole interface — Ctrl + / Ctrl − / Ctrl 0, or Ctrl + scroll. Saved on this device. (Over/within the PDF pane, those zoom the PDF instead.)"
      >
        <div style={{ display: "flex", "align-items": "center", gap: "8px" }}>
          <button class="settings-btn" title="Smaller (Ctrl −)" onClick={zoomOut}>−</button>
          <span class="mono" style={{ "min-width": "3.4em", "text-align": "center" }}>
            {Math.round(interfaceZoom() * 100)}%
          </span>
          <button class="settings-btn" title="Larger (Ctrl +)" onClick={zoomIn}>+</button>
          <Show when={interfaceZoom() !== 1}>
            <button class="settings-btn" style={{ "margin-left": "8px" }} onClick={zoomReset}>
              Reset
            </button>
          </Show>
        </div>
      </Field>

      <Field label="Wide mode" hint="Drops the reading-width cap.">
        <Toggle on={wideMode()} onClick={toggleWideMode} />
      </Field>

      <Field label="Document mode" hint="Hides bullets and indent guides for a cleaner prose view.">
        <Toggle on={documentMode()} onClick={toggleDocumentMode} />
      </Field>

      <Field
        label="Document-mode Enter creates a new block"
        hint={<>Keep the normal Enter = new block and Shift + Enter = line break mapping while Document mode is on. Off (the default) swaps them, like Logseq. Saved to <code>:shortcut/doc-mode-enter-for-new-block?</code> in <code>config.edn</code>.</>}
      >
        <Toggle
          on={docModeEnterForNewBlock()}
          onClick={() => changeDocModeEnterForNewBlock(!docModeEnterForNewBlock())}
        />
      </Field>

      <Field
        label="Show brackets"
        hint={<>Show the <code>[[ ]]</code> around page references. Saved to <code>:ui/show-brackets?</code> in <code>config.edn</code>; toggle with <code>mod+c mod+b</code>.</>}
      >
        <Toggle on={showBrackets()} onClick={() => changeShowBrackets(!showBrackets())} />
      </Field>

      <Field
        label="Typographic replacements"
        hint="Show arrows and dashes as glyphs — `->` → →, `-->` → ⟶, `--` → – (en dash), `---` → — (em dash). “While reading” keeps your Markdown as ASCII and only changes the rendered view (like `\Delta` → Δ); “While typing” rewrites the source itself as you type. A Tine touch, not Logseq."
      >
        <select
          class="settings-select"
          value={typographyMode()}
          onChange={(e) => {
            const v = e.currentTarget.value;
            setTypographyMode(v === "off" ? "off" : v === "type" ? "type" : "render");
          }}
        >
          <option value="render">On (while reading)</option>
          <option value="type">On (while typing)</option>
          <option value="off">Off</option>
        </select>
      </Field>

      <Field
        label="Auto-pair brackets & quotes"
        hint="Typing ( [ { &quot; ` inserts the matching closer with the caret between, wraps a selection, types through a closer, and Backspace on an empty pair clears both — the Logseq behaviour, ON by default. (Page-ref `[[ ]]` always auto-closes either way.) Turn it off if you dislike pairing."
      >
        <Toggle on={autoPairing()} onClick={() => setAutoPairing(!autoPairing())} />
      </Field>

      <OgField
        label="Space after inserting a reference"
        hint="After you pick a page from [[…]] or a block from ((…)) autocomplete, the caret lands past the closing brackets. ON (Tine default) also drops a space there so the next word flows on without stepping over the brackets; a block-final space is trimmed on save, so it never persists."
        ogNote="no space — the caret sits right after the closing brackets."
        ogValue={false}
        on={spaceAfterRefCompletion()}
        onToggle={() => setSpaceAfterRefCompletion(!spaceAfterRefCompletion())}
      />

      <Field
        label="Dim in focus mode"
        hint="Auto-enable dim inactive blocks (t b) when entering focus mode (t f)."
      >
        <Toggle on={dimInFocus()} onClick={() => setDimInFocus(!dimInFocus())} />
      </Field>

      <Field
        label="Load local-file images"
        hint="Let raw-HTML <img> tags in notes load images from absolute paths anywhere on this computer (e.g. an imported note's <img src=&quot;/home/…/pic.png&quot;>). Off by default — this is a permission: a synced or imported note isn't self-authored, so only enable it for graphs you trust. In-graph images and https images always work regardless."
      >
        <Toggle on={allowLocalFileImages()} onClick={() => setAllowLocalFileImages(!allowLocalFileImages())} />
      </Field>

      <AdvancedSection tab="appearance" forceOpen={advancedMatch("appearance", props.search)}>
        <Field
          label="Standard page width"
          hint="Maximum reading-column width on this device. Reset uses the active theme's default (810 px in Tine's built-in themes)."
        >
          <div class="settings-width-control">
            <input
              class="settings-width-range"
              aria-label="Standard page width"
              type="range"
              min={MIN_CONTENT_WIDTH}
              max={CONTENT_WIDTH_SLIDER_MAX}
              step="10"
              value={standardContentWidth() ?? DEFAULT_STANDARD_CONTENT_WIDTH}
              onInput={(event) => changeStandardContentWidth(event.currentTarget.valueAsNumber)}
            />
            <input
              class="settings-num settings-width-number"
              aria-label="Standard page width in pixels"
              type="number"
              min={MIN_CONTENT_WIDTH}
              max={MAX_CONTENT_WIDTH}
              step="10"
              value={standardContentWidth() ?? DEFAULT_STANDARD_CONTENT_WIDTH}
              onChange={(event) => changeStandardContentWidth(event.currentTarget.valueAsNumber)}
            />
            <span class="settings-width-unit">px</span>
            <Show when={standardContentWidth() !== null}>
              <button class="settings-btn" onClick={resetStandardContentWidth}>Reset</button>
            </Show>
          </div>
        </Field>

        <Field
          label="Wide page width"
          hint="Wide mode can fill the available pane or stop at a custom maximum. Saved on this device."
        >
          <div class="settings-width-control">
            <select
              class="settings-select"
              aria-label="Wide page width mode"
              value={wideContentWidth() === null ? "fill" : "custom"}
              onChange={(event) =>
                changeWideContentWidth(
                  event.currentTarget.value === "fill"
                    ? null
                    : (wideContentWidth() ?? DEFAULT_CUSTOM_WIDE_CONTENT_WIDTH),
                )
              }
            >
              <option value="fill">Fill pane</option>
              <option value="custom">Custom maximum</option>
            </select>
            <Show when={wideContentWidth() !== null}>
              <input
                class="settings-width-range"
                aria-label="Wide page width"
                type="range"
                min={MIN_CONTENT_WIDTH}
                max={CONTENT_WIDTH_SLIDER_MAX}
                step="10"
                value={wideContentWidth() ?? DEFAULT_CUSTOM_WIDE_CONTENT_WIDTH}
                onInput={(event) => changeWideContentWidth(event.currentTarget.valueAsNumber)}
              />
              <input
                class="settings-num settings-width-number"
                aria-label="Wide page width in pixels"
                type="number"
                min={MIN_CONTENT_WIDTH}
                max={MAX_CONTENT_WIDTH}
                step="10"
                value={wideContentWidth() ?? DEFAULT_CUSTOM_WIDE_CONTENT_WIDTH}
                onChange={(event) => changeWideContentWidth(event.currentTarget.valueAsNumber)}
              />
              <span class="settings-width-unit">px</span>
            </Show>
          </div>
        </Field>

        <Field
          label="Smooth scrolling (experimental)"
          hint="Animate the journal feed's scrolling to smooth out WebKitGTK's stepped mouse-wheel jumps. Off by default; this is a feel experiment — turn it off if it gets in the way."
        >
          <Toggle on={smoothScrollEnabled()} onClick={() => setSmoothScroll(!smoothScrollEnabled())} />
        </Field>
      </AdvancedSection>

      {/* Window chrome. macOS always uses its native frame (rounded corners +
          traffic lights, via the build-time Overlay title bar), so the toggle is
          only meaningful — and only shown — on Linux/Windows. */}
      <Show when={isTauri() && !isMac}>
        <Field
          label="System title bar & window controls"
          hint="Use your OS's native window frame (title bar, minimize/maximize/close, rounded corners) instead of Tine's compact built-in controls. Restart Tine after changing this setting. Off by default — the built-in controls save a row of vertical space."
        >
          <Toggle on={nativeFrameEnabled()} onClick={() => void changeNativeFrame()} />
        </Field>
      </Show>
      <Show when={isTauri() && isMac}>
        <Field
          label="Window controls"
          hint="macOS draws Tine with native rounded corners and traffic-light buttons."
        >
          <span style={{ color: "var(--text-muted)", "font-size": "12px" }}>Native (macOS)</span>
        </Field>
      </Show>

    </>
  );
}

// New-journal template picker: a dropdown of all `template::` templates + "(none)
// = blank days" (the factory default — clearing the pointer), and an "Edit →" jump
// to the chosen template's block. Uses existing concepts only: templates + the
// config pointer. No catalogue, no built-in default.
function JournalTemplateField(): JSX.Element {
  const [templatesResource] = createResource(() => backend().listTemplates());
  // An unreadable template list offers no templates; the field still shows and
  // still accepts the configured pointer.
  const templates = () => readOr(templatesResource, undefined, "journal templates");
  const current = () => graphMeta()?.default_journal_template ?? "";
  const list = () => templates() ?? [];
  const selected = () => list().find((t) => t.name === current());
  // A configured name that no longer matches a template (stale pointer) — surface
  // it so the dropdown reflects config rather than silently showing "(none)".
  const missing = () => current() !== "" && !list().some((t) => t.name === current());
  return (
    <Field
      label="New-journal template"
      hint={
        <>
          Template inserted into a new day's journal. Saved to{" "}
          <code>:default-templates {"{:journals …}"}</code> in <code>config.edn</code>. “(none)” →
          blank days (the default). Make a template via a block's right-click menu.
        </>
      }
    >
      <div class="settings-jtmpl">
        <select
          class="settings-select"
          value={current()}
          onChange={(e) => setJournalTemplate(e.currentTarget.value || null)}
        >
          <option value="">(none) — blank days</option>
          <Show when={missing()}>
            <option value={current()}>{current()} (not found)</option>
          </Show>
          <For each={list()}>{(t) => <option value={t.name}>{t.name}</option>}</For>
        </select>
        <Show when={selected()}>
          {(t) => (
            <button
              class="settings-link"
              onClick={() => {
                openPage(t().page, t().kind);
                closeSettings();
              }}
            >
              Edit →
            </button>
          )}
        </Show>
      </div>
    </Field>
  );
}

/** Journal display-title format picker. Shows each pattern with a live example
 *  (today rendered in it). Includes the graph's current value even if it isn't
 *  one of the presets, so a hand-edited config.edn round-trips. */
function DateFormatSelect(): JSX.Element {
  const today = new Date();
  const current = () => graphMeta()?.journal_page_title_format || "MMM do, yyyy";
  const options = () => (DATE_FORMATS.includes(current()) ? DATE_FORMATS : [current(), ...DATE_FORMATS]);
  return (
    <select
      class="settings-select"
      value={current()}
      onChange={(e) => changeJournalTitleFormat(e.currentTarget.value)}
    >
      <For each={options()}>
        {(fmt) => <option value={fmt}>{`${fmt}  —  ${formatJournal(today, fmt)}`}</option>}
      </For>
    </select>
  );
}

function EditorTab(props: { search: string }): JSX.Element {
  // Re-scan installed dictionaries each time Settings opens (the user may have
  // just installed one). The rows are the union of installed ∪ already-selected,
  // so a selected-but-uninstalled language still shows (flagged) instead of
  // silently vanishing.
  onMount(() => void loadDictionaries());
  const dictRows = createMemo(() => {
    const installed = new Set(spellcheckDictionaries());
    const selected = new Set(parseLanguages(spellcheckLanguages()));
    const codes = [...new Set([...installed, ...selected])].sort((a, b) =>
      languageDisplayName(a).localeCompare(languageDisplayName(b)),
    );
    return codes.map((code) => ({
      code,
      name: languageDisplayName(code),
      selected: selected.has(code),
      installed: installed.has(code),
    }));
  });
  return (
    <>
      <Field
        label="File format"
        hint={
          <>
            Format for <strong>new</strong> pages and journals — Markdown or Org. Existing{" "}
            <code>.md</code> and <code>.org</code> files keep their own format and are edited in
            place. Saved to <code>:preferred-format</code> in <code>config.edn</code>.
          </>
        }
      >
        <div class="settings-segment">
          <button
            classList={{ active: (graphMeta()?.preferred_format ?? "md") === "md" }}
            onClick={() => changePreferredFormat("md")}
          >
            Markdown
          </button>
          <button
            classList={{ active: graphMeta()?.preferred_format === "org" }}
            onClick={() => changePreferredFormat("org")}
          >
            Org
          </button>
        </div>
      </Field>

      <Field
        label="Spell checker"
        hint={
          <>
            Underline misspelled words while editing, with right-click suggestions and
            “add to dictionary” (uses the system spell checker). <strong>On by default</strong>,
            like Logseq. Applies live — no restart needed.
          </>
        }
      >
        <Toggle on={spellcheckEnabled()} onClick={() => setSpellcheckEnabled(!spellcheckEnabled())} />
      </Field>

      <Field
        label="Logical outdenting"
        hint={<>Move an outdented block after its parent while leaving following siblings in place. Off (the default) reparents those siblings beneath the moved block. Saved to <code>:editor/logical-outdenting?</code> in <code>config.edn</code>.</>}
      >
        <Toggle on={logicalOutdenting()} onClick={() => changeLogicalOutdenting(!logicalOutdenting())} />
      </Field>

      <Show when={spellcheckEnabled()}>
        <Field
          label="Spellcheck languages"
          hint={
            <>
              Tick the dictionaries to check — several at once is fine (e.g. English + Czech),
              and a word valid in <em>any</em> ticked language isn’t flagged, so bilingual notes
              don’t squiggle. <strong>None ticked → follows your OS locale</strong> (all Logseq
              can do). Dictionaries are discovered from the system; install more with your
              package manager (e.g. <code>hunspell-cs</code>), then Rescan.
            </>
          }
        >
          <div class="spellcheck-dicts">
            <For each={dictRows()}>
              {(row) => (
                <label class="spellcheck-dict">
                  <input
                    type="checkbox"
                    checked={row.selected}
                    onChange={(e) => toggleSpellcheckLanguage(row.code, e.currentTarget.checked)}
                  />
                  <span class="spellcheck-dict-name">{row.name}</span>
                  <code>{row.code}</code>
                  <Show when={!row.installed}>
                    <span class="spellcheck-dict-missing">not installed</span>
                  </Show>
                </label>
              )}
            </For>
            <Show when={dictRows().length === 0}>
              <div class="spellcheck-empty">
                No dictionaries found. Install one (e.g. <code>hunspell-en-us</code>), then Rescan.
              </div>
            </Show>
            <button class="spellcheck-rescan" type="button" onClick={() => void loadDictionaries()}>
              ↻ Rescan
            </button>
          </div>
        </Field>
      </Show>

      <AdvancedSection tab="editor" forceOpen={advancedMatch("editor", props.search)}>
        <Field
          label="Link autocomplete default"
          hint="Controls Enter for non-exact [[name and #name completion. OG adaptive (default) picks the shortest lexical strict-prefix match and puts Create immediately after it; fuzzy-only matches leave Create first. Prefer existing always leads with a match; Prefer exactly what I typed leads with Create. Exact existing names always select the existing page."
        >
          <select
            aria-label="Link autocomplete default"
            value={linkAutocompletePolicy()}
            onChange={(event) => setLinkAutocompletePolicy(event.currentTarget.value as LinkAutocompletePolicy)}
          >
            <option value="adaptive">OG adaptive</option>
            <option value="existing">Prefer existing</option>
            <option value="typed">Prefer exactly what I typed</option>
          </select>
        </Field>

        <Field
          label="Switch to an already-open tab when navigating"
          hint="Plain navigation to a page, journal, or exact zoomed/file-pinned view focuses the matching tab if one is already open. Middle-click and explicit Open in new tab still create another tab."
        >
          <Toggle on={navReuseTabs()} onClick={() => setNavReuseTabs(!navReuseTabs())} />
        </Field>

        <Field
          label="Learn Ctrl+K choices"
          hint="After you deliberately open the same result more than once for a query, Ctrl+K may prefer it only among equally strong matches. History stays on this device and in this graph; saved searches and queries remain deterministic."
        >
          <>
            <Toggle
              on={launcherRankingEnabled()}
              onClick={() => setLauncherRankingEnabled(!launcherRankingEnabled())}
            />
            <button
              type="button"
              class="og-revert"
              onClick={() => {
                resetLauncherRanking(graphMeta()?.root ?? "");
                pushToast("Ctrl+K ranking reset for this graph");
              }}
            >
              Reset ranking
            </button>
          </>
        </Field>

        <OgField
        label="Copy a parent block's sub-blocks"
        hint="When you copy/cut a selected block that has children: ON copies the whole sub-tree; OFF copies only the block(s) you actually selected. Tine defaults to OFF because selecting just the parent and getting its entire tree is surprising."
        ogNote="always copies a selected block's whole sub-tree."
        ogValue={true}
        on={copyIncludeSubtree()}
        onToggle={() => setCopyIncludeSubtree(!copyIncludeSubtree())}
      />

        <OgField
        label="Strip collapsed:: when copying"
        hint="A collapsed block carries a hidden collapsed:: true property (view state, not content). Tine defaults to ON (drops it from copied text for a cleaner paste); OFF keeps it. (id:: is always stripped from copies, like Logseq.)"
        ogNote="keeps collapsed:: in the copied text (only id:: is stripped)."
        ogValue={false}
        on={copyStripCollapsed()}
        onToggle={() => setCopyStripCollapsed(!copyStripCollapsed())}
        />
      </AdvancedSection>

      <OgField
        label="Click a block reference to zoom in"
        hint="Plain-clicking an inline ((block reference)): ON zooms into the referenced block (opens it as its own page, like Logseq); OFF (Tine default) scrolls to it in place and flashes it. Shift-click always opens it in the sidebar."
        ogNote="zooms into the referenced block on click."
        ogValue={true}
        on={refClickZoom()}
        onToggle={() => setRefClickZoom(!refClickZoom())}
      />
    </>
  );
}

function JournalsTab(props: { search: string }): JSX.Element {
  // Quick-capture Enter behaviour (app-level setting, read by the capture window).
  const [captureEnterFiles, setCaptureEnterFiles] = createSignal(false);
  void backend()
    .getCaptureEnterFiles()
    .then(setCaptureEnterFiles)
    .catch(() => {});
  const toggleCaptureEnter = () => {
    const v = !captureEnterFiles();
    setCaptureEnterFiles(v);
    void backend().setCaptureEnterFiles(v).catch(() => {});
  };
  return (
    <>
      <Field
        label="Journal date format"
        hint={
          <>
            How journal dates are displayed and how new <code>[[date]]</code> titles are written.
            Display-only — your journal <em>file names</em> are untouched and existing journals keep
            working. Saved to <code>:journal/page-title-format</code>.
          </>
        }
      >
        <DateFormatSelect />
      </Field>

      <Field
        label="First day of week"
        hint={
          <>
            Starting column of the calendar and the scheduled/deadline date
            pickers. Saved to <code>:start-of-week</code> in <code>config.edn</code>
            (Logseq’s setting), so it travels with the graph.
          </>
        }
      >
        <select
          class="settings-select"
          value={String(graphMeta()?.start_of_week ?? 6)}
          onChange={(e) => changeStartOfWeek(Number(e.currentTarget.value))}
        >
          {/* Logseq convention: 0=Monday … 6=Sunday. */}
          <option value="0">Monday</option>
          <option value="1">Tuesday</option>
          <option value="2">Wednesday</option>
          <option value="3">Thursday</option>
          <option value="4">Friday</option>
          <option value="5">Saturday</option>
          <option value="6">Sunday</option>
        </select>
      </Field>

      <Field
        label="Show carry-over buttons"
        hint="Show the carry buttons next to journal titles. Off → use the right-click menu instead."
      >
        <Toggle on={showCarryButtons()} onClick={() => setShowCarryButtons(!showCarryButtons())} />
      </Field>

      <Field
        label="Carry-over keeps context"
        hint="Move whole blocks that contain an open task (on) vs. pull out just the task (off)."
      >
        <Toggle on={carryKeepsContext()} onClick={() => setCarryKeepsContext(!carryKeepsContext())} />
      </Field>

      <Field label="Carry-over header" hint="Add a “Carried over” heading above carried tasks.">
        <Toggle on={carryHeader()} onClick={() => setCarryHeader(!carryHeader())} />
      </Field>

      <Field
        label="Carry “last N days”"
        hint="N for the “Carry last N days” button on today’s journal (and the Ctrl-K command)."
      >
        <input
          type="number"
          min="1"
          max="3650"
          class="settings-num"
          value={carryDays()}
          onChange={(e) => setCarryDays(Number(e.currentTarget.value))}
        />
      </Field>

      <Field
        label="Task workflow"
        hint={
          <>
            Which markers Tab/⌘↵ cycle through and what new tasks use. Saved to{" "}
            <code>:preferred-workflow</code> in <code>config.edn</code>, so it travels with the
            graph.
          </>
        }
      >
        <div class="settings-segment">
          <button
            classList={{ active: workflow() === "todo" }}
            onClick={() => changeWorkflow("todo")}
          >
            TODO / DOING
          </button>
          <button classList={{ active: workflow() === "now" }} onClick={() => changeWorkflow("now")}>
            NOW / LATER
          </button>
        </div>
      </Field>

      <Field
        label="Time tracking"
        hint={
          <>
            Marker transitions write OG-compatible <code>:LOGBOOK:</code> CLOCK rows. Saved to{" "}
            <code>:feature/enable-timetracking?</code>; seconds mode follows{" "}
            <code>:logbook/settings</code> and is {graphMeta()?.logbook_with_second_support ?? true ? "on" : "off"}.
          </>
        }
      >
        <Toggle on={timetrackingEnabled()} onClick={() => changeTimetrackingEnabled(!timetrackingEnabled())} />
      </Field>

      <JournalTemplateField />

      <AdvancedSection tab="journals" forceOpen={advancedMatch("journals", props.search)}>
        <Field
          label="Quick-capture Enter key"
          hint={`In the quick-capture window: ON → Enter files the capture. OFF → Enter starts a new block; the “Quick-capture: file to today’s journal” shortcut files (default Ctrl+Shift+Enter, remappable under Keyboard shortcuts). Ctrl+Enter stays free for cycling the task marker.`}
        >
          <Toggle on={captureEnterFiles()} onClick={toggleCaptureEnter} />
        </Field>
      </AdvancedSection>

      <Field
        label="Agenda window"
        hint={
          <>
            Today’s “Scheduled &amp; Deadline” list shows items whose scheduled/deadline date is
            within this window; older/further ones are hidden.
          </>
        }
      >
        <input
          type="number"
          min="0"
          max="3650"
          class="settings-num"
          value={agendaDaysBack()}
          onChange={(e) => setAgendaDaysBack(Number(e.currentTarget.value))}
        />
        <span class="settings-hint">days back ·</span>
        <input
          type="number"
          min="0"
          max="3650"
          class="settings-num"
          value={agendaDaysAhead()}
          onChange={(e) => setAgendaDaysAhead(Number(e.currentTarget.value))}
        />
        <span class="settings-hint">days ahead</span>
      </Field>
    </>
  );
}

// Optional graph home page (GH #245/#269): opened automatically in the primary
// tab whenever this graph is opened. Logseq-compatible config.edn owns it;
// picking only offers existing pages and a stale value has an explicit Clear.
function HomePageField(): JSX.Element {
  const root = () => graphMeta()?.root ?? "";
  const [value, setValue] = createSignal<string | null>(null);
  const [missing, setMissing] = createSignal(false);
  const [picking, setPicking] = createSignal(false);
  const [q, setQ] = createSignal("");
  const [dq, setDq] = createSignal("");
  let dqTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    const s = q();
    clearTimeout(dqTimer);
    dqTimer = setTimeout(() => setDq(s), 120);
  });
  onCleanup(() => clearTimeout(dqTimer));
  // Page picker over the existing quick-switch index; home pages are ordinary
  // pages, so journals are filtered out here and at open time.
  const [matchesResource] = createResource(dq, async (s) => {
    if (value() && !picking()) return [];
    const hits = await backend().quickSwitch(s, 8).catch(() => [] as PageEntry[]);
    return (hits ?? []).filter((p) => p.kind === "page");
  });
  const matches = () => readOr(matchesResource, undefined, "home page picker completions");

  createEffect(on(root, async (r) => {
    setValue(null);
    if (!r) {
      setValue("");
      return;
    }
    setValue(await getHomePageSetting(r));
  }));

  // A configured page that no longer resolves (deleted / renamed) is surfaced
  // so the user can clear or replace it; the startup navigation itself just
  // skips it silently.
  createEffect(() => {
    const v = value();
    if (!v || !root()) {
      setMissing(false);
      return;
    }
    let alive = true;
    void backend()
      .getPage(v, "page")
      .then((dto) => {
        if (alive) setMissing(!dto);
      })
      .catch(() => {
        if (alive) setMissing(true);
      });
    onCleanup(() => {
      alive = false;
    });
  });

  const commit = async (name: string) => {
    const r = root();
    if (!r) return;
    if (!(await setHomePageSetting(r, name))) return;
    setValue(name.trim());
    setPicking(false);
    setQ("");
    setDq("");
  };
  const clear = async () => {
    const r = root();
    if (!r) return;
    if (!(await setHomePageSetting(r, null))) return;
    setValue("");
    setQ("");
    setDq("");
  };

  return (
    <Field
      label="Home page"
      hint={
        <>
          Open this page automatically whenever this graph opens. Only existing
          pages can be picked; if the page is deleted or renamed the setting is
          skipped. <code>/</code> deep links and explicit launch intents still
          win over it.
        </>
      }
    >
      <Show when={value() !== null} fallback={<span class="settings-value">…</span>}>
        <Show
          when={picking() || !value()}
          fallback={
            <div style={{ display: "flex", "align-items": "center", gap: "8px", "flex-wrap": "wrap" }}>
              <span class="settings-value" data-home-page-value>{value()}</span>
              <button class="settings-btn" onClick={() => setPicking(true)}>
                Change…
              </button>
              <button class="settings-btn" onClick={() => void clear()}>
                Clear
              </button>
              <Show when={missing()}>
                <span class="settings-hint" data-home-page-missing>
                  Not found in this graph — it may have been deleted or renamed.
                  Clear it or pick a replacement.
                </span>
              </Show>
            </div>
          }
        >
          <div>
            <input
              class="settings-input"
              style={{ width: "260px" }}
              placeholder="Search pages…"
              value={q()}
              onInput={(e) => setQ(e.currentTarget.value)}
              onKeyDown={(e) => {
                const first = (matches() ?? [])[0];
                if (e.key === "Enter" && first) void commit(first.name);
                else if (e.key === "Escape") {
                  setPicking(false);
                  setQ("");
                  setDq("");
                }
              }}
            />
            <For each={matches() ?? []}>
              {(p) => (
                <button class="settings-btn" style={{ "margin-left": "6px" }} onClick={() => void commit(p.name)}>
                  {p.name}
                </button>
              )}
            </For>
            <Show when={value()}>
              <button
                class="settings-btn"
                style={{ "margin-left": "6px" }}
                onClick={() => {
                  setPicking(false);
                  setQ("");
                  setDq("");
                }}
              >
                Cancel
              </button>
            </Show>
          </div>
        </Show>
      </Show>
    </Field>
  );
}

function GraphTab(props: { publishMsg: string; doPublish: () => void }): JSX.Element {
  return (
    <>
      <div class="settings-row">
        <span class="settings-label">Graph</span>
        <div>
          <span class="settings-value mono">{graphMeta()?.root ?? "—"}</span>
          <div style={{ "margin-top": "6px" }}>
            <button class="settings-btn" onClick={() => void switchGraph()}>
              Open another graph…
            </button>
          </div>
        </div>
      </div>

      <HomePageField />

      <div class="settings-row">
        <span class="settings-label">Publish</span>
        <div>
          <button class="settings-btn" onClick={props.doPublish}>
            Export graph to HTML
          </button>
          <Show when={props.publishMsg}>
            <div class="settings-hint" style={{ "margin-top": "4px" }}>
              {props.publishMsg}
            </div>
          </Show>
        </div>
      </div>

      <div class="settings-row" data-setting-label="Query export size limit">
        <span class="settings-label">Query export size limit</span>
        <div>
          <div class="settings-width-control">
            <input
              class="settings-num settings-width-number"
              aria-label="Query export size limit in MiB"
              type="number"
              min={MIN_QUERY_EXPORT_BUDGET_MIB}
              max={MAX_QUERY_EXPORT_BUDGET_MIB}
              step="64"
              value={queryExportBudgetMiB()}
              onChange={(event) => changeQueryExportBudgetMiB(event.currentTarget.valueAsNumber)}
            />
            <span class="settings-width-unit">MiB</span>
            <Show when={queryExportBudgetMiB() !== DEFAULT_QUERY_EXPORT_BUDGET_MIB}>
              <button class="settings-btn" onClick={resetQueryExportBudget}>Reset</button>
            </Show>
          </div>
          <div class="settings-hint" style={{ "margin-top": "4px" }}>
            "Export…" on a query copies the images and files its pages reference into the export
            folder so the folder can be moved anywhere. An export that would copy more than this
            stops instead of leaving a folder with missing files. Saved on this device.
          </div>
        </div>
      </div>
    </>
  );
}

function BackupsTab(props: { search: string }): JSX.Element {
  const [keep, setKeep] = createSignal(12);
  const [list, setList] = createSignal<BackupInfo[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [loading, setLoading] = createSignal(true);
  const [loadError, setLoadError] = createSignal<string | null>(null);
  const ready = () => !loading() && !loadError();

  const refresh = async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const [nextKeep, nextList] = await Promise.all([
        backend().getBackupKeep(),
        backend().listBackups(),
      ]);
      setKeep(nextKeep);
      setList(nextList);
    } catch (e) {
      setList([]);
      setLoadError(String(e));
    } finally {
      setLoading(false);
    }
  };

  // Load the current keep count + snapshot list when this tab mounts, before
  // enabling controls that depend on that data.
  createEffect(() => {
    void refresh();
  });

  const saveKeep = async (n: number) => {
    const v = Math.max(1, Math.min(1000, Math.floor(n) || 12));
    setKeep(v);
    try {
      await backend().setBackupKeep(v);
      void refresh(); // a lower cap prunes immediately on the Rust side
    } catch (e) {
      pushToast(`Couldn't save: ${String(e)}`, "error");
    }
  };

  const restore = async (b: BackupInfo) => {
    if (!ready() || busy()) return;
    setBusy(true);
    const when = fmtStamp(b.stamp);
    // Native GTK confirm — window.confirm silently returns true here, which would
    // overwrite the graph with no prompt.
    if (
      !(await backend().confirm(
        `Restore the snapshot from ${when}?\n\n` +
          `This restores the ${b.files} file(s) in that backup to their original locations. ` +
          `Your current state is snapshotted first, so this is reversible.`
      ))
    ) {
      setBusy(false);
      return;
    }
    setGraphTransitioning(true);
    try {
      // Persist current edits first so the pre-restore safety snapshot captures
      // them (and the reload below doesn't write stale edits over the restore).
      // Abort if a page couldn't be saved rather than discard it.
      if (!(await flushAll())) {
        pushToast("Some pages couldn't be saved — resolve conflicts before restoring.", "error");
        setBusy(false);
        return;
      }
      await backend().restoreBackup(b.stamp);
      const root = graphMeta()?.root ?? "";
      await loadGraphPath(root, { forceRefresh: true, transitionHeld: true }); // rebuild restored files
      pushToast(`Restored snapshot from ${when}`, "success");
      void refresh();
    } catch (e) {
      pushToast(`Restore failed: ${String(e)}`, "error");
    } finally {
      setGraphTransitioning(false);
      setBusy(false);
    }
  };

  return (
    <>
      <div class="settings-hint settings-block">
          Tine snapshots your graph’s Markdown/Org files to a local folder each time it opens
        (outside the graph, so Syncthing never sees it). A safety net against a bad
        write — independent of OG Logseq’s own backups.
      </div>

      <Field label="Snapshots to keep" hint="Oldest snapshots beyond this are pruned.">
        <input
          type="number"
          min="1"
          max="1000"
          class="settings-num"
          value={keep()}
          disabled={!ready() || busy()}
          onChange={(e) => void saveKeep(Number(e.currentTarget.value))}
        />
      </Field>

      <div class="settings-section">
        Available snapshots
        <button
          class="settings-btn"
          style={{ "margin-left": "10px" }}
          disabled={loading() || busy()}
          onClick={() => void refresh()}
        >
          Refresh
        </button>
      </div>
      <Show when={loading()}>
        <div class="settings-hint settings-block" role="status">
          Loading snapshot settings…
        </div>
      </Show>
      <Show when={loadError()}>
        {(error) => (
          <div class="settings-hint settings-block" role="alert">
            Couldn&apos;t load backup settings: {error()}
            <button class="settings-btn" style={{ "margin-left": "10px" }} onClick={() => void refresh()}>
              Retry
            </button>
          </div>
        )}
      </Show>
      <Show
        when={ready() && list().length}
        fallback={
          <Show when={ready()}>
            <div class="settings-hint settings-block">No snapshots yet.</div>
          </Show>
        }
      >
        <div class="settings-backups">
          <For each={list()}>
            {(b) => (
              <div class="settings-backup-row">
                <span class="settings-backup-when">{fmtStamp(b.stamp)}</span>
                <span class="settings-backup-files mono">{b.files} files</span>
                <button class="settings-btn" disabled={!ready() || busy()} onClick={() => void restore(b)}>
                  Restore
                </button>
              </div>
            )}
          </For>
        </div>
      </Show>

      {/* Concord P5 — the one user-visible conflict policy switch. */}
      <div class="settings-section" style={{ "margin-top": "18px" }}>
        External changes
      </div>
      <Field
        label="Always ask before applying an external change"
        hint="By default a page you have open with nothing unsaved updates silently when another editor or a sync tool changes its file — the same as VS Code. Turn this on and Tine holds the change instead: the page keeps showing what you were reading and offers Reload from disk / Keep mine. Everything that already asks keeps asking — a page with unsaved edits, or one being edited, is unaffected either way."
      >
        <Toggle
          on={conflictPolicyAlwaysAsk()}
          onClick={() => setConflictPolicyAlwaysAsk(!conflictPolicyAlwaysAsk())}
        />
      </Field>

      <SettingsConflictPanels />
    </>
  );
}

/** The conflict INVENTORY: everything on disk that needs the user's judgement,
 *  plus the actions only this surface can offer (discard a copy, rename journal
 *  files, reconcile a duplicate day). Resolution itself happens at the page.
 *  Exported so it can be mounted on its own in tests. */
export function SettingsConflictPanels(): JSX.Element {
  return (
    <>
      <JournalFilenamePanel />
      <JournalConflictsPanel />
      <ConflictOverviewPointer />
    </>
  );
}

// Orphaned-media cleanup: scan (on demand — it parses the whole graph) for
// assets/ files no block links to, and let the user move them to the recoverable
// trash. Tine never auto-deletes media (a deleted block keeps its files), so this
// is how unused media gets cleaned up.
// One file in a duplicate-day conflict. Click the name to reveal its full
// contents; the action buttons let you reach and reconcile it (#21): Open
// navigates to THIS specific file (editable, saves back to itself), Merge folds a
// stray into the canonical day, Rename rescues it as a normal page, Trash removes
// the redundant one (recoverable).
// Duplicate journal days: a date that resolves to >1 file (e.g. a date-stem file
// plus a title-named one, usually from a date-format change). Tine never
// auto-merges, so list each file with reconcile actions (Open/Merge/Rename/Trash).
function JournalConflictsPanel(): JSX.Element {
  void refreshJournalConflicts(); // refresh when the Backups tab opens
  // Run a reconcile op, toast the outcome, and refresh the (now-changed) list.
  const reconcile = async (op: () => Promise<void>, ok: string) => {
    try {
      await op();
      pushToast(ok, "success");
      await refreshJournalConflicts();
    } catch (e) {
      pushToast(`Couldn’t do that: ${String(e)}`, "error");
    }
  };
  const trashFile = async (name: string) => {
    if (
      !(await backend().confirm(
        `Move the journal file “${name}” to the trash?\n\n` +
          `It's a duplicate of another file for the same day. It moves to logseq/.tine-trash (recoverable).`
      ))
    )
      return;
    await reconcile(() => backend().trashJournalFile(name), `Moved ${name} to trash`);
  };
  const openFileRow = (file: JournalFile, title: string) => {
    openFile(file.path, title, "journal");
    closeSettings();
  };
  const openDay = (title: string) => {
    openPage(title, "journal");
    closeSettings();
  };
  return (
    <Show when={journalConflicts().length}>
      <div class="settings-section" style={{ "margin-top": "18px" }}>
        Duplicate journal days
      </div>
      <div class="settings-hint settings-block">
        These days have more than one file (e.g. a <code>2026_06_26.org</code> and a
        title-named <code>Friday, 26-06-2026.org</code>) — usually left over from changing the
        date format. <strong>Open</strong> reaches a file directly (it's editable and saves back
        to itself); <strong>Merge</strong> folds a stray into the canonical day;{" "}
        <strong>Rename</strong> turns it into a normal page; <strong>Trash</strong> removes the
        redundant one (recoverable).
      </div>
      <For each={journalConflicts()}>
        {(c) => {
          const canonical = c.files.find((f) => f.canonical);
          return (
            <div class="settings-block">
              <button class="settings-asset-name" onClick={() => openDay(c.title)}>
                {c.title} →
              </button>
              <For each={c.files}>
                {(f) => (
                  <ConflictFileRow
                    file={f}
                    onOpen={() => openFileRow(f, c.title)}
                    onMerge={!f.canonical && canonical ? () => void reconcile(
                      () => backend().mergePages(f.path, canonical.path),
                      `Merged ${f.name} into ${canonical.name}`
                    ) : undefined}
                    onRename={(n) => void reconcile(
                      () => backend().renameFileToPage(f.path, n),
                      `Renamed ${f.name} → ${n}`
                    )}
                    onTrash={() => void trashFile(f.name)}
                  />
                )}
              </For>
            </div>
          );
        }}
      </For>
    </Show>
  );
}

// Journal files whose names don't round-trip to a date (a title-named
// "Jun 18th, 2026.md" left behind by a date-format change or another tool).
// Such a file can't be parsed back to its day, so the day looks empty in the
// feed — a real repair. But it is a rename in a tree the user owns, and until
// Concord P5 Tine performed it silently at every graph open, which lands as an
// unrequested diff in a graph kept in git (invariant 4, write-shyness). It is
// now proposed here and applied only on this button, after a snapshot.
function JournalFilenamePanel(): JSX.Element {
  const [pendingResource, { refetch }] = createResource(async () => {
    // Best-effort like the other inventories: an absent panel beats a broken
    // Backups tab.
    try {
      return await backend().listJournalFilenameMigrations();
    } catch {
      return [];
    }
  });
  // The panel's own comment: an absent panel beats a broken Backups tab.
  const pending = () => readOr(pendingResource, undefined, "journal filename migrations");
  const [busy, setBusy] = createSignal(false);
  const apply = async () => {
    const files = pending() ?? [];
    if (
      !(await backend().confirm(
        `Rename ${files.length} journal file${files.length === 1 ? "" : "s"} to their date names?\n\n` +
          `A snapshot is taken first, so the original names stay in Backups & recovery. ` +
          `Nothing is overwritten — a file whose date name is already taken is left alone.`
      ))
    )
      return;
    setBusy(true);
    try {
      const n = await backend().applyJournalFilenameMigrations();
      pushToast(`Renamed ${n} journal file${n === 1 ? "" : "s"}`, "success");
      void refetch();
      await refreshJournalConflicts();
    } catch (e) {
      pushToast(`Couldn’t rename them: ${String(e)}`, "error");
    } finally {
      setBusy(false);
    }
  };
  return (
    <Show when={(pending() ?? []).length}>
      <div class="settings-section" style={{ "margin-top": "18px" }}>
        Journal files named by title
      </div>
      <div class="settings-hint settings-block">
        These journal files aren’t named after their date, so Tine can’t place them in the journal
        feed and their days look empty. Renaming them fixes that — but it changes files you own, so
        Tine never does it on its own. Your version-control tool will see these as renames.
      </div>
      <For each={pending() ?? []}>
        {(m) => (
          <div class="settings-block journal-rename-row">
            <span class="journal-conflict-preview mono">{m.from}</span>
            <span class="journal-conflict-preview mono">→ {m.to}</span>
          </div>
        )}
      </For>
      <span class="journal-conflict-actions">
        <button class="settings-btn" disabled={busy()} onClick={() => void apply()}>
          {busy() ? "Renaming…" : "Rename to date names"}
        </button>
      </span>
    </Show>
  );
}

// Concord inventory (GH #536): sync conflict copies, marker-bearing files and
// the copy whose page is gone used to be listed here AND walked from the badge.
// They now live on one surface, the Conflicts overview, which the `N conflicts`
// badge opens; Settings only points there. Two lists over the same queue drift,
// which is ADR 0057's reason for removing the old in-Settings merge dialog.
function ConflictOverviewPointer(): JSX.Element {
  void refreshSyncConflicts(); // refresh when the Backups tab opens
  const count = () => conflictQueue().length + syncConflicts().filter((c) => !c.base_path).length;
  return (
    <Show when={count()}>
      <div class="settings-section" style={{ "margin-top": "18px" }}>
        Conflicts
      </div>
      <div class="settings-hint settings-block">
        {count()} {count() === 1 ? "item needs" : "items need"} a decision: sync conflict copies,
        version-control merge markers, or unsaved drafts. The Conflicts page lists them, with
        block counts and <strong>Discard copy</strong> for sync copies; the{" "}
        <strong>N conflicts</strong> badge in the sidebar opens it too.
      </div>
      <span class="journal-conflict-actions">
        <button
          class="settings-btn"
          onClick={() => {
            openConflicts();
            closeSettings();
          }}
        >
          Open conflicts
        </button>
      </span>
    </Show>
  );
}

function FilesTab(props: { search: string }): JSX.Element {
  // Unknown platforms fail closed to "not desktop": a section whose backend
  // command errors on this platform must not be offered.
  const [filesPlatformResource] = createResource(async () => {
    try {
      return await platformKind();
    } catch {
      return undefined;
    }
  });
  // Fails closed to "not desktop", exactly as the fetcher's own catch does.
  const filesPlatform = () => readOr(filesPlatformResource, undefined, "files platform");
  // Live preview of the asset-name template, on a fixed sample so every token is
  // visible (and the example doesn't jitter by the second). Shows both a named
  // drag/insert and a clipboard paste (which has no name → timestamp fallback).
  const sampleDate = new Date(2030, 0, 2, 3, 4, 5);
  const dragExample = () => formatAssetName(assetNameFormat(), "Holiday Photo.JPG", sampleDate);
  const pasteExample = () => formatAssetName(assetNameFormat(), undefined, sampleDate);
  // File-watch mechanism (device-local). Loaded from the backend on mount.
  const [watchMode, setWatchMode] = createSignal<"inotify" | "poll">("inotify");
  void backend()
    .getWatchMode()
    .then((m) => setWatchMode(m === "poll" ? "poll" : "inotify"))
    .catch(() => {});
  const changeWatchMode = (m: "inotify" | "poll") => {
    if (m === watchMode()) return;
    setWatchMode(m);
    void backend().setWatchMode(m).catch(() => {});
  };
  return (
    <>
      <div class="settings-section">Asset names</div>
      <Field
        label="New asset filename"
        hint={
          <>
            How files are named when you paste, drag, or insert media into{" "}
            <code>assets/</code>. Tokens: <code>%assetname</code> (the original file’s name),{" "}
            <code>%ext</code>, <code>%yyyymmdd</code>, <code>%hhmmss</code> — also granular{" "}
            <code>%yyyy</code> <code>%MM</code> <code>%dd</code> <code>%HH</code> <code>%mm</code>{" "}
            <code>%ss</code>. Anything else is literal. A clipboard paste has no name, so{" "}
            <code>%assetname</code> falls back to a timestamp. Device-local; Logseq keeps the
            original name for dragged files.
          </>
        }
      >
        <input
          type="text"
          class="settings-input mono"
          value={assetNameFormat()}
          spellcheck={false}
          onChange={(e) => setAssetNameFormat(e.currentTarget.value)}
        />
      </Field>
      <div class="asset-name-extras">
        <div class="settings-segment">
          <button
            classList={{ active: assetNameFormat() === DEFAULT_ASSET_NAME_FORMAT }}
            onClick={() => setAssetNameFormat(DEFAULT_ASSET_NAME_FORMAT)}
          >
            Original name
          </button>
          <button
            classList={{ active: assetNameFormat() === STAMPED_ASSET_NAME_FORMAT }}
            onClick={() => setAssetNameFormat(STAMPED_ASSET_NAME_FORMAT)}
          >
            Date + name
          </button>
        </div>
        <div class="asset-name-preview">
          <div>
            Drag <code class="mono">Holiday Photo.JPG</code> →{" "}
            <code class="mono">{dragExample()}</code>
          </div>
          <div>
            Paste an image → <code class="mono">{pasteExample()}</code>
          </div>
        </div>
      </div>

      <Field
        label="Watch for external edits"
        hint={
          <>
            How Tine notices changes made outside it (OG Logseq, Syncthing, an
            editor). <b>Live</b> uses the OS file watcher (inotify) — no idle CPU
            wakeups; the right choice on a normal local disk. <b>Poll</b> rescans
            every 3 seconds — only needed on filesystems where inotify is
            unreliable (some network/NFS mounts). Saved per device.
          </>
        }
      >
        <div class="settings-segment">
          <button
            classList={{ active: watchMode() === "inotify" }}
            onClick={() => changeWatchMode("inotify")}
          >
            Live (inotify)
          </button>
          <button
            classList={{ active: watchMode() === "poll" }}
            onClick={() => changeWatchMode("poll")}
          >
            Poll (3s)
          </button>
        </div>
      </Field>

      {/* "Desktop only" is a claim about the backend, not a style: both
          `edit_asset_external` and `detect_media_editor` refuse on mobile
          (src-tauri/src/commands.rs, `#[cfg(not(desktop))]`). Gate the section
          rather than only saying so in its hint text. `MediaEditorsSection` is
          the whole content of Files → Advanced today; if a non-desktop item is
          ever added there, move this guard onto the section itself.
          Pinned by Settings.mediaEditors.platform.test.tsx. */}
      <Show when={filesPlatform() === "desktop"}>
        <AdvancedSection tab="files" forceOpen={advancedMatch("files", props.search)}>
          <MediaEditorsSection />
        </AdvancedSection>
      </Show>

      <AssetsTab />
    </>
  );
}

// Configurable external editors for diagram assets (GH #38): drawio, Excalidraw.
// Each registry entry gets one command row; empty = the OS default opener. drawio
// offers an autodetect probe.
function MediaEditorsSection(): JSX.Element {
  const [detecting, setDetecting] = createSignal<string | null>(null);
  const autodetect = async (id: string, settingKey: string) => {
    setDetecting(id);
    try {
      const cmd = await backend().detectMediaEditor(id);
      if (cmd) {
        setMediaEditorCommand(settingKey, cmd);
        pushToast(`Found: ${cmd}`, "success");
      } else {
        pushToast("Couldn’t find it — set the command manually.", "error");
      }
    } catch {
      pushToast("Autodetect failed.", "error");
    } finally {
      setDetecting(null);
    }
  };
  return (
    <>
      <div class="settings-section">Diagram editors</div>
      <div class="settings-hint" style={{ "margin-bottom": "8px" }}>
        Edit diagram assets in your own installed app. A <code class="mono">/drawio</code> command
        creates a new editable <code>.drawio.svg</code>; hovering any matching image shows an
        “Edit in …” button. Leave a command blank to use the system default opener. A{" "}
        <code class="mono">{"{}"}</code> in the command is replaced by the file path (otherwise it’s
        appended). Wrap a program or argument containing spaces in double quotes. Desktop only;
        device-local.
      </div>
      <For each={MEDIA_EDITORS}>
        {(ed) => (
          <Field label={ed.settingLabel}>
            <div class="media-editor-row">
              <input
                type="text"
                class="settings-input mono"
                placeholder="system default opener"
                value={mediaEditorCommand(ed.settingKey)}
                spellcheck={false}
                onChange={(e) => setMediaEditorCommand(ed.settingKey, e.currentTarget.value)}
              />
              <Show when={ed.detectable}>
                <button
                  class="settings-btn"
                  disabled={detecting() === ed.id}
                  onClick={() => void autodetect(ed.id, ed.settingKey)}
                >
                  {detecting() === ed.id ? "Detecting…" : "Autodetect"}
                </button>
              </Show>
            </div>
          </Field>
        )}
      </For>
    </>
  );
}

function AssetsTab(): JSX.Element {
  const [list, setList] = createSignal<AssetInfo[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [scanned, setScanned] = createSignal(false);
  const [trashInfo, setTrashInfo] = createSignal<TrashStats>({
    count: 0,
    bytes: 0,
    pages: 0,
    journals: 0,
    conflicts: 0,
    other: 0,
  });

  const fmtSize = (n: number) =>
    n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : n >= 1024 ? `${Math.round(n / 1024)} KB` : `${n} B`;
  const fmtDate = (secs: number | null) =>
    secs == null
      ? ""
      : new Date(secs * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
  const total = () => list().reduce((s, a) => s + a.size, 0);
  const protectedTrashCount = () =>
    trashInfo().pages + trashInfo().journals + trashInfo().conflicts + trashInfo().other;
  const protectedTrashLabel = () =>
    [
      trashInfo().pages ? `${trashInfo().pages} page${trashInfo().pages === 1 ? "" : "s"}` : "",
      trashInfo().journals ? `${trashInfo().journals} journal${trashInfo().journals === 1 ? "" : "s"}` : "",
      trashInfo().conflicts ? `${trashInfo().conflicts} conflict${trashInfo().conflicts === 1 ? "" : "s"}` : "",
      trashInfo().other ? `${trashInfo().other} other` : "",
    ]
      .filter(Boolean)
      .join(", ");

  const refreshTrash = async () => {
    try {
      setTrashInfo(await backend().assetTrashStats());
    } catch {
      /* trash stats are best-effort */
    }
  };

  const refresh = async () => {
    setBusy(true);
    try {
      // Persist edits first so a just-deleted block's media counts as orphaned
      // (and a just-inserted one counts as referenced).
      await flushAll();
      setList(await backend().listOrphanAssets());
      setScanned(true);
      await refreshTrash();
    } catch (e) {
      pushToast(`Scan failed: ${String(e)}`, "error");
    } finally {
      setBusy(false);
    }
  };

  const open = async (a: AssetInfo) => {
    try {
      await backend().openAsset(a.name);
    } catch (e) {
      pushToast(`Couldn’t open ${a.name}: ${String(e)}`, "error");
    }
  };

  const trash = async (a: AssetInfo) => {
    // No confirm: the file only moves to the recoverable logseq/.tine-trash, so
    // trashing a batch stays fast. (Empty-trash, which is permanent, still asks.)
    try {
      await backend().trashAsset(a.name);
      setList((l) => l.filter((x) => x.name !== a.name));
      pushToast(`Moved ${a.name} to trash`, "success");
      await refreshTrash();
    } catch (e) {
      pushToast(`Couldn’t trash: ${String(e)}`, "error");
    }
  };

  const emptyTrash = async () => {
    const info = trashInfo();
    if (!info.count) return;
    if (
      !(await backend().confirm(
        `Permanently delete ${info.count} asset file${info.count === 1 ? "" : "s"} (${fmtSize(info.bytes)}) in the trash?\n\n` +
          `This cannot be undone. Page, journal, and conflict recovery files in logseq/.tine-trash will be kept.`
      ))
    )
      return;
    try {
      const n = await backend().emptyAssetTrash();
      setTrashInfo((t) => ({ ...t, count: 0, bytes: 0 }));
      pushToast(`Emptied asset trash (${n} file${n === 1 ? "" : "s"})`, "success");
    } catch (e) {
      pushToast(`Couldn’t empty trash: ${String(e)}`, "error");
    }
  };

  return (
    <>
      <div class="settings-section">
        Orphaned media
        <button class="settings-btn" style={{ "margin-left": "10px" }} disabled={busy()} onClick={() => void refresh()}>
          {busy() ? "Scanning…" : scanned() ? "Rescan" : "Scan for orphans"}
        </button>
        <Show when={trashInfo().count > 0}>
          <button
            class="settings-btn settings-btn-danger"
            style={{ "margin-left": "8px" }}
            onClick={() => void emptyTrash()}
            title="Permanently delete asset files in logseq/.tine-trash"
          >
            Empty asset trash ({trashInfo().count})
          </button>
        </Show>
      </div>
      <Show when={protectedTrashCount() > 0}>
        <div class="settings-hint settings-block">
          Protected recovery trash kept: {protectedTrashLabel()}
        </div>
      </Show>
      <div class="settings-hint settings-block">
        Files in <code>assets/</code> that no block links to. Deleting a block never
        deletes its media (a safety net), so unused files can accumulate — review and
        trash them here. Trashed files move to <code>logseq/.tine-trash</code> (recoverable);
        click a name to open it in your default app.
      </div>
      <Show when={scanned()}>
        <Show
          when={list().length}
          fallback={<div class="settings-hint settings-block">No orphaned media — every asset is referenced. 🎉</div>}
        >
          <div class="settings-hint settings-block">
            {list().length} orphan{list().length === 1 ? "" : "s"} · {fmtSize(total())} reclaimable
          </div>
          <div class="settings-backups">
            <For each={list()}>
              {(a) => (
                <div class="settings-asset-row">
                  <button class="settings-asset-name mono" title="Open in the default app" onClick={() => void open(a)}>
                    {a.name}
                  </button>
                  <span class="settings-asset-date">{fmtDate(a.modified)}</span>
                  <span class="settings-backup-files mono">{fmtSize(a.size)}</span>
                  <button class="settings-btn" onClick={() => void trash(a)}>
                    Trash
                  </button>
                </div>
              )}
            </For>
          </div>
        </Show>
      </Show>
    </>
  );
}

// Build timestamp (stamped by Vite at bundle time) — handy for confirming the
// running binary is the latest, not a stale Syncthing copy.
function buildStamp(): string {
  try {
    return new Date(__BUILD_TIME__).toLocaleString();
  } catch {
    return __BUILD_TIME__;
  }
}

// "2026-06-17_14-30-05" (UTC) → a readable local timestamp.
function fmtStamp(s: string): string {
  const m = s.match(/^(\d{4})-(\d{2})-(\d{2})_(\d{2})-(\d{2})-(\d{2})$/);
  if (!m) return s;
  const d = new Date(`${m[1]}-${m[2]}-${m[3]}T${m[4]}:${m[5]}:${m[6]}Z`);
  return isNaN(d.getTime()) ? s : d.toLocaleString();
}
