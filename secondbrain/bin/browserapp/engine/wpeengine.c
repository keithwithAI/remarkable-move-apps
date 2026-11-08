/* wpeengine — embed a WPE WebKit view rendering off-screen to SHM buffers.
 * See wpeengine.h for the ABI contract. Implementation notes:
 *  - One GLib main loop runs on a dedicated thread; the WebKitWebView and the FDO
 *    exportable backend live entirely on that thread.
 *  - Each exported SHM frame (ARGB8888, i.e. BGRA bytes) is copied under a mutex
 *    into `frame`; wpe_engine_take_frame hands the latest to the caller.
 *  - Navigation + input are marshalled onto the loop via g_main_context_invoke.
 *  - Software rendering only (no GPU on the device): the caller's environment sets
 *    the Mesa surfaceless-swrast EGL path (see env.sh / the glvnd vendor JSON).
 */
#include "wpeengine.h"

#include <wpe/webkit.h>
#include <wpe/fdo.h>
#include <wpe/unstable/fdo-shm.h>
#include <wayland-server.h>
#include <glib.h>
#include <libsoup/soup.h>
#include <string.h>
#include <stdlib.h>

/* A desktop Safari UA so Amazon serves the full Cloud Reader (it gates on engine). */
#define DESKTOP_UA \
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 " \
    "(KHTML, like Gecko) Version/16.4 Safari/605.1.15"

struct WpeEngine {
    uint32_t w, h;       /* PHYSICAL output size (frame buffer + exact-size gate) */
    uint32_t lw, lh;     /* logical (CSS) view size = physical / scale */
    float scale;         /* device scale factor: page sees devicePixelRatio == scale */
    int log_frames;      /* log the next N exported buffer sizes (reset per expectation change) */
    int warned_mismatch; /* one throttled wrong-size-drop warning per expectation epoch */
    char *data_dir;

    GThread *thread;
    GMainContext *ctx;
    GMainLoop *loop;

    struct wpe_view_backend_exportable_fdo *exportable;
    struct wpe_view_backend *wpe_backend;
    WebKitWebView *view;

    GMutex lock;         /* guards frame/has_new/uri/progress */
    uint8_t *frame;      /* w*h*4 BGRA */
    int has_new;
    double zoom;         /* full-page zoom (layout size; density comes from `scale`) */

    char uri[2048];
    int progress;        /* 0..100 */
    int load_failed;     /* the last navigation failed (DNS/offline) — cleared when a load starts */
    int kb_focus;        /* an editable element is focused (wants the on-screen keyboard) */
    char js_result[256]; /* latest wpe_engine_eval_js result (stringified) */
    uint32_t js_result_seq; /* bumped per stored result — the caller polls for a new seq */

    /* startup handshake */
    GMutex ready_lock;
    GCond ready_cond;
    int ready;           /* 0 pending, 1 ok, -1 failed */
    char *initial_uri;   /* loaded once the view exists */
};

/* -------------------------------------------------- input-method context
 * WebKit calls notify_focus_in/out on the view's WebKitInputMethodContext when an editable
 * element gains/loses focus — the native "this field wants a keyboard" signal (how a phone
 * knows to raise its keyboard). We subclass only to capture that flag; typing itself still
 * goes through synthesized keysym events, so every other vfunc stays default/NULL. */
typedef struct {
    WebKitInputMethodContext parent;
    WpeEngine *engine;
} BrowserIMContext;
typedef struct {
    WebKitInputMethodContextClass parent;
} BrowserIMContextClass;

G_DEFINE_TYPE(BrowserIMContext, browser_im_context, WEBKIT_TYPE_INPUT_METHOD_CONTEXT)

static void browser_im_context_init(BrowserIMContext *self) { (void)self; }

static void im_set_focus(WebKitInputMethodContext *ctx, int focused) {
    WpeEngine *e = ((BrowserIMContext *)ctx)->engine;
    if (!e) return;
    g_mutex_lock(&e->lock);
    e->kb_focus = focused;
    g_mutex_unlock(&e->lock);
}
static void im_focus_in(WebKitInputMethodContext *ctx) { im_set_focus(ctx, 1); }
static void im_focus_out(WebKitInputMethodContext *ctx) { im_set_focus(ctx, 0); }

static void browser_im_context_class_init(BrowserIMContextClass *klass) {
    WebKitInputMethodContextClass *c = WEBKIT_INPUT_METHOD_CONTEXT_CLASS(klass);
    c->notify_focus_in = im_focus_in;
    c->notify_focus_out = im_focus_out;
}

