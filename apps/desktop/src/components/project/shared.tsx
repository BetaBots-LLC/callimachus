import { type ReactElement, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Check, Copy } from "lucide-react";
import { api, OPEN_TARGETS } from "../../lib/api";
import { CAN_LAUNCH_CLI_FALLBACK } from "../../lib/platform";
import { copyText } from "../../lib/clipboard";
import { errorText } from "../../lib/toast";
import { useUi } from "../../store/ui";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Menu as MenuPrimitive } from "@base-ui/react/menu";

/** How many recent sessions the page lists (and scans for produced commits). */
export const RECENT_SESSIONS = 10;

/** The last sessions in a project. Shared by Recent sessions and Produced commits, so with
 *  both open it is one fetch. */
export function recentSessionsQuery(project: string) {
  return {
    queryKey: ["project_recent", project] as const,
    queryFn: () => api.recentThreads({ project, limit: RECENT_SESSIONS }),
  };
}

/** Open a thread in the Search view's reader (at `messageId` when given). */
export function useOpenThread() {
  return useUi((s) => s.openThread);
}

/** Move keyboard focus off a list row that is about to be removed, so it does not drop to
 *  the page body when the row unmounts under it: to the next row's `row` control (its first
 *  button by default), else the previous row's, else `fallback` inside the enclosing section
 *  (by default the Project page section's header button). The target stays mounted, so this
 *  runs before the removal. */
export function focusSuccessor(
  from: Element | null,
  { row = "button", fallback = "h2 button" }: { row?: string; fallback?: string } = {},
) {
  const li = from?.closest("li");
  const sibling = li?.nextElementSibling ?? li?.previousElementSibling;
  const target =
    sibling?.querySelector<HTMLElement>(row) ??
    li?.closest("section")?.querySelector<HTMLElement>(fallback);
  target?.focus();
}

/** Whether the backend can open a terminal here; fixed for the session (see ThreadView). */
export function useCanLaunchCli(): boolean {
  const q = useQuery({
    queryKey: ["cli_launch_supported"],
    queryFn: api.cliLaunchSupported,
    staleTime: Number.POSITIVE_INFINITY,
  });
  return q.data ?? CAN_LAUNCH_CLI_FALLBACK;
}

/** "4 min ago", "3 h ago", "2 d ago", then a date: rows are scanned for recency, so the
 *  near past reads as a distance and the far past as a date. */
export function relativeAge(epochSeconds: number | null | undefined): string {
  if (!epochSeconds) return "";
  const secs = Math.max(0, Date.now() / 1000 - epochSeconds);
  if (secs < 60) return "just now";
  if (secs < 3600) return `${Math.floor(secs / 60)} min ago`;
  if (secs < 86_400) return `${Math.floor(secs / 3600)} h ago`;
  if (secs < 30 * 86_400) return `${Math.floor(secs / 86_400)} d ago`;
  return new Date(epochSeconds * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

/** A full timestamp for a `title`, so the relative age can be checked on hover. */
export function fullDate(epochSeconds: number | null | undefined): string | undefined {
  return epochSeconds ? new Date(epochSeconds * 1000).toLocaleString() : undefined;
}

/** "12.4k chars" style sizes for checkpoint bodies and agent context. */
export function formatChars(n: number): string {
  if (n < 1000) return `${n} chars`;
  return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k chars`;
}

/** A menu of the agent CLIs a thread or checkpoint can be opened in. `trigger` is the
 *  button element; each pick calls `onPick` with the program name. */
export function AgentMenu({
  trigger,
  label,
  onPick,
}: {
  trigger: ReactElement;
  label: string;
  onPick: (program: string) => void;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger render={trigger} />
      <DropdownMenuContent align="end" className="min-w-40">
        <MenuPrimitive.Group>
          <DropdownMenuLabel>{label}</DropdownMenuLabel>
          {OPEN_TARGETS.map((t) => (
            <DropdownMenuItem key={t.program} onClick={() => onPick(t.program)}>
              {t.label}
            </DropdownMenuItem>
          ))}
        </MenuPrimitive.Group>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** Copy a known string, with the button itself confirming (the text never leaves view). */
export function CopyButton({
  text,
  label = "Copy",
  disabled,
}: {
  text: string;
  label?: string;
  disabled?: boolean;
}) {
  const [state, setState] = useState<"idle" | "copied" | "failed">("idle");
  const [error, setError] = useState<string | null>(null);
  return (
    <>
      <Button
        size="xs"
        variant="outline"
        disabled={disabled}
        onClick={() => {
          copyText(text).then(
            () => {
              setError(null);
              setState("copied");
              setTimeout(() => setState("idle"), 1500);
            },
            (e) => {
              setState("failed");
              setError(errorText(e));
            },
          );
        }}
      >
        {state === "copied" ? (
          <Check data-icon="inline-start" />
        ) : (
          <Copy data-icon="inline-start" />
        )}
        {state === "copied" ? "Copied" : label}
      </Button>
      {state === "failed" && error && (
        <span role="alert" className="text-xs text-destructive">
          Couldn't copy: {error}
        </span>
      )}
    </>
  );
}
