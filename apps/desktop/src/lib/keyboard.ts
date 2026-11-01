import type { KeyboardEvent } from "react";

/**
 * True while an IME (Chinese, Japanese, Korean input) is composing. The Enter that
 * confirms a candidate must not submit the form. WebKit fires `compositionend` before
 * that keydown, so `isComposing` is already false there and the event instead reports
 * the legacy keyCode 229; both checks are needed.
 */
export function isImeComposing(e: KeyboardEvent): boolean {
  return e.nativeEvent.isComposing || e.keyCode === 229;
}

/** Plain Enter (no Shift) that is not confirming an IME candidate: the "send" key. */
export function isSubmitEnter(e: KeyboardEvent): boolean {
  return e.key === "Enter" && !e.shiftKey && !isImeComposing(e);
}
