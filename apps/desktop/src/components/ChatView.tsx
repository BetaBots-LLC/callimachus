import { memo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type ChatChunk, PROVIDERS } from "../lib/api";
import { useChat, type StreamPart, type ToolStep } from "../store/chat";
import { humanizeApiError } from "../lib/errors";
import { loadModelCache, saveModelCache, MODELS_TTL } from "../lib/models";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import { ArrowUp, Brain, Check, Loader2, Search, Square, Terminal, X } from "lucide-react";
import { StickToBottom } from "use-stick-to-bottom";
import { ChatSidebar } from "./ChatSidebar";
import { Markdown, StreamingMarkdown } from "./Markdown";

export function ChatView() {
  const provider = useChat((s) => s.provider);
  const model = useChat((s) => s.model);
  const baseUrl = useChat((s) => s.baseUrl);
  const messages = useChat((s) => s.messages);
  const loadingThread = useChat((s) => s.loadingThread);
  const error = useChat((s) => s.error);

  const setProvider = useChat((s) => s.setProvider);
  const setModel = useChat((s) => s.setModel);
  const setBaseUrl = useChat((s) => s.setBaseUrl);
  const newChat = useChat((s) => s.newChat);

  const [draft, setDraft] = useState("");
  const [keyDraft, setKeyDraft] = useState("");
  // Local "busy" flag — drives the loader/Stop/streaming bubble directly so the UI
  // updates the instant we send, independent of the store flag's render timing.
  const [sending, setSending] = useState(false);
  const queryClient = useQueryClient();

  const hasKey = useQuery({
    queryKey: ["hasKey", provider],
    queryFn: () => api.providerHasKey(provider),
    // Keyless engines (Ollama, CLI) never need a key — skip the probe so the keychain stays quiet.
    enabled: provider !== "ollama" && !provider.endsWith("-cli"),
  });
  const saveKey = useMutation({
    mutationFn: () => api.setApiKey(provider, keyDraft.trim()),
    onSuccess: () => {
      setKeyDraft("");
      queryClient.invalidateQueries({ queryKey: ["hasKey", provider] });
    },
  });

  // CLI backends (claude-cli / codex-cli) use their own logged-in auth — no API key.
  const isCli = provider.endsWith("-cli");
  const needsKey = provider !== "ollama" && !isCli;

  // Offer installed CLI backends alongside the keyed providers (CLI has no default model — the
  // CLI picks one when model is empty).
  const cliEngines = useQuery({ queryKey: ["cli_engines"], queryFn: api.cliEngines });
  const providerOptions = [
    ...PROVIDERS.map((p) => ({ id: p.id, label: p.label, defaultModel: p.defaultModel })),
    ...(cliEngines.data ?? [])
      .filter((e) => e.installed)
      .map((e) => ({ id: e.id, label: `${e.label} (no key)`, defaultModel: "" })),
  ];

  // Live model list from the provider API, cached in localStorage so the dropdown
  // is instant and self-refreshes (TTL / version) — banned models drop off.
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

  async function send() {
    const text = draft.trim();
    const chat = useChat.getState();
    if (!text || sending) return;
    const threadId = chat.threadId; // guard against thread switches mid-stream
    chat.pushUser(text);
    setDraft("");
    setSending(true);
    chat.setStreaming(true);
    chat.setError(null);

    // Coalesce streamed chunks into one store update per animation frame so we cap
    // React renders at the display refresh regardless of token rate.
    let textBuf = "";
    let reasonBuf = "";
    let raf = 0;
    const flush = () => {
      raf = 0;
      const st = useChat.getState();
      if (st.threadId !== threadId) return; // stale generation
      if (reasonBuf) {
        st.appendChunk({ kind: "reasoning", text: reasonBuf });
        reasonBuf = "";
      }
      if (textBuf) {
        st.appendChunk({ kind: "text", text: textBuf });
        textBuf = "";
      }
    };
    const schedule = () => {
      if (!raf) raf = requestAnimationFrame(flush);
    };
    const onChunk = (chunk: ChatChunk) => {
      if (chunk.kind === "text") {
        textBuf += chunk.text;
        schedule();
      } else if (chunk.kind === "reasoning") {
        reasonBuf += chunk.text;
        schedule();
      } else {
        // tool_*: preserve interleave order — drain buffered text first, then apply.
        if (raf) cancelAnimationFrame(raf);
        flush();
        if (useChat.getState().threadId === threadId) useChat.getState().appendChunk(chunk);
      }
    };

    try {
      const full = await api.sendChat(
        {
          threadId,
          provider,
          model,
          baseUrl: baseUrl || null,
          messages: useChat.getState().messages,
        },
        onChunk,
      );
      if (raf) cancelAnimationFrame(raf);
      flush();
      if (useChat.getState().threadId === threadId) {
        useChat.getState().finishAssistant(full);
        queryClient.invalidateQueries({ queryKey: ["db_stats"] });
        queryClient.invalidateQueries({ queryKey: ["chats"] });
      }
    } catch (e) {
      if (raf) cancelAnimationFrame(raf);
      if (useChat.getState().threadId === threadId) {
        useChat.getState().setError(humanizeApiError(String(e), provider, model));
      }
    } finally {
      setSending(false);
    }
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
          {provider === "ollama" && (
            <Input
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.currentTarget.value)}
              placeholder="http://localhost:11434"
            />
          )}
          <Button size="sm" variant="outline" onClick={newChat}>
            New chat
          </Button>
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

        <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
          <StickToBottom
            className="min-h-0 min-w-0 flex-1 overflow-y-auto overflow-x-hidden"
            resize="smooth"
            initial="smooth"
          >
            <StickToBottom.Content
              className="mx-auto flex w-full min-w-0 max-w-3xl flex-col gap-6 px-5 py-6"
              role="log"
              aria-live="polite"
              aria-relevant="additions text"
            >
              {messages.length === 0 && !sending && (
                <div className="m-auto text-muted-foreground">
                  Ask anything. Replies are saved and searchable.
                </div>
              )}
              {messages.map((m) => (
                <div
                  key={m.id}
                  className="[contain-intrinsic-size:0_80px] [content-visibility:auto]"
                >
                  <Bubble role={m.role} content={m.content} />
                </div>
              ))}
              {sending && <StreamingArea />}
              {error && (
                <div
                  role="alert"
                  className="animate-in fade-in slide-in-from-bottom-2 rounded-lg border border-l-2 border-l-destructive bg-destructive/5 px-3 py-2 text-sm text-destructive"
                >
                  {error}
                </div>
              )}
            </StickToBottom.Content>
          </StickToBottom>
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
              className="max-h-48 min-h-13 flex-1 resize-none border-0 bg-transparent py-3.5 pl-4 pr-14 shadow-none focus-visible:border-0 focus-visible:ring-0 dark:bg-transparent"
              rows={1}
              value={draft}
              placeholder="Message…"
              onChange={(e) => setDraft(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  send();
                }
              }}
            />
            {sending ? (
              <Button
                type="button"
                size="icon"
                aria-label="Stop generating"
                onClick={() => api.cancelChat()}
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
          <p className="mt-1.5 text-center text-[0.7rem] text-muted-foreground">
            <kbd className="font-sans">Enter</kbd> to send · <kbd className="font-sans">Shift</kbd>+
            <kbd className="font-sans">Enter</kbd> for newline
          </p>
        </form>
      </div>
    </div>
  );
}

