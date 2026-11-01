import { memo, useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { api, OPEN_TARGETS, SOURCE_LABELS, type MessageRow, type ThreadDetail } from "../lib/api";
import { useUi } from "../store/ui";
import { MAX_CONTEXT_CHARS, MAX_CONTEXTS, useChat } from "../store/chat";
import { useSettings } from "../store/settings";
import { formatTime } from "../lib/format";
import { track, trackError } from "../lib/telemetry";
import { copyAsyncText } from "../lib/clipboard";
import { CAN_LAUNCH_CLI_FALLBACK } from "../lib/platform";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import { ExternalLink, GitCommitHorizontal, MoreHorizontal, Star } from "lucide-react";
import { TagsEditor } from "./TagsEditor";
import { KnowledgeButton } from "./KnowledgeSection";
import { Loading } from "./Loading";
// Lazy: keeps the markdown + highlight stack off the startup path (see LazyMarkdown).
import { Markdown } from "./LazyMarkdown";

/** An inline notice tied to the thread it is about. */
type Notice = { threadId: number; text: string };

// Module-level, not a ref: this view remounts on every tab switch, and coming back to the
// Search tab with the same thread selected is not a new "thread opened".
let lastReportedThread: number | null = null;

export function ThreadView() {
  const threadId = useUi((s) => s.selectedThreadId);
  const setView = useUi((s) => s.setView);
  const addContext = useChat((s) => s.addContext);
  const vaultDir = useSettings((s) => s.vaultDir);
  const synthProvider = useSettings((s) => s.synthProvider);
  const synthModel = useSettings((s) => s.synthModel);
  const [copyState, setCopyState] = useState<"idle" | "copying" | "copied">("idle");
  // Notices remember which thread they are about, so opening another thread hides them.
  const [copyError, setCopyError] = useState<Notice | null>(null);
  const [contextNotice, setContextNotice] = useState<Notice | null>(null);
  const [exported, setExported] = useState(false);

  const { data, isLoading } = useQuery({
    queryKey: ["thread", threadId],
    queryFn: () => api.getThread(threadId as number),
    enabled: threadId != null,
    // A long transcript is ~12 MB of heap, and nothing reads a thread once it is closed, so
    // drop it after a minute instead of the default five. Not shorter: this view remounts
    // on every tab switch, and a quick look at Chat shouldn't refetch the open thread.
    gcTime: 60_000,
  });

  // Once per opened thread, not per refetch (live updates refetch open threads).
  useEffect(() => {
    if (data && data.id === threadId && lastReportedThread !== threadId) {
      lastReportedThread = threadId;
      track("thread_opened", { source: data.source });
    }
  }, [data, threadId]);

  // The backend says whether it can open a terminal here (a Linux box with no terminal
  // emulator can't), so Resume / Open-in-CLI are hidden instead of failing on click. The
  // answer can't change mid-session; the OS guess covers the moment before it arrives.
  const cliLaunch = useQuery({
    queryKey: ["cli_launch_supported"],
    queryFn: api.cliLaunchSupported,
    staleTime: Infinity,
  });
  const canLaunchCli = cliLaunch.data ?? CAN_LAUNCH_CLI_FALLBACK;
  const canResume = canLaunchCli && (data?.source === "claude_code" || data?.source === "codex");
  const resume = useMutation({ mutationFn: () => api.resumeThread(threadId as number) });
  const addToChat = useMutation({
    // Chat context is re-sent every turn, so ask for a compact pack (see store/chat.ts).
    // The thread is passed in (not read from render scope) so a reply that lands after
    // the user opened another thread is still attributed to the right one.
    mutationFn: (t: { id: number; title: string | null }) =>
      api.threadContext(t.id, MAX_CONTEXT_CHARS),
    onMutate: () => setContextNotice(null),
    onSuccess: (ctx, { id, title }) => {
      const result = addContext(ctx, { threadId: id, title });
      if (result === "limit") {
        setContextNotice({
          threadId: id,
          text: `Chat already has ${MAX_CONTEXTS} attached threads. Remove one in Chat, or start a new chat, to add this one.`,
        });
      } else if (result === "busy") {
        setContextNotice({
          threadId: id,
          text: "Chat is still replying. Add this thread once the reply finishes.",
        });
      } else {
        setView("chat"); // "added", or "duplicate" (already attached): show the chat
      }
    },
  });

  // Copy runs straight from the click (no await first): WebKit only allows clipboard
  // writes during the user gesture, so the IPC result is handed to the clipboard as a
  // promise (see lib/clipboard).
  const copyContext = () => {
    if (threadId == null || copyState === "copying") return;
    setCopyError(null);
    setCopyState("copying");
    copyAsyncText(() => api.threadContext(threadId)).then(
      () => {
        setCopyState("copied");
        setTimeout(() => setCopyState("idle"), 1500);
      },
      (e) => {
        setCopyState("idle");
        setCopyError({
          threadId,
          text: `Couldn't copy the context: ${e instanceof Error ? e.message : e}`,
        });
      },
    );
  };
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
    // A plain export is a file write, so its errors (permission denied, missing folder)
    // must not be read as provider errors by the keyword classifier.
    onError: () => track("error_shown", { kind: "export_failed" }),
  });
  const synthExport = useMutation({
    mutationFn: () => api.synthesizeExport(threadId as number, vaultDir, synthProvider, synthModel),
    onSuccess: markExported,
    // Synthesis calls an LLM first, so a provider error keeps its class.
    onError: (e) => trackError(e, "export_failed"),
  });

  const queryClient = useQueryClient();
  const toggleStar = useMutation({
    // The id travels as a variable: a pending mutation picks up the newest render's
    // callbacks, so reading `threadId` in onSuccess would patch whichever thread is open
    // when setStar returns (it can queue behind a reindex write), not the one starred.
    mutationFn: ({ id, starred }: { id: number; starred: boolean }) => api.setStar(id, starred),
    onSuccess: (_, { id, starred }) => {
      // Patch the cached thread instead of refetching it: a big thread is megabytes over
      // IPC, and only the flag changed. Results still refetch (they filter by star).
      queryClient.setQueryData<ThreadDetail | null>(["thread", id], (d) =>
        d ? { ...d, starred } : d,
      );
      queryClient.invalidateQueries({ queryKey: ["results"] });
    },
  });

  if (threadId == null)
    return (
      <div className="flex h-full items-center justify-center text-muted-foreground">
        Select a thread to read it.
      </div>
    );
  if (isLoading) return <Loading label="Loading…" className="h-full" />;
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
          <div className="flex items-center justify-end gap-1.5">
            <KnowledgeButton threadId={data.id} />
            <Button
              size="icon-sm"
              variant="outline"
              onClick={() => toggleStar.mutate({ id: data.id, starred: !data.starred })}
              disabled={toggleStar.isPending}
              title={data.starred ? "Unstar" : "Star"}
            >
              <Star className={cn("size-3.5", data.starred && "fill-current text-primary")} />
            </Button>
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
            <DropdownMenu>
              <DropdownMenuTrigger
                render={<Button size="icon-sm" variant="outline" title="More actions" />}
              >
                <MoreHorizontal className="size-4" />
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                <DropdownMenuItem
                  onClick={() => addToChat.mutate({ id: data.id, title: data.title })}
                  disabled={addToChat.isPending}
                >
                  Add to chat
                </DropdownMenuItem>
                <DropdownMenuItem onClick={copyContext} disabled={copyState === "copying"}>
                  {copyState === "copied" ? "Copied ✓" : "Copy context"}
                </DropdownMenuItem>

                <div className="-mx-1 my-1 h-px bg-border" />
                <div className="px-2 py-1 text-xs text-muted-foreground">
                  {vaultDir ? "Export to Obsidian" : "Set a vault in Settings first"}
                </div>
                <DropdownMenuItem onClick={() => exportNote.mutate()} disabled={!vaultDir}>
                  {exported ? "Exported ✓" : "Quick note (transcript)"}
                </DropdownMenuItem>
                <DropdownMenuItem
                  onClick={() => synthExport.mutate()}
                  disabled={!vaultDir || !canSynth.data}
                >
                  Synthesize &amp; export{canSynth.data === false ? " (add API key)" : ""}
                </DropdownMenuItem>

                {canLaunchCli && (
                  <>
                    <div className="-mx-1 my-1 h-px bg-border" />
                    <div className="px-2 py-1 text-xs text-muted-foreground">Open thread in…</div>
                    {OPEN_TARGETS.map((t) => (
                      <DropdownMenuItem key={t.program} onClick={() => openInCli.mutate(t.program)}>
                        <ExternalLink />
                        {t.label}
                      </DropdownMenuItem>
                    ))}
                  </>
                )}
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        </div>
        <h2 className="mt-1.5 text-lg font-semibold">{data.title || "Untitled thread"}</h2>
        <div className="break-all text-xs text-muted-foreground">
          {data.projectPath}
          {data.gitBranch && <span> · {data.gitBranch}</span>}
          <span> · {formatTime(data.updatedAt)}</span>
        </div>
        {/* Keyed per thread so a half-typed tag doesn't carry over to the next thread. */}
        <TagsEditor key={data.id} threadId={data.id} tags={data.tags} />
        {addToChat.isError && (
          <div className="mt-1 text-xs text-destructive">{String(addToChat.error)}</div>
        )}
        {contextNotice?.threadId === data.id && (
          <div className="mt-1 text-xs text-destructive">{contextNotice.text}</div>
        )}
        {copyError?.threadId === data.id && (
          <div className="mt-1 text-xs text-destructive">{copyError.text}</div>
        )}
        {copyState === "copied" && (
          // The menu closes on click, so confirm here where it stays visible.
          <div className="mt-1 text-xs text-muted-foreground">Copied the thread context.</div>
        )}
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
      <CommitsSection threadId={data.id} />
      <MessageList key={data.id} messages={data.messages} />
    </div>
  );
}

