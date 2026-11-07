import { create } from "zustand";
import { type AskSource, api, type ChatChunk, type ChatMessage } from "../lib/api";
import { humanizeApiError } from "../lib/errors";
import { invalidateIndexDerived, queryClient } from "../lib/queryClient";
import { classifyError, trackError } from "../lib/telemetry";

/** Where an injected context message came from, for the attachment chip. */
export interface ContextMeta {
  threadId: number | null;
  title: string | null;
  chars: number; // length of the injected body
  truncated: boolean; // the body was elided to fit MAX_CONTEXT_CHARS
}

/** An answer that came from "Ask your history" (ask_history): the threads it drew on. Only
 *  the turn answered in this window has them; a saved chat reloads as plain messages, and
 *  its [thread N] citations still render as chips. */
export interface AskMeta {
  sources: AskSource[];
}

// A committed message with a stable id (keeps React subtree identity stable across
// the streaming→committed transition, so memoized bubbles don't remount). `context` and
// `ask` are UI-only metadata and are stripped before the message goes to the backend.
export type StoredMessage = ChatMessage & { id: string; context?: ContextMeta; ask?: AskMeta };

/** How the next message is answered. "chat": the chat provider, with the history-search
 *  tool. "ask": one retrieval-grounded answer from ask_history on the memory engine, with
 *  [thread N] citations; the mode then returns to "chat", so follow-ups are a normal chat
 *  over the same conversation. */
export type ChatMode = "chat" | "ask";

// A tool the agent invoked during a turn (shown live as a step).
export interface ToolStep {
  id: string;
  name: string;
  arg: string; // the announced call (query / command / thread id)
  output?: string; // result once it returns
  status: "running" | "awaiting" | "done" | "denied";
}

// The live assistant turn is an ordered list of parts so text and tool steps
// render in the exact order they streamed in (text → tool → text → …).
export type StreamPart = { kind: "text"; text: string } | { kind: "tool"; step: ToolStep };

/** The prefix `addContext` prepends when injecting context as a user turn. Committed
 *  turns starting with it render as a compact attachment chip instead of a bubble. */
export const CONTEXT_PREFIX = "Here is context for our conversation:";

/**
 * Injected context is re-sent with every later turn (the whole history goes out each
 * time, and only some providers cache it), so it is the main driver of chat token
 * spend. Cap how much can stack up: at most two attached threads, each trimmed to
 * ~16k chars (~4k tokens).
 */
export const MAX_CONTEXTS = 2;
export const MAX_CONTEXT_CHARS = 16_000;

/** Outcome of `addContext`, so the caller can explain a refusal. */
export type AddContextResult = "added" | "duplicate" | "limit" | "busy";

interface ChatState {
  threadId: string;
  provider: string;
  model: string;
  mode: ChatMode;
  // What the in-flight turn is: a streamed chat reply, or an Ask (one request, no stream).
  pending: ChatMode | null;
  messages: StoredMessage[];
  reasoning: string; // in-progress "thinking" text
  parts: StreamPart[]; // in-progress answer, interleaved text + tool steps
  // True from send until the reply settles. Lives here (not in ChatView) so leaving the
  // Chat tab mid-reply and coming back still shows the live reply and the Stop button,
  // and the send guard still blocks a second, concurrent stream.
  isStreaming: boolean;
  loadingThread: boolean; // fetching a thread to open
  error: string | null;

  setProvider: (p: string, defaultModel: string) => void;
  setModel: (m: string) => void;
  setMode: (m: ChatMode) => void;
  newChat: () => void;
  loadChat: (threadId: string, messages: ChatMessage[]) => void;
  pushUser: (content: string) => void;
  addContext: (
    text: string,
    source?: { threadId?: number | null; title?: string | null },
  ) => AddContextResult;
  removeMessage: (id: string) => void;
  appendChunk: (chunk: ChatChunk) => void;
  setStepStatus: (id: string, status: ToolStep["status"]) => void;
  finishAssistant: (full: string) => void;
  setError: (e: string | null) => void;
  setLoadingThread: (v: boolean) => void;
  /** Send a user message: in chat mode stream the reply into `parts`, in ask mode answer
   *  it with ask_history. */
  send: (text: string, opts?: { baseUrl?: string | null }) => Promise<void>;
  /** Stop the in-flight reply. The partial reply is still committed and saved. */
  stop: () => void;
}