/* ------------------------------------------------------------------ frames */

static void on_export_shm(void *data, struct wpe_fdo_shm_exported_buffer *buffer) {
    WpeEngine *e = data;
    struct wl_shm_buffer *shm = wpe_fdo_shm_exported_buffer_get_shm_buffer(buffer);
    if (shm) {
        wl_shm_buffer_begin_access(shm);
        uint32_t w = wl_shm_buffer_get_width(shm);
        uint32_t h = wl_shm_buffer_get_height(shm);
        uint32_t stride = wl_shm_buffer_get_stride(shm);
        const uint8_t *px = wl_shm_buffer_get_data(shm);
        /* Diagnostics: log the first few frames after every expectation change (creation or a
         * runtime resize — a frame already in flight at the OLD size arrives first, so one line
         * would mislead), and warn once per epoch when wrong-size frames are being dropped. */
        if (e->log_frames > 0) {
            e->log_frames--;
            g_message("wpeengine: SHM frame %ux%u (expect %ux%u, scale %.2f)",
                      w, h, e->w, e->h, (double)e->scale);
        }
        if (px && (w != e->w || h != e->h) && !e->warned_mismatch) {
            e->warned_mismatch = 1;
            g_message("wpeengine: dropping %ux%u frames (expect %ux%u)", w, h, e->w, e->h);
        }
        if (px && w == e->w && h == e->h) {
            g_mutex_lock(&e->lock);
            if (stride == w * 4) {
                /* The common SHM case: rows are contiguous — one memcpy, not ~1600. */
                memcpy(e->frame, px, (size_t)w * h * 4);
            } else {
                for (uint32_t y = 0; y < h; y++)
                    memcpy(e->frame + (size_t)y * w * 4, px + (size_t)y * stride, (size_t)w * 4);
            }
            e->has_new = 1;
            g_mutex_unlock(&e->lock);
        }
        wl_shm_buffer_end_access(shm);
    }
    wpe_view_backend_exportable_fdo_dispatch_release_shm_exported_buffer(e->exportable, buffer);
    wpe_view_backend_exportable_fdo_dispatch_frame_complete(e->exportable);
}

/* ------------------------------------------------------------- view signals */

static const char *load_ev_name(WebKitLoadEvent ev) {
    switch (ev) {
        case WEBKIT_LOAD_STARTED: return "started";
        case WEBKIT_LOAD_REDIRECTED: return "redirected";
        case WEBKIT_LOAD_COMMITTED: return "committed";
        case WEBKIT_LOAD_FINISHED: return "finished";
        default: return "?";
    }
}

static void on_load_changed(WebKitWebView *view, WebKitLoadEvent ev, gpointer data) {
    WpeEngine *e = data;
    const char *u = webkit_web_view_get_uri(view);
    /* Boot audit: the network phases, timestamped — separates DNS/TLS/fetch from the JS boot. */
    g_message("wpeengine: load %s → %s", load_ev_name(ev), u ? u : "?");
    g_mutex_lock(&e->lock);
    if (u) { g_strlcpy(e->uri, u, sizeof e->uri); }
    e->progress = (int)(webkit_web_view_get_estimated_load_progress(view) * 100.0);
    /* A fresh navigation clears the failure latch — the app's retry loop watches this. */
    if (ev == WEBKIT_LOAD_STARTED || ev == WEBKIT_LOAD_COMMITTED) e->load_failed = 0;
    g_mutex_unlock(&e->lock);
}

/* The engine is an old WebKit running as root with its sandbox off, so it must not wander: only
 * Amazon's own sites may load in a frame. Plain http is refused too. */
static const char *const ALLOWED_DOMAINS[] = {
    "amazon.com", "amazon.co.uk", "amazon.ca", "amazon.com.au", "amazon.de", "amazon.fr",
    "amazon.es", "amazon.it", "amazon.nl", "amazon.se", "amazon.pl", "amazon.in", "amazon.co.jp",
    "amazon.com.br", "amazon.com.mx", "amazon.sg", "amazon.ae", "amazon.sa", "amazon.com.tr",
    "media-amazon.com", "ssl-images-amazon.com", "images-amazon.com",
};