/** The git commits this thread likely produced (inferred file-overlap links). Reads stored
 *  links; "Find/Rescan" recomputes by reading the project's `git log`. */
function CommitsSection({ threadId }: { threadId: number }) {
  const queryClient = useQueryClient();
  const { data: commits } = useQuery({
    queryKey: ["thread_commits", threadId],
    queryFn: () => api.threadCommits(threadId),
  });
  const link = useMutation({
    mutationFn: () => api.linkThreadCommits(threadId),
    onSuccess: (rows) => queryClient.setQueryData(["thread_commits", threadId], rows),
  });

  if (!commits || commits.length === 0) {
    return (
      <div className="flex items-center gap-2 border-b px-5 py-2 text-xs text-muted-foreground">
        <GitCommitHorizontal className="size-3.5 shrink-0" />
        <span>No linked commits.</span>
        <button
          type="button"
          className="text-foreground underline-offset-2 hover:underline disabled:opacity-50"
          onClick={() => link.mutate()}
          disabled={link.isPending}
        >
          {link.isPending ? "Scanning git…" : "Find produced commits"}
        </button>
        {link.isError && <span className="text-destructive">git scan failed</span>}
      </div>
    );
  }

  return (
    <div className="border-b px-5 py-2">
      <div className="mb-1 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
        <GitCommitHorizontal className="size-3.5" />
        Produced commits
        <button
          type="button"
          className="ml-auto font-normal underline-offset-2 hover:underline disabled:opacity-50"
          onClick={() => link.mutate()}
          disabled={link.isPending}
        >
          {link.isPending ? "…" : "Rescan"}
        </button>
      </div>
      <ul className="space-y-0.5">
        {commits.map((c) => (
          <li key={c.sha} className="flex items-baseline gap-2 text-xs">
            <code className="shrink-0 text-muted-foreground">{c.shortSha}</code>
            <span className="truncate">{c.subject ?? "(no subject)"}</span>
            <span className="ml-auto shrink-0 text-[0.65rem] text-muted-foreground">
              {c.overlap} file{c.overlap === 1 ? "" : "s"}
            </span>
          </li>
        ))}
      </ul>
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
  // Arriving from a search hit: land centered on the matched message. Otherwise open
  // pinned to the newest message (chat-style), scroll up for history. A second pass on
  // the next frame corrects the landing after dynamic heights are measured.
  const target = useUi((s) => s.targetMessageId);
  useEffect(() => {
    if (messages.length === 0) return;
    const idx = target != null ? messages.findIndex((m) => m.id === target) : -1;
    const index = idx >= 0 ? idx : messages.length - 1;
    const align = idx >= 0 ? "center" : "end";
    const go = () => virtualizer.scrollToIndex(index, { align });
    go();
    const raf = requestAnimationFrame(go);
    return () => cancelAnimationFrame(raf);
  }, [messages, target, virtualizer]);
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
            <Message m={messages[vrow.index]} highlight={messages[vrow.index].id === target} />
          </div>
        ))}
      </div>
    </div>
  );
}

