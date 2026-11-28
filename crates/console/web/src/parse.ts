// Parsers for the values the hub keeps (docs/protocol.md §8, docs/owner.md §3). Each
// returns plain data; a line that does not parse is skipped, as the Rust side does.

/** `20261127T041100Z` → `2026-11-27 04:11 UTC`; other text as it is. */
export function when(stamp: string): string {
  const m = /^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z$/.exec(stamp);
  return m ? `${m[1]}-${m[2]}-${m[3]} ${m[4]}:${m[5]} UTC` : stamp;
}

/** Milliseconds since the epoch of a compact stamp, or null. */
export function stampMs(stamp: string): number | null {
  const m = /^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z$/.exec(stamp);
  if (!m) return null;
  return Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6]);
}

/** Relative to `now`: `5m ago`, `in 3h 10m`, `just now`. */
export function relative(ms: number, now: number): string {
  const d = ms - now;
  const m = Math.floor(Math.abs(d) / 60000);
  if (m < 1) return "just now";
  const t = span(m);
  return d < 0 ? `${t} ago` : `in ${t}`;
}

/** Minutes as `45m`, `3h 10m`, `2d 4h`. */
export function span(min: number): string {
  const m = Math.max(0, Math.floor(min));
  if (m < 60) return `${m}m`;
  if (m < 1440) return m % 60 ? `${Math.floor(m / 60)}h ${m % 60}m` : `${Math.floor(m / 60)}h`;
  const h = Math.floor((m % 1440) / 60);
  return h ? `${Math.floor(m / 1440)}d ${h}h` : `${Math.floor(m / 1440)}d`;
}

export interface PrRow {
  repo: string;
  number: number;
  head: string;
  draft: boolean;
  bot: string;
  unresolved: number;
  checks: string;
  /** Checks passed and in all, from the `# pr` line; null without one. */
  passed: number | null;
  total: number | null;
  title: string;
}

/** `HGL_STATE/prs`: `# read`, `# cost`, then `REPO NUMBER HEAD open|draft BOT THREADS CHECKS UPDATED`,
 *  each followed by `# pr REPO NUMBER PASSED TOTAL TITLE`. */
export function parsePrs(text: string): { read: string; rows: PrRow[] } {
  let read = "";
  const rows: PrRow[] = [];
  for (const line of text.split("\n")) {
    if (line.startsWith("# read ")) read = line.slice(7).trim();
    if (line.startsWith("# pr\t")) {
      const f = line.slice(5).split("\t");
      const r = f.length >= 5 ? [...rows].reverse().find((x) => x.repo === f[0] && String(x.number) === f[1]) : undefined;
      if (r) {
        const n = (v: string) => (/^\d+$/.test(v) ? +v : null);
        [r.passed, r.total] = n(f[2]) !== null && n(f[3]) !== null ? [n(f[2]), n(f[3])] : [null, null];
        r.title = f.slice(4).join(" ");
      }
    }
    if (line.startsWith("#") || !line.trim()) continue;
    const f = line.split("\t");
    if (f.length < 7 || !/^\d+$/.test(f[1])) continue;
    rows.push({
      repo: f[0],
      number: +f[1],
      head: f[2],
      draft: f[3] === "draft",
      bot: f[4],
      unresolved: +f[5] || 0,
      checks: f[6],
      passed: null,
      total: null,
      title: "",
    });
  }
  return { read, rows };
}

export interface QueueRow {
  repo: string;
  number: number;
  class: string;
  created: string;
  title: string;
}

export interface Backlog {
  read: string;
  labels: { repo: string; label: string; open: number }[];
  queue: QueueRow[];
  /** Open issues per repository. */
  open: Map<string, number>;
  /** `github.queue_priority`: the repositories first in the queue. */
  priority: string[];
}

/** `HGL_STATE/backlog`: `# read`, `# open REPO N`, `# label REPO LABEL N`, `# priority REPO...`,
 *  then the queue. */
export function parseBacklog(text: string): Backlog {
  const b: Backlog = { read: "", labels: [], queue: [], open: new Map(), priority: [] };
  for (const line of text.split("\n")) {
    if (line.startsWith("# read ")) {
      b.read = line.slice(7).trim();
    } else if (line.startsWith("# priority ")) {
      b.priority = line.slice(11).trim().split(/\s+/).filter(Boolean);
    } else if (line.startsWith("# open ")) {
      const w = line.slice(7).trim().split(" ");
      if (w.length === 2 && /^\d+$/.test(w[1])) b.open.set(w[0], +w[1]);
    } else if (line.startsWith("# label ")) {
      const w = line.slice(8).trim().split(" ");
      const n = Number(w[w.length - 1]);
      if (w.length >= 3 && Number.isFinite(n)) {
        b.labels.push({ repo: w[0], label: w.slice(1, -1).join(" "), open: n });
      }
    } else if (!line.startsWith("#") && line.trim()) {
      const f = line.split("\t");
      if (f.length >= 5 && /^\d+$/.test(f[1])) {
        b.queue.push({ repo: f[0], number: +f[1], class: f[2], created: f[3], title: f.slice(4).join(" ") });
      }
    }
  }
  return b;
}

