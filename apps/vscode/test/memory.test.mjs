// The Memory view's vscode-free parts (src/memoryModel.ts, src/calCore.ts), from the built
// `dist/`: run `pnpm --filter callimachus-vscode build:host` first (`pnpm test` in
// apps/vscode does both). Fixtures are synthetic, written by hand from
// schemas/cal/memory.v1.json and issues.v1.json, and validated against them below.

// Dates render in local time; pin it before any Date is made.
process.env.TZ = "UTC";

import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, describe, test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import {
  CalSetupError,
  CalStateError,
  CalVersionError,
  calProblem,
  runCalAt,
} from "../dist/calCore.js";
import {
  EVERY_PROJECT_TEXT,
  MEMORY_PER_KIND,
  WELCOME_STATES,
  WRITE_COMMANDS,
  assertReadOnlyCall,
  childNodes,
  formatDate,
  holdsEveryProject,
  issueNodes,
  issuesArgs,
  issuesProblemText,
  loadIssues,
  loadMemory,
  memoryArgs,
  oneLine,
  parseIssues,
  parseMemory,
  rootNodes,
  rowView,
  stateOfError,
  stateOfMemory,
} from "../dist/memoryModel.js";
import { validate } from "./schema-subset.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const extensionDir = join(here, "..");
const repoRoot = join(extensionDir, "..", "..");
const fixture = (name) => join(here, "fixtures", name);
const readFixture = (name) => readFileSync(fixture(name), "utf8");
const json = (path) => JSON.parse(readFileSync(path, "utf8"));
const schema = (name) => json(join(repoRoot, "schemas", "cal", name));

// Core's no-index lines carry an em-dash; built from its code point so this file has none
// (the repo bans the character in prose, and biome turns a \u escape back into it).
const EM_DASH = String.fromCharCode(0x2014);

// The exact stderr core prints for each setup state (run_and_exit's `cal: {e}`), as the
// Batch L debug `cal` printed them on 2026-11-10; paths are placeholders.
const STDERR = {
  noIndexReadonly: `cal: no index at /home/u/.local/share/dev.shaller.callimachus/index.db ${EM_DASH} open the Callimachus app once to build it (or set CALLIMACHUS_DB)`,
  noIndexWriter: `cal: no index at /tmp/x/index.db ${EM_DASH} open the app to build it first`,
  busy: "cal: the index is busy (an upgrade or a large cleanup may be running); try again shortly",
  noSuchTable: "cal: no such table: facts",
  noSuchColumn: "cal: no such column: f.uid",
  restore:
    "cal: a restore was interrupted before it finished: the index's latest changes are in /tmp/x/index.db.damaged-20261110T000000Z-wal, not next to the index; nothing writes to the index until that restore is finished (open the Callimachus app, which offers it)",
  systemCal: "Usage: cal [general options] [-hjy] [[month] year]",
  unknown: "cal: unknown command 'memory' (run `cal help`)",
};