// Chat's provider and model are the app's chat default (Settings > AI engine edits the same
// values), so they survive a restart. Storage can be blocked (a locked-down webview, a
// private profile): reads then fall back to the built-in default and writes are dropped.
const CHAT_PROVIDER_KEY = "callimachus.chatProvider";
const CHAT_MODEL_KEY = "callimachus.chatModel";
const DEFAULT_PROVIDER = "anthropic";
const DEFAULT_MODEL = "claude-opus-4-8";
function readStored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function writeStored(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* storage blocked: the choice lasts for this session */
  }
}
const storedProvider = readStored(CHAT_PROVIDER_KEY);
const initialProvider = storedProvider || DEFAULT_PROVIDER;
// A stored model belongs to the stored provider; without one, the built-in pair.
const initialModel = storedProvider ? (readStored(CHAT_MODEL_KEY) ?? "") : DEFAULT_MODEL;

const newId = () =>
  globalThis.crypto?.randomUUID?.() ?? `chat-${Math.random().toString(36).slice(2)}`;

const mapTool = (parts: StreamPart[], id: string | undefined, fn: (s: ToolStep) => ToolStep) =>
  parts.map((p) =>
    p.kind === "tool" && p.step.id === id ? { kind: "tool" as const, step: fn(p.step) } : p,
  );

/** The backend's ChatMessage is exactly {role, content}; drop UI-only fields. */
const toWire = (m: StoredMessage): ChatMessage => ({ role: m.role, content: m.content });

/** Backstop for empty assistant turns (a reply stopped before its first token, from a
 *  saved chat): providers such as Anthropic reject them anywhere but the final position,
 *  so sending one would fail every later message in that chat. */
const isSendable = (m: StoredMessage) => m.role !== "assistant" || m.content.trim() !== "";

/**
 * A failed Ask in words. Ask runs on the memory engine, which may be a CLI login (no key)
 * or "the first saved API key" (no provider named), so Chat's copy ("No  API key set.",
 * "Pick another from the dropdown") named the wrong thing or a control Ask does not have.
 * Every branch points at Settings > AI engine, where that engine is set. A backend message
 * that already names the fix (memory is off) is shown as it is.
 */
export function askErrorText(raw: string): string {
  const s = raw.replace(/^Error:\s*/, "");
  if (s.includes("Settings > AI engine")) return s.split("\n")[0];
  const detail = s.match(/"message"\s*:\s*"([^"]+)"/)?.[1];
  const more = detail ? ` (${detail})` : "";
  switch (classifyError(s)) {
    case "model_unavailable":
      return `The memory engine's model isn't available${more}. Pick another model in Settings > AI engine.`;
    case "auth_rejected":
      return `The memory engine rejected the request${more}. Check its login or API key in Settings > AI engine.`;
    case "missing_key":
      return "The memory engine has no API key. Add one, or pick another engine, in Settings > AI engine.";
    case "rate_limited":
      return `The memory engine is rate-limited or out of quota${more}. Wait a moment, then ask again.`;
    case "network":
      return "Couldn't reach the memory engine. Check the connection (for Ollama, that it is running), then ask again.";
    default:
      return detail || s.split("\n")[0].slice(0, 300) || "Ask failed. Try again.";
  }
}

export const isContextMessage = (m: Pick<StoredMessage, "role" | "content" | "context">) =>
  m.role === "user" && (m.context != null || m.content.startsWith(CONTEXT_PREFIX));

/** The thread title from a packed `<reference_thread title="…">` envelope, if present.
 *  Used for context turns loaded from a saved chat, which carry no metadata. */
export function contextTitle(content: string): string | null {
  return content.match(/<reference_thread[^>]*\btitle="([^"]*)"/)?.[1] ?? null;
}

/**
 * Trim a packed thread to `max` chars by eliding its middle, keeping the head (the
 * envelope and opening turns) and a larger tail (the latest turns and the closing
 * tag). Cuts snap to line breaks so a message isn't split mid-line where avoidable.
 * Chars are code points, the unit the backend packs to (Rust `chars`): UTF-16 `length`
 * counts an emoji as two, so a full 16,000-char pack with one emoji in it measured over
 * budget and lost a middle chunk the backend had already fitted.
 */
