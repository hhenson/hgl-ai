import { afterEach, describe, expect, it, vi } from "vitest";
import { Backoff, connectRetrying, type Entry, follow, hubUrl, QUIET_MS } from "./hub";
import { Store } from "./store";

describe("hubUrl", () => {
  it("is the page's own origin", () => {
    expect(hubUrl({ protocol: "https:", host: "hub.example.ts.net" }, "/nats")).toBe("wss://hub.example.ts.net/nats");
    expect(hubUrl({ protocol: "http:", host: "127.0.0.1:8080" }, "/nats")).toBe("ws://127.0.0.1:8080/nats");
  });
});

describe("connectRetrying", () => {
  it("tries the first connection again until it succeeds", async () => {
    let tries = 0;
    const backoff = new Backoff(3000, 60_000, () => 0);
    const failures: unknown[] = [];
    const waits: number[] = [];
    const c = await connectRetrying(
      async () => {
        if (++tries < 3) throw new Error(`down ${tries}`);
        return "conn";
      },
      (e) => failures.push((e as Error).message),
      () => false,
      backoff,
      async (ms) => void waits.push(ms),
    );
    expect(c).toBe("conn");
    expect(failures).toEqual(["down 1", "down 2"]);
    expect(waits).toEqual([3000, 6000]);
    // Connected: the next outage starts again from 3 s.
    expect(backoff.next()).toBe(3000);
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
      new Backoff(),
      async () => void (stopped = true),
    );
    expect(c).toBeNull();
    expect(tries).toBe(1);
  });
});

describe("Backoff", () => {
  it("doubles from 3 s to a 60 s cap, and starts again on reset", () => {
    const b = new Backoff(3000, 60_000, () => 0);
    expect(Array.from({ length: 7 }, () => b.next())).toEqual([3000, 6000, 12000, 24000, 48000, 60000, 60000]);
    b.reset();
    expect(b.next()).toBe(3000);
  });

  it("takes up to a quarter off at random, never above the cap", () => {
    const low = new Backoff(3000, 60_000, () => 1);
    expect([low.next(), low.next()]).toEqual([2250, 4500]);
    const b = new Backoff();
    for (let i = 0; i < 50; i++) {
      const ms = b.next();
      expect(ms).toBeGreaterThan(0);
      expect(ms).toBeLessThanOrEqual(60_000);
    }
  });
});

describe("follow", () => {
  afterEach(() => void vi.useRealTimers());
  const entry = (key: string, value: string, isUpdate = false): Entry => ({
    key,
    operation: "PUT",
    created: new Date(0),
    isUpdate,
    string: () => value,
  });
  const fleet = (s: Store) => {
    s.flush();
    return Object.fromEntries(s.rows("fleet").map((r) => [r.id, r.value]));
  };

  it("a watch that fails mid-replay drops nothing it had not delivered", async () => {
    vi.useFakeTimers();
    const s = new Store(undefined, Date.now, 0);
    const src = { bucket: "HGL_STATE" as const };
    s.put("HGL_STATE", "mode", "drain");
    s.put("HGL_STATE", "hold", "landing");
    // The watch starts again, delivers `hold`, and fails before `mode` comes.
    s.resync(src);
    async function* failing(): AsyncGenerator<Entry> {
      yield entry("hold", "landing");
      throw new Error("the connection went");
    }
    await expect(follow(failing(), s, src, () => false, () => {})).rejects.toThrow("the connection went");
    await vi.advanceTimersByTimeAsync(QUIET_MS * 2);
    expect(fleet(s)).toMatchObject({ mode: "drain", hold: "landing" });

    // The next watch's catch-up is its own: no earlier timer ends it.
    s.resync(src);
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    async function* slow(): AsyncGenerator<Entry> {
      yield entry("hold", "landing");
      await gate;
      yield entry("mode", "drain");
      yield entry("hold", "none-yet", true);
    }
    const done = follow(slow(), s, src, () => false, () => {});
    await vi.advanceTimersByTimeAsync(QUIET_MS / 2);
    release();
    await done;
    await vi.advanceTimersByTimeAsync(QUIET_MS * 2);
    expect(fleet(s)).toMatchObject({ mode: "drain" });
  });

  it("is caught up when the watch is quiet, and what it did not deliver goes", async () => {
    vi.useFakeTimers();
    const s = new Store(undefined, Date.now, 0);
    const src = { bucket: "HGL_STATE" as const, key: "hold" };
    s.put("HGL_STATE", "hold", "landing");
    s.resync(src);
    let end!: () => void;
    const open = new Promise<void>((r) => (end = r));
    async function* empty(): AsyncGenerator<Entry> {
      await open;
    }
    const done = follow(empty(), s, src, () => false, () => {});
    await vi.advanceTimersByTimeAsync(QUIET_MS + 10);
    expect(fleet(s)).toMatchObject({ hold: "none" });
    end();
    await done;
  });
});
