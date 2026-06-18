import type * as React from "react";
import { type VariantProps, cva } from "class-variance-authority";
import { cn } from "@/lib/utils";

// Catalogue tag — small, monospaced, lettered. The recurring "filed under" mark.
const badgeVariants = cva(
  "inline-flex items-center gap-1.5 rounded-sm border px-2 py-0.5 font-mono text-[0.68rem] uppercase tracking-[0.12em]",
  {
    variants: {
      variant: {
        outline: "border-border text-muted-foreground",
        solid: "border-primary/30 bg-primary/12 text-link",
        accent: "border-accent/40 bg-accent/15 text-accent",
      },
    },
    defaultVariants: { variant: "outline" },
  },
);

export interface BadgeProps
  extends React.HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ variant }), className)} {...props} />;
}
