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
      <ItemGroup>
        {ROWS.map((row) => {
          const Icon = row.icon;
          return (
            <Item key={row.no} variant="outline" className="transition-colors hover:border-link/40">
              <span className="hidden w-8 font-mono text-xs text-muted-foreground sm:block">
                № {row.no}
              </span>
              <ItemMedia variant="icon">
                <Icon className="size-5 text-muted-foreground transition-colors group-hover/item:text-link" />
              </ItemMedia>
              <ItemContent>
                <ItemTitle className="font-display text-lg font-normal text-foreground">
                  {row.os}
                </ItemTitle>
                <ItemDescription className="font-mono text-xs">
                  {row.format} · {row.note}
                </ItemDescription>
              </ItemContent>
              <ItemActions>
                <a
                  href={row.href(release)}
                  className={cn(buttonVariants({ variant: "outline", size: "sm" }), "group/btn")}
                >
                  <Download className="transition-transform duration-200 ease-[var(--ease-out-quint)] group-hover/btn:translate-y-0.5" />
                  Download
                </a>
              </ItemActions>
            </Item>
          );
        })}
      </ItemGroup>

      <p className="mt-5 text-sm text-muted-foreground">
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