describe("arguments", () => {
  test("cal memory never gets a positional, whatever the limit", () => {
    for (const n of [undefined, 1, 40, 0, -5, 2.7, 1e9, Number.NaN, Number.POSITIVE_INFINITY]) {
      const args = memoryArgs(n);
      assert.equal(args[0], "memory");
      assert.equal(args[1], "--json", "the first argument after memory is a flag");
      assert.equal(args.length, 4);
      assert.equal(args[2], "-n");
      assert.match(args[3], /^\d+$/);
      assert.doesNotThrow(() => assertReadOnlyCall(args));
    }
    assert.deepEqual(memoryArgs(), ["memory", "--json", "-n", "40"]);
  });

  test("cal issues gets the canonical project, or is skipped", () => {
    assert.deepEqual(issuesArgs("/work/acme/app"), [
      "issues",
      "/work/acme/app",
      "--json",
      "-n",
      "20",
    ]);
    // Without a positional `cal issues` lists every project; a leading dash reads as a flag.
    for (const p of ["", "   ", "-p", "--all"]) assert.equal(issuesArgs(p), null);
    assert.doesNotThrow(() => assertReadOnlyCall(issuesArgs("/work/acme/app")));
  });

  test("cal issues is skipped for a root, the home folder or a folder above it", () => {
    // Its positional is a substring match, so each of these lists every project's errors.
    const home = "/home/u";
    for (const p of ["/", "//", "C:\\", "c:/", "D:", "/home/u", "/home/u/", "/home"]) {
      assert.equal(issuesArgs(p, home), null, p);
      assert.ok(holdsEveryProject(p, home), p);
    }
    for (const p of ["/home/u/app", "/home/uv", "/home/u-old", "/work"]) {
      assert.deepEqual(issuesArgs(p, home)?.slice(0, 2), ["issues", p], p);
    }
    // Drive paths compare without case; without a home only roots are skipped.
    assert.equal(issuesArgs("c:\\users\\u", "C:\\Users\\U\\"), null);
    assert.ok(issuesArgs("/home/u")?.length);
    assert.equal(issuesArgs("/"), null);
    assert.equal(holdsEveryProject("", home), false, "no project is not a broad one");
  });

  test("anything that could write is refused before a process starts", () => {
    const refused = [
      ["memory", "publish"],
      ["memory", "import"],
      ["memory", "import", "--review"],
      ["memory", "review"],
      ["memory", "--json", "publish"],
      ["memory", "--json", "-n", "40", "review"],
      ["memory", "/work/acme/app", "--json"],
      ["memory", "--json", "-n", "forty"],
      ["memory", "--include-superseded", "--json"],
      ["issues"],
      ["issues", "--json", "-n", "20"],
      ["issues", "/work/acme/app", "--json", "-n", "20", "--all"],
      ["issues", "/work/acme/app", "--json", "-n", "twenty"],
      ["search", "x", "--json"],
      [],
      ...WRITE_COMMANDS.map((c) => [c]),
      ...WRITE_COMMANDS.map((c) => ["issues", c, "--json", "-n", "20"]),
    ];
    for (const args of refused) {
      assert.throws(() => assertReadOnlyCall(args), /refused/, JSON.stringify(args));
    }
  });

  test("the write list names every write command and subcommand the repo lists", () => {
    for (const c of [
      "star",
      "tag",
      "distill",
      "done",
      "remember",
      "snapshot",
      "commits",
      "audit-pr",
      "index",
      "watch",
      "embed",
      "backup",
      "trace",
      "publish",
      "import",
      "review",
      "hook",
      "snapshot-hook",
      "session-end",
      "recall-now",
      "recall-error",
    ]) {
      assert.ok(WRITE_COMMANDS.includes(c), c);
    }
  });
});

describe("stderr classification", () => {
  test("each setup message maps to its state", () => {
    assert.equal(calProblem(STDERR.noIndexReadonly), "no-index");
    assert.equal(calProblem(STDERR.noIndexWriter), "no-index");
    assert.equal(calProblem(STDERR.busy), "busy");
    assert.equal(calProblem(STDERR.noSuchTable), "schema-behind");
    assert.equal(calProblem(STDERR.noSuchColumn), "schema-behind");
    assert.equal(calProblem(STDERR.restore), "restore-interrupted");
    assert.equal(calProblem(STDERR.systemCal, "", 64), "system-cal");
    assert.equal(calProblem("", STDERR.systemCal, 1), "system-cal");
    assert.equal(calProblem(STDERR.unknown), null);
    // What the extension matched before, which core never prints.
    assert.equal(calProblem("cal: no index found"), null);
  });

  test("a failure becomes the view's state", () => {
    assert.deepEqual(stateOfError(new CalSetupError("missing", "x")), { state: "missing" });
    assert.deepEqual(stateOfError(new CalSetupError("no-index", "x")), { state: "noIndex" });
    assert.deepEqual(stateOfError(new CalStateError("busy", "x")), { state: "busy" });
    assert.deepEqual(stateOfError(new CalStateError("schema-behind", "x")), {
      state: "schemaBehind",
    });
    assert.deepEqual(stateOfError(new CalStateError("restore-interrupted", "x")), {
      state: "restoreInterrupted",
    });
    const v = stateOfError(new CalVersionError("memory", 2));
    assert.equal(v.state, "error");
    assert.match(v.message, /version 2/);
    assert.deepEqual(stateOfError(new Error("cal: boom\n  at line two")), {
      state: "error",
      message: "cal: boom at line two",
    });
  });

  test("under Recurring errors a failure is one line that names no write command", () => {
    const lines = [
      new CalStateError("busy", "x"),
      new CalStateError("schema-behind", "x"),
      new CalStateError("restore-interrupted", "x"),
      new CalSetupError("no-index", "x"),
      new Error("cal timed out after 30000 ms"),
    ].map(issuesProblemText);
    assert.match(lines[1], /open the Callimachus app once to upgrade it/i);
    assert.match(lines[4], /timed out/);
    for (const line of lines) {
      assert.doesNotMatch(line, /\n/);
      assert.doesNotMatch(line, /\bcal (index|distill|memory|embed|backup)\b/);
    }
  });
});

