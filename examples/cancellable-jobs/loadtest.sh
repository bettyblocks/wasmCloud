#!/usr/bin/env bash
# Load test for the cancellable-jobs demo.
#
# Creates N job groups (each spawning 10 concurrent counter calls) and
# holds one live SSE connection per group, then reports per-connection
# event counts. By default every group is cancelled on exit so repeated
# runs don't accumulate running counters.
#
# Usage: ./loadtest.sh [-n connections] [-d duration_s] [-m sleep|burn] [-u url] [-k]
#   -n  number of concurrent groups + SSE connections   (default 5)
#   -d  watch duration in seconds before teardown        (default 20)
#   -m  counter mode: sleep | burn                       (default sleep)
#   -u  base URL of the wash dev HTTP server             (default http://127.0.0.1:8000)
#   -k  keep groups running after the test (skip cancel)
#
# Interpreting results (sleep mode): each group's 10 counters tick once
# per second, so a healthy connection sees ~10 count events/second and
# distinct_counters=10. Burn-mode counters emit no counts (pure CPU) —
# expect 0 events and mind your CPU with large -n.

set -euo pipefail

CONNS=5
DURATION=20
MODE=sleep
URL=http://127.0.0.1:8000
CANCEL=1

while getopts "n:d:m:u:k" opt; do
    case $opt in
        n) CONNS=$OPTARG ;;
        d) DURATION=$OPTARG ;;
        m) MODE=$OPTARG ;;
        u) URL=$OPTARG ;;
        k) CANCEL=0 ;;
        *) echo "usage: $0 [-n connections] [-d duration_s] [-m sleep|burn] [-u url] [-k]" >&2; exit 2 ;;
    esac
done

case $MODE in
    sleep) CREATE_URL="$URL/create" ;;
    burn)  CREATE_URL="$URL/create?mode=burn" ;;
    *) echo "invalid mode '$MODE' (sleep|burn)" >&2; exit 2 ;;
esac

WORKDIR=$(mktemp -d /tmp/cancellable-jobs-load.XXXXXX)
declare -a IDS=() PIDS=()

cleanup() {
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    if [[ $CANCEL -eq 1 && ${#IDS[@]} -gt 0 ]]; then
        echo "cancelling ${#IDS[@]} groups ..."
        for id in "${IDS[@]}"; do
            curl -s -m 5 -X POST "$URL/cancel/$id" >/dev/null 2>&1 || true
        done
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT INT TERM

echo "creating $CONNS groups (mode=$MODE) against $URL ..."
create_start_ns=$(date +%s%N)
created=0
for i in $(seq 1 "$CONNS"); do
    id=$(curl -s -m 10 -X POST "$CREATE_URL" | tr -d '\n')
    if [[ -z $id ]]; then
        echo "  conn $i: CREATE FAILED (timeout or error)"
        continue
    fi
    IDS+=("$id")
    created=$((created + 1))
    # One live SSE connection per group; -m bounds it past the test window.
    curl -s -N -m $((DURATION + 10)) "$URL/events/$id" > "$WORKDIR/conn-$i.txt" 2>/dev/null &
    PIDS+=($!)
done
create_ms=$(( ($(date +%s%N) - create_start_ns) / 1000000 ))
echo "created $created/$CONNS groups in ${create_ms}ms (avg $(( create_ms / (created > 0 ? created : 1) ))ms/create)"
echo "watching for ${DURATION}s ..."
sleep "$DURATION"

for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
wait 2>/dev/null || true

echo
echo "=== results (window: ${DURATION}s) ==="
total_events=0
conns_with_data=0
for i in $(seq 1 "$CONNS"); do
    f="$WORKDIR/conn-$i.txt"
    if [[ ! -s $f ]]; then
        echo "conn $i: no data received"
        continue
    fi
    events=$(grep -c '^event: count' "$f" || true)
    distinct=$(grep -o '"index":[0-9]*' "$f" | sort -u | wc -l || true)
    terminal=$(grep -oE '^event: (cancelled|done|error)' "$f" | tail -1 | awk '{print $2}' || true)
    total_events=$((total_events + events))
    if [[ $events -gt 0 || -n $terminal ]]; then
        conns_with_data=$((conns_with_data + 1))
    fi
    printf 'conn %-4s events=%-6s rate=%5.1f/s  distinct_counters=%-3s%s\n' \
        "$i" "$events" "$(echo "$events / $DURATION" | bc -l)" "$distinct" \
        "${terminal:+  terminal=$terminal}"
done

echo "---"
echo "connections healthy:  $conns_with_data/$CONNS"
echo "total count events:   $total_events"
if [[ $MODE == sleep ]]; then
    echo "expected (10/s/conn): ~$((created * DURATION * 10)) (minus watcher attach lag; no replay)"
fi
