// The page's watches against a real hub, as the `viewer` user. Skipped unless the Rust
// live test (crates/install/tests/users_live.rs, `the_page_watches_as_the_viewer`) runs it
// with HGL_AI_LIVE_WS (the hub's WebSocket listener) and HGL_AI_LIVE_SEED (the viewer's seed).

import { nkeyAuthenticator, wsconnect } from "@nats-io/nats-core";
import { describe, expect, it } from "vitest";
import { INBOX_PREFIX, watch } from "./hub";
import { Store } from "./store";
import { SOURCES } from "./tables";

const url = process.env.HGL_AI_LIVE_WS;
const seed = process.env.HGL_AI_LIVE_SEED;

async function until(what: string, ok: () => boolean, ms = 15_000): Promise<void> {
  const end = Date.now() + ms;
  while (!ok()) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 100));
  }
}

describe.skipIf(!url || !seed)("the page against a live hub", () => {
  it("watches every source as the viewer, refused nothing", async () => {
    const nc = await wsconnect({
      servers: url!,
      authenticator: nkeyAuthenticator(new TextEncoder().encode(seed!)),
      inboxPrefix: INBOX_PREFIX,
    });
    const errors: string[] = [];
    void (async () => {
      for await (const s of nc.status()) if (s.type === "error") errors.push(String(s.error));
    })();
    const store = new Store(undefined, Date.now, 0);
    let stop = false;
    const watches = SOURCES.map((src) => watch(nc, store, src, () => stop));
    const fleet = () => {
      store.flush();
      return Object.fromEntries(store.rows("fleet").map((r) => [r.id, r.value]));
    };
    try {
      // What the hub held before: a value of a key the page reads, and a host.
      await until("mode", () => fleet().mode === "drain");
      await until("a host", () => store.rows("hosts").some((r) => r.host === "lab1"));
      // A later write (the Rust test keeps writing `hold`) comes live.
      await until("hold", () => fleet().hold === "landing");
      expect(errors).toEqual([]);
    } finally {
      stop = true;
      await nc.close();
      await Promise.allSettled(watches);
    }
  }, 30_000);
});
