import { type ReactNode, useEffect, useRef, useState } from "react";
import { type UseQueryResult, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, ChevronDown, Minus } from "lucide-react";
import {
  type AgentHooksStatus,
  type AgentIntegration,
  api,
  type HookAgentId,
  type IndexProgress,
  type IntegrationStatus,
  ONBOARDING_STEPS,
  type OnboardingStep,
  type OnboardingSteps,
  type OnboardingStepState,
  SOURCE_LABELS,
  type SourceKind,
} from "../lib/api";
import { MOD_KEY } from "../lib/platform";
import {
  useBuildSemanticIndex,
  useEmbedError,
  useEmbedStatus,
  useIndexError,
} from "../lib/queries";
import { errorText, notify } from "../lib/toast";
import { useChecklist } from "../store/checklist";
import { Button } from "@/components/ui/button";
import { Progress, ProgressLabel } from "@/components/ui/progress";
import { Spinner } from "@/components/ui/spinner";
import { cn } from "@/lib/utils";
import { MemorySetupCard } from "./MemorySetupCard";
import { TelemetryConsentLine } from "./TelemetryConsent";

/** Semantic search counts as set up at this share of embeddable messages: the watcher's
 *  background top-up keeps the rest current, and a build that stops a few percent short
 *  (messages that failed inference, sessions added since) should not hold the step open. */
const SEMANTIC_DONE_RATIO = 0.9;

type StepStatus = "done" | "skipped" | "pending";

const ONBOARDING_KEY = ["onboarding_state"] as const;
const NOTHING_ANSWERED: OnboardingSteps = {
  index: null,
  agents: null,
  memory: null,
  semantic: null,
};

const STEP_COPY: Record<OnboardingStep, { title: string; unlocks: string }> = {
  index: {
    title: "Index my history",
    unlocks: "Unlocks search across every agent session on this machine.",
  },
  agents: {
    title: "Connect your agents",
    unlocks:
      "Unlocks recall inside your agents: they search this history and get each repo's memory.",
  },
  memory: {
    title: "Turn on memory",
    unlocks: "Unlocks each repo's decisions and gotchas for Ask, Projects and your agents' hooks.",
  },
  semantic: {
    title: "Semantic search",
    unlocks: "Unlocks search by meaning, so a session turns up even when you forget its words.",
  },
};

/** Claude Code counts as connected on the same rule as Settings > Agents: with the Claude
 *  Code plugin enabled the plugin brings the skill, the MCP server and the hooks (so the app
 *  removes its own and those flags read false), and the `cal` CLI its hooks run is all the
 *  app adds. */
function claudeConnected(s: IntegrationStatus | undefined): boolean {
  if (!s) return false;
  if (s.pluginManaged) return s.calInstalled;
  return s.skillInstalled && s.mcpRegistered && s.hookInstalled && !s.skillOutdated;
}

/** Whether to offer Claude Code at all. Connecting it on a machine without it would create
 *  `~/.claude.json`, `~/.claude/settings.json` and a skill for an agent nobody runs. An older
 *  backend sends no `present`, and anything of ours already installed means it is in use. */
function claudeDetected(s: IntegrationStatus | undefined): boolean {
  if (!s) return true;
  return (
    s.present !== false ||
    !!s.pluginManaged ||
    s.skillInstalled ||
    s.mcpRegistered ||
    s.hookInstalled
  );
}

/** The header button of a step (always rendered, unlike its body). */
function stepHeader(li: HTMLLIElement | null | undefined): HTMLButtonElement | null {
  return li?.querySelector<HTMLButtonElement>(":scope > button") ?? null;
}

/**
 * First-run setup checklist, shown in place of the Search view while the index is empty
 * and for the rest of that session (store/checklist.ts), or when the user reopens it from
 * the command palette or Settings > General. Four steps: index the history (required), then
 * connect agents, turn on memory and build semantic search (each skippable). A step's done
 * state comes from live data and is recorded in `onboarding.steps` so it sticks; skips are
 * recorded there too.
 */
