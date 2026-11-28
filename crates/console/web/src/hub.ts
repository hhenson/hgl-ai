// Reading the hub over WebSocket as the `viewer` user (docs/owner.md §3): one watch per
// telemetry bucket, fed into the store. A first connection that fails is tried again; a made one
// reconnects by itself; a watch that ends is started again and its bucket resynchronised.

import { Kvm } from "@nats-io/kv";
import { type NatsConnection, nkeyAuthenticator, wsconnect } from "@nats-io/nats-core";
import type { Store } from "./store";
import { BUCKETS, type Bucket } from "./tables";

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

async function watch(nc: NatsConnection, store: Store, bucket: Bucket, stopped: () => boolean): Promise<void> {
  while (!stopped() && !nc.isClosed()) {
    try {
      const kv = await new Kvm(nc).open(bucket);
      store.resync(bucket);
      const iter = await kv.watch();
      // Nothing to catch up on: the bucket is empty, or its values have all come.
      let quiet = setTimeout(() => store.synced(bucket), 1500);
      for await (const e of iter) {
        store.put(bucket, e.key, e.operation === "PUT" ? e.string() : null, e.created.getTime(), e.isUpdate);
        clearTimeout(quiet);
        if (e.isUpdate) store.synced(bucket);
        else quiet = setTimeout(() => store.synced(bucket), 1500);
        if (stopped()) iter.stop();
      }
    } catch (err) {
      console.warn(`watching ${bucket}:`, err);
    }
    if (!stopped()) await new Promise((r) => setTimeout(r, RETRY_MS));
  }
}

/**
 * `connect` until it succeeds, waiting `RETRY_MS` after each failure; null once `stopped()`.
 * The client retries only a connection it has made: a first attempt that fails is final.
 */
export async function connectRetrying<T>(
  connect: () => Promise<T>,
  failed: (err: unknown) => void,
  stopped: () => boolean,
  wait: (ms: number) => Promise<void> = (ms) => new Promise((r) => setTimeout(r, ms)),
): Promise<T | null> {
  while (!stopped()) {
    try {
      return await connect();
    } catch (err) {
      failed(err);
    }
    await wait(RETRY_MS);
  }
  return null;
}

/** Connect and watch every bucket; returns a function that closes it all, retries included. */
export function start(viewer: Viewer, store: Store): () => Promise<void> {
  let stop = false;
  let nc: NatsConnection | null = null;
  store.setConnection("connecting");
  void (async () => {
    const c = await connectRetrying(
      () =>
        wsconnect({
          servers: hubUrl(window.location, viewer.path),
          authenticator: nkeyAuthenticator(new TextEncoder().encode(viewer.seed)),
          name: "hgl-ai console page",
          maxReconnectAttempts: -1,
          reconnectTimeWait: 2000,
        }),
      (err) => store.setConnection("failed", String(err)),
      () => stop,
    );
    if (!c) return;
    if (stop) return void c.close();
    nc = c;
    store.setConnection("connected");
    void (async () => {
      for await (const s of c.status()) {
        if (s.type === "disconnect") store.setConnection("reconnecting");
        else if (s.type === "reconnecting") store.setConnection("reconnecting");
        else if (s.type === "reconnect") store.setConnection("connected");
      }
    })();
    void c.closed().then((err) => {
      if (!stop) store.setConnection("disconnected", err ? String(err) : "");
    });
    for (const b of BUCKETS) void watch(c, store, b, () => stop);
  })();
  return async () => {
    stop = true;
    await nc?.close();
  };
}
