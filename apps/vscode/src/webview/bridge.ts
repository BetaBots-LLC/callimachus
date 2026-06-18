// Webview side of the host RPC. `acquireVsCodeApi()` may be called exactly once
// per document, so it is cached in a module singleton. The webview never touches
// Tauri or `cal` directly — every data fetch and action goes through here.

import type {
  ActionName,
  FromWebview,
  InitPayload,
  RpcMap,
  RpcMethod,
  ThreadSummary,
  ToWebview,
} from "../protocol";

interface VsCodeApi {
  postMessage(msg: FromWebview): void;
  getState<T>(): T | undefined;
  setState<T>(state: T): void;
}
declare function acquireVsCodeApi(): VsCodeApi;

let api: VsCodeApi | undefined;
const vscode = (): VsCodeApi => (api ??= acquireVsCodeApi());

let seq = 0;
const pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();
let initHandler: ((init: InitPayload) => void) | null = null;
let refreshHandler: (() => void) | null = null;
let relatedHandler: ((label: string, results: ThreadSummary[]) => void) | null = null;

window.addEventListener("message", (ev: MessageEvent<ToWebview>) => {
  const msg = ev.data;
  switch (msg.kind) {
    case "init":
      initHandler?.(msg.init);
      break;
    case "refresh":
      refreshHandler?.();
      break;
    case "related":
      relatedHandler?.(msg.label, msg.results);
      break;
    case "rpc-result": {
      const p = pending.get(msg.id);
      if (!p) return;
      pending.delete(msg.id);
      if (msg.ok) p.resolve(msg.result);
      else p.reject(new Error(msg.error));
      break;
    }
  }
});

/** Call a host RPC method and await its typed result. */
export function request<M extends RpcMethod>(
  method: M,
  params: RpcMap[M]["params"],
): Promise<RpcMap[M]["result"]> {
  const id = ++seq;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
    vscode().postMessage({ kind: "rpc", id, method, params });
  });
}

/** Fire a host action for a thread (no reply). */
export function action(name: ActionName, id: number, title?: string | null): void {
  vscode().postMessage({ kind: "action", action: name, id, title });
}

export const onInit = (fn: (init: InitPayload) => void): void => void (initHandler = fn);
export const onRefresh = (fn: () => void): void => void (refreshHandler = fn);
export const onRelated = (fn: (label: string, results: ThreadSummary[]) => void): void => {
  relatedHandler = fn;
};
export const ready = (): void => vscode().postMessage({ kind: "ready" });

export const getState = <T>(): T | undefined => vscode().getState<T>();
export const setState = <T>(state: T): void => vscode().setState(state);
