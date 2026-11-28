// The telemetry tables (docs/owner.md §3): one definition per table, and its rows
// derived from the hub's buckets. A row's `id` is the KV key where one key is one row,
// and a stable id from the value otherwise (`alpha#12` in `prs`).

import type { Row } from "./diff";
import { age, parseBacklog, parseLanded, parsePrs, parseTasks, stage, stampMs, when } from "./parse";

export const BUCKETS = ["HGL_HOSTS", "HGL_TREES", "HGL_STATE", "HGL_METRICS", "HGL_BUILD", "HGL_USAGE"] as const;
export type Bucket = (typeof BUCKETS)[number];
/** Each bucket's current values, by key. */
export type Raw = Record<Bucket, Map<string, string>>;

export function emptyRaw(): Raw {
  return Object.fromEntries(BUCKETS.map((b) => [b, new Map<string, string>()])) as Raw;
}

/** How a column is shown, filtered and sorted. */
export type Kind = "text" | "long" | "code" | "num" | "rank" | "age";

export interface Column {
  field: string;
  header: string;
  kind: Kind;
}

export interface TableDef {
  id: string;
  caption: string;
  empty: string;
  sort?: [string, "asc" | "desc"];
  columns: Column[];
}

export interface Tab {
  id: string;
  label: string;
  tables: TableDef[];
  /** The tab's label shows its first table's row count. */
  count?: boolean;
}

const c = (field: string, header: string, kind: Kind = "text"): Column => ({ field, header, kind });

export const HOSTS: TableDef = {
  id: "hosts",
  caption: "Live hosts (a host record lapses two minutes after its bridge stops)",
  empty: "No live host.",
  columns: [
    c("host", "Host"), c("seat", "Seat"), c("role", "Role"), c("runtime", "Runtime"), c("model", "Model"),
    c("memcap_gb", "Memory cap (GB)", "num"), c("free_gb", "Free disk (GB)", "num"),
  ],
};
export const TASKS: TableDef = {
  id: "tasks",
  caption: "Registered worktrees",
  empty: "No task in flight.",
  columns: [
    c("seat", "Seat"), c("task", "Issue"), c("work", "Work"), c("stage", "Stage"), c("last", "Last note"),
    c("last_at", "Last note at"), c("host", "Host"), c("age", "Age", "age"), c("stale", "Stale"),
  ],
};
export const PRS: TableDef = {
  id: "prs",
  caption: "Open pull requests",
  empty: "None open.",
  columns: [
    c("pr", "Pull request"), c("head", "Head", "code"), c("state", "State"), c("bot", "Bot"),
    c("threads", "Threads", "num"), c("checks", "Checks"),
  ],
};
export const QUEUE: TableDef = {
  id: "queue",
  caption: "Claimable issues in order, priority first",
  empty: "Nothing claimable.",
  sort: ["pos", "asc"],
  columns: [c("pos", "#", "rank"), c("issue", "Issue"), c("class", "Class"), c("opened", "Opened"), c("title", "Title", "long")],
};
export const LABELS: TableDef = {
  id: "labels",
  caption: "Open issues by label and repository",
  empty: "No backlog reading yet.",
  columns: [c("repo", "Repository"), c("label", "Label"), c("open", "Open", "num")],
};
export const BURNDOWN: TableDef = {
  id: "burndown",
  caption: "Burndown: open, opened, closed and net per day",
  empty: "No samples.",
  sort: ["day", "desc"],
  columns: [
    c("repo", "Repository"), c("day", "Day"), c("open", "Open", "num"), c("opened", "Opened", "num"),
    c("closed", "Closed", "num"), c("net", "Net", "num"),
  ],
};
export const LANDINGS: TableDef = {
  id: "landings",
  caption: "The newest landings first",
  empty: "Nothing landed yet.",
  sort: ["when", "desc"],
  columns: [c("when", "When"), c("pr", "Pull request"), c("merge", "Merge", "code")],
};
export const FLEET: TableDef = {
  id: "fleet",
  caption: "Mode, hold, stale trees and open questions",
  empty: "No reading yet.",
  columns: [c("item", "Item"), c("value", "Value", "long")],
};
export const RUNS: TableDef = {
  id: "runs",
  caption: "Recent build runs",
  empty: "No build run kept.",
  sort: ["started", "desc"],
  columns: [
    c("run", "Run", "code"), c("repo", "Repository"), c("head", "Head", "code"), c("host", "Builder"),
    c("platform", "Platform"), c("status", "Status"), c("started", "Started"), c("wall_s", "Wall (s)", "num"),
    c("errors", "Errors", "num"), c("passed", "Passed", "num"), c("failed", "Failed", "num"),
  ],
};
export const BUILDERS: TableDef = {
  id: "builders",
  caption: "Live builders",
  empty: "No live builder.",
  columns: [
    c("builder", "Builder"), c("platform", "Platform"), c("arch", "Arch"), c("executor", "Executor"),
    c("state", "State"), c("reachable", "Reachable"), c("busy", "Busy", "num"), c("slots", "Slots", "num"),
    c("free_gb", "Free disk (GB)", "num"), c("toolchains", "Toolchains", "long"), c("updated", "Updated"),
  ],
};
export const USAGE: TableDef = {
  id: "usage",
  caption: "Subscription usage, one row per limit of each login",
  empty: "No usage record.",
  columns: [
    c("login", "Login"), c("runtime", "Runtime"), c("host", "Host"), c("plan", "Plan"), c("scope", "Scope"),
    c("window", "Window"), c("used_pct", "Used %", "num"), c("resets", "Resets"), c("observed", "Observed"),
    c("limit", "Limit reached"),
  ],
};