describe("schemas and fixtures", () => {
  const memorySchema = schema("memory.v1.json");
  const issuesSchema = schema("issues.v1.json");

  test("the versioned fixtures validate against the checked-in schemas", () => {
    for (const name of ["memory.v1.json", "memory.empty.v1.json"]) {
      assert.deepEqual(validate(memorySchema, json(fixture(name))), [], name);
    }
    for (const name of ["issues.v1.json", "issues.empty.v1.json"]) {
      assert.deepEqual(validate(issuesSchema, json(fixture(name))), [], name);
    }
  });

  test("the validator is not vacuous", () => {
    const mem = json(fixture("memory.v1.json"));
    delete mem.project;
    mem.decisions[0].status = "maybe";
    delete mem.gotchas[0].threadId;
    const errors = validate(memorySchema, mem);
    assert.ok(errors.some((e) => /missing required project/.test(e)));
    assert.ok(errors.some((e) => /status/.test(e)));
    assert.ok(errors.some((e) => /missing required threadId/.test(e)));
    const issues = json(fixture("issues.v1.json"));
    delete issues.results[1].fix;
    issues.results[0].fix.subject = 3;
    const ie = validate(issuesSchema, issues);
    assert.ok(ie.some((e) => /missing required fix/.test(e)));
    assert.ok(ie.some((e) => /matches none of anyOf/.test(e)));
    // The bare shapes predate versioning, so they are not v1 payloads.
    assert.notDeepEqual(validate(memorySchema, json(fixture("memory.bare.json"))), []);
  });
});

