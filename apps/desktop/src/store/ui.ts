import { create } from "zustand";
import type { SourceKind } from "../lib/api";

export type View = "search" | "chat" | "knowledge" | "ask" | "stats" | "settings";

interface UiState {
  view: View;
  setView: (v: View) => void;
  query: string;
  sources: SourceKind[]; // empty = all sources
  includeSubagents: boolean;
  hybrid: boolean;
  starredOnly: boolean; // collection filter: only starred threads
  selectedTags: string[]; // collection filter: threads having ANY of these tags
  selectedThreadId: number | null;
  targetMessageId: number | null; // scroll-to target when opening from a search hit
  setQuery: (q: string) => void;
  toggleSource: (s: SourceKind) => void;
  toggleSubagents: () => void;
  toggleHybrid: () => void;
  toggleStarredOnly: () => void;
  toggleTag: (t: string) => void;
  clearCollectionFilters: () => void;
  selectThread: (id: number | null, messageId?: number | null) => void;
}

// Subscribe with selectors (e.g. useUi((s) => s.query)) — never the whole store.
export const useUi = create<UiState>((set) => ({
  view: "search",
  setView: (view) => set({ view }),
  query: "",
  sources: [],
  includeSubagents: false,
  hybrid: false,
  starredOnly: false,
  selectedTags: [],
  selectedThreadId: null,
  targetMessageId: null,
  setQuery: (query) => set({ query }),
  toggleSource: (s) =>
    set((state) => ({
      sources: state.sources.includes(s)
        ? state.sources.filter((x) => x !== s)
        : [...state.sources, s],
    })),
  toggleSubagents: () => set((state) => ({ includeSubagents: !state.includeSubagents })),
  toggleHybrid: () => set((state) => ({ hybrid: !state.hybrid })),
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
}));
