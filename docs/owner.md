# Owner channel and console

Status: proposed — not implemented. Nothing here is built yet: today the controller is
an interactive seat (`note wait`, PROTO-6), and the bus has one hub credential. The
current rules are in `docs/protocol.md`, `docs/roles.md` and `docs/design.md`.

The controller runs headless, like every other seat. The owner talks to it through a
web console served beside the hub: status reports, questions, conversations, and
live telemetry. GitHub stays the record (PROTO-1).

## 1. Interactions

- **OWN-1 — Status.** The controller posts a status report on a schedule
  (`[console] status_every`, 2 h) and after a notable event (a landing, a
  release, a blocked seat, a gate failing twice). A report is terse Markdown: what
  landed, what is in flight, what is blocked, what changed in the backlog. Reports
  are kept; the console shows the newest first.
- **OWN-2 — Questions.** A question from the controller or triage carries:
  - `id` (the thread), `title` (a short description), `priority` (`p1` blocks work
    now, `p2` blocks work soon, `p3` whenever), and the issue it concerns
    (`repo#number`) or another identifier;
  - `context`, the `question`, the `options` (each a label and a line), and a
    `suggested` option.

  The owner accepts the suggestion, picks another option, rejects, or writes an
  answer of their own ("other"), with an optional note. The answer goes to the asker
  as an `answer` note; follow-ups stay in the same thread. A question is open until
  answered or withdrawn.
- **OWN-3 — Conversations.** The owner starts a thread with a short description and
  a message; the controller replies in it. Either side may continue it. Threads are
  found by description, issue, state and date.
- **OWN-4 — Decisions reach the record.** An answer or a conversation message that
  decides something is posted by the controller, once, on the owning issue, quoting
  the owner's words and naming the thread id. Until it is posted it is not a rule.

## 2. Owner words

- **OWN-5 — Only the console speaks for the owner.** Notes and thread messages with
  sender `owner` are published only by the console's hub credential. The hub gives
  each client its own user and permissions: agent seats cannot publish as `owner`
  or to the owner subjects. A message claiming the owner's words that did not come
  through the console is agent traffic (PROTO-3).
  - The owner subjects are `hgl.owner.>` (thread events) and `hgl.notes.owner.<seat>`
    (notes to a seat); `HGL_THREADS` is the console's alone. Seat and coordinator users
    are denied all three (docs/install.md, Hub users).
  - NATS stores a message under the subject it was published on, so a receiver checks
    the subject, never a header or the payload: the bridge accepts sender `owner` only on
    `hgl.notes.owner.<recipient>` and nothing else there; the console folds owner events
    only from `hgl.owner.thread.<id>` and agent events only from `hgl.console.thread.<id>`.
  - The server side (hub, coordinator, console) runs as its own OS user, so no agent,
    which runs as the client user, can read the console's credential; `doctor` checks it.
- **OWN-6 — Sign-in.** The console listens on a configured address (loopback or a
  tailnet address, never a public interface) over HTTPS, at a stable host name, which
  passkeys need. Users sign in with a passkey (WebAuthn). A user's first sign-in, and
  adding a passkey for a new device, goes through GitHub, Sign in with Apple or Google;
  the console then registers the passkey. Only users the console's configuration lists
  (`[console] users`) are let in; the host owner edits that list. The OAuth client secrets live in files of mode 0600 beside the
  configuration, never in the repository. Every owner message records which user sent it.
  - HTTPS is the console's own (`tls_cert`, `tls_key`) or a local TLS proxy's in front of
    plain HTTP on loopback (`tailscale serve`); `[console] url` is the origin either way.
  - Sessions: random ids in HMAC-signed cookies (HttpOnly, Secure, SameSite=Strict),
    `session_ttl` (7 d), kept on the hub in `HGL_CONSOLE` (only the console's user reads
    or writes it; a session is keyed by the SHA-256 of its id), so a restart signs no
    one out. A form post needs the console's `Origin` and the session's CSRF token.
  - A user is keyed by email: `{ email, handle, github, apple, google }`. Google signs
    in by its verified email and Apple by the email in its `id_token` (a private relay
    address needs `apple`); both default to `email`. GitHub signs in by `github` (a
    login), else by the account's verified emails (`user:email`) for a user without one.
    An identity matching no user, or two, is refused. The kinds never mix: a `github:`
    login, an `apple:` email and a `google:` email are three identities.
  - Google is OpenID Connect with PKCE, `state` and `nonce`. Google's and Apple's
    `id_token`s are checked against their published keys (JWKS, cached for their
    `max-age`), then issuer, audience, expiry, nonce and `email_verified`.
  - The OAuth flow cookie is `SameSite=None` (Apple returns by a cross-site form post)
    and only names a pending flow.
  - Passkeys: `<data_dir>/console/passkeys.json` (mode 0600), with the cookie key.
- **OWN-9 — Notifications.** A new `p1` question notifies at once; `p2` questions are
  batched into one notification per hour; `p3` questions and status reports show only
  in the console. Channels are tried in the configured order and the first that
  accepts is used: a phone push (an ntfy topic) or a Slack incoming webhook, then
  email as the last resort. A notification carries the title, priority and a link to
  the thread, never the context. An email channel without `to` goes to every user's
  `email`. The pending `p2` batch is kept in `HGL_CONSOLE` across restarts and cleared
  once a channel took it. A `p1` that no channel took is kept there too, shown on every
  console page as not delivered, and retried from 30 s, doubling to 30 min, until one
  takes it or the question closes.
