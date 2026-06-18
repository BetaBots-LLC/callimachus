import { Apple, Download, Monitor, Terminal } from "lucide-react";
import type { ComponentType } from "react";
import { RELEASES_URL } from "@/lib/site";
import type { Release } from "@/server/releases";
import { buttonVariants } from "@/components/ui/button";
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
    format: "Universal .dmg",
    note: "Apple Silicon + Intel",
    href: (r) => r.assets.mac_arm,
    icon: Apple,
  },
  {
    no: "02",
    os: "Windows",
    format: "Installer .msi",
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
      <ul className="border-t border-border">
        {ROWS.map((row) => {
          const Icon = row.icon;
          return (
            <li
              key={row.no}
              className="group flex flex-wrap items-center gap-4 border-b border-border py-5 transition-colors hover:bg-card/60"
            >
              <span className="hidden w-8 font-mono text-xs text-muted-foreground sm:block">
                № {row.no}
              </span>
              <Icon className="size-5 text-muted-foreground transition-colors group-hover:text-link" />
              <div className="min-w-0 flex-1">
                <p className="font-display text-lg text-foreground">{row.os}</p>
                <p className="font-mono text-xs text-muted-foreground">
                  {row.format} · {row.note}
                </p>
              </div>
              <a
                href={row.href(release)}
                className={cn(buttonVariants({ variant: "outline", size: "sm" }))}
              >
                <Download />
                Download
              </a>
            </li>
          );
        })}
      </ul>

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