export interface Landing {
  at: string;
  repo: string;
  pr: string;
  sha: string;
  title: string;
  line: string;
}

/** `HGL_STATE/landed.log`, newest first: `<UTC> <repo> #<pr> <merge sha> [<title>]`. */
export function parseLanded(text: string): Landing[] {
  const out: Landing[] = [];
  for (const line of text.split("\n")) {
    const w = line.trim().split(/\s+/);
    if (!line.trim()) continue;
    out.push(
      w.length >= 4
        ? { at: w[0], repo: w[1], pr: w[2], sha: w[3], title: w.slice(4).join(" "), line: line.trim() }
        : { at: line.trim(), repo: "", pr: "", sha: "", title: "", line: line.trim() },
    );
  }
  return out.reverse();
}

export interface LastNote {
  kind: string;
  sender: string;
  recipient: string;
  at: string;
}

/** `HGL_STATE/tasks` (the coordinator): `REPO ISSUE KIND SENDER RECIPIENT AT`, by `repo#issue`. */
export function parseTasks(text: string): Map<string, LastNote> {
  const out = new Map<string, LastNote>();
  for (const line of text.split("\n")) {
    const f = line.split("\t");
    if (f.length < 6 || !/^\d+$/.test(f[1])) continue;
    out.set(`${f[0]}#${f[1]}`, { kind: f[2], sender: f[3], recipient: f[4], at: f[5] });
  }
  return out;
}

export interface Tree {
  /** As the `HGL_TREES` key: `host.seat.repo-issue[-review]`. */
  id: string;
  host: string;
  seat: string;
  repo: string;
  issue: number;
  review: boolean;
  created: string;
  /** The newest note about the task that this tree's seat sent or got. */
  last: LastNote | undefined;
}

/** `HGL_STATE/tasks`' `# tree HOST SEAT REPO ISSUE implement|review CREATED KIND SENDER
 *  RECIPIENT AT` lines: the worktrees, without their paths. */
export function parseTrees(text: string): Tree[] {
  const out: Tree[] = [];
  for (const line of text.split("\n")) {
    if (!line.startsWith("# tree\t")) continue;
    const f = line.slice(7).split("\t");
    if (f.length < 10 || !/^\d+$/.test(f[3])) continue;
    const review = f[4] === "review";
    out.push({
      id: `${f[0]}.${f[1]}.${f[2]}-${f[3]}${review ? "-review" : ""}`,
      host: f[0],
      seat: f[1],
      repo: f[2],
      issue: +f[3],
      review,
      created: f[5],
      last: f[6] === "-" ? undefined : { kind: f[6], sender: f[7], recipient: f[8], at: f[9] },
    });
  }
  return out;
}

/** `HGL_STATE/tasks`' `# title REPO ISSUE TITLE` lines, by `repo#issue`. */
export function parseTitles(text: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const line of text.split("\n")) {
    if (!line.startsWith("# title\t")) continue;
    const f = line.slice(8).split("\t");
    if (f.length >= 3 && /^\d+$/.test(f[1])) out.set(`${f[0]}#${f[1]}`, f.slice(2).join(" "));
  }
  return out;
}

/** A task's stage from its last note (docs/owner.md §3). */
export function stage(last: LastNote | undefined, review: boolean): string {
  // An ended task is done, in a review tree too.
  if (last && ["done", "landed", "closed"].includes(last.kind)) return "done";
  if (review) return "in review";
  switch (last?.kind) {
    case "assign":
      return "assigned";
    case "ready":
      return "ready";
    case "review":
    case "receipt":
      return "in review";
    case "sendback":
      return "sent back";
    case "approved":
      return "approved";
    case "blocked":
      return "blocked";
    default:
      return "in progress";
  }
}

/** Minutes as the page shows them. */
export function age(min: number): string {
  const m = Math.max(0, Math.floor(min));
  return m >= 1440 ? `${Math.floor(m / 1440)}d` : m >= 60 ? `${Math.floor(m / 60)}h` : `${m}m`;
}
