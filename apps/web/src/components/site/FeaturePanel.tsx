import type { ComponentType, ReactNode } from "react";
import { cn } from "@/lib/utils";

export function FeaturePanel({
  icon: Icon,
  label,
  title,
  children,
  className,
}: {
  icon: ComponentType<{ className?: string }>;
  label: string;
  title: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex flex-col gap-3 rounded-lg border border-border bg-card p-7 transition-colors hover:border-foreground/15 sm:p-8",
        className,
      )}
    >
      <div className="flex items-center gap-2">
        <Icon className="size-4 text-primary" />
        <span className="cat-label">{label}</span>
      </div>
      <h3 className="font-display text-xl text-foreground sm:text-2xl">{title}</h3>
      <p className="max-w-[52ch] leading-relaxed text-muted-foreground">{children}</p>
    </div>
  );
}
