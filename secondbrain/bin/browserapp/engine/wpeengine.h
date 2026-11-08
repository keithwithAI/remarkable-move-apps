/* wpeengine — a minimal C ABI around an embedded WPE WebKit view that renders
 * off-screen to CPU (SHM) buffers. The engine owns a GLib main loop on its own
 * thread; all entry points below are safe to call from another thread (input and
 * navigation are marshalled onto the loop; frame reads are mutex-guarded).
 *
 * Pixels are delivered as packed BGRA (4 bytes/px, the native wl_shm ARGB8888
 * byte order on little-endian), top-down, stride = width*4.
 */
#ifndef WPEENGINE_H
#define WPEENGINE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct WpeEngine WpeEngine;

/* Create an engine whose output frames are width×height PHYSICAL pixels. `scale` is the device
 * scale factor: the view's logical (CSS) size is width/scale × height/scale and the page sees
 * devicePixelRatio == scale, so content rasterizes at native panel density (scale 2 on the
 * ~264 DPI panel) instead of being bitmap-upscaled by page zoom. Pass 1.0 for the legacy 1:1
 * behaviour. `data_dir` (may be NULL) is where cookies + website data persist (the Amazon login
 * survives restarts). Returns NULL on failure. Blocks until the engine thread is ready.
 *
 * Env knobs (read once at creation):
 *   BROWSER_CONSOLE=1        page console.log → stdout (lands in the launch wrapper's log) */
WpeEngine *wpe_engine_new(uint32_t width, uint32_t height, float scale, const char *data_dir);

/* Navigation (thread-safe). */
void wpe_engine_load_uri(WpeEngine *e, const char *uri);
void wpe_engine_reload(WpeEngine *e);
/* Run JavaScript in the page (fire-and-forget; used for Kindle DOM shortcuts). */
void wpe_engine_run_js(WpeEngine *e, const char *js);
/* Run JavaScript and capture its (stringified) result: the completion stores it internally and
 * bumps a sequence number. Poll with wpe_engine_js_result — a changed seq means a new result. */
void wpe_engine_eval_js(WpeEngine *e, const char *js);
/* Copy the latest eval_js result into `buf` (NUL-terminated, truncated) and return its sequence
 * number (0 = no result yet). */
uint32_t wpe_engine_js_result(WpeEngine *e, char *buf, size_t len);
/* Full-page zoom (1.0 = 100%). Bigger text on the high-DPI panel. */
void wpe_engine_set_zoom(WpeEngine *e, double zoom);

/* Copy the latest frame into `dst` (must be width*height*4 bytes). Returns 1 if a
 * new frame was copied since the previous take, 0 otherwise (dst untouched). */
int wpe_engine_take_frame(WpeEngine *e, uint8_t *dst, size_t dst_len);

/* Input (thread-safe, marshalled onto the loop).
 * touch phase: 0=down, 1=motion, 2=up. Coordinates are view pixels. */
void wpe_engine_touch(WpeEngine *e, int phase, int32_t x, int32_t y);
/* Key: `keysym` is an X keysym (printable ASCII == codepoint; Enter=0xFF0D,
 * Backspace=0xFF08, Left=0xFF51, Right=0xFF53). `pressed`: 1=down, 0=up. */
void wpe_engine_key(WpeEngine *e, uint32_t keysym, int pressed);

/* Copy the current document URI into `buf` (NUL-terminated, truncated to len). */
void wpe_engine_current_uri(WpeEngine *e, char *buf, size_t len);
/* Estimated load progress 0..100 (100 = finished). */
int wpe_engine_load_progress(WpeEngine *e);
/* 1 after a navigation FAILED (DNS/offline/TLS); cleared when the next load starts/commits.
 * Lets the app show an honest "no connection" status and retry instead of stranding a blank
 * error page behind its loading gate. */
int wpe_engine_load_failed(WpeEngine *e);
/* 1 while an editable element is focused in the page (i.e. it wants the on-screen keyboard),
 * else 0. Driven by WebKit's input-method focus notifications. */
int wpe_engine_kb_focus(WpeEngine *e);

/* Foreground/background the view: active=0 removes the visible/focused/in-window activity
 * states so WebKit throttles rendering and timers while the app is detached from the screen
 * (the warm-daemon idle state); active=1 restores them on re-attach. Thread-safe. */
void wpe_engine_set_active(WpeEngine *e, int active);

/* Re-size/re-scale the live view (rotation): output frames become width×height PHYSICAL px at
 * the new device scale; the page reflows to the new logical size. Frames already in flight at
 * the old size are dropped by the exact-size gate. Thread-safe. Out-of-range args are ignored. */
void wpe_engine_resize(WpeEngine *e, float scale, uint32_t width, uint32_t height);

void wpe_engine_destroy(WpeEngine *e);

#ifdef __cplusplus
}
#endif
#endif /* WPEENGINE_H */
