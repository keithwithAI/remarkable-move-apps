#!/bin/sh
# Idempotent installer for the second-brain device tools.
#
# Run ON the reMarkable as root, and RE-RUN after any OS update — it re-applies the systemd
# units that OS updates wipe from /etc. Safe to run repeatedly; never overwrites your secrets.
#
#   scp -r secondbrain root@10.11.99.1:/home/root/
#   ssh root@10.11.99.1 'sh /home/root/secondbrain/deploy/install.sh'
set -eu

ROOT=/home/root/secondbrain
SRC="$(cd "$(dirname "$0")/.." && pwd)"   # the secondbrain dir this script lives in

mkdir -p "$ROOT/bin" "$ROOT/config" "$ROOT/cache" "$ROOT/vault/Habits" "$ROOT/vault/Recipes"

# BusyBox (the device shell) has no coreutils `install`; use a portable cp+chmod shim.
inst() { cp -f "$2" "$3" && chmod "$1" "$3"; }   # inst <mode> <src> <dst>

# The root partition is mounted read-only and /etc is a RAM overlay on top of it, so changes made
# through /etc vanish at the next reboot. Anything that has to persist also goes on the real
# partition, reached through a bind mount so the overlay stays up (unmounting it drops SSH).
REAL=/tmp/secondbrain-rootfs
release_root() {
  umount "$REAL" 2>/dev/null || true
  rmdir "$REAL" 2>/dev/null || true
  mount -o remount,ro / 2>/dev/null || true
}
trap release_root EXIT
mount -o remount,rw /
mkdir -p "$REAL"
mount --bind / "$REAL"
wanted_by() {   # wanted_by <target> <unit>: the persistent half of `systemctl enable`
  mkdir -p "$REAL/etc/systemd/system/$1.wants"
  ln -sfn "/etc/systemd/system/$2" "$REAL/etc/systemd/system/$1.wants/$2"
}

# 1. Binaries — copy the prebuilt aarch64 binaries staged in dist/ (see deploy notes in README).
for b in agenda vaultsync selfupdate agendaapp reader-app habitapp recipeapp onethingapp browserapp; do
  if [ -f "$SRC/dist/$b" ]; then
    inst 0755 "$SRC/dist/$b" "$ROOT/bin/$b"
    echo "installed $ROOT/bin/$b"
  else
    echo "WARN: $SRC/dist/$b not found — cross-compile and stage it (see README)."
  fi
done

# 1b. Browser (WPE WebKit) runtime. The engine libs are a large app-owned bundle at $ROOT/wpe that
#     survives OS updates (deploy/wpe-setup.sh fetches it if absent). Install the launch wrapper and
#     env, and maintain the multiarch symlink WebKit's compiled-in helper paths need. /usr is wiped
#     by OS updates, so re-running this script after an update puts the link back.
if [ -f "$ROOT/bin/browserapp" ] || [ -f "$SRC/dist/browserapp" ]; then
  inst 0755 "$SRC/deploy/browser-launch.sh" "$ROOT/bin/browser-launch.sh"
  mkdir -p "$ROOT/wpe"
  inst 0644 "$SRC/deploy/wpe/env.sh" "$ROOT/wpe/env.sh"
  if [ -d "$ROOT/wpe/usr/lib/aarch64-linux-gnu" ]; then
    ln -sfn "$ROOT/wpe/usr/lib/aarch64-linux-gnu" /usr/lib/aarch64-linux-gnu
    echo "browser: WPE runtime linked ($ROOT/wpe)"
  else
    echo "NOTE: WPE runtime missing — run  sh $SRC/deploy/wpe-setup.sh  to fetch it (~large, one-time)."
  fi
fi

# Record the installed version (selfupdate compares this against GitHub Releases).
if [ -x "$ROOT/bin/selfupdate" ]; then
  "$ROOT/bin/selfupdate" --version > "$ROOT/VERSION" 2>/dev/null || true
fi

# 2. Config — install the example only if there's no real config yet (never clobber secrets).
if [ ! -f "$ROOT/config/config.toml" ]; then
  cp "$SRC/config/config.example.toml" "$ROOT/config/config.toml"
  echo "wrote $ROOT/config/config.toml — fill in service account, Todoist token, vault remote."
