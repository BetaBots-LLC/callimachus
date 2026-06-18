import { useRef, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { api, OPEN_TARGETS, SOURCE_LABELS, type MessageRow } from "../lib/api";
import { useUi } from "../store/ui";
import { useChat } from "../store/chat";
import { useSettings } from "../store/settings";
import { formatTime } from "../lib/format";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import { ChevronDown, ExternalLink } from "lucide-react";

export function ThreadView() {
  const threadId = useUi((s) => s.selectedThreadId);
  const setView = useUi((s) => s.setView);
  const addContext = useChat((s) => s.addContext);
  const vaultDir = useSettings((s) => s.vaultDir);
  const synthProvider = useSettings((s) => s.synthProvider);
  const synthModel = useSettings((s) => s.synthModel);
  const [copied, setCopied] = useState(false);
  const [exported, setExported] = useState(false);

  const { data, isLoading } = useQuery({
    queryKey: ["thread", threadId],
    queryFn: () => api.getThread(threadId as number),
    enabled: threadId != null,
  });

  const canResume = data?.source === "claude_code" || data?.source === "codex";
  const resume = useMutation({ mutationFn: () => api.resumeThread(threadId as number) });
  const addToChat = useMutation({
    mutationFn: () => api.threadContext(threadId as number),
    onSuccess: (ctx) => {
      addContext(ctx);
      setView("chat");
    },
  });
  const copyContext = useMutation({
    mutationFn: () => api.threadContext(threadId as number),
    onSuccess: async (ctx) => {
      try {
        await navigator.clipboard.writeText(ctx);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      } catch {
        /* clipboard unavailable */
      }
    },
  });
  const openInCli = useMutation({
    mutationFn: (program: string) => api.openThreadInCli(threadId as number, program),
  });
  const canSynth = useQuery({ queryKey: ["can_synthesize"], queryFn: api.canSynthesize });
  const markExported = () => {
    setExported(true);
    setTimeout(() => setExported(false), 1500);
  };
  const exportNote = useMutation({
    mutationFn: () => api.exportThread(threadId as number, vaultDir),
    onSuccess: markExported,
  });
  const synthExport = useMutation({
    mutationFn: () => api.synthesizeExport(threadId as number, vaultDir, synthProvider, synthModel),
    onSuccess: markExported,
  });
  const exportBusy = exportNote.isPending || synthExport.isPending;

  if (threadId == null)
    return (
      <div className="flex h-full items-center justify-center text-muted-foreground">
        Select a thread to read it.
      </div>
    );
  if (isLoading)
    return (
      <div className="flex h-full items-center justify-center text-muted-foreground">Loading…</div>
    );
  if (!data)
    return (
      <div className="flex h-full items-center justify-center text-muted-foreground">
        Thread not found.
      </div>
    );

  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="border-b px-5 py-3">
        <div className="flex items-center justify-between gap-2">
          <Badge variant="outline" className="text-[0.62rem] uppercase">
            {SOURCE_LABELS[data.source]}
          </Badge>
          <div className="flex flex-wrap justify-end gap-1.5">
            <Button
              size="xs"
              variant="outline"
              onClick={() => addToChat.mutate()}
              disabled={addToChat.isPending}
            >
              + Add to chat
            </Button>
            <Button
              size="xs"
              variant="outline"
              onClick={() => copyContext.mutate()}
              disabled={copyContext.isPending}
            >
              {copied ? "Copied ✓" : "Copy context"}
            </Button>
            <DropdownMenu>
              <DropdownMenuTrigger
                render={<Button size="xs" variant="outline" disabled={exportBusy} />}
              >
                {exportBusy ? "Exporting…" : exported ? "Exported ✓" : "Export"}
                <ChevronDown className="size-3.5" />
              </DropdownMenuTrigger>
              <DropdownMenuContent>
                <div className="px-2 py-1 text-xs text-muted-foreground">
                  {vaultDir ? "Export to Obsidian" : "Set a vault in Settings first"}
                </div>
                <DropdownMenuItem onClick={() => exportNote.mutate()} disabled={!vaultDir}>
                  Quick note (transcript)
                </DropdownMenuItem>
                <DropdownMenuItem
                  onClick={() => synthExport.mutate()}
                  disabled={!vaultDir || !canSynth.data}
                >
                  Synthesize &amp; export
                  {canSynth.data === false ? " (add API key)" : ""}
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
            <DropdownMenu>
              <DropdownMenuTrigger
                render={<Button size="xs" variant="outline" disabled={openInCli.isPending} />}
              >
                {openInCli.isPending ? "Opening…" : "Open in"}
                <ChevronDown className="size-3.5" />
              </DropdownMenuTrigger>
              <DropdownMenuContent>
                {/* Plain div, not DropdownMenuLabel: Base UI's GroupLabel throws
                    unless wrapped in a <Menu.Group>, which this menu has none of. */}
                <div className="px-2 py-1 text-xs text-muted-foreground">Open thread in…</div>
                {OPEN_TARGETS.map((t) => (
                  <DropdownMenuItem key={t.program} onClick={() => openInCli.mutate(t.program)}>
                    <ExternalLink />
                    {t.label}
                  </DropdownMenuItem>
                ))}
              </DropdownMenuContent>
            </DropdownMenu>
            {canResume && (
              <Button
                size="xs"
                variant="secondary"
                onClick={() => resume.mutate()}
                disabled={resume.isPending}
              >
                {resume.isPending ? "…" : "Resume ↗"}
              </Button>
            )}
          </div>
        </div>
        <h2 className="mt-1.5 text-lg font-semibold">{data.title || "Untitled thread"}</h2>
        <div className="break-all text-xs text-muted-foreground">
          {data.projectPath}
          {data.gitBranch && <span> · {data.gitBranch}</span>}
          <span> · {formatTime(data.updatedAt)}</span>
        </div>
        {resume.isError && (
          <div className="mt-1 text-xs text-destructive">{String(resume.error)}</div>
        )}
        {openInCli.isError && (
          <div className="mt-1 text-xs text-destructive">{String(openInCli.error)}</div>
        )}
        {exportNote.isError && (
          <div className="mt-1 text-xs text-destructive">{String(exportNote.error)}</div>
        )}
        {synthExport.isError && (
          <div className="mt-1 text-xs text-destructive">{String(synthExport.error)}</div>
        )}
        {synthExport.isPending && (
          <div className="mt-1 text-xs text-muted-foreground">Synthesizing with the LLM…</div>
        )}
        {exported && (synthExport.data || exportNote.data) && (
          <div className="mt-1 text-xs text-muted-foreground">
            Wrote {synthExport.data ?? exportNote.data}
          </div>
        )}
      </header>
      <MessageList messages={data.messages} />
    </div>
  );
}

