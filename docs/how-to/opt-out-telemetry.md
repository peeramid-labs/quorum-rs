---
title: Opt out telemetry
order: 13
tagline: Disable all telemetry emission from your agent with a single config flag.
---
# Opt out telemetry

Disable all telemetry emission from your agent.

## Steps

Set `telemetry.enabled: false` in your agent config:

```yaml
telemetry:
  enabled: false
```

Default is `enabled: true`. With this flag off, your agent participates in the deliberation normally but publishes zero telemetry events. Operational visibility for your agent is then limited to what the orchestrator observes externally.
