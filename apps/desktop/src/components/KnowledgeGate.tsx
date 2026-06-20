import type { ReactNode } from "react";
import { useUi } from "../store/ui";
import { Button } from "@/components/ui/button";

/**
 * Gate for the knowledge-powered views (Knowledge / Ask / Projects). When distillation is
 * off, the tab stays visible but shows a teaser + a CTA into Settings — so the feature is
 * discoverable instead of hidden behind a flag.
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
  const setView = useUi((s) => s.setView);
  if (enabled) return <>{children}</>;
  return (
    <div className="mx-auto flex h-full w-full max-w-md flex-col items-center justify-center gap-3 p-6 text-center">
      <h2 className="text-lg font-semibold tracking-tight">{feature}</h2>
      <p className="text-sm text-muted-foreground">{blurb}</p>
      <p className="text-xs text-muted-foreground">
        Turn on the Knowledge layer to use it. Free heuristic TODOs need no key; decisions,
        gotchas, and summaries use local Ollama or your own API key.
      </p>
      <Button className="mt-1" onClick={() => setView("settings")}>
        Enable in Settings
      </Button>
    </div>
  );
}
