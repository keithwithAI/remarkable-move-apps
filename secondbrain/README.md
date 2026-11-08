# secondbrain — Rust workspace

On-device tools for the reMarkable Paper Pro Move. Deploys to `/home/root/secondbrain`.
See the project root `ARCHITECTURE.md` for the why; this file is how to build, test, and deploy.

## Crates

| Crate | Role | Phase |
|---|---|---|
| `core` (`sb-core`) | Event/Task, `AgendaState` (day) + `PlannerState` (multi-day), caches, config | 1–4 |
| `sources` | Google Calendar (service account) + Todoist (API v1: fetch/close/defer) | 1, 4 |
| `render` | Portrait 954×1696 "day rail" sleep screen; IBM Plex type system + `draw` primitives | 1, 4 |
| `device` | Suspend-screen install behind the `SuspendScreen` trait | 1, 4 |
| `syncqueue` | Git vault sync (vendored libgit2): commit/pull/push, offline queue | 2 |
| `highlight` | `==highlight==` insertion, ported from `reading-highlighter` | 3 |
| `reader` | Markdown reader/highlighter: parse→layout→paint (Elm-style), byte-span glyph map, pen hit-testing | 3 |
| `imagecache` | Shared image cache (URL→`sha256`→disk): reader fetches at read time, `vaultsync` pre-warms at sync time + negative-caches dead URLs → fully offline reading incl. images | 3 |
| `actionqueue` | Offline Todoist write queue (complete/defer): retry, park, overlays | 4B |
| `ui` | Pure planner app: Elm-style model/update/view, pixel-true hit-testing | 4B |
| `qtfb` | Clean-room qtfb compositor client (protocol pure; transport Linux-only) | 4B, 3 |
| `habit` | Habit tracker model (pure Elm-style) | later |
| `onething` | One Thing focus timer + Todoist one-task picker (pure Elm-style) | later |
| `recipe` | Recipe viewer model (pure Elm-style) | later |
| `bin/agenda` | fetch → render → install suspend screen; flushes the action queue | 1, 4A |
| `bin/vaultsync` | one Git sync cycle; pre-warms article/book images into the reader's cache after a pull | 2 |
| `bin/selfupdate` | WiFi self-update from GitHub Releases (sha256-verified, rollback) | all |
| `bin/agendaapp` | **the interactive Agenda** (AppLoad external app; Mac sim + headless) | 4B |
| `bin/reader-app` | **the interactive Reader** (AppLoad external app; Mac sim + headless) | 3 |
| `bin/browserapp` | **the Kindle Browser** — embedded WPE WebKit running Kindle Cloud Reader (warm daemon, pen highlighting, ⟲/⟳ rotation). Device-only; builds in the `wpe-build` container | later |
| `bin/habitapp` · `bin/onethingapp` · `bin/recipeapp` | the Habit / One Thing / Recipe AppLoad apps | later |
| `bin/qtfbspike` | D4 hardware gate (throwaway, local-only — not on AppLoad): grid + touch dots + color strip | spike |

Both surfaces share one type system — **IBM Plex** (OFL; `assets/fonts/`, license included):
Serif Bold for the sleep screen's date numeral, Sans/SemiBold for titles, Mono for every time
figure. Icon glyphs are drawn primitives, so no font has to carry ↻⟳✓.

The Agenda app (v2) shows **one merged calendar per day**: Task blocks from the Diarized scheduler
(recognized by the Todoist link in the event description) are checkable and washed in their
Eisenhower priority color — P1 Tomato, P2 Tangerine, P3 Banana, P4 Sage; plain events stay
neutral. Checkbox completes; tapping the row opens an action card (Complete ✓ / quick-pick
defer / Cancel — defer hidden for recurring or unlinked tasks). Everything renders RGB
end-to-end; `render::TINT_WASH` is the one knob to tune once the Gallery 3 panel is seen.

## Build & test (on the Mac)

```sh
cargo test --workspace      # ~378 tests (browserapp's own tests only compile on Linux —
                            # run them inside the wpe-build container, see below)
# Preview the sleep-screen agenda with sample data (no network/credentials):
cargo run -p agenda -- --demo --out agenda-preview.png
```

**Drive the Agenda app in a window** (mouse = finger, Esc quits; half-scale 477×848 window):

```sh
cargo run -p agendaapp -- --demo                 # sample data
cargo run -p agendaapp -- --config config/config.toml   # real data (uses cache/actions.json)
```