describe("parsing", () => {
  test("cal memory, versioned", () => {
    const mem = parseMemory(readFixture("memory.v1.json"));
    assert.equal(mem.project, "/work/acme/app");
    assert.equal(mem.decisions.length, 3);
    assert.equal(mem.gotchas.length, 3);
    assert.equal(mem.openTodos.length, 1);
    assert.equal(mem.pendingCount, 12);
    assert.equal(stateOfMemory(mem), "ready");
  });

  test("cal memory, the bare shape an older cal prints", () => {
    const mem = parseMemory(readFixture("memory.bare.json"));
    assert.equal(mem.decisions.length, 1);
    assert.equal(mem.decisions[0].status, undefined);
    assert.equal(stateOfMemory(mem), "ready");
  });

  test("an empty memory is the empty state", () => {
    assert.equal(stateOfMemory(parseMemory(readFixture("memory.empty.v1.json"))), "empty");
  });

  test("a newer schema_version is refused", () => {
    assert.throws(() => parseMemory(readFixture("memory.v2.json")), CalVersionError);
    assert.throws(() => parseIssues(readFixture("issues.v2.json")), CalVersionError);
  });

  test("output that is not a memory answer is an error", () => {
    assert.throws(() => parseMemory("not json"), /wasn't valid JSON/);
    assert.throws(() => parseMemory("[]"), /isn't an object/);
    assert.throws(() => parseMemory('{"schema_version":1}'), /without a project's memory/);
  });

  test("cal issues, versioned, bare and empty", () => {
    const issues = parseIssues(readFixture("issues.v1.json"));
    assert.equal(issues.length, 3);
    assert.equal(issues[0].fix.shortSha, "3f9c2ab");
    const bare = parseIssues(readFixture("issues.bare.json"));
    assert.equal(bare.length, 1);
    assert.equal(bare[0].threadIds, undefined);
    assert.deepEqual(parseIssues(readFixture("issues.empty.v1.json")), []);
  });
});

/** Every node under `nodes`, depth first. */
function walk(nodes, issues) {
  const out = [];
  for (const node of nodes) {
    out.push(node);
    const kids = node.type === "errors" ? issueNodes(issues) : childNodes(node);
    out.push(...walk(kids, issues));
  }
  return out;
}

describe("rows", () => {
  const mem = parseMemory(readFixture("memory.v1.json"));
  const issues = parseIssues(readFixture("issues.v1.json"));
  const fact = (id) =>
    rowView({
      type: "fact",
      fact: [...mem.decisions, ...mem.gotchas, ...mem.openTodos].find((f) => f.id === id),
    });

  test("the top rows are the three groups, then Recurring errors", () => {
    const roots = rootNodes(mem);
    assert.deepEqual(
      roots.map((n) => rowView(n).label),
      ["Decisions", "Gotchas", "Open TODOs", "Recurring errors"],
    );
    assert.deepEqual(
      roots.map((n) => rowView(n).description),
      ["3", "3", "1", undefined],
    );
    assert.equal(rowView(roots[3], 3).description, "3");
    assert.equal(rowView(roots[3]).collapsible, "collapsed", "fetched on expand");
  });

  test("a group that reached cal memory's cap says so instead of reading as a total", () => {
    const facts = (n) =>
      Array.from({ length: n }, (_, i) => ({ ...mem.decisions[0], id: 1000 + i, text: `d${i}` }));
    const full = rowView({ type: "group", group: "decisions", facts: facts(MEMORY_PER_KIND) });
    assert.equal(full.description, `${MEMORY_PER_KIND}+`);
    assert.match(full.tooltip, new RegExp(`first ${MEMORY_PER_KIND} .*there may be more`));
    const below = rowView({ type: "group", group: "gotchas", facts: facts(MEMORY_PER_KIND - 1) });
    assert.equal(below.description, String(MEMORY_PER_KIND - 1));
    assert.equal(below.tooltip, undefined);
  });

  test("no project, no Recurring errors (cal issues would list every project)", () => {
    for (const project of ["", "  "]) {
      const roots = rootNodes({ ...mem, project });
      assert.deepEqual(
        roots.map((n) => n.type),
        ["group", "group", "group"],
      );
    }
  });

  test("a folder that holds every project says why it has no Recurring errors", () => {
    for (const [project, home] of [
      ["/home/u", "/home/u"],
      ["/", undefined],
      ["/home", "/home/u/"],
    ]) {
      const roots = rootNodes({ ...mem, project }, home);
      assert.deepEqual(
        roots.map((n) => n.type),
        ["group", "group", "group", "info"],
        project,
      );
      const why = rowView(roots[3]);
      assert.equal(why.label, EVERY_PROJECT_TEXT);
      assert.equal(why.command, undefined);
    }
    const empty = parseMemory(readFixture("memory.empty.v1.json"));
    assert.deepEqual(
      rootNodes({ ...empty, project: "/home/u" }, "/home/u").map((n) => n.type),
      ["empty", "info"],
    );
    assert.equal(rootNodes(mem, "/home/u").at(-1).type, "errors", "a project under home");
  });

  test("an empty memory keeps Recurring errors reachable (they need no distilling)", () => {
    const empty = parseMemory(readFixture("memory.empty.v1.json"));
    const roots = rootNodes(empty);
    assert.deepEqual(
      roots.map((n) => n.type),
      ["empty", "errors"],
    );
    const notice = rowView(roots[0]);
    assert.match(notice.label, /^Nothing remembered yet/);
    assert.equal(notice.icon, "info");
    assert.equal(notice.command, undefined);
    assert.equal(rowView(roots[1]).collapsible, "collapsed");
    // With no project to scope errors to there are no rows, so the `empty` welcome shows.
    assert.deepEqual(rootNodes({ ...empty, project: "" }), []);
  });

  test("Recurring errors says it also lists projects whose path contains this one", () => {
    // cal issues matches its positional as a substring (cmd_issues), cal memory exactly.
    const tip = rowView(rootNodes(mem)[3]).tooltip;
    assert.match(tip, /every project whose path contains \/work\/acme\/app /);
    assert.match(tip, /\/work\/acme\/app-web/);
  });

  test("a fact row: text, decided date and source, status icon, opens its session", () => {
    const r = fact(101);
    assert.equal(r.label, "Keep the local cache in SQLite with WAL mode, not a JSON file.");
    assert.equal(r.description, "2026-01-01 · Claude Code");
    assert.equal(r.icon, "circle-small-filled");
    assert.deepEqual(r.command, {
      command: "callimachus.openThread",
      title: "Open session",
      arguments: [7, "Pick a cache store"],
    });
    assert.match(r.tooltip, /Reaffirmed by 2 other sessions/);
    assert.match(r.tooltip, /Session: Pick a cache store/);
    assert.equal(fact(102).icon, "pinned");
    assert.equal(fact(102).command.arguments[1], null);
    assert.equal(fact(103).icon, "question");
    assert.match(fact(103).tooltip, /Status: disputed/);
    assert.match(fact(103).tooltip, /Disputed by: Never retry uploads/);
    // Flagged outranks pinned for the icon; the tooltip says both.
    assert.equal(fact(201).icon, "warning");
    assert.match(fact(201).tooltip, /Status: flagged stale or wrong\nPinned/);
    assert.equal(fact(202).description, "2026-09-09 · recorded");
    assert.equal(fact(203).description, "2026-10-02 · from the repo");
    assert.equal(fact(301).description, "2026-10-28 · Copilot CLI");
    // No originRef path in what the row shows.
    assert.doesNotMatch(fact(203).tooltip, /\.callimachus/);
  });

  test("a bare fact row falls back to its created date and no source", () => {
    const bare = parseMemory(readFixture("memory.bare.json"));
    const r = rowView({ type: "fact", fact: bare.decisions[0] });
    assert.equal(r.description, "2026-01-01");
    assert.equal(r.icon, "pinned");
    assert.equal(rowView({ type: "fact", fact: bare.gotchas[0] }).icon, "circle-small-filled");
  });

  test("an error row: count and example, with its fix and playbook below", () => {
    const [first, second, third] = issueNodes(issues);
    const r = rowView(first);
    assert.equal(r.label, "error[E0425]: cannot find value `conn` in this scope");
    assert.equal(r.description, "7× · last 2026-10-28");
    assert.equal(r.collapsible, "collapsed");
    assert.deepEqual(r.command.arguments, [42, null], "the newest sighting");
    const kids = childNodes(first).map((n) => rowView(n));
    assert.deepEqual(
      kids.map((k) => k.label),
      ["3f9c2ab fix: pass the connection through", oneLine(issues[0].playbook)],
    );
    assert.deepEqual(kids[0].command.arguments, [42, null]);
    assert.equal(kids[0].icon, "git-commit");
    assert.equal(kids[1].command, undefined, "a playbook has no session of its own");
    assert.equal(rowView(second).collapsible, "none");
    assert.deepEqual(childNodes(second), []);
    assert.deepEqual(
      childNodes(third).map((n) => rowView(n).label),
      ["a1b2c3d"],
    );
    // An older cal's bare clusters name no session: nothing to open.
    const [bare] = issueNodes(parseIssues(readFixture("issues.bare.json")));
    assert.equal(rowView(bare).command, undefined);
    assert.equal(rowView(bare).collapsible, "none");
  });

  test("no recurring errors is one info row", () => {
    const [only, ...rest] = issueNodes([]);
    assert.equal(rest.length, 0);
    assert.equal(rowView(only).icon, "info");
    assert.equal(rowView(only).command, undefined);
  });

  test("every row's ids are unique and its only command opens a session", () => {
    const rows = walk(rootNodes(mem), issues).map((n) => rowView(n));
    assert.equal(new Set(rows.map((r) => r.id)).size, rows.length);
    for (const r of rows) {
      if (r.command) assert.equal(r.command.command, "callimachus.openThread", r.id);
    }
  });

  test("labels are one line and capped; dates are local days", () => {
    assert.equal(oneLine("a\n\tb   c"), "a b c");
    const long = oneLine("x".repeat(500));
    assert.equal(long.length, 160);
    assert.ok(long.endsWith("…"));
    assert.equal(formatDate(1767268800), "2026-01-01");
    assert.equal(formatDate(0), undefined);
    assert.equal(formatDate(undefined), undefined);
  });
});

describe("package.json", () => {
  const pkg = json(join(extensionDir, "package.json"));
  const c = pkg.contributes;

  test("a tree view named Memory next to the History webview", () => {
    const views = c.views.callimachus;
    assert.deepEqual(
      views.map((v) => v.id),
      ["callimachus.sidebar", "callimachus.memory"],
    );
    const memory = views.find((v) => v.id === "callimachus.memory");
    assert.equal(memory.name, "Memory");
    assert.equal(memory.type, undefined, "a tree, the default");
  });

  test("a welcome text for every state the view can be in, and only those", () => {
    const welcome = c.viewsWelcome.filter((w) => w.view === "callimachus.memory");
    const states = welcome.map((w) => {
      const m = /^callimachus\.memoryState == (\w+)$/.exec(w.when);
      assert.ok(m, w.when);
      return m[1];
    });
    assert.deepEqual([...states].sort(), [...WELCOME_STATES].sort());
    for (const w of welcome) {
      // Remedies are the app, a download page, a setting or Refresh: never a write command.
      for (const [, target] of w.contents.matchAll(/\]\(([^)]+)\)/g)) {
        assert.ok(
          target === "command:callimachus.refreshMemory" ||
            target === "https://callimachus.app/download",
          target,
        );
      }
      assert.doesNotMatch(w.contents, /\bcal (index|distill|memory|embed|backup|remember)\b/);
      assert.ok(!w.contents.includes(EM_DASH));
    }
  });

  test("the view's only menu entry is Refresh", () => {
    const forView = Object.entries(c.menus).flatMap(([menu, entries]) =>
      entries.filter((e) => /callimachus\.memory\b/.test(e.when ?? "")).map((e) => [menu, e]),
    );
    assert.deepEqual(
      forView.map(([menu, e]) => [menu, e.command]),
      [["view/title", "callimachus.refreshMemory"]],
    );
    assert.ok(c.commands.some((x) => x.command === "callimachus.refreshMemory"));
  });

  test("the tests stay out of the VSIX", () => {
    const ignore = readFileSync(join(extensionDir, ".vscodeignore"), "utf8").split("\n");
    assert.ok(ignore.includes("test/**"));
  });
});

describe("through a fake cal process", { skip: process.platform === "win32" }, () => {
  const dir = mkdtempSync(join(tmpdir(), "callimachus-vscode-test-"));
  after(() => rmSync(dir, { recursive: true, force: true }));
  const bin = join(dir, "cal");
  writeFileSync(
    bin,
    `#!/bin/sh\nexec ${JSON.stringify(process.execPath)} ${JSON.stringify(join(here, "fake-cal.mjs"))} "$@"\n`,
  );
  chmodSync(bin, 0o755);
  const argvFile = join(dir, "argv.json");
  const folder = mkdtempSync(join(dir, "workspace-"));

  /** Run with the fake's FAKE_CAL_* variables set, as the extension passes its env on. */
  async function withFake(vars, fn) {
    const keys = Object.keys(vars);
    for (const k of keys) process.env[k] = String(vars[k]);
    process.env.FAKE_CAL_ARGV = argvFile;
    try {
      return await fn((args, opts) => runCalAt(bin, args, opts));
    } finally {
      for (const k of [...keys, "FAKE_CAL_ARGV"]) delete process.env[k];
    }
  }
  const seen = () => json(argvFile);

  test("cal memory runs in the folder with no positional and CAL_JSON_VERSION=1", async () => {
    const mem = await withFake({ FAKE_CAL_STDOUT: fixture("memory.v1.json") }, (run) =>
      loadMemory(run, folder),
    );
    assert.equal(mem.decisions.length, 3);
    const s = seen();
    assert.deepEqual(s.argv, ["memory", "--json", "-n", "40"]);
    assert.equal(realpathSync(s.cwd), realpathSync(folder));
    assert.equal(s.jsonVersion, "1");
  });

  test("cal issues gets the memory answer's project", async () => {
    const issues = await withFake({ FAKE_CAL_STDOUT: fixture("issues.v1.json") }, (run) =>
      loadIssues(run, folder, "/work/acme/app"),
    );
    assert.equal(issues.length, 3);
    assert.deepEqual(seen().argv, ["issues", "/work/acme/app", "--json", "-n", "20"]);
    const none = await withFake({}, (run) => loadIssues(run, folder, ""));
    assert.equal(none, null, "no process for an empty project");
    const home = await withFake({}, (run) =>
      loadIssues(run, folder, "/home/u", { home: "/home/u" }),
    );
    assert.equal(home, null, "no process for the home folder");
  });

  test("each stderr reaches the view as its state", async () => {
    const cases = [
      [STDERR.noIndexReadonly, 1, "noIndex"],
      [STDERR.busy, 1, "busy"],
      [STDERR.noSuchTable, 1, "schemaBehind"],
      [STDERR.noSuchColumn, 1, "schemaBehind"],
      [STDERR.restore, 1, "restoreInterrupted"],
      [STDERR.systemCal, 64, "missing"],
      [STDERR.unknown, 1, "error"],
    ];
    for (const [stderr, code, state] of cases) {
      await withFake({ FAKE_CAL_STDERR: stderr, FAKE_CAL_EXIT: code }, async (run) => {
        await assert.rejects(loadMemory(run, folder), (err) => {
          assert.equal(stateOfError(err).state, state, stderr);
          return true;
        });
      });
    }
  });

  test("a newer cal's output is a version error, not a broken tree", async () => {
    await withFake({ FAKE_CAL_STDOUT: fixture("memory.v2.json") }, async (run) => {
      await assert.rejects(loadMemory(run, folder), CalVersionError);
    });
  });

  test("a cal that never answers is stopped at the timeout", async () => {
    const started = Date.now();
    await withFake({ FAKE_CAL_SLEEP_MS: 10_000 }, async () => {
      await assert.rejects(
        runCalAt(bin, ["issues", "/work/acme/app", "--json", "-n", "20"], { timeoutMs: 300 }),
        (err) => {
          assert.match(issuesProblemText(err), /timed out after 300 ms/);
          return true;
        },
      );
    });
    assert.ok(Date.now() - started < 5_000);
  });

  test("both loads hand their abort signal to cal", async () => {
    const opts = [];
    const run = async (args, o) => {
      opts.push(o);
      return readFixture(args[0] === "memory" ? "memory.v1.json" : "issues.v1.json");
    };
    const { signal } = new AbortController();
    await loadMemory(run, folder, { signal });
    await loadIssues(run, folder, "/work/acme/app", { signal });
    assert.deepEqual(
      opts.map((o) => o.signal === signal),
      [true, true],
    );
  });

  test("an aborted load kills cal (Refresh and dispose abort the old generation)", async () => {
    rmSync(argvFile, { force: true });
    const controller = new AbortController();
    const started = Date.now();
    const runningPid = () => {
      try {
        return seen().pid;
      } catch {
        return undefined; // not written yet, or half written
      }
    };
    let pid;
    await withFake({ FAKE_CAL_SLEEP_MS: 10_000 }, async (run) => {
      const loading = loadIssues(run, folder, "/work/acme/app", { signal: controller.signal });
      // Abort once the fake is running (it writes its argv and pid first).
      for (pid = runningPid(); !pid && Date.now() - started < 5_000; pid = runningPid()) {
        await delay(20);
      }
      controller.abort();
      await assert.rejects(loading, /aborted/);
    });
    assert.ok(Number.isInteger(pid), "the fake cal started");
    const alive = () => {
      try {
        process.kill(pid, 0);
        return true;
      } catch {
        return false;
      }
    };
    while (alive() && Date.now() - started < 5_000) await delay(20);
    assert.equal(alive(), false, "the aborted cal is gone");
    assert.ok(Date.now() - started < 5_000);
  });

  test("a workspace folder that is gone is an error naming it, not a missing cal", async () => {
    // spawn fails with ENOENT for a missing cwd as for a missing binary (Node's docs).
    const gone = join(dir, "deleted-workspace");
    await withFake({ FAKE_CAL_STDOUT: fixture("memory.v1.json") }, async (run) => {
      await assert.rejects(loadMemory(run, gone), (err) => {
        assert.ok(!(err instanceof CalSetupError), "cal.ts keeps its cached binary");
        const { state, message } = stateOfError(err);
        assert.equal(state, "error");
        assert.ok(message.includes(gone), message);
        return true;
      });
    });
    // A missing binary run in a folder that exists is still the missing state.
    await assert.rejects(
      runCalAt(join(dir, "nope", "cal"), memoryArgs(), { cwd: folder }),
      (err) => {
        assert.equal(stateOfError(err).state, "missing");
        return true;
      },
    );
  });

  test("a missing binary is the missing state", async () => {
    await assert.rejects(runCalAt(join(dir, "nope", "cal"), memoryArgs(), {}), (err) => {
      assert.equal(stateOfError(err).state, "missing");
      return true;
    });
  });
});
