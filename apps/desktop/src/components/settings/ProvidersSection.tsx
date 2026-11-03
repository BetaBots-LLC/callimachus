import { useId, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Eye, EyeOff } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group";
import { Spinner } from "@/components/ui/spinner";
import { useAppForm } from "@/lib/form";
import { api, PROVIDERS } from "../../lib/api";
import { describeKeySaveError } from "../../lib/errors";
import { track } from "../../lib/telemetry";
import { errorText, notify } from "../../lib/toast";
import { InlineError, SectionHeader, SettingRow, SettingsGroup, StatePill } from "./shared";

/** API keys (OS keychain) and the Ollama server URLs. */
export function ProvidersSection() {
  return (
    <>
      <SectionHeader
        title="Providers"
        description="Keys for chat, Ask and distillation. They're stored in your OS keychain, never on disk."
      />
      <div className="space-y-8">
        <SettingsGroup title="API keys">
          {PROVIDERS.map((p) => (
            <ProviderRow key={p.id} id={p.id} label={p.label} />
          ))}
        </SettingsGroup>
      </div>
    </>
  );
}

const KEY_SAVE_PREFIX = /^Couldn't save the key:\s*/;

function ProviderRow({ id, label }: { id: string; label: string }) {
  const keyInputId = useId();
  const urlInputId = useId();
  const queryClient = useQueryClient();
  const hasKey = useQuery({ queryKey: ["hasKey", id], queryFn: () => api.providerHasKey(id) });
  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["hasKey", id] });
  // Remove is two-step, like deleting threads in Index: the first click asks, the second
  // removes. A keychain delete can't be undone from here.
  const [confirmRemove, setConfirmRemove] = useState(false);
  const remove = useMutation({
    mutationFn: () => api.deleteApiKey(id),
    onSuccess: () => {
      invalidate();
      track("setting_changed", { key: "api_key" });
      notify.success(`${label} key removed`);
      setConfirmRemove(false);
    },
    onError: (e) => {
      setConfirmRemove(false);
      notify.error("Couldn't remove the key", { description: errorText(e) });
    },
  });

  // Local Ollama and Ollama Cloud accept a custom server URL (persisted; shared with the chat
  // header). Local Ollama is keyless; Ollama Cloud needs a key like any cloud provider.
  const isOllamaLike = id === "ollama" || id === "ollama_cloud";
  const keyless = id === "ollama";
  const baseUrl = useQuery({
    queryKey: ["providerBaseUrl", id],
    queryFn: () => api.providerBaseUrl(id),
    enabled: isOllamaLike,
  });
  const saveUrl = useMutation({
    mutationFn: (url: string) => api.setProviderBaseUrl(id, url),
    // Reflect the new URL immediately (also mirrors to the chat header, same query key); the
    // invalidate reconciles with what the backend actually stored.
    onMutate: (url: string) => queryClient.setQueryData(["providerBaseUrl", id], url),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["providerBaseUrl", id] });
      track("setting_changed", { key: "provider_base_url" });
    },
    onError: (e) => {
      // Drop the optimistic value so the field shows what is actually stored.
      queryClient.invalidateQueries({ queryKey: ["providerBaseUrl", id] });
      notify.error("Couldn't save the server URL", { description: errorText(e) });
    },
  });
  // `null` = not editing → show the saved value; a local draft overrides it while typing so an
  // in-flight query result can't clobber the input.
  const [urlEdit, setUrlEdit] = useState<string | null>(null);
  const [showKey, setShowKey] = useState(false);

  // A failed save (e.g. no Secret Service keyring on Linux) must be visible: without it,
  // Save looks like it did nothing and chat later fails with "No API key set".
  const saveKey = useMutation({
    mutationFn: (key: string) => api.setApiKey(id, key),
    onSuccess: () => {
      invalidate();
      track("setting_changed", { key: "api_key" });
      notify.success(`${label} key saved`);
    },
    onError: (e) =>
      notify.error("Couldn't save the key", {
        description: describeKeySaveError(e).replace(KEY_SAVE_PREFIX, ""),
      }),
  });
  const form = useAppForm({
    defaultValues: { key: "" },
    onSubmit: async ({ value }) => {
      const k = value.key.trim();
      if (!k) return;
      try {
        await saveKey.mutateAsync(k);
        form.reset();
        setShowKey(false);
      } catch {
        // Shown below via saveKey.error; swallowing here avoids an unhandled rejection
        // from the (unawaited) form.handleSubmit().
      }
    },
  });

  const set = !!hasKey.data;
  // The key went away (removed here or elsewhere): there is nothing left to confirm.
  if (!set && confirmRemove) setConfirmRemove(false);

  const removeControl =
    keyless || !set ? undefined : confirmRemove ? (
      <>
        <span className="text-[13px] text-muted-foreground">Remove the {label} key?</span>
        {!remove.isPending && (
          <Button size="xs" variant="ghost" type="button" onClick={() => setConfirmRemove(false)}>
            Cancel
          </Button>
        )}
        <Button
          size="xs"
          variant="destructive"
          type="button"
          disabled={remove.isPending}
          onClick={() => remove.mutate()}
        >
          {remove.isPending && <Spinner data-icon="inline-start" />}
          Remove
        </Button>
      </>
    ) : (
      <Button size="sm" variant="ghost" type="button" onClick={() => setConfirmRemove(true)}>
        Remove
      </Button>
    );

  return (
    <SettingRow
      htmlFor={keyless ? undefined : keyInputId}
      title={
        <span className="inline-flex items-center gap-2">
          {label}
          {set && <StatePill>Saved</StatePill>}
        </span>
      }
      description={keyless ? "Runs locally on your machine, so it needs no API key." : undefined}
      control={removeControl}
    >
      {!keyless && (
        <form
          className="flex items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            form.handleSubmit();
          }}
        >
          <form.Field name="key">
            {(field) => (
              <InputGroup className="flex-1">
                <InputGroupInput
                  id={keyInputId}
                  aria-label={`${label} API key`}
                  type={showKey ? "text" : "password"}
                  autoComplete="off"
                  spellCheck={false}
                  placeholder={set ? "Paste a new key to replace it" : "API key"}
                  value={field.state.value}
                  onChange={(e) => field.handleChange(e.currentTarget.value)}
                  onBlur={field.handleBlur}
                />
                <InputGroupAddon align="inline-end">
                  <InputGroupButton
                    size="icon-xs"
                    aria-label={showKey ? "Hide key" : "Show key"}
                    aria-pressed={showKey}
                    onClick={() => setShowKey((v) => !v)}
                  >
                    {showKey ? <EyeOff /> : <Eye />}
                  </InputGroupButton>
                </InputGroupAddon>
              </InputGroup>
            )}
          </form.Field>
          <form.Subscribe
            selector={(s) => ({ empty: !s.values.key.trim(), isSubmitting: s.isSubmitting })}
          >
            {({ empty, isSubmitting }) => (
              <Button
                type="submit"
                size="default"
                variant={empty ? "outline" : "default"}
                disabled={empty || isSubmitting || saveKey.isPending}
              >
                {saveKey.isPending && <Spinner data-icon="inline-start" />}
                {saveKey.isPending ? "Saving…" : "Save key"}
              </Button>
            )}
          </form.Subscribe>
        </form>
      )}
      {saveKey.isError && <InlineError>{describeKeySaveError(saveKey.error)}</InlineError>}
      {remove.isError && (
        <InlineError>Couldn't remove the key: {errorText(remove.error)}</InlineError>
      )}
      {isOllamaLike && (
        <Field>
          <FieldLabel
            htmlFor={urlInputId}
            className="text-[13px] font-normal text-muted-foreground"
          >
            Server URL
          </FieldLabel>
          <Input
            id={urlInputId}
            value={urlEdit ?? baseUrl.data ?? ""}
            onChange={(e) => setUrlEdit(e.target.value)}
            onBlur={(e) => {
              const url = e.target.value.trim();
              setUrlEdit(null);
              if (url !== (baseUrl.data ?? "")) saveUrl.mutate(url);
            }}
            placeholder={id === "ollama_cloud" ? "https://ollama.com" : "http://localhost:11434"}
            spellCheck={false}
            className="font-mono text-[13px]"
          />
        </Field>
      )}
      {saveUrl.isError && (
        <InlineError>Couldn't save the server URL: {errorText(saveUrl.error)}</InlineError>
      )}
    </SettingRow>
  );
}
