import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, SOURCE_LABELS, type CleanupRow } from "../lib/api";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatTime } from "../lib/format";
import { useAppForm } from "@/lib/form";
import { ChevronLeft, ChevronRight, Loader2 } from "lucide-react";

/** A cleanup row plus its form-managed selection flag. */
type Row = CleanupRow & { selected: boolean };

const PAGE_SIZE = 12;
const FETCH_LIMIT = 500; // oldest N threads; paginate client-side over these

const AGES: { value: string; label: string; days: number | null }[] = [
  { value: "all", label: "Any age", days: null },
  { value: "30", label: "Older than 30 days", days: 30 },
  { value: "90", label: "Older than 90 days", days: 90 },
  { value: "182", label: "Older than 6 months", days: 182 },
  { value: "365", label: "Older than 1 year", days: 365 },
];

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

export function CleanupCard() {
  const queryClient = useQueryClient();
  const [age, setAge] = useState("all");
  const [page, setPage] = useState(0);
  const [err, setErr] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);

  const days = AGES.find((a) => a.value === age)?.days ?? null;
  const before = days ? Math.floor(Date.now() / 1000) - days * 86400 : undefined;

  const rows = useQuery({
    queryKey: ["cleanup", before],
    queryFn: () => api.cleanupCandidates({ before, limit: FETCH_LIMIT }),
    staleTime: 30_000,
  });

  // Form holds the full list + a per-row `selected` flag (TanStack array field).
  const form = useAppForm({ defaultValues: { rows: [] as Row[] } });

  // Re-seed the form + reset paging whenever the query data changes.
  useEffect(() => {
    if (rows.data) {
      form.reset({ rows: rows.data.map((r) => ({ ...r, selected: false })) });
      setConfirming(false);
      setPage(0);
    }
    // form identity is stable; depend only on the data
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rows.data]);

  const total = rows.data?.length ?? 0;
  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const safePage = Math.min(page, pageCount - 1);
  const start = safePage * PAGE_SIZE;

  function invalidateAll() {
    for (const key of ["db_stats", "index_stats", "cleanup", "results", "recent"]) {
      queryClient.invalidateQueries({ queryKey: [key] });
    }
  }

  const del = useMutation({
    mutationFn: (ids: number[]) => api.deleteThreads(ids),
    onMutate: () => setErr(null),
    onSuccess: (removed) => {
      setConfirming(false);
      invalidateAll();
      void rows.refetch();
      if (removed === 0) setErr("No threads were deleted.");
    },
    onError: (e) => setErr(String(e)),
  });
  const vacuum = useMutation({
    mutationFn: api.vacuumDb,
    onMutate: () => setErr(null),
    onSuccess: invalidateAll,
    onError: (e) => setErr(String(e)),
  });

  /** Toggle selection for the rows on the current page only. */
  function setPageSelected(value: boolean) {
    form.setFieldValue(
      "rows",
      form.state.values.rows.map((r, i) =>
        i >= start && i < start + PAGE_SIZE ? { ...r, selected: value } : r,
      ),
    );
    setConfirming(false);
  }

  function onDeleteClick() {
    const ids = form.state.values.rows.filter((r) => r.selected).map((r) => r.id);
    if (ids.length === 0 || del.isPending) return;
    if (!confirming) {
      setConfirming(true);
      return;
    }
    del.mutate(ids);
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Storage cleanup</CardTitle>
        <p className="text-sm text-muted-foreground">
          Remove old threads to free space — listed least-recently-active first. Deletes cascade to
          messages, search, and embeddings.
        </p>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex items-center gap-2">
          <Select value={age} onValueChange={(v) => setAge(v ?? "all")}>
            <SelectTrigger size="sm" className="w-52">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {AGES.map((a) => (
                <SelectItem key={a.value} value={a.value}>
                  {a.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button
            size="sm"
            variant="ghost"
            className="ml-auto"
            onClick={() => vacuum.mutate()}
            disabled={vacuum.isPending}
            title="Rewrite the database to return freed space to disk"
          >
            {vacuum.isPending && <Loader2 className="size-3.5 animate-spin" />}
            {vacuum.isPending ? "Reclaiming…" : "Reclaim space"}
          </Button>
        </div>

        <div className="rounded-lg border">
          <Table className="table-fixed">
            <TableHeader className="[&_th]:h-8 [&_th]:text-[0.68rem] [&_th]:uppercase [&_th]:tracking-wide">
              <TableRow className="hover:bg-transparent">
                <TableHead className="w-9">
                  <form.Subscribe selector={(s) => s.values.rows}>
                    {(formRows) => {
                      const pageRows = formRows.slice(start, start + PAGE_SIZE);
                      const allSel = pageRows.length > 0 && pageRows.every((r) => r.selected);
                      return (
                        <Checkbox
                          checked={allSel}
                          onCheckedChange={(c) => setPageSelected(c === true)}
                          disabled={pageRows.length === 0}
                          aria-label="Select all on this page"
                        />
                      );
                    }}
                  </form.Subscribe>
                </TableHead>
                <TableHead>Thread</TableHead>
                <TableHead className="w-28">Source</TableHead>
                <TableHead className="w-16 text-right">Size</TableHead>
                <TableHead className="w-28 text-right">Last active</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.isLoading && (
                <TableRow className="hover:bg-transparent">
                  <TableCell colSpan={5} className="text-muted-foreground">
                    <span className="inline-flex items-center gap-2">
                      <Loader2 className="size-3.5 animate-spin" /> Loading…
                    </span>
                  </TableCell>
                </TableRow>
              )}
              {!rows.isLoading && total === 0 && (
                <TableRow className="hover:bg-transparent">
                  <TableCell colSpan={5} className="text-muted-foreground">
                    No threads in this range.
                  </TableCell>
                </TableRow>
              )}
              <form.Field name="rows" mode="array">
                {(arrayField) =>
                  arrayField.state.value.slice(start, start + PAGE_SIZE).map((row, j) => {
                    const i = start + j;
                    return (
                      <TableRow key={row.id}>
                        <TableCell>
                          <form.AppField name={`rows[${i}].selected`}>
                            {(f) => <f.CheckboxField />}
                          </form.AppField>
                        </TableCell>
                        <TableCell className="truncate text-foreground">
                          {row.title?.trim() || "(untitled)"}
                        </TableCell>
                        <TableCell>
                          <Badge variant="outline" className="text-[0.62rem] uppercase">
                            {SOURCE_LABELS[row.source] ?? row.source}
                          </Badge>
                        </TableCell>
                        <TableCell className="text-right text-xs tabular-nums text-muted-foreground">
                          {fmtBytes(row.bytes)}
                        </TableCell>
                        <TableCell className="text-right text-xs text-muted-foreground">
                          {formatTime(row.updatedAt)}
                        </TableCell>
                      </TableRow>
                    );
                  })
                }
              </form.Field>
            </TableBody>
          </Table>
        </div>

        {/* Pagination */}
        {total > 0 && (
          <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
            <span>
              {total} thread{total === 1 ? "" : "s"}
              {total >= FETCH_LIMIT ? "+" : ""}
            </span>
            <div className="flex items-center gap-2">
              <Button
                size="xs"
                variant="outline"
                onClick={() => setPage(safePage - 1)}
                disabled={safePage <= 0}
              >
                <ChevronLeft className="size-3.5" />
                Prev
              </Button>
              <span>
                Page {safePage + 1} of {pageCount}
              </span>
              <Button
                size="xs"
                variant="outline"
                onClick={() => setPage(safePage + 1)}
                disabled={safePage >= pageCount - 1}
              >
                Next
                <ChevronRight className="size-3.5" />
              </Button>
            </div>
          </div>
        )}

        {err && <div className="text-xs text-destructive">{err}</div>}

        <form.Subscribe selector={(s) => s.values.rows}>
          {(formRows) => {
            const sel = formRows.filter((r) => r.selected);
            const bytes = sel.reduce((n, r) => n + r.bytes, 0);
            return (
              <div className="flex items-center justify-between gap-2 border-t pt-3">
                <span className="text-xs text-muted-foreground">
                  {confirming
                    ? `Delete ${sel.length} permanently? This can't be undone.`
                    : sel.length > 0
                      ? `${sel.length} selected · ${fmtBytes(bytes)}`
                      : "Nothing selected"}
                </span>
                <div className="flex shrink-0 gap-2">
                  {confirming && !del.isPending && (
                    <Button size="sm" variant="ghost" onClick={() => setConfirming(false)}>
                      Cancel
                    </Button>
                  )}
                  <Button
                    size="sm"
                    variant="destructive"
                    onClick={onDeleteClick}
                    disabled={sel.length === 0 || del.isPending}
                  >
                    {del.isPending && <Loader2 className="size-3.5 animate-spin" />}
                    {del.isPending
                      ? "Deleting…"
                      : confirming
                        ? `Confirm delete (${sel.length})`
                        : `Delete selected${sel.length > 0 ? ` (${sel.length})` : ""}`}
                  </Button>
                </div>
              </div>
            );
          }}
        </form.Subscribe>
      </CardContent>
    </Card>
  );
}
