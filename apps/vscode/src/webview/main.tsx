// Webview entry. One bundle serves both surfaces; the host tells us which to
// mount via the init message. Deliberately NOT wrapped in <StrictMode> — its
// double-invoke would call acquireVsCodeApi twice (see bridge.ts) and fire two
// `ready` messages.

import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import { onInit, ready } from "./bridge";
import type { InitPayload } from "../protocol";
import { SidebarApp } from "./SidebarApp";
import { ThreadApp } from "./ThreadApp";

function Root() {
  const [init, setInit] = useState<InitPayload | null>(null);

  useEffect(() => {
    onInit(setInit);
    ready();
  }, []);

  if (!init) return null;
  return init.view === "thread" ? <ThreadApp init={init} /> : <SidebarApp init={init} />;
}

createRoot(document.getElementById("root")!).render(<Root />);
