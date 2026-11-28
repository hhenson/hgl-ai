import { describe, expect, it } from "vitest";
import type { Transaction } from "./diff";
import { Store } from "./store";
import { TABLES } from "./tables";

const NOW = Date.UTC(2026, 10, 28, 10, 0, 0);

function store() {
  const s = new Store({ staleAfterMs: 3 * 86_400_000, hostsTtlMs: 120_000 }, () => NOW, 1);
  const seen: Record<string, Transaction[]> = {};
  for (const t of TABLES) s.subscribe(t.id, (tx) => (seen[t.id] ??= []).push(tx));
  return { s, seen };
}

/** A `# tree` line of `tasks`, with the seat's last note (`KIND\tSENDER\tRECIPIENT\tAT`). */
const tree = (seat: string, issue: number, review = false, last = "-\t-\t-\t-") =>
  `# tree\tlab1\t${seat}\talpha\t${issue}\t${review ? "review" : "implement"}\t20261128T080000Z\t${last}\n`;

describe("Store", () => {
  it("turns a put or delete into one transaction of the rows that changed", () => {
    const { s, seen } = store();
    s.put("HGL_STATE", "tasks", "alpha\t7\tassign\tcontroller\tw1\t20261128T081500Z\n" + tree("w1", 7, false, "assign\tcontroller\tw1\t20261128T081500Z"));
    s.put("HGL_STATE", "prs", "# read 20261128T095500Z\nalpha\t12\t2222\topen\tclean\t0\tpending\tx\n");
    s.flush();
    expect(seen.tasks[0].add).toEqual([
      {
        id: "lab1.w1.alpha-7",
        task: "alpha#7",
        task_url: null,
        title: null,
        seat: "w1",
        work: "implement",
        stage: "assigned",
        turn: "idle",
        turn_since: null,
        last: "assign, controller to w1",
        last_at: "20261128T081500Z",
        host: "lab1",
        age: 120,
        stale: null,
      },
    ]);
    expect(seen.prs[0].add.map((r) => r.id)).toEqual(["alpha#12"]);
    expect(s.getMeta().counts).toEqual({ tasks: 1, prs: 1, queue: 0 });
    expect(s.getMeta().notes.prs).toBe("Read 5m ago (2026-11-28 09:55 UTC).");

    // One value changes: only the rows it changes move, in that table only.
    s.put("HGL_STATE", "prs", "# read 20261128T095500Z\nalpha\t12\t2222\topen\tclean\t0\tpass\tx\nalpha\t14\t4444\tdraft\tnone\t0\tnone\tx\n");
    s.flush();
    expect(seen.prs[1].update.map((r) => [r.id, r.checks])).toEqual([["alpha#12", "✓ pass"]]);
    expect(seen.prs[1].add.map((r) => r.id)).toEqual(["alpha#14"]);
    expect(seen.tasks.length).toBe(1);

    // A key deleted is a row removed; the stage follows the coordinator's summary.
    s.put("HGL_STATE", "tasks", tree("w1", 7, false, "ready\tw1\tcontroller\t20261128T090000Z"));
    s.flush();
    expect(seen.tasks[1].update[0].stage).toBe("ready");
    s.put("HGL_STATE", "tasks", null);
    s.flush();
    expect(seen.tasks[2].remove.map((r) => r.id)).toEqual(["lab1.w1.alpha-7"]);
    expect(s.getMeta().counts.tasks).toBe(0);
  });

  it("resynchronises a bucket after its watch restarts", () => {
    const { s, seen } = store();
    const host = (h: string) => JSON.stringify({ host: h, seats: [], memcap_gb: 1, work_root_free_gb: 1, updated_at: "x" });
    s.put("HGL_HOSTS", "a", host("a"));
    s.put("HGL_HOSTS", "b", host("b"));
    s.flush();
    // The watch starts again and delivers only `b`: `a` went while it was away.
    s.resync({ bucket: "HGL_HOSTS" });
    s.put("HGL_HOSTS", "b", host("b"));
    s.synced({ bucket: "HGL_HOSTS" });
    s.flush();
    expect(seen.hosts[1]).toEqual({ add: [], update: [], remove: [expect.objectContaining({ id: "a" })] });
  });

  it("resynchronises one key of HGL_STATE without touching the others", () => {
    const { s } = store();
    s.put("HGL_STATE", "mode", "drain");
    s.put("HGL_STATE", "hold", "landing");
    s.flush();
    // `hold`'s watch starts again and delivers nothing: `hold` went; `mode` stays.
    s.resync({ bucket: "HGL_STATE", key: "hold" });
    s.synced({ bucket: "HGL_STATE", key: "hold" });
    s.flush();
    const fleet = Object.fromEntries(s.rows("fleet").map((r) => [r.id, r.value]));
    expect(fleet).toMatchObject({ mode: "drain", hold: "none" });
  });

  it("watches HGL_STATE key by key, the keys the hub's viewer may read", async () => {
    const { SOURCES, STATE_KEYS } = await import("./tables");
    expect(SOURCES.filter((x) => x.bucket === "HGL_STATE").map((x) => x.key)).toEqual([...STATE_KEYS]);
    expect(SOURCES.filter((x) => x.bucket !== "HGL_STATE").every((x) => x.key === undefined)).toBe(true);
  });

  it("shows what each seat does now, titles and links", () => {
    const { s } = store();
    const turn = { task: { repo: "alpha", issue: 7 }, started_at: "20261128T094000Z" };
    s.put("HGL_HOSTS", "lab1", JSON.stringify({
      host: "lab1",
      seats: [
        { seat: "w1", role: "implementor", runtime: "claude", family: "claude", model: "m", turn },
        { seat: "w2", role: "implementor", runtime: "codex", family: "gpt", model: "g" },
        { seat: "r1", role: "reviewer", runtime: "codex", family: "gpt", model: "g", turn: { started_at: "20261128T095900Z" } },
      ],
      memcap_gb: 64, work_root_free_gb: 500, updated_at: "x",
    }));
    s.put("HGL_STATE", "tasks",
      "alpha\t7\tsendback\tcontroller\tw1\t20261128T093000Z\nalpha\t8\tlanded\tcontroller\tall\t20261128T090000Z\n" +
      tree("w1", 7, false, "sendback\tcontroller\tw1\t20261128T093000Z") +
      // Handed off from w1 to w2: each tree its own seat's note.
      tree("w1", 8, false, "blocked\tw1\tcontroller\t20261128T080000Z") +
      tree("w2", 8, false, "landed\tcontroller\tall\t20261128T090000Z") +
      tree("r1", 7, true, "review\tcontroller\tr1\t20261128T091000Z") +
      // A retained review tree of a landed task, and a task whose notes aged out.
      tree("r1", 8, true, "landed\tcontroller\tall\t20261128T090000Z") +
      tree("w2", 9) +
      "# title\talpha\t7\tFix the parser\n# title\talpha\t9\tOld task\n");
    s.put("HGL_STATE", "prs", "# read 20261128T095500Z\nalpha\t12\t2222\topen\tfindings\t1\tfail\tx\n# pr\talpha\t12\t2\t3\tGate it\n");
    s.put("HGL_STATE", "landed.log", "20261127T040000Z alpha #5 0123abcd Land it\n");
    s.put("HGL_STATE", "backlog", "# read 20261128T094500Z\n# priority beta\nalpha\t9\tcodex\t20261101T000000Z\tNine\n");
    s.setContext({ githubOwner: "o" });
    s.flush();
    expect(s.rows("hosts").map((r) => [r.seat, r.doing, r.doing_url, r.since])).toEqual([
      ["w1", "alpha#7 Fix the parser", "https://github.com/o/alpha/issues/7", "20261128T094000Z"],
      ["w2", "idle", null, null],
      ["r1", "inbox: notes outside a task", null, "20261128T095900Z"],
    ]);
    const tasks = Object.fromEntries(s.rows("tasks").map((r) => [r.id, [r.title, r.stage, r.turn]]));
    expect(tasks).toEqual({
      "lab1.w1.alpha-7": ["Fix the parser", "sent back", "running"],
      "lab1.w1.alpha-8": [null, "blocked", "idle"],
      "lab1.w2.alpha-8": [null, "done", "idle"],
      // The reviewer's turn is in its inbox, not in the review tree.
      "lab1.r1.alpha-7-review": ["Fix the parser", "in review", "idle"],
      "lab1.r1.alpha-8-review": [null, "done", "idle"],
      "lab1.w2.alpha-9": ["Old task", "in progress", "idle"],
    });
    expect(s.rows("prs")[0]).toMatchObject({
      pr_url: "https://github.com/o/alpha/pull/12", title: "Gate it", bot: "⚠ findings", checks: "✗ 2 of 3", head: "2222",
    });
    expect(s.rows("landings")[0]).toMatchObject({
      pr: "alpha#5", pr_url: "https://github.com/o/alpha/pull/5", title: "Land it",
      merge: "0123abcd", merge_url: "https://github.com/o/alpha/commit/0123abcd",
    });
    expect(s.rows("queue")[0]).toMatchObject({ issue_url: "https://github.com/o/alpha/issues/9", class: "ready", title: "Nine" });
    expect(s.getMeta().notes.queue).toContain("within each, beta first, then the other repositories; then oldest first");
    expect(s.openByRepo().size).toBe(0);
    expect(s.githubOwner()).toBe("o");
  });

  it("derives every table with unique ids", () => {
    const { s } = store();
    s.put("HGL_HOSTS", "lab1", JSON.stringify({ host: "lab1", seats: [{ seat: "w1", role: "implementor", runtime: "claude", family: "claude", model: "m" }], memcap_gb: 64, work_root_free_gb: 500, updated_at: "x" }));
    s.put("HGL_HOSTS", "builder.lab1", JSON.stringify({ label: "lab1", platform: "linux", arch: "x86_64", executor: "local", reachable: true, state: "idle", slots: 2, busy: 0, toolchains: { cargo: "1.98" }, updated_at: "20261128T090000Z" }));
    s.put("HGL_BUILD", "run.20261128-050000-3fa9c1", JSON.stringify({ id: "20261128-050000-3fa9c1", repo: "alpha", head: "8a5c3d264c2e", host: "lab1", platform: "linux", status: "pass", started_at: "20261128T050000Z", wall_ms: 75000, errors_total: 0, tests: { passed: 120, failed: 0 } }));
    s.put("HGL_BUILD", "recipe.alpha", "not a run");
    s.put("HGL_USAGE", "claude.0123456789ab", JSON.stringify({ runtime: "claude", account: "0123456789ab", host: "lab1", observed_at: "20261128T090000Z", buckets: [{ scope: "all", window: "week", used_pct: 41.25 }, { scope: "all", window: "5h", used_pct: 60, resets_at: "20261128T110000Z" }, { scope: "fable", window: "5h", used_pct: 100, resets_at: "20261128T093000Z" }], limit_reached: "fable" }));
    s.put("HGL_USAGE", "label.0123456789ab", JSON.stringify({ label: "team", set_at: "x" }));
    s.put("HGL_METRICS", "alpha.20261127", JSON.stringify({ repo: "alpha", date: "20261127", open: 10, opened: 1, closed: 3 }));
    s.put("HGL_STATE", "landed.log", "20261127T040000Z alpha #5 0123abcd\n20261127T040000Z alpha #5 0123abcd\n");
    s.put("HGL_STATE", "hold", "20261128T120000Z gh allowance\n");
    s.setContext({ openQuestions: { p1: 1, p2: 0, p3: 2 } });
    s.flush();
    expect(s.rows("hosts").map((r) => r.id)).toEqual(["lab1/w1"]);
    expect(s.rows("builders")[0]).toMatchObject({ id: "builder.lab1", toolchains: "cargo 1.98", reachable: "yes" });
    expect(s.rows("runs")).toHaveLength(1);
    expect(s.rows("usage")[0]).toMatchObject({ id: "claude.0123456789ab/all/week", login: "team", used_pct: 41.3, level: "unknown" });
    // 5h window resetting at 11:00, observed at 09:00: 3h in, 60% used, 100% right at the reset.
    expect(s.rows("usage")[1]).toMatchObject({ level: "watch", severity: "warn", projected: 100, limit: "fable" });
    // Full, but its window reset at 09:30: neither "limit reached" nor at risk now.
    expect(s.rows("usage")[2]).toMatchObject({ level: "reset", severity: "reset", limit: null, used_pct: 100 });
    expect(s.rows("usage")[0]).toMatchObject({ severity: "none" });
    expect(s.rows("burndown")[0]).toMatchObject({ day: "2026-11-27", net: -2 });
    expect(s.rows("landings").map((r) => [r.id, r.when, r.order])).toEqual([
      ["20261127T040000Z alpha #5 0123abcd", "20261127T040000Z", 1],
      ["20261127T040000Z alpha #5 0123abcd (2)", "20261127T040000Z", 0],
    ]);
    expect(Object.fromEntries(s.rows("fleet").map((r) => [r.id, r.value]))).toEqual({
      mode: "normal",
      hold: "20261128T120000Z gh allowance",
      stale: "0",
      p1: "1",
      p2: "0",
      p3: "2",
    });
    for (const t of TABLES) {
      const ids = s.rows(t.id).map((r) => r.id);
      expect(new Set(ids).size, t.id).toBe(ids.length);
    }
  });
});

