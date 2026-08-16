---
title: Dashboard config
order: 7
tagline: The knobs controlling whether the agent dashboard starts, where it binds, and how it authenticates.
---
# Dashboard config

> Reference for the knobs that control whether the unified agent
> dashboard starts, where it binds, and how it authenticates.

## Knobs

| input | type | source | precedence |
|---|---|---|---|
| `--dashboard-port <PORT>` | `u16` | `quorum serve` CLI flag | wins over yaml |
| `dashboard_port` | `Option<u16>` | top-level field in `quorum.yml` | falls back to None |
| `--dashboard-bind <ADDR>` | `IpAddr` string | `quorum serve` CLI flag | wins over env var |
| `QUORUM_DASHBOARD_BIND` | `IpAddr` string | env var | falls back to `127.0.0.1` |
| `QUORUM_DASHBOARD_TOKEN` | `String` | env var | falls back to no auth |

## Resolution

```text
port  = --dashboard-port > quorum.yml::dashboard_port > None
bind  = --dashboard-bind > $QUORUM_DASHBOARD_BIND > "127.0.0.1"
token = $QUORUM_DASHBOARD_TOKEN > (unset → auth disabled)
```

When `port` resolves to `None`, **no dashboard starts** regardless
of bind. When `port` is set but the `status-server` feature is
compiled out, `quorum serve` logs a warn and continues without
the dashboard.

## Bind values

Any string `std::net::IpAddr::from_str` accepts:

| value | meaning |
|---|---|
| `127.0.0.1` (default) | loopback IPv4 — local only |
| `::1` | loopback IPv6 — local only |
| `0.0.0.0` | all IPv4 interfaces — LAN-visible |
| `::` | all interfaces, dual-stack — LAN-visible |
| `192.168.1.42` | bind to a specific NIC IP only |

Malformed input (typo, hostname instead of IP, garbage) is
**silently downgraded to loopback** — the boot-time `info!` log
line shows the resolved bind so the operator can spot the
fallback.

## Feature gate

The dashboard implementation lives behind `[features]
status-server` in `crates/quorum-rs/Cargo.toml`. The default
feature set on this crate is:

```text
default = ["audit", "cli", "tui", "status-server"]
```

…so a stock `cargo install` build includes the dashboard. Builds
that opt out (`--no-default-features --features cli,tui`) compile
the flag-resolution code but never start the server; a warn line
notes the no-op:

```text
WARN dashboard_port set but `status-server` feature not compiled in
     — no dashboard will start.
```

## Bearer-token auth

Set `QUORUM_DASHBOARD_TOKEN` to guard the `/api/*` control plane
with a bearer token. When set, every `/api/*` request must carry
`Authorization: Bearer <token>` or it is rejected with `401`. The
dashboard HTML page, the Swagger UI, `/api-docs/openapi.json`, and
`GET /auth/status` stay public so the page can load and the
frontend can prompt for the token before connecting. The dashboard
UI exposes a token field; the value is stored in `localStorage`
(`quorum_dashboard_token`) and attached to every guarded request.

Token comparison is constant-time. When `QUORUM_DASHBOARD_TOKEN`
is unset (or empty) auth is disabled and `/api/*` is open — the
historical loopback-default behaviour, intended for local dev.

`GET /auth/status` reports the guard state:

```json
{ "auth_required": true, "authenticated": false }
```

A non-loopback bind **without** a token is **fail-closed** — the server
**refuses to start the dashboard** rather than expose the control plane
unauthenticated:

```text
ERROR refusing to start the dashboard: bound to a non-loopback address with
      no QUORUM_DASHBOARD_TOKEN — that would expose the control plane
      unauthenticated. Set QUORUM_DASHBOARD_TOKEN, or bind to loopback.
```

Loopback binds stay open (local dev); anything wider requires a token. See
[how-to/expose-agent-dashboard-on-lan.md] for the full recipe
(token, bind, plus firewall / reverse-proxy hardening).

## See also

- [how-to/expose-agent-dashboard-on-lan.md] — operator recipe
- `crates/quorum-rs/src/status/multi_server/mod.rs::run_control_plane`
  — bind resolution + boot
- `crates/quorum-rs/src/config.rs::AgentFleetConfig` — `dashboard_port` field
- `crates/quorum-rs/src/main.rs::Serve` — CLI flags
