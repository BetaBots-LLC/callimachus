import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import { api, INDEXABLE_SOURCES, SOURCE_LABELS, type SourceKind } from "../lib/api";
import { useUi } from "../store/ui";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Check, ChevronDown } from "lucide-react";
import { cn } from "@/lib/utils";

// The three most-used sources get top-level chips; the rest live under "More".
const PRIMARY: SourceKind[] = ["claude_code", "codex", "cursor"];
const MORE: SourceKind[] = [
  ...INDEXABLE_SOURCES.filter((s) => !PRIMARY.includes(s)),
  "in_app",
];

export function SearchBar() {
  const setQuery = useUi((s) => s.setQuery);
  const sources = useUi((s) => s.sources);
  const toggleSource = useUi((s) => s.toggleSource);
  const includeSubagents = useUi((s) => s.includeSubagents);
  const toggleSubagents = useUi((s) => s.toggleSubagents);
  const hybrid = useUi((s) => s.hybrid);
  const toggleHybrid = useUi((s) => s.toggleHybrid);
  const queryClient = useQueryClient();

  const [text, setText] = useState("");
  // Pacer handles the debounce (no manual timer / useEffect).
  const commitQuery = useDebouncedCallback((value: string) => setQuery(value.trim()), {
    wait: 180,
  });

  function onChange(value: string) {
    setText(value);
    commitQuery(value);
  }

  const reindex = useMutation({
    mutationFn: api.indexAll,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["results"] });
      queryClient.invalidateQueries({ queryKey: ["db_stats"] });
    },
  });

  const embed = useQuery({
    queryKey: ["embed_status"],
    queryFn: api.embeddingStatus,
    refetchInterval: (q) => (q.state.data?.running ? 700 : false),
  });
  const buildIndex = useMutation({ mutationFn: api.buildEmbeddings, onSuccess: () => embed.refetch() });

  const embedPct =
    embed.data && embed.data.total > 0 ? Math.round((embed.data.done / embed.data.total) * 100) : 0;

  // How many "More" sources are active — surfaced on the dropdown so a hidden
  // filter is never silently in effect.
  const moreActiveCount = MORE.filter((s) => sources.includes(s)).length;

  return (
    <div className="flex flex-col gap-2 border-b px-4 py-3">
      <div className="flex gap-2">
        <Input
          placeholder="Search every thread…"
          value={text}
          autoFocus
          onChange={(e) => onChange(e.currentTarget.value)}
        />
        <Button variant="secondary" onClick={() => reindex.mutate()} disabled={reindex.isPending}>
          {reindex.isPending ? "Indexing…" : "Reindex"}
        </Button>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        {PRIMARY.map((s) => (
          <Button
            key={s}
            size="xs"
            variant={sources.includes(s) ? "default" : "outline"}
            onClick={() => toggleSource(s)}
            title={sources.length === 0 ? "All sources" : undefined}
          >
            {SOURCE_LABELS[s]}
          </Button>
        ))}

        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <Button size="xs" variant={moreActiveCount > 0 ? "default" : "outline"} />
            }
          >
            More{moreActiveCount > 0 ? ` (${moreActiveCount})` : ""}
            <ChevronDown className="size-3.5" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start">
            {MORE.map((s) => (
              <DropdownMenuItem
                key={s}
                closeOnClick={false}
                onClick={() => toggleSource(s)}
              >
                <Check
                  className={cn("size-4", sources.includes(s) ? "opacity-100" : "opacity-0")}
                />
                {SOURCE_LABELS[s]}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>

        <Separator />
        <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Switch checked={includeSubagents} onCheckedChange={toggleSubagents} />
          subagents
        </label>
        <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Switch checked={hybrid} onCheckedChange={toggleHybrid} />
          semantic
        </label>

        <span className="ml-auto flex items-center gap-2 text-xs text-muted-foreground">
          {embed.data?.running ? (
            <>indexing meaning… {embedPct}%</>
          ) : embed.data && embed.data.done < embed.data.total ? (
            <Button
              size="xs"
              variant="outline"
              onClick={() => buildIndex.mutate()}
              title={`${embed.data.done}/${embed.data.total} messages embedded`}
            >
              Build semantic index ({embedPct}%)
            </Button>
          ) : embed.data && embed.data.total > 0 ? (
            <span>semantic ready</span>
          ) : null}
          {reindex.data && (
            <span>
              +{reindex.data.threadsIndexed} threads · {reindex.data.messagesIndexed} msgs
            </span>
          )}
        </span>
      </div>
    </div>
  );
}

function Separator() {
  return <span className="mx-1 h-4 w-px bg-border" />;
}
