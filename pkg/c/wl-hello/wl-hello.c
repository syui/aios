/* wl-hello: libwayland-client と xdg-shell で窓を出し、動く模様を描く (aios の動的リンクの確かめ) */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm_base;
static struct wl_surface *surface;
static int width = 640, height = 480, configured, running = 1, frames;
static struct wl_buffer *buffer;
static uint32_t *pixels;
static int buf_w, buf_h;

static void make_buffer(void) {
    if (buffer && buf_w == width && buf_h == height) return;
    if (buffer) { wl_buffer_destroy(buffer); munmap(pixels, buf_w * buf_h * 4); }
    int size = width * height * 4;
    int fd = memfd_create("wl-hello", MFD_CLOEXEC);
    ftruncate(fd, size);
    pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
    buffer = wl_shm_pool_create_buffer(pool, 0, width, height, width * 4, WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    buf_w = width; buf_h = height;
}

static void draw(uint32_t t);
static void frame_done(void *data, struct wl_callback *cb, uint32_t time) {
    wl_callback_destroy(cb);
    draw(time);
}
static const struct wl_callback_listener frame_listener = { frame_done };

static void draw(uint32_t t) {
    make_buffer();
    int s = t / 16;
    for (int y = 0; y < height; y++)
        for (int x = 0; x < width; x++) {
            int r = (x + s) & 255, g = (y + s / 2) & 255, b = ((x ^ y) + s) & 255;
            pixels[y * width + x] = r << 16 | g << 8 | b;
        }
    /* 真ん中に黄色い四角 (ふわふわ動く) */
    int cx = width / 2 + (int)((s % 200) - 100), cy = height / 2;
    for (int y = cy - 40; y < cy + 40; y++)
        for (int x = cx - 40; x < cx + 40; x++)
            if (x >= 0 && y >= 0 && x < width && y < height) pixels[y * width + x] = 0xf5c518;
    wl_surface_attach(surface, buffer, 0, 0);
    wl_surface_damage_buffer(surface, 0, 0, width, height);
    struct wl_callback *cb = wl_surface_frame(surface);
    wl_callback_add_listener(cb, &frame_listener, NULL);
    wl_surface_commit(surface);
    frames++;
}

static void ping(void *data, struct xdg_wm_base *b, uint32_t serial) { xdg_wm_base_pong(b, serial); }
static const struct xdg_wm_base_listener wm_listener = { ping };

static void surface_configure(void *data, struct xdg_surface *xs, uint32_t serial) {
    xdg_surface_ack_configure(xs, serial);
    if (!configured) { configured = 1; draw(0); }
}
static const struct xdg_surface_listener xs_listener = { surface_configure };

static void top_configure(void *data, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *states) {
    if (w > 0 && h > 0) { width = w; height = h; }
}
static void top_close(void *data, struct xdg_toplevel *t) { running = 0; }
static void top_bounds(void *data, struct xdg_toplevel *t, int32_t w, int32_t h) {}
static void top_caps(void *data, struct xdg_toplevel *t, struct wl_array *c) {}
static const struct xdg_toplevel_listener top_listener = { top_configure, top_close, top_bounds, top_caps };

static void global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t ver) {
    if (!strcmp(iface, wl_compositor_interface.name)) compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
    else if (!strcmp(iface, wl_shm_interface.name)) shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
    else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, ver < 2 ? ver : 2);
        xdg_wm_base_add_listener(wm_base, &wm_listener, NULL);
    }
}
static void global_remove(void *data, struct wl_registry *r, uint32_t name) {}
static const struct wl_registry_listener reg_listener = { global, global_remove };

int main(void) {
    struct wl_display *d = wl_display_connect(NULL);
    if (!d) { fprintf(stderr, "wl-hello: cannot connect to the Wayland display\n"); return 1; }
    struct wl_registry *reg = wl_display_get_registry(d);
    wl_registry_add_listener(reg, &reg_listener, NULL);
    wl_display_roundtrip(d);
    if (!compositor || !shm || !wm_base) { fprintf(stderr, "wl-hello: missing globals\n"); return 1; }
    surface = wl_compositor_create_surface(compositor);
    struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wm_base, surface);
    xdg_surface_add_listener(xs, &xs_listener, NULL);
    struct xdg_toplevel *top = xdg_surface_get_toplevel(xs);
    xdg_toplevel_add_listener(top, &top_listener, NULL);
    xdg_toplevel_set_title(top, "wl-hello (libwayland)");
    wl_surface_commit(surface);
    printf("wl-hello: connected with libwayland-client %s\n", "1.24.0");
    fflush(stdout);
    while (running && wl_display_dispatch(d) != -1) {}
    printf("wl-hello: %d frames\n", frames);
    wl_display_disconnect(d);
    return 0;
}
