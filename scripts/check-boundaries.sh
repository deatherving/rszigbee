#!/usr/bin/env bash
# Enforces the dependency rules from the README, "Boundaries". These are
# the rules that keep MQTT out of the core; a comment saying "must not depend
# on MQTT" is worth nothing without a check that fails the build.
set -euo pipefail

fail=0
# All diagnostics go to stderr: `tree` is called inside a command
# substitution, and anything it writes to stdout would be captured into the
# caller's variable instead of shown.
note() { printf '  %s\n' "$*" >&2; }
bad()  { printf 'FAIL: %s\n' "$*" >&2; fail=1; }
ok()   { printf 'ok:   %s\n' "$*" >&2; }

# A dependency check that silently sees an empty tree passes vacuously, which
# is worse than no check at all. Fail loudly instead.
tree() {
  local out
  if ! out=$(cargo tree --quiet -p "$1" --edges normal --prefix none 2>&1); then
    bad "cannot resolve the dependency tree for '$1'"
    note "cargo tree said: $(head -2 <<<"$out" | tr '\n' ' ')"
    note "Every boundary check below would pass vacuously, so stopping."
    exit 1
  fi
  if [ -z "$out" ]; then
    bad "the dependency tree for '$1' is empty; the checks would pass vacuously"
    exit 1
  fi
  sort -u <<<"$out"
}

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

# --- EZSP must be contained in the Ember adapter ---
# The rule the architecture rests on: core and the adapter trait do not know
# EZSP exists. Only rszigbee-adapter-ember does.
for crate in rszigbee-core rszigbee-adapter rszigbee-spec; do
  t="$(tree "$crate")"
  for forbidden in ezsp ashv2 tokio-serial serialport; do
    if grep -qi "^${forbidden} " <<<"$t"; then
      bad "$crate depends on '${forbidden}'"
      note "Coordinator protocols belong in rszigbee-adapter-<family> only."
    fi
  done
done
grep -qiE '^(ezsp|ashv2) ' <<<"$(tree rszigbee-core)$(tree rszigbee-adapter)$(tree rszigbee-spec)" \
  || ok "EZSP is contained in the Ember adapter"

# --- the facade must expose every internal crate a user needs ---
if [ -d crates/rszigbee ]; then
  facade="$(tree rszigbee)"
  for needed in rszigbee-core rszigbee-adapter rszigbee-spec; do
    grep -q "^${needed} " <<<"$facade" \
      || bad "the rszigbee facade does not re-export ${needed}"
  done
  ok "the facade covers the internal crates"
fi

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
