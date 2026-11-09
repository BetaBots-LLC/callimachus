import { useState } from "react";
import type { Conflict } from "../../lib/api";
import { Button } from "@/components/ui/button";
import { dateTitle, factDate } from "./FactTruth";

/**
 * Review conflicts' results: pairs of decisions the model read as a reversal or a conflict,
 * each side dated. Nothing is stored until the user acts: "Newer replaces older" links the
 * newer decision to the older one (the older leaves memory and shows under Replaced), "Both
 * stand" only drops the pair from this list, and Hide hides one side as before.
 *
 * When both sides carry the same date (two decisions from one session share its start), which
 * one is newer is only the model's guess, which the backend does not trust either. Each side
 * then gets its own "Keep this one" instead, so the user picks the survivor knowingly.
 */

/** A decision's text cut to fit a button title. */
function excerpt(text: string, max = 60): string {
  const t = text.trim().replace(/\s+/g, " ");
  return t.length > max ? `${t.slice(0, max - 1).trimEnd()}…` : t;
}
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
        // Same timestamp: no date says which came later, so don't offer "Newer replaces older".
        const sameTime = sides[0].at === sides[1].at;
        const keep = async (winner: (typeof sides)[number]) => {
          const loser = winner === sides[0] ? sides[1] : sides[0];
          if (await onReplace(winner.id, loser.id)) settle(key);
        };
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
                <span
                  className="w-24 shrink-0 tabular-nums text-muted-foreground"
                  title={dateTitle(d.at)}
                >
                  {factDate(d.at)}
                  {d.newer && !sameTime && (
                    <span className="block text-[0.65rem] uppercase tracking-wide">Newer</span>
                  )}
                </span>
                <span className="flex-1">{d.text}</span>
                {sameTime && (
                  <button
                    type="button"
                    title={`Keep "${excerpt(d.text)}"; "${excerpt((d === sides[0] ? sides[1] : sides[0]).text)}" moves to Replaced`}
                    onClick={() => keep(d)}
                    className="shrink-0 cursor-pointer rounded-sm text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50"
                  >
                    Keep this one
                  </button>
                )}
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
              {!sameTime && (
                <Button
                  size="xs"
                  variant="outline"
                  title={`Keep "${excerpt(newer.text)}" (${factDate(newer.at)}); "${excerpt(older.text)}" (${factDate(older.at)}) moves to Replaced`}
                  onClick={() => keep(newer)}
                >
                  Newer replaces older
                </Button>
              )}
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
