import type { ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import {
  api,
  INDEXABLE_SOURCES,
  type KnowledgeConfig,
  PROVIDERS,
  SOURCE_LABELS,
  type SourceKind,
} from "../lib/api";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { useAppForm } from "@/lib/form";
import { useSettings } from "../store/settings";
import { CleanupCard } from "./CleanupCard";

const INDEXABLE: SourceKind[] = INDEXABLE_SOURCES;

export function SettingsView() {
  const queryClient = useQueryClient();
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  // Progress is pushed via embed:progress/embed:done events (see main.tsx); a slow
  // safety-net refetch covers any missed event while a build runs.
  const embed = useQuery({
    queryKey: ["embed_status"],
    queryFn: api.embeddingStatus,
    refetchInterval: (q) => (q.state.data?.running ? 5_000 : false),
  });

  const indexOne = useMutation({
    mutationFn: (kind: SourceKind) => api.indexSource(kind),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["db_stats"] });
      queryClient.invalidateQueries({ queryKey: ["results"] });
    },
  });
  const buildIndex = useMutation({ mutationFn: api.buildEmbeddings });
  const vaultDir = useSettings((s) => s.vaultDir);
  const setVaultDir = useSettings((s) => s.setVaultDir);
  const synthProvider = useSettings((s) => s.synthProvider);
  const setSynthProvider = useSettings((s) => s.setSynthProvider);
  const synthModel = useSettings((s) => s.synthModel);
  const setSynthModel = useSettings((s) => s.setSynthModel);
  const vaults = useQuery({ queryKey: ["obsidian_vaults"], queryFn: api.obsidianVaults });

  return (
    <div className="mx-auto w-full max-w-3xl space-y-6 overflow-y-auto p-6">
      <Card>
        <CardHeader>
          <CardTitle>Providers &amp; keys</CardTitle>
          <p className="text-sm text-muted-foreground">
            Keys are stored in your OS keychain, never on disk.
          </p>
        </CardHeader>
        <CardContent className="space-y-2">
          {PROVIDERS.map((p) => (
            <ProviderRow key={p.id} id={p.id} label={p.label} />
          ))}
        </CardContent>
      </Card>

      <RecallIntegrationCard />

      <Card>
        <CardHeader>
          <CardTitle>Sources</CardTitle>
          <p className="text-sm text-muted-foreground">
            {stats.data
              ? `${stats.data.threads.toLocaleString()} threads · ${stats.data.messages.toLocaleString()} messages indexed`
              : ""}
          </p>
        </CardHeader>
        <CardContent className="space-y-2">
          {INDEXABLE.map((k) => (
            <div key={k} className="flex items-center justify-between border-b pb-2 last:border-0">
              <span className="text-sm">{SOURCE_LABELS[k]}</span>
              <Button
                size="sm"
                variant="outline"
                onClick={() => indexOne.mutate(k)}
                disabled={indexOne.isPending && indexOne.variables === k}
              >
                {indexOne.isPending && indexOne.variables === k ? "Indexing…" : "Reindex"}
              </Button>
            </div>
          ))}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Semantic index</CardTitle>
          <p className="text-sm text-muted-foreground">
            {embed.data
              ? `${embed.data.done.toLocaleString()} / ${embed.data.total.toLocaleString()} messages embedded`
              : ""}
          </p>
        </CardHeader>
        <CardContent>
          <Button
            size="sm"
            variant="outline"
            onClick={() => buildIndex.mutate()}
            disabled={embed.data?.running}
          >
            {embed.data?.running ? "Building…" : "Build / update semantic index"}
          </Button>
        </CardContent>
      </Card>

      <DistillationCard />

      <Card>
        <CardHeader>
          <CardTitle>Obsidian export</CardTitle>
          <p className="text-sm text-muted-foreground">
            Vault folder that “Export to Obsidian” writes into, and which LLM does the synthesized
            version.
          </p>
        </CardHeader>
        <CardContent className="space-y-4">
          {vaults.data && vaults.data.length > 0 && (
            <Field label="Detected vaults">
              <div className="flex flex-wrap gap-1.5">
                {vaults.data.map((path) => {
                  const name = path.split("/").filter(Boolean).pop() ?? path;
                  return (
                    <Button
                      key={path}
                      size="xs"
                      variant={vaultDir === path ? "secondary" : "outline"}
                      onClick={() => setVaultDir(path)}
                      title={path}
                    >
                      {name}
                    </Button>
                  );
                })}
              </div>
            </Field>
          )}
          <Field label="Vault folder">
            <Input
              value={vaultDir}
              onChange={(e) => setVaultDir(e.target.value)}
              placeholder="/Users/you/Documents/Obsidian Vault/Vault"
              spellCheck={false}
            />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Synthesis provider">
              <select
                value={synthProvider}
                onChange={(e) => setSynthProvider(e.target.value)}
                className="h-8 w-full rounded-lg border border-input bg-transparent px-2.5 text-sm text-foreground outline-none transition-colors focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 dark:bg-input/30"
              >
                <option value="">Auto (first key)</option>
                {PROVIDERS.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.label}
                  </option>
                ))}
              </select>
            </Field>
            <Field label={synthProvider ? "Model" : "Model (auto)"}>
              <Input
                value={synthModel}
                onChange={(e) => setSynthModel(e.target.value)}
                placeholder="default for provider"
                disabled={!synthProvider}
                spellCheck={false}
              />
            </Field>
          </div>
          <p className="text-xs text-muted-foreground">
            Auto uses the first provider you've added a key for, at its cheapest model.
          </p>
        </CardContent>
      </Card>

      <CleanupCard />
    </div>
  );
}