int wpe_engine_uri_allowed(const char *uri) {
    if (!uri) return 0;
    /* In-page documents have no host of their own; they belong to the page that made them. */
    if (g_str_has_prefix(uri, "about:") || g_str_has_prefix(uri, "blob:https://") ||
        g_str_has_prefix(uri, "data:"))
        return 1;
    GUri *parsed = g_uri_parse(uri, G_URI_FLAGS_NONE, NULL);
    if (!parsed) return 0;
    int allowed = 0;
    const char *scheme = g_uri_get_scheme(parsed);
    const char *raw_host = g_uri_get_host(parsed);
    if (scheme && raw_host && g_ascii_strcasecmp(scheme, "https") == 0) {
        char *host = g_ascii_strdown(raw_host, -1);
        for (gsize i = 0; i < G_N_ELEMENTS(ALLOWED_DOMAINS) && !allowed; i++) {
            const char *domain = ALLOWED_DOMAINS[i];
            gsize h = strlen(host), d = strlen(domain);
            /* Exact, or a real subdomain: "evilamazon.com" must not match "amazon.com". */
            allowed = g_str_equal(host, domain) ||
                      (h > d && host[h - d - 1] == '.' && g_str_equal(host + h - d, domain));
        }
        g_free(host);
    }
    g_uri_unref(parsed);
    return allowed;
}

static gboolean on_decide_policy(WebKitWebView *view, WebKitPolicyDecision *decision,
                                 WebKitPolicyDecisionType type, gpointer data) {
    (void)view;
    (void)data;
    if (type != WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION &&
        type != WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION)
        return FALSE;
    WebKitNavigationAction *action = webkit_navigation_policy_decision_get_navigation_action(
        WEBKIT_NAVIGATION_POLICY_DECISION(decision));
    const char *uri = webkit_uri_request_get_uri(webkit_navigation_action_get_request(action));
    if (wpe_engine_uri_allowed(uri)) return FALSE;
    g_message("wpeengine: blocked navigation to %s", uri ? uri : "?");
    webkit_policy_decision_ignore(decision);
    return TRUE;
}

static void on_progress(GObject *obj, GParamSpec *ps, gpointer data) {
    WpeEngine *e = data;
    (void)ps;
    double p = webkit_web_view_get_estimated_load_progress(WEBKIT_WEB_VIEW(obj));
    g_mutex_lock(&e->lock);
    e->progress = (int)(p * 100.0);
    g_mutex_unlock(&e->lock);
}

/* SPA navigation (pushState — how Kindle moves between library and book) never fires load-changed;
 * notify::uri catches every URL change, so current_uri() tracks the real location. That is what the
 * last-book deep-link persists. */
static void on_uri_changed(GObject *obj, GParamSpec *ps, gpointer data) {
    WpeEngine *e = data;
    (void)ps;
    const char *u = webkit_web_view_get_uri(WEBKIT_WEB_VIEW(obj));
    if (!u) return;
    g_mutex_lock(&e->lock);
    g_strlcpy(e->uri, u, sizeof e->uri);
    g_mutex_unlock(&e->lock);
    if (g_getenv("BROWSER_CONSOLE")) g_message("wpeengine: uri → %s", u);
}

/* If the web process crashes, WebKit stops producing frames and the browser freezes with no way
 * back. Reload the view so it respawns the process and re-renders. Runs on the loop thread. */
static void on_web_process_terminated(WebKitWebView *view, WebKitWebProcessTerminationReason reason,
                                      gpointer data) {
    (void)data;
    g_warning("wpeengine: web process terminated (reason %d) — reloading", (int)reason);
    webkit_web_view_reload(view);
}

/* A failed navigation can leave load-progress stuck below 100, which would otherwise keep the app's
 * initial-load wait armed. Force progress to 100 so the UI unblocks; let WebKit show its error page. */
static gboolean on_load_failed(WebKitWebView *view, WebKitLoadEvent ev, const char *failing_uri,
                               GError *error, gpointer data) {
    WpeEngine *e = data;
    (void)view;
    (void)ev;
    g_warning("wpeengine: load failed for %s: %s", failing_uri ? failing_uri : "?",
              error ? error->message : "?");
    g_mutex_lock(&e->lock);
    e->progress = 100;
    e->load_failed = 1; /* the app shows "No connection — retrying…" and re-issues the load */
    g_mutex_unlock(&e->lock);
    return FALSE;
}

