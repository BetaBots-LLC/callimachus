import type { ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, INDEXABLE_SOURCES, PROVIDERS, SOURCE_LABELS, type SourceKind } from "../lib/api";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { useAppForm } from "@/lib/form";
import { useSettings } from "../store/settings";
import { CleanupCard } from "./CleanupCard";

const INDEXABLE: SourceKind[] = INDEXABLE_SOURCES;

export function SettingsView() {
  const queryClient = useQueryClient();
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  const embed = useQuery({
    queryKey: ["embed_status"],
    queryFn: api.embeddingStatus,
    refetchInterval: (q) => (q.state.data?.running ? 700 : false),
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

/** A uniform labeled field: muted label above a control. */
function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-1.5">
      <span className="block text-xs font-medium text-muted-foreground">{label}</span>
      {children}
    </div>
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
