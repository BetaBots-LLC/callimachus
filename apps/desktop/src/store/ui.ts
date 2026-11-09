import { create } from "zustand";
import { api, type SearchFilters, type SourceKind } from "../lib/api";

/** The app's views. Today, Search, Chat and Projects are tabs (App.tsx TABS, in that order);
 *  Stats and Settings are header icons. Ask lives inside Chat (its "Ask" mode); the Coach and
 *  Knowledge tabs are gone: the heatmap is in Stats, errors and TODOs on the Project page.
 *  Report ("Your agents, so far") has no tab: Today's card and a Stats link open it. */
export type View = "today" | "search" | "chat" | "projects" | "stats" | "settings" | "report";

/** The sections of the Settings view (its left-hand navigation), in display order. */
export type SettingsSection =
  | "general"
  | "engine"
  | "agents"
  | "sources"
  | "index"
  | "export"
  | "updates";

/** Section ids from before "AI engine" merged Providers and Knowledge. Links and toasts that
 *  still name them (and older mock URLs) land on the merged section. */
export type SettingsSectionAlias = "providers" | "knowledge";
const SECTION_ALIASES: Record<SettingsSectionAlias, SettingsSection> = {
  providers: "engine",
  knowledge: "engine",
};
export function resolveSettingsSection(
  section: SettingsSection | SettingsSectionAlias,
): SettingsSection {
  return (SECTION_ALIASES as Record<string, SettingsSection>)[section] ?? section;
}

/** What `openSearch` can set: any search filter plus the query. Keys left out keep their
 *  current value, so a caller narrows the search without clearing what the user picked. */
export type OpenSearchOptions = Partial<SearchFilters> & { query?: string };
/** The same shape under the name the Search scope code uses. */
export type OpenSearchFilters = OpenSearchOptions;

/** The Search date presets. "custom" is a range set from elsewhere (a Stats heatmap day). */
export type DatePreset = "24h" | "7d" | "30d" | "all" | "custom";

export const DATE_PRESET_SECS: Record<"24h" | "7d" | "30d", number> = {
  "24h": 86_400,
  "7d": 7 * 86_400,
  "30d": 30 * 86_400,
};

/** The range a search runs with, in unix seconds. A preset is relative to the moment the
 *  search runs, not to the click: the store lasts the session, so "24h" picked yesterday
 *  would otherwise quietly cover 48 hours today. A custom range is the stored one. */
export function searchRange(
  s: Pick<UiState, "datePreset" | "after" | "before">,
  nowSecs = Math.floor(Date.now() / 1000),
): { after: number | null; before: number | null } {
  if (s.datePreset === "custom") return { after: s.after, before: s.before };
  if (s.datePreset === "all") return { after: null, before: null };
  return { after: nowSecs - DATE_PRESET_SECS[s.datePreset], before: null };
}

