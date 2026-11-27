# Usage collector fixtures

Real outputs captured from a client host's agent user (2026-11-27), every email,
account and organisation id replaced. `*.synthetic.*` files are made up in the
real shape.

| File | Source |
| --- | --- |
| `claude-usage.txt` | `claude -p "/usage"` |
| `claude-usage-session.synthetic.txt` | the same with a session line and a line that does not parse |
| `claude-auth.json` | `claude auth status` |
| `agy-usage.txt` | `agy -p "/usage"` |
| `agy-cli.log` | lines of agy's `log/cli-*.log` around its sign-in |
| `codex-rollout.jsonl` | the two real `token_count` events of a rollout, a stand-in `session_meta`, two filler lines |
| `codex-auth.synthetic.json` | the shape of `~/.codex/auth.json`, no real token |
