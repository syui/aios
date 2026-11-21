// listns (Linux 6.19、7.4 の形) と nsfs の NS_GET_ID / NS_GET_MNTNS_ID / NS_GET_NSTYPE / NS_GET_OWNER_UID が Linux と
// 同じか (root で)。はじめの namespace の番号 (ipc 1、uts 2、user 3、pid 4、net 7、mnt 8)、listns は使われている
// namespace を番号の順に (種類でしぼる、ns_id の次から、持ち主のユーザーの namespace でしぼる、root でなければ自分の
// ものだけ)、子が作ったものは Linux の作る順 (mnt、uts、ipc、pid、net) に新しい番号、子が終わればなくなる (nsfs の口が
// あればのこる)。たしかめる順 (flags、数、入れ物、大きさ、spare、知らない種類)。cgroup と time の namespace は aios に
// ないので、しぼって見る
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define NS_GET_NSTYPE 0xb703
#define NS_GET_OWNER_UID 0xb704
#define NS_GET_MNTNS_ID 0x8008b705ul
#define NS_GET_ID 0x8008b70dul
#define T_IPC 0x08000000u
#define T_UTS 0x04000000u
#define T_USER 0x10000000u
#define T_PID 0x20000000u
#define T_NET 0x40000000u
#define T_MNT 0x00020000u
#define T6 (T_IPC | T_UTS | T_USER | T_PID | T_NET | T_MNT)
#define CURRENT_USER 0xffffffffffffffffull

struct req {
  uint32_t size, spare;
  uint64_t ns_id;
  uint32_t ns_type, spare2;
  uint64_t user_ns_id;
};

static long lns(uint32_t type, uint64_t last, uint64_t user, uint64_t *ids, size_t n, unsigned fl) {
  struct req r = {32, 0, last, type, 0, user};
  long x = syscall(470, &r, ids, n, fl);
  return x < 0 ? -errno : x;
}
static long nsid_fd(int fd, unsigned long req) {
  uint64_t id = 0;
  return ioctl(fd, req, &id) < 0 ? -errno : (long)id;
}
static long nsid(const char *p) {
  int fd = open(p, O_RDONLY);
  long r = fd < 0 ? -errno : nsid_fd(fd, NS_GET_ID);
  if (fd >= 0)
    close(fd);
  return r;
}

// 子の新しい namespace の番号 (種類ごと)
static const char *kinds[] = {"mnt", "uts", "ipc", "pid_for_children", "net"};
static uint64_t child_ids[5];

