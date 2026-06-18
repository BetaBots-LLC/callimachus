import type * as React from "react";
import { type VariantProps, cva } from "class-variance-authority";
import { cn } from "@/lib/utils";

// Letterpress-y buttons: the primary action gets a subtle inset highlight + cast
// shadow so it reads like a stamped key, reinforcing the print/catalogue theme.
export const buttonVariants = cva(
  "inline-flex select-none items-center justify-center gap-2 whitespace-nowrap rounded-md font-medium outline-none transition-[transform,filter,background-color,border-color] duration-150 ease-[var(--ease-out-quint)] focus-visible:ring-2 focus-visible:ring-ring/60 focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:pointer-events-none disabled:opacity-50 [&_svg]:size-[1.1em] [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        primary:
          "bg-primary text-primary-foreground shadow-[inset_0_1px_0_oklch(1_0_0/0.22),0_2px_10px_oklch(0.2_0.06_50/0.45)] hover:brightness-[1.07] active:translate-y-px active:brightness-95",
        accent:
          "bg-accent text-accent-foreground shadow-[inset_0_1px_0_oklch(1_0_0/0.3)] hover:brightness-[1.05] active:translate-y-px",
        outline:
          "border border-border bg-transparent text-foreground hover:border-foreground/30 hover:bg-card active:translate-y-px",
        ghost: "text-foreground hover:bg-card active:translate-y-px",
        link: "text-link underline-offset-4 hover:underline",
      },
      size: {
        sm: "h-8 px-3 text-sm",
        default: "h-10 px-5 text-sm",
        lg: "h-12 px-6 text-[0.95rem]",
      },
    },
    defaultVariants: { variant: "primary", size: "default" },
  },
);

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {}

export function Button({ className, variant, size, ...props }: ButtonProps) {
  return <button className={cn(buttonVariants({ variant, size }), className)} {...props} />;
}