interface UiState {
  view: View;
  setView: (v: View) => void;
  settingsSection: SettingsSection; // which Settings section is open; survives leaving the view
  setSettingsSection: (s: SettingsSection) => void;
  // Jump straight to a section; the pre-merge ids ("providers", "knowledge") still work.
  openSettings: (section?: SettingsSection | SettingsSectionAlias) => void;
  /** Switch to Search with these filters (and query) applied, e.g. a Stats heatmap day. */
  openSearch: (filters?: OpenSearchOptions) => void;
  /** Switch to Search with this thread open, landing on `messageId` when given. */
  openThread: (id: number, messageId?: number | null) => void;
  query: string;
  sources: SourceKind[]; // empty = all sources
  includeSubagents: boolean;
  hybrid: boolean;
  // The user flipped the semantic switch this session, so the automatic default (on once
  // 90% of messages are embedded) no longer overrides their choice.
  hybridChosen: boolean;
  // Search scope beyond the source chips: one project and a time range in unix seconds
  // (null = unscoped); SearchBar and ResultsList read them. `project` is a project key (a
  // repo path), matched as the backend's substring filter. It defaults to the remembered
  // last project until the user picks or clears one this session (`projectChosen`);
  // `scopeReady` is false until that default has been looked up, so the first results are
  // never fetched unscoped and then replaced.
  project: string | null;
  projectChosen: boolean;
  scopeReady: boolean;
  // `after` / `before` hold only a custom range (null for a preset): a preset's bound is
  // derived when the search runs (`searchRange`), and the results key holds the preset name,
  // so it stays stable while the user reads.
  datePreset: DatePreset;
  after: number | null;
  before: number | null;
  setProject: (project: string | null) => void;
  /** The remembered default scope arrived (null: none, or an older backend). */
  applyDefaultProject: (project: string | null) => void;
  setDatePreset: (preset: Exclude<DatePreset, "custom">) => void;
  /** Turn semantic on by default, unless the user chose a setting this session. */
  autoHybrid: () => void;
  starredOnly: boolean; // collection filter: only starred threads
  selectedTags: string[]; // collection filter: threads having ANY of these tags
  selectedThreadId: number | null;
  targetMessageId: number | null; // scroll-to target when opening from a search hit
  // Every matched message of the opened thread, in rank order (the first is
  // targetMessageId), for the thread's match navigation. Empty when not opened from a
  // search; cleared when the query changes.
  matchMessageIds: number[];
  // Bumped by every selectThread, so opening the thread that is already open (clicking its
  // row again) lands on the hit again instead of being a no-op.
  openSeq: number;
  // The keyboard-highlighted results row (-1: none). SearchBar moves it with the arrow
  // keys while focus stays in the box; ResultsList draws it and keeps it scrolled into view.
  highlightedIndex: number;
  setHighlightedIndex: (i: number) => void;
  // Fresh rows ResultsList shows for the committed query (what the arrow keys clamp to).
  // 0 while the list loads, errors, or still shows the previous query's rows as placeholder.
  resultCount: number;
  setResultCount: (n: number) => void;
  // Enter in the search box before the query's rows arrived: ResultsList opens row 0 once
  // they do (or drops the request if nothing matched). Typing or opening a thread cancels.
  openFirstPending: boolean;
  setOpenFirstPending: (pending: boolean) => void;
  // ResultsList is showing "Nothing indexed yet": its own Reindex is then the one action,
  // and the thread pane has nothing to point at.
  indexEmpty: boolean;
  setIndexEmpty: (empty: boolean) => void;
  selectedProject: string | null; // active project in the Projects view
  openProject: (project: string) => void; // jump to a project's page
  commandOpen: boolean; // the Cmd/Ctrl-K command palette
  setCommandOpen: (open: boolean) => void;
  setQuery: (q: string) => void;
  toggleSource: (s: SourceKind) => void;
  toggleSubagents: () => void;
  toggleHybrid: () => void;
  toggleStarredOnly: () => void;
  toggleTag: (t: string) => void;
  clearCollectionFilters: () => void;
  clearSources: () => void;
  selectThread: (
    id: number | null,
    messageId?: number | null,
    matchMessageIds?: readonly number[],
  ) => void;
  // When the watcher last reported new transcripts that an on-screen search was NOT
  // refetched for (main.tsx). ResultsList offers a refresh while its data is older.
  searchActivityAt: number;
  noteSearchActivity: () => void;
}

