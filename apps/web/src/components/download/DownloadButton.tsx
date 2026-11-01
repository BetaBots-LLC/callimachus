import { Link } from "@tanstack/react-router";
import { Download } from "lucide-react";
import { buttonVariants } from "@/components/ui/button";
import { type OsKey, osFamily } from "@/lib/os-detect";
import type { Release } from "@/server/releases";
import { cn } from "@/lib/utils";
import { type DownloadPlacement, useTrack } from "@/lib/analytics";

// The primary OS is detected server-side and arrives via loader data, so there's no
// state, effect, or hydration flash: the right button is in the HTML. `placement` only
// labels the (cookieless) download_clicked event.
export function DownloadButton({
  release,
  primaryOs,
  placement,
  showAll = true,
  className,
}: {
  release: Release;
  primaryOs: OsKey;
  placement: DownloadPlacement;
  showAll?: boolean;
  className?: string;
}) {
  const track = useTrack();
  const version = release.version === "latest" ? "" : `v${release.version}`;

  return (
    <div className={cn("flex flex-col items-start gap-2", className)}>
      <a
        href={release.assets[primaryOs]}
        onClick={() => track("download_clicked", { os: primaryOs, placement })}
        className={cn(buttonVariants({ size: "lg" }), "group")}
      >
        <Download className="transition-transform duration-200 ease-[var(--ease-out-quint)] group-hover:translate-y-0.5" />
        Download for {osFamily(primaryOs)}
      </a>
      <p className="font-mono text-xs text-muted-foreground">
        {version && <span className="text-foreground/80">{version}</span>}
        {version && " · "}
        Free · macOS, Windows &amp; Linux
        {showAll && (
          <>
            {" · "}
            <Link to="/download" className="text-link hover:underline">
              all builds
            </Link>
          </>
        )}
      </p>
    </div>
  );
}