/* Resource-load audit (BROWSER_NETLOG=1): for every subresource the page loads, log the response
 * status, mime, byte size, and cache-control header alongside the (truncated) URL. Purpose is the
 * offline-book feasibility spike — it shows whether book CONTENT is delivered as cacheable resources
 * with stable URLs (replayable offline) or one-off signed/tokened blobs behind a live licence call.
 * Off unless BROWSER_NETLOG is set; rides the BROWSER_CONSOLE/g_message log. */
static void on_resource_finished(WebKitWebResource *resource, gpointer data) {
    (void)data;
    const char *uri = webkit_web_resource_get_uri(resource);
    WebKitURIResponse *resp = webkit_web_resource_get_response(resource);
    if (!resp) {
        g_message("wpeengine: netlog (no-response) %.200s", uri ? uri : "?");
        return;
    }
    guint status = webkit_uri_response_get_status_code(resp);
    const char *mime = webkit_uri_response_get_mime_type(resp);
    guint64 len = webkit_uri_response_get_content_length(resp);
    const char *cc = "-";
    SoupMessageHeaders *hdrs = webkit_uri_response_get_http_headers(resp);
    if (hdrs) {
        const char *v = soup_message_headers_get_one(hdrs, "cache-control");
        if (v) cc = v;
    }
    g_message("wpeengine: netlog %u %s len=%" G_GUINT64_FORMAT " cc=[%s] %.200s",
              status, mime ? mime : "?", len, cc, uri ? uri : "?");
}

static void on_resource_load_started(WebKitWebView *view, WebKitWebResource *resource,
                                     WebKitURIRequest *request, gpointer data) {
    (void)view;
    (void)request;
    g_signal_connect(resource, "finished", G_CALLBACK(on_resource_finished), data);
}

/* --------------------------------------------------------------- loop thread */