function MessageList({ messages }: { messages: MessageRow[] }) {
  const parentRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: messages.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 110,
    overscan: 8,
  });
  return (
    <div ref={parentRef} className="min-h-0 flex-1 overflow-y-auto pt-3">
      <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
        {virtualizer.getVirtualItems().map((vrow) => (
          <div
            key={vrow.key}
            data-index={vrow.index}
            ref={virtualizer.measureElement}
            className="absolute left-0 top-0 w-full px-5 pb-3"
            style={{ transform: `translateY(${vrow.start}px)` }}
          >
            <Message m={messages[vrow.index]} />
          </div>
        ))}
      </div>
    </div>
  );
}

function Message({ m }: { m: MessageRow }) {
  if (m.toolName || m.role === "tool") {
    return (
      <details className="rounded-lg border bg-muted/40 px-3 py-2 text-sm">
        <summary className="cursor-pointer text-muted-foreground">
          <span className="text-[0.68rem] uppercase tracking-wide">
            {m.toolName ? `tool · ${m.toolName}` : "result"}
          </span>
        </summary>
        <pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap break-words text-[0.8rem] text-muted-foreground">
          {m.text}
        </pre>
      </details>
    );
  }
  return (
    <div
      className={cn(
        "rounded-lg border bg-card px-3 py-2",
        m.role === "user" ? "border-l-2 border-l-blue-500" : "border-l-2 border-l-emerald-500",
      )}
    >
      <div className="mb-1 text-[0.68rem] uppercase tracking-wide text-muted-foreground">
        {m.role}
      </div>
      <div className="whitespace-pre-wrap break-words leading-relaxed">{m.text}</div>
    </div>
  );
}
