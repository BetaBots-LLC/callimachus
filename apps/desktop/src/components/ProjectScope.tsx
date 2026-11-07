import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { Combobox as ComboboxPrimitive } from "@base-ui/react/combobox";
import { ChevronDown, FolderGit2, Search, X } from "lucide-react";
import { api } from "../lib/api";
import { baseName, shortPath } from "../lib/format";
import { AGGREGATE_STALE_MS } from "../lib/queryClient";
import { useUi } from "../store/ui";
import { Button } from "@/components/ui/button";
import {
  ComboboxContent,
  ComboboxEmpty,
  ComboboxItem,
  ComboboxList,
} from "@/components/ui/combobox";
import { cn } from "@/lib/utils";

/** The repo the search is scoped to: a searchable picker (repo lists run to hundreds), and
 *  while one is set, an X that goes back to every repo in one click. */
export function ProjectScope() {
  const project = useUi((s) => s.project);
  const setProject = useUi((s) => s.setProject);
  // Shared with the Project page's picker (same key); index:done refreshes it.
  const projects = useQuery({
    queryKey: ["projects"],
    queryFn: api.listProjects,
    staleTime: AGGREGATE_STALE_MS,
  });
  // "" stands for every repo, so it can sit in the list as an item.
  const items = useMemo(
    () => ["", ...(projects.data?.map((p) => p.project) ?? [])],
    [projects.data],
  );
  // Two repos with the same folder name read the same in the list, so those show their
  // parent folder too.
  const label = useMemo(() => {
    const counts = new Map<string, number>();
    for (const p of items) if (p) counts.set(baseName(p), (counts.get(baseName(p)) ?? 0) + 1);
    return (p: string) =>
      !p ? "All repos" : (counts.get(baseName(p)) ?? 0) > 1 ? shortPath(p) : baseName(p);
  }, [items]);
  const threadCount = useMemo(
    () => new Map(projects.data?.map((p) => [p.project, p.threadCount])),
    [projects.data],
  );
  const name = project ? baseName(project) : null;

  return (
    <div className="flex shrink-0 items-center">
      <ComboboxPrimitive.Root
        items={items}
        value={project ?? ""}
        onValueChange={(v) => setProject((v as string | null) || null)}
        itemToStringLabel={label}
      >
        <ComboboxPrimitive.Trigger
          render={
            <Button
              variant={project ? "secondary" : "outline"}
              className={cn("max-w-48", project && "rounded-r-none")}
              title={project ?? "Search every repo, or pick one"}
              aria-label={project ? `Repo: ${name}. Change repo` : "Repo: all. Pick a repo"}
            />
          }
        >
          <FolderGit2 />
          <span className="truncate">{name ?? "All repos"}</span>
          <ChevronDown className="size-3.5 opacity-60" />
        </ComboboxPrimitive.Trigger>
        <ComboboxContent className="w-72 p-0">
          <div className="flex items-center gap-2 border-b px-2.5">
            <Search className="size-3.5 shrink-0 text-muted-foreground" />
            <ComboboxPrimitive.Input
              placeholder="Find a repo"
              aria-label="Find a repo"
              className="h-8 min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
            />
          </div>
          <ComboboxEmpty className="px-2.5 py-2 empty:p-0">No repo matches.</ComboboxEmpty>
          <ComboboxList className="max-h-72 overflow-y-auto p-1">
            {(p: string) => (
              <ComboboxItem key={p || "all"} value={p}>
                <span className="flex items-baseline gap-2">
                  <span className="min-w-0 flex-1 truncate" title={p || undefined}>
                    {label(p)}
                  </span>
                  {p && (
                    <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
                      {threadCount.get(p)?.toLocaleString()}
                    </span>
                  )}
                </span>
              </ComboboxItem>
            )}
          </ComboboxList>
        </ComboboxContent>
      </ComboboxPrimitive.Root>
      {project && (
        <Button
          variant="secondary"
          size="icon"
          className="rounded-l-none border-l border-background/60"
          aria-label={`Search all repos, not just ${name}`}
          title="Search all repos"
          onClick={() => setProject(null)}
        >
          <X />
        </Button>
      )}
    </div>
  );
}
