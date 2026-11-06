import type { ReactNode } from "react";
import { MemorySetupCard } from "./MemorySetupCard";

/**
 * Gate for a view that only works with LLM distillation on (Ask). When it is off, the tab
 * stays visible and shows what it does plus the one-click memory card, so the feature is
 * discoverable instead of hidden behind a flag. Projects and the Knowledge tab are not
 * gated: TODOs, Remember and harvested memory work without an engine, so they show the
 * card inline instead.
 */
export function KnowledgeGate({
  enabled,
  feature,
  blurb,
  children,
}: {
  enabled: boolean;
  feature: string;
  blurb: string;
  children: ReactNode;
}) {
  if (enabled) return <>{children}</>;
  return (
    // m-auto rather than justify-center: a tall card then scrolls instead of losing its top.
    <div className="flex h-full overflow-y-auto p-6">
      <div className="m-auto flex w-full max-w-lg flex-col items-center gap-4 text-center">
        <div className="space-y-1.5">
          <h2 className="text-lg font-semibold tracking-tight">{feature}</h2>
          <p className="text-sm text-muted-foreground">{blurb}</p>
        </div>
        <MemorySetupCard whenDismissed="button" className="w-full" />
      </div>
    </div>
  );
}