export function Onboarding() {
  const qc = useQueryClient();
  const firstRun = useChecklist((s) => s.firstRun);
  const close = useChecklist((s) => s.close);
  const focus = useChecklist((s) => s.focus);
  const focusSeq = useChecklist((s) => s.focusSeq);

  const saved = useQuery({ queryKey: ONBOARDING_KEY, queryFn: api.onboardingState });
  const record = useMutation({
    mutationFn: (v: { step: OnboardingStep; state: OnboardingStepState }) =>
      api.setOnboardingStep(v.step, v.state),
    onSuccess: (steps) => qc.setQueryData(ONBOARDING_KEY, steps),
    onError: (e) => notify.error("Couldn't save the checklist", { description: errorText(e) }),
  });

  // The same caches the header, Settings and the memory card read, so nothing extra runs.
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  const recall = useQuery({
    queryKey: ["recall_integration"],
    queryFn: api.recallIntegrationStatus,
  });
  const others = useQuery({
    queryKey: ["agent_integrations"],
    queryFn: api.agentIntegrationsStatus,
  });
  const knowledge = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  const embed = useEmbedStatus();

  const threads = stats.data?.threads ?? 0;
  const coverage = embed.data && embed.data.total > 0 ? embed.data.done / embed.data.total : null;
  const live: Record<OnboardingStep, boolean> = {
    index: threads > 0,
    agents:
      claudeConnected(recall.data) || (others.data ?? []).some((a) => a.present && a.registered),
    memory: !!knowledge.data?.enabled,
    semantic: coverage !== null && coverage >= SEMANTIC_DONE_RATIO,
  };
  const steps = saved.data ?? NOTHING_ANSWERED;
  const status = (step: OnboardingStep): StepStatus => {
    // Indexing follows the index itself: an emptied index needs indexing again.
    if (step === "index") return live.index ? "done" : "pending";
    if (live[step] || steps[step] === "done") return "done";
    return steps[step] === "skipped" ? "skipped" : "pending";
  };
  const statuses = ONBOARDING_STEPS.map(status);
  const doneCount = statuses.filter((s) => s === "done").length;
  const firstPending = ONBOARDING_STEPS.find((s) => status(s) === "pending") ?? null;
  const allAnswered = saved.isSuccess && firstPending === null;

  // Record a step the moment its live state says done, once per step per mount, so it
  // stays done when that state changes later (an agent's config edited by hand).
  const recorded = useRef(new Set<OnboardingStep>());
  const { mutate } = record;
  const toRecord = saved.isSuccess
    ? ONBOARDING_STEPS.filter((s) => live[s] && steps[s] !== "done").join(",")
    : "";
  useEffect(() => {
    for (const step of toRecord.split(",") as OnboardingStep[]) {
      if (!step || recorded.current.has(step)) continue;
      recorded.current.add(step);
      mutate({ step, state: "done" });
    }
  }, [toRecord, mutate]);

  // A first-run checklist steps aside once nothing is left to answer. One the user reopened
  // stays until they close it, so they can look over what they set up.
  const anySkipped = statuses.includes("skipped");
  useEffect(() => {
    if (!firstRun || !allAnswered) return;
    close();
    const reopen = `the command palette (${MOD_KEY} K) or Settings > General`;
    if (anySkipped)
      notify.success("Setup finished", {
        description: `The steps you skipped wait in the checklist: reopen it anytime from ${reopen}.`,
      });
    else
      notify.success("Setup complete", {
        description: `Reopen the checklist anytime from ${reopen}.`,
      });
  }, [firstRun, allAnswered, anySkipped, close]);

  // Which step is open: the one a link asked for, else the user's pick, else the next one
  // to do. Opening one collapses the rest; "none" is every step collapsed.
  const [picked, setPicked] = useState<OnboardingStep | "none" | null>(focus);
  const [lastSeq, setLastSeq] = useState(focusSeq);
  if (focusSeq !== lastSeq) {
    setLastSeq(focusSeq);
    setPicked(focus);
  }
  const expanded = picked === "none" ? null : (picked ?? firstPending);
  const stepRefs = useRef<Partial<Record<OnboardingStep, HTMLLIElement | null>>>({});
  // A link that asked for a step ("Go to the memory step" in Ask or Projects) unmounted the
  // view it sat in, so focus would fall to <body>: put it on that step's header. Once per
  // request, since effects re-run each time the Search tab's Activity becomes visible again.
  const handledSeq = useRef(0);
  useEffect(() => {
    if (focusSeq === 0 || !focus || handledSeq.current === focusSeq) return;
    handledSeq.current = focusSeq;
    const li = stepRefs.current[focus];
    li?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    stepHeader(li)?.focus({ preventScroll: true });
  }, [focus, focusSeq]);

  const skip = (step: OnboardingStep) => {
    record.mutate({ step, state: "skipped" });
    // Move on to the next step rather than leaving the skipped one open, and take focus
    // there: Skip for now unmounts with the step's body, which would drop focus to <body>.
    const at = ONBOARDING_STEPS.indexOf(step);
    const after = [...ONBOARDING_STEPS.slice(at + 1), ...ONBOARDING_STEPS.slice(0, at)];
    const next = after.find((s) => status(s) === "pending") ?? null;
    setPicked(next ?? "none");
    stepHeader(stepRefs.current[next ?? step])?.focus();
  };

  const welcome = status("index") === "pending";

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex w-full max-w-xl flex-col gap-6 px-6 py-10">
        <header className="space-y-1.5 text-center">
          <h1 className="text-2xl font-semibold tracking-tight">
            {welcome ? "Welcome to Callimachus" : "Setup checklist"}
          </h1>
          <p className="text-sm text-muted-foreground">
            {welcome
              ? "One local, searchable index of every AI coding-agent conversation you've had. Four steps; only the first is required."
              : firstPending
                ? `${doneCount} of ${ONBOARDING_STEPS.length} done. Skip what you don't need and come back to it anytime.`
                : `${doneCount} of ${ONBOARDING_STEPS.length} done. Open a step to see or change it.`}
          </p>
        </header>

        <ol className="space-y-2">
          {ONBOARDING_STEPS.map((step, i) => {
            const pending = statuses[i] === "pending";
            return (
              <StepItem
                key={step}
                ref={(el) => {
                  stepRefs.current[step] = el;
                }}
                n={i + 1}
                step={step}
                status={statuses[i]}
                open={expanded === step}
                onToggle={() => setPicked(expanded === step ? "none" : step)}
              >
                {step === "index" && <IndexStep />}
                {step === "agents" && (
                  <AgentsStep
                    recall={recall}
                    others={others}
                    done={statuses[i] === "done"}
                    onSkip={pending ? () => skip("agents") : undefined}
                  />
                )}
                {step === "memory" && (
                  <MemoryStep
                    done={statuses[i] === "done"}
                    onSkip={pending ? () => skip("memory") : undefined}
                  />
                )}
                {step === "semantic" && (
                  <SemanticStep
                    indexed={live.index}
                    onSkip={pending ? () => skip("semantic") : undefined}
                  />
                )}
              </StepItem>
            );
          })}
        </ol>

        <footer className="space-y-3 text-center">
          <TelemetryConsentLine />
          <p className="text-xs text-muted-foreground">
            Your index stays on your machine. Memory sends sessions only to the engine you pick.
          </p>
          {live.index && (
            <Button size="sm" variant="ghost" onClick={close}>
              {firstPending ? "Finish later" : "Close checklist"}
            </Button>
          )}
        </footer>
      </div>
    </div>
  );
}

