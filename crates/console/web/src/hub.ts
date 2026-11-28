// Reading the hub over WebSocket as the `viewer` user (docs/owner.md §3): one watch per
// telemetry bucket (per key of `HGL_STATE`), fed into the store. A first connection that fails
// is tried again and a made one reconnects, both on one backoff; a watch that ends is started
// again and what it covers resynchronised.

import { Kvm } from "@nats-io/kv";
import { type NatsConnection, nkeyAuthenticator, wsconnect } from "@nats-io/nats-core";
import type { Store } from "./store";
import { SOURCES, type Source } from "./tables";

export interface Viewer {
  user: string;
  seed: string;
  path: string;
}

/** `wss://<this origin><path>`: the TLS proxy serves the hub's listener there. */
export function hubUrl(loc: Pick<Location, "protocol" | "host">, path: string): string {
  return `${loc.protocol === "https:" ? "wss" : "ws"}://${loc.host}${path}`;
}

const RETRY_MS = 3000;
/** The `viewer` user may subscribe here only (`VIEWER_INBOX` in crates/proto). */
export const INBOX_PREFIX = "_INBOX_VIEWER";

/** What `follow` reads of a watch's entry. */
export interface Entry {
  key: string;
  operation: string;
  created: Date;
  isUpdate: boolean;
  string(): string;
}

/** How long a watch's catch-up may be quiet before it counts as caught up. */
export const QUIET_MS = 1500;

/**
 * Feed a watch's entries to the store until it ends or throws. Caught up once an entry is
 * live, or after `QUIET_MS` without one; the timer for that never outlives the watch, so a
 * watch that fails mid-replay drops nothing it had not delivered, and never ends the next
 * watch's catch-up early.
 */
export async function follow(
  iter: AsyncIterable<Entry>,
  store: Store,
  source: Source,
  stopped: () => boolean,
  stop: () => void,
): Promise<void> {
  // Nothing to catch up on: nothing there, or its values have all come.
  let quiet: ReturnType<typeof setTimeout> | undefined = setTimeout(() => store.synced(source), QUIET_MS);
  try {
    for await (const e of iter) {
      store.put(source.bucket, e.key, e.operation === "PUT" ? e.string() : null, e.created.getTime(), e.isUpdate);
      clearTimeout(quiet);
      quiet = undefined;
      if (e.isUpdate) store.synced(source);
      else quiet = setTimeout(() => store.synced(source), QUIET_MS);
      if (stopped()) stop();
    }
  } finally {
    clearTimeout(quiet);
  }
}

export async function watch(nc: NatsConnection, store: Store, source: Source, stopped: () => boolean): Promise<void> {
  const { bucket, key } = source;
  while (!stopped() && !nc.isClosed()) {
    try {
      const kv = await new Kvm(nc).open(bucket);
      store.resync(source);
      // One key or the whole bucket: either way one filter, so the client asks for the
      // consumer in the filtered form, the only one the viewer may use.
      const iter = await kv.watch(key === undefined ? {} : { key });
      await follow(iter, store, source, stopped, () => iter.stop());
    } catch (err) {
      console.warn(`watching ${key === undefined ? bucket : `${bucket}/${key}`}:`, err);
    }
    if (!stopped()) await new Promise((r) => setTimeout(r, RETRY_MS));
  }
}

/**
 * Waits between attempts at the hub: 3 s doubling to 60 s, each less up to a quarter at random
 * so that open pages spread out during an outage; `reset()` once connected.
 */
export class Backoff {
  private attempt = 0;
  constructor(
    private readonly baseMs = 3000,
    private readonly capMs = 60_000,
    private readonly random: () => number = Math.random,
  ) {}
  next(): number {
    const ms = Math.min(this.capMs, this.baseMs * 2 ** Math.min(this.attempt, 30));
    this.attempt++;
    return Math.round(ms * (1 - 0.25 * this.random()));
  }
  reset(): void {
    this.attempt = 0;
  }
}

/**
 * `connect` until it succeeds, waiting `backoff.next()` after each failure; null once `stopped()`.
 * The client retries only a connection it has made: a first attempt that fails is final.
 */
export async function connectRetrying<T>(
  connect: () => Promise<T>,
  failed: (err: unknown) => void,
  stopped: () => boolean,
  backoff: Backoff,
  wait: (ms: number) => Promise<void> = (ms) => new Promise((r) => setTimeout(r, ms)),
): Promise<T | null> {
  while (!stopped()) {
    try {
      const c = await connect();
      backoff.reset();
      return c;
    } catch (err) {
      failed(err);
    }
    await wait(backoff.next());
  }
  return null;
}

/** Connect and watch every bucket; returns a function that closes it all, retries included. */
export function start(viewer: Viewer, store: Store): () => Promise<void> {
  let stop = false;
  let nc: NatsConnection | null = null;
  const backoff = new Backoff();
  store.setConnection("connecting");
  void (async () => {
    const c = await connectRetrying(
      () =>
        wsconnect({
          servers: hubUrl(window.location, viewer.path),
          authenticator: nkeyAuthenticator(new TextEncoder().encode(viewer.seed)),
          name: "hgl-ai console page",
          inboxPrefix: INBOX_PREFIX,
          maxReconnectAttempts: -1,
          reconnectDelayHandler: () => backoff.next(),
        }),
      (err) => store.setConnection("failed", String(err)),
      () => stop,
      backoff,
    );
    if (!c) return;
    if (stop) return void c.close();
    nc = c;
    store.setConnection("connected");
    void (async () => {
      for await (const s of c.status()) {
        if (s.type === "disconnect") store.setConnection("reconnecting");
        else if (s.type === "reconnecting") store.setConnection("reconnecting");
        else if (s.type === "reconnect") {
          backoff.reset();
          store.setConnection("connected");
        }
      }
    })();
    void c.closed().then((err) => {
      if (!stop) store.setConnection("disconnected", err ? String(err) : "");
    });
    for (const src of SOURCES) void watch(c, store, src, () => stop);
  })();
  return async () => {
    stop = true;
    await nc?.close();
  };
}
