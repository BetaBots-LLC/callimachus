import { useQuery } from "@tanstack/react-query";
import { api, PROVIDERS } from "../lib/api";
import { loadModelCache, MODELS_TTL, saveModelCache } from "../lib/models";
import { useChat } from "../store/chat";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";

/**
 * The chat provider and model (store/chat.ts persists them as the app's chat default), with
 * what the Chat view and Settings > AI engine need to know about them: the provider's saved
 * base URL, whether it needs a key and has one, and the live model list.
 */
export function useChatEngine() {
  const provider = useChat((s) => s.provider);
  const model = useChat((s) => s.model);

  // Local Ollama and Ollama Cloud take a custom endpoint, persisted per provider and edited
  // in Settings > AI engine. react-query is the source of truth: the saved URL feeds the model
  // list and send directly.
  const isOllamaLike = provider === "ollama" || provider === "ollama_cloud";
  const savedBaseUrl = useQuery({
    queryKey: ["providerBaseUrl", provider],
    queryFn: () => api.providerBaseUrl(provider),
    enabled: isOllamaLike,
  });
  const baseUrl = savedBaseUrl.data ?? "";

  // CLI backends (claude-cli / codex-cli) use their own logged-in auth: no API key.
  const isCli = provider.endsWith("-cli");
  const needsKey = provider !== "ollama" && !isCli;
  const hasKey = useQuery({
    queryKey: ["hasKey", provider],
    queryFn: () => api.providerHasKey(provider),
    // Keyless engines (Ollama, CLI) never need a key, so skip the probe so the keychain stays quiet.
    enabled: needsKey,
  });

  // Offer installed CLI backends alongside the keyed providers (CLI has no default model; the
  // CLI picks one when model is empty).
  const cliEngines = useQuery({ queryKey: ["cli_engines"], queryFn: api.cliEngines });
  const providerOptions = [
    ...PROVIDERS.map((p) => ({ id: p.id, label: p.label, defaultModel: p.defaultModel })),
    ...(cliEngines.data ?? [])
      .filter((e) => e.installed)
      .map((e) => ({ id: e.id, label: `${e.label} (no key)`, defaultModel: "" })),
  ];
  const providerLabel =
    providerOptions.find((p) => p.id === provider)?.label ??
    cliEngines.data?.find((e) => e.id === provider)?.label ??
    provider;

  // Live model list from the provider API, cached in localStorage so the dropdown
  // is instant and self-refreshes (TTL / version), so banned models drop off.
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

  return {
    provider,
    model,
    providerLabel,
    providerOptions,
    modelList,
    modelsLoading: modelsQuery.isFetching,
    baseUrl,
    needsKey,
    // Only a settled "no": while the keychain probe runs, nothing claims the key is missing.
    keyMissing: needsKey && hasKey.data === false,
  };
}

export type ChatEngine = ReturnType<typeof useChatEngine>;

/** The provider and model selects, shared by the Chat header and Settings > AI engine. */
export function ChatEnginePicker({
  engine,
  className,
  providerId,
  modelId,
}: {
  engine: ChatEngine;
  className?: string;
  providerId?: string;
  modelId?: string;
}) {
  const setProvider = useChat((s) => s.setProvider);
  const setModel = useChat((s) => s.setModel);
  const { provider, model, providerOptions, modelList } = engine;
  return (
    <div className={cn("flex min-w-0 items-center gap-2", className)}>
      <Select
        items={providerOptions.map((p) => ({ value: p.id, label: p.label }))}
        value={provider}
        onValueChange={(v) => {
          const p = providerOptions.find((x) => x.id === v);
          if (p) setProvider(p.id, p.defaultModel);
        }}
      >
        <SelectTrigger id={providerId} className="w-40 shrink-0" aria-label="Chat provider">
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
        <SelectTrigger id={modelId} className="min-w-0 flex-1" aria-label="Model">
          <SelectValue placeholder={engine.modelsLoading ? "Loading models…" : "Model"} />
        </SelectTrigger>
        <SelectContent>
          {modelList.map((m) => (
            <SelectItem key={m} value={m}>
              {m}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
