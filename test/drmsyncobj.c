// DRM の syncobj (kernel/src/syncobj.rs) の ioctl が Linux と同じ答えになるか (render の口。なければ card0):
//   caps       GET_CAP の SYNCOBJ と SYNCOBJ_TIMELINE、card0 でも使える
//   create     CREATE (SIGNALED)、DESTROY、ハンドルはいちばん小さい空いた番号、まちがった flags と pad
//   wait       WAIT: 入っていないものは EINVAL、WAIT_FOR_SUBMIT と WAIT_ALL、first_signaled、時間切れ (ETIME)、ほかの
//              プロセスが SIGNAL するまで待つ、count 0、ないハンドル、まちがった flags
//   signal     SIGNAL と RESET (count 0、pad、ないハンドル)
//   fd         HANDLE_TO_FD と FD_TO_HANDLE (anon_inode:syncobj_file、O_CLOEXEC、同じ箱、エラー)
//   sync_file  EXPORT_SYNC_FILE と IMPORT_SYNC_FILE (FILE_INFO の名前と中の fence、エラー)
//   timeline   TIMELINE_SIGNAL、QUERY (LAST_SUBMITTED)、TIMELINE_WAIT (WAIT_AVAILABLE)、点を出す (前の点は stub)、
//              順でない点、2 値の箱に点を足す
//   transfer   TRANSFER (timeline から 2 値へ、2 値から timeline へ、WAIT_FOR_SUBMIT、エラー)
//   eventfd    EVENTFD (入ったら 1、timeline の点、エラー)
//   exec       EXECBUFFER の out と in の syncobj (3D の virtio-gpu だけ: 2D では ENOSYS なので見ない)
// 1 行に ok NAME か FAIL NAME、おしまいに drmsyncobj: N failed
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define IOWR(nr, sz) (0xc0000000u | ((unsigned)(sz) << 16) | ('d' << 8) | (nr))

struct cap {
  uint64_t cap, value;
};
struct create {
  uint32_t handle, flags;
};
struct sohandle {
  uint32_t handle, flags;
  int32_t fd;
  uint32_t pad;
  uint64_t point;
};
struct sowait {
  uint64_t handles;
  int64_t timeout_nsec;
  uint32_t count_handles, flags, first_signaled, pad;
  uint64_t deadline_nsec;
};
struct tlwait {
  uint64_t handles, points;
  int64_t timeout_nsec;
  uint32_t count_handles, flags, first_signaled, pad;
  uint64_t deadline_nsec;
};
struct soarray {
  uint64_t handles;
  uint32_t count_handles, pad;
};
struct tlarray {
  uint64_t handles, points;
  uint32_t count_handles, flags;
};
struct transfer {
  uint32_t src_handle, dst_handle;
  uint64_t src_point, dst_point;
  uint32_t flags, pad;
};
struct soeventfd {
  uint32_t handle, flags;
  uint64_t point;
  int32_t fd;
  uint32_t pad;
};
struct exec {
  uint32_t flags, size;
  uint64_t command, bo_handles;
  uint32_t num_bo_handles;
  int32_t fence_fd;
  uint32_t ring_idx, syncobj_stride, num_in_syncobjs, num_out_syncobjs;
  uint64_t in_syncobjs, out_syncobjs;
};
struct execso {
  uint32_t handle, flags;
  uint64_t point;
};
struct fenceinfo {
  char obj_name[32], driver_name[32];
  int32_t status;
  uint32_t flags;
  uint64_t timestamp_ns;
};
struct fileinfo {
  char name[32];
  int32_t status;
  uint32_t flags, num_fences, pad;
  uint64_t info;
};

#define GET_CAP IOWR(0x0c, sizeof(struct cap))
#define EXECBUFFER IOWR(0x42, sizeof(struct exec))
#define SO_CREATE IOWR(0xbf, sizeof(struct create))
#define SO_DESTROY IOWR(0xc0, sizeof(struct create))
#define SO_HANDLE_TO_FD IOWR(0xc1, sizeof(struct sohandle))
#define SO_FD_TO_HANDLE IOWR(0xc2, sizeof(struct sohandle))
#define SO_WAIT IOWR(0xc3, sizeof(struct sowait))
#define SO_RESET IOWR(0xc4, sizeof(struct soarray))
#define SO_SIGNAL IOWR(0xc5, sizeof(struct soarray))
#define SO_TIMELINE_WAIT IOWR(0xca, sizeof(struct tlwait))
#define SO_QUERY IOWR(0xcb, sizeof(struct tlarray))
#define SO_TRANSFER IOWR(0xcc, sizeof(struct transfer))
#define SO_TIMELINE_SIGNAL IOWR(0xcd, sizeof(struct tlarray))
#define SO_EVENTFD IOWR(0xcf, sizeof(struct soeventfd))
#define SYNC_FILE_INFO 0xc0383e04u

