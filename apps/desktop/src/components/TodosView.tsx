import { useQuery } from "@tanstack/react-query";
import { api, SOURCE_LABELS } from "../lib/api";
import { useUi } from "../store/ui";
import { shortPath } from "../lib/format";
import { Badge } from "@/components/ui/badge";

/**
 * Open TODOs / action items pulled from history by the free heuristic knowledge tier
 * (markdown task checkboxes + TODO/FIXME markers). Click one to jump to its thread.
 */
export function TodosView() {
  const selectThread = useUi((s) => s.selectThread);
  const setView = useUi((s) => s.setView);
  const todos = useQuery({ queryKey: ["open_todos"], queryFn: () => api.listOpenTodos() });

  function open(threadId: number) {
    selectThread(threadId);
    setView("search");
  }

  if (todos.isLoading) {
    return <div className="p-6 text-sm text-muted-foreground">Loading…</div>;
  }

  const items = todos.data ?? [];
  if (items.length === 0) {
    return (
      <div className="mx-auto max-w-2xl p-8">
        <h2 className="mb-1 text-base font-semibold">No open TODOs yet</h2>
        <p className="text-sm text-muted-foreground">
          Callimachus scans your indexed conversations for action items — markdown task checkboxes
          and <code className="rounded bg-muted px-1">TODO</code>/
          <code className="rounded bg-muted px-1">FIXME</code> notes. Reindex your sources from
          Settings to populate this list.
        </p>
      </div>
    );
  }

  return (
    <div className="mx-auto w-full max-w-3xl space-y-2 overflow-y-auto p-6">
      <div className="mb-2 flex items-baseline justify-between">
        <h2 className="text-base font-semibold">Open TODOs</h2>
        <span className="text-xs text-muted-foreground">
          {items.length.toLocaleString()} found across your history
        </span>
      </div>
      {items.map((t) => (
        <button
          key={t.id}
          type="button"
          onClick={() => open(t.threadId)}
          className="block w-full cursor-pointer rounded-lg border px-3 py-2.5 text-left transition-colors hover:bg-muted/50"
        >
          <div className="text-sm">{t.text}</div>
          <div className="mt-1.5 flex items-center gap-2 text-[0.72rem] text-muted-foreground">
            <Badge variant="outline" className="shrink-0 text-[0.6rem] uppercase">
              {SOURCE_LABELS[t.source]}
            </Badge>
            <span className="truncate">{t.title || "Untitled thread"}</span>
            {t.projectPath && <span className="shrink-0">· {shortPath(t.projectPath)}</span>}
          </div>
        </button>
      ))}
    </div>
  );
}
