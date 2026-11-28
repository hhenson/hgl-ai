// One table as an AG Grid: columns from its definition, rows from the store, each change
// applied as a transaction by row id, so sort, filter, column widths and scroll stay. A
// table with `detail` fields shows them for the row last clicked.

import type { ColDef, GridApi, ICellRendererParams } from "ag-grid-community";
import { AgGridReact } from "ag-grid-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { Row } from "./diff";
import { age, relative, stampMs, when } from "./parse";
import type { Store } from "./store";
import type { Column, TableDef } from "./tables";

/** A stamp as the page shows it: from now, `12m ago`; the stamp itself otherwise. */
export function fromNow(v: unknown, now: number): string {
  const ms = stampMs(String(v));
  return ms === null ? String(v) : relative(ms, now);
}

/** A value as the grid shows it. */
function shown(c: Column, v: unknown): string {
  if (c.kind === "age") return age(Number(v));
  if (c.kind === "stamp") return fromNow(v, Date.now());
  return String(v);
}

/** Stamps sort to the second; rows with the same stamp by their `order`, if any. */
export function byStamp(a: unknown, b: unknown, ra?: Row, rb?: Row): number {
  const x = String(a ?? ""), y = String(b ?? "");
  if (x !== y) return x < y ? -1 : 1;
  return Number(ra?.order ?? 0) - Number(rb?.order ?? 0);
}

/** At least as wide as the header and the first rows' text, up to a limit. */
function width(c: Column, rows: Row[]): number {
  const text = rows.slice(0, 200).reduce((n, r) => {
    const v = r[c.field];
    return Math.max(n, v == null ? 0 : shown(c, v).length);
  }, 0);
  return Math.min(c.kind === "long" ? 280 : 260, Math.max(80, c.header.length * 8 + 76, text * 8 + 36));
}

/** A cell that links where its row's `href` field says, in a new tab. */
function Link(p: ICellRendererParams<Row> & { href: string }) {
  const url = p.data?.[p.href];
  const text = p.valueFormatted ?? (p.value == null ? "" : String(p.value));
  return typeof url === "string" && url ? (
    <a href={url} target="_blank" rel="noreferrer">
      {text}
    </a>
  ) : (
    <>{text}</>
  );
}

export function columnDefs(def: TableDef, rows: Row[]): ColDef<Row>[] {
  return def.columns.map((c) => {
    const numeric = c.kind === "num" || c.kind === "rank" || c.kind === "age";
    const col: ColDef<Row> = {
      field: c.field,
      headerName: c.header,
      headerTooltip: c.tip,
      minWidth: width(c, rows),
      // A position or an age moves without the value being news.
      enableCellChangeFlash: c.kind !== "rank" && c.kind !== "age",
      filter: numeric ? "agNumberColumnFilter" : "agTextColumnFilter",
      cellDataType: numeric ? "number" : "text",
      type: numeric ? "numericColumn" : undefined,
    };
    if (c.kind === "age") col.valueFormatter = (p) => (p.value == null ? "" : age(Number(p.value)));
    if (c.kind === "stamp") {
      col.valueFormatter = (p) => (p.value == null ? "" : fromNow(p.value, Date.now()));
      col.tooltipValueGetter = (p) => (p.value == null ? "" : when(String(p.value)));
      col.filterValueGetter = (p) => (p.data?.[c.field] == null ? "" : when(String(p.data[c.field])));
      col.comparator = (a, b, na, nb) => byStamp(a, b, na?.data, nb?.data);
    }
    if (c.kind === "long") col.tooltipField = c.field;
    if (c.href) Object.assign(col, { cellRenderer: Link, cellRendererParams: { href: c.href } });
    if (c.kind === "code") col.cellClass = "font-mono text-[0.9em]";
    if (c.kind === "long") Object.assign(col, { wrapText: true, autoHeight: true, flex: 3 });
    if (c.kind === "rank") Object.assign(col, { flex: 0, width: col.minWidth });
    if (def.sort?.[0] === c.field) col.sort = def.sort[1];
    return col;
  });
}

/** How often times shown from now are written again. */
const TICK_MS = 30_000;

