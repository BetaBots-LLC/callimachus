import { useId } from "react";
import { ChatEnginePicker, useChatEngine } from "../ChatEnginePicker";
import { MemoryEngineSettings } from "./KnowledgeSection";
import { ProviderKeys } from "./ProvidersSection";
import { Note, SectionHeader, SettingRow, SettingsGroup } from "./shared";

/**
 * Settings > AI engine: everything that decides which model reads or answers, in one place.
 * Memory (the distillation switch, its engine and the auto-distill allowance) leads, since
 * Ask, recall and the hooks depend on it; then Chat's provider; then the keys and server URLs
 * both draw on. Replaces the separate Providers and Knowledge sections, whose ids still open
 * it (store/ui.ts resolveSettingsSection).
 */
export function EngineSection() {
  return (
    <>
      <SectionHeader
        title="AI engine"
        description="The models behind memory, Ask and Chat. Search, open TODOs and Remember need none. Conversation text goes only to the engines you pick here."
      />
      <div className="space-y-8">
        <MemoryEngineSettings />
        <ChatDefault />
        <ProviderKeys />
      </div>
    </>
  );
}

/** Chat's provider and model: the same pair the Chat header picks (store/chat.ts keeps it). */
function ChatDefault() {
  const providerId = useId();
  const engine = useChatEngine();
  return (
    <SettingsGroup title="Chat">
      <SettingRow
        htmlFor={providerId}
        title="Provider and model"
        description="What the Chat tab sends your messages to. Ask uses the memory engine instead."
      >
        <ChatEnginePicker engine={engine} providerId={providerId} />
        {engine.keyMissing && (
          <Note>
            No {engine.providerLabel} API key is saved yet. Add it under API keys and servers below.
          </Note>
        )}
      </SettingRow>
    </SettingsGroup>
  );
}
