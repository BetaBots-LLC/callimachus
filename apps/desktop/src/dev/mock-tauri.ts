// Browser-only fake of the Tauri runtime, so the UI runs (and can be screenshotted) without Rust.
// Use: `pnpm --filter callimachus dev`, then open
//   http://localhost:1420/mock.html?view=settings&section=engine&theme=light
// Params: theme, consent, memory (alias: knowledge), engines, agents, agenthooks, plugin, claude, keys,
// update, jobs, semantic, searchMs, onboarding, ledger, chats, projectdata, lastproject, muted,
// sections, today, scope (see SCENARIO below). mock-entry.ts also reads view, section,
// chatmode=ask and ask=<question> (sends it on load). Dev-only.
import type {
  AgentHooksStatus,
  AgentIntegration,
  AutoDistillBudget,
  AgentView,
  CleanupRow,
  CliEngine,
  CommitLink,
  CompactSummary,
  DataRoots,
  DbStats,
  EmbedStatus,
  FactHit,
  IndexReport,
  HarvestSettings,
  HookAgentId,
  HookSettings,
  IntegrationStatus,
  IssueCluster,
  JobsStatus,
  KnowledgeConfig,
  MemoryEngine,
  MemoryFact,
  MemorySetup,
  OnboardingSteps,
  ProjectInfo,
  ProjectMemory,
  SearchFilters,
  SearchHit,
  Snapshot,
  SourceKind,
  Spend,
  Stats,
  ThreadDetail,
  ThreadKnowledge,
  ThreadSummary,
  Today,
  ValueLedger,
} from "../lib/api";
import type { TelemetryStatus } from "../lib/telemetry";

// ---------------------------------------------------------------------------
// Scenario (URL params)
// ---------------------------------------------------------------------------

const params = new URLSearchParams(window.location.search);
function pick<T extends string>(name: string, allowed: readonly T[], fallback: T): T {
  const v = params.get(name);
  return v && (allowed as readonly string[]).includes(v) ? (v as T) : fallback;
}

// The setup checklist: `fresh` is a first launch (empty index until Index my history runs,
// nothing connected, memory off, no semantic index); `partial` reopens it with the index
// built, Claude Code connected and semantic search skipped; `done` with every step done.
// The other params still override what they name.
const ONBOARDING = pick("onboarding", ["none", "fresh", "partial", "done"] as const, "none");

const SCENARIO = {
  theme: pick("theme", ["light", "dark"] as const, "dark"),
  consent: pick("consent", ["unset", "granted", "denied"] as const, "unset"),
  onboarding: ONBOARDING,
  // LLM distillation on or off. `knowledge` is the older name for the same switch.
  memory: pick(
    "memory",
    ["on", "off"] as const,
    pick(
      "knowledge",
      ["on", "off"] as const,
      ONBOARDING === "fresh" || ONBOARDING === "partial" ? "off" : "on",
    ),
  ),
  // Which engines memory_setup detects: the claude / codex CLIs, only a saved API key, none,
  // or only Ollama: on localhost with models, on another host, or running with none pulled.
  engines: pick(
    "engines",
    ["cli", "key", "none", "ollama", "ollama-remote", "ollama-empty"] as const,
    "cli",
  ),
  agents: pick(
    "agents",
    ["connected", "partial", "none"] as const,
    ONBOARDING === "fresh" ? "none" : "partial",
  ),
  // `approve`: installed, but Codex wants them approved again in /hooks (after an update).
  agenthooks: pick("agenthooks", ["off", "on", "blocked", "approve"] as const, "off"),
  // The Callimachus Claude Code plugin is enabled, so the app installs only `cal`.
  plugin: pick("plugin", ["on", "off"] as const, "off"),
  // `absent`: no Claude Code config dir, so the checklist does not offer Claude Code.
  claude: pick("claude", ["present", "absent"] as const, "present"),
  keys: pick("keys", ["none", "some"] as const, "some"),
  update: pick("update", ["available", "fail", "none"] as const, "none"),
  jobs: pick("jobs", ["idle", "indexing"] as const, "idle"),
  // What a hybrid search reports as `semanticSkipped`: the model is ready (null), still
  // loading, or not cached at all.
  semantic: pick(
    "semantic",
    ["ready", "loading", "unavailable"] as const,
    ONBOARDING === "fresh" || ONBOARDING === "partial" ? "unavailable" : "ready",
  ),
  // How long a search takes. Above 0, a search still running when the next one starts
  // comes back superseded (empty), as the backend's cancellation does.
  searchMs: Math.max(0, Number(params.get("searchMs")) || 0),
  // The Stats value card: a week of hook and MCP activity with 312 sessions Claude Code
  // deleted (default), a fresh install with nothing yet, or retention already at a year.
  ledger: pick("ledger", ["active", "empty", "kept"] as const, "active"),
  // Chat's saved conversations: a few chats and one Ask answer (default), or none yet.
  chats: pick("chats", ["some", "none"] as const, "some"),
  // The Project page: `full` gives every section data; `empty` is a repo with sessions but
  // no snapshots, commits, recurring errors, spend or agent context yet.
  projectdata: pick("projectdata", ["full", "empty"] as const, "full"),
  // The remembered project (last_project). `web-dashboard` shows the page opening on it
  // rather than on the most recent repo; `none` falls back to the most recent one.
  lastproject: pick(
    "lastproject",
    ["web-dashboard", "payments-api", "none"] as const,
    "payments-api",
  ),
  // agent_view: Claude Code's SessionStart hook is installed (default) or not, and the
  // repo's AGENTS.md / CLAUDE.md block is current (default), older than the memory, or absent.
  hook: pick("hook", ["installed", "missing"] as const, "installed"),
  agentsfile: pick("agentsfile", ["current", "stale", "absent"] as const, "current"),
  // The repo is muted for the SessionStart hook (`cal hook --mute`).
  muted: pick("muted", ["on", "off"] as const, "off"),
  // The Today view: a working morning across three repos (default), nothing since the last
  // open, or the busy morning reported as cut at the backend's newest-200 cap.
  today: pick("today", ["busy", "quiet", "capped"] as const, "busy"),
  // The Stats activity card: loaded (default), a cold cache that never answers, or a failure.
  activity: pick("activity", ["ready", "slow", "error"] as const, "ready"),
  // The remembered last project (`last_project`), which Search scopes to by default:
  // payments-api (default), or none.
  scope: pick("scope", ["repo", "all"] as const, "repo"),
};

// sections=all|none: open (or close) every Project page section before the app reads the
// remembered state, for screenshots.
try {
  const sections = params.get("sections");
  if (sections === "all") {
    localStorage.setItem(
      "cal:project-sections",
      JSON.stringify(["sessions", "memory", "snapshots", "commits", "errors", "spend", "agents"]),
    );
  } else if (sections === "none") {
    localStorage.setItem("cal:project-sections", "[]");
  } else if (sections) {
    localStorage.setItem("cal:project-sections", JSON.stringify(sections.split(",")));
  }
} catch {
  /* storage blocked: the page uses its defaults */
}

// The theme store reads this at module load, which happens after this file runs.
try {
  if (params.has("theme")) localStorage.setItem("theme", SCENARIO.theme);
} catch {
  /* storage blocked: the app falls back to dark */
}

// ---------------------------------------------------------------------------
// Callback registry (mirrors Tauri's transformCallback: window[`_${id}`] = cb)
// ---------------------------------------------------------------------------

type Callback = (payload: unknown) => void;
const win = window as unknown as Record<string, unknown>;
let nextCallbackId = 1;

function transformCallback(callback?: Callback, once = false): number {
  const id = nextCallbackId++;
  const prop = `_${id}`;
  Object.defineProperty(window, prop, {
    value: (result: unknown) => {
      if (once) Reflect.deleteProperty(window, prop);
      return callback?.(result);
    },
    writable: false,
    configurable: true,
  });
  return id;
}

function unregisterCallback(id: number): void {
  Reflect.deleteProperty(window, `_${id}`);
}

function runCallback(id: number, payload: unknown): void {
  const cb = win[`_${id}`];
  if (typeof cb === "function") (cb as Callback)(payload);
}

