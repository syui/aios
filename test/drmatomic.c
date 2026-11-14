// DRM の atomic の KMS とカーソル (/dev/dri/card0: virtio-gpu の画面) が Linux と同じ答えになるか (kernel/src/kms.rs):
//   caps     SET_CLIENT_CAP の ATOMIC (0 1 2 3) と CURSOR_PLANE_HOTSPOT。atomic にすると universal planes も。
//            仮想の GPU (virtio_gpu) では、HOTSPOT を言わない atomic のファイルにはカーソルの plane が見えない
//   props    plane (primary と cursor)、CRTC、connector の atomic のプロパティ (番号、名前、形、値)
//   blob     CREATEPROPBLOB と GETPROPBLOB と DESTROYPROPBLOB (モードの blob)
//   atomic   MODE_ATOMIC: atomic でないファイル、空のコミット、ALLOW_MODESET なしのモード変え、TEST_ONLY、本当のモード変え
//            (GETCRTC で見る)、PAGE_FLIP_EVENT と NONBLOCK (できごとを read)、まちがい (ないオブジェクト、そのオブジェクトに
//            ないプロパティ、変えられないプロパティ、DPMS、拡大、CRTC のない plane、知らない flags、TEST_ONLY と EVENT)、
//            ACTIVE=0 と 1、OUT_FENCE_PTR (sync_file)
//   cursor   cursor plane を atomic で (FB_ID、CRTC_X/Y、HOTSPOT)、古い MODE_CURSOR (BO、MOVE、消す) と MODE_CURSOR2
// card0 がなければ何もしない。1 行に ok NAME か FAIL NAME、おしまいに drmatomic: N failed。Linux の答えは test/drm-linux.sh
// (aarch64-linux-gnu-gcc -O2 -static -o drmatomic test/drmatomic.c)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#define IOC(dir, nr, sz) ((unsigned long)(dir) << 30 | (unsigned long)(sz) << 16 | ('d' << 8) | (nr))
#define IOWR(nr, sz) IOC(3, nr, sz)
#define IOW(nr, sz) IOC(1, nr, sz)

struct modeinfo {
  uint32_t clock;
  uint16_t hdisplay, hsync_start, hsync_end, htotal, hskew;
  uint16_t vdisplay, vsync_start, vsync_end, vtotal, vscan;
  uint32_t vrefresh, flags, type;
  char name[32];
};
struct card_res {
  uint64_t fbs, crtcs, connectors, encoders;
  uint32_t count_fbs, count_crtcs, count_connectors, count_encoders;
  uint32_t min_width, max_width, min_height, max_height;
};
struct crtc {
  uint64_t set_connectors_ptr;
  uint32_t count_connectors, crtc_id, fb_id, x, y, gamma_size, mode_valid;
  struct modeinfo mode;
};
struct connector {
  uint64_t encoders_ptr, modes_ptr, props_ptr, prop_values_ptr;
  uint32_t count_modes, count_props, count_encoders, encoder_id, connector_id, connector_type, connector_type_id, connection, mm_width,
      mm_height, subpixel, pad;
};
struct property {
  uint64_t values_ptr, enum_blob_ptr;
  uint32_t prop_id, flags;
  char name[32];
  uint32_t count_values, count_enum_blobs;
};
struct prop_enum {
  uint64_t value;
  char name[32];
};
struct blob {
  uint32_t blob_id, length;
  uint64_t data;
};
struct create_blob {
  uint64_t data;
  uint32_t length, blob_id;
};
struct obj_props {
  uint64_t props_ptr, prop_values_ptr;
  uint32_t count_props, obj_id, obj_type;
};
struct plane_res {
  uint64_t plane_id_ptr;
  uint32_t count_planes;
};
struct plane {
  uint32_t plane_id, crtc_id, fb_id, possible_crtcs, gamma_size, count_format_types;
  uint64_t format_type_ptr;
};
struct create_dumb {
  uint32_t height, width, bpp, flags, handle, pitch;
  uint64_t size;
};
struct map_dumb {
  uint32_t handle, pad;
  uint64_t offset;
};
struct fb_cmd2 {
  uint32_t fb_id, width, height, pixel_format, flags;
  uint32_t handles[4], pitches[4], offsets[4];
  uint64_t modifier[4];
};
struct atomic {
  uint32_t flags, count_objs;
  uint64_t objs_ptr, count_props_ptr, props_ptr, prop_values_ptr, reserved, user_data;
};
struct cursor {
  uint32_t flags, crtc_id;
  int32_t x, y;
  uint32_t width, height, handle;
};
struct cursor2 {
  uint32_t flags, crtc_id;
  int32_t x, y;
  uint32_t width, height, handle;
  int32_t hot_x, hot_y;
};
struct vblank_event {
  uint32_t type, length;
  uint64_t user_data;
  uint32_t tv_sec, tv_usec, sequence, crtc_id;
};

