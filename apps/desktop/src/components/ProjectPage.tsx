import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import { AGGREGATE_STALE_MS } from "../lib/queryClient";
import { useUi } from "../store/ui";
import { AgentViewSection } from "./project/AgentViewSection";
import { AllProjects } from "./project/AllProjects";
import { CommitsSection } from "./project/CommitsSection";
import { ErrorsSection } from "./project/ErrorsSection";
import { MemorySection } from "./project/MemorySection";
import { ALL_PROJECTS, ProjectPicker } from "./project/ProjectPicker";
import { ProjectSection, SectionNote, SectionSkeleton } from "./project/ProjectSection";
import { RecentSessions } from "./project/RecentSessions";
import { fullDate, relativeAge } from "./project/shared";
import { useProjectSections } from "./project/sections";
import { SnapshotsSection } from "./project/SnapshotsSection";
import { SpendSection } from "./project/SpendSection";

/**
 * The Project page: one home per repo. A picker (the remembered project, else the most
 * recently active one) and a stack of collapsible sections, each fetching only while open:
 * recent sessions, memory, snapshots, produced commits, recurring errors, spend, and what
 * agents see when they start here. "All projects" lists every repo with its counts, the
 * open TODOs across them, the week's new decisions and gotchas, and the errors that recur
 * across repos.
 */
