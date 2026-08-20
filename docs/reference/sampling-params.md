---
title: Sampling Parameters
order: 12
tagline: Which request fields are sent, when they are omitted, and how to talk to a backend that fixes them server-side.
---
# Sampling parameters

`temperature`, `presence_penalty`, `frequency_penalty` and `max_tokens` are
assembled by one helper shared by every chat strategy, so the rules below hold
whichever engine an agent resolves to.

## What is sent

| Field | Sent when |
| --- | --- |
| `temperature` | always, unless `omit_sampling_params` is set |
| `presence_penalty` | when resolved to a value and `omit_sampling_params` is unset |
| `frequency_penalty` | when the agent sets one and `omit_sampling_params` is unset |
| `max_tokens` | when greater than zero |

`presence_penalty` defaults to `1.5`. Setting `presence_penalty: null`
explicitly suppresses it; omitting the key does not.

## `omit_sampling_params`

```yaml
- name: "Example"
  model_name: "some-model"
  omit_sampling_params: true
  max_tokens: 8192
```

Some backends fix the sampling parameters server-side and **reject** a request
that carries them, rather than clamping the value. Those backends cannot be
satisfied by sending a "neutral" number, because there is no neutral number:
`temperature: 0.0` is a real request for deterministic output, so an unset
field and a zeroed field are indistinguishable on the wire.

`omit_sampling_params: true` drops all three sampling fields from the request.
It does not affect `max_tokens`, which such backends still expect.

## `max_tokens: 0`

Zero means "unset" here, and the field is omitted. It is not a portable way to
say "unlimited" — some backends accept it, others reject it outright with a
`400` naming their valid range. Since the config default is `0`, an agent that
never sets `max_tokens` sends no limit at all and inherits the backend's own
default.

## Provider throttles

`concurrency` and `qps` on a provider entry bound how hard the fleet may push
that endpoint. Both are shared by every agent pointed at the same `base_url`,
because provider limits are per account rather than per agent, and both are
per-process: N agent processes on one provider get N independent budgets.

```yaml
providers:
  example:
    type: openai
    base_url: "https://api.example.test/v1"
    concurrency: 10   # in-flight requests
    qps: 8            # requests per second
```

Omit either to leave that dimension unthrottled. `0` is read as unset, not as a
total block. A provider whose published cap is per-model cannot be expressed by
one entry — route each capacity class through its own entry.

## Value precision

Sampling values are rounded to two decimals before they are sent. The config
holds them as `f32`, which widens to its binary approximation on the wire —
`0.7` becomes `0.699999988079071` — and a backend that validates decimal
places rejects that as an illegal parameter rather than rounding it for us.

## Endpoint paths

A `base_url` that already names an API version is used verbatim; one that does
not gets `/v1` appended. Assuming `/v1` unconditionally puts a request for a
differently-versioned endpoint on `/v4/v1/chat/completions`, which 404s in a
way that reads like a bad model id rather than a client bug.

## Reasoning effort

`reasoning_effort` is independent of the above and is emitted top-level for
every engine except the two that need a different shape. Models that reason by
default can be expensive left unset; setting it explicitly is usually worth it.
