#!/bin/bash
# Performance benchmarks for VocalScope on macOS.
#
#   scripts/bench.sh                 run everything, 5 launches per scenario
#   scripts/bench.sh --runs 10       more launches
#   scripts/bench.sh --save NAME     also write results to bench-results/NAME.txt
#
# Run it before and after a change and compare the two outputs. Budgets
# (see docs/PERFORMANCE.md): window visible well under half a second, and
# memory in normal use under 100-200 MB.
#
# What is measured
#   1. Core: decode + waveform speed and peak memory per file format.
#   2. App launch: time from process creation to the window appearing, to a
#      file's waveform being on screen and to its pitch being on screen
#      (first open, then cached).
#   3. App at rest: memory footprint and CPU with a file open, idle and
#      (silently, muted) playing.
#
# The app is pointed at a throwaway data folder, so your settings, recent
# files and waveform cache are never touched. Needs test audio from
# `uv run scripts/make_test_audio.py --long` and a built app
# (`apps/macos/build.sh`).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
RUNS=5
SAVE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --runs) RUNS="$2"; shift 2 ;;
    --save) SAVE="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

APP="$ROOT/apps/macos/build/VocalScope.app"
BIN="$APP/Contents/MacOS/VocalScope"
AUDIO="$ROOT/test-audio"
SHORT="$AUDIO/synthetic-vocal-4min.mp3"
LONG="$AUDIO/synthetic-vocal-60min.mp3"
[[ -x "$BIN" ]] || { echo "Build the app first: apps/macos/build.sh" >&2; exit 1; }
[[ -f "$SHORT" ]] || { echo "Create test audio first: uv run scripts/make_test_audio.py --long" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export VOCALSCOPE_DATA_ROOT="$WORK/home"

median() { sort -n | awk '{a[NR]=$1} END {if (NR==0) {print "n/a"} else if (NR%2) {print a[(NR+1)/2]} else {printf "%.1f\n", (a[NR/2]+a[NR/2+1])/2}}'; }
mb() { awk '{printf "%.1f", $1/1048576}'; }

report() {
echo "VocalScope benchmark — $(date '+%Y-%m-%d %H:%M')"
echo "Machine: $(sysctl -n machdep.cpu.brand_string), $(( $(sysctl -n hw.memsize) / 1073741824 )) GB RAM, macOS $(sw_vers -productVersion)"
echo "App: $(du -sh "$APP" | cut -f1) bundle, $(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")"
echo

echo "== 1. Core: decode + waveform =="
cargo build -q --release -p vocalscope-core --example bench_waveform
for file in "$AUDIO"/synthetic-vocal-4min.{wav,flac,mp3,m4a} "$LONG"; do
  [[ -f "$file" ]] || continue
  out="$( { /usr/bin/time -l target/release/examples/bench_waveform "$file"; } 2>&1 )"
  line="$(echo "$out" | head -1)"
  peak="$(echo "$out" | awk '/maximum resident set size/ {print $1}' | mb)"
  echo "  $line, peak memory ${peak} MB"
done
echo

# Launches the app in benchmark mode; prints
# "<window ms> <waveform ms> <pitch ms> <peak footprint bytes>".
launch() {
  local out
  out="$( { VOCALSCOPE_BENCH=1 VOCALSCOPE_BENCH_EXIT=1 /usr/bin/time -l "$BIN" "$@"; } 2>&1 )"
  local window waveform pitch peak
  window="$(echo "$out" | awk '$1=="bench" && $2=="window" {print $3}')"
  waveform="$(echo "$out" | awk '$1=="bench" && $2=="waveform" {print $3}')"
  pitch="$(echo "$out" | awk '$1=="bench" && $2=="analysis" {print $3}')"
  peak="$(echo "$out" | awk '/peak memory footprint/ {print $1}')"
  echo "${window:-0} ${waveform:-0} ${pitch:-0} ${peak:-0}"
}

scenario() {
  local label="$1"; shift
  local clear_cache="$1"; shift
  local windows=() waveforms=() pitches=() peaks=()
  for _ in $(seq 1 "$RUNS"); do
    [[ "$clear_cache" == yes ]] && rm -rf "$VOCALSCOPE_DATA_ROOT/cache"
    read -r w f a p <<<"$(launch "$@")"
    windows+=("$w"); waveforms+=("$f"); pitches+=("$a"); peaks+=("$p")
  done
  local w f a p
  w="$(printf '%s\n' "${windows[@]}" | median)"
  f="$(printf '%s\n' "${waveforms[@]}" | median)"
  a="$(printf '%s\n' "${pitches[@]}" | median)"
  p="$(printf '%s\n' "${peaks[@]}" | median | mb)"
  if [[ $# -eq 0 ]]; then
    printf '  %-24s window %6s ms                                          peak memory %6s MB\n' "$label" "$w" "$p"
  else
    printf '  %-24s window %6s ms   waveform %7s ms   pitch %7s ms   peak memory %6s MB\n' "$label" "$w" "$f" "$a" "$p"
  fi
}

echo "== 2. App launch (median of $RUNS; ms since process creation) =="
launch >/dev/null   # warm the disk cache so run 1 is not an outlier
scenario "no file" no
scenario "4 min MP3, first open" yes "$SHORT"
scenario "4 min MP3, cached" no "$SHORT"
if [[ -f "$LONG" ]]; then
  scenario "60 min MP3, first open" yes "$LONG"
  scenario "60 min MP3, cached" no "$LONG"
fi
echo

echo "== 3. App at rest (file open) =="
sample() {
  local label="$1" pid="$2"
  local footprint cpu
  footprint="$(footprint -p "$pid" 2>/dev/null | awk '/phys_footprint:/ {print $2, $3; exit}')"
  [[ -n "$footprint" ]] || footprint="$(vmmap --summary "$pid" 2>/dev/null | awk '/Physical footprint:/ {print $3; exit}')"
  # Second sample of top is a true interval measurement; the first is since launch.
  cpu="$(top -l 2 -s 3 -pid "$pid" -stats cpu 2>/dev/null | awk '/^[0-9. ]+$/ {v=$1} END {print v}')"
  printf '  %-34s memory footprint %-10s CPU %s%%\n' "$label" "$footprint" "${cpu:-n/a}"
}
for file in "$SHORT" "$LONG"; do
  [[ -f "$file" ]] || continue
  "$BIN" "$file" >/dev/null 2>&1 &
  pid=$!
  sleep 5
  sample "$(basename "$file" .mp3 | sed 's/synthetic-vocal-//'), idle" "$pid"
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null || true
done
}

if [[ -n "$SAVE" ]]; then
  mkdir -p bench-results
  report | tee "bench-results/$SAVE.txt"
  echo; echo "Saved to bench-results/$SAVE.txt"
else
  report
fi
