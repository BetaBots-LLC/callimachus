import { useState } from "react";
import type { Conflict } from "../../lib/api";
import { Button } from "@/components/ui/button";
import { factDate } from "./FactTruth";

/**
 * Review conflicts' results: pairs of decisions the model read as a reversal or a conflict,
 * each side dated. Nothing is stored until the user acts: "Newer replaces older" links the
 * newer decision to the older one (the older leaves memory and shows under Replaced), "Both
 * stand" only drops the pair from this list, and Hide hides one side as before.
 */
export function ConflictReview({
  conflicts,
  hiddenIds,
  onHide,
  onReplace,
}: {
  conflicts: Conflict[];
  /** Facts hidden since the review ran: a pair with a hidden side is settled. */
  hiddenIds: Set<number>;
  onHide: (id: number) => void;
  /** Link `newerId` as replacing `olderId`. Resolves once the link is stored. */
  onReplace: (newerId: number, olderId: number) => Promise<boolean>;
}) {
  // Pairs settled here (Both stand, or a replace that went through), by "aId-bId".
  const [settled, setSettled] = useState<Set<string>>(() => new Set());
  const settle = (key: string) => setSettled((prev) => new Set(prev).add(key));
  const visible = conflicts.filter(
    (c) => !hiddenIds.has(c.aId) && !hiddenIds.has(c.bId) && !settled.has(`${c.aId}-${c.bId}`),
  );
  if (!visible.length) {
    return (
      <p className="px-1 text-xs text-muted-foreground">
        {conflicts.length ? "All conflicts settled." : "No conflicting decisions found."}
      </p>
    );
  }
  return (
    <div className="space-y-3 rounded-lg border border-amber-500/50 bg-amber-500/5 p-3">
      <div className="text-[0.7rem] font-medium uppercase tracking-wide text-amber-600 dark:text-amber-400">
        Possible conflicts ({visible.length})
      </div>
      {visible.map((c) => {
        const key = `${c.aId}-${c.bId}`;
        const sides = [
          { id: c.aId, text: c.aText, at: c.aDecidedAt, newer: c.newer === "a" },
          { id: c.bId, text: c.bText, at: c.bDecidedAt, newer: c.newer === "b" },
        ];
        const newer = sides.find((s) => s.newer) ?? sides[0];
        const older = sides.find((s) => s !== newer) ?? sides[1];
        return (
          <div key={key} className="space-y-1.5 text-sm">
            <p className="text-muted-foreground">
              <span className="font-medium text-foreground">
                {c.kind === "supersedes" ? "Looks like a reversal." : "Looks like a conflict."}
              </span>{" "}
              {c.reason}
            </p>
            {sides.map((d) => (
              <div
                key={d.id}
                className="flex items-start gap-2 rounded-md border px-2 py-1 text-xs"
              >
                <span className="w-24 shrink-0 tabular-nums text-muted-foreground">
                  {factDate(d.at)}
                  {d.newer && sides[0].at !== sides[1].at && (
                    <span className="block text-[0.65rem] uppercase tracking-wide">Newer</span>
                  )}
                </span>
                <span className="flex-1">{d.text}</span>
                <button
                  type="button"
                  onClick={() => onHide(d.id)}
                  className="shrink-0 cursor-pointer rounded-sm text-muted-foreground outline-none hover:text-destructive focus-visible:ring-2 focus-visible:ring-ring/50"
                >
                  Hide
                </button>
              </div>
            ))}
            <div className="flex flex-wrap gap-1">
              <Button
                size="xs"
                variant="outline"
                title={`Keep the ${factDate(newer.at)} decision; the ${factDate(older.at)} one moves to Replaced`}
                onClick={async () => {
                  if (await onReplace(newer.id, older.id)) settle(key);
                }}
              >
                Newer replaces older
              </Button>
              <Button
                size="xs"
                variant="ghost"
                title="They don't conflict: drop this pair from the list"
                onClick={() => settle(key)}
              >
                Both stand
              </Button>
            </div>
          </div>
        );
      })}
    </div>
  );
}
