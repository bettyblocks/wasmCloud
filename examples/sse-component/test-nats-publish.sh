#!/usr/bin/env bash
# test-nats-publish.sh -- Publish test messages to NATS for the SSE bridge
#
# Usage:
#   ./test-nats-publish.sh                        # single message to events.demo
#   ./test-nats-publish.sh events.alerts           # single message to custom subject
#   ./test-nats-publish.sh events.demo 5 1         # 5 messages, 1 second apart
#   ./test-nats-publish.sh events.demo 1 0 "hi"   # single message with custom payload
#
# Requires: nats CLI (https://github.com/nats-io/natscli)

set -euo pipefail

SUBJECT="${1:-events.demo}"
COUNT="${2:-1}"
INTERVAL="${3:-0}"
CUSTOM_MSG="${4:-}"
NATS_URL="${NATS_URL:-nats://localhost:4222}"

for i in $(seq 1 "$COUNT"); do
  TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  if [ -n "$CUSTOM_MSG" ]; then
    MESSAGE="$CUSTOM_MSG"
  else
    MESSAGE="Hello from NATS #${i} at ${TIMESTAMP}"
  fi
  echo "Publishing to ${SUBJECT}: ${MESSAGE}"
  nats pub --server "$NATS_URL" "$SUBJECT" "$MESSAGE"
  if [ "$i" -lt "$COUNT" ] && [ "$INTERVAL" -gt 0 ]; then
    sleep "$INTERVAL"
  fi
done

echo "Done. Published ${COUNT} message(s) to ${SUBJECT}."