/** Wrap arbitrary text (e.g. tool output) as one fenced code block so it renders
 *  monospaced + syntax-highlighted through the Markdown pipeline. The fence is one
 *  backtick longer than any run already in the text, so embedded fences can't escape. */
function asCodeBlock(text: string, lang = ""): string {
  const longest = (text.match(/`+/g) ?? []).reduce((m, s) => Math.max(m, s.length), 0);
  const fence = "`".repeat(Math.max(3, longest + 1));
  return `${fence}${lang}\n${text}\n${fence}`;
}

/** Render a tool message body: pretty-print JSON args (tool calls are stored as
 *  "Name: {json}") or a JSON result; otherwise a plain highlighted code block (e.g.
 *  a line-numbered file read, which isn't valid JSON). */
function toolBody(text: string, toolName: string | null): string {
  let body = text;
  if (toolName) {
    const sep = text.indexOf(": ");
    if (sep > 0) body = text.slice(sep + 2);
  }
  const trimmed = body.trim();
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      return asCodeBlock(JSON.stringify(JSON.parse(trimmed), null, 2), "json");
    } catch {
      // Not valid JSON (e.g. a line-numbered file read) — fall through.
    }
  }
  // Label non-JSON output as plain text: unlabeled fences trigger highlight.js language
  // auto-detection across every grammar, ~30x the cost of plain rendering per row.
  return asCodeBlock(body, "text");
}

