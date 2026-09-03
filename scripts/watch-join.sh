#!/usr/bin/env bash
# Opens a join window on the remote dongle and streams events live.
#
# Written because the obvious form of this -- piping the run through `tail` --
# buffers everything until the process exits, so nothing is visible while the
# pairing window is actually open. During a finite window that is useless: you
# cannot tell whether the device is being seen, and by the time output arrives
# the window has closed.
#
#   scripts/watch-join.sh grownest@grownest.local [/dev/ttyUSB0]
set -euo pipefail

HOST="${1:?usage: watch-join.sh user@host [serial-path]}"
PORT="${2:-/dev/ttyUSB0}"
DIR="${3:-/tmp/rszigbee-build}"

# -tt forces a pty so the remote process line-buffers rather than block-buffers,
# and no local pipe means nothing sits in a buffer waiting for EOF.
exec ssh -tt "$HOST" \
  "cd '$DIR' && RUST_LOG=info,rszigbee_adapter_ember=debug \
   stdbuf -oL -eL ./target/release/examples/ember_runtime '$PORT' --permit-join"
