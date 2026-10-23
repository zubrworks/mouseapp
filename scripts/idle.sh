#!/usr/bin/env bash
# Measures idle cost of mouse-agent (and bluetoothd for comparison) over N
# seconds (default 60): CPU% from the `ps -o time` difference, idle wakeups
# (IDLEW) and POWER from `top -l 2 -s N`. Leave the mouse untouched meanwhile.
set -euo pipefail

SECS="${1:-60}"
if ! [[ "$SECS" =~ ^[1-9][0-9]*$ ]]; then
  echo "usage: idle.sh [seconds=60]" >&2
  exit 1
fi

AGENT="$(pgrep -x mouse-agent | head -1 || true)"
BT="$(pgrep -x bluetoothd | head -1 || true)"
PIDS=()
NAMES=()
if [[ -n "$AGENT" ]]; then
  PIDS+=("$AGENT")
  NAMES+=(mouse-agent)
else
  echo "idle.sh: mouse-agent is not running, measuring bluetoothd only" >&2
fi
if [[ -n "$BT" ]]; then
  PIDS+=("$BT")
  NAMES+=(bluetoothd)
fi
if (( ${#PIDS[@]} == 0 )); then
  echo "idle.sh: neither mouse-agent nor bluetoothd is running" >&2
  exit 1
fi

# CPU time of a process in seconds; ps prints [[H:]M]M:SS.ss.
cpu_secs() {
  ps -o time= -p "$1" | awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; print s }'
}

TOP_ARGS=()
for i in "${!PIDS[@]}"; do
  TOP_ARGS+=(-pid "${PIDS[$i]}")
  START_CPU[$i]="$(cpu_secs "${PIDS[$i]}")"
done

echo "Measuring for ${SECS} s, do not touch the mouse..."
START="$(date +%s)"
TOP_OUT="$(top -l 2 -s "$SECS" "${TOP_ARGS[@]}" -stats pid,idlew,power)"
WALL=$(( $(date +%s) - START ))
(( WALL > 0 )) || WALL=1

echo "-- $(date '+%Y-%m-%d %H:%M'), ${WALL} s"
printf '%-12s %8s %10s %8s\n' process 'CPU%' 'IDLEW/s' POWER
for i in "${!PIDS[@]}"; do
  pid="${PIDS[$i]}"
  end_cpu="$(cpu_secs "$pid")"
  # IDLEW is a running total: first and second sample give the difference.
  # POWER from the second sample (the first has no interval).
  read -r w1 w2 power < <(echo "$TOP_OUT" | awk -v p="$pid" '
    $1 == p { gsub(/[^0-9.]/, "", $2); w[++n] = $2; pw = $3 }
    END { print w[1], w[2], pw }')
  awk -v n="${NAMES[$i]}" -v c0="${START_CPU[$i]}" -v c1="$end_cpu" \
      -v w1="$w1" -v w2="$w2" -v pw="$power" -v t="$WALL" -v s="$SECS" \
      'BEGIN { printf "%-12s %8.3f %10.2f %8s\n", n, (c1 - c0) / t * 100, (w2 - w1) / s, pw }'
done
