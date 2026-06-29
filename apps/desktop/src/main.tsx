import React, { useEffect } from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import App from "./App";
import { api, type EmbedStatus, type IndexProgress } from "./lib/api";
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
// Per-thread project-distill progress + completion → drive the bar, refresh the memory.
void listen<{ done: number; total: number }>("distill:progress", (e) => {
  queryClient.setQueryData(["distill_progress"], e.payload);
});
void listen("distill:done", () => {
  queryClient.setQueryData(["distill_progress"], null);
  for (const key of ["distill_status", "project_memory", "projects", "knowledge_config"]) {
    void queryClient.invalidateQueries({ queryKey: [key] });
  }
});
// Background re-index finished — clear progress, refresh results/stats/button state.
void listen("index:done", () => {
  queryClient.setQueryData(["index_progress"], null);
  // A reindex has now completed at least once this session. The empty-state
  // (Onboarding) reads this to say "no threads found" instead of re-offering the
  // first-run "Index my history" button, which read as "nothing happened".
  queryClient.setQueryData(["index_ran"], true);
  for (const key of ["results", "db_stats", "index_stats", "index_status"]) {
    void queryClient.invalidateQueries({ queryKey: [key] });
  }
});

/** The main window is hidden at launch (tauri.conf) while the splash WINDOW shows. The
 *  app loads here behind it; once our initial data settles we signal the backend, which
 *  closes the splash and reveals this window only when its own setup is also done. The
 *  db_stats query shares the header's ["db_stats"] cache, so this adds no extra fetch. */
function AppShell() {
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats, retry: 0 });
  useEffect(() => {
    // Settled (success OR error) → frontend is ready. set_complete is idempotent, so an
    // extra call is harmless; the backend reveals the window only when its half is done too.
    if (!stats.isLoading) void invoke("set_complete", { task: "frontend" });
  }, [stats.isLoading]);
  return <App />;
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <AppShell />
    </QueryClientProvider>
  </React.StrictMode>,
);