#define SET_CLIENT_CAP IOW(0x0d, 16)
#define GETRESOURCES IOWR(0xa0, sizeof(struct card_res))
#define GETCRTC IOWR(0xa1, sizeof(struct crtc))
#define CURSOR IOWR(0xa3, sizeof(struct cursor))
#define GETCONNECTOR IOWR(0xa7, sizeof(struct connector))
#define GETPROPERTY IOWR(0xaa, sizeof(struct property))
#define GETPROPBLOB IOWR(0xac, sizeof(struct blob))
#define RMFB IOWR(0xaf, 4)
#define CREATE_DUMB IOWR(0xb2, sizeof(struct create_dumb))
#define MAP_DUMB IOWR(0xb3, sizeof(struct map_dumb))
#define GETPLANERESOURCES IOWR(0xb5, sizeof(struct plane_res))
#define GETPLANE IOWR(0xb6, sizeof(struct plane))
#define ADDFB2 IOWR(0xb8, sizeof(struct fb_cmd2))
#define OBJ_GETPROPERTIES IOWR(0xb9, sizeof(struct obj_props))
#define CURSOR2 IOWR(0xbb, sizeof(struct cursor2))
#define ATOMIC IOWR(0xbc, sizeof(struct atomic))
#define CREATEPROPBLOB IOWR(0xbd, sizeof(struct create_blob))
#define DESTROYPROPBLOB IOWR(0xbe, 4)

#define OBJ_CRTC 0xccccccccu
#define OBJ_CONNECTOR 0xc0c0c0c0u
#define OBJ_PLANE 0xeeeeeeeeu
#define XR24 0x34325258u
#define AR24 0x34325241u

#define A_EVENT 0x1
#define A_TEST 0x100
#define A_NONBLOCK 0x200
#define A_MODESET 0x400

static int fails;

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static int io(int fd, unsigned long req, void *arg) {
  return ioctl(fd, req, arg) == 0 ? 0 : -errno;
}

static int client_cap(int fd, uint64_t c, uint64_t v) {
  uint64_t a[2] = {c, v};
  return io(fd, SET_CLIENT_CAP, a);
}

// オブジェクトのプロパティを「番号:名前=値」で。見たことのない番号は seen に足す
static uint32_t seen[64];
static int nseen;
static void props_str(int fd, uint32_t id, uint32_t type, char *out, size_t n, int *err) {
  uint32_t ids[32];
  uint64_t vals[32];
  struct obj_props op = {(uintptr_t)ids, (uintptr_t)vals, 32, id, type};
  *err = io(fd, OBJ_GETPROPERTIES, &op);
  out[0] = 0;
  if (*err)
    return;
  for (uint32_t i = 0; i < op.count_props && i < 32; i++) {
    struct property p = {.prop_id = ids[i]};
    io(fd, GETPROPERTY, &p);
    // blob と framebuffer の番号は作るたびに変わるので、0 かどうかだけ。FB_DAMAGE_CLIPS は前のコミットの変わった四角
    // (Linux では fbdev が書いたときのものが残る): 値は見ない
    int idlike = !strcmp(p.name, "FB_ID") || !strcmp(p.name, "MODE_ID") || !strcmp(p.name, "EDID");
    if (!strcmp(p.name, "FB_DAMAGE_CLIPS"))
      snprintf(out + strlen(out), n - strlen(out), "%s%u:%s", i ? " " : "", ids[i], p.name);
    else if (idlike)
      snprintf(out + strlen(out), n - strlen(out), "%s%u:%s=%s", i ? " " : "", ids[i], p.name, vals[i] ? "set" : "0");
    else
      snprintf(out + strlen(out), n - strlen(out), "%s%u:%s=%lld", i ? " " : "", ids[i], p.name, (long long)vals[i]);
    int have = 0;
    for (int k = 0; k < nseen; k++)
      have |= seen[k] == ids[i];
    if (!have && nseen < 64)
      seen[nseen++] = ids[i];
  }
}

