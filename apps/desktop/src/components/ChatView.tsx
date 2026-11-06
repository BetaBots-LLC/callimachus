import { memo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, PROVIDERS } from "../lib/api";
import {
  CONTEXT_PREFIX,
  type ContextMeta,
  contextTitle,
  isContextMessage,
  useChat,
  type StreamPart,
  type ToolStep,
} from "../store/chat";
import { describeKeySaveError } from "../lib/errors";
import { track } from "../lib/telemetry";
import { isSubmitEnter } from "../lib/keyboard";
import { loadModelCache, saveModelCache, MODELS_TTL } from "../lib/models";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Kbd, KbdGroup } from "@/components/ui/kbd";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import {
  ArrowUp,
  Brain,
  Check,
  Loader2,
  Paperclip,
  Search,
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
import { ChatSidebar } from "./ChatSidebar";
import { Markdown, StreamingMarkdown } from "./Markdown";

export function ChatView() {
  const provider = useChat((s) => s.provider);
  const model = useChat((s) => s.model);
  const messages = useChat((s) => s.messages);
  const loadingThread = useChat((s) => s.loadingThread);
  const error = useChat((s) => s.error);
  // Streaming state lives in the store, so it survives this view unmounting when the
  // user switches tabs mid-reply (the reply, its tool steps and Stop are still here).
  const isStreaming = useChat((s) => s.isStreaming);
  const stop = useChat((s) => s.stop);

  const setProvider = useChat((s) => s.setProvider);
  const setModel = useChat((s) => s.setModel);

  const [draft, setDraft] = useState("");
  const [keyDraft, setKeyDraft] = useState("");
  const queryClient = useQueryClient();

  // Local Ollama and Ollama Cloud take a custom endpoint, persisted per provider and shared with
  // Settings. react-query is the source of truth (no store mirror): the saved URL feeds the model
  // list and send directly; edits persist on blur and re-key the model list via invalidation.
  const isOllamaLike = provider === "ollama" || provider === "ollama_cloud";
  const savedBaseUrl = useQuery({
    queryKey: ["providerBaseUrl", provider],
    queryFn: () => api.providerBaseUrl(provider),
    enabled: isOllamaLike,
  });
  const baseUrl = savedBaseUrl.data ?? "";
  // `null` = not editing → show the saved value (updates live as the query resolves or the
  // provider switches). While editing, this local draft overrides it, so an in-flight query
  // result can never clobber what the user is typing.
  const [urlEdit, setUrlEdit] = useState<string | null>(null);
  const saveBaseUrl = useMutation({
    mutationFn: (url: string) => api.setProviderBaseUrl(provider, url),
    // Update the cache immediately so the box + model list reflect the new URL without waiting
    // on the round-trip; the invalidate then reconciles with what the backend actually stored.
    onMutate: (url: string) => queryClient.setQueryData(["providerBaseUrl", provider], url),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["providerBaseUrl", provider] });
      track("setting_changed", { key: "provider_base_url" });
    },
  });

  const hasKey = useQuery({
    queryKey: ["hasKey", provider],
    queryFn: () => api.providerHasKey(provider),
    // Keyless engines (Ollama, CLI) never need a key, so skip the probe so the keychain stays quiet.
    enabled: provider !== "ollama" && !provider.endsWith("-cli"),
  });
  const saveKey = useMutation({
    mutationFn: () => api.setApiKey(provider, keyDraft.trim()),
    onSuccess: () => {
      setKeyDraft("");
      queryClient.invalidateQueries({ queryKey: ["hasKey", provider] });
      track("setting_changed", { key: "api_key" });
    },
  });

  // CLI backends (claude-cli / codex-cli) use their own logged-in auth: no API key.
  const isCli = provider.endsWith("-cli");
  const needsKey = provider !== "ollama" && !isCli;

  // Offer installed CLI backends alongside the keyed providers (CLI has no default model; the
  // CLI picks one when model is empty).
  const cliEngines = useQuery({ queryKey: ["cli_engines"], queryFn: api.cliEngines });
  const providerOptions = [
    ...PROVIDERS.map((p) => ({ id: p.id, label: p.label, defaultModel: p.defaultModel })),
    ...(cliEngines.data ?? [])
      .filter((e) => e.installed)
      .map((e) => ({ id: e.id, label: `${e.label} (no key)`, defaultModel: "" })),
  ];

  // Live model list from the provider API, cached in localStorage so the dropdown
  // is instant and self-refreshes (TTL / version), so banned models drop off.
  // Only fetch once we can (key present, or providers that don't need one).
  const canList =
    provider === "ollama" || provider === "openrouter" || isCli || hasKey.data === true;
  const modelsQuery = useQuery({
    queryKey: ["models", provider, baseUrl],
    enabled: canList,
    staleTime: MODELS_TTL,
    initialData: () => loadModelCache(provider)?.models,
    initialDataUpdatedAt: () => loadModelCache(provider)?.at,
    queryFn: async () => {
      const list = await api.listModels(provider, baseUrl || undefined);
      saveModelCache(provider, list);
      // If the selected model is gone (e.g. retired/banned), fall back to a valid one
      // (prefer the provider's default if the API still offers it).
      const cur = useChat.getState();
      if (cur.provider === provider && list.length && !list.includes(cur.model)) {
        const def = PROVIDERS.find((p) => p.id === provider)?.defaultModel;
        cur.setModel(def && list.includes(def) ? def : list[0]);
      }
      return list;
    },
  });
  const staticModels: string[] = [...(PROVIDERS.find((p) => p.id === provider)?.models ?? [])];
  const modelOptions: string[] = modelsQuery.data?.length ? modelsQuery.data : staticModels;
  // Always include the current model so the (strict) dropdown can display it even
  // before the live list loads or if it isn't in the provider's list.
  const modelList =
    model && !modelOptions.includes(model) ? [model, ...modelOptions] : modelOptions;

  function send() {
    const text = draft.trim();
    if (!text || useChat.getState().isStreaming) return;
    setDraft("");
    void useChat.getState().send(text, { baseUrl: baseUrl || null });
  }

  return (
    <div className="flex min-h-0 flex-1">
      <ChatSidebar />
      <div className="flex min-h-0 flex-1 flex-col">
        <div className="flex items-center gap-2 border-b px-4 py-2.5">
          <Select
            value={provider}
            onValueChange={(v) => {
              const p = providerOptions.find((x) => x.id === v);
              if (p) setProvider(p.id, p.defaultModel);
            }}
          >
            <SelectTrigger className="w-40">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {providerOptions.map((p) => (
                <SelectItem key={p.id} value={p.id}>
                  {p.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select value={model} onValueChange={(v) => v && setModel(v)}>
            <SelectTrigger className="flex-1" aria-label="Model">
              <SelectValue placeholder={modelsQuery.isFetching ? "loading models…" : "model"} />
            </SelectTrigger>
            <SelectContent>
              {modelList.map((m) => (
                <SelectItem key={m} value={m}>
                  {m}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {isOllamaLike && (
            <Input
              // Controlled: show the saved URL, but a local draft overrides it while editing so an
              // async query result never clobbers in-progress typing. Persist on blur, which fires
              // before a provider switch, so it always writes under the correct provider.
              value={urlEdit ?? baseUrl}
              onChange={(e) => setUrlEdit(e.currentTarget.value)}
              onBlur={(e) => {
                const url = e.currentTarget.value.trim();
                setUrlEdit(null);
                if (url !== baseUrl) saveBaseUrl.mutate(url);
              }}
              placeholder={
                provider === "ollama_cloud" ? "https://ollama.com" : "http://localhost:11434"
              }
            />
          )}
        </div>

        {needsKey && !hasKey.data && (
          <div className="flex items-center gap-2 border-b px-4 py-2.5">
            <Input
              type="password"
              value={keyDraft}
              onChange={(e) => setKeyDraft(e.currentTarget.value)}
              placeholder={`${provider} API key (stored in your OS keychain)`}
            />
            <Button
              size="sm"
              onClick={() => saveKey.mutate()}
              disabled={!keyDraft.trim() || saveKey.isPending}
            >
              Save key
            </Button>
          </div>
        )}
        {needsKey && !hasKey.data && saveKey.isError && (
          <p role="alert" className="border-b px-4 py-2 text-xs text-destructive">
            {describeKeySaveError(saveKey.error)}
          </p>
        )}

        <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
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
                  {messages.length === 0 && !isStreaming && (
                    <Marker variant="separator" className="my-auto">
                      <MarkerContent>Ask anything. Replies are saved and searchable.</MarkerContent>
                    </Marker>
                  )}
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
                          <StreamingArea />
                        </MessageContent>
                      </Message>
                    </MessageScrollerItem>
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
              className="max-h-48 min-h-13 flex-1 resize-none border-0 bg-transparent py-3.5 pl-4 pr-14 shadow-none focus-visible:border-0 focus-visible:ring-0 dark:bg-transparent"
              rows={1}
              value={draft}
              placeholder="Message…"
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
                aria-label="Stop generating"
                onClick={stop}
                className="absolute bottom-2 right-2 size-9 rounded-full transition-transform duration-150 hover:scale-105 active:scale-95"
              >
                <Square className="size-3.5 fill-current" />
              </Button>
            ) : (
              <Button
                type="submit"
                size="icon"
                aria-label="Send message"
                disabled={!draft.trim()}
                className="absolute bottom-2 right-2 size-9 rounded-full transition-transform duration-150 enabled:hover:scale-105 enabled:active:scale-95 disabled:opacity-40"
              >
                <ArrowUp className="size-4" />
              </Button>
            )}
          </div>
          <p className="mt-1.5 flex items-center justify-center gap-1 text-[0.7rem] text-muted-foreground">
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
  removable,
}: {
  id: string;
  role: string;
  content: string;
  context?: ContextMeta;
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
            <Markdown>{content}</Markdown>
          </BubbleContent>
        </Bubble>
      </MessageContent>
    </Message>
  );
});

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
 *  first token arrives: a shimmering "Thinking" label with bouncing dots. */
function TypingDots() {
  return (
    // Screen readers get the sr-only sentence; the icon, the short label and the dots are
    // decorative.
    <div className="flex items-center gap-2 text-sm text-muted-foreground">
      <span className="sr-only">Assistant is thinking</span>
      <Brain className="size-4 animate-pulse" aria-hidden="true" />
      <span className="animate-pulse font-medium" aria-hidden="true">
        Thinking
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
