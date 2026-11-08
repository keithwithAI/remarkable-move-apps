#!/bin/sh
# One command to ship. Bumps the workspace version, refreshes Cargo.lock, commits, pushes, and
# publishes the GitHub Release (wrapping deploy/release.sh). The device then self-updates over WiFi
# on its own — the update timer, every sleep-screen render, and every Agenda/Reader launch all run
# `selfupdate` outbound. No USB, no `selfupdate --force` (that's only for reinstalling the *same*
# version). Run on the Mac.
#
#   ./deploy/ship.sh              # bump patch   (0.9.1 -> 0.9.2)
#   ./deploy/ship.sh minor        # bump minor   (0.9.1 -> 0.10.0)
#   ./deploy/ship.sh major        # bump major   (0.9.1 -> 1.0.0)
#   ./deploy/ship.sh 1.2.3        # explicit version
#   ./deploy/ship.sh patch --now  # also apply immediately IF the device is plugged in over USB
#
# Requires a clean git tree (commit your code first), `gh`, and a cross toolchain (`cross` or
# `cargo-zigbuild`) — the same tools release.sh needs. Also builds browserapp in the native-arm64
# `wpe-build` container (it links WPE, which the cross toolchain lacks), so colima + that container
# must be up. Set SKIP_BROWSERAPP=1 to ship a release without touching browserapp (e.g. a quick
# reader/agenda patch when colima is off) — devices keep whatever browserapp they already have.
set -eu

SRC="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SRC"

# CARGO_TARGET_DIR is left alone: release.sh finds the real target dir via `cargo metadata`.

# Parse args: an optional bump keyword / explicit version, and an optional --now.
BUMP="patch"
NOW=0
for a in "$@"; do
  case "$a" in
    --now) NOW=1 ;;
    patch|minor|major) BUMP="$a" ;;
    [0-9]*.[0-9]*.[0-9]*) BUMP="$a" ;;
    *) echo "usage: ship.sh [patch|minor|major|X.Y.Z] [--now]" >&2; exit 2 ;;
  esac
done

# Fail before mutating anything if the release tools are missing or gh's auth has lapsed —
# an expired token would otherwise only surface inside release.sh, after the commit + push.
command -v gh >/dev/null 2>&1 || { echo "ship.sh: 'gh' (GitHub CLI) not found." >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "ship.sh: 'gh' is not authenticated — run 'gh auth login' first." >&2; exit 1; }
command -v cross >/dev/null 2>&1 || command -v cargo-zigbuild >/dev/null 2>&1 || {
  echo "ship.sh: need 'cross' or 'cargo-zigbuild' to cross-compile." >&2; exit 1; }
# browserapp is built in the wpe-build container — check it's up NOW, before we mutate anything, so a
# stopped colima fails fast rather than after the commit + push. SKIP_BROWSERAPP=1 opts out entirely.
if [ "${SKIP_BROWSERAPP:-0}" != "1" ]; then
  WPE_CTR="${WPE_CONTAINER:-wpe-build}"
  if ! command -v docker >/dev/null 2>&1 || ! docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$WPE_CTR"; then
    echo "ship.sh: the '$WPE_CTR' container isn't running — browserapp can't be built." >&2
    echo "  start it (colima start && docker start $WPE_CTR), or set SKIP_BROWSERAPP=1 to ship without browserapp." >&2
    exit 1
  fi
fi

# Preflight: on main + clean tree, so the release is exactly HEAD + the version bump.
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "main" ] || { echo "ship.sh: on branch '$BRANCH', expected 'main'." >&2; exit 1; }
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "ship.sh: working tree is dirty — commit your code changes first, then ship." >&2
  git status --short >&2
  exit 1
fi

CUR="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
[ -n "$CUR" ] || { echo "ship.sh: could not read [workspace.package] version from Cargo.toml" >&2; exit 1; }

