# Expose the agent dashboard on the LAN

> Recipe for operators who want the unified per-agent control
> plane (status, chat capture, buffer inspection, live config
> tuning) reachable from machines other than the host running
> `quorum serve`. Assumes you already have a working `agent.yml`
> and know the host's network configuration.

## The recipe

### 1. Pick a port

```yaml
# agent.yml
dashboard_port: 8081
```

…or pass on the command line:

```
quorum serve --dashboard-port 8081
```

The CLI flag wins over the yaml field. Either alone is enough.
With both unset, no dashboard starts.

### 2. Pick a bind address

The dashboard defaults to `127.0.0.1` (loopback) — invisible from
LAN. To make it reachable from other hosts:

```
quorum serve --dashboard-port 8081 --dashboard-bind 0.0.0.0
```

Equivalent env var:

```
QUORUM_DASHBOARD_BIND=0.0.0.0 quorum serve --dashboard-port 8081
```

CLI flag wins over env var. Other usable values: a specific NIC
IP (e.g. `192.168.1.42`), `::` for dual-stack, or any address
`IpAddr::from_str` accepts.

### 3. Verify

```
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

```
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

## See also

- [reference/dashboard-config.md] — flags, env var, precedence,
  feature gate
- `crates/quorum-rs/src/status/multi_server/mod.rs::run_control_plane`
  — implementation
