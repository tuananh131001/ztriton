#!/usr/bin/env bash
# Measure ztriton's CPU and memory while it plays (or idles on Home).
#
#   scripts/bench.sh [binary] [label]      # default: target/release/ztriton
#   MODE=idle scripts/bench.sh ...          # Home screen, no playback
#   SECONDS_MEASURED=30 WARMUP=5 OUT=proof/perf scripts/bench.sh ...
#
# Playback uses `--autoplay first` with the Keychain session, so the app must
# already be signed in. CPU % is process CPU time over wall time (100 % = one
# core), measured over the window only, not startup. Memory is the physical
# footprint (what Activity Monitor shows as "Memory") from vmmap.
set -euo pipefail

BIN=${1:-target/release/ztriton}
LABEL=${2:-$(basename "$BIN")}
MODE=${MODE:-play}
SECONDS_MEASURED=${SECONDS_MEASURED:-30}
WARMUP=${WARMUP:-5}
OUT=${OUT:-proof/perf}
mkdir -p "$OUT"
LOG="$OUT/$LABEL-$MODE.log"
TOP="$OUT/$LABEL-$MODE.top"

cpu_seconds() { # ps "time" is [H:]M:SS.ss
    ps -o time= -p "$1" | awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; print s }'
}
footprint_mb() { # $2: "Physical footprint:" or "Physical footprint (peak):"
    vmmap -summary "$1" 2>/dev/null | awk -v key="$2" 'index($0, key) == 1 {
        v = $NF; unit = substr(v, length(v)); n = substr(v, 1, length(v) - 1)
        if (unit == "G") n *= 1024; else if (unit == "K") n /= 1024; else if (unit == "B") n /= 1048576
        printf "%.1f", n }'
}

# The Keychain grants access per Team ID, so signing every build with the same
# Apple Development identity keeps one "Always Allow" valid across rebuilds
# (an ad-hoc signature changes with each build and prompts for the password).
identity="${ZTRITON_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk -F'"' '/"Apple Development: /{print $2; exit}')}"
if [ -n "$identity" ]; then
    codesign --force --sign "$identity" --identifier dev.ztriton.player "$BIN"
fi

args=()
[ "$MODE" = play ] && args=(--autoplay first)
ZTRITON_MUTE=1 RUST_LOG="info,ztriton::stats=debug" "$BIN" ${args[@]+"${args[@]}"} >"$LOG" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true' EXIT

if [ "$MODE" = play ]; then
    for _ in $(seq 1 120); do
        grep -q "playback-restart" "$LOG" && break
        sleep 0.5
    done
    grep -q "playback-restart" "$LOG" || { echo "playback never started, see $LOG" >&2; exit 1; }
else
    sleep 10
fi
sleep "$WARMUP"

top -l $((SECONDS_MEASURED + 1)) -s 1 -pid "$PID" -stats pid,cpu,mem,power >"$TOP" &
TOP_PID=$!
start_cpu=$(cpu_seconds "$PID")
start_wall=$(python3 -c 'import time; print(time.time())')
start_line=$(wc -l <"$LOG")
sleep "$SECONDS_MEASURED"
end_cpu=$(cpu_seconds "$PID")
end_wall=$(python3 -c 'import time; print(time.time())')
footprint=$(footprint_mb "$PID" "Physical footprint:")
peak=$(footprint_mb "$PID" "Physical footprint (peak):")
rss_mb=$(ps -o rss= -p "$PID" | awk '{ printf "%.1f", $1 / 1024 }')
wait "$TOP_PID" || true

# First top sample has no CPU delta; average the rest.
power=$(awk -v pid="$PID" '$1 == pid { n++; if (n > 1) { s += $4; c++ } } END { if (c) printf "%.1f", s / c; else print "n/a" }' "$TOP")

stats=$(tail -n +"$((start_line + 1))" "$LOG" | grep "ztriton::stats" || true)
fps="n/a"; drops="n/a"
if [ -n "$stats" ]; then
    read -r fps drops < <(printf '%s\n' "$stats" | awk '
        { for (i = 1; i <= NF; i++) { split($i, kv, "="); v[kv[1]] = kv[2] } }
        NR == 1 { t0 = v["time-pos"]; p0 = v["presented"]; d0 = v["vo_drops"] + v["decoder_drops"] }
        { t1 = v["time-pos"]; p1 = v["presented"]; d1 = v["vo_drops"] + v["decoder_drops"] }
        END { dt = t1 - t0; if (dt > 0) printf "%.2f %d\n", (p1 - p0) / dt, d1 - d0; else print "n/a n/a" }')
fi

cpu=$(awk -v a="$start_cpu" -v b="$end_cpu" -v s="$start_wall" -v e="$end_wall" 'BEGIN { printf "%.1f", (b - a) / (e - s) * 100 }')
line="label=$LABEL mode=$MODE cpu_pct=$cpu footprint_mb=$footprint footprint_peak_mb=$peak rss_mb=$rss_mb energy_impact=$power presented_fps_per_video_sec=$fps dropped_frames=$drops window_s=$SECONDS_MEASURED"
echo "$line" | tee -a "$OUT/results.txt"