fi
chmod 700 "$ROOT/config" || true
for f in config.toml service-account.json todoist.token id_ed25519; do
  [ -f "$ROOT/config/$f" ] && chmod 600 "$ROOT/config/$f" || true
done

# 3. systemd units (live in /etc → wiped by OS updates → THIS is why install.sh is re-runnable).
for u in agenda.service agenda.timer vaultsync.service vaultsync.timer \
         secondbrain-update.service secondbrain-update.timer secondbrain-sleep.service; do
  inst 0644 "$SRC/deploy/systemd/$u" "/etc/systemd/system/$u"
done
# The sleep-screen units set TZ explicitly, so give them this device's zone (see SETUP.md).
DEVICE_TZ="$(timedatectl show -p Timezone --value 2>/dev/null || true)"
if [ -n "$DEVICE_TZ" ]; then
  sed -i "s|^Environment=TZ=.*|Environment=TZ=$DEVICE_TZ|" \
    /etc/systemd/system/agenda.service /etc/systemd/system/secondbrain-sleep.service
fi
systemctl daemon-reload
systemctl enable --now agenda.timer vaultsync.timer
# The daily update timer runs selfupdate directly, so `[update] auto` has to be honoured here.
if grep -Eq '^[[:space:]]*auto[[:space:]]*=[[:space:]]*true' "$ROOT/config/config.toml"; then
  systemctl enable --now secondbrain-update.timer
  wanted_by timers.target secondbrain-update.timer
else
  systemctl disable --now secondbrain-update.timer 2>/dev/null || true
  rm -f "$REAL/etc/systemd/system/timers.target.wants/secondbrain-update.timer"
  echo "self-update timer off ([update] auto = false)"
fi
# Pre-sleep lock-screen refresh: enable (NOT --now) so it's pulled in each time sleep.target starts.
systemctl enable secondbrain-sleep.service

# Repeat the units and their links on the real partition so they survive a reboot.
for u in agenda.service agenda.timer vaultsync.service vaultsync.timer \
         secondbrain-update.service secondbrain-update.timer secondbrain-sleep.service; do
  inst 0644 "/etc/systemd/system/$u" "$REAL/etc/systemd/system/$u"
done
wanted_by timers.target agenda.timer
wanted_by timers.target vaultsync.timer
wanted_by sleep.target secondbrain-sleep.service

# 4. AppLoad launcher entries (agenda + reader) — only once XOVI + AppLoad are installed.
#    /home/root survives OS updates, so these persist; harmless to re-run. Each app dir carries
#    an optional icon.png (the launcher auto-discovers it beside the manifest).
#    A fresh AppLoad has no exthome folder until something creates it, so check for the extension.
APPLOAD=/home/root/xovi/exthome/appload
if [ -d "$APPLOAD" ] || [ -f /home/root/xovi/extensions.d/appload.so ]; then
  for app in agenda reader habits recipes onething browser; do
    mkdir -p "$APPLOAD/$app"
    inst 0644 "$SRC/deploy/appload/$app/external.manifest.json" "$APPLOAD/$app/external.manifest.json"
    [ -f "$SRC/deploy/appload/$app/icon.png" ] && inst 0644 "$SRC/deploy/appload/$app/icon.png" "$APPLOAD/$app/icon.png"
    echo "installed AppLoad app: $app"
  done
else
  echo "NOTE: $APPLOAD missing — install XOVI + AppLoad first, then re-run for the Agenda + Reader apps."
fi

# 5. Keep OS auto-updates off (re-enabled by every upgrade). Codex OS (Paper Pro Move) uses
#    swupdate + the reMarkable OTA applier; older reMarkable OS used update-engine. Disable
#    whichever exist so this stays correct across devices.
for u in swupdate.service rm-apply-ota.service update-engine.service; do
  systemctl disable --now "$u" 2>/dev/null || true
  rm -f "$REAL"/etc/systemd/system/*.wants/"$u"
done

echo "---"
echo "Installed. Active timers:"
systemctl list-timers --no-pager 2>/dev/null | grep -E 'agenda|vaultsync|secondbrain-update' || true
echo "Run once now:  $ROOT/bin/agenda --config $ROOT/config/config.toml --out $ROOT/cache/preview.png"
echo "Check updates: $ROOT/bin/selfupdate --config $ROOT/config/config.toml"
