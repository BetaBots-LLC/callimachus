import { useId, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { create } from "zustand";
import { Sparkles } from "lucide-react";
import { api, PROVIDERS, type MemoryEngine } from "../lib/api";
import { errorText, notify } from "../lib/toast";
import { useUi } from "../store/ui";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { cn } from "@/lib/utils";

/** "Not now" lasts for this app session only: the card comes back on the next launch, since
 *  memory stays empty for almost every repo until distillation is on. */
const useCardDismissed = create<{ dismissed: boolean; set: (d: boolean) => void }>((set) => ({
  dismissed: false,
  set: (dismissed) => set({ dismissed }),
}));

/** The Ollama server an engine entry probed. `memory_setup` writes the saved base URL into
 *  the detail ("Running at <base>, 2 models" / "Not running at <base>"), and that base can
 *  be another host (a GPU box on the LAN), so the card reads it back instead of assuming
 *  localhost. */
function ollamaServer(detail: string): { base: string; host: string; local: boolean } | null {
  const base = /\bat (\S+?)(?:,|$)/.exec(detail)?.[1];
  if (!base) return null;
  // A base saved without a scheme ("gpu-box:11434") parses as scheme "gpu-box:" with no
  // host, so retry it as http.
  const hostOf = (u: string) => {
    try {
      return new URL(u).hostname;
    } catch {
      return "";
    }
  };
  const host = hostOf(base) || hostOf(`http://${base}`);
  if (!host) return null;
  const bare = host.replace(/^\[|\]$/g, "");
  const local =
    bare === "localhost" ||
    bare.endsWith(".localhost") ||
    bare === "::1" ||
    bare === "0.0.0.0" ||
    /^127\.\d+\.\d+\.\d+$/.test(bare);
  return { base, host: bare, local };
}

/** Where an engine sends the transcripts, in the words the consent rests on. The privacy
 *  promise is that conversation content leaves the machine only for a provider the user
 *  picked, so every option names that provider before the button does anything. The CLI
 *  lines make no billing claim: either CLI can be signed in with an API key instead of a
 *  subscription, so the bill goes to whatever that login is. */
function consentFor(e: MemoryEngine): string {
  if (e.id === "claude-cli")
    return "Sends session transcripts to Anthropic through your logged-in claude CLI (billed to that login).";
  if (e.id === "codex-cli")
    return "Sends session transcripts to OpenAI through your logged-in codex CLI (billed to that login).";
  if (e.kind === "ollama") {
    const server = ollamaServer(e.detail);
    if (server?.local) return "Stays on this machine.";
    return server
      ? `Sends session transcripts to the Ollama server at ${server.host}.`
      : "Sends session transcripts to your Ollama server.";
  }
  if (e.kind === "key")
    return `Sends session transcripts to ${providerName(e)} with your saved API key.`;
  return `Sends session transcripts to the provider behind your logged-in ${e.label}.`;
}

/** The engine's name as the card shows it. The backend labels its one Ollama entry
 *  "Ollama (local)" even when the saved base points at another host, so that suffix goes
 *  when the server is not on this machine. */
function engineLabel(e: MemoryEngine): string {
  if (e.kind !== "ollama" || ollamaServer(e.detail)?.local) return e.label;
  return e.label.replace(/\s*\(local\)$/, "");
}

function providerName(e: MemoryEngine): string {
  return PROVIDERS.find((p) => p.id === e.id)?.label ?? e.label;
}

/**
 * The one-click "turn on memory" consent: the engines this machine already has, as a
 * radio list with the saved engine (else the recommended one) preselected, each naming
 * where transcripts go. It replaces the old "Enable in Settings" teaser on Ask and sits
 * inline on Projects and the Knowledge tab's recall modes while LLM distillation is off.
 *
 * `whenDismissed`: "hide" removes it after Not now; "button" leaves a small button that
 * brings it back (for a view that has nothing else to show while memory is off).
 */
export function MemorySetupCard({
  whenDismissed = "hide",
  className,
}: {
  whenDismissed?: "hide" | "button";
  className?: string;
}) {
  const qc = useQueryClient();
  const openSettings = useUi((s) => s.openSettings);
  const dismissed = useCardDismissed((s) => s.dismissed);
  const setDismissed = useCardDismissed((s) => s.set);
  const name = useId();
  const setup = useQuery({ queryKey: ["memory_setup"], queryFn: api.memorySetup });
  // The hosts gate on knowledge_config, so the card hides on the same source: a cached
  // memory_setup can lag a Settings toggle by a refetch.
  const cfg = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  // null = the saved engine, else the recommended one; set once the user picks another.
  const [picked, setPicked] = useState<string | null>(null);
  const [pickedModel, setPickedModel] = useState<string | null>(null);

  const s = setup.data;
  const detected = (s?.engines ?? []).filter((e) => e.detected);
  // A config turned off later keeps its provider and model, so turning memory back on
  // offers the same engine instead of switching it to the recommended one.
  const chosen =
    detected.find((e) => e.id === picked) ??
    detected.find((e) => e.id === s?.provider) ??
    detected.find((e) => e.id === s?.recommended) ??
    detected[0] ??
    null;
  const ollama = chosen?.kind === "ollama" ? ollamaServer(chosen.detail) : null;
  // Ollama has no default every server carries: with no model sent the backend falls back to
  // llama3.1, and on a server without it every auto-distill fails and spends the daily cap.
  // So the card lists what the server has pulled and sends one of those.
  const ollamaModels = useQuery({
    queryKey: ["ollama_models", ollama?.base ?? null],
    queryFn: () => api.listModels("ollama", ollama?.base),
    enabled: chosen?.kind === "ollama",
  });
  const pulled = ollamaModels.data ?? [];
  const savedModel = chosen && chosen.id === s?.provider ? (s?.model ?? null) : null;
  const ollamaModel =
    (pickedModel && pulled.includes(pickedModel) ? pickedModel : null) ??
    (savedModel && pulled.includes(savedModel) ? savedModel : null) ??
    pulled[0] ??
    null;
  const model = chosen?.kind === "ollama" ? ollamaModel : savedModel;
  const ollamaBlocked = chosen?.kind === "ollama" && !ollamaModel;

  const enable = useMutation({
    mutationFn: (v: { engine: MemoryEngine; model: string | null }) =>
      api.enableMemory(v.engine.id, v.model),
    onSuccess: (next, { engine }) => {
      qc.setQueryData(["knowledge_config"], next);
      for (const key of [
        "knowledge_config",
        "memory_setup",
        "auto_distill_budget",
        "distill_status",
        "projects",
        "project_memory",
      ])
        void qc.invalidateQueries({ queryKey: [key] });
      notify.success("Memory is on", {
        description: `Distilling recent sessions with ${engineLabel(engine)} in the background. Change the engine in Settings > Knowledge.`,
      });
    },
  });

  if (cfg.data?.enabled) return null;
  if (dismissed) {
    return whenDismissed === "button" ? (
      <Button size="sm" variant="outline" onClick={() => setDismissed(false)}>
        <Sparkles /> Set up memory
      </Button>
    ) : null;
  }

  // Turning on also turns auto-distill on and kicks the drain, so the card says the work
  // starts now, not only how much qualifies.
  const qualifying =
    s && chosen
      ? `${
          s.eligibleThreads === 0
            ? "No sessions from the last 30 days are waiting."
            : s.eligibleThreads === 1
              ? "1 session from the last 30 days qualifies."
              : `${s.eligibleThreads.toLocaleString()} sessions from the last 30 days qualify.`
        } Turning on starts distilling ${
          s.eligibleThreads === 0 ? "new sessions" : s.eligibleThreads === 1 ? "it" : "them"
        } in the background right away with ${engineLabel(chosen)}, up to ${s.dailyCap} a day.`
      : null;

  return (
    <section
      aria-label="Turn on memory"
      className={cn("space-y-3 rounded-xl border bg-card p-4 text-left text-sm", className)}
    >
      <div className="space-y-1">
        <h3 className="flex items-center gap-1.5 font-semibold">
          <Sparkles className="size-4 text-muted-foreground" aria-hidden="true" />
          Turn on memory
        </h3>
        <p className="text-[13px] leading-normal text-muted-foreground">
          An LLM reads your past sessions and keeps each repo's decisions, gotchas and summaries, so
          Ask, recall, Projects and your agents' session hooks have something to say. TODOs and
          Remember work without it.
        </p>
      </div>

      {setup.isPending ? (
        <p className="flex items-center gap-2 text-[13px] text-muted-foreground">
          <Spinner /> Checking which engines this machine has…
        </p>
      ) : setup.isError ? (
        <p role="alert" className="text-[13px] text-destructive">
          Couldn't check for engines: {errorText(setup.error)}. Pick one in Settings &gt; Knowledge
          instead.
        </p>
      ) : detected.length === 0 ? (
        <div className="space-y-2 text-[13px] leading-normal text-muted-foreground">
          <p>No engine found on this machine yet. Any one of these works:</p>
          <ul className="list-disc space-y-1 pl-5">
            <li>
              Sign in to the <code>claude</code> or <code>codex</code> CLI.
            </li>
            <li>Run Ollama locally, so transcripts stay on this machine.</li>
            <li>Save an API key for a provider in Settings &gt; Providers.</li>
          </ul>
        </div>
      ) : (
        <fieldset className="space-y-1.5">
          <legend className="sr-only">Engine</legend>
          {detected.map((e) => {
            const on = chosen?.id === e.id;
            return (
              <label
                key={e.id}
                className={cn(
                  "relative flex cursor-pointer items-start gap-2.5 rounded-lg border px-3 py-2 transition-colors",
                  on ? "border-ring/60 bg-muted/50" : "hover:bg-muted/30",
                )}
              >
                {/* A drawn indicator, not accent-primary: in dark mode the native radio drew
                    unchecked options as solid grey discs and the checked one as a ring, so the
                    selection read inverted. */}
                <input
                  type="radio"
                  name={name}
                  value={e.id}
                  checked={on}
                  onChange={() => setPicked(e.id)}
                  className="peer sr-only"
                />
                <span
                  aria-hidden="true"
                  className={cn(
                    "mt-0.5 grid size-4 shrink-0 place-items-center rounded-full border transition-colors peer-focus-visible:ring-3 peer-focus-visible:ring-ring/50",
                    on ? "border-primary" : "border-muted-foreground/50",
                  )}
                >
                  {on && <span className="size-2 rounded-full bg-primary" />}
                </span>
                <span className="min-w-0 flex-1 space-y-0.5">
                  <span className="flex flex-wrap items-baseline gap-x-2">
                    <span className="font-medium">{engineLabel(e)}</span>
                    {e.id === s?.recommended && (
                      <span className="text-[11px] text-muted-foreground">recommended</span>
                    )}
                    <span className="min-w-0 truncate font-mono text-[11px] text-muted-foreground">
                      {e.detail}
                    </span>
                  </span>
                  <span className="block text-[13px] leading-normal text-muted-foreground">
                    {consentFor(e)}
                  </span>
                </span>
              </label>
            );
          })}
        </fieldset>
      )}

      {chosen?.kind === "ollama" && (
        <OllamaModelPicker
          models={pulled}
          value={ollamaModel}
          onChange={setPickedModel}
          pending={ollamaModels.isPending}
          error={ollamaModels.isError ? errorText(ollamaModels.error) : null}
          host={ollama?.host ?? null}
        />
      )}

      {qualifying && detected.length > 0 && !ollamaBlocked && (
        <p className="text-[13px] text-muted-foreground">{qualifying}</p>
      )}
      {enable.isError && (
        <p role="alert" className="text-[13px] break-words text-destructive">
          Couldn't turn on memory: {errorText(enable.error)}. Try again, or pick another engine.
        </p>
      )}

      <div className="flex flex-wrap items-center gap-2">
        {detected.length > 0 ? (
          <Button
            size="sm"
            onClick={() => chosen && enable.mutate({ engine: chosen, model })}
            disabled={!chosen || ollamaBlocked || enable.isPending}
          >
            {enable.isPending && <Spinner data-icon="inline-start" />}
            {enable.isPending ? "Turning on…" : "Turn on memory"}
          </Button>
        ) : setup.isError ? (
          <Button size="sm" onClick={() => openSettings("knowledge")}>
            Open Knowledge settings
          </Button>
        ) : (
          !setup.isPending && (
            <Button size="sm" onClick={() => openSettings("providers")}>
              Open Providers
            </Button>
          )
        )}
        {((setup.isError || detected.length === 0) && !setup.isPending) ||
        (ollamaBlocked && !ollamaModels.isPending) ? (
          <Button
            size="sm"
            variant="outline"
            onClick={() => {
              void setup.refetch();
              if (chosen?.kind === "ollama") void ollamaModels.refetch();
            }}
          >
            Check again
          </Button>
        ) : null}
        <Button
          size="sm"
          variant="ghost"
          onClick={() => setDismissed(true)}
          disabled={enable.isPending}
        >
          Not now
        </Button>
      </div>
    </section>
  );
}

/** The Ollama model the card sends: the server's pulled models, or why there are none. */
function OllamaModelPicker({
  models,
  value,
  onChange,
  pending,
  error,
  host,
}: {
  models: string[];
  value: string | null;
  onChange: (m: string) => void;
  pending: boolean;
  error: string | null;
  host: string | null;
}) {
  const id = useId();
  const where = host ? `the Ollama server at ${host}` : "your Ollama server";
  if (pending)
    return (
      <p className="flex items-center gap-2 text-[13px] text-muted-foreground">
        <Spinner /> Listing the models on {where}…
      </p>
    );
  if (error)
    return (
      <p role="alert" className="text-[13px] break-words text-destructive">
        Couldn't list the models on {where}: {error}. Check that Ollama is running, then Check
        again.
      </p>
    );
  if (models.length === 0)
    return (
      <p className="text-[13px] leading-normal text-muted-foreground">
        No models are pulled on {where} yet. Pull one with <code>ollama pull llama3.1</code>, then
        Check again.
      </p>
    );
  return (
    <div className="flex items-center gap-2 text-[13px]">
      <label htmlFor={id} className="text-muted-foreground">
        Model
      </label>
      <Select
        items={models.map((m) => ({ value: m, label: m }))}
        value={value}
        onValueChange={(v) => v && onChange(v)}
      >
        <SelectTrigger id={id} size="sm" className="min-w-40 font-mono text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {models.map((m) => (
            <SelectItem key={m} value={m} className="font-mono text-xs">
              {m}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
