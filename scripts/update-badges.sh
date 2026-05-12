#!/bin/bash
# Requires: python3 (for per-package parsing), bc (for color thresholds)
# Works with Bash 3.2+ (no associative arrays).
set -e

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}📊 Updating README coverage badges from CI data...${NC}"

# ── helpers ──────────────────────────────────────────────────────────────────

coverage_color() {
    local percent=$1
    if (( $(echo "$percent >= 80" | bc -l 2>/dev/null || echo "0") )); then
        echo "brightgreen"
    elif (( $(echo "$percent >= 60" | bc -l 2>/dev/null || echo "0") )); then
        echo "yellow"
    elif (( $(echo "$percent >= 40" | bc -l 2>/dev/null || echo "0") )); then
        echo "orange"
    else
        echo "red"
    fi
}

# Replace a shields.io coverage badge in a file.
# Usage: update_badge <file> <percent> <color>
update_badge() {
    local file=$1 percent=$2 color=$3
    if [[ "$OSTYPE" == "darwin"* ]]; then
        sed -i '' "s/coverage-[0-9.]*%25-[a-z]*/coverage-${percent}%25-${color}/g" "$file"
    else
        sed -i "s/coverage-[0-9.]*%25-[a-z]*/coverage-${percent}%25-${color}/g" "$file"
    fi
}

# ── fetch CI data ────────────────────────────────────────────────────────────

echo "Fetching CI badge data..."
git fetch origin gh-pages 2>/dev/null || echo "  (gh-pages not found — using local data)"

# Try unified badges.json first, fall back to legacy coverage.json
CI_DATA=$(git show origin/gh-pages:badges.json 2>/dev/null || echo "")
if [ -z "$CI_DATA" ]; then
    CI_DATA=$(git show origin/gh-pages:coverage.json 2>/dev/null || echo "")
fi

# ── workspace coverage ───────────────────────────────────────────────────────

COVERAGE=""
if [ -n "$CI_DATA" ]; then
    # badges.json format: {"coverage":"51.2", ...}
    COVERAGE=$(echo "$CI_DATA" | grep -oE '"coverage":"[0-9.]+"' | grep -oE '[0-9.]+' || echo "")
    # legacy coverage.json format: {"message":"51.2%", ...}
    if [ -z "$COVERAGE" ]; then
        COVERAGE=$(echo "$CI_DATA" | grep -oE '"message":"[0-9.]+%"' | grep -oE '[0-9.]+' || echo "")
    fi
fi

# Fall back to local cobertura.xml
if [ -z "$COVERAGE" ] && [ -f "coverage/cobertura.xml" ]; then
    LINE_RATE=$(grep -oE 'line-rate="[0-9]+(\.[0-9]+)?"' coverage/cobertura.xml | head -1 | grep -oE '[0-9]+(\.[0-9]+)?' || echo "")
    if [ -n "$LINE_RATE" ]; then
        COVERAGE=$(echo "$LINE_RATE * 100" | bc -l | awk '{printf "%.1f", $0}')
    fi
fi
COVERAGE="${COVERAGE:-0.0}"
COVERAGE_COLOR=$(coverage_color "$COVERAGE")

echo -e "   Workspace coverage: ${COVERAGE}%  (${COVERAGE_COLOR})"

# ── update README badges ─────────────────────────────────────────────────────

echo "Updating badges..."

# Workspace README
cp README.md README.md.bak
update_badge README.md "$COVERAGE" "$COVERAGE_COLOR"

if ! cmp -s README.md README.md.bak; then
    echo -e "  ${GREEN}✓${NC} README.md  →  ${COVERAGE}%"
else
    echo -e "  ℹ️  README.md — already up to date"
fi
rm -f README.md.bak

# ── per-package READMEs (uses python3 to parse JSON, avoids Bash 4+ requirement) ──

if [ -n "$CI_DATA" ]; then
    echo "$CI_DATA" | python3 -c "
import json, sys, subprocess, os

data = json.load(sys.stdin)
packages = data.get('packages', {})
if not packages:
    sys.exit(0)

def badge_color(pct):
    p = float(pct)
    if p >= 80: return 'brightgreen'
    if p >= 60: return 'yellow'
    if p >= 40: return 'orange'
    return 'red'

is_mac = sys.platform == 'darwin'

for crate_dir in sorted(os.listdir('crates')):
    readme = os.path.join('crates', crate_dir, 'README.md')
    if not os.path.isfile(readme):
        continue
    pct = packages.get(crate_dir)
    if pct is None:
        continue
    color = badge_color(pct)

    with open(readme) as f:
        original = f.read()

    import re
    updated = re.sub(
        r'coverage-[0-9.]*%25-[a-z]*',
        f'coverage-{pct}%25-{color}',
        original,
    )
    if updated != original:
        with open(readme, 'w') as f:
            f.write(updated)
        print(f'  ✓ {readme}  →  {pct}%')
" 2>/dev/null || true
fi

echo ""
echo -e "${YELLOW}💡 Coverage data sourced from CI (gh-pages branch).${NC}"
echo -e "${GREEN}🚀 Ready for commit! Your badges are now current.${NC}"
