// The telemetry page: the console's header, a tab per status area (the selected tab is
// the URL fragment), and the grids, kept current from the hub.

import * as Tabs from "@radix-ui/react-tabs";
import { useQuery } from "@tanstack/react-query";
import { useEffect, useState, useSyncExternalStore } from "react";
import { getSession, getViewer } from "./api";
import { BacklogBars, UsageMeters } from "./Charts";
import { Grid } from "./Grid";
import { start } from "./hub";
import type { Connection, Store } from "./store";
import { TABS } from "./tables";

const LABEL: Record<Connection, string> = {
  connecting: "Connecting to the hub…",
  connected: "Live",
  reconnecting: "Reconnecting…",
  disconnected: "Disconnected",
  failed: "Cannot reach the hub; retrying",
};

function tabFromHash(): string {
  const h = decodeURIComponent(window.location.hash.slice(1));
  return TABS.some((t) => t.id === h) ? h : TABS[0].id;
}

export function App({ store }: { store: Store }) {
  const session = useQuery({ queryKey: ["session"], queryFn: getSession, refetchInterval: 30_000, refetchIntervalInBackground: true, retry: false });
  const sessionValid = session.isSuccess && !session.isError;
  const viewer = useQuery({ queryKey: ["viewer"], queryFn: getViewer, enabled: sessionValid, staleTime: Infinity, gcTime: Infinity });
  const meta = useSyncExternalStore(store.onMeta, store.getMeta);
  const [tab, setTab] = useState(tabFromHash);

  useEffect(() => {
    const on = () => setTab(tabFromHash());
    window.addEventListener("hashchange", on);
    return () => window.removeEventListener("hashchange", on);
  }, []);
  useEffect(() => {
    const d = session.data;
    store.setContext({
      openQuestions: d?.open_questions ?? null,
      ...(d ? { staleAfterMs: d.stale_after_secs * 1000, hostsTtlMs: d.hosts_ttl_secs * 1000, githubOwner: d.github_owner } : {}),
    });
  }, [store, session.data]);
  useEffect(() => {
    if (!sessionValid || !viewer.data) return;
    const close = start(viewer.data, store);
    return () => void close();
  }, [store, viewer.data, sessionValid]);
  // On a narrow screen the tab strip scrolls sideways: keep the selected tab in view.
  useEffect(() => {
    document.getElementById(`tab-${tab}`)?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [tab]);
  // Ages move with the clock.
  useEffect(() => {
    const t = setInterval(() => store.flush(), 30_000);
    return () => clearInterval(t);
  }, [store]);

  const select = (id: string) => {
    history.replaceState(null, "", `#${id}`);
    setTab(id);
  };
  const open = session.data?.open_questions;
  const openCount = open ? Object.values(open).reduce((a, b) => a + b, 0) : null;
  const status =
    viewer.error ? `No hub credential: ${(viewer.error as Error).message}` :
    meta.connection === "connected" && meta.updated ? `Live; updated ${new Date(meta.updated).toLocaleTimeString()}` :
    LABEL[meta.connection] + (meta.detail ? `: ${meta.detail}` : "");
  const live = meta.connection === "connected";

  return (
    <>
      <a className="skip" href="#main">Skip to content</a>
      <header className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-line bg-panel px-4 py-3">
        <p className="m-0 font-semibold">hgl-ai console</p>
        <nav aria-label="Main">
          <ul className="m-0 flex list-none flex-wrap gap-x-4 gap-y-1 p-0">
            <li><a href="/">Status</a></li>
            <li><a href="/questions">Questions{openCount === null ? "" : ` (${openCount} open)`}</a></li>
            <li><a href="/conversations">Conversations</a></li>
            <li><a href="/telemetry" aria-current="page" className="font-semibold no-underline">Telemetry</a></li>
          </ul>
        </nav>
        <form className="ml-auto flex items-center gap-2 max-sm:ml-0" method="post" action="/auth/signout">
          <input type="hidden" name="csrf" value={session.data?.csrf ?? ""} />
          <span className="text-sm text-muted">{session.data?.user ?? ""}</span>
          <button type="submit" className="btn">Sign out</button>
        </form>
      </header>
      <main id="main" className="mx-auto max-w-[100rem] p-4 max-sm:px-2">
        <h1 className="mb-3 mt-1 text-2xl font-semibold">Telemetry</h1>
        <p className="meta flex items-center gap-2" role="status" aria-live="polite">
          <span className={`inline-block size-2.5 rounded-full ${live ? "bg-ok" : meta.connection === "failed" || meta.connection === "disconnected" || viewer.error ? "bg-warn" : "bg-pending"}`} aria-hidden="true" />
          {status}
        </p>
        <Tabs.Root value={tab} onValueChange={select} activationMode="manual">
          <Tabs.List aria-label="Telemetry" className="tabs sticky top-0 z-10 mt-2 flex gap-1 overflow-x-auto border-b border-line bg-bg">
            {TABS.map((t) => (
              <Tabs.Trigger key={t.id} value={t.id} id={`tab-${t.id}`} className="tab">
                {t.label}
                {t.count && meta.counts[t.id] !== undefined ? <span className="count">{meta.counts[t.id]}</span> : null}
              </Tabs.Trigger>
            ))}
          </Tabs.List>
          {TABS.map((t) => (
            // Every panel stays mounted, so a grid keeps its sort, filter and scroll.
            <Tabs.Content key={t.id} value={t.id} forceMount className="panel">
              {meta.notes[t.id] ? <p className="meta mt-3">{meta.notes[t.id]}</p> : null}
              {t.chart === "usage" ? <UsageMeters store={store} /> : t.chart === "backlog" ? <BacklogBars store={store} /> : null}
              {t.tables.map((d) => (
                <section key={d.id} aria-label={d.caption}>
                  <p className="caption">{d.caption}</p>
                  <Grid def={d} store={store} />
                </section>
              ))}
            </Tabs.Content>
          ))}
        </Tabs.Root>
      </main>
    </>
  );
}