static gpointer engine_thread(gpointer data) {
    WpeEngine *e = data;

    e->ctx = g_main_context_new();
    g_main_context_push_thread_default(e->ctx);
    e->loop = g_main_loop_new(e->ctx, FALSE);

    wpe_fdo_initialize_shm();

    static const struct wpe_view_backend_exportable_fdo_client client = {
        .export_shm_buffer = on_export_shm,
    };
    /* The exportable is created at the LOGICAL size; with the device scale factor dispatched below,
     * WebKit renders (and exports) buffers at logical×scale = the physical panel size. */
    e->exportable = wpe_view_backend_exportable_fdo_create(
        (struct wpe_view_backend_exportable_fdo_client *)&client, e, e->lw, e->lh);
    if (!e->exportable) goto fail;
    e->wpe_backend = wpe_view_backend_exportable_fdo_get_view_backend(e->exportable);

    /* Context with persistent cookies for the Amazon session. */
    WebKitWebContext *webctx;
    if (e->data_dir) {
        char *cookiefile = g_build_filename(e->data_dir, "cookies.sqlite", NULL);
        WebKitWebsiteDataManager *dm = webkit_website_data_manager_new(
            "base-data-directory", e->data_dir, "base-cache-directory", e->data_dir, NULL);
        webctx = webkit_web_context_new_with_website_data_manager(dm);
        WebKitCookieManager *cm = webkit_web_context_get_cookie_manager(webctx);
        webkit_cookie_manager_set_persistent_storage(cm, cookiefile,
                                                     WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE);
        webkit_cookie_manager_set_accept_policy(cm, WEBKIT_COOKIE_POLICY_ACCEPT_ALWAYS);
        g_object_unref(dm);
        g_free(cookiefile);
    } else {
        webctx = webkit_web_context_new();
    }
    /* Startup wins: resolve both Amazon hosts while the engine is still booting (the page shell
     * comes from read.amazon.*, the big JS bundles from m.media-amazon.com), and pin the most
     * aggressive cache model explicitly rather than trusting the default. */
    webkit_web_context_set_cache_model(webctx, WEBKIT_CACHE_MODEL_WEB_BROWSER);
    webkit_web_context_prefetch_dns(webctx, "read.amazon.co.uk");
    webkit_web_context_prefetch_dns(webctx, "m.media-amazon.com");

    WebKitWebViewBackend *vb = webkit_web_view_backend_new(e->wpe_backend, NULL, NULL);

    // User stylesheet: kill CSS animations/transitions and smooth scrolling. They're pointless on
    // e-ink and otherwise repaint continuously (e.g. the Amazon login's animated background), which
    // floods the e-ink refresh and makes typing feel frozen. Injected at the USER level so it wins.
    WebKitUserContentManager *ucm = webkit_user_content_manager_new();
    static const char *kill_anim_css =
        "*,*::before,*::after{"
        "animation:none!important;-webkit-animation:none!important;"
        "transition:none!important;-webkit-transition:none!important;}"
        "html{scroll-behavior:auto!important;}";
    WebKitUserStyleSheet *ss = webkit_user_style_sheet_new(
        kill_anim_css, WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES, WEBKIT_USER_STYLE_LEVEL_USER, NULL, NULL);
    webkit_user_content_manager_add_style_sheet(ucm, ss);
    webkit_user_style_sheet_unref(ss);

    // g_object_new lets us set backend + context + content-manager together (all construct-only).
    e->view = WEBKIT_WEB_VIEW(g_object_new(WEBKIT_TYPE_WEB_VIEW, "backend", vb, "web-context",
                                           webctx, "user-content-manager", ucm, NULL));
    g_object_unref(webctx);
    g_object_unref(ucm);
    if (!e->view) goto fail; /* don't signal ready with a NULL view — return a clean failure */

    WebKitSettings *s = webkit_web_view_get_settings(e->view);
    webkit_settings_set_user_agent(s, DESKTOP_UA);
    webkit_settings_set_enable_developer_extras(s, FALSE);
    /* BROWSER_CONSOLE=1 routes page console.log to stdout (→ the wrapper's log file) — used by the
     * Rust side's BROWSER_PROBE diagnostics; off by default to avoid Amazon's console chatter. */
    webkit_settings_set_enable_write_console_messages_to_stdout(
        s, g_getenv("BROWSER_CONSOLE") ? TRUE : FALSE);
    /* No GPU: WebGL runs on llvmpipe here, so a WebGL probe/context costs software shader compiles
     * and can steer Kindle's renderer onto a slow GL path — the canvas-2D/DOM path is the fast one
     * on this device. BROWSER_WEBGL=1 restores it if a page ever needs it. WebAudio likewise off. */
    webkit_settings_set_enable_webgl(s, g_getenv("BROWSER_WEBGL") ? TRUE : FALSE);
    webkit_settings_set_enable_webaudio(s, FALSE);
    if (e->zoom > 0.0) webkit_web_view_set_zoom_level(e->view, e->zoom);

    /* Order matters (mirrors Cog): scale first, then the LOGICAL size — both after the view exists,
     * since libwpe drops dispatches until WebKit registers its backend client. */
    wpe_view_backend_dispatch_set_device_scale_factor(e->wpe_backend, e->scale);
    wpe_view_backend_dispatch_set_size(e->wpe_backend, e->lw, e->lh);
    /* Without an activity state WebKit treats the view as hidden — it renders the
     * first paint but then ignores input and stops producing frames. Mark it live. */
    wpe_view_backend_add_activity_state(e->wpe_backend,
        wpe_view_activity_state_visible | wpe_view_activity_state_focused |
        wpe_view_activity_state_in_window);

    g_signal_connect(e->view, "decide-policy", G_CALLBACK(on_decide_policy), e);
    g_signal_connect(e->view, "load-changed", G_CALLBACK(on_load_changed), e);
    g_signal_connect(e->view, "notify::uri", G_CALLBACK(on_uri_changed), e);
    g_signal_connect(e->view, "notify::estimated-load-progress", G_CALLBACK(on_progress), e);
    g_signal_connect(e->view, "web-process-terminated", G_CALLBACK(on_web_process_terminated), e);
    g_signal_connect(e->view, "load-failed", G_CALLBACK(on_load_failed), e);
    if (g_getenv("BROWSER_NETLOG"))
        g_signal_connect(e->view, "resource-load-started",
                         G_CALLBACK(on_resource_load_started), e);

    /* Focus-driven keyboard: the view takes its own ref on the IM context. */
    BrowserIMContext *im = g_object_new(browser_im_context_get_type(), NULL);
    im->engine = e;
    webkit_web_view_set_input_method_context(e->view, WEBKIT_INPUT_METHOD_CONTEXT(im));
    g_object_unref(im);

    if (e->initial_uri) webkit_web_view_load_uri(e->view, e->initial_uri);

    /* Signal ready. */
    g_mutex_lock(&e->ready_lock);
    e->ready = 1;
    g_cond_signal(&e->ready_cond);
    g_mutex_unlock(&e->ready_lock);

    g_main_loop_run(e->loop);

    g_main_context_pop_thread_default(e->ctx);
    return NULL;

fail:
    g_mutex_lock(&e->ready_lock);
    e->ready = -1;
    g_cond_signal(&e->ready_cond);
    g_mutex_unlock(&e->ready_lock);
    return NULL;
}

