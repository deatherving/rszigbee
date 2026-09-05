#!/usr/bin/env bash
# Runs every check CI runs, in CI's order, before you push.
#
# This exists because a formatting-only failure reached `main` twice in a row:
# clippy and the test suite were run by hand, `cargo fmt --check` was not, and
# a green local run meant nothing. Verifying a *subset* of CI is the same as
# not verifying it, so this file is the subset-free version. If you add a step
# to .github/workflows/ci.yml, add it here too.
#
#   ./scripts/verify.sh          everything
#   ./scripts/verify.sh --fast   skip the slower duplicate clippy passes
#
# Check the EXIT CODE, do not grep the output. Piping this through a filter is
# how a failure reached main a second time: the summary line was dropped and the
# run looked clean. Non-zero means do not push.
set -euo pipefail

cd "$(cd "$(dirname "$0")/.." && pwd)"
FAST="${1:-}"
failed=()

step() {
  local name="$1"; shift
  printf '\n\033[1m==> %s\033[0m\n' "$name"
  if "$@"; then
    printf '\033[32mok\033[0m   %s\n' "$name"
  else
    printf '\033[31mFAIL\033[0m %s\n' "$name"
    failed+=("$name")
  fi
}

# Formatting first, because it is the cheapest and the one that actually broke.
step "cargo fmt --check"        cargo fmt --all -- --check
step "clippy (all features)"    cargo clippy --workspace --all-targets --all-features -- -D warnings
step "tests"                    cargo test --workspace --all-features
step "doctests"                 cargo test --workspace --all-features --doc
step "cargo doc"                env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
step "architectural boundaries" ./scripts/check-boundaries.sh

# The feature-combination passes catch code that only compiles with everything
# switched on. Slower, and skippable while iterating.
if [[ "$FAST" != "--fast" ]]; then
  step "clippy (no default features)" cargo clippy --workspace --all-targets --no-default-features -- -D warnings
  step "clippy (default features)"    cargo clippy --workspace --all-targets -- -D warnings
  step "check (all features)"         cargo check --workspace --all-features
  # Licences. Only if the tool is installed; CI always has it.
  if command -v cargo-deny >/dev/null; then
    step "dependency licences" cargo deny check
  else
    printf '\n\033[33mskip\033[0m dependency licences (cargo-deny not installed)\n'
  fi
fi

printf '\n'
if ((${#failed[@]})); then
  printf '\033[31m%d check(s) failed:\033[0m\n' "${#failed[@]}"
  printf '  - %s\n' "${failed[@]}"
  exit 1
fi
printf '\033[32mall checks passed\033[0m\n'
