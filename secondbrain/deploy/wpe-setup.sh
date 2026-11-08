#!/bin/sh
# One-time bootstrap of the WPE WebKit runtime for the Browser app (~138 MB download). Run ON the
# device. The runtime is app-owned at /home/root/secondbrain/wpe and survives OS updates; you only
# need this on a fresh/reflashed device (or to update the engine). install.sh links it afterwards.
#
set -eu
ROOT=/home/root/secondbrain
REPO=keithwithAI/remarkable-move-apps
TAG=wpe-runtime-v1
ASSET=wpe-runtime.tar.gz
SHA=cf1ca7cbcb0eb96b235c36df6a1ab2391a08a233186156020ba00a5ac69b3fa1
TMP=/home/root/wpe-runtime.tar.gz

echo "downloading WPE runtime (~138 MB) ..."
wget -q -O "$TMP" "https://github.com/$REPO/releases/download/$TAG/$ASSET"

echo "verifying ..."
GOT=$(sha256sum "$TMP" | cut -c1-64)
if [ "$GOT" != "$SHA" ]; then
  echo "ERROR: sha256 mismatch (got $GOT) — aborting, not extracting." >&2
  rm -f "$TMP"; exit 1
fi

# Stage into wpe.new then swap, so a failed/partial extract never wrecks a working runtime.
echo "extracting ..."
rm -rf "$ROOT/wpe.new"
mkdir -p "$ROOT/wpe.new"
tar xzf "$TMP" -C "$ROOT/wpe.new" --strip-components=1
rm -f "$TMP"
rm -rf "$ROOT/wpe.old"
[ -d "$ROOT/wpe" ] && mv "$ROOT/wpe" "$ROOT/wpe.old"
mv "$ROOT/wpe.new" "$ROOT/wpe"
rm -rf "$ROOT/wpe.old"

# Maintain the multiarch symlink WebKit's compiled-in helper paths need (writable /usr, no remount).
ln -sfn "$ROOT/wpe/usr/lib/aarch64-linux-gnu" /usr/lib/aarch64-linux-gnu
echo "WPE runtime installed at $ROOT/wpe. Re-run install.sh (or launch the Browser app) next."
