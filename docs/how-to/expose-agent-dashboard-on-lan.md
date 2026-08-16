---
title: Expose dashboard (LAN)
order: 11
tagline: Reach the per-agent control-plane dashboard from other hosts, and lock it down safely.
---
# Expose dashboard (LAN)

> Recipe for operators who want the unified per-agent control
> plane (status, chat capture, buffer inspection, live config
> tuning) reachable from machines other than the host running
> `quorum serve`. Assumes you already have a working `quorum.yml`
> and know the host's network configuration.

## The recipe

### 1. Pick a port

```yaml
# quorum.yml
dashboard_port: 8081
```

…or pass on the command line:

```bash
quorum serve --dashboard-port 8081
```

The CLI flag wins over the yaml field. Either alone is enough.
With both unset, no dashboard starts.

### 2. Pick a bind address

The dashboard defaults to `127.0.0.1` (loopback) — invisible from
LAN. To make it reachable from other hosts:

```bash
quorum serve --dashboard-port 8081 --dashboard-bind 0.0.0.0
```

Equivalent env var:

```bash
QUORUM_DASHBOARD_BIND=0.0.0.0 quorum serve --dashboard-port 8081
```

CLI flag wins over env var. Other usable values: a specific NIC
IP (e.g. `192.168.1.42`), `::` for dual-stack, or any address
`IpAddr::from_str` accepts.

### 3. Require a token

Before exposing beyond loopback, guard the `/api/*` control plane
with a bearer token:

```bash
QUORUM_DASHBOARD_TOKEN=$(openssl rand -hex 32) \
QUORUM_DASHBOARD_BIND=0.0.0.0 \
  quorum serve --dashboard-port 8081
```

With the token set, every `/api/*` request must carry
`Authorization: Bearer <token>` or it is rejected with `401`. The
dashboard page, Swagger UI, and `GET /auth/status` stay public so
the page loads and prompts for the token. Open the dashboard in a
browser, paste the token into the field in the top bar, and click
**Connect** — it is stored in `localStorage` and attached to every
guarded request.

Leave the token unset only for loopback dev; an unset token means
`/api/*` is open.

### 4. Verify

```bash
curl -sf http://<host>:8081/api/orchestrators \
  -H "Authorization: Bearer $QUORUM_DASHBOARD_TOKEN"
```

…should return JSON. Without the header (when a token is set) you
get `401`. If `curl` hangs from another machine, your host
firewall is blocking the port — see step 5. Check the guard state
any time with the public `curl -sf http://<host>:8081/auth/status`.

### 5. Defense in depth

The token is the primary control. It gives every host on the
segment access to the control plane **only** with the credential:

- per-agent status, including current job IDs and provider
  configuration
- chat capture endpoints
- response-buffer inspection (mid-flight LLM output)
- live config tuning (e.g. SLA changes that affect job routing)

Boot a non-loopback bind **without** a token and the server **refuses to
start the dashboard** (fail-closed — a missing token can't silently open it):

```text
ERROR refusing to start the dashboard: bound to a non-loopback address with
      no QUORUM_DASHBOARD_TOKEN — that would expose the control plane
      unauthenticated. Set QUORUM_DASHBOARD_TOKEN, or bind to loopback.
```

**Additional hardening (layer on top of the token):**

- Restrict the port to known IPs at the host firewall (`ufw allow
  from <admin-ip> to any port 8081`, or equivalent).
- Front with a reverse proxy for TLS (nginx / Caddy / traefik) and
  bind the dashboard to loopback so only the proxy can reach it.
- Treat the dashboard like an `ssh -L` tunnel destination for the
  most sensitive deployments.

If you don't need LAN reach today, don't enable it today. The
loopback default exists for a reason.

## Pull agent metrics + latest errors

Operators read an agent's health straight from the dashboard, independently of
the orchestrator (which deliberately does not carry agent-side failure
reasons — agents are independent and may not report them upstream):

```text
GET /api/agents/{name}/diagnostics
```

Returns the reliability metrics plus the newest error activity for one agent:

```json
{
  "name": "Corepunk18",
  "model_name": "openrouter/…",
  "uptime_secs": 3600,
  "tasks_completed": 12,
  "tasks_failed": 5,
  "error_rate": 0.29,
  "recent_errors": [
    { "timestamp": "…", "event_type": "agent_error", "job_id": "…",
      "detail": "API request failed with status 404 Not Found" }
  ],
  "recent_failed_tasks": [
    { "timestamp": "…", "action": "evaluate", "job_id": "…", "round": 4, "status": "error" }
  ]
}
```

Use it to answer "why is my engine misperforming" — an agent whose model
404s every round shows the exact upstream error here, even though the
orchestrator only ever saw an abstention. The listing (`GET /api/agents`)
carries the summary `error_rate`; this endpoint adds the actual errors. Both
are in the dashboard's OpenAPI (`/api-docs/openapi.json`).

## See also

- [reference/dashboard-config.md] — flags, env var, precedence,
  feature gate
- `crates/quorum-rs/src/status/multi_server/mod.rs::run_control_plane`
  — implementation
