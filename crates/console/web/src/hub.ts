// Reading the hub over WebSocket as the `viewer` user (docs/owner.md §3): one watch per
// telemetry bucket, fed into the store. The connection reconnects by itself; a watch
// that ends is started again and its bucket resynchronised.

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
        store.put(bucket, e.key, e.operation === "PUT" ? e.string() : null);
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

/** Connect and watch every bucket; returns a function that closes it all. */
export async function start(viewer: Viewer, store: Store): Promise<() => Promise<void>> {
  let stop = false;
  store.setConnection("connecting");
  let nc: NatsConnection;
  try {
    nc = await wsconnect({
      servers: hubUrl(window.location, viewer.path),
      authenticator: nkeyAuthenticator(new TextEncoder().encode(viewer.seed)),
      name: "hgl-ai console page",
      maxReconnectAttempts: -1,
      reconnectTimeWait: 2000,
    });
  } catch (err) {
    store.setConnection("failed", String(err));
    throw err;
  }
  store.setConnection("connected");
  void (async () => {
    for await (const s of nc.status()) {
      if (s.type === "disconnect") store.setConnection("reconnecting");
      else if (s.type === "reconnecting") store.setConnection("reconnecting");
      else if (s.type === "reconnect") store.setConnection("connected");
    }
  })();
  void nc.closed().then((err) => {
    if (!stop) store.setConnection("disconnected", err ? String(err) : "");
  });
  for (const b of BUCKETS) void watch(nc, store, b, () => stop);
  return async () => {
    stop = true;
    await nc.close();
  };
}
