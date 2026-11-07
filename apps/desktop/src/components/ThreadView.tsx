import {
  type KeyboardEvent as ReactKeyboardEvent,
  memo,
  type ReactNode,
  type Ref,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { elementScroll, useVirtualizer } from "@tanstack/react-virtual";
import {
  api,
  type CommitLink,
  OPEN_TARGETS,
  SOURCE_LABELS,
  type MessageRow,
  type ThreadDetail,
} from "../lib/api";
import { focusSearch, SEARCH_INPUT_ID, useUi } from "../store/ui";
import { MAX_CONTEXT_CHARS, MAX_CONTEXTS, useChat } from "../store/chat";
import { useSettings } from "../store/settings";
import { formatTime, shortPath } from "../lib/format";
import { track, trackError } from "../lib/telemetry";
import { copyAsyncText } from "../lib/clipboard";
import { CAN_LAUNCH_CLI_FALLBACK } from "../lib/platform";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import {
  ArrowUpRight,
  Check,
  ChevronDown,
  ChevronRight,
  ChevronUp,
  CornerDownRight,
  ExternalLink,
  FolderGit2,
  GitCommitHorizontal,
  MoreHorizontal,
  Star,
} from "lucide-react";
import { TagsEditor } from "./TagsEditor";
import { KnowledgeButton } from "./KnowledgeSection";
import { Loading } from "./Loading";
import { Spinner } from "@/components/ui/spinner";
import {
  type LargeMessageState,
  LargeMessageStates,
  MAX_MARKDOWN_CHARS,
} from "./largeMessageState";
// Lazy: keeps the markdown + highlight stack off the startup path (see LazyMarkdown).
import { Markdown } from "./LazyMarkdown";

/** An inline notice tied to the thread it is about. */
type Notice = { threadId: number; text: string };

// Module-level, not a ref: effects re-run each time the Search tab is shown again (App keeps
// it in an Activity), and coming back to the same thread is not a new "thread opened".
let lastReportedThread: number | null = null;

// Opened from a search, every matched message of the thread (ResultsList fills the store).
// Only that list feeds the stepper: a new query clears it, and the hit that opened the
// thread must not keep a "Match 1 of 1" for a query that matched nothing.
const EMPTY_IDS: readonly number[] = [];

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Typing targets, where the prompt shortcuts ([ and ]) must stay plain characters. */
function isTypingTarget(t: EventTarget | null): boolean {
  if (!(t instanceof HTMLElement)) return false;
  return t.isContentEditable || t.tagName === "INPUT" || t.tagName === "TEXTAREA";
}

const PROMPT_KEYS = "[ or Alt+Up";
const NEXT_PROMPT_KEYS = "] or Alt+Down";
const MOD = typeof navigator !== "undefined" && /Mac/.test(navigator.platform) ? "Cmd" : "Ctrl";

/** What the message list lets the header drive. */
type ThreadNav = {
  /** Scroll to the previous (-1) or next (1) user prompt from the top of the viewport. */
  stepPrompt: (dir: 1 | -1) => void;
  /** Scroll a message to the center, opening its tool run first if it is folded. */
  jumpTo: (messageId: number) => void;
};

export function ThreadView() {
  const threadId = useUi((s) => s.selectedThreadId);
  const setView = useUi((s) => s.setView);
  const openProject = useUi((s) => s.openProject);
  const target = useUi((s) => s.targetMessageId);
  const matchIds = useUi((s) => s.matchMessageIds);
  const openSeq = useUi((s) => s.openSeq);
  const addContext = useChat((s) => s.addContext);
  const vaultDir = useSettings((s) => s.vaultDir);
  const synthProvider = useSettings((s) => s.synthProvider);
  const synthModel = useSettings((s) => s.synthModel);
  const [copyState, setCopyState] = useState<"idle" | "copying" | "copied">("idle");
  // Notices remember which thread they are about, so opening another thread hides them.
  const [copyError, setCopyError] = useState<Notice | null>(null);
  const [contextNotice, setContextNotice] = useState<Notice | null>(null);
  const [exported, setExported] = useState(false);
  const nav = useRef<ThreadNav>(null);

  const { data, isLoading } = useQuery({
    queryKey: ["thread", threadId],
    queryFn: () => api.getThread(threadId as number),
    enabled: threadId != null,
    // A long transcript is ~12 MB of heap, and nothing reads a thread once it is closed, so
    // drop it after a minute instead of the default five. App holds the open thread while
    // the Search tab is hidden (RetainOpenThread), so a visit to Chat doesn't drop it.
    gcTime: 60_000,
    // Never refetch on its own: a big thread is megabytes over IPC, and every change to it
    // goes through the cache (star and tag edits patch it, main.tsx marks it stale when the
    // watcher reindexes). Otherwise showing the Search tab again refetched it after 5 s.
    staleTime: Infinity,
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
          text: `Couldn't copy the context: ${errorText(e)}`,
        });
      },
    );
  };
  const openInCli = useMutation({
    mutationFn: (program: string) => api.openThreadInCli(threadId as number, program),
  });
  // Default staleTime on purpose: nothing invalidates this when a key is saved in Settings,
  // so it refreshes when the Search tab is shown again (one cheap IPC).
  const canSynth = useQuery({ queryKey: ["can_synthesize"], queryFn: api.canSynthesize });
  // Shared with App's header count. With nothing indexed there is nothing to select, so the
  // empty pane says where threads will come from instead.
  // The results list owns the "nothing indexed" state (its empty-state Reindex is the one
  // primary button then); db_stats alone can't tell, because Onboarding covers threads === 0.
  const indexEmpty = useUi((s) => s.indexEmpty);
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

  // The git commits this thread likely produced (inferred file-overlap links). Reads stored
  // links; "Find produced commits" (More actions) and "Rescan" recompute them by reading the
  // project's `git log`.
  const commits = useQuery({
    queryKey: ["thread_commits", threadId],
    queryFn: () => api.threadCommits(threadId as number),
    enabled: threadId != null,
    // Default staleTime: besides the scan below (which writes the cache), `cal commits`
    // writes stored links from a terminal, so main.tsx invalidates this on index:updated
    // and index:done.
  });
  const linkCommits = useMutation({
    // The id travels as a variable, like toggleStar: a scan can outlive the open thread.
    mutationFn: (id: number) => api.linkThreadCommits(id),
    onSuccess: (rows, id) => queryClient.setQueryData(["thread_commits", id], rows),
  });

  // Matches in this thread, in reading order. The hit that opened the thread is the
  // first current match; the control then steps through the rest.
  const messageOrder = useMemo(() => {
    const order = new Map<number, number>();
    data?.messages.forEach((m, i) => {
      order.set(m.id, i);
    });
    return order;
  }, [data]);
  const matches = useMemo(() => {
    const ids = matchIds.length > 0 ? matchIds : EMPTY_IDS;
    return ids
      .filter((id) => messageOrder.has(id))
      .sort((a, b) => (messageOrder.get(a) ?? 0) - (messageOrder.get(b) ?? 0));
  }, [matchIds, messageOrder]);
  // The picked match belongs to one opening of one thread and one set of matches; a new
  // search, thread, or a click on the open thread's row again (openSeq) falls back to the
  // hit that opened it, without an effect to reset it.
  const matchKey = `${data?.id}:${target}:${openSeq}:${matches.join(",")}`;
  const [picked, setPicked] = useState<{ key: string; id: number } | null>(null);
  const currentMatch =
    picked?.key === matchKey
      ? picked.id
      : target != null && matches.includes(target)
        ? target
        : (matches[0] ?? null);
  const stepMatch = useCallback(
    (dir: 1 | -1) => {
      if (matches.length === 0) return;
      const at = currentMatch == null ? -1 : matches.indexOf(currentMatch);
      const next = matches[(at + dir + matches.length) % matches.length];
      setPicked({ key: matchKey, id: next });
      nav.current?.jumpTo(next);
    },
    [matches, currentMatch, matchKey],
  );
  const promptCount = useMemo(
    () => data?.messages.reduce((n, m) => (m.role === "user" && !isTool(m) ? n + 1 : n), 0) ?? 0,
    [data],
  );

  // Reading shortcuts. A window listener (not the pane's onKeyDown) so they work right
  // after clicking a result, while focus is still in the list. The Search tab's Activity
  // removes it while another tab is shown.
  const hasThread = data != null;
  useEffect(() => {
    if (!hasThread) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || useUi.getState().commandOpen) return;
      const typing = isTypingTarget(e.target);
      // AltGr arrives as Ctrl+Alt on Windows; it types characters, so it is no modifier.
      const altGr = e.ctrlKey && e.altKey;
      const mod = e.metaKey || (e.ctrlKey && !altGr);
      if (mod && !e.altKey && e.key.toLowerCase() === "g") {
        // From the search box (where the matches came from) or outside text fields only:
        // in a tag input or a textarea it stays the field's own shortcut.
        if (typing && (e.target as HTMLElement).id !== SEARCH_INPUT_ID) return;
        if (matches.length === 0) return;
        e.preventDefault();
        stepMatch(e.shiftKey ? -1 : 1);
        return;
      }
      if (typing || mod) return;
      // [ and ] by the character typed, whatever modifier the layout needs for it (Option+5
      // on a German Mac sets altKey), so the Alt+Arrow pair is matched separately.
      const arrows = e.altKey && !altGr;
      const prev = e.key === "[" || (arrows && e.key === "ArrowUp");
      const next = e.key === "]" || (arrows && e.key === "ArrowDown");
      if (!prev && !next) return;
      e.preventDefault();
      nav.current?.stepPrompt(prev ? -1 : 1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [hasThread, matches, stepMatch]);

  // Esc anywhere in the pane goes back to the search box, unless an open popup (the tag
  // suggestions) is taking it. Menus are portaled outside the pane's DOM, so their Esc
  // (which bubbles here through React) is left alone too.
  const onPaneKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.key !== "Escape" || e.defaultPrevented) return;
    const t = e.target as HTMLElement;
    if (!e.currentTarget.contains(t)) return;
    if (t.getAttribute("role") === "combobox" && t.getAttribute("aria-expanded") === "true") return;
    if (focusSearch()) e.preventDefault();
  };

  if (threadId == null)
    return (
      <div className="flex h-full items-center justify-center text-muted-foreground">
        {indexEmpty
          ? "Your threads appear here after the first index."
          : "Select a thread to read it."}
      </div>
    );
  if (isLoading) return <Loading label="Loading…" className="h-full" />;
  if (!data)
    return (
      <div className="flex h-full items-center justify-center text-muted-foreground">
        Thread not found.
      </div>
    );

  const title = data.title || "Untitled thread";
  // The Project page groups by the canonical key (a worktree resolves to its main repo); an
  // older backend sends only the path.
  const projectKey = data.projectKey ?? data.projectPath;
  const commitRows = commits.data ?? [];
  const scanning = linkCommits.isPending && linkCommits.variables === data.id;
  const scanned = linkCommits.variables === data.id;
  const matchAt = currentMatch == null ? -1 : matches.indexOf(currentMatch);

  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: Esc bubbling up from the pane's own controls, not a control itself
    <div className="flex h-full min-h-0 flex-col" onKeyDown={onPaneKeyDown}>
      <header className="@container/header border-b px-5 pt-3 pb-2.5">
        <div className="flex items-center gap-3">
          <h2 className="min-w-0 flex-1 truncate text-[15px] font-semibold" title={title}>
            {title}
          </h2>
          <div className="flex shrink-0 items-center gap-1.5">
            <KnowledgeButton threadId={data.id} />
            <Button
              size="icon-sm"
              variant="outline"
              onClick={() => toggleStar.mutate({ id: data.id, starred: !data.starred })}
              disabled={toggleStar.isPending}
              title={data.starred ? "Unstar" : "Star"}
              aria-label={data.starred ? "Unstar" : "Star"}
              aria-pressed={data.starred}
            >
              <Star className={cn("size-3.5", data.starred && "fill-current text-primary")} />
            </Button>
            {canResume && (
              <Button
                size="sm"
                variant="outline"
                onClick={() => resume.mutate()}
                disabled={resume.isPending}
                title={`Resume this session in ${SOURCE_LABELS[data.source]}`}
              >
                {resume.isPending ? (
                  <>
                    <Spinner data-icon="inline-start" className="size-3.5" />
                    Resuming…
                  </>
                ) : (
                  <>
                    Resume
                    <ArrowUpRight data-icon="inline-end" />
                  </>
                )}
              </Button>
            )}
            <DropdownMenu>
              <DropdownMenuTrigger
                render={
                  <Button
                    size="icon-sm"
                    variant="outline"
                    title="More actions"
                    aria-label="More actions"
                  />
                }
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
                  {copyState === "copied" ? (
                    <>
                      <Check />
                      Copied
                    </>
                  ) : (
                    "Copy context"
                  )}
                </DropdownMenuItem>
                {projectKey && (
                  <DropdownMenuItem onClick={() => openProject(projectKey)}>
                    <FolderGit2 />
                    Open project
                  </DropdownMenuItem>
                )}
                <DropdownMenuItem
                  onClick={() => linkCommits.mutate(data.id)}
                  disabled={linkCommits.isPending}
                >
                  <GitCommitHorizontal />
                  {commitRows.length > 0 ? "Rescan produced commits" : "Find produced commits"}
                </DropdownMenuItem>

                <div className="-mx-1 my-1 h-px bg-border" />
                <div className="px-2 py-1 text-xs text-muted-foreground">
                  {vaultDir ? "Export to Obsidian" : "Set a vault in Settings first"}
                </div>
                <DropdownMenuItem onClick={() => exportNote.mutate()} disabled={!vaultDir}>
                  {exported ? (
                    <>
                      <Check />
                      Exported
                    </>
                  ) : (
                    "Quick note (transcript)"
                  )}
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
        {/* One line: the path truncates first; the separators sit inside the segment
            they lead, so nothing can leave a dangling dot. */}
        <p className="mt-1 flex min-w-0 items-baseline gap-x-1 whitespace-nowrap text-[13px] text-muted-foreground">
          <span className="shrink-0">{SOURCE_LABELS[data.source]}</span>
          {data.projectPath && (
            <span className="min-w-0 truncate" title={data.projectPath}>
              · {shortPath(data.projectPath)}
            </span>
          )}
          {data.gitBranch && (
            <span className="min-w-0 max-w-[45%] shrink-0 truncate" title={data.gitBranch}>
              · {data.gitBranch}
            </span>
          )}
          <span className="shrink-0">· {formatTime(data.updatedAt)}</span>
        </p>
        <div className="mt-1 flex min-h-7 items-center gap-2">
          <div className="min-w-0">
            {/* Keyed per thread so a half-typed tag doesn't carry over to the next thread. */}
            <TagsEditor key={data.id} threadId={data.id} tags={data.tags} />
          </div>
          {(matches.length > 0 || promptCount > 1) && (
            <div className="ml-auto flex shrink-0 items-center gap-1.5">
              {matches.length > 0 && (
                <Stepper
                  // A narrow pane drops the word so the tags row stays one line; the
                  // group and button labels still say "match".
                  label={
                    <>
                      <span className="@max-[34rem]/header:hidden">Match </span>
                      {matchAt + 1} of {matches.length}
                    </>
                  }
                  groupLabel="Search matches"
                  onStep={stepMatch}
                  prevTitle={`Previous match (Shift+Enter, Shift+${MOD}+G)`}
                  nextTitle={`Next match (Enter, ${MOD}+G)`}
                  enterSteps
                />
              )}
              {promptCount > 1 && (
                <Stepper
                  label="Prompts"
                  groupLabel="Your prompts"
                  onStep={(dir) => nav.current?.stepPrompt(dir)}
                  prevTitle={`Previous prompt (${PROMPT_KEYS})`}
                  nextTitle={`Next prompt (${NEXT_PROMPT_KEYS})`}
                />
              )}
            </div>
          )}
        </div>
        {addToChat.isError && (
          <div className="mt-1 text-xs text-destructive">
            Couldn't add the thread to chat: {errorText(addToChat.error)}
          </div>
        )}
        {contextNotice?.threadId === data.id && (
          <div className="mt-1 text-xs text-destructive">{contextNotice.text}</div>
        )}
        {copyError?.threadId === data.id && (
          <div className="mt-1 text-xs text-destructive">{copyError.text}</div>
        )}
        {copyState === "copied" && (
          // The menu closes on click, so confirm here where it stays visible.
          <div className="mt-1 flex items-center gap-1 text-xs text-muted-foreground">
            <Check className="size-3" />
            Copied the thread context.
          </div>
        )}
        {resume.isError && (
          <div className="mt-1 text-xs text-destructive">
            Couldn't resume the session: {errorText(resume.error)}
          </div>
        )}
        {openInCli.isError && (
          <div className="mt-1 text-xs text-destructive">
            Couldn't open a terminal: {errorText(openInCli.error)}
          </div>
        )}
        {scanning && commitRows.length === 0 && (
          <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
            <Spinner className="size-3" />
            Scanning git for produced commits…
          </div>
        )}
        {scanned && linkCommits.isError && (
          <div className="mt-1 text-xs text-destructive">
            Couldn't scan git: {errorText(linkCommits.error)}. Check that the project folder is
            still a git repository, then try again from More actions.
          </div>
        )}
        {scanned && linkCommits.isSuccess && linkCommits.data.length === 0 && (
          <div className="mt-1 text-xs text-muted-foreground">
            No commits in this project's git log match the files this thread changed.
          </div>
        )}
        {exportNote.isError && (
          <div className="mt-1 text-xs text-destructive">
            Couldn't write the note: {errorText(exportNote.error)}
          </div>
        )}
        {synthExport.isError && (
          <div className="mt-1 text-xs text-destructive">
            Couldn't synthesize the note: {errorText(synthExport.error)}
          </div>
        )}
        {synthExport.isPending && (
          <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
            <Spinner className="size-3" />
            Synthesizing with the LLM…
          </div>
        )}
        {exported && (synthExport.data || exportNote.data) && (
          <div className="mt-1 text-xs text-muted-foreground">
            Wrote {synthExport.data ?? exportNote.data}
          </div>
        )}
      </header>
      {commitRows.length > 0 && (
        <CommitsSection
          commits={commitRows}
          rescanning={scanning}
          onRescan={() => linkCommits.mutate(data.id)}
        />
      )}
      <MessageList
        key={data.id}
        messages={data.messages}
        navRef={nav}
        matches={matches}
        currentMatch={currentMatch}
        openSeq={openSeq}
      />
    </div>
  );
}

