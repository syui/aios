// DRM の render の口 (/dev/dri/renderD128: virtio-gpu の 3D) の ioctl が Linux と同じ答えになるか (kernel/src/drm.rs):
//   node       fstat の番号 (226:128)、/proc/self/fd の名前
//   version    VERSION の名前・日付・説明と版。置き場が小さいときは切って、長さは本当の長さ
//   errors     'd' でない ioctl は ENOTTY、ドライバのない番号は EINVAL、KMS と SET_CLIENT_CAP と GET_UNIQUE と昔の
//              UPDATE_DRAW (0x3f) は render の口では EACCES
//   params     GET_CAP、GETPARAM、GET_CAPS (capset の大きさ。中身はホストのもの)
//   bo         RESOURCE_CREATE のハンドル (いちばん小さい空いた番号)、RESOURCE_INFO、MAP、mmap のきまり (オフセットは BO の
//              はじめ、大きさは BO まで)、GEM_CLOSE
//   transfer   BO に書いて TRANSFER_TO_HOST、消して TRANSFER_FROM_HOST と WAIT で、ホストから同じものが返る
//   wait       WAIT の flags (Linux 6.18 は見ない) とないハンドル (ENOENT)
//   exec       EXECBUFFER の終わりの印 (sync_file: poll で POLLIN、anon_inode:sync_file、O_CLOEXEC、FILE_INFO、MERGE)、
//              まちがった flags、ないハンドル、sync_file でない前の印
// render の口がなければ (3D のない VM) 何もしない。1 行に ok NAME か FAIL NAME、おしまいに drmrender: N failed
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

#define IOWR(nr, sz) (0xc0000000u | ((unsigned)(sz) << 16) | ('d' << 8) | (nr))
#define IOW(nr, sz) (0x40000000u | ((unsigned)(sz) << 16) | ('d' << 8) | (nr))

struct version {
  int major, minor, patch;
  size_t name_len;
  char *name;
  size_t date_len;
  char *date;
  size_t desc_len;
  char *desc;
};
struct cap {
  uint64_t cap, value;
};
struct handle {
  uint32_t handle, pad;
};
struct getparam {
  uint64_t param, value;
};
struct getcaps {
  uint32_t id, ver;
  uint64_t addr;
  uint32_t size, pad;
};
struct create {
  uint32_t target, format, bind, width, height, depth, array_size, last_level, nr_samples, flags, bo_handle, res_handle, size, stride;
};
struct info {
  uint32_t bo_handle, res_handle, size, blob_mem;
};
struct map {
  uint64_t offset;
  uint32_t handle, pad;
};
struct xfer {
  uint32_t bo_handle, x, y, z, w, h, d, level, offset, stride, layer_stride;
};
struct wait {
  uint32_t handle, flags;
};
struct exec {
  uint32_t flags, size;
  uint64_t command, bo_handles;
  uint32_t num_bo_handles;
  int32_t fence_fd;
  uint32_t ring_idx, syncobj_stride, num_in_syncobjs, num_out_syncobjs;
  uint64_t in_syncobjs, out_syncobjs;
};
struct merge {
  char name[32];
  int32_t fd2, fence;
  uint32_t flags, pad;
};
struct fileinfo {
  char name[32];
  int32_t status;
  uint32_t flags, num_fences, pad;
  uint64_t info;
};

#define VERSION IOWR(0x00, sizeof(struct version))
#define GET_UNIQUE IOWR(0x01, 16)
#define GEM_CLOSE IOW(0x09, sizeof(struct handle))
#define GET_CAP IOWR(0x0c, sizeof(struct cap))
#define SET_CLIENT_CAP IOW(0x0d, 16)
#define MODE_GETRESOURCES IOWR(0xa0, 64)
#define MAP IOWR(0x41, sizeof(struct map))
#define EXECBUFFER IOWR(0x42, sizeof(struct exec))
#define GETPARAM IOWR(0x43, sizeof(struct getparam))
#define CREATE IOWR(0x44, sizeof(struct create))
#define INFO IOWR(0x45, sizeof(struct info))
#define FROM_HOST IOWR(0x46, sizeof(struct xfer))
#define TO_HOST IOWR(0x47, sizeof(struct xfer))
#define WAIT IOWR(0x48, sizeof(struct wait))
#define GET_CAPS IOWR(0x49, sizeof(struct getcaps))
#define SYNC_MERGE 0xc0303e03u
#define SYNC_FILE_INFO 0xc0383e04u

static int fails;

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

// ioctl の答え: 0 か -errno
static int io(int fd, unsigned long req, void *arg) {
  return ioctl(fd, req, arg) == 0 ? 0 : -errno;
}