- **OWN-10 — Handles.** Each user signs the owner's words with a handle of their own
  choosing (`handle`, 1 to 31 letters, digits, spaces or `-_.'`, unique ignoring case
  and repeated spaces; shorter reads better). Every owner event records it; owner notes
  carry it (`by`), the console shows `This Dude, 27 Nov 14:05`, the prompt says
  `This Dude (owner) says:`, and `decisions.log` names it. An agent quoting an owner
  decision on GitHub writes the handle and date, then the words:
  `This Dude, 2026-11-27: "..."`.

## 3. Telemetry

One page, live (server-sent events), from the hub and the coordinator, read every
10 s as the console's hub user:

| Panel | Source |
| --- | --- |
| Hosts and seats: live, role, runtime, model, memory, free disk | `HGL_HOSTS` |
| Tasks in flight: seat, issue, stage (assigned, working, ready, review, sendback), last note, age | `HGL_TREES`, the notes stream |
| Pull requests: head, bot, threads, checks | `HGL_STATE/prs` |
| Queue: claimable issues in order, priority first | `HGL_STATE/backlog` |
| Backlog by label and repository; opened, closed and net per day (burndown) | `HGL_METRICS` |
| Landings and releases | `HGL_STATE/landed.log` |
| Fleet: `mode`, `hold`, stale trees, open questions by priority | `HGL_STATE`, `HGL_TREES`, threads |

The coordinator service adds two writes to its round: `backlog` (open issues by
label, the ordered claimable queue) every 15 min, and one `HGL_METRICS` sample per
repository per day.

- `backlog`: `# read <UTC>`, `# open <repo> <n>`, `# label <repo> <label> <n>`, then
  rows `REPO NUMBER CLASS CREATED TITLE`: `codex-priority` issues, then `codex`, each
  oldest first; `codex-claimed`, `codex-blocked`, `needs-owner`, `codex-ready` left out.
  Each class is its own connection, read to its last page.
- `HGL_METRICS/<repo>.<YYYYMMDD>`: `{repo, date, open, opened, closed, labels,
  taken_at}`, rewritten each backlog round; the day's last write is its sample.
- A task's stage is its last note's kind: `assign` assigned, `ready` ready, `review` or
  `receipt` review, `sendback` sendback, `approved`, `blocked`, else working.

## 4. Storage

| Primitive | Name | Content |
| --- | --- | --- |
| Stream | `HGL_OWNER`, subjects `hgl.owner.>`, `hgl.console.>` | status reports, thread events; no age limit; dedup 24 h |
| KV | `HGL_THREADS` | one record per thread: id, kind, title, issue, priority, state, times, options, suggested, answer, messages |
| KV | `HGL_METRICS` | daily samples per repository |
| KV | `HGL_CONSOLE` | the console's sessions, pending `p2` batch and undelivered `p1`s; the console's user only |

| Subject | Publisher | Event |
| --- | --- | --- |
| `hgl.console.status` | controller | a status report |
| `hgl.console.thread.<id>` | agents | `ask`, `message`, `withdraw` |
| `hgl.owner.thread.<id>` | console | `answer`, `owner_message`, `start`, `close`, with the signed-in user |
| `hgl.notes.owner.<seat>` | console | the `answer` or `note` a folded owner event sends |

- Thread ids are `q-` (question) or `c-` (conversation) and 8 hex digits.
- The console alone writes `HGL_THREADS`: its durable feed `console` holds one event in
  flight and folds them in order; a failed fold stops the feed until that event folds.
  A fold sends the owner's notes, appends an answer to `decisions.log` (once), then
  writes the record; each step is idempotent, and the stream sequence in the record
  makes a refold a no-op. A record stays within 256 KiB: the newest 50 messages, long
  texts shortened, a count of what it leaves out; `thread show` and the console read a
  long thread whole by folding its events again from `HGL_OWNER`. Until the console has folded an `ask`, `thread show` does not
  find it.
- `decisions.log`: `HGL_STATE`, newest 2000 lines and at most 768 KiB, trimmed in the
  same compare-and-set as the append; a line per owner answer (from the console) and per
  `hgl-ai decision add` (`<UTC> <seat>: <text>`).

## 5. The headless controller

- **OWN-7 — Seat.** The controller is seat `claude` served by a bridge like any other
  (`role = "controller"`, `headless = true`; without it the controller stays
  interactive, PROTO-6, until a controller runbook exists). Its turns start on notes,
  on owner answers and messages, and on the status schedule (a `tick` note from the
  console).
  - Ticks fall on multiples of `status_every` since the epoch, so a restarted console
    resends the same tick and the hub drops it.
- **OWN-8 — Memory.** The controller keeps a decision log: every owner ruling and
  every controller judgement that a later turn needs, appended to `HGL_STATE`
  `decisions.log` (newest kept, the full text mirrored to the owning issues). Each
  turn reads the log's tail, the open threads and `main`, never a previous turn's
  transcript.

## 6. Commands

```
hgl-ai console run                 the web server (the hub's user service runs it)
hgl-ai ask --title T --priority p1 --issue R#N --context-file F --question Q \
    --option a:"…" --option b:"…" --suggest a      open a question; prints its id
hgl-ai thread list [--open] [--kind question|conversation] [--search TEXT]
hgl-ai thread show ID
hgl-ai thread reply ID --body-file F
hgl-ai thread withdraw ID [--reason R]
hgl-ai status post --body-file F
hgl-ai status list [--days N]
hgl-ai decision add LINE           a controller judgement (OWN-8)
hgl-ai decision tail [N]
```

The owner answers only in the console. Pages: status reports (newest first), questions
(open first, by priority; accept, option, reject or other, with a note), conversations
(start, reply, close; search by description, issue, state, date), telemetry. One
embedded script and stylesheet, no CDN; works without the script except for passkeys
and live updates.
