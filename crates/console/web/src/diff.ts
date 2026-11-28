// Row changes by id: what one AG Grid transaction applies.

export type Row = { id: string } & Record<string, unknown>;

export interface Transaction {
  add: Row[];
  update: Row[];
  remove: Row[];
}

function same(a: Row, b: Row): boolean {
  const ka = Object.keys(a);
  if (ka.length !== Object.keys(b).length) return false;
  return ka.every((k) => Object.is(a[k], b[k]));
}

/** The changes from `prev` to `next`: added and changed rows in `next`'s order. */
export function diffRows(prev: Row[], next: Row[]): Transaction {
  const before = new Map(prev.map((r) => [r.id, r]));
  const after = new Set(next.map((r) => r.id));
  const t: Transaction = { add: [], update: [], remove: [] };
  for (const r of next) {
    const old = before.get(r.id);
    if (!old) t.add.push(r);
    else if (!same(old, r)) t.update.push(r);
  }
  for (const r of prev) if (!after.has(r.id)) t.remove.push(r);
  return t;
}

export function isEmpty(t: Transaction): boolean {
  return !t.add.length && !t.update.length && !t.remove.length;
}
