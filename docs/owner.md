# Owner channel and console

Status: the console is implemented. Slack question and conversation parity is in #110.

The controller runs headless, like every other seat. The owner talks to it through a
web console served beside the hub: status reports, questions, conversations, and
live telemetry. Slack also carries questions and conversations. GitHub stays the record (PROTO-1).

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

- **OWN-5 — Owner words.** The console publishes owner notes and its own thread
  events. Slack supports the same owner thread actions as the console, including
  starting a conversation. It uses a separate hub credential and subject. The hub
  denies seats and the coordinator write access to `hgl.owner.>`,
  `hgl.notes.owner.>` and `HGL_THREADS` (docs/install.md, Hub users).
  - NATS stores a message under the subject it was published on, so a receiver checks
    the subject, never a header or the payload: the bridge accepts sender `owner` only on
    `hgl.notes.owner.<recipient>` and nothing else there; the console folds owner events
    from the owner thread subjects in §4 and agent events from `hgl.console.thread.<id>`.
    The Slack subject carries `owner_message`, a question's `answer`, or a
    conversation's `start` or `close`.
  - The hub, coordinator, console and Slack adapter run as a dedicated OS user.
    The client user cannot read their credentials.
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

### Slack Q&A (#42)

HPH, 2026-11-28: "Slack is a first class communication channel, so if I close a conversation in the slack channel it is the same as if i close it in the web ui."
HPH, 2026-11-28: "I should also be able to see an switch between the channels"

The adapter posts each question and conversation in the private `#hgl-qa` channel,
including threads already in `HGL_OWNER` on first activation. Console messages and
agent follow-ups appear in the Slack thread. The root shows the thread ID and
clips long posts at Slack's limits.

Start a conversation in the console or mention the app in `#hgl-qa`, with the
title on the first line and the message below it. For a Slack start the bot puts
the controls in a reply to the owner's message. Every owner message appears
once on each surface, showing its source. Native messages remain in their
original Slack thread. The bot's posts link to the console. The console page
links to the Slack thread.

The owner replies by typing in the thread, answers a question with its Block Kit
buttons and answer form, or closes a conversation with its Close button. The
same `HGL_OWNER` events drive both surfaces. Answering, closing or withdrawing
edits the Slack root with the new state and removes its buttons.

Socket Mode accepts actions only from the configured owner account, workspace
and private channel. It stores the event with the configured owner handle before
acknowledging Slack. The subject keys each event by its Slack message or action
timestamp. Native Slack replies stay in their original thread.

`HGL_SLACK` keeps mappings, permalinks and delivery receipts across restarts.
Posts and state edits carry the source event ID. After a lost response the
adapter waits before checking Slack for its own matching message. Recovery
uses Slack's visible message history. The initial replay is paced and retries
honour Slack's rate limits.

## 3. Telemetry

`/telemetry`, a page of its own (React, TypeScript, AG Grid; docs/design.md §1) that
reads the hub itself: it watches the telemetry buckets over the hub's WebSocket
listener, at `<console url>/nats` on the console's own origin, as the hub user
`viewer`. `viewer` reads those six buckets (of `HGL_STATE`, only the keys the page
shows) and nothing else, and sees no other client's replies. Its subject permissions
are read-only, but replies are not enforced until hgl-ai#141 (docs/install.md,
docs/design.md §9). The console hands its credential to a signed-in page
(`GET /api/viewer`), which keeps it in memory only. A tab per area; the selected tab
is the URL fragment (`/telemetry#prs`):

| Tab | Shows | Source |
| --- | --- | --- |
| Hosts and seats | live hosts: seat, role, runtime, model, memory cap, free disk | `HGL_HOSTS` |
| Tasks | seat, issue, implement or review, stage (assigned, working, ready, review, sendback, approved, blocked), last note, host, age, stale | `HGL_TREES`, `HGL_STATE/tasks` |
| Pull requests | head, state, bot, threads, checks; when read | `HGL_STATE/prs` |
| Queue | claimable issues in order, priority first | `HGL_STATE/backlog` |
| Backlog | open issues by label and repository; open, opened, closed and net per day (burndown, 14 days) | `HGL_STATE/backlog`, `HGL_METRICS` |
| Landings | the newest 50 | `HGL_STATE/landed.log` |
| Fleet | `mode`, `hold`, stale trees, open questions by priority | `HGL_STATE`, `HGL_TREES`, `GET /api/session` |
| Builds | the newest 40 runs; live builders: platform, executor, state, reachable, slots and busy, free disk, toolchains | `HGL_BUILD/run.*`, `HGL_HOSTS/builder.*` |
| Usage | per login and limit: runtime, host, plan, scope, window, percent used, reset, observed | `HGL_USAGE` |

