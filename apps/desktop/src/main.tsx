import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";
import App from "./App";
import type { EmbedStatus } from "./lib/api";
import "./index.css";

const queryClient = new QueryClient({
  defaultOptions: {
    // Window refocus would otherwise fire a ~5-query burst — each serialized behind
    // the app's single DB connection. Stale window keeps it calm.
    queries: { refetchOnWindowFocus: false, staleTime: 5_000 },
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

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </React.StrictMode>,
);