export const TABS: Tab[] = [
  { id: "hosts", label: "Hosts and seats", tables: [HOSTS] },
  { id: "tasks", label: "Tasks", tables: [TASKS], count: true },
  { id: "prs", label: "Pull requests", tables: [PRS], count: true },
  { id: "queue", label: "Queue", tables: [QUEUE], count: true },
  { id: "backlog", label: "Backlog", tables: [LABELS, BURNDOWN] },
  { id: "landings", label: "Landings", tables: [LANDINGS] },
  { id: "fleet", label: "Fleet", tables: [FLEET] },
  { id: "builds", label: "Builds", tables: [RUNS, BUILDERS] },
  { id: "usage", label: "Usage", tables: [USAGE] },
];

export const TABLES: TableDef[] = TABS.flatMap((t) => t.tables);

/** What the rows depend on besides the buckets. */
export interface Context {
  now: number;
  /** A worktree older than this is stale (`stale_after`). */
  staleAfterMs: number;
  /** Open questions by priority, from the console's API (the page cannot read threads). */
  openQuestions?: Record<string, number> | null;
}

/** Runs and landings shown; burndown days per repository. */
export const RUNS_MAX = 40;
export const LANDINGS_MAX = 50;
export const BURNDOWN_DAYS = 14;

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Json = any;

function json(v: string): Json | null {
  try {
    return JSON.parse(v);
  } catch {
    return null;
  }
}

const day = (d: string) => (/^\d{8}$/.test(d) ? `${d.slice(0, 4)}-${d.slice(4, 6)}-${d.slice(6)}` : d);
const opt = <T,>(v: T | undefined | null) => (v === undefined ? null : v);

function sortedKeys(m: Map<string, string>): string[] {
  return [...m.keys()].sort();
}

