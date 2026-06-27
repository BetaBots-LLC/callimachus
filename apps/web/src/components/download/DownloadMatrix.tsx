import { Apple, Download, Monitor, Terminal } from "lucide-react";
import type { ComponentType } from "react";
import { RELEASES_URL } from "@/lib/site";
import type { Release } from "@/server/releases";
import { buttonVariants } from "@/components/ui/button";
import {
  Item,
  ItemActions,
  ItemContent,
  ItemDescription,
  ItemGroup,
  ItemMedia,
  ItemTitle,
} from "@/components/ui/item";
import { cn } from "@/lib/utils";

interface Row {
  no: string;
  os: string;
  format: string;
  note: string;
  href: (r: Release) => string;
  icon: ComponentType<{ className?: string }>;
}

const ROWS: Row[] = [
  {
    no: "01",
    os: "macOS",
    format: ".dmg",
    note: "Apple Silicon (M-series)",
    href: (r) => r.assets.mac_arm,
    icon: Apple,
  },
  {
    no: "02",
    os: "Windows",
    format: "Installer .exe",
    note: "x64",
    href: (r) => r.assets.win,
    icon: Monitor,
  },
  {
    no: "03",
    os: "Linux",
    format: "AppImage",
    note: "x64 · portable",
    href: (r) => r.assets.linux_appimage,
    icon: Terminal,
  },
  {
    no: "04",
    os: "Linux",
    format: "Debian .deb",
    note: "x64 · apt-based",
    href: (r) => r.assets.linux_deb,
    icon: Terminal,
  },
];

export function DownloadMatrix({ release }: { release: Release }) {
  return (
    <div>
      {/* A catalogue ledger: hairline-ruled rows in a drawer, framed top and bottom. */}
      <div className="rule-fade" aria-hidden="true" />
      <ItemGroup className="gap-0">
        {ROWS.map((row, i) => {
          const Icon = row.icon;
          return (
            <div key={row.no}>
              {i > 0 && <div className="rule-fade" aria-hidden="true" />}
              <Item
                variant="default"
                className="group/item -mx-3 gap-4 rounded-md px-3 py-4 transition-colors duration-200 ease-[var(--ease-out-quint)] hover:bg-card/60 motion-safe:animate-in motion-safe:fade-in motion-safe:slide-in-from-bottom-1 motion-safe:fill-mode-both"
                style={{ animationDelay: `${i * 60}ms` }}
              >
                <span className="cat-label hidden w-9 shrink-0 self-center tabular-nums transition-colors group-hover/item:text-link sm:block">
                  № {row.no}
                </span>
                <ItemMedia variant="icon" className="text-primary">
                  <Icon className="size-5" />
                </ItemMedia>
                <ItemContent className="gap-0.5">
                  <ItemTitle className="font-display text-lg font-normal leading-snug text-foreground">
                    {row.os}
                  </ItemTitle>
                  <ItemDescription className="font-mono text-xs text-muted-foreground">
                    {row.format} · {row.note}
                  </ItemDescription>
                </ItemContent>
                <ItemActions className="self-center">
                  <a
                    href={row.href(release)}
                    className={cn(
                      buttonVariants({ variant: "outline", size: "sm" }),
                      "group/btn transition-colors group-hover/item:border-link/50 group-hover/item:text-link",
                    )}
                  >
                    <Download className="size-4 transition-transform duration-200 ease-[var(--ease-out-quint)] group-hover/btn:translate-y-0.5" />
                    Download
                  </a>
                </ItemActions>
              </Item>
            </div>
          );
        })}
      </ItemGroup>
      <div className="rule-fade" aria-hidden="true" />

      <p className="mt-6 max-w-prose text-sm leading-relaxed text-muted-foreground">
        {release.fallback
          ? "Showing the latest published build. "
          : `Latest build${release.version !== "latest" ? ` — v${release.version}` : ""}. `}
        The desktop app auto-updates after install. Checksums and release notes live on{" "}
        <a
          href={RELEASES_URL}
          target="_blank"
          rel="noreferrer"
          className="text-link hover:underline"
        >
          GitHub Releases
        </a>
        .
      </p>
    </div>
  );
}