/* ---------------------------------------------------------------- lifecycle */

WpeEngine *wpe_engine_new(uint32_t width, uint32_t height, float scale, const char *data_dir) {
    WpeEngine *e = g_new0(WpeEngine, 1);
    e->w = width; e->h = height;
    if (!(scale >= 1.0f && scale <= 4.0f)) scale = 1.0f; /* also catches NaN */
    e->scale = scale;
    e->lw = (uint32_t)((double)width / scale + 0.5);
    e->lh = (uint32_t)((double)height / scale + 0.5);
    e->log_frames = 3;
    e->data_dir = data_dir ? g_strdup(data_dir) : NULL;
    e->frame = g_malloc0((size_t)width * height * 4);
    e->progress = 100;
    g_mutex_init(&e->lock);
    g_mutex_init(&e->ready_lock);
    g_cond_init(&e->ready_cond);

    e->thread = g_thread_new("wpeengine", engine_thread, e);

    g_mutex_lock(&e->ready_lock);
    while (e->ready == 0) g_cond_wait(&e->ready_cond, &e->ready_lock);
    int ok = e->ready;
    g_mutex_unlock(&e->ready_lock);

    if (ok != 1) { wpe_engine_destroy(e); return NULL; }
    return e;
}

/* Monotonic event timestamp in ms (WebKit orders/coalesces on this; 0 can be dropped). */
static uint32_t ev_time(void) { return (uint32_t)(g_get_monotonic_time() / 1000); }

/* Marshal a closure onto the loop thread. */
typedef struct { WpeEngine *e; void (*fn)(WpeEngine *, void *); void *arg; } Cmd;
static gboolean run_cmd(gpointer data) {
    Cmd *c = data;
    c->fn(c->e, c->arg);
    g_free(c->arg);
    g_free(c);
    return G_SOURCE_REMOVE;
}
static void invoke(WpeEngine *e, void (*fn)(WpeEngine *, void *), void *arg) {
    if (!e || !e->ctx) { g_free(arg); return; }
    Cmd *c = g_new0(Cmd, 1);
    c->e = e; c->fn = fn; c->arg = arg;
    g_main_context_invoke(e->ctx, run_cmd, c);
}

/* --------------------------------------------------------------- navigation */

static void do_load(WpeEngine *e, void *arg) { webkit_web_view_load_uri(e->view, (const char *)arg); }
void wpe_engine_load_uri(WpeEngine *e, const char *uri) {
    if (!e) return;
    if (!e->view) { /* pre-start: stash */ g_free(e->initial_uri); e->initial_uri = g_strdup(uri); return; }
    invoke(e, do_load, g_strdup(uri));
}
static void do_reload(WpeEngine *e, void *a) { (void)a; webkit_web_view_reload(e->view); }
void wpe_engine_reload(WpeEngine *e) { invoke(e, do_reload, NULL); }
static void do_js(WpeEngine *e, void *a) { webkit_web_view_run_javascript(e->view, (const char *)a, NULL, NULL, NULL); }
void wpe_engine_run_js(WpeEngine *e, const char *js) { if (e) invoke(e, do_js, g_strdup(js)); }

/* eval_js: like run_js, but the completion stores the stringified result under the lock and bumps
 * the sequence — wpe_engine_js_result() polls it. Used to verify a rotate actually reflowed the
 * page (viewport/canvas widths) rather than trusting the buffer size alone. */
static void on_js_result(GObject *src, GAsyncResult *res, gpointer data) {
    WpeEngine *e = data;
    GError *err = NULL;
    WebKitJavascriptResult *jr =
        webkit_web_view_run_javascript_finish(WEBKIT_WEB_VIEW(src), res, &err);
    if (!jr) {
        if (err) {
            g_message("wpeengine: eval_js failed: %s", err->message);
            g_error_free(err);
        }
        return;
    }
    JSCValue *v = webkit_javascript_result_get_js_value(jr);
    char *s = v ? jsc_value_to_string(v) : NULL;
    g_mutex_lock(&e->lock);
    g_strlcpy(e->js_result, s ? s : "", sizeof e->js_result);
    e->js_result_seq++;
    g_mutex_unlock(&e->lock);
    g_free(s);
    webkit_javascript_result_unref(jr);
}
static void do_eval_js(WpeEngine *e, void *a) {
    webkit_web_view_run_javascript(e->view, (const char *)a, NULL, on_js_result, e);
}
void wpe_engine_eval_js(WpeEngine *e, const char *js) { if (e) invoke(e, do_eval_js, g_strdup(js)); }

