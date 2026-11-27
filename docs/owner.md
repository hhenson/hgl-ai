# Owner channel and console

Status: proposed.

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
- **OWN-6 — Sign-in.** The console listens on a configured address (loopback or a
  tailnet address, never a public interface) over HTTPS, at a stable host name, which
  passkeys need. Users sign in with a passkey (WebAuthn). A user's first sign-in, and
  adding a passkey for a new device, goes through GitHub or Sign in with Apple; the
  console then registers the passkey. Only users the console's configuration lists
  (`[console] users`: GitHub logins, Apple account emails) are let in; the host owner
  edits that list. The OAuth client secrets live in files of mode 0600 beside the
  configuration, never in the repository. Every owner message records which user sent it.
- **OWN-9 — Notifications.** A new `p1` question notifies at once; `p2` questions are
  batched into one notification per hour; `p3` questions and status reports show only
  in the console. Channels are tried in the configured order and the first that
  accepts is used: a phone push (an ntfy topic) or a Slack incoming webhook, then
  email as the last resort. A notification carries the title, priority and a link to
  the thread, never the context.

## 3. Telemetry

One page, live (server-sent events), from the hub and the coordinator:

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

## 4. Storage

| Primitive | Name | Content |
| --- | --- | --- |
| Stream | `HGL_OWNER`, subjects `hgl.owner.>` | status reports, thread messages; no age limit |
| KV | `HGL_THREADS` | one record per thread: id, kind, title, issue, priority, state, times, options, suggested, answer |
| KV | `HGL_METRICS` | daily samples per repository |

## 5. The headless controller

- **OWN-7 — Seat.** The controller is seat `claude` served by a bridge like any other
  (`role = "controller"`). Its turns start on notes, on owner answers and messages,
  and on the status schedule (a `tick` note from the console).
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
hgl-ai thread list [--open] [--kind question|conversation]
hgl-ai thread show ID
hgl-ai thread reply ID --body-file F
hgl-ai status post --body-file F
```