/** One step: a header row that opens it, and its body while open. */
function StepItem({
  ref,
  n,
  step,
  status,
  open,
  onToggle,
  children,
}: {
  ref: (el: HTMLLIElement | null) => void;
  n: number;
  step: OnboardingStep;
  status: StepStatus;
  open: boolean;
  onToggle: () => void;
  children: ReactNode;
}) {
  const copy = STEP_COPY[step];
  const bodyId = `checklist-step-${step}`;
  return (
    <li
      ref={ref}
      className={cn(
        "rounded-xl border bg-card text-card-foreground transition-colors",
        open && "border-ring/50",
      )}
    >
      <button
        type="button"
        aria-expanded={open}
        aria-controls={open ? bodyId : undefined}
        onClick={onToggle}
        className="flex w-full cursor-pointer items-start gap-3 rounded-xl px-4 py-3 text-left outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
      >
        <StepMarker n={n} status={status} />
        <span className="min-w-0 flex-1 space-y-0.5">
          <span className="flex items-baseline gap-2">
            <span
              className={cn(
                "text-sm font-medium",
                status !== "pending" && !open && "text-muted-foreground",
              )}
            >
              {copy.title}
            </span>
            {/* aria-hidden: the marker's sr-only "Done:" / "Skipped:" already says it, ahead
                of the title, and both would be read. */}
            {status === "done" && (
              <span aria-hidden="true" className="text-xs text-muted-foreground">
                Done
              </span>
            )}
            {status === "skipped" && (
              <span aria-hidden="true" className="text-xs text-muted-foreground">
                Skipped
              </span>
            )}
          </span>
          <span className="block text-[13px] leading-normal text-muted-foreground">
            {copy.unlocks}
          </span>
        </span>
        <ChevronDown
          aria-hidden="true"
          className={cn(
            "mt-0.5 size-4 shrink-0 text-muted-foreground transition-transform",
            open && "rotate-180",
          )}
        />
      </button>
      {open && (
        <div id={bodyId} className="pr-4 pb-4 pl-13">
          {children}
        </div>
      )}
    </li>
  );
}