/** A compact up/down stepper: [^ label v]. With `enterSteps`, Enter and Shift+Enter on
 *  either button step forward and back (like a find bar). */
function Stepper({
  label,
  groupLabel,
  onStep,
  prevTitle,
  nextTitle,
  enterSteps,
}: {
  label: ReactNode;
  groupLabel: string;
  onStep: (dir: 1 | -1) => void;
  prevTitle: string;
  nextTitle: string;
  enterSteps?: boolean;
}) {
  return (
    <fieldset
      aria-label={groupLabel}
      className="m-0 flex h-7 min-w-0 items-center rounded-lg border border-border bg-background p-0 dark:border-input dark:bg-input/30"
      onKeyDown={(e) => {
        if (!enterSteps || e.key !== "Enter") return;
        e.preventDefault();
        onStep(e.shiftKey ? -1 : 1);
      }}
    >
      <Button
        size="icon-xs"
        variant="ghost"
        className="ml-0.5"
        onClick={() => onStep(-1)}
        title={prevTitle}
        aria-label={prevTitle}
      >
        <ChevronUp />
      </Button>
      <span className="px-1 text-xs tabular-nums text-muted-foreground">{label}</span>
      <Button
        size="icon-xs"
        variant="ghost"
        className="mr-0.5"
        onClick={() => onStep(1)}
        title={nextTitle}
        aria-label={nextTitle}
      >
        <ChevronDown />
      </Button>
    </fieldset>
  );
}