uint32_t wpe_engine_js_result(WpeEngine *e, char *buf, size_t len) {
    if (!e) return 0;
    g_mutex_lock(&e->lock);
    if (buf && len) g_strlcpy(buf, e->js_result, len);
    uint32_t seq = e->js_result_seq;
    g_mutex_unlock(&e->lock);
    return seq;
}
static void do_zoom(WpeEngine *e, void *a) {
    e->zoom = *(double *)a;
    if (e->view) webkit_web_view_set_zoom_level(e->view, e->zoom);
}
void wpe_engine_set_zoom(WpeEngine *e, double zoom) {
    if (!e) return;
    if (!e->view) { e->zoom = zoom; return; } // pre-start: applied at view creation
    double *p = g_new(double, 1);
    *p = zoom;
    invoke(e, do_zoom, p);
}

/* -------------------------------------------------------------------- frames */

int wpe_engine_take_frame(WpeEngine *e, uint8_t *dst, size_t dst_len) {
    if (!e) return 0;
    g_mutex_lock(&e->lock);
    /* `need` must be computed under the lock: a runtime resize (loop thread) swaps w/h/frame
     * together, and a stale size here would memcpy past the reallocated buffer. */
    size_t need = (size_t)e->w * e->h * 4;
    int got = 0;
    if (dst_len >= need && e->has_new) { memcpy(dst, e->frame, need); e->has_new = 0; got = 1; }
    g_mutex_unlock(&e->lock);
    return got;
}

/* Runtime re-size/re-scale (rotation): swap the expected physical dims + frame buffer under the
 * lock, then re-dispatch scale and logical size to WebKit. Runs on the loop thread, like every
 * other view mutation; on_export_shm (same thread) then gates on the NEW dims, and the one-shot
 * size log re-arms so the next exported frame reports whether WebKit honoured the resize. */
typedef struct { float scale; uint32_t w, h; } ResizeArg;
static void do_resize(WpeEngine *e, void *a) {
    ResizeArg *r = a;
    uint32_t lw = (uint32_t)((double)r->w / r->scale + 0.5);
    uint32_t lh = (uint32_t)((double)r->h / r->scale + 0.5);
    g_mutex_lock(&e->lock);
    e->w = r->w;
    e->h = r->h;
    e->scale = r->scale;
    e->lw = lw;
    e->lh = lh;
    e->frame = g_realloc(e->frame, (size_t)r->w * r->h * 4);
    e->has_new = 0;
    e->log_frames = 3;
    e->warned_mismatch = 0;
    g_mutex_unlock(&e->lock);
    wpe_view_backend_dispatch_set_device_scale_factor(e->wpe_backend, e->scale);
    wpe_view_backend_dispatch_set_size(e->wpe_backend, lw, lh);
    g_message("wpeengine: resize dispatched → logical %ux%u @ %.2f (physical %ux%u)",
              lw, lh, (double)r->scale, r->w, r->h);
}
void wpe_engine_resize(WpeEngine *e, float scale, uint32_t width, uint32_t height) {
    if (!e) return;
    if (!(scale >= 1.0f && scale <= 4.0f) || width < 100 || height < 100 || width > 4096 ||
        height > 4096)
        return;
    ResizeArg *r = g_new0(ResizeArg, 1);
    r->scale = scale;
    r->w = width;
    r->h = height;
    invoke(e, do_resize, r);
}

/* --------------------------------------------------------------------- input */

