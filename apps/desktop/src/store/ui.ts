import { create } from "zustand";
import { api, type SourceKind } from "../lib/api";

export type View =
  | "search"
  | "chat"
  | "coach"
  | "knowledge"
  | "ask"
  | "projects"
  | "stats"
  | "settings";

/** The sections of the Settings view (its left-hand navigation), in display order. */
export type SettingsSection =
  | "general"
  | "providers"
  | "agents"
  | "sources"
  | "index"
  | "knowledge"
  | "export"
  | "updates";

interface UiState {
  view: View;
  setView: (v: View) => void;
  settingsSection: SettingsSection; // which Settings section is open; survives leaving the view
  setSettingsSection: (s: SettingsSection) => void;
  openSettings: (section?: SettingsSection) => void; // jump straight to a section
  query: string;
  sources: SourceKind[]; // empty = all sources
  includeSubagents: boolean;
  hybrid: boolean;
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
  // ResultsList is showing "Nothing indexed yet": its own Reindex is then the one primary
  // action, so the toolbar's turns quiet, and the thread pane has nothing to point at.
  indexEmpty: boolean;
  setIndexEmpty: (empty: boolean) => void;
  selectedProject: string | null; // active project in the Projects view
  openProject: (project: string) => void; // jump to a project's memory
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
  view: "search",
  setView: (view) => set({ view }),
  settingsSection: "general",
  setSettingsSection: (settingsSection) => set({ settingsSection }),
  openSettings: (section) =>
    set((state) => ({ view: "settings", settingsSection: section ?? state.settingsSection })),
  query: "",
  sources: [],
  includeSubagents: false,
  hybrid: false,
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
    set({ hybrid });
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

/** How SearchBar opens the highlighted row on Enter. ResultsList installs `open` while it
 *  is mounted; a plain object rather than store state, since it is a callback, not data. */
export const resultsNav: { open: (index: number) => void } = { open: () => {} };