/** The git commits this thread likely produced. Shown only when there are some; finding
 *  them the first time is in More actions. */
function CommitsSection({
  commits,
  rescanning,
  onRescan,
}: {
  commits: CommitLink[];
  rescanning: boolean;
  onRescan: () => void;
}) {
  return (
    <div className="border-b px-5 py-2">
      <div className="mb-1 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
        <GitCommitHorizontal className="size-3.5" />
        Produced commits
        <button
          type="button"
          className="ml-auto inline-flex items-center gap-1 rounded font-normal underline-offset-2 outline-none hover:underline focus-visible:ring-2 focus-visible:ring-ring/50 disabled:opacity-50"
          onClick={onRescan}
          disabled={rescanning}
        >
          {rescanning ? (
            <>
              <Spinner className="size-3" />
              Rescanning…
            </>
          ) : (
            "Rescan"
          )}
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

/** A tool call (stored as an assistant row with toolName) or a tool result (role "tool"). */
function isTool(m: MessageRow): boolean {
  return m.toolName != null || m.role === "tool";
}

/** One tool row: a call with the result that answers it, or a call or result on its own
 *  (when the pairing is not certain). `label` is the tool name, or "Result" for a result
 *  whose call can't be named. */
type ToolRow = { m: MessageRow; result: MessageRow | null; label: string };

/** A run of consecutive tool messages [start, end], keyed by its first message id, which
 *  survives a refetch that appends messages. Folded into one summary row when it has two or
 *  more tool rows; a lone call with its result is one tool row, readable in one click. */
type ToolRun = {
  key: number;
  start: number;
  end: number;
  rows: ToolRow[];
  folded: boolean;
  summary: string;
};

/** One row of the virtual list: a prose message, a tool row (alone or inside an open run),
 *  or a folded run's summary. */
type Item =
  | { kind: "message"; m: MessageRow; showModel: boolean }
  | { kind: "tool"; row: ToolRow; member: boolean; last: boolean }
  | { kind: "run"; run: ToolRun; open: boolean };

/** "12 tool calls · Bash 5 · Read 4 · Edit 3" for a run's rows. */
function runSummary(rows: ToolRow[]): string {
  const byName = new Map<string, number>();
  let calls = 0;
  for (const { m } of rows) {
    const name = m.toolName;
    if (!name) continue;
    calls++;
    byName.set(name, (byName.get(name) ?? 0) + 1);
  }
  if (calls === 0) return `${rows.length} tool result${rows.length === 1 ? "" : "s"}`;
  const names = [...byName].sort((a, b) => b[1] - a[1]);
  const shown = names.slice(0, 4).map(([name, n]) => (n > 1 ? `${name} ${n}` : name));
  if (names.length > 4) shown.push(`${names.length - 4} more`);
  return [`${calls} tool call${calls === 1 ? "" : "s"}`, ...shown].join(" · ");
}

/** Pair each result in [start, end] with the call it answers. A result has no tool name or
 *  call id of its own, but results come back in call order (parallel calls are listed
 *  first, then their results), so a batch of calls followed by at least as many results
 *  pairs in order. With fewer results than calls (an interrupted call, a source that drops
 *  some), which call each answers would be a guess that mislabels every later one, so they
 *  stay separate rows labeled plainly "Result". */
function pairRun(messages: MessageRow[], start: number, end: number): ToolRow[] {
  const rows: ToolRow[] = [];
  let j = start;
  while (j <= end) {
    const calls: MessageRow[] = [];
    while (j <= end && messages[j].toolName) calls.push(messages[j++]);
    const results: MessageRow[] = [];
    while (j <= end && !messages[j].toolName) results.push(messages[j++]);
    const paired = results.length >= calls.length ? calls.length : 0;
    calls.forEach((c, k) => {
      rows.push({ m: c, result: k < paired ? results[k] : null, label: c.toolName ?? "Tool" });
    });
    for (let k = paired; k < results.length; k++) {
      rows.push({ m: results[k], result: null, label: "Result" });
    }
  }
  return rows;
}

/** Find the tool runs and pair their rows. `runAt` is keyed by a run's first index,
 *  `runOf` maps every message of a folded run to it, and `rowOf` maps every tool message
 *  to the id of the row that shows it (the call's, for a paired result). */
function analyzeThread(messages: MessageRow[]) {
  const runAt = new Map<number, ToolRun>();
  const runOf = new Map<number, ToolRun>();
  const rowOf = new Map<number, number>();
  let i = 0;
  while (i < messages.length) {
    if (!isTool(messages[i])) {
      i++;
      continue;
    }
    let end = i;
    while (end + 1 < messages.length && isTool(messages[end + 1])) end++;
    const rows = pairRun(messages, i, end);
    for (const r of rows) {
      rowOf.set(r.m.id, r.m.id);
      if (r.result) rowOf.set(r.result.id, r.m.id);
    }
    const folded = rows.length > 1;
    const run = {
      key: messages[i].id,
      start: i,
      end,
      rows,
      folded,
      summary: folded ? runSummary(rows) : "",
    };
    runAt.set(i, run);
    if (folded) for (let j = i; j <= end; j++) runOf.set(messages[j].id, run);
    i = end + 1;
  }
  return { runAt, runOf, rowOf };
}

/** Height guesses for rows before they are measured, so a long thread's scroll height is
 *  close from the start (one flat guess for every row made a 7,570-message thread's height
 *  about 25% wrong, and the virtualizer kept correcting the offset while scrolling up).
 *  Prose was fitted on that thread's rendered rows at the default 1,100 px window: about
 *  86 + 0.145 px per char + 17 px per line break (overall bias under 1%); a message over
 *  the markdown limit renders as a collapsed block. Tool rows are one line when collapsed:
 *  a run summary 36 px, a tool row 40 px alone or 34 px inside an open run (measured in
 *  the mock, row padding included). */
const RUN_ROW_PX = 36;
const TOOL_ROW_PX = 40;
const MEMBER_ROW_PX = 34;

function estimateMessageSize(m: MessageRow): number {
  if (m.text.length > MAX_MARKDOWN_CHARS) return 90;
  let breaks = 0;
  for (let i = m.text.indexOf("\n"); i !== -1; i = m.text.indexOf("\n", i + 1)) breaks++;
  return Math.round(86 + 0.145 * m.text.length + 17 * breaks);
}

type Align = "center" | "end" | "start";

/** How long a jump is held against late layout (rows measured after it, markdown that
 *  loads late). virtual-core reconciles its own scrollToIndex until two stable frames;
 *  this covers the lazy markdown chunk landing after that. */
const HOLD_MS = 2_000;
/** Scroll offsets within this many px are the same position (subpixel rounding). */
const SCROLL_SLOP_PX = 2;

/** Whether the scroller sits where the virtualizer last put it: at the offset it wrote, or
 *  where that write landed (the browser clamps a write past the end, and again when the
 *  content shrinks under it). Anything else is the reader, a scrollbar drag, assistive
 *  tech or a focus scrollIntoView. */
function isOwnScroll(el: HTMLElement, w: { target: number; landed: number }): boolean {
  const top = el.scrollTop;
  const max = el.scrollHeight - el.clientHeight;
  const near = (a: number, b: number) => Math.abs(a - b) <= SCROLL_SLOP_PX;
  return (
    near(top, w.landed) ||
    near(top, Math.max(0, Math.min(w.target, max))) ||
    (w.target >= max - SCROLL_SLOP_PX && top >= max - SCROLL_SLOP_PX)
  );
}

function MessageList({
  messages,
  navRef,
  matches,
  currentMatch,
  openSeq,
}: {
  messages: MessageRow[];
  navRef: Ref<ThreadNav>;
  matches: readonly number[];
  currentMatch: number | null;
  openSeq: number;
}) {
  const parentRef = useRef<HTMLElement>(null);
  // Expansion state lives here, keyed by message id, not in each row: the virtualizer
  // unmounts rows that leave the overscan window, which used to collapse an expanded tool
  // row (and forget a large message's "Show more" progress) after scrolling ~10 rows away.
  // The list is keyed per thread, so each thread starts collapsed. Open tool runs are
  // keyed by their first message id the same way, and a paired tool row by its call's id.
  const [openTools, setOpenTools] = useState<ReadonlySet<number>>(() => new Set());
  const [openRuns, setOpenRuns] = useState<ReadonlySet<number>>(() => new Set());
  const [largeStates] = useState(() => new Map<number, LargeMessageState>());
  const setToolOpen = useCallback((id: number, open: boolean) => {
    setOpenTools((prev) => toggled(prev, id, open));
  }, []);
  const setRunOpen = useCallback((key: number, open: boolean) => {
    setOpenRuns((prev) => toggled(prev, key, open));
  }, []);

  // Runs and pairs depend only on the messages; computed once per `messages`.
  const thread = useMemo(() => analyzeThread(messages), [messages]);
  // The rows to virtualize: each folded run is one summary row, followed by its tool rows
  // while it is open. Message estimates are computed here, once per `messages` (counting
  // line breaks is a scan of the text), not each time the virtualizer asks.
  const messageEstimates = useMemo(() => messages.map(estimateMessageSize), [messages]);
  const { items, estimates, indexOf, prompts } = useMemo(() => {
    const items: Item[] = [];
    const estimates: number[] = [];
    // Message id to its row; a paired result maps to its call's row, so a match on the
    // output still lands.
    const indexOf = new Map<number, number>();
    const prompts: number[] = [];
    let model: string | null = null;
    let i = 0;
    while (i < messages.length) {
      const run = thread.runAt.get(i);
      if (run) {
        const open = run.folded && openRuns.has(run.key);
        if (run.folded) {
          items.push({ kind: "run", run, open });
          estimates.push(RUN_ROW_PX);
        }
        if (!run.folded || open) {
          run.rows.forEach((row, k) => {
            indexOf.set(row.m.id, items.length);
            if (row.result) indexOf.set(row.result.id, items.length);
            items.push({ kind: "tool", row, member: run.folded, last: k === run.rows.length - 1 });
            estimates.push(run.folded ? MEMBER_ROW_PX : TOOL_ROW_PX);
          });
        }
        i = run.end + 1;
        continue;
      }
      const m = messages[i];
      indexOf.set(m.id, items.length);
      if (m.role === "user") prompts.push(items.length);
      // The model id shows on the first reply and wherever it changes, not on every card.
      const showModel = m.role === "assistant" && m.model != null && m.model !== model;
      if (m.role === "assistant" && m.model) model = m.model;
      items.push({ kind: "message", m, showModel });
      estimates.push(messageEstimates[i]);
      i++;
    }
    return { items, estimates, indexOf, prompts };
  }, [messages, thread, openRuns, messageEstimates]);

  // Stable per `items`: the virtualizer rebuilds its layout when getItemKey changes.
  // Keying by message id keeps measured heights on the right rows when a refetch changes
  // the array or a run opens.
  const getItemKey = useCallback(
    (i: number) => {
      const it = items[i];
      return it.kind === "run" ? `run-${it.run.key}` : it.kind === "tool" ? it.row.m.id : it.m.id;
    },
    [items],
  );
  const estimateSize = useCallback((i: number) => estimates[i], [estimates]);
  // Every offset the virtualizer writes (its own reconcile and size adjustments too), and
  // where it landed, so onScroll can tell its scrolls from everyone else's.
  const written = useRef<{ target: number; landed: number } | null>(null);
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize,
    getItemKey,
    overscan: 8,
    scrollToFn: (offset, options, instance) => {
      elementScroll(offset, options, instance);
      written.current = {
        target: offset + (options.adjustments ?? 0),
        landed: instance.scrollElement?.scrollTop ?? 0,
      };
    },
  });

  // Rows measured after a jump (and markdown that loads late) change the heights above
  // the target, so the target is held for a moment: each change in total size re-applies
  // it. One correction on the next frame left the newest message ~100 px short of the end
  // in the long mock thread. The hold ends after HOLD_MS, or as soon as anything else moves
  // the scroller (onScroll), so it never undoes the reader.
  const hold = useRef<{ index: number; align: Align; until: number } | null>(null);
  const scrollToItem = useCallback(
    (index: number, align: Align) => {
      hold.current = { index, align, until: performance.now() + HOLD_MS };
      virtualizer.scrollToIndex(index, { align });
    },
    [virtualizer],
  );
  const totalSize = virtualizer.getTotalSize();
  useEffect(() => {
    const h = hold.current;
    if (!h || totalSize === 0) return;
    if (performance.now() > h.until) {
      hold.current = null;
      return;
    }
    virtualizer.scrollToIndex(h.index, { align: h.align });
  }, [totalSize, virtualizer]);
  // Hiding the Search tab (Activity) runs this cleanup: showing it again must not re-apply
  // an old jump.
  useEffect(
    () => () => {
      hold.current = null;
    },
    [],
  );
  const release = () => {
    hold.current = null;
  };
  // A tool row is aligned to its top, but one short row above it (its run's summary, or the
  // call before it) stays in view as context.
  const scrollToRow = useCallback(
    (idx: number, align: Align) => {
      const prev = items[idx - 1];
      const context = align === "start" && prev != null && prev.kind !== "message";
      scrollToItem(context ? idx - 1 : idx, align);
    },
    [items, scrollToItem],
  );
  // A jump into a folded run opens it first; the scroll then waits for the rows to exist.
  const pending = useRef<{ id: number; align: Align } | null>(null);
  const jumpTo = useCallback(
    (id: number) => {
      // A tool row opens too: most hits are tool output, and the matched text is in its
      // body, past the one-line summary. Aligned to its top so the opened body is in view.
      const row = thread.rowOf.get(id);
      if (row != null) setToolOpen(row, true);
      const align: Align = row != null ? "start" : "center";
      const run = thread.runOf.get(id);
      if (run && !openRuns.has(run.key)) {
        pending.current = { id, align };
        setRunOpen(run.key, true);
        return;
      }
      const idx = indexOf.get(id);
      if (idx != null) scrollToRow(idx, align);
    },
    [thread, openRuns, indexOf, setToolOpen, setRunOpen, scrollToRow],
  );
  useEffect(() => {
    const p = pending.current;
    if (p == null) return;
    const idx = indexOf.get(p.id);
    if (idx == null) return;
    pending.current = null;
    scrollToRow(idx, p.align);
  }, [indexOf, scrollToRow]);

  const stepPrompt = useCallback(
    (dir: 1 | -1) => {
      const el = parentRef.current;
      if (!el || prompts.length === 0) return;
      // A prompt counts as "here" within a few px of the top, so stepping from a prompt
      // that a previous step aligned moves on to the next one.
      const top = el.scrollTop;
      const start = (i: number) => virtualizer.measurementsCache[i]?.start ?? 0;
      let idx: number | undefined;
      if (dir === 1) idx = prompts.find((i) => start(i) > top + 4);
      else for (const i of prompts) if (start(i) < top - 4) idx = i;
      if (idx != null) scrollToItem(idx, "start");
    },
    [prompts, virtualizer, scrollToItem],
  );
  useImperativeHandle(navRef, () => ({ stepPrompt, jumpTo }), [stepPrompt, jumpTo]);

  // Arriving from a search hit: land on the matched message (opening its tool run and row
  // when they are folded). Otherwise open pinned to the newest message (chat-style), scroll
  // up for history. Once per opening (openSeq bumps on every selectThread, so clicking the
  // open thread's row again re-lands): a refetch (new `messages`), opening a run, showing
  // the Search tab again (effects re-run) or a new query clearing the target must not move
  // the reader.
  const target = useUi((s) => s.targetMessageId);
  const landed = useRef<number | undefined>(undefined);
  useEffect(() => {
    if (items.length === 0 || landed.current === openSeq) return;
    landed.current = openSeq;
    if (target != null && (indexOf.has(target) || thread.runOf.has(target))) jumpTo(target);
    else scrollToItem(items.length - 1, "end");
  }, [items.length, openSeq, target, thread, indexOf, jumpTo, scrollToItem]);

  // Matches inside each folded run, so its summary can say so (open or not).
  const matchSet = useMemo(() => new Set(matches), [matches]);
  const runMatches = useMemo(() => {
    const counts = new Map<number, number>();
    for (const id of matches) {
      const run = thread.runOf.get(id);
      if (run) counts.set(run.key, (counts.get(run.key) ?? 0) + 1);
    }
    return counts;
  }, [matches, thread]);

  // App hides the Search tab with display: none while another tab is open. Chromium keeps
  // a hidden scroller's offset, but not every engine is guaranteed to, so remember the
  // last offset seen while visible and put it back when the tab is shown (layout effects
  // re-run then) if it was lost.
  const savedTop = useRef(0);
  useLayoutEffect(() => {
    const el = parentRef.current;
    if (el && el.scrollTop === 0 && savedTop.current > 0) el.scrollTop = savedTop.current;
  }, []);
  return (
    <LargeMessageStates.Provider value={largeStates}>
      <section
        ref={parentRef}
        // Focusable so arrow keys and Page Up/Down scroll the transcript.
        aria-label="Transcript"
        // biome-ignore lint/a11y/noNoninteractiveTabindex: a scroll container must take focus to scroll by keyboard
        tabIndex={0}
        className="min-h-0 flex-1 overflow-y-auto pt-3 outline-none focus-visible:ring-2 focus-visible:ring-ring/50 focus-visible:ring-inset"
        // The reader taking over the scroll ends a held jump (see scrollToItem).
        onWheel={release}
        onPointerDown={release}
        onKeyDown={release}
        onTouchMove={release}
        onScroll={(e) => {
          const el = e.currentTarget;
          // clientHeight is 0 while hidden: an engine resetting the offset then is not the
          // reader scrolling.
          if (el.clientHeight === 0) return;
          savedTop.current = el.scrollTop;
          // A scroll the virtualizer did not write (a scrollbar drag sends no pointer
          // events in WKWebView) ends the hold.
          const w = written.current;
          if (hold.current && !(w && isOwnScroll(el, w))) release();
        }}
      >
        <div className="relative w-full" style={{ height: totalSize }}>
          {virtualizer.getVirtualItems().map((vrow) => {
            const it = items[vrow.index];
            return (
              <div
                key={vrow.key}
                data-index={vrow.index}
                ref={virtualizer.measureElement}
                className={cn(
                  "absolute left-0 top-0 w-full px-5",
                  it.kind === "run"
                    ? it.open
                      ? "pb-1"
                      : "pb-3"
                    : it.kind === "tool" && it.member && !it.last
                      ? ""
                      : "pb-3",
                )}
                style={{ transform: `translateY(${vrow.start}px)` }}
              >
                {it.kind === "run" ? (
                  <RunSummary
                    run={it.run}
                    open={it.open}
                    matches={runMatches.get(it.run.key) ?? 0}
                    onToggle={setRunOpen}
                  />
                ) : it.kind === "tool" ? (
                  <div className={cn(it.member && "ml-[11px] border-l pb-1 pl-3")}>
                    <ToolMessage
                      row={it.row}
                      ring={ringFor(it.row.m.id, it.row.result?.id, currentMatch, matchSet)}
                      open={openTools.has(it.row.m.id)}
                      onToggle={setToolOpen}
                    />
                  </div>
                ) : (
                  <Message
                    m={it.m}
                    showModel={it.showModel}
                    ring={ringFor(it.m.id, undefined, currentMatch, matchSet)}
                  />
                )}
              </div>
            );
          })}
        </div>
      </section>
    </LargeMessageStates.Provider>
  );
}