function StepMarker({ n, status }: { n: number; status: StepStatus }) {
  const base = "mt-px grid size-6 shrink-0 place-items-center rounded-full text-xs font-medium";
  if (status === "done")
    return (
      <span className={cn(base, "bg-primary text-primary-foreground")}>
        <Check className="size-3.5" aria-hidden="true" />
        <span className="sr-only">Done:</span>
      </span>
    );
  if (status === "skipped")
    return (
      <span
        className={cn(
          base,
          "border border-dashed border-muted-foreground/50 text-muted-foreground",
        )}
      >
        <Minus className="size-3.5" aria-hidden="true" />
        <span className="sr-only">Skipped:</span>
      </span>
    );
  return (
    <span
      className={cn(base, "border border-muted-foreground/40 text-muted-foreground tabular-nums")}
    >
      {n}
    </span>
  );
}

function SkipButton({ onSkip }: { onSkip?: () => void }) {
  if (!onSkip) return null;
  return (
    <Button size="sm" variant="ghost" onClick={onSkip}>
      Skip for now
    </Button>
  );
}

/** Step 1, the old one-screen onboarding: one click indexes the local agent history. */
function IndexStep() {
  const qc = useQueryClient();
  const indexing = useQuery({
    queryKey: ["index_status"],
    queryFn: api.indexingStatus,
    refetchInterval: (q) => (q.state.data ? 1000 : false),
  });
  const progress = useQuery<IndexProgress | null>({
    queryKey: ["index_progress"],
    queryFn: () => qc.getQueryData<IndexProgress>(["index_progress"]) ?? null,
    staleTime: Number.POSITIVE_INFINITY,
  });
  const reindex = useMutation({
    mutationFn: api.indexAll,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["index_status"] }),
  });
  // db_stats is the shared header cache (no extra fetch). `index_ran` is set by the
  // index:done listener in main.tsx, so it's true once a reindex has finished this session.
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  const ran = useQuery<boolean>({
    queryKey: ["index_ran"],
    queryFn: () => qc.getQueryData<boolean>(["index_ran"]) ?? false,
    staleTime: Number.POSITIVE_INFINITY,
  });

  // Set by the index:error listener in main.tsx. A failed scan also ends with an empty
  // index:done, so this is checked first: "no threads found" would be the wrong story.
  const indexError = useIndexError();

  const threads = stats.data?.threads ?? 0;
  const busy = !!indexing.data || reindex.isPending;
  const failed = !!indexError && !busy;
  // A reindex finished but the store is still empty → say so, instead of silently
  // re-offering the same "Index my history" button (which read as "nothing happened").
  const emptyAfterRun = !failed && !!ran.data && !busy && threads === 0;
  const ip = progress.data;
  const source = ip?.current ? (SOURCE_LABELS[ip.current as SourceKind] ?? ip.current) : "…";
  // Determinate % when we have an estimated total (re-index); indeterminate on first run.
  const pct = ip && ip.total > 0 ? Math.min(99, Math.round((ip.done / ip.total) * 100)) : null;

  const indexedLine = (
    <p className="text-[13px] text-muted-foreground">
      {threads.toLocaleString()} {threads === 1 ? "thread" : "threads"} indexed. New sessions are
      picked up as you work.
    </p>
  );
  if (threads > 0 && !busy && !failed) return indexedLine;
  // A reopened checklist on a populated index whose last reindex failed this session: what
  // is indexed still is, so this is not the "couldn't index anything" story.
  if (threads > 0 && failed) {
    return (
      <div className="space-y-2">
        {indexedLine}
        <p className="text-[13px] leading-normal text-muted-foreground">
          The last reindex stopped with an error. Everything indexed before it is still searchable.
          Try again, and if it keeps failing, the reason below says what to fix.
        </p>
        <p
          role="alert"
          className="break-words rounded-md bg-destructive/10 px-3 py-2 font-mono text-xs text-destructive"
        >
          {indexError}
        </p>
        <Button size="sm" variant="outline" onClick={() => reindex.mutate()}>
          Try again
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {failed ? (
        <div className="space-y-2">
          <p className="text-[13px] font-medium">Couldn't index your history</p>
          <p className="text-[13px] leading-normal text-muted-foreground">
            The scan stopped before it could index anything, so your history may well be there. Your
            agents' files were only read, never changed. Try again, and if it keeps failing, the
            reason below says what to fix.
          </p>
          <p
            role="alert"
            className="break-words rounded-md bg-destructive/10 px-3 py-2 font-mono text-xs text-destructive"
          >
            {indexError}
          </p>
        </div>
      ) : emptyAfterRun ? (
        <div className="space-y-1">
          <p className="text-[13px] font-medium">No threads found</p>
          <p className="text-[13px] leading-normal text-muted-foreground">
            Callimachus scanned this machine for Claude Code, Codex, Cursor, and 9 more agents but
            found no history to index. If you just used one, reindex to pick it up.
          </p>
        </div>
      ) : (
        !busy && (
          <p className="text-[13px] leading-normal text-muted-foreground">
            Builds one local index from Claude Code, Codex, Cursor, and 9 more agents. Read-only:
            your agents' files are never changed.
          </p>
        )
      )}

      {busy ? (
        <div className="space-y-2">
          <Progress value={pct} className="gap-1.5">
            <ProgressLabel className="text-xs font-normal text-muted-foreground">
              Indexing {source}
            </ProgressLabel>
            <span className="ml-auto text-xs text-muted-foreground tabular-nums">
              {(ip?.done ?? 0).toLocaleString()} scanned
            </span>
          </Progress>
          <p className="text-xs text-muted-foreground">
            Scanning Claude Code, Codex, Cursor, and 9 more. Read-only.
          </p>
        </div>
      ) : (
        <Button onClick={() => reindex.mutate()}>
          {failed ? "Try again" : emptyAfterRun ? "Reindex" : "Index my history"}
        </Button>
      )}
    </div>
  );
}

