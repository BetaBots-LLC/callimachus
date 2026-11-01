// Webview entry. One bundle serves both surfaces; the host tells us which to
// mount via the init message. Deliberately NOT wrapped in <StrictMode> — its
// double-invoke would call acquireVsCodeApi twice (see bridge.ts) and fire two
// `ready` messages.

import { lazy, Suspense, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import { onInit, ready } from "./bridge";
import type { InitPayload } from "../protocol";
import { SidebarApp } from "./SidebarApp";

// The thread panel is the only surface that renders markdown, and the markdown +
// highlight.js stack is about half the bundle. Loading it as its own chunk keeps it out
// of the sidebar, which every window keeps alive (retainContextWhenHidden).
const ThreadApp = lazy(() => import("./ThreadApp").then((m) => ({ default: m.ThreadApp })));

function Root() {
  const [init, setInit] = useState<InitPayload | null>(null);

  useEffect(() => {
    onInit(setInit);
    ready();
  }, []);

  if (!init) return null;
  if (init.view !== "thread") return <SidebarApp init={init} />;
  return (
    <Suspense fallback={<p className="p-5 text-sm text-muted-foreground">Loading transcript…</p>}>
      <ThreadApp init={init} />
    </Suspense>
  );
}

createRoot(document.getElementById("root")!).render(<Root />);
