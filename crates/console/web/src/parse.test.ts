import { describe, expect, it } from "vitest";
import { age, parseBacklog, parseLanded, parsePrs, parseTasks, stage, when } from "./parse";

describe("parsers", () => {
  it("reads the prs table", () => {
    const t = parsePrs(
      "# read 20260105T120000Z\n# cost 13 remaining 4945 resetAt x\n" +
        "alpha\t11\t1111111111\topen\tfindings\t2\tpass\t20260105T120000Z\n" +
        "alpha\t14\t4444444444\tdraft\tnone\t0\tnone\t20260105T120000Z\nbroken line\n",
    );
    expect(t.read).toBe("20260105T120000Z");
    expect(t.rows).toEqual([
      { repo: "alpha", number: 11, head: "1111111111", draft: false, bot: "findings", unresolved: 2, checks: "pass" },
      { repo: "alpha", number: 14, head: "4444444444", draft: true, bot: "none", unresolved: 0, checks: "none" },
    ]);
  });

  it("reads the backlog, labels with spaces too", () => {
    const b = parseBacklog(
      "# read 20260105T120000Z\n# open alpha 9\n# label alpha good first issue 2\n# label beta codex 2\n" +
        "alpha\t105\tcodex-priority\t20260103T080000Z\tPriority work\n",
    );
    expect(b.read).toBe("20260105T120000Z");
    expect(b.labels).toEqual([
      { repo: "alpha", label: "good first issue", open: 2 },
      { repo: "beta", label: "codex", open: 2 },
    ]);
    expect(b.queue).toEqual([
      { repo: "alpha", number: 105, class: "codex-priority", created: "20260103T080000Z", title: "Priority work" },
    ]);
  });

  it("reads landings newest first and tasks by issue", () => {
    const l = parseLanded("20261127T040000Z alpha #5 0123abcd\n20261128T010000Z beta #9 abcdef01\n");
    expect(l.map((x) => x.pr)).toEqual(["#9", "#5"]);
    const t = parseTasks("alpha\t7\tsendback\tcontroller\tw1\t20261128T000000Z\nnot a line\n");
    expect(t.get("alpha#7")).toEqual({ kind: "sendback", sender: "controller", recipient: "w1", at: "20261128T000000Z" });
    expect(t.size).toBe(1);
  });

  it("names stages and ages as the console does", () => {
    expect(stage({ kind: "assign", sender: "c", recipient: "w1", at: "" }, false)).toBe("assigned");
    expect(stage({ kind: "receipt", sender: "r", recipient: "c", at: "" }, false)).toBe("review");
    expect(stage(undefined, true)).toBe("review");
    expect(stage(undefined, false)).toBe("working");
    expect([age(5), age(125), age(3 * 1440 + 7), age(-3)]).toEqual(["5m", "2h", "3d", "0m"]);
    expect(when("20261127T041100Z")).toBe("2026-11-27 04:11 UTC");
    expect(when("soon")).toBe("soon");
  });
});