/** Agents with a hook system Callimachus can add memory hooks to (Cursor has none it can
 *  use: its sessionStart payload carries no working directory to pick the repo). */
const HOOK_AGENT_IDS: readonly string[] = ["codex", "gemini"] satisfies HookAgentId[];

const AFTER_CONNECT: Record<string, string> = {
  codex:
    "Codex runs a new hook only after you trust it: run /hooks in Codex and approve the Callimachus entries, then start a new session.",
  gemini: "New Gemini CLI sessions pick up the server and the hooks.",
  cursor: "Reload Cursor's MCP config (or restart it) to pick up the server.",
};

/** Step 2: Claude Code's own integration, and for each other agent this machine has, the
 *  MCP registration plus (Codex, Gemini CLI) its memory hooks, through the install commands
 *  Settings > Agents uses. */
function AgentsStep({
  recall,
  others,
  done,
  onSkip,
}: {
  recall: UseQueryResult<IntegrationStatus>;
  others: UseQueryResult<AgentIntegration[]>;
  done: boolean;
  onSkip?: () => void;
}) {
  const qc = useQueryClient();
  // The same cache Settings > Agents reads.
  const hooks = useQuery({ queryKey: ["agent_hooks"], queryFn: api.agentHooksStatus });
  const installClaude = useMutation({
    mutationFn: api.installRecallIntegration,
    onSuccess: (s) => qc.setQueryData(["recall_integration"], s),
    onSettled: () => qc.invalidateQueries({ queryKey: ["recall_integration"] }),
  });

  const s = recall.data;
  const managed = !!s?.pluginManaged;
  const connected = claudeConnected(s);
  const showClaude = claudeDetected(s);
  // Running from a disk image, App Translocation or an AppImage mount: install would
  // register a path that disappears, so the backend refuses with this message (on macOS it
  // asks to move the app to /Applications first). `=== false` so an older backend never
  // locks the button.
  const exeProblem = s?.exeStable === false ? s.exeProblem : null;
  const present = (others.data ?? []).filter((a) => a.present);
  const claudeAction = connected
    ? null
    : managed
      ? "Install the cal CLI"
      : s && (s.skillInstalled || s.mcpRegistered || s.hookInstalled)
        ? "Finish setup"
        : "Connect";
  const nothingFound = !showClaude && present.length === 0 && others.isSuccess;

  return (
    <div className="space-y-3">
      {nothingFound ? (
        <p className="text-[13px] leading-normal text-muted-foreground">
          No supported agent is set up on this machine yet. Install Claude Code, Codex, Cursor or
          Gemini CLI and come back here, or skip this step.
        </p>
      ) : (
        <ul className="divide-y divide-border rounded-lg border">
          {showClaude && (
            <AgentRow
              name="Claude Code"
              detail={
                managed
                  ? "Managed by the Claude Code plugin, which brings the skill, the MCP server and the hooks. Callimachus adds the cal CLI those hooks run."
                  : "The /recall skill, the MCP server, the session hooks and the cal CLI."
              }
              connected={connected}
              checking={recall.isPending}
              pending={installClaude.isPending}
              disabled={!!exeProblem}
              action={claudeAction}
              onConnect={() => installClaude.mutate()}
            >
              {recall.isError && (
                <p role="alert" className="text-[13px] text-destructive">
                  Couldn't read the Claude Code setup: {errorText(recall.error)}
                </p>
              )}
              {installClaude.isError && (
                <p role="alert" className="text-[13px] break-words text-destructive">
                  Couldn't connect Claude Code: {errorText(installClaude.error)}
                </p>
              )}
              {installClaude.isSuccess && connected && (
                <p className="text-[13px] text-muted-foreground">
                  {managed ? (
                    "The plugin's hooks can run cal from the next Claude Code session."
                  ) : (
                    <>
                      Restart Claude Code (or run <code>/mcp</code>) to pick up the server, then
                      type <code>/recall</code>.
                    </>
                  )}
                </p>
              )}
            </AgentRow>
          )}
          {present.map((a) => (
            <OtherAgentRow
              key={a.id}
              agent={a}
              hooks={hooks.data?.find((h) => h.agent === a.id)}
              hooksPending={HOOK_AGENT_IDS.includes(a.id) && hooks.isPending}
              disabled={!!exeProblem}
            />
          ))}
        </ul>
      )}
      {exeProblem && (
        <p role="alert" className="text-[13px] leading-normal text-destructive">
          {exeProblem}
        </p>
      )}
      {/* install_agent_integrations has no per-agent form, so say what a Connect does. */}
      {present.length > 1 && present.some((a) => !a.registered) && (
        <p className="text-xs text-muted-foreground">
          Connecting one of these registers the MCP server with all of them; memory hooks are added
          per agent.
        </p>
      )}
      {others.isError && (
        <p role="alert" className="text-[13px] text-destructive">
          Couldn't check for other agents: {errorText(others.error)}
        </p>
      )}
      {hooks.isError && (
        <p role="alert" className="text-[13px] text-destructive">
          Couldn't read the Codex and Gemini CLI hooks: {errorText(hooks.error)}
        </p>
      )}
      {done ? (
        <p className="text-[13px] text-muted-foreground">
          Proactive recall and the hook options are in Settings &gt; Agents.
        </p>
      ) : (
        <SkipButton onSkip={onSkip} />
      )}
    </div>
  );
}