static uint32_t prop_of(int fd, uint32_t id, uint32_t type, const char *name) {
  uint32_t ids[32];
  uint64_t vals[32];
  struct obj_props op = {(uintptr_t)ids, (uintptr_t)vals, 32, id, type};
  if (io(fd, OBJ_GETPROPERTIES, &op))
    return 0;
  for (uint32_t i = 0; i < op.count_props && i < 32; i++) {
    struct property p = {.prop_id = ids[i]};
    io(fd, GETPROPERTY, &p);
    if (!strcmp(p.name, name))
      return ids[i];
  }
  return 0;
}

static uint64_t value_of(int fd, uint32_t id, uint32_t type, uint32_t prop) {
  uint32_t ids[32];
  uint64_t vals[32];
  struct obj_props op = {(uintptr_t)ids, (uintptr_t)vals, 32, id, type};
  if (io(fd, OBJ_GETPROPERTIES, &op))
    return ~0ull;
  for (uint32_t i = 0; i < op.count_props && i < 32; i++)
    if (ids[i] == prop)
      return vals[i];
  return ~0ull;
}

// 1 つのコミット (オブジェクトごとにプロパティを並べたもの) を組む
struct req {
  uint32_t objs[8], counts[8], props[32];
  uint64_t vals[32];
  int nobj, nprop;
};
static void add(struct req *r, uint32_t obj, uint32_t prop, uint64_t val) {
  if (r->nobj == 0 || r->objs[r->nobj - 1] != obj) {
    r->objs[r->nobj] = obj;
    r->counts[r->nobj++] = 0;
  }
  r->counts[r->nobj - 1]++;
  r->props[r->nprop] = prop;
  r->vals[r->nprop++] = val;
}
static int commit(int fd, struct req *r, uint32_t flags, uint64_t user) {
  struct atomic a = {flags, r->nobj, (uintptr_t)r->objs, (uintptr_t)r->counts, (uintptr_t)r->props, (uintptr_t)r->vals, 0, user};
  return io(fd, ATOMIC, &a);
}