static const char *name_of(uint64_t id) {
  static const char *init[] = {"?", "ipc", "uts", "user", "pid", "cgroup", "time", "net", "mnt"};
  if (id >= 1 && id <= 8)
    return init[id];
  for (int i = 0; i < 5; i++)
    if (child_ids[i] == id)
      return i == 3 ? "new-pid" : i == 0 ? "new-mnt" : i == 1 ? "new-uts" : i == 2 ? "new-ipc" : "new-net";
  return "other";
}
// テストが知らない namespace (Linux の devtmpfsd が作るマウントの namespace など) は数えない
static void show(const char *what, long r, uint64_t *ids) {
  if (r < 0) {
    printf("%s: %ld\n", what, r);
    return;
  }
  long n = 0;
  for (long i = 0; i < r; i++)
    n += strcmp(name_of(ids[i]), "other") != 0;
  printf("%s: %ld [", what, n);
  for (long i = 0, k = 0; i < r; i++)
    if (strcmp(name_of(ids[i]), "other"))
      printf("%s%s", k++ ? " " : "", name_of(ids[i]));
  printf("]\n");
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (getuid() != 0) {
    printf("listns: not root, skipped\n");
    return 0;
  }
  // nsfs の ioctl
  int uts = open("/proc/self/ns/uts", O_RDONLY), mnt = open("/proc/self/ns/mnt", O_RDONLY), user = open("/proc/self/ns/user", O_RDONLY);
  uid_t owner = 12345;
  long ro = ioctl(user, NS_GET_OWNER_UID, &owner) < 0 ? -errno : 0;
  printf("ids: ipc %ld uts %ld user %ld pid %ld net %ld mnt %ld, mntns id %ld (uts %ld), type of uts %#x, owner of user %ld %u (of uts %ld)\n",
         nsid("/proc/self/ns/ipc"), nsid("/proc/self/ns/uts"), nsid("/proc/self/ns/user"), nsid("/proc/self/ns/pid"), nsid("/proc/self/ns/net"),
         nsid("/proc/self/ns/mnt"), nsid_fd(mnt, NS_GET_MNTNS_ID), nsid_fd(uts, NS_GET_MNTNS_ID), ioctl(uts, NS_GET_NSTYPE), ro, owner,
         ioctl(uts, NS_GET_OWNER_UID, &owner) < 0 ? -errno : 0L);
  close(uts);
  close(mnt);
  close(user);

  uint64_t ids[64];
  show("all (6 kinds)", lns(T6, 0, 0, ids, 64, 0), ids);
  show("uts", lns(T_UTS, 0, 0, ids, 64, 0), ids);
  show("uts after 2", lns(T_UTS, 2, 0, ids, 64, 0), ids);
  show("after pid", lns(T6, 4, 0, ids, 64, 0), ids);
  show("first 2", lns(T6, 0, 0, ids, 2, 0), ids);
  show("owned by me", lns(T6, 0, CURRENT_USER, ids, 64, 0), ids);
  show("uts and net", lns(T_UTS | T_NET, 0, 0, ids, 64, 0), ids);

  // 子が作った namespace
  int p[2], q[2];
  if (pipe(p) || pipe(q))
    return 1;
  pid_t c = fork();
  if (c == 0) {
    if (unshare(CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWPID | CLONE_NEWNET))
      _exit(1);
    uint64_t v[5];
    for (int i = 0; i < 5; i++) {
      char path[64];
      snprintf(path, sizeof path, "/proc/self/ns/%s", kinds[i]);
      v[i] = nsid(path);
    }
    if (write(p[1], v, sizeof v) != sizeof v)
      _exit(2);
    char b;
    read(q[0], &b, 1);
    _exit(0);
  }
  if (read(p[0], child_ids, sizeof child_ids) != sizeof child_ids)
    return 2;
  int sorted = 1;
  for (int i = 1; i < 5; i++)
    sorted &= child_ids[i] > child_ids[i - 1];
  printf("child: new ids above 8 %d, in the order mnt uts ipc pid net %d\n", child_ids[0] > 8, sorted);
  long n = lns(T6, 0, 0, ids, 64, 0);
  show("all with the child", n, ids);
  uint64_t maxid = n > 0 ? ids[n - 1] : 0;
  show("after the last", lns(T6, maxid, 0, ids, 64, 0), ids);
  show("uts with the child", lns(T_UTS, 0, 0, ids, 64, 0), ids);
  show("owned by me with the child", lns(T6, 0, CURRENT_USER, ids, 64, 0), ids);
  show("after mnt (8)", lns(T6, 8, 0, ids, 64, 0), ids);
  // root でなければ自分のものだけ
  pid_t g = fork();
  if (g == 0) {
    if (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
      _exit(1);
    show("as nobody", lns(T6, 0, 0, ids, 64, 0), ids);
    show("as nobody, owned by me", lns(T6, 0, CURRENT_USER, ids, 64, 0), ids);
    _exit(0);
  }
  waitpid(g, NULL, 0);
  // 子の uts の nsfs の口を持っておく
  char path[64];
  snprintf(path, sizeof path, "/proc/%d/ns/uts", c);
  int keep = open(path, O_RDONLY);
  if (write(q[1], "x", 1) != 1)
    return 3;
  waitpid(c, NULL, 0);
  show("child gone, its uts open", lns(T6, 0, 0, ids, 64, 0), ids);
  close(keep);
  show("and closed", lns(T6, 0, 0, ids, 64, 0), ids);

  // ユーザーの namespace
  memset(child_ids, 0, sizeof child_ids);
  if (pipe(p) || pipe(q))
    return 1;
  c = fork();
  if (c == 0) {
    if (unshare(CLONE_NEWUSER))
      _exit(1);
    uint64_t v[2] = {(uint64_t)nsid("/proc/self/ns/user"), 0};
    long a = lns(T6, 0, CURRENT_USER, ids, 64, 0);
    if (unshare(CLONE_NEWUTS))
      _exit(3);
    v[1] = nsid("/proc/self/ns/uts");
    long b2 = lns(T6, 0, CURRENT_USER, ids, 64, 0);
    printf("in a user ns: owned by it %ld, after a new uts %ld (it %d)\n", a, b2, b2 == 1 && ids[0] == v[1]);
    if (write(p[1], v, sizeof v) != sizeof v)
      _exit(2);
    char b;
    read(q[0], &b, 1);
    _exit(0);
  }
  uint64_t uv[2];
  if (read(p[0], uv, sizeof uv) != sizeof uv)
    return 4;
  n = lns(T_USER, 0, 0, ids, 64, 0);
  int has_user = 0;
  for (long i = 0; i < n; i++)
    has_user += ids[i] == uv[0];
  long o = lns(T6, 0, uv[0], ids, 64, 0);
  printf("user ns: listed %d, owned by it %ld (the uts %d), its uts owned by me %ld\n", has_user, o, o == 1 && ids[0] == uv[1],
         lns(T_UTS, uv[1] - 1, CURRENT_USER, ids, 64, 0));
  if (write(q[1], "x", 1) != 1)
    return 5;
  waitpid(c, NULL, 0);

  // たしかめる順
  struct req big = {8192, 0, 0, T6, 0, 0}, small = {16, 0, 0, T6, 0, 0}, spare = {32, 1, 0, T6, 0, 0}, bad = {32, 0, 0, 1, 0, 0}, unk = {32, 0, 0, T6, 0, 999999};
  long e[9];
  e[0] = syscall(470, &(struct req){32, 0, 0, T6, 0, 0}, ids, 64, 1) < 0 ? -errno : 0;
  e[1] = syscall(470, &(struct req){32, 0, 0, T6, 0, 0}, ids, 2000000, 0) < 0 ? -errno : 0;
  e[2] = syscall(470, &(struct req){32, 0, 0, T6, 0, 0}, (void *)8, 4, 0) < 0 ? -errno : 0;
  e[3] = syscall(470, &big, ids, 64, 0) < 0 ? -errno : 0;
  e[4] = syscall(470, &small, ids, 64, 0) < 0 ? -errno : 0;
  e[5] = syscall(470, &spare, ids, 64, 0) < 0 ? -errno : 0;
  e[6] = syscall(470, &bad, ids, 64, 0) < 0 ? -errno : 0;
  e[7] = syscall(470, &unk, ids, 64, 0) < 0 ? -errno : 0;
  e[8] = syscall(470, (void *)8, ids, 64, 0) < 0 ? -errno : 0;
  printf("checks: flags %ld, too many %ld, bad ids %ld, too big %ld, too small %ld, spare %ld, unknown type %ld, unknown user ns %ld, bad req %ld, zero %ld\n",
         e[0], e[1], e[2], e[3], e[4], e[5], e[6], e[7], e[8], lns(T6, 0, 0, ids, 0, 0));
  return 0;
}
