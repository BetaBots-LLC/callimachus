import { useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
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
 * and Enter to create a brand-new tag. Replace-set semantics — every change
 * persists the whole list via `set_thread_tags`.
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

  const save = useMutation({
    mutationFn: (next: string[]) => api.setThreadTags(threadId, next),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["thread", threadId] });
      queryClient.invalidateQueries({ queryKey: ["results"] });
      queryClient.invalidateQueries({ queryKey: ["list_tags"] });
    },
  });

  // Everything the dropdown can offer: known tags ∪ currently-applied (so chips
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
      <ComboboxChips className="mt-2">
        <ComboboxValue>
          {(selected: string[]) =>
            selected.map((t) => (
              <ComboboxChip key={t} aria-label={t}>
                #{t}
                <ComboboxChipRemove aria-label={`Remove ${t}`} />
              </ComboboxChip>
            ))
          }
        </ComboboxValue>
        <ComboboxInput
          placeholder={tags.length ? "" : "add tags…"}
          onKeyDown={(e) => {
            if (e.key !== "Enter" || highlighted.current) return;
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
