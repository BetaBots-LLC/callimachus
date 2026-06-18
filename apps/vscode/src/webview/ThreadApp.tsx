// The editor-tab transcript view, styled to match the desktop app: user turns as
// right-aligned bubbles, assistant turns as full-width rendered markdown (the
// desktop Markdown component, so it looks identical), tool turns collapsed. The
// packed `cal cat` blob is split into role turns client-side (see transcript.ts).

import { useEffect, useMemo, useState } from "react";
import { CornerDownLeft, Copy, FileDown, Terminal } from "lucide-react";
import { Button } from "@desktop/components/ui/button";
import { Badge } from "@desktop/components/ui/badge";
import { StreamingMarkdown } from "@desktop/components/Markdown";
import { shortPath } from "@desktop/lib/format";
import type { InitPayload } from "../protocol";
import { sourceLabel } from "../protocol";
import { action, request } from "./bridge";
import { parseTranscript, type Turn } from "./transcript";

function TurnBlock({ turn }: { turn: Turn }) {
  if (turn.kind === "divider") {
    return (
      <div className="flex items-center gap-3 text-[0.68rem] uppercase tracking-wide text-muted-foreground">
        <span className="h-px flex-1 bg-border" />
        <span>{turn.label || "turns elided"}</span>
        <span className="h-px flex-1 bg-border" />
      </div>
    );
  }

  if (turn.kind === "tool") {
    return (
      <details className="rounded-lg border bg-muted/40 px-3 py-2 text-sm">
        <summary className="cursor-pointer text-muted-foreground">
          <span className="text-[0.68rem] uppercase tracking-wide">{turn.label}</span>
        </summary>
        <pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap wrap-break-word text-[0.8rem] text-muted-foreground">
          {turn.text}
        </pre>
      </details>
    );
  }

  if (turn.kind === "user") {
    return (
      <div className="flex justify-end">
        <div className="min-w-0 max-w-[85%] whitespace-pre-wrap wrap-break-word rounded-2xl bg-muted px-4 py-2.5 text-[0.95rem] leading-relaxed">
          {turn.text}
        </div>
      </div>
    );
  }

  return (
    <div className="w-full min-w-0">
      <StreamingMarkdown>{turn.text}</StreamingMarkdown>
    </div>
  );
}

export function ThreadApp({ init }: { init: InitPayload }) {
  const id = init.threadId ?? -1;
  const [md, setMd] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    request("cat", { id })
      .then((text) => live && setMd(text))
      .catch((e) => live && setError((e as Error).message));
    return () => {
      live = false;
    };
  }, [id]);

  const transcript = useMemo(
    () => (md === null ? null : parseTranscript(md, init.title)),
    [md, init.title],
  );

  const title = transcript?.meta.title?.trim() || init.title?.trim() || `Thread ${id}`;
  const turnCount = transcript?.turns.filter((t) => t.kind !== "divider").length ?? 0;

  return (
    <div className="flex h-screen min-h-0 flex-col">
      <header className="flex-none border-b px-5 py-3">
        <div className="flex items-center justify-between gap-2">
          {transcript?.meta.source ? (
            <Badge variant="outline" className="text-[0.62rem] uppercase">
              {sourceLabel(transcript.meta.source)}
            </Badge>
          ) : (
            <span />
          )}
          <div className="flex flex-wrap justify-end gap-1.5">
            <Button size="xs" variant="outline" onClick={() => action("insertThread", id)}>
              <CornerDownLeft /> Insert
            </Button>
            <Button size="xs" variant="outline" onClick={() => action("copyThread", id)}>
              <Copy /> Copy
            </Button>
            <Button size="xs" variant="outline" onClick={() => action("exportThread", id)}>
              <FileDown /> Export
            </Button>
            <Button size="xs" variant="outline" onClick={() => action("openInCli", id)}>
              <Terminal /> Open in CLI
            </Button>
          </div>
        </div>
        <h2 className="mt-1.5 text-lg font-semibold">{title}</h2>
        <div className="break-all text-xs text-muted-foreground">
          {transcript?.meta.project ? <span>{shortPath(transcript.meta.project)} · </span> : null}
          {turnCount} turns
        </div>
      </header>

      <div className="mx-auto flex w-full min-w-0 max-w-3xl flex-1 flex-col gap-6 overflow-y-auto px-5 py-6">
        {error ? (
          <p className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">{error}</p>
        ) : !transcript ? (
          <p className="text-sm text-muted-foreground">Loading transcript…</p>
        ) : (
          <>
            {transcript.meta.note ? (
              <p className="rounded-md border bg-muted/40 px-3 py-1.5 text-[0.7rem] text-muted-foreground">
                {transcript.meta.note}
              </p>
            ) : null}
            {transcript.turns.map((turn) => (
              <TurnBlock key={turn.id} turn={turn} />
            ))}
          </>
        )}
      </div>
    </div>
  );
}
