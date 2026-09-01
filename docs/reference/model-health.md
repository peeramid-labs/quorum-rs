---
title: Model Health
order: 15
tagline: How an agent decides its own model is unusable, and why only one kind of answer benches it.
---
# Model health

A roster pins each agent to a `model_name` on a provider. Providers retire
ids, rename them, and sometimes advertise ids they will not serve — and a dead
id fails every request the agent is ever given. An agent therefore judges its
own model and benches itself, so the scheduler stops assigning it work.

There are two detectors, and they answer different questions.

| | Detector | Question |
| --- | --- | --- |
| Reactive | `ModelDownDetector` | Did the task that just failed fail *because the model is gone*? |
| Proactive | `ModelAvailability` | Is the model usable *before* a task is spent finding out? |

Reactive detection alone means the first caller routed to a dead seat pays for
the discovery. The proactive probe exists so nobody does.

## What the proactive probe checks

One free request, throttled to once every five minutes: fetch the provider's
OpenAI-compatible `/models` list and look for the id. A successful fetch that
omits it is a verdict.

```rust
ModelAvailability::new(format!("{base}/models"), provider_id)
```

The probe never sends a completion. A catalog is an advertisement — a
provider can list an id whose `/chat/completions` answers `404` — but
confirming that costs a billed request per seat per interval, and a fleet
doing so paid for a token on every tick. A listed-but-unserved model is
instead caught by the reactive detector on its first real task.

## Fail-open is the invariant

`Availability::Unavailable` is returned **only** when the provider says the
model is not there — omitted from a successfully fetched catalog. Everything
else is `Availability::Unknown`, which never
benches:

- a catalog fetch that failed
- a rate limit, an auth failure, a provider `5xx`, a transport error
- an agent on a provider this probe does not cover
- a probe that has not run yet

The reason is blunt: every agent in a fleet shares a provider, so a rule that
benched on any error would turn one provider hiccup into a total outage. A
model that is briefly unreachable keeps serving; only one that is *reported
gone* stops.

## Benching

A verdict of `Unavailable` arms `model_down` for an escalating cooldown —
repeat strikes bench progressively longer, capped — and the heartbeat reports
`model_down` while the deadline stands. The scheduler excludes a down-model
agent from assignment even while it is otherwise online, and the bench clears
on expiry so a transient outage does not sideline an agent permanently.
