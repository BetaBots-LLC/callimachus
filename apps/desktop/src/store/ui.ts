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
  selectThread: (id: number | null, messageId?: number | null) => void;
  // When the watcher last reported new transcripts that an on-screen search was NOT
  // refetched for (main.tsx). ResultsList offers a refresh while its data is older.
  searchActivityAt: number;
  noteSearchActivity: () => void;
}

// Subscribe with selectors (e.g. useUi((s) => s.query)) — never the whole store.
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
  selectedProject: null,
  openProject: (project) => set({ selectedProject: project, view: "projects" }),
  commandOpen: false,
  setCommandOpen: (commandOpen) => set({ commandOpen }),
  setQuery: (query) => set({ query }),
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
  selectThread: (id, messageId) =>
    set({ selectedThreadId: id, targetMessageId: messageId ?? null }),
  searchActivityAt: 0,
  noteSearchActivity: () => set({ searchActivityAt: Date.now() }),
}));
