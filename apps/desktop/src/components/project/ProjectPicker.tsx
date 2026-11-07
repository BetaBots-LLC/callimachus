import { useState } from "react";
import { Combobox as ComboboxPrimitive } from "@base-ui/react/combobox";
import { ChevronsUpDown, FolderGit2, Layers, Search } from "lucide-react";
import type { ProjectInfo } from "../../lib/api";
import { baseName, shortPath } from "../../lib/format";
import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from "@/components/ui/combobox";
import { relativeAge } from "./shared";

/** The picker's value for "All projects" (never a real project key: those are paths). */
export const ALL_PROJECTS = "*";

/**
 * The project picker: a searchable select (the input sits in the popup, Base UI's
 * input-inside-popup combobox), so a long project list filters by name or path as you type
 * and the closed trigger stays a plain, scannable label.
 */
export function ProjectPicker({
  value,
  projects,
  onChange,
}: {
  value: string | null;
  projects: ProjectInfo[];
  onChange: (project: string) => void;
}) {
  const [query, setQuery] = useState("");
  const byKey = new Map(projects.map((p) => [p.project, p]));
  const items = [ALL_PROJECTS, ...projects.map((p) => p.project)];
  const label = (v: string) => (v === ALL_PROJECTS ? "All projects" : baseName(v));
  const q = query.trim().toLowerCase();

  return (
    <Combobox
      items={items}
      value={value}
      onValueChange={(v: string | null) => {
        if (v) onChange(v);
      }}
      inputValue={query}
      onInputValueChange={setQuery}
      onOpenChange={(open: boolean) => {
        if (!open) setQuery("");
      }}
      itemToStringLabel={label}
      // Match the folder name or anywhere in the path ("betabots" finds every repo under it).
      filter={(item: string) =>
        !q || label(item).toLowerCase().includes(q) || item.toLowerCase().includes(q)
      }
    >
      <ComboboxPrimitive.Trigger
        aria-label="Project"
        className="flex h-9 min-w-0 max-w-full cursor-pointer items-center gap-2 rounded-lg border border-input bg-transparent pr-2 pl-2.5 text-left outline-none transition-colors hover:bg-muted/50 focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 data-popup-open:bg-muted/50"
      >
        {value === ALL_PROJECTS ? (
          <Layers aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        ) : (
          <FolderGit2 aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        )}
        <span className="min-w-0 truncate text-sm font-semibold">
          {value ? label(value) : "Pick a project"}
        </span>
        {value && value !== ALL_PROJECTS && (
          <span className="hidden min-w-0 truncate text-[13px] text-muted-foreground @min-[36rem]/page:inline">
            {shortPath(value)}
          </span>
        )}
        <ChevronsUpDown aria-hidden="true" className="size-3.5 shrink-0 text-muted-foreground" />
      </ComboboxPrimitive.Trigger>
      <ComboboxContent className="w-auto max-w-[min(32rem,calc(100vw-2rem))] min-w-72 p-0">
        <div className="flex items-center gap-2 border-b px-2.5 py-2">
          <Search aria-hidden="true" className="size-3.5 shrink-0 text-muted-foreground" />
          <ComboboxInput placeholder="Find a project" aria-label="Find a project" />
        </div>
        {/* Base UI keeps Empty mounted as a live region, so drop its padding while it has no
            text, or it leaves a blank band above the first match. */}
        <ComboboxEmpty className="px-3 py-2 empty:p-0">
          No project matches “{query.trim()}”.
        </ComboboxEmpty>
        <ComboboxList className="max-h-80 overflow-y-auto p-1">
          {(item: string) => {
            const info = byKey.get(item);
            return (
              <ComboboxItem key={item} value={item} className="py-1.5">
                {item === ALL_PROJECTS ? (
                  <span className="flex flex-col">
                    <span className="font-medium">All projects</span>
                    <span className="text-xs text-muted-foreground">
                      Every repo with its counts, and open TODOs across them
                    </span>
                  </span>
                ) : (
                  <span className="flex min-w-0 flex-col">
                    <span className="flex min-w-0 items-baseline gap-2">
                      <span className="truncate font-medium">{baseName(item)}</span>
                      {info && (
                        <span className="ml-auto shrink-0 text-xs tabular-nums text-muted-foreground">
                          {relativeAge(info.lastActivity)}
                        </span>
                      )}
                    </span>
                    <span className="truncate text-xs text-muted-foreground" title={item}>
                      {shortPath(item)}
                      {info ? ` · ${info.threadCount.toLocaleString()} sessions` : ""}
                    </span>
                  </span>
                )}
              </ComboboxItem>
            );
          }}
        </ComboboxList>
      </ComboboxContent>
    </Combobox>
  );
}
