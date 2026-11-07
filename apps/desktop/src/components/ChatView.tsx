import { memo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Toggle } from "@base-ui/react/toggle";
import { ToggleGroup } from "@base-ui/react/toggle-group";
import { api, PROVIDERS } from "../lib/api";
import {
  type AskMeta,
  CONTEXT_PREFIX,
  type ChatMode,
  type ContextMeta,
  contextTitle,
  isContextMessage,
  useChat,
  type StreamPart,
  type ToolStep,
} from "../store/chat";
import { useUi } from "../store/ui";
import { isSubmitEnter } from "../lib/keyboard";
import { Button } from "@/components/ui/button";
import { Kbd, KbdGroup } from "@/components/ui/kbd";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import {
  ArrowUp,
  Brain,
  Check,
  KeyRound,
  Loader2,
  MessagesSquare,
  Paperclip,
  Search,
  Settings2,
  Sparkles,
  Square,
  Terminal,
  TriangleAlert,
  X,
} from "lucide-react";
import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from "@/components/ui/message-scroller";
import { Message, MessageContent } from "@/components/ui/message";
import { Bubble, BubbleContent } from "@/components/ui/bubble";
import { Marker, MarkerContent, MarkerIcon } from "@/components/ui/marker";
import {
  Attachment,
  AttachmentAction,
  AttachmentActions,
  AttachmentContent,
  AttachmentDescription,
  AttachmentMedia,
  AttachmentTitle,
} from "@/components/ui/attachment";
import { AskAnswer, CitedMarkdown, splitSavedSources } from "./AskAnswer";
import { ChatEnginePicker, useChatEngine } from "./ChatEnginePicker";
import { ChatSidebar } from "./ChatSidebar";
import { MemorySetupCard } from "./MemorySetupCard";
import { StreamingMarkdown } from "./Markdown";

const ASK_EXAMPLES = [
  "How did we handle authentication?",
  "What did we decide about the database schema?",
  "Have we hit this kind of error before?",
];

const MODES: { id: ChatMode; label: string; icon: typeof Sparkles; hint: string }[] = [
  {
    id: "chat",
    label: "Chat",
    icon: MessagesSquare,
    hint: "Chat with your provider; it searches your history when that helps",
  },
  {
    id: "ask",
    label: "Ask",
    icon: Sparkles,
    hint: "One cited answer from your past sessions, on your memory engine",
  },
];

/** "Chat" | "Ask": how the next message is answered (store/chat.ts ChatMode). Same control
 *  as Settings > General's theme picker. */
function ModeToggle({
  mode,
  disabled,
  onChange,
}: {
  mode: ChatMode;
  disabled: boolean;
  onChange: (m: ChatMode) => void;
}) {
  return (
    <ToggleGroup
      aria-label="Answer with"
      value={[mode]}
      disabled={disabled}
      onValueChange={(v) => {
        const next = v[0] as ChatMode | undefined;
        if (next) onChange(next);
      }}
      className="inline-flex shrink-0 rounded-lg bg-muted p-0.5"
    >
      {MODES.map((m) => (
        <Toggle
          key={m.id}
          value={m.id}
          title={m.hint}
          className={cn(
            "inline-flex h-7 items-center gap-1.5 rounded-[min(var(--radius-md),12px)] px-2.5 text-[0.8rem] font-medium whitespace-nowrap text-muted-foreground transition-colors outline-none select-none",
            "hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50",
            "data-pressed:bg-background data-pressed:text-foreground data-pressed:shadow-sm",
            "data-disabled:pointer-events-none data-disabled:opacity-60",
            "[&_svg]:size-3.5 [&_svg]:shrink-0",
          )}
        >
          <m.icon aria-hidden="true" />
          {m.label}
        </Toggle>
      ))}
    </ToggleGroup>
  );
}

/** The memory engine's name as the Ask header shows it: the label Settings uses for it. */
function useMemoryEngineLabel(): {
  label: string;
  model: string | null;
  on: boolean;
  /** knowledge_config has not answered yet, or failed: whether memory is on is unknown. */
  status: "pending" | "error" | "success";
  retry: () => void;
} {
  // Same key App and the memory card read, so these are cache hits.
  const knowledge = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  const cliEngines = useQuery({ queryKey: ["cli_engines"], queryFn: api.cliEngines });
  const provider = knowledge.data?.provider ?? null;
  const label = !provider
    ? "First saved API key"
    : (cliEngines.data?.find((e) => e.id === provider)?.label ??
      PROVIDERS.find((p) => p.id === provider)?.label ??
      provider);
  return {
    label,
    model: knowledge.data?.model ?? null,
    on: !!knowledge.data?.enabled,
    status: knowledge.status,
    retry: () => void knowledge.refetch(),
  };
}

