// The page's state: each bucket's values as the hub's watches deliver them, the tables
// derived from them, and the listeners each table's grid registers. A change to a value
// becomes, per table, one transaction of the rows that changed (by row id).

import { diffRows, isEmpty, type Row, type Transaction } from "./diff";
import { type Bucket, type Context, derive, emptyRaw, notes, type Raw, TABLES, TABS } from "./tables";

export type Connection = "connecting" | "connected" | "reconnecting" | "disconnected" | "failed";

export interface Meta {
  connection: Connection;
  /** Why the connection failed, if it did. */
  detail: string;
  /** When a value last arrived. */
  updated: number | null;
  notes: Record<string, string>;
  counts: Record<string, number>;
}

type Listener = (t: Transaction) => void;

export class Store {
  private raw: Raw = emptyRaw();
  private rows_: Record<string, Row[]> = Object.fromEntries(TABLES.map((t) => [t.id, []]));
  private listeners = new Map<string, Set<Listener>>();
  private metaListeners = new Set<() => void>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  /** Keys seen since a bucket's watch (re)started; the rest go when it has caught up. */
  private syncing = new Map<Bucket, Set<string>>();
  /** When each `HGL_HOSTS` key was written, on the hub's clock. */
  private hostsWritten = new Map<string, number>();
  /** This page's clock minus the hub's, as live writes show it (the smallest gap seen). */
  private skew: number | null = null;
  meta: Meta = { connection: "connecting", detail: "", updated: null, notes: {}, counts: {} };

  constructor(
    private ctx: Omit<Context, "now" | "hostsAt"> = { staleAfterMs: 3 * 86_400_000, hostsTtlMs: 120_000 },
    private now: () => number = Date.now,
    /** How long changes gather before the tables are derived again. */
    private delayMs = 50,
  ) {}

  /** A value put (`value`) or deleted (`null`); `written` is when the hub stored it, and
   *  `live` whether it arrived as it happened (not while a watch caught up). */
  put(bucket: Bucket, key: string, value: string | null, written?: number, live = false): void {
    if (value === null) this.raw[bucket].delete(key);
    else this.raw[bucket].set(key, value);
    if (written !== undefined) {
      if (live) {
        const gap = this.now() - written;
        this.skew = this.skew === null ? gap : Math.min(this.skew, gap);
      }
      if (bucket === "HGL_HOSTS") {
        if (value === null) this.hostsWritten.delete(key);
        else this.hostsWritten.set(key, written);
      }
    }
    this.syncing.get(bucket)?.add(key);
    this.meta = { ...this.meta, updated: this.now() };
    this.schedule();
  }

  /** A bucket's watch starts (again): what it does not deliver before `synced` is gone. */
  resync(bucket: Bucket): void {
    this.syncing.set(bucket, new Set());
  }

  synced(bucket: Bucket): void {
    const seen = this.syncing.get(bucket);
    if (!seen) return;
    this.syncing.delete(bucket);
    for (const k of [...this.raw[bucket].keys()]) if (!seen.has(k)) this.raw[bucket].delete(k);
    this.schedule();
  }

  setContext(ctx: Partial<Omit<Context, "now" | "hostsAt">>): void {
    this.ctx = { ...this.ctx, ...ctx };
    this.schedule();
  }

  setConnection(connection: Connection, detail = ""): void {
    this.meta = { ...this.meta, connection, detail };
    this.emitMeta();
  }

  /** Derive now (ages move with the clock; tests call it directly). */
  flush(): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    const now = this.now();
    const skew = this.skew ?? 0;
    const hostsAt = new Map([...this.hostsWritten].map(([k, t]) => [k, t + skew]));
    const next = derive(this.raw, { ...this.ctx, now, hostsAt });
    for (const t of TABLES) {
      const tx = diffRows(this.rows_[t.id], next[t.id]);
      this.rows_[t.id] = next[t.id];
      if (!isEmpty(tx)) this.listeners.get(t.id)?.forEach((l) => l(tx));
    }
    const counts: Record<string, number> = {};
    for (const tab of TABS) if (tab.count) counts[tab.id] = next[tab.tables[0].id].length;
    this.meta = { ...this.meta, notes: notes(this.raw, now), counts };
    this.emitMeta();
  }

  private schedule(): void {
    if (!this.timer) this.timer = setTimeout(() => this.flush(), this.delayMs);
  }

  /** A table's rows now. */
  rows(table: string): Row[] {
    return this.rows_[table] ?? [];
  }

  /** The table's rows now; `l` gets each change after. */
  subscribe(table: string, l: Listener): { rows: Row[]; unsubscribe: () => void } {
    const set = this.listeners.get(table) ?? new Set();
    set.add(l);
    this.listeners.set(table, set);
    return { rows: this.rows_[table] ?? [], unsubscribe: () => set.delete(l) };
  }

  // For React's useSyncExternalStore.
  onMeta = (l: () => void): (() => void) => {
    this.metaListeners.add(l);
    return () => this.metaListeners.delete(l);
  };
  getMeta = (): Meta => this.meta;

  private emitMeta(): void {
    this.metaListeners.forEach((l) => l());
  }
}