/** One-click Claude Code integration: install the /recall skill + register this
 *  app as the `callimachus` MCP server, no terminal or cargo. */
function RecallIntegrationCard() {
  const queryClient = useQueryClient();
  const status = useQuery({
    queryKey: ["recall_integration"],
    queryFn: api.recallIntegrationStatus,
  });
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["recall_integration"] });
  const install = useMutation({ mutationFn: api.installRecallIntegration, onSuccess: refresh });
  const uninstall = useMutation({ mutationFn: api.uninstallRecallIntegration, onSuccess: refresh });

  const s = status.data;
  const connected = !!s && s.skillInstalled && s.mcpRegistered && !s.skillOutdated;
  const partial = !!s && (s.skillInstalled || s.mcpRegistered) && !connected;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          Claude Code
          {connected && (
            <span className="rounded-full bg-emerald-500/15 px-2 py-0.5 text-[0.6rem] font-medium uppercase tracking-wide text-emerald-600 dark:text-emerald-400">
              connected
            </span>
          )}
        </CardTitle>
        <p className="text-sm text-muted-foreground">
          Let Claude Code (and other agents) search your history. Installs the <code>/recall</code>{" "}
          skill, registers Callimachus as an MCP server, and adds the <code>cal</code> CLI (used by
          the VS Code extension) — no terminal, no setup.
        </p>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span>
            {s?.skillInstalled ? (s.skillOutdated ? "⚠ skill outdated" : "✓ skill") : "○ skill"}
          </span>
          <span>{s?.mcpRegistered ? "✓ MCP server" : "○ MCP server"}</span>
          <span>{s?.calInstalled ? "✓ cal CLI" : "○ cal CLI"}</span>
        </div>
        <div className="flex items-center gap-2">
          <Button size="sm" onClick={() => install.mutate()} disabled={install.isPending}>
            {install.isPending
              ? "Installing…"
              : connected
                ? "Reinstall"
                : partial
                  ? "Finish setup"
                  : "Enable for Claude Code"}
          </Button>
          {(s?.skillInstalled || s?.mcpRegistered) && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => uninstall.mutate()}
              disabled={uninstall.isPending}
            >
              Remove
            </Button>
          )}
        </div>
        {install.isSuccess && (
          <p className="text-xs text-muted-foreground">
            Done. Restart Claude Code (or run <code>/mcp</code>) to pick up the server, then type{" "}
            <code>/recall</code>.
          </p>
        )}
        {install.isError && <p className="text-xs text-destructive">{String(install.error)}</p>}
      </CardContent>
    </Card>
  );
}

/** A uniform labeled field: muted label above a control. */
function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-1.5">
      <span className="block text-xs font-medium text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}

const DISTILL_ENGINES = [
  { id: "", label: "Auto (first API key)" },
  { id: "ollama", label: "Ollama (local · private)" },
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "gemini", label: "Gemini" },
  { id: "openrouter", label: "OpenRouter" },
];

const SELECT_CLASS =
  "h-8 w-full rounded-lg border border-input bg-transparent px-2.5 text-sm text-foreground outline-none transition-colors focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 dark:bg-input/30";