# Compute the next version.
case "$BUMP" in
  patch|minor|major)
    MAJ="${CUR%%.*}"; REST="${CUR#*.}"; MIN="${REST%%.*}"; PAT="${REST#*.}"
    case "$BUMP" in
      major) MAJ=$((MAJ + 1)); MIN=0; PAT=0 ;;
      minor) MIN=$((MIN + 1)); PAT=0 ;;
      patch) PAT=$((PAT + 1)) ;;
    esac
    NEXT="$MAJ.$MIN.$PAT"
    ;;
  *) NEXT="$BUMP" ;;
esac

echo "==> ship $CUR -> $NEXT"

# Bump the [workspace.package] version — the first line starting with `version = ` (deps are written
# `name = { version = ... }`, so `^version = ` only matches the workspace one). macOS/BSD sed.
sed -i '' "s/^version = \".*\"/version = \"$NEXT\"/" Cargo.toml

# Refresh Cargo.lock (all workspace members inherit the version) + validate the bump compiles.
cargo check --workspace

git add Cargo.toml Cargo.lock
git commit -m "v$NEXT" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"

# Cross-build BEFORE pushing, so a build failure leaves the bump commit local (recoverable) rather
# than pushed with no release. release.sh's own build then reuses this warm cache in seconds.
echo "==> cross-building (before push, so a failure stays local)"
# Exclude browserapp — it links WPE (unavailable in the cross toolchain); built in-container next.
if ! {
  if command -v cross >/dev/null 2>&1; then
    ( cd "$SRC" && cross build --release --target aarch64-unknown-linux-gnu --workspace --exclude browserapp )
  else
    ( cd "$SRC" && cargo zigbuild --release --target aarch64-unknown-linux-gnu --workspace --exclude browserapp )
  fi
}; then
  echo "ship.sh: cross-build FAILED. The v$NEXT commit is still local — undo it with:" >&2
  echo "  git reset --hard HEAD~1" >&2
  exit 1
fi

# Build browserapp in the container too (fail-fast before push; also warms the cache so release.sh's
# own browserapp build is a hit). Skippable for non-browser releases.
if [ "${SKIP_BROWSERAPP:-0}" != "1" ]; then
  echo "==> container-building browserapp (before push, so a failure stays local)"
  if ! "$SRC/deploy/build-browserapp.sh"; then
    echo "ship.sh: browserapp container build FAILED. The v$NEXT commit is still local — undo it with:" >&2
    echo "  git reset --hard HEAD~1" >&2
    echo "  (or re-run with SKIP_BROWSERAPP=1 to ship without browserapp)" >&2
    exit 1
  fi
fi

git push origin main

# Write dist/manifest.json and publish the GitHub Release (release.sh pins the tag to this pushed
# commit via --target; its build step is a cache hit).
if ! "$SRC/deploy/release.sh" "$NEXT"; then
  echo "ship.sh: release.sh FAILED after the push. The v$NEXT commit is already on origin/main." >&2
  echo "Recover by re-running the publish step directly (do NOT re-run ship.sh — it would bump again):" >&2
  echo "  ./deploy/release.sh $NEXT" >&2
  exit 1
fi

echo
echo "shipped v$NEXT — open the Agenda or Reader on the device to pull it over WiFi (or just wait for"
echo "the update timer). No USB needed; 'selfupdate --force' is only for reinstalling the same version."

# Optional: apply right now over USB, if the device happens to be plugged in. A plain (non-force)
# selfupdate is enough because $NEXT is strictly newer than what's installed.
if [ "$NOW" = "1" ]; then
  if ssh -o ConnectTimeout=3 rmpp true 2>/dev/null; then
    echo "==> --now: device reachable over USB — applying immediately"
    ssh rmpp '/home/root/secondbrain/bin/selfupdate' || echo "(update check failed; the device will retry on its own)"
  else
    echo "==> --now: device not reachable over USB (rmpp) — it will self-update over WiFi anyway"
  fi
fi