export function Grid({ def, store }: { def: TableDef; store: Store }) {
  const api = useRef<GridApi<Row> | null>(null);
  const sized = useRef(false);
  const [picked, setPicked] = useState<string | null>(null);
  const [, rerender] = useState(0);
  const cols = useMemo(() => columnDefs(def, store.rows(def.id)), [def, store]);
  useEffect(() => {
    const s = store.subscribe(def.id, (t) => {
      const g = api.current;
      if (!g) return;
      g.applyTransaction(t);
      // A link cell's text may stay while its link comes or goes (the owner arrives after
      // the rows): the grid redraws only a cell whose own value changed.
      const links = def.columns.filter((c) => c.href).map((c) => c.field);
      if (links.length && t.update.length) g.refreshCells({ columns: links, force: true, suppressFlash: true });
      // Columns are sized from the first rows there are, before anyone changes them.
      if (!sized.current && t.add.length) {
        sized.current = true;
        g.setGridOption("columnDefs", columnDefs(def, store.rows(def.id)));
      }
      if (def.detail) rerender((n) => n + 1);
    });
    return s.unsubscribe;
  }, [def, store]);
  useEffect(() => {
    const stamps = def.columns.filter((c) => c.kind === "stamp").map((c) => c.field);
    if (!stamps.length) return;
    const t = setInterval(() => api.current?.refreshCells({ columns: stamps, force: true }), TICK_MS);
    return () => clearInterval(t);
  }, [def]);
  // A row's class follows the pick.
  useEffect(() => {
    if (def.detail) api.current?.redrawRows();
  }, [def, picked]);
  const row = def.detail && picked ? store.rows(def.id).find((r) => r.id === picked) : undefined;
  return (
    <>
      {def.explain ? <Key def={def} /> : null}
      <AgGridReact<Row>
        theme="legacy"
        className="ag-theme-quartz-auto-dark tgrid"
        columnDefs={cols}
        defaultColDef={{ sortable: true, resizable: true, flex: 1 }}
        getRowId={(p) => p.data.id}
        domLayout="autoHeight"
        ensureDomOrder
        enableCellTextSelection
        enableBrowserTooltips
        suppressDragLeaveHidesColumns
        cellFlashDuration={1200}
        cellFadeDuration={1500}
        rowClassRules={def.detail ? { picked: (p) => p.data?.id === picked } : undefined}
        onRowClicked={def.detail ? (e) => setPicked(e.data?.id ?? null) : undefined}
        overlayNoRowsTemplate={`<span class="empty">${escapeHtml(def.empty)}</span>`}
        onGridReady={(e) => {
          api.current = e.api;
          const rows = store.rows(def.id);
          sized.current = rows.length > 0;
          e.api.applyTransaction({ add: rows });
        }}
      />
      {def.detail ? (
        row ? (
          <Detail def={def} row={row} onClose={() => setPicked(null)} />
        ) : (
          <p className="meta -mt-2 mb-4">Select a row for its {def.detail.map((d) => d.header.toLowerCase()).join(", ")}.</p>
        )
      ) : null}
    </>
  );
}

/** What each column means, under the caption. */
function Key({ def }: { def: TableDef }) {
  const cols = def.columns.filter((c) => c.tip);
  return (
    <dl className="key">
      {cols.map((c) => (
        <div key={c.field}>
          <dt>{c.header}</dt>
          <dd>{c.tip}</dd>
        </div>
      ))}
    </dl>
  );
}

function Detail({ def, row, onClose }: { def: TableDef; row: Row; onClose: () => void }) {
  const first = def.columns[0];
  return (
    <section className="detail" aria-label={`Detail of ${String(row[first.field] ?? row.id)}`}>
      <div className="flex items-center justify-between gap-2">
        <p className="m-0 font-semibold">{String(row[first.field] ?? row.id)}</p>
        <button type="button" className="btn-quiet" onClick={onClose}>
          Close
        </button>
      </div>
      <dl className="m-0 mt-2 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1">
        {def.detail!.map((c) => {
          const v = row[c.field];
          const url = c.href ? row[c.href] : null;
          const text = v == null ? "–" : c.kind === "stamp" ? `${fromNow(v, Date.now())} (${when(String(v))})` : String(v);
          return (
            <div key={c.field} className="contents">
              <dt className="text-muted">{c.header}</dt>
              <dd className={`m-0 break-all ${c.kind === "code" ? "font-mono text-[0.9em]" : ""}`}>
                {typeof url === "string" && url ? (
                  <a href={url} target="_blank" rel="noreferrer">
                    {text}
                  </a>
                ) : (
                  text
                )}
              </dd>
            </div>
          );
        })}
      </dl>
    </section>
  );
}

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (ch) => `&#${ch.charCodeAt(0)};`);
}
