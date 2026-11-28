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

export interface PrRow {
  repo: string;
  number: number;
  head: string;
  draft: boolean;
  bot: string;
  unresolved: number;
  checks: string;
}

/** `HGL_STATE/prs`: `# read`, `# cost`, then `REPO NUMBER HEAD open|draft BOT THREADS CHECKS UPDATED`. */
export function parsePrs(text: string): { read: string; rows: PrRow[] } {
  let read = "";
  const rows: PrRow[] = [];
  for (const line of text.split("\n")) {
    if (line.startsWith("# read ")) read = line.slice(7).trim();
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
}

/** `HGL_STATE/backlog`: `# read`, `# open REPO N`, `# label REPO LABEL N`, then the queue. */
export function parseBacklog(text: string): Backlog {
  const b: Backlog = { read: "", labels: [], queue: [] };
  for (const line of text.split("\n")) {
    if (line.startsWith("# read ")) {
      b.read = line.slice(7).trim();
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
  line: string;
}

/** `HGL_STATE/landed.log`, newest first: `<UTC> <repo> #<pr> <merge sha>`. */
export function parseLanded(text: string): Landing[] {
  const out: Landing[] = [];
  for (const line of text.split("\n")) {
    const w = line.trim().split(/\s+/);
    if (!line.trim()) continue;
    out.push(
      w.length >= 4
        ? { at: w[0], repo: w[1], pr: w[2], sha: w[3], line: line.trim() }
        : { at: line.trim(), repo: "", pr: "", sha: "", line: line.trim() },
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

/** A task's stage from its last note (docs/owner.md §3). */
export function stage(last: LastNote | undefined, review: boolean): string {
  if (review) return "review";
  switch (last?.kind) {
    case "assign":
      return "assigned";
    case "ready":
      return "ready";
    case "review":
    case "receipt":
      return "review";
    case "sendback":
      return "sendback";
    case "approved":
      return "approved";
    case "blocked":
      return "blocked";
    default:
      return "working";
  }
}

/** Minutes as the page shows them. */
export function age(min: number): string {
  const m = Math.max(0, Math.floor(min));
  return m >= 1440 ? `${Math.floor(m / 1440)}d` : m >= 60 ? `${Math.floor(m / 60)}h` : `${m}m`;
}