typedef struct { int phase; int32_t x, y; } TouchArg;
static void do_touch(WpeEngine *e, void *a) {
    TouchArg *t = a;
    enum wpe_input_touch_event_type ty =
        t->phase == 0 ? wpe_input_touch_event_type_down :
        t->phase == 1 ? wpe_input_touch_event_type_motion : wpe_input_touch_event_type_up;
    uint32_t tm = ev_time();
    /* Coordinates arrive in PHYSICAL panel px — WebKit's WPE event factory divides pointer/touch
     * positions by the device scale factor itself (device-verified on the shipped build). */
    int32_t x = t->x, y = t->y;
    /* Designated initializers so a libwpe struct-field reorder can't silently misassign. */
    struct wpe_input_touch_event_raw raw = { .type = ty, .time = tm, .id = 0, .x = x, .y = y };
    struct wpe_input_touch_event ev = {
        .touchpoints = &raw, .touchpoints_length = 1, .type = ty, .id = 0, .time = tm, .modifiers = 0,
    };
    wpe_view_backend_dispatch_touch_event(e->wpe_backend, &ev);
}
void wpe_engine_touch(WpeEngine *e, int phase, int32_t x, int32_t y) {
    TouchArg *t = g_new0(TouchArg, 1); t->phase = phase; t->x = x; t->y = y;
    invoke(e, do_touch, t);
}

typedef struct { uint32_t keysym; int pressed; } KeyArg;
static void do_key(WpeEngine *e, void *a) {
    KeyArg *k = a;
    struct wpe_input_keyboard_event ev = {
        .time = ev_time(), .key_code = k->keysym, .hardware_key_code = 0,
        .pressed = k->pressed ? true : false, .modifiers = 0,
    };
    wpe_view_backend_dispatch_keyboard_event(e->wpe_backend, &ev);
}
void wpe_engine_key(WpeEngine *e, uint32_t keysym, int pressed) {
    KeyArg *k = g_new0(KeyArg, 1); k->keysym = keysym; k->pressed = pressed;
    invoke(e, do_key, k);
}

/* --------------------------------------------------------------------- state */

void wpe_engine_current_uri(WpeEngine *e, char *buf, size_t len) {
    if (!e || !buf || len == 0) return;
    g_mutex_lock(&e->lock);
    g_strlcpy(buf, e->uri, len);
    g_mutex_unlock(&e->lock);
}

int wpe_engine_load_progress(WpeEngine *e) {
    if (!e) return 100;
    g_mutex_lock(&e->lock);
    int p = e->progress;
    g_mutex_unlock(&e->lock);
    return p;
}

int wpe_engine_kb_focus(WpeEngine *e) {
    if (!e) return 0;
    g_mutex_lock(&e->lock);
    int f = e->kb_focus;
    g_mutex_unlock(&e->lock);
    return f;
}

int wpe_engine_load_failed(WpeEngine *e) {
    if (!e) return 0;
    g_mutex_lock(&e->lock);
    int f = e->load_failed;
    g_mutex_unlock(&e->lock);
    return f;
}

static void do_set_active(WpeEngine *e, void *a) {
    int active = *(int *)a;
    uint32_t states = wpe_view_activity_state_visible | wpe_view_activity_state_focused |
                      wpe_view_activity_state_in_window;
    if (active)
        wpe_view_backend_add_activity_state(e->wpe_backend, states);
    else
        wpe_view_backend_remove_activity_state(e->wpe_backend, states);
}
void wpe_engine_set_active(WpeEngine *e, int active) {
    if (!e) return;
    int *p = g_new(int, 1);
    *p = active;
    invoke(e, do_set_active, p);
}

/* GSourceFunc-shaped wrapper — a direct (GSourceFunc)g_main_loop_quit cast is UB-adjacent
 * (-Wcast-function-type) even though it happens to work on this ABI. */
static gboolean quit_loop_cb(gpointer loop) {
    g_main_loop_quit((GMainLoop *)loop);
    return G_SOURCE_REMOVE;
}

void wpe_engine_destroy(WpeEngine *e) {
    if (!e) return;
    if (e->loop) {
        g_main_context_invoke(e->ctx, quit_loop_cb, e->loop);
        if (e->thread) g_thread_join(e->thread);
    }
    /* Release what we own. The view holds the strong ref from g_object_new; unref it here. We
     * deliberately do NOT wpe_view_backend_exportable_fdo_destroy() the exportable: the WebKit view
     * owns the wpe_view_backend it wraps, and destroying the exportable after the view finalizes
     * would risk a double-free of that backend. The loop/context/mutexes are ours exclusively. */
    if (e->view) g_object_unref(e->view);
    if (e->loop) g_main_loop_unref(e->loop);
    if (e->ctx) g_main_context_unref(e->ctx);
    g_mutex_clear(&e->lock);
    g_mutex_clear(&e->ready_lock);
    g_cond_clear(&e->ready_cond);
    g_free(e->frame);
    g_free(e->data_dir);
    g_free(e->initial_uri);
    g_free(e);
}