describe("timestamps", () => {
  it("sort to the second, and a tie by the log's order", async () => {
    const { byStamp } = await import("./Grid");
    // Two landings in one minute: the later second is the newer.
    expect(byStamp("20261128T101005Z", "20261128T101050Z")).toBeLessThan(0);
    // The same second: the later line in landed.log is the newer.
    expect(byStamp("20261128T101005Z", "20261128T101005Z", { id: "a", order: 3 }, { id: "b", order: 4 })).toBeLessThan(0);
    expect(byStamp(null, "20261128T101005Z")).toBeLessThan(0);
  });
});

describe("host records", () => {
  const host = (h: string) => JSON.stringify({ host: h, seats: [], memcap_gb: 1, work_root_free_gb: 1, updated_at: "x" });

  it("go when their heartbeat lapses, though expiry sends no delete", () => {
    let now = NOW;
    const s = new Store({ staleAfterMs: 86_400_000, hostsTtlMs: 120_000 }, () => now, 1);
    s.put("HGL_HOSTS", "lab1", host("lab1"), NOW - 10_000);
    s.put("HGL_HOSTS", "lab2", host("lab2"), NOW - 200_000);
    s.flush();
    expect(s.rows("hosts").map((r) => r.id)).toEqual(["lab1"]);
    now = NOW + 115_000; // lab1's last heartbeat is now 125 s old
    s.flush();
    expect(s.rows("hosts")).toEqual([]);
    s.put("HGL_HOSTS", "lab1", host("lab1"), now, true); // a heartbeat
    s.flush();
    expect(s.rows("hosts").map((r) => r.id)).toEqual(["lab1"]);
  });

  it("are aged on the hub's clock, not the page's", () => {
    // The page's clock is 300 s ahead of the hub's: a live write shows it.
    const s = new Store({ staleAfterMs: 86_400_000, hostsTtlMs: 120_000 }, () => NOW, 1);
    s.put("HGL_HOSTS", "lab1", host("lab1"), NOW - 300_000 - 10_000);
    s.put("HGL_STATE", "mode", "normal", NOW - 300_000, true);
    s.flush();
    expect(s.rows("hosts").map((r) => r.id)).toEqual(["lab1"]);
  });
});
