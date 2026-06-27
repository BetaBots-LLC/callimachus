import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type KFact } from "../lib/api";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from "@/components/ui/sheet";
import { Sparkles } from "lucide-react";
import { InlineMarkdown } from "./Markdown";

/**
 * Distilled knowledge for a thread, in a right-side sheet so it floats over the
 * transcript with its own scroll and never reflows the message list: summary,
 * decisions, gotchas (LLM tier) + open TODOs (free heuristic tier). Distillation is
 * user-triggered per thread — the global Settings toggle is consent; the Distill
 * button here is the per-thread spend.
 */
export function KnowledgeButton({ threadId }: { threadId: number }) {
  const queryClient = useQueryClient();
  const k = useQuery({
    queryKey: ["thread_knowledge", threadId],
    queryFn: () => api.threadKnowledge(threadId),
  });
  const distill = useMutation({
    mutationFn: () => api.distillThread(threadId),
    onSuccess: (data) => {
      queryClient.setQueryData(["thread_knowledge", threadId], data);
      queryClient.invalidateQueries({ queryKey: ["open_todos"] });
    },
  });

  const d = k.data;
  if (!d) return null;

  const hasDistilled = !!d.summary || d.decisions.length > 0 || d.gotchas.length > 0;
  const showDistill = d.canDistill && (!d.extracted || d.stale);
  if (!hasDistilled && d.todos.length === 0 && !showDistill) return null;

  const count = d.decisions.length + d.gotchas.length + d.todos.length;

  return (
    <Sheet>
      <SheetTrigger
        render={<Button size="xs" variant="outline" title="Distilled knowledge for this thread" />}
      >
        <Sparkles className="size-3.5" />
        Knowledge
        {count > 0 && <span className="text-muted-foreground">{count}</span>}
      </SheetTrigger>
      <SheetContent side="right" className="gap-0 sm:max-w-md">
        <SheetHeader className="border-b">
          <div className="flex items-center justify-between gap-2 pr-8">
            <SheetTitle>Knowledge</SheetTitle>
            {showDistill && (
              <Button
                size="xs"
                variant="outline"
                onClick={() => distill.mutate()}
                disabled={distill.isPending}
              >
                <Sparkles className="size-3.5" />
                {distill.isPending ? "Distilling…" : d.stale ? "Re-distill" : "Distill"}
              </Button>
            )}
          </div>
          <SheetDescription>
            {hasDistilled
              ? "Distilled from this conversation."
              : showDistill
                ? "Extract decisions, gotchas, and a summary from this thread."
                : "Open TODOs found in this thread."}
          </SheetDescription>
        </SheetHeader>

        <div className="min-w-0 flex-1 space-y-4 overflow-y-auto overflow-x-hidden p-4">
          {distill.isError && <p className="text-xs text-destructive">{String(distill.error)}</p>}
          {d.error && !distill.isPending && (
            <p className="text-xs text-destructive">Last distillation failed: {d.error}</p>
          )}
          {d.stale && hasDistilled && (
            <p className="text-xs text-muted-foreground">
              Thread changed since this was distilled.
            </p>
          )}

          {d.summary && (
            <p className="text-sm leading-relaxed wrap-break-word">
              <InlineMarkdown>{d.summary}</InlineMarkdown>
            </p>
          )}
          {d.decisions.length > 0 && <FactList label="Decisions" items={d.decisions} />}
          {d.gotchas.length > 0 && <FactList label="Gotchas" items={d.gotchas} />}
          {d.todos.length > 0 && <FactList label="TODOs" items={d.todos} />}

          {!hasDistilled && d.todos.length === 0 && (
            <p className="text-sm text-muted-foreground">
              Nothing distilled yet. Hit Distill to extract this thread's key decisions, gotchas,
              and a summary.
            </p>
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}

function FactList({ label, items }: { label: string; items: KFact[] }) {
  return (
    <div>
      <div className="mb-1 text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
        {label}
      </div>
      <ul className="space-y-1">
        {items.map((f) => (
          <li key={f.id} className="flex gap-2 text-sm leading-snug">
            <span className="text-muted-foreground">•</span>
            <span className="min-w-0 wrap-break-word">
              <InlineMarkdown>{f.text}</InlineMarkdown>
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}
