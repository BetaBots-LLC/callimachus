import { create } from "zustand";
import { useUi } from "../../store/ui";

/** The Project page's sections, in display order. */
export const PROJECT_SECTIONS = [
  "sessions",
  "memory",
  "snapshots",
  "commits",
  "errors",
  "spend",
  "agents",
] as const;
export type ProjectSectionId = (typeof PROJECT_SECTIONS)[number];

// What a first visit shows open: the two sections people come for. The rest stay collapsed,
// and a collapsed section mounts nothing, so it fetches nothing (spend and recurring errors
// each cost a scan of the project's messages).
const DEFAULT_OPEN: ProjectSectionId[] = ["sessions", "memory"];
const STORAGE_KEY = "cal:project-sections";

function readOpen(): Set<ProjectSectionId> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const ids = JSON.parse(raw);
      if (Array.isArray(ids)) {
        return new Set(ids.filter((id) => (PROJECT_SECTIONS as readonly string[]).includes(id)));
      }
    }
  } catch {
    /* storage blocked or a bad value: fall back to the defaults */
  }
  return new Set(DEFAULT_OPEN);
}

function writeOpen(open: Set<ProjectSectionId>) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify([...open]));
  } catch {
    /* storage blocked: the choice lasts for this session only */
  }
}

interface SectionState {
  open: Set<ProjectSectionId>;
  /** The section to scroll into view once the page renders it (a link from Today). */
  focus: ProjectSectionId | null;
  /** Bumped by every reveal, so asking for the section already focused scrolls again. */
  focusSeq: number;
  toggle: (id: ProjectSectionId) => void;
  reveal: (id: ProjectSectionId) => void;
  /** The page scrolled to the section: a later visit opens at the top again. */
  clearFocus: () => void;
}

/** Which Project page sections are expanded (remembered per viewer) and which one a link
 *  asked to show. A store of its own so Today and the palette can open a section without
 *  loading the Project page chunk. */
export const useProjectSections = create<SectionState>((set) => ({
  open: readOpen(),
  focus: null,
  focusSeq: 0,
  toggle: (id) =>
    set((s) => {
      const open = new Set(s.open);
      if (open.has(id)) open.delete(id);
      else open.add(id);
      writeOpen(open);
      return { open };
    }),
  reveal: (id) =>
    set((s) => {
      const open = new Set(s.open).add(id);
      writeOpen(open);
      return { open, focus: id, focusSeq: s.focusSeq + 1 };
    }),
  clearFocus: () => set({ focus: null }),
}));

/** Go to a project's page with one section expanded and scrolled into view, for example
 *  from Today's "recurring errors seen again" or "snapshots captured". */
export function openProjectSection(project: string, section: ProjectSectionId) {
  useProjectSections.getState().reveal(section);
  useUi.getState().openProject(project);
}
