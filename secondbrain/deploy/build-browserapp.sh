#!/bin/sh
# Build the browserapp aarch64 binary and stage it into dist/. Run on the Mac.
#
# browserapp links WPE WebKit (build.rs cc-compiles engine/wpeengine.c + pkg-config links
# wpe-webkit-1.1/wpebackend-fdo-1.0), which the `cross`/`cargo-zigbuild` toolchains DON'T have — so
# it can't ride release.sh's normal cross build. Instead we build it NATIVELY in the `wpe-build`
# Debian arm64 container (colima), which carries the exact device WPE 2.38.6 dev libs. The container
# bind-mounts the live repo at $WPE_CTR_REPO and writes to a host-visible target dir, so the finished
# aarch64 ELF lands back on the Mac with no cross toolchain involved.
#
#   ./deploy/build-browserapp.sh                # stage to dist/browserapp
#   ./deploy/build-browserapp.sh /tmp/foo       # stage to a custom path
#
# Overridable: WPE_CONTAINER, WPE_CTR_REPO (repo mount inside container), WPE_CTR_TARGET
# (CARGO_TARGET_DIR inside container), WPE_HOST_TARGET (its host-side path).
set -eu

SRC="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$SRC/dist/browserapp}"

CTR="${WPE_CONTAINER:-wpe-build}"
CTR_REPO="${WPE_CTR_REPO:-/repo}"
CTR_TARGET="${WPE_CTR_TARGET:-/work/target}"
HOST_TARGET="${WPE_HOST_TARGET:-$HOME/wpe-build/target}"

command -v docker >/dev/null 2>&1 || {
  echo "build-browserapp: 'docker' not found — start colima (colima start)." >&2; exit 1; }
if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$CTR"; then
  echo "build-browserapp: container '$CTR' is not running." >&2
  echo "  bring it up with:  colima start && docker start $CTR" >&2
  exit 1
fi

echo "==> container-building browserapp ($CTR, native aarch64) from $CTR_REPO"
# --locked: build against the committed Cargo.lock; never mutate the repo tree.
docker exec "$CTR" sh -lc \
  ". ~/.cargo/env && cd '$CTR_REPO' && CARGO_TARGET_DIR='$CTR_TARGET' cargo build --release --locked -p browserapp"

BIN="$HOST_TARGET/release/browserapp"
[ -f "$BIN" ] || { echo "build-browserapp: expected binary not found at $BIN" >&2; exit 1; }
mkdir -p "$(dirname "$OUT")"
cp "$BIN" "$OUT"
echo "staged browserapp -> $OUT"