function toggled(prev: ReadonlySet<number>, id: number, on: boolean): ReadonlySet<number> {
  if (prev.has(id) === on) return prev;
  const next = new Set(prev);
  if (on) next.add(id);
  else next.delete(id);
  return next;
}

/** Matches get their own hue, the brand sunset (as in the Stats heatmap and the results'
 *  marks), so they never read as a card border or the gray keyboard focus ring: the current
 *  match a strong ring, other matches a faint ring and tint, findable while scrolling. The
 *  tint is a background image so it lays over the card's fill instead of replacing it. A
 *  paired tool row matches when its call or its result does. */
function ringFor(
  id: number,
  resultId: number | undefined,
  current: number | null,
  matches: ReadonlySet<number>,
): string {
  if (current != null && (id === current || resultId === current))
    return "ring-2 ring-[#C16E2C]/70";
  return matches.has(id) || (resultId != null && matches.has(resultId))
    ? "ring-1 ring-[#C16E2C]/40 bg-[linear-gradient(rgb(193_110_44/0.04),rgb(193_110_44/0.04))]"
    : "";
}

/** One line for a folded run of tool rows; opens in place to the rows themselves. */
const RunSummary = memo(function RunSummary({
  run,
  open,
  matches,
  onToggle,
}: {
  run: ToolRun;
  open: boolean;
  matches: number;
  onToggle: (key: number, open: boolean) => void;
}) {
  return (
    <button
      type="button"
      aria-expanded={open}
      onClick={() => onToggle(run.key, !open)}
      className="flex h-6 w-full items-center gap-1.5 rounded-md pr-1.5 text-left text-xs text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50"
    >
      <ChevronRight
        className={cn("size-3.5 shrink-0 transition-transform", open && "rotate-90")}
        aria-hidden="true"
      />
      <span className="min-w-0 truncate">{run.summary}</span>
      {matches > 0 && (
        <span className="ml-auto shrink-0 font-medium text-primary">
          {matches} match{matches === 1 ? "" : "es"}
        </span>
      )}
    </button>
  );
});

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
 *  a line-numbered file read, which isn't valid JSON). A call whose only argument is a
 *  command (Bash) shows just the command as shell: the JSON wrapper only repeats it. */
