// Browser-only fake of the Tauri runtime, so the UI runs (and can be screenshotted) without Rust.
// Use: `pnpm --filter callimachus dev`, then open
//   http://localhost:1420/mock.html?view=settings&section=providers&theme=light
// Params: theme, consent, memory (alias: knowledge), engines, agents, agenthooks, plugin, claude, keys,
// update, jobs, semantic, searchMs, onboarding (see SCENARIO below). Dev-only.
import type {
  AgentHooksStatus,
  AgentIntegration,
  AutoDistillBudget,
  CleanupRow,
  CliEngine,
  CompactSummary,
  DataRoots,
  DbStats,
  EmbedStatus,
  IndexReport,
  HarvestSettings,
  HookAgentId,
  HookSettings,
  IntegrationStatus,
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
  SourceKind,
  Stats,
  ThreadDetail,
  ThreadKnowledge,
  ThreadSummary,
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
};

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
].map(([source, title, p, count, daysAgo], i) => ({
  id: 4100 - i * 37,
  source: source as SourceKind,
  title: title as string,
  projectPath: PROJECTS[p as number],
  messageCount: count as number,
  updatedAt: NOW - Math.round((daysAgo as number) * DAY),
  starred: i === 0 || i === 5,
}));

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
    topProjects: PROJECTS.map((project, i) => ({ project, threads: [412, 318, 201, 176, 98][i] })),
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
  let out = threads;
  if (filters?.sources?.length) out = out.filter((t) => filters.sources?.includes(t.source));
  if (filters?.project) out = out.filter((t) => t.projectPath === filters.project);
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

const projectInfos = (): ProjectInfo[] =>
  PROJECTS.map((project, i) => ({
    project,
    threadCount: [412, 318, 201, 176, 98][i],
    distilledCount: knowledge.enabled ? [230, 140, 88, 40, 12][i] : 0,
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
  coach_overview: () => ({
    heatmap: Array.from({ length: 365 }, (_, i) => ({
      day: Math.floor((NOW - (364 - i) * DAY) / DAY) * DAY,
      messages: (i * 37) % 7 === 0 ? 0 : 20 + ((i * 53) % 260),
    })),
    decisions: [],
    gotchas: [],
    since: NOW - 7 * DAY,
  }),
  recurring_issues: () => [
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
  ],
  spend: () => ({
    totalCost: 412.37,
    trackedCalls: 18_902,
    untrackedCalls: 311,
    byModel: [
      {
        model: "claude-opus-4-8",
        cost: 301.2,
        input: 41_000_000,
        output: 3_100_000,
        cacheRead: 220_000_000,
        calls: 9_870,
        priced: true,
      },
      {
        model: "gpt-5-codex",
        cost: 111.17,
        input: 22_000_000,
        output: 1_900_000,
        cacheRead: 64_000_000,
        calls: 9_032,
        priced: true,
      },
    ],
    topThreads: threads.slice(0, 3).map((t, i) => ({
      threadId: t.id,
      title: t.title,
      project: t.projectPath,
      cost: [18.4, 12.9, 9.75][i],
    })),
  }),
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
  thread_commits: () => [],
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

  // Knowledge ---------------------------------------------------------------
  list_projects: projectInfos,
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
        `${provider} is not an available memory engine: pick one the card lists, or save its API key in Settings > Providers`,
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
  ask_history: () =>
    sleep(900).then(() => ({
      answer: "You fixed this in the payments repo by awaiting the scheduler before timers.",
      sources: threads.slice(0, 2).map((t) => ({
        threadId: t.id,
        title: t.title,
        source: t.source,
        projectPath: t.projectPath,
      })),
    })),

  // Chat and providers ------------------------------------------------------
  send_chat: async (a) => {
    const ch = channelId(a.onToken);
    const parts = ["Looking at your history, ", "you solved this ", "in payments-api last week."];
    for (const text of parts) {
      await sleep(200);
      if (ch !== null) sendToChannel(ch, { kind: "text", text });
    }
    return parts.join("");
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
