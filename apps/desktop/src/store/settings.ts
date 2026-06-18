import { create } from "zustand";

// App-level settings that live on disk (localStorage), mirroring the theme store.
// `vaultDir` is the Obsidian vault folder that "Export to Obsidian" writes into.
// `synthProvider`/`synthModel` pin which LLM does the synthesis ("" = auto: the
// first provider with a stored key, and that provider's default cheap model).
const VAULT_KEY = "callimachus.vaultDir";
const SYNTH_PROVIDER_KEY = "callimachus.synthProvider";
const SYNTH_MODEL_KEY = "callimachus.synthModel";

interface SettingsState {
  vaultDir: string;
  synthProvider: string;
  synthModel: string;
  setVaultDir: (dir: string) => void;
  setSynthProvider: (provider: string) => void;
  setSynthModel: (model: string) => void;
}

export const useSettings = create<SettingsState>((set) => ({
  vaultDir: localStorage.getItem(VAULT_KEY) ?? "",
  synthProvider: localStorage.getItem(SYNTH_PROVIDER_KEY) ?? "",
  synthModel: localStorage.getItem(SYNTH_MODEL_KEY) ?? "",
  setVaultDir: (dir) => {
    const vaultDir = dir.trim();
    localStorage.setItem(VAULT_KEY, vaultDir);
    set({ vaultDir });
  },
  setSynthProvider: (provider) => {
    localStorage.setItem(SYNTH_PROVIDER_KEY, provider);
    set({ synthProvider: provider });
  },
  setSynthModel: (model) => {
    const synthModel = model.trim();
    localStorage.setItem(SYNTH_MODEL_KEY, synthModel);
    set({ synthModel });
  },
}));