export function ProjectPage() {
  const qc = useQueryClient();
  const selectedProject = useUi((s) => s.selectedProject);
  const openProject = useUi((s) => s.openProject);
  // An explicit pick on this visit, which wins over the remembered choice while the write of
  // that pick is still on its way into the cache.
  const [allChoice, setAllChoice] = useState<boolean | null>(null);

  // Refreshed by index:done / distill:done (main.tsx), so revisits reuse the cache.
  const projects = useQuery({
    queryKey: ["projects"],
    queryFn: api.listProjects,
    staleTime: AGGREGATE_STALE_MS,
  });
  // The project picked last time (app_config ui.last_project), or ALL_PROJECTS when that
  // pick was "All projects". An error (an older backend) reads as "none remembered" rather
  // than blocking the page.
  const last = useQuery({
    queryKey: ["last_project"],
    queryFn: api.lastProject,
    staleTime: Number.POSITIVE_INFINITY,
    retry: false,
  });
  // Stable across renders (TanStack Query v5), so effects can depend on it.
  const { mutate: rememberProject } = useMutation({
    mutationFn: (project: string) => api.setLastProject(project),
    onMutate: async (project) => {
      // A first read still in flight would land after this write and put the old value back
      // in the cache, after which picking that old value would look like "no change" and
      // never be written.
      await qc.cancelQueries({ queryKey: ["last_project"] });
      qc.setQueryData(["last_project"], project);
    },
  });

  const known = (p: string | null | undefined): p is string =>
    !!p && !!projects.data?.some((x) => x.project === p);
  // Wait for the remembered project before falling back, or the page would flash the most
  // recent repo and then jump to the remembered one.
  const settled = !projects.isPending && !last.isPending;
  // "All projects" is a remembered choice like a repo, so it survives a trip to another tab
  // and a relaunch. Any navigation to a repo (the picker, a row here, a thread's "Open
  // project", Today) sets selectedProject, which ends it.
  const allMode = settled && selectedProject === null && (allChoice ?? last.data === ALL_PROJECTS);
  const project = !settled
    ? null
    : (selectedProject ??
      (known(last.data) ? last.data : null) ??
      projects.data?.[0]?.project ??
      null);

  // Any way of landing on a project (the picker, a thread's "Open project", Today) is a
  // selection worth remembering. Picking "All projects" clears selectedProject, so even the
  // repo that was showing before it counts as a new selection here.
  useEffect(() => {
    if (selectedProject && qc.getQueryData(["last_project"]) !== selectedProject) {
      rememberProject(selectedProject);
    }
  }, [selectedProject, qc, rememberProject]);

  const pick = (v: string) => {
    if (v === ALL_PROJECTS) {
      setAllChoice(true);
      useUi.setState({ selectedProject: null });
      rememberProject(ALL_PROJECTS);
      return;
    }
    setAllChoice(false);
    openProject(v);
  };

  // A link asked for a section (Today, the palette) but this render shows none: drop the
  // request, or a later pick of any repo would scroll to it unasked.
  const focus = useProjectSections((s) => s.focus);
  const clearFocus = useProjectSections((s) => s.clearFocus);
  const showsSections = settled && !projects.isError && !allMode && !!project;
  useEffect(() => {
    if (settled && !showsSections && focus) clearFocus();
  }, [settled, showsSections, focus, clearFocus]);

  // The sticky header's height, as the scroll margin a revealed section needs to land below
  // it rather than under it. Measured, because the header wraps to two rows at narrow widths.
  const scrollerRef = useRef<HTMLDivElement>(null);
  const headerRef = useRef<HTMLElement>(null);
  useLayoutEffect(() => {
    const scroller = scrollerRef.current;
    const header = headerRef.current;
    if (!scroller || !header) return;
    const measure = () =>
      scroller.style.setProperty("--page-header-h", `${header.getBoundingClientRect().height}px`);
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(measure);
    ro.observe(header);
    return () => ro.disconnect();
  }, []);

  const info = projects.data?.find((p) => p.project === project);

  return (
    <div ref={scrollerRef} className="@container/page h-full overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-6 pb-10">
        <header
          ref={headerRef}
          className="sticky top-0 z-10 flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 border-b bg-background pt-5 pb-3"
        >
          {projects.data && projects.data.length > 0 ? (
            <ProjectPicker
              value={allMode ? ALL_PROJECTS : project}
              projects={projects.data}
              onChange={pick}
            />
          ) : (
            <h1 className="text-sm font-semibold">Projects</h1>
          )}
          {!allMode && info && (
            <p className="min-w-0 truncate text-[13px] text-muted-foreground">
              {info.threadCount.toLocaleString()} {info.threadCount === 1 ? "session" : "sessions"}
              {" · "}
              <span title={fullDate(info.lastActivity)}>
                last active {relativeAge(info.lastActivity)}
              </span>
            </p>
          )}
        </header>

        <div className="pt-3">
          {projects.isError ? (
            <SectionNote tone="error">
              Couldn't load your projects: {String(projects.error)}
            </SectionNote>
          ) : !settled ? (
            <SectionSkeleton rows={4} />
          ) : allMode ? (
            <AllProjects projects={projects.data} onOpen={pick} />
          ) : !project ? (
            <SectionNote>
              No projects yet. A project appears here once an indexed agent session ran inside a
              folder; sessions from chat apps with no working folder stay in Search.
            </SectionNote>
          ) : (
            // Keyed by project: switching repos drops every section's local state (an open
            // Remember form, a pending delete confirm) instead of carrying it over.
            <div key={project} className="divide-y divide-border/70">
              <ProjectSection
                id="sessions"
                title="Recent sessions"
                hint="The last 10, to reopen or resume"
              >
                <RecentSessions project={project} />
              </ProjectSection>
              <ProjectSection id="memory" title="Memory" hint="Decisions, gotchas and TODOs">
                <MemorySection project={project} />
              </ProjectSection>
              <ProjectSection
                id="snapshots"
                title="Snapshots"
                hint="Checkpoints to resume in any agent"
              >
                <SnapshotsSection project={project} />
              </ProjectSection>
              <ProjectSection
                id="commits"
                title="Produced commits"
                hint="What the recent sessions committed"
              >
                <CommitsSection project={project} />
              </ProjectSection>
              <ProjectSection
                id="errors"
                title="Recurring errors"
                hint="Errors that keep coming back here"
              >
                <ErrorsSection project={project} />
              </ProjectSection>
              <ProjectSection id="spend" title="Spend" hint="Estimated, last 30 days and all time">
                <SpendSection project={project} />
              </ProjectSection>
              <ProjectSection
                id="agents"
                title="What agents see"
                hint="The hook text and AGENTS.md block"
              >
                <AgentViewSection project={project} />
              </ProjectSection>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
