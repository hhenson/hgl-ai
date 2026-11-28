// The figures above the Usage and Backlog tables: a meter per limit of each login, and
// open issues per label as bars, one group per repository. Widths are SVG attributes (the
// console's CSP allows no inline style); each mark's <title> is its hover text, and the
// tables below are the same data as a table.

import { useSyncExternalStore } from "react";
import { relative, stampMs } from "./parse";
import type { Store } from "./store";
import { github } from "./tables";

/** How a limit reads: by its percent used and its run-out risk. */
export function severity(used: number, level: string): "ok" | "warn" | "crit" {
  if (used >= 90 || level === "out") return "crit";
  if (used >= 70 || level === "watch") return "warn";
  return "ok";
}

const WORD = { ok: "On track", warn: "Watch", crit: "At risk" } as const;
const ICON = { ok: "✓", warn: "⚠", crit: "✗" } as const;

export function UsageMeters({ store }: { store: Store }) {
  useSyncExternalStore(store.onMeta, store.getMeta);
  const rows = store.rows("usage").filter((r) => r.used_pct != null);
  if (!rows.length) return <p className="meta mt-3">No usage record.</p>;
  const now = Date.now();
  return (
    <>
      <p className="caption">
        Percent used of each limit. The tick marks where the window's burn rate so far puts it at the reset.
      </p>
      <ul className="meters">
        {rows.map((r) => {
          const used = Number(r.used_pct);
          const level = String(r.level ?? "unknown");
          const sev = severity(used, level);
          const projected = r.projected == null ? null : Math.min(100, Number(r.projected));
          const resets = stampMs(String(r.resets ?? ""));
          return (
            <li key={r.id} className={`meter sev-${sev}`}>
              <div className="flex flex-wrap items-baseline justify-between gap-x-3">
                <span className="font-semibold">{String(r.login)}</span>
                <span className="text-sm text-muted">
                  {String(r.runtime)} · {String(r.scope)} · {String(r.window)} window
                </span>
              </div>
              <svg className="meter-bar" width="100%" height="14" role="img" aria-label={`${used}% used`}>
                <title>{`${used}% used${projected === null ? "" : `; on pace for ${r.projected}% by the reset`}`}</title>
                <rect className="track" x="0" y="1" width="100%" height="12" rx="4" />
                {used > 0 ? <rect className="fill" x="0" y="1" width={`${Math.min(100, used)}%`} height="12" rx="4" /> : null}
                {projected !== null && projected > used ? (
                  <rect className="pace" x={`${Math.min(projected, 99.4)}%`} y="0" width="2" height="14" />
                ) : null}
              </svg>
              <p className="m-0 text-sm">
                <span className="font-semibold">{used}% used</span>
                {resets !== null ? <> · resets {relative(resets, now)}</> : null}
                {" · "}
                <span className={`status sev-${sev}`}>
                  <span aria-hidden="true">{ICON[sev]}</span> {WORD[sev]}
                </span>
                {": "}
                {String(r.risk ?? "")}
              </p>
            </li>
          );
        })}
      </ul>
    </>
  );
}

export function BacklogBars({ store }: { store: Store }) {
  useSyncExternalStore(store.onMeta, store.getMeta);
  const labels = store.rows("labels");
  const open = store.openByRepo();
  const owner = store.githubOwner();
  if (!labels.length && !open.size) return <p className="meta mt-3">No backlog reading yet.</p>;
  const repos = [...new Set([...open.keys(), ...labels.map((l) => String(l.repo))])].sort();
  const max = Math.max(1, ...labels.map((l) => Number(l.open)));
  return (
    <>
      <p className="caption">Open issues per label, one group per repository (an issue with two labels counts in both).</p>
      <div className="bars">
        {repos.map((repo) => {
          const rows = labels.filter((l) => l.repo === repo).sort((a, b) => Number(b.open) - Number(a.open));
          const base = github(owner).issue(repo, "");
          return (
            <section key={repo} className="bar-group" aria-label={`${repo}: open issues by label`}>
              <p className="m-0 mb-1 font-semibold">
                {base ? (
                  <a href={base.replace(/\/issues\/$/, "/issues")} target="_blank" rel="noreferrer">
                    {repo}
                  </a>
                ) : (
                  repo
                )}{" "}
                <span className="font-normal text-muted">· {open.get(repo) ?? "?"} open</span>
              </p>
              {rows.map((l) => (
                <div key={String(l.id)} className="bar-row">
                  <span className="bar-label" title={String(l.label)}>
                    {String(l.label)}
                  </span>
                  <svg width="100%" height="16" role="img" aria-label={`${l.label}: ${l.open} open`}>
                    <title>{`${repo} · ${l.label}: ${l.open} open`}</title>
                    <rect className="bar" x="0" y="2" width={`${(Number(l.open) / max) * 100}%`} height="12" rx="4" />
                    {/* Square at the baseline, round at the data end. */}
                    <rect className="bar" x="0" y="2" width="4" height="12" />
                  </svg>
                  <span className="bar-value">{String(l.open)}</span>
                </div>
              ))}
            </section>
          );
        })}
      </div>
    </>
  );
}
