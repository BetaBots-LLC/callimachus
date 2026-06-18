import { startTransition, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { Loader2, Plus } from "lucide-react";
import { api, type ChatMessage } from "../lib/api";
import { useChat } from "../store/chat";
import { Button } from "@/components/ui/button";
import { formatTime } from "../lib/format";
import { cn } from "@/lib/utils";

export function ChatSidebar() {
  const newChat = useChat((s) => s.newChat);
  const loadChat = useChat((s) => s.loadChat);
  const setLoadingThread = useChat((s) => s.setLoadingThread);
  const [activeId, setActiveId] = useState<number | null>(null);

  const chats = useQuery({
    queryKey: ["chats"],
    queryFn: () => api.recentThreads({ sources: ["in_app"], limit: 100 }),
  });

  const load = useMutation({
    mutationFn: (id: number) => api.getThread(id),
    onMutate: () => setLoadingThread(true),
    onSuccess: (detail) => {
      if (!detail) return;
      const msgs: ChatMessage[] = detail.messages
        .filter((m) => m.role === "user" || m.role === "assistant" || m.role === "system")
        .map((m) => ({ role: m.role as ChatMessage["role"], content: m.text }));
      setActiveId(detail.id);
      // Render the (potentially large) conversation as a non-blocking transition so
      // input stays responsive while React reconciles it.
      startTransition(() => loadChat(detail.externalId, msgs));
    },
    onSettled: () => setLoadingThread(false),
  });

  return (
    <aside className="flex w-60 shrink-0 flex-col border-r">
      <div className="border-b p-2">
        <Button
          size="sm"
          variant="outline"
          className="w-full justify-start gap-2"
          onClick={() => {
            newChat();
            setActiveId(null);
          }}
        >
          <Plus className="size-4" /> New chat
        </Button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {chats.data?.length ? (
          chats.data.map((c) => (
            <button
              key={c.id}
              onClick={() => load.mutate(c.id)}
              disabled={load.isPending}
              className={cn(
                "block w-full cursor-pointer border-b px-3 py-2 text-left hover:bg-muted/50",
                activeId === c.id && "bg-muted",
              )}
            >
              <div className="flex items-center gap-1.5">
                <span className="truncate text-sm">{c.title || "Untitled chat"}</span>
                {load.isPending && load.variables === c.id && (
                  <Loader2 className="size-3 shrink-0 animate-spin text-muted-foreground" />
                )}
              </div>
              <div className="text-[0.68rem] text-muted-foreground">
                {c.messageCount} msgs · {formatTime(c.updatedAt)}
              </div>
            </button>
          ))
        ) : (
          <div className="p-3 text-xs text-muted-foreground">No saved chats yet.</div>
        )}
      </div>
    </aside>
  );
}
