// hgl-ai console: passkey ceremonies, live telemetry and the answer form. Embedded in
// the binary; the CSP allows only the console's own scripts: this one and, on the
// telemetry page, the vendored AG Grid Community.
"use strict";

function b64urlToBuf(s) {
  s = s.replace(/-/g, "+").replace(/_/g, "/");
  while (s.length % 4) s += "=";
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

function bufToB64url(buf) {
  const bytes = new Uint8Array(buf);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function say(text) {
  const el = document.getElementById("auth-status");
  if (el) el.textContent = text;
}

async function post(url, body) {
  const r = await fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? "{}" : JSON.stringify(body),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data.error || ("HTTP " + r.status));
  return data;
}

async function register() {
  say("Waiting for your passkey…");
  const opts = await post("/auth/passkey/register/start");
  const pk = opts.publicKey;
  pk.challenge = b64urlToBuf(pk.challenge);
  pk.user.id = b64urlToBuf(pk.user.id);
  (pk.excludeCredentials || []).forEach((c) => { c.id = b64urlToBuf(c.id); });
  const cred = await navigator.credentials.create({ publicKey: pk });
  const resp = cred.response;
  const done = await post("/auth/passkey/register/finish", {
    id: cred.id,
    rawId: bufToB64url(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: bufToB64url(resp.attestationObject),
      clientDataJSON: bufToB64url(resp.clientDataJSON),
      transports: resp.getTransports ? resp.getTransports() : undefined,
    },
    extensions: cred.getClientExtensionResults ? cred.getClientExtensionResults() : {},
  });
  window.location.assign(done.redirect || "/");
}

async function signin() {
  say("Waiting for your passkey…");
  const opts = await post("/auth/passkey/login/start");
  const pk = opts.publicKey;
  pk.challenge = b64urlToBuf(pk.challenge);
  (pk.allowCredentials || []).forEach((c) => { c.id = b64urlToBuf(c.id); });
  const cred = await navigator.credentials.get({ publicKey: pk });
  const resp = cred.response;
  const done = await post("/auth/passkey/login/finish", {
    id: cred.id,
    rawId: bufToB64url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: bufToB64url(resp.authenticatorData),
      clientDataJSON: bufToB64url(resp.clientDataJSON),
      signature: bufToB64url(resp.signature),
      userHandle: resp.userHandle ? bufToB64url(resp.userHandle) : null,
    },
    extensions: cred.getClientExtensionResults ? cred.getClientExtensionResults() : {},
  });
  window.location.assign(done.redirect || "/");
}

function wire(id, fn) {
  const b = document.getElementById(id);
  if (!b) return;
  if (!window.PublicKeyCredential) {
    say("This browser has no passkey support.");
    b.disabled = true;
    return;
  }
  b.addEventListener("click", () => {
    b.disabled = true;
    fn().catch((e) => { say("Not signed in: " + e.message); b.disabled = false; });
  });
}

// ---- telemetry: tabs, and grids kept current by row diffs (crates/console/src/grid.rs) ----

function tabs() {
  const nav = document.querySelector("nav.tabs");
  if (!nav) return;
  const links = [...nav.querySelectorAll("a[data-tab]")];
  const panels = links.map((a) => document.getElementById(a.dataset.tab));
  if (!links.length || panels.some((p) => !p)) return;
  nav.querySelector("ul").setAttribute("role", "tablist");
  links.forEach((a, i) => {
    a.parentElement.setAttribute("role", "presentation");
    a.setAttribute("role", "tab");
    a.setAttribute("aria-controls", panels[i].id);
    panels[i].setAttribute("role", "tabpanel");
    panels[i].setAttribute("aria-labelledby", a.id);
  });
  document.body.classList.add("tabbed");
  const show = (id, focus) => {
    if (!links.some((a) => a.dataset.tab === id)) id = links[0].dataset.tab;
    links.forEach((a, i) => {
      const on = a.dataset.tab === id;
      a.setAttribute("aria-selected", on ? "true" : "false");
      a.tabIndex = on ? 0 : -1;
      panels[i].hidden = !on;
      if (on) {
        // On a narrow screen the strip scrolls sideways: keep the selected tab in it.
        const ul = a.closest("ul");
        const l = a.offsetLeft - ul.offsetLeft;
        if (l < ul.scrollLeft || l + a.offsetWidth > ul.scrollLeft + ul.clientWidth) ul.scrollLeft = l - 16;
        if (focus) a.focus();
      }
    });
  };
  // The selected tab is the URL fragment, so a reload or a shared link keeps it.
  const select = (id, focus) => {
    history.replaceState(null, "", "#" + id);
    show(id, focus);
  };
  links.forEach((a, i) => {
    a.addEventListener("click", (e) => { e.preventDefault(); select(a.dataset.tab); });
    a.addEventListener("keydown", (e) => {
      const n = links.length;
      const j = { ArrowRight: i + 1, ArrowLeft: i - 1 + n, Home: 0, End: n - 1 }[e.key];
      if (j === undefined) return;
      e.preventDefault();
      select(links[j % n].dataset.tab, true);
    });
  });
  window.addEventListener("hashchange", () => show(decodeURIComponent(location.hash.slice(1))));
  show(decodeURIComponent(location.hash.slice(1)));
  // Without this the browser keeps its jump to the fragment's panel, past the tabs.
  if (location.hash) window.scrollTo(0, 0);
}

function age(min) {
  const m = Math.max(0, min | 0);
  return m >= 1440 ? Math.floor(m / 1440) + "d" : m >= 60 ? Math.floor(m / 60) + "h" : m + "m";
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => "&#" + c.charCodeAt(0) + ";");
}

