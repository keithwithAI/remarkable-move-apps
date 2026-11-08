#!/bin/sh
# Build the aarch64 binaries, compute sha256s, write manifest.json, and publish a GitHub Release
# that the device's `selfupdate` will pull over WiFi. Run on the Mac.
#
#   ./deploy/release.sh 0.2.0
#
# Requires: a cross toolchain (`cross` or `cargo-zigbuild`), `gh`, and `shasum`/`sha256sum`.
set -eu

VERSION="${1:?usage: release.sh <version, e.g. 0.2.0>}"
TARGET="${TARGET:-aarch64-unknown-linux-gnu}"
REPO="${REPO:-keithwithAI/remarkable-move-apps}"
SRC="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$SRC/dist"
# Where cargo actually writes. A build can be configured to land outside the repo.
TARGET_DIR="$(cd "$SRC" && cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$TARGET_DIR" ] || TARGET_DIR="${CARGO_TARGET_DIR:-$SRC/target}"
# Cross-built binaries shipped in the release manifest (self-updated on device). The qtfbspike binary
# is staged to dist/ for manual scp but deliberately kept out of the manifest — it's throwaway.
# browserapp is NOT here: it links WPE WebKit, which the cross toolchain lacks, so it's built
# separately in the native-arm64 container (see build-browserapp.sh) and appended below.
BINS="agenda vaultsync selfupdate agendaapp reader-app habitapp recipeapp onethingapp"

# Guard against release/crate version drift: the binaries bake in CARGO_PKG_VERSION (the workspace
# version), so `--version` only matches the release if they agree. Abort loudly if they don't.
CARGO_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -n 1)"
if [ "$CARGO_VERSION" != "$VERSION" ]; then
  echo "version mismatch: Cargo.toml workspace version is $CARGO_VERSION but you asked to release $VERSION." >&2
  echo "Bump [workspace.package] version in Cargo.toml to $VERSION (or release v$CARGO_VERSION)." >&2
  exit 1
fi

mkdir -p "$DIST"

# Keep the builder's home directory out of the panic paths compiled into the binaries.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/build"

echo "==> building $TARGET (release)"
# Exclude browserapp — it links WPE (unavailable in the cross toolchain); built in-container below.
if command -v cross >/dev/null 2>&1; then
  ( cd "$SRC" && cross build --release --target "$TARGET" --workspace --exclude browserapp )
elif command -v cargo-zigbuild >/dev/null 2>&1; then
  ( cd "$SRC" && cargo zigbuild --release --target "$TARGET" --workspace --exclude browserapp )
else
  echo "need 'cross' or 'cargo-zigbuild' to cross-compile — see README." >&2
  exit 1
fi

sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }

echo "==> staging dist/ and writing manifest.json"
# Stage the spike for manual deploys (outside the manifest).
[ -f "$TARGET_DIR/$TARGET/release/qtfbspike" ] && cp "$TARGET_DIR/$TARGET/release/qtfbspike" "$DIST/qtfbspike"
# Stage the cross-built binaries.
for b in $BINS; do
  cp "$TARGET_DIR/$TARGET/release/$b" "$DIST/$b"
done

# browserapp: built natively in the wpe-build container (links WPE). SKIP_BROWSERAPP=1 ships a
# release without it — devices then keep whatever browserapp they already have.
MANIFEST_BINS="$BINS"
if [ "${SKIP_BROWSERAPP:-0}" = "1" ]; then
  echo "==> SKIP_BROWSERAPP=1 — not building/shipping browserapp this release"
else
  "$SRC/deploy/build-browserapp.sh" "$DIST/browserapp"
  MANIFEST_BINS="$BINS browserapp"
fi

# Launcher icons ride the release too (assets icon-<app>.png; installed by selfupdate into the
# AppLoad dirs — they show after the next xovi re-injection). Sourced from the committed PNGs.
ICON_APPS="agenda reader browser habits recipes onething"
for a in $ICON_APPS; do
  cp "$SRC/deploy/appload/$a/icon.png" "$DIST/icon-$a.png"
done

: > "$DIST/manifest.json.tmp"
i=0
for b in $MANIFEST_BINS; do
  [ "$i" -gt 0 ] && printf ',\n' >> "$DIST/manifest.json.tmp"
  printf '    {"name": "%s", "sha256": "%s"}' "$b" "$(sha "$DIST/$b")" >> "$DIST/manifest.json.tmp"
  i=1
done
: > "$DIST/manifest-icons.json.tmp"
i=0
for a in $ICON_APPS; do
  [ "$i" -gt 0 ] && printf ',\n' >> "$DIST/manifest-icons.json.tmp"
  printf '    {"app": "%s", "sha256": "%s"}' "$a" "$(sha "$DIST/icon-$a.png")" >> "$DIST/manifest-icons.json.tmp"
  i=1
done
{
  printf '{\n  "version": "%s",\n  "binaries": [\n' "$VERSION"
  cat "$DIST/manifest.json.tmp"
  printf '\n  ],\n  "icons": [\n'
  cat "$DIST/manifest-icons.json.tmp"
  printf '\n  ]\n}\n'
} > "$DIST/manifest.json"
rm -f "$DIST/manifest.json.tmp" "$DIST/manifest-icons.json.tmp"
cat "$DIST/manifest.json"

echo "==> packing the install bundle"
BUNDLE="$DIST/secondbrain-device.tar.gz"
BUNDLE_BINS=""
for b in $MANIFEST_BINS; do BUNDLE_BINS="$BUNDLE_BINS secondbrain/dist/$b"; done
# shellcheck disable=SC2086 # word-splitting BUNDLE_BINS is the point; names contain no spaces
COPYFILE_DISABLE=1 tar czf "$BUNDLE" -C "$SRC/.." --no-xattrs $BUNDLE_BINS \
  secondbrain/deploy secondbrain/config/config.example.toml

echo "==> publishing v$VERSION to $REPO"
# Build the asset list as positional params so paths with spaces survive.
set -- "$DIST/manifest.json" "$BUNDLE"
for b in $MANIFEST_BINS; do set -- "$@" "$DIST/$b"; done
for a in $ICON_APPS; do set -- "$@" "$DIST/icon-$a.png"; done
# Pin the tag to the exact commit we built from (must already be pushed). Without --target, gh tags
# whatever the remote default branch currently points at — a race if anything else pushed meanwhile.
gh release create "v$VERSION" --repo "$REPO" --title "v$VERSION" --notes "secondbrain v$VERSION" \
  --target "$(git -C "$SRC" rev-parse HEAD)" "$@"

echo "released v$VERSION — the device will pick it up on its next update timer (or: selfupdate --force)"