export function ChatView() {
  const messages = useChat((s) => s.messages);
  const loadingThread = useChat((s) => s.loadingThread);
  const error = useChat((s) => s.error);
  // Streaming state lives in the store, so it survives this view unmounting when the
  // user switches tabs mid-reply (the reply, its tool steps and Stop are still here).
  const isStreaming = useChat((s) => s.isStreaming);
  const pending = useChat((s) => s.pending);
  const stop = useChat((s) => s.stop);
  const mode = useChat((s) => s.mode);
  const setMode = useChat((s) => s.setMode);
  const openSettings = useUi((s) => s.openSettings);

  const [draft, setDraft] = useState("");
  const engine = useChatEngine();
  const memory = useMemoryEngineLabel();
  // Ask answers on the memory engine (ask_history), which needs LLM distillation on. Only a
  // config that answered "off" blocks: while it loads (or failed) the setup card would flash
  // for someone whose memory is on, and the backend still refuses a send while it is off.
  const askBlocked = mode === "ask" && memory.status === "success" && !memory.on;

  /** Send the draft, or `example` (an Ask example question), which leaves the draft alone
   *  so a half-typed message survives the click. */
  function send(example?: string) {
    const q = (example ?? draft).trim();
    if (!q || useChat.getState().isStreaming || askBlocked) return;
    if (example === undefined) setDraft("");
    void useChat.getState().send(q, { baseUrl: engine.baseUrl || null });
  }

  const askEmpty = mode === "ask" && messages.length === 0 && !isStreaming;
  const askSetupOnly = askEmpty && askBlocked;

  return (
    <div className="flex min-h-0 flex-1">
      <ChatSidebar />
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div className="flex min-w-0 items-center gap-3 border-b px-4 py-2.5">
          <ModeToggle mode={mode} disabled={isStreaming} onChange={setMode} />
          {mode === "chat" ? (
            <ChatEnginePicker engine={engine} className="flex-1" />
          ) : (
            <p className="min-w-0 flex-1 truncate text-[13px] text-muted-foreground">
              {memory.status === "pending" ? (
                "Answers from your history"
              ) : memory.status === "error" ? (
                "Couldn't read the memory engine setting"
              ) : memory.on ? (
                <>
                  Answers from your history with{" "}
                  <span className="font-medium text-foreground">{memory.label}</span>
                  {memory.model ? ` · ${memory.model}` : ""}
                </>
              ) : (
                "Ask needs memory turned on"
              )}
            </p>
          )}
          <Button
            size="icon-sm"
            variant="ghost"
            aria-label="AI engine settings"
            title="AI engine settings: keys, server URLs and the memory engine"
            onClick={() => openSettings("engine")}
          >
            <Settings2 />
          </Button>
        </div>

        {mode === "chat" && engine.keyMissing && (
          <div
            role="status"
            className="flex items-center gap-2 border-b bg-muted/40 px-4 py-2 text-[13px] text-muted-foreground"
          >
            <KeyRound className="size-3.5 shrink-0" aria-hidden="true" />
            <span className="min-w-0 flex-1">
              No {engine.providerLabel} API key saved, so Chat can't send yet. Add one in Settings,
              or pick a provider that needs none.
            </span>
            <Button size="xs" variant="outline" onClick={() => openSettings("engine")}>
              Add an API key
            </Button>
          </div>
        )}

        <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
          {askSetupOnly ? (
            // Nothing to scroll to yet: the card reads from its top, where the scroller would
            // pin a short window to its end and hide the heading.
            <div className="flex min-h-0 flex-1 overflow-y-auto">
              <div className="m-auto w-full max-w-3xl px-5 py-6">
                <MemorySetupCard whenDismissed="button" checklistLink className="w-full" />
              </div>
            </div>
          ) : (
            <MessageScrollerProvider autoScroll>
              <MessageScroller>
                <MessageScrollerViewport>
                  <MessageScrollerContent
                    className="mx-auto w-full min-w-0 max-w-3xl px-5 py-6"
                    role="log"
                    aria-live="polite"
                    // Announce only committed messages. The live reply changes every frame,
                    // so "text" changes (and an un-busy region) would read out each token.
                    aria-relevant="additions"
                    aria-busy={isStreaming}
                  >
                    {messages.length === 0 && !isStreaming && mode === "chat" && (
                      <Marker variant="separator" className="my-auto">
                        <MarkerContent>
                          Ask anything. Replies are saved and searchable.
                        </MarkerContent>
                      </Marker>
                    )}
                    {askEmpty && !askBlocked && <AskIntro onPick={(q) => send(q)} />}
                    {messages.map((m) => (
                      <MessageScrollerItem
                        key={m.id}
                        messageId={m.id}
                        scrollAnchor={m.role === "user"}
                      >
                        <MessageRow
                          id={m.id}
                          role={m.role}
                          content={m.content}
                          context={m.context}
                          ask={m.ask}
                          // Only context chips use this; keeping it false for other rows
                          // means a stream starting/ending doesn't re-render every bubble.
                          removable={!isStreaming && isContextMessage(m)}
                        />
                      </MessageScrollerItem>
                    ))}
                    {isStreaming && (
                      // The live reply must never be paint-deferred. The base MessageScrollerItem
                      // sets content-visibility:auto (+ contain-intrinsic-size), which lets the
                      // browser skip rendering the growing reply while it sits a hair below the
                      // fold, and report a stale placeholder size, so the scroller's
                      // ResizeObserver never sees the growth, autoScroll doesn't follow, and
                      // streamed text "pops in" in chunks when it finally scrolls into view.
                      // Force it visible so tokens paint as they arrive and the scroller stays
                      // pinned to the live edge.
                      <MessageScrollerItem
                        messageId="streaming"
                        className="[content-visibility:visible]"
                      >
                        <Message align="start">
                          <MessageContent>
                            {pending === "ask" ? (
                              <TypingDots label="Searching your history" />
                            ) : (
                              <StreamingArea />
                            )}
                          </MessageContent>
                        </Message>
                      </MessageScrollerItem>
                    )}
                    {/* Ask runs on the memory engine; while memory is off, its one-click setup
                        sits right above the composer, as on Projects. In the log rather than the
                        form, so a short window scrolls it instead of squeezing the log away. */}
                    {askBlocked && !isStreaming && (
                      <MemorySetupCard
                        whenDismissed="button"
                        checklistLink
                        className="mt-4 w-full"
                      />
                    )}
                    {mode === "ask" && memory.status === "error" && !isStreaming && (
                      <Marker role="alert" className="text-destructive">
                        <MarkerIcon>
                          <TriangleAlert />
                        </MarkerIcon>
                        <MarkerContent>
                          Couldn't check whether memory is on, which Ask needs.{" "}
                          <Button
                            size="xs"
                            variant="outline"
                            className="ml-1 align-baseline"
                            onClick={memory.retry}
                          >
                            Try again
                          </Button>
                        </MarkerContent>
                      </Marker>
                    )}
                    {error && (
                      <Marker role="alert" className="text-destructive">
                        <MarkerIcon>
                          <TriangleAlert />
                        </MarkerIcon>
                        <MarkerContent>{error}</MarkerContent>
                      </Marker>
                    )}
                  </MessageScrollerContent>
                </MessageScrollerViewport>
                <MessageScrollerButton direction="end" />
              </MessageScroller>
            </MessageScrollerProvider>
          )}
          {loadingThread && (
            <div className="absolute inset-0 z-10 flex items-center justify-center bg-background/60 backdrop-blur-[1px]">
              <span className="flex items-center gap-2 text-sm text-muted-foreground">
                <Loader2 className="size-4 animate-spin" /> Loading conversation…
              </span>
            </div>
          )}
        </div>

        <form
          className="mx-auto w-full min-w-0 max-w-3xl px-4 py-3"
          onSubmit={(e) => {
            e.preventDefault();
            send();
          }}
        >
          <div className="relative flex items-end rounded-2xl border border-input bg-card shadow-sm transition-all focus-within:border-ring focus-within:shadow-md focus-within:ring-4 focus-within:ring-ring/15">
            <Textarea
              // The composer is the view's only job, so opening Chat lands in it.
              autoFocus
              aria-label={mode === "ask" ? "Question for your history" : "Message"}
              className="max-h-48 min-h-13 flex-1 resize-none border-0 bg-transparent py-3.5 pl-4 pr-14 shadow-none focus-visible:border-0 focus-visible:ring-0 dark:bg-transparent"
              rows={1}
              value={draft}
              placeholder={
                mode === "ask"
                  ? "Ask your history, e.g. how did we set up release versioning?"
                  : "Message…"
              }
              onChange={(e) => setDraft(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (isSubmitEnter(e)) {
                  e.preventDefault();
                  send();
                }
              }}
            />
            {isStreaming ? (
              <Button
                type="button"
                size="icon"
                aria-label={pending === "ask" ? "Stop waiting for the answer" : "Stop generating"}
                onClick={stop}
                className="absolute bottom-2 right-2 size-9 rounded-full transition-transform duration-150 hover:scale-105 active:scale-95"
              >
                <Square className="size-3.5 fill-current" />
              </Button>
            ) : (
              <Button
                type="submit"
                size="icon"
                aria-label={mode === "ask" ? "Ask" : "Send message"}
                disabled={!draft.trim() || askBlocked}
                className="absolute bottom-2 right-2 size-9 rounded-full transition-transform duration-150 enabled:hover:scale-105 enabled:active:scale-95 disabled:opacity-40"
              >
                <ArrowUp className="size-4" />
              </Button>
            )}
          </div>
          <p className="mt-1.5 flex items-center justify-center gap-1 text-[0.7rem] text-muted-foreground">
            {mode === "ask" && <span>Follow-ups continue as a chat ·</span>}
            <Kbd>Enter</Kbd> to send ·{" "}
            <KbdGroup>
              <Kbd>Shift</Kbd>
              <Kbd>Enter</Kbd>
            </KbdGroup>{" "}
            for newline
          </p>
        </form>
      </div>
    </div>
  );
}