export function fitContext(text: string, max: number): { text: string; truncated: boolean } {
  const cps = Array.from(text);
  if (cps.length <= max) return { text, truncated: false };
  const marker = "\n\n[… middle of the thread omitted to fit the chat context budget …]\n\n";
  const room = Math.max(0, max - Array.from(marker).length);
  let headEnd = Math.floor(room * 0.35);
  let tailStart = cps.length - (room - headEnd);
  const nl = cps.lastIndexOf("\n", headEnd);
  if (nl > headEnd * 0.8) headEnd = nl;
  const nl2 = cps.indexOf("\n", tailStart);
  if (nl2 !== -1 && nl2 - tailStart < (room - headEnd) * 0.2) tailStart = nl2 + 1;
  return {
    text: cps.slice(0, headEnd).join("") + marker + cps.slice(tailStart).join(""),
    truncated: true,
  };
}

// --- Stream bookkeeping (module scope: one chat stream per app) ---------------------

// Generation id of the stream whose output the UI shows (0 = none). Bumped per send and
// zeroed when the user abandons a stream (New chat / opening another chat), so late
// chunks and the late result of an abandoned stream are dropped.
let activeStream = 0;
let streamSeq = 0;
// The most recent send_chat IPC call, until it settles. A cancelled stream keeps
// unwinding in the backend for a moment, and its cleanup clears the single cancel slot
// (lib.rs send_chat). Starting the next stream only after it settles keeps the new
// stream stoppable.
let inflight: Promise<unknown> | null = null;
const SETTLE_WAIT_MS = 3_000;
// Set by stop() to the generation it targeted, so a Stop pressed while a new send is
// still waiting for the previous stream to settle cancels that send before it starts.
let stopRequested = 0;

/** Answer "deny" for any run_shell approval the stream is blocked on, so a cancelled
 *  stream can't sit waiting on a prompt nobody can see any more. */
function denyPendingApprovals(parts: StreamPart[]) {
  for (const p of parts) {
    if (p.kind === "tool" && p.step.status === "awaiting") {
      void api.approveTool(p.step.id, false).catch(() => {});
    }
  }
}

