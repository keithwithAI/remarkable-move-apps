# ARCHITECTURE.md — reMarkable Paper Pro Move second brain

This records the hardware limits that shape the project, the stack choices and why they were
made, and the code structure. `SETUP.md` has the install steps.

---

## 1. Hardware reality that constrains everything

| Fact | Consequence for design |
|---|---|
| 7.3" **1696×954** Canvas Color (E Ink Gallery 3) | Render target is 1696×954 (suspend) / 954×1696 portrait (planner). **The pipeline is RGB end-to-end**: the qtfb compositor takes RGB888 and Qt mediates the color waveforms, so the planner and sleep screen carry the scheduler's priority palette. Gallery 3 mutes colors — tint strength is one constant (`render::TINT_WASH`) calibrated via the spike's color strip; everything stays legible in pure greyscale too (black text, P-tags). |
| NXP i.MX93, **1.7 GHz dual Cortex‑A55, 2 GB RAM, 64 GB** | Modest. Favor a lean static binary; avoid Electron/Chromium/heavy Qt. |
| **Encrypted device; `xochitl` is a required compositor** | You **cannot** stop xochitl and draw to `/dev/fb0` like an rM2. On‑screen apps go through **XOVI → AppLoad → qtfb**. |
| **OS updates wipe everything outside `/home/root`** and re‑enable the updater | All our code + data live in `/home/root/secondbrain/`; system touch‑points are re‑applied by an idempotent `install.sh`. Auto‑updates disabled. |
| Dev mode requires a **one‑time factory reset** | Backups before enablement (see `SETUP.md`); after that, dev mode persists. |
| Often offline (phone hotspot) | Everything is **offline‑first**: cache last good state, queue writes, sync on connect. |

---

## 2. Core principles

1. **Isolate the device/display layer behind a trait.** It is the most Paper‑Pro‑specific code
   and the most likely to break on a firmware change — quarantine it in one crate (`device`).
2. **Idempotent & reinstallable.** `install.sh` is safe to re‑run after every OS update.
3. **Offline‑first.** Cache + queue + sync‑on‑connect everywhere a network is touched.
4. **Sources of truth stay external:** the `.md` files (vault), Todoist, and Google Calendar.
   We never invent a competing store and never touch reMarkable's proprietary `.rm` format.
5. **Port rather than depend.** The `==highlight==` rules from `reading-highlighter` are
   re-implemented in Rust instead of being pulled in at runtime.

---

## 3. The stack — and why

