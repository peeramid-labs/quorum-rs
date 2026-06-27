# `persona` yaml shapes

> Reference for the two shapes the `persona:` field on an agent
> entry accepts. The runtime always sees a single resolved
> `Option<String>`; the array form is collapsed at parse time.

## Grammar

```yaml
agents:
  - name: <agent_name>
    persona: <persona-source>
```

Where `<persona-source>` is one of:

### Shape A — inline string (back-compat)

```yaml
persona: "you are a careful reviewer"
```

Equivalent to `Some("you are a careful reviewer")` after
deserialization. Same as the pre-PR-#457 behaviour.

### Shape B — layered array

```yaml
persona:
  - type: <text | md>
    prompt: <inline-string | filesystem-path>
  - type: <text | md>
    prompt: <inline-string | filesystem-path>
  ...
```

Each layer is one map with two required keys:

| key | type | semantic |
|---|---|---|
| `type` | `"text"` or `"md"` | which layer variant |
| `prompt` | string | inline content (for `type: text`) or filesystem path (for `type: md`) |

Layers are processed in document order. The resolved persona is
the `\n\n`-separated join of all layers' contributed strings:

- `type: text` contributes its `prompt` verbatim.
- `type: md` reads the file at `prompt` and contributes its
  full content (no trimming, no template expansion).

### Shape C — null / absent

```yaml
persona: null
```

or omitted entirely. Resolves to `None`.

## Path semantics

`md` layer paths resolve against the **process CWD when the
yaml is parsed** (typically when `quorum serve` is invoked) —
NOT against the yaml file's parent directory. Absolute paths are
honoured as given. See
[explanation/persona-layer-stacking.md] for why CWD-relative.

## Error modes

| condition | behaviour |
|---|---|
| `type` is not `"text"` or `"md"` | parse error: serde reports the unknown variant |
| `md` layer's `prompt` file does not exist | parse error: `persona md layer at \`<path>\` could not be read: <io::Error>` |
| `md` layer's `prompt` file is unreadable (permissions, EIO) | same error as missing — surfaces the io error verbatim |
| `text` layer with empty `prompt: ""` | accepted; contributes the empty string (joined as `\n\n\n\n` between adjacent non-empty layers) |
| Mixed yaml — both string AND array on same field | impossible at the yaml syntax level; one or the other |

## Roundtrip serialisation

`AgentConfig` serialises `persona` back as the resolved string
(shape A). The original layered shape is NOT preserved on the
serialised form — by the time it leaves the deserializer it's a
single string. Operators editing `quorum.yml` by hand keep the
layered shape; tooling that emits `quorum.yml` from
`AgentConfig` instances writes shape A.

## See also

- [how-to/compose-persona-from-shared-files.md] — the recipe.
- [explanation/persona-layer-stacking.md] — design rationale.
- `crates/quorum-rs/src/agents/config.rs::deserialize_persona`
  — implementation.
