// Clipboard writes that work inside the Tauri webviews.
//
// WebKit (macOS WKWebView, Linux WebKitGTK) only allows clipboard writes during a user
// gesture. When the text comes from an async IPC call, a plain
// `await invoke(...); navigator.clipboard.writeText(text)` runs after the gesture has
// ended and is rejected. `ClipboardItem` accepts a *promise* of the data, so the write
// can start synchronously inside the click handler and resolve once the IPC returns.
// That is the path WebKit recommends; the fallbacks cover webviews without it.

/** Copy text that is produced asynchronously. Call this directly from a click handler
 *  (not after an await) so the user gesture is still active. Rejects with the
 *  producer's error, or with a clipboard error if every write path fails. */
export async function copyAsyncText(produce: () => Promise<string>): Promise<void> {
  const text = produce();

  if (typeof ClipboardItem !== "undefined" && navigator.clipboard?.write) {
    try {
      const blob = text.then((t) => new Blob([t], { type: "text/plain" }));
      await navigator.clipboard.write([new ClipboardItem({ "text/plain": blob })]);
      return;
    } catch {
      // Fall through: either the producer failed (re-thrown below with its real
      // message) or this webview rejected the async ClipboardItem.
    }
  }

  await copyText(await text);
}

/** Copy text that is already available. */
export async function copyText(text: string): Promise<void> {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // Fall through to the legacy path.
    }
  }
  if (legacyCopy(text)) return;
  throw new Error("The clipboard isn't available in this window.");
}

/** `execCommand("copy")` through an off-screen textarea: deprecated, but still the only
 *  option on webviews that expose no async clipboard API to scripts. */
function legacyCopy(text: string): boolean {
  const el = document.createElement("textarea");
  el.value = text;
  el.setAttribute("readonly", "");
  el.style.position = "fixed";
  el.style.top = "-1000px";
  el.style.opacity = "0";
  document.body.appendChild(el);
  try {
    el.select();
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    el.remove();
  }
}
