// The telemetry tables (docs/owner.md §3): one definition per table, and its rows
// derived from the hub's buckets. A row's `id` is the KV key where one key is one row,
// and a stable id from the value otherwise (`alpha#12` in `prs`).

import type { Row } from "./diff";
import { age, parseBacklog, parseLanded, parsePrs, parseTitles, parseTrees, relative, span, stage, stampMs, when } from "./parse";

/** Not `HGL_TREES`: its records hold paths; the trees come in `HGL_STATE/tasks`. */
export const BUCKETS = ["HGL_HOSTS", "HGL_STATE", "HGL_METRICS", "HGL_BUILD", "HGL_USAGE"] as const;
export type Bucket = (typeof BUCKETS)[number];
/** The `HGL_STATE` keys the page reads, each watched on its own: the hub's `viewer` user
 *  may read no other (`VIEWER_STATE_KEYS` in crates/proto). */
export const STATE_KEYS = ["prs", "backlog", "landed.log", "tasks", "mode", "hold"] as const;
/** What one watch covers: a whole bucket, or one key of it. */
export interface Source {
  bucket: Bucket;
  key?: string;
}
/** The page's watches: every bucket whole, but `HGL_STATE` key by key. */
export const SOURCES: Source[] = BUCKETS.flatMap((bucket): Source[] =>
  bucket === "HGL_STATE" ? STATE_KEYS.map((key) => ({ bucket, key })) : [{ bucket }],
);
/** Each bucket's current values, by key. */
export type Raw = Record<Bucket, Map<string, string>>;

export function emptyRaw(): Raw {
  return Object.fromEntries(BUCKETS.map((b) => [b, new Map<string, string>()])) as Raw;
}

/** How a column is shown, filtered and sorted. A `stamp` holds the compact UTC stamp
 *  (`20261127T041100Z`, which sorts to the second) and shows the time from now (`12m ago`),
 *  with `2026-11-27 04:11 UTC` as its tooltip. */
export type Kind = "text" | "long" | "code" | "num" | "rank" | "age" | "stamp";

export interface Column {
  field: string;
  header: string;
  kind: Kind;
  /** What the column means: the header's tooltip and the table's key. */
  tip?: string;
  /** The row field holding the link the cell goes to. */
  href?: string;
}

export interface TableDef {
  id: string;
  caption: string;
  empty: string;
  sort?: [string, "asc" | "desc"];
  columns: Column[];
  /** Fields shown only in the detail of a selected row. */
  detail?: Column[];
  /** The columns explained under the caption. */
  explain?: boolean;
}

export interface Tab {
  id: string;
  label: string;
  tables: TableDef[];
  /** The tab's label shows its first table's row count. */
  count?: boolean;
  /** A figure above the tables. */
  chart?: "usage" | "backlog";
}

const c = (field: string, header: string, kind: Kind = "text", more: Partial<Column> = {}): Column => ({
  field,
  header,
  kind,
  ...more,
});