function toolBody(text: string, toolName: string | null): string {
  let body = text;
  if (toolName) {
    const sep = text.indexOf(": ");
    if (sep > 0) body = text.slice(sep + 2);
  }
  const trimmed = body.trim();
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      const parsed: unknown = JSON.parse(trimmed);
      if (toolName && parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        const keys = Object.keys(parsed);
        const command = (parsed as Record<string, unknown>).command;
        if (keys.length === 1 && typeof command === "string") return asCodeBlock(command, "sh");
      }
      return asCodeBlock(JSON.stringify(parsed, null, 2), "json");
    } catch {
      // Not valid JSON (e.g. a line-numbered file read), so fall through.
    }
  }
  // Label non-JSON output as plain text: unlabeled fences trigger highlight.js language
  // auto-detection across every grammar, ~30x the cost of plain rendering per row.
  return asCodeBlock(body, "text");
}

// The argument that says what a call did, in the order tools tend to carry them. Matched
// with a regex, not JSON.parse: get_thread sends a 300-char preview, which cuts long
// arguments (an Edit's old_string) mid-string.
const HINT_ARGS = ["command", "pattern", "file_path", "path", "query", "url", "description"].map(
  (k) => new RegExp(`"${k}"\\s*:\\s*"((?:[^"\\\\]|\\\\.)*)`),
);

