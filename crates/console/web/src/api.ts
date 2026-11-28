// The console's own API: who is signed in, and the hub credential (docs/owner.md §3).

import type { Viewer } from "./hub";

export interface Session {
  user: string;
  csrf: string;
  open_questions: Record<string, number> | null;
  stale_after_secs: number;
  hosts_ttl_secs: number;
  /** `[github] owner`: the page links issues and pull requests there. */
  github_owner?: string;
}

async function get<T>(path: string): Promise<T> {
  const r = await fetch(path, { credentials: "same-origin", cache: "no-store", signal: AbortSignal.timeout(10_000) });
  if (r.status === 401) {
    window.location.assign("/signin");
    throw new Error("not signed in");
  }
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body.error ?? `HTTP ${r.status}`);
  return body as T;
}

export const getSession = () => get<Session>("/api/session");
/** Held in memory only (TanStack Query's cache), never in storage. */
export const getViewer = () => get<Viewer>("/api/viewer");
