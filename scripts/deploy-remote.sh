#!/usr/bin/env bash
# Copy the workspace to a remote host for hardware testing.
#
# Not piped through `head`/`grep`: closing the pipe early SIGPIPEs tar and
# truncates the transfer, which silently leaves stale source on the remote.
# That cost real debugging time, so the transfer is verified afterwards.
set -euo pipefail

HOST="${1:?usage: deploy-remote.sh user@host [remote-dir]}"
DIR="${2:-/tmp/rszigbee-build}"
SSH=(ssh -o ConnectTimeout=10 -o BatchMode=yes "$HOST")

tmp=$(mktemp -t rszigbee-src.XXXXXX).tgz
trap 'rm -f "$tmp"' EXIT
# --no-mac-metadata and COPYFILE_DISABLE stop macOS tar emitting AppleDouble
# companion files and xattr pax headers. Without them a Linux host extracts
# literal `._lib.rs` files alongside the real ones, which doubles the source
# file count, litters the tree with junk, and makes GNU tar warn about
# `LIBARCHIVE.xattr.com.apple.provenance` on every file. Cosmetic-looking, but
# it broke the transfer verification below and cost real debugging time.
COPYFILE_DISABLE=1 tar czf "$tmp" \
  --no-mac-metadata --no-xattrs \
  --exclude target --exclude .git --exclude docs --exclude rszigbee-data .
printf 'archive: %s bytes\n' "$(wc -c <"$tmp" | tr -d ' ')"

"${SSH[@]}" "rm -rf '$DIR/crates' && mkdir -p '$DIR'"
"${SSH[@]}" "tar xzf - -C '$DIR'" <"$tmp"

# Verify rather than assume. Sorted, because `find` traversal order differs
# between macOS and Linux, and so does `sort` collation for '.' against '/'
# under a non-C locale. Both produced a false "transfer truncated" on an
# identical tree, so LC_ALL=C is load-bearing here, not tidiness.
sum_tree() {
  find crates -name '*.rs' | LC_ALL=C sort | xargs cat | shasum | cut -d' ' -f1
}
local_sum=$(sum_tree)
remote_sum=$("${SSH[@]}" "cd '$DIR' && find crates -name '*.rs' | LC_ALL=C sort | xargs cat | shasum | cut -d' ' -f1")
if [ "$local_sum" != "$remote_sum" ]; then
  echo "FAIL: the remote source does not match local" >&2
  echo "  Check for a truncated transfer, stale files on the remote, or" >&2
  echo "  AppleDouble ._* files if the archive was built on macOS." >&2
  echo "  local  $local_sum" >&2
  echo "  remote $remote_sum" >&2
  exit 1
fi
echo "verified: remote source matches local ($local_sum)"