/** Codex, Cursor or Gemini CLI: Connect registers the MCP server and, where the agent has a
 *  hook system Callimachus supports, adds the memory hooks that inject each repo's memory. */
function OtherAgentRow({
  agent,
  hooks,
  hooksPending,
  disabled,
}: {
  agent: AgentIntegration;
  hooks: AgentHooksStatus | undefined;
  hooksPending: boolean;
  disabled: boolean;
}) {
  const qc = useQueryClient();
  const hookId = HOOK_AGENT_IDS.includes(agent.id) ? (agent.id as HookAgentId) : null;
  const hooksWanted = !!hookId && !!hooks && !hooks.installed && !hooks.blocked;
  const connect = useMutation({
    mutationFn: async () => {
      if (!agent.registered) {
        const list = await api.installAgentIntegrations();
        qc.setQueryData(["agent_integrations"], list);
      }
      if (hookId && hooksWanted) {
        const row = await api.installAgentHooks(hookId);
        qc.setQueryData<AgentHooksStatus[]>(["agent_hooks"], (prev) =>
          prev?.map((r) => (r.agent === row.agent ? row : r)),
        );
      }
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ["agent_integrations"] });
      qc.invalidateQueries({ queryKey: ["agent_hooks"] });
    },
  });
  // Blocked hooks (turned off in the agent's own config) leave MCP as all a Connect can do.
  const connected = agent.registered && !hooksWanted;
  const action = connected ? null : agent.registered ? "Add memory hooks" : "Connect";
  const detail =
    !hookId || hooks?.blocked
      ? "The callimachus MCP server, so it can search this history."
      : "The callimachus MCP server, so it can search this history, and memory hooks that give each new session its repo's memory.";
  return (
    <AgentRow
      name={agent.label}
      detail={detail}
      connected={connected}
      checking={hooksPending}
      pending={connect.isPending}
      disabled={disabled}
      action={action}
      onConnect={() => connect.mutate()}
    >
      {hooks?.blocked && (
        <p className="text-[13px] leading-normal text-muted-foreground">
          No memory hooks: {hooks.blocked}. Settings &gt; Agents says how to turn them on.
        </p>
      )}
      {hooks?.installed && hooks.needsApproval && (
        <p className="text-[13px] leading-normal text-muted-foreground">
          Codex asks you to approve the hooks again in <code>/hooks</code> after an update or move.
        </p>
      )}
      {connect.isError && (
        <p role="alert" className="text-[13px] break-words text-destructive">
          Couldn't connect {agent.label}: {errorText(connect.error)}
        </p>
      )}
      {connect.isSuccess && connected && (
        <p className="text-[13px] leading-normal text-muted-foreground">
          {AFTER_CONNECT[agent.id] ??
            "Restart it (or reload its MCP config) to pick up the server."}
        </p>
      )}
    </AgentRow>
  );
}