// Columns from the plain table's header: one definition, on the server. Each column is
// at least as wide as its header and the first rows' text, up to a limit.
function columnDefs(table, rows) {
  const [sortField, sortDir] = (table.dataset.sort || "").split(" ");
  return [...table.querySelectorAll("thead th[data-field]")].map((th) => {
    const kind = th.dataset.kind;
    const f = th.dataset.field;
    const text = rows.slice(0, 200).reduce((n, r) => {
      const v = r[f] == null ? "" : kind === "age" ? age(r[f]) : String(r[f]);
      return Math.max(n, v.length);
    }, 0);
    const minWidth = Math.min(kind === "long" ? 220 : 260,
      Math.max(80, th.textContent.length * 8 + 76, text * 8 + 36));
    const c = {
      field: th.dataset.field,
      headerName: th.textContent,
      enableCellChangeFlash: kind !== "rank" && kind !== "age",
      filter: "agTextColumnFilter",
      cellDataType: "text",
    };
    if (kind === "num" || kind === "rank" || kind === "age") {
      c.type = "numericColumn";
      c.filter = "agNumberColumnFilter";
      c.cellDataType = "number";
    }
    if (kind === "age") c.valueFormatter = (p) => (p.value == null ? "" : age(p.value));
    if (kind === "code") c.cellClass = "mono";
    c.minWidth = minWidth;
    if (kind === "long") Object.assign(c, { wrapText: true, autoHeight: true, flex: 3 });
    if (kind === "rank") Object.assign(c, { flex: 0, width: minWidth });
    if (c.field === sortField) c.sort = sortDir;
    return c;
  });
}

function makeGrid(table, rows) {
  const div = document.createElement("div");
  div.className = "tgrid ag-theme-quartz-auto-dark";
  const cap = table.querySelector("caption");
  const label = document.createElement("p");
  label.className = "caption";
  label.textContent = cap ? cap.textContent : "";
  table.after(label, div);
  table.hidden = true;
  return agGrid.createGrid(div, {
    theme: "legacy",
    columnDefs: columnDefs(table, rows),
    defaultColDef: { sortable: true, resizable: true, flex: 1 },
    getRowId: (p) => p.data.id,
    domLayout: "autoHeight",
    rowData: [],
    animateRows: false,
    ensureDomOrder: true,
    enableCellTextSelection: true,
    suppressDragLeaveHidesColumns: true,
    cellFlashDuration: 1200,
    cellFadeDuration: 1500,
    overlayNoRowsTemplate: '<span class="empty">' + escapeHtml(table.dataset.empty || "") + "</span>",
  });
}

function live() {
  const panels = document.getElementById("panels");
  const status = document.getElementById("live");
  if (!panels || !panels.dataset.sse || !window.EventSource) return;
  if (!window.agGrid) {
    if (status) status.textContent = "The grid did not load; this is the reading the page was loaded with.";
    return;
  }
  const grids = {};
  // A grid is made from the first rows it gets (they size its columns).
  const grid = (name, rows) => {
    if (!grids[name]) {
      const table = panels.querySelector('table[data-grid="' + name + '"]');
      if (!table) return null;
      grids[name] = makeGrid(table, rows);
    }
    return grids[name];
  };
  const meta = (m) => {
    if (!m) return;
    const read = document.getElementById("read");
    if (read) read.textContent = m.read;
    const err = document.getElementById("read-error");
    if (err) { err.textContent = m.error || ""; err.hidden = !m.error; }
    panels.querySelectorAll("[data-note]").forEach((p) => {
      const t = (m.notes || {})[p.dataset.note];
      p.textContent = t || "";
      p.hidden = !t;
    });
    Object.entries(m.counts || {}).forEach(([tab, n]) => {
      const c = document.querySelector("#tab-" + tab + " .count");
      if (c) c.textContent = n;
    });
    if (status) status.textContent = "Live; updated " + new Date().toLocaleTimeString();
  };
  const es = new EventSource(panels.dataset.sse);
  // Every row, first on each connection: applied as a transaction against what the grid
  // has, so a reconnect keeps sort, filter, scroll and selection too.
  es.addEventListener("reset", (e) => {
    const d = JSON.parse(e.data);
    Object.entries(d.tables || {}).forEach(([name, rows]) => {
      const api = grid(name, rows);
      if (!api) return;
      const want = new Set(rows.map((r) => r.id));
      const have = new Set();
      const remove = [];
      api.forEachNode((n) => { have.add(n.data.id); if (!want.has(n.data.id)) remove.push(n.data); });
      api.applyTransaction({
        add: rows.filter((r) => !have.has(r.id)),
        update: rows.filter((r) => have.has(r.id)),
        remove,
      });
    });
    meta(d.meta);
  });
  es.addEventListener("diff", (e) => {
    const d = JSON.parse(e.data);
    Object.entries(d.tables || {}).forEach(([name, t]) => {
      const api = grid(name, t.add || []);
      if (!api) return;
      api.applyTransaction({
        add: t.add || [],
        update: t.update || [],
        remove: (t.remove || []).map((id) => ({ id })),
      });
    });
    meta(d.meta);
  });
  es.onerror = () => { if (status) status.textContent = "Reconnecting…"; };
}

function answerForm() {
  const form = document.querySelector("form.answer");
  if (!form) return;
  const opt = form.querySelector(".when-option");
  const other = form.querySelector(".when-other");
  const sync = () => {
    const c = form.querySelector("input[name=choice]:checked");
    const v = c ? c.value : "";
    if (opt) opt.hidden = v !== "option";
    if (other) other.hidden = v !== "other";
  };
  form.querySelectorAll("input[name=choice]").forEach((r) => r.addEventListener("change", sync));
  sync();
}

document.addEventListener("DOMContentLoaded", () => {
  wire("passkey-register", register);
  wire("passkey-signin", signin);
  tabs();
  live();
  answerForm();
});