// PIPE_BUFFER (R8_UNORM) の資源。bind は VIRGL_BIND_VERTEX_BUFFER (ホストに GL のバッファができる) か
// VIRGL_BIND_STAGING (Mesa の virgl が写すのに使う。ホストには中身がなく、ゲストのページだけ)
static int create(int fd, uint32_t size, uint32_t bind, struct create *c) {
  memset(c, 0, sizeof *c);
  c->target = 0;
  c->format = 64;
  c->bind = bind;
  c->width = size;
  c->height = 1;
  c->depth = 1;
  c->array_size = 1;
  c->size = size;
  return io(fd, CREATE, c);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    printf("drmrender: no /dev/dri/renderD128 (no 3D virtio-gpu): skipped\n");
    return 0;
  }
  char name[256], buf[256];

  // node
  {
    struct stat st;
    fstat(fd, &st);
    snprintf(buf, sizeof buf, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(buf, name, sizeof name - 1);
    name[n > 0 ? n : 0] = 0;
    snprintf(buf, sizeof buf, "node (chr %u:%u, %s)", major(st.st_rdev), minor(st.st_rdev), name);
    check(buf, S_ISCHR(st.st_mode) && major(st.st_rdev) == 226 && minor(st.st_rdev) == 128 && !strcmp(name, "/dev/dri/renderD128"));
  }

  // version
  {
    struct version v = {0};
    int r1 = io(fd, VERSION, &v);
    char n[32] = {0}, d[32] = {0}, s[32] = {0};
    v.name = n;
    v.date = d;
    v.desc = s;
    int r2 = io(fd, VERSION, &v);
    snprintf(buf, sizeof buf, "version (%d.%d.%d, %s %zu, %s %zu, %s %zu)", v.major, v.minor, v.patch, n, v.name_len, d, v.date_len, s, v.desc_len);
    check(buf, r1 == 0 && r2 == 0 && !strcmp(n, "virtio_gpu") && v.name_len == 10);
    char small[8] = {0};
    struct version w = {.name_len = 3, .name = small};
    int r3 = io(fd, VERSION, &w);
    snprintf(buf, sizeof buf, "version with a small buffer (%s, %zu)", small, w.name_len);
    check(buf, r3 == 0 && !strcmp(small, "vir") && w.name_len == 10);
  }

  // errors
  {
    char z[64] = {0};
    int a = io(fd, TCGETS, z);
    int b = io(fd, IOWR(0x3f, 8), z);
    int c = io(fd, IOWR(0x5f, 8), z);
    int d = io(fd, MODE_GETRESOURCES, z);
    int e = io(fd, SET_CLIENT_CAP, z);
    int f = io(fd, GET_UNIQUE, z);
    snprintf(buf, sizeof buf, "errors (TCGETS %d, UPDATE_DRAW %d, driver 0x5f %d, KMS %d, SET_CLIENT_CAP %d, GET_UNIQUE %d)", a, b, c, d, e, f);
    check(buf, a == -ENOTTY && b == -EACCES && c == -EINVAL && d == -EACCES && e == -EACCES && f == -EACCES);
  }

  // params
  {
    struct cap c = {.cap = 6};
    int r = io(fd, GET_CAP, &c);
    snprintf(buf, sizeof buf, "GET_CAP TIMESTAMP_MONOTONIC (%d, %llu)", r, (unsigned long long)c.value);
    check(buf, r == 0 && c.value == 1);
    int v[9] = {0};
    char s[200] = "";
    for (int p = 1; p <= 7; p++) {
      v[p] = -1;
      struct getparam g = {.param = p, .value = (uintptr_t)&v[p]};
      int e = io(fd, GETPARAM, &g);
      snprintf(s + strlen(s), sizeof s - strlen(s), " %d", e ? e : v[p]);
    }
    int bad = -1;
    struct getparam g = {.param = 100, .value = (uintptr_t)&bad};
    int e = io(fd, GETPARAM, &g);
    snprintf(buf, sizeof buf, "GETPARAM 1-7 (%s), 100 (%d)", s + 1, e);
    check(buf, v[1] == 1 && v[2] == 1 && v[7] == 6 && e == -EINVAL);

    static unsigned char caps[4096];
    struct getcaps gc = {.id = 2, .ver = 0, .addr = (uintptr_t)caps, .size = sizeof caps};
    int r1 = io(fd, GET_CAPS, &gc);
    size_t n = sizeof caps;
    while (n > 0 && caps[n - 1] == 0)
      n--;
    struct getcaps g2 = {.id = 99, .addr = (uintptr_t)caps, .size = 4};
    struct getcaps g3 = {.id = 2, .addr = (uintptr_t)caps, .size = 0};
    struct getcaps g4 = {.id = 2, .ver = 9, .addr = (uintptr_t)caps, .size = 4};
    int r2 = io(fd, GET_CAPS, &g2), r3 = io(fd, GET_CAPS, &g3), r4 = io(fd, GET_CAPS, &g4);
    // 中身はホストのもの (QEMU を起こすたびに少し変わる) なので、長さだけ
    snprintf(buf, sizeof buf, "GET_CAPS VIRGL2 (%d, %zu bytes up to the last nonzero), id 99 %d, size 0 %d, version 9 %d", r1, n, r2, r3, r4);
    check(buf, r1 == 0 && n > 0 && n <= 1384 && r2 == -EINVAL && r3 == -EINVAL && r4 == -EINVAL);
  }

  // bo
  struct create c1, c2;
  void *m = MAP_FAILED;
  struct map mp = {0};
  {
    int r1 = create(fd, 8192, 1u << 4, &c1), r2 = create(fd, 100, 1u << 19, &c2);
    snprintf(buf, sizeof buf, "RESOURCE_CREATE (%d %d, handles %u %u)", r1, r2, c1.bo_handle, c2.bo_handle);
    check(buf, r1 == 0 && r2 == 0 && c1.bo_handle == 1 && c2.bo_handle == 2 && c1.res_handle != 0 && c2.res_handle != c1.res_handle);
    struct info i1 = {.bo_handle = 1}, i2 = {.bo_handle = 2}, i9 = {.bo_handle = 9};
    int e1 = io(fd, INFO, &i1), e2 = io(fd, INFO, &i2), e9 = io(fd, INFO, &i9);
    snprintf(buf, sizeof buf, "RESOURCE_INFO (sizes %u %u, same res %d, blob %u, none %d)", i1.size, i2.size, i1.res_handle == c1.res_handle, i1.blob_mem, e9);
    check(buf, e1 == 0 && e2 == 0 && i1.size == 8192 && i2.size == 4096 && i1.res_handle == c1.res_handle && e9 == -ENOENT);

    mp.handle = 1;
    struct map m2 = {.handle = 1}, m9 = {.handle = 9};
    int a = io(fd, MAP, &mp), b = io(fd, MAP, &m2), e = io(fd, MAP, &m9);
    snprintf(buf, sizeof buf, "MAP (%d, offset >= 4 GiB %d, page aligned %d, again the same %d, none %d)", a, mp.offset >= (1ull << 32), (mp.offset & 4095) == 0, mp.offset == m2.offset, e);
    check(buf, a == 0 && b == 0 && mp.offset >= (1ull << 32) && (mp.offset & 4095) == 0 && mp.offset == m2.offset && e == -ENOENT);

    m = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, mp.offset);
    void *x1 = mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, mp.offset + 4096);
    int e1b = x1 == MAP_FAILED ? errno : 0;
    void *x2 = mmap(NULL, 16384, PROT_READ, MAP_SHARED, fd, mp.offset);
    int e2b = x2 == MAP_FAILED ? errno : 0;
    void *x3 = mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, 0x77770000000ull);
    int e3b = x3 == MAP_FAILED ? errno : 0;
    snprintf(buf, sizeof buf, "mmap (BO %s, middle of the BO %d, bigger than the BO %d, unknown offset %d)", m == MAP_FAILED ? "failed" : "ok", e1b, e2b, e3b);
    check(buf, m != MAP_FAILED && e1b == EINVAL && e2b == EINVAL && e3b == EINVAL);
  }

  // transfer
  if (m != MAP_FAILED) {
    unsigned char *p = m;
    for (int i = 0; i < 8192; i++)
      p[i] = (unsigned char)(i * 7 + 3);
    struct xfer t = {.bo_handle = 1, .w = 8192, .h = 1, .d = 1};
    int a = io(fd, TO_HOST, &t);
    memset(p, 0, 8192);
    struct xfer f = t;
    int b = io(fd, FROM_HOST, &f);
    struct wait w = {.handle = 1};
    int c = io(fd, WAIT, &w);
    int same = 1;
    for (int i = 0; i < 8192; i++)
      same &= p[i] == (unsigned char)(i * 7 + 3);
    struct xfer bad = t;
    bad.stride = 1;
    int d = io(fd, TO_HOST, &bad);
    struct xfer none = t;
    none.bo_handle = 9;
    int e = io(fd, TO_HOST, &none);
    snprintf(buf, sizeof buf, "transfer of a vertex buffer (to host %d, from host %d, wait %d, same bytes %d, stride %d, none %d)", a, b, c, same, d, e);
    check(buf, a == 0 && b == 0 && c == 0 && same && d == -EINVAL && e == -ENOENT);
  }

  // wait
  {
    struct wait a = {.handle = 1, .flags = 2}, b = {.handle = 9}, c = {.handle = 9, .flags = 1}, d = {.handle = 2, .flags = 1};
    int ra = io(fd, WAIT, &a), rb = io(fd, WAIT, &b), rc = io(fd, WAIT, &c), rd = io(fd, WAIT, &d);
    snprintf(buf, sizeof buf, "WAIT (flags 2 %d, none %d, none NOWAIT %d, idle NOWAIT %d)", ra, rb, rc, rd);
    check(buf, ra == 0 && rb == -ENOENT && rc == -ENOENT && rd == 0);
  }

  // exec
  {
    uint32_t handles[2] = {1, 2};
    struct exec e = {.flags = 2, .size = 0, .command = (uintptr_t)buf, .bo_handles = (uintptr_t)handles, .num_bo_handles = 2, .fence_fd = -1};
    int r = io(fd, EXECBUFFER, &e);
    int ff = e.fence_fd;
    struct pollfd pf = {.fd = ff, .events = POLLIN};
    int pr = poll(&pf, 1, 2000);
    snprintf(buf, sizeof buf, "/proc/self/fd/%d", ff);
    ssize_t n = readlink(buf, name, sizeof name - 1);
    name[n > 0 ? n : 0] = 0;
    int cloexec = fcntl(ff, F_GETFD) & FD_CLOEXEC;
    struct fileinfo fi = {0};
    int ri = io(ff, SYNC_FILE_INFO, &fi);
    snprintf(buf, sizeof buf, "EXECBUFFER with an out fence (%d, poll %d %#x, %s, cloexec %d, info %d status %d fences %u)", r, pr, pf.revents, name, !!cloexec, ri, fi.status, fi.num_fences);
    check(buf, r == 0 && ff >= 0 && pr == 1 && pf.revents == POLLIN && !strcmp(name, "anon_inode:sync_file") && cloexec && ri == 0 && fi.status == 1 && fi.num_fences == 1);

    struct exec e2 = e;
    e2.flags = 3;
    e2.fence_fd = ff;
    int r2 = io(fd, EXECBUFFER, &e2);
    struct merge mg = {.name = "two", .fd2 = e2.fence_fd};
    int rm = io(ff, SYNC_MERGE, &mg);
    struct fileinfo f2 = {0};
    int ri2 = io(mg.fence, SYNC_FILE_INFO, &f2);
    snprintf(buf, sizeof buf, "fence in and MERGE (%d, merge %d, name %s, status %d, fences %u)", r2, rm, f2.name, f2.status, f2.num_fences);
    check(buf, r2 == 0 && rm == 0 && ri2 == 0 && !strcmp(f2.name, "two") && f2.status == 1 && f2.num_fences == 1);
    close(mg.fence);
    close(e2.fence_fd);
    close(ff);

    struct exec b1 = e, b2 = e, b3 = e;
    b1.flags = 0x80;
    uint32_t none = 9;
    b2.flags = 0;
    b2.bo_handles = (uintptr_t)&none;
    b2.num_bo_handles = 1;
    b3.flags = 1;
    b3.fence_fd = 0;
    int x1 = io(fd, EXECBUFFER, &b1), x2 = io(fd, EXECBUFFER, &b2), x3 = io(fd, EXECBUFFER, &b3);
    snprintf(buf, sizeof buf, "EXECBUFFER errors (flags 0x80 %d, no such handle %d, in fence not a sync_file %d)", x1, x2, x3);
    check(buf, x1 == -EINVAL && x2 == -ENOENT && x3 == -EINVAL);
  }

  // GEM_CLOSE: ハンドルはいちばん小さい空いた番号にもどる
  {
    if (m != MAP_FAILED)
      munmap(m, 8192);
    struct handle h = {.handle = 1};
    int a = io(fd, GEM_CLOSE, &h), b = io(fd, GEM_CLOSE, &h);
    struct create c3;
    int r = create(fd, 4096, 1u << 19, &c3);
    snprintf(buf, sizeof buf, "GEM_CLOSE (%d, again %d, next handle %u)", a, b, c3.bo_handle);
    check(buf, a == 0 && b == -EINVAL && r == 0 && c3.bo_handle == 1);
  }
  close(fd);
  printf("drmrender: %d failed\n", fails);
  return fails != 0;
}
