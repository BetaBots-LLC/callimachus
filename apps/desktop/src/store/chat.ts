import { create } from "zustand";
import type { ChatChunk, ChatMessage } from "../lib/api";

// A committed message with a stable id (keeps React subtree identity stable across
// the streaming→committed transition, so memoized bubbles don't remount).
export type StoredMessage = ChatMessage & { id: string };

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

interface ChatState {
  threadId: string;
  provider: string;
  model: string;
  baseUrl: string;
  messages: StoredMessage[];
  reasoning: string; // in-progress "thinking" text
  parts: StreamPart[]; // in-progress answer, interleaved text + tool steps
  isStreaming: boolean;
  loadingThread: boolean; // fetching a thread to open
  error: string | null;

  setProvider: (p: string, defaultModel: string) => void;
  setModel: (m: string) => void;
  setBaseUrl: (u: string) => void;
  newChat: () => void;
  loadChat: (threadId: string, messages: ChatMessage[]) => void;
  pushUser: (content: string) => void;
  addContext: (text: string) => void;
  appendChunk: (chunk: ChatChunk) => void;
  setStepStatus: (id: string, status: ToolStep["status"]) => void;
  finishAssistant: (full: string) => void;
  setError: (e: string | null) => void;
  setStreaming: (v: boolean) => void;
  setLoadingThread: (v: boolean) => void;
}

const newId = () =>
  globalThis.crypto?.randomUUID?.() ?? `chat-${Math.random().toString(36).slice(2)}`;

const mapTool = (parts: StreamPart[], id: string | undefined, fn: (s: ToolStep) => ToolStep) =>
  parts.map((p) =>
    p.kind === "tool" && p.step.id === id ? { kind: "tool" as const, step: fn(p.step) } : p,
  );

export const useChat = create<ChatState>((set) => ({
  threadId: newId(),
  provider: "anthropic",
  model: "claude-opus-4-8",
  baseUrl: "",
  messages: [],
  reasoning: "",
  parts: [],
  isStreaming: false,
  loadingThread: false,
  error: null,

  setProvider: (provider, defaultModel) => set({ provider, model: defaultModel }),
  setModel: (model) => set({ model }),
  setBaseUrl: (baseUrl) => set({ baseUrl }),
  newChat: () => set({ threadId: newId(), messages: [], reasoning: "", parts: [], error: null }),
  loadChat: (threadId, messages) =>
    set({
      threadId,
      messages: messages.map((m) => ({ ...m, id: newId() })),
      reasoning: "",
      parts: [],
      error: null,
      isStreaming: false,
    }),
  pushUser: (content) =>
    set((s) => ({ messages: [...s.messages, { role: "user", content, id: newId() }] })),
  addContext: (text) =>
    set((s) => ({
      messages: [
        ...s.messages,
        { role: "user", content: `Here is context for our conversation:\n\n${text}`, id: newId() },
      ],
    })),
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
      messages: [...s.messages, { role: "assistant", content: full, id: newId() }],
      reasoning: "",
      parts: [],
      isStreaming: false,
    })),
  setError: (error) => set({ error, isStreaming: false }),
  setStreaming: (isStreaming) => set({ isStreaming }),
  setLoadingThread: (loadingThread) => set({ loadingThread }),
}));