/** Channels deliver `{ index, message }`, in order; track the index per channel. */
const channelIndex = new Map<number, number>();
function channelId(arg: unknown): number | null {
  if (arg && typeof arg === "object" && "id" in arg && typeof arg.id === "number") return arg.id;
  return null;
}
function sendToChannel(id: number, message: unknown): void {
  const index = channelIndex.get(id) ?? 0;
  channelIndex.set(id, index + 1);
  runCallback(id, { index, message });
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

interface Listener {
  event: string;
  handlerId: number;
}
const listeners = new Map<number, Listener>();
let nextEventId = 1;

export function emit(event: string, payload?: unknown): void {
  for (const [id, l] of listeners) {
    if (l.event === event) runCallback(l.handlerId, { event, id, payload });
  }
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
// Bumped by every search_threads call; a slower call that sees a newer one is superseded.
let searchGeneration = 0;

// ---------------------------------------------------------------------------
// Fake data: a developer with a year of Claude Code + Codex history
// ---------------------------------------------------------------------------

const NOW = Math.floor(Date.now() / 1000);
const DAY = 86_400;
const HOME = "/Users/dev";

const PROJECTS = [
  `${HOME}/code/payments-api`,
  `${HOME}/code/web-dashboard`,
  `${HOME}/code/infra`,
  `${HOME}/code/mobile-app`,
  `${HOME}/code/data-pipeline`,
  // A name that extends another repo's, so a substring scope (payments-api matching this
  // one too) shows up as a leaked row in every scoped list.
  `${HOME}/code/payments-api-v2`,
];

let threads: ThreadSummary[] = [
  ["claude_code", "Fix flaky webhook retry test in payments", 0, 64, 0.1],
  ["codex", "Migrate dashboard charts to Recharts 3", 1, 112, 0.4],
  ["claude_code", "Terraform: split prod and staging state", 2, 38, 1.2],
  ["claude_code", "Debug iOS push token refresh on cold start", 3, 91, 2.5],
  ["codex", "Add idempotency keys to the refunds endpoint", 0, 57, 3.1],
  ["claude_code", "Speed up nightly ETL by batching inserts", 4, 140, 5.8],
  ["cursor", "Tailwind 4 upgrade: fix dark mode tokens", 1, 26, 9.4],
  ["claude_code", "Rotate Stripe keys and update CI secrets", 2, 19, 14.0],
  ["codex", "Write a migration for soft-deleted invoices", 0, 73, 21.6],
  ["claude_code", "Profile React re-renders in the orders table", 1, 48, 33.2],
  // Older payments-api sessions, so the Project page's recent sessions has a full list.
  ["claude_code", "Ledger outbox: retry poison messages", 0, 66, 41.0],
  ["codex", "Stripe webhook signature check on raw body", 0, 29, 52.5],
  ["claude_code", "Split the refunds service out of the monolith", 0, 188, 67.0],
  ["gemini", "Explain the dispute lifecycle states", 0, 12, 88.3],
  // The prefix-named repo's one session, the oldest, so the lists above stay as they were.
  ["codex", "v2: port the webhook retry worker", 5, 34, 95.0],
].map(([source, title, p, count, daysAgo], i) => ({
  id: 4100 - i * 37,
  source: source as SourceKind,
  title: title as string,
  projectPath: PROJECTS[p as number],
  messageCount: count as number,
  updatedAt: NOW - Math.round((daysAgo as number) * DAY),
  starred: i === 0 || i === 5,
  // Gemini records no branch; the others carry the one the session ran on.
  gitBranch:
    source === "gemini"
      ? null
      : ["fix/webhook-retry", "main", "chore/tf-state", "feat/refund-idempotency"][i % 4],
}));

// Saved in-app chats (source in_app), listed in the Chat sidebar. An Ask turn or a chat
// reply saves into this list under its threadId, as persist_chat does.
interface MockChat {
  summary: ThreadSummary;
  externalId: string;
  messages: { role: string; text: string }[];
}
let nextChatId = 9000;
const chats: MockChat[] =
  SCENARIO.chats === "none"
    ? []
    : [
        {
          externalId: "chat-ask-1",
          messages: [
            { role: "user", text: "Why do refund requests need an idempotency key?" },
            {
              role: "assistant",
              // Saved as ask_history's answerWithSources: the footer lists the sources.
              text: "A client retry after a timeout created a second refund for the same charge [thread 3952]. Every POST that moves money now requires an Idempotency-Key header, and the webhook retry fix the same week relies on it [thread 4100].\n\nSources:\n[thread 3952] Add idempotency keys to the refunds endpoint\n[thread 4100] Fix flaky webhook retry test in payments",
            },
          ],
        },
        {
          externalId: "chat-2",
          messages: [
            { role: "user", text: "Draft a release note for the Recharts 3 migration" },
            { role: "assistant", text: "Charts now render with Recharts 3 [thread 4063]." },
          ],
        },
      ].map((c, i) => ({
        ...c,
        summary: {
          id: nextChatId++,
          source: "in_app" as SourceKind,
          title: c.messages[0].text,
          projectPath: null,
          messageCount: c.messages.length,
          updatedAt: NOW - (i + 1) * 5 * 3_600,
          starred: false,
        },
      }));

/** persist_chat's mock: create or replace the chat saved under `externalId`. */
function saveChat(externalId: string, turns: { role: string; content: string }[], reply: string) {
  const messages = [
    ...turns.map((m) => ({ role: m.role, text: m.content })),
    ...(reply.trim() ? [{ role: "assistant", text: reply }] : []),
  ];
  const title = messages.find((m) => m.role === "user")?.text.slice(0, 80) ?? null;
  const existing = chats.find((c) => c.externalId === externalId);
  const summary: ThreadSummary = {
    id: existing?.summary.id ?? nextChatId++,
    source: "in_app",
    title,
    projectPath: null,
    messageCount: messages.length,
    updatedAt: Math.floor(Date.now() / 1000),
    starred: false,
  };
  if (existing) chats.splice(chats.indexOf(existing), 1);
  chats.unshift({ externalId, messages, summary });
}

const CLEANUP_TITLES = [
  "Scaffold the first API routes",
  "Explore auth options (Clerk vs Supabase)",
  "Set up GitHub Actions cache",
  "Investigate memory leak in worker",
  "Prototype CSV importer",
  "Fix timezone bug in reports",
  "Bump Node to 22 across services",
  "Draft README for infra repo",
];
const CLEANUP_SOURCES: SourceKind[] = ["claude_code", "codex", "claude_code", "cursor", "gemini"];
let cleanupRows: CleanupRow[] = Array.from({ length: 24 }, (_, i) => ({
  id: 900 + i,
  source: CLEANUP_SOURCES[i % CLEANUP_SOURCES.length],
  title: `${CLEANUP_TITLES[i % CLEANUP_TITLES.length]}${i >= 8 ? ` (${Math.floor(i / 8) + 1})` : ""}`,
  projectPath: PROJECTS[(i * 3) % PROJECTS.length],
  messageCount: 12 + ((i * 29) % 180),
  bytes: 40_000 + ((i * 7_919_133) % 18_000_000),
  updatedAt: NOW - (330 - i * 9) * DAY,
}));

// semantic=unavailable means no model is cached, so nothing was ever embedded either.
const embed: EmbedStatus = {
  done: SCENARIO.semantic === "unavailable" ? 0 : 46_120,
  total: 48_213,
  running: false,
};
let closeToTray = false;
const keys = new Map<string, boolean>(
  ["anthropic", "openai", "gemini", "openrouter", "ollama", "ollama_cloud"].map((p) => [
    p,
    // engines=key needs a saved key whatever `keys` says; engines=none and the Ollama-only
    // scenarios need none.
    SCENARIO.engines === "key"
      ? p === "anthropic"
      : SCENARIO.engines !== "none" &&
        !SCENARIO.engines.startsWith("ollama") &&
        SCENARIO.keys === "some" &&
        p === "anthropic",
  ]),
);
const baseUrls = new Map<string, string>();
const knowledge: KnowledgeConfig = {
  enabled: SCENARIO.memory === "on",
  provider: SCENARIO.memory === "on" ? "claude-cli" : null,
  model: SCENARIO.memory === "on" ? "claude-sonnet-4-6" : null,
  autoDistill: SCENARIO.memory === "on",
};
// The hook digest and harvest toggles persist for the page's lifetime.
const hookSettings: HookSettings = {
  digestEnabled: true,
  mutedProjects: [`${HOME}/code/infra`],
};
// The retention setting persists for the page's lifetime, like set_claude_cleanup_days.
const retention: { days: number | null; previous: { days: number | null } | null } = {
  days: SCENARIO.ledger === "kept" ? 365 : null,
  previous: SCENARIO.ledger === "kept" ? { days: null } : null,
};
function valueLedger(): ValueLedger {
  const empty = SCENARIO.ledger === "empty";
  return {
    week: empty
      ? { hook: 0, mcp: 0, recall: 0, digest: 0 }
      : { hook: 23, mcp: 141, recall: 6, digest: 9 },
    allTime: empty ? 0 : 1874,
    preserved: {
      threads: empty ? 0 : 312,
      source: "claude_code",
      cleanupPeriodDays: retention.days,
      effectiveDays: retention.days ?? 30,
      previousCleanupDays: retention.previous?.days ?? null,
      canRestore: retention.previous !== null,
    },
  };
}
// Two imported facts, the two claude-memory rows projectMemory shows.
const harvest: HarvestSettings = { claudeMemory: true, importedFacts: 2 };
const CLI_ENGINES = [
  { id: "claude-cli", label: "Claude Code CLI", bin: "claude", path: "/opt/homebrew/bin/claude" },
  { id: "codex-cli", label: "Codex CLI", bin: "codex", path: "/opt/homebrew/bin/codex" },
];
const cliDetected = SCENARIO.engines === "cli";
const ollamaRunning = SCENARIO.engines.startsWith("ollama");
const ollamaBase =
  SCENARIO.engines === "ollama-remote" ? "http://gpu-box:11434" : "http://localhost:11434";
const ollamaPulled =
  SCENARIO.engines === "ollama-empty" ? [] : ["llama3.1:8b", "qwen2.5-coder:14b"];
function memoryEngines(): MemoryEngine[] {
  const engines: MemoryEngine[] = CLI_ENGINES.map((e) => ({
    id: e.id,
    label: e.label,
    kind: "cli",
    detected: cliDetected,
    detail: cliDetected ? e.path : "not found",
  }));
  // Labels and details as lib.rs `memory_engines` writes them.
  const n = ollamaPulled.length;
  engines.push({
    id: "ollama",
    label: "Ollama (local)",
    kind: "ollama",
    detected: ollamaRunning,
    detail: ollamaRunning
      ? `Running at ${ollamaBase}, ${n === 1 ? "1 model" : `${n} models`}`
      : `Not running at ${ollamaBase}`,
  });
  for (const [id, label] of [
    ["anthropic", "Anthropic"],
    ["openai", "OpenAI"],
    ["gemini", "Gemini"],
    ["openrouter", "OpenRouter"],
    ["ollama_cloud", "Ollama Cloud"],
  ] as const) {
    if (keys.get(id))
      engines.push({ id, label, kind: "key", detected: true, detail: "API key saved" });
  }
  return engines;
}
function memorySetup(): MemorySetup {
  const engines = memoryEngines();
  return {
    enabled: knowledge.enabled,
    provider: knowledge.provider,
    model: knowledge.model,
    engines,
    recommended: engines.find((e) => e.detected)?.id ?? null,
    eligibleThreads: budget.eligibleThreads,
    dailyCap: budget.dailyCap,
  };
}
const budget: AutoDistillBudget = {
  eligibleThreads: 37,
  dailyCap: 25,
  remainingToday: 18,
  recentDays: 30, // knowledge::AUTO_DISTILL_RECENT_DAYS
};
let telemetry: TelemetryStatus = {
  available: true,
  consent: SCENARIO.consent,
  enabled: SCENARIO.consent === "granted",
  envOptOut: false,
  anonId: "c3a1f0e2-7b4d-4c8e-9f21-5a6b7c8d9e0f",
};
const dataRoots: DataRoots = {
  claudeConfigDir: "",
  codexHome: "",
  effectiveClaudeConfigDir: `${HOME}/.claude`,
  effectiveCodexHome: `${HOME}/.codex`,
};
const jobs: JobsStatus = {
  index: SCENARIO.jobs === "indexing",
  catchUp: false,
  embed: false,
  distill: false,
  rewrite: false,
};

function recallStatus(installed: boolean): IntegrationStatus {
  const managed = SCENARIO.plugin === "on";
  return {
    present: SCENARIO.claude === "present",
    pluginManaged: managed,
    skillInstalled: installed && !managed,
    skillOutdated: false,
    mcpRegistered: installed && !managed,
    hookInstalled: installed && !managed,
    proactiveRecallInstalled: false,
    calInstalled: installed,
    exeStable: true,
    exeProblem: null,
    calProblem: null,
    pathHint: null,
    skillPath: `${HOME}/.claude/skills/recall/SKILL.md`,
    configPath: `${HOME}/.claude.json`,
  };
}
let recall = recallStatus(SCENARIO.agents !== "none" && SCENARIO.claude === "present");

function agentList(mode: "connected" | "partial" | "none"): AgentIntegration[] {
  return [
    {
      id: "codex",
      label: "Codex",
      present: true,
      registered: mode !== "none",
      configPath: `${HOME}/.codex/config.toml`,
    },
    {
      id: "cursor",
      label: "Cursor",
      present: true,
      registered: mode === "connected",
      configPath: `${HOME}/.cursor/mcp.json`,
    },
    {
      id: "gemini",
      label: "Gemini CLI",
      // Detected only in the fully connected scenario, so its hooks row can be checked too.
      present: mode === "connected",
      registered: mode === "connected",
      configPath: `${HOME}/.gemini/settings.json`,
    },
  ];
}
let agents = agentList(SCENARIO.agents);

/** `agenthooks=off|on|blocked|approve`: the Codex / Gemini CLI memory hooks. */
function agentHookRows(mode: "off" | "on" | "blocked" | "approve"): AgentHooksStatus[] {
  const present = (id: string) => agents.find((a) => a.id === id)?.present ?? false;
  const row = (agent: HookAgentId, configPath: string, dir: string): AgentHooksStatus => ({
    agent,
    configPath,
    installed: (mode === "on" || mode === "approve") && present(agent),
    needsApproval: mode === "approve" && agent === "codex" && present(agent),
    blocked: !present(agent)
      ? `${agent === "codex" ? "Codex" : "Gemini CLI"} is not set up on this machine (no ${dir})`
      : mode === "blocked" && agent === "codex"
        ? "hooks are disabled in config.toml"
        : null,
  });
  return [
    row("codex", `${HOME}/.codex/hooks.json`, `${HOME}/.codex`),
    row("gemini", `${HOME}/.gemini/settings.json`, `${HOME}/.gemini`),
  ];
}
const agentHooks = agentHookRows(SCENARIO.agenthooks);

const SOURCE_COUNTS: [SourceKind, number, number][] = [
  ["claude_code", 842, 33_904],
  ["codex", 361, 11_870],
  ["cursor", 58, 1_912],
  ["gemini", 23, 527],
];

// Free pages inside the mock index: under the 50 MB floor until threads are deleted.
let freeBytes = 248 * 1024;
const FILE_BYTES = 412_000_000;

// onboarding=fresh: nothing is indexed until index_all runs.
let indexed = SCENARIO.onboarding !== "fresh";

let onboardingSteps: OnboardingSteps = {
  index: SCENARIO.onboarding === "partial" || SCENARIO.onboarding === "done" ? "done" : null,
  agents: SCENARIO.onboarding === "partial" || SCENARIO.onboarding === "done" ? "done" : null,
  memory: SCENARIO.onboarding === "done" ? "done" : null,
  semantic:
    SCENARIO.onboarding === "partial" ? "skipped" : SCENARIO.onboarding === "done" ? "done" : null,
};

function dbStats(): DbStats {
  return {
    threads: indexed ? 1_284 - (24 - cleanupRows.length) : 0,
    messages: indexed ? 48_213 : 0,
    sources: 4,
    reclaimable: {
      bytes: freeBytes,
      fileBytes: FILE_BYTES,
      worthwhile: freeBytes >= 50 * 1024 * 1024 && freeBytes * 100 >= FILE_BYTES,
    },
  };
}

function stats(): Stats {
  return {
    threads: dbStats().threads,
    messages: 48_213,
    embedded: embed.done,
    embeddable: embed.total,
    earliest: NOW - 371 * DAY,
    latest: NOW - 600,
    perSource: SOURCE_COUNTS.map(([kind, t, m]) => ({ kind, threads: t, messages: m })),
    perRole: [
      { role: "assistant", messages: 21_480 },
      { role: "user", messages: 9_311 },
      { role: "tool", messages: 17_422 },
    ],
    topProjects: PROJECTS.map((project, i) => ({
      project,
      threads: [412, 318, 201, 176, 98, 7][i],
    })),
  };
}

function threadDetail(id: number): ThreadDetail | null {
  const t = threads.find((x) => x.id === id) ?? threads[0];
  if (!t) return null;
  const at = t.updatedAt ?? NOW;
  return {
    id: t.id,
    source: t.source,
    externalId: "7f3c2a10-5e8b-4d1f-a2c4-9b6e0d1f2a3b",
    title: t.title,
    projectPath: t.projectPath,
    projectKey: t.projectPath,
    gitBranch: "fix/webhook-retry",
    createdAt: at - 3_600,
    updatedAt: at,
    starred: t.starred,
    tags: ["payments", "flaky-test"],
    // Like the backend: tool rows carry a 300-char preview plus the full length, and
    // message_text serves the rest on expand.
    messages: threadMessages(at).map((m) => ({
      ...m,
      text: m.role === "tool" || m.toolName ? m.text.slice(0, 300) : m.text,
      textLen: m.text.length,
    })),
  };
}

function threadMessages(at: number) {
  return [
    {
      id: 1,
      role: "user",
      text: "The `webhook retries after 5xx` test fails about 1 in 10 runs on CI. Can you find out why?",
      toolName: null,
      ts: at - 3_600,
      model: null,
    },
    {
      id: 2,
      role: "assistant",
      text: "The test advances fake timers before the retry is scheduled, so the assertion races the queue. I'll await the scheduler tick first.",
      toolName: null,
      ts: at - 3_540,
      model: "claude-opus-4-8",
    },
    {
      id: 3,
      role: "tool",
      text: "PASS tests/webhooks/retry.test.ts (50 runs, 0 failures)",
      toolName: "Bash",
      ts: at - 3_200,
      model: null,
    },
    {
      id: 4,
      role: "assistant",
      text: "Fixed: `retry.test.ts` now awaits `scheduler.flush()` before advancing timers. 50 consecutive runs pass.",
      toolName: null,
      ts: at - 3_100,
      model: "claude-opus-4-8",
    },
  ];
}

/** The first thread as a long session: 18 prompts, each answered through a run of tool
 *  calls and results (one run has two parallel calls), like real Claude Code transcripts,
 *  where 70% of rows are tool rows. Ids start at 1 like threadMessages, so the search hits
 *  on this thread (HIT_PLAN: 1 a prompt, 2 a reply, 3 a Bash row) land on the same roles,
 *  and hit 3 sits inside a tool run. */
function longThreadMessages(at: number) {
  const tools: [string, (n: number) => string, (n: number) => string][] = [
    [
      "Bash",
      (n) => `{"command":"pnpm test webhooks --run ${n}"}`,
      (n) => `PASS tests/webhooks/retry.test.ts (${n} tests)\n${"ok ".repeat(140)}`,
    ],
    [
      "Read",
      (n) => `{"file_path":"/Users/dev/code/payments-api/src/webhooks/retry${n}.ts"}`,
      (n) =>
        Array.from(
          { length: 40 },
          (_, i) => `${i + 1}\texport const step${n}_${i} = schedule(${i});`,
        ).join("\n"),
    ],
    [
      "Edit",
      (n) =>
        `{"file_path":"/Users/dev/code/payments-api/src/webhooks/queue.ts","old_string":"tick()","new_string":"await tick(${n})"}`,
      () => "The file has been updated.",
    ],
    [
      "Grep",
      (n) => `{"pattern":"scheduler.flush","path":"src/${n % 2 ? "webhooks" : "queue"}"}`,
      () => "src/webhooks/retry.ts:41\nsrc/queue/scheduler.ts:88",
    ],
  ];
  type Row = {
    id: number;
    role: string;
    text: string;
    toolName: string | null;
    ts: number;
    model: string | null;
  };
  const rows: Row[] = [];
  let id = 1;
  const push = (role: string, text: string, toolName: string | null = null) =>
    rows.push({
      id: id++,
      role,
      text,
      toolName,
      ts: at - 7_200 + id * 6,
      model: role === "assistant" && !toolName ? "claude-opus-4-8" : null,
    });
  for (let turn = 0; turn < 18; turn++) {
    push(
      "user",
      turn === 0
        ? "The `webhook retries after 5xx` test fails about 1 in 10 runs on CI. Can you find out why?"
        : `Step ${turn + 1}: now check the ${["queue drain", "backoff jitter", "dead-letter path", "idempotency keys"][turn % 4]} and run the suite again.`,
    );
    push(
      "assistant",
      `Looking at the ${["retry scheduler", "fake timers", "queue worker"][turn % 3]} first, then I'll run the suite.`,
    );
    const calls = 2 + ((turn * 5) % 9);
    for (let c = 0; c < calls; c++) {
      const [name, args, out] = tools[(turn + c * (c % 3 === 0 ? 1 : 3)) % tools.length];
      if (turn === 3 && c === 1) {
        // Two parallel calls, then both results in order.
        push("assistant", `Read: ${tools[1][1](c)}`, "Read");
        push("assistant", `Grep: ${tools[3][1](c)}`, "Grep");
        push("tool", tools[1][2](c));
        push("tool", tools[3][2](c));
        continue;
      }
      push("assistant", `${name}: ${args(turn * 10 + c)}`, name);
      push("tool", out(turn * 10 + c));
    }
    push(
      "assistant",
      turn === 17
        ? "Fixed: `retry.test.ts` now awaits `scheduler.flush()` before advancing timers. 50 consecutive runs pass."
        : `The ${["timer", "queue", "retry"][turn % 3]} change holds; ${turn + 3} runs pass so far.`,
    );
  }
  return rows;
}

function filterThreads(filters?: SearchFilters): ThreadSummary[] {
  // Saved chats are threads too, but the fake history above is all agents; only a list that
  // asks for in_app (the Chat sidebar) gets them, so the other views' screenshots stay put.
  let out = filters?.sources?.includes("in_app")
    ? [...chats.map((c) => c.summary), ...threads]
    : threads;
  if (filters?.sources?.length) out = out.filter((t) => filters.sources?.includes(t.source));
  // The exact project key, like the backend's app filter: payments-api must not list the
  // payments-api-v2 session.
  const project = filters?.project;
  if (project) out = out.filter((t) => t.projectPath === project);
  const after = filters?.after;
  if (after != null) out = out.filter((t) => (t.updatedAt ?? 0) >= after);
  const before = filters?.before;
  if (before != null) out = out.filter((t) => (t.updatedAt ?? 0) <= before);
  if (filters?.starred) out = out.filter((t) => t.starred);
  return out.slice(0, filters?.limit ?? 50);
}

// Ranked like the backend: several hits per thread (it keeps up to 3), most of them tool
// output, so the results list groups them per thread and labels where each one matched.
// [thread index, role, message number in the thread, snippet with {q} and {p}]. The first
// thread is the long one, where 4 is the first Bash result; elsewhere 3 is the tool row.
// Snippets put the match 40+ characters in, like snippet() often does.
const HIT_PLAN: [number, string, number, string][] = [
  [0, "tool", 4, "FAIL tests/webhooks/retry.test.ts > {q} header is re-signed on retry"],
  [0, "assistant", 2, "...the retry path drops the {q} token, so attempt two gets a 401..."],
  [1, "user", 1, "Why does the charts page lose {q} state after the Recharts upgrade?"],
  [0, "user", 1, "Can you check whether {q} survives the webhook retry?"],
  [2, "tool", 3, '{p}/main.tf: module "{q}" { source = "./modules/iam" }'],
  [1, "tool", 3, "GET /api/{q}/session 200 in 41ms"],
  [3, "assistant", 2, "The push token refresh runs before {q} completes on cold start..."],
  [4, "tool", 3, '{"error":"missing {q} header","status":400}'],
  [2, "assistant", 4, "Split the {q} roles per workspace so staging cannot assume prod."],
  [5, "user", 1, "Batch the inserts but keep the {q} check per row."],
];

/** A mock message id: unique across threads like the backend's (get_thread serves these),
 *  so message_text and the thread's match ids find the right row. */
const MESSAGE_ID_SPAN = 10_000;
const messageId = (threadId: number, n: number) => threadId * MESSAGE_ID_SPAN + n;

// `?index=empty` shows the empty-index state; a query containing "nomatch" or
// "kubernetes" finds nothing (the no-match state).
const EMPTY_INDEX = params.get("index") === "empty";

function searchHits(query: string, filters?: SearchFilters): SearchHit[] {
  const q = query.trim() || "retry";
  if (EMPTY_INDEX || !indexed || /nomatch|kubernetes/i.test(q)) return [];
  const pool = filterThreads(filters).slice(0, 6);
  return HIT_PLAN.filter(([t]) => t < pool.length).map(([t, role, n, text]) => {
    const thread = pool[t];
    return {
      threadId: thread.id,
      messageId: messageId(thread.id, n),
      source: thread.source,
      title: thread.title,
      projectPath: thread.projectPath,
      // No toolName: the backend's SearchHit (search/mod.rs) has none.
      role,
      // Match boundaries are the backend's char(1)/char(2) sentinels (lib/format renderSnippet
      // escapes the text first, then turns them into <mark>), not literal tags.
      snippet: text
        .replace("{q}", `\u0001${q}\u0002`)
        .replace("{p}", thread.projectPath?.split("/").pop() ?? ""),
      ts: thread.updatedAt,
    };
  });
}

// Facts search_facts matches (every word of the query; "retry" and "webhook" find most).
const FACTS: FactHit[] = [
  [1, "decision", "Retries use exponential backoff capped at 5 minutes.", 0, true, "user"],
  [
    2,
    "gotcha",
    "Fake timers must be advanced after `scheduler.flush()` or the retry test hangs.",
    0,
    false,
    "llm",
  ],
  [
    3,
    "decision",
    "Webhook signatures are re-computed on every retry attempt, never cached.",
    0,
    false,
    "llm",
  ],
  [
    4,
    "gotcha",
    "Run the webhook tests with --test-threads=1; they share a port.",
    0,
    false,
    "claude-memory",
  ],
  [
    5,
    "decision",
    "Dashboard charts read from the reporting replica, never the primary.",
    1,
    false,
    "llm",
  ],
  [
    6,
    "gotcha",
    "Terraform state locks stick after a cancelled CI run; release them before a retry.",
    2,
    false,
    "llm",
  ],
  [
    7,
    "decision",
    "The v2 retry worker reads its schedule from the queue, not cron.",
    5,
    false,
    "llm",
  ],
].map(([id, kind, text, p, pinned, extractor]) => {
  const thread = threads[(id as number) % 3];
  return {
    id: id as number,
    kind: kind as FactHit["kind"],
    text: text as string,
    project: PROJECTS[p as number],
    threadId: thread.id,
    // Like the backend: facts distilled from a message know it (the thread then opens
    // there); imported ones (Claude Code memory, user notes) do not.
    sourceMessageId: extractor === "llm" ? 2 : null,
    title: thread.title,
    pinned: pinned as boolean,
    extractor: extractor as string,
    // Two days apart, so 24h keeps none, 7d the first three and 30d all of them.
    createdAt: NOW - (id as number) * 2 * DAY,
  };
});

function searchFacts(query: string, project: string | null, limit: number): FactHit[] {
  if (EMPTY_INDEX || !indexed || /nomatch|kubernetes/i.test(query)) return [];
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  // Like knowledge::search_facts: the exact project key, and every query word.
  return FACTS.filter((f) => !project || f.project === project)
    .filter((f) => words.every((w) => f.text.toLowerCase().includes(w)))
    .sort((a, b) => Number(b.pinned) - Number(a.pinned))
    .slice(0, limit);
}

function todayReport(since: number | null): Today {
  const from = since ?? NOW - 15 * 3_600;
  const busy = SCENARIO.today !== "quiet";
  const t = (
    i: number,
    durationMins: number,
    hoursAgo: number,
    commits: [string, string][] = [],
  ) => ({
    id: threads[i].id,
    title: threads[i].title,
    source: threads[i].source,
    lastActive: NOW - Math.round(hoursAgo * 3_600),
    durationSecs: durationMins * 60,
    messages: threads[i].messageCount,
    commits: commits.map(([sha, subject], j) => ({
      sha,
      shortSha: sha.slice(0, 7),
      subject,
      committedAt: NOW - Math.round((hoursAgo + 0.1 * (j + 1)) * 3_600),
    })),
  });
  if (!busy) {
    return {
      since: from,
      groups: [],
      newTodos: [],
      issuesSeenAgain: [],
      snapshots: [],
      commitsPartial: false,
      truncated: false,
    };
  }
  return {
    since: from,
    groups: [
      {
        project: PROJECTS[0],
        threads: [
          t(0, 42, 0.4, [
            [
              "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
              "test: await scheduler flush before advancing timers",
            ],
            [
              "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432",
              "fix(webhooks): re-sign the payload on every retry",
            ],
          ]),
          // The retry fix is linked to this session too (gitlink matches by files and
          // time), so the header and the standup must count it once.
          t(4, 95, 3.2, [
            [
              "0c1d2e3f405162738495a6b7c8d9e0f1a2b3c4d5",
              "feat(refunds): require an idempotency key",
            ],
            [
              "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432",
              "fix(webhooks): re-sign the payload on every retry",
            ],
          ]),
        ],
      },
      {
        project: PROJECTS[1],
        threads: [
          t(1, 128, 5.5, [
            ["5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f", "chore: move charts to Recharts 3"],
          ]),
          t(6, 0, 6.1),
        ],
      },
      { project: PROJECTS[2], threads: [t(2, 18, 9.8)] },
    ],
    newTodos: [
      {
        id: 71,
        text: "Backfill idempotency keys for legacy refunds.",
        project: PROJECTS[0],
        threadId: threads[4].id,
      },
      {
        id: 72,
        text: "Delete the Recharts 2 compatibility shim once the orders page ships.",
        project: PROJECTS[1],
        threadId: threads[1].id,
      },
      {
        id: 73,
        text: "Add a staging alarm for state lock age.",
        project: PROJECTS[2],
        threadId: threads[2].id,
      },
    ],
    issuesSeenAgain: [
      {
        signature: "error[E0502]: cannot borrow `self` as mutable",
        count: 41,
        lastSeen: NOW - 2 * 3_600,
        project: PROJECTS[0],
      },
      {
        signature: "TypeError: Cannot read properties of undefined (reading 'map')",
        count: 28,
        lastSeen: NOW - 5 * 3_600,
        project: PROJECTS[1],
      },
    ],
    snapshots: [
      {
        id: 12,
        threadId: threads[0].id,
        threadTitle: threads[0].title,
        // Captured in a worktree: the stored path is no project key, projectKey is the repo.
        projectPath: `${threads[0].projectPath}/.claude/worktrees/webhook-retry`,
        projectKey: threads[0].projectPath,
        sourceKind: threads[0].source,
        label: "auto · PreCompact",
        event: "PreCompact",
        createdAt: NOW - 3_600,
        chars: 18_400,
      },
    ],
    commitsPartial: false,
    truncated: SCENARIO.today === "capped",
  };
}

const projectInfos = (): ProjectInfo[] =>
  PROJECTS.map((project, i) => ({
    project,
    threadCount: [412, 318, 201, 176, 98, 7][i],
    distilledCount: knowledge.enabled ? [230, 140, 88, 40, 12, 3][i] : 0,
    lastActivity: NOW - i * 2 * DAY,
  }));

function projectMemory(project: string): ProjectMemory {
  const fact = (
    id: number,
    text: string,
    daysAgo: number,
    extractor: MemoryFact["extractor"],
    pinned = false,
  ): MemoryFact => ({
    id,
    threadId: threads[id % threads.length].id,
    text,
    title: extractor === "claude-memory" ? "Recorded memory" : threads[id % threads.length].title,
    createdAt: NOW - daysAgo * DAY,
    pinned,
    extractor,
    originRef:
      extractor === "claude-memory"
        ? `claude-memory:-Users-dev-code-payments-api/${id === 5 ? "project_ledger.md" : "feedback_tests.md"}`
        : null,
  });
  // With distillation off only the engine-free facts exist: Remember, TODO heuristics and
  // harvested Claude Code memory.
  const on = knowledge.enabled;
  const harvested = harvest.claudeMemory;
  const keep = (f: MemoryFact | false): f is MemoryFact => !!f;
  return {
    project,
    decisions: [
      fact(1, "Use idempotency keys on every POST that moves money.", 3, "user", true),
      on && fact(2, "Retries use exponential backoff capped at 5 minutes.", 12, "llm"),
      harvested &&
        fact(
          5,
          "Ledger writes go through the outbox table, never straight to Postgres.",
          2,
          "claude-memory",
        ),
    ].filter(keep),
    gotchas: [
      on && fact(3, "Fake timers must be advanced after `scheduler.flush()`.", 1, "llm"),
      harvested &&
        fact(
          6,
          "Run the webhook tests with --test-threads=1; they share a port.",
          4,
          "claude-memory",
        ),
    ].filter(keep),
    openTodos: [fact(4, "Backfill idempotency keys for legacy refunds.", 5, "heuristic")],
    threadCount: 412,
    distilledCount: on ? 230 : 0,
    pendingCount: 182,
  };
}

const threadKnowledge = (): ThreadKnowledge => ({
  summary: "Fixed a race between fake timers and the retry scheduler in the webhook tests.",
  decisions: [{ id: 1, text: "Await scheduler.flush() before advancing timers.", pinned: false }],
  gotchas: [
    { id: 3, text: "CI runs tests in parallel, which hid the race locally.", pinned: false },
  ],
  todos: [],
  extracted: knowledge.enabled,
  stale: false,
  error: null,
  canDistill: knowledge.enabled,
});

const CHANGELOG = `## Callimachus 0.12.0

- **Settings redesign**: sections on the left, every option in one place.
- Faster startup on large indexes (the splash closes about 40% sooner).
- \`cal search\` gains \`--since\` and \`--project\` filters.
- Codex sessions now show their reasoning blocks.
- Fixed: the tray icon could stay after quitting on Windows.
- Fixed: \`cal hook\` no longer prints a warning when the index is locked.

See the full list of changes on GitHub.`;

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

type Args = Record<string, unknown>;
type Handler = (args: Args) => unknown;

// Project page --------------------------------------------------------------

const RECURRING: IssueCluster[] = [
  {
    example: "error[E0502]: cannot borrow `self` as mutable",
    count: 41,
    threads: 17,
    firstSeen: NOW - 300 * DAY,
    lastSeen: NOW - 2 * DAY,
  },
  {
    example: "TypeError: Cannot read properties of undefined (reading 'map')",
    count: 28,
    threads: 12,
    firstSeen: NOW - 200 * DAY,
    lastSeen: NOW - 6 * DAY,
  },
  {
    example: "FAIL tests/webhooks/retry.test.ts > signature header is re-signed on retry",
    count: 9,
    threads: 4,
    firstSeen: NOW - 40 * DAY,
    lastSeen: NOW - 1 * DAY,
  },
];

/** Spend all time (since 0) or for a window; a project scope is a slice of the total. */
function spendFor(project: string | null, since: number): Spend {
  const empty = !!project && SCENARIO.projectdata === "empty";
  const scale = (since ? 0.18 : 1) * (project ? 0.42 : 1) * (empty ? 0 : 1);
  const pool = project ? threads.filter((t) => t.projectPath === project) : threads;
  return {
    totalCost: 412.37 * scale,
    trackedCalls: Math.round(18_902 * scale),
    untrackedCalls: empty ? 0 : 311,
    byModel: empty
      ? []
      : [
          {
            model: "claude-opus-4-8",
            cost: 301.2 * scale,
            input: 41_000_000,
            output: 3_100_000,
            cacheRead: 220_000_000,
            calls: Math.round(9_870 * scale),
            priced: true,
          },
          {
            model: "gpt-5-codex",
            cost: 111.17 * scale,
            input: 22_000_000,
            output: 1_900_000,
            cacheRead: 64_000_000,
            calls: Math.round(9_032 * scale),
            priced: true,
          },
        ],
    topThreads: empty
      ? []
      : pool.slice(0, 3).map((t, i) => ({
          threadId: t.id,
          title: t.title,
          project: t.projectPath,
          cost: [18.4, 12.9, 9.75][i] * (project ? 0.6 : 1),
        })),
  };
}

/** Linked commits for the payments-api sessions (the first and fifth thread). */
function commitsFor(threadId: number): CommitLink[] {
  if (SCENARIO.projectdata === "empty") return [];
  const at = threads.findIndex((t) => t.id === threadId);
  const commit = (sha: string, subject: string, daysAgo: number): CommitLink => ({
    sha,
    shortSha: sha.slice(0, 7),
    subject,
    committedAt: NOW - Math.round(daysAgo * DAY),
    overlap: 3,
  });
  if (at === 0) {
    return [
      commit(
        "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
        "test: await scheduler flush before advancing timers",
        0.1,
      ),
      commit(
        "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432",
        "fix(webhooks): re-sign the payload on every retry",
        0.2,
      ),
    ];
  }
  if (at === 4) {
    return [
      commit(
        "0badc0ffee15900dfacade0123456789abcdef01",
        "feat(refunds): require an Idempotency-Key header",
        3.0,
      ),
    ];
  }
  return [];
}

let snapshots: Snapshot[] =
  SCENARIO.projectdata === "empty"
    ? []
    : [
        [0, "auto · PreCompact", "PreCompact", 0.08, 41_200],
        [4, "before the refunds rewrite", "manual", 3.1, 18_400],
        [8, "auto · PreCompact", "PreCompact", 21.4, 47_900],
      ].map(([t, label, event, daysAgo, chars], i) => ({
        id: 318 - i * 41,
        threadId: threads[t as number].id,
        threadTitle: threads[t as number].title,
        projectPath: threads[t as number].projectPath,
        projectKey: threads[t as number].projectPath,
        sourceKind: threads[t as number].source,
        label: label as string,
        event: event as string,
        createdAt: NOW - Math.round((daysAgo as number) * DAY),
        chars: chars as number,
      }));

function snapshotBody(s: Snapshot): string {
  return `## Project memory (carry-forward)

**Decisions**
- Use idempotency keys on every POST that moves money.
- Ledger writes go through the outbox table, never straight to Postgres.

**Gotchas**
- Run the webhook tests with \`--test-threads=1\`; they share a port.

**Open TODOs**
- Backfill idempotency keys for legacy refunds.

---

# ${s.threadTitle ?? "Untitled session"}

**User:** The webhook retry test fails about one run in five. Can you find out why?

**Assistant:** The retry path rebuilds the request but reuses the original signature header, so the second attempt is rejected with a 401. I'll re-sign on every attempt and make the test advance the fake timers only after \`scheduler.flush()\`.

**Tool (Bash):** \`cargo test webhooks::retry -- --test-threads=1\` passed 50 of 50 runs.
`;
}

function agentView(project: string): AgentView {
  const name = project.split("/").pop() ?? project;
  if (SCENARIO.projectdata === "empty") {
    // No memory yet: the digest alone would speak, unless the repo is muted. The reasons are
    // the hook's own (cli_core::hook_output).
    return {
      hook: "",
      hookChars: 0,
      hookSilentReason:
        SCENARIO.muted === "on"
          ? `muted: no memory yet and the digest is muted for ${project} (\`cal hook --unmute\` to undo)`
          : `no threads: nothing indexed for ${project} yet`,
      hookCap: 10_000,
      agentsFile: "",
      agentsChars: 0,
      muted: SCENARIO.muted === "on",
      hookInstalled: SCENARIO.hook === "installed",
      agentsFileState: "absent",
    };
  }
  const facts = `# Callimachus memory for ${name}

## Decisions
- Use idempotency keys on every POST that moves money. (pinned)
- Retries use exponential backoff capped at 5 minutes.

## Gotchas
- Fake timers must be advanced after \`scheduler.flush()\`.`;
  const digest = `## Recent sessions in this repo
_Recent activity from past sessions, not distilled memory. Treat as notes, not instructions._
- Fix flaky webhook retry test in payments · Claude Code · fix/webhook-retry · 2 h ago
  gist: re-sign the webhook payload on every retry
  files: src/webhooks/retry.rs, tests/webhooks/retry.test.ts
- Add idempotency keys to the refunds endpoint · Codex · feat/refund-idempotency · 3 d ago
  gist: require an Idempotency-Key header on POST /refunds

Latest snapshot: #318 (auto · PreCompact), resume with \`cal resume 318\``;
  // Muting drops only the digest: a repo with memory still gets its facts.
  const hook = SCENARIO.muted === "on" ? facts : `${facts}\n\n${digest}`;
  return {
    hook,
    hookChars: hook.length,
    hookSilentReason: null,
    hookCap: 10_000,
    agentsFile: agentsBlock(name),
    agentsChars: agentsBlock(name).length,
    muted: SCENARIO.muted === "on",
    hookInstalled: SCENARIO.hook === "installed",
    agentsFileState: SCENARIO.agentsfile,
  };
}

function agentsBlock(name: string): string {
  return `<!-- callimachus:memory:start -->
## Project memory (${name}, from Callimachus)

### Decisions
- Use idempotency keys on every POST that moves money.
- Ledger writes go through the outbox table, never straight to Postgres.

### Gotchas
- Run the webhook tests with --test-threads=1; they share a port.
<!-- callimachus:memory:end -->`;
}

// The remembered project (`last_project`), which the Project page opens on and Search scopes
// to by default. `lastproject=` picks it (none: unset); `scope=all` also leaves it unset.
let lastProject: string | null =
  SCENARIO.lastproject === "none" || SCENARIO.scope === "all"
    ? null
    : (PROJECTS.find((p) => p.endsWith(`/${SCENARIO.lastproject}`)) ?? null);

const ok = () => null;

async function runIndexAll(): Promise<void> {
  jobs.index = true;
  const kinds = SOURCE_COUNTS.map(([k]) => k);
  for (let i = 0; i < kinds.length; i++) {
    emit("index:progress", { done: i, total: kinds.length, current: kinds[i] });
    await sleep(500);
  }
  jobs.index = false;
  indexed = true;
  const report: IndexReport = {
    threadsIndexed: 12,
    threadsSkipped: 1_272,
    messagesIndexed: 418,
    errors: 0,
  };
  emit("index:done", report);
}

async function runEmbeddings(): Promise<void> {
  embed.running = true;
  jobs.embed = true;
  const start = embed.done;
  for (let step = 1; step <= 5; step++) {
    await sleep(550);
    embed.done = Math.round(start + ((embed.total - start) * step) / 5);
    emit("embed:progress", { done: embed.done, total: embed.total });
  }
  await sleep(250);
  embed.running = false;
  jobs.embed = false;
  emit("embed:done");
}

const handlers: Record<string, Handler> = {
  // Plugins -----------------------------------------------------------------
  "plugin:event|listen": (a) => {
    const id = nextEventId++;
    listeners.set(id, { event: String(a.event), handlerId: Number(a.handler) });
    return id;
  },
  "plugin:event|unlisten": (a) => {
    listeners.delete(Number(a.eventId));
    return null;
  },
  "plugin:event|emit": ok,
  "plugin:event|emit_to": ok,
  "plugin:app|version": () => "0.11.1",
  "plugin:app|name": () => "Callimachus",
  "plugin:opener|open_url": (a) => {
    console.log("[mock] open url", a.url);
    return null;
  },
  "plugin:opener|open_path": (a) => {
    console.log("[mock] open path", a.path);
    return null;
  },
  "plugin:opener|reveal_item_in_dir": (a) => {
    console.log("[mock] reveal", a.paths);
    return null;
  },
  "plugin:process|restart": () => {
    console.log("[mock] relaunch");
    return null;
  },
  "plugin:process|exit": () => {
    console.log("[mock] exit");
    return null;
  },
  "plugin:resources|close": ok,
  "plugin:updater|check": () => {
    if (SCENARIO.update === "fail") {
      return Promise.reject(
        new Error(
          "error sending request for url (https://github.com/BetaBots-LLC/callimachus/releases/latest/download/latest.json)",
        ),
      );
    }
    if (SCENARIO.update !== "available") return null;
    return {
      rid: 7,
      currentVersion: "0.11.1",
      version: "0.12.0",
      date: "2026-11-02T14:55:12Z",
      body: CHANGELOG,
      rawJson: {},
    };
  },
  "plugin:updater|download": async (a) => {
    const ch = channelId(a.onEvent);
    if (ch !== null) {
      sendToChannel(ch, { event: "Started", data: { contentLength: 52_000_000 } });
      for (let i = 0; i < 10; i++) {
        await sleep(150);
        sendToChannel(ch, { event: "Progress", data: { chunkLength: 5_200_000 } });
      }
      sendToChannel(ch, { event: "Finished" });
    }
    return 8; // rid of the downloaded bytes
  },
  "plugin:updater|install": () => sleep(400).then(ok),

  // Splash / telemetry ------------------------------------------------------
  set_complete: ok,
  // TodayView marks the open after its first load (and again when the window hides); the
  // mock only logs it.
  mark_app_opened: () => {
    console.log("[mock] mark_app_opened");
    return null;
  },
  telemetry_state: () => telemetry,
  set_telemetry: (a) => {
    const granted = Boolean(a.granted);
    telemetry = { ...telemetry, consent: granted ? "granted" : "denied", enabled: granted };
    return telemetry;
  },
  track_event: ok,
  telemetry_flush: ok,

  // Stats and index ---------------------------------------------------------
  db_stats: dbStats,
  index_stats: stats,
  coach_overview: () => {
    if (SCENARIO.activity === "error") throw new Error("database is locked");
    const overview = {
      // Local calendar days with their own bounds, as search::coach_heatmap returns them.
      heatmap: Array.from({ length: 365 }, (_, i) => {
        const now = new Date(NOW * 1000);
        const day = new Date(now.getFullYear(), now.getMonth(), now.getDate() - (364 - i));
        const next = new Date(day.getFullYear(), day.getMonth(), day.getDate() + 1);
        const pad = (n: number) => String(n).padStart(2, "0");
        return {
          day: `${day.getFullYear()}-${pad(day.getMonth() + 1)}-${pad(day.getDate())}`,
          start: day.getTime() / 1000,
          end: next.getTime() / 1000 - 1,
          messages: (i * 37) % 7 === 0 ? 0 : 20 + ((i * 53) % 260),
        };
      }),
      decisions: [],
      gotchas: [],
      since: NOW - 7 * DAY,
    };
    // A cold heatmap cache: the card's loading state stays up.
    return SCENARIO.activity === "slow" ? new Promise<never>(() => {}) : overview;
  },
  recurring_issues: (a) => (a.project && SCENARIO.projectdata === "empty" ? [] : RECURRING),
  spend: (a) => spendFor(a.project ? String(a.project) : null, a.since ? Number(a.since) : 0),
  get_close_to_tray: () => closeToTray,

  set_close_to_tray: (a) => {
    closeToTray = Boolean(a.on);
    return null;
  },
  index_all: () => {
    void runIndexAll();
    return null;
  },
  indexing_status: () => jobs.index,
  index_source: () =>
    sleep(800).then(
      (): IndexReport => ({
        threadsIndexed: 3,
        threadsSkipped: 358,
        messagesIndexed: 96,
        errors: 0,
      }),
    ),
  jobs_status: () => ({ ...jobs }),
  embedding_status: () => ({ ...embed }),
  build_embeddings: () => {
    void runEmbeddings();
    return null;
  },

  // Storage -----------------------------------------------------------------
  cleanup_candidates: (a) => {
    const before = typeof a.before === "number" ? a.before : null;
    const sources = Array.isArray(a.sources) ? (a.sources as string[]) : null;
    const limit = typeof a.limit === "number" ? a.limit : 200;
    return cleanupRows
      .filter((r) => before === null || (r.updatedAt ?? 0) < before)
      .filter((r) => !sources || sources.includes(r.source))
      .slice(0, limit);
  },
  delete_threads: (a) => {
    const ids = new Set(Array.isArray(a.ids) ? (a.ids as number[]) : []);
    cleanupRows = cleanupRows.filter((r) => !ids.has(r.id));
    threads = threads.filter((t) => !ids.has(t.id));
    freeBytes += ids.size * 30_000_000;
    return ids.size;
  },
  vacuum_db: async () => {
    const before = dbStats().reclaimable;
    if (!before?.worthwhile)
      return { ran: false, freedBytes: 0, reclaimableBytes: freeBytes, pending: false };
    await sleep(1_200);
    const freed = freeBytes;
    freeBytes = 0;
    return { ran: true, freedBytes: freed, reclaimableBytes: freed, pending: false };
  },
  compact_index: async (): Promise<CompactSummary> => {
    jobs.rewrite = true;
    await sleep(600);
    emit("compact:progress", { rows: 140, bytesSaved: 21_000_000 });
    await sleep(600);
    jobs.rewrite = false;
    // Under the 50 MB rule, as on a real index: the space stays inside the file.
    freeBytes += 48_000_000;
    return {
      rows: 312,
      bytesSaved: 48_000_000,
      vacuum: { ran: false, freedBytes: 0, reclaimableBytes: freeBytes, pending: false },
    };
  },
  get_data_roots: () => ({ ...dataRoots }),
  set_data_roots: (a) => {
    if (typeof a.claudeConfigDir === "string") {
      dataRoots.claudeConfigDir = a.claudeConfigDir;
      dataRoots.effectiveClaudeConfigDir = a.claudeConfigDir || `${HOME}/.claude`;
    }
    if (typeof a.codexHome === "string") {
      dataRoots.codexHome = a.codexHome;
      dataRoots.effectiveCodexHome = a.codexHome || `${HOME}/.codex`;
    }
    return { ...dataRoots };
  },

  // Export ------------------------------------------------------------------
  obsidian_vaults: () => [`${HOME}/Documents/Obsidian Vault`, `${HOME}/Notes/Work`],
  export_thread: (a) => `${String(a.vaultDir)}/Callimachus/thread-${String(a.threadId)}.md`,
  synthesize_export: (a) =>
    sleep(900).then(() => `${String(a.vaultDir)}/Callimachus/thread-${String(a.threadId)}.md`),
  can_synthesize: () => [...keys.values()].some(Boolean),

  // Threads -----------------------------------------------------------------
  search_threads: async (a) => {
    const generation = ++searchGeneration;
    if (SCENARIO.searchMs > 0) await sleep(SCENARIO.searchMs);
    const filters = a.filters as SearchFilters | undefined;
    if (generation !== searchGeneration) {
      return { hits: [], semanticSkipped: null, superseded: true };
    }
    return {
      hits: searchHits(String(a.query ?? ""), filters),
      semanticSkipped: filters?.hybrid && SCENARIO.semantic !== "ready" ? SCENARIO.semantic : null,
      superseded: false,
    };
  },
  warm_semantic: ok,
  recent_threads: (a) =>
    EMPTY_INDEX || !indexed ? [] : filterThreads(a.filters as SearchFilters | undefined),
  search_by_file: () => threads.slice(0, 3),
  get_thread: (a) => {
    const chat = chats.find((c) => c.summary.id === Number(a.threadId));
    if (chat) {
      const at = chat.summary.updatedAt ?? NOW;
      return {
        ...chat.summary,
        externalId: chat.externalId,
        gitBranch: null,
        createdAt: at,
        tags: [],
        messages: chat.messages.map((m, i) => ({
          id: messageId(chat.summary.id, i + 1),
          role: m.role,
          text: m.text,
          toolName: null,
          ts: at,
          textLen: m.text.length,
        })),
      };
    }
    const d = threadDetail(Number(a.threadId));
    if (!d) return d;
    const own = <M extends { id: number }>(m: M): M => ({ ...m, id: messageId(d.id, m.id) });
    if (d.id !== threads[0]?.id) return { ...d, messages: d.messages.map(own) };
    // The first thread is the long one (longThreadMessages), previewed like the backend.
    const msgs = longThreadMessages(d.updatedAt ?? NOW).map((m) => ({
      ...m,
      text: m.role === "tool" || m.toolName ? m.text.slice(0, 300) : m.text,
      textLen: m.text.length,
    }));
    return { ...d, messages: msgs.map(own) };
  },
  message_text: (a) => {
    const id = Number(a.id);
    const threadId = Math.floor(id / MESSAGE_ID_SPAN);
    const msgs = threadId === threads[0]?.id ? longThreadMessages(NOW) : threadMessages(NOW);
    return msgs.find((m) => m.id === id % MESSAGE_ID_SPAN)?.text ?? "";
  },
  set_star: (a) => {
    threads = threads.map((t) =>
      t.id === Number(a.threadId) ? { ...t, starred: Boolean(a.starred) } : t,
    );
    return null;
  },
  set_thread_tags: ok,
  list_tags: () => [
    ["payments", 14],
    ["infra", 9],
    ["flaky-test", 4],
  ],
  thread_commits: (a) => commitsFor(Number(a.threadId)),
  link_thread_commits: () => [
    {
      sha: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
      shortSha: "a1b2c3d",
      subject: "test: await scheduler flush before advancing timers",
      committedAt: NOW - DAY,
      overlap: 3,
    },
  ],
  find_prior_work: () => [],
  resume_thread: ok,
  cli_launch_supported: () => true,
  thread_context: () => "# Context\n\n(mock) packed thread context.",
  open_thread_in_cli: (a) => String(a.program ?? "claude"),

  // Project page (Batch F backend commands) -------------------------------------
  list_snapshots: (a) =>
    snapshots
      .filter(
        (x) =>
          !a.project ||
          x.projectPath === a.project ||
          threads.find((t) => t.id === x.threadId)?.projectPath === a.project,
      )
      .slice(0, Number(a.limit ?? 20)),
  load_snapshot: (a) => {
    const snap = snapshots.find((x) => x.id === Number(a.id));
    if (!snap) throw new Error(`no snapshot #${String(a.id)}`);
    return sleep(250).then(() => ({ ...snap, body: snapshotBody(snap) }));
  },
  delete_snapshot: (a) => {
    const before = snapshots.length;
    snapshots = snapshots.filter((x) => x.id !== Number(a.id));
    return snapshots.length < before;
  },
  resume_snapshot: ok,
  agent_view: (a) => agentView(String(a.project)),
  last_project: () => lastProject,
  set_last_project: (a) => {
    lastProject = typeof a.project === "string" ? a.project : null;
    return null;
  },

  // Knowledge ---------------------------------------------------------------
  list_projects: projectInfos,
  search_facts: (a) =>
    searchFacts(
      String(a.query ?? ""),
      typeof a.project === "string" ? a.project : null,
      Number(a.limit) || 5,
    ),
  today: (a) => todayReport(typeof a.since === "number" ? a.since : null),
  project_memory: (a) => projectMemory(String(a.project)),
  project_brief: () => "Payments API: Rust + Postgres, Stripe for card processing.",
  distill_project: ok,
  distilling_status: () => jobs.distill,
  cancel_distill: ok,
  write_project_memory_file: (a) => `${String(a.project)}/MEMORY.md`,
  write_agent_memory_file: (a) => `${String(a.project)}/${String(a.filename)}`,
  set_fact_pinned: ok,
  hide_fact: ok,
  set_todo_done: ok,
  edit_fact: ok,
  detect_conflicts: () => [],
  remember: ok,
  list_open_todos: () => [
    {
      id: 4,
      threadId: threads[0]?.id ?? 1,
      text: "Backfill idempotency keys for legacy refunds.",
      source: "claude_code",
      title: threads[0]?.title ?? null,
      projectPath: PROJECTS[0],
      createdAt: NOW - 5 * DAY,
    },
  ],
  knowledge_config: () => ({ ...knowledge }),
  set_knowledge_config: (a) => {
    knowledge.enabled = Boolean(a.enabled);
    if (typeof a.provider === "string" || a.provider === null) knowledge.provider = a.provider;
    if (typeof a.model === "string" || a.model === null) knowledge.model = a.model;
    return null;
  },
  set_auto_distill: (a) => {
    knowledge.autoDistill = Boolean(a.on);
    return null;
  },
  auto_distill_budget: () => ({ ...budget }),
  set_auto_distill_daily_cap: (a) => {
    budget.dailyCap = Math.max(0, Number(a.cap) || 0);
    budget.remainingToday = Math.min(budget.remainingToday, budget.dailyCap);
    return { ...budget };
  },
  memory_setup: memorySetup,
  enable_memory: (a) => {
    const provider = String(a.provider);
    // As lib.rs `memory_engine_listed`: a CLI or Ollama even when not detected, a cloud
    // provider only with a saved key.
    const listed =
      provider.endsWith("-cli") ||
      provider === "ollama" ||
      memoryEngines().some((e) => e.id === provider && e.kind === "key");
    if (!listed)
      throw new Error(
        `${provider} is not an available memory engine: pick one the card lists, or save its API key in Settings > AI engine`,
      );
    knowledge.enabled = true;
    knowledge.provider = provider;
    knowledge.model = typeof a.model === "string" ? a.model : null;
    knowledge.autoDistill = true;
    return sleep(400).then(() => ({ ...knowledge }));
  },
  onboarding_state: () => ({ ...onboardingSteps }),
  set_onboarding_step: (a) => {
    const step = String(a.step);
    const state = a.state === "done" || a.state === "skipped" ? a.state : null;
    if (!(step in onboardingSteps)) throw new Error(`unknown checklist step "${step}"`);
    if (step === "index" && state === "skipped")
      throw new Error("indexing can't be skipped: the other steps need an index");
    onboardingSteps = { ...onboardingSteps, [step]: state };
    return { ...onboardingSteps };
  },
  hook_settings: () => ({ ...hookSettings, mutedProjects: [...hookSettings.mutedProjects] }),
  set_hook_digest: (a) => {
    hookSettings.digestEnabled = Boolean(a.on);
    return null;
  },
  value_ledger: () => valueLedger(),
  // As lib.rs: remembers the replaced value once; setting it back is the restore.
  set_claude_cleanup_days: (a) => {
    const days = typeof a.days === "number" ? a.days : null;
    if (days !== null && days < 1)
      throw new Error(`Transcript retention must be at least 1 day (Claude Code rejects ${days})`);
    if (days === retention.days) return null;
    if (retention.previous && retention.previous.days === days) retention.previous = null;
    else if (!retention.previous) retention.previous = { days: retention.days };
    retention.days = days;
    return sleep(300).then(() => null);
  },
  harvest_settings: () => ({ ...harvest }),
  set_harvest_claude_memory: (a) => {
    harvest.claudeMemory = Boolean(a.on);
    harvest.importedFacts = harvest.claudeMemory ? 2 : 0;
    return null;
  },
  thread_knowledge: threadKnowledge,
  distill_thread: () => sleep(900).then(threadKnowledge),
  recall_decisions: () => [],
  recall_gotchas: () => [],
  ask_history: async () => {
    await sleep(900);
    // Like the backend: memory has to be on, since Ask runs on the memory engine.
    if (!knowledge.enabled) {
      throw new Error(
        "LLM distillation is off: turn on memory in Settings > AI engine (a CLI subscription, local Ollama or an API key)",
      );
    }
    const [first, second] = threads;
    const answer = `You fixed this in payments-api by awaiting \`scheduler.flush()\` before advancing the fake timers, so the retry no longer races the scheduler [thread ${first.id}]. The refunds endpoint reused the same pattern for its idempotency tests [thread ${threads[4].id}, ${second.id}].`;
    const cited = [first, threads[4], second];
    // Saves nothing, like the backend: the chat store calls save_chat on success.
    return {
      answer,
      sources: cited.map((t) => ({
        threadId: t.id,
        title: t.title,
        source: t.source,
        projectPath: t.projectPath,
        messageId: messageId(t.id, 2),
      })),
      answerWithSources: `${answer}\n\nSources:${cited.map((t) => `\n[thread ${t.id}] ${t.title ?? "untitled"}`).join("")}`,
    };
  },
  save_chat: (a) => {
    saveChat(
      String(a.threadId),
      (a.messages as { role: string; content: string }[] | undefined) ?? [],
      String(a.reply ?? ""),
    );
    return null;
  },

  // Chat and providers ------------------------------------------------------
  send_chat: async (a) => {
    const ch = channelId(a.onToken);
    const parts = ["Looking at your history, ", "you solved this ", "in payments-api last week."];
    for (const text of parts) {
      await sleep(200);
      if (ch !== null) sendToChannel(ch, { kind: "text", text });
    }
    const reply = parts.join("");
    saveChat(
      String(a.threadId),
      (a.messages as { role: string; content: string }[] | undefined) ?? [],
      reply,
    );
    return reply;
  },
  cancel_chat: ok,
  approve_tool: ok,
  set_api_key: (a) => {
    keys.set(String(a.provider), true);
    return null;
  },
  delete_api_key: (a) => {
    keys.set(String(a.provider), false);
    return null;
  },
  provider_has_key: (a) => keys.get(String(a.provider)) ?? false,
  provider_base_url: (a) => baseUrls.get(String(a.provider)) ?? "",
  set_provider_base_url: (a) => {
    baseUrls.set(String(a.provider), String(a.url ?? ""));
    return null;
  },
  list_models: (a) =>
    sleep(300).then(() => {
      switch (String(a.provider)) {
        case "anthropic":
          return ["claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5"];
        case "openai":
          return ["gpt-4o", "gpt-4.1", "o3", "o4-mini"];
        case "ollama":
          return ollamaPulled;
        default:
          return [];
      }
    }),
  cli_engines: (): CliEngine[] =>
    CLI_ENGINES.map((e) => ({ id: e.id, label: e.label, bin: e.bin, installed: cliDetected })),

  // Agent integrations ------------------------------------------------------
  recall_integration_status: () => ({ ...recall }),
  install_recall_integration: () => {
    recall = recallStatus(true);
    return { ...recall };
  },
  uninstall_recall_integration: () => {
    recall = recallStatus(false);
    return null;
  },
  set_proactive_recall: (a) => {
    recall = { ...recall, proactiveRecallInstalled: Boolean(a.enabled) };
    return { ...recall };
  },
  agent_integrations_status: () => agents.map((x) => ({ ...x })),
  install_agent_integrations: () => {
    agents = agents.map((x) => ({ ...x, registered: x.present }));
    return agents.map((x) => ({ ...x }));
  },
  uninstall_agent_integrations: () => {
    agents = agents.map((x) => ({ ...x, registered: false }));
    return null;
  },
  agent_hooks_status: () => agentHooks.map((x) => ({ ...x })),
  install_agent_hooks: (a) => {
    const row = agentHooks.find((x) => x.agent === a.agent);
    if (!row) throw new Error(`memory hooks are available for codex and gemini, not '${a.agent}'`);
    if (row.blocked) throw new Error(`Can't add memory hooks: ${row.blocked}`);
    row.installed = true;
    return { ...row };
  },
  uninstall_agent_hooks: (a) => {
    const row = agentHooks.find((x) => x.agent === a.agent);
    if (!row) throw new Error(`memory hooks are available for codex and gemini, not '${a.agent}'`);
    row.installed = false;
    return { ...row };
  },
};

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

async function invoke(cmd: string, args: Args = {}): Promise<unknown> {
  const handler = handlers[cmd];
  if (!handler) {
    console.warn("[mock] no handler for", cmd, args);
    throw new Error(`[mock] no handler for ${cmd}`);
  }
  // A tick of latency, like real IPC, so loading states render at least once.
  await sleep(30);
  return handler(args);
}

win.__TAURI_INTERNALS__ = {
  invoke,
  transformCallback,
  unregisterCallback,
  convertFileSrc: (path: string, protocol = "asset") =>
    `${protocol}://localhost/${encodeURIComponent(path)}`,
  metadata: {
    currentWindow: { label: "main" },
    currentWebview: { label: "main", windowLabel: "main" },
  },
  plugins: {},
};
win.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
  unregisterListener: (_event: string, eventId: number) => {
    listeners.delete(eventId);
  },
};

// jobs=indexing: show a reindex in flight that never finishes, for the progress UI.
if (SCENARIO.jobs === "indexing") {
  setTimeout(() => emit("index:progress", { done: 1, total: 4, current: "codex" }), 500);
}

console.info("[mock] Tauri runtime faked", SCENARIO);
