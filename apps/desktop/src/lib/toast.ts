// The app's one notification channel. Toasts are for outcomes the user can't see where
// they are looking: a save that round-trips the keychain, a background job finishing, an
// update landing. A control that already shows its own state (a switch flipping, a field
// keeping its value, a row that updates in place) gets no toast.
//
// Renders through `<Toaster />` (components/ui/toast.tsx, mounted in main.tsx), which reads
// the global Base UI toast manager, so this works from stores and event listeners too.
// The manager only delivers to a mounted provider, so calls made before the first React
// commit (module-level code in main.tsx and the stores) are queued and flushed by the
// Toaster's mount effect; nothing is lost.
import { toast } from "@/components/ui/toast";

export type ToastKind = "success" | "info" | "warning" | "error" | "loading";

export interface ToastOptions {
  description?: string;
  /** Milliseconds until the toast closes itself. `0` keeps it until closed. */
  timeout?: number;
  /** One button on the toast. Clicking it runs `onClick` and closes the toast unless
   *  `keepOpen` is set (for a toast that then updates itself in place). */
  action?: { label: string; onClick: () => void; keepOpen?: boolean };
  /** Reusing an id updates that toast in place instead of stacking another. */
  id?: string;
  onClose?: () => void;
}

/** Errors stay longer: they carry a reason the user may want to read twice. */
const TIMEOUT: Record<ToastKind, number> = {
  success: 4_000,
  info: 5_000,
  warning: 7_000,
  error: 9_000,
  loading: 0,
};

let ready = false;
const queued: (() => void)[] = [];
function deliver(fn: () => void) {
  if (ready) fn();
  else queued.push(fn);
}

/** Called once by `<Toaster />` after it has mounted and subscribed to the manager. */
export function markToasterReady() {
  ready = true;
  for (const fn of queued.splice(0)) fn();
}

let counter = 0;
const nextId = () => `toast-${++counter}`;

function actionProps(id: string, action: ToastOptions["action"]) {
  if (!action) return undefined;
  return {
    children: action.label,
    onClick: () => {
      action.onClick();
      if (!action.keepOpen) toast.close(id);
    },
  };
}

function add(type: ToastKind, title: string, opts: ToastOptions = {}): string {
  // Ids are assigned here, not by the manager, so the action can close its own toast and
  // a queued call still returns an id the caller can update later.
  const id = opts.id ?? nextId();
  deliver(() =>
    toast.add({
      id,
      type,
      title,
      description: opts.description,
      timeout: opts.timeout ?? TIMEOUT[type],
      actionProps: actionProps(id, opts.action),
      onClose: opts.onClose,
    }),
  );
  return id;
}

export const notify = {
  success: (title: string, opts?: ToastOptions) => add("success", title, opts),
  info: (title: string, opts?: ToastOptions) => add("info", title, opts),
  warning: (title: string, opts?: ToastOptions) => add("warning", title, opts),
  error: (title: string, opts?: ToastOptions) => add("error", title, opts),
  /** A spinner toast that stays until `update`d or `close`d. */
  loading: (title: string, opts?: ToastOptions) => add("loading", title, opts),
  /**
   * Change an open toast. Only the keys given change: the manager merges the patch over
   * the toast, so an `undefined` would erase a field. A new `type` also resets the
   * timer to that kind's default unless `timeout` is given; `action: null` removes the
   * button. No-op once the toast has closed.
   */
  update: (
    id: string,
    patch: Partial<{ type: ToastKind; title: string; action: ToastOptions["action"] | null }> &
      Omit<ToastOptions, "id" | "action">,
  ) => {
    const u: Record<string, unknown> = {};
    if (patch.type !== undefined) u.type = patch.type;
    if (patch.title !== undefined) u.title = patch.title;
    if (patch.description !== undefined) u.description = patch.description;
    if (patch.timeout !== undefined) u.timeout = patch.timeout;
    else if (patch.type !== undefined) u.timeout = TIMEOUT[patch.type];
    if (patch.action !== undefined) u.actionProps = actionProps(id, patch.action ?? undefined);
    if (patch.onClose !== undefined) u.onClose = patch.onClose;
    deliver(() => toast.update(id, u));
  },
  close: (id: string) => deliver(() => toast.close(id)),
};

/** The message of a thrown value, without a leading "Error:" the backend may add. */
export function errorText(e: unknown): string {
  const raw = e instanceof Error ? e.message : String(e ?? "");
  return raw.replace(/^Error:\s*/, "").trim() || "unknown error";
}
