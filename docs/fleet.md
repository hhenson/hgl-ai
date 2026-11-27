# Fleet

Status: proposed. The hub holds the fleet's configuration; a client registers and is
given its part. A host's own file keeps only what it needs to reach the hub.

## 1. Hosts

- **FLEET-1 — Name.** A client is named by its computer's host name (lowercase,
  `[a-z0-9-]`). The name lives in the hub's state and the host's file, never in a
  repository.
- **FLEET-2 — Registration.** `hgl-ai client join --hub URL --credential FILE --work-root DIR`
  registers the host with:
  - its name, OS and architecture;
  - capacity: cores, memory, free disk under `work_root`;
  - the agent CLIs found (`claude`, `codex`, `agy`), their versions, whether each is
    signed in, and the models each lists;
  - `work_root`, the folder every task directory of this host goes under.

  The host is `pending` until the owner (console or `hgl-ai fleet host accept NAME`)
  accepts it. The client refreshes capacity and CLIs with its heartbeat.
- **FLEET-3 — Local file.** After joining, `config.toml` holds the hub address, the
  credential file, the host name and `work_root`. Everything else comes from the hub.

## 2. Workers and roles

- **FLEET-4 — Workers.** The owner sets a host's worker count:
  `hgl-ai fleet host set host-1 --workers 6`. The host then runs seats
  `host-1-w1` .. `host-1-w6`. Lowering the count drains the highest-numbered seats
  first: each finishes its task, then stops.
- **FLEET-5 — Roles.** Single roles are placed on a host:
  `hgl-ai fleet role place controller --host host-2` (also `triage`, `coordinator`,
  `desk`, `console`). Each role has one seat, named by the role (`controller`,
  `triage`, ...). Moving a role drains it where it runs (the current turn finishes,
  its lease is released) and brings it up on the new host; queued notes follow, since
  the inbox lives on the hub.
- **FLEET-6 — Reviewers.** Reviewer seats are workers too: `fleet host set host-1
  --reviewers 2` adds `host-1-r1`, `host-1-r2`.
- **FLEET-7 — Model per task.** A worker is a slot, not a model. The controller or
  triage picks the family, tier and effort for each task by the model standard
  (`docs/roles.md` §3) and puts them in the `assign` or `review` note; the bridge runs
  that task's turns with that runtime and model, among the CLIs the host has signed
  in. A host that lacks the family declines the note, and the sender reassigns it.
  Fallback and the independence rule (ROLE-9) apply per task.

- **FLEET-8 — Settled with the owner.** The fleet's shape (workers and reviewers per
  host, where each role runs, the model mix) is worked out with the controller through
  the owner channel (`docs/owner.md`). The controller proposes a breakdown as a
  question: the hosts' registrations and load as context, options with their trade-offs,
  a suggestion. On the owner's answer it applies the plan with the `fleet` commands and
  logs it in `decisions.log`. A new host's registration, a host going quiet, or a
  subscription running out raises such a question. The owner may also set the plan
  directly in the console.

## 3. Configuration on the hub

KV `HGL_FLEET`:

| Key | Content |
| --- | --- |
| `host.<name>` | registration: OS, arch, capacity, CLIs and models, `work_root`, state (`pending`, `active`, `draining`, `retired`) |
| `plan.<name>` | the owner's settings for the host: workers, reviewers, roles placed, limits (memory cap, minimum free disk) |
| `roles` | role → host |
| `models` | the tier table per family (`docs/roles.md` §3) |
| `github`, `limits` | fleet-wide settings the hosts share |

The client watches `plan.<name>`, `roles` and the shared keys, and starts, drains or
stops seats as they change. Only admin and console credentials write `plan.*`,
`roles` and the shared keys; a host writes only its own `host.<name>`.

## 4. Commands

```
hgl-ai client join --hub URL --credential FILE --work-root DIR
hgl-ai fleet show                                  hosts, seats, roles, states
hgl-ai fleet host accept|retire NAME
hgl-ai fleet host set NAME [--workers N] [--reviewers N] [--memcap-gb G] [--min-free-gb G]
hgl-ai fleet role place ROLE --host NAME           place or move a role
```

The console shows the same and offers the same actions.
