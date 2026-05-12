#!/usr/bin/env python3
"""
Supply-chain freshness check.

Reads Cargo.lock, queries crates.io for the publish date of every
registry dependency that is new (or newly versioned) on this branch,
and exits non-zero if any such version was published fewer than
MIN_AGE_DAYS days ago.

## Scoping to the diff

When ``--target REF`` is given, the script diffs the current lockfile
against ``git show REF:Cargo.lock`` and checks only the packages whose
``(name, version)`` is new in HEAD. Unchanged entries are skipped — if
a crate was already in the target at the same version, it was already
vetted on a previous run and re-checking it just burns rate-limit
budget. Without ``--target`` the script falls back to a full scan, so
nothing is missed on a pristine checkout or when the target ref is
unreachable.

## Fast-tracking security patches

Security-advisory updates may bypass the cooldown via the
``scripts/supply-chain-fasttrack.toml`` allowlist. Each fast-track
entry must include a reason (typically a RUSTSEC advisory ID) and an
expiry date; entries whose expiry has passed are warned about and
skipped so stale bypasses get flagged for cleanup. Entries that no
longer match any lockfile package are also warned about so the
allowlist stays in sync with the real dep graph.

Requires: Python 3.11+ (tomllib is stdlib) or the 'tomli' backport
for 3.9/3.10.

Usage:
    python3 scripts/check-dep-age.py [--min-age-days N] [--lockfile PATH]
                                     [--fasttrack PATH] [--target REF]
"""

import argparse
import sys
import time
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib  # type: ignore[no-redef]
    except ImportError:
        print(
            "error: tomllib requires Python 3.11+ or the 'tomli' backport "
            "(pip install tomli)",
            file=sys.stderr,
        )
        sys.exit(1)
import io
import subprocess
import urllib.request
import urllib.error
import json
from datetime import datetime, timezone, date
from pathlib import Path


CRATES_IO_API = "https://crates.io/api/v1/crates/{name}/{version}"
RATE_LIMIT_DELAY = 1.0  # seconds between requests — crates.io policy: ≤1 req/s
USER_AGENT = "nsed-supply-chain-check (github.com/peeramid-labs/nsed)"
DEFAULT_FASTTRACK = Path("scripts/supply-chain-fasttrack.toml")


def parse_lockfile_bytes(raw: bytes, *, source_label: str) -> list[tuple[str, str]]:
    """
    Parse lockfile bytes into a unique, order-preserving list of
    ``(name, version)`` tuples for every crates.io registry package.

    ``source_label`` is used in warning output so the operator can tell
    whether the parse failure came from the working-copy file or from
    a ``git show`` of the target ref.
    """
    lock = tomllib.load(io.BytesIO(raw))

    seen: set[tuple[str, str]] = set()
    packages: list[tuple[str, str]] = []
    for pkg in lock.get("package", []):
        source = pkg.get("source", "")
        if source.startswith("registry+"):
            name = pkg.get("name")
            version = pkg.get("version")
            if not name or not version:
                print(
                    f"  ⚠ Skipping malformed {source_label} entry "
                    f"(missing name/version): {pkg}",
                    file=sys.stderr,
                )
                continue
            key = (name, version)
            if key not in seen:
                seen.add(key)
                packages.append(key)
    return packages


def parse_lockfile(lockfile_path: Path) -> list[tuple[str, str]]:
    """Return unique [(name, version), ...] for all crates.io registry packages."""
    with open(lockfile_path, "rb") as f:
        return parse_lockfile_bytes(f.read(), source_label=str(lockfile_path))


def load_target_packages(
    target_ref: str, lockfile_path: Path
) -> tuple[set[tuple[str, str]] | None, str | None]:
    """
    Resolve the lockfile at ``target_ref:<lockfile_path>`` and return
    its ``(name, version)`` set.

    Returns a tuple of:
      - set of ``(name, version)`` tuples, or ``None`` if the target
        could not be loaded for any reason
      - human-readable error/note explaining the fallback, or ``None``
        when the load succeeded

    Callers treat ``None`` as "fall back to full scan". We intentionally
    never hard-fail here — on a fresh clone or for a ref that doesn't
    exist yet, a full scan is the correct, safe behavior.
    """
    git_path = f"{target_ref}:{lockfile_path.as_posix()}"
    try:
        result = subprocess.run(
            ["git", "show", git_path],
            check=True,
            capture_output=True,
        )
    except FileNotFoundError:
        return None, "`git` binary not found on PATH"
    except subprocess.CalledProcessError as exc:
        stderr = exc.stderr.decode("utf-8", errors="replace").strip()
        return None, f"`git show {git_path}` failed: {stderr or exc}"

    try:
        pkgs = parse_lockfile_bytes(result.stdout, source_label=f"{target_ref} lockfile")
    except tomllib.TOMLDecodeError as exc:
        return None, f"could not parse {target_ref} lockfile: {exc}"

    return set(pkgs), None


