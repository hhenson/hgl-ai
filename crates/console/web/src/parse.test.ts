import { describe, expect, it } from "vitest";
import { age, parseBacklog, parseLanded, parsePrs, parseTasks, parseTitles, relative, span, stage, when } from "./parse";
import { runOut, windowMinutes } from "./tables";

describe("parsers", () => {
  it("reads the prs table", () => {
    const t = parsePrs(
      "# read 20260105T120000Z\n# cost 13 remaining 4945 resetAt x\n" +
        "alpha\t11\t1111111111\topen\tfindings\t2\tpass\t20260105T120000Z\n" +
        "# pr\talpha\t11\t2\t3\tFix the parser\n" +
        "alpha\t14\t4444444444\tdraft\tnone\t0\tnone\t20260105T120000Z\n# pr\talpha\t14\t-\t-\tA draft\n" +
        "# pr\tbeta\t1\t1\t1\tNo row\nbroken line\n",
    );
    expect(t.read).toBe("20260105T120000Z");
    expect(t.rows).toEqual([
      { repo: "alpha", number: 11, head: "1111111111", draft: false, bot: "findings", unresolved: 2, checks: "pass", passed: 2, total: 3, title: "Fix the parser" },
      { repo: "alpha", number: 14, head: "4444444444", draft: true, bot: "none", unresolved: 0, checks: "none", passed: null, total: null, title: "A draft" },
    ]);
    // A table from before the `# pr` lines: rows without titles.
    expect(parsePrs("alpha\t11\t1\topen\tclean\t0\tpass\tx\n").rows[0]).toMatchObject({ title: "", total: null });
  });

  it("reads the backlog, labels with spaces too", () => {
    const b = parseBacklog(
      "# read 20260105T120000Z\n# open alpha 9\n# label alpha good first issue 2\n# label beta codex 2\n# priority beta alpha\n" +
        "alpha\t105\tcodex-priority\t20260103T080000Z\tPriority work\n",
    );
    expect(b.read).toBe("20260105T120000Z");
    expect(b.labels).toEqual([
      { repo: "alpha", label: "good first issue", open: 2 },
      { repo: "beta", label: "codex", open: 2 },
    ]);
    expect(b.priority).toEqual(["beta", "alpha"]);
    expect([...b.open]).toEqual([["alpha", 9]]);
    expect(b.queue).toEqual([
      { repo: "alpha", number: 105, class: "codex-priority", created: "20260103T080000Z", title: "Priority work" },
    ]);
  });

  it("reads landings newest first and tasks by issue", () => {
    const l = parseLanded("20261127T040000Z alpha #5 0123abcd\n20261128T010000Z beta #9 abcdef01 Fix  the gate\n");
    expect(l.map((x) => [x.pr, x.title])).toEqual([["#9", "Fix the gate"], ["#5", ""]]);
    const text = "alpha\t7\tsendback\tcontroller\tw1\t20261128T000000Z\nnot a line\n# title\talpha\t7\tFix\tit\n# title\tx\ty\tz\n";
    const t = parseTasks(text);
    expect(t.get("alpha#7")).toEqual({ kind: "sendback", sender: "controller", recipient: "w1", at: "20261128T000000Z" });
    expect(t.size).toBe(1);
    expect([...parseTitles(text)]).toEqual([["alpha#7", "Fix it"]]);
  });

  it("names stages and ages as the console does", () => {
    expect(stage({ kind: "assign", sender: "c", recipient: "w1", at: "" }, false)).toBe("assigned");
    expect(stage({ kind: "receipt", sender: "r", recipient: "c", at: "" }, false)).toBe("in review");
    expect(stage({ kind: "sendback", sender: "c", recipient: "w1", at: "" }, false)).toBe("sent back");
    for (const kind of ["done", "landed", "closed"]) expect(stage({ kind, sender: "c", recipient: "w1", at: "" }, false)).toBe("done");
    expect(stage(undefined, true)).toBe("in review");
    expect(stage(undefined, false)).toBe("in progress");
    const now = Date.UTC(2026, 10, 28, 12, 0, 0);
    expect([relative(now - 30_000, now), relative(now - 5 * 60_000, now), relative(now + 190 * 60_000, now)]).toEqual([
      "just now",
      "5m ago",
      "in 3h 10m",
    ]);
    expect([span(45), span(120), span(1440 * 2 + 240), span(1440)]).toEqual(["45m", "2h", "2d 4h", "1d"]);
    expect([age(5), age(125), age(3 * 1440 + 7), age(-3)]).toEqual(["5m", "2h", "3d", "0m"]);
    expect(when("20261127T041100Z")).toBe("2026-11-27 04:11 UTC");
    expect(when("soon")).toBe("soon");
  });

  it("judges a limit's run-out risk by its burn rate so far", () => {
    const H = 3_600_000;
    const now = Date.UTC(2026, 10, 28, 12, 0, 0);
    expect([windowMinutes("5h"), windowMinutes("week"), windowMinutes("90m"), windowMinutes("x")]).toEqual([300, 10080, 90, null]);
    // 5h window, 4h left: 1h in, 30% used: on pace for 150%, out in 1h 20m, 2h 40m before the reset.
    const out = runOut(30, now + 4 * H, "5h", now, now);
    expect(out).toMatchObject({ level: "out", projected: 150 });
    expect(out.outAt).toBe(now + (70 / 30) * H);
    expect(out.text).toBe("on pace for 150%: runs out in about 2h 20m, 1h 40m before the reset");
    // 3h in, 50% used: 83% by the reset; watch.
    expect(runOut(50, now + 2 * H, "5h", now, now)).toMatchObject({ level: "watch", projected: 83 });
    expect(runOut(10, now + 2 * H, "5h", now, now)).toMatchObject({ level: "ok", projected: 17 });
    // The record's own time counts, not the page's: observed an hour ago.
    expect(runOut(30, now + 3 * H, "5h", now - H, now).outAt).toBe(now - H + (70 / 30) * H);
    expect(runOut(100, now + H, "5h", now, now).level).toBe("out");
    expect(runOut(5, now + 299 * 60_000, "5h", now, now).level).toBe("unknown");
    expect(runOut(5, null, "5h", now, now).level).toBe("unknown");
  });
});
