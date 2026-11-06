import { useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type ThreadDetail } from "../lib/api";
import { isImeComposing } from "../lib/keyboard";
import { focusSearch } from "../store/ui";
import { Plus } from "lucide-react";
import {
  Combobox,
  ComboboxChip,
  ComboboxChipRemove,
  ComboboxChips,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
  ComboboxValue,
} from "@/components/ui/combobox";

/**
 * Free-form tag editor as a multi-select combobox: chips for applied tags, a
 * dropdown of corpus-wide suggestions (so the user converges on a vocabulary),
 * and Enter to create a brand-new tag. Replace-set semantics: every change
 * persists the whole list via `set_thread_tags`. Drawn inline (no boxed field) so it
 * sits on the thread header's tags row: the chips, then an "Add tag" input that widens
 * while focused.
 */
export function TagsEditor({ threadId, tags }: { threadId: number; tags: string[] }) {
  const [query, setQuery] = useState("");
  const highlighted = useRef<string | undefined>(undefined);
  const queryClient = useQueryClient();
  const suggestions = useQuery({
    queryKey: ["list_tags"],
    queryFn: api.listTags,
    staleTime: 60_000,
  });

  // Patch the cached thread optimistically instead of refetching it: get_thread ships the
  // whole transcript (megabytes for big threads) and only the tag list changed. Results
  // and the tag vocabulary still refresh, since they filter and count by tag.
  const threadKey = ["thread", threadId];
  const save = useMutation({
    mutationFn: (next: string[]) => api.setThreadTags(threadId, next),
    onMutate: async (next: string[]) => {
      await queryClient.cancelQueries({ queryKey: threadKey });
      const prev = queryClient.getQueryData<ThreadDetail | null>(threadKey);
      queryClient.setQueryData<ThreadDetail | null>(threadKey, (d) =>
        d ? { ...d, tags: next } : d,
      );
      // Carry the key with the snapshot: a pending mutation runs the newest render's
      // onError, so `threadKey` there may already name another thread, and restoring this
      // thread's snapshot into it would show the wrong transcript.
      return { prev, key: threadKey };
    },
    onError: (_e, _next, ctx) => {
      if (ctx && ctx.prev !== undefined) queryClient.setQueryData(ctx.key, ctx.prev);
    },
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: ["results"] });
      queryClient.invalidateQueries({ queryKey: ["list_tags"] });
    },
  });

  // Everything the dropdown can offer: known tags plus the applied ones (so chips
  // and checkmarks resolve). Base UI filters this by the typed query.
  const known = Array.from(new Set([...(suggestions.data ?? []).map(([t]) => t), ...tags]));

  function commit(next: string[]) {
    const clean = Array.from(new Set(next.map((t) => t.trim()).filter(Boolean)));
    save.mutate(clean);
    setQuery("");
  }

  return (
    <Combobox
      items={known}
      multiple
      value={tags}
      onValueChange={(next: string[]) => commit(next)}
      inputValue={query}
      onInputValueChange={setQuery}
      onItemHighlighted={(item: string | undefined) => {
        highlighted.current = item;
      }}
    >
      <ComboboxChips className="group/tags min-h-0 w-auto gap-1 rounded-none border-0 bg-transparent p-0 focus-within:ring-0">
        <ComboboxValue>
          {(selected: string[]) =>
            selected.map((t) => (
              <ComboboxChip key={t} aria-label={t} className="py-0 font-normal">
                #{t}
                <ComboboxChipRemove aria-label={`Remove ${t}`} />
              </ComboboxChip>
            ))
          }
        </ComboboxValue>
        {!query && (
          <Plus
            className="pointer-events-none -mr-1 size-3 shrink-0 text-muted-foreground group-focus-within/tags:hidden"
            aria-hidden="true"
          />
        )}
        <ComboboxInput
          placeholder="Add tag"
          aria-label="Add a tag"
          className="h-5 w-14 min-w-0 flex-none rounded-md px-1 text-xs transition-[width] placeholder:text-muted-foreground hover:placeholder:text-foreground focus:w-28 focus:bg-muted focus:placeholder:text-transparent focus-visible:ring-2 focus-visible:ring-ring/50"
          onKeyDown={(e) => {
            // Esc with the suggestions closed: Base UI would clear the whole selection,
            // removing every tag from the thread, so it is stopped here. It clears a
            // half-typed tag instead, and with nothing typed goes back to the search box
            // (the thread pane's own Esc handler can't: the combobox marks Esc handled).
            if (e.key === "Escape" && e.currentTarget.getAttribute("aria-expanded") !== "true") {
              e.preventBaseUIHandler();
              if (query) setQuery("");
              else if (focusSearch()) e.preventDefault();
              return;
            }
            if (e.key !== "Enter" || highlighted.current || isImeComposing(e)) return;
            const t = query.trim();
            if (!t) return;
            e.preventDefault();
            if (!tags.includes(t)) commit([...tags, t]);
            else setQuery("");
          }}
        />
      </ComboboxChips>
      <ComboboxContent>
        <ComboboxEmpty>
          {query.trim() ? `Press Enter to add “${query.trim()}”` : "No tags yet"}
        </ComboboxEmpty>
        <ComboboxList>
          {(tag: string) => (
            <ComboboxItem key={tag} value={tag}>
              #{tag}
            </ComboboxItem>
          )}
        </ComboboxList>
      </ComboboxContent>
    </Combobox>
  );
}