// Subscribe with selectors (e.g. useUi((s) => s.query)), never the whole store.
export const useUi = create<UiState>((set, get) => ({
  // Today is the launch view. App.tsx switches to Search when the index is empty, since the
  // setup checklist renders there.
  view: "today",
  setView: (view) => set({ view }),
  settingsSection: "general",
  setSettingsSection: (settingsSection) => set({ settingsSection }),
  openSettings: (section) =>
    set((state) => ({
      view: "settings",
      settingsSection: section ? resolveSettingsSection(section) : state.settingsSection,
    })),
  query: "",
  sources: [],
  includeSubagents: false,
  hybrid: false,
  hybridChosen: false,
  project: null,
  projectChosen: false,
  scopeReady: false,
  datePreset: "all",
  after: null,
  before: null,
  setProject: (project) => set({ project, projectChosen: true, scopeReady: true }),
  applyDefaultProject: (project) =>
    set((state) => (state.projectChosen ? { scopeReady: true } : { project, scopeReady: true })),
  setDatePreset: (datePreset) => set({ datePreset, after: null, before: null }),
  autoHybrid: () => {
    const state = get();
    if (state.hybridChosen || state.hybrid) return;
    // No warm-up here, unlike toggleHybrid: this runs at launch, and loading ONNX for a
    // window that never searches would undo the idle-memory work. SearchBar warms the model
    // on the first keystroke of the session instead.
    set({ hybrid: true });
  },
  openSearch: (f = {}) => {
    const wasHybrid = get().hybrid;
    set((state) => {
      // A new scope is a new list: the highlight and the open thread's match list refer to
      // rows that may not be in it.
      const next: Partial<UiState> = { view: "search", highlightedIndex: -1 };
      if (f.query !== undefined && f.query !== state.query) {
        Object.assign(next, {
          query: f.query,
          matchMessageIds: [],
          targetMessageId: null,
        });
      }
      if (f.sources) next.sources = [...f.sources] as SourceKind[];
      if (f.project !== undefined) {
        Object.assign(next, { project: f.project, projectChosen: true, scopeReady: true });
      }
      if (f.after !== undefined || f.before !== undefined) {
        const after = f.after ?? null;
        const before = f.before ?? null;
        Object.assign(next, {
          after,
          before,
          datePreset: after === null && before === null ? "all" : "custom",
        });
      }
      if (f.includeSubagents !== undefined) next.includeSubagents = f.includeSubagents;
      if (f.hybrid !== undefined) Object.assign(next, { hybrid: f.hybrid, hybridChosen: true });
      if (f.starred !== undefined) next.starredOnly = f.starred === true;
      if (f.tags) next.selectedTags = [...f.tags];
      return next;
    });
    // Same as toggleHybrid: turning semantic on loads the model in the background now.
    if (f.hybrid && !wasHybrid) api.warmSemantic().catch(() => {});
  },
  openThread: (id, messageId) => {
    get().selectThread(id, messageId);
    set({ view: "search" });
  },
  starredOnly: false,
  selectedTags: [],
  selectedThreadId: null,
  targetMessageId: null,
  matchMessageIds: [],
  highlightedIndex: -1,
  setHighlightedIndex: (highlightedIndex) => set({ highlightedIndex }),
  resultCount: 0,
  setResultCount: (resultCount) => set({ resultCount }),
  openFirstPending: false,
  setOpenFirstPending: (openFirstPending) => set({ openFirstPending }),
  indexEmpty: false,
  setIndexEmpty: (indexEmpty) => set({ indexEmpty }),
  selectedProject: null,
  openProject: (project) => set({ selectedProject: project, view: "projects" }),
  commandOpen: false,
  setCommandOpen: (commandOpen) => set({ commandOpen }),
  // A new query makes the open thread's match list and its landing target stale: they are
  // no longer what the list shows, and ThreadView shows no Match stepper once both are
  // cleared. The highlight restarts too (ResultsList also resets it on a filter change).
  setQuery: (query) =>
    set((state) =>
      state.query === query
        ? {}
        : { query, matchMessageIds: [], targetMessageId: null, highlightedIndex: -1 },
    ),
  toggleSource: (s) =>
    set((state) => ({
      sources: state.sources.includes(s)
        ? state.sources.filter((x) => x !== s)
        : [...state.sources, s],
    })),
  toggleSubagents: () => set((state) => ({ includeSubagents: !state.includeSubagents })),
  toggleHybrid: () => {
    const hybrid = !get().hybrid;
    // Turning semantic on: load the model now, in the background, so the first hybrid
    // query does not pay the ONNX load (or fall back to keyword-only while it runs).
    // An older backend without the command just rejects; nothing depends on it.
    if (hybrid) api.warmSemantic().catch(() => {});
    set({ hybrid, hybridChosen: true });
  },
  toggleStarredOnly: () => set((state) => ({ starredOnly: !state.starredOnly })),
  toggleTag: (t) =>
    set((state) => ({
      selectedTags: state.selectedTags.includes(t)
        ? state.selectedTags.filter((x) => x !== t)
        : [...state.selectedTags, t],
    })),
  clearCollectionFilters: () => set({ starredOnly: false, selectedTags: [] }),
  clearSources: () => set({ sources: [] }),
  openSeq: 0,
  selectThread: (id, messageId, matchMessageIds) =>
    set((state) => ({
      openSeq: state.openSeq + 1,
      selectedThreadId: id,
      targetMessageId: messageId ?? null,
      matchMessageIds: matchMessageIds ? [...matchMessageIds] : [],
      openFirstPending: false,
    })),
  searchActivityAt: 0,
  noteSearchActivity: () => set({ searchActivityAt: Date.now() }),
}));

/** The search box's DOM id (SearchBar renders it). */
export const SEARCH_INPUT_ID = "search-input";
/** The results listbox's DOM id, and each row's option id (aria-activedescendant). */
export const RESULTS_LIST_ID = "search-results";
export const resultOptionId = (index: number) => `search-result-${index}`;

/** Put focus back in the search box, for example on Esc from the thread pane. Returns
 *  whether it found the box (it is not in the DOM while Onboarding shows). */
export function focusSearch(): boolean {
  const el = document.getElementById(SEARCH_INPUT_ID);
  if (!(el instanceof HTMLInputElement)) return false;
  el.focus();
  return true;
}

/** How SearchBar opens the highlighted row on Enter (`open`), or the top result when none
 *  is highlighted (`openTop`: the first thread row, below any facts). ResultsList installs
 *  both while it is mounted; a plain object rather than store state, since they are
 *  callbacks, not data. */
export const resultsNav: { open: (index: number) => void; openTop: () => void } = {
  open: () => {},
  openTop: () => {},
};
