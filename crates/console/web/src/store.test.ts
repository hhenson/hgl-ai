import { describe, expect, it } from "vitest";
import type { Transaction } from "./diff";
import { Store } from "./store";
import { TABLES } from "./tables";

const NOW = Date.UTC(2026, 10, 28, 10, 0, 0);

function store() {
  const s = new Store({ staleAfterMs: 3 * 86_400_000 }, () => NOW, 1);
  const seen: Record<string, Transaction[]> = {};
  for (const t of TABLES) s.subscribe(t.id, (tx) => (seen[t.id] ??= []).push(tx));
  return { s, seen };
}

const tree = (seat: string, issue: number, review = false) =>
  JSON.stringify({ host: "lab1", seat, task: { repo: "alpha", issue }, review, path: "/w", created_at: "20261128T080000Z" });

describe("Store", () => {
  it("turns a put or delete into one transaction of the rows that changed", () => {
    const { s, seen } = store();
    s.put("HGL_TREES", "lab1.w1.alpha-7", tree("w1", 7));
    s.put("HGL_STATE", "tasks", "alpha\t7\tassign\tcontroller\tw1\t20261128T081500Z\n");
    s.put("HGL_STATE", "prs", "# read 20261128T095500Z\nalpha\t12\t2222\topen\tclean\t0\tpending\tx\n");
    s.flush();
    expect(seen.tasks[0].add).toEqual([
      {
        id: "lab1.w1.alpha-7",
        seat: "w1",
        task: "alpha#7",
        work: "implement",
        stage: "assigned",
        last: "assign controller→w1",
        last_at: "2026-11-28 08:15 UTC",
        host: "lab1",
        age: 120,
        stale: null,
      },
    ]);
    expect(seen.prs[0].add.map((r) => r.id)).toEqual(["alpha#12"]);
    expect(s.getMeta().counts).toEqual({ tasks: 1, prs: 1, queue: 0 });
    expect(s.getMeta().notes.prs).toBe("Read 2026-11-28 09:55 UTC");

    // One value changes: only the rows it changes move, in that table only.
    s.put("HGL_STATE", "prs", "# read 20261128T095500Z\nalpha\t12\t2222\topen\tclean\t0\tpass\tx\nalpha\t14\t4444\tdraft\tnone\t0\tnone\tx\n");
    s.flush();
    expect(seen.prs[1].update.map((r) => [r.id, r.checks])).toEqual([["alpha#12", "pass"]]);
    expect(seen.prs[1].add.map((r) => r.id)).toEqual(["alpha#14"]);
    expect(seen.tasks.length).toBe(1);

    // A key deleted is a row removed; the stage follows the coordinator's summary.
    s.put("HGL_STATE", "tasks", "alpha\t7\tready\tw1\tcontroller\t20261128T090000Z\n");
    s.flush();
    expect(seen.tasks[1].update[0].stage).toBe("ready");
    s.put("HGL_TREES", "lab1.w1.alpha-7", null);
    s.flush();
    expect(seen.tasks[2].remove.map((r) => r.id)).toEqual(["lab1.w1.alpha-7"]);
    expect(s.getMeta().counts.tasks).toBe(0);
  });

  it("resynchronises a bucket after its watch restarts", () => {
    const { s, seen } = store();
    s.put("HGL_TREES", "a", tree("w1", 7));
    s.put("HGL_TREES", "b", tree("w2", 8));
    s.flush();
    // The watch starts again and delivers only `b`: `a` went while it was away.
    s.resync("HGL_TREES");
    s.put("HGL_TREES", "b", tree("w2", 8));
    s.synced("HGL_TREES");
    s.flush();
    expect(seen.tasks[1]).toEqual({ add: [], update: [], remove: [expect.objectContaining({ id: "a" })] });
  });

  it("derives every table with unique ids", () => {
    const { s } = store();
    s.put("HGL_HOSTS", "lab1", JSON.stringify({ host: "lab1", seats: [{ seat: "w1", role: "implementor", runtime: "claude", family: "claude", model: "m" }], memcap_gb: 64, work_root_free_gb: 500, updated_at: "x" }));
    s.put("HGL_HOSTS", "builder.lab1", JSON.stringify({ label: "lab1", platform: "linux", arch: "x86_64", executor: "local", reachable: true, state: "idle", slots: 2, busy: 0, toolchains: { cargo: "1.98" }, updated_at: "20261128T090000Z" }));
    s.put("HGL_BUILD", "run.20261128-050000-3fa9c1", JSON.stringify({ id: "20261128-050000-3fa9c1", repo: "alpha", head: "8a5c3d264c2e", host: "lab1", platform: "linux", status: "pass", started_at: "20261128T050000Z", wall_ms: 75000, errors_total: 0, tests: { passed: 120, failed: 0 } }));
    s.put("HGL_BUILD", "recipe.alpha", "not a run");
    s.put("HGL_USAGE", "claude.0123456789ab", JSON.stringify({ runtime: "claude", account: "0123456789ab", host: "lab1", observed_at: "20261128T090000Z", buckets: [{ scope: "all", window: "week", used_pct: 41.25 }] }));
    s.put("HGL_USAGE", "label.0123456789ab", JSON.stringify({ label: "team", set_at: "x" }));
    s.put("HGL_METRICS", "alpha.20261127", JSON.stringify({ repo: "alpha", date: "20261127", open: 10, opened: 1, closed: 3 }));
    s.put("HGL_STATE", "landed.log", "20261127T040000Z alpha #5 0123abcd\n20261127T040000Z alpha #5 0123abcd\n");
    s.put("HGL_STATE", "hold", "20261128T120000Z gh allowance\n");
    s.setContext({ openQuestions: { p1: 1, p2: 0, p3: 2 } });
    s.flush();
    expect(s.rows("hosts").map((r) => r.id)).toEqual(["lab1/w1"]);
    expect(s.rows("builders")[0]).toMatchObject({ id: "builder.lab1", toolchains: "cargo 1.98", reachable: "yes" });
    expect(s.rows("runs")).toHaveLength(1);
    expect(s.rows("usage")[0]).toMatchObject({ id: "claude.0123456789ab/all/week", login: "team", used_pct: 41.3 });
    expect(s.rows("burndown")[0]).toMatchObject({ day: "2026-11-27", net: -2 });
    expect(s.rows("landings").map((r) => r.id)).toEqual([
      "20261127T040000Z alpha #5 0123abcd",
      "20261127T040000Z alpha #5 0123abcd (2)",
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