// できごとを 1 つ読む (なければ 0): 種類、crtc、user_data
static int read_event(int fd, int ms, struct vblank_event *ev) {
  struct pollfd p = {fd, POLLIN, 0};
  if (poll(&p, 1, ms) <= 0)
    return 0;
  char b[64];
  ssize_t n = read(fd, b, sizeof b);
  if (n < (ssize_t)sizeof *ev)
    return (int)n;
  memcpy(ev, b, sizeof *ev);
  return (int)n;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    printf("drmatomic: no /dev/dri/card0: skipped\n");
    return 0;
  }
  char buf[1400], s[1200];

  uint32_t crtc_id, conn_id;
  struct modeinfo modes[64], m640 = {0};
  {
    uint32_t crtcs[4], conns[4], encs[4], fbs[4];
    struct card_res r = {0};
    io(fd, GETRESOURCES, &r);
    r.fbs = (uintptr_t)fbs;
    r.crtcs = (uintptr_t)crtcs;
    r.connectors = (uintptr_t)conns;
    r.encoders = (uintptr_t)encs;
    io(fd, GETRESOURCES, &r);
    crtc_id = crtcs[0];
    conn_id = conns[0];
    struct connector c = {.connector_id = conn_id};
    io(fd, GETCONNECTOR, &c);
    c.modes_ptr = (uintptr_t)modes;
    c.count_modes = c.count_modes > 64 ? 64 : c.count_modes;
    c.count_props = 0;
    c.count_encoders = 0;
    io(fd, GETCONNECTOR, &c);
    for (uint32_t i = 0; i < c.count_modes; i++)
      if (modes[i].hdisplay == 640 && modes[i].vdisplay == 480 && modes[i].vrefresh == 60) {
        m640 = modes[i];
        break;
      }
  }

  // atomic でないファイルの MODE_ATOMIC
  {
    struct req r = {0};
    int a = commit(fd, &r, 0, 0);
    snprintf(buf, sizeof buf, "ATOMIC without the client cap %d", a);
    check(buf, a == -EINVAL);
  }

  // caps
  {
    uint32_t planes[4];
    struct plane_res pr = {(uintptr_t)planes, 4};
    int a3 = client_cap(fd, 3, 3), a2 = client_cap(fd, 3, 2), a1 = client_cap(fd, 3, 1);
    pr.count_planes = 4;
    io(fd, GETPLANERESOURCES, &pr);
    uint32_t n1 = pr.count_planes;
    int h2 = client_cap(fd, 6, 2), h1 = client_cap(fd, 6, 1);
    pr.count_planes = 4;
    io(fd, GETPLANERESOURCES, &pr);
    uint32_t n2 = pr.count_planes;
    snprintf(buf, sizeof buf, "SET_CLIENT_CAP ATOMIC (3 %d, 2 %d, 1 %d: %u planes; CURSOR_PLANE_HOTSPOT 2 %d, 1 %d: %u planes %u %u)", a3, a2, a1, n1, h2, h1, n2, planes[0], planes[1]);
    check(buf, a3 == -EINVAL && a1 == 0 && h1 == 0 && n2 == 2);
  }

  // props
  {
    int e1, e2, e3, e4;
    char ps[600];
    props_str(fd, 33, OBJ_PLANE, ps, sizeof ps, &e1);
    snprintf(buf, sizeof buf, "primary plane props (%d: %s)", e1, ps);
    check(buf, e1 == 0);
    props_str(fd, 34, OBJ_PLANE, ps, sizeof ps, &e2);
    snprintf(buf, sizeof buf, "cursor plane props (%d: %s)", e2, ps);
    check(buf, e2 == 0);
    props_str(fd, crtc_id, OBJ_CRTC, ps, sizeof ps, &e3);
    snprintf(buf, sizeof buf, "crtc props (%d: %s)", e3, ps);
    check(buf, e3 == 0);
    props_str(fd, conn_id, OBJ_CONNECTOR, ps, sizeof ps, &e4);
    snprintf(buf, sizeof buf, "connector props (%d: %s)", e4, ps);
    check(buf, e4 == 0);
    // 番号の順に、形と値の範囲と enum
    for (int i = 0; i < nseen; i++)
      for (int j = i + 1; j < nseen; j++)
        if (seen[j] < seen[i]) {
          uint32_t t = seen[i];
          seen[i] = seen[j];
          seen[j] = t;
        }
    for (int i = 0; i < nseen; i++) {
      uint64_t vals[8] = {0};
      struct prop_enum en[8];
      memset(en, 0, sizeof en);
      struct property p = {.prop_id = seen[i]};
      io(fd, GETPROPERTY, &p);
      p.values_ptr = (uintptr_t)vals;
      p.enum_blob_ptr = (uintptr_t)en;
      if (p.count_values > 8)
        p.count_values = 8;
      if (p.count_enum_blobs > 8)
        p.count_enum_blobs = 8;
      int g = io(fd, GETPROPERTY, &p);
      s[0] = 0;
      for (uint32_t k = 0; k < p.count_values && k < 8; k++)
        snprintf(s + strlen(s), sizeof s - strlen(s), " v%lld", (long long)vals[k]);
      for (uint32_t k = 0; k < p.count_enum_blobs && k < 8; k++)
        snprintf(s + strlen(s), sizeof s - strlen(s), " %s=%llu", en[k].name, (unsigned long long)en[k].value);
      snprintf(buf, sizeof buf, "GETPROPERTY %u (%d, %s, flags %#x, %u values, %u enums:%s)", seen[i], g, p.name, p.flags, p.count_values, p.count_enum_blobs, s);
      check(buf, g == 0);
    }
  }

  uint32_t p_fb = prop_of(fd, 33, OBJ_PLANE, "FB_ID"), p_crtc = prop_of(fd, 33, OBJ_PLANE, "CRTC_ID");
  uint32_t p_sx = prop_of(fd, 33, OBJ_PLANE, "SRC_X"), p_sy = prop_of(fd, 33, OBJ_PLANE, "SRC_Y"), p_sw = prop_of(fd, 33, OBJ_PLANE, "SRC_W"), p_sh = prop_of(fd, 33, OBJ_PLANE, "SRC_H");
  uint32_t p_cx = prop_of(fd, 33, OBJ_PLANE, "CRTC_X"), p_cy = prop_of(fd, 33, OBJ_PLANE, "CRTC_Y"), p_cw = prop_of(fd, 33, OBJ_PLANE, "CRTC_W"), p_ch = prop_of(fd, 33, OBJ_PLANE, "CRTC_H");
  uint32_t p_type = prop_of(fd, 33, OBJ_PLANE, "type");
  uint32_t p_active = prop_of(fd, crtc_id, OBJ_CRTC, "ACTIVE"), p_mode = prop_of(fd, crtc_id, OBJ_CRTC, "MODE_ID"), p_fence = prop_of(fd, crtc_id, OBJ_CRTC, "OUT_FENCE_PTR");
  uint32_t p_ccrtc = prop_of(fd, conn_id, OBJ_CONNECTOR, "CRTC_ID"), p_dpms = prop_of(fd, conn_id, OBJ_CONNECTOR, "DPMS");
  uint32_t p_hx = prop_of(fd, 34, OBJ_PLANE, "HOTSPOT_X"), p_hy = prop_of(fd, 34, OBJ_PLANE, "HOTSPOT_Y");

  // blob
  uint32_t mode_blob = 0;
  {
    struct create_blob cb = {(uintptr_t)&m640, sizeof m640, 0};
    int a = io(fd, CREATEPROPBLOB, &cb);
    mode_blob = cb.blob_id;
    struct modeinfo back = {0};
    struct blob g = {mode_blob, sizeof back, (uintptr_t)&back};
    int b = io(fd, GETPROPBLOB, &g);
    struct create_blob z = {(uintptr_t)&m640, 0, 0};
    int c = io(fd, CREATEPROPBLOB, &z);
    struct create_blob t = {(uintptr_t)&m640, sizeof m640, 0};
    io(fd, CREATEPROPBLOB, &t);
    uint32_t gone = t.blob_id;
    int d = io(fd, DESTROYPROPBLOB, &gone);
    int e = io(fd, DESTROYPROPBLOB, &gone);
    uint32_t edid = 40;
    int f = io(fd, DESTROYPROPBLOB, &edid);
    snprintf(buf, sizeof buf, "blobs (create %d, get %d same %d, length 0 %d, destroy %d, again %d, the EDID blob %d)", a, b, !memcmp(&back, &m640, sizeof back), c, d, e, f);
    check(buf, a == 0 && b == 0 && !memcmp(&back, &m640, sizeof back) && d == 0 && e == -ENOENT);
  }

  // framebuffers: 640x480 の XR24 を 2 つと、64x64 の AR24 (カーソル)
  uint32_t fb[2] = {0}, cfb = 0, chandle = 0;
  {
    for (int i = 0; i < 2; i++) {
      struct create_dumb d = {.height = 480, .width = 640, .bpp = 32};
      io(fd, CREATE_DUMB, &d);
      struct map_dumb md = {.handle = d.handle};
      io(fd, MAP_DUMB, &md);
      uint32_t *px = mmap(NULL, d.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, md.offset);
      if (px != MAP_FAILED) {
        for (int k = 0; k < 640 * 480; k++)
          px[k] = i ? 0x000000ff : 0x0000ff00;
        munmap(px, d.size);
      }
      struct fb_cmd2 f = {.width = 640, .height = 480, .pixel_format = XR24, .handles = {d.handle}, .pitches = {d.pitch}};
      io(fd, ADDFB2, &f);
      fb[i] = f.fb_id;
    }
    struct create_dumb d = {.height = 64, .width = 64, .bpp = 32};
    io(fd, CREATE_DUMB, &d);
    chandle = d.handle;
    struct map_dumb md = {.handle = d.handle};
    io(fd, MAP_DUMB, &md);
    uint32_t *px = mmap(NULL, d.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, md.offset);
    if (px != MAP_FAILED) {
      for (int k = 0; k < 64 * 64; k++)
        px[k] = (k % 64 < 8 || k / 64 < 8) ? 0xffffffff : 0;
      munmap(px, d.size);
    }
    struct fb_cmd2 f = {.width = 64, .height = 64, .pixel_format = AR24, .handles = {d.handle}, .pitches = {d.pitch}};
    io(fd, ADDFB2, &f);
    cfb = f.fb_id;
  }

  // atomic
  {
    struct req empty = {0};
    int a = commit(fd, &empty, 0, 0);
    struct req full = {0};
    add(&full, crtc_id, p_active, 1);
    add(&full, crtc_id, p_mode, mode_blob);
    add(&full, conn_id, p_ccrtc, crtc_id);
    add(&full, 33, p_fb, fb[0]);
    add(&full, 33, p_crtc, crtc_id);
    add(&full, 33, p_sx, 0);
    add(&full, 33, p_sy, 0);
    add(&full, 33, p_sw, 640 << 16);
    add(&full, 33, p_sh, 480 << 16);
    add(&full, 33, p_cx, 0);
    add(&full, 33, p_cy, 0);
    add(&full, 33, p_cw, 640);
    add(&full, 33, p_ch, 480);
    int b = commit(fd, &full, 0, 0);
    int c = commit(fd, &full, A_TEST | A_MODESET, 0);
    struct crtc cr0 = {.crtc_id = crtc_id};
    io(fd, GETCRTC, &cr0);
    int d = commit(fd, &full, A_MODESET, 0);
    struct crtc cr = {.crtc_id = crtc_id};
    io(fd, GETCRTC, &cr);
    snprintf(buf, sizeof buf, "ATOMIC modeset (empty %d, without ALLOW_MODESET %d, TEST_ONLY %d: crtc still %ux%u, commit %d: crtc %ux%u %s fb %s, MODE_ID %s, ACTIVE %llu)", a, b, c, cr0.mode.hdisplay,
             cr0.mode.vdisplay, d, cr.mode.hdisplay, cr.mode.vdisplay, cr.mode.name, cr.fb_id == fb[0] ? "ours" : "other", value_of(fd, crtc_id, OBJ_CRTC, p_mode) == mode_blob ? "ours" : "other",
             (unsigned long long)value_of(fd, crtc_id, OBJ_CRTC, p_active));
    check(buf, a == 0 && b == -EINVAL && c == 0 && d == 0 && cr.mode.hdisplay == 640 && cr.fb_id == fb[0]);

    // ページめくり (plane の FB_ID だけ): できごと
    struct req flip = {0};
    add(&flip, 33, p_fb, fb[1]);
    int f1 = commit(fd, &flip, A_EVENT | A_NONBLOCK, 0x1234);
    struct vblank_event ev = {0};
    int n = read_event(fd, 2000, &ev);
    struct crtc cr2 = {.crtc_id = crtc_id};
    io(fd, GETCRTC, &cr2);
    struct req flip2 = {0};
    add(&flip2, 33, p_fb, fb[0]);
    int f2 = commit(fd, &flip2, A_EVENT, 0x5678);
    struct vblank_event ev2 = {0};
    int n2 = read_event(fd, 2000, &ev2);
    int f3 = commit(fd, &flip2, A_EVENT | A_TEST, 0);
    struct req none = {0};
    int f4 = commit(fd, &none, A_EVENT, 0);
    snprintf(buf, sizeof buf, "ATOMIC flip (NONBLOCK + EVENT %d: read %d type %u length %u data %#llx crtc %s, fb now %s; blocking + EVENT %d: read %d data %#llx; TEST_ONLY + EVENT %d; EVENT with no crtc %d)", f1, n,
             ev.type, ev.length, (unsigned long long)ev.user_data, ev.crtc_id == crtc_id ? "ours" : "other", cr2.fb_id == fb[1] ? "second" : "other", f2, n2, (unsigned long long)ev2.user_data, f3, f4);
    check(buf, f1 == 0 && n == 32 && ev.type == 2 && ev.user_data == 0x1234 && f2 == 0 && f3 == -EINVAL);

    // まちがい
    int er[10];
    struct req r0 = {0};
    add(&r0, 999, p_fb, fb[0]);
    er[0] = commit(fd, &r0, 0, 0);
    struct req r1 = {0};
    add(&r1, 33, p_active, 1);
    er[1] = commit(fd, &r1, 0, 0);
    struct req r2 = {0};
    add(&r2, 33, p_type, 0);
    er[2] = commit(fd, &r2, 0, 0);
    struct req r3 = {0};
    add(&r3, conn_id, p_dpms, 3);
    er[3] = commit(fd, &r3, 0, 0);
    struct req r4 = {0};
    add(&r4, 33, p_sw, 320 << 16);
    add(&r4, 33, p_sh, 240 << 16);
    er[4] = commit(fd, &r4, 0, 0);
    struct req r5 = {0};
    add(&r5, 33, p_crtc, 0);
    er[5] = commit(fd, &r5, 0, 0);
    struct req r6 = {0};
    er[6] = commit(fd, &r6, 0x8000, 0);
    struct req r7 = {0};
    add(&r7, 33, p_fb, 9999);
    er[7] = commit(fd, &r7, 0, 0);
    struct req r8 = {0};
    add(&r8, 33, 9999, 1);
    er[8] = commit(fd, &r8, 0, 0);
    struct atomic bad = {0, 1, 0, 0, 0, 0, 1, 0};
    er[9] = io(fd, ATOMIC, &bad);
    snprintf(buf, sizeof buf, "ATOMIC errors (no such object %d, crtc prop on a plane %d, immutable type %d, DPMS %d, scaling %d, plane without crtc %d, unknown flag %d, no such fb %d, no such prop %d, reserved %d)", er[0],
             er[1], er[2], er[3], er[4], er[5], er[6], er[7], er[8], er[9]);
    check(buf, er[0] == -ENOENT && er[6] == -EINVAL && er[9] == -EINVAL);

    // ACTIVE を 0 にして、もどす
    struct req off = {0};
    add(&off, crtc_id, p_active, 0);
    int o1 = commit(fd, &off, 0, 0);
    int o2 = commit(fd, &off, A_MODESET, 0);
    struct crtc cr3 = {.crtc_id = crtc_id};
    io(fd, GETCRTC, &cr3);
    uint64_t act = value_of(fd, crtc_id, OBJ_CRTC, p_active);
    struct req on = {0};
    add(&on, crtc_id, p_active, 1);
    int o3 = commit(fd, &on, A_MODESET, 0);
    struct crtc cr4 = {.crtc_id = crtc_id};
    io(fd, GETCRTC, &cr4);
    snprintf(buf, sizeof buf, "ATOMIC ACTIVE (0 without ALLOW_MODESET %d, with %d: ACTIVE %llu mode_valid %u fb %s; 1 %d: mode_valid %u %ux%u)", o1, o2, (unsigned long long)act, cr3.mode_valid,
             cr3.fb_id ? "set" : "0", o3, cr4.mode_valid, cr4.mode.hdisplay, cr4.mode.vdisplay);
    check(buf, o1 == -EINVAL && o2 == 0 && act == 0 && o3 == 0 && cr4.mode_valid == 1);

    // OUT_FENCE_PTR
    int32_t ffd = -2;
    struct req fe = {0};
    add(&fe, crtc_id, p_fence, (uintptr_t)&ffd);
    add(&fe, 33, p_fb, fb[1]);
    int g1 = commit(fd, &fe, 0, 0);
    struct pollfd pf = {ffd, POLLIN, 0};
    int pr = ffd >= 0 ? poll(&pf, 1, 2000) : -1;
    if (ffd >= 0)
      close(ffd);
    struct req fe2 = {0};
    add(&fe2, crtc_id, p_fence, 8);
    add(&fe2, 33, p_fb, fb[0]);
    int g2 = commit(fd, &fe2, 0, 0);
    snprintf(buf, sizeof buf, "ATOMIC OUT_FENCE_PTR (%d, fd %s, poll %d %#x; bad pointer %d)", g1, ffd >= 0 ? "ok" : "none", pr, pf.revents, g2);
    check(buf, g1 == 0 && ffd >= 0 && pr == 1);
  }

  // cursor
  {
    struct req c1 = {0};
    add(&c1, 34, p_fb, cfb);
    add(&c1, 34, p_crtc, crtc_id);
    add(&c1, 34, p_sx, 0);
    add(&c1, 34, p_sy, 0);
    add(&c1, 34, p_sw, 64 << 16);
    add(&c1, 34, p_sh, 64 << 16);
    add(&c1, 34, p_cx, 100);
    add(&c1, 34, p_cy, 50);
    add(&c1, 34, p_cw, 64);
    add(&c1, 34, p_ch, 64);
    add(&c1, 34, p_hx, 4);
    add(&c1, 34, p_hy, 5);
    int a = commit(fd, &c1, 0, 0);
    struct plane pl = {.plane_id = 34};
    io(fd, GETPLANE, &pl);
    struct req mv = {0};
    add(&mv, 34, p_cx, 200);
    add(&mv, 34, p_cy, 120);
    int b = commit(fd, &mv, 0, 0);
    uint64_t x = value_of(fd, 34, OBJ_PLANE, p_cx), y = value_of(fd, 34, OBJ_PLANE, p_cy), hx = value_of(fd, 34, OBJ_PLANE, p_hx);
    struct req off = {0};
    add(&off, 34, p_fb, 0);
    add(&off, 34, p_crtc, 0);
    int c = commit(fd, &off, 0, 0);
    struct req bigger = c1;
    bigger.vals[8] = 32;
    bigger.vals[9] = 32;
    int d = commit(fd, &bigger, 0, 0);
    commit(fd, &off, 0, 0);
    snprintf(buf, sizeof buf, "cursor plane (set %d: crtc %s fb %s; move %d: %lld,%lld hotspot %lld; off %d; scaled %d)", a, pl.crtc_id == crtc_id ? "ours" : "other", pl.fb_id == cfb ? "ours" : "other", b,
             (long long)x, (long long)y, (long long)hx, c, d);
    check(buf, a == 0 && b == 0 && x == 200 && c == 0);
  }

  // 古いカーソルの ioctl (atomic でないファイルで)
  {
    int fd2 = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    struct cursor c = {.flags = 1, .crtc_id = crtc_id, .width = 64, .height = 64, .handle = chandle};
    int a = io(fd, CURSOR, &c);
    struct cursor m = {.flags = 2, .crtc_id = crtc_id, .x = 300, .y = 200};
    int b = io(fd, CURSOR, &m);
    struct cursor w = {.flags = 1, .crtc_id = crtc_id, .width = 65, .height = 64, .handle = chandle};
    int e1 = io(fd, CURSOR, &w);
    struct cursor nb = {.flags = 1, .crtc_id = crtc_id, .width = 64, .height = 64, .handle = 999};
    int e2 = io(fd, CURSOR, &nb);
    struct cursor nf = {.flags = 0, .crtc_id = crtc_id};
    int e3 = io(fd, CURSOR, &nf);
    struct cursor nc = {.flags = 2, .crtc_id = 999};
    int e4 = io(fd, CURSOR, &nc);
    struct cursor2 c2 = {.flags = 1, .crtc_id = crtc_id, .width = 64, .height = 64, .handle = chandle, .hot_x = 3, .hot_y = 7};
    int f = io(fd, CURSOR2, &c2);
    struct plane pl = {.plane_id = 34};
    io(fd, GETPLANE, &pl);
    struct cursor hide = {.flags = 1, .crtc_id = crtc_id, .handle = 0};
    int g = io(fd, CURSOR, &hide);
    struct plane pl2 = {.plane_id = 34};
    io(fd, GETPLANE, &pl2);
    snprintf(buf, sizeof buf, "MODE_CURSOR (BO %d, MOVE %d, width 65 %d, no such BO %d, no flags %d, no such crtc %d, CURSOR2 %d: plane crtc %s fb %s, hide %d: fb %u)", a, b, e1, e2, e3, e4, f,
             pl.crtc_id == crtc_id ? "ours" : "other", pl.fb_id ? "set" : "0", g, pl2.fb_id);
    check(buf, a == 0 && b == 0 && f == 0 && g == 0 && pl2.fb_id == 0);
    close(fd2);
  }

  for (int i = 0; i < 2; i++)
    io(fd, RMFB, &fb[i]);
  io(fd, RMFB, &cfb);
  close(fd);
  printf("drmatomic: %d failed\n", fails);
  return fails ? 1 : 0;
}
