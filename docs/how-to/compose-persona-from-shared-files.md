# Compose a persona from shared files

> Recipe for fleet operators who want a single persona text built
> from reusable building blocks: a domain preamble, a
> review-style block, and the specific agent's quirks. Assumes
> you already have a working `quorum.yml` and one or more
> `prompts/*.md` files to compose.

## The problem this fixes

A plain-string `persona:` field in `quorum.yml` is fine for one or
two agents. As fleets grow, operators end up copying the same
4–30 lines of prose across every agent block (drift), or
collapsing everything into one mega-string (unreadable diffs, no
PR review hygiene).

`persona:` accepts a stacked-layer array shape on top of the
plain-string form. Each layer is either an inline text snippet or
a path to a markdown file that gets read at fleet boot. Layers
join with `\n\n` into a single persona string the LLM sees as one
prompt.

## The recipe

### 1. Lay out shared prompt files next to `quorum.yml`

```
agents/
  quorum.yml
  prompts/
    review-style.md
    output-format.md
    safety-rules.md
```

Each `prompts/*.md` file is plain text (markdown is convention,
not enforced — the deserializer only reads the bytes). Keep each
file to one concern so a single composition decision picks one
or two files, not whole concatenated docs.

### 2. Reference them in `quorum.yml`

```yaml
agents:
  - name: ReviewerBot
    persona:
      - type: md
        prompt: ./prompts/review-style.md
      - type: text
        prompt: "Focus on memory-safety issues specifically."
      - type: md
        prompt: ./prompts/output-format.md
```

Layers run top-to-bottom. The final persona string is:

```
<contents of review-style.md>

Focus on memory-safety issues specifically.

<contents of output-format.md>
```

— with `\n\n` between each layer.

### 3. Boot the fleet

```
quorum serve --config ./quorum.yml
```

If a path is wrong, fleet boot fails with a parse error naming
the failing file. Fix the path and retry. Operators see the error
before any agent connects to NATS — not after the agent is
already advertising a partial persona.

## When NOT to use layers

- One-off agents where the persona is two lines — the plain
  string is shorter to read.
- Templating / variable substitution inside the md files. That's
  not supported; layers concatenate raw file content.

## Path resolution gotcha

Paths in `md` layers resolve relative to the **process CWD when
`quorum serve` was invoked**, NOT relative to the `quorum.yml`
file's parent directory. The deserializer has no yaml-path
context.

Operators with one canonical `quorum serve` working directory
(typical: project root) can use repo-relative paths
(`./prompts/x.md`). Anyone who invokes `quorum serve` from
multiple CWDs should use absolute paths or set a `cd` step in
their service unit / Docker entrypoint.

## See also

- [reference/persona-yaml-shapes.md] — formal grammar of the two
  shapes the field accepts.
- [explanation/persona-layer-stacking.md] — why path resolution
  is CWD-relative and not yaml-relative.
