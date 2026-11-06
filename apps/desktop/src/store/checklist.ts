import { create } from "zustand";
import type { OnboardingStep } from "../lib/api";
import { useUi } from "./ui";

/**
 * Whether the setup checklist (Onboarding.tsx) is open in this window. It opens by itself
 * when the index is empty and then stays open, so finishing the index step does not pull the
 * remaining steps away, until the user closes it or goes to a thread or a search (below);
 * the command palette and Settings > General reopen it. A store of its own, outside the lazy Onboarding chunk, so the header,
 * the palette and the memory cards can read it without loading the checklist.
 */
interface ChecklistState {
  open: boolean;
  /** Opened by itself on an empty index, rather than by the user. When every step is done
   *  or skipped a first-run checklist closes itself; one the user reopened stays put. */
  firstRun: boolean;
  /** The step to expand and scroll to (a "Go to setup checklist" link). */
  focus: OnboardingStep | null;
  /** Bumped by every show(), so asking for the step that is already focused scrolls again. */
  focusSeq: number;
  /** Open it from a palette entry, a Settings button or a link, and switch to it. */
  show: (step?: OnboardingStep) => void;
  /** Open it on first run (the index is empty); a no-op once open. */
  openFirstRun: () => void;
  close: () => void;
}

export const useChecklist = create<ChecklistState>((set) => ({
  open: false,
  firstRun: false,
  focus: null,
  focusSeq: 0,
  show: (step) => {
    // The checklist renders in place of the Search view.
    useUi.getState().setView("search");
    set((s) => ({
      open: true,
      firstRun: s.open ? s.firstRun : false,
      focus: step ?? null,
      focusSeq: s.focusSeq + 1,
    }));
  },
  openFirstRun: () => set((s) => (s.open ? {} : { open: true, firstRun: true, focus: null })),
  close: () => set({ open: false, firstRun: false, focus: null }),
}));

// The checklist renders in place of the Search view, so opening a thread (a palette recent
// thread, an Ask citation, a Projects fact, Knowledge, Coach) or running a search from the
// palette would otherwise land on the checklist and never show what was asked for. Any of
// those closes it; the palette's "Setup checklist" entry reopens it. selectThread(null)
// (a deleted thread) is not a navigation, and SearchBar is not mounted while the checklist
// shows, so a query change here always comes from outside it.
useUi.subscribe((s, prev) => {
  const openedThread = s.openSeq !== prev.openSeq && s.selectedThreadId !== null;
  const searched = s.query !== prev.query && s.query.trim() !== "";
  if ((openedThread || searched) && useChecklist.getState().open) useChecklist.getState().close();
});
