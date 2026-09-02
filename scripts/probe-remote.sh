#!/usr/bin/env bash
# Survey a remote Zigbee host, then build and run the read-only EZSP probe there.
#
# Phase 2 needs this repeatedly: the development machine and the machine with the
# dongle are usually not the same one.
#
#   ./scripts/probe-remote.sh grownest@grownest.local
#   ./scripts/probe-remote.sh grownest@grownest.local /dev/ttyUSB0 --baud 230400
#
# The probe itself is read-only. This script is too: it will NOT stop a running
# Zigbee service. If one holds the serial port it says so and stops, because
# taking down someone's live home automation to run a diagnostic is not a
# decision a script gets to make.
set -euo pipefail

HOST="${1:?usage: probe-remote.sh user@host [serial-path] [probe args...]}"
shift
PORT_HINT="${1:-}"
[ $# -gt 0 ] && shift || true
PROBE_ARGS=("$@")

SSH=(ssh -o ConnectTimeout=10 -o BatchMode=yes "$HOST")
say() { printf '\n=== %s ===\n' "$*"; }

# ---------------------------------------------------------------- reachability
if ! "${SSH[@]}" true 2>/dev/null; then
  cat >&2 <<EOF
cannot authenticate to $HOST with a key.

  ssh-copy-id -i ~/.ssh/id_ed25519.pub $HOST

Password auth cannot be used from a non-interactive shell.
EOF
  exit 1
fi

say "host"
"${SSH[@]}" 'echo "$(whoami)@$(hostname)"; uname -srm
  . /etc/os-release 2>/dev/null && echo "$PRETTY_NAME"
  echo "cpus: $(nproc 2>/dev/null || echo ?)  mem: $(awk "/MemTotal/{printf \"%.1f GiB\", \$2/1048576}" /proc/meminfo 2>/dev/null || echo ?)"'

# ------------------------------------------------------------- serial hardware
say "serial ports"
"${SSH[@]}" 'ls -l /dev/serial/by-id/ 2>/dev/null || echo "(no /dev/serial/by-id)"
  ls /dev/ttyUSB* /dev/ttyACM* 2>/dev/null || echo "(no ttyUSB*/ttyACM*)"'

# ---------------------------------------------------- who is holding the port?
# The single most important check. A running Zigbee2MQTT/ZHA/deconz holds the
# port exclusively, so the probe would fail to open it — and the useful output
# is "z2m is using it", not "permission denied".
say "port holders and Zigbee services"
"${SSH[@]}" '
  for d in /dev/ttyUSB* /dev/ttyACM*; do
    [ -e "$d" ] || continue
    holder=$( (command -v fuser >/dev/null && fuser -v "$d" 2>&1 | tail -n +2) \
              || (command -v lsof >/dev/null && lsof "$d" 2>/dev/null | tail -n +2) \
              || echo "(no fuser/lsof installed)")
    printf "%s: %s\n" "$d" "${holder:-free}"
  done
  echo "--- services ---"
  for s in zigbee2mqtt home-assistant homeassistant deconz zigbee2mqtt-edge; do
    st=$(systemctl is-active "$s" 2>/dev/null || true)
    [ -n "$st" ] && [ "$st" != "inactive" ] && [ "$st" != "unknown" ] && echo "$s: $st"
  done
  command -v docker >/dev/null && docker ps --format "{{.Names}} ({{.Image}})" 2>/dev/null | head
  true'

# --------------------------------------------------------------------- toolchain
say "rust toolchain"
HAS_CARGO=$("${SSH[@]}" 'command -v cargo >/dev/null && cargo --version || echo NONE')
echo "$HAS_CARGO"
if [ "$HAS_CARGO" = "NONE" ]; then
  cat <<EOF

No cargo on the remote. Either:
  a) install it there:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  b) cross-compile here and copy the binary over (needs a linker for the target)

Stopping: nothing has been changed on $HOST.
EOF
  exit 2
fi

# ----------------------------------------------------------------- pick a port
if [ -z "$PORT_HINT" ]; then
  PORT_HINT=$("${SSH[@]}" 'ls /dev/serial/by-id/* 2>/dev/null | head -1 || ls /dev/ttyUSB* /dev/ttyACM* 2>/dev/null | head -1 || true')
  [ -z "$PORT_HINT" ] && { echo "no serial port found; pass one explicitly" >&2; exit 3; }
  echo "auto-selected port: $PORT_HINT"
fi

BUSY=$("${SSH[@]}" "command -v fuser >/dev/null && fuser '$PORT_HINT' 2>/dev/null || true")
if [ -n "$BUSY" ]; then
  cat >&2 <<EOF

$PORT_HINT is held by another process (pid(s):$BUSY).

The probe needs exclusive access. Stop the Zigbee service yourself if you want
to run it -- this script will not do that for you:

  sudo systemctl stop zigbee2mqtt && ./scripts/probe-remote.sh $HOST $PORT_HINT
  sudo systemctl start zigbee2mqtt      # afterwards

EOF
  exit 4
fi

# ---------------------------------------------------------------- ship and run
say "copying the probe"
REMOTE_DIR="/tmp/rszigbee-ezsp-probe"
"${SSH[@]}" "rm -rf '$REMOTE_DIR' && mkdir -p '$REMOTE_DIR'"
tar czf - -C spikes ezsp-probe | "${SSH[@]}" "tar xzf - -C '$REMOTE_DIR'"
echo "-> $HOST:$REMOTE_DIR/ezsp-probe"

say "building on the remote (first build fetches crates; this takes a while on a Pi)"
"${SSH[@]}" "cd '$REMOTE_DIR/ezsp-probe' && cargo build --release 2>&1 | tail -5"

say "running the probe (READ-ONLY)"
"${SSH[@]}" "cd '$REMOTE_DIR/ezsp-probe' && RUST_LOG=\${RUST_LOG:-info} \
  ./target/release/ezsp-probe '$PORT_HINT' ${PROBE_ARGS[*]:-}" || {
  echo
  echo "probe reported failure. Re-run with RUST_LOG=trace to see whether any"
  echo "ASH bytes arrived at all, which separates wiring from protocol:"
  echo "  RUST_LOG=trace ./scripts/probe-remote.sh $HOST $PORT_HINT"
  exit 5
}