**Headless snapshot run** (a PNG per frame; taps from a script, one `x,y` per line):

```sh
printf '500,314\n477,728\n' > /tmp/taps.txt
cargo run -p agendaapp -- --demo --frames-dir /tmp/frames --script /tmp/taps.txt
```

**Drive the Reader** (same backends — a Mac window, or headless PNG frames over a vault of `.md`):

```sh
cargo run -p reader-app -- --dir <vault-dir>                      # window over your clippings
# headless: a PNG per frame; taps + `pen x1,y1 x2,y2` highlight-drags from a script
cargo run -p reader-app -- --dir <vault-dir> --frames-dir /tmp/r --script taps.txt
```

Real timer-path run (needs `config/config.toml` — copy from `config/config.example.toml`):

```sh
cargo run -p agenda -- --config config/config.toml --out preview.png   # preview, don't install
cargo run -p vaultsync -- --config config/config.toml -m "sync highlights"
```

## Cross-compile for the device (aarch64)

Install `zig` and `cargo-zigbuild` (both are in Homebrew). The whole
workspace except `browserapp` — rustls, vendored libgit2, the qtfb backend — builds for the device target:

```sh
rustup target add aarch64-unknown-linux-gnu
cargo zigbuild --release --target aarch64-unknown-linux-gnu
```

(`cross` via Docker remains an alternative.) Stage binaries for the installer:

```sh
mkdir -p dist
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
for b in agenda vaultsync selfupdate agendaapp reader-app habitapp recipeapp onethingapp; do
  cp "$TARGET_DIR/aarch64-unknown-linux-gnu/release/$b" dist/   # 8 of the 9 self-updated binaries
done
# browserapp is the 9th — it links WPE WebKit, which the cross toolchains don't have, so it
# builds NATIVELY (aarch64) in the `wpe-build` Debian container instead:
#   see "The Kindle Browser" below for the container, then: ./deploy/build-browserapp.sh
# (qtfbspike is the throwaway D4 spike — stage it separately if needed; it's outside the manifest)
```

(Or just run `deploy/release.sh <version>` — see **WiFi self-update** below — which builds, stages
`dist/`, and publishes a GitHub Release in one step.)

> **Confirmed on hardware:** the device's glibc matches the `gnu` target — every release since
> v0.1.0 has shipped `aarch64-unknown-linux-gnu` binaries that run on the panel.

## Deploy to the device

```sh
scp -r ../secondbrain root@10.11.99.1:/home/root/
ssh root@10.11.99.1 'sh /home/root/secondbrain/deploy/install.sh'
```

`install.sh` is idempotent and **re-runnable after every OS update** (it re-applies the systemd
units that updates wipe from `/etc`). It installs `agenda.timer` (05:30/12:30/18:30 + boot),
`vaultsync.timer` (every 20 min), and optionally `secondbrain-update.timer` (daily 04:00 + 5 min after boot),
sets secret perms to 600, writes a `VERSION` file, and turns off the OS's own auto-updates.
The update timer is enabled only when `[update] auto = true`.
Once XOVI + AppLoad are installed it also drops the **Agenda** and **Reader** launcher
entries (each with its `icon.png`) into `/home/root/xovi/exthome/appload/` (re-run it after
installing AppLoad). The Agenda app is launched from the AppLoad launcher — it has no systemd
unit; it shares `config.toml`, `cache/planner.json`, and `cache/actions.json` with the timer binaries.

One-time extras: [`deploy/reader-sync-setup.md`](deploy/reader-sync-setup.md) wires the Reader's
highlights back into Obsidian (shared Git vault); `deploy/wpe-setup.sh` installs the Browser's
WPE runtime under `/home/root/secondbrain/wpe/`.

## WiFi self-update (GitHub Releases)

Self-update is off by default; set `[update] auto = true` in `config.toml` and re-run `install.sh`
to turn it on. The device then updates **its own tool binaries** over WiFi — no USB, no
re-flashing the OS. It's a userspace binary swap (oneshot systemd services), not firmware, so it's
low-risk and fully reversible.

**Publish a release (on the Mac) — one command:**

```sh
./deploy/ship.sh              # commit your code first, then: bump patch, refresh Cargo.lock,
                              # commit, push, and publish the GitHub Release. (minor|major|X.Y.Z work too.)
```

