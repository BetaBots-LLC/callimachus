import { useId, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { api, type KnowledgeConfig } from "../../lib/api";
import { track } from "../../lib/telemetry";
import { errorText, notify } from "../../lib/toast";
import { InlineError, Note, SettingRow, SettingsGroup } from "./shared";

/** Base UI Select treats "" as "nothing selected" (placeholder styling, no label), so
 *  auto is a sentinel here and maps back to the stored "". */
const AUTO = "auto";
const DISTILL_ENGINES = [
  { id: AUTO, label: "First saved API key" },
  { id: "ollama", label: "Ollama (local · private)" },
  { id: "ollama_cloud", label: "Ollama Cloud" },
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "gemini", label: "Gemini" },
  { id: "openrouter", label: "OpenRouter" },
];

type KnowledgeDraft = { enabled: boolean; provider: string; model: string };

/** The memory part of Settings > AI engine (EngineSection): open TODOs and Remember are always
 *  on; the switch here is only the opt-in LLM distillation (local Ollama, a logged-in CLI or a
 *  cloud key), its engine, and its background auto-distill allowance. */
export function MemoryEngineSettings() {
  const queryClient = useQueryClient();
  const enabledId = useId();
  const engineId = useId();
  const modelId = useId();
  const autoId = useId();
  const capId = useId();

  const cfg = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  const cliEngines = useQuery({ queryKey: ["cli_engines"], queryFn: api.cliEngines });
  // Set when the engine picker changes, reported once the debounced save lands. Turning
  // knowledge on is reported by the backend (knowledge_enabled), not here.
  const engineChanged = useRef(false);
  const save = useMutation({
    mutationFn: (next: KnowledgeDraft) =>
      api.setKnowledgeConfig(next.enabled, next.provider || undefined, next.model || undefined),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["knowledge_config"] });
      // The memory card preselects the saved engine and model from memory_setup.
      queryClient.invalidateQueries({ queryKey: ["memory_setup"] });
      if (engineChanged.current) {
        engineChanged.current = false;
        track("setting_changed", { key: "knowledge_engine" });
      }
    },
    // The cache was written optimistically; reload it so the controls show what's stored.
    onError: () => queryClient.invalidateQueries({ queryKey: ["knowledge_config"] }),
  });
  // Debounce the actual write (toggling ON kicks a background backfill) so rapid flips
  // coalesce into one; the cache is updated optimistically so the UI stays instant.
  // Sections unmount on every nav switch, and the default unmount behavior cancels pending
  // work, so flush instead: a Model typed just before switching sections still saves.
  const debouncedSave = useDebouncedCallback((next: KnowledgeDraft) => save.mutate(next), {
    wait: 400,
    onUnmount: (d) => d.flush(),
  });

  const c = cfg.data;
  const enabled = c?.enabled ?? false;
  const provider = c?.provider ?? "";
  const model = c?.model ?? "";
  const update = (next: Partial<KnowledgeDraft>) => {
    const merged = { enabled, provider, model, ...next };
    if (merged.provider !== provider) engineChanged.current = true;
    queryClient.setQueryData<KnowledgeConfig>(["knowledge_config"], {
      enabled: merged.enabled,
      provider: merged.provider || null,
      model: merged.model || null,
      autoDistill: c?.autoDistill ?? false,
    });
    debouncedSave(merged);
  };
  // The engine the switch picked for the user (label), shown until they pick another.
  const [autoPicked, setAutoPicked] = useState<{ id: string; label: string } | null>(null);
  /** Turning distillation on with "First saved API key" and no key saved would make every
   *  distill fail ("none is saved"). So a fresh enable with that sentinel takes the engine
   *  the memory card recommends instead, and says so. Only on the user's own switch flip:
   *  an existing config is never switched behind their back. */
  const toggle = (on: boolean) => {
    setAutoPicked(null);
    update({ enabled: on });
    if (!on || provider) return;
    queryClient
      .fetchQuery({ queryKey: ["memory_setup"], queryFn: api.memorySetup })
      .then((setup) => {
        const now = queryClient.getQueryData<KnowledgeConfig>(["knowledge_config"]);
        // Turned off again, or an engine was picked while the probe ran.
        if (!now?.enabled || now.provider) return;
        if (setup.engines.some((e) => e.kind === "key" && e.detected)) return;
        const rec = setup.engines.find((e) => e.id === setup.recommended && e.detected);
        if (!rec) return;
        update({ enabled: true, provider: rec.id });
        // Not the user's pick, so not reported as an engine change.
        engineChanged.current = false;
        setAutoPicked({ id: rec.id, label: rec.label });
      })
      .catch(() => {
        // The probe failed: keep the sentinel; the engine picker below still works.
      });
  };
  const local = provider === "ollama";
  // CLI backends (claude-cli / codex-cli) are keyless like Ollama, but the text DOES leave the
  // machine (to the CLI's provider), so they're "no key" without Ollama's privacy claim.
  const isCli = provider.endsWith("-cli");
  const keyless = local || isCli;
  // Installed CLI backends first: a logged-in subscription is the keyless default the
  // memory card recommends, so the picker leads with the same thing.
  const installedClis = (cliEngines.data ?? []).filter((e) => e.installed);
  const engineOptions = [
    ...installedClis.map((e) => ({ id: e.id, label: `${e.label} (detected)` })),
    ...DISTILL_ENGINES,
  ];
  // A saved CLI engine that is no longer on PATH still has to show its label.
  if (provider.endsWith("-cli") && !engineOptions.some((e) => e.id === provider)) {
    const known = cliEngines.data?.find((e) => e.id === provider);
    engineOptions.unshift({ id: provider, label: `${known?.label ?? provider} (not found)` });
  }
  // Base UI's SelectValue renders the matching item's label from `items`.
  const engineItems = engineOptions.map((e) => ({ value: e.id, label: e.label }));
  const autoDistill = c?.autoDistill ?? false;
  // Refreshed on distill:done (main.tsx), when an auto-distill round spends the allowance.
  const budget = useQuery({
    queryKey: ["auto_distill_budget"],
    queryFn: api.autoDistillBudget,
    enabled,
  });
  const setCap = useMutation({
    mutationFn: (cap: number) => api.setAutoDistillDailyCap(cap),
    onSuccess: (next) => queryClient.setQueryData(["auto_distill_budget"], next),
    onError: (e) => notify.error("Couldn't save the daily limit", { description: errorText(e) }),
  });
  const b = budget.data;
  const toggleAuto = useMutation({
    mutationFn: (on: boolean) => api.setAutoDistill(on),
    onMutate: (on: boolean) =>
      queryClient.setQueryData<KnowledgeConfig>(["knowledge_config"], (prev) =>
        prev ? { ...prev, autoDistill: on } : prev,
      ),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["knowledge_config"] });
      track("setting_changed", { key: "auto_distill" });
    },
    onError: (e) => {
      // Undo the optimistic flip.
      queryClient.invalidateQueries({ queryKey: ["knowledge_config"] });
      notify.error("Couldn't change auto-distill", { description: errorText(e) });
    },
  });

  const privacy = local
    ? "Runs locally via Ollama, so thread text never leaves your machine."
    : isCli
      ? "Runs through your logged-in CLI (billed to that login), so no API key is needed. Thread text is sent to that CLI's provider, on demand per thread."
      : provider
        ? `Sends thread text to ${provider} using your stored API key, on demand per thread.`
        : "Uses the first provider you've added a key for. Sends thread text to that provider, on demand per thread.";

  return (
    <div className="space-y-8">
      <SettingsGroup title="Memory">
        <SettingRow
          htmlFor={enabledId}
          title="LLM distillation"
          description="An LLM reads your sessions and keeps each repo's decisions, gotchas and summaries for Ask, recall, Projects and your agents' hooks. Open TODOs and Remember work with it off."
          control={
            <Switch
              id={enabledId}
              checked={enabled}
              disabled={cfg.isPending}
              onCheckedChange={toggle}
            />
          }
        >
          {enabled && autoPicked && provider === autoPicked.id && (
            <Note>
              No API key is saved, so distillation uses {autoPicked.label}, the engine this machine
              has. Pick another under Memory engine.
            </Note>
          )}
          {cfg.isError && (
            <InlineError>Couldn't load the memory settings: {errorText(cfg.error)}</InlineError>
          )}
          {save.isError && (
            <InlineError>Couldn't save the memory settings: {errorText(save.error)}</InlineError>
          )}
        </SettingRow>
      </SettingsGroup>

      {enabled && (
        <SettingsGroup
          title="Memory engine"
          description="The LLM that reads your sessions and answers Ask. Distilled threads go only to the engine picked here."
        >
          {/* No title: the group names this row, and the two fields carry their labels. */}
          <SettingRow>
            <div className="grid grid-cols-2 gap-4">
              <Field>
                <FieldLabel htmlFor={engineId}>Engine</FieldLabel>
                <Select
                  items={engineItems}
                  value={provider || AUTO}
                  onValueChange={(v) => update({ provider: !v || v === AUTO ? "" : v })}
                >
                  <SelectTrigger id={engineId} className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {engineOptions.map((e) => (
                      <SelectItem key={e.id} value={e.id}>
                        {e.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
              <Field>
                <FieldLabel htmlFor={modelId}>{keyless ? "Model" : "Model (optional)"}</FieldLabel>
                <Input
                  id={modelId}
                  key={model}
                  defaultValue={model}
                  onBlur={(e) => {
                    if (e.target.value !== model) update({ model: e.target.value });
                  }}
                  placeholder={
                    local ? "llama3.1" : isCli ? "default for CLI" : "default for engine"
                  }
                  spellCheck={false}
                />
              </Field>
            </div>
            <Note>{privacy}</Note>
          </SettingRow>
        </SettingsGroup>
      )}

      {enabled && (
        <SettingsGroup title="Auto-distill">
          <SettingRow
            htmlFor={autoId}
            title="Auto-distill in the background"
            description="Distill new and changed threads as they're indexed, so Ask, recall and the Project page stay current. Paced, cancellable, and yields to indexing."
            control={
              <Switch
                id={autoId}
                checked={autoDistill}
                disabled={toggleAuto.isPending}
                onCheckedChange={(v) => toggleAuto.mutate(v)}
              />
            }
          >
            {budget.isPending && <Note>Loading the daily limit…</Note>}
            {budget.isError && (
              <InlineError>Couldn't load the daily limit: {errorText(budget.error)}</InlineError>
            )}
            {b && (
              <div className="flex items-center gap-2 text-[13px] text-muted-foreground">
                <label htmlFor={capId}>Up to</label>
                <Input
                  id={capId}
                  // Keyed on the saved cap so a save (or a rejected entry) resets the field.
                  key={b.dailyCap}
                  type="number"
                  min={0}
                  step={1}
                  inputMode="numeric"
                  defaultValue={b.dailyCap}
                  onBlur={(e) => {
                    const cap = Number.parseInt(e.target.value, 10);
                    if (Number.isFinite(cap) && cap >= 0 && cap !== b.dailyCap) setCap.mutate(cap);
                    else e.target.value = String(b.dailyCap);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") e.currentTarget.blur();
                  }}
                  disabled={setCap.isPending}
                  className="w-14 text-right tabular-nums"
                />
                <span>threads a day</span>
              </div>
            )}
            {b && (
              <Note>
                {b.dailyCap === 0
                  ? "A daily limit of 0 pauses auto-distill."
                  : autoDistill
                    ? `${b.remainingToday} of ${b.dailyCap} left today (resets at midnight UTC) · ${b.eligibleThreads} threads from the last ${b.recentDays} days still to distill.`
                    : `${b.eligibleThreads} threads from the last ${b.recentDays} days would be distilled, up to ${b.dailyCap} a day.`}
              </Note>
            )}
            {setCap.isError && (
              <InlineError>Couldn't save the daily limit: {errorText(setCap.error)}</InlineError>
            )}
          </SettingRow>
        </SettingsGroup>
      )}
    </div>
  );
}
