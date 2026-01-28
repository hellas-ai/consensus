#!/bin/bash
# Hellas Local Network Runner
set -e

LOG_DIR="${TMPDIR:-/tmp}/hellas-local"
mkdir -p "$LOG_DIR"

echo "Building..."
cargo build --package node --release

echo "Starting 6 validators..."
for i in $(seq 0 5); do
    ./target/release/node run --config node/config/node$i.toml > "$LOG_DIR/node$i.log" 2>&1 &
    echo "  Node $i started (PID: $!)"
done

echo ""
echo "All nodes started. Logs in $LOG_DIR"
echo "Press Ctrl+C to stop."

trap "pkill -f 'node run --config'" EXIT
wait
