#!/usr/bin/env python3
"""
Minimal NSED exec agent — reference implementation.

This script reads a JSON envelope from stdin (written by the orchestrator),
dispatches by phase, and writes the response JSON to stdout using the
___NSED_START___/___NSED_END___ delimiter protocol.

Usage in agent.yml:
    providers:
      exec_local:
        type: exec
    agents:
      - name: PYTHON_AGENT
        provider_id: exec_local
        model_name: custom
        exec:
          command: ["python3", "examples/exec_agent.py"]

Protocol:
    stdin  <- {"phase": "propose"|"evaluate", "context": {...}}
    stdout -> ___NSED_START___
              <response JSON>
              ___NSED_END___
    stderr -> diagnostics/logs (ignored by the orchestrator)
"""

import json
import sys


def handle_propose(context: dict) -> dict:
    """Generate a proposal from the deliberation context."""
    task = context.get("task_description", "")
    round_num = context.get("round_number", 0)
    previous = context.get("previous_own_proposal")

    if previous:
        prev_content = previous.get("content", "")
        thought = f"Round {round_num}: refining previous proposal based on feedback."
        content = f"Refined: {prev_content} (updated in round {round_num})"
    else:
        thought = f"Round {round_num}: initial analysis of the task."
        content = f"My proposal for: {task}"

    return {
        "thought_process": thought,
        "content": content,
    }


def handle_evaluate(context: dict) -> dict:
    """Evaluate candidate proposals."""
    candidates = context.get("candidates", [])
    evaluations = []

    for candidate in candidates:
        cid = candidate.get("id", "unknown")
        proposal = candidate.get("proposal", {})
        content = proposal.get("content", "")

        # Simple heuristic: score based on content length (longer = more thorough)
        score = min(len(content) / 500.0, 1.0)

        evaluations.append({
            "target_id": cid,
            "score": round(score, 2),
            "justification": f"Evaluated proposal from {cid}: "
            f"{'thorough' if score > 0.5 else 'brief'} response.",
        })

    return {"evaluations": evaluations}


def main():
    raw = sys.stdin.read()
    if not raw.strip():
        print("Error: empty stdin", file=sys.stderr)
        sys.exit(1)

    try:
        envelope = json.loads(raw)
    except json.JSONDecodeError as e:
        print(f"Error: invalid JSON on stdin: {e}", file=sys.stderr)
        sys.exit(1)

    phase = envelope.get("phase", "")
    context = envelope.get("context", {})

    print(f"[exec_agent] phase={phase} round={context.get('round_number', '?')}", file=sys.stderr)

    if phase == "propose":
        result = handle_propose(context)
    elif phase == "evaluate":
        result = handle_evaluate(context)
    else:
        print(f"Error: unknown phase '{phase}'", file=sys.stderr)
        sys.exit(1)

    # Write response with delimiters (pollution-resistant protocol)
    print("___NSED_START___")
    print(json.dumps(result))
    print("___NSED_END___")


if __name__ == "__main__":
    main()