/** Opt-in LLM distillation: enable + pick engine (local Ollama or a cloud key). */
function DistillationCard() {
  const queryClient = useQueryClient();
  const cfg = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  const save = useMutation({
    mutationFn: (next: { enabled: boolean; provider: string; model: string }) =>
      api.setKnowledgeConfig(next.enabled, next.provider || undefined, next.model || undefined),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["knowledge_config"] }),
  });
  // Debounce the actual write (toggling ON kicks a background backfill) so rapid flips
  // coalesce into one; the cache is updated optimistically so the UI stays instant.
  const debouncedSave = useDebouncedCallback(
    (next: { enabled: boolean; provider: string; model: string }) => save.mutate(next),
    { wait: 400 },
  );

  const c = cfg.data;
  const enabled = c?.enabled ?? false;
  const provider = c?.provider ?? "";
  const model = c?.model ?? "";
  const update = (next: Partial<{ enabled: boolean; provider: string; model: string }>) => {
    const merged = { enabled, provider, model, ...next };
    queryClient.setQueryData<KnowledgeConfig>(["knowledge_config"], {
      enabled: merged.enabled,
      provider: merged.provider || null,
      model: merged.model || null,
      autoDistill: c?.autoDistill ?? false,
    });
    debouncedSave(merged);
  };
  const local = provider === "ollama";
  const autoDistill = c?.autoDistill ?? false;
  const toggleAuto = useMutation({
    mutationFn: (on: boolean) => api.setAutoDistill(on),
    onMutate: (on: boolean) =>
      queryClient.setQueryData<KnowledgeConfig>(["knowledge_config"], (prev) =>
        prev ? { ...prev, autoDistill: on } : prev,
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["knowledge_config"] }),
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Knowledge</CardTitle>
        <p className="text-sm text-muted-foreground">
          Surface what matters from your history. When on, Callimachus extracts open TODOs from your
          threads (free, on-device) and adds a Todos tab. Optionally distill decisions, gotchas
          &amp; summaries with an LLM — local (Ollama) or your own API key. Off by default.
        </p>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex items-center gap-2 text-sm">
          <Switch checked={enabled} onCheckedChange={(v) => update({ enabled: v })} />
          Enable knowledge
        </div>
        {enabled && (
          <>
            <p className="text-xs text-muted-foreground">
              Distillation engine (optional — TODOs need none). Pick one to distill decisions &amp;
              gotchas per thread.
            </p>
            <div className="grid grid-cols-2 gap-3">
              <Field label="Engine">
                <select
                  value={provider}
                  onChange={(e) => update({ provider: e.target.value })}
                  className={SELECT_CLASS}
                >
                  {DISTILL_ENGINES.map((e) => (
                    <option key={e.id} value={e.id}>
                      {e.label}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label={local ? "Model" : "Model (optional)"}>
                <Input
                  key={model}
                  defaultValue={model}
                  onBlur={(e) => {
                    if (e.target.value !== model) update({ model: e.target.value });
                  }}
                  placeholder={local ? "llama3.1" : "default for engine"}
                  spellCheck={false}
                />
              </Field>
            </div>
            <p className="text-xs text-muted-foreground">
              {local
                ? "Runs locally via Ollama — thread text never leaves your machine."
                : provider
                  ? `Sends thread text to ${provider} using your stored API key, on demand per thread.`
                  : "Uses the first provider you've added a key for. Sends thread text to that provider, on demand per thread."}
            </p>
            <div className="flex items-center gap-2 pt-1 text-sm">
              <Switch checked={autoDistill} onCheckedChange={(v) => toggleAuto.mutate(v)} />
              Auto-distill in the background
            </div>
            <p className="text-xs text-muted-foreground">
              Distill new and changed threads automatically as they're indexed, so Ask, recall, and
              Project Memory stay populated without clicking. Paced, cancellable, and yields to
              indexing.{" "}
              {local
                ? "Free and on-device via Ollama."
                : "Uses your provider key, so it has a per-thread cost."}
            </p>
          </>
        )}
      </CardContent>
    </Card>
  );
}

function ProviderRow({ id, label }: { id: string; label: string }) {
  const queryClient = useQueryClient();
  const hasKey = useQuery({ queryKey: ["hasKey", id], queryFn: () => api.providerHasKey(id) });
  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["hasKey", id] });
  const remove = useMutation({ mutationFn: () => api.deleteApiKey(id), onSuccess: invalidate });

  const form = useAppForm({
    defaultValues: { key: "" },
    onSubmit: async ({ value }) => {
      const k = value.key.trim();
      if (!k) return;
      await api.setApiKey(id, k);
      invalidate();
      form.reset();
    },
  });

  // Fixed-width slots (label · status · input · Save · Remove) so a row never
  // reflows when a key is saved — Remove is always rendered, just hidden when unset.
  return (
    <form
      className="flex items-center gap-2 border-b pb-2 last:border-0"
      onSubmit={(e) => {
        e.preventDefault();
        form.handleSubmit();
      }}
    >
      <div className="flex w-40 shrink-0 items-center gap-2">
        <span className="truncate text-sm">{label}</span>
        {hasKey.data && (
          <span className="rounded-full bg-emerald-500/15 px-2 py-0.5 text-[0.6rem] font-medium uppercase tracking-wide text-emerald-600 dark:text-emerald-400">
            set
          </span>
        )}
      </div>
      <form.AppField name="key">
        {(field) => (
          <field.TextField
            type="password"
            className="ml-auto w-56"
            placeholder={
              id === "ollama"
                ? "no key needed"
                : hasKey.data
                  ? "saved — type to replace"
                  : "API key"
            }
            disabled={id === "ollama"}
          />
        )}
      </form.AppField>
      <form.AppForm>
        <form.SubmitButton>Save</form.SubmitButton>
      </form.AppForm>
      <Button
        size="sm"
        variant="ghost"
        type="button"
        onClick={() => remove.mutate()}
        className={hasKey.data ? "" : "invisible"}
      >
        Remove
      </Button>
    </form>
  );
}
