/* wl-keys: Wayland のキーボード (wl_keyboard) を libxkbcommon で読み、押したキーを文字にして出す。
 * コンポジタが渡すキーマップ (XKB_V1) と修飾キーの確かめ。押すたびに窓の色が変わる */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>
#include "xdg-shell-client-protocol.h"

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct wl_seat *seat;
static struct xdg_wm_base *wm_base;
static struct wl_surface *surface;
static struct xkb_context *xkb;
static struct xkb_keymap *keymap;
static struct xkb_state *state;
static int width = 400, height = 300, running = 1, keys;
static uint32_t color = 0x2a2e3a;
static char typed[256];

static void redraw(void) {
    int size = width * height * 4;
    int fd = memfd_create("wl-keys", MFD_CLOEXEC);
    ftruncate(fd, size);
    uint32_t *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    for (int i = 0; i < width * height; i++) p[i] = color;
    /* 押した数だけ黄色い棒 */
    for (int k = 0; k < keys && k < 40; k++)
        for (int y = 20; y < 60; y++)
            for (int x = 20 + k * 12; x < 28 + k * 12 && x < width; x++) p[y * width + x] = 0xf5c518;
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
    struct wl_buffer *b = wl_shm_pool_create_buffer(pool, 0, width, height, width * 4, WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    munmap(p, size);
    wl_surface_attach(surface, b, 0, 0);
    wl_surface_damage_buffer(surface, 0, 0, width, height);
    wl_surface_commit(surface);
}

static void kb_keymap(void *d, struct wl_keyboard *k, uint32_t format, int fd, uint32_t size) {
    if (format != WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1) {
        printf("wl-keys: keymap format %u (no xkb keymap)\n", format);
        close(fd);
        return;
    }
    char *map = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (map == MAP_FAILED) { printf("wl-keys: cannot map the keymap\n"); return; }
    keymap = xkb_keymap_new_from_string(xkb, map, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
    munmap(map, size);
    if (!keymap) { printf("wl-keys: bad keymap\n"); return; }
    state = xkb_state_new(keymap);
    printf("wl-keys: keymap %u bytes, layout \"%s\"\n", size, xkb_keymap_layout_get_name(keymap, 0));
    fflush(stdout);
}
static void kb_enter(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *s, struct wl_array *keys) {
    printf("wl-keys: focus\n"); fflush(stdout);
}
static void kb_leave(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *s) {}
static void kb_key(void *d, struct wl_keyboard *k, uint32_t serial, uint32_t time, uint32_t key, uint32_t st) {
    if (!state || st != WL_KEYBOARD_KEY_STATE_PRESSED) return;
    xkb_keycode_t code = key + 8;
    char name[64], utf8[16];
    xkb_keysym_get_name(xkb_state_key_get_one_sym(state, code), name, sizeof name);
    xkb_state_key_get_utf8(state, code, utf8, sizeof utf8);
    int shift = xkb_state_mod_name_is_active(state, XKB_MOD_NAME_SHIFT, XKB_STATE_MODS_EFFECTIVE) > 0;
    int ctrl = xkb_state_mod_name_is_active(state, XKB_MOD_NAME_CTRL, XKB_STATE_MODS_EFFECTIVE) > 0;
    printf("wl-keys: key %u sym %s utf8 \"%s\"%s%s\n", key, name, (unsigned char)utf8[0] >= 0x20 ? utf8 : "", shift ? " +shift" : "", ctrl ? " +ctrl" : "");
    fflush(stdout);
    if ((unsigned char)utf8[0] >= 0x20 && strlen(typed) + strlen(utf8) < sizeof typed - 1) strcat(typed, utf8);
    keys++;
    color = 0x202020 + (keys * 0x1b2d37 & 0x5f5f5f);
    redraw();
}
static void kb_mods(void *d, struct wl_keyboard *k, uint32_t serial, uint32_t dep, uint32_t lat, uint32_t lock, uint32_t group) {
    if (state) xkb_state_update_mask(state, dep, lat, lock, 0, 0, group);
}
static void kb_repeat(void *d, struct wl_keyboard *k, int32_t rate, int32_t delay) {}
static const struct wl_keyboard_listener kb_listener = { kb_keymap, kb_enter, kb_leave, kb_key, kb_mods, kb_repeat };

static void seat_caps(void *d, struct wl_seat *s, uint32_t caps) {
    static struct wl_keyboard *kb;
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !kb) {
        kb = wl_seat_get_keyboard(s);
        wl_keyboard_add_listener(kb, &kb_listener, NULL);
    }
}
static void seat_name(void *d, struct wl_seat *s, const char *n) {}
static const struct wl_seat_listener seat_listener = { seat_caps, seat_name };

static void ping(void *d, struct xdg_wm_base *b, uint32_t serial) { xdg_wm_base_pong(b, serial); }
static const struct xdg_wm_base_listener wm_listener = { ping };
static void xs_configure(void *d, struct xdg_surface *xs, uint32_t serial) { xdg_surface_ack_configure(xs, serial); redraw(); }
static const struct xdg_surface_listener xs_listener = { xs_configure };
static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *s) { if (w > 0 && h > 0) { width = w; height = h; } }
static void top_close(void *d, struct xdg_toplevel *t) { running = 0; }
static void top_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h) {}
static void top_caps(void *d, struct xdg_toplevel *t, struct wl_array *c) {}
static const struct xdg_toplevel_listener top_listener = { top_configure, top_close, top_bounds, top_caps };

static void global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t ver) {
    if (!strcmp(iface, wl_compositor_interface.name)) compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
    else if (!strcmp(iface, wl_shm_interface.name)) shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
    else if (!strcmp(iface, wl_seat_interface.name)) {
        seat = wl_registry_bind(r, name, &wl_seat_interface, ver < 5 ? ver : 5);
        wl_seat_add_listener(seat, &seat_listener, NULL);
    } else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, ver < 2 ? ver : 2);
        xdg_wm_base_add_listener(wm_base, &wm_listener, NULL);
    }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) {}
static const struct wl_registry_listener reg_listener = { global, global_remove };

int main(void) {
    xkb = xkb_context_new(XKB_CONTEXT_NO_DEFAULT_INCLUDES | XKB_CONTEXT_NO_ENVIRONMENT_NAMES);
    struct wl_display *dpy = wl_display_connect(NULL);
    if (!dpy) { fprintf(stderr, "wl-keys: cannot connect to the Wayland display\n"); return 1; }
    struct wl_registry *reg = wl_display_get_registry(dpy);
    wl_registry_add_listener(reg, &reg_listener, NULL);
    wl_display_roundtrip(dpy);
    if (!compositor || !shm || !wm_base || !seat) { fprintf(stderr, "wl-keys: missing globals\n"); return 1; }
    surface = wl_compositor_create_surface(compositor);
    struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wm_base, surface);
    xdg_surface_add_listener(xs, &xs_listener, NULL);
    struct xdg_toplevel *top = xdg_surface_get_toplevel(xs);
    xdg_toplevel_add_listener(top, &top_listener, NULL);
    xdg_toplevel_set_title(top, "wl-keys (xkbcommon)");
    wl_surface_commit(surface);
    while (running && wl_display_dispatch(dpy) != -1) {}
    printf("wl-keys: typed \"%s\"\n", typed);
    return 0;
}