/** A one-line hint after the tool row's label: the call's main argument, or the first
 *  line of a result. */
function toolHint(m: MessageRow): string {
  if (m.toolName) {
    const sep = m.text.indexOf(": ");
    const body = sep > 0 ? m.text.slice(sep + 2) : m.text;
    for (const re of HINT_ARGS) {
      const raw = body.match(re)?.[1];
      if (raw == null) continue;
      try {
        return (JSON.parse(`"${raw}"`) as string).replace(/\s+/g, " ").trim();
      } catch {
        return raw; // cut mid-escape by the preview
      }
    }
    return body.replace(/\s+/g, " ").trim().slice(0, 160);
  }
  const line = m.text.split("\n", 8).find((l) => l.trim()) ?? "";
  return line.trim().slice(0, 160);
}

/** A tool message's body. get_thread sends only a preview of a tool row's text (most are
 *  never expanded, and their output is most of a long thread's payload), so the full text
 *  is fetched when the row is first opened. Mounted only while the row is open. */
function ToolBody({ m, what }: { m: MessageRow; what: "arguments" | "output" }) {
  // textLen is the full length; a backend that sends whole rows makes these equal.
  const truncated = (m.textLen ?? 0) > m.text.length;
  const full = useQuery({
    queryKey: ["message_text", m.id],
    queryFn: () => api.messageText(m.id),
    enabled: truncated,
    // A stored message never changes once indexed.
    staleTime: Infinity,
  });
  const text = truncated ? (full.data ?? m.text) : m.text;
  return (
    <>
      {truncated && full.isPending && (
        <div className="mb-1 flex items-center gap-1.5 text-xs text-muted-foreground">
          <Spinner className="size-3" />
          Loading the full {what}…
        </div>
      )}
      {truncated && full.isError && (
        <div className="mb-1 text-xs text-destructive">
          Couldn't load the full {what}, showing the first {m.text.length} characters. Close and
          reopen this row to try again.
        </div>
      )}
      <Markdown stateId={m.id}>{toolBody(text, m.toolName)}</Markdown>
    </>
  );
}

