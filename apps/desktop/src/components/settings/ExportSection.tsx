import { useId, useRef } from "react";
import { useQuery } from "@tanstack/react-query";
import { Check } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { api, PROVIDERS } from "../../lib/api";
import { baseName } from "../../lib/format";
import { track } from "../../lib/telemetry";
import { useSettings } from "../../store/settings";
import { SectionHeader, SettingRow, SettingsGroup } from "./shared";

/** Where "Export to Obsidian" writes, and which LLM writes the synthesized export.
 *  All local, instant settings (store/settings.ts), so no toasts. */
export function ExportSection() {
  return (
    <>
      <SectionHeader
        title="Export"
        description="Where “Export to Obsidian” writes, and which model writes the synthesized version."
      />
      <div className="space-y-8">
        <SettingsGroup title="Obsidian vault">
          <VaultRows />
        </SettingsGroup>
        <SettingsGroup
          title="Synthesized export"
          description="Auto uses the first provider you've added a key for, at its cheapest model."
        >
          <SynthesisRow />
        </SettingsGroup>
      </div>
    </>
  );
}

function VaultRows() {
  const id = useId();
  const vaultDir = useSettings((s) => s.vaultDir);
  const setVaultDir = useSettings((s) => s.setVaultDir);
  const vaults = useQuery({ queryKey: ["obsidian_vaults"], queryFn: api.obsidianVaults });
  // The vault input saves on every keystroke, so its analytics event is sent once per
  // edit (on blur, if the value changed since focus) rather than per character.
  const vaultAtFocus = useRef<string | null>(null);
  const chooseVault = (path: string) => {
    if (path === vaultDir) return;
    setVaultDir(path);
    track("setting_changed", { key: "vault_dir" });
  };

  const detected = vaults.data ?? [];

  return (
    <SettingRow htmlFor={id} title="Vault folder">
      <Input
        id={id}
        className="font-mono text-[13px]"
        value={vaultDir}
        onChange={(e) => setVaultDir(e.target.value)}
        onFocus={() => {
          vaultAtFocus.current = vaultDir;
        }}
        onBlur={() => {
          const before = vaultAtFocus.current;
          vaultAtFocus.current = null;
          if (before !== null && before !== useSettings.getState().vaultDir) {
            track("setting_changed", { key: "vault_dir" });
          }
        }}
        placeholder="Paste a vault path, or pick a detected one"
        spellCheck={false}
      />
      {detected.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="mr-0.5 text-[13px] text-muted-foreground">Detected:</span>
          {detected.map((path) => {
            const selected = vaultDir === path;
            return (
              <Button
                key={path}
                size="xs"
                variant="outline"
                aria-pressed={selected}
                onClick={() => chooseVault(path)}
                title={path}
              >
                {selected && <Check aria-hidden="true" data-icon="inline-start" />}
                {baseName(path)}
              </Button>
            );
          })}
        </div>
      )}
    </SettingRow>
  );
}

/** Base UI Select treats "" as "nothing selected" (placeholder styling, no label), so
 *  auto is a sentinel here and maps back to the stored "". */
const AUTO = "auto";
const PROVIDER_ITEMS = [
  { value: AUTO, label: "Auto (first key)" },
  ...PROVIDERS.map((p) => ({ value: p.id, label: p.label })),
];

function SynthesisRow() {
  const providerId = useId();
  const modelId = useId();
  const synthProvider = useSettings((s) => s.synthProvider);
  const setSynthProvider = useSettings((s) => s.setSynthProvider);
  const synthModel = useSettings((s) => s.synthModel);
  const setSynthModel = useSettings((s) => s.setSynthModel);

  return (
    <SettingRow>
      <div className="grid grid-cols-2 gap-4">
        <Field>
          <FieldLabel htmlFor={providerId}>Synthesis provider</FieldLabel>
          <Select
            items={PROVIDER_ITEMS}
            value={synthProvider || AUTO}
            onValueChange={(v) => {
              const next = !v || v === AUTO ? "" : v;
              if (next === synthProvider) return;
              setSynthProvider(next);
              track("setting_changed", { key: "synth_engine" });
            }}
          >
            <SelectTrigger id={providerId} className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PROVIDER_ITEMS.map((p) => (
                <SelectItem key={p.value} value={p.value}>
                  {p.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Field>
        <Field>
          <FieldLabel htmlFor={modelId}>Model</FieldLabel>
          <Input
            id={modelId}
            value={synthModel}
            onChange={(e) => setSynthModel(e.target.value)}
            placeholder="Default for provider"
            disabled={!synthProvider}
            spellCheck={false}
          />
        </Field>
      </div>
    </SettingRow>
  );
}