function AgentRow({
  name,
  detail,
  connected,
  checking,
  pending,
  disabled,
  action,
  onConnect,
  children,
}: {
  name: string;
  detail: string;
  connected: boolean;
  checking: boolean;
  pending: boolean;
  disabled: boolean;
  action: string | null;
  onConnect: () => void;
  children?: ReactNode;
}) {
  return (
    <li className="space-y-2 px-3 py-2.5">
      <div className="flex items-center gap-3">
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium">{name}</p>
          <p className="text-[13px] leading-normal text-muted-foreground">{detail}</p>
        </div>
        {checking ? (
          <Spinner className="text-muted-foreground" />
        ) : connected ? (
          <span className="inline-flex shrink-0 items-center gap-1 text-xs text-muted-foreground">
            <Check className="size-3.5" aria-hidden="true" /> Connected
          </span>
        ) : (
          action && (
            <Button
              size="sm"
              className="shrink-0"
              // Several rows show the same "Connect", so the name says which agent.
              aria-label={action === "Connect" ? `Connect ${name}` : `${action} for ${name}`}
              onClick={onConnect}
              disabled={disabled || pending}
            >
              {pending && <Spinner data-icon="inline-start" />}
              {pending ? "Connecting…" : action}
            </Button>
          )
        )}
      </div>
      {children}
    </li>
  );
}