/** A collapsed tool row: a call with its result, or either alone. The summary line is the
 *  call's main argument with the result's first line after it; opened, the call's
 *  arguments, then its output below a hairline. Bodies are parsed only while open. */
function ToolMessage({
  row,
  ring,
  open,
  onToggle,
}: {
  row: ToolRow;
  ring: string;
  open: boolean;
  onToggle: (id: number, open: boolean) => void;
}) {
  const { m, result, label } = row;
  const hint = useMemo(() => toolHint(m), [m]);
  const resultHint = useMemo(() => (result ? toolHint(result) : ""), [result]);
  return (
    <details
      className={cn("group/tool rounded-md border bg-muted/40 text-sm", ring)}
      open={open}
      onToggle={(e) => {
        if (e.currentTarget.open !== open) onToggle(m.id, e.currentTarget.open);
      }}
    >
      <summary className="flex h-6 cursor-pointer list-none items-center gap-1.5 rounded-md px-2 text-xs outline-none focus-visible:ring-2 focus-visible:ring-ring/50 [&::-webkit-details-marker]:hidden">
        <ChevronRight
          className="size-3.5 shrink-0 text-muted-foreground transition-transform group-open/tool:rotate-90"
          aria-hidden="true"
        />
        <span className="shrink-0 font-medium text-foreground/80">{label}</span>
        {hint && (
          <span
            className={cn(
              "min-w-0 truncate font-mono text-[11px] text-muted-foreground",
              resultHint && "max-w-[60%] shrink-0",
            )}
          >
            {hint}
          </span>
        )}
        {resultHint && (
          <span className="flex min-w-0 flex-1 items-center gap-1 text-muted-foreground/70">
            <CornerDownRight className="size-3 shrink-0" aria-label="Output" />
            <span className="min-w-0 truncate font-mono text-[11px]">{resultHint}</span>
          </span>
        )}
      </summary>
      {open && (
        <div className="border-t text-[0.8rem]">
          <div className="max-h-80 overflow-auto px-3 py-2">
            <ToolBody m={m} what={m.toolName ? "arguments" : "output"} />
          </div>
          {result && (
            <div className="max-h-80 overflow-auto border-t px-3 py-2">
              <ToolBody m={result} what="output" />
            </div>
          )}
        </div>
      )}
    </details>
  );
}

const ROLE_LABELS: Record<string, string> = { user: "You", assistant: "Assistant" };

/** A prose message. Roles differ by their label and fill (your prompts sit on the muted
 *  fill), not by a colored stripe. */
const Message = memo(function Message({
  m,
  ring,
  showModel,
}: {
  m: MessageRow;
  ring: string;
  showModel: boolean;
}) {
  const user = m.role === "user";
  return (
    <div className={cn("rounded-lg border px-3 py-2", user ? "bg-muted/50" : "bg-card", ring)}>
      <div className="mb-1 flex items-center gap-2 text-xs">
        <span className="font-medium text-foreground">
          {ROLE_LABELS[m.role] ?? m.role.charAt(0).toUpperCase() + m.role.slice(1)}
        </span>
        {showModel && m.model && (
          <span className="font-mono text-[11px] text-muted-foreground">{m.model}</span>
        )}
      </div>
      <Markdown stateId={m.id}>{m.text}</Markdown>
    </div>
  );
});
