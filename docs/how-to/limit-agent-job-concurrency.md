# How to limit an agent's concurrent jobs

By default an agent processes many jobs at once. If an agent's jobs mutate shared
state — e.g. a middleware that resets a git repo per job — concurrent jobs race and
can corrupt that state. Cap the agent to one job at a time (or a small number).

## Set it

In the agent's fleet config (`quorum.yml`):

```yaml
agents:
  - name: "MyAgent"
    provider_id: "claude_cli"
    max_concurrent_jobs: 1   # process one job at a time
```

`max_concurrent_jobs` is optional; omit it to leave the agent unbounded.

## What it does

It sets the agent's JetStream pull-consumer `max_ack_pending` to that number. The
broker will not deliver the next task until an in-flight one is acked (i.e. its
job finishes), so no more than `N` jobs run concurrently for that agent. This is
durable — it survives worker restarts, unlike an in-process limit.

## When to use it

- **Set `1`** for agents whose jobs share a mutable resource that isn't
  concurrency-safe (a working repo, a scratch directory a middleware rewrites,
  an external tool with a single session).
- **Leave it unset** for stateless agents — bounding them only costs throughput.

Restart the agent (or `quorum serve`) after changing it; the consumer config is
applied at worker startup.
