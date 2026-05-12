#!/usr/bin/env python3
"""Extract workspace and per-package coverage from a Cobertura XML report.

Usage:
    python3 scripts/extract-coverage.py [coverage/cobertura.xml]

Outputs (when GITHUB_OUTPUT is set):
    percent=51.2
    color=orange
    packages_json={"nsed-orchestrator":"48.3","nsed-agent":"60.1",...}

Outputs (when run locally without GITHUB_OUTPUT):
    Prints human-readable summary to stdout.

Called from both ci.yml and coverage.yml to avoid duplication.
"""

import json
import os
import sys
import xml.etree.ElementTree as ET

DEFAULT_XML = "coverage/cobertura.xml"


def badge_color(percent: float) -> str:
    if percent >= 80:
        return "brightgreen"
    elif percent >= 60:
        return "yellow"
    elif percent >= 40:
        return "orange"
    else:
        return "red"


def extract(xml_path: str) -> dict:
    """Return {"percent": str, "color": str, "packages_json": str}."""

    if not os.path.isfile(xml_path):
        print(f"::warning::{xml_path} not found — defaulting to 0%", file=sys.stderr)
        return {"percent": "0.0", "color": "red", "packages_json": "{}"}

    tree = ET.parse(xml_path)
    root = tree.getroot()

    # ── Workspace-level coverage ──
    line_rate_str = root.get("line-rate", "0")
    try:
        line_rate = float(line_rate_str)
    except ValueError:
        line_rate = 0.0
    percent = round(line_rate * 100, 1)
    color = badge_color(percent)

    # ── Per-package coverage ──
    # Group <class> elements by crate directory and compute line hit ratios.
    crate_hits: dict[str, list[int]] = {}  # crate_name → [hits, total]

    for cls in root.iter("class"):
        fn = cls.get("filename", "")

        # Map filename to crate: crates/<name>/src/...
        if fn.startswith("crates/"):
            parts = fn.split("/")
            if len(parts) < 2 or not parts[1]:
                continue  # malformed path like bare "crates/" — skip
            crate = parts[1]
        else:
            crate = "nsed-orchestrator"  # root src/ belongs to the workspace crate

        if crate not in crate_hits:
            crate_hits[crate] = [0, 0]

        for line in cls.iter("line"):
            crate_hits[crate][1] += 1
            if int(line.get("hits", "0")) > 0:
                crate_hits[crate][0] += 1

    packages = {}
    for crate, (hits, total) in sorted(crate_hits.items()):
        pct = round(hits / total * 100, 1) if total > 0 else 0.0
        packages[crate] = str(pct)

    return {
        "percent": str(percent),
        "color": color,
        "packages_json": json.dumps(packages),
    }


def main() -> None:
    xml_path = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_XML
    result = extract(xml_path)

    github_output = os.environ.get("GITHUB_OUTPUT")

    if github_output:
        # Running in GitHub Actions — write to GITHUB_OUTPUT file
        with open(github_output, "a") as f:
            for key, value in result.items():
                f.write(f"{key}={value}\n")

    # Always print human-readable summary
    print(f"  Workspace coverage: {result['percent']}%  ({result['color']})")
    packages = json.loads(result["packages_json"])
    for crate, pct in sorted(packages.items()):
        print(f"  {crate}: {pct}%")


if __name__ == "__main__":
    main()
