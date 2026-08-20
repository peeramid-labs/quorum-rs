---
title: Proposal Structure Metadata
order: 11
tagline: The measured shape evaluators see on each candidate, and the tools that address it by line.
---
# Proposal structure metadata

Every candidate in an evaluation prompt carries a measured summary of its
final solution's shape, plus an outline of line anchors. The conciseness
axis is scored from these numbers rather than from the evaluator's
impression of length.

## Candidate attributes

```
<candidate id="Candidate_A" chars="1721" lines="48" lead_chars="180"
           prose_chars="900" headings="4" code_blocks="2"
           list_items="12" table_rows="3">
<outline>L1 # Recommendation | L9 ## Trade-offs | L17 code | L31 table</outline>
```

| Attribute | Meaning |
| --- | --- |
| `chars` | Total characters of the final solution. |
| `lines` | Total lines. |
| `lead_chars` | Characters before the first structural break — how long the reader waits for the answer. |
| `prose_chars` | Characters outside code fences, lists and tables — where padding hides. |
| `headings`, `code_blocks`, `list_items`, `table_rows` | Structure counts. |
| `<outline>` | `L<line> <label>` anchors: headings verbatim (to 60 chars), `code` per fenced block, `table` per table. |

Structure inside a code fence is not counted as structure: a fence
containing `# comment` adds one `code_blocks`, not one heading.

Outline labels are candidate-authored text, so `&`, `<` and `>` are escaped
to entities. A heading containing `</outline>` appears as `&lt;/outline&gt;`
and cannot close the element it is listed in.

## Content kinds

The attribute names above are one vocabulary, measured by one analyzer per
kind of content. The analyzers are tried in order and markdown answers last,
so every proposal is measured.

| Kind | Recognised by | Analyzer |
| --- | --- | --- |
| Structured | Parses as a JSON object or array | `prompts::shape::json` |
| Prose | Everything else | `prompts::shape::markdown` |

A structured proposal is stored as compact JSON on a single line. Measured as
prose it would report no structure at all, with every character counted as
both `prose_chars` and `lead_chars` — the shape the conciseness axis scores
most negatively — so a structured proposal would lose the axis on its
serialisation. It is instead measured by what it is:

| Attribute | From the JSON |
| --- | --- |
| `headings` | Top-level keys. Nested keys are detail within a section, not sections. |
| `list_items` | Array elements, at any depth. |
| `code_blocks` | String values containing a newline — an embedded diff or snippet. |
| `prose_chars` | Characters inside string values, excluding the syntax around them. |
| `lead_chars` | Characters before the first top-level key. |
| `table_rows` | Not expressed in this kind; always 0. |
| `<outline>` | Top-level key names, anchored at their line in the rendered form. |

Structured content is rendered into the candidate block one field per line
rather than as the stored single line. The outline is measured on that
rendering, so its anchors name lines `read_proposal` returns.

Adding a kind means adding an analyzer and registering it. An analyzer emits
the attributes above rather than new ones: the evaluator is told in prose what
these names mean, so a metric no prompt explains is a metric no evaluator can
act on.

## Addressing the numbers

The metadata is only useful if a suspicion can be checked, so both
retrieval tools speak line numbers.

`read_proposal` takes `from_line` and `to_line` (1-indexed, inclusive).
Setting either returns the final solution as numbered lines instead of raw
text; `to_line` past the end clamps, `from_line` past the end reports the
total. Character `offset`/`limit` continue to page the `thought_process`
independently.

```
read_proposal(agent_id="Candidate_A", from_line=31, to_line=48)
→ <lines from="31" to="48" total="48">
    31| | model | latency |
    …
```

`search_deliberation` reports `lines` and, when keywords matched,
`matched_lines="2,9,17"` (first 20 hits) on each `<proposal>`. Its
`<content>` is still truncated at 2000 characters — the line numbers are
what make the remainder reachable, by feeding them to `read_proposal`.

## Why shape rather than size

A one-line answer followed by a well-structured annex is a better artifact
than the same character count of undifferentiated prose, and total length
cannot tell them apart. Splitting the measurement into time-to-answer
(`lead_chars`), unstructured volume (`prose_chars`) and navigability
(the structure counts) lets the axis reward the first shape and punish the
second at equal `chars`.

## Where the measurement goes afterwards

The same measurement is published with the score it earned. Each
`ProposalScoreEntry` in a round summary carries a `shape` object holding the
attributes above, beside the `conciseness` mean in its `category_breakdown`.

Both fields are optional, and an absent `conciseness` means no evaluator
scored the axis — not that they scored it neutral. The mean is taken over the
evaluators who scored it, so an abstention does not pull the published figure
toward zero.

Emitting both in one entry is what makes score and size correlatable per
proposal per round, rather than a question answerable only by re-running an
offline experiment.