/** A collapsed tool call/result. The body is parsed and rendered only once the row is
 *  first opened: most tool rows are never expanded, and a large thread has thousands. */
function ToolMessage({ m, ring }: { m: MessageRow; ring: string }) {
  const [opened, setOpened] = useState(false);
  return (
    <details
      className={cn("rounded-lg border bg-muted/40 px-3 py-2 text-sm", ring)}
      onToggle={(e) => {
        if (e.currentTarget.open) setOpened(true);
      }}
    >
      <summary className="cursor-pointer text-muted-foreground">
        <span className="text-[0.68rem] uppercase tracking-wide">
          {m.toolName ? `tool · ${m.toolName}` : "result"}
        </span>
      </summary>
      {opened && (
        <div className="mt-2 max-h-80 overflow-auto text-[0.8rem]">
          <Markdown>{toolBody(m.text, m.toolName)}</Markdown>
        </div>
      )}
    </details>
  );
}

const Message = memo(function Message({ m, highlight }: { m: MessageRow; highlight?: boolean }) {
  const ring = highlight ? "ring-2 ring-primary/60" : "";
  if (m.toolName || m.role === "tool") {
    return <ToolMessage m={m} ring={ring} />;
  }
  return (
    <div
      className={cn(
        "rounded-lg border bg-card px-3 py-2",
        m.role === "user" ? "border-l-2 border-l-blue-500" : "border-l-2 border-l-emerald-500",
        ring,
      )}
    >
      <div className="mb-1 flex items-center gap-2 text-[0.68rem] uppercase tracking-wide text-muted-foreground">
        <span>{m.role}</span>
        {m.model && (
          <span className="rounded bg-muted px-1.5 py-0.5 font-mono text-[0.62rem] normal-case text-foreground/70">
            {m.model}
          </span>
        )}
      </div>
      <Markdown>{m.text}</Markdown>
    </div>
  );
});