export const HOSTS: TableDef = {
  id: "hosts",
  caption: "Live hosts and their seats (a host lapses two minutes after its bridge stops)",
  empty: "No live host.",
  columns: [
    c("seat", "Seat"),
    c("doing", "Doing now", "long", { href: "doing_url", tip: "Idle, or the task of the turn the seat runs now" }),
    c("since", "Turn started", "stamp"),
    c("role", "Role"), c("host", "Host"), c("runtime", "Runtime"), c("model", "Model"),
    c("memcap_gb", "Memory cap (GB)", "num"), c("free_gb", "Free disk (GB)", "num"),
  ],
};
export const TASKS: TableDef = {
  id: "tasks",
  caption: "Tasks with a worktree; a tree outlives its turns, so 'Turn' says whether one runs now",
  empty: "No task in flight.",
  explain: true,
  sort: ["last_at", "desc"],
  columns: [
    c("task", "Issue", "text", { href: "task_url" }),
    c("title", "Title", "long"),
    c("seat", "Seat"),
    c("work", "Work", "text", { tip: "Implementing the issue, or reviewing its pull request" }),
    c("stage", "Stage", "text", { tip: "From the task's last note: assigned, in progress, ready, in review, sent back, approved, blocked, done" }),
    c("turn", "Turn", "text", { tip: "Running: the seat is in a turn on this task now; idle: the tree waits for a note" }),
    c("turn_since", "Turn started", "stamp"),
    c("last", "Last note", "text", { tip: "The kind of the newest note about the task, and who sent it to whom" }),
    c("last_at", "Last note sent", "stamp"),
    c("age", "Tree age", "age", { tip: "How long the task's worktree has existed" }),
    c("stale", "Stale", "text", { tip: "The tree is older than stale_after" }),
  ],
};
export const PRS: TableDef = {
  id: "prs",
  caption: "Open pull requests",
  empty: "None open.",
  explain: true,
  columns: [
    c("pr", "Pull request", "text", { href: "pr_url" }),
    c("title", "Title", "long"),
    c("state", "State", "text", { tip: "Open, or draft" }),
    c("bot", "Review bot", "text", { tip: "The review bot on this head: clean, findings, reviewing, not reviewed, or at its usage limit" }),
    c("threads", "Unresolved threads", "num", { tip: "Review threads not resolved yet" }),
    c("checks", "CI checks", "text", { tip: "CI checks passed of all, on this head" }),
  ],
  detail: [c("head", "Head", "code"), c("rollup", "Checks rollup"), c("read", "Read", "stamp")],
};
export const QUEUE: TableDef = {
  id: "queue",
  caption: "Claimable issues in the order work is handed out",
  empty: "Nothing claimable.",
  sort: ["pos", "asc"],
  columns: [
    c("pos", "Order", "rank"),
    c("issue", "Issue", "text", { href: "issue_url" }),
    c("title", "Title", "long"),
    c("class", "Class", "text", { tip: "Priority (labelled codex-priority) or ready (labelled codex)" }),
    c("opened", "Opened", "stamp"),
  ],
};
export const LABELS: TableDef = {
  id: "labels",
  caption: "Open issues by label and repository, as a table",
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
  caption: "The newest landings first; select a row for its merge commit",
  empty: "Nothing landed yet.",
  sort: ["when", "desc"],
  columns: [c("when", "Landed", "stamp"), c("pr", "Pull request", "text", { href: "pr_url" }), c("title", "Title", "long")],
  detail: [c("merge", "Merge commit", "code", { href: "merge_url" }), c("line", "Log line", "code")],
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
    c("platform", "Platform"), c("status", "Status"), c("started", "Started", "stamp"),
    c("wall_s", "Took (s)", "num", { tip: "Wall-clock seconds" }),
    c("errors", "Errors", "num"), c("passed", "Tests passed", "num"), c("failed", "Tests failed", "num"),
  ],
};
export const BUILDERS: TableDef = {
  id: "builders",
  caption: "Live builders",
  empty: "No live builder.",
  columns: [
    c("builder", "Builder"), c("platform", "Platform"), c("arch", "Arch"), c("executor", "Executor"),
    c("state", "State"), c("reachable", "Reachable"), c("busy", "Busy", "num"), c("slots", "Slots", "num"),
    c("free_gb", "Free disk (GB)", "num"), c("toolchains", "Toolchains", "long"), c("updated", "Updated", "stamp"),
  ],
};
export const USAGE: TableDef = {
  id: "usage",
  caption: "The same as a table, one row per limit of each login",
  empty: "No usage record.",
  columns: [
    c("login", "Login"), c("scope", "Scope"), c("window", "Window"), c("used_pct", "Used %", "num"),
    c("resets", "Resets", "stamp"),
    c("risk", "Run-out risk", "long", { tip: "The burn rate so far in this window, against the time left before it resets" }),
    c("runtime", "Runtime"), c("host", "Host"), c("plan", "Plan"), c("observed", "Observed", "stamp"),
    c("limit", "Limit reached"),
  ],
};

export const TABS: Tab[] = [
  { id: "hosts", label: "Hosts and seats", tables: [HOSTS] },
  { id: "tasks", label: "Tasks", tables: [TASKS], count: true },
  { id: "prs", label: "Pull requests", tables: [PRS], count: true },
  { id: "queue", label: "Queue", tables: [QUEUE], count: true },
  { id: "backlog", label: "Backlog", tables: [LABELS, BURNDOWN], chart: "backlog" },
  { id: "landings", label: "Landings", tables: [LANDINGS] },
  { id: "fleet", label: "Fleet", tables: [FLEET] },
  { id: "builds", label: "Builds", tables: [RUNS, BUILDERS] },
  { id: "usage", label: "Usage", tables: [USAGE], chart: "usage" },
];

