---
title: Dashboard config
order: 7
tagline: The four knobs controlling whether the agent dashboard starts and where it binds.
---
# Dashboard config

> Reference for the four knobs that control whether the unified
> agent dashboard starts and where it binds.

## Knobs

| input | type | source | precedence |
|---|---|---|---|
| `--dashboard-port <PORT>` | `u16` | `quorum serve` CLI flag | wins over yaml |
| `dashboard_port` | `Option<u16>` | top-level field in `quorum.yml` | falls back to None |
| `--dashboard-bind <ADDR>` | `IpAddr` string | `quorum serve` CLI flag | wins over env var |
| `QUORUM_DASHBOARD_BIND` | `IpAddr` string | env var | falls back to `127.0.0.1` |

## Resolution

```text
port = --dashboard-port > quorum.yml::dashboard_port > None
bind = --dashboard-bind > $QUORUM_DASHBOARD_BIND > "127.0.0.1"
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

## Security note (no built-in auth)

The dashboard has **no authentication**. Loopback binds are an
implicit access control. Non-loopback binds expose every endpoint
to the network segment. On non-loopback bind the server emits:

```text
WARN dashboard bound to non-loopback address — control plane is
     reachable from the network with no built-in authentication.
```

See [how-to/expose-agent-dashboard-on-lan.md] for hardening
patterns (firewall rules, reverse proxy with auth).

## See also

- [how-to/expose-agent-dashboard-on-lan.md] — operator recipe
- `crates/quorum-rs/src/status/multi_server/mod.rs::run_control_plane`
  — bind resolution + boot
- `crates/quorum-rs/src/config.rs::AgentFleetConfig` — `dashboard_port` field
- `crates/quorum-rs/src/main.rs::Serve` — CLI flags
