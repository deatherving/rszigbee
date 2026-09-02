#!/usr/bin/env bash
# Enforces the dependency rules from the README, "Boundaries". These are
# the rules that keep MQTT out of the core; a comment saying "must not depend
# on MQTT" is worth nothing without a check that fails the build.
set -euo pipefail

fail=0
note() { printf '  %s\n' "$*"; }
bad()  { printf 'FAIL: %s\n' "$*"; fail=1; }
ok()   { printf 'ok:   %s\n' "$*"; }

tree() { cargo tree --quiet -p "$1" --edges normal --prefix none 2>/dev/null | sort -u; }

# --- rszigbee-core must not reach MQTT, JSON, or Home Assistant ---
core="$(tree rszigbee-core)"
for forbidden in rumqttc rumqttd mqtt serde_json; do
  if grep -qi "^${forbidden} " <<<"$core"; then
    bad "rszigbee-core depends on '${forbidden}'"
    note "MQTT and JSON belong in rszigbee-mqtt. See README, "Boundaries"."
  fi
done
grep -qi 'homeassistant\|home-assistant' <<<"$core" \
  && bad "rszigbee-core depends on a Home Assistant crate" \
  || ok "rszigbee-core is free of MQTT, JSON and Home Assistant"

# --- rszigbee-spec is sans-IO ---
spec="$(tree rszigbee-spec)"
for forbidden in tokio serialport tokio-serial mio socket2; do
  if grep -qi "^${forbidden} " <<<"$spec"; then
    bad "rszigbee-spec depends on '${forbidden}'"
    note "rszigbee-spec is sans-IO: codecs and data only."
  fi
done
grep -qiE '^(tokio|serialport|tokio-serial|mio|socket2) ' <<<"$spec" \
  || ok "rszigbee-spec is sans-IO"

# --- rszigbee-devices must not depend on the runtime (once it exists) ---
if [ -d crates/rszigbee-devices ]; then
  if tree rszigbee-devices | grep -q '^rszigbee-core '; then
    bad "rszigbee-devices depends on rszigbee-core"
    note "Definitions are data the runtime interprets, not the reverse."
    note "This rule is what makes the importer and validator possible."
  else
    ok "rszigbee-devices does not depend on rszigbee-core"
  fi
fi

# --- ZigbeeStore must stay a Zigbee-domain trait ---
# The generic blob escape hatch was removed in report r2; this stops it coming
# back through the side door.
if awk '/pub trait ZigbeeStore/,/^}/' crates/rszigbee-core/src/store.rs \
     | grep -qiE 'blob|mqtt|homeassistant|discovery'; then
  bad "ZigbeeStore has gained a blob/MQTT/Home Assistant method"
  note "Layers above core own their own persistence. See README, "Persistence"."
else
  ok "ZigbeeStore holds only Zigbee domain state"
fi

# --- no unsafe ---
if grep -rn --include='*.rs' -E '^\s*unsafe\b' crates/ | grep -v '^\s*//'; then
  bad "unsafe code found; it must be justified in an ADR first"
else
  ok "no unsafe code"
fi

exit "$fail"