#define WAIT_ALL 1
#define WAIT_FOR_SUBMIT 2
#define WAIT_AVAILABLE 4

static int fails;
static char buf[512];
// 終わった fence の名前 (Linux 6.17 から: dma_fence_driver_name と dma_fence_timeline_name)。sync_file の名前は 31 文字に切れる
#define DONE_NAME "detached-driver-signaled-timeli"
#define DONE_OBJ "signaled-timeline"
#define DONE_DRV "detached-driver"

static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static int io(int fd, unsigned long req, void *arg) {
  return ioctl(fd, req, arg) == 0 ? 0 : -errno;
}

static int64_t now_ns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

static uint32_t mk(int fd, uint32_t flags) {
  struct create c = {.flags = flags};
  return io(fd, SO_CREATE, &c) == 0 ? c.handle : 0;
}

// WAIT を n このハンドルで (first は first_signaled)
static int waitn(int fd, uint32_t *h, uint32_t n, uint32_t flags, int64_t timeout, uint32_t *first) {
  struct sowait w = {.handles = (uintptr_t)h, .timeout_nsec = timeout, .count_handles = n, .flags = flags, .first_signaled = 77};
  int r = io(fd, SO_WAIT, &w);
  if (first)
    *first = w.first_signaled;
  return r;
}

static int wait1(int fd, uint32_t h) {
  return waitn(fd, &h, 1, 0, 0, NULL);
}

static int tlwait1(int fd, uint32_t h, uint64_t point, uint32_t flags, int64_t timeout) {
  struct tlwait w = {.handles = (uintptr_t)&h, .points = (uintptr_t)&point, .timeout_nsec = timeout, .count_handles = 1, .flags = flags};
  return io(fd, SO_TIMELINE_WAIT, &w);
}

static int signal1(int fd, uint32_t h, unsigned long req) {
  struct soarray a = {.handles = (uintptr_t)&h, .count_handles = 1};
  return io(fd, req, &a);
}

static int tlsignal1(int fd, uint32_t h, uint64_t point) {
  struct tlarray a = {.handles = (uintptr_t)&h, .points = (uintptr_t)&point, .count_handles = 1};
  return io(fd, SO_TIMELINE_SIGNAL, &a);
}

// QUERY (答えの点。しくじったら -errno を point に)
static int64_t query1(int fd, uint32_t h, uint32_t flags) {
  uint64_t p = 99;
  struct tlarray a = {.handles = (uintptr_t)&h, .points = (uintptr_t)&p, .count_handles = 1, .flags = flags};
  int r = io(fd, SO_QUERY, &a);
  return r ? r : (int64_t)p;
}

// 箱の fence を sync_file に (点は TIMELINE で)。fd か -errno
static int export_sf(int fd, uint32_t h, uint64_t point) {
  struct sohandle a = {.handle = h, .flags = 1 | (point ? 2 : 0), .fd = -1, .point = point};
  int r = io(fd, SO_HANDLE_TO_FD, &a);
  return r ? r : a.fd;
}

// sync_file の FILE_INFO: 名前と中の fence の数、はじめの fence の obj_name と driver_name
static int sfinfo(int sf, struct fileinfo *fi, struct fenceinfo *fe) {
  memset(fi, 0, sizeof *fi);
  memset(fe, 0, 4 * sizeof *fe);
  fi->num_fences = 4;
  fi->info = (uintptr_t)fe;
  return io(sf, SYNC_FILE_INFO, fi);
}

static void fdname(int fd, char *out, size_t len) {
  char p[64];
  snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
  ssize_t n = readlink(p, out, len - 1);
  out[n > 0 ? n : 0] = 0;
}

// 子のプロセスが ms ミリ秒あとに、ハンドル h を SIGNAL (point が 0 でなければ TIMELINE_SIGNAL) する
static pid_t later(int fd, uint32_t h, uint64_t point, int ms) {
  pid_t p = fork();
  if (p == 0) {
    usleep(ms * 1000);
    _exit(point ? tlsignal1(fd, h, point) != 0 : signal1(fd, h, SO_SIGNAL) != 0);
  }
  return p;
}

