import { type ComponentType, type ReactNode, useRef } from "react";
import { Tabs as TabsPrimitive } from "@base-ui/react/tabs";
import {
  Bot,
  Cpu,
  Database,
  FileOutput,
  FolderOpen,
  RefreshCw,
  SlidersHorizontal,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { type SettingsSection, useUi } from "../store/ui";
import { useUpdater } from "../store/updater";
import { AgentsSection } from "./settings/AgentsSection";
import { EngineSection } from "./settings/EngineSection";
import { ExportSection } from "./settings/ExportSection";
import { GeneralSection } from "./settings/GeneralSection";
import { IndexSection } from "./settings/IndexSection";
import { SourcesSection } from "./settings/SourcesSection";
import { UpdatesSection } from "./settings/UpdatesSection";

const SECTIONS: {
  id: SettingsSection;
  label: string;
  icon: ComponentType<{ className?: string }>;
  render: () => ReactNode;
}[] = [
  { id: "general", label: "General", icon: SlidersHorizontal, render: () => <GeneralSection /> },
  { id: "engine", label: "AI engine", icon: Cpu, render: () => <EngineSection /> },
  { id: "agents", label: "Agents", icon: Bot, render: () => <AgentsSection /> },
  { id: "sources", label: "Sources", icon: FolderOpen, render: () => <SourcesSection /> },
  { id: "index", label: "Index & storage", icon: Database, render: () => <IndexSection /> },
  { id: "export", label: "Export", icon: FileOutput, render: () => <ExportSection /> },
  { id: "updates", label: "Updates", icon: RefreshCw, render: () => <UpdatesSection /> },
];

/**
 * Settings: a section list on the left, the open section on the right. The list is a
 * vertical tab strip (arrow keys move and select, like a system settings sidebar); the
 * open section is remembered across visits (store/ui). Sections live in ./settings.
 */
export function SettingsView() {
  const section = useUi((s) => s.settingsSection);
  const setSection = useUi((s) => s.setSettingsSection);
  const currentVersion = useUpdater((s) => s.currentVersion);
  const updateAvailable = useUpdater(
    (s) => s.update !== null && s.status !== "idle" && s.status !== "installed",
  );
  const pane = useRef<HTMLDivElement>(null);

  return (
    <TabsPrimitive.Root
      value={section}
      onValueChange={(v) => {
        setSection(v as SettingsSection);
        pane.current?.scrollTo({ top: 0 });
      }}
      orientation="vertical"
      className="flex min-h-0 flex-1"
    >
      <aside className="flex w-44 shrink-0 flex-col border-r bg-sidebar p-3 lg:w-52">
        <TabsPrimitive.List
          aria-label="Settings sections"
          activateOnFocus
          className="flex flex-col gap-0.5"
        >
          {SECTIONS.map((s) => (
            <TabsPrimitive.Tab
              key={s.id}
              value={s.id}
              className={cn(
                "flex h-8 w-full items-center gap-2.5 rounded-lg px-2.5 text-sm text-muted-foreground outline-none transition-colors duration-100 select-none",
                "hover:bg-foreground/[0.04] hover:text-foreground dark:hover:bg-sidebar-accent/60",
                "data-active:bg-foreground/[0.07] data-active:font-medium data-active:text-foreground dark:data-active:bg-sidebar-accent",
                "focus-visible:ring-3 focus-visible:ring-ring/50",
              )}
            >
              <s.icon className="size-4 shrink-0" aria-hidden="true" />
              <span className="min-w-0 flex-1 truncate text-left">{s.label}</span>
              {s.id === "updates" && updateAvailable && (
                <span
                  className="size-1.5 shrink-0 rounded-full bg-primary"
                  role="img"
                  aria-label="Update available"
                />
              )}
            </TabsPrimitive.Tab>
          ))}
        </TabsPrimitive.List>
        <div className="mt-auto px-2.5 pt-3 text-xs text-muted-foreground tabular-nums">
          Callimachus {currentVersion ?? "dev"}
        </div>
      </aside>

      <div ref={pane} className="min-h-0 flex-1 overflow-y-auto">
        {SECTIONS.map((s) => (
          <TabsPrimitive.Panel
            key={s.id}
            value={s.id}
            className="mx-auto w-full max-w-2xl px-6 pt-7 pb-28 outline-none lg:px-8"
          >
            {s.render()}
          </TabsPrimitive.Panel>
        ))}
      </div>
    </TabsPrimitive.Root>
  );
}