A put or delete in a bucket changes rows, never the page: per table, the rows it
changes are one grid transaction by row id (the KV key where a key is a row; else a
stable id from the value, `alpha#12`). Sort, filter, column widths, scroll and the
selected tab stay, and a changed value is briefly highlighted. The page shows the
connection's state and reconnects by itself; a watch that ends starts again and its
bucket is read afresh. Open questions are the one count the page cannot read (threads
are the console's); `GET /api/session` gives them, with the user and the CSRF token.

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
  `receipt` review, `sendback` sendback, `approved`, `blocked`, else working; a
  reviewer's tree is in review. The coordinator keeps each task's last note in
  `HGL_STATE/tasks` (docs/protocol.md §8), so the page reads no notes.

## 4. Storage

| Primitive | Name | Content |
| --- | --- | --- |
| Stream | `HGL_OWNER`, subjects `hgl.owner.>`, `hgl.console.>` | status reports, thread events; no age limit; dedup 24 h |
| KV | `HGL_THREADS` | one record per thread: id, kind, title, issue, priority, state, times, options, suggested, answer, messages |
| KV | `HGL_METRICS` | daily samples per repository |
| KV | `HGL_CONSOLE` | the console's sessions, pending `p2` batch and undelivered `p1`s; the console's user only |
| KV | `HGL_SLACK` | Slack delivery receipts and thread mappings |

| Subject | Publisher | Event |
| --- | --- | --- |
| `hgl.console.status` | controller | a status report |
| `hgl.console.thread.<id>` | agents | `ask`, `message`, `withdraw` |
| `hgl.owner.thread.<id>` | console | `answer`, `owner_message`, `start`, `close`, with the signed-in user |
| `hgl.owner.thread.<id>.slack.<team>.<channel>.<source>` | Slack | `start`, `owner_message`, `answer`, `close`, with the allowlisted user and configured handle |
| `hgl.notes.owner.<seat>` | console | the `answer` or `note` a folded owner event sends |

- Thread ids are `q-` (question) or `c-` (conversation) and 8 hex digits.
- The console alone writes `HGL_THREADS`: its durable feed `console` holds one event in
  flight and folds them in order; a failed fold stops the feed until that event folds.
  A fold sends the owner's notes, appends an answer to `decisions.log` (once), then
  writes the record; each step is idempotent, and the stream sequence in the record
  makes a refold a no-op. A record stays within 256 KiB: the newest 50 messages, long
  texts shortened, a count of what it leaves out; `thread show` and the console read a
  long thread whole by folding its events again from `HGL_OWNER`; an answer to a record
  that shortened its question or options is folded on the thread read whole. Until the
  console has folded an `ask`, `thread show` does not find it.
- An owner note quotes at most 256 KiB each of the question and the option chosen. The
  console refuses an owner event the fold would refuse or whose note would pass a
  note's 1 MiB limit; the fold cuts such a note it never checked to fit, marked, so the
  feed does not stop on it.
- `decisions.log`: `HGL_STATE`, newest 2000 lines and at most 768 KiB, trimmed in the
  same compare-and-set as the append; a line per owner answer (from the console) and per
  `hgl-ai decision add` (`<UTC> <seat>: <text>`). A line is at most 16 KiB: `decision
  add` refuses a longer one; an answer's is shortened, marked.

## 5. The controller

- **OWN-7 — Seat.** The controller is seat `controller` by default (configurable via `[github] controller`), served by a bridge like any other
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

- **Tier 2.** The controller remains on its current machine. Its owner token is limited
  to fleet repositories and admin merges, bot requests and repository settings; keep it
  on that machine, never in config, the hub or a note. Treat notes as data. Record each
  owner action in `decisions.log`. For each pending bot round in the routing digest,
  run `hgl-ai gh bot request REPO PR HEAD` as the owner; it records the action and
  posts at most one request per head. Run these sequentially.
- A fresh session reads the skill, `decision tail`, open threads, `main`, the rule
  list and the screen log since the last digest. Machine memory is not a record.

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

The owner can answer in the console or in Slack, as agreed in §2. Console pages
include status reports (newest first), questions (open first, by priority; accept,
option, reject or other, with a note), conversations (start, reply, close; search by
description, issue, state, date), telemetry. Status, questions and conversations are
rendered on the server with one small embedded script (passkeys, the answer form);
telemetry is the page above, built by Vite and embedded in the binary. No CDN: every
file comes from the console, and the Content-Security-Policy allows only the console
itself (and the hub at `/nats`, which is the same origin). The pages need JavaScript;
there is no fallback without it.