def fetch_publish_date(name: str, version: str) -> datetime | None:
    """Return the UTC publish datetime for a given crate version, or None on error."""
    url = CRATES_IO_API.format(name=name, version=version)
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())
        created = data["version"]["created_at"]
        return datetime.fromisoformat(created.replace("Z", "+00:00"))
    except (urllib.error.HTTPError, urllib.error.URLError, KeyError, ValueError) as exc:
        print(f"  ⚠ Could not fetch {name}@{version}: {exc}", file=sys.stderr)
        return None


def load_fasttrack(
    path: Path,
) -> tuple[dict[tuple[str, str], str], list[str]]:
    """
    Load the supply-chain fast-track allowlist from a TOML file.

    Returns a tuple of:
      - mapping of ``(name, version)`` → reason for entries that are
        still within their expiry window and should bypass the age check.
      - list of warning messages for stale, malformed, or unparseable
        entries. These are emitted to stderr by the caller but do not
        fail the check; the caller is responsible for surfacing them.

    A missing file is **not** an error — it returns empty results so
    the script works without any allowlist in place.
    """
    if not path.exists():
        return {}, []

    try:
        with open(path, "rb") as f:
            data = tomllib.load(f)
    except tomllib.TOMLDecodeError as exc:
        return {}, [f"could not parse {path}: {exc}"]

    warnings: list[str] = []
    allowlist: dict[tuple[str, str], str] = {}
    today = datetime.now(timezone.utc).date()

    for i, entry in enumerate(data.get("fasttrack", [])):
        name = entry.get("name")
        version = entry.get("version")
        reason = (entry.get("reason") or "").strip()
        expires = entry.get("expires")
        label = f"fasttrack[{i}] ({name}@{version})" if name and version else f"fasttrack[{i}]"

        if not name or not version:
            warnings.append(f"{label}: missing `name` or `version`, ignoring")
            continue
        if not reason:
            warnings.append(f"{label}: missing `reason`, ignoring")
            continue
        if expires is None:
            warnings.append(f"{label}: missing `expires` date, ignoring")
            continue
        if not isinstance(expires, date):
            warnings.append(
                f"{label}: `expires` must be a TOML local-date (YYYY-MM-DD), "
                f"got {type(expires).__name__}, ignoring"
            )
            continue
        if expires < today:
            warnings.append(
                f"{label}: expired on {expires:%Y-%m-%d} — please prune the "
                f"entry (the version has naturally aged past the cooldown)"
            )
            continue

        allowlist[(name, version)] = reason

    return allowlist, warnings


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--min-age-days", type=int, default=7)
    parser.add_argument("--lockfile", type=Path, default=Path("Cargo.lock"))
    parser.add_argument(
        "--fasttrack",
        type=Path,
        default=DEFAULT_FASTTRACK,
        help=(
            "Path to the supply-chain fast-track allowlist (TOML). "
            "Missing file is OK — no bypasses apply. "
            f"Default: {DEFAULT_FASTTRACK}"
        ),
    )
    parser.add_argument(
        "--target",
        type=str,
        default=None,
        help=(
            "Git ref to diff Cargo.lock against. When set, only "
            "packages whose (name, version) is new on this branch are "
            "checked — unchanged entries were already vetted on a "
            "previous run. Unreachable refs fall back to a full scan "
            "with a warning. Typical CI value: `origin/main` or "
            "`origin/${{ github.base_ref }}`."
        ),
    )
    parser.add_argument(
        "--allow-missing-metadata",
        action="store_true",
        help="Skip packages whose metadata cannot be fetched instead of failing CI.",
    )
    args = parser.parse_args()

    if not args.lockfile.exists():
        print(f"❌ Lockfile not found: {args.lockfile}", file=sys.stderr)
        return 1

    fasttrack, fasttrack_warnings = load_fasttrack(args.fasttrack)
    for msg in fasttrack_warnings:
        print(f"  ⚠ fasttrack: {msg}", file=sys.stderr)

    all_packages = parse_lockfile(args.lockfile)

    # Scope to packages new in this branch if a target ref was given.
    # We preserve the order from the working-copy lockfile so the log
    # output matches what developers see when they read Cargo.lock.
    if args.target:
        target_set, target_error = load_target_packages(args.target, args.lockfile)
        if target_set is None:
            print(
                f"  ⚠ diff scope: {target_error} — falling back to full scan",
                file=sys.stderr,
            )
            packages = all_packages
            scope_note = "full scan (target unreachable)"
        else:
            packages = [pkg for pkg in all_packages if pkg not in target_set]
            scope_note = (
                f"diff vs {args.target}: "
                f"{len(packages)} new / {len(all_packages) - len(packages)} unchanged"
            )
    else:
        packages = all_packages
        scope_note = "full scan"

    print(
        f"Checking {len(packages)} crates.io packages "
        f"(min age: {args.min_age_days}d, fast-tracked: {len(fasttrack)}, "
        f"scope: {scope_note})…"
    )

    now = datetime.now(timezone.utc)
    failures: list[tuple[str, str, float]] = []
    fetch_errors: list[tuple[str, str]] = []
    used_fasttracks: set[tuple[str, str]] = set()

    # Don't rate-limit between fast-tracked packages (we never hit
    # crates.io for them). Track the index of the last package that
    # actually made a network call so we sleep only where needed.
    last_network_index = -1
    for idx, (name, version) in enumerate(packages):
        if (name, version) not in fasttrack:
            last_network_index = idx

    for i, (name, version) in enumerate(packages):
        if (name, version) in fasttrack:
            used_fasttracks.add((name, version))
            reason = fasttrack[(name, version)]
            # Truncate the reason inline for readability; the full
            # text lives in the fast-track TOML file.
            short_reason = reason if len(reason) <= 80 else reason[:77] + "…"
            print(f"  ⚡ {name}@{version}  (fast-tracked: {short_reason})")
            continue

        published = fetch_publish_date(name, version)
        if published is None:
            fetch_errors.append((name, version))
            # Rate-limit even on failure — the request was still made
            if i < last_network_index:
                time.sleep(RATE_LIMIT_DELAY)
            continue

        age_days = (now - published).total_seconds() / 86400
        status = "✅" if age_days >= args.min_age_days else "🚨"
        print(f"  {status} {name}@{version}  ({age_days:.1f}d old)")

        if age_days < args.min_age_days:
            failures.append((name, version, age_days))

        # Rate-limit every network request except the last
        if i < last_network_index:
            time.sleep(RATE_LIMIT_DELAY)

    exit_code = 0

    # Fast-track hygiene report. Splits unused entries into two buckets:
    #   - "stale"      → version is not in the lockfile at all
    #   - "redundant"  → version IS in the lockfile but was filtered out
    #                    by the diff scope (target already has it, so the
    #                    fast-track isn't protecting anything anymore)
    all_package_set = set(all_packages)
    stale_fasttracks = sorted(set(fasttrack.keys()) - all_package_set)
    redundant_fasttracks = sorted(
        (set(fasttrack.keys()) & all_package_set) - used_fasttracks
    )
    if stale_fasttracks:
        print(
            "\n⚠ Fast-track entries whose version is no longer in "
            "Cargo.lock (please prune):",
            file=sys.stderr,
        )
        for name, version in stale_fasttracks:
            print(f"   • {name}@{version}", file=sys.stderr)
    if redundant_fasttracks:
        print(
            "\n⚠ Fast-track entries that were filtered out by the diff "
            "scope — the target branch already ships this version, so "
            "the allowlist entry is no longer needed (please prune):",
            file=sys.stderr,
        )
        for name, version in redundant_fasttracks:
            print(f"   • {name}@{version}", file=sys.stderr)

    if fetch_errors:
        if args.allow_missing_metadata:
            print(
                f"\n⚠ Skipped {len(fetch_errors)} package(s) with unresolvable metadata "
                f"(--allow-missing-metadata):",
                file=sys.stderr,
            )
            for name, version in fetch_errors:
                print(f"   • {name}@{version}", file=sys.stderr)
        else:
            print(
                f"\n❌ {len(fetch_errors)} package(s) had unresolvable metadata "
                f"(use --allow-missing-metadata to skip instead of failing):",
                file=sys.stderr,
            )
            for name, version in fetch_errors:
                print(f"   • {name}@{version}", file=sys.stderr)
            exit_code = 1

    if failures:
        print(
            f"\n❌ {len(failures)} package(s) published less than {args.min_age_days} days ago:",
            file=sys.stderr,
        )
        for name, version, age_days in failures:
            print(f"   • {name}@{version} ({age_days:.1f}d old)", file=sys.stderr)
        print(
            "\nThis check guards against supply-chain attacks. "
            "If this is a security update, re-run CI after the cooldown period.",
            file=sys.stderr,
        )
        exit_code = 1

    if exit_code == 0:
        print(f"\n✅ All packages are at least {args.min_age_days} days old.")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
