#!/bin/sh
# AppLoad entry point for browserapp — warm-daemon aware. The daemon owns WebKit and stays booted
# across app opens (leaving via the launcher keeps it warm; ⭲ quits it). This wrapper's job:
#   1. attach to a running daemon (reopen in ~1-2s — the booted page IS the cache);
#   2. no daemon → spawn one in its own session (setsid, so AppLoad's teardown can't reach it),
#      then attach;
#   3. attach still failing → legacy single-process mode. Never a dead app.
# Also self-heals the multiarch symlink WebKit's compiled-in helper paths need (OS updates wipe
# /usr; it's writable so no remount). The runtime is app-owned at $R and survives OS updates.
R=/home/root/secondbrain/wpe
export R
LNK=/usr/lib/aarch64-linux-gnu
TGT="$R/usr/lib/aarch64-linux-gnu"
[ "$(readlink "$LNK" 2>/dev/null)" = "$TGT" ] || ln -sfn "$TGT" "$LNK" 2>/dev/null
. "$R/env.sh"
mkdir -p "$XDG_RUNTIME_DIR" "$TMPDIR" /home/root/secondbrain/browser 2>/dev/null
# Force our log filter — AppLoad pre-sets RUST_LOG=info in the environment, so a `${RUST_LOG:-…}`
# default would never apply and the per-present debug timings would be silently dropped.
export RUST_LOG="info,browserapp=debug"
BIN=/home/root/secondbrain/bin/browserapp
DATA=/home/root/secondbrain/browser
LOG=/home/root/browser.log
# Optional: this file holds the Kindle library URL for a store other than the UK default.
if [ -f "$DATA/default-url" ]; then
  KINDLE_DEFAULT_URL=$(cat "$DATA/default-url")
  export KINDLE_DEFAULT_URL
fi

# Fast path: a warm daemon answers → attach for the whole session. One retry after a beat — a
# handshake can transiently fail (observed: EPIPE when reopening moments after a detach) and
# sweeping a healthy daemon over a blip costs a full 13s cold boot.
try=0
while [ $try -lt 2 ]; do
  "$BIN" --attach --data-dir "$DATA" >> "$LOG" 2>&1
  rc=$?
  [ $rc -eq 0 ] && exit 0
  # Killed by a signal (rc>128) = AppLoad tore the session down mid-attach — the daemon detached
  # and stays warm. Do NOT spawn a second daemon over it.
  [ $rc -gt 128 ] && exit $rc
  try=$((try+1))
  [ $try -lt 2 ] && sleep 1
done

# No healthy daemon: clear any wedged one, start fresh (own session), attach when the socket is up.
# (BusyBox has pgrep but no pkill.)
WEDGED=$(pgrep -f "browserapp --daemon" 2>/dev/null)
[ -n "$WEDGED" ] && kill $WEDGED 2>/dev/null && sleep 1
# A socket file here is stale by definition (a live daemon would have answered the attach above) —
# remove it BEFORE spawning, so the wait below only passes once the NEW daemon has actually bound.
# The daemon needs ~2s just to dynamic-link WebKit before it can rebind; waiting on the stale file
# made the wrapper attach into nothing and fall back to a cold single-process boot.
rm -f "$DATA/attach.sock"
setsid "$BIN" --daemon --data-dir "$DATA" >> "$LOG" 2>&1 &
i=0
while [ $i -lt 40 ] && [ ! -S "$DATA/attach.sock" ]; do
  i=$((i+1))
  usleep 250000 2>/dev/null || sleep 1
done
"$BIN" --attach --data-dir "$DATA" >> "$LOG" 2>&1
rc=$?
[ $rc -eq 0 ] && exit 0
[ $rc -gt 128 ] && exit $rc

# Last resort: today's single-process behaviour.
exec "$BIN" --data-dir "$DATA" >> "$LOG" 2>&1
