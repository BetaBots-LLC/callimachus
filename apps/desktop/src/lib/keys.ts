import { queryClient } from "./queryClient";

/**
 * Which cached queries show each kind of data, so a write refreshes every view that shows
 * what it changed rather than the ones its author thought of. Hand-written lists drifted:
 * closing a TODO in Projects once left Today's "new TODOs" strip stale for its 5-minute
 * window, and the TODO backfill refreshed Projects but not Today.
 *
 * - memory: decisions, gotchas and TODOs (recorded, edited, pinned, hidden, linked, closed,
 *   imported). agent_view is the "What agents see" preview, rendered from that memory.
 * - memorySettings: the memory switch, engine and auto-distill allowance.
 * - projects: the project list, whose rows count distilled sessions.
 */
export const DOMAIN_KEYS = {
  memory: ["project_memory", "open_todos", "today", "agent_view"],
  memorySettings: ["knowledge_config", "memory_setup", "auto_distill_budget", "distill_status"],
  projects: ["projects"],
} as const;

export type Domain = keyof typeof DOMAIN_KEYS;

/**
 * The mutationKey of a write that updates a memory list optimistically (closing a TODO;
 * pinning, hiding or editing a fact). While one is pending the live refresh (main.tsx) only
 * marks those lists stale: a refetch then reads the index before the write commits and puts
 * the row back until the write settles, and the write waits longest exactly while the
 * watcher holds the write lock, which is when the live refresh fires.
 */
export const MEMORY_WRITE_KEY = ["memory_write"] as const;

/** Whether a [MEMORY_WRITE_KEY] write is pending. */
export function memoryWritePending(): boolean {
  return queryClient.isMutating({ mutationKey: MEMORY_WRITE_KEY }) > 0;
}

/** Keys whose second element is a project, so a write to one project's memory refreshes
 *  only that project's copy of them. */
const PER_PROJECT = new Set<string>(["project_memory", "agent_view"]);

/**
 * Invalidate every query of `domain`; mounted ones refetch, the rest reload when a view
 * mounts them. With `project`, the per-project keys are limited to that project. Resolves
 * once the refetches land (for a caller that moves focus afterwards).
 */
export function invalidateDomain(domain: Domain, opts: { project?: string } = {}): Promise<void> {
  const { project } = opts;
  return Promise.all(
    DOMAIN_KEYS[domain].map((key) =>
      queryClient.invalidateQueries({
        queryKey: project !== undefined && PER_PROJECT.has(key) ? [key, project] : [key],
      }),
    ),
  ).then(() => undefined);
}
