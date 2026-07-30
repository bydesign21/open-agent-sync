#!/bin/bash
# Capture every TUI view from the demo world and render each to SVG + PNG.
set -euo pipefail
cd "$(dirname "$0")"

AS_BIN="$HOME/Documents/Repos/agentsync/target/release/agentsync"
export AS_BIN
D=/tmp/agentsync-demo
OUT=shots
rm -rf "$OUT" && mkdir -p "$OUT"

R=${ROWS:-26}
C=${COLS:-112}

shot() {
  local name=$1 keys=$2 rows=${3:-$R} hold=${4:-0.5}
  bash setup_demo.sh >/dev/null            # fresh world for every shot
  HOME="$D" PATH="$D/bin:/usr/bin:/bin" HOLD="$hold" \
    CHILD_ARGS="--repo $D/repos/infra --repo $D/repos/webapp" \
    python3 capture.py "$rows" "$C" "$keys" "$D" "$OUT/$name.raw"
  python3 ansi2svg.py "$OUT/$name.raw" "$rows" "$C" "$OUT/$name.svg" "agentsync"
  magick -density 144 -background none "$OUT/$name.svg" "$OUT/$name.png"
  rm -f "$OUT/$name.raw"
}

# 1. the default review list: differences only
shot review ""

# 2. accepted rows, showing the marks and the count
shot review-accepted "Ajjjj "

# 3. in-sync rows revealed, and a per-host removal chosen
shot removal "vjjjjd"

# 4. the project picker
shot projects "p" 14

# 5. the plan gate
shot plan "Ajjj$(printf '\r')" 30

# 6. streaming progress mid-run
shot running "A$(printf '\r')yzzzz" 24 0.1

# 7. the result screen
shot result "A$(printf '\r')yzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz" 30 3

ls -1 "$OUT"