export const TABLES: TableDef[] = TABS.flatMap((t) => t.tables);

/** GitHub links of `owner`'s repositories; none while the owner is not known. */
export function github(owner: string | undefined) {
  const base = (repo: string) => (owner && repo ? `https://github.com/${owner}/${repo}` : null);
  return {
    /** An issue, or a pull request: GitHub sends an issue link of a pull request on. */
    issue: (repo: string, n: number | string) => (base(repo) ? `${base(repo)}/issues/${n}` : null),
    pull: (repo: string, n: number | string) => (base(repo) ? `${base(repo)}/pull/${n}` : null),
    commit: (repo: string, sha: string) => (base(repo) && sha ? `${base(repo)}/commit/${sha}` : null),
  };
}

/** What the review bot's state of a head says. */
export const BOT: Record<string, string> = {
  clean: "✓ clean",
  findings: "⚠ findings",
  running: "… reviewing",
  none: "– not reviewed",
  limit: "⏸ at its limit",
};

/** A window's length in minutes: `5h`, `week`, `<n>m`; null for another. */
export function windowMinutes(w: string): number | null {
  if (w === "week") return 7 * 1440;
  const m = /^(\d+)([mhd])$/.exec(w);
  if (!m) return null;
  return +m[1] * (m[2] === "d" ? 1440 : m[2] === "h" ? 60 : 1);
}

export interface Risk {
  /** `ok`, `watch` (on pace for 80 % or more by the reset), `out` (runs out before it),
   *  `unknown` (no forecast), `reset` (the window has reset since the reading). */
  level: "ok" | "watch" | "out" | "unknown" | "reset";
  /** Percent used by the reset at the window's rate so far. */
  projected: number | null;
  /** When it reaches 100 % at that rate, if before the reset (ms). */
  outAt: number | null;
  text: string;
}

/**
 * The run-out risk of a limit: its burn rate so far in the window (used over the time since
 * the window started, as at `observed`) carried on to the reset. Too early in the window
 * (under a twentieth of it) to say.
 */
export function runOut(usedPct: number, resetsMs: number | null, window: string, observedMs: number | null, now: number): Risk {
  const len = windowMinutes(window);
  // A reading from a window that has ended says nothing of the one now.
  if (resetsMs !== null && resetsMs <= now)
    return { level: "reset", projected: null, outAt: null, text: "the window has reset since this reading" };
  if (usedPct >= 100) return { level: "out", projected: usedPct, outAt: observedMs, text: "limit reached" };
  if (resetsMs === null || len === null || observedMs === null)
    return { level: "unknown", projected: null, outAt: null, text: "no reset time to judge by" };
  const winMs = len * 60_000;
  const elapsed = winMs - (resetsMs - observedMs);
  if (elapsed < winMs / 20) return { level: "unknown", projected: null, outAt: null, text: "too early in the window to judge" };
  const rate = usedPct / elapsed;
  const projected = Math.round(usedPct + rate * Math.max(0, resetsMs - observedMs));
  if (rate > 0 && projected > 100) {
    const outAt = observedMs + (100 - usedPct) / rate;
    const left = outAt - now;
    return {
      level: "out",
      projected,
      outAt,
      text: `on pace for ${projected}%: runs out ${left > 0 ? `in about ${span(left / 60000)}` : "about now"}, ${span((resetsMs - outAt) / 60000)} before the reset`,
    };
  }
  return { level: projected >= 80 ? "watch" : "ok", projected, outAt: null, text: `on pace for ${projected}% by the reset` };
}

/** How a limit reads, from its unrounded percent used and its risk: `none` is no forecast
 *  (and not near the limit), never "on track". */
export type Severity = "ok" | "warn" | "crit" | "none" | "reset";

