# MCP & Multi-Host Client Configurations

`hgl-mcp` speaks `stdio` MCP locally and connects out to `nats-server` on TCP port `4222`, exposing 5 tools (`post_note`, `poll_inbox`, `get_state`, `put_state`, `list_hosts`).

## 1. Cross-Machine Transport (Port `4222`)

- **Tailscale / WireGuard**: Run `NATS_BIND=<tailscale-ip> NATS_TOKEN=<secret> ./bin/run-nats-hub` on the hub host and set `NATS_URL=nats://<tailscale-ip>:4222` on worker hosts.
- **OpenSSH port forward**:
  ```sh
  ssh -o ExitOnForwardFailure=yes -N -L 4222:127.0.0.1:4222 user@<hub-host>
  ```

---

## 2. Antigravity (`~/.gemini/config/mcp_config.json`)

```json
{
  "mcpServers": {
    "hgl-bus": {
      "command": "/path/to/hgl-ai/.venv/bin/hgl-mcp",
      "args": [],
      "env": {
        "NATS_URL": "nats://127.0.0.1:4222",
        "NATS_TOKEN": "YOUR_NATS_TOKEN"
      }
    }
  }
}
```

---

## 3. OpenAI Codex (`~/.codex/config.toml`)

```toml
[mcp_servers.hgl_bus]
command = "/path/to/hgl-ai/.venv/bin/hgl-mcp"
args = []

[mcp_servers.hgl_bus.env]
NATS_URL = "nats://127.0.0.1:4222"
NATS_TOKEN = "YOUR_NATS_TOKEN"
```

---

## 4. Claude Code (`~/.claude.json` or CLI)

```sh
claude mcp add hgl-bus \
  -e NATS_URL=nats://127.0.0.1:4222 \
  -e NATS_TOKEN="${NATS_TOKEN}" \
  -- /path/to/hgl-ai/.venv/bin/hgl-mcp
```

---

## 5. Shell & Coordinator Scripts via `hgl-ctl`

```sh
# Check if GitHub API hold is active (exits 0 with hold text if set, 1 if clear):
hgl-ctl state get hold

# Atomically publish the fresh PR status table from stdin:
printf '%s\n' "$ROWS" | hgl-ctl state put prs -

# Clear hold once rate limit resets:
hgl-ctl state del hold

# Post an urgent coordinator alert to the main session:
hgl-ctl post --from coord --to claude --issue 0 --kind note --body "$ALERT_MD"
```