export const useChat = create<ChatState>((set, get) => {
  /** Stop showing (and cancel) the current stream before switching conversations. */
  const abandonActiveStream = () => {
    if (!activeStream) return;
    activeStream = 0;
    // An Ask has no stream to cancel (and cancelling would hit a chat stream still
    // unwinding from earlier); dropping its result is enough.
    if (get().pending === "ask") return;
    denyPendingApprovals(get().parts);
    void api.cancelChat().catch(() => {});
  };

  /** One Ask turn: ask_history retrieves past sessions and answers on the memory engine.
   *  The answer is saved into this chat (with any earlier turns) only once it is shown, so
   *  it lists with the saved chats and a follow-up continues the same conversation. An Ask
   *  stopped or abandoned before it lands saves nothing: persist_chat replaces the whole
   *  thread, so a late save would wipe the turns sent after the Stop. */
  const askTurn = async (text: string) => {
    const gen = ++streamSeq;
    activeStream = gen;
    const { threadId } = get();
    set((s) => ({
      messages: [...s.messages, { role: "user", content: text, id: newId() }],
      isStreaming: true,
      pending: "ask",
      error: null,
      reasoning: "",
      parts: [],
    }));
    const isCurrent = () => activeStream === gen && get().threadId === threadId;
    try {
      const res = await api.askHistory(text);
      if (!isCurrent()) return;
      // The text with its sources footer is what is shown, saved and re-sent, so a reopened
      // chat and a follow-up both still know which threads the answer cited.
      const reply = res.answerWithSources ?? res.answer;
      const turns = get().messages.filter(isSendable).map(toWire);
      set((s) => ({
        messages: [
          ...s.messages,
          { role: "assistant", content: reply, id: newId(), ask: { sources: res.sources } },
        ],
        // Follow-ups are a normal chat over this conversation, with the history tool.
        mode: "chat",
      }));
      // The next send waits for this save (as for a settling stream), so its own save of
      // the longer conversation always lands last.
      const save = api.saveChat(threadId, turns, reply);
      inflight = save;
      void save
        .then(
          () => {
            void queryClient.invalidateQueries({ queryKey: ["chats"] });
            void queryClient.invalidateQueries({ queryKey: ["db_stats"] });
            invalidateIndexDerived();
          },
          (e) => {
            if (get().threadId === threadId) {
              set({ error: `Couldn't save this answer to your chats: ${String(e)}` });
            }
          },
        )
        .finally(() => {
          if (inflight === save) inflight = null;
        });
    } catch (e) {
      if (isCurrent()) {
        // Ask runs on the memory engine, not the chat provider: its own copy.
        set({ error: askErrorText(String(e)) });
        trackError(e);
      }
    } finally {
      if (activeStream === gen) {
        activeStream = 0;
        set({ isStreaming: false, pending: null });
      }
    }
  };

  return {
    threadId: newId(),
    provider: initialProvider,
    model: initialModel,
    mode: "chat",
    pending: null,
    messages: [],
    reasoning: "",
    parts: [],
    isStreaming: false,
    loadingThread: false,
    error: null,

    setProvider: (provider, defaultModel) => {
      writeStored(CHAT_PROVIDER_KEY, provider);
      writeStored(CHAT_MODEL_KEY, defaultModel);
      set({ provider, model: defaultModel });
    },
    setModel: (model) => {
      writeStored(CHAT_MODEL_KEY, model);
      set({ model });
    },
    setMode: (mode) => set({ mode }),
    newChat: () => {
      abandonActiveStream();
      set({
        threadId: newId(),
        messages: [],
        reasoning: "",
        parts: [],
        error: null,
        isStreaming: false,
        pending: null,
      });
    },
    loadChat: (threadId, messages) => {
      // Re-opening the conversation that is streaming right now would cancel it.
      if (get().isStreaming && get().threadId === threadId) return;
      abandonActiveStream();
      set({
        threadId,
        messages: messages.map((m) => ({ ...m, id: newId() })),
        reasoning: "",
        parts: [],
        error: null,
        isStreaming: false,
        pending: null,
        // A saved conversation continues as a chat; Ask is picked per question.
        mode: "chat",
      });
    },
    pushUser: (content) =>
      set((s) => ({ messages: [...s.messages, { role: "user", content, id: newId() }] })),
    addContext: (text, source) => {
      const s = get();
      // A context turn appended mid-reply would land between the question and its answer.
      if (s.isStreaming) return "busy";
      const threadId = source?.threadId ?? null;
      const contexts = s.messages.filter(isContextMessage);
      const { text: body, truncated } = fitContext(text.trim(), MAX_CONTEXT_CHARS);
      const content = `${CONTEXT_PREFIX}\n\n${body}`;
      const duplicate = contexts.some(
        (m) => (threadId != null && m.context?.threadId === threadId) || m.content === content,
      );
      // Ask answers from retrieval alone and ignores every earlier turn, so a send in Ask
      // mode would silently drop the context just attached: attaching switches to Chat.
      if (duplicate) {
        set({ mode: "chat" });
        return "duplicate";
      }
      if (contexts.length >= MAX_CONTEXTS) return "limit";
      set({
        mode: "chat",
        messages: [
          ...s.messages,
          {
            role: "user",
            content,
            id: newId(),
            context: {
              threadId,
              title: source?.title ?? contextTitle(body),
              chars: Array.from(body).length,
              truncated,
            },
          },
        ],
      });
      return "added";
    },
    removeMessage: (id) => {
      if (get().isStreaming) return;
      set((s) => ({ messages: s.messages.filter((m) => m.id !== id) }));
    },
    appendChunk: (chunk) =>
      set((s) => {
        switch (chunk.kind) {
          case "reasoning":
            return { reasoning: s.reasoning + chunk.text };
          case "text": {
            // Append to the trailing text part, or start a new one.
            const last = s.parts[s.parts.length - 1];
            if (last && last.kind === "text") {
              const parts = s.parts.slice(0, -1);
              parts.push({ kind: "text", text: last.text + chunk.text });
              return { parts };
            }
            return { parts: [...s.parts, { kind: "text", text: chunk.text }] };
          }
          case "tool_call":
            return {
              parts: [
                ...s.parts,
                {
                  kind: "tool",
                  step: {
                    id: chunk.toolId ?? newId(),
                    name: chunk.toolName ?? "tool",
                    arg: chunk.text,
                    status: "running",
                  },
                },
              ],
            };
          case "tool_request":
            return {
              parts: mapTool(s.parts, chunk.toolId, (st) => ({
                ...st,
                arg: chunk.text,
                status: "awaiting",
              })),
            };
          case "tool_result":
            return {
              parts: mapTool(s.parts, chunk.toolId, (st) => ({
                ...st,
                output: chunk.text,
                status: "done",
              })),
            };
          default:
            return {};
        }
      }),
    setStepStatus: (id, status) =>
      set((s) => ({ parts: mapTool(s.parts, id, (st) => ({ ...st, status })) })),
    finishAssistant: (full) =>
      set((s) => ({
        // A reply stopped before its first token has nothing worth keeping as a bubble.
        messages: full.trim()
          ? [...s.messages, { role: "assistant", content: full, id: newId() }]
          : s.messages,
        reasoning: "",
        parts: [],
        isStreaming: false,
        pending: null,
      })),
    setError: (error) => set({ error, isStreaming: false, pending: null }),
    setLoadingThread: (loadingThread) => set({ loadingThread }),

    send: async (raw, opts) => {
      const text = raw.trim();
      const start = get();
      if (!text || start.isStreaming) return;
      if (start.mode === "ask") return askTurn(text);
      const gen = ++streamSeq;
      activeStream = gen;
      const { threadId, provider, model } = start;
      set((s) => ({
        messages: [...s.messages, { role: "user", content: text, id: newId() }],
        isStreaming: true,
        pending: "chat",
        error: null,
        reasoning: "",
        parts: [],
      }));
      // This stream still owns the visible conversation.
      const isCurrent = () => activeStream === gen && get().threadId === threadId;

      // Coalesce streamed chunks into one store update per animation frame, capping
      // React renders at the display refresh rate regardless of token rate.
      let textBuf = "";
      let reasonBuf = "";
      let raf = 0;
      const flush = () => {
        raf = 0;
        if (!isCurrent()) {
          textBuf = "";
          reasonBuf = "";
          return;
        }
        const st = get();
        if (reasonBuf) {
          st.appendChunk({ kind: "reasoning", text: reasonBuf });
          reasonBuf = "";
        }
        if (textBuf) {
          st.appendChunk({ kind: "text", text: textBuf });
          textBuf = "";
        }
      };
      const drain = () => {
        if (raf) cancelAnimationFrame(raf);
        flush();
      };
      const onChunk = (chunk: ChatChunk) => {
        if (chunk.kind === "text") {
          textBuf += chunk.text;
          if (!raf) raf = requestAnimationFrame(flush);
        } else if (chunk.kind === "reasoning") {
          reasonBuf += chunk.text;
          if (!raf) raf = requestAnimationFrame(flush);
        } else {
          // tool_*: preserve interleave order by draining buffered text first.
          drain();
          if (isCurrent()) get().appendChunk(chunk);
        }
      };

      if (inflight) {
        await Promise.race([
          inflight.catch(() => {}),
          new Promise((resolve) => setTimeout(resolve, SETTLE_WAIT_MS)),
        ]);
      }
      if (!isCurrent()) return; // the user switched conversations while we waited
      if (stopRequested === gen) {
        activeStream = 0;
        set({ isStreaming: false });
        return;
      }

      const call = api.sendChat(
        {
          threadId,
          provider,
          model,
          baseUrl: opts?.baseUrl || null,
          messages: get().messages.filter(isSendable).map(toWire),
        },
        onChunk,
      );
      inflight = call;
      try {
        const full = await call;
        drain();
        if (isCurrent()) {
          get().finishAssistant(full);
          void queryClient.invalidateQueries({ queryKey: ["db_stats"] });
        }
        // Saved either way (an abandoned stream still persists its partial reply). The
        // chat is a thread, so Stats / Today / Projects / Spend count it too.
        void queryClient.invalidateQueries({ queryKey: ["chats"] });
        invalidateIndexDerived();
      } catch (e) {
        if (raf) cancelAnimationFrame(raf);
        if (isCurrent()) {
          set({ error: humanizeApiError(String(e), provider, model) });
          // The class of the message just shown (same classifier), never its text.
          trackError(e);
        }
      } finally {
        if (inflight === call) inflight = null;
        if (activeStream === gen) {
          activeStream = 0;
          set({ isStreaming: false, pending: null });
        }
      }
    },

    stop: () => {
      if (!get().isStreaming) return;
      if (get().pending === "ask") {
        // ask_history is one request with nothing to cancel: stop waiting for it. Its answer
        // is dropped when it lands and never saved (askTurn saves only a current answer).
        activeStream = 0;
        set({ isStreaming: false, pending: null });
        return;
      }
      stopRequested = activeStream;
      denyPendingApprovals(get().parts);
      void api.cancelChat().catch(() => {});
    },
  };
});