export function severity(usedPct: number, level: Risk["level"]): Severity {
  if (level === "reset") return "reset";
  if (usedPct >= 90 || level === "out") return "crit";
  if (usedPct >= 70 || level === "watch") return "warn";
  return level === "unknown" ? "none" : "ok";
}

/** What the rows depend on besides the buckets. */
export interface Context {
  now: number;
  /** A worktree older than this is stale (`stale_after`). */
  staleAfterMs: number;
  /** Open questions by priority, from the console's API (the page cannot read threads). */
  openQuestions?: Record<string, number> | null;
  /** A host or builder record lives this long without a heartbeat (`HOSTS_TTL`). */
  hostsTtlMs: number;
  /** When each `HGL_HOSTS` key was last written, on this page's clock. Its expiry sends
   *  a watch nothing, so an older record is dropped here. */
  hostsAt?: Map<string, number>;
  /** `[github] owner`, for links (from the console's API). */
  githubOwner?: string;
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

  const gh = github(ctx.githubOwner);
  const titles = parseTitles(state("tasks"));
  const queueTitles = new Map(parseBacklog(state("backlog")).queue.map((q) => [`${q.repo}#${q.number}`, q.title]));
  const titleOf = (task: string) => titles.get(task) ?? queueTitles.get(task) ?? null;
  /** Running turns by `host/seat`. */
  const turns = new Map<string, Json>();
  out.hosts = [];
  out.builders = [];
  for (const key of sortedKeys(raw.HGL_HOSTS)) {
    const at = ctx.hostsAt?.get(key);
    if (at !== undefined && ctx.now - at > ctx.hostsTtlMs) continue;
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
        updated: opt(v.updated_at),
      });
      continue;
    }
    const host = { host: v.host, memcap_gb: v.memcap_gb, free_gb: v.work_root_free_gb };
    const seats = (v.seats ?? []) as Json[];
    const none = { seat: null, doing: null, doing_url: null, since: null, role: null, runtime: null, model: null };
    if (!seats.length) out.hosts.push({ id: key, ...host, ...none });
    for (const s of seats) {
      const t = s.turn as Json;
      if (t) turns.set(`${v.host}/${s.seat}`, t);
      let doing = "idle";
      let url: string | null = null;
      if (t?.task) {
        const task = `${t.task.repo}#${t.task.issue}`;
        const title = titleOf(task);
        doing = `${task}${t.review ? " (review)" : ""}${title ? ` ${title}` : ""}`;
        url = gh.issue(t.task.repo, t.task.issue);
      } else if (t) {
        doing = "inbox: notes outside a task";
      }
      out.hosts.push({
        id: `${key}/${s.seat}`,
        ...host,
        seat: s.seat,
        doing,
        doing_url: url,
        since: t ? opt(t.started_at) : null,
        role: s.role,
        runtime: s.runtime,
        model: s.model,
      });
    }
  }

  out.tasks = [];
  let stale = 0;
  for (const t of parseTrees(state("tasks"))) {
    const task = `${t.repo}#${t.issue}`;
    const l = t.last;
    const created = stampMs(t.created);
    const old = created !== null && ctx.now - created > ctx.staleAfterMs;
    stale += old ? 1 : 0;
    const turn = turns.get(`${t.host}/${t.seat}`);
    const here = turn?.task && turn.task.repo === t.repo && turn.task.issue === t.issue && !!turn.review === t.review;
    out.tasks.push({
      id: t.id,
      task,
      task_url: gh.issue(t.repo, t.issue),
      title: titleOf(task),
      seat: t.seat,
      work: t.review ? "review" : "implement",
      stage: stage(l, !!t.review),
      turn: here ? "running" : "idle",
      turn_since: here ? opt(turn.started_at) : null,
      last: l ? `${l.kind}, ${l.sender} to ${l.recipient}` : null,
      last_at: l ? l.at : null,
      host: t.host,
      age: created === null ? null : Math.max(0, Math.floor((ctx.now - created) / 60000)),
      stale: old ? "stale" : null,
    });
  }

  const prs = parsePrs(state("prs"));
  out.prs = prs.rows.map((p) => {
    const pr = `${p.repo}#${p.number}`;
    const counts = p.total === null ? null : `${p.passed} of ${p.total}`;
    const mark = p.checks === "pass" ? "✓" : p.checks === "fail" ? "✗" : p.checks === "pending" ? "…" : "";
    return {
      id: pr,
      pr,
      pr_url: gh.pull(p.repo, p.number),
      title: p.title || null,
      state: p.draft ? "draft" : "open",
      bot: BOT[p.bot] ?? p.bot,
      threads: p.unresolved,
      checks: p.checks === "none" && !p.total ? "– none" : `${mark} ${counts ?? p.checks}`.trim(),
      head: p.head,
      rollup: p.checks,
      read: prs.read || null,
    };
  });

  const backlog = parseBacklog(state("backlog"));
  out.queue = backlog.queue.map((q, i) => {
    const issue = `${q.repo}#${q.number}`;
    return {
      id: issue,
      pos: i + 1,
      issue,
      issue_url: gh.issue(q.repo, q.number),
      class: q.class === "codex-priority" ? "priority" : q.class === "codex" ? "ready" : q.class,
      opened: q.created,
      title: q.title,
    };
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
  const landed = parseLanded(state("landed.log"));
  out.landings = landed
    .slice(0, LANDINGS_MAX)
    .map((l, i) => {
      // The same line twice (a landing run again) is two rows.
      const n = (seen.get(l.line) ?? 0) + 1;
      seen.set(l.line, n);
      return {
        id: n === 1 ? l.line : `${l.line} (${n})`,
        when: l.at,
        // The log's order: what breaks a tie between two landings of the same second.
        order: landed.length - 1 - i,
        pr: l.repo ? `${l.repo}${l.pr}` : null,
        pr_url: l.repo ? gh.pull(l.repo, l.pr.replace(/^#/, "")) : null,
        title: l.title || null,
        merge: l.sha || null,
        merge_url: l.repo ? gh.commit(l.repo, l.sha) : null,
        line: l.line,
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
      started: opt(s.started_at),
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
      observed: opt(u.observed_at),
      limit: opt(u.limit_reached),
    };
    const buckets = (u.buckets ?? []) as Json[];
    const none = { scope: null, window: null, used_pct: null, resets: null, risk: null, level: null, severity: null, projected: null };
    if (!buckets.length) out.usage.push({ id: key, ...base, ...none });
    for (const b of buckets) {
      const r = runOut(b.used_pct, stampMs(b.resets_at ?? ""), b.window, stampMs(u.observed_at ?? ""), ctx.now);
      out.usage.push({
        id: `${key}/${b.scope}/${b.window}`,
        ...base,
        scope: b.scope,
        window: b.window,
        // Classified on the reading as it came; rounded only to show.
        used_pct: Math.round(b.used_pct * 10) / 10,
        resets: opt(b.resets_at),
        risk: r.text,
        level: r.level,
        severity: severity(b.used_pct, r.level),
        projected: r.projected,
        // The limit the source said was reached is of a window that has since reset.
        ...(r.level === "reset" ? { limit: null } : {}),
      });
    }
  }
  return out;
}

/** `prs` read longer ago than this is stale (docs/protocol.md §8). */
export const PRS_STALE_MS = 15 * 60_000;

/** The line above a tab's tables. */
export function notes(raw: Raw, now: number): Record<string, string> {
  const read = (r: string) => {
    const at = stampMs(r);
    return at === null ? "Read never." : `Read ${relative(at, now)} (${when(r)}).`;
  };
  const prs = parsePrs(raw.HGL_STATE.get("prs") ?? "");
  const backlog = parseBacklog(raw.HGL_STATE.get("backlog") ?? "");
  const at = stampMs(prs.read);
  const stale = at !== null && now - at > PRS_STALE_MS ? " Stale: the coordinator reads every 5 minutes." : "";
  const first = backlog.priority.length ? `, ${backlog.priority.join(", then ")} first, then the other repositories` : "";
  const order =
    "The order the coordinator hands work out (docs/work.md WORK-1, WORK-3): issues labelled codex-priority, " +
    `then those labelled codex; within each${first || ", every repository alike"}; then oldest first. `;
  return { prs: read(prs.read) + stale, queue: order + read(backlog.read), backlog: read(backlog.read) };
}

export { age };
