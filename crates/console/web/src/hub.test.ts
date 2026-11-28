import { describe, expect, it } from "vitest";
import { connectRetrying, hubUrl } from "./hub";

describe("hubUrl", () => {
  it("is the page's own origin", () => {
    expect(hubUrl({ protocol: "https:", host: "hub.example.ts.net" }, "/nats")).toBe("wss://hub.example.ts.net/nats");
    expect(hubUrl({ protocol: "http:", host: "127.0.0.1:8080" }, "/nats")).toBe("ws://127.0.0.1:8080/nats");
  });
});

describe("connectRetrying", () => {
  it("tries the first connection again until it succeeds", async () => {
    let tries = 0;
    const failures: unknown[] = [];
    const waits: number[] = [];
    const c = await connectRetrying(
      async () => {
        if (++tries < 3) throw new Error(`down ${tries}`);
        return "conn";
      },
      (e) => failures.push((e as Error).message),
      () => false,
      async (ms) => void waits.push(ms),
    );
    expect(c).toBe("conn");
    expect(failures).toEqual(["down 1", "down 2"]);
    expect(waits).toEqual([3000, 3000]);
  });

  it("gives up once stopped", async () => {
    let stopped = false;
    let tries = 0;
    const c = await connectRetrying(
      async () => {
        tries++;
        throw new Error("down");
      },
      () => {},
      () => stopped,
      async () => void (stopped = true),
    );
    expect(c).toBeNull();
    expect(tries).toBe(1);
  });
});
