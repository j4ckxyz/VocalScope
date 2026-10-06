#!/bin/bash
# Takes the macOS screenshots of the analysis used in the README.
#
#   scripts/macos-screenshots.sh           writes docs/images/macos-main.png
#                                          and docs/images/macos-compare.png
#   scripts/macos-screenshots.sh OUT_DIR   writes them somewhere else
#
# The app draws its own window into each picture (see `Snapshot` in
# VocalScopeApp.swift), so no screen-recording permission is needed and the
# pictures are the same whatever else is on screen. It runs against a
# throwaway data folder, so your settings and recent files are untouched.
# Needs a built app (`apps/macos/build.sh`) and uv for the test audio.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
OUT="${1:-docs/images}"
APP="apps/macos/build/VocalScope.app/Contents/MacOS/VocalScope"
[[ -x "$APP" ]] || { echo "Build the app first: apps/macos/build.sh" >&2; exit 1; }

NATURAL="$ROOT/test-audio/synthetic-vocal-natural-1min.wav"
CORRECTED="$ROOT/test-audio/synthetic-vocal-corrected-1min.wav"
[[ -f "$NATURAL" && -f "$CORRECTED" ]] || uv run scripts/make_test_audio.py

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
DATA="$(mktemp -d)"
trap 'rm -rf "$DATA"' EXIT
export VOCALSCOPE_DATA_ROOT="$DATA"

# A recording with its pitch and the Analysis page.
VOCALSCOPE_SNAPSHOT="$OUT/macos-main.png" VOCALSCOPE_SNAPSHOT_VIEW=3,9 \
  "$APP" "$NATURAL" -inspectorShown YES -inspectorTab analysis -pitchShown YES >/dev/null
echo "wrote $OUT/macos-main.png"

# Two versions compared.
VOCALSCOPE_SNAPSHOT="$OUT/macos-compare.png" VOCALSCOPE_SNAPSHOT_VIEW=3,9 \
  VOCALSCOPE_SNAPSHOT_COMPARE="$CORRECTED" \
  "$APP" "$NATURAL" -inspectorShown YES -inspectorTab compare -pitchShown YES >/dev/null
echo "wrote $OUT/macos-compare.png"
