// One table as an AG Grid: columns from its definition, rows from the store, each change
// applied as a transaction by row id, so sort, filter, column widths and scroll stay.

import type { ColDef, GridApi } from "ag-grid-community";
import { AgGridReact } from "ag-grid-react";
import { useEffect, useMemo, useRef } from "react";
import type { Row } from "./diff";
import { age } from "./parse";
import type { Store } from "./store";
import type { Column, TableDef } from "./tables";

/** At least as wide as the header and the first rows' text, up to a limit. */
function width(c: Column, rows: Row[]): number {
  const text = rows.slice(0, 200).reduce((n, r) => {
    const v = r[c.field];
    return Math.max(n, v == null ? 0 : (c.kind === "age" ? age(Number(v)) : String(v)).length);
  }, 0);
  return Math.min(c.kind === "long" ? 220 : 260, Math.max(80, c.header.length * 8 + 76, text * 8 + 36));
}

export function columnDefs(def: TableDef, rows: Row[]): ColDef<Row>[] {
  return def.columns.map((c) => {
    const numeric = c.kind === "num" || c.kind === "rank" || c.kind === "age";
    const col: ColDef<Row> = {
      field: c.field,
      headerName: c.header,
      minWidth: width(c, rows),
      // A position or an age moves without the value being news.
      enableCellChangeFlash: c.kind !== "rank" && c.kind !== "age",
      filter: numeric ? "agNumberColumnFilter" : "agTextColumnFilter",
      cellDataType: numeric ? "number" : "text",
      type: numeric ? "numericColumn" : undefined,
    };
    if (c.kind === "age") col.valueFormatter = (p) => (p.value == null ? "" : age(Number(p.value)));
    if (c.kind === "code") col.cellClass = "font-mono text-[0.9em]";
    if (c.kind === "long") Object.assign(col, { wrapText: true, autoHeight: true, flex: 3 });
    if (c.kind === "rank") Object.assign(col, { flex: 0, width: col.minWidth });
    if (def.sort?.[0] === c.field) col.sort = def.sort[1];
    return col;
  });
}

export function Grid({ def, store }: { def: TableDef; store: Store }) {
  const api = useRef<GridApi<Row> | null>(null);
  const sized = useRef(false);
  const cols = useMemo(() => columnDefs(def, store.rows(def.id)), [def, store]);
  useEffect(() => {
    const s = store.subscribe(def.id, (t) => {
      const g = api.current;
      if (!g) return;
      g.applyTransaction(t);
      // Columns are sized from the first rows there are, before anyone changes them.
      if (!sized.current && t.add.length) {
        sized.current = true;
        g.setGridOption("columnDefs", columnDefs(def, store.rows(def.id)));
      }
    });
    return s.unsubscribe;
  }, [def, store]);
  return (
    <AgGridReact<Row>
      theme="legacy"
      className="ag-theme-quartz-auto-dark tgrid"
      columnDefs={cols}
      defaultColDef={{ sortable: true, resizable: true, flex: 1 }}
      getRowId={(p) => p.data.id}
      domLayout="autoHeight"
      ensureDomOrder
      enableCellTextSelection
      suppressDragLeaveHidesColumns
      cellFlashDuration={1200}
      cellFadeDuration={1500}
      overlayNoRowsTemplate={`<span class="empty">${escapeHtml(def.empty)}</span>`}
      onGridReady={(e) => {
        api.current = e.api;
        const rows = store.rows(def.id);
        sized.current = rows.length > 0;
        e.api.applyTransaction({ add: rows });
      }}
    />
  );
}

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (ch) => `&#${ch.charCodeAt(0)};`);
}