/** The empty Ask conversation: what Ask does and a few questions to start from. */
function AskIntro({ onPick }: { onPick: (q: string) => void }) {
  return (
    <div className="my-auto space-y-3">
      <div className="space-y-1">
        <h2 className="text-base font-semibold tracking-tight">Ask your history</h2>
        <p className="max-w-[60ch] text-sm text-muted-foreground">
          Callimachus finds the past sessions that bear on your question and answers from them,
          citing each thread it used. The answer is saved with your chats.
        </p>
      </div>
      <ul className="flex flex-col items-start gap-1.5" aria-label="Example questions">
        {ASK_EXAMPLES.map((ex) => (
          <li key={ex}>
            <Button size="sm" variant="outline" onClick={() => onPick(ex)}>
              {ex}
            </Button>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** Rough token count for a text blob (~4 chars per token for English and code). */
function approxTokens(chars: number): string {
  const t = Math.round(chars / 4);
  return t < 1000 ? `${t}` : `${(t / 1000).toFixed(1)}k`;
}

/** A committed message rendered with the shadcn conversation primitives: user = muted bubble
 *  (right), or a context Attachment; assistant = ghost bubble wrapping markdown; system = marker.
 *  Memoized so streaming a new reply never re-parses prior messages' markdown. */
const MessageRow = memo(function MessageRow({
  id,
  role,
  content,
  context,
  ask,
  removable,
}: {
  id: string;
  role: string;
  content: string;
  context?: ContextMeta;
  ask?: AskMeta;
  removable: boolean;
}) {
  if (role === "system") {
    return (
      <Marker variant="separator">
        <MarkerContent>{content}</MarkerContent>
      </Marker>
    );
  }
  if (role === "user") {
    if (context || content.startsWith(CONTEXT_PREFIX)) {
      return <ContextChip id={id} content={content} context={context} removable={removable} />;
    }
    return (
      <Message align="end">
        <MessageContent>
          <Bubble variant="muted">
            <BubbleContent className="whitespace-pre-wrap">{content}</BubbleContent>
          </Bubble>
        </MessageContent>
      </Message>
    );
  }
  return (
    <Message align="start">
      <MessageContent>
        <Bubble variant="ghost">
          <BubbleContent>
            {/* Chat replies cite [thread N] too (the history tool's prompt asks for it), so
                every answer gets the chips; an Ask answer also lists its sources. */}
            <AssistantAnswer content={content} ask={ask} />
          </BubbleContent>
        </Bubble>
      </MessageContent>
    </Message>
  );
});

/** An assistant turn. An Ask answer carries a sources footer (the text it is saved and
 *  re-sent as, so a follow-up knows the cited titles); it shows as the source list, with
 *  the live answer's full sources when this window has them, else the footer's. */
function AssistantAnswer({ content, ask }: { content: string; ask?: AskMeta }) {
  const saved = splitSavedSources(content);
  if (ask) return <AskAnswer answer={saved.body} sources={ask.sources} />;
  if (saved.sources.length > 0) return <AskAnswer answer={saved.body} sources={saved.sources} />;
  return <CitedMarkdown text={content} />;
}

/** An injected thread context, shown as a compact attachment with its size. Attached
 *  context is re-sent with every later message, so the token estimate is the per-turn
 *  cost the user is opting into; removing it stops that. */
function ContextChip({
  id,
  content,
  context,
  removable,
}: {
  id: string;
  content: string;
  context?: ContextMeta;
  removable: boolean;
}) {
  const removeMessage = useChat((s) => s.removeMessage);
  const body = content.slice(CONTEXT_PREFIX.length).trim();
  const chars = context?.chars ?? body.length;
  const title = context?.title ?? contextTitle(body);
  return (
    <Message align="end">
      <MessageContent>
        <Attachment size="sm" title="Attached context is sent with every message in this chat.">
          <AttachmentMedia>
            <Paperclip />
          </AttachmentMedia>
          <AttachmentContent>
            <AttachmentTitle>{title ? `Context: ${title}` : "Context"}</AttachmentTitle>
            <AttachmentDescription>
              {chars.toLocaleString()} chars · ~{approxTokens(chars)} tokens per message
              {context?.truncated ? " · trimmed to fit" : ""}
            </AttachmentDescription>
          </AttachmentContent>
          {removable && (
            <AttachmentActions>
              <AttachmentAction
                aria-label="Remove attached context"
                onClick={() => removeMessage(id)}
              >
                <X />
              </AttachmentAction>
            </AttachmentActions>
          )}
        </Attachment>
      </MessageContent>
    </Message>
  );
}

/** Subscribes to the live stream state itself, so per-frame token updates re-render
 *  only this subtree, not the composer, header, or committed message list. */
function StreamingArea() {
  const reasoning = useChat((s) => s.reasoning);
  const parts = useChat((s) => s.parts);
  return <StreamingTurn reasoning={reasoning} parts={parts} />;
}

/** The live assistant turn: reasoning, then text + tool steps in stream order,
 *  with a loading indicator trailing the most recent text. */
function StreamingTurn({ reasoning, parts }: { reasoning: string; parts: StreamPart[] }) {
  const empty = !reasoning && parts.length === 0;
  return (
    <div className="w-full min-w-0 animate-in fade-in slide-in-from-bottom-1 duration-300">
      {reasoning ? <ThinkingBlock text={reasoning} streaming={parts.length === 0} /> : null}
      {parts.map((p, i) =>
        p.kind === "text" ? (
          <Bubble
            // biome-ignore lint/suspicious/noArrayIndexKey: parts only ever append (the last text part grows in place), so a part's index is its stable identity
            key={i}
            variant="ghost"
          >
            <BubbleContent className="relative">
              {/* Only the last part can still grow: an earlier one is followed by a tool
                  step, so its blocks are final and get language detection. */}
              <StreamingMarkdown streaming={i === parts.length - 1}>{p.text}</StreamingMarkdown>
              {i === parts.length - 1 && <TrailingDots />}
            </BubbleContent>
          </Bubble>
        ) : (
          <ToolStepView
            // biome-ignore lint/suspicious/noArrayIndexKey: parts only ever append, so a part's index is its stable identity
            key={i}
            step={p.step}
          />
        ),
      )}
      {empty && <TypingDots />}
    </div>
  );
}

function ToolIcon({ name }: { name: string }) {
  if (name === "run_shell") return <Terminal className="size-3.5 shrink-0" />;
  return <Search className="size-3.5 shrink-0" />;
}

/** One live tool step: read-only tools auto-run; run_shell asks for approval.
 *  Memoized: the parts array replaces a step only via mapTool, so unchanged steps
 *  keep referential identity and skip re-render. */
const ToolStepView = memo(function ToolStepView({ step: s }: { step: ToolStep }) {
  const setStepStatus = useChat((st) => st.setStepStatus);
  return (
    <div className="my-3 min-w-0 rounded-lg border bg-muted/30 px-3 py-2 text-[0.82rem]">
      <div className="flex min-w-0 items-center gap-1.5 font-medium text-muted-foreground">
        <ToolIcon name={s.name} />
        <span className="shrink-0">{s.name}</span>
        {s.status === "running" && <Loader2 className="size-3 shrink-0 animate-spin" />}
        <span className="min-w-0 flex-1 truncate font-normal opacity-80">{s.arg}</span>
      </div>

      {s.status === "awaiting" && (
        <div className="mt-2 min-w-0">
          <pre className="overflow-x-auto rounded bg-background/60 p-2 text-xs">{s.arg}</pre>
          <div className="mt-2 flex gap-2">
            <Button
              size="xs"
              onClick={() => {
                setStepStatus(s.id, "running");
                api.approveTool(s.id, true);
              }}
            >
              <Check className="size-3.5" /> Approve &amp; run
            </Button>
            <Button
              size="xs"
              variant="outline"
              onClick={() => {
                setStepStatus(s.id, "denied");
                api.approveTool(s.id, false);
              }}
            >
              <X className="size-3.5" /> Deny
            </Button>
          </div>
        </div>
      )}

      {s.status === "denied" && <div className="mt-1 text-xs text-muted-foreground">Denied.</div>}

      {s.output && s.status === "done" && (
        <details className="mt-2 min-w-0">
          <summary className="cursor-pointer select-none text-xs text-muted-foreground">
            output
          </summary>
          <pre className="mt-1 max-h-72 overflow-auto whitespace-pre-wrap wrap-break-word rounded bg-background/60 p-2 text-xs">
            {s.output}
          </pre>
        </details>
      )}
    </div>
  );
});

/** Collapsible "thinking" panel showing the model's reasoning stream, ChatGPT-style:
 *  a shimmering label while live, collapsed-friendly once the answer starts. */
function ThinkingBlock({ text, streaming }: { text: string; streaming?: boolean }) {
  return (
    <details
      open
      className="mb-3 rounded-lg border border-dashed bg-muted/40 px-3 py-2 text-[0.82rem] text-muted-foreground"
    >
      <summary className="flex cursor-pointer select-none items-center gap-1.5 font-medium">
        <Brain className="size-3.5" />
        <span className={cn(streaming && "animate-pulse")}>
          {streaming ? "Thinking…" : "Thought process"}
        </span>
      </summary>
      <div className="mt-2 whitespace-pre-wrap wrap-break-word leading-relaxed opacity-90">
        {text}
      </div>
    </details>
  );
}

/** ChatGPT/Claude-style loading state shown the instant you hit Enter, until the
 *  first token arrives (or, for Ask, the answer): a shimmering label with bouncing dots. */
function TypingDots({ label = "Thinking" }: { label?: string }) {
  return (
    // Screen readers get the sr-only sentence; the icon, the short label and the dots are
    // decorative.
    <div className="flex items-center gap-2 text-sm text-muted-foreground">
      <span className="sr-only">{label === "Thinking" ? "Assistant is thinking" : label}</span>
      <Brain className="size-4 animate-pulse" aria-hidden="true" />
      <span className="animate-pulse font-medium" aria-hidden="true">
        {label}
      </span>
      <span className="flex items-center gap-1" aria-hidden="true">
        {[0, 160, 320].map((delay) => (
          <span
            key={delay}
            className="size-1.5 animate-bounce rounded-full bg-muted-foreground/70"
            style={{ animationDelay: `${delay}ms` }}
          />
        ))}
      </span>
    </div>
  );
}

/** Compact bouncing dots shown trailing the most recent streamed text. */
function TrailingDots() {
  return (
    <span className="ml-1 inline-flex translate-y-0.5 items-center gap-1 align-text-bottom">
      <span className="sr-only">Generating</span>
      {[0, 160, 320].map((delay) => (
        <span
          key={delay}
          aria-hidden="true"
          className="size-1.5 animate-bounce rounded-full bg-muted-foreground/70"
          style={{ animationDelay: `${delay}ms` }}
        />
      ))}
    </span>
  );
}