| Layer | Choice | Why (and what we rejected) |
|---|---|---|
| On‑device language | **Rust** (static `aarch64`) | One binary in `/home/root`, zero interpreter to install/maintain, survives OS updates. Rejected Python (not preinstalled; heavier to keep alive across updates). |
| Calendar/Todoist | **On‑device Rust, direct to Google + Todoist** | Google via **service account** (no OAuth dance); Todoist via **API v1** personal Bearer token. |
| Vault sync | **Git** | Atomic commits = native offline queue + explicit conflict handling + history; safest for *writing highlights back*. Rejected Dropbox (last‑write‑wins can drop a highlight) and WebDAV (server to run). |
| On‑screen app (Phase 3/4B) | **XOVI + AppLoad, native via `external.manifest.json` + qtfb** | The proven path on the Move (KOReader uses it). AppLoad's external‑manifest mode runs a **non‑Qt Rust binary** with framebuffer access (`"qtfb": true` + `QTFB_KEY`) — so no Qt SDK. Rejected libremarkable‑direct‑fb (no confirmed Move support; can't bypass the compositor). |
| Suspend screen | **`SleepScreenPath` in `xochitl.conf` → `/home/root/...png`** | Update‑safe (config + image both in `/home/root`). Works on OS 3.2–3.13 / 3.20+. Fallback: patch read‑only `/usr/.../suspended.png` (wiped by updates). Rendered **portrait 954×1696** as a "day rail" timetable (orientation `[verify on device]`, `--width/--height` override). |
| Typography | **IBM Plex** (OFL, embedded): Serif Bold (sleep-screen date numeral), Sans/SemiBold (titles/labels), Mono (all time/status figures) | One deliberate type system across sleep screen + planner; timetable voice (aligned mono figures). Icon glyphs are drawn primitives, so no font must carry ↻⟳✓. |
| Tool self‑update | **GitHub Releases → `selfupdate`, sha256‑verified binary swap** | Update our *own* binaries over WiFi without USB — a userspace swap of oneshot‑service binaries, not firmware, so low‑risk + reversible (`.bak` rollback). Rejected an OTA image scheme (that's the OS's job, and it's the thing we *disable*). sha256 from a published `manifest.json` is the integrity gate since this runs as root. |
| Kindle books | **Embedded WPE WebKit 2.38** (`browserapp`) — the device WPE runtime app‑owned under `/home/root/secondbrain/wpe/`, rendered off‑screen to SHM and blitted via qtfb | Kindle offers no export/API; Cloud Reader is a website and needs a real browser engine. WPE is the embedded WebKit (KOReader‑adjacent ecosystem, Debian arm64 prebuilts → no compile). Software rendering only (no GPU); a desktop‑Safari UA gets the full reader. See §6b. |

---

## 4. Execution contexts

- **Mac (host):** one‑time Google **service‑account** + Todoist **token** setup; all dev and
  tests; `aarch64` cross‑compile; **PNG preview** of the agenda. ~90% of Phases 1/2/4 are
  built and verified here before any hardware exists.
- **Device:** static Rust binaries under `/home/root/secondbrain/`, driven by **systemd timers/
  services**. The Phase 3/4B on‑screen app additionally runs under **XOVI + AppLoad**.
- **No external server dependency**.

---

## 5. Code structure — Rust Cargo workspace

```
secondbrain/                # /home/root/secondbrain on device
├── crates/
│   ├── core        # sb-core: Event/Task, AgendaState (day) + PlannerState (multi-day window),
│   │               #   offline caches (atomic JSON), config/creds loader
│   ├── sources     # gcal (service account) + todoist (v1 Bearer: fetch, close, defer) clients
│   ├── render      # agenda → greyscale PNG; pub `draw` primitives shared with the planner UI
│   ├── device      # ★ suspend-screen isolation — SuspendScreen trait (Config/Sysfs/Mock)
│   ├── syncqueue   # Git vault sync (vendored libgit2): offline queue, conflict = keep-both
│   ├── highlight   # ==highlight== insertion (Phase 3 logic, ported + tested)
│   ├── actionqueue # offline Todoist write queue: enqueue-persist, flush/retry/park, overlays
│   ├── ui          # ★ pure planner app (Elm-style): model/update/view; HitMap from layout
│   ├── qtfb        # ★ clean-room qtfb protocol client (proto = pure; transport Linux-only)
│   ├── reader      # ★ Markdown reader/highlighter (Phase 3): parse→layout→paint, byte-span map
│   ├── habit       # habit tracker model (pure Elm-style)
│   ├── onething    # One Thing focus timer + Todoist one-task picker (pure Elm-style)
│   ├── recipe      # recipe viewer model (pure Elm-style)
│   └── imagecache  # shared image cache (keying + fetch); reader fetches at read time, vaultsync
│                   #   pre-warms at sync time so the reader opens fully offline (images too)
├── bin/
│   ├── agenda      # Phase 1/4A: fetch → render → suspend screen; also flushes actionqueue
│   ├── vaultsync   # Phase 2: git pull/commit/push (systemd timer); after a pull, pre-warms every
│   │               #   article/book image into the reader's cache (best-effort) → offline images
│   ├── selfupdate  # WiFi self-update from GitHub Releases (systemd timer)
│   ├── agendaapp   # Phase 4B: the interactive Planner (AppLoad external app / Mac sim)
│   ├── reader-app  # Phase 3: the interactive Reader (AppLoad external app / Mac sim)
│   ├── browserapp  # the Kindle Browser: embedded WPE WebKit + warm daemon (see §6b)
│   ├── habitapp / onethingapp / recipeapp   # the smaller AppLoad apps
│   └── qtfbspike   # D4 hardware gate (throwaway): grid + touch-dot echo + event log
├── deploy/
│   ├── install.sh  # idempotent installer + reinstall hook (binaries, systemd, AppLoad entries)
│   ├── ship.sh     # (host) ONE command: version bump → build (incl. browserapp) → push → release
│   ├── release.sh  # (host) cross-build aarch64 → manifest.json (sha256s) → gh release create
│   ├── build-browserapp.sh · wpe-setup.sh · wpe/ · browser-launch.sh   # browser build + runtime
│   ├── appload/    # external.manifest.json entries → /home/root/xovi/exthome/appload
│   └── systemd/    # agenda, vaultsync, secondbrain-update .service/.timer units
└── config/         # service-account.json, todoist.token, git key, github.token (chmod 600)
```

### The display‑isolation interfaces (the parts that would break on firmware changes)

```rust
// crate: device — suspend screen (Phase 1/4A).
pub trait SuspendScreen {
    fn install(&self, png: &GrayImage /* 1696×954 */) -> anyhow::Result<()>;
}
//   ConfigSuspend  → /home/root/secondbrain/agenda.png + SleepScreenPath in xochitl.conf (preferred)
//   SysfsSuspend   → remounts rw, replaces /usr/share/remarkable/suspended.png            (fallback)
//   MockSuspendScreen → writes the PNG to disk on the Mac.

// bin/agendaapp — live drawing + input (Phase 4B); bin/reader-app is the analogous Phase 3 Reader runtime.
pub trait Backend {
    fn present(&mut self, frame: &GrayImage, dirty: Dirty) -> Result<()>;
    fn poll(&mut self, timeout: Duration) -> Result<BackendEvent>;   // → Tap { x, y } | Quit
}
//   QtfbBackend (device) → crates/qtfb: SEQPACKET socket /tmp/qtfb.sock + mmap'd shm.
//     KEY FACT: touch/pen input arrives as MESSAGE_USERINPUT on the SAME socket, already in
//     framebuffer pixels — no evdev. Waveform control via SET_REFRESH_MODE; periodic
//     REQUEST_FULL_REFRESH purges ghosting (full refresh every 8 partials).
//   SimBackend (Mac)     → minifb window at half scale, mouse taps.
//   PngBackend (any)     → PNG per frame + scripted taps (headless snapshot tests).
```

Everything above these two seams — including the entire `ui` crate (layout, hit-testing,
defer/complete state machine) — is pure and tested on the Mac. The qtfb *protocol* codec is
byte-level and unit-tested everywhere; only the ~200-line socket/mmap transport is
device-only (and already cross-compiles).

---

## 6. Per‑phase data flow

- **Phase 1 — Calendar agenda:** `sources::gcal` (service account, all calendars, today) →
  `render` (TimelineItem[] → greyscale PNG) → `device::SuspendScreen::install` →
  systemd timer (daily + on‑wake if possible). Offline → keep last PNG + stale marker.
- **Phase 2 — Vault sync:** `syncqueue` clones the vault to `/home/root/secondbrain/vault`;
  pull‑on‑connect; local edits (highlights) are commits; push; conflict = rebase, else
  keep‑both + flag (never drop a highlight).
- **Phase 3 — Reader + highlight:** *(built, shipping)* `reader` parses markdown keeping **source byte spans**
  (`pulldown-cmark`/`comrak`) → layout records **each glyph's source range** → qtfb blit.
  Pen swipe → hit‑test glyph rects → source char range → **`==` insertion (ported from
  `reading-highlighter`)** → write source `.md` → `syncqueue` commits. Owns the pen.
  **Fully offline** (v0.21.2+): text, highlights, and notes are all local (Markdown files,
  local‑first writes that sync on connect). Images were the one read‑time network dependency —
  fetched from their `http` URL and cached on disk (`cache/images/<sha256(url)>`); now `vaultsync`
  **pre‑warms** that same cache after each pull (shared `imagecache` crate = identical keying),
  so an offline open shows images too. Dead/slow image URLs get a dated skip‑marker
  (`imagecache` negative cache, 7‑day cooldown, v0.21.3) so the pre‑warm doesn't re‑hammer them
  every sync; the read‑time fetch still always retries so a transient miss loads on next open.
- **Phase 4 — Todoist:** **A (built):** `sources::todoist` (v1 `GET /tasks?filter=today`)
  feeds the **same** `render` model → combined agenda+tasks suspend screen. **B (built — the
  Planner app):** see below.
- **Phase 4B: Planner app (`agendaapp`).** The device fetches a window of calendar events
  (today-1 to today+7) through `gcal::window_range`. An event becomes a checkable task block when
  its description has a Todoist task link and the marker written by the Diarized scheduler,
  `Auto-scheduled by Diarized (task:<uuid>)`. `parse_events` reads only that marker and the Todoist
  ID, plus `colorId` for the tint. Every other event is a plain row. Todoist tasks are fetched as a
  metadata index for the recurring flag, priority fallback and defer guard.
  `ui::view` renders one merged list per day. A checkbox hides the row at once and queues
  `actionqueue::Complete` and `actionqueue::DeleteEvent`. Tapping a row opens an action card with
  complete, defer, priority and delete; defer is hidden for recurring or unlinked tasks.
  All writes go to Todoist and Google Calendar. The queue persists on enqueue, flushes on a worker
  thread (every 30 s, or when nudged), parks permanent failures in `actions.failed.json`, and is
  shared with the `agenda` timer, so the sleep screen and the app agree about completed tasks. Fetch
  failures fall back to the cached window, marked `stale`.

---

## 6b. The Kindle Browser (`browserapp`) — the one non-Rust-rendered surface

Everything else draws its own pixels; Kindle books need a real browser engine (there's no
export or API, and Cloud Reader is a website). Architecture, host→device:

- **Engine:** a ~600-line C shim (`bin/browserapp/engine/wpeengine.c`) embeds **WPE WebKit
  2.38** — a GLib main loop on its own thread owns the `WebKitWebView`; frames export via
  `wpebackend-fdo`'s SHM path (software rendering — no GPU) and are copied under a mutex for
  the Rust side to `take_frame`. Persistent cookies keep the Amazon login; a desktop-Safari UA
  gets the full reader; a user stylesheet kills CSS animations (they flood e-ink refresh).
- **Native density:** the view runs at device scale (1.8 portrait / 2.0 landscape) so Kindle
  rasterizes page bitmaps 1:1 with the panel — the grainy-text fix. The logical (CSS) width is
  the panel edge ÷ scale and must divide integrally.
- **Present pipeline (Rust, `bin/browserapp/src/main.rs`):** poll frames → sparse frame-hash
  change detection → present ONCE on settle (a page turn arrives as a burst of intermediates;
  we show the last one crisp) → periodic deep de-ghost. The bottom bar is the shared
  `render::ui` recipe: ↺ reload · ⟲/⟳ rotate · ⭲ exit.
- **Warm daemon:** the booted page IS the cache. One binary, three modes — `--daemon` owns
  WebKit and stays resident across app opens (setsid, so AppLoad teardown can't reach it);
  `--attach` is the tiny foreground client AppLoad watches, handing the daemon its framebuffer
  key; no flag = legacy single-process fallback. Tap ⭲ detaches warm (reopen ~200ms vs ~13s
  cold); hold ⭲ quits for real (self-updates apply then). `deploy/browser-launch.sh`
  orchestrates attach → spawn → attach → fallback.
- **Rotation** is manual (⟲/⟳; an accelerometer auto-rotate shipped and was pulled): persist →
  live resize + re-scale → masked phase where only a "Rotating…" card is on screen → reflow
  VERIFIED by a JS probe (layout viewport + widest canvas) → synthetic "poke" taps (Kindle
  idles on a loading view after a reflow until interacted with) → one full refresh onto the
  finished page (~5s). Fallbacks: reload behind the card, then restart.
- **Pen = highlighter**, mirroring the Reader: Kindle's web reader only starts selection from a
  stationary long-press, so a gate synthesizes one (anchor-pin the Down, then stream the drag)
  — a pen contact can never complete as a click, so it can never page-turn.
- **Build/runtime:** browserapp links WPE, which the cross toolchains lack — it builds NATIVELY
  (aarch64) in the `wpe-build` Debian container (`deploy/build-browserapp.sh`; `ship.sh` runs
  it too). On-device runtime libs are app-owned under `/home/root/secondbrain/wpe/`
  (`deploy/wpe-setup.sh`, Debian arm64 prebuilts), so OS updates can't wipe them.

---

## 7. Reuse map (port, don't depend)

| Source | Ported into | What transfers / what doesn't |
|---|---|---|
| `reading-highlighter` `src/main.ts` (TS, MIT) | `reader`'s `==` writer | **Transfers:** the `==`‑insertion rules — multi‑paragraph, preserve bold/italic/code/links, undo. **Doesn't:** Obsidian‑DOM selection→source mapping (we build the glyph→source map ourselves). Keeping identical syntax makes on‑device and Obsidian highlights byte‑identical → clean Git merges. |

---

## 8. Persistence & OS‑update survival

- Everything we own is under `/home/root/secondbrain/` → survives updates.
- `install.sh` (idempotent) re‑applies the system touch‑points that updates wipe: systemd unit
  files, the `xochitl.conf` `SleepScreenPath` line, and (Phase 3) the XOVI/AppLoad hooks.
- Run `install.sh` once at setup and again after any OS update (it's also runnable via
  reManager). Auto‑updates are disabled to minimize this.

### Self‑update over WiFi (after the first install)

We disable the *OS* updater, but our *own* tools still need to move forward without dragging out
the USB cable each time. `selfupdate` (run daily by `secondbrain-update.timer`) does a userspace
binary swap — not a firmware flash — so it's low‑risk and reversible:

1. **Publish (host):** `deploy/release.sh <ver>` cross‑builds the `aarch64` binaries, writes a
   `manifest.json` = `{version, binaries:[{name, sha256}]}`, and `gh release create`s a `v<ver>`
   release with the binaries + manifest as assets.
2. **Pull (device):** `selfupdate` reads the installed `VERSION`, queries the release
   (`channel = latest` or a pinned tag), and if newer downloads each asset and **verifies its
   sha256 against the manifest before anything touches disk** (the integrity gate — this binary
   runs as root and installs root‑run code).
3. **Swap + self‑heal:** atomic rename into `bin/` keeping a `.bak`, then `<bin> --version` as a
   smoke test; **any** failure rolls *all* binaries back to their `.bak`. On success it writes the
   new `VERSION` and restarts the timers. Offline → exits non‑zero, next timer retries. Everything
   lives in `/home/root`, so a self‑update survives an OS update just like the rest.

---

## 9. Security

- Secrets (`service-account.json`, `todoist.token`, Git deploy key)
  live in `/home/root/secondbrain/config/`, `chmod 600`, **never committed, never hardcoded**.
- Tokens live on the device. Scope each one minimally: the Google service account to the calendars
  it needs, and the Git key to the single vault repo. Self-update needs no token on a public repo.
- Self-update is off by default. Binaries are checked against the release's `manifest.json` before
  install and smoke-tested with rollback afterwards. That catches a corrupt download, but the
  manifest isn't signed, so it doesn't protect against a compromised release.

---

## 10. Risks (see `SETUP.md` for the brick/data‑loss detail)

Factory‑reset data loss (back up first) · OS updates wiping installs (reinstall hook) ·
XOVI hooks destabilizing `xochitl` (keep removable, SSH recovery) · PIN‑over‑suspend unknown
(doc fallback) · color/waveform unknowns (greyscale baseline).