/** A committed message: user = grey bubble (right); assistant = borderless markdown.
 *  Memoized so streaming a new reply never re-parses prior messages' markdown. */
const Bubble = memo(function Bubble({ role, content }: { role: string; content: string }) {
  if (role === "user") {
    return (
      <div className="flex justify-end">
        <div className="min-w-0 max-w-[85%] whitespace-pre-wrap wrap-break-word rounded-2xl bg-muted px-4 py-2.5 text-[0.95rem] leading-relaxed">
          {content}
        </div>
      </div>
    );
  }
  return (
    <div className="w-full min-w-0">
      <Markdown>{content}</Markdown>
    </div>
  );
});

/** Subscribes to the live stream state itself, so per-frame token updates re-render
 *  only this subtree — not the composer, header, or committed message list. */
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
          <div key={i} className="relative min-w-0">
            <StreamingMarkdown>{p.text}</StreamingMarkdown>
            {i === parts.length - 1 && <TrailingDots />}
          </div>
        ) : (
          <ToolStepView key={i} step={p.step} />
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
 *  Memoized — the parts array replaces a step only via mapTool, so unchanged steps
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
    <div
      className="flex items-center gap-2 text-sm text-muted-foreground"
      aria-label="Assistant is thinking"
    >
      <Brain className="size-4 animate-pulse" />
      <span className="animate-pulse font-medium">Thinking</span>
      <span className="flex items-center gap-1">
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
    <span
      className="ml-1 inline-flex translate-y-0.5 items-center gap-1 align-text-bottom"
      aria-label="Generating"
    >
      {[0, 160, 320].map((delay) => (
        <span
          key={delay}
          className="size-1.5 animate-bounce rounded-full bg-muted-foreground/70"
          style={{ animationDelay: `${delay}ms` }}
        />
      ))}
    </span>
  );
}