static int reap(pid_t p) {
  int st = 0;
  waitpid(p, &st, 0);
  return WIFEXITED(st) ? WEXITSTATUS(st) : 99;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // 2 値の箱の HANDLE_TO_FD と FD_TO_HANDLE に点をわたすと、Linux 7.4 は EINVAL (c2d3a7300695)。手本の Alpine の 6.18.64 は
  // 0: stable に入った 2e3649e23723 (TIMELINE がなければ点を見ない) を、本流は ee8ade4d9678 でもどした。aios は本流と同じ
  struct utsname un;
  int old_point = uname(&un) == 0 && !strcmp(un.sysname, "Linux") && un.release[0] == '6' && un.release[1] == '.';
  int point_err = old_point ? 0 : -EINVAL;
  const char *dev = "/dev/dri/renderD128";
  int fd = open(dev, O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    dev = "/dev/dri/card0";
    fd = open(dev, O_RDWR | O_CLOEXEC);
  }
  if (fd < 0) {
    printf("drmsyncobj: no /dev/dri (no virtio-gpu): skipped\n");
    return 0;
  }

  // caps
  {
    struct cap a = {.cap = 0x13}, b = {.cap = 0x14};
    int ra = io(fd, GET_CAP, &a), rb = io(fd, GET_CAP, &b);
    int c0 = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    uint32_t hc = c0 >= 0 ? mk(c0, 0) : 0;
    snprintf(buf, sizeof buf, "GET_CAP SYNCOBJ %d %llu, TIMELINE %d %llu; CREATE on card0 handle %u", ra, (unsigned long long)a.value, rb,
             (unsigned long long)b.value, hc);
    check(buf, ra == 0 && a.value == 1 && rb == 0 && b.value == 1 && hc == 1);
    if (c0 >= 0)
      close(c0);
  }

  // create
  uint32_t h1 = mk(fd, 0), h2 = mk(fd, 1);
  {
    struct create bad = {.flags = 2};
    int rb = io(fd, SO_CREATE, &bad);
    struct create d = {.handle = h1}, dp = {.handle = h2, .flags = 1};
    int r1 = io(fd, SO_DESTROY, &d), r2 = io(fd, SO_DESTROY, &d), r3 = io(fd, SO_DESTROY, &dp);
    uint32_t again = mk(fd, 0);
    snprintf(buf, sizeof buf, "CREATE %u, SIGNALED %u, flags 2 %d; DESTROY %d, again %d, pad %d; next %u", h1, h2, rb, r1, r2, r3, again);
    check(buf, h1 == 1 && h2 == 2 && rb == -EINVAL && r1 == 0 && r2 == -EINVAL && r3 == -EINVAL && again == 1);
    h1 = again;
  }

  // wait
  {
    uint32_t both[2] = {h1, h2}, none[1] = {9};
    uint32_t f1 = 0, f2 = 0, f3 = 0;
    int a = wait1(fd, h1), b = wait1(fd, h2);
    int c = waitn(fd, both, 2, 0, 0, NULL);
    int d = waitn(fd, both, 2, WAIT_FOR_SUBMIT, 0, &f1);
    int e = waitn(fd, both, 2, WAIT_ALL | WAIT_FOR_SUBMIT, 0, &f2);
    int g = waitn(fd, both, 0, 0, 0, NULL);
    int i = waitn(fd, none, 1, 0, 0, NULL);
    int j = waitn(fd, both, 2, 0x10, 0, NULL);
    int k = waitn(fd, (uint32_t *)8, 2, 0, 0, NULL);
    int l = waitn(fd, &h2, 1, 0, 0, &f3);
    snprintf(buf, sizeof buf, "WAIT (empty %d, signaled %d, both %d, SUBMIT %d first %u, ALL|SUBMIT %d, count 0 %d, no handle %d, flags 0x10 %d, bad ptr %d, first %u)", a,
             b, c, d, f1, e, g, i, j, k, f3);
    check(buf, a == -EINVAL && b == 0 && c == -EINVAL && d == 0 && f1 == 1 && e == -ETIME && g == 0 && i == -ENOENT && j == -EINVAL && k == -EFAULT && l == 0 && f3 == 0);

    int64_t t0 = now_ns();
    int t = waitn(fd, both, 2, WAIT_ALL | WAIT_FOR_SUBMIT, t0 + 100000000, NULL);
    int64_t took = now_ns() - t0;
    snprintf(buf, sizeof buf, "WAIT ALL|SUBMIT until 100 ms (%d, %s)", t, took >= 90000000 ? "waited" : "early");
    check(buf, t == -ETIME && took >= 90000000);

    pid_t p = later(fd, h1, 0, 100);
    uint32_t f4 = 0;
    int w = waitn(fd, both, 2, WAIT_ALL | WAIT_FOR_SUBMIT, now_ns() + 5000000000LL, &f4);
    int cs = reap(p);
    // はじめに見たときにあったもの (h2) が first_signaled。はじめになければ ~0 のまま
    signal1(fd, h1, SO_RESET);
    pid_t p2 = later(fd, h1, 0, 100);
    uint32_t f5 = 0;
    int w2 = waitn(fd, &h1, 1, WAIT_ALL | WAIT_FOR_SUBMIT, now_ns() + 5000000000LL, &f5);
    int cs2 = reap(p2);
    snprintf(buf, sizeof buf, "WAIT ALL|SUBMIT for another process's SIGNAL (%d first %#x child %d; only the empty one %d first %#x child %d)", w, f4, cs, w2, f5, cs2);
    check(buf, w == 0 && f4 == 1 && cs == 0 && w2 == 0 && f5 == 0xffffffffu && cs2 == 0);
  }

  // signal
  {
    int r = signal1(fd, h1, SO_RESET), a = wait1(fd, h1);
    int s = signal1(fd, h1, SO_SIGNAL), b = wait1(fd, h1);
    struct soarray z = {.handles = (uintptr_t)&h1}, pad = {.handles = (uintptr_t)&h1, .count_handles = 1, .pad = 1};
    int c = io(fd, SO_SIGNAL, &z), d = io(fd, SO_RESET, &z), e = io(fd, SO_SIGNAL, &pad), f = signal1(fd, 9, SO_RESET);
    snprintf(buf, sizeof buf, "RESET %d then WAIT %d, SIGNAL %d then WAIT %d; count 0 %d %d, pad %d, no handle %d", r, a, s, b, c, d, e, f);
    check(buf, r == 0 && a == -EINVAL && s == 0 && b == 0 && c == -EINVAL && d == -EINVAL && e == -EINVAL && f == -ENOENT);
  }

  // fd
  {
    struct sohandle x = {.handle = h2, .fd = -1};
    int r = io(fd, SO_HANDLE_TO_FD, &x);
    char name[64];
    fdname(x.fd, name, sizeof name);
    int cloexec = fcntl(x.fd, F_GETFD) & FD_CLOEXEC;
    struct sohandle y = {.fd = x.fd};
    int r2 = io(fd, SO_FD_TO_HANDLE, &y);
    // 同じ箱: こちらで RESET すると、もとのハンドルでも空
    int a = signal1(fd, y.handle, SO_RESET), b = wait1(fd, h2);
    signal1(fd, h2, SO_SIGNAL);
    int c = wait1(fd, y.handle);
    snprintf(buf, sizeof buf, "HANDLE_TO_FD %d (%s, cloexec %d), FD_TO_HANDLE %d handle %u, same syncobj (%d %d %d)", r, name, !!cloexec, r2, y.handle, a, b, c);
    check(buf, r == 0 && !strcmp(name, "anon_inode:syncobj_file") && cloexec && r2 == 0 && y.handle == 3 && a == 0 && b == -EINVAL && c == 0);

    struct sohandle e1 = {.handle = h2, .point = 1}, e2 = {.handle = h2, .pad = 1}, e3 = {.handle = h2, .flags = 4}, e4 = {.handle = 9}, e5 = {.handle = h2, .flags = 2, .point = 1};
    struct sohandle i1 = {.fd = fd}, i2 = {.fd = -1}, i3 = {.fd = x.fd, .point = 1}, i4 = {.fd = x.fd, .flags = 4};
    int r1 = io(fd, SO_HANDLE_TO_FD, &e1), r3 = io(fd, SO_HANDLE_TO_FD, &e2), r4 = io(fd, SO_HANDLE_TO_FD, &e3), r5 = io(fd, SO_HANDLE_TO_FD, &e4),
        r6 = io(fd, SO_HANDLE_TO_FD, &e5);
    int s1 = io(fd, SO_FD_TO_HANDLE, &i1), s2 = io(fd, SO_FD_TO_HANDLE, &i2), s3 = io(fd, SO_FD_TO_HANDLE, &i3), s4 = io(fd, SO_FD_TO_HANDLE, &i4);
    snprintf(buf, sizeof buf, "HANDLE_TO_FD errors (point %d, pad %d, flags 4 %d, no handle %d, TIMELINE point %d); FD_TO_HANDLE errors (drm fd %d, -1 %d, point %d, flags 4 %d)", r1,
             r3, r4, r5, r6, s1, s2, s3, s4);
    check(buf, r1 == point_err && r3 == -EINVAL && r4 == -EINVAL && r5 == -EINVAL && r6 == -EINVAL && s1 == -EINVAL && s2 == -EINVAL && s3 == point_err && s4 == -EINVAL);
    if (r1 == 0)
      close(e1.fd);
    struct create d = {.handle = y.handle}, d3 = {.handle = i3.handle};
    io(fd, SO_DESTROY, &d);
    if (s3 == 0)
      io(fd, SO_DESTROY, &d3);
    close(x.fd);
  }

  // sync_file
  {
    int sf = export_sf(fd, h2, 0);
    char name[64];
    fdname(sf, name, sizeof name);
    struct pollfd pf = {.fd = sf, .events = POLLIN};
    int pr = poll(&pf, 1, 1000);
    struct fileinfo fi;
    struct fenceinfo fe[4];
    int ri = sfinfo(sf, &fi, fe);
    snprintf(buf, sizeof buf, "EXPORT_SYNC_FILE (%s, poll %d, info %d %s status %d fences %u %s %s)", name, pr, ri, fi.name, fi.status, fi.num_fences, fe[0].obj_name,
             fe[0].driver_name);
    check(buf, sf >= 0 && !strcmp(name, "anon_inode:sync_file") && pr == 1 && ri == 0 && !strcmp(fi.name, DONE_NAME) && fi.status == 1 && fi.num_fences == 1 &&
                   !strcmp(fe[0].obj_name, DONE_OBJ) && !strcmp(fe[0].driver_name, DONE_DRV));

    uint32_t h3 = mk(fd, 0);
    int a = wait1(fd, h3);
    struct sohandle im = {.handle = h3, .flags = 1, .fd = sf};
    int r = io(fd, SO_FD_TO_HANDLE, &im);
    int b = wait1(fd, h3);
    int x1 = export_sf(fd, h1 + 10, 0);
    signal1(fd, h3, SO_RESET);
    int x2 = export_sf(fd, h3, 0);
    struct sohandle i1 = {.handle = h3, .flags = 1, .fd = fd}, i2 = {.handle = 9, .flags = 1, .fd = sf}, i3 = {.handle = h3, .flags = 1, .fd = -1};
    int y1 = io(fd, SO_FD_TO_HANDLE, &i1), y2 = io(fd, SO_FD_TO_HANDLE, &i2), y3 = io(fd, SO_FD_TO_HANDLE, &i3);
    snprintf(buf, sizeof buf, "IMPORT_SYNC_FILE (before %d, import %d, after %d); export errors (no handle %d, empty %d); import errors (not sync_file %d, no handle %d, -1 %d)", a, r,
             b, x1, x2, y1, y2, y3);
    check(buf, a == -EINVAL && r == 0 && b == 0 && x1 == -ENOENT && x2 == -EINVAL && y1 == -EINVAL && y2 == -ENOENT && y3 == -EINVAL);
    struct create d = {.handle = h3};
    io(fd, SO_DESTROY, &d);
    close(sf);
  }

  // timeline
  {
    uint32_t t = mk(fd, 0);
    int64_t q0 = query1(fd, t, 0);
    int s3 = tlsignal1(fd, t, 3);
    int64_t q3 = query1(fd, t, 0), q3l = query1(fd, t, 1);
    int w2 = tlwait1(fd, t, 2, 0, 0), w3 = tlwait1(fd, t, 3, 0, 0), w4 = tlwait1(fd, t, 4, 0, 0), w4a = tlwait1(fd, t, 4, WAIT_AVAILABLE, 0),
        w4s = tlwait1(fd, t, 4, WAIT_FOR_SUBMIT, 0);
    int s5 = tlsignal1(fd, t, 5);
    int64_t q5 = query1(fd, t, 0);
    int s4 = tlsignal1(fd, t, 4);
    int64_t q4 = query1(fd, t, 0);
    snprintf(buf, sizeof buf, "TIMELINE_SIGNAL and QUERY (empty %lld, 3: %d %lld last %lld, then 5: %d %lld, then 4: %d %lld)", (long long)q0, s3, (long long)q3,
             (long long)q3l, s5, (long long)q5, s4, (long long)q4);
    check(buf, q0 == 0 && s3 == 0 && q3 == 3 && q3l == 3 && s5 == 0 && q5 == 5 && s4 == 0 && q4 == 5);
    snprintf(buf, sizeof buf, "TIMELINE_WAIT (2 %d, 3 %d, 4 %d, 4 AVAILABLE %d, 4 SUBMIT %d)", w2, w3, w4, w4a, w4s);
    check(buf, w2 == 0 && w3 == 0 && w4 == -EINVAL && w4a == -ETIME && w4s == -ETIME);

    // 前の点と同じ流れの点を出すと、前の点は stub
    uint32_t u = mk(fd, 0);
    tlsignal1(fd, u, 1);
    tlsignal1(fd, u, 2);
    int sa = export_sf(fd, u, 1), sb = export_sf(fd, u, 2), sc = export_sf(fd, u, 3), sd = export_sf(fd, u, 0);
    struct fileinfo fa, fb, fd0;
    struct fenceinfo ea[4], eb[4], ed[4];
    int ia = sfinfo(sa, &fa, ea), ib = sfinfo(sb, &fb, eb), id = sfinfo(sd, &fd0, ed);
    size_t nb = strlen(fb.name);
    snprintf(buf, sizeof buf, "export timeline points (1: %d %s, 2: %d %s fences %u %s, 3: %d, 0: %d %s)", ia, fa.name, ib, fb.name, fb.num_fences, eb[0].obj_name, sc, id,
             fd0.name);
    check(buf, ia == 0 && !strcmp(fa.name, DONE_NAME) && ib == 0 && !strncmp(fb.name, "dma_fence_chain-unbound", 23) && nb > 2 && !strcmp(fb.name + nb - 2, "-2") &&
                   fb.num_fences == 1 && !strcmp(eb[0].obj_name, DONE_OBJ) && sc == -EINVAL && id == 0 && !strcmp(fd0.name, fb.name));
    close(sa);
    close(sb);
    close(sd);

    // timeline の点は、終わったかを聞かれるまで終わった印が立たない (名前が dma_fence_chain のまま)。QUERY の
    // LAST_SUBMITTED は聞かず、ふつうの QUERY と poll は聞く
    int64_t ql = query1(fd, u, 1);
    int sfa = export_sf(fd, u, 2);
    struct fileinfo f1i;
    struct fenceinfo e1i[4];
    sfinfo(sfa, &f1i, e1i);
    int64_t qn = query1(fd, u, 0);
    int sfb = export_sf(fd, u, 2);
    struct fileinfo f2i;
    struct fenceinfo e2i[4];
    sfinfo(sfb, &f2i, e2i);
    uint32_t w = mk(fd, 0);
    tlsignal1(fd, w, 1);
    int sfc = export_sf(fd, w, 1);
    struct pollfd p3 = {.fd = sfc, .events = POLLIN};
    int pr3 = poll(&p3, 1, 1000);
    struct fileinfo f3i;
    struct fenceinfo e3i[4];
    sfinfo(sfc, &f3i, e3i);
    snprintf(buf, sizeof buf, "timeline point flag (LAST_SUBMITTED %lld: %s; QUERY %lld: %s; poll %d: %s)", (long long)ql, f1i.name, (long long)qn, f2i.name, pr3, f3i.name);
    check(buf, ql == 2 && !strncmp(f1i.name, "dma_fence_chain-unbound", 23) && qn == 2 && !strcmp(f2i.name, DONE_NAME) && pr3 == 1 && !strcmp(f3i.name, DONE_NAME));
    close(sfa);
    close(sfb);
    close(sfc);

    // 2 値の箱は QUERY で 0、点は EINVAL。点を足すと timeline に
    int64_t qb = query1(fd, h2, 0);
    int wb = tlwait1(fd, h2, 1, 0, 0), w0 = tlwait1(fd, h2, 0, 0, 0);
    uint32_t b2 = mk(fd, 1);
    int sb7 = tlsignal1(fd, b2, 7);
    int64_t qb7 = query1(fd, b2, 0);
    snprintf(buf, sizeof buf, "binary syncobj (QUERY %lld, point 1 %d, point 0 %d, TIMELINE_SIGNAL 7 %d then QUERY %lld)", (long long)qb, wb, w0, sb7, (long long)qb7);
    check(buf, qb == 0 && wb == -EINVAL && w0 == 0 && sb7 == 0 && qb7 == 7);

    struct tlarray f1 = {.handles = (uintptr_t)&t, .points = 0, .count_handles = 1, .flags = 1}, f2 = {.handles = (uintptr_t)&t, .count_handles = 0},
                   f3 = {.handles = (uintptr_t)&t, .points = 0, .count_handles = 1, .flags = 2}, f4 = {.handles = (uintptr_t)&t, .points = 0, .count_handles = 1};
    int e1 = io(fd, SO_TIMELINE_SIGNAL, &f1), e2 = io(fd, SO_TIMELINE_SIGNAL, &f2), e3 = io(fd, SO_QUERY, &f3), e4 = io(fd, SO_QUERY, &f4), e5 = io(fd, SO_QUERY, &f2);
    snprintf(buf, sizeof buf, "TIMELINE_SIGNAL errors (flags 1 %d, count 0 %d); QUERY errors (flags 2 %d, points NULL %d, count 0 %d)", e1, e2, e3, e4, e5);
    check(buf, e1 == -EINVAL && e2 == -EINVAL && e3 == -EINVAL && e4 == -EFAULT && e5 == -EINVAL);

    // ほかのプロセスが点を足すまで待つ (WAIT_AVAILABLE)
    pid_t p = later(fd, t, 9, 100);
    int wa = tlwait1(fd, t, 9, WAIT_AVAILABLE, now_ns() + 5000000000LL);
    int cs = reap(p);
    int64_t q9 = query1(fd, t, 0);
    snprintf(buf, sizeof buf, "TIMELINE_WAIT AVAILABLE for another process (%d, child %d, QUERY %lld)", wa, cs, (long long)q9);
    check(buf, wa == 0 && cs == 0 && q9 == 9);

    // transfer
    uint32_t b = mk(fd, 0), t2 = mk(fd, 0), e = mk(fd, 0);
    // 点 0 は timeline のいちばん新しい点 (dma_fence_chain のまま: 2 値の箱でも QUERY で見える)。前の点 5 は stub
    struct transfer x1 = {.src_handle = t, .dst_handle = b};
    int r1 = io(fd, SO_TRANSFER, &x1);
    int64_t qx = query1(fd, b, 0);
    int wx = wait1(fd, b);
    struct transfer x0 = {.src_handle = t, .dst_handle = b, .src_point = 5};
    int r0 = io(fd, SO_TRANSFER, &x0);
    int64_t q0x = query1(fd, b, 0);
    struct transfer x2 = {.src_handle = h2, .dst_handle = t2, .dst_point = 9};
    int r2 = io(fd, SO_TRANSFER, &x2);
    int64_t qy = query1(fd, t2, 0);
    int sy = export_sf(fd, t2, 9);
    struct fileinfo fy;
    struct fenceinfo ey[4];
    int iy = sfinfo(sy, &fy, ey);
    snprintf(buf, sizeof buf, "TRANSFER (timeline to binary %d QUERY %lld WAIT %d, point 5 %d QUERY %lld; binary to timeline 9 %d QUERY %lld, export %d fences %u %s)", r1,
             (long long)qx, wx, r0, (long long)q0x, r2, (long long)qy, iy, fy.num_fences, ey[0].obj_name);
    check(buf, r1 == 0 && qx == 9 && wx == 0 && r0 == 0 && q0x == 0 && r2 == 0 && qy == 9 && iy == 0 && fy.num_fences == 1 && !strcmp(ey[0].obj_name, DONE_OBJ));
    close(sy);

    struct transfer y1 = x1, y2 = x1, y3 = x1, y4 = x1, y5 = x1;
    y1.pad = 1;
    y2.dst_handle = 99;
    y3.src_handle = 99;
    y4.src_handle = e;
    y4.src_point = 0;
    y5.flags = 1;
    int z1 = io(fd, SO_TRANSFER, &y1), z2 = io(fd, SO_TRANSFER, &y2), z3 = io(fd, SO_TRANSFER, &y3), z4 = io(fd, SO_TRANSFER, &y4), z5 = io(fd, SO_TRANSFER, &y5);
    snprintf(buf, sizeof buf, "TRANSFER errors (pad %d, no dst %d, no src %d, empty src %d, flags 1 %d)", z1, z2, z3, z4, z5);
    check(buf, z1 == -EINVAL && z2 == -ENOENT && z3 == -ENOENT && z4 == -EINVAL && z5 == -EINVAL);

    // WAIT_FOR_SUBMIT の TRANSFER は、入るまで待つ
    pid_t q = later(fd, e, 0, 100);
    struct transfer ws = {.src_handle = e, .dst_handle = b, .flags = WAIT_FOR_SUBMIT};
    int rw = io(fd, SO_TRANSFER, &ws);
    int cq = reap(q);
    snprintf(buf, sizeof buf, "TRANSFER WAIT_FOR_SUBMIT for another process (%d, child %d, QUERY %lld)", rw, cq, (long long)query1(fd, b, 0));
    check(buf, rw == 0 && cq == 0 && query1(fd, b, 0) == 0);

    // eventfd
    int ef = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    uint64_t v = 0;
    uint32_t n = mk(fd, 0);
    struct soeventfd ev = {.handle = n, .fd = ef};
    int ra = io(fd, SO_EVENTFD, &ev);
    int before = (int)read(ef, &v, 8);
    int be = errno;
    signal1(fd, n, SO_SIGNAL);
    v = 0;
    int after = (int)read(ef, &v, 8);
    uint64_t v1 = v;
    struct soeventfd ev2 = {.handle = n, .fd = ef};
    int rb = io(fd, SO_EVENTFD, &ev2);
    v = 0;
    int now = (int)read(ef, &v, 8);
    uint64_t v2 = v;
    uint32_t tl = mk(fd, 0);
    struct soeventfd ev3 = {.handle = tl, .fd = ef, .point = 3, .flags = WAIT_AVAILABLE};
    int rc = io(fd, SO_EVENTFD, &ev3);
    tlsignal1(fd, tl, 2);
    v = 0;
    int r_2 = (int)read(ef, &v, 8);
    tlsignal1(fd, tl, 3);
    v = 0;
    int r_3 = (int)read(ef, &v, 8);
    uint64_t v3 = v;
    snprintf(buf, sizeof buf, "EVENTFD (%d, before %d %s, after SIGNAL %d %llu, already signaled %d %d %llu, timeline 3 %d: at 2 %d, at 3 %d %llu)", ra, before,
             be == EAGAIN ? "EAGAIN" : "?", after, (unsigned long long)v1, rb, now, (unsigned long long)v2, rc, r_2, r_3, (unsigned long long)v3);
    check(buf, ra == 0 && before == -1 && be == EAGAIN && after == 8 && v1 == 1 && rb == 0 && now == 8 && v2 == 1 && rc == 0 && r_2 == -1 && r_3 == 8 && v3 == 1);
    struct soeventfd g1 = {.handle = n, .fd = ef, .flags = 1}, g2 = {.handle = n, .fd = ef, .pad = 1}, g3 = {.handle = 99, .fd = ef}, g4 = {.handle = n, .fd = fd},
                     g5 = {.handle = n, .fd = -1};
    int k1 = io(fd, SO_EVENTFD, &g1), k2 = io(fd, SO_EVENTFD, &g2), k3 = io(fd, SO_EVENTFD, &g3), k4 = io(fd, SO_EVENTFD, &g4), k5 = io(fd, SO_EVENTFD, &g5);
    snprintf(buf, sizeof buf, "EVENTFD errors (flags 1 %d, pad %d, no handle %d, not eventfd %d, -1 %d)", k1, k2, k3, k4, k5);
    check(buf, k1 == -EINVAL && k2 == -EINVAL && k3 == -ENOENT && k4 == -EINVAL && k5 == -EBADF);
    close(ef);
  }

  // exec (3D だけ)
  {
    uint32_t ob = mk(fd, 0), ot = mk(fd, 0), in = mk(fd, 1), empty = mk(fd, 0);
    struct execso outs[2] = {{.handle = ob}, {.handle = ot, .point = 1}};
    struct execso ins[1] = {{.handle = in, .flags = 1}};
    char cmd[16] = {0};
    struct exec e = {.size = 0, .command = (uintptr_t)cmd, .fence_fd = -1, .syncobj_stride = sizeof(struct execso), .num_in_syncobjs = 1, .num_out_syncobjs = 2,
                     .in_syncobjs = (uintptr_t)ins, .out_syncobjs = (uintptr_t)outs};
    int r = io(fd, EXECBUFFER, &e);
    if (r == -ENOSYS) {
      printf("drmsyncobj: EXECBUFFER is ENOSYS (no 3D): exec skipped\n");
    } else {
      int64_t until = now_ns() + 5000000000LL;
      int wb = waitn(fd, &ob, 1, 0, until, NULL), wt = tlwait1(fd, ot, 1, 0, until);
      int64_t qt = query1(fd, ot, 0);
      int wi = wait1(fd, in);
      int sf = export_sf(fd, ob, 0);
      struct pollfd pf = {.fd = sf, .events = POLLIN};
      poll(&pf, 1, 5000);
      struct fileinfo fi;
      struct fenceinfo fe[4];
      int ri = sfinfo(sf, &fi, fe);
      snprintf(buf, sizeof buf, "EXECBUFFER with syncobjs (%d; out binary WAIT %d, out timeline WAIT %d QUERY %lld, in RESET then WAIT %d; export %d %s fences %u %s)", r, wb, wt,
               (long long)qt, wi, ri, fi.name, fi.num_fences, fe[0].obj_name);
      check(buf, r == 0 && wb == 0 && wt == 0 && qt == 1 && wi == -EINVAL && ri == 0 && !strcmp(fi.name, DONE_NAME) && fi.num_fences == 1 &&
                     !strcmp(fe[0].obj_name, DONE_OBJ));
      close(sf);

      struct exec x1 = e, x2 = e, x3 = e, x4 = e;
      struct execso bad_out[1] = {{.handle = ob, .flags = 1}}, no_out[1] = {{.handle = 99}}, no_in[1] = {{.handle = 99}}, empty_in[1] = {{.handle = empty}};
      x1.out_syncobjs = (uintptr_t)bad_out;
      x1.num_out_syncobjs = 1;
      x1.num_in_syncobjs = 0;
      x2.out_syncobjs = (uintptr_t)no_out;
      x2.num_out_syncobjs = 1;
      x2.num_in_syncobjs = 0;
      x3.in_syncobjs = (uintptr_t)no_in;
      x3.num_out_syncobjs = 0;
      x4.in_syncobjs = (uintptr_t)empty_in;
      x4.num_out_syncobjs = 0;
      int y1 = io(fd, EXECBUFFER, &x1), y2 = io(fd, EXECBUFFER, &x2), y3 = io(fd, EXECBUFFER, &x3), y4 = io(fd, EXECBUFFER, &x4);
      snprintf(buf, sizeof buf, "EXECBUFFER syncobj errors (out flags 1 %d, no out %d, no in %d, empty in %d)", y1, y2, y3, y4);
      check(buf, y1 == -EINVAL && y2 == -EINVAL && y3 == -ENOENT && y4 == -EINVAL);
    }
  }
  close(fd);
  printf("drmsyncobj: %d failed\n", fails);
  return fails != 0;
}