/** Every table's rows, in display order. */
export function derive(raw: Raw, ctx: Context): Record<string, Row[]> {
  const out: Record<string, Row[]> = {};
  const state = (k: string) => raw.HGL_STATE.get(k) ?? "";

  out.hosts = [];
  out.builders = [];
  for (const key of sortedKeys(raw.HGL_HOSTS)) {
    const v = json(raw.HGL_HOSTS.get(key)!);
    if (!v) continue;
    if (key.startsWith("builder.")) {
      const tools = Object.entries((v.toolchains ?? {}) as Record<string, string>).map(([k, t]) => `${k} ${t}`);
      out.builders.push({
        id: key,
        builder: v.label,
        platform: v.platform,
        arch: v.arch,
        executor: v.proxy ? `${v.executor} via ${v.proxy}` : v.executor,
        state: v.state,
        reachable: v.reachable ? "yes" : v.reason ? `no: ${v.reason}` : "no",
        busy: v.busy,
        slots: v.slots,
        free_gb: opt(v.free_gb),
        toolchains: tools.join(", "),
        updated: when(v.updated_at ?? ""),
      });
      continue;
    }
    const host = { host: v.host, memcap_gb: v.memcap_gb, free_gb: v.work_root_free_gb };
    const seats = (v.seats ?? []) as Json[];
    if (!seats.length) out.hosts.push({ id: key, ...host, seat: null, role: null, runtime: null, model: null });
    for (const s of seats) {
      out.hosts.push({ id: `${key}/${s.seat}`, ...host, seat: s.seat, role: s.role, runtime: s.runtime, model: s.model });
    }
  }

  const last = parseTasks(state("tasks"));
  out.tasks = [];
  let stale = 0;
  for (const key of sortedKeys(raw.HGL_TREES)) {
    const t = json(raw.HGL_TREES.get(key)!);
    if (!t?.task) continue;
    const task = `${t.task.repo}#${t.task.issue}`;
    const l = last.get(task);
    const created = stampMs(t.created_at ?? "");
    const old = created !== null && ctx.now - created > ctx.staleAfterMs;
    stale += old ? 1 : 0;
    out.tasks.push({
      id: key,
      seat: t.seat,
      task,
      work: t.review ? "review" : "implement",
      stage: stage(l, !!t.review),
      last: l ? `${l.kind} ${l.sender}→${l.recipient}` : null,
      last_at: l ? when(l.at) : null,
      host: t.host,
      age: created === null ? null : Math.max(0, Math.floor((ctx.now - created) / 60000)),
      stale: old ? "stale" : null,
    });
  }

  out.prs = parsePrs(state("prs")).rows.map((p) => {
    const pr = `${p.repo}#${p.number}`;
    return {
      id: pr,
      pr,
      head: p.head.slice(0, 8),
      state: p.draft ? "draft" : "open",
      bot: p.bot,
      threads: p.unresolved,
      checks: p.checks,
    };
  });

  const backlog = parseBacklog(state("backlog"));
  out.queue = backlog.queue.map((q, i) => {
    const issue = `${q.repo}#${q.number}`;
    return { id: issue, pos: i + 1, issue, class: q.class, opened: when(q.created), title: q.title };
  });
  out.labels = backlog.labels.map((l) => ({ id: `${l.repo}/${l.label}`, repo: l.repo, label: l.label, open: l.open }));

  const byRepo = new Map<string, string[]>();
  for (const key of sortedKeys(raw.HGL_METRICS)) {
    const dot = key.lastIndexOf(".");
    const repo = key.slice(0, dot);
    byRepo.set(repo, [...(byRepo.get(repo) ?? []), key]);
  }
  out.burndown = [];
  for (const keys of byRepo.values()) {
    for (const key of keys.slice(-BURNDOWN_DAYS).reverse()) {
      const s = json(raw.HGL_METRICS.get(key)!);
      if (!s) continue;
      out.burndown.push({
        id: key,
        repo: s.repo,
        day: day(s.date),
        open: s.open,
        opened: s.opened,
        closed: s.closed,
        net: s.opened - s.closed,
      });
    }
  }

  const seen = new Map<string, number>();
  out.landings = parseLanded(state("landed.log"))
    .slice(0, LANDINGS_MAX)
    .map((l) => {
      // The same line twice (a landing run again) is two rows.
      const n = (seen.get(l.line) ?? 0) + 1;
      seen.set(l.line, n);
      return {
        id: n === 1 ? l.line : `${l.line} (${n})`,
        when: when(l.at),
        pr: l.repo ? `${l.repo} ${l.pr}` : null,
        merge: l.sha ? l.sha.slice(0, 8) : null,
      };
    });

  const q = (p: string) => (ctx.openQuestions ? String(ctx.openQuestions[p] ?? 0) : "?");
  out.fleet = [
    { id: "mode", item: "Mode", value: state("mode").trim() || "normal" },
    { id: "hold", item: "Hold", value: state("hold").trim() || "none" },
    { id: "stale", item: "Stale trees", value: String(stale) },
    { id: "p1", item: "Open questions, p1", value: q("p1") },
    { id: "p2", item: "Open questions, p2", value: q("p2") },
    { id: "p3", item: "Open questions, p3", value: q("p3") },
  ];

  out.runs = [];
  const runKeys = sortedKeys(raw.HGL_BUILD).filter((k) => k.startsWith("run.")).reverse().slice(0, RUNS_MAX);
  for (const key of runKeys) {
    const s = json(raw.HGL_BUILD.get(key)!);
    if (!s) continue;
    out.runs.push({
      id: key,
      run: s.id,
      repo: s.repo,
      head: (s.head ?? "").slice(0, 8),
      host: s.host,
      platform: s.platform,
      status: s.status,
      started: when(s.started_at ?? ""),
      wall_s: Math.floor((s.wall_ms ?? 0) / 1000),
      errors: s.errors_total ?? 0,
      passed: s.tests?.passed ?? 0,
      failed: s.tests?.failed ?? 0,
    });
  }

  out.usage = [];
  for (const key of sortedKeys(raw.HGL_USAGE)) {
    if (key.startsWith("label.")) continue;
    const u = json(raw.HGL_USAGE.get(key)!);
    if (!u) continue;
    // The label in force, not the one the record was written with.
    const label = json(raw.HGL_USAGE.get(`label.${u.account}`) ?? "")?.label;
    const base = {
      login: label ?? u.account,
      runtime: u.runtime,
      host: u.host,
      plan: opt(u.plan),
      observed: when(u.observed_at ?? ""),
      limit: opt(u.limit_reached),
    };
    const buckets = (u.buckets ?? []) as Json[];
    if (!buckets.length) out.usage.push({ id: key, ...base, scope: null, window: null, used_pct: null, resets: null });
    for (const b of buckets) {
      out.usage.push({
        id: `${key}/${b.scope}/${b.window}`,
        ...base,
        scope: b.scope,
        window: b.window,
        used_pct: Math.round(b.used_pct * 10) / 10,
        resets: b.resets_at ? when(b.resets_at) : null,
      });
    }
  }
  return out;
}

/** `prs` read longer ago than this is stale (docs/protocol.md §8). */
export const PRS_STALE_MS = 15 * 60_000;

/** The line above a tab's tables. */
export function notes(raw: Raw, now: number): Record<string, string> {
  const read = (r: string) => (r ? `Read ${when(r)}` : "Read never");
  const prs = parsePrs(raw.HGL_STATE.get("prs") ?? "");
  const backlog = parseBacklog(raw.HGL_STATE.get("backlog") ?? "");
  const at = stampMs(prs.read);
  const stale = at !== null && now - at > PRS_STALE_MS ? " · stale" : "";
  return { prs: read(prs.read) + stale, queue: read(backlog.read), backlog: read(backlog.read) };
}

export { age };
