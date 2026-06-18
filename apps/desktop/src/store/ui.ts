import { create } from "zustand";
import type { SourceKind } from "../lib/api";

export type View = "search" | "chat" | "stats" | "settings";

interface UiState {
  view: View;
  setView: (v: View) => void;
  query: string;
  sources: SourceKind[]; // empty = all sources
  includeSubagents: boolean;
  hybrid: boolean;
  selectedThreadId: number | null;
  setQuery: (q: string) => void;
  toggleSource: (s: SourceKind) => void;
  toggleSubagents: () => void;
  toggleHybrid: () => void;
  selectThread: (id: number | null) => void;
}

// Subscribe with selectors (e.g. useUi((s) => s.query)) — never the whole store.
export const useUi = create<UiState>((set) => ({
  view: "search",
  setView: (view) => set({ view }),
  query: "",
  sources: [],
  includeSubagents: false,
  hybrid: false,
  selectedThreadId: null,
  setQuery: (query) => set({ query }),
  toggleSource: (s) =>
    set((state) => ({
      sources: state.sources.includes(s)
        ? state.sources.filter((x) => x !== s)
        : [...state.sources, s],
    })),
  toggleSubagents: () => set((state) => ({ includeSubagents: !state.includeSubagents })),
  toggleHybrid: () => set((state) => ({ hybrid: !state.hybrid })),
  selectThread: (selectedThreadId) => set({ selectedThreadId }),
}));
