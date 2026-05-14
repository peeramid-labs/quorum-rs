#!/usr/bin/env python3
"""
Minimal NSED MCP agent — reference implementation (hybrid protocol).

This script demonstrates the hybrid stdin-push + MCP tool-calling protocol:
  1. Read the initial AgentContext JSON line from stdin (pushed by NSED)
  2. Connect as an MCP client over the same stdin/stdout
  3. Optionally call research tools (nsed_read_proposal, nsed_search, etc.)
  4. Call nsed_propose or nsed_evaluate to submit a result

The context is available immediately from stdin — no need to call
nsed_get_context (though it's available as a refresh/fallback).

Usage in agent.yml:
    providers:
      mcp_local:
        type: mcp
    agents:
      - name: PYTHON_MCP_AGENT
        provider_id: mcp_local
        model_name: custom
        mcp:
          command: ["python3", "examples/mcp_agent.py"]

Requirements:
    pip install mcp
"""

import asyncio
import json
import sys

import anyio
from mcp.client.session import ClientSession
from mcp.shared.message import SessionMessage
from mcp.types import JSONRPCMessage


async def main():
    loop = asyncio.get_event_loop()

    # ── Step 1: Read the initial context JSON line pushed by NSED ──
    first_line = await loop.run_in_executor(None, sys.stdin.readline)
    if not first_line.strip():
        print("[mcp_agent] ERROR: no initial context line received", file=sys.stderr)
        sys.exit(1)

    try:
        envelope = json.loads(first_line)
    except json.JSONDecodeError as e:
        print(f"[mcp_agent] ERROR: failed to parse context envelope: {e}", file=sys.stderr)
        sys.exit(1)

    context = envelope.get("context", {})
    phase = envelope.get("phase", "")
    task = context.get("task_description", "")
    round_num = context.get("round_number", 1)
    print(f"[mcp_agent] phase={phase} round={round_num} task={task[:80]}", file=sys.stderr)

    # ── Step 2: Set up MCP client over the same stdin/stdout ──
    # After the initial JSON line, stdin/stdout carry MCP JSON-RPC messages.
    server_to_client_send, server_to_client_recv = anyio.create_memory_object_stream[
        SessionMessage | Exception
    ](0)
    client_to_server_send, client_to_server_recv = anyio.create_memory_object_stream[
        SessionMessage
    ](0)

    async def read_stdin():
        """Read JSON-RPC messages from stdin and forward to ClientSession."""
        buffer = ""
        try:
            async with server_to_client_send:
                while True:
                    line = await loop.run_in_executor(None, sys.stdin.readline)
                    if not line:
                        break
                    buffer += line
                    lines = buffer.split("\n")
                    buffer = lines.pop()  # keep incomplete last line
                    for raw in lines:
                        raw = raw.strip()
                        if not raw:
                            continue
                        try:
                            msg = JSONRPCMessage.model_validate_json(raw)
                            await server_to_client_send.send(SessionMessage(msg))
                        except Exception as e:
                            print(
                                f"[mcp_agent] parse error: {e} on: {raw[:100]}",
                                file=sys.stderr,
                            )
        except anyio.ClosedResourceError:
            pass

    async def write_stdout():
        """Read messages from ClientSession and write to stdout."""
        try:
            async with client_to_server_recv:
                async for msg in client_to_server_recv:
                    json_str = msg.message.model_dump_json(by_alias=True, exclude_none=True)
                    sys.stdout.write(json_str + "\n")
                    sys.stdout.flush()
        except anyio.ClosedResourceError:
            pass

    # ── Step 3: Run MCP session and submit result ──
    async with anyio.create_task_group() as tg:
        tg.start_soon(read_stdin)
        tg.start_soon(write_stdout)

        async with ClientSession(
            server_to_client_recv, client_to_server_send
        ) as session:
            await session.initialize()

            tools = await session.list_tools()
            tool_names = [t.name for t in tools.tools]
            print(f"[mcp_agent] MCP tools: {tool_names}", file=sys.stderr)

            if phase == "propose":
                previous = context.get("previous_own_proposal")
                if previous:
                    content = f"Refined proposal for round {round_num}: {task}"
                    thought = f"Refining based on previous feedback in round {round_num}"
                else:
                    content = f"Initial proposal for: {task}"
                    thought = f"Round {round_num}: analyzing task and forming initial proposal"

                await session.call_tool(
                    "nsed_propose",
                    {"thought_process": thought, "content": content},
                )
                print("[mcp_agent] Proposal submitted via MCP", file=sys.stderr)

            elif phase == "evaluate":
                candidates = context.get("candidates", [])
                evaluations = []
                for c in candidates:
                    cid = c.get("id", "unknown")
                    proposal = c.get("proposal", {})
                    content_text = proposal.get("content", "")
                    score = min(len(content_text) / 500.0, 1.0)
                    evaluations.append({
                        "target_id": cid,
                        "score": round(score, 2),
                        "justification": f"Evaluated proposal from {cid}",
                    })

                await session.call_tool(
                    "nsed_evaluate", {"evaluations": evaluations}
                )
                print("[mcp_agent] Evaluations submitted via MCP", file=sys.stderr)
            else:
                print(f"[mcp_agent] Unknown phase: {phase}", file=sys.stderr)

        tg.cancel_scope.cancel()


if __name__ == "__main__":
    asyncio.run(main())
