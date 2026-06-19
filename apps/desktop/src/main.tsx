import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";
import App from "./App";
import type { EmbedStatus, IndexProgress } from "./lib/api";
import "./index.css";

const queryClient = new QueryClient({
  defaultOptions: {
    // Window refocus would otherwise fire a ~5-query burst — each serialized behind
    // the app's single DB connection. Stale window keeps it calm. Local invoke calls
    // mostly fail deterministically, so one quick retry (not the default 3 with
    // exponential backoff, ~7s) surfaces errors fast instead of looking "stuck".
    queries: { refetchOnWindowFocus: false, staleTime: 5_000, retry: 1, retryDelay: 400 },
  },
});

// Push-based embedding progress: the backend emits counts per batch, so the UI
// never polls `embedding_status` (two locked COUNT(*) scans) while a build runs.
void listen<{ done: number; total: number }>("embed:progress", (e) => {
  queryClient.setQueryData<EmbedStatus>(["embed_status"], {
    done: e.payload.done,
    total: e.payload.total,
    running: true,
  });
});
void listen("embed:done", () => {
  // One authoritative refetch for the final counts + running:false.
  void queryClient.invalidateQueries({ queryKey: ["embed_status"] });
});
// Background todo backfill (after enabling the Knowledge feature) finished.
void listen("knowledge:todos-ready", () => {
  void queryClient.invalidateQueries({ queryKey: ["open_todos"] });
});
// Per-source reindex progress → drives the progress bar.
void listen<IndexProgress>("index:progress", (e) => {
  queryClient.setQueryData<IndexProgress>(["index_progress"], e.payload);
});
// Background re-index finished — clear progress, refresh results/stats/button state.
void listen("index:done", () => {
  queryClient.setQueryData(["index_progress"], null);
  for (const key of ["results", "db_stats", "index_stats", "index_status"]) {
    void queryClient.invalidateQueries({ queryKey: [key] });
  }
});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </React.StrictMode>,
);
