import { useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { api, SOURCE_LABELS } from "../lib/api";
import { useUi } from "../store/ui";
import { shortPath } from "../lib/format";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Loading } from "./Loading";
import { Markdown } from "./Markdown";

const ASK_EXAMPLES = [
  "How did we handle authentication?",
  "What did we decide about the database schema?",
  "Have we hit this kind of error before?",
];

/**
 * Ask-your-history (RAG): a question → Callimachus retrieves the most relevant past
 * threads, has the configured LLM answer with [thread N] citations, and lists the
 * source threads (click to open). Needs distillation/LLM enabled.
 */
export function AskView() {
  const [question, setQuestion] = useState("");
  const selectThread = useUi((s) => s.selectThread);
  const setView = useUi((s) => s.setView);
  const ask = useMutation({ mutationFn: (q: string) => api.askHistory(q) });

  const submit = () => {
    const q = question.trim();
    if (q && !ask.isPending) ask.mutate(q);
  };

  return (
    <div className="mx-auto flex h-full w-full max-w-3xl flex-col p-6">
      <div className="shrink-0 pb-3">
        <Input
          value={question}
          autoFocus
          placeholder="Ask your history… e.g. how did we set up release versioning?"
          onChange={(e) => setQuestion(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              submit();
            }
          }}
        />
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {ask.isPending ? (
          <Loading label="Searching your history & answering…" />
        ) : ask.isError ? (
          <p className="px-1 text-sm text-destructive">{String(ask.error)}</p>
        ) : ask.data ? (
          <div className="space-y-4">
            <Markdown>{ask.data.answer}</Markdown>
            {ask.data.sources.length > 0 && (
              <div>
                <div className="mb-1 text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
                  Sources
                </div>
                <ul className="space-y-1.5">
                  {ask.data.sources.map((s) => (
                    <li key={s.threadId}>
                      <button
                        type="button"
                        onClick={() => {
                          selectThread(s.threadId);
                          setView("search");
                        }}
                        className="flex w-full cursor-pointer items-center gap-2 rounded-lg border px-3 py-2 text-left text-sm transition-colors hover:bg-muted/50"
                      >
                        <Badge variant="outline" className="shrink-0 text-[0.6rem] uppercase">
                          {SOURCE_LABELS[s.source]}
                        </Badge>
                        <span className="truncate">{s.title || "Untitled thread"}</span>
                        <span className="ml-auto shrink-0 text-[0.7rem] text-muted-foreground">
                          #{s.threadId}
                          {s.projectPath ? ` · ${shortPath(s.projectPath)}` : ""}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </div>
        ) : (
          <div className="space-y-3 px-1">
            <p className="text-sm text-muted-foreground">
              Ask a question — Callimachus searches your past sessions and answers with citations to
              the threads it used. Needs distillation enabled (an LLM engine) in Settings.
            </p>
            <div className="space-y-1.5">
              <p className="text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
                Try
              </p>
              {ASK_EXAMPLES.map((ex) => (
                <button
                  key={ex}
                  type="button"
                  onClick={() => {
                    setQuestion(ex);
                    ask.mutate(ex);
                  }}
                  className="block w-fit cursor-pointer rounded-md border px-2.5 py-1.5 text-left text-sm transition-colors hover:bg-muted/50"
                >
                  {ex}
                </button>
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