> `ship.sh` also container-builds **browserapp**, so colima + the `wpe-build` container must be
> running (`colima start && docker start wpe-build`). `SKIP_BROWSERAPP=1 ./deploy/ship.sh` ships
> without it (devices keep their current browserapp) — for a quick reader/agenda patch with
> colima off.

`ship.sh` just wraps `deploy/release.sh <version>` with the version bump + git steps; run
`release.sh` directly if you'd rather bump the workspace version by hand first:

```sh
./deploy/release.sh 0.2.0     # cross-builds aarch64, writes manifest.json (sha256s), gh release create
```

Either way this builds the nine binaries, computes each one's sha256 into `dist/manifest.json`
(`{version, binaries:[{name, sha256}]}`), and uploads the binaries + manifest as assets of the
GitHub Release (`REPO` in `deploy/release.sh`, or set in the environment for a fork).

**The device picks it up on its own — no USB.** `selfupdate` runs over WiFi from *three* triggers:
the `secondbrain-update.timer` (daily 04:00 + 5 min after boot), the sleep-screen render
(05:30/12:30/18:30), and **every time you open the Agenda or Reader app**. Each run reads the
installed `VERSION`, queries the release (`channel = "latest"` or a pinned tag), and if the tag is
**strictly newer**: downloads each asset, **verifies its sha256 against the manifest before touching
disk**, swaps it in atomically (keeping a `.bak`), runs `<bin> --version` as a smoke test, and
**rolls back all binaries if any smoke test fails**. On success it writes the new `VERSION` and
restarts the timers. A failed fetch (offline) just exits non-zero; the next trigger retries. New
binaries take effect on the affected app's **next** launch.

```sh
selfupdate --config config/config.toml            # check + apply now (only pulls a *newer* release)
selfupdate --config config/config.toml --dry-run  # show what would change, touch nothing
selfupdate --config config/config.toml --force    # reinstall the SAME version (recovery / dev-deploy)
```

> A version bump is load-bearing: the auto-triggers only pull a **strictly newer** tag, so `ship.sh`
> (which increments the version) is all you need. `--force` is impatience-only — for re-flashing an
> identical version (e.g. after a dev-deploy) — and is the one path that needs a USB cable.

> Runs as root installing root-run code → the sha256 check is the integrity gate. The manifest isn't signed, so this
> doesn't protect against a compromised release. Config lives under `[update]` in `config.toml`; a
> private fork also needs a fine-grained PAT (Contents:read) named by `token_file`.

## The Kindle Browser (`bin/browserapp`)

An embedded **WPE WebKit 2.38** browser that runs Amazon's Kindle Cloud Reader full-screen —
the one binary that doesn't follow the workspace rules:

- **Builds only in the `wpe-build` container**, a native aarch64 Debian 12 container with Rust and
  `libwpewebkit-1.1-dev`, `libwpe-1.0-dev` and `libwpebackend-fdo-1.0-dev` installed. Bind-mount this
  directory at `/repo` and a work directory at `/work`, then run `./deploy/build-browserapp.sh`. Its tests run there too:
  `docker exec wpe-build sh -lc '. ~/.cargo/env && cd /repo && CARGO_TARGET_DIR=/work/target cargo test --release --locked -p browserapp'`.
- **Runtime on-device** is app-owned under `/home/root/secondbrain/wpe/` (installed once by
  `deploy/wpe-setup.sh`); the AppLoad entry runs `deploy/browser-launch.sh` (scp-deployed, not
  self-updated), which keeps a **warm daemon** resident across app opens — tap ⭲ leaves it warm
  (reopen ~200ms), hold ⭲ quits it (updates apply on the next launch).
- Feature notes: fast start deep-links the last book; pen = highlighter (anchor-synthesized
  long-press, mirrors the Reader); ⟲/⟳ bar buttons rotate portrait/landscape (masked live
  resize, verified reflow, ~5s); native-density rendering at device scale 1.8/2.0.
- Logs land in `/home/root/browser.log` (per-boot timing audit lines included); remote
  screenshot: `ssh <device> 'touch /home/root/secondbrain/browser/shot.req'` → `browser/shot.png`.

## What's device-gated

Some tuning can only be done on the panel. Gallery 3 colour calibration is by eye on the device (a headless PNG
can't show the muting): the agenda's `render::TINT_WASH`, the reader's
`imaging::LEVELS_PER_CHANNEL` / `imaging::SATURATION`, and the browser's `ink_px` vividness
knobs.