/** Step 3: the one-click memory consent, embedded (it names where transcripts go). */
function MemoryStep({ done, onSkip }: { done: boolean; onSkip?: () => void }) {
  const knowledge = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  if (knowledge.data?.enabled) {
    return (
      <p className="text-[13px] text-muted-foreground">
        Memory is on. Recent sessions are distilled in the background; change the engine in Settings
        &gt; AI engine.
      </p>
    );
  }
  return (
    <div className="space-y-2">
      {done && (
        <p className="text-[13px] text-muted-foreground">
          Memory has been turned off since. Turn it back on here, or leave it off.
        </p>
      )}
      <MemorySetupCard embedded onSkip={onSkip} />
    </div>
  );
}

/** Step 4: the semantic index build, which runs in the background. */
function SemanticStep({ indexed, onSkip }: { indexed: boolean; onSkip?: () => void }) {
  const embed = useEmbedStatus();
  const build = useBuildSemanticIndex();
  const error = useEmbedError();
  const e = embed.data;
  const running = !!e?.running || build.isPending;
  const pct = e && e.total > 0 ? Math.floor((e.done / e.total) * 100) : null;

  if (!indexed) {
    return (
      <div className="flex flex-wrap items-center gap-2">
        <p className="min-w-0 flex-1 text-[13px] text-muted-foreground">
          Index your history first; this step embeds what it finds.
        </p>
        <SkipButton onSkip={onSkip} />
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {!running && pct !== null && pct >= SEMANTIC_DONE_RATIO * 100 ? (
        <p className="text-[13px] leading-normal text-muted-foreground">
          {pct}% of messages are embedded. New sessions are embedded in the background as you work.
        </p>
      ) : running ? (
        <div className="space-y-2">
          <Progress value={pct} className="gap-1.5">
            <ProgressLabel className="text-xs font-normal text-muted-foreground">
              Building the semantic index
            </ProgressLabel>
            <span className="ml-auto text-xs text-muted-foreground tabular-nums">
              {pct === null ? "Starting…" : `${pct}% of messages`}
            </span>
          </Progress>
          <p className="text-xs text-muted-foreground">
            Runs in the background: carry on with the other steps or start searching.
          </p>
        </div>
      ) : (
        <p className="text-[13px] leading-normal text-muted-foreground">
          {/* embedding_status does not say whether the model is cached, so the download is
              tied to the first build: a build that got this far already has the model. */}
          {pct !== null && pct > 0
            ? `${pct}% of messages are embedded so far. Finishing embeds the rest on this machine in the background.`
            : "Embeds your messages on this machine in the background; the first build downloads a model of about 128 MB."}{" "}
          Keyword search works the whole time.
        </p>
      )}
      {error && !running && (
        <p role="alert" className="text-[13px] break-words text-destructive">
          The last build stopped: {error}. Building again picks up where it left off.
        </p>
      )}
      {build.isError && (
        <p role="alert" className="text-[13px] break-words text-destructive">
          Couldn't start the build: {errorText(build.error)}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2">
        {!running && (pct === null || pct < SEMANTIC_DONE_RATIO * 100) && (
          <Button size="sm" onClick={() => build.mutate()}>
            {error
              ? "Build again"
              : pct !== null && pct > 0
                ? "Finish the build"
                : "Build semantic index"}
          </Button>
        )}
        <SkipButton onSkip={onSkip} />
      </div>
    </div>
  );
}
