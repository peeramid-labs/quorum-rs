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

### 3. Verify

```bash
curl -sf http://<host>:8081/api/orchestrators
```

…should return JSON. If `curl` hangs from another machine,
your host firewall is blocking the port — see step 4.

### 4. Security: what you just turned on

**The dashboard ships with NO authentication.** Loopback binds
are an implicit access control. Anything wider gives every host
on the network segment full access to:

- per-agent status, including current job IDs and provider
  configuration
- chat capture endpoints
- response-buffer inspection (mid-flight LLM output)
- live config tuning (e.g. SLA changes that affect job routing)

Boot a non-loopback bind and the server emits a single warn line
to make the choice visible:

```text
WARN dashboard bound to non-loopback address — control plane is
     reachable from the network with no built-in authentication.
     Restrict access via the host firewall, an external reverse
     proxy with auth, or revert to the loopback default.
```

**Minimum hardening:**

- Restrict the port to known IPs at the host firewall (`ufw allow
  from <admin-ip> to any port 8081`, or equivalent).
- Front with a reverse proxy that adds auth (nginx + basic auth,
  Caddy + JWT, traefik + forward auth) and bind the dashboard to
  loopback so only the proxy can reach it.
- Treat the dashboard like an `ssh -L` tunnel destination, not a
  public service.

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
