import { Loader2 } from "lucide-react";
import { cn } from "@/lib/utils";

/** Consistent loading state: a spinner + optional label. Pass `className="h-full"`
 *  to center it in a full-height pane. */
export function Loading({ label, className }: { label?: string; className?: string }) {
  return (
    <div
      className={cn(
        "flex items-center justify-center gap-2 p-6 text-sm text-muted-foreground",
        className,
      )}
    >
      <Loader2 className="size-4 animate-spin" />
      {label}
    </div>
  );
}
