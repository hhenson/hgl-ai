import { describe, expect, it } from "vitest";
import { diffRows } from "./diff";

describe("diffRows", () => {
  it("adds, updates and removes by id", () => {
    const a = { id: "alpha#1", checks: "pass" };
    const b = { id: "alpha#2", checks: "pending" };
    const t = diffRows([a, b], [{ id: "alpha#2", checks: "fail" }, { id: "beta#3", checks: "pass" }]);
    expect(t).toEqual({
      add: [{ id: "beta#3", checks: "pass" }],
      update: [{ id: "alpha#2", checks: "fail" }],
      remove: [a],
    });
    expect(diffRows([a], [{ ...a }])).toEqual({ add: [], update: [], remove: [] });
  });
});
