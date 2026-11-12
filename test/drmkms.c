// DRM の KMS の口 (/dev/dri/card0: virtio-gpu の画面) の ioctl が Linux と同じ答えになるか (kernel/src/kms.rs):
//   caps       GET_CAP の KMS のもの、SET_CLIENT_CAP (UNIVERSAL_PLANES、ASPECT_RATIO、まちがった値)
//   objects    GETRESOURCES (番号と大きさの範囲)、GETCONNECTOR (モードの数とはじめのもの、mm、プロパティ)、GETENCODER、
//              GETCRTC (はじめは /dev/fb0 の framebuffer を映している)、GETPLANERESOURCES と GETPLANE、OBJ_GETPROPERTIES、
//              GETPROPERTY (名前、形、値、enum)、GETPROPBLOB (EDID)
//   dumb       CREATE_DUMB (pitch と大きさ)、MAP_DUMB と mmap、ADDFB2 と ADDFB とそのまちがい、GETFB と GETFB2
//   modeset    SETCRTC (640x480 で dumb の framebuffer を映す) とそのまちがい、GETCRTC、DIRTYFB
//   flip       PAGE_FLIP とできごと (poll で POLLIN、read で 32 バイト、小さい置き場は 0、ないときは EAGAIN)
//   props      DPMS と VRR_ENABLED を変える、変えられないもの、ユーザーの blob
//   rmfb       映している framebuffer を RMFB すると primary plane が外れる (CRTC は動いたまま)、CLOSEFB
//   master     GET_MAGIC と AUTH_MAGIC、DROP_MASTER のあとは SETCRTC が EACCES、SET_MASTER でもどる
// card0 がなければ何もしない。1 行に ok NAME か FAIL NAME、おしまいに drmkms: N failed
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
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#define IOC(dir, nr, sz) ((unsigned long)(dir) << 30 | (unsigned long)(sz) << 16 | ('d' << 8) | (nr))
#define IOWR(nr, sz) IOC(3, nr, sz)
#define IOW(nr, sz) IOC(1, nr, sz)
#define IOR(nr, sz) IOC(2, nr, sz)
#define IO(nr) IOC(0, nr, 0)

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
struct encoder {
  uint32_t encoder_id, encoder_type, crtc_id, possible_crtcs, possible_clones;
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
struct obj_set {
  uint64_t value;
  uint32_t prop_id, obj_id, obj_type;
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
struct fb_cmd {
  uint32_t fb_id, width, height, pitch, bpp, depth, handle;
};
struct fb_cmd2 {
  uint32_t fb_id, width, height, pixel_format, flags;
  uint32_t handles[4], pitches[4], offsets[4];
  uint64_t modifier[4];
};
struct page_flip {
  uint32_t crtc_id, fb_id, flags, reserved;
  uint64_t user_data;
};
struct dirty {
  uint32_t fb_id, flags, color, num_clips;
  uint64_t clips_ptr;
};
struct clip {
  uint16_t x1, y1, x2, y2;
};
struct vblank_event {
  uint32_t type, length;
  uint64_t user_data;
  uint32_t tv_sec, tv_usec, sequence, crtc_id;
};

#define GET_MAGIC IOR(0x02, 4)
#define GET_CAP IOWR(0x0c, 16)
#define SET_CLIENT_CAP IOW(0x0d, 16)
#define AUTH_MAGIC IOW(0x11, 4)
#define SET_MASTER IO(0x1e)
#define DROP_MASTER IO(0x1f)
#define WAIT_VBLANK IOWR(0x3a, 24)
#define GETRESOURCES IOWR(0xa0, sizeof(struct card_res))
#define GETCRTC IOWR(0xa1, sizeof(struct crtc))
#define SETCRTC IOWR(0xa2, sizeof(struct crtc))
#define GETENCODER IOWR(0xa6, sizeof(struct encoder))
#define GETCONNECTOR IOWR(0xa7, sizeof(struct connector))
#define GETPROPERTY IOWR(0xaa, sizeof(struct property))
#define SETPROPERTY IOWR(0xab, 16)
#define GETPROPBLOB IOWR(0xac, sizeof(struct blob))
#define GETFB IOWR(0xad, sizeof(struct fb_cmd))
#define ADDFB IOWR(0xae, sizeof(struct fb_cmd))
#define RMFB IOWR(0xaf, 4)
#define PAGE_FLIP IOWR(0xb0, sizeof(struct page_flip))
#define DIRTYFB IOWR(0xb1, sizeof(struct dirty))
#define CREATE_DUMB IOWR(0xb2, sizeof(struct create_dumb))
#define MAP_DUMB IOWR(0xb3, sizeof(struct map_dumb))
#define DESTROY_DUMB IOWR(0xb4, 4)
#define GETPLANERESOURCES IOWR(0xb5, sizeof(struct plane_res))
#define GETPLANE IOWR(0xb6, sizeof(struct plane))
#define ADDFB2 IOWR(0xb8, sizeof(struct fb_cmd2))
#define OBJ_GETPROPERTIES IOWR(0xb9, sizeof(struct obj_props))
#define OBJ_SETPROPERTY IOWR(0xba, sizeof(struct obj_set))
#define CREATEPROPBLOB IOWR(0xbd, sizeof(struct create_blob))
#define DESTROYPROPBLOB IOWR(0xbe, 4)
#define GETFB2 IOWR(0xce, sizeof(struct fb_cmd2))
#define CLOSEFB IOWR(0xd0, 8)

#define OBJ_CRTC 0xccccccccu
#define OBJ_CONNECTOR 0xc0c0c0c0u
#define OBJ_ENCODER 0xe0e0e0e0u
#define OBJ_PLANE 0xeeeeeeeeu
#define XR24 0x34325258u
#define AR24 0x34325241u

static int fails;

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static int io(int fd, unsigned long req, void *arg) {
  return ioctl(fd, req, arg) == 0 ? 0 : -errno;
}

static uint64_t cap(int fd, uint64_t c, int *err) {
  struct {
    uint64_t c, v;
  } a = {c, 0};
  *err = io(fd, GET_CAP, &a);
  return a.v;
}

static int client_cap(int fd, uint64_t c, uint64_t v) {
  uint64_t a[2] = {c, v};
  return io(fd, SET_CLIENT_CAP, a);
}

// オブジェクトのプロパティを「名前=値」で
static void props_str(int fd, uint32_t id, uint32_t type, char *out, size_t n, int *err) {
  uint32_t ids[16];
  uint64_t vals[16];
  struct obj_props op = {(uintptr_t)ids, (uintptr_t)vals, 16, id, type};
  *err = io(fd, OBJ_GETPROPERTIES, &op);
  out[0] = 0;
  if (*err)
    return;
  for (uint32_t i = 0; i < op.count_props && i < 16; i++) {
    struct property p = {.prop_id = ids[i]};
    io(fd, GETPROPERTY, &p);
    snprintf(out + strlen(out), n - strlen(out), "%s%u:%s=%lld", i ? " " : "", ids[i], p.name, (long long)vals[i]);
  }
}

static uint32_t crc32(const unsigned char *p, size_t n) {
  uint32_t c = 0xffffffff;
  for (size_t i = 0; i < n; i++) {
    c ^= p[i];
    for (int k = 0; k < 8; k++)
      c = (c >> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    printf("drmkms: no /dev/dri/card0: skipped\n");
    return 0;
  }
  char buf[512], s[400];

  // caps
  {
    int e[12];
    uint64_t v[12];
    uint64_t caps[12] = {1, 2, 3, 4, 7, 8, 9, 0x10, 0x11, 0x12, 0x15, 0x99};
    s[0] = 0;
    for (int i = 0; i < 12; i++) {
      v[i] = cap(fd, caps[i], &e[i]);
      snprintf(s + strlen(s), sizeof s - strlen(s), " %llx=%lld", (unsigned long long)caps[i], e[i] ? (long long)e[i] : (long long)v[i]);
    }
    snprintf(buf, sizeof buf, "GET_CAP KMS (%s)", s + 1);
    check(buf, v[0] == 1 && v[1] == 1 && v[5] == 64 && v[9] == 1 && e[11] == -EINVAL);
    int a = client_cap(fd, 2, 1), b = client_cap(fd, 2, 2), c = client_cap(fd, 4, 1), d = client_cap(fd, 4, 0), f = client_cap(fd, 99, 1);
    snprintf(buf, sizeof buf, "SET_CLIENT_CAP (universal planes %d, value 2 %d, aspect ratio %d %d, 99 %d)", a, b, c, d, f);
    check(buf, a == 0 && b == -EINVAL && c == 0 && d == 0 && f == -EINVAL);
    client_cap(fd, 2, 0);
  }

  // objects
  uint32_t crtc_id = 0, conn_id = 0, enc_id = 0;
  struct modeinfo modes[64];
  int nmodes = 0;
  {
    uint32_t crtcs[4], conns[4], encs[4], fbs[4];
    struct card_res r = {0};
    int a = io(fd, GETRESOURCES, &r);
    r.fbs = (uintptr_t)fbs;
    r.crtcs = (uintptr_t)crtcs;
    r.connectors = (uintptr_t)conns;
    r.encoders = (uintptr_t)encs;
    int b = io(fd, GETRESOURCES, &r);
    crtc_id = crtcs[0];
    conn_id = conns[0];
    enc_id = encs[0];
    snprintf(buf, sizeof buf, "GETRESOURCES (%d %d, fbs %u, crtc %u, connector %u, encoder %u, width %u-%u, height %u-%u)", a, b, r.count_fbs, crtc_id, conn_id, enc_id,
             r.min_width, r.max_width, r.min_height, r.max_height);
    check(buf, a == 0 && b == 0 && r.count_crtcs == 1 && r.count_connectors == 1 && r.count_encoders == 1 && r.min_width == 32 && r.max_width == 8192);

    struct connector c = {.connector_id = conn_id};
    int e1 = io(fd, GETCONNECTOR, &c);
    uint32_t cenc[4], pids[16];
    uint64_t pvals[16];
    c.encoders_ptr = (uintptr_t)cenc;
    c.modes_ptr = (uintptr_t)modes;
    c.props_ptr = (uintptr_t)pids;
    c.prop_values_ptr = (uintptr_t)pvals;
    if (c.count_modes > 64)
      c.count_modes = 64;
    if (c.count_props > 16)
      c.count_props = 16;
    int e2 = io(fd, GETCONNECTOR, &c);
    nmodes = c.count_modes;
    struct modeinfo *m = &modes[0];
    snprintf(buf, sizeof buf, "GETCONNECTOR (%d %d, type %u-%u, connection %u, %ux%u mm, subpixel %u, encoder %u, %u encoders, %d modes, first %s %u Hz type %#x flags %#x clock %u, %u props)", e1, e2,
             c.connector_type, c.connector_type_id, c.connection, c.mm_width, c.mm_height, c.subpixel, c.encoder_id, c.count_encoders, nmodes, m->name, m->vrefresh, m->type, m->flags, m->clock, c.count_props);
    check(buf, e1 == 0 && e2 == 0 && c.connector_type == 15 && c.connection == 1 && nmodes > 0 && cenc[0] == enc_id);
    s[0] = 0;
    for (int i = 0; i < nmodes; i++)
      snprintf(s + strlen(s), sizeof s - strlen(s), "%s%s@%u", i ? " " : "", modes[i].name, modes[i].vrefresh);
    snprintf(buf, sizeof buf, "modes (%s)", s);
    check(buf, nmodes > 0);
    struct connector bad = {.connector_id = 999};
    int e3 = io(fd, GETCONNECTOR, &bad);
    int pe;
    props_str(fd, conn_id, OBJ_CONNECTOR, s, sizeof s, &pe);
    snprintf(buf, sizeof buf, "connector props (%s), no such connector %d", s, e3);
    check(buf, pe == 0 && e3 == -ENOENT);

    for (uint32_t i = 0; i < c.count_props && i < 16; i++) {
      uint64_t vals[8] = {0};
      struct prop_enum en[8];
      memset(en, 0, sizeof en);
      struct property p = {.prop_id = pids[i]};
      int g1 = io(fd, GETPROPERTY, &p);
      p.values_ptr = (uintptr_t)vals;
      p.enum_blob_ptr = (uintptr_t)en;
      if (p.count_values > 8)
        p.count_values = 8;
      if (p.count_enum_blobs > 8)
        p.count_enum_blobs = 8;
      int g2 = io(fd, GETPROPERTY, &p);
      s[0] = 0;
      for (uint32_t k = 0; k < p.count_values && k < 8; k++)
        snprintf(s + strlen(s), sizeof s - strlen(s), " v%lld", (long long)vals[k]);
      for (uint32_t k = 0; k < p.count_enum_blobs && k < 8; k++)
        snprintf(s + strlen(s), sizeof s - strlen(s), " %s=%llu", en[k].name, (unsigned long long)en[k].value);
      snprintf(buf, sizeof buf, "GETPROPERTY %u (%d %d, %s, flags %#x, %u values, %u enums:%s)", pids[i], g1, g2, p.name, p.flags, p.count_values, p.count_enum_blobs, s);
      check(buf, g1 == 0 && g2 == 0);
      if (!strcmp(p.name, "EDID") && pvals[i]) {
        static unsigned char edid[1024];
        struct blob bl = {.blob_id = (uint32_t)pvals[i]};
        int b1 = io(fd, GETPROPBLOB, &bl);
        bl.data = (uintptr_t)edid;
        int b2 = io(fd, GETPROPBLOB, &bl);
        struct blob wrong = {.blob_id = (uint32_t)pvals[i], .length = 10, .data = (uintptr_t)edid + 512};
        memset(edid + 512, 0x5a, 16);
        int b3 = io(fd, GETPROPBLOB, &wrong);
        snprintf(buf, sizeof buf, "GETPROPBLOB EDID (%d %d, %u bytes, crc %08x, a wrong length copies nothing %d %d)", b1, b2, bl.length, crc32(edid, bl.length), b3, edid[512] == 0x5a);
        check(buf, b1 == 0 && b2 == 0 && bl.length >= 128 && b3 == 0 && edid[512] == 0x5a);
      }
    }
    struct property none = {.prop_id = 9999};
    int pn = io(fd, GETPROPERTY, &none);
    snprintf(buf, sizeof buf, "GETPROPERTY no such property %d", pn);
    check(buf, pn == -ENOENT);

    struct encoder en = {.encoder_id = enc_id}, en2 = {.encoder_id = 999};
    int x1 = io(fd, GETENCODER, &en), x2 = io(fd, GETENCODER, &en2);
    snprintf(buf, sizeof buf, "GETENCODER (%d, type %u, crtc %u, possible %u, clones %u, none %d)", x1, en.encoder_type, en.crtc_id, en.possible_crtcs, en.possible_clones, x2);
    check(buf, x1 == 0 && en.encoder_type == 5 && en.crtc_id == crtc_id && x2 == -ENOENT);

    struct crtc cr = {.crtc_id = crtc_id}, cr2 = {.crtc_id = 999};
    int c1 = io(fd, GETCRTC, &cr), c2 = io(fd, GETCRTC, &cr2);
    snprintf(buf, sizeof buf, "GETCRTC at start (%d, fb %u, %u,%u, gamma %u, mode %u %s, none %d)", c1, cr.fb_id, cr.x, cr.y, cr.gamma_size, cr.mode_valid, cr.mode.name, c2);
    check(buf, c1 == 0 && cr.mode_valid == 1 && c2 == -ENOENT);

    uint32_t planes[4] = {0};
    struct plane_res pr = {(uintptr_t)planes, 4};
    int q1 = io(fd, GETPLANERESOURCES, &pr);
    uint32_t n0 = pr.count_planes;
    client_cap(fd, 2, 1);
    pr.count_planes = 4;
    int q2 = io(fd, GETPLANERESOURCES, &pr);
    s[0] = 0;
    for (uint32_t i = 0; i < pr.count_planes && i < 4; i++) {
      uint32_t fmts[8] = {0};
      struct plane p = {.plane_id = planes[i], .count_format_types = 8, .format_type_ptr = (uintptr_t)fmts};
      int g = io(fd, GETPLANE, &p);
      char ps[200];
      int pe2;
      props_str(fd, planes[i], OBJ_PLANE, ps, sizeof ps, &pe2);
      snprintf(s + strlen(s), sizeof s - strlen(s), "%s%u: %d crtc %u fb %u possible %u %u formats %.4s [%s]", i ? "; " : "", planes[i], g, p.crtc_id, p.fb_id, p.possible_crtcs, p.count_format_types,
               (char *)fmts, ps);
    }
    snprintf(buf, sizeof buf, "planes (%d %d, without universal planes %u, with %u: %s)", q1, q2, n0, pr.count_planes, s);
    check(buf, q1 == 0 && q2 == 0 && n0 == 0 && pr.count_planes == 2);
    char cs[200];
    int ce, ee, ne, te;
    props_str(fd, crtc_id, OBJ_CRTC, cs, sizeof cs, &ce);
    props_str(fd, enc_id, OBJ_ENCODER, s, sizeof s, &ee);
    props_str(fd, 999, 0, s, sizeof s, &ne);
    props_str(fd, crtc_id, OBJ_CONNECTOR, s, sizeof s, &te);
    snprintf(buf, sizeof buf, "OBJ_GETPROPERTIES (crtc %d [%s], encoder %d, none %d, wrong type %d)", ce, cs, ee, ne, te);
    check(buf, ce == 0 && ee == -EINVAL && ne == -ENOENT && te == -ENOENT);
  }

  // dumb
  struct create_dumb d1 = {.height = 480, .width = 640, .bpp = 32};
  uint32_t fb1 = 0, fb2 = 0;
  {
    int a = io(fd, CREATE_DUMB, &d1);
    struct create_dumb d2 = {.height = 480, .width = 640, .bpp = 32};
    int a2 = io(fd, CREATE_DUMB, &d2);
    struct create_dumb b1 = {.height = 480, .width = 640, .bpp = 16}, b2 = {.height = 480, .width = 0, .bpp = 32};
    int e1 = io(fd, CREATE_DUMB, &b1), e2 = io(fd, CREATE_DUMB, &b2);
    snprintf(buf, sizeof buf, "CREATE_DUMB (%d %d, handles %u %u, pitch %u, size %llu, 16 bpp %d, width 0 %d)", a, a2, d1.handle, d2.handle, d1.pitch, (unsigned long long)d1.size, e1, e2);
    check(buf, a == 0 && d1.pitch == 2560 && d1.size == 1228800 && e1 == -EINVAL && e2 == -EINVAL);
    struct map_dumb md = {.handle = d1.handle}, mb = {.handle = 99};
    int m1 = io(fd, MAP_DUMB, &md), m2 = io(fd, MAP_DUMB, &mb);
    uint32_t *px = mmap(NULL, d1.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, md.offset);
    if (px != MAP_FAILED)
      for (int i = 0; i < 640 * 480; i++)
        px[i] = 0x00ff8000 | (i % 640);
    snprintf(buf, sizeof buf, "MAP_DUMB and mmap (%d, none %d, mmap %s)", m1, m2, px == MAP_FAILED ? "failed" : "ok");
    check(buf, m1 == 0 && m2 == -ENOENT && px != MAP_FAILED);

    struct fb_cmd2 f = {.width = 640, .height = 480, .pixel_format = XR24, .handles = {d1.handle}, .pitches = {2560}};
    int r = io(fd, ADDFB2, &f);
    fb1 = f.fb_id;
    struct fb_cmd2 e[7];
    for (int i = 0; i < 7; i++)
      e[i] = f;
    e[0].pixel_format = 0x36314752; // RG16
    e[1].pixel_format = 0x5a5a5a5a;
    e[2].pitches[0] = 100;
    e[3].handles[0] = 0;
    e[4].handles[0] = 99;
    e[5].width = 16;
    e[6].flags = 2;
    int er[7];
    for (int i = 0; i < 7; i++)
      er[i] = io(fd, ADDFB2, &e[i]);
    snprintf(buf, sizeof buf, "ADDFB2 (%d, fb >= 42 %d, RG16 %d, unknown format %d, small pitch %d, handle 0 %d, no such handle %d, width 16 %d, modifiers %d)", r, fb1 >= 42, er[0], er[1], er[2], er[3], er[4],
             er[5], er[6]);
    check(buf, r == 0 && er[0] == -ENOENT && er[1] == -EINVAL && er[2] == -EINVAL && er[3] == -EINVAL && er[4] == -EINVAL && er[5] == -EINVAL && er[6] == -EINVAL);

    struct fb_cmd o = {.width = 640, .height = 480, .pitch = 2560, .bpp = 32, .depth = 24, .handle = d2.handle};
    int r2 = io(fd, ADDFB, &o);
    fb2 = o.fb_id;
    struct fb_cmd o2 = o, o3 = o;
    o2.bpp = 16;
    o2.depth = 16;
    o3.bpp = 12;
    o3.depth = 12;
    int r3 = io(fd, ADDFB, &o2), r4 = io(fd, ADDFB, &o3);
    snprintf(buf, sizeof buf, "ADDFB (%d, new fb %d, 16/16 %d, 12/12 %d)", r2, fb2 != fb1 && fb2 >= 42, r3, r4);
    check(buf, r2 == 0 && r3 == -ENOENT && r4 == -EINVAL);

    struct fb_cmd g = {.fb_id = fb1};
    struct fb_cmd2 g2 = {.fb_id = fb2};
    struct fb_cmd gn = {.fb_id = 9999};
    int x1 = io(fd, GETFB, &g), x2 = io(fd, GETFB2, &g2), x3 = io(fd, GETFB, &gn);
    snprintf(buf, sizeof buf, "GETFB (%d, %ux%u pitch %u bpp %u depth %u, handle %d) GETFB2 (%d, %.4s pitch %u, handle %d), none %d", x1, g.width, g.height, g.pitch, g.bpp, g.depth, g.handle != 0, x2,
             (char *)&g2.pixel_format, g2.pitches[0], g2.handles[0] != 0, x3);
    check(buf, x1 == 0 && g.bpp == 32 && g.depth == 24 && x2 == 0 && x3 == -ENOENT);
    uint32_t ids[4] = {0};
    struct card_res rr = {.fbs = (uintptr_t)ids, .count_fbs = 4};
    io(fd, GETRESOURCES, &rr);
    snprintf(buf, sizeof buf, "GETRESOURCES lists my framebuffers (%u, newest first %d)", rr.count_fbs, rr.count_fbs == 2 && ids[0] == fb2 && ids[1] == fb1);
    check(buf, rr.count_fbs == 2 && ids[0] == fb2 && ids[1] == fb1);
  }

  // modeset
  struct modeinfo *vga = NULL;
  for (int i = 0; i < nmodes; i++)
    if (modes[i].hdisplay == 640 && modes[i].vdisplay == 480) {
      vga = &modes[i];
      break;
    }
  {
    struct crtc c = {.set_connectors_ptr = (uintptr_t)&conn_id, .count_connectors = 1, .crtc_id = crtc_id, .fb_id = fb1, .mode_valid = 1};
    if (vga)
      c.mode = *vga;
    struct crtc big = c;
    big.mode = modes[0];
    int e0 = io(fd, SETCRTC, &big);
    int a = io(fd, SETCRTC, &c);
    struct crtc g = {.crtc_id = crtc_id};
    io(fd, GETCRTC, &g);
    struct encoder en = {.encoder_id = enc_id};
    io(fd, GETENCODER, &en);
    snprintf(buf, sizeof buf, "SETCRTC 640x480 (%d, the first mode on a small fb %d, now fb is mine %d, mode %s, encoder crtc %u)", a, e0, g.fb_id == fb1, g.mode.name, en.crtc_id);
    check(buf, vga && a == 0 && e0 == -ENOSPC && g.fb_id == fb1 && en.crtc_id == crtc_id);
    uint32_t badconn = 999;
    struct crtc e[5];
    for (int i = 0; i < 5; i++)
      e[i] = c;
    e[0].x = 0x10000;
    e[1].crtc_id = 999;
    e[2].fb_id = 9999;
    e[3].count_connectors = 0;
    e[4].set_connectors_ptr = (uintptr_t)&badconn;
    int er[5];
    for (int i = 0; i < 5; i++)
      er[i] = io(fd, SETCRTC, &e[i]);
    struct crtc keep = c;
    keep.fb_id = 0xffffffff;
    int k = io(fd, SETCRTC, &keep);
    snprintf(buf, sizeof buf, "SETCRTC errors (x 0x10000 %d, crtc %d, fb %d, no connectors %d, connector %d), keep the fb %d", er[0], er[1], er[2], er[3], er[4], k);
    check(buf, er[0] == -ERANGE && er[1] == -ENOENT && er[2] == -ENOENT && er[3] == -EINVAL && er[4] == -ENOENT && k == 0);

    struct clip cl[2] = {{0, 0, 100, 100}, {200, 200, 300, 250}};
    struct dirty dr = {.fb_id = fb1, .num_clips = 2, .clips_ptr = (uintptr_t)cl};
    struct dirty d0 = {.fb_id = fb1}, dn = {.fb_id = fb1, .num_clips = 2}, db = {.fb_id = 9999};
    int y1 = io(fd, DIRTYFB, &dr), y2 = io(fd, DIRTYFB, &d0), y3 = io(fd, DIRTYFB, &dn), y4 = io(fd, DIRTYFB, &db);
    snprintf(buf, sizeof buf, "DIRTYFB (clips %d, whole %d, clips without a pointer %d, none %d)", y1, y2, y3, y4);
    check(buf, y1 == 0 && y2 == 0 && y3 == -EINVAL && y4 == -ENOENT);
  }

  // flip
  {
    struct page_flip pf = {.crtc_id = crtc_id, .fb_id = fb2, .flags = 1, .user_data = 0x1234567890ull};
    int a = io(fd, PAGE_FLIP, &pf);
    struct pollfd p = {.fd = fd, .events = POLLIN | POLLOUT};
    int pr = poll(&p, 1, 2000);
    struct vblank_event ev = {0};
    ssize_t n = read(fd, &ev, sizeof ev);
    fcntl(fd, F_SETFL, O_NONBLOCK);
    char small[16];
    ssize_t n2 = read(fd, small, sizeof small);
    int e2 = errno;
    snprintf(buf, sizeof buf, "PAGE_FLIP with an event (%d, poll %d %#x, read %zd: type %u length %u user %#llx sequence %u crtc %u, then %zd %s)", a, pr, p.revents, n, ev.type, ev.length,
             (unsigned long long)ev.user_data, ev.sequence, ev.crtc_id, n2, n2 < 0 && e2 == EAGAIN ? "EAGAIN" : "?");
    check(buf, a == 0 && pr == 1 && p.revents == POLLIN && n == 32 && ev.type == 2 && ev.user_data == 0x1234567890ull && ev.crtc_id == crtc_id && n2 == -1 && e2 == EAGAIN);
    pf.fb_id = fb1;
    int b = io(fd, PAGE_FLIP, &pf);
    poll(&p, 1, 2000);
    ssize_t n3 = read(fd, small, sizeof small);
    ssize_t n4 = read(fd, &ev, sizeof ev);
    fcntl(fd, F_SETFL, 0);
    snprintf(buf, sizeof buf, "a small buffer reads nothing (%d, %zd, then %zd)", b, n3, n4);
    check(buf, b == 0 && n3 == 0 && n4 == 32);
    struct page_flip e[4];
    for (int i = 0; i < 4; i++)
      e[i] = pf;
    e[0].flags = 0x80;
    e[1].flags = 2;
    e[2].crtc_id = 999;
    e[3].fb_id = 9999;
    int er[4];
    for (int i = 0; i < 4; i++)
      er[i] = io(fd, PAGE_FLIP, &e[i]);
    snprintf(buf, sizeof buf, "PAGE_FLIP errors (flags 0x80 %d, async %d, crtc %d, fb %d)", er[0], er[1], er[2], er[3]);
    check(buf, er[0] == -EINVAL && er[1] == -EINVAL && er[2] == -ENOENT && er[3] == -ENOENT);
    uint64_t wv[3] = {0};
    int w = io(fd, WAIT_VBLANK, wv);
    snprintf(buf, sizeof buf, "WAIT_VBLANK (no vblank: %d)", w);
    check(buf, w == -EOPNOTSUPP);
  }

  // props
  {
    uint64_t sp[2] = {3, 0};
    ((uint32_t *)sp)[2] = 2;
    ((uint32_t *)sp)[3] = conn_id;
    int a = io(fd, SETPROPERTY, sp);
    char c1[200];
    int pe;
    props_str(fd, conn_id, OBJ_CONNECTOR, c1, sizeof c1, &pe);
    struct obj_set on = {0, 2, conn_id, OBJ_CONNECTOR};
    int b = io(fd, OBJ_SETPROPERTY, &on);
    struct obj_set nd = {1, 6, conn_id, OBJ_CONNECTOR}, vrr = {1, 24, crtc_id, OBJ_CRTC}, wrong = {1, 24, conn_id, OBJ_CONNECTOR};
    int c = io(fd, OBJ_SETPROPERTY, &nd), d = io(fd, OBJ_SETPROPERTY, &vrr), e = io(fd, OBJ_SETPROPERTY, &wrong);
    char c2[100];
    props_str(fd, crtc_id, OBJ_CRTC, c2, sizeof c2, &pe);
    vrr.value = 0;
    io(fd, OBJ_SETPROPERTY, &vrr);
    snprintf(buf, sizeof buf, "set properties (DPMS off %d [%s], on %d, non-desktop %d, VRR %d [%s], not on the object %d)", a, c1, b, c, d, c2, e);
    check(buf, a == 0 && b == 0 && c == -EINVAL && d == 0 && e == -EINVAL);
    unsigned char data[8] = {1, 2, 3, 4, 5, 6, 7, 8}, back[8] = {0};
    struct create_blob cb = {(uintptr_t)data, 8, 0};
    int x1 = io(fd, CREATEPROPBLOB, &cb);
    struct blob gb = {cb.blob_id, 8, (uintptr_t)back};
    int x2 = io(fd, GETPROPBLOB, &gb);
    uint32_t id = cb.blob_id;
    int x3 = io(fd, DESTROYPROPBLOB, &id), x4 = io(fd, DESTROYPROPBLOB, &id);
    struct create_blob c0 = {(uintptr_t)data, 0, 0};
    int x5 = io(fd, CREATEPROPBLOB, &c0);
    snprintf(buf, sizeof buf, "user blobs (create %d, get %d same %d, destroy %d, again %d, length 0 %d)", x1, x2, !memcmp(data, back, 8), x3, x4, x5);
    check(buf, x1 == 0 && x2 == 0 && !memcmp(data, back, 8) && x3 == 0 && x4 == -ENOENT && x5 == -EINVAL);
  }

  // master
  {
    uint32_t magic = 0;
    int a = io(fd, GET_MAGIC, &magic);
    int b = io(fd, AUTH_MAGIC, &magic), c = io(fd, AUTH_MAGIC, &magic);
    int d = io(fd, DROP_MASTER, NULL);
    struct crtc cr = {.crtc_id = crtc_id, .fb_id = 0xffffffff};
    int e = io(fd, SETCRTC, &cr);
    int f = io(fd, DROP_MASTER, NULL);
    int g = io(fd, SET_MASTER, NULL), h = io(fd, SET_MASTER, NULL);
    snprintf(buf, sizeof buf, "master (GET_MAGIC %d magic %d, AUTH_MAGIC %d, again %d, DROP_MASTER %d, SETCRTC %d, DROP again %d, SET_MASTER %d %d)", a, magic > 0, b, c, d, e, f, g, h);
    check(buf, a == 0 && magic > 0 && b == 0 && c == -EINVAL && d == 0 && e == -EACCES && f == -EINVAL && g == 0 && h == 0);
  }

  // rmfb
  {
    uint32_t id = fb1;
    int a = io(fd, RMFB, &id);
    struct crtc g = {.crtc_id = crtc_id};
    io(fd, GETCRTC, &g);
    struct encoder en = {.encoder_id = enc_id};
    io(fd, GETENCODER, &en);
    int b = io(fd, RMFB, &id);
    uint32_t cf[2] = {fb2, 0};
    int c = io(fd, CLOSEFB, cf), d = io(fd, CLOSEFB, cf);
    uint32_t h = d1.handle;
    int e = io(fd, DESTROY_DUMB, &h), f = io(fd, DESTROY_DUMB, &h);
    struct plane pl = {.plane_id = 33};
    io(fd, GETPLANE, &pl);
    struct page_flip pf = {.crtc_id = crtc_id, .fb_id = fb2};
    int fl = io(fd, PAGE_FLIP, &pf);
    snprintf(buf, sizeof buf, "RMFB the shown fb (%d: crtc fb %u mode %u, encoder crtc %u, plane crtc %u fb %u, flip %d, again %d), CLOSEFB (%d, again %d), DESTROY_DUMB (%d, again %d)", a, g.fb_id,
             g.mode_valid, en.crtc_id, pl.crtc_id, pl.fb_id, fl, b, c, d, e, f);
    check(buf, a == 0 && g.fb_id == 0 && b == -ENOENT && c == 0 && d == -ENOENT && e == 0 && f == -EINVAL);
  }
  close(fd);
  printf("drmkms: %d failed\n", fails);
  return fails != 0;
}
